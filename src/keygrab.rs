//! Alt+Tab (and Alt+Shift+Tab, Alt+F4, Alt+Esc...) for the remote computer.
//!
//! The local window manager takes Alt+Tab before any app sees it. So when
//! Alt goes down in a screen window (in control mode, mouse on the picture),
//! the viewer grabs the whole keyboard on its own window until Alt is let go
//! again; meanwhile the keys are read straight from the X server here and
//! sent to the remote computer. Linux (X11) only; elsewhere [`AltGrab::start`]
//! does nothing and Alt+Tab stays local.

use crate::proto::InputEvent;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Shift and Ctrl as last sent to the remote computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Held {
    pub shift: bool,
    pub ctrl: bool,
}

const ALT_L: u32 = 0xffe9;
const ALT_R: u32 = 0xffea;
const META_L: u32 = 0xffe7; // Alt on some layouts
const META_R: u32 = 0xffe8;

/// What to do with the keys read during a grab. Alt itself stays with the
/// viewer (it was pressed there and is let go there); letting go of it ends
/// the grab.
#[derive(Debug, Clone, Copy, Default)]
pub struct Forward {
    pub held: Held,
}

impl Forward {
    /// One key from the X server (its unshifted keysym). Returns the event
    /// for the remote computer, and `true` when the grab is over.
    pub fn key(&mut self, keysym: u32, down: bool) -> (Option<InputEvent>, bool) {
        if matches!(keysym, ALT_L | ALT_R | META_L | META_R) {
            return (None, !down);
        }
        let Some(name) = crate::keys::name_for_keysym(keysym) else { return (None, false) };
        match name.as_str() {
            "Shift" if self.held.shift == down => return (None, false),
            "Control" if self.held.ctrl == down => return (None, false),
            "Shift" => self.held.shift = down,
            "Control" => self.held.ctrl = down,
            _ => {}
        }
        (Some(InputEvent::Key { key: name, down }), false)
    }
}

/// The keyboard, grabbed while Alt is held. Dropping it lets go at once.
pub struct AltGrab {
    stop: Arc<AtomicBool>,
    /// Set when the grab ended (Alt let go): Shift/Ctrl as sent by then.
    done: Arc<Mutex<Option<Held>>>,
    thread: Option<JoinHandle<()>>,
}

impl AltGrab {
    /// Grab the keyboard on the focused window, if it is one of ours and Alt
    /// is held; keys then go to `send` until Alt is let go. `held`: Shift and
    /// Ctrl as the remote computer has them now.
    pub fn start(send: impl Fn(InputEvent) + Send + 'static, held: Held) -> Option<AltGrab> {
        #[cfg(target_os = "linux")]
        {
            let stop = Arc::new(AtomicBool::new(false));
            let done = Arc::new(Mutex::new(None));
            let thread = x11::start(Box::new(send), Forward { held }, stop.clone(), done.clone())
                .map_err(|e| log::debug!("Alt+Tab not grabbed: {e:#}"))
                .ok()?;
            Some(AltGrab { stop, done, thread: Some(thread) })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (send, held);
            None
        }
    }

    /// `Some` once Alt was let go: Shift/Ctrl as the remote computer has them.
    pub fn finished(&self) -> Option<Held> {
        *self.done.lock().unwrap()
    }
}

impl Drop for AltGrab {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(target_os = "linux")]
mod x11 {
    use super::*;
    use anyhow::{Context, Result, bail};
    use std::time::{Duration, Instant};
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, GrabMode, GrabStatus, InputFocus, Window};
    use x11rb::rust_connection::RustConnection;

    /// Longest a grab may last, in case Alt's release is never seen.
    const MAX_GRAB: Duration = Duration::from_secs(60);

    pub fn start(
        send: Box<dyn Fn(InputEvent) + Send>,
        fwd: Forward,
        stop: Arc<AtomicBool>,
        done: Arc<Mutex<Option<Held>>>,
    ) -> Result<JoinHandle<()>> {
        let (conn, screen) = RustConnection::connect(None).context("connecting to X")?;
        let root = conn.setup().roots[screen].root;
        let focus = conn.get_input_focus()?.reply()?.focus;
        if focus == u32::from(InputFocus::POINTER_ROOT) || focus == x11rb::NONE || !ours(&conn, focus, root)? {
            bail!("the focused window is not ours");
        }
        let keysyms = keymap(&conn)?;
        // On the focused window itself, so the window keeps the focus.
        let status = conn
            .grab_keyboard(false, focus, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?
            .reply()?
            .status;
        if status != GrabStatus::SUCCESS {
            bail!("keyboard grab refused: {status:?}");
        }
        let thread = std::thread::Builder::new().name("alt-grab".into()).spawn(move || {
            let held = run(&conn, &keysyms, fwd, &stop, &*send);
            let _ = conn.ungrab_keyboard(x11rb::CURRENT_TIME);
            let _ = conn.flush();
            *done.lock().unwrap() = Some(held);
        })?;
        Ok(thread)
    }

    /// Forward keys until Alt is let go (or [`AltGrab`] is dropped).
    fn run(conn: &RustConnection, keysyms: &Keysyms, mut fwd: Forward, stop: &AtomicBool, send: &dyn Fn(InputEvent)) -> Held {
        let started = Instant::now();
        let mut checked = Instant::now() - Duration::from_secs(1);
        while !stop.load(Ordering::SeqCst) && started.elapsed() < MAX_GRAB {
            // Alt may have been let go before the grab began, or its
            // release lost: look at the keys themselves now and then.
            if checked.elapsed() >= Duration::from_millis(50) {
                checked = Instant::now();
                match alt_down(conn, keysyms) {
                    Ok(true) => {}
                    _ => break,
                }
            }
            match conn.poll_for_event() {
                Ok(Some(Event::KeyPress(e))) => {
                    let (ev, end) = fwd.key(keysyms.get(e.detail), true);
                    if let Some(ev) = ev {
                        send(ev);
                    }
                    if end {
                        break;
                    }
                }
                Ok(Some(Event::KeyRelease(e))) => {
                    let (ev, end) = fwd.key(keysyms.get(e.detail), false);
                    if let Some(ev) = ev {
                        send(ev);
                    }
                    if end {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => std::thread::sleep(Duration::from_millis(3)),
                Err(_) => break,
            }
        }
        fwd.held
    }

    /// Unshifted keysym of every keycode.
    pub struct Keysyms {
        min: u8,
        per: usize,
        syms: Vec<u32>,
    }

    impl Keysyms {
        fn get(&self, code: u8) -> u32 {
            let i = (code.saturating_sub(self.min) as usize) * self.per;
            self.syms.get(i).copied().unwrap_or(0)
        }
    }

    fn keymap(conn: &RustConnection) -> Result<Keysyms> {
        let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
        let r = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        Ok(Keysyms { min, per: (r.keysyms_per_keycode as usize).max(1), syms: r.keysyms })
    }

    fn alt_down(conn: &RustConnection, keysyms: &Keysyms) -> Result<bool> {
        let keys = conn.query_keymap()?.reply()?.keys;
        Ok((0..=255u8).any(|code| {
            keys[code as usize / 8] & (1 << (code % 8)) != 0 && matches!(keysyms.get(code), ALT_L | ALT_R | META_L | META_R)
        }))
    }

    /// The window (or the top-level window it is in) belongs to this process.
    fn ours(conn: &RustConnection, mut w: Window, root: Window) -> Result<bool> {
        let pid_atom = conn.intern_atom(true, b"_NET_WM_PID")?.reply()?.atom;
        if pid_atom == x11rb::NONE {
            return Ok(false);
        }
        for _ in 0..8 {
            if w == root || w == x11rb::NONE {
                break;
            }
            let p = conn.get_property(false, w, pid_atom, AtomEnum::CARDINAL, 0, 1)?.reply()?;
            if let Some(pid) = p.value32().and_then(|mut v| v.next()) {
                return Ok(pid == std::process::id());
            }
            w = conn.query_tree(w)?.reply()?.parent;
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, down: bool) -> Option<InputEvent> {
        Some(InputEvent::Key { key: name.into(), down })
    }

    #[test]
    fn alt_tab_goes_through_and_letting_go_of_alt_ends_it() {
        let mut f = Forward::default();
        assert_eq!(f.key(0xff09, true), (key("Tab", true), false));
        assert_eq!(f.key(0xff09, false), (key("Tab", false), false));
        assert_eq!(f.key(0xff09, true), (key("Tab", true), false), "Tab again: next window");
        assert_eq!(f.key(0xff09, false), (key("Tab", false), false));
        assert_eq!(f.key(ALT_L, false), (None, true), "Alt itself is let go by the viewer");
    }

    #[test]
    fn alt_shift_tab_goes_backwards() {
        let mut f = Forward::default();
        assert_eq!(f.key(0xffe1, true), (key("Shift", true), false));
        assert_eq!(f.key(0xfe20, true), (key("Tab", true), false), "Shift+Tab's own keysym");
        assert_eq!(f.key(0xfe20, false), (key("Tab", false), false));
        assert_eq!(f.key(0xffe2, false), (key("Shift", false), false), "right Shift counts too");
        assert_eq!(f.held, Held::default());
    }

    #[test]
    fn modifiers_already_down_are_not_pressed_twice() {
        let mut f = Forward { held: Held { shift: false, ctrl: true } };
        assert_eq!(f.key(0xffe3, true), (None, false), "Ctrl was down before the grab (key repeat)");
        assert_eq!(f.key(0xffc1, true), (key("F4", true), false));
        assert_eq!(f.key(0xffe3, false), (key("Control", false), false));
        assert_eq!(f.held, Held { shift: false, ctrl: false });
    }

    #[test]
    fn other_alt_keys_and_unknown_symbols() {
        let mut f = Forward::default();
        assert_eq!(f.key(ALT_R, true), (None, false), "Alt pressed again: nothing");
        assert_eq!(f.key(META_L, false), (None, true), "Alt that the layout calls Meta");
        assert_eq!(f.key(0x6c1, true), (None, false), "no key name");
        assert_eq!(f.key(0xff1b, true), (key("Escape", true), false));
        assert_eq!(f.key(0x66, true), (key("F", true), false));
    }

    /// Needs a real X11 desktop: `cargo test -- --ignored x11_alt_tab`.
    /// Opens a small window of its own, gives it the keyboard and presses
    /// Alt+Tab with XTest: the keys must come here, not to the desktop.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn x11_alt_tab_is_grabbed_and_forwarded() {
        use std::time::{Duration, Instant};
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, InputFocus, PropMode, WindowClass};
        use x11rb::protocol::xtest::ConnectionExt as _;
        use x11rb::wrapper::ConnectionExt as _;

        let (conn, screen) = x11rb::rust_connection::RustConnection::connect(None).unwrap();
        let root = conn.setup().roots[screen].root;
        let win = conn.generate_id().unwrap();
        conn.create_window(0, win, root, 0, 0, 200, 100, 0, WindowClass::INPUT_OUTPUT, 0, &CreateWindowAux::new()).unwrap();
        let pid = conn.intern_atom(false, b"_NET_WM_PID").unwrap().reply().unwrap().atom;
        conn.change_property32(PropMode::REPLACE, win, pid, AtomEnum::CARDINAL, &[std::process::id()]).unwrap();
        conn.map_window(win).unwrap();
        conn.sync().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME).unwrap();
        conn.sync().unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let map = conn.get_keyboard_mapping(conn.setup().min_keycode, conn.setup().max_keycode - conn.setup().min_keycode + 1).unwrap().reply().unwrap();
        let code = |ks: u32| {
            let i = map.keysyms.iter().position(|&k| k == ks).unwrap();
            conn.setup().min_keycode + (i / map.keysyms_per_keycode as usize) as u8
        };
        let (alt, tab) = (code(ALT_L), code(0xff09));
        let press = |c: u8, down: bool| {
            let t = if down { x11rb::protocol::xproto::KEY_PRESS_EVENT } else { x11rb::protocol::xproto::KEY_RELEASE_EVENT };
            conn.xtest_fake_input(t, c, x11rb::CURRENT_TIME, root, 0, 0, 0).unwrap();
            conn.sync().unwrap();
            std::thread::sleep(Duration::from_millis(30));
        };

        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        press(alt, true);
        let grab = AltGrab::start(move |e| sink.lock().unwrap().push(e), Held::default());
        if grab.is_none() {
            press(alt, false);
            panic!("keyboard not grabbed");
        }
        let grab = grab.unwrap();
        press(tab, true);
        press(tab, false);
        press(alt, false);
        let t0 = Instant::now();
        while grab.finished().is_none() && t0.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(grab.finished(), Some(Held::default()), "letting go of Alt ends the grab");
        assert_eq!(*got.lock().unwrap(), vec![key("Tab", true).unwrap(), key("Tab", false).unwrap()]);
        drop(grab);
        conn.destroy_window(win).unwrap();
        conn.sync().unwrap();
    }
}
