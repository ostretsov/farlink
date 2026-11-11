//! The desktop app's icon in the system tray, with a small menu:
//! Open Farlink · Start at login · Quit.
//!
//! The tray runs on its own thread (ksni over D-Bus on Linux, a hidden Win32
//! window on Windows). Clicks become [`TrayEvent`]s on a channel and wake
//! the egui loop, which keeps running while the main window is hidden.

pub mod icon;

#[cfg(not(windows))]
mod linux;
#[cfg(not(windows))]
use linux as imp;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

use eframe::egui;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Show the main window (left click on the icon, or "Open Farlink").
    Open,
    /// "Start at login" was clicked.
    ToggleAutostart,
    /// Quit the app.
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayStatus {
    /// Still trying (at login the desktop's tray may come up after the app).
    Starting,
    /// The icon is shown.
    Ready,
    /// There is no tray on this desktop.
    Failed,
}

/// What the tray thread shares with the app.
struct Shared {
    tx: Mutex<Sender<TrayEvent>>,
    ctx: egui::Context,
    autostart: AtomicBool,
    status: AtomicU8,
    stop: AtomicBool,
}

impl Shared {
    fn send(&self, e: TrayEvent) {
        let _ = self.tx.lock().unwrap().send(e);
        self.ctx.request_repaint();
    }

    fn set_status(&self, s: TrayStatus) {
        self.status.store(s as u8, Ordering::SeqCst);
        self.ctx.request_repaint();
    }
}

pub struct Tray {
    rx: Receiver<TrayEvent>,
    shared: Arc<Shared>,
    inner: imp::Handle,
}

/// How long to keep trying to show the icon when there is no tray yet.
pub const RETRY_AT_LOGIN: Duration = Duration::from_secs(90);

impl Tray {
    /// Shows the icon; keeps trying for `retry_for` if the tray is not there yet.
    pub fn start(ctx: &egui::Context, autostart: bool, retry_for: Duration) -> Tray {
        let (tx, rx) = channel();
        let shared = Arc::new(Shared {
            tx: Mutex::new(tx),
            ctx: ctx.clone(),
            autostart: AtomicBool::new(autostart),
            status: AtomicU8::new(TrayStatus::Starting as u8),
            stop: AtomicBool::new(false),
        });
        let inner = imp::start(shared.clone(), retry_for);
        Tray { rx, shared, inner }
    }

    pub fn status(&self) -> TrayStatus {
        match self.shared.status.load(Ordering::SeqCst) {
            0 => TrayStatus::Starting,
            1 => TrayStatus::Ready,
            _ => TrayStatus::Failed,
        }
    }

    pub fn try_recv(&self) -> Option<TrayEvent> {
        self.rx.try_recv().ok()
    }

    /// Updates the "Start at login" check mark.
    pub fn set_autostart(&self, on: bool) {
        self.shared.autostart.store(on, Ordering::SeqCst);
        self.inner.refresh();
    }

    /// Removes the icon (also done on drop).
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.inner.shutdown();
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_reach_the_app_and_status_is_shared() {
        let (tx, rx) = channel();
        let shared = Shared {
            tx: Mutex::new(tx),
            ctx: egui::Context::default(),
            autostart: AtomicBool::new(false),
            status: AtomicU8::new(0),
            stop: AtomicBool::new(false),
        };
        shared.send(TrayEvent::Open);
        shared.send(TrayEvent::Quit);
        assert_eq!(rx.try_recv(), Ok(TrayEvent::Open));
        assert_eq!(rx.try_recv(), Ok(TrayEvent::Quit));
        shared.set_status(TrayStatus::Failed);
        assert_eq!(shared.status.load(Ordering::SeqCst), TrayStatus::Failed as u8);
    }
}
