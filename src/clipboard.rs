//! Copied text shared between the viewer's computer and the remote one
//! (control sessions only).
//!
//! The host keeps a [`ClipboardSync`]: text arriving from the viewer is put on
//! the remote clipboard, and [`poll_loop`] sends text copied on the remote
//! computer back. `last` remembers what both sides already have, so nothing
//! echoes back and forth.

use crate::proto::SessionMsg;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// Longer texts are not shared (keeps the session snappy).
pub const MAX_TEXT: usize = 1 << 20;
/// How often the host looks for newly copied text.
pub const POLL: Duration = Duration::from_millis(500);

/// A clipboard that holds text.
pub trait ClipboardBackend: Send {
    fn get(&mut self) -> Option<String>;
    fn set(&mut self, text: &str) -> Result<()>;
}

/// The real clipboard of this computer. One instance lives for the whole
/// process: on X11 the copied text exists only while its owner is alive.
static SYSTEM: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

fn with_system<R>(f: impl FnOnce(&mut arboard::Clipboard) -> Result<R, arboard::Error>) -> Result<R> {
    let mut guard = SYSTEM.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(arboard::Clipboard::new()?);
    }
    let r = f(guard.as_mut().expect("just opened"));
    if let Err(e) = &r
        && !matches!(e, arboard::Error::ContentNotAvailable)
    {
        *guard = None; // e.g. the display connection broke: open again next time
    }
    Ok(r?)
}

/// Text on this computer's clipboard (None: empty, not text, or no clipboard).
pub fn system_get() -> Option<String> {
    with_system(|c| c.get_text()).ok()
}

pub fn system_set(text: &str) -> Result<()> {
    with_system(|c| c.set_text(text))
}

pub struct SystemClipboard;

impl ClipboardBackend for SystemClipboard {
    fn get(&mut self) -> Option<String> {
        system_get()
    }
    fn set(&mut self, text: &str) -> Result<()> {
        system_set(text)
    }
}

/// The pretend computer's clipboard (`--demo` host and tests).
static DEMO: Mutex<Option<String>> = Mutex::new(None);

pub fn demo_get() -> Option<String> {
    DEMO.lock().unwrap().clone()
}

pub fn demo_set(text: &str) {
    *DEMO.lock().unwrap() = Some(text.to_string());
}

pub struct DemoClipboard;

impl ClipboardBackend for DemoClipboard {
    fn get(&mut self) -> Option<String> {
        demo_get()
    }
    fn set(&mut self, text: &str) -> Result<()> {
        demo_set(text);
        Ok(())
    }
}

/// Windows hands text back with `\r\n` line ends; compare without them.
pub fn same_text(a: &str, b: &str) -> bool {
    a == b || a.replace("\r\n", "\n") == b.replace("\r\n", "\n")
}

/// One side's clipboard plus what the other side already has.
pub struct ClipboardSync {
    backend: Box<dyn ClipboardBackend>,
    last: Option<String>,
}

impl ClipboardSync {
    /// Whatever is copied now stays here: only later copies are shared.
    pub fn new(mut backend: Box<dyn ClipboardBackend>) -> ClipboardSync {
        let last = backend.get();
        ClipboardSync { backend, last }
    }

    pub fn for_host(demo: bool) -> ClipboardSync {
        if demo { ClipboardSync::new(Box::new(DemoClipboard)) } else { ClipboardSync::new(Box::new(SystemClipboard)) }
    }

    /// The other side copied `text`: put it on this clipboard.
    pub fn receive(&mut self, text: String) {
        if text.len() > MAX_TEXT {
            return;
        }
        if let Err(e) = self.backend.set(&text) {
            log::warn!("clipboard: {e:#}");
        }
        self.last = Some(text);
    }

    /// Text copied here since the last look, if it should be shared.
    pub fn changed(&mut self) -> Option<String> {
        let now = self.backend.get()?;
        if self.last.as_deref().is_some_and(|l| same_text(l, &now)) {
            return None;
        }
        self.last = Some(now.clone());
        (!now.is_empty() && now.len() <= MAX_TEXT).then_some(now)
    }
}

/// Host side: send text copied on this computer to the viewer until `stop`.
/// Nothing is sent while the session is view-only (and what was copied
/// meanwhile is not sent later either).
pub fn poll_loop(sync: Arc<Mutex<ClipboardSync>>, tx: tokio::sync::mpsc::Sender<Message>, view_only: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
        let Some(text) = sync.lock().unwrap().changed() else { continue };
        if view_only.load(Ordering::Relaxed) {
            continue;
        }
        let msg = serde_json::to_string(&SessionMsg::Clipboard { text }).unwrap();
        if tx.blocking_send(Message::Text(msg.into())).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Mem(Arc<Mutex<Option<String>>>);
    impl ClipboardBackend for Mem {
        fn get(&mut self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
        fn set(&mut self, text: &str) -> Result<()> {
            // Like Windows: line ends come back as \r\n.
            *self.0.lock().unwrap() = Some(text.replace('\n', "\r\n"));
            Ok(())
        }
    }

    #[test]
    fn only_new_copies_are_shared_and_nothing_echoes() {
        let mem = Mem::default();
        *mem.0.lock().unwrap() = Some("before".into());
        let mut sync = ClipboardSync::new(Box::new(mem.clone()));
        assert_eq!(sync.changed(), None, "what was copied before the session stays");

        sync.receive("from viewer\nline 2".into());
        assert_eq!(mem.0.lock().unwrap().as_deref(), Some("from viewer\r\nline 2"));
        assert_eq!(sync.changed(), None, "received text is not sent back");

        *mem.0.lock().unwrap() = Some("copied here".into());
        assert_eq!(sync.changed().as_deref(), Some("copied here"));
        assert_eq!(sync.changed(), None, "sent once");

        *mem.0.lock().unwrap() = Some("x".repeat(MAX_TEXT + 1));
        assert_eq!(sync.changed(), None, "too long to share");
        sync.receive("y".repeat(MAX_TEXT + 1));
        assert_eq!(mem.0.lock().unwrap().as_ref().map(|t| t.len()), Some(MAX_TEXT + 1), "too long text from the viewer is ignored");
    }

    /// Real clipboard of this machine (needs a display):
    /// `cargo test -- --ignored system_clipboard`.
    #[test]
    #[ignore]
    fn system_clipboard_round_trip() {
        let text = format!("farlink test {}", std::process::id());
        system_set(&text).unwrap();
        assert_eq!(system_get().as_deref(), Some(text.as_str()));
    }
}
