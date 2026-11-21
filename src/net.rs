//! Desktop viewer networking: the server API and live sessions.

use crate::codec::Canvas;
use crate::codec::Frame;
use crate::proto::{FileChunk, FsEntry, InputEvent, MonitorInfo, PREVIEW_MAX, Preview, PreviewShot, SessionMsg, TileHeader};
use crate::quality::Quality;
use crate::server::Overview;
use crate::util::{url_encode, ws_url};
use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// One shared async runtime for all network work of the desktop app.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("farlink-net")
            .build()
            .expect("tokio runtime")
    })
}

fn api_error(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::StatusCode(401) => anyhow!("Wrong password or signed out"),
        ureq::Error::StatusCode(404) => anyhow!("Not found (link expired or withdrawn?)"),
        ureq::Error::StatusCode(c) => anyhow!("Server answered with error {c}"),
        other => anyhow!("Cannot reach the server: {other}"),
    }
}

/// HTTP calls to the admin API (blocking — call from a background thread).
#[derive(Clone, Debug)]
pub struct Api {
    pub server: String,
    pub token: String,
}

impl Api {
    /// Sign in. `username` empty = administrator; otherwise a person.
    pub fn login(server: &str, username: &str, password: &str) -> Result<String> {
        let url = format!("{}/api/login", server.trim_end_matches('/'));
        let mut r = ureq::post(&url).send_json(json!({ "username": username, "password": password })).map_err(api_error)?;
        let v: serde_json::Value = r.body_mut().read_json()?;
        v["token"].as_str().map(str::to_string).context("unexpected server answer")
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.server.trim_end_matches('/'), path)
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    pub fn overview(&self) -> Result<Overview> {
        let mut r = ureq::get(&self.url("/api/overview")).header("Authorization", &self.auth()).call().map_err(api_error)?;
        Ok(r.body_mut().read_json()?)
    }

    /// Small pictures of a computer's screens (taken by its helper right now).
    pub fn preview(&self, machine_id: &str) -> Result<Preview> {
        let mut r = ureq::get(&self.url(&format!("/api/machines/{}/preview", url_encode(machine_id))))
            .header("Authorization", &self.auth())
            .call()
            .map_err(|e| match e {
                ureq::Error::StatusCode(503) => anyhow!("This computer is offline right now"),
                ureq::Error::StatusCode(504) => anyhow!("No preview from this computer (its helper may be too old)"),
                e => api_error(e),
            })?;
        Ok(r.body_mut().read_json()?)
    }

    /// One screen at full size, taken by the computer's helper right now.
    /// Older helpers answer with the small preview pictures instead.
    pub fn screenshot(&self, machine_id: &str, index: u8) -> Result<Frame> {
        let mut r = ureq::get(&self.url(&format!("/api/machines/{}/screenshot/{index}", url_encode(machine_id))))
            .header("Authorization", &self.auth())
            .config()
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build()
            .call()
            .map_err(|e| match e {
                ureq::Error::StatusCode(503) => anyhow!("This computer is offline right now"),
                ureq::Error::StatusCode(504) => anyhow!("The computer did not send the screenshot"),
                e => api_error(e),
            })?;
        let p: Preview = r.body_mut().with_config().limit(64 << 20).read_json()?;
        decode_preview(&p).into_iter().find(|(i, _)| *i == index).map(|(_, f)| f).ok_or_else(|| anyhow!("The computer sent no picture of this screen"))
    }

    pub fn create_grant(&self, machine_id: &str, minutes: u64, view_only: bool, label: &str) -> Result<String> {
        let mut r = ureq::post(&self.url("/api/grants"))
            .header("Authorization", &self.auth())
            .send_json(json!({ "machine_id": machine_id, "minutes": minutes, "view_only": view_only, "label": label }))
            .map_err(api_error)?;
        let v: serde_json::Value = r.body_mut().read_json()?;
        v["url"].as_str().map(str::to_string).context("unexpected server answer")
    }

    /// Give a computer a new display name (administrator only).
    pub fn rename(&self, machine_id: &str, name: &str) -> Result<()> {
        ureq::post(&self.url(&format!("/api/machines/{machine_id}/rename")))
            .header("Authorization", &self.auth())
            .send_json(json!({ "name": name }))
            .map_err(api_error)?;
        Ok(())
    }

    /// Replace the administrator's notes about a computer.
    pub fn set_notes(&self, machine_id: &str, notes: &str) -> Result<()> {
        ureq::post(&self.url(&format!("/api/machines/{}/notes", url_encode(machine_id))))
            .header("Authorization", &self.auth())
            .send_json(json!({ "notes": notes }))
            .map_err(api_error)?;
        Ok(())
    }

    /// Make a computer's helper install the published version now
    /// (administrator only). Returns that version.
    pub fn update_machine(&self, machine_id: &str) -> Result<String> {
        let v = self.post_reading_error(&format!("/api/machines/{}/update", url_encode(machine_id)))?;
        v["version"].as_str().map(str::to_string).context("unexpected server answer")
    }

    /// Update every online helper older than the published version. Returns how many.
    pub fn update_all(&self) -> Result<u64> {
        let v = self.post_reading_error("/api/machines/update-all")?;
        v["updating"].as_u64().context("unexpected server answer")
    }

    /// POST without a body; a refusal shows the server's own explanation.
    fn post_reading_error(&self, path: &str) -> Result<serde_json::Value> {
        let mut r = ureq::post(&self.url(path))
            .config()
            .http_status_as_error(false)
            .build()
            .header("Authorization", &self.auth())
            .send_empty()
            .map_err(api_error)?;
        let status = r.status().as_u16();
        let v: serde_json::Value = r.body_mut().read_json().unwrap_or_default();
        match status {
            200..=299 => Ok(v),
            _ => match v["error"].as_str() {
                Some(e) => Err(anyhow!("{e}")),
                None => Err(api_error(ureq::Error::StatusCode(status))),
            },
        }
    }

    pub fn revoke(&self, token: &str) -> Result<()> {
        ureq::delete(&self.url(&format!("/api/grants/{token}"))).header("Authorization", &self.auth()).call().map_err(api_error)?;
        Ok(())
    }

    pub fn logout(&self) {
        let _ = ureq::post(&self.url("/api/logout")).header("Authorization", &self.auth()).send_empty();
    }
}

/// Decode the pictures of a [`Preview`] (broken ones are left out).
pub fn decode_preview(p: &Preview) -> Vec<(u8, Frame)> {
    p.shots
        .iter()
        .filter_map(|s| {
            let data = crate::util::base64_decode(&s.jpeg)?;
            let f = crate::codec::decode_image(&data).ok()?;
            Some((s.index, f))
        })
        .collect()
}

/// Preview of a sample computer (demo mode): the fake desktop of each monitor.
pub fn demo_preview(monitors: &[MonitorInfo], tick: u32) -> Preview {
    let shots = monitors
        .iter()
        .filter_map(|m| {
            let f = crate::desktop::TestPattern::render(m, tick, Some((m.width as i32 / 2, m.height as i32 / 2)));
            let (width, height, jpeg) = crate::codec::thumbnail_jpeg(&f, PREVIEW_MAX, 70).ok()?;
            Some(PreviewShot { index: m.index, width, height, jpeg: crate::util::base64_encode(&jpeg) })
        })
        .collect();
    Preview { taken: crate::util::now_secs(), shots }
}

/// A temporary access link: `https://server/g/<token>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestLink {
    pub server: String,
    pub token: String,
}

pub fn parse_link(link: &str) -> Option<GuestLink> {
    let s = link.trim().trim_end_matches('/');
    let (server, token) = s.rsplit_once("/g/")?;
    if !(server.starts_with("http://") || server.starts_with("https://")) || server.len() < 10 {
        return None;
    }
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(GuestLink { server: server.to_string(), token: token.to_string() })
}

/// Where a session connects.
#[derive(Debug, Clone)]
pub enum Target {
    Admin { server: String, token: String, machine_id: String, view_only: bool },
    Guest(GuestLink),
    /// In-process pretend computer (no network).
    Demo,
}

impl Target {
    pub fn url(&self) -> Option<String> {
        match self {
            Target::Admin { server, token, machine_id, view_only } => Some(ws_url(
                server,
                &format!("/ws/view/{}?token={}&view_only={}", url_encode(machine_id), url_encode(token), if *view_only { 1 } else { 0 }),
            )),
            Target::Guest(g) => Some(ws_url(&g.server, &format!("/ws/guest/{}", g.token))),
            Target::Demo => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnState {
    Connecting,
    Live,
    Closed(String),
}

/// Everything the UI shows about a session, updated by the network task.
pub struct SessionShared {
    pub state: ConnState,
    pub hostname: String,
    pub os: String,
    pub monitors: Vec<MonitorInfo>,
    pub view_only: bool,
    /// A view-only temporary link: the mode can't be switched to control.
    pub view_locked: bool,
    pub canvases: HashMap<u8, Canvas>,
    pub notice: Option<String>,
    pub fps: f32,
    pub kbps: f32,
    /// How the pictures get here (shown with the latency).
    pub route: Route,
    /// Where the time goes from screen to screen (see [`Latency`]).
    pub latency: Latency,
    /// Decode / draw times of the current one-second window (sum ms, count).
    window_decode: (f32, u32),
    window_draw: (f32, u32),
    /// Monitors with a complete picture not drawn yet, and when it arrived.
    undrawn: HashMap<u8, Instant>,
    window_start: Instant,
    window_bytes: usize,
    window_frames: usize,
    /// Folder shown in the remote pane of the file browser.
    pub remote_dir: RemoteDir,
    /// Uploads and downloads, newest last.
    pub transfers: Vec<Transfer>,
    /// The remote folder changed (upload finished): list it again.
    pub remote_changed: bool,
    /// Text copied on the remote computer, waiting to be put on this
    /// computer's clipboard.
    pub clipboard_in: Option<String>,
    /// What the remote clipboard holds as far as we know (sent or received),
    /// so the same text is not sent again.
    pub remote_clipboard: Option<String>,
    /// Pointer shapes from the host, by id. Empty = the host draws the
    /// pointer into the pictures (older helper).
    pub pointer_shapes: HashMap<u32, crate::desktop::PointerImage>,
    /// Where the remote pointer is (virtual-desktop pixels) and its shape id.
    pub pointer: Option<(i32, i32, u32)>,
    next_req: u32,
    /// Download request -> (local folder, open file once it started).
    downloads: HashMap<u32, (PathBuf, Option<std::fs::File>)>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RemoteDir {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<FsEntry>,
    pub error: Option<String>,
    pub loading: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TransferState {
    Running,
    Done,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transfer {
    pub req: u32,
    pub name: String,
    pub upload: bool,
    pub done: u64,
    pub total: u64,
    pub state: TransferState,
    /// Where it is (or will be) on this computer.
    pub local: PathBuf,
}

impl SessionShared {
    fn take_req(&mut self) -> u32 {
        self.next_req += 1;
        self.next_req
    }

    fn transfer(&mut self, req: u32) -> Option<&mut Transfer> {
        self.transfers.iter_mut().find(|t| t.req == req)
    }

    fn file_message(&mut self, m: SessionMsg) {
        match m {
            SessionMsg::FsListing { path, parent, entries, error, .. } => {
                self.remote_dir = RemoteDir { path, parent, entries, error, loading: false };
            }
            SessionMsg::FsBegin { req, name, size } => {
                let Some((dir, file)) = self.downloads.get_mut(&req) else { return };
                let name = crate::files::safe_name(&name).unwrap_or_else(|| "download".into());
                let path = crate::files::unique_path(dir, &name);
                let opened = std::fs::File::create(&path);
                let err = opened.as_ref().err().map(|e| e.to_string());
                *file = opened.ok();
                if let Some(t) = self.transfer(req) {
                    t.total = size;
                    t.local = path;
                    if let Some(e) = err {
                        t.state = TransferState::Failed(e);
                    }
                }
            }
            SessionMsg::FsDone { req, error, .. } => {
                self.downloads.remove(&req);
                if let Some(t) = self.transfer(req) {
                    t.state = match error {
                        Some(e) => TransferState::Failed(e),
                        None => TransferState::Done,
                    };
                    if t.upload {
                        self.remote_changed = true;
                    }
                } else {
                    self.remote_changed = true; // new folder
                }
            }
            _ => {}
        }
    }

    fn file_chunk(&mut self, c: FileChunk, data: &[u8]) {
        use std::io::Write;
        let Some((_, Some(file))) = self.downloads.get_mut(&c.req) else { return };
        let res = file.write_all(data);
        let last = c.last;
        if let Some(t) = self.transfer(c.req) {
            t.done += data.len() as u64;
            if let Err(e) = res {
                t.state = TransferState::Failed(e.to_string());
            } else if last {
                t.state = TransferState::Done;
            }
        }
        if last {
            self.downloads.remove(&c.req);
        }
    }

    fn new(view_only: bool) -> SessionShared {
        SessionShared {
            remote_dir: RemoteDir::default(),
            transfers: Vec::new(),
            remote_changed: false,
            clipboard_in: None,
            remote_clipboard: None,
            pointer_shapes: HashMap::new(),
            pointer: None,
            next_req: 0,
            downloads: HashMap::new(),
            state: ConnState::Connecting,
            hostname: String::new(),
            os: String::new(),
            monitors: Vec::new(),
            view_only,
            view_locked: false,
            canvases: HashMap::new(),
            notice: None,
            fps: 0.0,
            kbps: 0.0,
            route: Route::Demo,
            latency: Latency::default(),
            window_decode: (0.0, 0),
            window_draw: (0.0, 0),
            undrawn: HashMap::new(),
            window_start: Instant::now(),
            window_bytes: 0,
            window_frames: 0,
        }
    }

    /// Apply one incoming message. Returns a reply to send (ack / refresh).
    pub fn apply(&mut self, msg: &Message, quality: &Quality) -> Vec<SessionMsg> {
        self.apply_decoded(msg, None, quality)
    }

    /// Like [`apply`](Self::apply), with the picture part already decoded by
    /// [`decode_picture`] (done before taking the lock, so the UI never waits
    /// for decoding).
    pub fn apply_decoded(&mut self, msg: &Message, decoded: Option<crate::codec::Frame>, quality: &Quality) -> Vec<SessionMsg> {
        let mut replies = Vec::new();
        match msg {
            Message::Text(t) => match serde_json::from_str::<SessionMsg>(t) {
                Ok(SessionMsg::Hello { hostname, os, monitors, view_only, .. }) => {
                    self.hostname = hostname;
                    self.os = os;
                    self.monitors = monitors;
                    self.view_only |= view_only;
                    self.state = ConnState::Live;
                    replies.push(SessionMsg::Quality { quality: *quality });
                    // Draw the pointer here: it then moves without waiting for the network.
                    replies.push(SessionMsg::PointerMode { local: true });
                }
                Ok(SessionMsg::Monitors { monitors }) => {
                    self.monitors = monitors;
                    self.canvases.clear();
                    replies.push(SessionMsg::Refresh);
                }
                Ok(SessionMsg::Notice { text }) => self.notice = Some(text),
                Ok(SessionMsg::Closed { reason }) => self.state = ConnState::Closed(reason),
                Ok(SessionMsg::ViewMode { view_only, locked }) => {
                    self.view_only = view_only;
                    self.view_locked = locked;
                }
                Ok(SessionMsg::Clipboard { text }) => {
                    self.remote_clipboard = Some(text.clone());
                    self.clipboard_in = Some(text);
                }
                Ok(SessionMsg::PointerShape { id, width, height, hot_x, hot_y, rgba }) => {
                    if let Some(rgba) = crate::util::base64_decode(&rgba)
                        && (1..=256).contains(&width)
                        && (1..=256).contains(&height)
                        && rgba.len() == (width * height * 4) as usize
                        && id != 0
                    {
                        let img = crate::desktop::PointerImage { width, height, hot_x: hot_x.min(width - 1), hot_y: hot_y.min(height - 1), rgba };
                        self.pointer_shapes.insert(id, img);
                    }
                }
                Ok(SessionMsg::Pointer { x, y, shape }) => self.pointer = Some((x, y, shape)),
                Ok(SessionMsg::Timing { capture_ms, encode_ms, rtt_ms }) => {
                    let clean = |v: f32| if v.is_finite() { v.clamp(0.0, 60_000.0) } else { 0.0 };
                    self.latency.host = Some(HostTiming { capture_ms: clean(capture_ms), encode_ms: clean(encode_ms), rtt_ms: rtt_ms.map(clean) });
                }
                Ok(m @ (SessionMsg::FsListing { .. } | SessionMsg::FsBegin { .. } | SessionMsg::FsDone { .. })) => self.file_message(m),
                _ => {}
            },
            Message::Binary(b) => {
                self.window_bytes += b.len();
                if let Some((c, data)) = FileChunk::decode(b) {
                    self.file_chunk(c, data);
                } else if let Some((h, data)) = TileHeader::decode(b) {
                    match decoded.map(Ok).unwrap_or_else(|| crate::codec::decode_tile(h.format, data)) {
                        Ok(tile) => self.canvases.entry(h.monitor).or_default().apply_tile(&h, &tile),
                        Err(e) => log::debug!("bad tile: {e}"),
                    }
                    if h.is_last() {
                        self.window_frames += 1; // acked on arrival: see `ack_for`
                        self.undrawn.entry(h.monitor).or_insert_with(Instant::now);
                    }
                }
            }
            _ => {}
        }
        let secs = self.window_start.elapsed().as_secs_f32();
        if secs >= 1.0 {
            let mons = self.monitors.len().max(1) as f32;
            self.fps = self.window_frames as f32 / secs / mons;
            self.kbps = self.window_bytes as f32 / 1024.0 / secs;
            let avg = |(sum, n): (f32, u32), old: Option<f32>| if n > 0 { Some(sum / n as f32) } else { old };
            self.latency.decode_ms = avg(std::mem::take(&mut self.window_decode), self.latency.decode_ms);
            self.latency.draw_ms = avg(std::mem::take(&mut self.window_draw), self.latency.draw_ms);
            self.window_start = Instant::now();
            self.window_bytes = 0;
            self.window_frames = 0;
        }
        replies
    }
}

impl SessionShared {
    /// A picture part took `d` to decode.
    pub fn note_decode(&mut self, d: std::time::Duration) {
        self.window_decode.0 += d.as_secs_f32() * 1000.0;
        self.window_decode.1 += 1;
    }

    /// The viewer just put monitor `mon`'s picture on screen.
    pub fn drawn(&mut self, mon: u8) {
        if let Some(t) = self.undrawn.remove(&mon) {
            self.window_draw.0 += t.elapsed().as_secs_f32() * 1000.0;
            self.window_draw.1 += 1;
        }
    }
}

/// Decode `msg` (timed, outside the lock) and apply it.
fn receive(shared: &Mutex<SessionShared>, msg: &Message, quality: &Quality) -> Vec<SessionMsg> {
    let t = Instant::now();
    let decoded = decode_picture(msg);
    let took = t.elapsed();
    let mut sh = shared.lock().unwrap();
    if decoded.is_some() {
        sh.note_decode(took);
    }
    sh.apply_decoded(msg, decoded, quality)
}

/// How pictures reach this viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Through the Farlink server (the only way today: nothing goes
    /// straight between the computers). `encrypted` = wss (HTTPS).
    Relay { server: String, encrypted: bool },
    /// The pretend computer inside this program.
    Demo,
}

impl Route {
    pub fn for_url(url: Option<&str>) -> Route {
        let Some(url) = url else { return Route::Demo };
        let encrypted = url.starts_with("wss://");
        let rest = url.split_once("://").map_or(url, |(_, r)| r);
        let server = rest.split(['/', '?']).next().unwrap_or(rest).to_string();
        Route::Relay { server, encrypted }
    }

    /// Short text for the screen window's bar.
    pub fn short(&self) -> &'static str {
        match self {
            Route::Relay { .. } => "via relay",
            Route::Demo => "local demo",
        }
    }

    /// One line for the hover text.
    pub fn describe(&self) -> String {
        match self {
            Route::Relay { server, encrypted } => format!(
                "Connection: all traffic goes through the relay {server} ({}), no direct peer-to-peer path",
                if *encrypted { "encrypted, HTTPS" } else { "NOT encrypted, plain HTTP" }
            ),
            Route::Demo => "Connection: pretend computer inside this program (no network)".into(),
        }
    }
}

/// What the host says about its share of the time (see `SessionMsg::Timing`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HostTiming {
    pub capture_ms: f32,
    pub encode_ms: f32,
    pub rtt_ms: Option<f32>,
}

/// Where the time goes from the remote screen changing to this screen
/// showing it, in milliseconds (one-second averages).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Latency {
    /// From the host; `None` = an older helper that doesn't tell.
    pub host: Option<HostTiming>,
    pub decode_ms: Option<f32>,
    /// Complete picture arrived → put on screen here.
    pub draw_ms: Option<f32>,
}

impl Latency {
    /// Roughly what a click costs until its result is seen: the round trip
    /// stands for the input going there plus the picture coming back.
    /// `None` until the host has sent a round trip.
    pub fn total_ms(&self) -> Option<f32> {
        let h = self.host?;
        Some(h.capture_ms + h.encode_ms + h.rtt_ms? + self.decode_ms.unwrap_or(0.0) + self.draw_ms.unwrap_or(0.0))
    }

    /// The breakdown, one part per line.
    pub fn describe(&self) -> String {
        let ms = |v: Option<f32>| v.map_or("–".to_string(), |v| format!("{v:.0} ms"));
        let Some(h) = self.host else {
            return format!(
                "The remote helper doesn't report its times (older version)\nDecode here: {}\nOn screen here: {}",
                ms(self.decode_ms),
                ms(self.draw_ms)
            );
        };
        format!(
            "≈ {} from a click to seeing its result\n\nRead screen (remote): {}\nCompress (remote): {}\nNetwork round trip: {}\nDecode here: {}\nOn screen here: {}",
            ms(self.total_ms()),
            ms(Some(h.capture_ms)),
            ms(Some(h.encode_ms)),
            ms(h.rtt_ms),
            ms(self.decode_ms),
            ms(self.draw_ms),
        )
    }
}

/// The ack for a picture's last part, sent the moment it arrives (before
/// decoding), so the host's round-trip measurement sees the network only.
pub fn ack_for(msg: &Message) -> Option<SessionMsg> {
    let Message::Binary(b) = msg else { return None };
    let (h, _) = TileHeader::decode(b)?;
    h.is_last().then_some(SessionMsg::Ack { seq: h.seq })
}

/// Decode the picture in a binary message (`None` for anything else).
pub fn decode_picture(msg: &Message) -> Option<crate::codec::Frame> {
    let Message::Binary(b) = msg else { return None };
    if FileChunk::decode(b).is_some() {
        return None;
    }
    let (h, data) = TileHeader::decode(b)?;
    crate::codec::decode_tile(h.format, data).ok()
}

/// Handle the UI keeps for an open session.
pub struct SessionHandle {
    pub shared: Arc<Mutex<SessionShared>>,
    tx: mpsc::UnboundedSender<SessionMsg>,
    /// Ordered, paced channel for uploads (announcement + file pieces).
    raw: mpsc::Sender<Message>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl SessionHandle {
    /// Start connecting. `repaint` is called whenever something changed.
    pub fn start(target: Target, quality: Quality, view_only: bool, repaint: Arc<dyn Fn() + Send + Sync>) -> SessionHandle {
        let shared = Arc::new(Mutex::new(SessionShared::new(view_only)));
        shared.lock().unwrap().route = Route::for_url(target.url().as_deref());
        let (tx, rx) = mpsc::unbounded_channel();
        let (raw, raw_rx) = mpsc::channel(4);
        let s2 = shared.clone();
        let r2 = repaint.clone();
        match target.url() {
            Some(url) => {
                runtime().spawn(network_task(url, s2, rx, raw_rx, quality, r2));
            }
            None => {
                runtime().spawn(demo_task(s2, rx, raw_rx, quality, view_only, r2));
            }
        }
        SessionHandle { shared, tx, raw, repaint }
    }

    pub fn send(&self, m: SessionMsg) {
        let _ = self.tx.send(m);
    }

    /// Sends input from another thread (keys read during an Alt+Tab grab).
    pub fn input_sender(&self) -> impl Fn(InputEvent) + Send + 'static {
        let tx = self.tx.clone();
        move |event| {
            let _ = tx.send(SessionMsg::Input { event });
        }
    }

    /// Put `text` (copied on this computer) on the remote clipboard, unless
    /// it is already there or the session only watches. Sent before any
    /// input that follows, so a Ctrl+V right after pastes it.
    pub fn send_clipboard(&self, text: &str) {
        if text.is_empty() || text.len() > crate::clipboard::MAX_TEXT {
            return;
        }
        {
            let mut s = self.shared.lock().unwrap();
            if s.view_only || s.remote_clipboard.as_deref().is_some_and(|r| crate::clipboard::same_text(r, text)) {
                return;
            }
            s.remote_clipboard = Some(text.to_string());
        }
        self.send(SessionMsg::Clipboard { text: text.to_string() });
    }

    /// Switch between "only watch" and "control" (ignored for locked links).
    pub fn set_view_only(&self, view_only: bool) {
        {
            let mut s = self.shared.lock().unwrap();
            if s.view_locked {
                return;
            }
            s.view_only = view_only; // the relay confirms with ViewMode
        }
        self.send(SessionMsg::SetViewOnly { view_only });
        (self.repaint)();
    }

    /// Show a remote folder ("" = starting places) in the file browser.
    pub fn list_remote(&self, path: &str) {
        let req = {
            let mut s = self.shared.lock().unwrap();
            s.remote_dir.loading = true;
            s.remote_changed = false;
            s.take_req()
        };
        self.send(SessionMsg::FsList { req, path: path.to_string() });
    }

    pub fn mkdir_remote(&self, path: &str) {
        let req = self.shared.lock().unwrap().take_req();
        self.send(SessionMsg::FsMkdir { req, path: path.to_string() });
    }

    /// Copy a remote file into `local_dir` (never overwrites).
    pub fn download(&self, remote: &FsEntry, local_dir: PathBuf) {
        let req = {
            let mut s = self.shared.lock().unwrap();
            let req = s.take_req();
            s.downloads.insert(req, (local_dir.clone(), None));
            s.transfers.push(Transfer {
                req,
                name: remote.name.clone(),
                upload: false,
                done: 0,
                total: remote.size,
                state: TransferState::Running,
                local: local_dir.join(&remote.name),
            });
            req
        };
        self.send(SessionMsg::FsDownload { req, path: remote.path.clone() });
    }

    /// Copy a local file into the remote folder `remote_dir`.
    pub fn upload(&self, local: PathBuf, remote_dir: String) {
        let name = local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let size = std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0);
        let req = {
            let mut s = self.shared.lock().unwrap();
            let req = s.take_req();
            s.transfers.push(Transfer {
                req,
                name: name.clone(),
                upload: true,
                done: 0,
                total: size,
                state: TransferState::Running,
                local: local.clone(),
            });
            req
        };
        let (raw, shared, repaint) = (self.raw.clone(), self.shared.clone(), self.repaint.clone());
        runtime().spawn(async move {
            use tokio::io::AsyncReadExt;
            let fail = |e: String| {
                if let Some(t) = shared.lock().unwrap().transfer(req) {
                    t.state = TransferState::Failed(e);
                }
                repaint();
            };
            let mut f = match tokio::fs::File::open(&local).await {
                Ok(f) => f,
                Err(e) => return fail(e.to_string()),
            };
            let announce = SessionMsg::FsUpload { req, dir: remote_dir, name, size };
            if raw.send(Message::Text(serde_json::to_string(&announce).unwrap().into())).await.is_err() {
                return fail("connection closed".into());
            }
            if size == 0 {
                return;
            }
            let mut buf = vec![0u8; FileChunk::SIZE];
            let mut sent = 0u64;
            loop {
                let n = match f.read(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => return fail(e.to_string()),
                };
                sent += n as u64;
                let last = n == 0 || sent >= size;
                if raw.send(Message::Binary(FileChunk { req, last }.encode(&buf[..n]).into())).await.is_err() {
                    return fail("connection closed".into());
                }
                if let Some(t) = shared.lock().unwrap().transfer(req) {
                    t.done = sent;
                }
                repaint();
                if last {
                    return;
                }
            }
        });
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.shared.lock().unwrap().state, ConnState::Closed(_))
    }
}

async fn network_task(
    url: String,
    shared: Arc<Mutex<SessionShared>>,
    mut rx: mpsc::UnboundedReceiver<SessionMsg>,
    mut raw_rx: mpsc::Receiver<Message>,
    quality: Quality,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    let mut quality = quality;
    let close = |reason: String| {
        let mut s = shared.lock().unwrap();
        if !matches!(s.state, ConnState::Closed(_)) {
            s.state = ConnState::Closed(reason);
        }
    };
    let ws = match tokio_tungstenite::connect_async_with_config(url.as_str(), None, true).await {
        Ok((ws, _)) => ws,
        Err(e) => {
            close(match e {
                tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 401 => "Please sign in again".into(),
                tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 403 => {
                    "This link has expired or the access was withdrawn".into()
                }
                other => format!("Cannot reach the server: {other}"),
            });
            repaint();
            return;
        }
    };
    let (mut sink, mut stream) = ws.split();
    loop {
        tokio::select! {
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(_)) | None => { close("Connection lost".into()); break; }
                };
                if let Message::Close(_) = msg { close("Connection closed".into()); break; }
                if let Some(ack) = ack_for(&msg)
                    && sink.send(Message::Text(serde_json::to_string(&ack).unwrap().into())).await.is_err()
                {
                    close("Connection lost".into()); break;
                }
                let replies = receive(&shared, &msg, &quality);
                for r in replies {
                    if sink.send(Message::Text(serde_json::to_string(&r).unwrap().into())).await.is_err() { break; }
                }
                repaint();
                if matches!(shared.lock().unwrap().state, ConnState::Closed(_)) { break; }
            }
            cmd = rx.recv() => match cmd {
                Some(m) => {
                    if let SessionMsg::Quality { quality: q } = &m { quality = *q; }
                    if sink.send(Message::Text(serde_json::to_string(&m).unwrap().into())).await.is_err() {
                        close("Connection lost".into()); break;
                    }
                }
                None => { let _ = sink.close().await; break; }
            },
            Some(m) = raw_rx.recv() => {
                if sink.send(m).await.is_err() { close("Connection lost".into()); break; }
            }
        }
    }
    repaint();
}

/// Same as a network session but talking to a pretend two-screen computer
/// inside this program — lets the person try windows and quality settings.
async fn demo_task(
    shared: Arc<Mutex<SessionShared>>,
    mut rx: mpsc::UnboundedReceiver<SessionMsg>,
    mut raw_rx: mpsc::Receiver<Message>,
    quality: Quality,
    mut view_only: bool,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    let mut quality = quality;
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(32);
    let file_out = out_tx.clone();
    // The demo computer's files are this computer's files.
    let mut files = crate::files::FileService::default();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = stop.clone();
    std::thread::spawn(move || crate::host::screen_loop(true, view_only, cmd_rx, out_tx, stop2));
    loop {
        tokio::select! {
            out = out_rx.recv() => match out {
                Some(msg) => {
                    if let Some(ack) = ack_for(&msg) { let _ = cmd_tx.send(ack); }
                    let replies = receive(&shared, &msg, &quality);
                    for r in replies { let _ = cmd_tx.send(r); }
                    repaint();
                }
                None => break,
            },
            cmd = rx.recv() => match cmd {
                Some(m) => {
                    if let SessionMsg::Quality { quality: q } = &m { quality = *q; }
                    if let SessionMsg::SetViewOnly { view_only: v } = m {
                        // The demo plays the relay's part: confirm the switch.
                        view_only = v;
                        let _ = cmd_tx.send(SessionMsg::SetViewOnly { view_only: v });
                        shared.lock().unwrap().view_only = v;
                        repaint();
                        continue;
                    }
                    if view_only && m.is_control() { continue; }
                    if m.is_file_request() {
                        files.handle(m, &file_out).await;
                    } else {
                        let _ = cmd_tx.send(m);
                    }
                }
                None => break,
            },
            Some(m) = raw_rx.recv() => {
                if view_only { continue; }
                match m {
                    Message::Text(t) => {
                        if let Ok(m) = serde_json::from_str::<SessionMsg>(&t) { files.handle(m, &file_out).await; }
                    }
                    Message::Binary(b) => {
                        if let Some((c, data)) = FileChunk::decode(&b)
                            && let Some(reply) = files.chunk(c, data)
                        {
                            let _ = file_out.send(Message::Text(serde_json::to_string(&reply).unwrap().into())).await;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_tells_relay_and_encryption() {
        assert_eq!(
            Route::for_url(Some("wss://rd.example.com/ws/view/m1?token=x")),
            Route::Relay { server: "rd.example.com".into(), encrypted: true }
        );
        assert_eq!(Route::for_url(Some("ws://10.0.0.5:8470/ws/guest/ab")), Route::Relay { server: "10.0.0.5:8470".into(), encrypted: false });
        assert_eq!(Route::for_url(None), Route::Demo);
        assert!(Route::for_url(Some("ws://h/x")).describe().contains("NOT encrypted"));
    }

    #[test]
    fn latency_adds_up_host_network_and_viewer_parts() {
        let mut s = SessionShared::new(false);
        assert_eq!(s.latency.total_ms(), None);
        let t = serde_json::to_string(&SessionMsg::Timing { capture_ms: 3.0, encode_ms: 7.0, rtt_ms: Some(30.0) }).unwrap();
        s.apply(&Message::Text(t.into()), &Quality::default());
        assert_eq!(s.latency.total_ms(), Some(40.0), "nothing measured here yet");
        s.latency.decode_ms = Some(2.0);
        s.latency.draw_ms = Some(8.0);
        assert_eq!(s.latency.total_ms(), Some(50.0));
        assert!(s.latency.describe().contains("Network round trip: 30 ms"));
        // A host that sends nonsense can't break the numbers.
        let bad = r#"{"t":"timing","capture_ms":-5.0,"encode_ms":1e30,"rtt_ms":null}"#;
        s.apply(&Message::Text(bad.into()), &Quality::default());
        let h = s.latency.host.unwrap();
        assert_eq!((h.capture_ms, h.encode_ms, h.rtt_ms), (0.0, 60_000.0, None));
        assert_eq!(s.latency.total_ms(), None, "no round trip yet");
    }

    #[test]
    fn drawn_measures_arrival_to_screen() {
        let mut s = SessionShared::new(false);
        s.undrawn.insert(0, Instant::now() - std::time::Duration::from_millis(12));
        s.drawn(0);
        s.drawn(1); // nothing waiting: ignored
        assert_eq!(s.window_draw.1, 1);
        assert!(s.window_draw.0 >= 12.0);
    }

    #[test]
    fn parses_temporary_links() {
        assert_eq!(
            parse_link(" https://rd.example.com/g/abc123 "),
            Some(GuestLink { server: "https://rd.example.com".into(), token: "abc123".into() })
        );
        assert_eq!(parse_link("http://10.0.0.2:8470/g/ff00/").unwrap().server, "http://10.0.0.2:8470");
        assert!(parse_link("https://rd.example.com/x/abc").is_none());
        assert!(parse_link("rd.example.com/g/abc").is_none());
        assert!(parse_link("https://rd.example.com/g/a b").is_none());
    }

    #[test]
    fn target_urls() {
        let t = Target::Admin { server: "https://rd.x.com".into(), token: "tk".into(), machine_id: "m1".into(), view_only: true };
        assert_eq!(t.url().unwrap(), "wss://rd.x.com/ws/view/m1?token=tk&view_only=1");
        let g = Target::Guest(GuestLink { server: "http://h:1".into(), token: "zz".into() });
        assert_eq!(g.url().unwrap(), "ws://h:1/ws/guest/zz");
        assert!(Target::Demo.url().is_none());
    }

    #[test]
    fn shared_state_follows_messages() {
        let mut s = SessionShared::new(false);
        let q = Quality::default();
        let hello = SessionMsg::Hello {
            hostname: "pc".into(),
            os: "Windows".into(),
            version: "1".into(),
            monitors: crate::desktop::TestPattern::dual().monitors,
            view_only: true,
        };
        let r = s.apply(&Message::Text(serde_json::to_string(&hello).unwrap().into()), &q);
        assert_eq!(
            r,
            vec![SessionMsg::Quality { quality: q }, SessionMsg::PointerMode { local: true }],
            "sends our quality on hello and asks to draw the pointer here"
        );
        assert_eq!(s.state, ConnState::Live);
        assert!(s.view_only, "host can force view-only");
        assert_eq!(s.monitors.len(), 2);

        let mut enc = crate::codec::MonitorEncoder::default();
        let frame = crate::codec::Frame::filled(100, 50, [1, 2, 3, 255]);
        let msgs = enc.encode(frame, &q, 1, 7).unwrap();
        let mut acks = Vec::new();
        for m in msgs {
            let m = Message::Binary(m.into());
            acks.extend(ack_for(&m));
            assert!(s.apply(&m, &q).is_empty(), "acks go before decoding, not from apply");
        }
        assert_eq!(acks, vec![SessionMsg::Ack { seq: 7 }]);
        assert_eq!(ack_for(&Message::Text("{}".into())), None);
        assert_eq!(s.canvases[&1].frame.as_ref().unwrap().width, 100);

        // Pointer shapes are checked before use; positions are kept.
        let arrow = crate::desktop::PointerImage::arrow();
        let shape = |id, rgba: String| SessionMsg::PointerShape { id, width: arrow.width, height: arrow.height, hot_x: 0, hot_y: 0, rgba };
        for bad in [shape(0, crate::util::base64_encode(&arrow.rgba)), shape(2, "AAAA".into()), shape(3, "not base64!".into())] {
            s.apply(&Message::Text(serde_json::to_string(&bad).unwrap().into()), &q);
        }
        assert!(s.pointer_shapes.is_empty(), "broken shapes ignored");
        s.apply(&Message::Text(serde_json::to_string(&shape(1, crate::util::base64_encode(&arrow.rgba))).unwrap().into()), &q);
        assert_eq!(s.pointer_shapes[&1], arrow);
        let pos = SessionMsg::Pointer { x: 10, y: 20, shape: 1 };
        s.apply(&Message::Text(serde_json::to_string(&pos).unwrap().into()), &q);
        assert_eq!(s.pointer, Some((10, 20, 1)));

        let closed = SessionMsg::Closed { reason: "bye".into() };
        s.apply(&Message::Text(serde_json::to_string(&closed).unwrap().into()), &q);
        assert_eq!(s.state, ConnState::Closed("bye".into()));
    }

    #[test]
    fn demo_previews_decode_and_broken_pictures_are_skipped() {
        let mons = crate::desktop::TestPattern::dual().monitors;
        let mut p = demo_preview(&mons, 1);
        assert_eq!(p.shots.len(), 2);
        let frames = decode_preview(&p);
        assert_eq!(frames.iter().map(|(i, f)| (*i, f.width, f.height)).collect::<Vec<_>>(), vec![(0, 480, 270), (1, 480, 384)]);
        p.shots[0].jpeg = "not base64!".into();
        p.shots[1].jpeg = crate::util::base64_encode(b"not a jpeg");
        assert!(decode_preview(&p).is_empty());
    }
}
