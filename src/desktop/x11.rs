//! Ubuntu / Linux X11: RandR for monitors, GetImage for pictures, XFixes for
//! the mouse pointer, XTest for mouse and keyboard. Pure Rust (no C headers).

use super::{Desktop, PointerImage, PointerPos, draw_cursor, to_absolute};
use crate::codec::Frame;
use crate::keys;
use crate::proto::{InputEvent, MonitorInfo, MouseButton};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, ImageFormat, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
    MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const SHIFT_L: u32 = 0xffe1;

pub struct X11Desktop {
    conn: RustConnection,
    root: u32,
    screen_w: u16,
    screen_h: u16,
    /// keysym -> (keycode, needs shift)
    keymap: HashMap<u32, (u8, bool)>,
    /// A keycode with nothing on it, borrowed to type unusual characters.
    spare: Option<u8>,
    has_xfixes: bool,
    /// X Damage object on the root window: the X server tells us when pixels
    /// change, so watching an idle screen costs almost nothing.
    damage: Option<u32>,
    first: bool,
    last_pointer: (i16, i16),
    /// Draw the pointer into pictures (off when the viewer draws it).
    pointer_in_picture: bool,
    /// Last pointer shape read: X cursor serial, content key, picture.
    cursor: Option<(u32, u64, PointerImage)>,
}

impl X11Desktop {
    pub fn open() -> Result<X11Desktop> {
        let (conn, screen_num) = x11rb::connect(None).context(
            "cannot open the X11 display (is DISPLAY set and is the session X11, not Wayland?)",
        )?;
        let screen = &conn.setup().roots[screen_num];
        let (root, screen_w, screen_h) = (screen.root, screen.width_in_pixels, screen.height_in_pixels);
        conn.xtest_get_version(2, 2)?.reply().context("XTest extension missing")?;
        let has_xfixes = conn.xfixes_query_version(5, 0).ok().and_then(|c| c.reply().ok()).is_some();
        let damage = match conn.damage_query_version(1, 1).ok().and_then(|c| c.reply().ok()) {
            Some(_) => {
                let id = conn.generate_id()?;
                conn.damage_create(id, root, ReportLevel::NON_EMPTY)?;
                conn.flush()?;
                Some(id)
            }
            None => None,
        };
        let mut d = X11Desktop {
            conn,
            root,
            screen_w,
            screen_h,
            keymap: HashMap::new(),
            spare: None,
            has_xfixes,
            damage,
            first: true,
            last_pointer: (i16::MIN, i16::MIN),
            pointer_in_picture: true,
            cursor: None,
        };
        d.load_keymap()?;
        Ok(d)
    }

    fn load_keymap(&mut self) -> Result<()> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = self.conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        let per = reply.keysyms_per_keycode as usize;
        self.keymap.clear();
        self.spare = None;
        for (i, syms) in reply.keysyms.chunks(per.max(1)).enumerate() {
            let kc = min + i as u8;
            if syms.iter().all(|&s| s == 0) {
                self.spare = Some(kc);
                continue;
            }
            if syms[0] != 0 {
                self.keymap.entry(syms[0]).or_insert((kc, false));
            }
            if per > 1 && syms[1] != 0 {
                self.keymap.entry(syms[1]).or_insert((kc, true));
            }
        }
        Ok(())
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn.xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)?;
        Ok(())
    }

    fn move_to(&self, monitors: &[MonitorInfo], mon: u8, x: f32, y: f32) -> Result<()> {
        if let Some((ax, ay)) = to_absolute(monitors, mon, x, y) {
            self.fake(MOTION_NOTIFY_EVENT, 0, ax as i16, ay as i16)?;
        }
        Ok(())
    }

    fn tap_button(&self, button: u8, times: u32) -> Result<()> {
        for _ in 0..times {
            self.fake(BUTTON_PRESS_EVENT, button, 0, 0)?;
            self.fake(BUTTON_RELEASE_EVENT, button, 0, 0)?;
        }
        Ok(())
    }

    fn key_code(&self, kc: u8, down: bool) -> Result<()> {
        self.fake(if down { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT }, kc, 0, 0)
    }

    fn type_char(&mut self, c: char) -> Result<()> {
        let ks = keys::char_keysym(c);
        if let Some(&(kc, shift)) = self.keymap.get(&ks) {
            let shift_kc = self.keymap.get(&SHIFT_L).map(|e| e.0);
            if shift && let Some(s) = shift_kc {
                self.key_code(s, true)?;
            }
            self.key_code(kc, true)?;
            self.key_code(kc, false)?;
            if shift && let Some(s) = shift_kc {
                self.key_code(s, false)?;
            }
            return Ok(());
        }
        // Not on the keyboard layout: put it on the spare key for a moment.
        let Some(spare) = self.spare else { return Ok(()) };
        self.conn.change_keyboard_mapping(1, spare, 2, &[ks, ks])?;
        self.conn.get_input_focus()?.reply()?; // make sure the mapping is applied
        self.key_code(spare, true)?;
        self.key_code(spare, false)?;
        self.conn.get_input_focus()?.reply()?;
        self.conn.change_keyboard_mapping(1, spare, 2, &[0, 0])?;
        Ok(())
    }

    fn named_key(&self, name: &str, down: bool) -> Result<()> {
        let Some(ks) = keys::keysym(name) else { return Ok(()) };
        if let Some(&(kc, _)) = self.keymap.get(&ks) {
            self.key_code(kc, down)?;
        }
        Ok(())
    }
}

impl Desktop for X11Desktop {
    fn changed(&mut self) -> bool {
        let Some(damage) = self.damage else { return true };
        let mut changed = self.first;
        self.first = false;
        let mut got_damage = false;
        while let Ok(Some(ev)) = self.conn.poll_for_event() {
            if matches!(ev, Event::DamageNotify(_)) {
                got_damage = true;
            }
        }
        if got_damage {
            // Mark everything repaired so the next change sends a new notice.
            let _ = self.conn.damage_subtract(damage, x11rb::NONE, x11rb::NONE);
            let _ = self.conn.flush();
            changed = true;
        }
        // The pointer is drawn into the picture, so a move counts as a change.
        if self.pointer_in_picture
            && let Ok(cookie) = self.conn.query_pointer(self.root)
            && let Ok(p) = cookie.reply()
            && (p.root_x, p.root_y) != self.last_pointer
        {
            self.last_pointer = (p.root_x, p.root_y);
            changed = true;
        }
        changed
    }

    fn monitors(&mut self) -> Result<Vec<MonitorInfo>> {
        let mut out = Vec::new();
        if let Ok(reply) = self.conn.randr_get_monitors(self.root, true).and_then(|c| Ok(c.reply())) {
            if let Ok(reply) = reply {
                let mut mons = reply.monitors;
                // Left-to-right, top-to-bottom, like the person sees them.
                mons.sort_by_key(|m| (m.x, m.y));
                for (i, m) in mons.iter().enumerate() {
                    let name = self
                        .conn
                        .get_atom_name(m.name)
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                        .unwrap_or_else(|| format!("Monitor {}", i + 1));
                    out.push(MonitorInfo {
                        index: i as u8,
                        name,
                        x: m.x as i32,
                        y: m.y as i32,
                        width: m.width as u32,
                        height: m.height as u32,
                        primary: m.primary,
                    });
                }
            }
        }
        if out.is_empty() {
            out.push(MonitorInfo {
                index: 0,
                name: "Screen".into(),
                x: 0,
                y: 0,
                width: self.screen_w as u32,
                height: self.screen_h as u32,
                primary: true,
            });
        }
        Ok(out)
    }

    fn capture(&mut self, m: &MonitorInfo) -> Result<Frame> {
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, m.x as i16, m.y as i16, m.width as u16, m.height as u16, !0)?
            .reply()
            .context("reading the screen")?;
        let need = (m.width * m.height * 4) as usize;
        if reply.data.len() < need {
            bail!("unsupported screen colour format (depth {})", reply.depth);
        }
        let mut frame = Frame::from_bgra(m.width, m.height, &reply.data, m.width as usize * 4);
        if self.has_xfixes
            && self.pointer_in_picture
            && let Ok(cur) = self.conn.xfixes_get_cursor_image().and_then(|c| Ok(c.reply()))
            && let Ok(cur) = cur
        {
            let left = cur.x as i32 - cur.xhot as i32 - m.x;
            let top = cur.y as i32 - cur.yhot as i32 - m.y;
            draw_cursor(&mut frame, left, top, cur.width as u32, cur.height as u32, &cur.cursor_image);
        }
        Ok(frame)
    }

    fn set_pointer_in_picture(&mut self, on: bool) {
        self.pointer_in_picture = on;
    }

    fn pointer(&mut self) -> Option<PointerPos> {
        if !self.has_xfixes {
            return None;
        }
        // One request gives position, shape serial and picture.
        let cur = self.conn.xfixes_get_cursor_image().ok()?.reply().ok()?;
        let key = match &self.cursor {
            Some((serial, key, _)) if *serial == cur.cursor_serial => *key,
            _ => {
                let img = PointerImage::from_premultiplied_argb(
                    cur.width as u32,
                    cur.height as u32,
                    cur.xhot as u32,
                    cur.yhot as u32,
                    &cur.cursor_image,
                );
                let key = img.key();
                self.cursor = Some((cur.cursor_serial, key, img));
                key
            }
        };
        Some(PointerPos { x: cur.x as i32, y: cur.y as i32, shape: key })
    }

    fn pointer_image(&mut self) -> Option<PointerImage> {
        self.cursor.as_ref().map(|c| c.2.clone())
    }

    fn input(&mut self, monitors: &[MonitorInfo], ev: &InputEvent) -> Result<()> {
        match ev {
            InputEvent::MouseMove { mon, x, y } => self.move_to(monitors, *mon, *x, *y)?,
            InputEvent::MouseButton { mon, x, y, button, down } => {
                self.move_to(monitors, *mon, *x, *y)?;
                let b = match button {
                    MouseButton::Left => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Right => 3,
                };
                self.fake(if *down { BUTTON_PRESS_EVENT } else { BUTTON_RELEASE_EVENT }, b, 0, 0)?;
            }
            InputEvent::Scroll { dx, dy } => {
                let steps = |v: f32| (v.abs().round() as u32).clamp(if v != 0.0 { 1 } else { 0 }, 20);
                if *dy > 0.0 {
                    self.tap_button(4, steps(*dy))?;
                } else if *dy < 0.0 {
                    self.tap_button(5, steps(*dy))?;
                }
                if *dx > 0.0 {
                    self.tap_button(6, steps(*dx))?;
                } else if *dx < 0.0 {
                    self.tap_button(7, steps(*dx))?;
                }
            }
            InputEvent::Key { key, down } => self.named_key(key, *down)?,
            InputEvent::Text { text } => {
                for c in text.chars().take(512) {
                    self.type_char(c)?;
                }
            }
            InputEvent::CtrlAltDel => {
                for (k, d) in [("Control", true), ("Alt", true), ("Delete", true), ("Delete", false), ("Alt", false), ("Control", false)] {
                    self.named_key(k, d)?;
                }
            }
        }
        self.conn.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a real X11 desktop: `cargo test -- --ignored x11_capture`.
    /// Only reads the screen into memory; does not move the mouse.
    #[test]
    #[ignore]
    fn x11_capture_reads_every_monitor() {
        let mut d = X11Desktop::open().expect("X11 display");
        let mons = d.monitors().unwrap();
        assert!(!mons.is_empty());
        for m in &mons {
            let f = d.capture(m).unwrap();
            assert_eq!((f.width, f.height), (m.width, m.height));
            assert!(f.rgba.chunks_exact(4).any(|p| p[0] != 0 || p[1] != 0 || p[2] != 0), "picture is not all black");
            println!("{} {}x{} at {},{} primary={}", m.name, m.width, m.height, m.x, m.y, m.primary);
        }
        assert!(keys::keysym("A").is_some_and(|ks| d.keymap.contains_key(&ks)), "keyboard map loaded");
    }
}
