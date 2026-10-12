//! Window recording to MP4 with the Windows Media Foundation sink writer.
//!
//! Video comes from the same GDI path `look` uses (PrintWindow with
//! PW_RENDERFULLCONTENT), so occlusion does not matter and no screen-capture
//! API is involved. Audio comes from WASAPI — the default render endpoint in
//! loopback mode (system sound) or the default capture endpoint (microphone).
//! Video is H.264 and audio is AAC, muxed into one MP4 by the sink writer that
//! ships with Windows; there is no FFmpeg dependency.
//!
//! Recording is start/stop rather than one blocking call: `recordStart` returns
//! as soon as the first frame is encoded and hands the work to a background
//! thread, so `look` / `act` keep working while the file grows. The recording
//! stops itself at `maxDurationMs`, and a later `recordStop` still collects the
//! finished file.
//!
//! On non-Windows platforms every entry point returns `unsupported_platform`.

use serde_json::{json, Value};

use crate::error::{ErrorCode, ProtocolError};
use crate::refs::{RefStore, WindowRef};

#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::time::Duration;

/// Frames per second when the caller does not ask for a rate.
pub const DEFAULT_FPS: u32 = 10;
/// Fastest capture rate we honour.
pub const MAX_FPS: u32 = 30;
/// Recording length when the caller does not ask for one.
pub const DEFAULT_MAX_DURATION_MS: u64 = 60_000;
/// Longest recording we honour, so a forgotten stop cannot fill the disk.
pub const MAX_MAX_DURATION_MS: u64 = 10 * 60_000;
/// Smallest window cap we honour (below this nothing is readable anyway).
pub const MIN_MAX_DIMENSION: u32 = 64;
/// Largest window cap we honour; above this the cap never binds.
pub const MAX_MAX_DIMENSION: u32 = 8192;

/// MP4 audio track: what, if anything, is mixed in with the video.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioMode {
    None,
    /// What the machine is playing (WASAPI loopback on the render endpoint).
    System,
    /// The default recording device (microphone, line in, ...).
    Mic,
}

impl AudioMode {
    fn parse(args: &Value) -> Result<Self, ProtocolError> {
        match args.get("audio").and_then(Value::as_str) {
            None | Some("system") => Ok(AudioMode::System),
            Some("mic") => Ok(AudioMode::Mic),
            Some("none") => Ok(AudioMode::None),
            Some(other) => Err(invalid(format!(
                "audio must be system, mic, or none (got '{other}')"
            ))),
        }
    }

    fn label(self) -> &'static str {
        match self {
            AudioMode::None => "none",
            AudioMode::System => "system",
            AudioMode::Mic => "mic",
        }
    }
}

fn invalid(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(message, ErrorCode::InvalidRequest)
}

/// Parse the caller's cap on the longest side of the video.
///
/// A missing field and `maxDimension: 0` both mean "no cap", so callers can
/// pass the field unconditionally; anything else is clamped into the range we
/// are willing to encode.
fn parse_max_dimension(args: &Value) -> Option<u32> {
    match args.get("maxDimension").and_then(Value::as_u64) {
        None | Some(0) => None,
        Some(value) => Some(value.clamp(
            MIN_MAX_DIMENSION as u64,
            MAX_MAX_DIMENSION as u64,
        ) as u32),
    }
}

#[cfg(not(windows))]
fn unsupported() -> ProtocolError {
    ProtocolError::new(
        "Window recording is only supported on Windows",
        ErrorCode::UnsupportedPlatform,
    )
}

/// Start recording one window to an MP4 on the host that runs the helper.
///
/// Returns once the first frame has been encoded, so an unavailable encoder or
/// an uncapturable window is reported to the caller instead of failing
/// silently in the background.
pub fn start(store: &RefStore, target: &WindowRef, args: &Value) -> Result<Value, ProtocolError> {
    #[cfg(not(windows))]
    {
        let _ = (store, target, args);
        Err(unsupported())
    }

    #[cfg(windows)]
    {
        windows_impl::start(store, target, args)
    }
}

/// Stop the recording and report what landed on disk.
pub fn stop(args: &Value) -> Result<Value, ProtocolError> {
    #[cfg(not(windows))]
    {
        let _ = args;
        Err(unsupported())
    }

    #[cfg(windows)]
    {
        windows_impl::stop(args)
    }
}

/// Report on the active (or last finished) recording without stopping it.
pub fn status(args: &Value) -> Result<Value, ProtocolError> {
    #[cfg(not(windows))]
    {
        let _ = args;
        Err(unsupported())
    }

    #[cfg(windows)]
    {
        windows_impl::status(args)
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
//
// These carry no Windows types, so they are unit-tested on every platform.
// ---------------------------------------------------------------------------

/// Roughly 0.1 bits per pixel per frame: enough for readable text in a UI
/// recording without turning ten seconds into tens of megabytes.
fn video_bitrate(width: u32, height: u32, fps: u32) -> u32 {
    let pixels = width as u64 * height as u64;
    let bitrate = pixels * fps as u64 / 10;
    bitrate.clamp(500_000, 20_000_000) as u32
}

/// Bytes in one NV12 frame: a full luma plane plus a half-height chroma plane.
fn nv12_len(width: u32, height: u32) -> usize {
    (width as usize * height as usize * 3) / 2
}

/// Round a captured frame down to the even dimensions an NV12 video stream and
/// the H.264 encoder require.
fn even_dimensions(width: u32, height: u32) -> (u32, u32) {
    (width & !1, height & !1)
}

/// Fit a window into a `max_dimension` box, keeping its aspect ratio.
///
/// The answer is even (the encoder needs that), never larger than the source
/// (enlarging costs bitrate and adds nothing), and never larger than the box on
/// its longer side. A source that already fits comes back unchanged, so a cap
/// the caller passes for safety does not touch an ordinary recording.
fn scaled_size(src_w: u32, src_h: u32, max_dimension: u32) -> (u32, u32) {
    let longest = src_w.max(src_h);
    if max_dimension == 0 || longest <= max_dimension {
        return even_dimensions(src_w, src_h);
    }
    let width = (src_w as u64 * max_dimension as u64 / longest as u64) as u32;
    let height = (src_h as u64 * max_dimension as u64 / longest as u64) as u32;
    even_dimensions(width.max(2), height.max(2))
}

/// Box-filter a BGRA frame down to `dst_w` x `dst_h`.
///
/// Every destination pixel averages the source pixels it covers, which is exact
/// when the ratio is an integer and well behaved when it is not. Only shrinking
/// is supported: callers size the destination with `scaled_size`, which never
/// returns something larger than the source.
fn downscale_bgra(src: &[u8], src_w: u32, src_h: u32, dst: &mut [u8], dst_w: u32, dst_h: u32) {
    debug_assert!(dst_w > 0 && dst_h > 0);
    debug_assert!(dst_w <= src_w && dst_h <= src_h);
    debug_assert!(dst.len() >= dst_w as usize * dst_h as usize * 4);

    // GDI hands us B, G, R, A in that order; a short read counts as black.
    let sample = |x: u32, y: u32| -> (u32, u32, u32, u32) {
        let offset = ((y as usize * src_w as usize) + x as usize) * 4;
        if offset + 3 >= src.len() {
            return (0, 0, 0, 0);
        }
        (
            src[offset] as u32,
            src[offset + 1] as u32,
            src[offset + 2] as u32,
            src[offset + 3] as u32,
        )
    };

    for dy in 0..dst_h {
        let y0 = dy * src_h / dst_h;
        let y1 = (((dy + 1) * src_h + dst_h - 1) / dst_h)
            .max(y0 + 1)
            .min(src_h);
        for dx in 0..dst_w {
            let x0 = dx * src_w / dst_w;
            let x1 = (((dx + 1) * src_w + dst_w - 1) / dst_w)
                .max(x0 + 1)
                .min(src_w);
            let (mut b, mut g, mut r, mut a, mut count) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for y in y0..y1 {
                for x in x0..x1 {
                    let (sb, sg, sr, sa) = sample(x, y);
                    b += sb;
                    g += sg;
                    r += sr;
                    a += sa;
                    count += 1;
                }
            }
            let count = count.max(1);
            let offset = ((dy as usize * dst_w as usize) + dx as usize) * 4;
            if offset + 3 >= dst.len() {
                continue;
            }
            dst[offset] = (b / count) as u8;
            dst[offset + 1] = (g / count) as u8;
            dst[offset + 2] = (r / count) as u8;
            dst[offset + 3] = (a / count) as u8;
        }
    }
}

/// Where to start reading the source so the destination frame sits in the
/// middle of it (used both to crop a window that grew and to pad one that
/// shrank).
fn center_offsets(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (i32, i32) {
    (
        (src_w as i32 - dst_w as i32) / 2,
        (src_h as i32 - dst_h as i32) / 2,
    )
}

/// Convert top-down BGRA into NV12 (BT.709, studio range).
///
/// `src_x`/`src_y` may be negative: the parts of the destination outside the
/// source are filled with black (luma 16, chroma 128), which is what a window
/// resized mid-recording needs.
#[allow(clippy::too_many_arguments)]
fn bgra_to_nv12(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    src_x: i32,
    src_y: i32,
    dst_w: u32,
    dst_h: u32,
    dst: &mut [u8],
) {
    let y_len = (dst_w * dst_h) as usize;
    debug_assert_eq!(dst.len(), nv12_len(dst_w, dst_h));
    // The chroma plane is written two pixels at a time, so an odd dimension
    // would run off the end of the buffer. Every caller goes through
    // even_dimensions() first.
    debug_assert!(dst_w % 2 == 0 && dst_h % 2 == 0);

    let sample = |x: i32, y: i32| -> (i32, i32, i32) {
        let sx = x + src_x;
        let sy = y + src_y;
        if sx < 0 || sy < 0 || sx >= src_w as i32 || sy >= src_h as i32 {
            return (0, 0, 0);
        }
        let offset = (sy as usize * src_w as usize + sx as usize) * 4;
        if offset + 3 >= src.len() {
            return (0, 0, 0);
        }
        // GDI hands us B, G, R, A in that order.
        (
            src[offset + 2] as i32,
            src[offset + 1] as i32,
            src[offset] as i32,
        )
    };

    for y in 0..dst_h as i32 {
        for x in 0..dst_w as i32 {
            let (r, g, b) = sample(x, y);
            dst[y as usize * dst_w as usize + x as usize] = luma(r, g, b);
        }
    }

    let chroma = &mut dst[y_len..];
    for y in (0..dst_h as i32).step_by(2) {
        for x in (0..dst_w as i32).step_by(2) {
            let mut r = 0;
            let mut g = 0;
            let mut b = 0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let (pr, pg, pb) = sample(x + dx, y + dy);
                    r += pr;
                    g += pg;
                    b += pb;
                }
            }
            let (u, v) = chroma_for(r / 4, g / 4, b / 4);
            let row = (y / 2) as usize;
            let column = (x / 2) as usize * 2;
            chroma[row * dst_w as usize + column] = u;
            chroma[row * dst_w as usize + column + 1] = v;
        }
    }
}

/// BT.709 studio-range luma.
#[inline]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    clamp_u8(((47 * r + 157 * g + 16 * b) >> 8) + 16)
}

/// BT.709 studio-range chroma (U, V).
///
/// Both coefficient sets sum to zero on purpose: a grey pixel then lands on
/// exactly 128, which is what the encoder's neutral chroma is. An off-by-one
/// set (the classic `-26 -87 +112`) tints every grey by a bit.
#[inline]
fn chroma_for(r: i32, g: i32, b: i32) -> (u8, u8) {
    let u = ((-26 * r - 86 * g + 112 * b) >> 8) + 128;
    let v = ((112 * r - 102 * g - 10 * b) >> 8) + 128;
    (clamp_u8(u), clamp_u8(v))
}

#[inline]
fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::{mpsc, Arc, Mutex, OnceLock};
    use std::thread::JoinHandle;
    use std::time::Instant;

    /// How long `recordStart` waits for the first encoded frame before it
    /// answers anyway.
    const START_TIMEOUT: Duration = Duration::from_secs(20);
    /// How long `recordStop` waits for the recording thread to finalize.
    const STOP_TIMEOUT: Duration = Duration::from_secs(30);

    fn internal(message: impl Into<String>) -> ProtocolError {
        ProtocolError::new(message, ErrorCode::InternalError)
    }

    /// Counters a running (or finished) recording publishes to `recordStatus`
    /// and `recordStop`. Plain numbers, so the recording thread never blocks on
    /// the registry lock.
    #[derive(Default)]
    struct RecordStats {
        video_frames: AtomicU64,
        audio_frames: AtomicU64,
        width: AtomicU32,
        height: AtomicU32,
        /// First fatal problem seen by the recording thread, if any.
        error: Mutex<Option<String>>,
        /// Non-fatal problem with the audio track (no device, a format we
        /// cannot convert, ...). The recording still produces a video file.
        audio_warning: Mutex<Option<String>>,
    }

    /// One recording the helper knows about: running until `stop` is set, then
    /// finished-but-uncollected until the next `recordStart` replaces it.
    struct Recording {
        id: String,
        path: PathBuf,
        fps: u32,
        audio: AudioMode,
        stats: Arc<RecordStats>,
        stop: Arc<Mutex<bool>>,
        started: Instant,
        handle: Option<JoinHandle<()>>,
        finished: bool,
    }

    fn registry() -> &'static Mutex<Option<Recording>> {
        static ACTIVE: OnceLock<Mutex<Option<Recording>>> = OnceLock::new();
        ACTIVE.get_or_init(|| Mutex::new(None))
    }

    /// Everything one recording run needs, parsed once at `recordStart` time.
    #[derive(Clone)]
    struct RecordParams {
        /// Window handle, already resolved from the caller's root ref.
        hwnd: isize,
        fps: u32,
        max_duration: Duration,
        audio: AudioMode,
        path: PathBuf,
        /// Longest side of the encoded video, when the caller capped it.
        max_dimension: Option<u32>,
    }

    pub(super) fn start(
        store: &RefStore,
        target: &WindowRef,
        args: &Value,
    ) -> Result<Value, ProtocolError> {
        let native = store.get_window(target).ok_or_else(|| {
            ProtocolError::new(
                format!("Window ref '{target}' not found"),
                ErrorCode::TargetNotFound,
            )
        })?;

        let fps = args
            .get("fps")
            .and_then(Value::as_u64)
            .map(|value| value.clamp(1, MAX_FPS as u64) as u32)
            .unwrap_or(DEFAULT_FPS);
        let max_duration_ms = args
            .get("maxDurationMs")
            .and_then(Value::as_u64)
            .map(|value| value.clamp(1_000, MAX_MAX_DURATION_MS))
            .unwrap_or(DEFAULT_MAX_DURATION_MS);
        let audio = AudioMode::parse(args)?;
        let max_dimension = parse_max_dimension(args);

        let mut guard = registry()
            .lock()
            .map_err(|_| internal("recording registry lock poisoned"))?;
        if let Some(active) = guard.as_ref() {
            if !active.finished {
                return Err(invalid(format!(
                    "recording '{}' is still running; stop it first",
                    active.id
                )));
            }
        }

        let id = crate::state::StateId::fresh("rec");
        let path = match args.get("outputPath").and_then(Value::as_str) {
            Some(raw) if !raw.trim().is_empty() => resolve_output_path(raw.trim())?,
            _ => std::env::temp_dir().join(format!("aimux-{id}.mp4")),
        };

        let params = RecordParams {
            hwnd: native.raw(),
            fps,
            max_duration: Duration::from_millis(max_duration_ms),
            audio,
            path,
            max_dimension,
        };
        let stats = Arc::new(RecordStats::default());
        let stop = Arc::new(Mutex::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

        let thread_params = params.clone();
        let thread_stats = Arc::clone(&stats);
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("aimux-record".to_owned())
            .spawn(move || record_thread(thread_params, thread_stop, thread_stats, ready_tx))
            .map_err(|e| internal(format!("could not start the recording thread: {e}")))?;

        let started = match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => Instant::now(),
            Ok(Err(message)) => {
                let _ = handle.join();
                return Err(ProtocolError::new(message, ErrorCode::CaptureFailed));
            }
            Err(_) => {
                // Still starting up. Leave it recording rather than killing a
                // run the caller may still collect with recordStop.
                Instant::now()
            }
        };

        let response = json!({
            "recordingId": id,
            "target": target.to_string(),
            "path": params.path.to_string_lossy(),
            "width": stats.width.load(Ordering::Relaxed),
            "height": stats.height.load(Ordering::Relaxed),
            "fps": fps,
            "maxDurationMs": max_duration_ms,
            "maxDimension": max_dimension,
            "audio": audio.label(),
            "hasAudio": audio != AudioMode::None,
            "audioWarning": stats.audio_warning.lock().ok().and_then(|w| w.clone()),
            "startedAtMs": now_unix_ms(),
        });

        *guard = Some(Recording {
            id,
            path: params.path,
            fps,
            audio,
            stats,
            stop,
            started,
            handle: Some(handle),
            finished: false,
        });
        Ok(response)
    }

    pub(super) fn stop(args: &Value) -> Result<Value, ProtocolError> {
        let requested = args.get("recordingId").and_then(Value::as_str);
        let timeout = Duration::from_millis(
            args.get("timeoutMs")
                .and_then(Value::as_u64)
                .map(|value| value.clamp(1_000, 120_000))
                .unwrap_or(STOP_TIMEOUT.as_millis() as u64),
        );

        let mut guard = registry()
            .lock()
            .map_err(|_| internal("recording registry lock poisoned"))?;
        let recording = guard
            .as_mut()
            .ok_or_else(|| invalid("no recording has been started"))?;
        if let Some(requested) = requested {
            if requested != recording.id {
                return Err(invalid(format!(
                    "recording '{requested}' is not the active recording ('{}')",
                    recording.id
                )));
            }
        }

        if let Ok(mut flag) = recording.stop.lock() {
            *flag = true;
        }

        if !recording.finished {
            if let Some(handle) = recording.handle.take() {
                let deadline = Instant::now() + timeout;
                while !handle.is_finished() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                if !handle.is_finished() {
                    // Put the handle back: the caller can try again, and the
                    // file keeps growing until the thread decides to stop.
                    recording.handle = Some(handle);
                    return Err(ProtocolError::new(
                        "the recording thread did not finish in time; try recordStop again",
                        ErrorCode::InternalError,
                    ));
                }
                let _ = handle.join();
            }
            recording.finished = true;
        }

        Ok(summary(recording))
    }

    pub(super) fn status(args: &Value) -> Result<Value, ProtocolError> {
        let requested = args.get("recordingId").and_then(Value::as_str);
        let guard = registry()
            .lock()
            .map_err(|_| internal("recording registry lock poisoned"))?;
        let Some(recording) = guard.as_ref() else {
            return Ok(json!({ "active": false }));
        };
        if let Some(requested) = requested {
            if requested != recording.id {
                return Ok(json!({ "active": false, "recordingId": recording.id }));
            }
        }
        Ok(json!({
            "active": !recording.finished,
            "recordingId": recording.id,
            "path": recording.path.to_string_lossy(),
            "elapsedMs": recording.started.elapsed().as_millis() as u64,
            "width": recording.stats.width.load(Ordering::Relaxed),
            "height": recording.stats.height.load(Ordering::Relaxed),
            "fps": recording.fps,
            "videoFrames": recording.stats.video_frames.load(Ordering::Relaxed),
            "audioFrames": recording.stats.audio_frames.load(Ordering::Relaxed),
            "audio": recording.audio.label(),
            "audioWarning": recording.stats.audio_warning.lock().ok().and_then(|w| w.clone()),
            "error": recording.stats.error.lock().ok().and_then(|e| e.clone()),
            "finished": recording.finished,
        }))
    }

    fn summary(recording: &Recording) -> Value {
        let bytes = std::fs::metadata(&recording.path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        json!({
            "recordingId": recording.id,
            "path": recording.path.to_string_lossy(),
            "bytes": bytes,
            "durationMs": recording.started.elapsed().as_millis() as u64,
            "width": recording.stats.width.load(Ordering::Relaxed),
            "height": recording.stats.height.load(Ordering::Relaxed),
            "fps": recording.fps,
            "videoFrames": recording.stats.video_frames.load(Ordering::Relaxed),
            "audioFrames": recording.stats.audio_frames.load(Ordering::Relaxed),
            "audio": recording.audio.label(),
            "hasAudio": recording.audio != AudioMode::None,
            "audioWarning": recording.stats.audio_warning.lock().ok().and_then(|w| w.clone()),
            "error": recording.stats.error.lock().ok().and_then(|e| e.clone()),
            "stopped": true,
        })
    }

    fn now_unix_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Resolve the caller's output path: absolute, `.mp4`, parent directory
    /// made on demand. A relative path is refused rather than guessed at,
    /// because the helper's working directory is not the user's.
    fn resolve_output_path(raw: &str) -> Result<PathBuf, ProtocolError> {
        let mut path = PathBuf::from(raw);
        if !path.is_absolute() {
            return Err(invalid(format!("outputPath must be absolute (got '{raw}')")));
        }
        if path.extension().is_none() {
            path.set_extension("mp4");
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ProtocolError::new(
                    format!("could not create {}: {e}", parent.display()),
                    ErrorCode::InvalidRequest,
                )
            })?;
        }
        Ok(path)
    }

    /// Capture frames until asked to stop, mixing in audio when it is available.
    ///
    /// The thread owns the sink writer from creation to finalize, so the MP4 is
    /// written by exactly one thread and needs no locking.
    fn record_thread(
        params: RecordParams,
        stop: Arc<Mutex<bool>>,
        stats: Arc<RecordStats>,
        ready: mpsc::Sender<Result<(), String>>,
    ) {
        // A recording thread does its own COM/MF setup: the helper's request
        // workers have their own apartment and must not be disturbed.
        let com = unsafe { com::CoInitializeEx(None, com::COINIT_MULTITHREADED) };
        let started = Instant::now();

        let outcome = run_recording(&params, &stop, &stats, &ready, started);
        if let Err(message) = outcome {
            if let Ok(mut slot) = stats.error.lock() {
                if slot.is_none() {
                    *slot = Some(message.clone());
                }
            }
            // Before the first frame nothing has been handed to the caller, so
            // the failure is reported through `recordStart` instead.
            let _ = ready.send(Err(message));
        }

        let _ = unsafe { mf::MFShutdown() };
        if com.is_ok() {
            unsafe { com::CoUninitialize() };
        }
    }

    fn run_recording(
        params: &RecordParams,
        stop: &Arc<Mutex<bool>>,
        stats: &Arc<RecordStats>,
        ready: &mpsc::Sender<Result<(), String>>,
        started: Instant,
    ) -> Result<(), String> {
        unsafe { mf::MFStartup(mf::MF_VERSION, mf::MFSTARTUP_FULL) }
            .map_err(|e| format!("Media Foundation is unavailable on this system: {e}"))?;

        let hwnd = windows::Win32::Foundation::HWND(params.hwnd as *mut _);

        let first = crate::capture::capture_bgra(hwnd).map_err(|e| e.message)?;
        // A capped recording keeps the encoder's frame size fixed from here on;
        // later frames are downscaled into it, and a window that changes size
        // mid-recording is still centred inside it.
        let (frame_w, frame_h) = match params.max_dimension {
            Some(limit) => scaled_size(first.width, first.height, limit),
            None => even_dimensions(first.width, first.height),
        };
        if frame_w == 0 || frame_h == 0 {
            return Err("the target window has no capturable area".to_owned());
        }
        // Reused across frames so a capped recording does not allocate per tick.
        let mut scratch: Vec<u8> = Vec::new();

        // Audio is a nice-to-have: a device that refuses to open must not cost
        // the caller the video, so degrade to video-only and say why.
        let mut audio_warning: Option<String> = None;
        let mut audio = match params.audio {
            AudioMode::None => None,
            mode => match AudioCapture::open(mode) {
                Ok(capture) => match capture.start() {
                    Ok(()) => Some(capture),
                    Err(reason) => {
                        audio_warning = Some(format!("audio capture unavailable: {reason}"));
                        None
                    }
                },
                Err(reason) => {
                    audio_warning = Some(format!("audio capture unavailable: {reason}"));
                    None
                }
            },
        };

        let mut writer = Encoder::create(&params.path, frame_w, frame_h, params.fps, audio.as_ref())
            .map_err(|e| format!("could not create the MP4 encoder: {e}"))?;

        stats.width.store(frame_w, Ordering::Relaxed);
        stats.height.store(frame_h, Ordering::Relaxed);
        if let Ok(mut slot) = stats.audio_warning.lock() {
            *slot = audio_warning;
        }
        // Everything past this point is collectable with `recordStop`.
        let _ = ready.send(Ok(()));

        let interval = Duration::from_nanos(1_000_000_000u64 / params.fps as u64);
        let mut next_frame = Instant::now();
        let mut consecutive_failures = 0u32;
        let mut pending = Some(first);

        loop {
            let should_stop = stop.lock().map(|flag| *flag).unwrap_or(true)
                || started.elapsed() >= params.max_duration;
            if should_stop {
                break;
            }

            let captured = match pending.take() {
                Some(frame) => Ok(frame),
                None => crate::capture::capture_bgra(hwnd),
            };
            match captured {
                Ok(frame) => {
                    consecutive_failures = 0;
                    let timestamp = started.elapsed().as_nanos() as i64 / 100;
                    let (pixels, src_w, src_h) = match params.max_dimension {
                        Some(limit) => {
                            let (target_w, target_h) =
                                scaled_size(frame.width, frame.height, limit);
                            // A window that collapsed to nothing (or a frame
                            // that already matches) goes in as captured.
                            if target_w > 0
                                && target_h > 0
                                && (target_w, target_h) != (frame.width, frame.height)
                            {
                                let needed = target_w as usize * target_h as usize * 4;
                                if scratch.len() != needed {
                                    scratch.resize(needed, 0);
                                }
                                downscale_bgra(
                                    &frame.pixels,
                                    frame.width,
                                    frame.height,
                                    &mut scratch,
                                    target_w,
                                    target_h,
                                );
                                (&scratch[..], target_w, target_h)
                            } else {
                                (&frame.pixels[..], frame.width, frame.height)
                            }
                        }
                        None => (&frame.pixels[..], frame.width, frame.height),
                    };
                    if let Err(message) = writer.write_video(
                        pixels,
                        src_w,
                        src_h,
                        frame_w,
                        frame_h,
                        timestamp,
                        params.fps,
                    ) {
                        // Encoder trouble is fatal: later frames cannot help.
                        return Err(format!("video encoding failed: {message}"));
                    }
                    stats.video_frames.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) => {
                    consecutive_failures += 1;
                    if consecutive_failures > params.fps * 5 {
                        // Five seconds without a frame: the window is gone.
                        if let Ok(mut slot) = stats.error.lock() {
                            if slot.is_none() {
                                *slot = Some(format!("window capture stopped: {}", error.message));
                            }
                        }
                        break;
                    }
                }
            }

            let mut audio_failed = false;
            if let Some(capture) = audio.as_mut() {
                match capture.drain(&mut writer, started) {
                    Ok(frames) => {
                        stats.audio_frames.fetch_add(frames, Ordering::Relaxed);
                    }
                    Err(reason) => {
                        if let Ok(mut slot) = stats.audio_warning.lock() {
                            *slot = Some(format!("audio capture stopped: {reason}"));
                        }
                        audio_failed = true;
                    }
                }
            }
            if audio_failed {
                audio = None;
            }

            next_frame += interval;
            let now = Instant::now();
            if next_frame > now {
                std::thread::sleep(next_frame - now);
            } else {
                next_frame = now;
            }
        }

        if let Some(capture) = audio.as_ref() {
            capture.stop();
        }
        // Top the audio track up to the video length so the file has sound for
        // its whole duration even if the last pull brought nothing back.
        if audio.is_some() {
            let elapsed_hns = started.elapsed().as_nanos() as i64 / 100;
            let _ = writer.write_audio(&[], elapsed_hns);
        }
        writer
            .finish()
            .map_err(|e| format!("finalizing the MP4 failed: {e}"))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Media Foundation encode path
    // -----------------------------------------------------------------------

    mod mf {
        pub use windows::Win32::Media::MediaFoundation::*;
    }

    mod com {
        pub use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    }

    mod wasapi {
        pub use windows::Win32::Media::Audio::*;
    }

    /// The MMDeviceEnumerator coclass. The `windows` crate does not ship this
    /// class id, so it is spelled out here.
    const CLSID_MM_DEVICE_ENUMERATOR: windows::core::GUID =
        windows::core::GUID::from_u128(0xbcde0395_e52f_467c_8e3d_c4579291692e);

    struct Encoder {
        writer: mf::IMFSinkWriter,
        video_stream: u32,
        audio_stream: Option<u32>,
        /// Scratch buffer, reused for every frame so a long recording does not
        /// churn allocations.
        nv12: Vec<u8>,
        sample_rate: u32,
        channels: u16,
        audio_written: u64,
    }

    impl Encoder {
        fn create(
            path: &std::path::Path,
            width: u32,
            height: u32,
            fps: u32,
            audio: Option<&AudioCapture>,
        ) -> windows::core::Result<Self> {
            let url = wide_path(path);
            let writer = unsafe {
                mf::MFCreateSinkWriterFromURL(
                    windows::core::PCWSTR(url.as_ptr()),
                    None::<&mf::IMFByteStream>,
                    None::<&mf::IMFAttributes>,
                )?
            };

            let video_out = unsafe { mf::MFCreateMediaType()? };
            set_guid(&video_out, &mf::MF_MT_MAJOR_TYPE, &mf::MFMediaType_Video)?;
            set_guid(&video_out, &mf::MF_MT_SUBTYPE, &mf::MFVideoFormat_H264)?;
            set_u32(&video_out, &mf::MF_MT_AVG_BITRATE, video_bitrate(width, height, fps))?;
            set_u32(
                &video_out,
                &mf::MF_MT_INTERLACE_MODE,
                mf::MFVideoInterlace_Progressive.0 as u32,
            )?;
            set_size(&video_out, &mf::MF_MT_FRAME_SIZE, width, height)?;
            set_ratio(&video_out, &mf::MF_MT_FRAME_RATE, fps, 1)?;
            set_ratio(&video_out, &mf::MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            let video_stream = unsafe { writer.AddStream(&video_out)? };

            let video_in = unsafe { mf::MFCreateMediaType()? };
            set_guid(&video_in, &mf::MF_MT_MAJOR_TYPE, &mf::MFMediaType_Video)?;
            set_guid(&video_in, &mf::MF_MT_SUBTYPE, &mf::MFVideoFormat_NV12)?;
            set_u32(
                &video_in,
                &mf::MF_MT_INTERLACE_MODE,
                mf::MFVideoInterlace_Progressive.0 as u32,
            )?;
            set_size(&video_in, &mf::MF_MT_FRAME_SIZE, width, height)?;
            set_ratio(&video_in, &mf::MF_MT_FRAME_RATE, fps, 1)?;
            set_ratio(&video_in, &mf::MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            // Top-down rows, one row of pixels per stride: the layout
            // `bgra_to_nv12` writes.
            set_u32(&video_in, &mf::MF_MT_DEFAULT_STRIDE, width)?;
            set_u32(&video_in, &mf::MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            unsafe { writer.SetInputMediaType(video_stream, &video_in, None::<&mf::IMFAttributes>)? };

            let mut audio_stream = None;
            let mut sample_rate = 0u32;
            let mut channels = 0u16;
            if let Some(capture) = audio {
                let out = unsafe { mf::MFCreateMediaType()? };
                set_guid(&out, &mf::MF_MT_MAJOR_TYPE, &mf::MFMediaType_Audio)?;
                set_guid(&out, &mf::MF_MT_SUBTYPE, &mf::MFAudioFormat_AAC)?;
                set_u32(&out, &mf::MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                set_u32(&out, &mf::MF_MT_AUDIO_SAMPLES_PER_SECOND, capture.sample_rate)?;
                set_u32(&out, &mf::MF_MT_AUDIO_NUM_CHANNELS, capture.out_channels as u32)?;
                // 128 kbps: plenty for screen-recording sound.
                set_u32(&out, &mf::MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000)?;
                let stream = unsafe { writer.AddStream(&out)? };

                let input = unsafe { mf::MFCreateMediaType()? };
                set_guid(&input, &mf::MF_MT_MAJOR_TYPE, &mf::MFMediaType_Audio)?;
                set_guid(&input, &mf::MF_MT_SUBTYPE, &mf::MFAudioFormat_PCM)?;
                set_u32(&input, &mf::MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                set_u32(&input, &mf::MF_MT_AUDIO_SAMPLES_PER_SECOND, capture.sample_rate)?;
                set_u32(&input, &mf::MF_MT_AUDIO_NUM_CHANNELS, capture.out_channels as u32)?;
                set_u32(
                    &input,
                    &mf::MF_MT_AUDIO_BLOCK_ALIGNMENT,
                    capture.out_channels as u32 * 2,
                )?;
                set_u32(
                    &input,
                    &mf::MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
                    capture.sample_rate * capture.out_channels as u32 * 2,
                )?;
                unsafe { writer.SetInputMediaType(stream, &input, None::<&mf::IMFAttributes>)? };

                audio_stream = Some(stream);
                sample_rate = capture.sample_rate;
                channels = capture.out_channels;
            }

            unsafe { writer.BeginWriting()? };

            Ok(Self {
                writer,
                video_stream,
                audio_stream,
                nv12: vec![0u8; nv12_len(width, height)],
                sample_rate,
                channels,
                audio_written: 0,
            })
        }

        /// Encode one captured frame. The frame may differ in size from the
        /// stream (the window was resized): it is centred, cropped or padded.
        /// Encode one frame of BGRA pixels into the (fixed size) video stream.
        ///
        /// `src_w`/`src_h` are the size of `pixels`, which may differ from the
        /// stream's `width`/`height`: a capped recording downscales the frame
        /// before calling in, and a window resized mid-recording keeps the
        /// stream size but gets centred inside it.
        fn write_video(
            &mut self,
            pixels: &[u8],
            src_w: u32,
            src_h: u32,
            width: u32,
            height: u32,
            timestamp_hns: i64,
            fps: u32,
        ) -> windows::core::Result<()> {
            let (crop_x, crop_y) = center_offsets(src_w, src_h, width, height);
            bgra_to_nv12(
                pixels,
                src_w,
                src_h,
                crop_x,
                crop_y,
                width,
                height,
                &mut self.nv12,
            );

            let buffer = unsafe { mf::MFCreateMemoryBuffer(self.nv12.len() as u32)? };
            unsafe {
                let mut target: *mut u8 = std::ptr::null_mut();
                buffer.Lock(&mut target, None, None)?;
                std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), target, self.nv12.len());
                buffer.Unlock()?;
                buffer.SetCurrentLength(self.nv12.len() as u32)?;
            }

            let sample = unsafe { mf::MFCreateSample()? };
            unsafe {
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(timestamp_hns)?;
                sample.SetSampleDuration(10_000_000i64 / fps.max(1) as i64)?;
                self.writer.WriteSample(self.video_stream, &sample)?;
            }
            Ok(())
        }

        /// Write one block of 16-bit PCM, padding silence up to the wall clock
        /// so the audio track stays aligned with the video.
        fn write_audio(&mut self, samples: &[i16], elapsed_hns: i64) -> windows::core::Result<()> {
            let Some(stream) = self.audio_stream else {
                return Ok(());
            };
            if self.sample_rate == 0 || self.channels == 0 {
                return Ok(());
            }
            let block = self.channels as usize;
            let target_samples =
                (elapsed_hns.max(0) as u128 * self.sample_rate as u128 / 10_000_000) as u64;

            let mut pcm: Vec<i16> = Vec::new();
            let captured = samples.len() as u64;
            if target_samples > self.audio_written + captured {
                let missing = (target_samples - self.audio_written - captured) as usize;
                pcm.resize(missing * block, 0);
            }
            pcm.extend_from_slice(samples);
            let frames = (pcm.len() / block) as u64;
            if frames == 0 {
                return Ok(());
            }

            let buffer = unsafe { mf::MFCreateMemoryBuffer((pcm.len() * 2) as u32)? };
            unsafe {
                let mut target: *mut u8 = std::ptr::null_mut();
                buffer.Lock(&mut target, None, None)?;
                std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, target, pcm.len() * 2);
                buffer.Unlock()?;
                buffer.SetCurrentLength((pcm.len() * 2) as u32)?;
            }

            let start_hns =
                (self.audio_written as i128 * 10_000_000i128 / self.sample_rate as i128) as i64;
            let duration_hns = (frames as i128 * 10_000_000i128 / self.sample_rate as i128) as i64;

            let sample = unsafe { mf::MFCreateSample()? };
            unsafe {
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(start_hns)?;
                sample.SetSampleDuration(duration_hns.max(1))?;
                self.writer.WriteSample(stream, &sample)?;
            }
            self.audio_written += frames;
            Ok(())
        }

        fn finish(&mut self) -> windows::core::Result<()> {
            unsafe { self.writer.Finalize() }
        }
    }

    fn set_guid(
        target: &mf::IMFMediaType,
        key: &windows::core::GUID,
        value: &windows::core::GUID,
    ) -> windows::core::Result<()> {
        // IMFMediaType derefs to IMFAttributes, which is where setters live.
        unsafe { target.SetGUID(key, value) }
    }

    fn set_u32(
        target: &mf::IMFMediaType,
        key: &windows::core::GUID,
        value: u32,
    ) -> windows::core::Result<()> {
        unsafe { target.SetUINT32(key, value) }
    }

    /// Two packed 32-bit halves, the encoding Media Foundation uses for sizes
    /// and ratios (`MFSetAttributeSize` does the same thing in C++).
    fn set_size(
        target: &mf::IMFMediaType,
        key: &windows::core::GUID,
        width: u32,
        height: u32,
    ) -> windows::core::Result<()> {
        unsafe { target.SetUINT64(key, ((width as u64) << 32) | height as u64) }
    }

    fn set_ratio(
        target: &mf::IMFMediaType,
        key: &windows::core::GUID,
        numerator: u32,
        denominator: u32,
    ) -> windows::core::Result<()> {
        unsafe { target.SetUINT64(key, ((numerator as u64) << 32) | denominator as u64) }
    }

    fn wide_path(path: &std::path::Path) -> Vec<u16> {
        path.as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }

    // -----------------------------------------------------------------------
    // WASAPI audio capture
    // -----------------------------------------------------------------------

    /// Sample rates the Media Foundation AAC encoder accepts.
    const AAC_SAMPLE_RATES: [u32; 9] = [
        8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000,
    ];

    /// The rate we ask WASAPI to convert to, and the rate we hand the encoder.
    const TARGET_SAMPLE_RATE: u32 = 48_000;

    struct AudioCapture {
        client: wasapi::IAudioClient,
        capture: wasapi::IAudioCaptureClient,
        /// Rate of the stream we actually opened.
        sample_rate: u32,
        /// Channels in each device frame (may exceed what we keep).
        source_channels: u16,
        /// Channels we hand the encoder: one or two.
        out_channels: u16,
        /// True when the device hands us 32-bit floats to convert ourselves.
        is_float: bool,
    }

    impl AudioCapture {
        fn open(mode: AudioMode) -> Result<Self, String> {
            unsafe {
                let enumerator: wasapi::IMMDeviceEnumerator =
                    windows::Win32::System::Com::CoCreateInstance(
                        &CLSID_MM_DEVICE_ENUMERATOR,
                        None::<&windows::core::IUnknown>,
                        windows::Win32::System::Com::CLSCTX_ALL,
                    )
                    .map_err(|e| format!("no audio endpoint enumerator: {e}"))?;

                let (data_flow, flags) = match mode {
                    // Loopback captures what the machine is playing instead of
                    // what a microphone hears.
                    AudioMode::System => (wasapi::eRender, wasapi::AUDCLNT_STREAMFLAGS_LOOPBACK),
                    _ => (wasapi::eCapture, 0),
                };
                let device = enumerator
                    .GetDefaultAudioEndpoint(data_flow, wasapi::eConsole)
                    .map_err(|e| format!("no default audio endpoint: {e}"))?;
                let client: wasapi::IAudioClient = device
                    .Activate(windows::Win32::System::Com::CLSCTX_ALL, None)
                    .map_err(|e| format!("audio endpoint refused activation: {e}"))?;

                let mix = client
                    .GetMixFormat()
                    .map_err(|e| format!("could not read the device format: {e}"))?;
                if mix.is_null() {
                    return Err("the device reported no mix format".to_owned());
                }
                let parsed = parse_mix_format(mix);

                // 500 ms of slack per pull, the usual shared-mode choice.
                let hns_buffer = 10_000_000i64 / 2;
                let out_channels = if parsed.channels >= 2 { 2u16 } else { 1u16 };
                let pcm = wasapi::WAVEFORMATEX {
                    wFormatTag: 1, // WAVE_FORMAT_PCM
                    nChannels: out_channels,
                    nSamplesPerSec: TARGET_SAMPLE_RATE,
                    nAvgBytesPerSec: TARGET_SAMPLE_RATE * out_channels as u32 * 2,
                    nBlockAlign: out_channels * 2,
                    wBitsPerSample: 16,
                    cbSize: 0,
                };
                let convert = wasapi::AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | wasapi::AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

                // Preferred: let the audio engine hand us 16-bit PCM at a rate
                // the AAC encoder takes.
                let opened = match client.Initialize(
                    wasapi::AUDCLNT_SHAREMODE_SHARED,
                    flags | convert,
                    hns_buffer,
                    0,
                    &pcm,
                    None,
                ) {
                    Ok(()) => Ok((TARGET_SAMPLE_RATE, out_channels, out_channels, false)),
                    Err(_) => {
                        // No converter for this endpoint (loopback capture
                        // refuses it on some drivers): take the device's own
                        // format and convert by hand instead.
                        if !AAC_SAMPLE_RATES.contains(&parsed.sample_rate) {
                            Err(format!(
                                "the device runs at {} Hz, which the AAC encoder does not take",
                                parsed.sample_rate
                            ))
                        } else {
                            client
                                .Initialize(
                                    wasapi::AUDCLNT_SHAREMODE_SHARED,
                                    flags,
                                    hns_buffer,
                                    0,
                                    mix,
                                    None,
                                )
                                .map_err(|e| format!("audio client refused to start: {e}"))?;
                            Ok((
                                parsed.sample_rate,
                                parsed.channels,
                                out_channels,
                                parsed.is_float,
                            ))
                        }
                    }
                };

                windows::Win32::System::Com::CoTaskMemFree(Some(mix as *const std::ffi::c_void));
                let (sample_rate, source_channels, out_channels, is_float) = opened?;

                let capture: wasapi::IAudioCaptureClient = client
                    .GetService()
                    .map_err(|e| format!("no capture service on the endpoint: {e}"))?;

                Ok(Self {
                    client,
                    capture,
                    sample_rate,
                    source_channels,
                    out_channels,
                    is_float,
                })
            }
        }

        fn start(&self) -> Result<(), String> {
            unsafe { self.client.Start() }.map_err(|e| e.to_string())
        }

        fn stop(&self) {
            unsafe {
                let _ = self.client.Stop();
            }
        }

        /// Pull whatever the device has buffered into the encoder. Returns how
        /// many device frames were written.
        fn drain(&mut self, encoder: &mut Encoder, started: Instant) -> Result<u64, String> {
            let mut total = 0u64;
            loop {
                let packet = match unsafe { self.capture.GetNextPacketSize() } {
                    Ok(size) => size,
                    Err(e) => return Err(e.to_string()),
                };
                if packet == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if let Err(e) = unsafe {
                    self.capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                } {
                    return Err(e.to_string());
                }
                if frames == 0 {
                    let _ = unsafe { self.capture.ReleaseBuffer(0) };
                    continue;
                }
                let silent = flags & wasapi::AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                // SAFETY: `GetBuffer` just handed us `frames` frames of the
                // format the client was initialized with.
                let samples = unsafe { self.convert(data, frames, silent) };
                let _ = unsafe { self.capture.ReleaseBuffer(frames) };

                let elapsed_hns = started.elapsed().as_nanos() as i64 / 100;
                if let Err(e) = encoder.write_audio(&samples, elapsed_hns) {
                    return Err(e.to_string());
                }
                total += frames as u64;
            }
            Ok(total)
        }

        /// Device samples → interleaved 16-bit stereo.
        ///
        /// # Safety
        ///
        /// `data` must point at `frames` frames in the format the client was
        /// initialized with, which is what `GetBuffer` guarantees.
        unsafe fn convert(&self, data: *mut u8, frames: u32, silent: bool) -> Vec<i16> {
            let source_channels = self.source_channels as usize;
            let out_channels = self.out_channels as usize;
            let mut out = vec![0i16; frames as usize * out_channels];
            if silent || data.is_null() {
                return out;
            }
            for frame in 0..frames as usize {
                for channel in 0..out_channels {
                    // More device channels than we keep: take the first two.
                    let index = frame * source_channels + channel;
                    out[frame * out_channels + channel] = if self.is_float {
                        let value = *(data as *const f32).add(index);
                        (value.clamp(-1.0, 1.0) * 32_767.0) as i16
                    } else {
                        *(data as *const i16).add(index)
                    };
                }
            }
            out
        }
    }

    struct ParsedFormat {
        sample_rate: u32,
        channels: u16,
        is_float: bool,
    }

    /// Read a `WAVEFORMATEX` — or the `WAVEFORMATEXTENSIBLE` that shared-mode
    /// endpoints normally hand out — into the few fields we care about.
    ///
    /// Both structs are `repr(packed)`, so every field is copied out by value
    /// and no reference into the packed struct is ever taken.
    fn parse_mix_format(format: *const wasapi::WAVEFORMATEX) -> ParsedFormat {
        const WAVE_FORMAT_PCM: u16 = 1;
        const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
        const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
        const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows::core::GUID =
            windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

        let base = unsafe { *format };
        let tag = base.wFormatTag;
        let mut is_float = tag == WAVE_FORMAT_IEEE_FLOAT;
        if tag == WAVE_FORMAT_EXTENSIBLE && base.cbSize >= 22 {
            let extensible = unsafe { *(format as *const wasapi::WAVEFORMATEXTENSIBLE) };
            let sub_format = extensible.SubFormat;
            is_float = sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
        } else if tag != WAVE_FORMAT_PCM && tag != WAVE_FORMAT_IEEE_FLOAT {
            // Anything else (a compressed or extensible-without-subformat
            // device) is handled by the audio engine's converter instead.
            is_float = false;
        }
        ParsedFormat {
            sample_rate: base.nSamplesPerSec,
            channels: base.nChannels.max(1),
            is_float,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn nv12_black_frame_is_studio_black_and_neutral_chroma() {
        let src = vec![0u8; 4 * 2 * 2];
        let mut dst = vec![0u8; nv12_len(2, 2)];
        bgra_to_nv12(&src, 2, 2, 0, 0, 2, 2, &mut dst);
        assert_eq!(&dst[..4], &[16, 16, 16, 16]);
        assert_eq!(&dst[4..], &[128, 128]);
    }

    #[test]
    fn nv12_white_frame_reaches_studio_white() {
        let mut src = vec![0u8; 4 * 2 * 2];
        for pixel in src.chunks_exact_mut(4) {
            pixel[0] = 255;
            pixel[1] = 255;
            pixel[2] = 255;
        }
        let mut dst = vec![0u8; nv12_len(2, 2)];
        bgra_to_nv12(&src, 2, 2, 0, 0, 2, 2, &mut dst);
        assert_eq!(dst[0], 235);
        assert_eq!(&dst[4..], &[128, 128]);
    }

    #[test]
    fn nv12_reads_blue_and_red_from_bgra_order() {
        // Top row: one blue pixel then one red pixel, both with G = 0. GDI
        // hands us BGRA, so the blue channel is the first byte of a pixel and
        // the red channel is the third.
        let mut src = vec![0u8; 4 * 2 * 2];
        src[0] = 255; // pixel (0, 0), B
        src[6] = 255; // pixel (1, 0), R
        let mut dst = vec![0u8; nv12_len(2, 2)];
        bgra_to_nv12(&src, 2, 2, 0, 0, 2, 2, &mut dst);
        assert_eq!(dst[0], 31, "blue carries little luma");
        assert_eq!(dst[1], 62, "red carries more");
    }

    #[test]
    fn nv12_pads_with_black_when_the_destination_is_larger() {
        let mut src = vec![0u8; 4 * 2 * 2];
        for pixel in src.chunks_exact_mut(4) {
            pixel[2] = 255; // red everywhere
        }
        let mut dst = vec![0u8; nv12_len(4, 2)];
        // The 2x2 source sits centred in a 4x2 frame, which is what
        // center_offsets reports for a window that shrank: columns 0 and 3 pad.
        assert_eq!(center_offsets(2, 2, 4, 2), (-1, 0));
        bgra_to_nv12(&src, 2, 2, -1, 0, 4, 2, &mut dst);
        assert_eq!(dst[0], 16, "left padding column should be black");
        assert_eq!(dst[3], 16, "right padding column should be black");
        assert_eq!(dst[1], 62, "the red source column keeps its luma");
    }

    #[test]
    fn nv12_crops_from_a_larger_source() {
        let mut src = vec![0u8; 4 * 2 * 4]; // 4x2: left half red, right half blue
        for (index, pixel) in src.chunks_exact_mut(4).enumerate() {
            if index % 4 < 2 {
                pixel[2] = 255;
            } else {
                pixel[0] = 255;
            }
        }
        let mut dst = vec![0u8; nv12_len(2, 2)];
        // A window that grew: the 2x2 destination covers source columns 1..3,
        // so the last red column and the first blue column survive.
        assert_eq!(center_offsets(4, 2, 2, 2), (1, 0));
        bgra_to_nv12(&src, 4, 2, 1, 0, 2, 2, &mut dst);
        assert_eq!(dst[0], 62, "the left column of the crop is still red");
        assert_eq!(dst[1], 31, "the right column of the crop is already blue");
    }

    #[test]
    fn video_bitrate_stays_inside_the_agreed_band() {
        assert_eq!(video_bitrate(1280, 720, 10), 921_600);
        assert_eq!(video_bitrate(320, 200, 5), 500_000);
        assert_eq!(video_bitrate(3840, 2160, 30), 20_000_000);
    }

    #[test]
    fn frames_are_rounded_down_to_even_dimensions() {
        assert_eq!(even_dimensions(1280, 721), (1280, 720));
        assert_eq!(even_dimensions(1, 1), (0, 0));
    }

    #[test]
    fn a_cap_keeps_the_aspect_ratio_and_only_shrinks() {
        // 3096x1944 capped to 1280 lands on 1280x803, which the encoder needs even.
        assert_eq!(scaled_size(3096, 1944, 1280), (1280, 802));
        // A source that already fits, and a disabled cap, both come back as-is.
        assert_eq!(scaled_size(1920, 1080, 4096), (1920, 1080));
        assert_eq!(scaled_size(1920, 1080, 0), (1920, 1080));
        // Odd sides are still rounded for the encoder.
        assert_eq!(scaled_size(1281, 721, 4096), (1280, 720));
        assert_eq!(scaled_size(65, 65, 64), (64, 64));
        // A tall window is capped on its longest side too, not on the width.
        assert_eq!(scaled_size(1000, 4000, 1000), (250, 1000));
        // A tiny source cannot collapse to zero.
        assert_eq!(scaled_size(4, 2, 2), (2, 2));
    }

    #[test]
    fn a_cap_never_reaches_a_recording_that_already_fits() {
        for (w, h) in [(1280, 720), (640, 480), (1920, 1080), (3840, 2160)] {
            assert_eq!(scaled_size(w, h, 4096), even_dimensions(w, h));
        }
    }

    #[test]
    fn downscaling_to_the_same_size_copies_the_pixels() {
        let mut src = vec![0u8; 2 * 2 * 4];
        for (index, pixel) in src.chunks_exact_mut(4).enumerate() {
            pixel[0] = index as u8; // B
            pixel[1] = 100 + index as u8; // G
            pixel[2] = 200 - index as u8; // R
            pixel[3] = 255;
        }
        let mut dst = vec![0u8; 2 * 2 * 4];
        downscale_bgra(&src, 2, 2, &mut dst, 2, 2);
        assert_eq!(dst, src);
    }

    #[test]
    fn downscaling_averages_the_source_pixels_it_covers() {
        // 4x2: columns 0..2 red, columns 2..4 blue. Halving the width puts one
        // source colour in each destination column, so the average is exact.
        let mut src = vec![0u8; 4 * 2 * 4];
        for (index, pixel) in src.chunks_exact_mut(4).enumerate() {
            if index % 4 < 2 {
                pixel[2] = 255;
            } else {
                pixel[0] = 255;
            }
        }
        let mut dst = vec![0u8; 2 * 2 * 4];
        downscale_bgra(&src, 4, 2, &mut dst, 2, 2);
        for row in dst.chunks_exact(2 * 4) {
            assert_eq!(&row[0..4], &[0, 0, 255, 0], "left column stays red");
            assert_eq!(&row[4..8], &[255, 0, 0, 0], "right column stays blue");
        }
    }

    #[test]
    fn downscaling_a_uniform_frame_changes_nothing() {
        // An 8x8 field shrunk to 3x3 has ragged box edges; a uniform colour is
        // the case where those edges must not leak black.
        let src = vec![7u8; 8 * 8 * 4];
        let mut dst = vec![0u8; 3 * 3 * 4];
        downscale_bgra(&src, 8, 8, &mut dst, 3, 3);
        assert!(dst.iter().all(|&byte| byte == 7));
    }

    #[test]
    fn a_short_source_buffer_reads_as_black() {
        // GDI can hand back fewer bytes than the window claims; the missing
        // pixels must average in as black rather than panic. Only row 0 of this
        // 4x2 frame is present, so the destination's second row is all black.
        let src = vec![255u8; 4 * 4];
        let mut dst = vec![9u8; 2 * 2 * 4];
        downscale_bgra(&src, 4, 2, &mut dst, 2, 2);
        assert_eq!(&dst[0..4], &[255, 255, 255, 255], "the present row wins");
        assert_eq!(&dst[4..8], &[255, 255, 255, 255], "and so does its neighbour");
        assert_eq!(&dst[8..12], &[0, 0, 0, 0], "the missing row is black");
        assert_eq!(&dst[12..16], &[0, 0, 0, 0]);
    }

    #[test]
    fn a_window_cap_is_optional_and_clamped() {
        assert_eq!(parse_max_dimension(&json!({})), None);
        assert_eq!(parse_max_dimension(&json!({"maxDimension": 0})), None);
        assert_eq!(parse_max_dimension(&json!({"maxDimension": 1280})), Some(1280));
        assert_eq!(parse_max_dimension(&json!({"maxDimension": 1})), Some(64));
        assert_eq!(
            parse_max_dimension(&json!({"maxDimension": 99999})),
            Some(8192)
        );
    }

    #[test]
    fn center_offsets_pad_and_crop_symmetrically() {
        assert_eq!(center_offsets(100, 50, 100, 50), (0, 0));
        assert_eq!(center_offsets(200, 100, 100, 50), (50, 25));
        assert_eq!(center_offsets(100, 50, 200, 100), (-50, -25));
    }

    #[test]
    fn audio_mode_defaults_to_system_sound() {
        assert_eq!(AudioMode::parse(&json!({})).unwrap(), AudioMode::System);
        assert_eq!(
            AudioMode::parse(&json!({"audio": "mic"})).unwrap(),
            AudioMode::Mic
        );
        assert_eq!(
            AudioMode::parse(&json!({"audio": "none"})).unwrap(),
            AudioMode::None
        );
        assert!(AudioMode::parse(&json!({"audio": "loud"})).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn other_platforms_refuse_to_record() {
        let store = RefStore::new();
        let target = WindowRef { id: 1 };
        for result in [
            start(&store, &target, &json!({})),
            stop(&json!({})),
            status(&json!({})),
        ] {
            let error = result.expect_err("recording must be refused off Windows");
            assert_eq!(error.code, ErrorCode::UnsupportedPlatform);
        }
    }
}
