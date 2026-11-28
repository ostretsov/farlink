//! MCP server: lets an AI assistant (Claude Code, Claude Desktop, any MCP
//! client) use the computers of a Farlink relay — list them, look at their
//! screens, click and type, run shell commands and move files.
//!
//! ```text
//! farlink mcp [--server https://remote.example.com] [--user anna] [--read-only]
//! ```
//!
//! It speaks JSON-RPC 2.0 over stdin/stdout (one message per line, the MCP
//! "stdio" transport) and signs in to the relay like the desktop app does:
//! `FARLINK_PASSWORD` (with `--user` / `FARLINK_USER` for a person), else
//! `FARLINK_TOKEN`, else the desktop app's saved sign-in. Everything goes
//! through the relay's normal checks: a person reaches only their own
//! computers and never gets the shell (administrator only).
//!
//! Each computer gets one control session, opened on first use and kept
//! (no screens watched, so nothing is captured until a screenshot is asked
//! for: those come from the helper at full size, like "Take screenshot").

use crate::net::{Api, ConnState, SessionHandle, SessionShared, ShellText, Target, TransferState};
use crate::proto::{FsEntry, InputEvent, MonitorInfo, MouseButton, SessionMsg};
use crate::server::{MachineView, Overview};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// MCP versions this server knows (newest last); a client asking for
/// another one gets the newest.
const PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

/// Longest side of a screenshot unless the call says otherwise (pictures
/// much bigger than this are scaled down by the model anyway).
pub const SHOT_SIZE: u32 = 1280;
/// Most output of one command handed back (the end is kept).
pub const MAX_OUTPUT: usize = 100 * 1024;
/// Biggest file `read_file` returns as text.
pub const MAX_READ: u64 = 2 * 1024 * 1024;
const DEFAULT_TIMEOUT: u64 = 120;
const MAX_TIMEOUT: u64 = 3600;
/// An open connection that does not come up within this long has failed.
const CONNECT_WAIT: f32 = 20.0;
/// A transfer that moves no bytes for this long has failed.
const STALL: Duration = Duration::from_secs(60);
/// `wait_until_still`: time between two looks at the screen…
const STILL_GAP: Duration = Duration::from_millis(300);
/// …and the longest it waits before handing back the latest picture.
const STILL_LIMIT: Duration = Duration::from_secs(5);

/// How to sign in to the relay.
#[derive(Debug, Clone, Default)]
pub struct Login {
    pub server: String,
    /// Empty = the administrator.
    pub user: String,
    pub password: Option<String>,
    pub token: Option<String>,
}

impl Login {
    /// From the command line, the environment and the desktop app's saved
    /// settings, in that order.
    pub fn find(args: &crate::util::Args) -> Result<Login> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let saved: crate::config::ViewerConfig =
            crate::config::load_json(&crate::config::viewer_dir().join("viewer.json"));
        let server = args
            .get("server")
            .map(str::to_string)
            .or_else(|| env("FARLINK_SERVER"))
            .unwrap_or_else(|| saved.server_url.clone());
        let server = crate::util::normalize_server_url(&server);
        if server.is_empty() {
            bail!(
                "No server: pass --server https://your-relay (or set FARLINK_SERVER), \
                 or sign in once in the Farlink app"
            );
        }
        let password = env("FARLINK_PASSWORD");
        let explicit_user = args
            .get("user")
            .map(str::to_string)
            .or_else(|| env("FARLINK_USER"));
        let mut token = env("FARLINK_TOKEN");
        let same_server = crate::util::normalize_server_url(&saved.server_url) == server;
        let mut user = explicit_user.clone().unwrap_or_default();
        if password.is_none() && token.is_none() && same_server {
            token = saved.admin_token.clone();
            if explicit_user.is_none() {
                user = saved.user_name.clone();
            }
        }
        if password.is_none() && token.is_none() {
            bail!(
                "Not signed in: set FARLINK_PASSWORD (and --user for a person), \
                 or FARLINK_TOKEN, or sign in to {server} once in the Farlink app"
            );
        }
        Ok(Login {
            server,
            user,
            password,
            token,
        })
    }
}

/// The MCP server's state: the relay sign-in and one session per computer.
pub struct Mcp {
    api: Api,
    login: Login,
    /// Signed in as the administrator (shell, notes).
    admin: bool,
    /// Only look: no input, shell, uploads or new folders.
    read_only: bool,
    sessions: HashMap<String, SessionHandle>,
    /// Size of the last screenshot per (machine, monitor index): click
    /// coordinates are in its pixels.
    shot_size: HashMap<(String, u8), (u32, u32)>,
}

impl Mcp {
    /// Sign in and find out who we are.
    pub fn connect(login: Login, read_only: bool) -> Result<Mcp> {
        let token = match (&login.password, &login.token) {
            (Some(pw), _) => Api::login(&login.server, &login.user, pw)?,
            (None, Some(t)) => t.clone(),
            (None, None) => bail!("no password or token"),
        };
        let api = Api {
            server: login.server.clone(),
            token,
        };
        let ov = api.overview()?;
        let mut mcp = Mcp::new(api, ov.role == "admin", read_only);
        mcp.login = login;
        Ok(mcp)
    }

    /// With a sign-in made elsewhere (tests).
    pub fn new(api: Api, admin: bool, read_only: bool) -> Mcp {
        Mcp {
            login: Login {
                server: api.server.clone(),
                ..Default::default()
            },
            api,
            admin,
            read_only,
            sessions: HashMap::new(),
            shot_size: HashMap::new(),
        }
    }

    /// One JSON-RPC message (a line from the client) → the line to answer
    /// with (`None` for notifications).
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let reply = match serde_json::from_str::<Value>(line) {
            Err(e) => Some(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))),
            Ok(Value::Array(batch)) => {
                let out: Vec<Value> = batch.into_iter().filter_map(|m| self.handle(m)).collect();
                (!out.is_empty()).then_some(Value::Array(out))
            }
            Ok(m) => self.handle(m),
        };
        reply.map(|r| r.to_string())
    }

    /// One JSON-RPC message → its answer (`None` for notifications).
    pub fn handle(&mut self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        // Notifications (and answers to requests we never make) need no reply.
        let id = id?;
        let result = match method {
            "initialize" => {
                let asked = params["protocolVersion"].as_str().unwrap_or("");
                let version = PROTOCOLS
                    .iter()
                    .find(|v| **v == asked)
                    .unwrap_or(PROTOCOLS.last().unwrap());
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "farlink", "title": "Farlink", "version": crate::VERSION },
                    "instructions": INSTRUCTIONS,
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({ "tools": self.tools() }),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("").to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match self.call(&name, &args) {
                    Ok(content) => json!({ "content": content, "isError": false }),
                    Err(e) => json!({ "content": [text(&format!("{e:#}"))], "isError": true }),
                }
            }
            "" => return Some(rpc_error(id, -32600, "invalid request")),
            other => return Some(rpc_error(id, -32601, &format!("unknown method {other}"))),
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    /// The tools this sign-in may use.
    pub fn tools(&self) -> Vec<Value> {
        tool_list()
            .into_iter()
            .filter(|t| {
                let name = t["name"].as_str().unwrap_or("");
                (self.admin || name != "run_command")
                    && (!self.read_only || READ_TOOLS.contains(&name))
            })
            .collect()
    }

    /// Run one tool.
    pub fn call(&mut self, name: &str, args: &Value) -> Result<Vec<Value>> {
        if !self.tools().iter().any(|t| t["name"] == name) {
            if name == "run_command" && !self.admin {
                bail!("The remote shell is for the administrator only");
            }
            if self.read_only && tool_list().iter().any(|t| t["name"] == name) {
                bail!("{name} is switched off: this Farlink MCP server runs with --read-only");
            }
            bail!("Unknown tool {name}");
        }
        let a = ArgList(args);
        if name == "list_computers" {
            let ov = self.overview()?;
            return Ok(vec![text(&list_computers(&ov, a.opt_str("search")))]);
        }
        let machine = self.machine(a.str("computer")?)?;
        match name {
            "screenshot" => self.screenshot(&machine, &a),
            "click" | "move_mouse" | "drag" | "scroll" | "type_text" | "press_keys" => {
                let events = self.input_events(&machine, name, &a)?;
                let h = self.session(&machine)?;
                for event in events {
                    h.send(SessionMsg::Input { event });
                }
                // Give the computer a moment to react before the next look.
                std::thread::sleep(Duration::from_millis(150));
                Ok(vec![text(
                    "Done. Take a screenshot to see the result (wait_until_still: true if something is still opening or loading).",
                )])
            }
            "run_command" => self.run_command(&machine, &a),
            "list_files" => self.list_files(&machine, a.opt_str("path").unwrap_or("")),
            "read_file" => self.read_file(&machine, &a),
            "download_file" => self.download_file(&machine, &a),
            "upload_file" => self.upload_file(&machine, &a),
            "write_file" => self.write_file(&machine, &a),
            "make_folder" => self.make_folder(&machine, a.str("path")?),
            _ => bail!("Unknown tool {name}"),
        }
    }

    /// The relay's overview (signs in again with the password if the
    /// sign-in ran out).
    fn overview(&mut self) -> Result<Overview> {
        match self.api.overview() {
            Ok(ov) => Ok(ov),
            Err(e) => match &self.login.password {
                Some(pw) if e.to_string().contains("signed out") => {
                    self.api.token = Api::login(&self.login.server, &self.login.user, pw)?;
                    self.sessions.clear();
                    self.api.overview()
                }
                _ => Err(e),
            },
        }
    }

    fn machine(&mut self, query: &str) -> Result<MachineView> {
        let ov = self.overview()?;
        find_machine(&ov.machines, query).cloned()
    }

    /// This computer's session, opened (and waited for) when needed.
    fn session(&mut self, m: &MachineView) -> Result<&SessionHandle> {
        let id = &m.machine.id;
        if self.sessions.get(id).is_some_and(|h| h.is_closed()) {
            self.sessions.remove(id);
        }
        if !self.sessions.contains_key(id) {
            if !m.online {
                bail!("{} is offline right now", m.machine.name);
            }
            let target = Target::Admin {
                server: self.api.server.clone(),
                token: self.api.token.clone(),
                machine_id: id.clone(),
                view_only: false,
            };
            let h = SessionHandle::start(target, Default::default(), false, Arc::new(|| {}));
            wait(&h, CONNECT_WAIT, "connecting", |s| {
                (s.state == ConnState::Live).then_some(())
            })
            .with_context(|| format!("Cannot connect to {}", m.machine.name))?;
            // Nothing to capture: screenshots come from the helper on their own.
            h.send(SessionMsg::Watch { monitors: vec![] });
            self.sessions.insert(id.clone(), h);
        }
        Ok(&self.sessions[id])
    }

    fn screenshot(&mut self, m: &MachineView, a: &ArgList) -> Result<Vec<Value>> {
        let mon = monitor(m, a)?;
        if !m.online {
            bail!("{} is offline right now", m.machine.name);
        }
        let max = a
            .num("max_size")
            .map(|v| v as u32)
            .unwrap_or(SHOT_SIZE)
            .clamp(256, 4096);
        let (frame, waited) = if a.bool("wait_until_still") {
            let (f, s) = wait_until_still(
                || self.api.screenshot(&m.machine.id, mon.index),
                STILL_GAP,
                STILL_LIMIT,
            )?;
            (f, Some(s))
        } else {
            (self.api.screenshot(&m.machine.id, mon.index)?, None)
        };
        let (w, h, jpeg) = crate::codec::thumbnail_jpeg(&frame, max, 85)?;
        self.shot_size
            .insert((m.machine.id.clone(), mon.index), (w, h));
        let screens = m.machine.monitors.len();
        let mut about = format!(
            "{}, screen {} of {screens} ({}×{}), picture {w}×{h}. Click coordinates are pixels of this picture.",
            m.machine.name,
            mon.index + 1,
            frame.width,
            frame.height,
        );
        match waited {
            Some(Still::Stopped(t)) => {
                about += &format!(" The screen stopped changing after {:.1} s.", t.as_secs_f32())
            }
            Some(Still::KeptChanging(t)) => {
                about += &format!(
                    " The screen kept changing for {:.0} s; this is the latest picture.",
                    t.as_secs_f32()
                )
            }
            None => {}
        }
        Ok(vec![
            json!({ "type": "image", "data": crate::util::base64_encode(&jpeg), "mimeType": "image/jpeg" }),
            text(&about),
        ])
    }

    /// The input a mouse / keyboard tool stands for.
    fn input_events(&self, m: &MachineView, tool: &str, a: &ArgList) -> Result<Vec<InputEvent>> {
        let point = |xk: &str, yk: &str| -> Result<(u8, f32, f32)> {
            let mon = monitor(m, a)?;
            let size = self
                .shot_size
                .get(&(m.machine.id.clone(), mon.index))
                .copied()
                .unwrap_or_else(|| scaled_size(mon.width, mon.height, SHOT_SIZE));
            let x = a.num(xk).with_context(|| format!("{xk} is missing"))?;
            let y = a.num(yk).with_context(|| format!("{yk} is missing"))?;
            let (fx, fy) = to_fraction(x, y, size);
            Ok((mon.index, fx, fy))
        };
        let mut ev = Vec::new();
        match tool {
            "click" => {
                let (mon, x, y) = point("x", "y")?;
                let button = match a.opt_str("button").unwrap_or("left") {
                    "left" => MouseButton::Left,
                    "right" => MouseButton::Right,
                    "middle" => MouseButton::Middle,
                    other => bail!("button must be left, right or middle, not {other}"),
                };
                ev.push(InputEvent::MouseMove { mon, x, y });
                let times = if a.bool("double") { 2 } else { 1 };
                for _ in 0..times {
                    for down in [true, false] {
                        ev.push(InputEvent::MouseButton {
                            mon,
                            x,
                            y,
                            button,
                            down,
                        });
                    }
                }
            }
            "move_mouse" => {
                let (mon, x, y) = point("x", "y")?;
                ev.push(InputEvent::MouseMove { mon, x, y });
            }
            "drag" => {
                let (mon, x0, y0) = point("from_x", "from_y")?;
                let (_, x1, y1) = point("to_x", "to_y")?;
                let button = MouseButton::Left;
                ev.push(InputEvent::MouseMove { mon, x: x0, y: y0 });
                ev.push(InputEvent::MouseButton {
                    mon,
                    x: x0,
                    y: y0,
                    button,
                    down: true,
                });
                for i in 1..=10 {
                    let t = i as f32 / 10.0;
                    ev.push(InputEvent::MouseMove {
                        mon,
                        x: x0 + (x1 - x0) * t,
                        y: y0 + (y1 - y0) * t,
                    });
                }
                ev.push(InputEvent::MouseButton {
                    mon,
                    x: x1,
                    y: y1,
                    button,
                    down: false,
                });
            }
            "scroll" => {
                if a.num("x").is_some() || a.num("y").is_some() {
                    let (mon, x, y) = point("x", "y")?;
                    ev.push(InputEvent::MouseMove { mon, x, y });
                }
                let amount = a
                    .num("amount")
                    .context("amount is missing")?
                    .clamp(-50.0, 50.0) as f32;
                ev.push(InputEvent::Scroll {
                    dx: 0.0,
                    dy: amount,
                });
            }
            "type_text" => {
                let t = a.str("text")?;
                for (i, line) in t.split('\n').enumerate() {
                    if i > 0 {
                        ev.extend(press(&["Enter".to_string()]));
                    }
                    let line = line.trim_end_matches('\r');
                    if !line.is_empty() {
                        ev.push(InputEvent::Text {
                            text: line.to_string(),
                        });
                    }
                }
            }
            "press_keys" => {
                for combo in a.str("keys")?.split_whitespace() {
                    let names = key_combo(combo)?;
                    if is_ctrl_alt_del(&names) {
                        ev.push(InputEvent::CtrlAltDel);
                    } else {
                        ev.extend(press(&names));
                    }
                }
                if ev.is_empty() {
                    bail!("keys is empty");
                }
            }
            _ => unreachable!(),
        }
        Ok(ev)
    }

    fn run_command(&mut self, m: &MachineView, a: &ArgList) -> Result<Vec<Value>> {
        let command = a.str("command")?.to_string();
        if command.trim().is_empty() {
            bail!("command is empty");
        }
        let timeout = a
            .num("timeout_secs")
            .map(|v| v as u64)
            .unwrap_or(DEFAULT_TIMEOUT)
            .clamp(1, MAX_TIMEOUT);
        let stdin = a.opt_str("stdin").map(str::to_string);
        let powershell = a.opt_bool("powershell").unwrap_or(true);
        let name = m.machine.name.clone();
        let h = self.session(m)?;
        // The first time: ask where the shell is (tells Windows apart too).
        if h.shared.lock().unwrap().shell.cwd.is_none() {
            h.shell_hello();
            wait(h, 15.0, "the remote shell", |s| {
                s.shell.cwd.is_some().then_some(())
            })
            .context("The remote shell did not answer (is the helper older than this app?)")?;
        }
        {
            let mut s = h.shared.lock().unwrap();
            if s.shell.running.is_some() {
                bail!("A command is still running on {name}");
            }
            s.shell.clear();
            s.shell.done = None;
            s.shell.powershell = powershell;
        }
        h.shell_run(&command);
        let req = h
            .shared
            .lock()
            .unwrap()
            .shell
            .running
            .context("the command did not start")?;
        if let Some(t) = &stdin
            && !t.is_empty()
        {
            h.shell_input(t);
        }
        h.shell_input(""); // nobody types: end of its input
        let finished = |s: &mut SessionShared| s.shell.done.clone().filter(|d| d.0 == req);
        let mut stopped = false;
        let done = match wait(h, timeout as f32, "command", finished) {
            Ok(d) => Some(d),
            Err(_) if !h.is_closed() => {
                stopped = true;
                h.shell_stop();
                wait(h, 10.0, "stopping", finished).ok()
            }
            Err(e) => return Err(e),
        };
        let (output, cwd) = {
            let s = h.shared.lock().unwrap();
            let out: String = s
                .shell
                .parts
                .iter()
                .filter(|(k, _)| matches!(k, ShellText::Out | ShellText::Err))
                .map(|(_, t)| t.as_str())
                .collect();
            (out, s.shell.cwd.clone().unwrap_or_default())
        };
        if done.is_none() {
            // It never ended: a fresh connection next time.
            self.sessions.remove(&m.machine.id);
        }
        let mut out = keep_end(&output, MAX_OUTPUT);
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if stopped {
            out.push_str(&format!("[stopped after {timeout} s]\n"));
        }
        match done {
            Some((_, _, Some(e))) => out.push_str(&format!("[{e}]\n")),
            Some((_, Some(code), None)) => out.push_str(&format!("[exit code {code}]\n")),
            _ => {}
        }
        out.push_str(&format!("[folder: {cwd}]"));
        Ok(vec![text(&out)])
    }

    fn listing(&mut self, m: &MachineView, path: &str) -> Result<crate::net::RemoteDir> {
        let h = self.session(m)?;
        h.list_remote(path);
        let dir = wait(h, 30.0, "folder listing", |s| {
            (!s.remote_dir.loading).then(|| s.remote_dir.clone())
        })?;
        if let Some(e) = &dir.error {
            bail!("{path}: {e}");
        }
        Ok(dir)
    }

    fn list_files(&mut self, m: &MachineView, path: &str) -> Result<Vec<Value>> {
        let dir = self.listing(m, path)?;
        Ok(vec![text(&format_listing(&dir))])
    }

    /// Size of a remote file (from its folder's listing).
    fn remote_file(&mut self, m: &MachineView, path: &str) -> Result<FsEntry> {
        let (parent, name) =
            split_path(path).with_context(|| format!("{path} is not a file path"))?;
        let dir = self.listing(m, &parent)?;
        let e = dir
            .entries
            .into_iter()
            .find(|e| e.name == name)
            .with_context(|| format!("{path} does not exist"))?;
        if e.is_dir {
            bail!("{path} is a folder (use list_files)");
        }
        Ok(e)
    }

    /// Copy a remote file into `local_dir`; returns where it went.
    fn fetch(&mut self, m: &MachineView, entry: &FsEntry, local_dir: PathBuf) -> Result<PathBuf> {
        let h = self.session(m)?;
        let req = h.download(entry, local_dir);
        wait_transfer(h, req)
    }

    fn read_file(&mut self, m: &MachineView, a: &ArgList) -> Result<Vec<Value>> {
        let path = a.str("path")?;
        let entry = self.remote_file(m, path)?;
        if entry.size > MAX_READ {
            bail!(
                "{path} is {} bytes, more than read_file returns ({MAX_READ}); use download_file",
                entry.size
            );
        }
        let dir = crate::util::unique_temp_dir("farlink-mcp-read");
        let got = self
            .fetch(m, &entry, dir.clone())
            .and_then(|p| Ok(std::fs::read(p)?));
        let _ = std::fs::remove_dir_all(&dir);
        let data = got?;
        match String::from_utf8(data) {
            Ok(t) => Ok(vec![text(&t)]),
            Err(e) => bail!(
                "{path} is not text ({} bytes); use download_file",
                e.into_bytes().len()
            ),
        }
    }

    fn download_file(&mut self, m: &MachineView, a: &ArgList) -> Result<Vec<Value>> {
        let path = a.str("path")?;
        let local_dir = match a.opt_str("local_folder") {
            Some(d) => PathBuf::from(d),
            None => dirs::download_dir()
                .or_else(dirs::home_dir)
                .unwrap_or_else(std::env::temp_dir),
        };
        if !local_dir.is_dir() {
            bail!("{} is not a folder on this computer", local_dir.display());
        }
        let entry = self.remote_file(m, path)?;
        let local = self.fetch(m, &entry, local_dir)?;
        Ok(vec![text(&format!(
            "Saved {path} ({} bytes) as {}",
            entry.size,
            local.display()
        ))])
    }

    /// Copy a local file into a remote folder; returns its remote path.
    fn send_file(&mut self, m: &MachineView, local: PathBuf, folder: &str) -> Result<String> {
        if !local.is_file() {
            bail!("{} is not a file on this computer", local.display());
        }
        let h = self.session(m)?;
        let req = h.upload(local, folder.to_string());
        wait_transfer(h, req)?;
        let s = h.shared.lock().unwrap();
        Ok(s.transfers
            .iter()
            .find(|t| t.req == req)
            .and_then(|t| t.remote.clone())
            .unwrap_or_default())
    }

    fn upload_file(&mut self, m: &MachineView, a: &ArgList) -> Result<Vec<Value>> {
        let local = PathBuf::from(a.str("local_path")?);
        let remote = self.send_file(m, local.clone(), a.str("folder")?)?;
        Ok(vec![text(&format!(
            "Uploaded {} as {remote}",
            local.display()
        ))])
    }

    fn write_file(&mut self, m: &MachineView, a: &ArgList) -> Result<Vec<Value>> {
        let name = a.str("name")?;
        let safe = crate::files::safe_name(name)
            .with_context(|| format!("{name:?} is not a usable file name"))?;
        let dir = crate::util::unique_temp_dir("farlink-mcp-write");
        let local = dir.join(&safe);
        let res = std::fs::write(&local, a.str("content")?)
            .map_err(anyhow::Error::from)
            .and_then(|_| self.send_file(m, local, a.str("folder")?));
        let _ = std::fs::remove_dir_all(&dir);
        let remote = res?;
        let note = if !remote.ends_with(&safe) {
            " (a file with that name was there already)"
        } else {
            ""
        };
        Ok(vec![text(&format!("Wrote {remote}{note}"))])
    }

    fn make_folder(&mut self, m: &MachineView, path: &str) -> Result<Vec<Value>> {
        let h = self.session(m)?;
        let req = h.mkdir_remote(path);
        let (_, error) = wait(h, 30.0, "new folder", |s| {
            s.fs_done.clone().filter(|d| d.0 == req)
        })?;
        match error {
            Some(e) => bail!("{path}: {e}"),
            None => Ok(vec![text(&format!("Made {path}"))]),
        }
    }
}

/// Read requests from stdin and answer on stdout until stdin ends.
pub fn run(args: &crate::util::Args) -> Result<()> {
    let login = Login::find(args)?;
    let mut mcp = Mcp::connect(login, args.has("read-only"))
        .context("Cannot sign in to the Farlink server")?;
    log::info!("farlink MCP server signed in to {}", mcp.api.server);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = mcp.handle_line(&line) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ tools

const INSTRUCTIONS: &str = "Farlink reaches the user's own computers through their Farlink relay. \
Start with list_computers; every other tool takes `computer` (its name, hostname or id). \
To work with a screen: screenshot, then click / type_text / press_keys using pixel coordinates \
of that screenshot, then screenshot again to check. Prefer run_command (administrator only) and \
the file tools over clicking when a task can be done without the screen. These are real computers, \
possibly with someone working on them: ask before anything destructive.";

/// Tools that only look (all that `--read-only` leaves).
const READ_TOOLS: &[&str] = &[
    "list_computers",
    "screenshot",
    "list_files",
    "read_file",
    "download_file",
];

fn tool_list() -> Vec<Value> {
    let computer = json!({ "type": "string", "description": "Computer name, hostname or id (from list_computers)" });
    let screen = json!({ "type": "integer", "minimum": 1, "description": "Screen number, 1 = first (default: the main screen)" });
    let coord = |what: &str| json!({ "type": "number", "description": format!("{what} in pixels of the last screenshot of this screen") });
    let tool = |name: &str,
                title: &str,
                description: &str,
                props: Value,
                required: &[&str],
                read_only: bool| {
        json!({
            "name": name,
            "title": title,
            "description": description,
            "inputSchema": { "type": "object", "properties": props, "required": required },
            "annotations": { "readOnlyHint": read_only, "openWorldHint": true },
        })
    };
    vec![
        tool(
            "list_computers",
            "List computers",
            "The computers on this Farlink relay: name, online or not, operating system, who is signed in, network addresses, screens and load.",
            json!({ "search": { "type": "string", "description": "Only computers whose name, hostname, OS, user or address contains this" } }),
            &[],
            true,
        ),
        tool(
            "screenshot",
            "Take a screenshot",
            "A picture of one screen of a computer, as it is right now. Coordinates for click, move_mouse, drag and scroll are pixels of the last screenshot of that screen.",
            json!({
                "computer": computer, "screen": screen,
                "max_size": { "type": "integer", "minimum": 256, "maximum": 4096, "description": format!("Longest side of the picture (default {SHOT_SIZE})") },
                "wait_until_still": { "type": "boolean", "description": format!("Wait (up to {} s) until the screen stops changing, e.g. after a click opened a window or a page is loading. A blinking cursor or a small spinner does not count as a change", STILL_LIMIT.as_secs()) },
            }),
            &["computer"],
            true,
        ),
        tool(
            "click",
            "Click",
            "Move the mouse to a point and click.",
            json!({
                "computer": computer, "screen": screen, "x": coord("x"), "y": coord("y"),
                "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "default left" },
                "double": { "type": "boolean", "description": "Double-click" },
            }),
            &["computer", "x", "y"],
            false,
        ),
        tool(
            "move_mouse",
            "Move the mouse",
            "Move the mouse pointer (hover) without clicking.",
            json!({ "computer": computer, "screen": screen, "x": coord("x"), "y": coord("y") }),
            &["computer", "x", "y"],
            false,
        ),
        tool(
            "drag",
            "Drag",
            "Press the left button at one point, move to another and let go.",
            json!({
                "computer": computer, "screen": screen,
                "from_x": coord("Start x"), "from_y": coord("Start y"), "to_x": coord("End x"), "to_y": coord("End y"),
            }),
            &["computer", "from_x", "from_y", "to_x", "to_y"],
            false,
        ),
        tool(
            "scroll",
            "Scroll",
            "Turn the mouse wheel, optionally over a point first.",
            json!({
                "computer": computer, "screen": screen, "x": coord("x"), "y": coord("y"),
                "amount": { "type": "number", "description": "Wheel steps: positive scrolls up, negative down" },
            }),
            &["computer", "amount"],
            false,
        ),
        tool(
            "type_text",
            "Type text",
            "Type text where the keyboard focus is (any language; a newline presses Enter).",
            json!({ "computer": computer, "text": { "type": "string" } }),
            &["computer", "text"],
            false,
        ),
        tool(
            "press_keys",
            "Press keys",
            "Press keys or shortcuts, one after another, separated by spaces: \"ctrl+c\", \"alt+tab\", \"enter\", \"ctrl+shift+escape\", \"win+r\", \"f5 tab tab enter\". Ctrl+Alt+Delete works too.",
            json!({ "computer": computer, "keys": { "type": "string" } }),
            &["computer", "keys"],
            false,
        ),
        tool(
            "run_command",
            "Run a shell command",
            "Run a command on the computer (bash on Linux, PowerShell or cmd on Windows; as the helper's user: SYSTEM on Windows) and get its output and exit code. No terminal: interactive full-screen programs don't work. The shell's folder is kept between calls (`cd` changes it). Administrator only.",
            json!({
                "computer": computer,
                "command": { "type": "string" },
                "stdin": { "type": "string", "description": "Text for the command's standard input" },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT, "description": format!("Stop the command after this long (default {DEFAULT_TIMEOUT})") },
                "powershell": { "type": "boolean", "description": "Windows: PowerShell (default) or cmd (false)" },
            }),
            &["computer", "command"],
            false,
        ),
        tool(
            "list_files",
            "List a folder",
            "The files and folders in a folder of the computer. Without a path: the starting places (home folder, drives).",
            json!({ "computer": computer, "path": { "type": "string", "description": "Folder on the computer" } }),
            &["computer"],
            true,
        ),
        tool(
            "read_file",
            "Read a text file",
            "The contents of a text file on the computer (up to 2 MB).",
            json!({ "computer": computer, "path": { "type": "string", "description": "File on the computer" } }),
            &["computer", "path"],
            true,
        ),
        tool(
            "download_file",
            "Download a file",
            "Copy a file from the computer to this computer (where the MCP server runs). Never overwrites: a copy gets \"(2)\" added.",
            json!({
                "computer": computer,
                "path": { "type": "string", "description": "File on the remote computer" },
                "local_folder": { "type": "string", "description": "Folder here (default: Downloads)" },
            }),
            &["computer", "path"],
            true,
        ),
        tool(
            "upload_file",
            "Upload a file",
            "Copy a file from this computer (where the MCP server runs) into a folder of the remote computer. Never overwrites: a copy gets \"(2)\" added.",
            json!({
                "computer": computer,
                "local_path": { "type": "string", "description": "File here" },
                "folder": { "type": "string", "description": "Folder on the remote computer" },
            }),
            &["computer", "local_path", "folder"],
            false,
        ),
        tool(
            "write_file",
            "Write a new text file",
            "Make a new text file in a folder of the computer. Never overwrites: if the name is taken, the new file gets \"(2)\" added (to replace a file, use run_command).",
            json!({
                "computer": computer,
                "folder": { "type": "string", "description": "Folder on the computer" },
                "name": { "type": "string", "description": "File name" },
                "content": { "type": "string" },
            }),
            &["computer", "folder", "name", "content"],
            false,
        ),
        tool(
            "make_folder",
            "Make a folder",
            "Make a new folder on the computer.",
            json!({ "computer": computer, "path": { "type": "string", "description": "The new folder's full path" } }),
            &["computer", "path"],
            false,
        ),
    ]
}

// ---------------------------------------------------------------- helpers

fn text(t: &str) -> Value {
    json!({ "type": "text", "text": t })
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A tool call's arguments.
struct ArgList<'a>(&'a Value);

impl ArgList<'_> {
    fn opt_str(&self, k: &str) -> Option<&str> {
        self.0.get(k).and_then(Value::as_str)
    }
    fn str(&self, k: &str) -> Result<&str> {
        self.opt_str(k).with_context(|| format!("{k} is missing"))
    }
    fn num(&self, k: &str) -> Option<f64> {
        let v = self.0.get(k)?;
        v.as_f64()
            .or_else(|| v.as_str()?.trim().parse().ok())
            .filter(|v| v.is_finite())
    }
    fn opt_bool(&self, k: &str) -> Option<bool> {
        self.0.get(k).and_then(Value::as_bool)
    }
    fn bool(&self, k: &str) -> bool {
        self.opt_bool(k).unwrap_or(false)
    }
}

/// The screen a call names (`screen` counts from 1), else the main one.
fn monitor<'a>(m: &'a MachineView, a: &ArgList) -> Result<&'a MonitorInfo> {
    let mons = &m.machine.monitors;
    match a.num("screen") {
        Some(n) => {
            let n = n as usize;
            mons.iter()
                .find(|mon| mon.index as usize + 1 == n)
                .with_context(|| {
                    format!(
                        "{} has no screen {n} (it has {})",
                        m.machine.name,
                        mons.len()
                    )
                })
        }
        None => mons
            .iter()
            .find(|mon| mon.primary)
            .or(mons.first())
            .with_context(|| format!("{} has no screens", m.machine.name)),
    }
}

/// Wait until `f` gives something (or the session closes, or time runs out).
fn wait<T>(
    h: &SessionHandle,
    secs: f32,
    what: &str,
    mut f: impl FnMut(&mut SessionShared) -> Option<T>,
) -> Result<T> {
    let end = Instant::now() + Duration::from_secs_f32(secs);
    loop {
        {
            let mut s = h.shared.lock().unwrap();
            if let Some(v) = f(&mut s) {
                return Ok(v);
            }
            if let ConnState::Closed(r) = &s.state {
                bail!("{what}: the connection closed ({r})");
            }
        }
        if Instant::now() >= end {
            bail!("{what}: no answer within {secs} s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wait for a transfer to end while it keeps moving; its local path.
fn wait_transfer(h: &SessionHandle, req: u32) -> Result<PathBuf> {
    let mut last = (0u64, Instant::now());
    loop {
        let t = {
            let s = h.shared.lock().unwrap();
            if let ConnState::Closed(r) = &s.state {
                bail!("the connection closed ({r})");
            }
            s.transfers
                .iter()
                .find(|t| t.req == req)
                .cloned()
                .context("transfer lost")?
        };
        match t.state {
            TransferState::Done => return Ok(t.local),
            TransferState::Failed(e) => bail!("{}: {e}", t.name),
            TransferState::Running => {}
        }
        if t.done != last.0 {
            last = (t.done, Instant::now());
        } else if last.1.elapsed() > STALL {
            bail!("{}: nothing moved for {} s", t.name, STALL.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The computer a name, hostname or id stands for. Exact matches first,
/// then a unique partial one.
pub fn find_machine<'a>(machines: &'a [MachineView], query: &str) -> Result<&'a MachineView> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        bail!("computer is missing");
    }
    let exact: Vec<_> = machines
        .iter()
        .filter(|m| {
            m.machine.id.to_lowercase() == q
                || m.machine.name.to_lowercase() == q
                || m.machine.hostname.to_lowercase() == q
        })
        .collect();
    let found = if exact.is_empty() {
        machines
            .iter()
            .filter(|m| {
                m.machine.name.to_lowercase().contains(&q)
                    || m.machine.hostname.to_lowercase().contains(&q)
            })
            .collect()
    } else {
        exact
    };
    match found.as_slice() {
        [one] => Ok(one),
        [] => Err(anyhow!("No computer called {query:?} (see list_computers)")),
        many => {
            // The same name twice: the online one is meant, if only one is.
            let online: Vec<_> = many.iter().filter(|m| m.online).collect();
            if let [one] = online.as_slice() {
                return Ok(one);
            }
            let names: Vec<String> = many
                .iter()
                .map(|m| format!("{} (id {})", m.machine.name, m.machine.id))
                .collect();
            Err(anyhow!(
                "{query:?} could be any of: {}; use the id",
                names.join(", ")
            ))
        }
    }
}

/// The computers as text for the model, online ones first.
pub fn list_computers(ov: &Overview, search: Option<&str>) -> String {
    let q = search
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let mut list: Vec<&MachineView> = ov
        .machines
        .iter()
        .filter(|m| {
            let Some(q) = &q else { return true };
            let user = m
                .stats
                .as_ref()
                .and_then(|s| s.user.clone())
                .unwrap_or_default();
            let addrs: Vec<&str> = m
                .stats
                .as_ref()
                .map(|s| s.addrs().collect())
                .unwrap_or_default();
            [&m.machine.name, &m.machine.hostname, &m.machine.os, &user]
                .iter()
                .any(|f| f.to_lowercase().contains(q))
                || addrs.iter().any(|a| a.contains(q.as_str()))
        })
        .collect();
    list.sort_by_key(|m| (!m.online, m.machine.name.to_lowercase()));
    if list.is_empty() {
        return if ov.machines.is_empty() {
            "No computers yet: install the Farlink helper with the install link from the web admin."
                .into()
        } else {
            "No computer matches.".into()
        };
    }
    let mut out = String::new();
    for m in list {
        let mc = &m.machine;
        let state = if m.online {
            "online".to_string()
        } else {
            format!(
                "offline, last seen {} ago",
                crate::util::human_duration(ov.now.saturating_sub(mc.last_seen))
            )
        };
        out.push_str(&format!("- {} — {state}\n  id: {}", mc.name, mc.id));
        if mc.hostname != mc.name {
            out.push_str(&format!(" · hostname: {}", mc.hostname));
        }
        out.push_str(&format!("\n  system: {} · helper {}\n", mc.os, mc.version));
        let screens: Vec<String> = mc
            .monitors
            .iter()
            .map(|s| {
                format!(
                    "{}: {}×{}{}",
                    s.index + 1,
                    s.width,
                    s.height,
                    if s.primary { " (main)" } else { "" }
                )
            })
            .collect();
        if !screens.is_empty() {
            out.push_str(&format!("  screens: {}\n", screens.join(", ")));
        }
        if let Some(s) = &m.stats {
            if let Some(u) = crate::sysstat::user_line(s) {
                out.push_str(&format!("  signed in: {u}\n"));
            }
            let addrs: Vec<&str> = s.addrs().collect();
            if !addrs.is_empty() {
                out.push_str(&format!("  addresses: {}\n", addrs.join(", ")));
            }
            let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
            out.push_str(&format!(
                "  load: CPU {:.0}% of {} cores · memory {:.1} of {:.1} GB\n",
                s.cpu,
                s.cores,
                gb(s.mem_used),
                gb(s.mem_total)
            ));
        }
        if m.sessions > 0 {
            out.push_str(&format!("  open connections: {}\n", m.sessions));
        }
        if let Some(o) = &m.owner_name {
            out.push_str(&format!("  belongs to: {o}\n"));
        }
        let notes = mc.notes.trim();
        if !notes.is_empty() {
            out.push_str(&format!("  notes: {}\n", notes.replace('\n', " / ")));
        }
    }
    out
}

/// A folder listing as text.
fn format_listing(d: &crate::net::RemoteDir) -> String {
    let mut out = if d.path.is_empty() {
        "Starting places:\n".to_string()
    } else {
        format!("Folder: {}\n", d.path)
    };
    if let Some(p) = &d.parent {
        out.push_str(&format!("Parent: {p}\n"));
    }
    if d.entries.is_empty() {
        out.push_str("(empty)\n");
    }
    for e in &d.entries {
        if e.is_dir {
            out.push_str(&format!("[dir]  {}  ({})\n", e.name, e.path));
        } else {
            out.push_str(&format!(
                "{:>12}  {}  {}\n",
                e.size,
                date(e.modified),
                e.name
            ));
        }
    }
    out
}

/// "2026-11-28 14:05" (UTC) for seconds since 1970.
pub fn date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// A remote path's folder and name (either kind of slash).
pub fn split_path(path: &str) -> Option<(String, String)> {
    let path = path.trim();
    let cut = path.rfind(['/', '\\'])?;
    let name = &path[cut + 1..];
    if name.is_empty() {
        return None;
    }
    let mut parent = path[..cut].to_string();
    // "/file" → "/", "C:\file" → "C:\".
    if parent.is_empty() || (parent.len() == 2 && parent.ends_with(':')) {
        parent.push_str(&path[cut..cut + 1]);
    }
    Some((parent, name.to_string()))
}

/// How a `wait_until_still` screenshot went (time since the first look).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Still {
    Stopped(Duration),
    KeptChanging(Duration),
}

/// Look at the screen (`take`) every `gap` until two pictures in a row are
/// [`nearly_same`], at most `limit` long; the last picture and how it went.
pub fn wait_until_still(
    mut take: impl FnMut() -> Result<crate::codec::Frame>,
    gap: Duration,
    limit: Duration,
) -> Result<(crate::codec::Frame, Still)> {
    let start = Instant::now();
    let mut prev = take()?;
    loop {
        if start.elapsed() + gap > limit {
            return Ok((prev, Still::KeptChanging(start.elapsed())));
        }
        std::thread::sleep(gap);
        let cur = take()?;
        if nearly_same(&prev, &cur) {
            return Ok((cur, Still::Stopped(start.elapsed())));
        }
        prev = cur;
    }
}

/// Same size and at most 0.01 % of the pixels different (at least 64):
/// a blinking text cursor or a small spinner is not "changing".
pub fn nearly_same(a: &crate::codec::Frame, b: &crate::codec::Frame) -> bool {
    if (a.width, a.height) != (b.width, b.height) {
        return false;
    }
    let allowed = (a.width as usize * a.height as usize / 10_000).max(64);
    let mut differ = 0;
    for (pa, pb) in a.rgba.chunks_exact(4).zip(b.rgba.chunks_exact(4)) {
        if pa[..3] != pb[..3] {
            differ += 1;
            if differ > allowed {
                return false;
            }
        }
    }
    true
}

/// The size a screenshot of a `w`×`h` screen gets with longest side `max`
/// (the same rounding as [`crate::codec::thumbnail_jpeg`]).
pub fn scaled_size(w: u32, h: u32, max: u32) -> (u32, u32) {
    let k = (max as f32 / w.max(h).max(1) as f32).min(1.0);
    (
        ((w as f32 * k).round() as u32).max(1),
        ((h as f32 * k).round() as u32).max(1),
    )
}

/// Picture pixels → the 0..1 position inside the screen the protocol uses.
/// The middle of the pixel; outside the picture means its edge.
pub fn to_fraction(x: f64, y: f64, (w, h): (u32, u32)) -> (f32, f32) {
    let f = |v: f64, size: u32| (((v + 0.5) / size.max(1) as f64).clamp(0.0, 1.0)) as f32;
    (f(x, w), f(y, h))
}

/// The end of `s`, at most `max` bytes (on a character boundary).
fn keep_end(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = s.len() - max;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    format!("[… {cut} bytes of earlier output left out]\n{}", &s[cut..])
}

/// "ctrl+shift+t" → the protocol's key names, in order.
pub fn key_combo(combo: &str) -> Result<Vec<String>> {
    combo
        .split('+')
        .map(|k| {
            // "ctrl++" = Control and the plus key.
            let k = if k.is_empty() { "+" } else { k };
            key_name(k).with_context(|| format!("Unknown key {k:?} in {combo:?}"))
        })
        .collect::<Result<Vec<_>>>()
        .and_then(|v| {
            if v.is_empty() {
                bail!("no keys")
            } else {
                Ok(v)
            }
        })
}

/// One key as people write it → the protocol's name for it.
fn key_name(k: &str) -> Option<String> {
    let lower = k.trim().to_lowercase();
    let alias = match lower.as_str() {
        "ctrl" | "control" | "ctl" => "Control",
        "shift" => "Shift",
        "alt" | "option" | "opt" | "altgr" => "Alt",
        "win" | "windows" | "super" | "meta" | "cmd" | "command" | "start" => "Meta",
        "enter" | "return" => "Enter",
        "esc" | "escape" => "Escape",
        "tab" => "Tab",
        "space" | "spacebar" => "Space",
        "backspace" | "bksp" => "Backspace",
        "delete" | "del" => "Delete",
        "insert" | "ins" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" | "pgup" => "PageUp",
        "pagedown" | "pgdn" | "pgdown" => "PageDown",
        "up" | "arrowup" => "ArrowUp",
        "down" | "arrowdown" => "ArrowDown",
        "left" | "arrowleft" => "ArrowLeft",
        "right" | "arrowright" => "ArrowRight",
        "capslock" | "caps" => "CapsLock",
        "printscreen" | "prtsc" | "print" => "PrintScreen",
        "menu" | "contextmenu" | "apps" => "ContextMenu",
        "-" | "minus" => "Minus",
        "=" | "equals" | "equal" => "Equals",
        "[" => "OpenBracket",
        "]" => "CloseBracket",
        "\\" => "Backslash",
        ";" => "Semicolon",
        "'" => "Quote",
        "," | "comma" => "Comma",
        "." | "period" | "dot" => "Period",
        "/" | "slash" => "Slash",
        "`" | "backtick" => "Backtick",
        _ => "",
    };
    let name = if !alias.is_empty() {
        alias.to_string()
    } else if lower.len() == 1 {
        lower.to_uppercase()
    } else if let Some(n) = lower
        .strip_prefix('f')
        .filter(|n| n.chars().all(|c| c.is_ascii_digit()))
    {
        format!("F{n}")
    } else {
        crate::keys::all_names()
            .into_iter()
            .find(|n| n.to_lowercase() == lower)?
    };
    crate::keys::keysym(&name).is_some().then_some(name)
}

fn is_ctrl_alt_del(names: &[String]) -> bool {
    let mut v: Vec<&str> = names.iter().map(String::as_str).collect();
    v.sort();
    v == ["Alt", "Control", "Delete"]
}

/// Press keys in order and let go of them the other way round.
fn press(names: &[String]) -> Vec<InputEvent> {
    let down = names.iter().map(|k| InputEvent::Key {
        key: k.clone(),
        down: true,
    });
    let up = names.iter().rev().map(|k| InputEvent::Key {
        key: k.clone(),
        down: false,
    });
    down.chain(up).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{Frame, Rect};
    use crate::server::MachineSummary;

    /// A 1000×500 screen with a `w`×`h` block drawn in the corner.
    fn screen_with_block(w: u32, h: u32) -> Frame {
        let mut f = Frame::new(1000, 500);
        f.fill_rect(Rect { x: 0, y: 0, w, h }, [255, 0, 0, 255]);
        f
    }

    #[test]
    fn a_blinking_cursor_is_not_a_change() {
        let empty = screen_with_block(0, 0);
        // 2×20 cursor = 40 pixels; 0.01 % of 500 000 = 50, at least 64.
        assert!(nearly_same(&empty, &screen_with_block(2, 20)));
        assert!(nearly_same(&empty, &screen_with_block(8, 8)));
        assert!(!nearly_same(&empty, &screen_with_block(9, 8)));
        // A window opening is.
        assert!(!nearly_same(&empty, &screen_with_block(300, 200)));
        // So is another screen size.
        assert!(!nearly_same(&empty, &Frame::new(500, 1000)));
    }

    #[test]
    fn waits_until_two_looks_agree() {
        // A window opening over three looks, then staying.
        let mut looks = [0, 100, 200, 300, 300, 300].into_iter();
        let mut taken = 0;
        let (f, how) = wait_until_still(
            || {
                taken += 1;
                Ok(screen_with_block(looks.next().unwrap(), 100))
            },
            Duration::ZERO,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(matches!(how, Still::Stopped(_)));
        assert_eq!(taken, 5);
        assert_eq!(f.rgba, screen_with_block(300, 100).rgba);
    }

    #[test]
    fn gives_up_on_a_screen_that_keeps_changing() {
        let mut n = 0;
        let (f, how) = wait_until_still(
            || {
                n += 50;
                Ok(screen_with_block(n % 1000, 100))
            },
            Duration::from_millis(20),
            Duration::from_millis(200),
        )
        .unwrap();
        assert!(matches!(how, Still::KeptChanging(t) if t < Duration::from_millis(400)));
        // The latest picture comes back.
        assert_eq!(f.rgba, screen_with_block(n % 1000, 100).rgba);
        // A failed look ends it.
        let err = wait_until_still(
            || bail!("offline"),
            Duration::ZERO,
            Duration::from_secs(1),
        );
        assert!(err.is_err());
    }

    fn mcp() -> Mcp {
        Mcp::new(
            Api {
                server: "http://127.0.0.1:9".into(),
                token: "t".into(),
            },
            true,
            false,
        )
    }

    fn machine(id: &str, name: &str, host: &str, online: bool) -> MachineView {
        MachineView {
            machine: MachineSummary {
                id: id.into(),
                name: name.into(),
                hostname: host.into(),
                os: "Ubuntu 24.04".into(),
                version: "0.1.24".into(),
                monitors: crate::desktop::TestPattern::dual().monitors,
                last_seen: 100,
                owner: None,
                notes: String::new(),
            },
            online,
            sessions: 0,
            up_to_date: true,
            owner_name: None,
            updating: false,
            update_error: None,
            stats: None,
            load: Vec::new(),
        }
    }

    fn reply(m: &mut Mcp, req: Value) -> Value {
        serde_json::from_str(&m.handle_line(&req.to_string()).expect("an answer")).unwrap()
    }

    #[test]
    fn initialize_ping_and_unknown_methods() {
        let mut m = mcp();
        let r = reply(
            &mut m,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
        );
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["result"]["serverInfo"]["name"], "farlink");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        let r = reply(
            &mut m,
            json!({"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":"1999-01-01"}}),
        );
        assert_eq!(
            r["result"]["protocolVersion"],
            *PROTOCOLS.last().unwrap(),
            "unknown version: the newest"
        );

        assert!(
            m.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );
        assert_eq!(
            reply(&mut m, json!({"jsonrpc":"2.0","id":2,"method":"ping"}))["result"],
            json!({})
        );
        assert_eq!(
            reply(
                &mut m,
                json!({"jsonrpc":"2.0","id":3,"method":"resources/list"})
            )["error"]["code"],
            -32601
        );
        let bad: Value = serde_json::from_str(&m.handle_line("{nope").unwrap()).unwrap();
        assert_eq!(bad["error"]["code"], -32700);
        let batch: Value = serde_json::from_str(
            &m.handle_line(
                r#"[{"jsonrpc":"2.0","id":7,"method":"ping"},{"jsonrpc":"2.0","method":"x"}]"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            batch.as_array().unwrap().len(),
            1,
            "notifications in a batch get no answer"
        );
    }

    #[test]
    fn tools_follow_the_sign_in() {
        let names = |m: &Mcp| {
            m.tools()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let admin = mcp();
        assert!(names(&admin).contains(&"run_command".to_string()));
        assert_eq!(names(&admin).len(), tool_list().len());
        for t in admin.tools() {
            assert_eq!(t["inputSchema"]["type"], "object", "{t}");
            for r in t["inputSchema"]["required"].as_array().unwrap() {
                assert!(
                    t["inputSchema"]["properties"]
                        .get(r.as_str().unwrap())
                        .is_some(),
                    "{t}"
                );
            }
        }
        let mut person = mcp();
        person.admin = false;
        assert!(!names(&person).contains(&"run_command".to_string()));
        let err = person
            .call("run_command", &json!({"computer":"x","command":"ls"}))
            .unwrap_err();
        assert!(err.to_string().contains("administrator"), "{err}");

        let mut look = mcp();
        look.read_only = true;
        assert_eq!(names(&look), READ_TOOLS);
        let err = look
            .call("click", &json!({"computer":"x","x":1,"y":1}))
            .unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
        // A refused call is a tool error, not a protocol error.
        let r = reply(
            &mut look,
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"type_text","arguments":{}}}),
        );
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn computers_are_found_by_name_hostname_or_id() {
        let list = vec![
            machine("m1", "Office PC", "DESKTOP-1", true),
            machine("m2", "Laptop", "anna-laptop", false),
            machine("m3", "Laptop", "old-laptop", true),
            machine("m4", "Office printer box", "pbox", true),
        ];
        assert_eq!(find_machine(&list, "office pc").unwrap().machine.id, "m1");
        assert_eq!(find_machine(&list, "desktop-1").unwrap().machine.id, "m1");
        assert_eq!(find_machine(&list, "M2").unwrap().machine.id, "m2");
        assert_eq!(
            find_machine(&list, "Laptop").unwrap().machine.id,
            "m3",
            "the online one of two"
        );
        assert_eq!(
            find_machine(&list, "printer").unwrap().machine.id,
            "m4",
            "a unique part"
        );
        assert!(
            find_machine(&list, "office")
                .unwrap_err()
                .to_string()
                .contains("m4")
        );
        assert!(find_machine(&list, "nothing").is_err());
        assert!(find_machine(&list, " ").is_err());
    }

    #[test]
    fn computer_list_text() {
        let mut ov: Overview = serde_json::from_value(json!({
            "machines": [], "grants": [], "sessions": [], "install_url": "", "server_version": "x",
            "release_version": null, "now": 7300,
        }))
        .unwrap();
        assert!(list_computers(&ov, None).contains("No computers yet"));
        let mut pc = machine("m1", "Office PC", "DESKTOP-1", true);
        pc.machine.notes = "Reception\nfront desk".into();
        pc.stats = Some(crate::sysstat::SystemStats {
            cpu: 12.0,
            cores: 8,
            mem_total: 16 << 30,
            mem_used: 4 << 30,
            user: Some("olga".into()),
            ifaces: vec![crate::sysstat::NetIface {
                name: "eth0".into(),
                addrs: vec!["192.168.1.20".into()],
            }],
            ..Default::default()
        });
        ov.machines = vec![machine("m2", "Attic", "attic", false), pc];
        let t = list_computers(&ov, None);
        assert!(
            t.find("Office PC").unwrap() < t.find("Attic").unwrap(),
            "online first: {t}"
        );
        for want in [
            "id: m1",
            "hostname: DESKTOP-1",
            "signed in: olga",
            "192.168.1.20",
            "1: 1600×900 (main), 2: 1280×1024",
            "notes: Reception / front desk",
            "CPU 12% of 8 cores",
            "last seen 2 h ago",
        ] {
            assert!(t.contains(want), "{want} in {t}");
        }
        let only = list_computers(&ov, Some("192.168"));
        assert!(
            only.contains("Office PC") && !only.contains("Attic"),
            "{only}"
        );
        assert_eq!(list_computers(&ov, Some("zzz")), "No computer matches.");
    }

    #[test]
    fn keys_as_people_write_them() {
        assert_eq!(
            key_combo("ctrl+shift+t").unwrap(),
            ["Control", "Shift", "T"]
        );
        assert_eq!(key_combo("Win+R").unwrap(), ["Meta", "R"]);
        assert_eq!(key_combo("alt+F4").unwrap(), ["Alt", "F4"]);
        assert_eq!(key_combo("Enter").unwrap(), ["Enter"]);
        assert_eq!(key_combo("PageDown").unwrap(), ["PageDown"]);
        assert_eq!(key_combo("ctrl+-").unwrap(), ["Control", "Minus"]);
        assert_eq!(key_combo("cmd+.").unwrap(), ["Meta", "Period"]);
        assert!(key_combo("ctrl+banana").is_err());
        assert!(key_combo("f99").is_err());
        assert!(is_ctrl_alt_del(&key_combo("ctrl+alt+del").unwrap()));
        assert!(!is_ctrl_alt_del(&key_combo("ctrl+shift+esc").unwrap()));
        let ev = press(&key_combo("ctrl+c").unwrap());
        let k = |key: &str, down| InputEvent::Key {
            key: key.into(),
            down,
        };
        assert_eq!(
            ev,
            [
                k("Control", true),
                k("C", true),
                k("C", false),
                k("Control", false)
            ]
        );
    }

    #[test]
    fn input_follows_the_last_screenshot() {
        let m = mcp();
        let pc = machine("m1", "Office PC", "DESKTOP-1", true);
        let a = json!({"x": 639, "y": 0, "button": "right"});
        let ev = m.input_events(&pc, "click", &ArgList(&a)).unwrap();
        // Screen 1 is 1600×900 → screenshots 1280×720 by default.
        let InputEvent::MouseMove { mon: 0, x, y } = ev[0] else {
            panic!("{ev:?}")
        };
        assert!((x - 0.5).abs() < 0.001 && y < 0.001, "{x} {y}");
        assert!(matches!(
            ev[1],
            InputEvent::MouseButton {
                button: MouseButton::Right,
                down: true,
                ..
            }
        ));
        assert_eq!(ev.len(), 3);

        let mut m = m;
        m.shot_size.insert(("m1".into(), 1), (100, 50));
        let a = json!({"screen": 2, "x": 49.5, "y": 200, "double": true});
        let ev = m.input_events(&pc, "click", &ArgList(&a)).unwrap();
        assert_eq!(ev.len(), 5, "move + two clicks");
        let InputEvent::MouseMove { mon: 1, x, y } = ev[0] else {
            panic!("{ev:?}")
        };
        assert!(
            (x - 0.5).abs() < 0.001 && y == 1.0,
            "outside = the edge: {x} {y}"
        );
        assert!(
            m.input_events(
                &pc,
                "click",
                &ArgList(&json!({"screen": 3, "x": 1, "y": 1}))
            )
            .is_err()
        );
        assert!(
            m.input_events(&pc, "click", &ArgList(&json!({"x": 1})))
                .is_err()
        );

        let ev = m
            .input_events(
                &pc,
                "type_text",
                &ArgList(&json!({"text": "ls -l\r\nпривет"})),
            )
            .unwrap();
        assert_eq!(
            ev[0],
            InputEvent::Text {
                text: "ls -l".into()
            }
        );
        assert_eq!(
            ev[1],
            InputEvent::Key {
                key: "Enter".into(),
                down: true
            }
        );
        assert_eq!(
            ev[3],
            InputEvent::Text {
                text: "привет".into()
            }
        );
        let ev = m
            .input_events(
                &pc,
                "press_keys",
                &ArgList(&json!({"keys": "ctrl+alt+delete tab"})),
            )
            .unwrap();
        assert_eq!(ev[0], InputEvent::CtrlAltDel);
        assert_eq!(ev.len(), 3);
        let ev = m
            .input_events(&pc, "scroll", &ArgList(&json!({"amount": -3})))
            .unwrap();
        assert_eq!(ev, [InputEvent::Scroll { dx: 0.0, dy: -3.0 }]);
        let ev = m
            .input_events(
                &pc,
                "drag",
                &ArgList(&json!({"from_x": 0, "from_y": 0, "to_x": 1279, "to_y": 719})),
            )
            .unwrap();
        assert!(
            matches!(ev.last(), Some(InputEvent::MouseButton { down: false, x, y, .. }) if *x > 0.99 && *y > 0.99),
            "{ev:?}"
        );
    }

    #[test]
    fn small_helpers() {
        assert_eq!(scaled_size(1920, 1080, 1280), (1280, 720));
        assert_eq!(scaled_size(800, 600, 1280), (800, 600), "never bigger");
        assert_eq!(scaled_size(1080, 1920, 1280), (720, 1280));
        assert_eq!(to_fraction(-5.0, 10_000.0, (100, 100)), (0.0, 1.0));
        assert_eq!(date(0), "1970-01-01 00:00");
        assert_eq!(date(1_795_865_100), "2026-11-28 11:25");
        assert_eq!(date(951_782_400), "2000-02-29 00:00");
        assert_eq!(
            split_path("/home/a/notes.txt"),
            Some(("/home/a".into(), "notes.txt".into()))
        );
        assert_eq!(
            split_path("/notes.txt"),
            Some(("/".into(), "notes.txt".into()))
        );
        assert_eq!(
            split_path(r"C:\Users\a\x.log"),
            Some((r"C:\Users\a".into(), "x.log".into()))
        );
        assert_eq!(
            split_path(r"C:\x.log"),
            Some((r"C:\".into(), "x.log".into()))
        );
        assert_eq!(split_path("notes.txt"), None);
        assert_eq!(split_path("/home/"), None);
        assert_eq!(keep_end("hello", 10), "hello");
        let cut = keep_end("ааааа", 4);
        assert!(cut.ends_with("аа") && cut.contains("6 bytes"), "{cut}");
    }
}
