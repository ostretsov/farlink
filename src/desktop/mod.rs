//! Screen capture and input injection on the remote machine.

use crate::codec::{Frame, Rect};
use crate::proto::{InputEvent, MonitorInfo};
use anyhow::Result;

#[cfg(target_os = "linux")]
pub mod x11;
#[cfg(windows)]
pub mod dxgi;
#[cfg(windows)]
pub mod windows;

/// One machine's screens, mouse and keyboard.
pub trait Desktop {
    /// Current monitors in the remote virtual desktop.
    fn monitors(&mut self) -> Result<Vec<MonitorInfo>>;
    /// Picture of one monitor.
    fn capture(&mut self, mon: &MonitorInfo) -> Result<Frame>;
    /// Perform a mouse / keyboard action.
    fn input(&mut self, monitors: &[MonitorInfo], ev: &InputEvent) -> Result<()>;
    /// Did anything on screen (or the pointer) change since the last call?
    /// Systems that can't tell answer `true`; the caller then compares pictures.
    fn changed(&mut self) -> bool {
        true
    }
    /// Draw the mouse pointer into pictures (`true`, the default) or leave
    /// it out because the viewer draws it itself. With `false`, pointer
    /// moves alone no longer count as a change.
    fn set_pointer_in_picture(&mut self, _on: bool) {}
    /// Where the pointer is right now. `None` = this system can't tell; the
    /// pointer then stays in the pictures.
    fn pointer(&mut self) -> Option<PointerPos> {
        None
    }
    /// The look of the pointer as of the last [`pointer`](Self::pointer) call.
    fn pointer_image(&mut self) -> Option<PointerImage> {
        None
    }
}

/// The mouse pointer's hot spot (virtual-desktop pixels) and which shape it
/// has: the same `shape` key always means the same picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerPos {
    pub x: i32,
    pub y: i32,
    pub shape: u64,
}

/// A pointer shape: straight (not premultiplied) RGBA pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerImage {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub rgba: Vec<u8>,
}

impl PointerImage {
    /// From premultiplied ARGB words (as X11 gives them).
    pub fn from_premultiplied_argb(width: u32, height: u32, hot_x: u32, hot_y: u32, argb: &[u32]) -> PointerImage {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for i in 0..(width * height) as usize {
            let p = argb.get(i).copied().unwrap_or(0);
            let a = (p >> 24) & 0xff;
            let un = |c: u32| (c * 255).checked_div(a).map_or(0, |v| v.min(255) as u8);
            rgba.extend_from_slice(&[un((p >> 16) & 0xff), un((p >> 8) & 0xff), un(p & 0xff), a as u8]);
        }
        PointerImage { width, height, hot_x: hot_x.min(width.saturating_sub(1)), hot_y: hot_y.min(height.saturating_sub(1)), rgba }
    }

    /// The demo pointer: a white arrow with a dark edge.
    pub fn arrow() -> PointerImage {
        let (w, h) = (11u32, 17u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            // Rows 0..16 widen by one pixel every row (the arrow's slanted side).
            let width = (y + 1).min(w);
            for x in 0..width {
                let edge = x == 0 || x + 1 == width || y + 1 == h;
                let c = if edge { [20, 20, 20, 255] } else { [255, 255, 255, 255] };
                rgba[((y * w + x) * 4) as usize..][..4].copy_from_slice(&c);
            }
        }
        PointerImage { width: w, height: h, hot_x: 0, hot_y: 0, rgba }
    }

    /// A small number that is the same for the same picture.
    pub fn key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (self.width, self.height, self.hot_x, self.hot_y, &self.rgba).hash(&mut h);
        h.finish()
    }
}

/// Open the real desktop of this computer.
pub fn open_desktop() -> Result<Box<dyn Desktop>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(x11::X11Desktop::open()?))
    }
    #[cfg(windows)]
    {
        Ok(Box::new(windows::WinDesktop::open()?))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        anyhow::bail!("this operating system is not supported")
    }
}

/// Map a normalised position (0..1 inside monitor `mon`) to absolute
/// virtual-desktop pixels.
pub fn to_absolute(monitors: &[MonitorInfo], mon: u8, x: f32, y: f32) -> Option<(i32, i32)> {
    let m = monitors.iter().find(|m| m.index == mon)?;
    let x = x.clamp(0.0, 1.0);
    let y = y.clamp(0.0, 1.0);
    let px = m.x + ((x * m.width as f32) as i32).min(m.width as i32 - 1);
    let py = m.y + ((y * m.height as f32) as i32).min(m.height as i32 - 1);
    Some((px, py))
}

/// Bounding box of all monitors (Windows' "virtual screen").
pub fn virtual_bounds(monitors: &[MonitorInfo]) -> (i32, i32, i32, i32) {
    let x0 = monitors.iter().map(|m| m.x).min().unwrap_or(0);
    let y0 = monitors.iter().map(|m| m.y).min().unwrap_or(0);
    let x1 = monitors.iter().map(|m| m.x + m.width as i32).max().unwrap_or(1);
    let y1 = monitors.iter().map(|m| m.y + m.height as i32).max().unwrap_or(1);
    (x0, y0, x1 - x0, y1 - y0)
}

/// Draw a mouse pointer picture (premultiplied ARGB, as X11 and Windows give
/// it) onto a monitor picture. `left`/`top` are relative to the monitor.
pub fn draw_cursor(frame: &mut Frame, left: i32, top: i32, w: u32, h: u32, argb: &[u32]) {
    for cy in 0..h as i32 {
        let y = top + cy;
        if y < 0 || y >= frame.height as i32 {
            continue;
        }
        for cx in 0..w as i32 {
            let x = left + cx;
            if x < 0 || x >= frame.width as i32 {
                continue;
            }
            let Some(&p) = argb.get((cy as u32 * w + cx as u32) as usize) else { return };
            let a = (p >> 24) & 0xff;
            if a == 0 {
                continue;
            }
            let src = [(p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff];
            let d = frame.pixel(x as u32, y as u32);
            let blend = |s: u32, d: u8| (s + d as u32 * (255 - a) / 255).min(255) as u8;
            frame.set_pixel(x as u32, y as u32, [blend(src[0], d[0]), blend(src[1], d[1]), blend(src[2], d[2]), 255]);
        }
    }
}

/// A pretend computer with two screens, used by tests, the `--demo` host and
/// screenshots. It draws a desktop-like picture and a visible mouse pointer,
/// and records the input it receives.
pub struct TestPattern {
    pub monitors: Vec<MonitorInfo>,
    pub tick: u32,
    pub pointer: (i32, i32),
    pub received: Vec<InputEvent>,
    /// Draw the pointer into pictures (off when the viewer draws it).
    pub pointer_in_picture: bool,
}

impl TestPattern {
    pub fn new(sizes: &[(u32, u32)]) -> TestPattern {
        let mut x = 0;
        let monitors = sizes
            .iter()
            .enumerate()
            .map(|(i, &(w, h))| {
                let m = MonitorInfo { index: i as u8, name: format!("DEMO-{}", i + 1), x, y: 0, width: w, height: h, primary: i == 0 };
                x += w as i32;
                m
            })
            .collect();
        TestPattern { monitors, tick: 0, pointer: (100, 100), received: Vec::new(), pointer_in_picture: true }
    }

    pub fn dual() -> TestPattern {
        TestPattern::new(&[(1600, 900), (1280, 1024)])
    }

    /// Draw the fake desktop of monitor `m` (also used for viewer previews).
    pub fn render(m: &MonitorInfo, tick: u32, pointer: Option<(i32, i32)>) -> Frame {
        let (w, h) = (m.width, m.height);
        let mut f = Frame::new(w, h);
        // Wallpaper gradient, different per monitor.
        for y in 0..h {
            for x in 0..w {
                let t = (x + y) as f32 / (w + h) as f32;
                let c = if m.index == 0 {
                    [(22.0 + 30.0 * t) as u8, (60.0 + 70.0 * t) as u8, (110.0 + 60.0 * t) as u8, 255]
                } else {
                    [(70.0 + 50.0 * t) as u8, (40.0 + 30.0 * t) as u8, (90.0 + 50.0 * t) as u8, 255]
                };
                f.set_pixel(x, y, c);
            }
        }
        // Task bar.
        f.fill_rect(Rect { x: 0, y: h - h / 22, w, h: h / 22 }, [24, 26, 32, 255]);
        f.fill_rect(Rect { x: 12, y: h - h / 22 + 6, w: h / 22 - 12, h: h / 22 - 12 }, [80, 160, 240, 255]);
        // Two "application windows".
        let win = |f: &mut Frame, x: u32, y: u32, ww: u32, hh: u32, accent: [u8; 4]| {
            f.fill_rect(Rect { x, y, w: ww, h: hh }, [245, 246, 248, 255]);
            f.fill_rect(Rect { x, y, w: ww, h: 28 }, accent);
            for line in 0..((hh - 50) / 22) {
                let lw = ww - 40 - (line * 37 % (ww / 3));
                f.fill_rect(Rect { x: x + 20, y: y + 44 + line * 22, w: lw, h: 9 }, [190, 195, 205, 255]);
            }
        };
        win(&mut f, w / 12, h / 10, w / 2, h / 2, [52, 120, 220, 255]);
        win(&mut f, w / 2, h / 3, w * 2 / 5, h / 2, [40, 160, 110, 255]);
        // Moving progress bar so there is always something changing.
        let bar_w = w / 3;
        let bx = w / 2 + 20;
        let by = h / 3 + h / 2 - 40;
        f.fill_rect(Rect { x: bx, y: by, w: bar_w, h: 12 }, [220, 222, 228, 255]);
        f.fill_rect(Rect { x: bx, y: by, w: (tick * 7) % bar_w + 1, h: 12 }, [40, 160, 110, 255]);
        if let Some((px, py)) = pointer {
            let (lx, ly) = (px - m.x, py - m.y);
            if lx >= 0 && ly >= 0 && (lx as u32) < w && (ly as u32) < h {
                for i in 0..16u32 {
                    f.fill_rect(Rect { x: lx as u32, y: ly as u32 + i, w: (i / 2 + 1).min(w - lx as u32), h: 1 }, [255, 255, 255, 255]);
                }
            }
        }
        f
    }
}

impl Desktop for TestPattern {
    fn monitors(&mut self) -> Result<Vec<MonitorInfo>> {
        Ok(self.monitors.clone())
    }

    fn capture(&mut self, mon: &MonitorInfo) -> Result<Frame> {
        self.tick = self.tick.wrapping_add(1);
        Ok(TestPattern::render(mon, self.tick, self.pointer_in_picture.then_some(self.pointer)))
    }

    fn set_pointer_in_picture(&mut self, on: bool) {
        self.pointer_in_picture = on;
    }

    fn pointer(&mut self) -> Option<PointerPos> {
        Some(PointerPos { x: self.pointer.0, y: self.pointer.1, shape: 1 })
    }

    fn pointer_image(&mut self) -> Option<PointerImage> {
        Some(PointerImage::arrow())
    }

    fn input(&mut self, monitors: &[MonitorInfo], ev: &InputEvent) -> Result<()> {
        match ev {
            InputEvent::MouseMove { mon, x, y } | InputEvent::MouseButton { mon, x, y, .. } => {
                if let Some(p) = to_absolute(monitors, *mon, *x, *y) {
                    self.pointer = p;
                }
            }
            _ => {}
        }
        self.received.push(ev.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_positions_span_both_monitors() {
        let t = TestPattern::dual();
        assert_eq!(to_absolute(&t.monitors, 0, 0.0, 0.0), Some((0, 0)));
        assert_eq!(to_absolute(&t.monitors, 0, 1.0, 1.0), Some((1599, 899)));
        assert_eq!(to_absolute(&t.monitors, 1, 0.5, 0.5), Some((1600 + 640, 512)));
        assert_eq!(to_absolute(&t.monitors, 1, 7.0, -3.0), Some((1600 + 1279, 0)), "clamped");
        assert_eq!(to_absolute(&t.monitors, 5, 0.5, 0.5), None);
    }

    #[test]
    fn virtual_bounds_with_negative_left_monitor() {
        let mons = vec![
            MonitorInfo { index: 0, name: "a".into(), x: 0, y: 0, width: 1920, height: 1080, primary: true },
            MonitorInfo { index: 1, name: "b".into(), x: -1280, y: -200, width: 1280, height: 1024, primary: false },
        ];
        assert_eq!(virtual_bounds(&mons), (-1280, -200, 3200, 1280));
    }

    #[test]
    fn cursor_blending_and_clipping() {
        let mut f = Frame::filled(4, 4, [0, 0, 0, 255]);
        // 2x2 cursor: opaque white, transparent, half-transparent red (premultiplied), opaque white.
        let cur = [0xffffffff, 0x00000000, 0x80800000, 0xffffffff];
        draw_cursor(&mut f, 3, 3, 2, 2, &cur); // mostly off-screen: must not panic
        assert_eq!(f.pixel(3, 3), [255, 255, 255, 255]);
        let mut g = Frame::filled(4, 4, [0, 0, 200, 255]);
        draw_cursor(&mut g, 0, 0, 2, 2, &cur);
        assert_eq!(g.pixel(1, 0), [0, 0, 200, 255], "transparent pixel leaves screen as is");
        let p = g.pixel(0, 1);
        assert_eq!(p[0], 128);
        assert!((p[2] as i32 - 99).abs() <= 1);
    }

    #[test]
    fn pointer_image_unpremultiplies_and_keys_by_content() {
        // Opaque white, nothing, half-transparent red (premultiplied 0x80 red).
        let img = PointerImage::from_premultiplied_argb(3, 1, 9, 9, &[0xffffffff, 0, 0x80800000]);
        assert_eq!(&img.rgba[0..4], &[255, 255, 255, 255]);
        assert_eq!(&img.rgba[4..8], &[0, 0, 0, 0]);
        assert_eq!(&img.rgba[8..12], &[255, 0, 0, 128]);
        assert_eq!((img.hot_x, img.hot_y), (2, 0), "hot spot kept inside");
        assert_eq!(img.key(), img.clone().key());
        assert_ne!(img.key(), PointerImage::arrow().key());
        let a = PointerImage::arrow();
        assert_eq!(a.rgba.len(), (a.width * a.height * 4) as usize);
    }

    #[test]
    fn test_pattern_pointer_can_leave_the_picture() {
        let mut t = TestPattern::dual();
        let mons = t.monitors().unwrap();
        let with = t.capture(&mons[0]).unwrap();
        t.set_pointer_in_picture(false);
        t.tick = 0; // same moment as `with`
        let without = t.capture(&mons[0]).unwrap();
        let reference = TestPattern::render(&mons[0], 1, None);
        assert_eq!(without, reference);
        assert_ne!(with, reference);
        assert_eq!(t.pointer(), Some(PointerPos { x: 100, y: 100, shape: 1 }));
    }

    #[test]
    fn test_pattern_moves_pointer_and_changes_over_time() {
        let mut t = TestPattern::dual();
        let mons = t.monitors().unwrap();
        let a = t.capture(&mons[0]).unwrap();
        let b = t.capture(&mons[0]).unwrap();
        assert_ne!(a, b, "progress bar animates");
        t.input(&mons, &InputEvent::MouseMove { mon: 1, x: 0.0, y: 0.0 }).unwrap();
        assert_eq!(t.pointer, (1600, 0));
        assert_eq!(t.received.len(), 1);
    }
}
