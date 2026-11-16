//! The tray icon: normal, paused, the progress ring, and notifications.

use super::*;

/// Copy text into a UTF-16 field of fixed size. Cut it to fit with its NUL.
pub(super) fn fill(field: &mut [u16], text: &str) {
    let mut w: Vec<u16> = text.encode_utf16().take(field.len() - 1).collect();
    w.push(0);
    field[..w.len()].copy_from_slice(&w);
}

pub(super) unsafe fn show_note(hwnd: HWND) {
    let Some((title, text)) = bench_state().note.take() else {
        return;
    };
    let mut nid = icon_data(hwnd);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    fill(&mut nid.szInfoTitle, &title);
    fill(&mut nid.szInfo, &text);
    Shell_NotifyIconW(NIM_MODIFY, &nid);
}

/// The data of our tray icon (id 1 of `hwnd`), without fields to change.
pub(super) fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    // All fields of NOTIFYICONDATAW are plain data: zero is valid.
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid
}

pub(super) unsafe fn update_icon(hwnd: HWND, message: u32) {
    let paused = ENGINE.with(|e| e.try_borrow().ok().and_then(|g| g.as_ref().map(|e| e.paused)));
    let mut nid = icon_data(hwnd);
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    let (status, progress) = {
        let state = bench_state();
        (state.status.clone(), state.progress)
    };
    // While a benchmark runs: a ring that fills with the progress.
    let ring = match status {
        Some(_) => progress_icon(progress),
        None => null_mut(),
    };
    if ring.is_null() {
        // Embedded icons: 1 = normal, 2 = paused (see build.rs).
        let id: usize = if paused == Some(true) { 2 } else { 1 };
        nid.hIcon = LoadImageW(
            GetModuleHandleW(null()),
            id as *const u16,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_SHARED,
        );
    } else {
        nid.hIcon = ring;
    }
    if nid.hIcon.is_null() {
        nid.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
    }
    let tip = match status {
        Some(s) => format!("Pinfold benchmark: {s}"),
        None if paused == Some(true) => format!("Pinfold {VERSION} (paused)"),
        None => format!("Pinfold {VERSION}"),
    };
    fill(&mut nid.szTip, &tip);
    Shell_NotifyIconW(message, &nid);
    // The shell keeps its own copy. Destroy the previous ring.
    let old = RING_ICON.with(|r| r.replace(ring));
    if !old.is_null() {
        DestroyIcon(old);
    }
}

thread_local! {
    static RING_ICON: Cell<HICON> = const { Cell::new(null_mut()) };
}

/// Pixels (ARGB, rows from the top) of a progress ring: a track, the done
/// part clockwise from 12 o'clock, and a dot in the middle.
pub(super) fn ring_pixels(size: usize, done: f64) -> Vec<u32> {
    const LIT: u32 = 0x1E90FF; // blue
    const TRACK: u32 = 0x9A9A9A; // grey, half transparent
    const SAMPLES: usize = 4; // for each axis, for smooth edges
    let c = size as f64 / 2.0;
    let (outer, inner, dot) = (c - 0.5, c * 0.58, c * 0.28);
    let mut out = Vec::with_capacity(size * size);
    for y in 0..size {
        for x in 0..size {
            let (mut lit, mut track) = (0usize, 0usize);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = x as f64 + (sx as f64 + 0.5) / SAMPLES as f64 - c;
                    let py = y as f64 + (sy as f64 + 0.5) / SAMPLES as f64 - c;
                    let r = (px * px + py * py).sqrt();
                    if r <= dot {
                        lit += 1;
                    } else if r >= inner && r <= outer {
                        // Angle clockwise from the top, 0 to 1.
                        let a = px.atan2(-py) / std::f64::consts::TAU;
                        if a.rem_euclid(1.0) < done {
                            lit += 1;
                        } else {
                            track += 1;
                        }
                    }
                }
            }
            let n = (SAMPLES * SAMPLES) as f64;
            let alpha_lit = lit as f64 / n * 255.0;
            let alpha_track = track as f64 / n * 150.0;
            let (rgb, alpha) = if alpha_lit >= alpha_track {
                (LIT, alpha_lit + alpha_track * 0.3)
            } else {
                (TRACK, alpha_track)
            };
            out.push(((alpha.min(255.0) as u32) << 24) | rgb);
        }
    }
    out
}

/// A tray icon with the progress ring. Null if it fails.
pub(super) unsafe fn progress_icon(done: f64) -> HICON {
    let size = GetSystemMetrics(SM_CXSMICON).max(16) as usize;
    let pixels = ring_pixels(size, done);
    let color = CreateBitmap(size as i32, size as i32, 1, 32, pixels.as_ptr().cast());
    let mask_bits = vec![0u8; size.div_ceil(16) * 2 * size];
    let mask = CreateBitmap(size as i32, size as i32, 1, 1, mask_bits.as_ptr().cast());
    let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon =
        if color.is_null() || mask.is_null() { null_mut() } else { CreateIconIndirect(&info) };
    DeleteObject(color);
    DeleteObject(mask);
    icon
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_ring_fills_clockwise_from_the_top() {
        let size = 16;
        let px = |p: &[u32], x: usize, y: usize| p[y * size + x];
        let lit = |v: u32| v & 0xFFFFFF == 0x1E90FF && v >> 24 > 128;
        let track = |v: u32| v & 0xFFFFFF == 0x9A9A9A && v >> 24 > 64;
        // Right (3 o'clock), bottom, left of the ring, and the middle.
        let (right, bottom, left) = ((14, 8), (8, 14), (1, 8));
        let quarter = ring_pixels(size, 0.3);
        assert!(lit(px(&quarter, right.0, right.1)));
        assert!(track(px(&quarter, bottom.0, bottom.1)));
        assert!(track(px(&quarter, left.0, left.1)));
        assert!(lit(px(&quarter, 8, 8)), "the dot is always drawn");
        let most = ring_pixels(size, 0.8);
        assert!(lit(px(&most, left.0, left.1)));
        let none = ring_pixels(size, 0.0);
        assert!(track(px(&none, right.0, right.1)));
        assert_eq!(px(&none, 0, 0) >> 24, 0, "the corners are transparent");
    }

    #[test]
    fn long_text_is_cut_to_fit_the_field() {
        let mut field = [1u16; 8];
        super::fill(&mut field, "Pinfold benchmark");
        assert_eq!(super::from_wide(&field), "Pinfold");
        assert_eq!(field[7], 0);
    }
}
