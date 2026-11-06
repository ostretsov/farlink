//! farlink-host — the helper on every remote computer. It can only be
//! controlled, never controls others, and only connects out to your server.
//!
//! ```text
//! farlink-host install --server https://remote.example.com --token TOKEN   # done by the install link
//! farlink-host run [--demo]                                                # normal operation
//! farlink-host service                                                     # Windows service entry
//! farlink-host uninstall
//! ```

// Console program on purpose: the installer shows its progress, and the
// service starts the helper with CREATE_NO_WINDOW so no window appears.

use anyhow::{Context, Result};
use farlink::config::{HostConfig, load_host_config, save_json};
use farlink::host::{FileLogger, HostOptions, run_agent};
use farlink::util::Args;

const HELP: &str = "farlink-host — Farlink helper for remote computers

USAGE:
  farlink-host install --server URL --token TOKEN [--user NAME] [--dry-run]
  farlink-host run [--demo] [--server URL --token TOKEN]
  farlink-host uninstall [--dry-run]
  farlink-host version";

fn main() {
    let args = Args::parse(std::env::args().skip(1), &["server", "token", "user"]);
    let code = match real_main(&args) {
        Ok(code) => code,
        Err(e) => {
            log::error!("{e:#}");
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn real_main(args: &Args) -> Result<i32> {
    match args.command().unwrap_or("run") {
        "run" => {
            let (path, mut cfg) = load_host_config();
            if let (Some(s), Some(t)) = (args.get("server"), args.get("token")) {
                cfg = HostConfig { server_url: farlink::util::normalize_server_url(s), enroll_token: Some(t.into()), ..Default::default() };
                save_json(&path, &cfg)?;
            }
            if std::env::var_os("FARLINK_LOG_STDERR").is_some() || args.has("verbose") {
                env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
            } else {
                let _ = FileLogger::install(&path.with_file_name("host.log"));
            }
            log::info!("Farlink helper {} starting", farlink::VERSION);
            let opts = HostOptions { demo: args.has("demo"), no_update: args.has("no-update") };
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(run_agent(path, cfg, opts))
        }
        "install" => {
            let server = farlink::util::normalize_server_url(args.get("server").context("--server is required")?);
            let token = args.get("token").context("--token is required")?;
            let dry = args.has("dry-run");
            #[cfg(windows)]
            let steps = {
                let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
                let pd = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
                farlink::install::host_windows_plan(&server, token, pf.as_ref(), pd.as_ref())
            };
            #[cfg(not(windows))]
            let steps = {
                let user = args
                    .get("user")
                    .map(str::to_string)
                    .or_else(|| std::env::var("SUDO_USER").ok())
                    .context("--user is required (the desktop user)")?;
                let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
                let uid = farlink::install::uid_of(&user, &passwd).with_context(|| format!("user '{user}' not found"))?;
                farlink::install::host_linux_plan(&server, token, &user, uid)
            };
            farlink::install::execute(&steps, dry)?;
            Ok(0)
        }
        "uninstall" => {
            #[cfg(windows)]
            {
                let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
                let pd = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
                farlink::install::execute(&farlink::install::host_windows_uninstall_plan(pf.as_ref(), pd.as_ref()), args.has("dry-run"))?;
            }
            #[cfg(not(windows))]
            {
                println!("Run: sudo systemctl --global disable farlink-host.service && sudo rm -rf /opt/farlink /etc/farlink {}", farlink::install::LINUX_USER_UNIT);
            }
            Ok(0)
        }
        "service" => {
            #[cfg(windows)]
            {
                let (path, _) = load_host_config();
                let _ = FileLogger::install(&path.with_file_name("service.log"));
                farlink::host::service::run()?;
            }
            Ok(0)
        }
        "version" | "--version" => {
            println!("{}", farlink::VERSION);
            Ok(0)
        }
        _ => {
            println!("{HELP}");
            Ok(0)
        }
    }
}
