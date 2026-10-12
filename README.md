# aimux helper (Windows bridge)

Native helper that the aimux reverse client spawns on a Windows desktop to
observe and drive GUI applications. It is a single executable
(`windows-bridge.exe`) that speaks JSON lines over stdin/stdout: one request in,
one response out. The binary is downloaded and verified by the aimux client at
startup, so nothing here has to be installed by hand on the target machine.

Upstream: [injaneity/pi-computer-use](https://github.com/injaneity/pi-computer-use)
(`native/windows/bridge-rs`), MIT licensed — see `LICENSE` (Copyright (c) 2026
Zane Chee) and the patch list below. This repository is the aimux home for that
crate: it carries the local patches, builds the binary in CI, and documents how
the artifact is published and consumed.

## Thanks

This helper is the work of the upstream
[pi-computer-use](https://github.com/injaneity/pi-computer-use) project — the
root-forest window model, the accessibility outline, the ref/lookId action
resolution and the input layer all come from there, and none of it would exist
here without them. Thanks to its authors for building it and releasing it under
MIT. Everything this repository adds is the patch list below; any shortcoming in
the binary we ship is ours, not theirs.

## Layout

```
native/windows/bridge-rs/     Rust crate producing windows-bridge.exe
.github/workflows/build.yml   CI: build + test on windows-latest, refresh the `build` release
scripts/publish-helper.sh     upload the binary + update the helper manifest
```

## Patches carried on top of upstream v0.5.1

- **PW_RENDERFULLCONTENT (0x2) on `PrintWindow`** (`capture.rs`) — GPU-accelerated
  windows (Chromium / Electron / Qt WebEngine / DirectComposition) keep their
  pixels out of the GDI surface, so a plain `PrintWindow` or `BitBlt` returns a
  black frame. With the flag, and a screen-region `BitBlt` fallback when the
  result is still blank or the call failed, capture works for those windows.
- **`AttachThreadInput` around `SetForegroundWindow`** (`window.rs`) — Windows
  otherwise refuses a foreground switch requested by a background process and
  only flashes the taskbar button instead of raising the window.
- **Scan codes on injected keys** (`input.rs` `key()`) — raw-input consumers such
  as games identify keys by scan code. A `KEYBDINPUT` carrying only `wVk`
  arrives with an unmapped scan code and is dropped, so `keypress` / hotkeys /
  `typeText` had no effect in Unity titles. `wScan` is now resolved with
  `MapVirtualKeyW` and sent with `KEYEVENTF_SCANCODE` (plus
  `KEYEVENTF_EXTENDEDKEY` for extended keys).
- **`keepPressdownMs` on keypress** (`input.rs` `send_keys`) — a scan code alone
  is not enough for per-frame input pollers: releasing inside the same
  `SendInput` batch leaves the key down for under a millisecond, so no sampled
  frame ever sees it. Callers can hold the key down for N ms (capped at 5000,
  default 0).

## Protocol

Protocol version **4**. Envelope:

```json
{"protocolVersion": 4, "id": "m1728000000000", "cmd": "look", "args": {"rootRef": "@w1"}}
{"protocolVersion": 4, "id": "m1728000000000", "ok": true, "result": {...}}
```

A request whose `protocolVersion` does not match is rejected; clients treat the
mismatch as "helper needs reinstalling" rather than as a bad request.

Commands: `diagnostics`, `listRoots` (alias `listWindows`), `look` (alias
`screenshot`), `focusWindow`, `act`, `actBatch`, `uiaReadText` (alias
`axReadText`), `uiaWaitFor` (alias `axWaitFor`), `openBrowserLocation`.
Unknown commands return `UnsupportedCommand`.

`look` returns a `lookId` that `act` resolves refs and image coordinates
against; `act` accepts either an element ref from the outline or image
coordinates, which it inverts back to physical pixels through the scale factor
recorded by that `look`.

## Build

The crate is a normal Rust binary crate and builds on Windows with
`cargo build --release --locked`; the CI workflow does exactly that on
`windows-latest` and refreshes the `build` release with
`windows-bridge.exe` + `sha256.txt`, which is convenient from any machine:

```bash
gh release download build --repo <this repo> --dir out --clobber
```

Cross-compiling from Linux is not supported by this repository. The published
binary is the release asset, not a local build.

## Publish

The aimux client compares the `sha256` in the helper manifest against the binary
it has on disk and re-downloads when it differs, so publishing a new binary is a
change of the manifest plus the upload:

```bash
scripts/publish-helper.sh --exe out/windows-bridge.exe --helper-version 0.5.1-aimux5 ...
```

The script reads the manifest from the publish destination, updates
`helperVersion`, `protocolVersion`, `aimuxMinVersion`, `notes`, `url`, `sha256`
and `size`, uploads the binary and writes the manifest back. Host and paths come
from the environment (see the script's `--help`) and are deliberately not
committed here.

## Roadmap

- **Window recording to MP4.** Add `recordStart` / `recordStop`: a helper-side
  thread captures the target window at N fps through the existing capture path
  (so occlusion does not matter) and muxes H.264 with the Windows Media
  Foundation sink writer — no FFmpeg dependency — writing an `.mp4` on the
  remote host. Recording has to be start/stop rather than one blocking call so
  the helper stays usable for `look` / `act` while it records. That bumps the
  protocol version, so the aimux client's expected version and the manifest's
  `protocolVersion` move together.
- Multi-monitor and per-monitor DPI reporting are already handled at startup
  (per-monitor-v2 awareness); deeper capture backends (Windows Graphics Capture)
  are not planned for now.

## Contributing

Commit messages in English, no personal names and no AI tool names. Keep private
hosts, users and paths out of committed files.
