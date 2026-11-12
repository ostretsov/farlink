//! Remembered window positions for each remote machine and monitor, so the
//! viewer reopens screens exactly where they were left.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Where a remote-monitor window sits on the local desktop (in points).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowPlace {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default)]
    pub maximized: bool,
}

impl WindowPlace {
    /// Fix values that cannot be a real window (NaN, tiny, absurdly far away).
    pub fn sanitized(mut self) -> WindowPlace {
        let fix = |v: f32, d: f32| if v.is_finite() { v } else { d };
        self.x = fix(self.x, 40.0).clamp(-16_000.0, 16_000.0);
        self.y = fix(self.y, 40.0).clamp(-16_000.0, 16_000.0);
        self.w = fix(self.w, 960.0).clamp(320.0, 16_000.0);
        self.h = fix(self.h, 600.0).clamp(200.0, 16_000.0);
        self
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutStore {
    /// machine key -> monitor fingerprint -> place
    pub machines: BTreeMap<String, BTreeMap<String, WindowPlace>>,
}

impl LayoutStore {
    pub fn load(path: &Path) -> LayoutStore {
        crate::config::load_json(path)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        crate::config::save_json(path, self)
    }

    pub fn get(&self, machine: &str, monitor: &str) -> Option<WindowPlace> {
        self.machines.get(machine)?.get(monitor).map(|p| p.sanitized())
    }

    /// Remember a place. Returns true if something changed (small jitters of
    /// less than one point are ignored so we don't write the file constantly).
    pub fn set(&mut self, machine: &str, monitor: &str, place: WindowPlace) -> bool {
        let place = place.sanitized();
        let entry = self.machines.entry(machine.to_string()).or_default();
        match entry.get(monitor) {
            Some(old)
                if (old.x - place.x).abs() < 1.0
                    && (old.y - place.y).abs() < 1.0
                    && (old.w - place.w).abs() < 1.0
                    && (old.h - place.h).abs() < 1.0
                    && old.fullscreen == place.fullscreen
                    && old.maximized == place.maximized =>
            {
                false
            }
            _ => {
                entry.insert(monitor.to_string(), place);
                true
            }
        }
    }

    pub fn forget(&mut self, machine: &str) -> bool {
        self.machines.remove(machine).is_some()
    }

    /// Forget where one monitor's window goes (it opens side by side again).
    pub fn forget_monitor(&mut self, machine: &str, monitor: &str) -> bool {
        let Some(m) = self.machines.get_mut(machine) else { return false };
        let removed = m.remove(monitor).is_some();
        if m.is_empty() {
            self.machines.remove(machine);
        }
        removed
    }

    pub fn count(&self, machine: &str) -> usize {
        self.machines.get(machine).map_or(0, |m| m.len())
    }
}

/// First-time placement: remote monitors side by side, in their real order,
/// each sized to keep the remote aspect ratio within `area` (local screen).
pub fn default_place(index: usize, count: usize, remote_w: u32, remote_h: u32, area: (f32, f32)) -> WindowPlace {
    let count = count.max(1) as f32;
    let margin = 24.0;
    let slot_w = ((area.0 - margin * (count + 1.0)) / count).max(320.0);
    let slot_h = (area.1 - 120.0).max(240.0);
    let aspect = remote_w.max(1) as f32 / remote_h.max(1) as f32;
    let (mut w, mut h) = (slot_w, slot_w / aspect);
    if h > slot_h {
        h = slot_h;
        w = h * aspect;
    }
    WindowPlace {
        x: margin + index as f32 * (slot_w + margin),
        y: 60.0,
        w: w.round(),
        h: (h + 36.0).round(), // + toolbar
        fullscreen: false,
        maximized: false,
    }
    .sanitized()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(x: f32) -> WindowPlace {
        WindowPlace { x, y: 10.0, w: 800.0, h: 600.0, fullscreen: false, maximized: false }
    }

    #[test]
    fn remembers_per_machine_and_monitor_and_survives_disk() {
        let dir = crate::util::unique_temp_dir("farlink-layout");
        let path = dir.join("layouts.json");
        let mut s = LayoutStore::default();
        assert!(s.set("pc-1", "0:1920x1080@0,0", place(100.0)));
        assert!(s.set("pc-1", "1:1920x1080@1920,0", place(2000.0)));
        assert!(s.set("pc-2", "0:2560x1440@0,0", place(5.0)));
        s.save(&path).unwrap();
        let back = LayoutStore::load(&path);
        assert_eq!(back, s);
        assert_eq!(back.get("pc-1", "1:1920x1080@1920,0").unwrap().x, 2000.0);
        assert_eq!(back.count("pc-1"), 2);
        assert!(back.get("pc-1", "other-monitor").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tiny_moves_are_not_changes() {
        let mut s = LayoutStore::default();
        assert!(s.set("m", "a", place(100.0)));
        assert!(!s.set("m", "a", place(100.4)));
        assert!(s.set("m", "a", place(140.0)));
        let mut fs = place(140.0);
        fs.fullscreen = true;
        assert!(s.set("m", "a", fs));
    }

    #[test]
    fn forget_machine() {
        let mut s = LayoutStore::default();
        s.set("m", "a", place(1.0));
        assert!(s.forget("m"));
        assert!(!s.forget("m"));
        let p = default_place(0, 1, 1920, 1080, (1920.0, 1080.0));
        s.set("m", "a", p);
        s.set("m", "b", p);
        assert!(s.forget_monitor("m", "a"));
        assert!(!s.forget_monitor("m", "a"));
        assert_eq!(s.count("m"), 1);
        assert!(s.forget_monitor("m", "b"));
        assert!(s.machines.is_empty());
        assert_eq!(s.count("m"), 0);
    }

    #[test]
    fn broken_values_are_repaired() {
        let p = WindowPlace { x: f32::NAN, y: -99_999.0, w: 5.0, h: f32::INFINITY, fullscreen: false, maximized: false }.sanitized();
        assert_eq!(p.x, 40.0);
        assert_eq!(p.y, -16_000.0);
        assert_eq!(p.w, 320.0);
        assert_eq!(p.h, 600.0);
    }

    #[test]
    fn missing_or_corrupt_file_gives_empty_store() {
        let dir = crate::util::unique_temp_dir("farlink-layout");
        assert_eq!(LayoutStore::load(&dir.join("none.json")), LayoutStore::default());
        std::fs::write(dir.join("bad.json"), "{not json").unwrap();
        assert_eq!(LayoutStore::load(&dir.join("bad.json")), LayoutStore::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn default_places_side_by_side_keep_aspect() {
        let a = default_place(0, 2, 1920, 1080, (2560.0, 1440.0));
        let b = default_place(1, 2, 1920, 1080, (2560.0, 1440.0));
        assert!(b.x >= a.x + a.w, "second window to the right of the first");
        let ratio = a.w / (a.h - 36.0);
        assert!((ratio - 16.0 / 9.0).abs() < 0.02);
    }
}
