//! Linux tray: a StatusNotifierItem over D-Bus (ksni). Ubuntu shows it with
//! the "Ubuntu AppIndicators" GNOME extension (on by default); KDE, XFCE and
//! others have SNI trays too.

use super::{Shared, TrayEvent, TrayStatus, icon};
use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Icon, MenuItem, ToolTip};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Pixmap sizes offered to the panel; it picks the closest one.
const ICON_SIZES: [u32; 4] = [16, 22, 32, 48];

pub struct SniTray {
    shared: Arc<Shared>,
}

impl ksni::Tray for SniTray {
    fn id(&self) -> String {
        "farlink".into()
    }

    fn title(&self) -> String {
        crate::PRODUCT.into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        ICON_SIZES
            .iter()
            .map(|&size| Icon { width: size as i32, height: size as i32, data: icon::argb(size) })
            .collect()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip { title: crate::PRODUCT.into(), description: "See and control your computers".into(), ..Default::default() }
    }

    // Left click opens the window (trays that call Activate do this).
    fn activate(&mut self, _x: i32, _y: i32) {
        self.shared.send(TrayEvent::Open);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: format!("Open {}", crate::PRODUCT),
                activate: Box::new(|t: &mut Self| t.shared.send(TrayEvent::Open)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Start at login".into(),
                checked: self.shared.autostart.load(Ordering::SeqCst),
                activate: Box::new(|t: &mut Self| t.shared.send(TrayEvent::ToggleAutostart)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| t.shared.send(TrayEvent::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

#[derive(Clone)]
pub struct Handle(Arc<Mutex<Option<ksni::blocking::Handle<SniTray>>>>);

pub fn start(shared: Arc<Shared>, retry_for: Duration) -> Handle {
    let handle = Handle(Arc::new(Mutex::new(None)));
    let h = handle.clone();
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            let started = Instant::now();
            loop {
                match (SniTray { shared: shared.clone() }).spawn() {
                    Ok(tray) => {
                        if shared.stop.load(Ordering::SeqCst) {
                            tray.shutdown().wait();
                            return;
                        }
                        *h.0.lock().unwrap() = Some(tray);
                        shared.set_status(TrayStatus::Ready);
                        return;
                    }
                    Err(e) if started.elapsed() >= retry_for || shared.stop.load(Ordering::SeqCst) => {
                        log::warn!("no system tray: {e}");
                        shared.set_status(TrayStatus::Failed);
                        return;
                    }
                    Err(e) => {
                        log::info!("tray not ready yet: {e}");
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            }
        })
        .expect("start the tray thread");
    handle
}

impl Handle {
    /// Asks the panel to read the menu again.
    pub fn refresh(&self) {
        if let Some(t) = self.0.lock().unwrap().as_ref() {
            t.update(|_| {});
        }
    }

    pub fn shutdown(&self) {
        if let Some(t) = self.0.lock().unwrap().take() {
            t.shutdown().wait();
        }
    }
}
