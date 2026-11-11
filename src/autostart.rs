//! "Start at login" for the desktop app. It starts hidden in the tray
//! (`farlink --tray`).
//!
//! * Linux: an XDG autostart entry `~/.config/autostart/farlink.desktop`
//!   (GNOME and every other freedesktop session run these after login).
//! * Windows: the value `Farlink` under
//!   `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

use std::path::Path;

/// The command-line switch for starting hidden in the tray.
pub const TRAY_SWITCH: &str = "--tray";

/// Whether the app starts at login (for this exe or any other).
pub fn is_enabled() -> bool {
    imp::read().is_some_and(|entry| entry_enabled(&entry))
}

/// Turn starting at login on (for `exe`) or off.
pub fn set(enabled: bool, exe: &Path) -> anyhow::Result<()> {
    if enabled { imp::write(exe) } else { imp::remove() }
}

/// Points an enabled entry at `exe` if it runs another file (the app moved).
pub fn refresh(exe: &Path) -> anyhow::Result<()> {
    match imp::read() {
        Some(entry) if entry_enabled(&entry) && !entry.contains(&command(exe)) => imp::write(exe),
        _ => Ok(()),
    }
}

/// The command that starts `exe` hidden in the tray.
pub fn command(exe: &Path) -> String {
    format!("{} {TRAY_SWITCH}", quote(&exe.to_string_lossy()))
}

/// `Hidden=true` deletes an XDG entry; `X-GNOME-Autostart-enabled=false` is
/// what GNOME's "Startup Applications" writes when switched off.
fn entry_enabled(text: &str) -> bool {
    !text.lines().map(|l| l.replace(' ', "")).any(|l| {
        l.eq_ignore_ascii_case("Hidden=true") || l.eq_ignore_ascii_case("X-GNOME-Autostart-enabled=false")
    })
}

/// Quotes a path for an XDG `Exec` key or a Windows command line.
fn quote(arg: &str) -> String {
    let plain = !arg.is_empty() && arg.chars().all(|c| c.is_alphanumeric() || "/\\._-+,:@".contains(c));
    if plain && !cfg!(windows) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            // Desktop Entry Specification: escape inside quotes, `%` doubled.
            '"' | '`' | '$' if !cfg!(windows) => {
                out.push_str("\\\\");
                out.push(c);
            }
            '%' if !cfg!(windows) => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={product}\n\
         Comment=See and control your computers (starts in the tray)\n\
         Exec={exec}\n\
         Terminal=false\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n",
        product = crate::PRODUCT,
        exec = command(exe),
    )
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    use std::path::PathBuf;

    pub fn entry_path() -> PathBuf {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        config.join("autostart").join("farlink.desktop")
    }

    pub fn read() -> Option<String> {
        std::fs::read_to_string(entry_path()).ok()
    }

    pub fn write(exe: &Path) -> anyhow::Result<()> {
        let path = entry_path();
        std::fs::create_dir_all(path.parent().expect("has a parent"))?;
        std::fs::write(&path, desktop_entry(exe))?;
        Ok(())
    }

    pub fn remove() -> anyhow::Result<()> {
        match std::fs::remove_file(entry_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub fn read() -> Option<String> {
        let (key, name) = (wide(RUN_KEY), wide(crate::PRODUCT));
        let mut buf = vec![0u16; 2048];
        let mut bytes = (buf.len() * 2) as u32;
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    pub fn write(exe: &Path) -> anyhow::Result<()> {
        let (key, name, value) = (wide(RUN_KEY), wide(crate::PRODUCT), wide(&command(exe)));
        let rc = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            )
        };
        anyhow::ensure!(rc == ERROR_SUCCESS, "could not write the Run registry value (error {rc})");
        Ok(())
    }

    pub fn remove() -> anyhow::Result<()> {
        let (key, name) = (wide(RUN_KEY), wide(crate::PRODUCT));
        let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
        // 2 = ERROR_FILE_NOT_FOUND: already off.
        anyhow::ensure!(rc == ERROR_SUCCESS || rc == 2, "could not remove the Run registry value (error {rc})");
        Ok(())
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn entry_starts_the_app_in_the_tray() {
        let e = desktop_entry(Path::new("/home/a/bin/farlink"));
        assert!(e.starts_with("[Desktop Entry]\n"));
        assert!(e.contains("\nExec=/home/a/bin/farlink --tray\n"));
        assert!(entry_enabled(&e));
    }

    #[test]
    fn exec_is_quoted_when_needed() {
        assert_eq!(quote("/opt/my apps/farlink"), "\"/opt/my apps/farlink\"");
        assert_eq!(quote("/a/$x"), "\"/a/\\\\$x\"");
        assert_eq!(quote("/a/100%"), "\"/a/100%%\"");
    }

    #[test]
    fn switched_off_entries_count_as_off() {
        assert!(!entry_enabled("[Desktop Entry]\nHidden=true\n"));
        assert!(!entry_enabled("[Desktop Entry]\nX-GNOME-Autostart-enabled = false\n"));
    }
}
