//! "Ping": the viewer's person Ctrl+Shift+clicks a spot on the remote screen
//! and the remote person sees a dot with waves spreading out from it for a
//! moment. The host draws it in a small click-through window on top of
//! everything (so the viewer sees it in the pictures too).

use crate::desktop::to_absolute;
use crate::proto::MonitorInfo;
use std::time::{Duration, Instant};

/// How long the effect lasts.
pub const DURATION: Duration = Duration::from_millis(1100);
/// Time between drawn frames.
const FRAME: Duration = Duration::from_millis(16);
/// Colour of the dot and the waves (straight RGB).
const COLOR: [f32; 3] = [255.0, 64.0, 48.0];
/// Waves: start (fraction of [`DURATION`]) of each one.
const WAVES: [f32; 3] = [0.0, 0.17, 0.34];
/// Fraction of [`DURATION`] one wave takes to reach the edge.
const WAVE_LEN: f32 = 0.62;

/// Radius of the effect for a monitor: bigger on bigger screens.
pub fn radius_for(m: &MonitorInfo) -> u32 {
    (m.height as f32 * 0.11).clamp(80.0, 220.0) as u32
}

/// Where (virtual-desktop pixels) and how big a ping at (`x`, `y`, 0..1
/// inside monitor `mon`) is.
pub fn place(monitors: &[MonitorInfo], mon: u8, x: f32, y: f32) -> Option<(i32, i32, u32)> {
    let m = monitors.iter().find(|m| m.index == mon)?;
    let (px, py) = to_absolute(monitors, mon, x, y)?;
    Some((px, py, radius_for(m)))
}

/// The effect at `t` (0..1 of [`DURATION`]) as a square of `2 * radius`
/// pixels, premultiplied ARGB words (`0xAARRGGBB`), centre = the click.
pub fn render(t: f32, radius: u32) -> Vec<u32> {
    let size = 2 * radius;
    let r = radius as f32;
    let t = t.clamp(0.0, 1.0);
    // The dot: pops in slightly bigger, settles, fades over the last 40 %.
    let dot_r = r * 0.06 + 4.0 * (1.0 - (t * 6.0).min(1.0));
    let dot_a = 1.0 - ((t - 0.6) / 0.4).clamp(0.0, 1.0);
    // Each wave: (radius, half thickness, opacity).
    let waves: Vec<(f32, f32, f32)> = WAVES
        .iter()
        .filter_map(|start| {
            let p = (t - start) / WAVE_LEN;
            if !(0.0..1.0).contains(&p) {
                return None;
            }
            let eased = 1.0 - (1.0 - p) * (1.0 - p); // fast at first, then slowing
            let wr = dot_r + eased * (r - 2.0 - dot_r);
            Some((wr, 2.2 - 1.2 * p, (1.0 - p).powf(1.6) * 0.9))
        })
        .collect();
    let mut out = vec![0u32; (size * size) as usize];
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 + 0.5 - r).powi(2) + (y as f32 + 0.5 - r).powi(2)).sqrt();
            // Straight colour + coverage, composited front to back (dot on top).
            let mut layers: [(f32, [f32; 3]); 5] = [(0.0, COLOR); 5];
            let mut n = 0;
            if dot_a > 0.0 {
                // White rim around the dot so it shows on red backgrounds too.
                let rim = (dot_r + 2.0 - d + 0.5).clamp(0.0, 1.0) * dot_a;
                let fill = (dot_r - d + 0.5).clamp(0.0, 1.0) * dot_a;
                layers[n] = (fill, COLOR);
                layers[n + 1] = (rim, [255.0, 255.0, 255.0]);
                n += 2;
            }
            for &(wr, half, a) in &waves {
                let cov = (half - (d - wr).abs() + 0.5).clamp(0.0, 1.0) * a;
                if cov > 0.0 && n < layers.len() {
                    layers[n] = (cov, COLOR);
                    n += 1;
                }
            }
            // Front-to-back "over".
            let (mut a, mut c) = (0.0f32, [0.0f32; 3]);
            for &(la, lc) in &layers[..n] {
                let w = la * (1.0 - a);
                for i in 0..3 {
                    c[i] += lc[i] * w;
                }
                a += w;
            }
            if a > 0.0 {
                let b = |v: f32| (v.round() as u32).min(255);
                out[(y * size + x) as usize] = b(a * 255.0) << 24 | b(c[0]) << 16 | b(c[1]) << 8 | b(c[2]);
            }
        }
    }
    out
}

/// Fraction of the effect done `elapsed` after it started (≥ 1 = over).
pub fn progress(elapsed: Duration) -> f32 {
    elapsed.as_secs_f32() / DURATION.as_secs_f32()
}

/// Show a ping centred on (`x`, `y`) virtual-desktop pixels. Returns at
/// once: the effect runs on its own thread and cleans up after itself.
pub fn show(x: i32, y: i32, radius: u32) {
    let spawned = std::thread::Builder::new().name("ping".into()).spawn(move || {
        if let Err(e) = overlay(x, y, radius) {
            log::warn!("ping: {e}");
        }
    });
    if let Err(e) = spawned {
        log::warn!("ping: {e}");
    }
}

/// Runs `frame(t)` for every frame of the effect, then stops.
fn animate(mut frame: impl FnMut(f32) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        let t = progress(start.elapsed());
        if t >= 1.0 {
            return Ok(());
        }
        frame(t)?;
        let next = start + FRAME.mul_f32(((start.elapsed().as_secs_f32() / FRAME.as_secs_f32()).floor()) + 1.0);
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
}

#[cfg(target_os = "linux")]
fn overlay(x: i32, y: i32, radius: u32) -> anyhow::Result<()> {
    use anyhow::Context;
    use x11rb::connection::Connection;
    use x11rb::protocol::shape::{self, ConnectionExt as _};
    use x11rb::protocol::xfixes::ConnectionExt as _;
    use x11rb::protocol::xproto::*;
    use x11rb::wrapper::ConnectionExt as _;

    let (conn, n) = x11rb::connect(None).context("cannot open the X display")?;
    let screen = conn.setup().roots[n].clone();
    let msb = conn.setup().image_byte_order == ImageOrder::MSB_FIRST;
    let size = (2 * radius) as u16;
    let (left, top) = ((x - radius as i32) as i16, (y - radius as i32) as i16);
    // Real transparency needs a 32-bit visual and a compositing manager;
    // without them the window is cut to the effect's shape instead.
    let argb = screen
        .allowed_depths
        .iter()
        .filter(|d| d.depth == 32)
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.class == VisualClass::TRUE_COLOR)
        .map(|v| v.visual_id);
    let cm = conn.intern_atom(false, format!("_NET_WM_CM_S{n}").as_bytes())?.reply()?.atom;
    let composited = conn.get_selection_owner(cm)?.reply()?.owner != x11rb::NONE;
    let win = conn.generate_id()?;
    let aux = CreateWindowAux::new().override_redirect(1).border_pixel(0).background_pixel(0);
    let alpha = match argb {
        Some(visual) if composited => {
            let cmap = conn.generate_id()?;
            conn.create_colormap(ColormapAlloc::NONE, cmap, screen.root, visual)?;
            conn.create_window(32, win, screen.root, left, top, size, size, 0, WindowClass::INPUT_OUTPUT, visual, &aux.colormap(cmap))?;
            true
        }
        _ => {
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                win,
                screen.root,
                left,
                top,
                size,
                size,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &aux,
            )?;
            conn.shape_rectangles(shape::SO::SET, shape::SK::BOUNDING, ClipOrdering::UNSORTED, win, 0, 0, &[])?;
            false
        }
    };
    // Clicks go through to whatever is underneath.
    conn.xfixes_query_version(5, 0)?.reply()?;
    let region = conn.generate_id()?;
    conn.xfixes_create_region(region, &[])?;
    conn.xfixes_set_window_shape_region(win, shape::SK::INPUT, 0, 0, region)?;
    conn.xfixes_destroy_region(region)?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new())?;
    conn.map_window(win)?;
    conn.flush()?;

    let depth = if alpha { 32 } else { screen.root_depth };
    let result = animate(|t| {
        let px = render(t, radius);
        let mut bytes = Vec::with_capacity(px.len() * 4);
        for &p in &px {
            // Without transparency: the effect's colour at full strength where
            // it is clearly visible, nothing elsewhere.
            let p = if alpha { p } else { unpremultiply(p) };
            bytes.extend_from_slice(&if msb { p.to_be_bytes() } else { p.to_le_bytes() });
        }
        if !alpha {
            let rects = shape_runs(&px, 2 * radius, 0x60);
            conn.shape_rectangles(shape::SO::SET, shape::SK::BOUNDING, ClipOrdering::Y_SORTED, win, 0, 0, &rects)?;
        }
        conn.put_image(ImageFormat::Z_PIXMAP, win, gc, size, size, 0, 0, 0, depth, &bytes)?;
        conn.configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        conn.sync()?;
        Ok(())
    });
    let _ = conn.destroy_window(win);
    let _ = conn.free_gc(gc);
    let _ = conn.flush();
    result
}

/// Rows of pixels whose alpha is at least `min`, as rectangles (X11 shape).
#[cfg(target_os = "linux")]
fn shape_runs(px: &[u32], size: u32, min: u32) -> Vec<x11rb::protocol::xproto::Rectangle> {
    let mut out = Vec::new();
    for y in 0..size {
        let row = &px[(y * size) as usize..((y + 1) * size) as usize];
        let mut x = 0;
        while x < size {
            if row[x as usize] >> 24 >= min {
                let start = x;
                while x < size && row[x as usize] >> 24 >= min {
                    x += 1;
                }
                out.push(x11rb::protocol::xproto::Rectangle { x: start as i16, y: y as i16, width: (x - start) as u16, height: 1 });
            } else {
                x += 1;
            }
        }
    }
    out
}

/// Premultiplied ARGB → the same colour at full strength.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unpremultiply(p: u32) -> u32 {
    let a = p >> 24;
    if a == 0 {
        return 0;
    }
    let un = |c: u32| ((c * 255) / a).min(255);
    0xff00_0000 | un((p >> 16) & 0xff) << 16 | un((p >> 8) & 0xff) << 8 | un(p & 0xff)
}

#[cfg(windows)]
fn overlay(x: i32, y: i32, radius: u32) -> anyhow::Result<()> {
    use std::mem::{size_of, zeroed};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::StationsAndDesktops::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    let size = (2 * radius) as i32;
    unsafe {
        // Draw on the desktop that is on screen (also the lock screen / UAC).
        let desk = OpenInputDesktop(0, 0, GENERIC_ALL);
        if !desk.is_null() {
            SetThreadDesktop(desk);
        }
        let class: Vec<u16> = "FarlinkPing\0".encode_utf16().collect();
        let inst = GetModuleHandleW(null());
        let mut wc: WNDCLASSW = zeroed();
        wc.lpfnWndProc = Some(DefWindowProcW);
        wc.hInstance = inst;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc); // fails harmlessly when already registered
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            null(),
            WS_POPUP,
            x - radius as i32,
            y - radius as i32,
            size,
            size,
            null_mut(),
            null_mut(),
            inst,
            null(),
        );
        if hwnd.is_null() {
            if !desk.is_null() {
                CloseDesktop(desk);
            }
            anyhow::bail!("cannot create the ping window");
        }
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = size;
        bmi.bmiHeader.biHeight = -size; // top-down rows
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut core::ffi::c_void = null_mut();
        let bmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let old = if bmp.is_null() { null_mut() } else { SelectObject(dc, bmp as HGDIOBJ) };
        let mut shown = false;
        let result = if bmp.is_null() || bits.is_null() {
            Err(anyhow::anyhow!("cannot create the ping picture"))
        } else {
            animate(|t| {
                // Premultiplied 0xAARRGGBB words are exactly what a 32-bit DIB
                // with per-pixel alpha wants (BGRA bytes).
                let px = render(t, radius);
                std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());
                let pos = POINT { x: x - radius as i32, y: y - radius as i32 };
                let sz = SIZE { cx: size, cy: size };
                let src = POINT { x: 0, y: 0 };
                let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
                UpdateLayeredWindow(hwnd, screen, &pos, &sz, dc, &src, 0, &blend, ULW_ALPHA);
                if !shown {
                    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    shown = true;
                }
                SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                let mut msg: MSG = zeroed();
                while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                Ok(())
            })
        };
        DestroyWindow(hwnd);
        if !bmp.is_null() {
            SelectObject(dc, old);
            DeleteObject(bmp as HGDIOBJ);
        }
        DeleteDC(dc);
        ReleaseDC(null_mut(), screen);
        if !desk.is_null() {
            CloseDesktop(desk);
        }
        result
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn overlay(_x: i32, _y: i32, _radius: u32) -> anyhow::Result<()> {
    anyhow::bail!("not supported on this system")
}

/// Blend the effect at `t` onto a monitor picture (the demo desktop).
/// (`cx`, `cy`) is the centre relative to the picture.
pub fn draw_into(frame: &mut crate::codec::Frame, cx: i32, cy: i32, radius: u32, t: f32) {
    let px = render(t, radius);
    let size = 2 * radius as i32;
    for y in 0..size {
        let fy = cy - radius as i32 + y;
        if fy < 0 || fy >= frame.height as i32 {
            continue;
        }
        for x in 0..size {
            let fx = cx - radius as i32 + x;
            if fx < 0 || fx >= frame.width as i32 {
                continue;
            }
            let p = px[(y * size + x) as usize];
            let a = p >> 24;
            if a == 0 {
                continue;
            }
            let under = frame.pixel(fx as u32, fy as u32);
            let mix = |src: u32, dst: u8| (src + dst as u32 * (255 - a) / 255).min(255) as u8;
            frame.set_pixel(fx as u32, fy as u32, [mix((p >> 16) & 0xff, under[0]), mix((p >> 8) & 0xff, under[1]), mix(p & 0xff, under[2]), 255]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(px: &[u32], radius: u32, x: u32, y: u32) -> u32 {
        px[(y * 2 * radius + x) as usize] >> 24
    }

    #[test]
    fn starts_with_a_solid_dot_and_ends_empty() {
        let r = 100;
        let start = render(0.0, r);
        assert_eq!(alpha(&start, r, r, r), 255, "the dot is solid at the click");
        assert_eq!(alpha(&start, r, 0, 0), 0, "corners stay clear");
        let end = render(0.999, r);
        assert!(end.iter().all(|p| p >> 24 < 8), "nearly invisible at the end");
    }

    #[test]
    fn waves_move_outwards_and_fade() {
        let r = 100;
        // Farthest visible pixel along the row through the centre.
        let reach = |t: f32| {
            let px = render(t, r);
            (r..2 * r).rev().find(|&x| alpha(&px, r, x, r) > 20).map(|x| x - r).unwrap_or(0)
        };
        let strongest = |t: f32| {
            let px = render(t, r);
            ((r + 20)..2 * r).map(|x| alpha(&px, r, x, r)).max().unwrap_or(0)
        };
        assert!(reach(0.1) < reach(0.3) && reach(0.3) < reach(0.5), "{} {} {}", reach(0.1), reach(0.3), reach(0.5));
        assert!(reach(0.5) < r);
        assert!(strongest(0.3) > strongest(0.85));
    }

    #[test]
    fn premultiplied_and_unpremultiplied() {
        for p in render(0.4, 60) {
            let a = p >> 24;
            assert!((p >> 16) & 0xff <= a && (p >> 8) & 0xff <= a && p & 0xff <= a);
        }
        assert_eq!(unpremultiply(0x80800000), 0xffff0000);
        assert_eq!(unpremultiply(0), 0);
    }

    /// Real overlay on this machine's X display: the ping shows up in a
    /// screen capture while it runs and is gone afterwards.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn x11_ping_overlay() {
        use crate::desktop::Desktop;
        let mut d = crate::desktop::x11::X11Desktop::open().expect("X display");
        let mons = d.monitors().unwrap();
        let m = mons[0].clone();
        let (cx, cy) = (m.width / 2, m.height / 2);
        d.input(&mons, &crate::proto::InputEvent::Ping { mon: m.index, x: 0.5, y: 0.5 }).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        let during = d.capture(&m).unwrap();
        if let Ok(dir) = std::env::var("FARLINK_PING_SHOT") {
            let crop = |f: &crate::codec::Frame| {
                let r = radius_for(&m);
                image::RgbaImage::from_fn(2 * r, 2 * r, |x, y| image::Rgba(f.pixel(cx - r + x, cy - r + y)))
            };
            crop(&during).save(format!("{dir}/ping-during.png")).unwrap();
        }
        let c = during.pixel(cx, cy);
        assert!(c[0] > 200 && c[1] < 120 && c[2] < 120, "red dot at the centre, got {c:?}");
        std::thread::sleep(DURATION);
        let after = d.capture(&m).unwrap();
        assert_ne!(after.pixel(cx, cy), c, "gone afterwards");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shape_runs_cover_only_visible_pixels() {
        // 3x3: row 0 = visible, hidden, visible; row 1 = all faint; row 2 = hidden, visible, visible.
        let (on, faint) = (0xff000000, 0x10000000);
        let px = [on, 0, 0x80000000, faint, faint, faint, 0, on, on];
        let runs = shape_runs(&px, 3, 0x60);
        let got: Vec<_> = runs.iter().map(|r| (r.x, r.y, r.width, r.height)).collect();
        assert_eq!(got, vec![(0, 0, 1, 1), (2, 0, 1, 1), (1, 2, 2, 1)]);
        assert!(shape_runs(&render(0.999, 50), 100, 0x60).is_empty(), "nothing left at the end");
        assert!(!shape_runs(&render(0.2, 50), 100, 0x60).is_empty());
    }

    #[test]
    fn progress_runs_over_the_duration() {
        assert_eq!(progress(Duration::ZERO), 0.0);
        assert!((progress(DURATION / 2) - 0.5).abs() < 1e-3);
        assert!(progress(DURATION) >= 1.0);
    }

    #[test]
    fn sizes_follow_the_screen_within_limits() {
        let m = |h| MonitorInfo { index: 0, name: String::new(), x: 0, y: 0, width: h * 16 / 9, height: h, primary: true };
        assert_eq!(radius_for(&m(1080)), 118);
        assert_eq!(radius_for(&m(480)), 80, "never tiny");
        assert_eq!(radius_for(&m(4320)), 220, "never huge");
    }

    #[test]
    fn drawing_near_the_edge_is_clipped() {
        let mut f = crate::codec::Frame::new(50, 40);
        draw_into(&mut f, 0, 0, 30, 0.0);
        let c = f.pixel(0, 0);
        assert!(c[0] > 200 && c[1] < 120, "dot drawn at the corner, got {c:?}");
        draw_into(&mut f, -500, 900, 30, 0.3); // fully outside: no panic
        assert_eq!(f.rgba.len(), 50 * 40 * 4);
    }

    #[test]
    fn placed_on_the_right_monitor_and_sized_for_it() {
        let t = crate::desktop::TestPattern::dual();
        assert_eq!(place(&t.monitors, 1, 0.5, 0.5), Some((1600 + 640, 512, 112)));
        assert_eq!(place(&t.monitors, 7, 0.5, 0.5), None);
    }
}
