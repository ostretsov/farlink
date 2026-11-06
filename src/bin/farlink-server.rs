//! farlink-server — relay, web admin, install links and update feed.
//!
//! ```text
//! farlink-server install --domain remote.example.com   # on a fresh Ubuntu/Debian server (root)
//! farlink-server serve [--config FILE] [--bind 0.0.0.0:8470] [--url https://...] [--data DIR] [--tls-domain D]
//! farlink-server publish VERSION DIR                    # make a new version available (auto-update)
//! farlink-server set-password NEW_PASSWORD
//! ```

use anyhow::{Context, Result, bail};
use farlink::config::{SERVER_CONFIG_PATH, ServerConfig, Tls};
use farlink::server::Relay;
use farlink::util::Args;
use std::path::{Path, PathBuf};

const HELP: &str = "farlink-server — relay + web admin for Farlink

USAGE:
  farlink-server install --domain DOMAIN [--port PORT] [--tls-cert FULLCHAIN.pem --tls-key KEY.pem]
                         [--github OWNER/REPO] [--releases DIR] [--dry-run]
      Set up this Linux server: service, HTTPS for DOMAIN, firewall.
      Port 443 (default): free Let's Encrypt certificate, fetched and renewed automatically.
      Any other port (443 already in use): give your own certificate files, because
      Let's Encrypt only checks domains on port 443.
      --github: install new GitHub releases of OWNER/REPO automatically (checked hourly):
      the server updates itself, then the computers update from it.
  farlink-server serve [--config FILE] [--bind ADDR] [--url PUBLIC_URL] [--data DIR]
                       [--tls-domain DOMAIN [--tls-email EMAIL] [--tls-staging]]
                       [--tls-cert FULLCHAIN.pem --tls-key KEY.pem] [--no-tls]
      Run the server (default command).
      --tls-domain   HTTPS with a free Let's Encrypt certificate, fetched and renewed
                     automatically (bind to :443, DNS of DOMAIN must point here).
      --tls-cert/key HTTPS with certificate files you manage yourself (any port).
      --no-tls       plain HTTP (behind your own HTTPS proxy, or on a LAN).
  farlink-server publish VERSION DIR [--config FILE]
      Publish programs from DIR/<target>/... as VERSION; computers update themselves.
  farlink-server set-password PASSWORD [--config FILE]
  farlink-server version";

fn load_config(args: &Args) -> Result<ServerConfig> {
    let path = args.get("config").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(SERVER_CONFIG_PATH));
    let mut cfg = if path.exists() {
        serde_json::from_slice(&std::fs::read(&path)?).with_context(|| format!("reading {}", path.display()))?
    } else {
        ServerConfig::default()
    };
    if let Some(b) = args.get("bind") {
        cfg.bind = b.to_string();
    }
    if let Some(u) = args.get("url") {
        cfg.public_url = farlink::util::normalize_server_url(u);
    }
    if let Some(d) = args.get("data") {
        cfg.data_dir = PathBuf::from(d);
        cfg.releases_dir = cfg.data_dir.join("releases");
    }
    if let Some(domain) = args.get("tls-domain") {
        cfg.tls = Tls::Auto {
            domain: domain.trim().to_string(),
            email: args.get("tls-email").unwrap_or_default().to_string(),
            staging: args.has("tls-staging"),
        };
        if args.get("url").is_none() {
            cfg.public_url = format!("https://{}", domain.trim());
        }
    }
    match (args.get("tls-cert"), args.get("tls-key")) {
        (Some(cert), Some(key)) => cfg.tls = Tls::Files { cert: cert.into(), key: key.into() },
        (None, None) => {}
        _ => bail!("--tls-cert and --tls-key go together"),
    }
    if args.has("no-tls") {
        cfg.tls = Tls::Off;
    }
    if cfg.tls != Tls::Off && !cfg.public_url.starts_with("https://") {
        log::warn!("HTTPS is on but the public address {} is not https://", cfg.public_url);
    }
    Ok(cfg)
}

fn publish(cfg: &ServerConfig, version: &str, from: &Path) -> Result<()> {
    if farlink::version::Version::parse(version).is_none() {
        bail!("'{version}' is not a version number like 1.2.0");
    }
    let mut copied = 0;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let target = entry.file_name();
        let dest = cfg.releases_dir.join(&target);
        std::fs::create_dir_all(&dest)?;
        for f in std::fs::read_dir(entry.path())? {
            let f = f?;
            let name = f.file_name();
            let name_s = name.to_string_lossy();
            if ["farlink", "farlink.exe", "farlink-host", "farlink-host.exe"].contains(&name_s.as_ref()) {
                // Copy then rename so a half-copied file is never served.
                let tmp = dest.join(format!("{name_s}.part"));
                std::fs::copy(f.path(), &tmp)?;
                std::fs::rename(&tmp, dest.join(&name))?;
                println!("  {}/{}", target.to_string_lossy(), name_s);
                copied += 1;
            }
        }
    }
    if copied == 0 {
        bail!("no programs found in {} (expected <target>/farlink-host etc.)", from.display());
    }
    std::fs::write(cfg.releases_dir.join("version.txt"), format!("{version}\n"))?;
    println!("Published {version}. Connected computers update within a minute.");
    Ok(())
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse(std::env::args().skip(1), &["config", "bind", "url", "data", "domain", "releases", "tls-domain", "tls-email", "tls-cert", "tls-key", "port", "github"]);
    match args.command().unwrap_or("serve") {
        "serve" => {
            let cfg = load_config(&args)?;
            let (relay, first_password) = Relay::new(cfg.clone())?;
            if let Some(pw) = first_password {
                let note = cfg.data_dir.join("initial-admin-password.txt");
                let _ = std::fs::write(&note, format!("{pw}\n"));
                println!("\n  First start. Administrator password: {pw}\n  (also saved in {})\n", note.display());
            }
            let https = match &cfg.tls {
                Tls::Off => "plain HTTP".to_string(),
                Tls::Auto { domain, staging, .. } => {
                    format!("HTTPS, Let's Encrypt{} certificate for {domain}", if *staging { " (staging)" } else { "" })
                }
                Tls::Files { cert, .. } => format!("HTTPS, certificate {}", cert.display()),
            };
            println!("Farlink server {} on {} ({https}) — public address {}", farlink::VERSION, cfg.bind, cfg.public_url);
            println!("Install link: {}", relay.install_url());
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(farlink::tls::run(relay, cfg))
        }
        "install" => {
            let domain = args.get("domain").context("--domain is required, e.g. --domain remote.example.com")?;
            let releases = args.get("releases").map(PathBuf::from).or_else(|| {
                let p = std::env::current_exe().ok()?.parent()?.join("releases");
                p.is_dir().then_some(p)
            });
            let port: u16 = match args.get("port") {
                Some(p) => p.parse().ok().filter(|p| *p > 0).with_context(|| format!("--port {p} is not a port number"))?,
                None => 443,
            };
            let cert = match (args.get("tls-cert"), args.get("tls-key")) {
                (Some(c), Some(k)) => Some((PathBuf::from(c), PathBuf::from(k))),
                (None, None) => None,
                _ => bail!("--tls-cert and --tls-key go together"),
            };
            let steps = farlink::install::server_linux_plan(domain, port, cert, args.get("github"), releases.as_deref())?;
            farlink::install::execute(&steps, args.has("dry-run"))?;
            if !args.has("dry-run") {
                println!("\nThe administrator password is in /var/lib/farlink-server/initial-admin-password.txt");
                println!("(change it with: farlink-server set-password NEW --config {SERVER_CONFIG_PATH})");
            }
            Ok(())
        }
        "publish" => {
            let cfg = load_config(&args)?;
            let (Some(version), Some(dir)) = (args.positional.get(1), args.positional.get(2)) else {
                bail!("usage: farlink-server publish VERSION DIR");
            };
            publish(&cfg, version, Path::new(dir))
        }
        "set-password" => {
            let cfg = load_config(&args)?;
            let pw = args.positional.get(1).context("usage: farlink-server set-password NEW_PASSWORD")?;
            if pw.len() < 8 {
                bail!("use at least 8 characters");
            }
            let (relay, _) = Relay::new(cfg)?;
            relay.with_store(|s| s.set_password(pw));
            println!("Password changed. Everyone was signed out. Restart is not needed.");
            Ok(())
        }
        "version" | "--version" => {
            println!("{}", farlink::VERSION);
            Ok(())
        }
        _ => {
            println!("{HELP}");
            Ok(())
        }
    }
}
