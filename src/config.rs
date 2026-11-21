//! Settings files of the three programs.

use crate::quality::Quality;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::{Path, PathBuf};

/// Read a JSON file; a missing or broken file gives the default value.
pub fn load_json<T: DeserializeOwned + Default>(path: &Path) -> T {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            log::warn!("ignoring unreadable {}: {e}", path.display());
            T::default()
        }),
        Err(_) => T::default(),
    }
}

/// Write a JSON file atomically (temp file + rename) so a crash never leaves
/// half a file behind.
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", crate::util::random_token(4)));
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    make_private(&tmp, path);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Settings files hold secrets (machine keys, sign-ins, link tokens): only
/// their owner may read them. A file replaced by someone else (root running
/// `farlink-server set-password`) keeps its old owner, so the service can
/// still read it.
#[cfg(unix)]
fn make_private(tmp: &Path, target: &Path) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let _ = std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o600));
    if let Ok(old) = std::fs::metadata(target) {
        let _ = std::os::unix::fs::chown(tmp, Some(old.uid()), Some(old.gid()));
    }
}

#[cfg(not(unix))]
fn make_private(_tmp: &Path, _target: &Path) {}

// ---------------------------------------------------------------- host

/// Settings of the helper on a remote machine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostConfig {
    pub server_url: String,
    /// One-time enrolment token from the install link.
    #[serde(default)]
    pub enroll_token: Option<String>,
    /// Identity given by the server after enrolment.
    #[serde(default)]
    pub machine_id: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
}

impl HostConfig {
    pub fn is_enrolled(&self) -> bool {
        self.machine_id.is_some() && self.secret.is_some()
    }
}

/// Where the host keeps its settings.
/// Windows: `%ProgramData%\Farlink\host.json` (only SYSTEM/Administrators).
/// Linux: `~/.config/farlink/host.json`, seeded from `/etc/farlink/host.json`.
pub fn host_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("FARLINK_HOST_CONFIG") {
        return PathBuf::from(p);
    }
    if cfg!(windows) {
        let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
        PathBuf::from(base).join("Farlink").join("host.json")
    } else {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("farlink").join("host.json")
    }
}

/// System-wide seed written by the Linux installer.
pub const LINUX_HOST_SEED: &str = "/etc/farlink/host.json";

/// Load the host settings, falling back to the installer's seed file.
pub fn load_host_config() -> (PathBuf, HostConfig) {
    let path = host_config_path();
    let mut cfg: HostConfig = load_json(&path);
    if cfg.server_url.is_empty() && cfg!(target_os = "linux") {
        let seed: HostConfig = load_json(Path::new(LINUX_HOST_SEED));
        if !seed.server_url.is_empty() {
            cfg = seed;
        }
    }
    (path, cfg)
}

// ---------------------------------------------------------------- viewer

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewerConfig {
    #[serde(default)]
    pub server_url: String,
    /// Admin session token from signing in (never the password).
    #[serde(default)]
    pub admin_token: Option<String>,
    /// Sign-in name (empty = administrator).
    #[serde(default)]
    pub user_name: String,
    #[serde(default, deserialize_with = "upgrade_quality")]
    pub quality: Quality,
    /// Machines open in view-only mode by default.
    #[serde(default)]
    pub view_only_default: bool,
    /// Temporary links opened in this app (newest first).
    #[serde(default)]
    pub recent_links: Vec<String>,
    /// Check for viewer updates in the background.
    #[serde(default = "yes")]
    pub auto_update: bool,
    /// How remote screens are drawn in their windows (last choice is remembered).
    #[serde(default)]
    pub scale_mode: ScaleMode,
    /// "Start at login" was switched on once (on the first start); after that
    /// only the tray menu changes it.
    #[serde(default)]
    pub autostart_set_up: bool,
    /// Screen windows show only the remote screen (the controls come back
    /// while the mouse rests at the top edge).
    #[serde(default)]
    pub hide_controls: bool,
    /// File browser, this computer's side: the folder it was last in and
    /// whether hidden files are shown.
    #[serde(default)]
    pub files_local: FilesSide,
    /// File browser, per remote computer (by machine id): the same.
    #[serde(default)]
    pub files_remote: std::collections::BTreeMap<String, FilesSide>,
}

/// What the file browser remembers about one side.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesSide {
    /// Last folder ("" = none yet).
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub show_hidden: bool,
}

/// How a remote screen is drawn in its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleMode {
    /// One remote pixel = one local pixel; the window is sized to the remote
    /// screen, scroll bars if it does not fit, black around it if bigger.
    #[default]
    Original,
    /// Stretch (keeping the aspect ratio) to fill the window.
    Fit,
}

impl ScaleMode {
    pub fn label(self) -> &'static str {
        match self {
            ScaleMode::Original => "Original size",
            ScaleMode::Fit => "Fit to window",
        }
    }
}

fn yes() -> bool {
    true
}

/// Saved 0.1 presets become the new (sharper, faster) presets.
fn upgrade_quality<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Quality, D::Error> {
    Ok(Quality::deserialize(d)?.upgraded())
}

impl Default for ViewerConfig {
    fn default() -> Self {
        ViewerConfig {
            server_url: String::new(),
            admin_token: None,
            user_name: String::new(),
            quality: Quality::default(),
            view_only_default: false,
            recent_links: Vec::new(),
            auto_update: true,
            scale_mode: ScaleMode::default(),
            autostart_set_up: false,
            hide_controls: false,
            files_local: Default::default(),
            files_remote: Default::default(),
        }
    }
}

impl ViewerConfig {
    pub fn remember_link(&mut self, link: &str) {
        self.recent_links.retain(|l| l != link);
        self.recent_links.insert(0, link.to_string());
        self.recent_links.truncate(8);
    }
}

pub fn viewer_dir() -> PathBuf {
    if let Ok(p) = std::env::var("FARLINK_VIEWER_DIR") {
        return PathBuf::from(p);
    }
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("farlink")
}

// ---------------------------------------------------------------- server

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Public address people and machines use, e.g. `https://rd.example.com`.
    pub public_url: String,
    /// Address to listen on: `0.0.0.0:443` with built-in HTTPS, or a local
    /// port behind an HTTPS proxy.
    pub bind: String,
    /// Where the state file lives.
    pub data_dir: PathBuf,
    /// Published program files: `<releases>/<target>/<file>` and
    /// `<releases>/version.txt`.
    pub releases_dir: PathBuf,
    /// HTTPS on `bind` (off: plain HTTP, e.g. behind a proxy or on a LAN).
    #[serde(default)]
    pub tls: Tls,
    /// GitHub repository (`owner/repo`) whose releases this server installs
    /// on its own: itself plus the programs for computers. `None`: manual
    /// updates only (`farlink-server publish`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_from_github: Option<String>,
    /// UDP port of the STUN answerer that lets viewers and helpers connect
    /// directly (peer-to-peer, see `crate::direct`); `0` = direct connections
    /// off, everything goes through this relay.
    #[serde(default = "default_stun_port")]
    pub stun_port: u16,
    /// Public STUN servers (`host:port`) viewers and helpers also ask, to
    /// tell a "hard" NAT (a new port for every destination: hole punching
    /// can't work, so they stay on the relay). Empty: only this relay is
    /// asked, and hard NATs just fail to connect directly after a few seconds.
    /// These servers see the public addresses of viewers and helpers.
    #[serde(default = "default_extra_stun")]
    pub extra_stun_servers: Vec<String>,
}

fn default_stun_port() -> u16 {
    crate::stun::DEFAULT_PORT
}

pub fn default_extra_stun() -> Vec<String> {
    vec!["stun.cloudflare.com:3478".into(), "stun.l.google.com:19302".into()]
}

/// Where the server's HTTPS certificate comes from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Tls {
    /// Plain HTTP.
    #[default]
    Off,
    /// Let's Encrypt, obtained and renewed automatically. Let's Encrypt checks
    /// the domain on port 443, so `bind` must be port 443 (or 443 forwarded to
    /// it) and DNS for `domain` must point here.
    Auto {
        domain: String,
        /// Optional contact address given to Let's Encrypt.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        email: String,
        /// Use the Let's Encrypt test service (certificates browsers don't trust).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        staging: bool,
    },
    /// PEM files you manage yourself (full chain + private key). Read at start.
    Files { cert: PathBuf, key: PathBuf },
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            public_url: "http://127.0.0.1:8470".into(),
            bind: "127.0.0.1:8470".into(),
            data_dir: PathBuf::from("farlink-data"),
            releases_dir: PathBuf::from("farlink-data/releases"),
            tls: Tls::Off,
            update_from_github: None,
            stun_port: crate::stun::DEFAULT_PORT,
            extra_stun_servers: default_extra_stun(),
        }
    }
}

pub const SERVER_CONFIG_PATH: &str = "/etc/farlink-server/config.json";

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn saved_settings_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::util::unique_temp_dir("farlink-private");
        let p = dir.join("state.json");
        std::fs::write(&p, "{}").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        save_json(&p, &serde_json::json!({"secret": "x"})).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600, "replaced file is owner-only");
        let fresh = dir.join("new.json");
        save_json(&fresh, &1).unwrap();
        assert_eq!(std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_round_trip_and_atomic_write() {
        let dir = crate::util::unique_temp_dir("farlink-cfg");
        let p = dir.join("sub").join("host.json");
        let cfg = HostConfig {
            server_url: "https://rd.example.com".into(),
            enroll_token: Some("t".into()),
            machine_id: None,
            secret: None,
        };
        save_json(&p, &cfg).unwrap();
        assert_eq!(load_json::<HostConfig>(&p), cfg);
        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temp files left");
        assert!(!cfg.is_enrolled());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn server_tls_settings() {
        let old: ServerConfig =
            serde_json::from_str(r#"{"public_url":"https://x","bind":"127.0.0.1:8470","data_dir":"/d","releases_dir":"/d/r"}"#).unwrap();
        assert_eq!(old.tls, Tls::Off, "configs written before built-in HTTPS still load");
        let auto: ServerConfig = serde_json::from_str(
            r#"{"public_url":"https://x","bind":"0.0.0.0:443","data_dir":"/d","releases_dir":"/d/r",
                "tls":{"mode":"auto","domain":"rd.example.com"}}"#,
        )
        .unwrap();
        assert_eq!(auto.tls, Tls::Auto { domain: "rd.example.com".into(), email: String::new(), staging: false });
        let back: ServerConfig = serde_json::from_str(&serde_json::to_string(&auto).unwrap()).unwrap();
        assert_eq!(back, auto);
        let files: Tls = serde_json::from_str(r#"{"mode":"files","cert":"/c.pem","key":"/k.pem"}"#).unwrap();
        assert_eq!(files, Tls::Files { cert: "/c.pem".into(), key: "/k.pem".into() });
    }

    #[test]
    fn old_viewer_files_still_load() {
        let v: ViewerConfig = serde_json::from_str(r#"{"server_url":"https://x"}"#).unwrap();
        assert!(v.auto_update);
        assert_eq!(v.quality, Quality::default());
    }

    #[test]
    fn hidden_controls_are_remembered() {
        let v: ViewerConfig = serde_json::from_str(r#"{"server_url":"https://x"}"#).unwrap();
        assert!(!v.hide_controls, "controls shown unless hidden before");
        let v = ViewerConfig { hide_controls: true, ..ViewerConfig::default() };
        let back: ViewerConfig = serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
        assert!(back.hide_controls);
    }

    #[test]
    fn recent_links_are_unique_and_bounded() {
        let mut v = ViewerConfig::default();
        for i in 0..12 {
            v.remember_link(&format!("https://x/g/{i}"));
        }
        v.remember_link("https://x/g/5");
        assert_eq!(v.recent_links.len(), 8);
        assert_eq!(v.recent_links[0], "https://x/g/5");
        assert_eq!(v.recent_links.iter().filter(|l| *l == "https://x/g/5").count(), 1);
    }
}
