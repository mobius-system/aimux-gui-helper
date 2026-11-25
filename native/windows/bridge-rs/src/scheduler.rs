//! Declarative input scheduler for the `scheduleFuture` helper command.
//!
//! There is no worker thread per scheduled action. The helper keeps one table
//! that says which key or mouse button should be held during which interval,
//! plus the point events (cursor moves, wheel notches) that are due at a given
//! time. A single thread wakes every [`TICK`], compares the table with the
//! state it last injected and injects only the differences.
//!
//! The table is the source of truth. Overlapping intervals for the same
//! actuator merge into one continuous hold, which is exactly what a caller
//! means when it renews a hold before the previous one expires.
//!
//! Two rules keep the 10 ms sampling honest:
//!
//! * a hold is never shorter than [`MIN_HOLD_MS`], so an interval always spans
//!   several ticks and cannot be slept over entirely;
//! * when a gap between two intervals of the same actuator was slept over, the
//!   release is injected now and the following press is deferred by one tick,
//!   so the application really observes the gap instead of one merged hold.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

/// Sampling period of the one scheduler thread.
pub const TICK_MS: u64 = 10;
pub const TICK: Duration = Duration::from_millis(TICK_MS);
/// Shortest accepted hold. Anything below one tick could be slept over.
pub const MIN_HOLD_MS: u64 = 50;
pub const MAX_HOLD_MS: u64 = 10_000;
pub const MAX_DELAY_MS: u64 = 60_000;
pub const MAX_TASKS: usize = 256;
/// How long a finished task stays queryable before it is dropped.
pub const TASK_RETENTION: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "left" => Some(Button::Left),
            "right" => Some(Button::Right),
            "middle" => Some(Button::Middle),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Right => "right",
            Button::Middle => "middle",
        }
    }
}

/// Anything the scheduler can hold down: a keyboard key or a mouse button.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Actuator {
    Key(u16),
    Button(Button),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Scheduled,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Scheduled => "scheduled",
            Phase::Running => "running",
            Phase::Completed => "completed",
            Phase::Failed => "failed",
            Phase::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Phase::Completed | Phase::Failed | Phase::Cancelled)
    }
}

/// One-shot work that has a due time but no hold.
#[derive(Clone, Debug, PartialEq)]
pub enum PointAction {
    Move { x: i32, y: i32 },
    Scroll { x: i32, y: i32, dx: f64, dy: f64 },
}

#[derive(Clone, Debug)]
pub struct Task {
    pub id: String,
    pub phase: Phase,
    pub submitted_ms: u128,
    pub scheduled_ms: u128,
    pub started_ms: Option<u128>,
    pub finished_ms: Option<u128>,
    pub scheduled_at: Instant,
    pub ends_at: Instant,
    /// Monotonic stamp of the terminal transition, for retention only.
    pub finished_at: Option<Instant>,
    pub error: Option<String>,
}

/// Everything the caller has to inject for one tick, in injection order.
#[derive(Default, Debug)]
pub struct TickReport {
    pub releases: Vec<Actuator>,
    pub presses: Vec<Actuator>,
    pub points: Vec<PointAction>,
    pub deferred: Vec<Actuator>,
    pub failures: Vec<(Option<String>, String)>,
}

/// Platform layer. The scheduler itself never touches Win32.
pub trait Injector {
    fn press(&mut self, actuator: Actuator) -> Result<(), String>;
    fn release(&mut self, actuator: Actuator) -> Result<(), String>;
    fn point(&mut self, action: &PointAction) -> Result<(), String>;
}

#[derive(Clone, Debug)]
struct HoldInterval {
    task_id: String,
    start: Instant,
    end: Instant,
    order: u32,
}

#[derive(Clone, Debug)]
struct PointEvent {
    task_id: String,
    due: Instant,
    sequence: u64,
    action: PointAction,
}

pub struct Scheduler {
    holds: HashMap<Actuator, Vec<HoldInterval>>,
    points: Vec<PointEvent>,
    tasks: HashMap<String, Task>,
    /// What this thread last injected, per actuator.
    shadow: HashMap<Actuator, bool>,
    /// Actuators whose press was deferred because a gap was slept over.
    pending_press: HashMap<Actuator, Instant>,
    last_tick: Instant,
    next_sequence: u64,
}

impl Scheduler {
    pub fn new(now: Instant) -> Self {
        Self {
            holds: HashMap::new(),
            points: Vec::new(),
            tasks: HashMap::new(),
            shadow: HashMap::new(),
            pending_press: HashMap::new(),
            last_tick: now,
            next_sequence: 1,
        }
    }

    /// Register a hold. `actuators` carries the press order within the task.
    pub fn submit_hold(
        &mut self,
        id: &str,
        now: Instant,
        now_ms: u128,
        delay: Duration,
        hold: Duration,
        actuators: Vec<(Actuator, u32)>,
    ) -> Result<(), String> {
        if self.tasks.contains_key(id) {
            return Err(format!("duplicate task id '{id}'"));
        }
        if actuators.is_empty() {
            return Err("a hold needs at least one actuator".to_owned());
        }
        self.make_room()?;
        let start = now + delay;
        let end = start + hold;
        for (actuator, order) in &actuators {
            self.holds
                .entry(*actuator)
                .or_default()
                .push(HoldInterval { task_id: id.to_owned(), start, end, order: *order });
        }
        self.tasks.insert(
            id.to_owned(),
            Task {
                id: id.to_owned(),
                phase: Phase::Scheduled,
                submitted_ms: now_ms,
                scheduled_ms: now_ms + delay.as_millis(),
                started_ms: None,
                finished_ms: None,
                scheduled_at: start,
                ends_at: end,
                finished_at: None,
                error: None,
            },
        );
        Ok(())
    }

    /// Register a one-shot event (cursor move or wheel notch).
    pub fn submit_point(
        &mut self,
        id: &str,
        now: Instant,
        now_ms: u128,
        delay: Duration,
        action: PointAction,
    ) -> Result<(), String> {
        if self.tasks.contains_key(id) {
            return Err(format!("duplicate task id '{id}'"));
        }
        self.make_room()?;
        let due = now + delay;
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.points.push(PointEvent { task_id: id.to_owned(), due, sequence, action });
        self.tasks.insert(
            id.to_owned(),
            Task {
                id: id.to_owned(),
                phase: Phase::Scheduled,
                submitted_ms: now_ms,
                scheduled_ms: now_ms + delay.as_millis(),
                started_ms: None,
                finished_ms: None,
                scheduled_at: due,
                ends_at: due,
                finished_at: None,
                error: None,
            },
        );
        Ok(())
    }

    pub fn status(&self, id: &str) -> Option<&Task> {
        self.tasks.get(id)
    }

    /// True while any task is still scheduled or running.
    pub fn active(&self) -> bool {
        self.tasks
            .values()
            .any(|task| matches!(task.phase, Phase::Scheduled | Phase::Running))
    }

    /// Drop a queued or running task. Returns the phase it had before.
    pub fn cancel(&mut self, id: &str, now: Instant, now_ms: u128) -> Option<Phase> {
        let phase = self.tasks.get(id)?.phase;
        if !matches!(phase, Phase::Scheduled | Phase::Running) {
            return Some(phase);
        }
        self.remove_task_inputs(id);
        if let Some(task) = self.tasks.get_mut(id) {
            task.phase = Phase::Cancelled;
            task.finished_ms = Some(now_ms);
            task.finished_at = Some(now);
        }
        Some(phase)
    }

    /// One sampling step: compare the table with the injected state and emit
    /// only the differences.
    pub fn tick<I: Injector>(&mut self, now: Instant, now_ms: u128, injector: &mut I) -> TickReport {
        let mut report = TickReport::default();
        let window_start = self.last_tick;
        self.last_tick = now;
        // Intervals that ended before the previous tick no longer matter for
        // the gap test, so they can go now.
        self.prune_intervals(window_start);

        let desired = self.covered_now(now);
        let mut actors: BTreeSet<Actuator> = BTreeSet::new();
        actors.extend(desired.iter().copied());
        for (actuator, down) in &self.shadow {
            if *down {
                actors.insert(*actuator);
            }
        }
        actors.extend(self.pending_press.keys().copied());

        let mut presses: Vec<Actuator> = Vec::new();
        let mut releases: Vec<Actuator> = Vec::new();
        let mut deferred: Vec<Actuator> = Vec::new();

        for actuator in actors {
            let covered = desired.contains(&actuator);
            let down = self.shadow.get(&actuator).copied().unwrap_or(false);

            if !covered {
                self.pending_press.remove(&actuator);
                if down {
                    releases.push(actuator);
                }
                continue;
            }

            if let Some(pending_at) = self.pending_press.get(&actuator).copied() {
                if now < pending_at {
                    continue;
                }
                self.pending_press.remove(&actuator);
                if !down {
                    presses.push(actuator);
                }
                continue;
            }

            if down {
                if self.has_gap(&actuator, window_start, now) {
                    releases.push(actuator);
                    deferred.push(actuator);
                }
            } else {
                presses.push(actuator);
            }
        }

        let press_key = |a: &Actuator| {
            self.earliest_covering(a, now)
                .map(|(start, order)| (start, order))
                .unwrap_or((now, u32::MAX))
        };
        presses.sort_by_key(&press_key);
        let release_key = |a: &Actuator| self.latest_end(a).unwrap_or((now, 0));
        releases.sort_by(|a, b| release_key(b).cmp(&release_key(a)));

        // Releases first: a chord is released in reverse press order, and a
        // release caused by a slept-over gap must land before the next press.
        for actuator in releases {
            match injector.release(actuator) {
                Ok(()) => {
                    self.shadow.insert(actuator, false);
                    report.releases.push(actuator);
                    if deferred.contains(&actuator) {
                        self.pending_press.insert(actuator, now + TICK);
                        report.deferred.push(actuator);
                    }
                }
                Err(message) => {
                    report.failures.push((self.task_for(&actuator, now), message));
                }
            }
        }

        for actuator in presses {
            match injector.press(actuator) {
                Ok(()) => {
                    self.shadow.insert(actuator, true);
                    report.presses.push(actuator);
                }
                Err(message) => {
                    let task = self.task_for(&actuator, now);
                    report.failures.push((task.clone(), message.clone()));
                    if let Some(task_id) = task {
                        self.fail_task(&task_id, now, now_ms, &message);
                    }
                }
            }
        }

        self.run_points(now, now_ms, injector, &mut report);
        self.advance_tasks(now, now_ms);
        self.prune_tasks(now);
        report
    }

    /// Actuators an application should currently see as held.
    fn covered_now(&self, now: Instant) -> BTreeSet<Actuator> {
        self.holds
            .iter()
            .filter(|(_, intervals)| {
                intervals.iter().any(|interval| interval.start <= now && interval.end > now)
            })
            .map(|(actuator, _)| *actuator)
            .collect()
    }

    fn earliest_covering(&self, actuator: &Actuator, now: Instant) -> Option<(Instant, u32)> {
        self.holds.get(actuator)?.iter()
            .filter(|interval| interval.start <= now && interval.end > now)
            .map(|interval| (interval.start, interval.order))
            .min()
    }

    fn latest_end(&self, actuator: &Actuator) -> Option<(Instant, u32)> {
        self.holds.get(actuator)?.iter()
            .map(|interval| (interval.end, interval.order))
            .max()
    }

    /// True when the actuator's coverage lapsed at some point in
    /// `(window_start, now]`, i.e. a release was slept over.
    fn has_gap(&self, actuator: &Actuator, window_start: Instant, now: Instant) -> bool {
        let Some(intervals) = self.holds.get(actuator) else {
            return false;
        };
        let mut spans: Vec<(Instant, Instant)> = intervals
            .iter()
            .filter(|interval| interval.end > window_start && interval.start <= now)
            .map(|interval| (interval.start.max(window_start), interval.end.min(now)))
            .collect();
        spans.sort();
        let mut cursor = window_start;
        for (start, end) in spans {
            if start > cursor {
                return true;
            }
            if end > cursor {
                cursor = end;
            }
            if cursor >= now {
                return false;
            }
        }
        cursor < now
    }

    fn task_for(&self, actuator: &Actuator, now: Instant) -> Option<String> {
        self.holds.get(actuator)?.iter()
            .filter(|interval| interval.start <= now && interval.end > now)
            .min_by_key(|interval| (interval.start, interval.order))
            .map(|interval| interval.task_id.clone())
    }

    fn run_points<I: Injector>(
        &mut self,
        now: Instant,
        now_ms: u128,
        injector: &mut I,
        report: &mut TickReport,
    ) {
        if self.points.is_empty() {
            return;
        }
        let mut future = Vec::with_capacity(self.points.len());
        let mut due = Vec::new();
        for point in std::mem::take(&mut self.points) {
            if point.due <= now {
                due.push(point);
            } else {
                future.push(point);
            }
        }
        due.sort_by(|a, b| (a.due, a.sequence).cmp(&(b.due, b.sequence)));
        for point in due {
            match injector.point(&point.action) {
                Ok(()) => {
                    report.points.push(point.action.clone());
                    if let Some(task) = self.tasks.get_mut(&point.task_id) {
                        if !task.phase.is_terminal() {
                            task.phase = Phase::Completed;
                            task.started_ms.get_or_insert(now_ms);
                            task.finished_ms = Some(now_ms);
                            task.finished_at = Some(now);
                        }
                    }
                }
                Err(message) => {
                    report.failures.push((Some(point.task_id.clone()), message.clone()));
                    self.fail_task(&point.task_id, now, now_ms, &message);
                }
            }
        }
        self.points = future;
    }

    fn advance_tasks(&mut self, now: Instant, now_ms: u128) {
        for task in self.tasks.values_mut() {
            if matches!(task.phase, Phase::Scheduled) && task.scheduled_at <= now {
                task.phase = Phase::Running;
                task.started_ms.get_or_insert(now_ms);
            }
            if matches!(task.phase, Phase::Scheduled | Phase::Running) && task.ends_at <= now {
                task.phase = Phase::Completed;
                task.started_ms.get_or_insert(now_ms);
                task.finished_ms = Some(now_ms);
                task.finished_at = Some(now);
            }
        }
    }

    fn fail_task(&mut self, task_id: &str, now: Instant, now_ms: u128, message: &str) {
        if let Some(task) = self.tasks.get_mut(task_id) {
            if !task.phase.is_terminal() {
                task.phase = Phase::Failed;
                task.finished_ms = Some(now_ms);
                task.finished_at = Some(now);
                task.error = Some(message.to_owned());
            }
        }
        self.remove_task_inputs(task_id);
    }

    fn remove_task_inputs(&mut self, task_id: &str) {
        for intervals in self.holds.values_mut() {
            intervals.retain(|interval| interval.task_id != task_id);
        }
        self.holds.retain(|_, intervals| !intervals.is_empty());
        self.points.retain(|point| point.task_id != task_id);
    }

    fn prune_intervals(&mut self, before: Instant) {
        for intervals in self.holds.values_mut() {
            intervals.retain(|interval| interval.end > before);
        }
        self.holds.retain(|_, intervals| !intervals.is_empty());
    }

    fn prune_tasks(&mut self, now: Instant) {
        let stale: Vec<String> = self.tasks.iter()
            .filter(|(_, task)| {
                task.phase.is_terminal()
                    && task
                        .finished_at
                        .map(|finished| now.saturating_duration_since(finished) > TASK_RETENTION)
                        .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.tasks.remove(&id);
        }
    }

    fn make_room(&mut self) -> Result<(), String> {
        if self.tasks.len() < MAX_TASKS {
            return Ok(());
        }
        let oldest = self.tasks.iter()
            .filter(|(_, task)| task.phase.is_terminal())
            .min_by_key(|(_, task)| task.submitted_ms)
            .map(|(id, _)| id.clone());
        match oldest {
            Some(id) => {
                self.tasks.remove(&id);
                    Ok(())
            }
            None => Err(format!("future task limit reached ({MAX_TASKS} active tasks)")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        log: Vec<String>,
    }

    impl Injector for Recorder {
        fn press(&mut self, actuator: Actuator) -> Result<(), String> {
            self.log.push(format!("down {actuator:?}"));
            Ok(())
        }
        fn release(&mut self, actuator: Actuator) -> Result<(), String> {
            self.log.push(format!("up {actuator:?}"));
            Ok(())
        }
        fn point(&mut self, action: &PointAction) -> Result<(), String> {
            self.log.push(format!("point {action:?}"));
            Ok(())
        }
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn run(scheduler: &mut Scheduler, recorder: &mut Recorder, t0: Instant, at: u64) -> Vec<String> {
        scheduler.tick(t0 + ms(at), at as u128, recorder);
        recorder.log.clone()
    }

    #[test]
    fn overlapping_holds_merge_into_one_press() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::new(t0);
        let mut recorder = Recorder::default();
        let key = Actuator::Key(0x57);
        scheduler
            .submit_hold("first", t0, 0, Duration::ZERO, ms(100), vec![(key, 0)])
            .unwrap();
        scheduler
            .submit_hold("second", t0 + ms(50), 50, Duration::ZERO, ms(100), vec![(key, 0)])
            .unwrap();

        assert_eq!(run(&mut scheduler, &mut recorder, t0, 10), vec!["down Key(87)"]);
        for at in [20u64, 30, 40, 50, 60, 80, 100, 120, 140] {
            run(&mut scheduler, &mut recorder, t0, at);
        }
        assert_eq!(recorder.log, vec!["down Key(87)"], "renewal must not churn");
        assert_eq!(
            run(&mut scheduler, &mut recorder, t0, 160),
            vec!["down Key(87)", "up Key(87)"]
        );
    }

    #[test]
    fn slept_over_gap_releases_now_and_defers_the_press() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::new(t0);
        let mut recorder = Recorder::default();
        let key = Actuator::Key(0x41);
        scheduler
            .submit_hold("a", t0, 0, Duration::ZERO, ms(105), vec![(key, 0)])
            .unwrap();
        scheduler
            .submit_hold("b", t0, 0, ms(106), ms(100), vec![(key, 0)])
            .unwrap();

        run(&mut scheduler, &mut recorder, t0, 10);
        run(&mut scheduler, &mut recorder, t0, 100);
        assert_eq!(recorder.log, vec!["down Key(65)"]);
        // The window (100, 110] contains the 105..106 gap.
        assert_eq!(
            run(&mut scheduler, &mut recorder, t0, 110),
            vec!["down Key(65)", "up Key(65)"]
        );
        // The deferred press lands one tick later.
        assert_eq!(
            run(&mut scheduler, &mut recorder, t0, 120),
            vec!["down Key(65)", "up Key(65)", "down Key(65)"]
        );
        run(&mut scheduler, &mut recorder, t0, 220);
        assert_eq!(recorder.log.last().unwrap(), "up Key(65)");
    }

    #[test]
    fn touching_intervals_do_not_churn() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::new(t0);
        let mut recorder = Recorder::default();
        let key = Actuator::Key(0x42);
        scheduler
            .submit_hold("a", t0, 0, Duration::ZERO, ms(100), vec![(key, 0)])
            .unwrap();
        scheduler
            .submit_hold("b", t0, 0, ms(100), ms(100), vec![(key, 0)])
            .unwrap();

        for at in [10u64, 50, 100, 150, 200, 210] {
            run(&mut scheduler, &mut recorder, t0, at);
        }
        assert_eq!(recorder.log, vec!["down Key(66)", "up Key(66)"]);
    }

    #[test]
    fn cancel_releases_on_the_next_tick() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::new(t0);
        let mut recorder = Recorder::default();
        let key = Actuator::Key(0x43);
        scheduler
            .submit_hold("hold", t0, 0, Duration::ZERO, ms(500), vec![(key, 0)])
            .unwrap();
        run(&mut scheduler, &mut recorder, t0, 10);
        assert_eq!(scheduler.cancel("hold", t0 + ms(20), 20), Some(Phase::Running));
        run(&mut scheduler, &mut recorder, t0, 20);
        assert_eq!(recorder.log, vec!["down Key(67)", "up Key(67)"]);
        assert!(!scheduler.active());
    }

    #[test]
    fn point_events_run_in_due_then_acceptance_order() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::new(t0);
        let mut recorder = Recorder::default();
        scheduler
            .submit_point("late", t0, 0, ms(50), PointAction::Move { x: 2, y: 2 })
            .unwrap();
        scheduler
            .submit_point("early", t0, 0, ms(20), PointAction::Move { x: 1, y: 1 })
            .unwrap();
        run(&mut scheduler, &mut recorder, t0, 10);
        assert!(recorder.log.is_empty());
        run(&mut scheduler, &mut recorder, t0, 20);
        run(&mut scheduler, &mut recorder, t0, 50);
        assert_eq!(
            recorder.log,
            vec![
                "point Move { x: 1, y: 1 }".to_owned(),
                "point Move { x: 2, y: 2 }".to_owned()
            ]
        );
        assert_eq!(scheduler.status("late").map(|task| task.phase), Some(Phase::Completed));
    }

    #[test]
    fn chord_presses_in_order_and_releases_in_reverse() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::new(t0);
        let mut recorder = Recorder::default();
        scheduler
            .submit_hold(
                "chord",
                t0,
                0,
                Duration::ZERO,
                ms(100),
                vec![(Actuator::Key(0x11), 0), (Actuator::Key(0x43), 1)],
            )
            .unwrap();
        run(&mut scheduler, &mut recorder, t0, 10);
        run(&mut scheduler, &mut recorder, t0, 120);
        assert_eq!(
            recorder.log,
            vec!["down Key(17)", "down Key(67)", "up Key(67)", "up Key(17)"]
        );
    }

    #[test]
    fn tasks_run_from_scheduled_to_completed() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::new(t0);
        let mut recorder = Recorder::default();
        scheduler
            .submit_hold("task", t0, 0, ms(50), ms(50), vec![(Actuator::Key(0x44), 0)])
            .unwrap();
        assert_eq!(scheduler.status("task").unwrap().phase, Phase::Scheduled);
        run(&mut scheduler, &mut recorder, t0, 10);
        assert_eq!(scheduler.status("task").unwrap().phase, Phase::Scheduled);
        run(&mut scheduler, &mut recorder, t0, 60);
        assert_eq!(scheduler.status("task").unwrap().phase, Phase::Running);
        run(&mut scheduler, &mut recorder, t0, 120);
        assert_eq!(scheduler.status("task").unwrap().phase, Phase::Completed);
    }
}
