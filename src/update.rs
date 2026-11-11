//! Background self-update: ask the server for the published version, download,
//! check the SHA-256, swap the program file. The service manager (systemd or
//! the Windows service) starts the new version.

use crate::util::{sha256_hex, url_encode};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Exit code meaning "updated, please start me again".
pub const EXIT_UPDATED: i32 = 42;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub version: String,
    pub sha256: String,
    pub size: u64,
    /// Path on the server, e.g. `/download/x86_64-pc-windows-gnu/farlink-host.exe`.
    pub path: String,
}

/// Ask the server what is published for `app` on this platform.
/// Returns `Some` only if it is newer than the running version.
pub fn check(server: &str, app: &str, target: &str, installed: &str) -> Result<Option<UpdateInfo>> {
    Ok(published(server, app, target)?.filter(|info| crate::version::is_newer(&info.version, installed)))
}

/// What the server publishes for `app` on `target` (`None`: nothing for this platform).
pub fn published(server: &str, app: &str, target: &str) -> Result<Option<UpdateInfo>> {
    let url = format!(
        "{}/api/update?app={}&target={}",
        server.trim_end_matches('/'),
        url_encode(app),
        url_encode(target)
    );
    let mut resp = match ureq::get(&url).call() {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(e) => return Err(e).context("asking server for updates"),
    };
    Ok(Some(resp.body_mut().read_json()?))
}

pub fn download(server: &str, info: &UpdateInfo) -> Result<Vec<u8>> {
    let url = format!("{}{}", server.trim_end_matches('/'), info.path);
    let bytes = ureq::get(&url)
        .call()
        .context("downloading update")?
        .body_mut()
        .with_config()
        .limit(400 * 1024 * 1024)
        .read_to_vec()?;
    verify(&bytes, info)?;
    Ok(bytes)
}

/// Make sure downloaded bytes are exactly what the server published.
pub fn verify(bytes: &[u8], info: &UpdateInfo) -> Result<()> {
    if bytes.len() as u64 != info.size {
        bail!("update has wrong size ({} instead of {})", bytes.len(), info.size);
    }
    let got = sha256_hex(bytes);
    if !got.eq_ignore_ascii_case(&info.sha256) {
        bail!("update checksum mismatch");
    }
    Ok(())
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

/// Replace the program file. Works while it is running: on Windows a running
/// program may be renamed (not overwritten), so the old one is moved aside.
pub fn replace_exe(exe: &Path, new_bytes: &[u8]) -> Result<()> {
    let new = sibling(exe, ".new");
    let old = sibling(exe, ".old");
    std::fs::write(&new, new_bytes).with_context(|| format!("writing {}", new.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    }
    let _ = std::fs::remove_file(&old);
    if exe.exists() {
        std::fs::rename(exe, &old).context("moving old version aside")?;
    }
    if let Err(e) = std::fs::rename(&new, exe) {
        // Put the old one back so the machine keeps working.
        let _ = std::fs::rename(&old, exe);
        return Err(e).context("installing new version");
    }
    Ok(())
}

/// Remove the previous version left by [`replace_exe`] (call at start-up).
pub fn cleanup_old(exe: &Path) {
    let _ = std::fs::remove_file(sibling(exe, ".old"));
    let _ = std::fs::remove_file(sibling(exe, ".new"));
}

/// Check + download + install in one go. Returns the new version if updated.
pub fn update_self(server: &str, app: &str) -> Result<Option<String>> {
    let target = crate::util::current_target();
    let Some(info) = check(server, app, target, crate::VERSION)? else {
        return Ok(None);
    };
    install(server, app, &info).map(Some)
}

/// Install whatever version the server publishes, as long as it differs from
/// the running one (the administrator asked for it, so also an older one).
pub fn install_published(server: &str, app: &str) -> Result<String> {
    let target = crate::util::current_target();
    let Some(info) = published(server, app, target)? else {
        bail!("the server has no {app} for {target}");
    };
    if info.version == crate::VERSION {
        bail!("already running {}", info.version);
    }
    install(server, app, &info)
}

fn install(server: &str, app: &str, info: &UpdateInfo) -> Result<String> {
    let bytes = download(server, info)?;
    let exe = std::env::current_exe()?;
    replace_exe(&exe, &bytes)?;
    log::info!("updated {app} {} -> {}", crate::VERSION, info.version);
    Ok(info.version.clone())
}

// ------------------------------------------------------------ Debian package

/// Where the .deb installs the desktop app. An app running from here was
/// installed with the package, so it updates with a newer package (the
/// program file belongs to root and dpkg must know about the new version).
pub const DEB_EXE: &str = "/usr/bin/farlink";

pub fn installed_from_deb(exe: &Path) -> bool {
    cfg!(target_os = "linux") && exe == Path::new(DEB_EXE)
}

/// Download a published .deb into the user's cache folder (not /tmp: root
/// installs it, so nobody else may be able to swap the file).
pub fn download_deb(server: &str, info: &UpdateInfo) -> Result<PathBuf> {
    let bytes = download(server, info)?; // checks size and SHA-256
    let dir = dirs::cache_dir().context("no cache folder")?.join("farlink");
    std::fs::create_dir_all(&dir)?;
    // Older downloads are no use any more.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().ends_with(".deb") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let path = dir.join(format!("farlink_{}.deb", info.version));
    std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Install a downloaded .deb as root. `pkexec` asks for the password in a
/// desktop dialog; apt also pulls in any new dependencies.
pub fn install_deb(path: &Path) -> Result<()> {
    let status = std::process::Command::new("pkexec")
        .arg("apt-get")
        .args(["install", "-y", "--no-install-recommends"])
        .arg(path)
        .status()
        .with_context(|| format!("could not start pkexec; install it with: sudo apt install {}", path.display()))?;
    match status.code() {
        Some(0) => Ok(()),
        Some(126) => bail!("cancelled"),
        Some(127) => bail!("not allowed (no password dialog?); install it with: sudo apt install {}", path.display()),
        _ => bail!("apt-get failed ({status})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_for(bytes: &[u8]) -> UpdateInfo {
        UpdateInfo { version: "9.9.9".into(), sha256: sha256_hex(bytes), size: bytes.len() as u64, path: "/x".into() }
    }

    #[test]
    fn deb_install_is_recognised_by_its_path() {
        assert_eq!(installed_from_deb(Path::new("/usr/bin/farlink")), cfg!(target_os = "linux"));
        assert!(!installed_from_deb(Path::new("/home/a/.local/bin/farlink")));
        assert!(!installed_from_deb(Path::new("/usr/bin/farlink.old")));
    }

    #[test]
    fn verify_checks_size_and_hash() {
        let data = b"new program";
        let info = info_for(data);
        assert!(verify(data, &info).is_ok());
        assert!(verify(b"new program!", &info).is_err());
        assert!(verify(b"NEW PROGRAM", &info).is_err());
    }

    #[test]
    fn replace_keeps_running_copy_aside_then_cleans_up() {
        let dir = crate::util::unique_temp_dir("farlink-upd");
        let exe = dir.join("farlink-host");
        std::fs::write(&exe, b"v1").unwrap();
        replace_exe(&exe, b"v2").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"v2");
        assert_eq!(std::fs::read(dir.join("farlink-host.old")).unwrap(), b"v1");
        cleanup_old(&exe);
        assert!(!dir.join("farlink-host.old").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o755);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn replace_works_when_exe_missing() {
        let dir = crate::util::unique_temp_dir("farlink-upd");
        let exe = dir.join("prog.exe");
        replace_exe(&exe, b"fresh").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"fresh");
        let _ = std::fs::remove_dir_all(dir);
    }
}
