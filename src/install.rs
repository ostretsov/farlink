//! Installation plans. Each plan is a list of plain steps, built as data so it
//! can be tested here and printed with `--dry-run` before running.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub const WIN_SERVICE: &str = "FarlinkHost";
pub const WIN_SERVICE_DISPLAY: &str = "Farlink Remote Host";
pub const FIREWALL_RULE: &str = "Farlink Remote Host";
pub const SERVER_PORT: u16 = 8470;

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Mkdir(PathBuf),
    /// Copy the running program to this path.
    CopySelf(PathBuf),
    /// Copy a folder (e.g. published releases) if it exists.
    CopyDir { from: PathBuf, to: PathBuf },
    WriteFile { path: PathBuf, contents: String, mode: Option<u32> },
    /// Add `line` to the file unless it is already there.
    EnsureLine { path: PathBuf, line: String },
    Run { program: String, args: Vec<String>, ignore_failure: bool },
    Note(String),
}

impl Step {
    fn run(program: &str, args: &[&str]) -> Step {
        Step::Run { program: program.into(), args: args.iter().map(|s| s.to_string()).collect(), ignore_failure: false }
    }
    fn try_run(program: &str, args: &[&str]) -> Step {
        Step::Run { program: program.into(), args: args.iter().map(|s| s.to_string()).collect(), ignore_failure: true }
    }

    pub fn describe(&self) -> String {
        match self {
            Step::Mkdir(p) => format!("create folder {}", p.display()),
            Step::CopySelf(p) => format!("copy program to {}", p.display()),
            Step::CopyDir { from, to } => format!("copy {} to {}", from.display(), to.display()),
            Step::WriteFile { path, .. } => format!("write {}", path.display()),
            Step::EnsureLine { path, line } => format!("make sure {} contains `{line}`", path.display()),
            Step::Run { program, args, ignore_failure } => {
                format!("run {program} {}{}", args.join(" "), if *ignore_failure { "  (may fail)" } else { "" })
            }
            Step::Note(n) => format!("note: {n}"),
        }
    }
}

fn host_json(server: &str, token: &str) -> String {
    serde_json::to_string_pretty(&crate::config::HostConfig {
        server_url: server.trim_end_matches('/').to_string(),
        enroll_token: Some(token.to_string()),
        machine_id: None,
        secret: None,
    })
    .unwrap()
}

// ------------------------------------------------------------------ Windows

/// Install the host on Windows 10/11 / Server 2016+ (run as Administrator).
///
/// The host runs as a SYSTEM service that starts the screen helper inside the
/// active console session, so the lock screen, sign-in screen and UAC prompts
/// can be seen and controlled.
pub fn host_windows_plan(server: &str, token: &str, program_files: &Path, program_data: &Path) -> Vec<Step> {
    let dir = program_files.join("Farlink");
    let exe = dir.join("farlink-host.exe");
    let exe_s = exe.display().to_string();
    let data = program_data.join("Farlink");
    let data_s = data.display().to_string();
    let bin_path = format!("\"{exe_s}\" service");
    vec![
        Step::try_run("sc.exe", &["stop", WIN_SERVICE]),
        // Windows Defender often flags new, unsigned remote-control programs;
        // allow our folders before the program is copied there (and so
        // self-updates are not removed). May fail with another antivirus.
        defender_exclusion("Add-MpPreference", &dir, &data),
        Step::Mkdir(dir.clone()),
        Step::CopySelf(exe.clone()),
        Step::Mkdir(data.clone()),
        Step::WriteFile { path: data.join("host.json"), contents: host_json(server, token), mode: None },
        // Only SYSTEM and Administrators may read the machine key.
        Step::try_run(
            "icacls.exe",
            &[&data_s, "/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "/grant:r", "*S-1-5-32-544:(OI)(CI)F"],
        ),
        // Firewall: allow the program in both directions (idempotent).
        Step::try_run("netsh.exe", &["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")]),
        Step::run(
            "netsh.exe",
            &[
                "advfirewall", "firewall", "add", "rule", &format!("name={FIREWALL_RULE}"), "dir=out", "action=allow",
                &format!("program={exe_s}"), "enable=yes", "profile=any",
            ],
        ),
        Step::run(
            "netsh.exe",
            &[
                "advfirewall", "firewall", "add", "rule", &format!("name={FIREWALL_RULE}"), "dir=in", "action=allow",
                &format!("program={exe_s}"), "enable=yes", "profile=any",
            ],
        ),
        // Allow a service to send Ctrl+Alt+Del (SendSAS).
        Step::run(
            "reg.exe",
            &[
                "add", r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System", "/v", "SoftwareSASGeneration",
                "/t", "REG_DWORD", "/d", "1", "/f",
            ],
        ),
        Step::try_run(
            "sc.exe",
            &["create", WIN_SERVICE, "binPath=", &bin_path, "start=", "auto", "DisplayName=", WIN_SERVICE_DISPLAY],
        ),
        Step::run("sc.exe", &["config", WIN_SERVICE, "binPath=", &bin_path, "start=", "auto"]),
        Step::try_run("sc.exe", &["description", WIN_SERVICE, "Lets your Farlink server show and control this computer."]),
        // Restart automatically (also used after background updates).
        Step::run("sc.exe", &["failure", WIN_SERVICE, "reset=", "86400", "actions=", "restart/3000/restart/3000/restart/3000"]),
        Step::try_run("sc.exe", &["failureflag", WIN_SERVICE, "1"]),
        Step::run("sc.exe", &["start", WIN_SERVICE]),
        Step::Note("Installed. This computer will appear in your Farlink list within a few seconds.".into()),
    ]
}

/// `Add-MpPreference` / `Remove-MpPreference` for the program and data folders.
fn defender_exclusion(cmdlet: &str, dir: &Path, data: &Path) -> Step {
    let q = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "''"));
    let cmd = format!("{cmdlet} -ExclusionPath {},{}", q(dir), q(data));
    Step::try_run("powershell.exe", &["-NoProfile", "-NonInteractive", "-Command", &cmd])
}

pub fn host_windows_uninstall_plan(program_files: &Path, program_data: &Path) -> Vec<Step> {
    vec![
        Step::try_run("sc.exe", &["stop", WIN_SERVICE]),
        Step::try_run("sc.exe", &["delete", WIN_SERVICE]),
        Step::try_run("netsh.exe", &["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")]),
        defender_exclusion("Remove-MpPreference", &program_files.join("Farlink"), &program_data.join("Farlink")),
        Step::Note(format!(
            "Remove the folders {} and {} to finish.",
            program_files.join("Farlink").display(),
            program_data.join("Farlink").display()
        )),
    ]
}

// ------------------------------------------------------------------ Linux host

pub const LINUX_HOST_DIR: &str = "/opt/farlink";
pub const LINUX_USER_UNIT: &str = "/etc/systemd/user/farlink-host.service";

pub fn linux_user_unit() -> String {
    format!(
        "[Unit]\nDescription=Farlink remote desktop host\nAfter=graphical-session.target\nPartOf=graphical-session.target\n\n\
         [Service]\nExecStart={LINUX_HOST_DIR}/farlink-host run\nRestart=always\nRestartSec=3\n\n\
         [Install]\nWantedBy=graphical-session.target\n"
    )
}

/// Install the host on Ubuntu (X11 session), run with sudo. The helper runs in
/// the desktop session of `user`; the program folder belongs to that user so
/// background updates need no password.
pub fn host_linux_plan(server: &str, token: &str, user: &str, uid: u32) -> Vec<Step> {
    let exe = PathBuf::from(LINUX_HOST_DIR).join("farlink-host");
    let runtime = format!("XDG_RUNTIME_DIR=/run/user/{uid}");
    let bus = format!("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus");
    vec![
        Step::Mkdir(LINUX_HOST_DIR.into()),
        Step::CopySelf(exe),
        Step::run("chown", &["-R", &format!("{user}:{user}"), LINUX_HOST_DIR]),
        Step::Mkdir("/etc/farlink".into()),
        Step::WriteFile { path: crate::config::LINUX_HOST_SEED.into(), contents: host_json(server, token), mode: Some(0o644) },
        Step::WriteFile { path: LINUX_USER_UNIT.into(), contents: linux_user_unit(), mode: Some(0o644) },
        Step::run("systemctl", &["--global", "enable", "farlink-host.service"]),
        Step::try_run("runuser", &["-u", user, "--", "env", &runtime, &bus, "systemctl", "--user", "daemon-reload"]),
        Step::try_run("runuser", &["-u", user, "--", "env", &runtime, &bus, "systemctl", "--user", "restart", "farlink-host.service"]),
        Step::Note("Installed. The helper runs whenever this user is signed in to the desktop (X11).".into()),
        Step::Note("For unattended access turn on automatic login in Settings > Users.".into()),
    ]
}

// ------------------------------------------------------------------ server

pub fn server_unit(bin: &Path) -> String {
    format!(
        "[Unit]\nDescription=Farlink relay and web admin\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nUser=farlink\nExecStart={} serve --config {}\nRestart=always\nRestartSec=2\n\
         AmbientCapabilities=CAP_NET_BIND_SERVICE\nCapabilityBoundingSet=CAP_NET_BIND_SERVICE\n\
         NoNewPrivileges=true\nProtectSystem=strict\nReadWritePaths=/var/lib/farlink-server /opt/farlink-server\n\n\
         [Install]\nWantedBy=multi-user.target\n",
        bin.display(),
        crate::config::SERVER_CONFIG_PATH
    )
}

/// Older installs put Caddy in front of the server; it holds ports 80/443.
const OLD_CADDY_SITE: &str = "/etc/caddy/farlink.caddy";

/// Check a domain name typed at install time.
pub fn valid_domain(d: &str) -> bool {
    let d = d.trim();
    !d.is_empty()
        && d.len() <= 253
        && d.contains('.')
        && d.split('.').all(|l| {
            !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Install relay + web admin on a public Ubuntu/Debian server for `domain`.
/// The server answers HTTPS on `port` itself. On 443 it fetches and renews a
/// Let's Encrypt certificate on its own; on any other port Let's Encrypt can't
/// check the domain, so `cert` (full chain + key PEM files) is required.
/// Linux slows a connection down to its starting speed after every short
/// pause (`tcp_slow_start_after_idle`, on by default). A remote desktop is
/// mostly pauses: the big picture after a click then needs several round
/// trips instead of one (measured: +540 ms for 300 KB at 180 ms away). The
/// relay's installer switches that off; [`tcp_stays_warm`] checks it.
pub const TCP_WARM_SYSCTL: &str = "/etc/sysctl.d/60-farlink.conf";
pub const TCP_WARM_CONTENTS: &str =
    "# Farlink relay: keep connections at speed after a pause (pictures after a click arrive in one go).
net.ipv4.tcp_slow_start_after_idle = 0
";
/// The command that fixes an already installed relay (run as root).
pub const TCP_WARM_FIX: &str =
    "echo 'net.ipv4.tcp_slow_start_after_idle = 0' | sudo tee /etc/sysctl.d/60-farlink.conf && sudo sysctl -p /etc/sysctl.d/60-farlink.conf";

/// Does this Linux keep connections at speed after a pause? `None`: not
/// Linux, or it can't be read.
pub fn tcp_stays_warm() -> Option<bool> {
    let v = std::fs::read_to_string("/proc/sys/net/ipv4/tcp_slow_start_after_idle").ok()?;
    Some(v.trim() == "0")
}

/// `github` (`owner/repo`): the server then installs that repository's
/// releases on its own (itself + the programs for computers).
pub fn server_linux_plan(
    domain: &str,
    port: u16,
    cert: Option<(PathBuf, PathBuf)>,
    github: Option<&str>,
    releases_src: Option<&Path>,
) -> Result<Vec<Step>> {
    if !valid_domain(domain) {
        bail!("'{domain}' is not a valid domain name (example: remote.example.com)");
    }
    if let Some(repo) = github
        && !crate::server_update::valid_repo(repo)
    {
        bail!("'{repo}' is not a GitHub repository like owner/farlink");
    }
    let tls = match cert {
        Some((cert, key)) => crate::config::Tls::Files { cert, key },
        None if port == 443 => crate::config::Tls::Auto { domain: domain.to_string(), email: String::new(), staging: false },
        None => bail!(
            "automatic certificates only work on port 443 (Let's Encrypt checks the domain there).\n\
             For port {port} give your own certificate: --tls-cert fullchain.pem --tls-key privkey.pem"
        ),
    };
    let public_url = if port == 443 { format!("https://{domain}") } else { format!("https://{domain}:{port}") };
    let mut notes = vec![];
    if let crate::config::Tls::Files { cert, key } = &tls {
        notes.push(Step::Note(format!(
            "The service runs as user 'farlink': it must be able to read {} and {}. Restart it after renewing them: systemctl restart farlink-server",
            cert.display(),
            key.display()
        )));
    }
    let bin = PathBuf::from("/opt/farlink-server/farlink-server");
    let data = PathBuf::from("/var/lib/farlink-server");
    let cfg = crate::config::ServerConfig {
        public_url: public_url.clone(),
        bind: format!("0.0.0.0:{port}"),
        data_dir: data.clone(),
        releases_dir: data.join("releases"),
        tls,
        update_from_github: github.map(str::to_string),
    };
    let mut steps = vec![
        Step::Mkdir("/opt/farlink-server".into()),
        Step::CopySelf(bin.clone()),
        Step::Mkdir(data.join("releases")),
        Step::Mkdir("/etc/farlink-server".into()),
        Step::WriteFile {
            path: crate::config::SERVER_CONFIG_PATH.into(),
            contents: serde_json::to_string_pretty(&cfg)?,
            mode: Some(0o644),
        },
    ];
    if let Some(src) = releases_src {
        steps.push(Step::CopyDir { from: src.to_path_buf(), to: data.join("releases") });
    }
    steps.extend([
        Step::try_run("useradd", &["--system", "--home", "/var/lib/farlink-server", "--shell", "/usr/sbin/nologin", "farlink"]),
        Step::run("chown", &["-R", "farlink:farlink", "/var/lib/farlink-server"]),
        // The service replaces its own program when it updates itself.
        Step::run("chown", &["-R", "farlink:farlink", "/opt/farlink-server"]),
        Step::WriteFile { path: "/etc/systemd/system/farlink-server.service".into(), contents: server_unit(&bin), mode: Some(0o644) },
        // Keep connections fast after a pause (see `TCP_WARM_SYSCTL`).
        Step::WriteFile { path: TCP_WARM_SYSCTL.into(), contents: TCP_WARM_CONTENTS.into(), mode: Some(0o644) },
        Step::try_run("sysctl", &["-p", TCP_WARM_SYSCTL]),
    ]);
    if Path::new(OLD_CADDY_SITE).exists() {
        // Upgrading from the Caddy setup: its Farlink site points at the old
        // local port, which is gone now.
        steps.extend([
            Step::try_run("sed", &["-i", "\\#import /etc/caddy/farlink.caddy#d", "/etc/caddy/Caddyfile"]),
            Step::try_run("rm", &["-f", OLD_CADDY_SITE]),
            Step::try_run("systemctl", &["reload", "caddy"]),
            Step::Note("Removed the Farlink site from Caddy. If Caddy serves nothing else, stop it: systemctl disable --now caddy".into()),
        ]);
    }
    steps.extend([
        Step::run("systemctl", &["daemon-reload"]),
        Step::run("systemctl", &["enable", "--now", "farlink-server.service"]),
        Step::run("systemctl", &["restart", "farlink-server.service"]),
        Step::try_run("ufw", &["allow", &format!("{port}/tcp")]),
    ]);
    steps.extend(notes);
    if let Some(repo) = github {
        steps.push(Step::Note(format!(
            "Updates: the server checks https://github.com/{repo}/releases every hour and updates itself and the computers."
        )));
    }
    steps.push(Step::Note(format!(
        "Point the DNS record of {domain} to this server, then open {public_url} (problems: journalctl -u farlink-server)"
    )));
    Ok(steps)
}

// ------------------------------------------------------------------ running

/// Run every step. With `dry_run` only print them.
pub fn execute(steps: &[Step], dry_run: bool) -> Result<()> {
    for (i, step) in steps.iter().enumerate() {
        println!("[{}/{}] {}", i + 1, steps.len(), step.describe());
        if dry_run {
            continue;
        }
        match step {
            Step::Mkdir(p) => std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?,
            Step::CopySelf(to) => {
                let me = std::env::current_exe()?;
                if me != *to {
                    // A running copy cannot be overwritten on Windows: move it aside.
                    if to.exists() {
                        let _ = crate::update::replace_exe(to, &std::fs::read(&me)?);
                    } else {
                        std::fs::copy(&me, to).with_context(|| format!("copying to {}", to.display()))?;
                    }
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o755))?;
                }
            }
            Step::CopyDir { from, to } => {
                if from.is_dir() {
                    copy_dir(from, to)?;
                }
            }
            Step::WriteFile { path, contents, mode } => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
                #[cfg(unix)]
                if let Some(mode) = mode {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(*mode))?;
                }
                #[cfg(not(unix))]
                let _ = mode;
            }
            Step::EnsureLine { path, line } => {
                let current = std::fs::read_to_string(path).unwrap_or_default();
                if let Some(updated) = with_line(&current, line) {
                    std::fs::write(path, updated)?;
                }
            }
            Step::Run { program, args, ignore_failure } => {
                let status = std::process::Command::new(program).args(args).status();
                match status {
                    Ok(s) if s.success() => {}
                    _ if *ignore_failure => {}
                    Ok(s) => bail!("`{}` failed ({s})", step.describe()),
                    Err(e) => bail!("`{}` could not start: {e}", step.describe()),
                }
            }
            Step::Note(n) => println!("    {n}"),
        }
    }
    Ok(())
}

/// The text with `line` added, or `None` if it's already present.
pub fn with_line(current: &str, line: &str) -> Option<String> {
    if current.lines().any(|l| l.trim() == line.trim()) {
        return None;
    }
    let mut s = current.to_string();
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(line);
    s.push('\n');
    Some(s)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

/// Look up a user's numeric id in /etc/passwd (Linux installer).
pub fn uid_of(user: &str, passwd: &str) -> Option<u32> {
    passwd.lines().find_map(|l| {
        let mut f = l.split(':');
        (f.next()? == user).then(|| f.nth(1)?.parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(steps: &[Step]) -> Vec<String> {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Run { program, args, .. } => Some(format!("{program} {}", args.join(" "))),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn windows_plan_opens_firewall_and_installs_service() {
        let steps = host_windows_plan("https://rd.example.com/", "tok123", Path::new(r"C:\Program Files"), Path::new(r"C:\ProgramData"));
        let cmds = runs(&steps);
        let fw: Vec<_> = cmds.iter().filter(|c| c.contains("advfirewall firewall add rule")).collect();
        assert_eq!(fw.len(), 2, "inbound and outbound rules");
        assert!(fw.iter().any(|c| c.contains("dir=in")) && fw.iter().any(|c| c.contains("dir=out")));
        assert!(fw.iter().all(|c| c.contains("farlink-host.exe")));
        // Old rule removed first so re-installing doesn't duplicate rules.
        let del = cmds.iter().position(|c| c.contains("delete rule")).unwrap();
        let add = cmds.iter().position(|c| c.contains("add rule")).unwrap();
        assert!(del < add);
        assert!(cmds.iter().any(|c| c.starts_with("sc.exe create FarlinkHost") && c.contains("service")));
        assert!(cmds.iter().any(|c| c.contains("failure FarlinkHost") && c.contains("restart/3000")));
        assert!(cmds.iter().any(|c| c.contains("SoftwareSASGeneration")));
        let mp = cmds.iter().position(|c| {
            c.starts_with("powershell.exe") && c.contains("Add-MpPreference -ExclusionPath 'C:\\Program Files") && c.contains("ProgramData")
        });
        let copy = steps.iter().position(|s| matches!(s, Step::CopySelf(_))).unwrap();
        assert!(mp.is_some(), "Defender exclusion for the install folders: {cmds:?}");
        assert!(steps.iter().position(|s| s.describe().contains("Add-MpPreference")).unwrap() < copy, "before copying");
        assert_eq!(cmds.last().unwrap(), "sc.exe start FarlinkHost");
        let cfg = steps
            .iter()
            .find_map(|s| match s {
                Step::WriteFile { path, contents, .. } if path.ends_with("host.json") => Some(contents.clone()),
                _ => None,
            })
            .unwrap();
        let parsed: crate::config::HostConfig = serde_json::from_str(&cfg).unwrap();
        assert_eq!(parsed.server_url, "https://rd.example.com");
        assert_eq!(parsed.enroll_token.as_deref(), Some("tok123"));
    }

    #[test]
    fn windows_uninstall_removes_service_and_rule() {
        let cmds = runs(&host_windows_uninstall_plan(Path::new("C:/PF"), Path::new("C:/PD")));
        assert!(cmds.iter().any(|c| c == "sc.exe delete FarlinkHost"));
        assert!(cmds.iter().any(|c| c.contains("Remove-MpPreference")));
        assert!(cmds.iter().any(|c| c.contains("delete rule")));
    }

    #[test]
    fn linux_plan_uses_user_session_service() {
        let steps = host_linux_plan("https://rd.example.com", "tok", "ana", 1000);
        let cmds = runs(&steps);
        assert!(cmds.contains(&"chown -R ana:ana /opt/farlink".to_string()), "user can self-update");
        assert!(cmds.contains(&"systemctl --global enable farlink-host.service".to_string()));
        assert!(cmds.iter().any(|c| c.contains("/run/user/1000") && c.contains("restart farlink-host.service")));
        assert!(linux_user_unit().contains("ExecStart=/opt/farlink/farlink-host run"));
        assert!(linux_user_unit().contains("Restart=always"));
    }

    fn written_config(steps: &[Step]) -> crate::config::ServerConfig {
        let cfg = steps
            .iter()
            .find_map(|s| match s {
                Step::WriteFile { path, contents, .. } if path.ends_with("config.json") => Some(contents.clone()),
                _ => None,
            })
            .unwrap();
        serde_json::from_str(&cfg).unwrap()
    }

    #[test]
    fn server_plan_needs_valid_domain_and_sets_up_https() {
        assert!(server_linux_plan("not a domain", 443, None, None, None).is_err());
        assert!(server_linux_plan("localhost", 443, None, None, None).is_err());
        let steps = server_linux_plan("remote.example.com", 443, None, None, Some(Path::new("/tmp/rel"))).unwrap();
        let cfg = written_config(&steps);
        assert_eq!(cfg.public_url, "https://remote.example.com");
        assert_eq!(cfg.bind, "0.0.0.0:443");
        assert!(matches!(&cfg.tls, crate::config::Tls::Auto { domain, .. } if domain == "remote.example.com"));
        assert!(steps.iter().any(|s| matches!(s, Step::CopyDir { .. })));
        assert!(server_unit(Path::new("/x")).contains("CAP_NET_BIND_SERVICE"), "port 443 without root");
        let cmds = runs(&steps);
        assert!(cmds.contains(&"ufw allow 443/tcp".to_string()));
        assert!(!cmds.iter().any(|c| c.contains("80/tcp")), "port 80 is left alone");
        assert!(
            steps.iter().any(|s| matches!(s, Step::WriteFile { path, contents, .. }
                if path == Path::new(TCP_WARM_SYSCTL) && contents.contains("net.ipv4.tcp_slow_start_after_idle = 0"))),
            "connections stay at speed after a pause"
        );
        assert!(cmds.contains(&format!("sysctl -p {TCP_WARM_SYSCTL}")));
        assert!(!cmds.iter().any(|c| c.contains("apt-get install")), "no Caddy any more");
        assert_eq!(cfg.update_from_github, None);
        assert!(server_unit(Path::new("/x")).contains("ReadWritePaths=/var/lib/farlink-server /opt/farlink-server"));
        assert!(cmds.contains(&"chown -R farlink:farlink /opt/farlink-server".to_string()), "can replace itself");
        let steps = server_linux_plan("remote.example.com", 443, None, Some("artem/farlink"), None).unwrap();
        assert_eq!(written_config(&steps).update_from_github.as_deref(), Some("artem/farlink"));
        assert!(server_linux_plan("remote.example.com", 443, None, Some("not a repo"), None).is_err());
    }

    #[test]
    fn server_plan_on_another_port_needs_certificate_files() {
        let err = server_linux_plan("remote.example.com", 8443, None, None, None).unwrap_err();
        assert!(err.to_string().contains("--tls-cert"), "{err}");
        let files = Some(("/etc/ssl/rd/fullchain.pem".into(), "/etc/ssl/rd/privkey.pem".into()));
        let steps = server_linux_plan("remote.example.com", 8443, files, None, None).unwrap();
        let cfg = written_config(&steps);
        assert_eq!(cfg.public_url, "https://remote.example.com:8443");
        assert_eq!(cfg.bind, "0.0.0.0:8443");
        assert!(matches!(&cfg.tls, crate::config::Tls::Files { cert, .. } if cert.ends_with("fullchain.pem")));
        let cmds = runs(&steps);
        assert!(cmds.contains(&"ufw allow 8443/tcp".to_string()));
        assert!(!cmds.iter().any(|c| c.contains("443/tcp") && !c.contains("8443")));
    }

    #[test]
    fn domain_validation() {
        assert!(valid_domain("rd.example.com"));
        assert!(valid_domain("a-b.c.io"));
        assert!(!valid_domain("-bad.com"));
        assert!(!valid_domain("bad..com"));
        assert!(!valid_domain("https://x.com"));
    }

    #[test]
    fn ensure_line_is_idempotent() {
        assert_eq!(with_line("", "import x").unwrap(), "import x\n");
        assert_eq!(with_line("a", "import x").unwrap(), "a\nimport x\n");
        assert!(with_line("a\nimport x\n", "import x").is_none());
    }

    #[test]
    fn execute_writes_files_and_runs_commands() {
        let dir = crate::util::unique_temp_dir("farlink-inst");
        let f = dir.join("deep/conf.txt");
        let steps = vec![
            Step::Mkdir(dir.join("a")),
            Step::WriteFile { path: f.clone(), contents: "hello".into(), mode: Some(0o600) },
            Step::EnsureLine { path: f.clone(), line: "more".into() },
            Step::try_run("this-program-does-not-exist", &[]),
        ];
        execute(&steps, false).unwrap();
        assert!(dir.join("a").is_dir());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello\nmore\n");
        let failing = vec![Step::run("this-program-does-not-exist", &[])];
        assert!(execute(&failing, false).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn passwd_lookup() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\nana:x:1000:1000:Ana:/home/ana:/bin/bash\n";
        assert_eq!(uid_of("ana", passwd), Some(1000));
        assert_eq!(uid_of("root", passwd), Some(0));
        assert_eq!(uid_of("bob", passwd), None);
    }
}
