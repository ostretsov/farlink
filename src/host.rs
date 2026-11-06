//! The helper on a remote machine. It keeps one connection open to the relay
//! (outgoing only — nothing listens on this machine), opens a session socket
//! when someone wants to see the screen, and installs updates by itself.

use crate::codec::{Frame, MonitorEncoder, prepare};
use crate::config::{HostConfig, save_json};
use crate::desktop::{Desktop, TestPattern, open_desktop};
use crate::proto::{AgentMsg, MonitorInfo, PREVIEW_MAX, PreviewShot, SessionMsg};
use crate::quality::Quality;
use crate::update::EXIT_UPDATED;
use crate::util::{current_target, hostname, os_label, url_encode, ws_url};
use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

/// How often the host asks for updates on its own (the server also pushes).
const UPDATE_CHECK: Duration = Duration::from_secs(6 * 3600);
const HEARTBEAT: Duration = Duration::from_secs(30);
/// A screen still for this long gets its rough JPEG parts sent again sharp.
const REFINE_AFTER: Duration = Duration::from_millis(300);

#[derive(Clone)]
pub struct HostOptions {
    /// Show a pretend two-monitor desktop instead of the real one.
    pub demo: bool,
    /// Never self-update (used by tests).
    pub no_update: bool,
}

fn desktop(demo: bool) -> Result<Box<dyn Desktop>> {
    if demo { Ok(Box::new(TestPattern::dual())) } else { open_desktop() }
}

fn current_monitors(demo: bool) -> Vec<MonitorInfo> {
    desktop(demo).and_then(|mut d| d.monitors()).unwrap_or_default()
}

/// Run until updated (returns [`EXIT_UPDATED`]) — reconnects forever otherwise.
pub async fn run_agent(cfg_path: PathBuf, mut cfg: HostConfig, opts: HostOptions) -> Result<i32> {
    if cfg.server_url.is_empty() {
        bail!("no server configured — install this computer from your Farlink install link");
    }
    if !opts.no_update
        && let Ok(exe) = std::env::current_exe()
    {
        crate::update::cleanup_old(&exe);
    }
    let mut backoff = Duration::from_secs(2);
    let mut last_update_check = Instant::now();
    loop {
        match agent_connection(&cfg_path, &mut cfg, &opts, &mut last_update_check).await {
            Ok(Some(code)) => return Ok(code),
            Ok(None) => backoff = Duration::from_secs(2),
            Err(e) => log::warn!("relay connection: {e:#}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// One connection to the relay. `Ok(Some(code))` means exit the program.
async fn agent_connection(
    cfg_path: &PathBuf,
    cfg: &mut HostConfig,
    opts: &HostOptions,
    last_update_check: &mut Instant,
) -> Result<Option<i32>> {
    let url = ws_url(&cfg.server_url, "/ws/agent");
    let (ws, _) = tokio_tungstenite::connect_async_with_config(url.as_str(), None, true).await.with_context(|| format!("connecting to {url}"))?;
    let (mut sink, mut stream) = ws.split();
    let demo = opts.demo;
    let monitors = tokio::task::spawn_blocking(move || current_monitors(demo)).await.unwrap_or_default();
    let register = AgentMsg::Register {
        enroll_token: if cfg.is_enrolled() { None } else { cfg.enroll_token.clone() },
        machine_id: cfg.machine_id.clone(),
        secret: cfg.secret.clone(),
        hostname: hostname(),
        os: os_label(),
        version: crate::VERSION.to_string(),
        target: current_target().to_string(),
        monitors: monitors.clone(),
    };
    sink.send(Message::Text(serde_json::to_string(&register)?.into())).await?;
    log::info!("connected to {}", cfg.server_url);

    // Preview pictures are taken on their own thread (screen APIs block).
    let (preview_tx, preview_rx) = std::sync::mpsc::channel::<u32>();
    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel::<AgentMsg>();
    std::thread::Builder::new().name("preview".into()).spawn(move || preview_worker(demo, preview_rx, reply_tx))?;

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;
    loop {
        tokio::select! {
            reply = reply_rx.recv() => {
                let Some(reply) = reply else { continue };
                sink.send(Message::Text(serde_json::to_string(&reply)?.into())).await?;
            }
            msg = stream.next() => {
                let Some(msg) = msg else { return Ok(None) };
                let text = match msg? {
                    Message::Text(t) => t.to_string(),
                    Message::Close(_) => return Ok(None),
                    _ => continue,
                };
                let Ok(msg) = serde_json::from_str::<AgentMsg>(&text) else { continue };
                match msg {
                    AgentMsg::Registered { machine_id, secret, name } => {
                        if cfg.machine_id.as_deref() != Some(&machine_id) || cfg.secret.as_deref() != Some(&secret) {
                            cfg.machine_id = Some(machine_id);
                            cfg.secret = Some(secret);
                            cfg.enroll_token = None;
                            if let Err(e) = save_json(cfg_path, cfg) {
                                log::error!("cannot save {}: {e}", cfg_path.display());
                            }
                        }
                        log::info!("registered as '{name}'");
                    }
                    AgentMsg::Rejected { reason } => {
                        log::error!("server refused this computer: {reason}");
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        return Ok(None);
                    }
                    AgentMsg::OpenSession { session_id, view_only } => {
                        let (Some(id), Some(secret)) = (cfg.machine_id.clone(), cfg.secret.clone()) else { continue };
                        let server = cfg.server_url.clone();
                        tokio::spawn(async move {
                            if let Err(e) = run_session(server, id, secret, session_id, view_only, demo).await {
                                log::warn!("session ended: {e:#}");
                            }
                        });
                    }
                    AgentMsg::PreviewRequest { req } => {
                        let _ = preview_tx.send(req);
                    }
                    AgentMsg::UpdateAvailable { version, force } => {
                        if !force && (opts.no_update || !crate::version::is_newer(&version, crate::VERSION)) {
                            continue;
                        }
                        let result = if opts.no_update {
                            Err("updates are switched off on this computer".to_string())
                        } else {
                            run_update(&cfg.server_url, force).await
                        };
                        match result {
                            Ok(Some(code)) => return Ok(Some(code)),
                            Ok(None) => {}
                            Err(error) => {
                                log::warn!("update to {version} failed: {error}");
                                let msg = AgentMsg::UpdateFailed { version, error };
                                sink.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ = heartbeat.tick() => {
                let hb = AgentMsg::Heartbeat { monitors: monitors.clone() };
                sink.send(Message::Text(serde_json::to_string(&hb)?.into())).await?;
                if !opts.no_update && last_update_check.elapsed() > UPDATE_CHECK {
                    *last_update_check = Instant::now();
                    if let Some(code) = try_update(&cfg.server_url).await {
                        return Ok(Some(code));
                    }
                }
            }
        }
    }
}

/// Keep the desktop open this long after the last preview (Windows screen
/// capture takes a moment to start; previews come every few seconds).
const PREVIEW_KEEP_OPEN: Duration = Duration::from_secs(60);

/// Answers [`AgentMsg::PreviewRequest`]s until the connection ends.
fn preview_worker(demo: bool, rx: std::sync::mpsc::Receiver<u32>, tx: tokio::sync::mpsc::UnboundedSender<AgentMsg>) {
    let mut desk: Option<Box<dyn Desktop>> = None;
    loop {
        let first = match rx.recv_timeout(PREVIEW_KEEP_OPEN) {
            Ok(req) => req,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                desk = None;
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        };
        // Requests that piled up meanwhile all get the same pictures.
        let reqs: Vec<u32> = std::iter::once(first).chain(rx.try_iter()).collect();
        if desk.is_none() {
            desk = desktop(demo).map_err(|e| log::debug!("preview: {e:#}")).ok();
        }
        let shots = match desk.as_mut().map(|d| preview_shots(d.as_mut())) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                log::debug!("preview: {e:#}");
                desk = None; // e.g. the desktop switched; open it again next time
                Vec::new()
            }
            None => Vec::new(),
        };
        for req in reqs {
            if tx.send(AgentMsg::Preview { req, shots: shots.clone() }).is_err() {
                return;
            }
        }
    }
}

/// Small JPEG pictures of every monitor.
pub fn preview_shots(desk: &mut dyn Desktop) -> Result<Vec<PreviewShot>> {
    let _ = desk.changed(); // lets change-tracking capture (DXGI) take the newest picture
    let mut shots = Vec::new();
    for m in desk.monitors()? {
        let frame = desk.capture(&m)?;
        let (width, height, jpeg) = crate::codec::thumbnail_jpeg(&frame, PREVIEW_MAX, 70)?;
        shots.push(PreviewShot { index: m.index, width, height, jpeg: crate::util::base64_encode(&jpeg) });
    }
    Ok(shots)
}

/// Regular update check: logs failures, returns the exit code if updated.
async fn try_update(server: &str) -> Option<i32> {
    match run_update(server, false).await {
        Ok(code) => code,
        Err(e) => {
            log::warn!("update failed: {e}");
            None
        }
    }
}

/// Update to the published version (`force`: even if not newer). Returns
/// [`EXIT_UPDATED`] after installing it.
async fn run_update(server: &str, force: bool) -> Result<Option<i32>, String> {
    let server = server.to_string();
    let job = tokio::task::spawn_blocking(move || {
        if force {
            crate::update::install_published(&server, "farlink-host").map(Some)
        } else {
            crate::update::update_self(&server, "farlink-host")
        }
    });
    match job.await {
        Ok(Ok(Some(v))) => {
            log::info!("updated to {v}, restarting");
            Ok(Some(EXIT_UPDATED))
        }
        Ok(Ok(None)) => Ok(None),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(e) => Err(format!("{e}")),
    }
}

/// One viewing session: connect the session socket and stream pictures.
pub async fn run_session(server: String, machine_id: String, secret: String, session_id: String, mut view_only: bool, demo: bool) -> Result<()> {
    let url = ws_url(
        &server,
        &format!("/ws/host-session/{}?machine={}&secret={}", url_encode(&session_id), url_encode(&machine_id), url_encode(&secret)),
    );
    let (ws, _) = tokio_tungstenite::connect_async_with_config(url.as_str(), None, true).await.context("opening session")?;
    let (mut sink, mut stream) = ws.split();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<SessionMsg>();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Message>(32);
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let file_out = out_tx.clone();
    let mut files = crate::files::FileService::default();
    std::thread::Builder::new()
        .name("screen".into())
        .spawn(move || screen_loop(demo, view_only, cmd_rx, out_tx, stop2))?;
    let result = async {
        loop {
            tokio::select! {
                msg = stream.next() => match msg {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(m) = serde_json::from_str::<SessionMsg>(&t) {
                            if matches!(m, SessionMsg::Closed { .. }) { break; }
                            // Only the relay sends this, after checking the person may switch.
                            if let SessionMsg::SetViewOnly { view_only: v } = m { view_only = v; }
                            if m.is_file_request() {
                                // Files only in control sessions (the relay checks too).
                                if !view_only { files.handle(m, &file_out).await; }
                                continue;
                            }
                            let _ = cmd_tx.send(m);
                        }
                    }
                    Some(Ok(Message::Binary(b))) => {
                        if !view_only
                            && let Some((c, data)) = crate::proto::FileChunk::decode(&b)
                            && let Some(reply) = files.chunk(c, data)
                        {
                            let _ = file_out.send(Message::Text(serde_json::to_string(&reply)?.into())).await;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => return Err(e.into()),
                    _ => {}
                },
                out = out_rx.recv() => match out {
                    Some(m) => sink.send(m).await?,
                    None => break,
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stop.store(true, Ordering::Relaxed);
    let _ = sink.close().await;
    result
}

/// Capture + encode loop on its own thread (screen APIs are not async).
pub fn screen_loop(
    demo: bool,
    mut view_only: bool,
    rx: std::sync::mpsc::Receiver<SessionMsg>,
    tx: tokio::sync::mpsc::Sender<Message>,
    stop: Arc<AtomicBool>,
) {
    let send_json = |m: &SessionMsg| tx.blocking_send(Message::Text(serde_json::to_string(m).unwrap().into())).is_ok();
    let mut desk = match desktop(demo) {
        Ok(d) => d,
        Err(e) => {
            send_json(&SessionMsg::Notice { text: format!("Cannot show this screen: {e:#}") });
            return;
        }
    };
    let mut monitors = desk.monitors().unwrap_or_default();
    if !send_json(&SessionMsg::Hello {
        hostname: hostname(),
        os: os_label(),
        version: crate::VERSION.into(),
        monitors: monitors.clone(),
        view_only,
    }) {
        return;
    }
    let mut quality = Quality::default();
    // The viewer answers the hello with its quality choice; wait for it
    // briefly so the first (largest) picture is already the right size.
    match rx.recv_timeout(Duration::from_millis(1500)) {
        Ok(SessionMsg::Quality { quality: q }) => quality = q.clamped(),
        Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
    }
    let mut encoders: Vec<MonitorEncoder> = monitors.iter().map(|_| MonitorEncoder::default()).collect();
    // Last full-size picture per monitor: unchanged screens are not scaled or encoded.
    let mut prev_raw: Vec<Option<Frame>> = monitors.iter().map(|_| None).collect();
    // When each monitor's picture last changed (rough parts are sharpened after a pause).
    let mut last_change: Vec<Instant> = monitors.iter().map(|_| Instant::now()).collect();
    // Monitors the viewer has on screen (None = all).
    let mut watch: Option<Vec<u8>> = None;
    let mut idle = IdleTracker::new(Instant::now());
    let mut seq: u32 = 0;
    let mut last_sent: u32 = 0;
    let mut acked: u32 = 0;
    let mut waiting_since: Option<Instant> = None;
    let mut last_monitor_check = Instant::now();
    let mut errors = 0u32;

    while !stop.load(Ordering::Relaxed) {
        let start = Instant::now();
        loop {
            match rx.try_recv() {
                Ok(SessionMsg::Quality { quality: q }) => {
                    quality = q.clamped();
                    encoders.iter_mut().for_each(|e| e.reset());
                    prev_raw.iter_mut().for_each(|p| *p = None);
                }
                Ok(SessionMsg::SetViewOnly { view_only: v }) => view_only = v,
                Ok(SessionMsg::Input { event }) => {
                    idle.activity(Instant::now());
                    if !view_only && let Err(e) = desk.input(&monitors, &event) {
                        log::debug!("input: {e}");
                    }
                }
                Ok(SessionMsg::Ack { seq: s }) => acked = acked.max(s),
                Ok(SessionMsg::Refresh) => {
                    encoders.iter_mut().for_each(|e| e.reset());
                    prev_raw.iter_mut().for_each(|p| *p = None);
                }
                Ok(SessionMsg::Watch { monitors: w }) => {
                    // Screens that come back into view get a complete picture.
                    for (i, m) in monitors.iter().enumerate() {
                        let was = watch.as_ref().is_none_or(|v| v.contains(&m.index));
                        if !was && w.contains(&m.index) {
                            encoders[i].reset();
                            prev_raw[i] = None;
                        }
                    }
                    watch = Some(w);
                    idle.activity(Instant::now());
                }
                Ok(_) => {}
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }

        if last_monitor_check.elapsed() > Duration::from_secs(3) {
            last_monitor_check = Instant::now();
            if let Ok(now) = desk.monitors()
                && now != monitors
            {
                monitors = now;
                encoders = monitors.iter().map(|_| MonitorEncoder::default()).collect();
                prev_raw = monitors.iter().map(|_| None).collect();
                last_change = monitors.iter().map(|_| Instant::now()).collect();
                if !send_json(&SessionMsg::Monitors { monitors: monitors.clone() }) {
                    return;
                }
            }
        }

        // Flow control: at most two pictures on the way. A lost ack can't
        // freeze the session for more than 3 seconds.
        if last_sent.wrapping_sub(acked) >= 2 && last_sent > acked {
            let since = *waiting_since.get_or_insert_with(Instant::now);
            if since.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            acked = last_sent;
        }
        waiting_since = None;

        // Auto quality: parts first sent as rough JPEG (photos, video) are
        // sent again sharp once that screen has been still for a moment.
        let mut refined = false;
        for (i, m) in monitors.iter().enumerate() {
            if watch.as_ref().is_some_and(|w| !w.contains(&m.index))
                || !encoders[i].needs_refine()
                || last_change[i].elapsed() < REFINE_AFTER
            {
                continue;
            }
            seq = seq.wrapping_add(1).max(1);
            match encoders[i].refine(&quality, m.index, seq) {
                Ok(msgs) => {
                    for msg in msgs {
                        refined = true;
                        if tx.blocking_send(Message::Binary(msg.into())).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => log::warn!("encode: {e}"),
            }
        }
        if refined {
            last_sent = seq;
            continue; // flow control first, then look at the screen again
        }

        // Nothing changed on screen (X11 tells us), and no safety re-check is
        // due: don't even read the screen.
        let now = Instant::now();
        let changed = desk.changed();
        if !changed && !idle.recheck_due(now) {
            sleep_until_next(start, idle.interval(&quality, now));
            continue;
        }
        idle.checked(now);

        seq = seq.wrapping_add(1).max(1);
        let mut sent_any = false;
        for (i, m) in monitors.iter().enumerate() {
            if watch.as_ref().is_some_and(|w| !w.contains(&m.index)) {
                continue; // nobody is looking at this screen
            }
            let frame = match desk.capture(m) {
                Ok(f) => {
                    errors = 0;
                    f
                }
                Err(e) => {
                    errors += 1;
                    log::debug!("capture: {e}");
                    if errors > 10 {
                        std::thread::sleep(Duration::from_millis(500));
                        if let Ok(d) = desktop(demo) {
                            desk = d;
                        }
                        errors = 0;
                    }
                    continue;
                }
            };
            let prepared = if quality.scale_pct >= 100 && quality.color == crate::quality::ColorDepth::Full {
                frame // nothing to prepare; the encoder skips unchanged parts itself
            } else {
                if prev_raw[i].as_ref() == Some(&frame) {
                    continue; // identical picture: skip scaling and compression
                }
                let prepared = prepare(&frame, &quality);
                prev_raw[i] = Some(frame);
                prepared
            };
            match encoders[i].encode(prepared, &quality, m.index, seq) {
                Ok(msgs) => {
                    if !msgs.is_empty() {
                        last_change[i] = Instant::now();
                    }
                    for msg in msgs {
                        sent_any = true;
                        if tx.blocking_send(Message::Binary(msg.into())).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => log::warn!("encode: {e}"),
            }
        }
        if sent_any {
            last_sent = seq;
            idle.activity(Instant::now());
        }
        sleep_until_next(start, idle.interval(&quality, Instant::now()));
    }
}

fn sleep_until_next(start: Instant, interval: Duration) {
    if let Some(rest) = interval.checked_sub(start.elapsed()) {
        std::thread::sleep(rest);
    }
}

/// Picture pacing: full speed while things happen, slow checks when the
/// screen has been still for a while (an idle session costs almost nothing).
#[derive(Debug, Clone, Copy)]
pub struct IdleTracker {
    last_activity: Instant,
    last_check: Instant,
}

impl IdleTracker {
    /// After this long without changes the session counts as idle.
    pub const IDLE_AFTER: Duration = Duration::from_secs(2);
    /// Checks per second while idle.
    pub const IDLE_INTERVAL: Duration = Duration::from_millis(250);
    /// Even when the system says "nothing changed", look again this often.
    pub const RECHECK: Duration = Duration::from_secs(2);

    pub fn new(now: Instant) -> IdleTracker {
        IdleTracker { last_activity: now, last_check: now }
    }

    /// Something changed or the person used mouse/keyboard.
    pub fn activity(&mut self, now: Instant) {
        self.last_activity = now;
    }

    pub fn checked(&mut self, now: Instant) {
        self.last_check = now;
    }

    pub fn recheck_due(&self, now: Instant) -> bool {
        now.duration_since(self.last_check) >= Self::RECHECK
    }

    pub fn is_idle(&self, now: Instant) -> bool {
        now.duration_since(self.last_activity) >= Self::IDLE_AFTER
    }

    /// Time between screen checks right now.
    pub fn interval(&self, q: &Quality, now: Instant) -> Duration {
        let busy = Duration::from_millis(q.frame_interval_ms());
        if self.is_idle(now) { busy.max(Self::IDLE_INTERVAL) } else { busy }
    }
}

// ------------------------------------------------------------------ logging

/// Simple log file next to the settings (services have no console).
pub struct FileLogger {
    file: std::sync::Mutex<std::fs::File>,
}

impl FileLogger {
    pub fn install(path: &std::path::Path) -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        if std::fs::metadata(path).map(|m| m.len() > 5_000_000).unwrap_or(false) {
            let _ = std::fs::rename(path, path.with_extension("log.1"));
        }
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        log::set_boxed_logger(Box::new(FileLogger { file: std::sync::Mutex::new(file) }))
            .map(|()| log::set_max_level(log::LevelFilter::Info))
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

impl log::Log for FileLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            use std::io::Write;
            let mut f = self.file.lock().unwrap();
            let _ = writeln!(f, "{} {} {}", crate::util::now_secs(), r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

// ------------------------------------------------------------------ Windows service

#[cfg(windows)]
pub mod service {
    //! Windows service (runs as SYSTEM). It starts `farlink-host run` inside
    //! the active console session with a SYSTEM token, so the helper can see
    //! the sign-in screen and UAC prompts, restarts it when the session
    //! changes, and sends Ctrl+Alt+Del on the helper's request.

    use crate::desktop::windows::{SAS_EVENT, send_sas, wide};
    use crate::install::WIN_SERVICE;
    use crate::update::EXIT_UPDATED;
    use std::ffi::OsString;
    use std::mem::{size_of, zeroed};
    use std::ptr::{null, null_mut};
    use std::sync::mpsc;
    use std::time::Duration;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher};
    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::Security::*;
    use windows_sys::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    use windows_sys::Win32::System::Threading::*;

    define_windows_service!(ffi_service_main, service_main);

    pub fn run() -> anyhow::Result<()> {
        service_dispatcher::start(WIN_SERVICE, ffi_service_main)?;
        Ok(())
    }

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = run_service() {
            log::error!("service: {e:#}");
        }
    }

    fn status(state: ServiceState, code: u32) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: if code == 0 { ServiceExitCode::Win32(0) } else { ServiceExitCode::ServiceSpecific(code) },
            checkpoint: 0,
            wait_hint: Duration::from_secs(5),
            process_id: None,
        }
    }

    struct Child {
        process: HANDLE,
        session: u32,
    }

    fn run_service() -> anyhow::Result<()> {
        let (stop_tx, stop_rx) = mpsc::channel();
        let handle = service_control_handler::register(WIN_SERVICE, move |c| match c {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = stop_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;
        handle.set_service_status(status(ServiceState::Running, 0))?;

        // Ctrl+Alt+Del requests from the helper.
        std::thread::spawn(|| unsafe {
            let ev = CreateEventW(null(), 0, 0, wide(SAS_EVENT).as_ptr());
            if ev.is_null() {
                return;
            }
            loop {
                if WaitForSingleObject(ev, INFINITE) == WAIT_OBJECT_0 {
                    send_sas();
                }
            }
        });

        let mut child: Option<Child> = None;
        let mut exit_code = 0;
        loop {
            if stop_rx.recv_timeout(Duration::from_millis(700)).is_ok() {
                break;
            }
            let session = unsafe { WTSGetActiveConsoleSessionId() };
            if let Some(c) = &child {
                let mut code = 0u32;
                unsafe { GetExitCodeProcess(c.process, &mut code) };
                if code != STILL_ACTIVE as u32 {
                    unsafe { CloseHandle(c.process) };
                    child = None;
                    if code as i32 == EXIT_UPDATED {
                        // The program file was replaced: stop with an error so
                        // Windows restarts the (new) service.
                        exit_code = EXIT_UPDATED as u32;
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                if c.session != session {
                    unsafe {
                        TerminateProcess(c.process, 0);
                        CloseHandle(c.process);
                    }
                    child = None;
                }
            }
            if child.is_none() && session != 0xFFFF_FFFF {
                match spawn_in_session(session) {
                    Ok(p) => child = Some(Child { process: p, session }),
                    Err(e) => {
                        log::error!("starting helper: {e:#}");
                        std::thread::sleep(Duration::from_secs(3));
                    }
                }
            }
        }
        if let Some(c) = child {
            unsafe {
                TerminateProcess(c.process, 0);
                CloseHandle(c.process);
            }
        }
        handle.set_service_status(status(ServiceState::Stopped, exit_code))?;
        Ok(())
    }

    /// Start `farlink-host run` in `session` as SYSTEM on winsta0\default.
    fn spawn_in_session(session: u32) -> anyhow::Result<HANDLE> {
        unsafe {
            let mut token: HANDLE = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut token) == 0 {
                anyhow::bail!("OpenProcessToken failed");
            }
            let mut dup: HANDLE = null_mut();
            let ok = DuplicateTokenEx(token, 0x0200_0000 /* MAXIMUM_ALLOWED */, null(), SecurityImpersonation, TokenPrimary, &mut dup);
            CloseHandle(token);
            if ok == 0 {
                anyhow::bail!("DuplicateTokenEx failed");
            }
            let mut sid = session;
            SetTokenInformation(dup, TokenSessionId, &mut sid as *mut u32 as *const _, size_of::<u32>() as u32);

            let exe = std::env::current_exe()?;
            let app = wide(&exe.display().to_string());
            let mut cmd = wide(&format!("\"{}\" run", exe.display()));
            let mut desktop = wide("winsta0\\default");
            let mut si: STARTUPINFOW = zeroed();
            si.cb = size_of::<STARTUPINFOW>() as u32;
            si.lpDesktop = desktop.as_mut_ptr();
            let mut pi: PROCESS_INFORMATION = zeroed();
            let ok = CreateProcessAsUserW(
                dup,
                app.as_ptr(),
                cmd.as_mut_ptr(),
                null(),
                null(),
                0,
                CREATE_NO_WINDOW,
                null(),
                null(),
                &si,
                &mut pi,
            );
            CloseHandle(dup);
            if ok == 0 {
                anyhow::bail!("CreateProcessAsUserW failed ({})", GetLastError());
            }
            CloseHandle(pi.hThread);
            Ok(pi.hProcess)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::TileHeader;
    use crate::quality::Preset;

    #[test]
    fn idle_sessions_slow_down_and_wake_on_activity() {
        let t0 = Instant::now();
        let mut idle = IdleTracker::new(t0);
        let q = Quality { max_fps: 20, ..Quality::preset(Preset::Balanced) };
        assert_eq!(idle.interval(&q, t0), Duration::from_millis(50), "busy: full speed");
        let later = t0 + Duration::from_secs(3);
        assert!(idle.is_idle(later));
        assert_eq!(idle.interval(&q, later), IdleTracker::IDLE_INTERVAL, "idle: 4 checks a second");
        idle.activity(later);
        assert_eq!(idle.interval(&q, later), Duration::from_millis(50), "mouse use wakes it up");
        assert!(!idle.recheck_due(t0 + Duration::from_millis(500)));
        assert!(idle.recheck_due(t0 + IdleTracker::RECHECK));
    }

    /// Runs the real capture loop on the pretend two-screen computer.
    fn run_loop(first: Vec<SessionMsg>, millis: u64) -> Vec<Message> {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(64);
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let th = std::thread::spawn(move || screen_loop(true, false, cmd_rx, out_tx, s2));
        for m in first {
            cmd_tx.send(m).unwrap();
        }
        let mut got = Vec::new();
        let end = Instant::now() + Duration::from_millis(millis);
        while Instant::now() < end {
            match out_rx.try_recv() {
                Ok(m) => {
                    if let Message::Binary(b) = &m
                        && let Some((h, _)) = TileHeader::decode(b)
                        && h.is_last()
                    {
                        let _ = cmd_tx.send(SessionMsg::Ack { seq: h.seq });
                    }
                    got.push(m);
                }
                Err(_) => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        stop.store(true, Ordering::Relaxed);
        drop(out_rx);
        let _ = th.join();
        got
    }

    fn tile_monitors(msgs: &[Message]) -> HashSet<u8> {
        msgs.iter()
            .filter_map(|m| match m {
                Message::Binary(b) => TileHeader::decode(b).map(|(h, _)| h.monitor),
                _ => None,
            })
            .collect()
    }

    use std::collections::HashSet;

    #[test]
    fn only_watched_screens_are_captured() {
        let fast = Quality::preset(Preset::Fast);
        let all = run_loop(vec![SessionMsg::Quality { quality: fast }], 600);
        assert_eq!(tile_monitors(&all), HashSet::from([0, 1]));
        let one = run_loop(vec![SessionMsg::Quality { quality: fast }, SessionMsg::Watch { monitors: vec![1] }], 600);
        assert_eq!(tile_monitors(&one), HashSet::from([1]), "screen 1 window closed: not captured");
        let none = run_loop(vec![SessionMsg::Quality { quality: fast }, SessionMsg::Watch { monitors: vec![] }], 400);
        assert!(tile_monitors(&none).is_empty(), "all windows minimised: nothing captured");
    }

    #[test]
    fn preview_has_a_small_picture_of_every_screen() {
        let mut desk = TestPattern::dual();
        let shots = preview_shots(&mut desk).unwrap();
        assert_eq!(shots.iter().map(|s| (s.index, s.width, s.height)).collect::<Vec<_>>(), vec![(0, 480, 270), (1, 480, 384)]);
        for s in &shots {
            let jpeg = crate::util::base64_decode(&s.jpeg).unwrap();
            let f = crate::codec::decode_tile(crate::proto::TileFormat::Jpeg, &jpeg).unwrap();
            assert_eq!((f.width, f.height), (s.width, s.height));
            assert!(jpeg.len() < 100_000, "previews stay small: {} bytes", jpeg.len());
        }
    }

    #[test]
    fn preview_worker_answers_every_request_and_stops_with_the_connection() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        // Several requests at once share one set of pictures.
        for req in [7, 8, 9] {
            tx.send(req).unwrap();
        }
        let th = std::thread::spawn(move || preview_worker(true, rx, out_tx));
        let mut answered = Vec::new();
        for _ in 0..3 {
            match out_rx.blocking_recv().unwrap() {
                AgentMsg::Preview { req, shots } => {
                    assert_eq!(shots.len(), 2);
                    answered.push(req);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(answered, vec![7, 8, 9]);
        drop(tx);
        th.join().unwrap();
        assert!(out_rx.blocking_recv().is_none(), "worker ended when the connection went away");
    }
}
