//! Fast screenshot capture for the local-only fast OODA path.
//!
//! This module is deliberately separate from [`crate::capture`]: the existing
//! `look` command keeps its behaviour, warnings and UIA extraction untouched.
//! Everything here exists to make one thing cheap: get the window pixels into
//! a PNG file with as little work as possible.
//!
//! What is different from the normal path:
//!
//! * No UIA traversal at all. On a large Chromium window the accessibility
//!   subtree walk is by far the most expensive step.
//! * When the target is the foreground window, the pixels are blitted straight
//!   from the screen DC (and stretched straight to the output size). That skips
//!   `PrintWindow`, which forces the application to render a fresh frame and
//!   then copies the full-size surface back from the GPU.
//! * After capture the BGRA surface is converted and downscaled in one pass,
//!   instead of converting the full-size surface to RGBA and then resizing it.
//! * The PNG holds RGB (no alpha) and is encoded with the fast deflate preset.
//!
//! Correctness trade-offs are explicit: the screen path is only used for the
//! foreground window, and only when the sampled pixels are not blank. Anything
//! else falls back to the normal `PrintWindow` capture, so an occluded or
//! background window still produces its own content rather than whatever is on
//! top of it.

use serde_json::Value;

use crate::error::{ErrorCode, ProtocolError};
use crate::refs::{RefStore, WindowRef};

/// Result of a fast capture: encoded PNG plus the numbers needed to explain
/// where the time went.
#[derive(Debug)]
pub struct FastShot {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
    pub warnings: Vec<String>,
    /// `screen`, `screen_fallback` or `printwindow`.
    pub strategy: &'static str,
    pub capture_ms: u64,
    pub convert_ms: u64,
    pub encode_ms: u64,
}

/// Capture one window as a PNG, skipping accessibility extraction entirely.
///
/// On non-Windows platforms this returns `unsupported_platform`, matching the
/// normal capture entry point.
pub fn screenshot_fast(
    store: &mut RefStore,
    target_ref: &WindowRef,
    max_dimension: Option<u32>,
) -> Result<FastShot, ProtocolError> {
    #[cfg(not(windows))]
    {
        let _ = store;
        let _ = target_ref;
        let _ = max_dimension;
        Err(ProtocolError::new(
            "Fast screenshot capture is only supported on Windows",
            ErrorCode::UnsupportedPlatform,
        ))
    }

    #[cfg(windows)]
    {
        screenshot_fast_impl(store, target_ref, max_dimension)
    }
}

/// The helper's JSON shape for this command, kept next to the capture itself so
/// the binary and the library cannot drift apart.
pub fn response_json(shot: &FastShot, image_path: Option<&str>) -> Value {
    serde_json::json!({
        "imagePath": image_path,
        "width": shot.width,
        "height": shot.height,
        "strategy": shot.strategy,
        "warnings": shot.warnings,
        "timings": {
            "captureMs": shot.capture_ms,
            "convertMs": shot.convert_ms,
            "encodeMs": shot.encode_ms
        }
    })
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
use std::time::Instant;

#[cfg(windows)]
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
#[cfg(windows)]
use image::{ExtendedColorType, ImageEncoder};

#[cfg(windows)]
use windows::Win32::Foundation::{HWND, RECT};
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HALFTONE, HGDIOBJ, SRCCOPY,
};
#[cfg(windows)]
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect, IsIconic};

#[cfg(windows)]
fn screenshot_fast_impl(
    store: &mut RefStore,
    target_ref: &WindowRef,
    max_dimension: Option<u32>,
) -> Result<FastShot, ProtocolError> {
    let native = store.get_window(target_ref).ok_or_else(|| {
        ProtocolError::new(
            format!("Window ref '{}' not found", target_ref),
            ErrorCode::TargetNotFound,
        )
    })?;
    let hwnd = HWND(native.raw() as *mut _);

    let is_minimized = unsafe { IsIconic(hwnd).as_bool() };
    let (x, y, width, height) = unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return Err(ProtocolError::new(
                "Failed to get window bounds",
                ErrorCode::CaptureFailed,
            ));
        }
        (
            rect.left,
            rect.top,
            (rect.right - rect.left).max(0),
            (rect.bottom - rect.top).max(0),
        )
    };

    if width == 0 || height == 0 {
        return Err(ProtocolError::new(
            "The window has no capturable area",
            ErrorCode::CaptureFailed,
        ));
    }

    let (output_width, output_height) =
        output_size(width as u32, height as u32, max_dimension);
    let mut warnings: Vec<String> = Vec::new();
    if is_minimized {
        warnings.push("window_minimized".to_owned());
    }

    let capture_started = Instant::now();
    let foreground = unsafe { GetForegroundWindow() };
    let is_foreground = foreground.0 == hwnd.0;

    let mut buffer: Option<Vec<u8>> = None;
    let mut strategy = "printwindow";

    // Fast path: the window is in front, so the screen holds its pixels. This
    // is one compositor blit, including the downscale.
    if is_foreground && !is_minimized {
        if let Ok(candidate) =
            unsafe { screen_capture_bgra(x, y, width, height, output_width, output_height) }
        {
            if !is_blank(&candidate) {
                buffer = Some(candidate);
                strategy = "screen";
            }
        }
    }

    // Correct path: render the window into a private surface. Used when the
    // window is not in front, when the screen path failed, or when the screen
    // pixels were blank (locked desktop, remote session, odd composition).
    if buffer.is_none() {
        match unsafe { print_window_bgra(hwnd, width, height) } {
            Ok(candidate) if !is_blank(&candidate) => {
                buffer = Some(candidate);
                strategy = "printwindow";
            }
            Ok(_) => warnings.push("print_window_blank".to_owned()),
            Err(_) => warnings.push("print_window_failed".to_owned()),
        }
    }

    // Last resort, identical to the normal capture fallback: screen pixels even
    // without foreground, which may include an overlapping window.
    if buffer.is_none() {
        if let Ok(candidate) =
            unsafe { screen_capture_bgra(x, y, width, height, output_width, output_height) }
        {
            if !is_blank(&candidate) {
                buffer = Some(candidate);
                strategy = "screen_fallback";
                warnings.push("screen_fallback_may_include_overlapping_windows".to_owned());
            }
        }
    }

    let pixels = buffer.ok_or_else(|| {
        ProtocolError::new("Screenshot capture produced no pixels", ErrorCode::CaptureFailed)
    })?;
    let capture_ms = capture_started.elapsed().as_millis() as u64;

    // The screen path already produced the output size; the PrintWindow path is
    // still at the window size and is downscaled here, fused with BGRA→RGB.
    let (source_width, source_height) = if strategy.starts_with("screen") {
        (output_width as usize, output_height as usize)
    } else {
        (width as usize, height as usize)
    };

    let convert_started = Instant::now();
    let rgb = bgra_to_rgb8(
        &pixels,
        source_width,
        source_height,
        output_width as usize,
        output_height as usize,
    );
    let convert_ms = convert_started.elapsed().as_millis() as u64;

    let encode_started = Instant::now();
    let mut png: Vec<u8> = Vec::new();
    PngEncoder::new_with_quality(
        &mut png,
        CompressionType::Fast,
        FilterType::Sub,
    )
    .write_image(
        &rgb,
        output_width,
        output_height,
        ExtendedColorType::Rgb8,
    )
    .map_err(|error| {
        ProtocolError::new(
            format!("PNG encoding failed: {error}"),
            ErrorCode::CaptureFailed,
        )
    })?;
    let encode_ms = encode_started.elapsed().as_millis() as u64;

    Ok(FastShot {
        width: output_width,
        height: output_height,
        png,
        warnings,
        strategy,
        capture_ms,
        convert_ms,
        encode_ms,
    })
}

#[cfg(windows)]
fn output_size(width: u32, height: u32, max_dimension: Option<u32>) -> (u32, u32) {
    match max_dimension.filter(|limit| *limit > 0) {
        Some(limit) if width.max(height) > limit => {
            let scale = limit as f64 / width.max(height) as f64;
            (
                (width as f64 * scale).round().max(1.0) as u32,
                (height as f64 * scale).round().max(1.0) as u32,
            )
        }
        _ => (width, height),
    }
}

/// Copy the screen region straight into a bitmap of the output size.
///
/// # Safety
///
/// Caller must provide a valid HWND and positive dimensions.
#[cfg(windows)]
unsafe fn screen_capture_bgra(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    output_width: u32,
    output_height: u32,
) -> Result<Vec<u8>, ProtocolError> {
    let screen_dc = GetDC(HWND(std::ptr::null_mut()));
    if screen_dc.is_invalid() {
        return Err(ProtocolError::new("GetDC(screen) failed", ErrorCode::CaptureFailed));
    }
    let mem_dc = CreateCompatibleDC(screen_dc);
    if mem_dc.is_invalid() {
        ReleaseDC(HWND(std::ptr::null_mut()), screen_dc);
        return Err(ProtocolError::new(
            "CreateCompatibleDC failed",
            ErrorCode::CaptureFailed,
        ));
    }
    let bitmap = CreateCompatibleBitmap(screen_dc, output_width as i32, output_height as i32);
    if bitmap.is_invalid() {
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND(std::ptr::null_mut()), screen_dc);
        return Err(ProtocolError::new(
            "CreateCompatibleBitmap failed",
            ErrorCode::CaptureFailed,
        ));
    }

    let old_bitmap = SelectObject(mem_dc, bitmap);
    let blit_ok = if output_width as i32 == width && output_height as i32 == height {
        BitBlt(mem_dc, 0, 0, width, height, screen_dc, x, y, SRCCOPY).is_ok()
    } else {
        // HALFTONE gives a usable area average for text-heavy windows when the
        // caller asked for a smaller image than the window.
        SetStretchBltMode(mem_dc, HALFTONE);
        StretchBlt(
            mem_dc,
            0,
            0,
            output_width as i32,
            output_height as i32,
            screen_dc,
            x,
            y,
            width,
            height,
            SRCCOPY,
        )
        .is_ok()
    };

    let mut bits = vec![0u8; (output_width as usize) * (output_height as usize) * 4];
    let dib_lines = if blit_ok {
        get_dibits(
            mem_dc,
            bitmap,
            output_width as i32,
            output_height as i32,
            &mut bits,
        )
    } else {
        0
    };

    SelectObject(mem_dc, old_bitmap);
    let _ = DeleteObject(HGDIOBJ(bitmap.0));
    let _ = DeleteDC(mem_dc);
    ReleaseDC(HWND(std::ptr::null_mut()), screen_dc);

    if dib_lines == 0 {
        return Err(ProtocolError::new(
            "GetDIBits failed for the screen capture",
            ErrorCode::CaptureFailed,
        ));
    }
    Ok(bits)
}

/// Render the window into a private BGRA surface with `PrintWindow`.
///
/// # Safety
///
/// Caller must provide a valid HWND and positive dimensions.
#[cfg(windows)]
unsafe fn print_window_bgra(
    hwnd: HWND,
    width: i32,
    height: i32,
) -> Result<Vec<u8>, ProtocolError> {
    let window_dc = GetDC(hwnd);
    if window_dc.is_invalid() {
        return Err(ProtocolError::new("GetDC(window) failed", ErrorCode::CaptureFailed));
    }
    let mem_dc = CreateCompatibleDC(window_dc);
    if mem_dc.is_invalid() {
        ReleaseDC(hwnd, window_dc);
        return Err(ProtocolError::new(
            "CreateCompatibleDC failed",
            ErrorCode::CaptureFailed,
        ));
    }
    let bitmap = CreateCompatibleBitmap(window_dc, width, height);
    if bitmap.is_invalid() {
        let _ = DeleteDC(mem_dc);
        ReleaseDC(hwnd, window_dc);
        return Err(ProtocolError::new(
            "CreateCompatibleBitmap failed",
            ErrorCode::CaptureFailed,
        ));
    }

    let old_bitmap = SelectObject(mem_dc, bitmap);
    // PW_RENDERFULLCONTENT is required for GPU-composited windows; this is the
    // same flag the normal look path uses.
    let _ = PrintWindow(hwnd, mem_dc, PRINT_WINDOW_FLAGS(2));

    let mut bits = vec![0u8; (width as usize) * (height as usize) * 4];
    let dib_lines = get_dibits(mem_dc, bitmap, width, height, &mut bits);

    SelectObject(mem_dc, old_bitmap);
    let _ = DeleteObject(HGDIOBJ(bitmap.0));
    let _ = DeleteDC(mem_dc);
    ReleaseDC(hwnd, window_dc);

    if dib_lines == 0 {
        return Err(ProtocolError::new(
            "GetDIBits failed for the PrintWindow capture",
            ErrorCode::CaptureFailed,
        ));
    }
    Ok(bits)
}

/// Read a 32-bit top-down BGRA DIB into `bits`.
///
/// # Safety
///
/// `bits` must hold `width * height * 4` bytes.
#[cfg(windows)]
unsafe fn get_dibits(
    mem_dc: windows::Win32::Graphics::Gdi::HDC,
    bitmap: windows::Win32::Graphics::Gdi::HBITMAP,
    width: i32,
    height: i32,
    bits: &mut [u8],
) -> i32 {
    let header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height, // negative = top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let mut bmi = BITMAPINFO {
        bmiHeader: header,
        ..Default::default()
    };
    GetDIBits(
        mem_dc,
        bitmap,
        0,
        height as u32,
        Some(bits.as_mut_ptr() as *mut std::ffi::c_void),
        &mut bmi,
        DIB_RGB_COLORS,
    )
}

/// True when a sample of the frame is effectively black.
///
/// `PrintWindow` and screen captures both return a black surface when they
/// cannot see the window, so this is the signal to try the other strategy.
#[cfg(windows)]
fn is_blank(pixels: &[u8]) -> bool {
    let mut sampled = 0usize;
    let mut all_dark = true;
    for pixel in pixels.chunks_exact(4).step_by(97) {
        sampled += 1;
        if pixel[0] >= 8 || pixel[1] >= 8 || pixel[2] >= 8 {
            all_dark = false;
            break;
        }
    }
    sampled > 0 && all_dark
}

/// Convert BGRA to tightly packed RGB, downscaling in the same pass.
///
/// A box average is used for the downscale: each output pixel is the mean of
/// the source block it covers. It is cheap, has no ringing, and is a good fit
/// for text-heavy UI screenshots.
#[cfg(windows)]
fn bgra_to_rgb8(
    source: &[u8],
    source_width: usize,
    source_height: usize,
    output_width: usize,
    output_height: usize,
) -> Vec<u8> {
    let mut output = vec![0u8; output_width * output_height * 3];

    if output_width == source_width && output_height == source_height {
        for (index, pixel) in source.chunks_exact(4).enumerate() {
            let offset = index * 3;
            output[offset] = pixel[2];
            output[offset + 1] = pixel[1];
            output[offset + 2] = pixel[0];
        }
        return output;
    }

    for output_y in 0..output_height {
        let source_y0 = output_y * source_height / output_height;
        let source_y1 = (((output_y + 1) * source_height + output_height - 1) / output_height)
            .min(source_height)
            .max(source_y0 + 1);
        for output_x in 0..output_width {
            let source_x0 = output_x * source_width / output_width;
            let source_x1 = (((output_x + 1) * source_width + output_width - 1) / output_width)
                .min(source_width)
                .max(source_x0 + 1);

            let mut red = 0u32;
            let mut green = 0u32;
            let mut blue = 0u32;
            let mut count = 0u32;
            for y in source_y0..source_y1 {
                let row_start = (y * source_width + source_x0) * 4;
                let row_end = (y * source_width + source_x1) * 4;
                for pixel in source[row_start..row_end].chunks_exact(4) {
                    blue += pixel[0] as u32;
                    green += pixel[1] as u32;
                    red += pixel[2] as u32;
                    count += 1;
                }
            }

            let offset = (output_y * output_width + output_x) * 3;
            output[offset] = (red / count) as u8;
            output[offset + 1] = (green / count) as u8;
            output[offset + 2] = (blue / count) as u8;
        }
    }
    output
}
