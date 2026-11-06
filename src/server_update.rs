//! The server keeps itself and the computers up to date from GitHub Releases.
//!
//! Every hour (and at start) it asks GitHub for the latest release of the
//! configured repository. When that is newer, it downloads the single-file
//! programs the release workflow uploads, checks each against the release's
//! `SHA256SUMS.txt`, swaps its own program file, publishes the computer
//! programs into the releases folder (connected computers then update
//! themselves) and restarts in place.
//!
//! Order matters: the server is replaced before the computers are offered the
//! new version, so computers never run ahead of their server.

use crate::util::sha256_hex;
use crate::version::Version;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const GITHUB_API: &str = "https://api.github.com";
const CHECK_EVERY: Duration = Duration::from_secs(60 * 60);
const SUMS: &str = "SHA256SUMS.txt";
/// Set across the restart: the version the new program file should be.
const UPDATED_TO: &str = "FARLINK_UPDATED_TO";

/// Release asset name (see `.github/workflows/build.yml`) → (target, file name).
const ASSETS: &[(&str, &str, &str)] = &[
    ("farlink-linux-x86_64", "x86_64-unknown-linux-gnu", "farlink"),
    ("farlink-host-linux-x86_64", "x86_64-unknown-linux-gnu", "farlink-host"),
    ("farlink-server-linux-x86_64", "x86_64-unknown-linux-gnu", "farlink-server"),
    ("farlink-linux-arm64", "aarch64-unknown-linux-gnu", "farlink"),
    ("farlink-host-linux-arm64", "aarch64-unknown-linux-gnu", "farlink-host"),
    ("farlink-server-linux-arm64", "aarch64-unknown-linux-gnu", "farlink-server"),
    ("farlink-windows-x86_64.exe", "x86_64-pc-windows-gnu", "farlink.exe"),
    ("farlink-host-windows-x86_64.exe", "x86_64-pc-windows-gnu", "farlink-host.exe"),
];

/// The latest release: its version and asset name → download address.
#[derive(Debug, Clone)]
pub struct Latest {
    pub version: String,
    pub assets: HashMap<String, String>,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// `owner/repo` with plain GitHub name characters only.
pub fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |p: Option<&str>| {
        p.is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)))
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(300))).build().into()
}

/// Ask GitHub for the latest (non-draft, non-pre-release) release of `repo`.
pub fn fetch_latest(api: &str, repo: &str) -> Result<Latest> {
    if !valid_repo(repo) {
        bail!("'{repo}' is not a GitHub repository name like owner/farlink");
    }
    let url = format!("{}/repos/{repo}/releases/latest", api.trim_end_matches('/'));
    let rel: GhRelease = agent()
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "farlink-server")
        .call()
        .with_context(|| format!("asking {url}"))?
        .body_mut()
        .read_json()?;
    if rel.draft || rel.prerelease {
        bail!("latest release {} is a draft or pre-release", rel.tag_name);
    }
    let version = rel.tag_name.trim_start_matches('v').to_string();
    if Version::parse(&version).is_none() {
        bail!("release tag '{}' is not a version like v1.2.0", rel.tag_name);
    }
    Ok(Latest { version, assets: rel.assets.into_iter().map(|a| (a.name, a.browser_download_url)).collect() })
}

fn download(url: &str) -> Result<Vec<u8>> {
    Ok(agent()
        .get(url)
        .header("User-Agent", "farlink-server")
        .call()
        .with_context(|| format!("downloading {url}"))?
        .body_mut()
        .with_config()
        .limit(400 * 1024 * 1024)
        .read_to_vec()?)
}

/// `sha256sum` output: `<hex>  <name>` (binary mode `<hex> *<name>`).
pub fn parse_sums(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let (hash, name) = l.trim().split_once(char::is_whitespace)?;
            Some((name.trim().trim_start_matches('*').to_string(), hash.to_ascii_lowercase()))
        })
        .collect()
}

/// What an update did.
#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    pub server_replaced: bool,
    pub published: Option<String>,
}

fn is_newer(a: &str, b: Option<&str>) -> bool {
    match (Version::parse(a), b.and_then(Version::parse)) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Bring this server and its releases folder up to `latest`.
/// `server_exe`: the program file to replace (`None` = leave the server alone).
pub fn apply(
    latest: &Latest,
    running: &str,
    published: Option<&str>,
    releases_dir: &Path,
    server_exe: Option<(&Path, &str)>,
) -> Result<Applied> {
    let mut done = Applied::default();
    let server_needed = server_exe.is_some() && is_newer(&latest.version, Some(running));
    let publish_needed = is_newer(&latest.version, published);
    if !server_needed && !publish_needed {
        return Ok(done);
    }
    let sums_url = latest.assets.get(SUMS).with_context(|| format!("release {} has no {SUMS}", latest.version))?;
    let sums = parse_sums(&String::from_utf8_lossy(&download(sums_url)?));
    let fetch = |asset: &str| -> Result<Vec<u8>> {
        let url = latest.assets.get(asset).with_context(|| format!("release {} has no {asset}", latest.version))?;
        let want = sums.get(asset).with_context(|| format!("{asset} is not listed in {SUMS}"))?;
        let bytes = download(url)?;
        if sha256_hex(&bytes) != *want {
            bail!("{asset}: checksum does not match {SUMS}");
        }
        Ok(bytes)
    };

    // Download and check everything first, so a broken release changes nothing.
    let server_bytes = match server_exe {
        Some((_, target)) if server_needed => {
            let asset = ASSETS.iter().find(|(_, t, f)| *t == target && *f == "farlink-server").map(|a| a.0);
            Some(fetch(asset.with_context(|| format!("no server program for {target}"))?)?)
        }
        _ => None,
    };
    let mut programs = vec![];
    if publish_needed {
        for (asset, target, file) in ASSETS.iter().filter(|a| a.2 != "farlink-server") {
            if latest.assets.contains_key(*asset) {
                programs.push((*target, *file, fetch(asset)?));
            } else {
                log::warn!("release {} has no {asset}; {target} computers stay on their version", latest.version);
            }
        }
        if programs.is_empty() {
            bail!("release {} has no programs for computers", latest.version);
        }
    }

    if let (Some(bytes), Some((exe, _))) = (server_bytes, server_exe) {
        crate::update::replace_exe(exe, &bytes).with_context(|| format!("replacing {}", exe.display()))?;
        done.server_replaced = true;
    }
    if publish_needed {
        for (target, file, bytes) in programs {
            let dir = releases_dir.join(target);
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            // Write then rename so a half-written file is never served.
            let tmp = dir.join(format!("{file}.part"));
            std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
            }
            std::fs::rename(&tmp, dir.join(file))?;
        }
        // Last: this is what tells computers there is something new.
        std::fs::write(releases_dir.join("version.txt"), format!("{}\n", latest.version))?;
        done.published = Some(latest.version.clone());
    }
    Ok(done)
}

/// Start the program again in this same process (same PID, so systemd keeps
/// tracking it), now running the new program file.
fn restart(exe: &Path, version: &str) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(exe).args(std::env::args_os().skip(1)).env(UPDATED_TO, version).exec();
        log::error!("restarting {}: {err}; exiting so the service manager restarts it", exe.display());
    }
    std::process::exit(1)
}

/// Background loop: check now, then every hour.
pub fn spawn(relay: crate::server::Shared, repo: String) {
    let exe: Option<PathBuf> = std::env::current_exe().ok();
    if let Some(exe) = &exe {
        crate::update::cleanup_old(exe);
    }
    // A release whose server program isn't the version its tag says would
    // otherwise be installed again and again. Don't replace for that one.
    let broken = std::env::var(UPDATED_TO).ok().filter(|v| v != crate::VERSION);
    if let Some(v) = &broken {
        log::error!(
            "updates: release {v} contains server version {}, not {v}; not installing it again (fix the release)",
            crate::VERSION
        );
    }
    tokio::spawn(async move {
        log::info!("updates: following GitHub releases of {repo}");
        let mut tick = tokio::time::interval(CHECK_EVERY);
        loop {
            tick.tick().await;
            let (relay2, repo2, exe2, broken2) = (relay.clone(), repo.clone(), exe.clone(), broken.clone());
            let result = tokio::task::spawn_blocking(move || -> Result<(Applied, String)> {
                // FARLINK_GITHUB_API: a stand-in GitHub for testing.
                let api = std::env::var("FARLINK_GITHUB_API").unwrap_or_else(|_| GITHUB_API.into());
                let latest = fetch_latest(&api, &repo2)?;
                let published = relay2.release_version();
                let target = crate::util::current_target();
                let server = exe2.as_deref().filter(|_| broken2.as_deref() != Some(&latest.version)).map(|e| (e, target));
                Ok((apply(&latest, crate::VERSION, published.as_deref(), &relay2.cfg.releases_dir, server)?, latest.version))
            })
            .await;
            match result {
                Ok(Ok((done, version))) => {
                    if let Some(v) = &done.published {
                        log::info!("updates: published {v} for computers");
                    }
                    if done.server_replaced
                        && let Some(exe) = &exe
                    {
                        log::info!("updates: server {} -> {version}, restarting", crate::VERSION);
                        restart(exe, &version);
                    }
                }
                Ok(Err(e)) => log::warn!("updates: {e:#}"),
                Err(e) => log::warn!("updates: {e}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names() {
        assert!(valid_repo("artem/farlink"));
        assert!(valid_repo("a-b_c.d/x"));
        assert!(!valid_repo("farlink"));
        assert!(!valid_repo("a/b/c"));
        assert!(!valid_repo("a/../b"));
        assert!(!valid_repo("a/b?x=1"));
        assert!(!valid_repo("/b"));
    }

    #[test]
    fn sums_file() {
        let s = parse_sums("ABC123  farlink-linux-x86_64\ndef456 *farlink-host-windows-x86_64.exe\n\n");
        assert_eq!(s.get("farlink-linux-x86_64").map(String::as_str), Some("abc123"));
        assert_eq!(s.get("farlink-host-windows-x86_64.exe").map(String::as_str), Some("def456"));
    }

    #[test]
    fn every_program_has_an_asset() {
        for target in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-pc-windows-gnu"] {
            for base in ["farlink", "farlink-host"] {
                let file = crate::util::exe_name(base, target);
                assert!(ASSETS.iter().any(|a| a.1 == target && a.2 == file), "{target}/{file}");
            }
        }
    }

    #[test]
    fn nothing_to_do_when_current() {
        let latest = Latest { version: "0.2.0".into(), assets: HashMap::new() };
        let dir = Path::new("/nonexistent");
        let done = apply(&latest, "0.2.0", Some("0.2.0"), dir, Some((Path::new("/x"), "x86_64-unknown-linux-gnu"))).unwrap();
        assert_eq!(done, Applied::default());
        let older = Latest { version: "0.1.9".into(), assets: HashMap::new() };
        assert_eq!(apply(&older, "0.2.0", Some("0.2.0"), dir, None).unwrap(), Applied::default());
    }
}
