//! Remote shell on the helper (the administrator only, control sessions
//! only): each command runs on its own (`bash -c` / `cmd /C`, as the helper's
//! user, no terminal; on Windows PowerShell when the viewer asks for it)
//! in the session's current folder, its output streams to
//! the viewer as [`SessionMsg::ShellOutput`] and its end comes as
//! [`SessionMsg::ShellDone`]. `cd` is run by the shell too (`cd … && pwd`),
//! and the folder it ends up in becomes the session's folder, so `~`,
//! variables and `cd -` work as usual.
//!
//! The helper never writes down what was run (no history, no command text
//! in the log).

use crate::proto::SessionMsg;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Output still arriving this long after the command ended (something it
/// started in the background keeps the pipe open) no longer holds up the
/// "finished" answer.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct State {
    cwd: PathBuf,
    /// Folder before the last `cd` (for `cd -`).
    prev: Option<PathBuf>,
    running: Option<Running>,
}

#[derive(Debug)]
struct Running {
    req: u32,
    pid: u32,
    /// Standard input of the command; `None` once closed.
    stdin: Option<std::sync::mpsc::Sender<String>>,
}

/// The shell of one session.
pub struct ShellService {
    /// The relay said the viewer is the administrator.
    allowed: bool,
    state: Arc<Mutex<State>>,
}

fn json(m: &SessionMsg) -> Message {
    Message::Text(serde_json::to_string(m).unwrap().into())
}

/// Where a new shell starts: the home folder (Windows SYSTEM service: the
/// system drive instead of the service profile).
pub fn start_folder() -> PathBuf {
    let home = dirs::home_dir().filter(|h| h.is_dir());
    if cfg!(windows) {
        let system = home
            .as_ref()
            .is_none_or(|h| h.to_string_lossy().to_ascii_lowercase().contains("systemprofile"));
        if system {
            let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
            return PathBuf::from(format!("{drive}\\"));
        }
    }
    home.unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" }))
}

/// Which program runs the commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// `bash -c` (or `sh -c`): Linux.
    Sh,
    /// `cmd /C`: Windows.
    Cmd,
    /// Windows PowerShell (`powershell.exe -EncodedCommand`).
    PowerShell,
}

impl Shell {
    /// The shell for this computer (`powershell` only means something on Windows).
    pub fn here(powershell: bool) -> Shell {
        match (cfg!(windows), powershell) {
            (false, _) => Shell::Sh,
            (true, false) => Shell::Cmd,
            (true, true) => Shell::PowerShell,
        }
    }
}

/// Is `line` a change of folder the shell must remember? Only a plain
/// `cd …` (cmd: also `chdir …` and `D:`; PowerShell: also `Set-Location`,
/// `sl`, `chdir`, `D:`); `cd x && make` runs as any other command (the
/// folder change ends with it). cmd's bare `cd` just prints the folder.
pub fn is_cd(line: &str, shell: Shell) -> bool {
    let line = line.trim();
    if line.contains(['&', '|', ';', '<', '>', '\n', '`']) || line.contains("$(") {
        return false;
    }
    let drive = |l: &str| {
        let b = l.as_bytes();
        b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
    };
    if shell == Shell::PowerShell {
        if drive(line) {
            return true;
        }
        let lower = line.to_ascii_lowercase();
        let word = lower.split_whitespace().next().unwrap_or("");
        return ["cd", "chdir", "sl", "set-location", "cd..", "cd\\"].contains(&word);
    }
    if shell == Shell::Cmd {
        if drive(line) {
            return true;
        }
        let lower = line.to_ascii_lowercase();
        for word in ["cd", "chdir"] {
            if let Some(rest) = lower.strip_prefix(word) {
                // `cd..` and `cd\` work in cmd without a space.
                return rest.starts_with([' ', '\t', '.', '\\', '/']) && !rest.trim().is_empty();
            }
        }
        false
    } else {
        line == "cd" || line.starts_with("cd ") || line.starts_with("cd\t")
    }
}

/// The text a command line runs as: a folder change also prints where it
/// ended up; PowerShell also answers in UTF-8 and with a real exit code.
pub fn script(line: &str, cd: bool, shell: Shell) -> String {
    match (shell, cd) {
        (Shell::Sh, false) | (Shell::Cmd, false) => line.to_string(),
        (Shell::Sh, true) => format!("{line} >/dev/null && pwd"),
        (Shell::Cmd, true) => format!("{line} && cd"),
        (Shell::PowerShell, cd) => {
            let tail = if cd {
                "if ($?) { (Get-Location).ProviderPath } else { exit 1 }".to_string()
            } else {
                // A failed cmdlet = 1, a program = its own code.
                "$farlinkOk = $?\nif ($LASTEXITCODE) { exit $LASTEXITCODE }\nif (-not $farlinkOk) { exit 1 }".to_string()
            };
            format!(
                "$ProgressPreference = 'SilentlyContinue'\n[Console]::OutputEncoding = [Text.Encoding]::UTF8\n$OutputEncoding = [Text.Encoding]::UTF8\n{line}\n{tail}"
            )
        }
    }
}

/// PowerShell's `-EncodedCommand`: base64 of the UTF-16LE text (no quoting
/// problems whatever was typed).
pub fn powershell_encoded(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    crate::util::base64_encode(&bytes)
}

fn command(script: &str, shell: Shell, cwd: &Path, prev: Option<&Path>) -> std::process::Command {
    #[cfg(unix)]
    let mut c = {
        use std::os::unix::process::CommandExt;
        let _ = shell; // always the POSIX shell here
        let sh = if Path::new("/bin/bash").exists() { "/bin/bash" } else { "/bin/sh" };
        let mut c = std::process::Command::new(sh);
        c.arg("-c").arg(script);
        // Its own process group: "Stop" ends everything it started.
        c.process_group(0);
        c
    };
    #[cfg(windows)]
    let mut c = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut c = if shell == Shell::PowerShell {
            let mut c = std::process::Command::new("powershell.exe");
            c.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"]);
            c.arg(powershell_encoded(script));
            c
        } else {
            let mut c = std::process::Command::new("cmd.exe");
            // Passed as typed (cmd has its own quoting); output in UTF-8.
            c.raw_arg(format!("/D /S /C \"chcp 65001>nul & {script}\""));
            c
        };
        c.creation_flags(CREATE_NO_WINDOW);
        c
    };
    c.current_dir(cwd)
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("SYSTEMD_PAGER", "cat")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(p) = prev {
        c.env("OLDPWD", p);
    }
    c
}

/// Take the complete UTF-8 text out of `pending`, leaving a character cut
/// in half at the end for the next read. Bytes that are not UTF-8 become �.
pub fn take_text(pending: &mut Vec<u8>) -> String {
    let mut out = String::new();
    loop {
        match std::str::from_utf8(pending) {
            Ok(s) => {
                out.push_str(s);
                pending.clear();
                return out;
            }
            Err(e) => {
                let good = e.valid_up_to();
                out.push_str(std::str::from_utf8(&pending[..good]).unwrap());
                match e.error_len() {
                    None => {
                        pending.drain(..good);
                        return out;
                    }
                    Some(bad) => {
                        out.push('\u{FFFD}');
                        pending.drain(..good + bad);
                    }
                }
            }
        }
    }
}

/// Read one of the command's outputs until it ends: sent piece by piece, or
/// kept (`keep`) for a folder change. Waits when the connection is slow
/// (bounded channel), so a flood of output can't pile up in memory.
fn pump(
    mut from: impl Read,
    req: u32,
    err: bool,
    out: mpsc::Sender<Message>,
    keep: Option<Arc<Mutex<String>>>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    let mut pending = Vec::new();
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => 0,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        let mut text = take_text(&mut pending);
        if n == 0 && !pending.is_empty() {
            text.push_str(&String::from_utf8_lossy(&pending));
            pending.clear();
        }
        if !text.is_empty() {
            match &keep {
                Some(k) => k.lock().unwrap().push_str(&text),
                None => {
                    if out
                        .blocking_send(json(&SessionMsg::ShellOutput { req, text, err }))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
        if n == 0 {
            return;
        }
    }
}

/// End process `pid` and everything it started.
fn kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .creation_flags(0x0800_0000)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

impl ShellService {
    /// `allowed`: the relay opened this session for the administrator.
    pub fn new(allowed: bool) -> ShellService {
        ShellService::starting_in(allowed, start_folder())
    }

    pub fn starting_in(allowed: bool, cwd: PathBuf) -> ShellService {
        ShellService {
            allowed,
            state: Arc::new(Mutex::new(State {
                cwd,
                prev: None,
                running: None,
            })),
        }
    }

    /// Handle a shell request (see [`SessionMsg::is_shell_request`]).
    pub async fn handle(&mut self, m: SessionMsg, out: &mpsc::Sender<Message>) {
        match m {
            SessionMsg::ShellRun { req, command, powershell } => {
                if let Some(reply) = self.run(req, &command, Shell::here(powershell), out) {
                    let _ = out.send(json(&reply)).await;
                }
            }
            SessionMsg::ShellInput { req, text } => {
                let mut st = self.state.lock().unwrap();
                if let Some(r) = st.running.as_mut().filter(|r| r.req == req) {
                    if text.is_empty() {
                        r.stdin = None; // empty = end of input (Ctrl+D)
                    } else if let Some(tx) = &r.stdin {
                        let _ = tx.send(text);
                    }
                }
            }
            SessionMsg::ShellStop { req } => self.stop(Some(req)),
            _ => {}
        }
    }

    /// Stop the running command (only if it is `req`, when given).
    pub fn stop(&self, req: Option<u32>) {
        let st = self.state.lock().unwrap();
        if let Some(r) = st.running.as_ref().filter(|r| req.is_none_or(|q| q == r.req)) {
            kill_tree(r.pid);
        }
    }

    /// Start a command; the answer when it can't start (or is only a question
    /// for the folder).
    fn run(&mut self, req: u32, line: &str, shell: Shell, out: &mpsc::Sender<Message>) -> Option<SessionMsg> {
        let mut st = self.state.lock().unwrap();
        let cwd = st.cwd.display().to_string();
        let done = |error: Option<&str>| SessionMsg::ShellDone {
            req,
            code: None,
            cwd: cwd.clone(),
            error: error.map(str::to_string),
        };
        if !self.allowed {
            return Some(done(Some("Only the administrator can use the shell")));
        }
        if line.trim().is_empty() {
            return Some(done(None));
        }
        if st.running.is_some() {
            return Some(done(Some("Another command is still running")));
        }
        // The folder may have gone away meanwhile: start from where a new shell would.
        if !st.cwd.is_dir() {
            st.cwd = start_folder();
        }
        let cd = is_cd(line, shell);
        let mut child = match command(&script(line, cd, shell), shell, &st.cwd, st.prev.as_deref()).spawn() {
            Ok(c) => c,
            Err(e) => return Some(done(Some(&format!("cannot start the shell: {e}")))),
        };
        log::debug!("shell: command {req} started");
        let (stdout, stderr, stdin) = (child.stdout.take(), child.stderr.take(), child.stdin.take());
        let (in_tx, in_rx) = std::sync::mpsc::channel::<String>();
        if let Some(mut stdin) = stdin {
            std::thread::spawn(move || {
                for text in in_rx {
                    if stdin.write_all(text.as_bytes()).and_then(|_| stdin.flush()).is_err() {
                        break;
                    }
                }
            });
        }
        st.running = Some(Running {
            req,
            pid: child.id(),
            stdin: Some(in_tx),
        });
        drop(st);
        let kept = cd.then(|| Arc::new(Mutex::new(String::new())));
        let (ended_tx, ended_rx) = std::sync::mpsc::channel::<()>();
        for (pipe, err) in [
            (stdout.map(|p| Box::new(p) as Box<dyn Read + Send>), false),
            (stderr.map(|p| Box::new(p) as Box<dyn Read + Send>), true),
        ] {
            let Some(pipe) = pipe else { continue };
            let (out, keep, ended) = (out.clone(), if err { None } else { kept.clone() }, ended_tx.clone());
            std::thread::spawn(move || {
                pump(pipe, req, err, out, keep);
                let _ = ended.send(());
            });
        }
        drop(ended_tx);
        let (state, out) = (self.state.clone(), out.clone());
        std::thread::spawn(move || {
            let status = child.wait();
            // Output first, then "finished".
            let deadline = std::time::Instant::now() + OUTPUT_GRACE;
            for _ in 0..2 {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if ended_rx.recv_timeout(left).is_err() {
                    break;
                }
            }
            let (code, error) = match &status {
                Ok(s) if s.code().is_some() => (s.code(), None),
                Ok(_) => (None, Some("stopped".to_string())),
                Err(e) => (None, Some(e.to_string())),
            };
            let mut st = state.lock().unwrap();
            st.running = None;
            if code == Some(0)
                && let Some(kept) = kept
            {
                let printed = kept.lock().unwrap().clone();
                let to = printed.lines().map(str::trim).rfind(|l| !l.is_empty()).map(PathBuf::from);
                if let Some(to) = to.filter(|p| p.is_dir()) {
                    let from = std::mem::replace(&mut st.cwd, to);
                    st.prev = Some(from);
                }
            }
            let cwd = st.cwd.display().to_string();
            drop(st);
            log::debug!("shell: command {req} ended");
            let _ = out.blocking_send(json(&SessionMsg::ShellDone { req, code, cwd, error }));
        });
        None
    }
}

impl Drop for ShellService {
    /// The session ended: so does whatever still runs in its shell.
    fn drop(&mut self) {
        self.stop(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_changes_are_recognised() {
        assert!(is_cd("cd", Shell::Sh));
        assert!(is_cd("cd /tmp", Shell::Sh));
        assert!(is_cd("  cd ~/My\\ Files ", Shell::Sh));
        assert!(is_cd("cd -", Shell::Sh));
        assert!(!is_cd("cd /tmp && make", Shell::Sh));
        assert!(!is_cd("cd $(mktemp -d)", Shell::Sh));
        assert!(!is_cd("cdrecord", Shell::Sh));
        assert!(!is_cd("ls", Shell::Sh));

        assert!(is_cd("cd C:\\Users", Shell::Cmd));
        assert!(is_cd("CD /d D:\\Work", Shell::Cmd));
        assert!(is_cd("chdir ..", Shell::Cmd));
        assert!(is_cd("cd..", Shell::Cmd));
        assert!(is_cd("D:", Shell::Cmd));
        assert!(!is_cd("cd", Shell::Cmd), "Windows' bare cd only prints the folder");
        assert!(!is_cd("cd x & dir", Shell::Cmd));
        assert!(!is_cd("cdx", Shell::Cmd));

        let ps = Shell::PowerShell;
        assert!(is_cd("Set-Location C:\\Windows", ps));
        assert!(is_cd("sl ..", ps));
        assert!(is_cd("cd $env:USERPROFILE", ps));
        assert!(is_cd("cd", ps));
        assert!(is_cd("E:", ps));
        assert!(!is_cd("cd C:\\x; Get-ChildItem", ps));
        assert!(!is_cd("Get-ChildItem | Select -First 3", ps));
        assert!(!is_cd("cdd", ps));
    }

    #[test]
    fn powershell_gets_utf8_a_real_exit_code_and_the_folder_after_cd() {
        let run = script("Get-Service", false, Shell::PowerShell);
        assert!(run.contains("[Console]::OutputEncoding = [Text.Encoding]::UTF8"));
        assert!(run.contains("\nGet-Service\n"));
        assert!(run.contains("exit $LASTEXITCODE"));
        let cd = script("cd C:\\Temp", true, Shell::PowerShell);
        assert!(cd.contains("\ncd C:\\Temp\nif ($?) { (Get-Location).ProviderPath }"));
        assert_eq!(script("cd /tmp", true, Shell::Sh), "cd /tmp >/dev/null && pwd");
        assert_eq!(script("cd /d D:\\x", true, Shell::Cmd), "cd /d D:\\x && cd");
        assert_eq!(script("dir", false, Shell::Cmd), "dir");
    }

    #[test]
    fn powershell_commands_go_encoded_as_utf16() {
        // What `[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('dir "a b"'))` gives.
        assert_eq!(powershell_encoded("dir \"a b\""), "ZABpAHIAIAAiAGEAIABiACIA");
        let text = "Write-Output 'Привет'";
        let bytes = crate::util::base64_decode(&powershell_encoded(text)).unwrap();
        let units: Vec<u16> = bytes.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16(&units).unwrap(), text);
    }

    #[test]
    fn text_is_cut_only_between_characters() {
        let mut pending = b"ab\xD0".to_vec(); // ends with the first half of "Ж"
        assert_eq!(take_text(&mut pending), "ab");
        assert_eq!(pending, b"\xD0");
        pending.push(0x96);
        assert_eq!(take_text(&mut pending), "Ж");
        assert!(pending.is_empty());
        let mut bad = b"a\xFFb".to_vec();
        assert_eq!(take_text(&mut bad), "a\u{FFFD}b");
    }

    #[cfg(unix)]
    async fn run_to_end(sh: &mut ShellService, req: u32, line: &str) -> (String, String, SessionMsg) {
        let (tx, mut rx) = mpsc::channel(32);
        sh.handle(SessionMsg::ShellRun { req, command: line.into(), powershell: false }, &tx).await;
        let (mut out, mut err) = (String::new(), String::new());
        loop {
            let m = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("answer").unwrap();
            let Message::Text(t) = m else { continue };
            match serde_json::from_str::<SessionMsg>(&t).unwrap() {
                SessionMsg::ShellOutput { text, err: true, .. } => err.push_str(&text),
                SessionMsg::ShellOutput { text, .. } => out.push_str(&text),
                done => return (out, err, done),
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn commands_run_in_the_shell_folder_and_cd_is_remembered() {
        let dir = crate::util::unique_temp_dir("farlink-shell");
        std::fs::create_dir_all(dir.join("sub dir")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let mut sh = ShellService::starting_in(true, dir.clone());

        let (out, _, done) = run_to_end(&mut sh, 1, "echo hello; echo oops >&2; exit 3").await;
        assert_eq!(out, "hello\n");
        let SessionMsg::ShellDone { code, cwd, .. } = done else { panic!("{done:?}") };
        assert_eq!(code, Some(3));
        assert_eq!(cwd, dir.display().to_string());

        let (out, _, done) = run_to_end(&mut sh, 2, "cd 'sub dir'").await;
        assert_eq!(out, "", "the folder change prints nothing");
        let SessionMsg::ShellDone { code: Some(0), cwd, .. } = done else { panic!("{done:?}") };
        assert_eq!(cwd, dir.join("sub dir").display().to_string());
        let (out, _, _) = run_to_end(&mut sh, 3, "pwd").await;
        assert_eq!(out.trim(), dir.join("sub dir").display().to_string());

        let (_, _, done) = run_to_end(&mut sh, 4, "cd -").await;
        let SessionMsg::ShellDone { cwd, .. } = done else { panic!() };
        assert_eq!(cwd, dir.display().to_string(), "cd - goes back");

        let (_, err, done) = run_to_end(&mut sh, 5, "cd /no/such/folder").await;
        assert!(!err.is_empty());
        let SessionMsg::ShellDone { code, cwd, .. } = done else { panic!() };
        assert_ne!(code, Some(0));
        assert_eq!(cwd, dir.display().to_string(), "a failed cd stays put");

        // Standard input reaches the command; empty input ends it.
        let (tx, mut rx) = mpsc::channel(32);
        sh.handle(SessionMsg::ShellRun { req: 6, command: "read a; echo got $a; cat".into(), powershell: false }, &tx).await;
        sh.handle(SessionMsg::ShellInput { req: 6, text: "x\n".into() }, &tx).await;
        sh.handle(SessionMsg::ShellInput { req: 6, text: String::new() }, &tx).await;
        let mut out = String::new();
        loop {
            let Message::Text(t) = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap() else { continue };
            match serde_json::from_str::<SessionMsg>(&t).unwrap() {
                SessionMsg::ShellOutput { text, .. } => out.push_str(&text),
                SessionMsg::ShellDone { code, .. } => {
                    assert_eq!(code, Some(0));
                    break;
                }
                m => panic!("{m:?}"),
            }
        }
        assert_eq!(out, "got x\n");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_running_command_can_be_stopped_and_one_runs_at_a_time() {
        let mut sh = ShellService::starting_in(true, std::env::temp_dir());
        let (tx, mut rx) = mpsc::channel(32);
        sh.handle(SessionMsg::ShellRun { req: 1, command: "sleep 30 & sleep 30".into(), powershell: false }, &tx).await;
        let (_, _, busy) = run_to_end(&mut sh, 2, "echo second").await;
        assert!(matches!(busy, SessionMsg::ShellDone { req: 2, error: Some(_), .. }), "{busy:?}");
        let started = std::time::Instant::now();
        sh.handle(SessionMsg::ShellStop { req: 1 }, &tx).await;
        let Message::Text(t) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap() else { panic!() };
        let done: SessionMsg = serde_json::from_str(&t).unwrap();
        assert!(matches!(done, SessionMsg::ShellDone { req: 1, code: None, error: Some(_), .. }), "{done:?}");
        assert!(started.elapsed() < Duration::from_secs(4), "background children stop too (pipe closed)");
    }

    #[tokio::test]
    async fn only_the_administrator_gets_a_shell() {
        let mut sh = ShellService::starting_in(false, std::env::temp_dir());
        let (tx, mut rx) = mpsc::channel(4);
        sh.handle(SessionMsg::ShellRun { req: 1, command: "echo hi".into(), powershell: false }, &tx).await;
        let Message::Text(t) = rx.recv().await.unwrap() else { panic!() };
        let m: SessionMsg = serde_json::from_str(&t).unwrap();
        assert!(matches!(m, SessionMsg::ShellDone { code: None, error: Some(_), .. }), "{m:?}");
    }
}
