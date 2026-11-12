//! Relay + web admin + install links + update feed (`farlink-server`).
//!
//! * Hosts keep a control WebSocket open (`/ws/agent`); they never accept
//!   incoming connections.
//! * Viewers (desktop app or browser) connect to `/ws/view/{machine}` (admin)
//!   or `/ws/guest/{token}` (temporary link). The relay asks the host to open
//!   a session socket and then pipes the two together, dropping mouse and
//!   keyboard messages for view-only sessions.
//! * Withdrawing or expiring a link ends its sessions immediately.

use crate::config::{ServerConfig, load_json, save_json};
use crate::grants::{Grant, HostInfo, Machine, Store, Who};
use crate::proto::{AgentMsg, Preview, PreviewShot, SessionMsg};
use crate::update::UpdateInfo;
use crate::util::{now_secs, random_token, sha256_hex};
use anyhow::Result;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{mpsc, oneshot, watch};

const ADMIN_HTML: &str = include_str!("web/admin.html");
const VIEWER_HTML: &str = include_str!("web/viewer.html");
const INSTALL_HTML: &str = include_str!("web/install.html");
const MESSAGE_HTML: &str = include_str!("web/message.html");
const APP_CSS: &str = include_str!("web/app.css");
const ADMIN_JS: &str = include_str!("web/admin.js");
const VIEWER_JS: &str = include_str!("web/viewer.js");

/// Files that may be downloaded from the releases folder.
const PUBLISHED_APPS: [&str; 3] = ["farlink-host", "farlink", crate::util::DEB_APP];

struct AgentConn {
    tx: mpsc::UnboundedSender<AgentMsg>,
    conn: String,
    version: String,
    /// Load, memory and disks from the latest heartbeat (memory only).
    stats: Option<crate::sysstat::SystemStats>,
}

struct Pending {
    machine_id: String,
    tx: oneshot::Sender<WebSocket>,
}

#[derive(Clone, Serialize)]
pub struct SessionView {
    pub id: String,
    pub machine_id: String,
    pub who: String,
    pub view_only: bool,
    pub started: u64,
    #[serde(skip)]
    pub grant: Option<String>,
    /// Person (user id) who opened it, if not the admin or a link.
    #[serde(skip)]
    pub user: Option<String>,
}

struct SessionEntry {
    view: SessionView,
    kill: watch::Sender<Option<String>>,
}

pub struct Relay {
    pub cfg: ServerConfig,
    state_path: PathBuf,
    store: Mutex<Store>,
    agents: Mutex<HashMap<String, AgentConn>>,
    pending: Mutex<HashMap<String, Pending>>,
    sessions: Mutex<HashMap<String, SessionEntry>>,
    sha_cache: Mutex<HashMap<PathBuf, (SystemTime, u64, String)>>,
    update_status: Mutex<crate::server_update::UpdateStatus>,
    /// Wakes the update loop for a check right now.
    pub update_now: tokio::sync::Notify,
    /// Preview requests waiting for the host: req -> (machine, answer).
    previews: Mutex<HashMap<u32, (String, oneshot::Sender<Vec<PreviewShot>>)>>,
    preview_seq: AtomicU32,
    /// Latest preview per machine, shared by everyone looking at it.
    preview_cache: Mutex<HashMap<String, (Instant, Preview)>>,
    /// Updates the administrator asked for, per machine (not saved).
    forced_updates: Mutex<HashMap<String, ForcedUpdate>>,
}

/// An "Update" the administrator pressed for one computer.
#[derive(Clone, Debug)]
struct ForcedUpdate {
    /// Version the helper was told to install.
    version: String,
    asked: Instant,
    /// What went wrong, as reported by the helper.
    error: Option<String>,
}

/// After this long without the helper coming back, "updating" is no longer shown.
const FORCED_UPDATE_WAIT: Duration = Duration::from_secs(180);

/// Previews younger than this are served from the cache.
const PREVIEW_CACHE: Duration = Duration::from_secs(3);
/// How long the relay waits for a host's preview pictures.
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(8);

pub type Shared = Arc<Relay>;

const UPDATE_STATUS: &str = "update-status.json";

/// Load the saved state or create it. Returns the new admin password when
/// the server starts for the very first time.
pub fn load_or_init_store(path: &std::path::Path) -> Result<(Store, Option<String>)> {
    if path.exists() {
        let store: Store = load_json(path);
        if !store.admin_hash.is_empty() {
            return Ok((store, None));
        }
    }
    let password = random_token(6);
    let store = Store::new(&password);
    save_json(path, &store)?;
    Ok((store, Some(password)))
}

impl Relay {
    pub fn new(cfg: ServerConfig) -> Result<(Shared, Option<String>)> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let state_path = cfg.data_dir.join("state.json");
        let (store, first_password) = load_or_init_store(&state_path)?;
        let source = cfg.update_from_github.clone().filter(|r| !r.trim().is_empty());
        let update_status = crate::server_update::UpdateStatus::load(&cfg.data_dir.join(UPDATE_STATUS), source, now_secs());
        if let Err(e) = save_json(&cfg.data_dir.join(UPDATE_STATUS), &update_status) {
            log::warn!("saving update status: {e}");
        }
        let relay = Relay {
            cfg,
            state_path,
            store: Mutex::new(store),
            agents: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            sha_cache: Mutex::new(HashMap::new()),
            update_status: Mutex::new(update_status),
            update_now: tokio::sync::Notify::new(),
            previews: Mutex::new(HashMap::new()),
            preview_seq: AtomicU32::new(1),
            preview_cache: Mutex::new(HashMap::new()),
            forced_updates: Mutex::new(HashMap::new()),
        };
        Ok((Arc::new(relay), first_password))
    }

    /// Change the store and save it to disk.
    pub fn with_store<R>(&self, f: impl FnOnce(&mut Store) -> R) -> R {
        let mut store = self.store.lock().unwrap();
        let r = f(&mut store);
        if let Err(e) = save_json(&self.state_path, &*store) {
            log::error!("saving state: {e}");
        }
        r
    }

    pub fn update_status(&self) -> crate::server_update::UpdateStatus {
        self.update_status.lock().unwrap().clone()
    }

    /// Change the update status and save it.
    pub fn set_update_status(&self, f: impl FnOnce(&mut crate::server_update::UpdateStatus)) {
        let mut s = self.update_status.lock().unwrap();
        f(&mut s);
        if let Err(e) = save_json(&self.cfg.data_dir.join(UPDATE_STATUS), &*s) {
            log::warn!("saving update status: {e}");
        }
    }

    fn read_store<R>(&self, f: impl FnOnce(&Store) -> R) -> R {
        f(&self.store.lock().unwrap())
    }

    pub fn enroll_token(&self) -> String {
        self.read_store(|s| s.enroll_token.clone())
    }

    pub fn install_url(&self) -> String {
        format!("{}/install/{}", self.cfg.public_url.trim_end_matches('/'), self.enroll_token())
    }

    pub fn grant_url(&self, token: &str) -> String {
        format!("{}/g/{}", self.cfg.public_url.trim_end_matches('/'), token)
    }

    pub fn is_online(&self, machine_id: &str) -> bool {
        self.agents.lock().unwrap().contains_key(machine_id)
    }

    /// Version published in the releases folder (`version.txt`).
    pub fn release_version(&self) -> Option<String> {
        std::fs::read_to_string(self.cfg.releases_dir.join("version.txt"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| crate::version::Version::parse(s).is_some())
    }

    pub fn release_info(&self, app: &str, target: &str) -> Option<UpdateInfo> {
        if !PUBLISHED_APPS.contains(&app) || !safe_segment(target) {
            return None;
        }
        let version = self.release_version()?;
        let file = crate::util::published_file(app, target)?;
        let path = self.cfg.releases_dir.join(target).join(&file);
        let meta = std::fs::metadata(&path).ok()?;
        let mtime = meta.modified().ok()?;
        let mut cache = self.sha_cache.lock().unwrap();
        let sha = match cache.get(&path) {
            Some((t, len, sha)) if *t == mtime && *len == meta.len() => sha.clone(),
            _ => {
                let sha = sha256_hex(&std::fs::read(&path).ok()?);
                cache.insert(path.clone(), (mtime, meta.len(), sha.clone()));
                sha
            }
        };
        Some(UpdateInfo { version, sha256: sha, size: meta.len(), path: format!("/download/{target}/{file}") })
    }

    /// Tell every connected host that is older than the published version.
    pub fn push_updates(&self) -> usize {
        let Some(v) = self.release_version() else { return 0 };
        let agents = self.agents.lock().unwrap();
        let mut n = 0;
        for a in agents.values() {
            if crate::version::is_newer(&v, &a.version) && a.tx.send(AgentMsg::UpdateAvailable { version: v.clone(), force: false }).is_ok() {
                n += 1;
            }
        }
        n
    }

    /// Tell one online helper to install the published version now, even if
    /// it would not do so on its own. Returns that version.
    pub fn force_update(&self, machine_id: &str) -> Result<String, String> {
        let v = self.release_version().ok_or("No version is published for computers yet")?;
        let agents = self.agents.lock().unwrap();
        let a = agents.get(machine_id).ok_or("This computer is offline")?;
        if a.version == v {
            return Err(format!("Already running {v}"));
        }
        a.tx.send(AgentMsg::UpdateAvailable { version: v.clone(), force: true }).map_err(|_| "This computer is offline")?;
        let asked = ForcedUpdate { version: v.clone(), asked: Instant::now(), error: None };
        self.forced_updates.lock().unwrap().insert(machine_id.to_string(), asked);
        log::info!("administrator asked {machine_id} to update {} -> {v}", a.version);
        Ok(v)
    }

    /// [`Relay::force_update`] for every online helper older than the
    /// published version (except those already updating).
    pub fn force_update_all(&self) -> usize {
        let Some(v) = self.release_version() else { return 0 };
        let older: Vec<String> = {
            let agents = self.agents.lock().unwrap();
            agents.iter().filter(|(_, a)| crate::version::is_newer(&v, &a.version)).map(|(id, _)| id.clone()).collect()
        };
        let older: Vec<String> = older.into_iter().filter(|id| !self.forced_update_state(id, true).0).collect();
        older.iter().filter(|id| self.force_update(id).is_ok()).count()
    }

    /// (`updating`, last error) of an update the administrator asked for.
    fn forced_update_state(&self, machine_id: &str, online: bool) -> (bool, Option<String>) {
        match self.forced_updates.lock().unwrap().get(machine_id) {
            Some(f) => (online && f.error.is_none() && f.asked.elapsed() < FORCED_UPDATE_WAIT, f.error.clone()),
            None => (false, None),
        }
    }

    fn update_failed(&self, machine_id: &str, version: String, error: String) {
        log::warn!("{machine_id} could not update to {version}: {error}");
        let mut forced = self.forced_updates.lock().unwrap();
        let f = forced.entry(machine_id.to_string()).or_insert_with(|| ForcedUpdate { version, asked: Instant::now(), error: None });
        f.error = Some(error);
    }

    /// End sessions matching `pred`, telling the viewer why.
    pub fn kill_sessions(&self, reason: &str, pred: impl Fn(&SessionView) -> bool) -> usize {
        let sessions = self.sessions.lock().unwrap();
        let mut n = 0;
        for s in sessions.values().filter(|s| pred(&s.view)) {
            let _ = s.kill.send(Some(reason.to_string()));
            n += 1;
        }
        n
    }

    pub fn session_list(&self) -> Vec<SessionView> {
        let mut v: Vec<SessionView> = self.sessions.lock().unwrap().values().map(|s| s.view.clone()).collect();
        v.sort_by_key(|s| s.started);
        v
    }

    /// End sessions whose temporary link expired.
    pub fn expire_sessions(&self) -> usize {
        let now = now_secs();
        let expired: Vec<String> = self
            .session_list()
            .into_iter()
            .filter_map(|s| s.grant)
            .filter(|g| self.read_store(|st| st.active_grant(g, now).is_none()))
            .collect();
        if expired.is_empty() {
            return 0;
        }
        self.kill_sessions("This temporary link has expired", |s| s.grant.as_ref().is_some_and(|g| expired.contains(g)))
    }

    /// Ask the host for a session socket and wait for it.
    async fn open_session(
        self: &Arc<Self>,
        machine_id: &str,
        view_only: bool,
        grant: Option<String>,
        user: Option<String>,
        who: String,
    ) -> std::result::Result<(String, WebSocket, watch::Receiver<Option<String>>), String> {
        let tx = {
            let agents = self.agents.lock().unwrap();
            agents.get(machine_id).map(|a| a.tx.clone()).ok_or("This computer is offline right now")?
        };
        let session_id = random_token(12);
        let (otx, orx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(session_id.clone(), Pending { machine_id: machine_id.to_string(), tx: otx });
        if tx.send(AgentMsg::OpenSession { session_id: session_id.clone(), view_only }).is_err() {
            self.pending.lock().unwrap().remove(&session_id);
            return Err("This computer just went offline".into());
        }
        let host = match tokio::time::timeout(Duration::from_secs(20), orx).await {
            Ok(Ok(ws)) => ws,
            _ => {
                self.pending.lock().unwrap().remove(&session_id);
                return Err("The computer did not answer in time".into());
            }
        };
        let (kill_tx, kill_rx) = watch::channel(None);
        let view =
            SessionView { id: session_id.clone(), machine_id: machine_id.into(), who, view_only, started: now_secs(), grant, user };
        self.sessions.lock().unwrap().insert(session_id.clone(), SessionEntry { view, kill: kill_tx });
        Ok((session_id, host, kill_rx))
    }
}

impl Relay {
    /// Small pictures of a machine's screens (from the cache or asked from the host).
    pub async fn preview(&self, machine_id: &str) -> std::result::Result<Preview, (StatusCode, &'static str)> {
        if let Some((at, p)) = self.preview_cache.lock().unwrap().get(machine_id)
            && at.elapsed() < PREVIEW_CACHE
        {
            return Ok(p.clone());
        }
        let tx = self.agents.lock().unwrap().get(machine_id).map(|a| a.tx.clone());
        let Some(tx) = tx else { return Err((StatusCode::SERVICE_UNAVAILABLE, "This computer is offline right now")) };
        let req = self.preview_seq.fetch_add(1, Ordering::Relaxed);
        let (otx, orx) = oneshot::channel();
        self.previews.lock().unwrap().insert(req, (machine_id.to_string(), otx));
        if tx.send(AgentMsg::PreviewRequest { req }).is_err() {
            self.previews.lock().unwrap().remove(&req);
            return Err((StatusCode::SERVICE_UNAVAILABLE, "This computer just went offline"));
        }
        let shots = match tokio::time::timeout(PREVIEW_TIMEOUT, orx).await {
            Ok(Ok(shots)) => shots,
            _ => {
                self.previews.lock().unwrap().remove(&req);
                return Err((StatusCode::GATEWAY_TIMEOUT, "The computer did not send a preview"));
            }
        };
        let p = Preview { taken: now_secs(), shots };
        self.preview_cache.lock().unwrap().insert(machine_id.to_string(), (Instant::now(), p.clone()));
        Ok(p)
    }

    /// A host answered a preview request (only its own requests count).
    fn preview_arrived(&self, machine_id: &str, req: u32, shots: Vec<PreviewShot>) {
        let mut pending = self.previews.lock().unwrap();
        if pending.get(&req).is_some_and(|(m, _)| m == machine_id)
            && let Some((_, tx)) = pending.remove(&req)
        {
            let _ = tx.send(shots);
        }
    }
}

fn safe_segment(s: &str) -> bool {
    !s.is_empty() && s.len() < 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') && !s.contains("..")
}

pub fn router(relay: Shared) -> Router {
    Router::new()
        .route("/", get(admin_page))
        .route("/assets/app.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS) }))
        .route("/assets/admin.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], ADMIN_JS) }))
        .route("/assets/viewer.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], VIEWER_JS) }))
        .route("/view/{machine}", get(admin_viewer_page))
        .route("/g/{token}", get(guest_page))
        // Never reveal an install link to someone who just guesses the address.
        .route("/install", get(|| async { message_page("Install link needed", "Ask the administrator for the install link of this computer.") }))
        .route("/app", get(app_download_page))
        .route("/install/{token}", get(install_page))
        .route("/install/{token}/linux.sh", get(install_linux))
        .route("/install/{token}/windows.ps1", get(install_windows))
        .route("/install/{token}/install-windows.cmd", get(install_windows_cmd))
        .route("/download/{target}/{file}", get(download))
        .route("/api/update", get(api_update))
        .route("/api/login", post(api_login))
        .route("/api/logout", post(api_logout))
        .route("/api/overview", get(api_overview))
        .route("/api/machines/{id}/rename", post(api_rename))
        .route("/api/machines/{id}/notes", post(api_notes))
        .route("/api/machines/{id}", delete(api_remove_machine))
        .route("/api/machines/{id}/owner", post(api_set_owner))
        .route("/api/machines/{id}/preview", get(api_preview))
        .route("/api/machines/{id}/update", post(api_update_machine))
        .route("/api/machines/update-all", post(api_update_all))
        .route("/api/users", post(api_create_user))
        .route("/api/users/{id}", delete(api_delete_user))
        .route("/api/users/{id}/password", post(api_user_password))
        .route("/api/grants", post(api_create_grant))
        .route("/api/grants/{token}", delete(api_revoke))
        .route("/api/sessions/{id}/end", post(api_end_session))
        .route("/api/enroll/rotate", post(api_rotate))
        .route("/api/server/check-update", post(api_check_update))
        .route("/api/guest/{token}", get(api_guest_info))
        .route("/ws/agent", get(ws_agent))
        .route("/ws/host-session/{session}", get(ws_host_session))
        .route("/ws/view/{machine}", get(ws_view))
        .route("/ws/guest/{token}", get(ws_guest))
        .with_state(relay)
}

/// Background jobs: expire link sessions, prune old links, push updates.
pub fn spawn_housekeeping(relay: Shared) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        let mut last_release = relay.release_version();
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            n += 1;
            relay.expire_sessions();
            if n.is_multiple_of(360) {
                relay.with_store(|s| s.prune(now_secs()));
            }
            let release = relay.release_version();
            if release != last_release {
                last_release = release;
                let pushed = relay.push_updates();
                log::info!("new release published, {pushed} computers told to update");
            }
        }
    });
}

/// Run the server on an already-bound listener (plain TCP, or TLS from `crate::tls`).
pub async fn serve<L>(relay: Shared, listener: L) -> Result<()>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
{
    spawn_housekeeping(relay.clone());
    if let Some(repo) = relay.cfg.update_from_github.clone().filter(|r| !r.trim().is_empty()) {
        crate::server_update::spawn(relay.clone(), repo.trim().to_string());
    }
    axum::serve(listener, router(relay)).await?;
    Ok(())
}

// ------------------------------------------------------------------ helpers

fn bearer(headers: &HeaderMap) -> String {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string()
}

fn is_admin(relay: &Relay, headers: &HeaderMap) -> bool {
    let token = bearer(headers);
    relay.read_store(|s| s.check_admin(&token, now_secs()))
}

fn who(relay: &Relay, headers: &HeaderMap) -> Option<Who> {
    let token = bearer(headers);
    relay.read_store(|s| s.who(&token, now_secs()))
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": "Please sign in again"}))).into_response()
}

fn bad(msg: impl ToString) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": msg.to_string()}))).into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn message_page(title: &str, text: &str) -> Html<String> {
    Html(MESSAGE_HTML.replace("{{TITLE}}", &html_escape(title)).replace("{{TEXT}}", &html_escape(text)))
}

/// Viewer page configuration (JSON embedded in the page).
fn viewer_page(cfg: serde_json::Value) -> Html<String> {
    let json = serde_json::to_string(&cfg).unwrap().replace('<', "\\u003c");
    Html(VIEWER_HTML.replace("{{CONFIG}}", &json))
}

// ------------------------------------------------------------------ pages

/// Download page for the desktop app (no install token needed).
async fn app_download_page() -> Html<String> {
    Html(
        MESSAGE_HTML
            .replace("{{TITLE}}", "Farlink desktop app")
            .replace(
                "{{TEXT}}",
                "</p><p><a class=\"btn primary\" href=\"/download/x86_64-pc-windows-gnu/farlink.exe\">Farlink for Windows</a> \
                 <a class=\"btn\" href=\"/download/x86_64-unknown-linux-gnu/farlink\">Farlink for Linux</a></p>\
                 <p class=\"muted\">Sign in with the server address and your sign-in name and password. You can also use this web page instead.",
            ),
    )
}

async fn admin_page() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

async fn admin_viewer_page(State(r): State<Shared>, Path(machine): Path<String>) -> Response {
    let name = r.read_store(|s| s.machines.get(&machine).map(|m| m.name.clone()));
    match name {
        Some(name) => viewer_page(json!({"mode": "admin", "machine": machine, "name": name})).into_response(),
        None => message_page("Not found", "This computer is not registered on this server.").into_response(),
    }
}

async fn guest_page(State(r): State<Shared>, Path(token): Path<String>) -> Response {
    let now = now_secs();
    let info = r.read_store(|s| {
        s.active_grant(&token, now).map(|g| {
            let name = s.machines.get(&g.machine_id).map(|m| m.name.clone()).unwrap_or_default();
            (name, g.view_only, g.expires, g.label.clone())
        })
    });
    match info {
        Some((name, view_only, expires, label)) => viewer_page(json!({
            "mode": "guest", "token": token, "name": name, "view_only": view_only,
            "expires": expires, "label": label,
        }))
        .into_response(),
        None => message_page("Link not available", "This link has expired or the access was withdrawn. Ask the administrator for a new one.")
            .into_response(),
    }
}

/// The general install link or a person's own install link.
fn valid_enroll(r: &Relay, token: &str) -> bool {
    r.read_store(|s| s.enroll_owner(token).is_some())
}

async fn install_page(State(r): State<Shared>, Path(token): Path<String>) -> Response {
    let Some(owner) = r.read_store(|s| s.enroll_owner(&token).map(|o| o.and_then(|id| s.users.get(&id).map(|u| u.name.clone()))))
    else {
        return message_page("Install link not valid", "Ask the administrator for a fresh install link.").into_response();
    };
    let base = r.cfg.public_url.trim_end_matches('/').to_string();
    let for_whom = match owner {
        Some(name) => format!(
            "This computer will be <b>{}'s</b> computer: only {} and the administrator will see and control it.",
            html_escape(&name),
            html_escape(&name)
        ),
        None => "This computer will be visible to the administrator.".to_string(),
    };
    let page = INSTALL_HTML
        .replace("{{FOR}}", &for_whom)
        .replace("{{SERVER}}", &html_escape(&base))
        .replace("{{TOKEN}}", &html_escape(&token))
        .replace("{{VERSION}}", &html_escape(&r.release_version().unwrap_or_else(|| crate::VERSION.into())));
    Html(page).into_response()
}

pub fn linux_script(server: &str, token: &str) -> String {
    format!(
        r#"#!/bin/sh
# Farlink helper installer for Ubuntu (X11 desktop).
set -e
SERVER='{server}'
TOKEN='{token}'
TARGET=x86_64-unknown-linux-gnu
case "$(uname -m)" in aarch64|arm64) TARGET=aarch64-unknown-linux-gnu;; esac
TMP=$(mktemp -d)
echo "Downloading the Farlink helper..."
curl -fsSL "$SERVER/download/$TARGET/farlink-host" -o "$TMP/farlink-host"
chmod +x "$TMP/farlink-host"
USER_NAME="${{SUDO_USER:-$(id -un)}}"
if [ "$(id -u)" -ne 0 ]; then SUDO=sudo; else SUDO=; fi
$SUDO "$TMP/farlink-host" install --server "$SERVER" --token "$TOKEN" --user "$USER_NAME"
rm -rf "$TMP"
echo "Done."
"#
    )
}

pub fn windows_script(server: &str, token: &str) -> String {
    format!(
        r#"# Farlink helper installer for Windows 10/11 and Windows Server 2016+.
$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$server = '{server}'
$token = '{token}'
$me = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {{
  $cmd = "[Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; iex (irm '$server/install/$token/windows.ps1')"
  Start-Process powershell.exe -Verb RunAs -ArgumentList "-NoProfile -ExecutionPolicy Bypass -Command `"$cmd`""
  return
}}
function Fail($msg) {{
  Write-Host ''
  Write-Host $msg -ForegroundColor Red
  Write-Host ''
  Read-Host 'Install failed. Press Enter to close'
  exit 1
}}
# Windows Defender often flags new, unsigned remote-control programs. Allow
# the Farlink folders (and a private download folder) before downloading.
$dl = Join-Path $env:TEMP 'farlink-setup'
$allowed = @("$env:ProgramFiles\Farlink", "$env:ProgramData\Farlink", $dl)
try {{ Add-MpPreference -ExclusionPath $allowed -ErrorAction Stop }} catch {{ Write-Host "Note: could not add Windows Defender exclusions ($($_.Exception.Message))." }}
New-Item -ItemType Directory -Force $dl | Out-Null
$tmp = Join-Path $dl 'farlink-host-setup.exe'
Write-Host 'Downloading the Farlink helper...'
try {{
  Invoke-WebRequest -UseBasicParsing -Uri "$server/download/x86_64-pc-windows-gnu/farlink-host.exe" -OutFile $tmp
}} catch {{
  Fail "Could not download the Farlink helper from $server : $($_.Exception.Message)"
}}
Start-Sleep -Seconds 2
if (-not (Test-Path $tmp)) {{
  Fail ("The downloaded helper disappeared - most likely your antivirus (Windows Defender) removed it.`n" +
        "Open Windows Security > Virus & threat protection > Protection history, allow 'farlink-host', then run this install link again.")
}}
& $tmp install --server $server --token $token
$code = $LASTEXITCODE
Remove-Item $tmp -ErrorAction SilentlyContinue
try {{ Remove-MpPreference -ExclusionPath $dl -ErrorAction Stop }} catch {{ }}
if ($code -ne 0) {{
  Fail "The Farlink installer failed (exit code $code). See the messages above."
}}
if (-not (Get-Service FarlinkHost -ErrorAction SilentlyContinue)) {{
  Fail ("The Farlink service was not installed - most likely your antivirus removed the program.`n" +
        "Check Windows Security > Protection history, allow 'farlink-host', then run this install link again.")
}}
Write-Host 'Done. This computer now appears in your Farlink list.' -ForegroundColor Green
Start-Sleep -Seconds 5
"#
    )
}

pub fn windows_cmd(server: &str, token: &str) -> String {
    format!(
        "@echo off\r\necho Installing Farlink helper...\r\npowershell -NoProfile -ExecutionPolicy Bypass -Command \"[Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; iex (irm '{server}/install/{token}/windows.ps1')\"\r\npause\r\n"
    )
}

async fn install_linux(State(r): State<Shared>, Path(token): Path<String>) -> Response {
    if !valid_enroll(&r, &token) {
        return (StatusCode::NOT_FOUND, "echo 'This install link is not valid any more'; exit 1\n").into_response();
    }
    ([(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")], linux_script(r.cfg.public_url.trim_end_matches('/'), &token))
        .into_response()
}

async fn install_windows(State(r): State<Shared>, Path(token): Path<String>) -> Response {
    if !valid_enroll(&r, &token) {
        return (StatusCode::NOT_FOUND, "Write-Host 'This install link is not valid any more'\n").into_response();
    }
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], windows_script(r.cfg.public_url.trim_end_matches('/'), &token))
        .into_response()
}

async fn install_windows_cmd(State(r): State<Shared>, Path(token): Path<String>) -> Response {
    if !valid_enroll(&r, &token) {
        return (StatusCode::NOT_FOUND, "install link not valid").into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"install-farlink.cmd\"".to_string()),
        ],
        windows_cmd(r.cfg.public_url.trim_end_matches('/'), &token),
    )
        .into_response()
}

async fn download(State(r): State<Shared>, Path((target, file)): Path<(String, String)>) -> Response {
    let allowed = PUBLISHED_APPS.iter().any(|a| crate::util::published_file(a, &target).as_deref() == Some(file.as_str()));
    if !allowed || !safe_segment(&target) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match tokio::fs::read(r.cfg.releases_dir.join(&target).join(&file)).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{file}\"")),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "This program has not been published on the server yet").into_response(),
    }
}

// ------------------------------------------------------------------ API

async fn api_update(State(r): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let app = q.get("app").map(String::as_str).unwrap_or("farlink-host");
    let target = q.get("target").map(String::as_str).unwrap_or("");
    match r.release_info(app, target) {
        Some(info) => Json(info).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "nothing published"}))).into_response(),
    }
}

#[derive(Deserialize)]
struct LoginReq {
    /// Empty (or "admin") for the administrator.
    #[serde(default)]
    username: String,
    password: String,
}

async fn api_login(State(r): State<Shared>, Json(req): Json<LoginReq>) -> Response {
    let token = r.with_store(|s| s.login_as(&req.username, &req.password, now_secs()));
    match token {
        Some((t, who, name)) => {
            let role = if who == Who::Admin { "admin" } else { "user" };
            Json(json!({"token": t, "role": role, "name": name})).into_response()
        }
        None => {
            tokio::time::sleep(Duration::from_millis(800)).await; // slow down guessing
            (StatusCode::UNAUTHORIZED, Json(json!({"error": "Wrong password"}))).into_response()
        }
    }
}

async fn api_logout(State(r): State<Shared>, headers: HeaderMap) -> Response {
    let t = bearer(&headers);
    r.with_store(|s| s.logout(&t));
    Json(json!({"ok": true})).into_response()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MachineView {
    #[serde(flatten)]
    pub machine: MachineSummary,
    pub online: bool,
    pub sessions: usize,
    pub up_to_date: bool,
    /// Name of the person this computer belongs to.
    #[serde(default)]
    pub owner_name: Option<String>,
    /// The administrator pressed "Update" and the helper has not come back yet.
    #[serde(default)]
    pub updating: bool,
    /// Why the last update of this helper failed.
    #[serde(default)]
    pub update_error: Option<String>,
    /// CPU, memory and disks while online (helpers from 0.1.9 on).
    #[serde(default)]
    pub stats: Option<crate::sysstat::SystemStats>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MachineSummary {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub os: String,
    pub version: String,
    pub monitors: Vec<crate::proto::MonitorInfo>,
    pub last_seen: u64,
    #[serde(default)]
    pub owner: Option<String>,
    /// Administrator's notes (empty for people).
    #[serde(default)]
    pub notes: String,
}

impl From<&Machine> for MachineSummary {
    fn from(m: &Machine) -> Self {
        MachineSummary {
            id: m.id.clone(),
            name: m.name.clone(),
            hostname: m.hostname.clone(),
            os: m.os.clone(),
            version: m.version.clone(),
            monitors: m.monitors.clone(),
            last_seen: m.last_seen,
            owner: m.owner.clone(),
            notes: m.notes.clone(),
        }
    }
}

/// A person, as the admin sees them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UserView {
    pub id: String,
    pub name: String,
    pub login: String,
    /// Their personal install link.
    pub install_url: String,
    /// Ids of their computers.
    pub machines: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GrantView {
    #[serde(flatten)]
    pub grant: Grant,
    pub url: String,
    pub machine_name: String,
    pub status: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Overview {
    pub machines: Vec<MachineView>,
    pub grants: Vec<GrantView>,
    pub sessions: Vec<serde_json::Value>,
    pub install_url: String,
    pub server_version: String,
    pub release_version: Option<String>,
    pub now: u64,
    /// "admin" or "user".
    #[serde(default = "admin_role")]
    pub role: String,
    /// Who is signed in.
    #[serde(default)]
    pub user_name: String,
    /// People (admin only).
    #[serde(default)]
    pub users: Vec<UserView>,
    /// The server's own updates (what went wrong: admin only).
    #[serde(default)]
    pub server_update: crate::server_update::UpdateStatus,
    /// The command that makes this relay faster, when its Linux still slows
    /// connections down after every pause (admin only; see
    /// `install::TCP_WARM_SYSCTL`).
    #[serde(default)]
    pub slow_after_pause_fix: Option<String>,
}

fn admin_role() -> String {
    "admin".into()
}

/// What the signed-in person may see: everything for the admin, only their
/// own computers (and nothing else) for a person.
pub fn overview_for(r: &Relay, who: &Who) -> Overview {
    let mut o = overview(r);
    if let Who::User(uid) = who {
        o.machines.retain(|m| m.machine.owner.as_deref() == Some(uid.as_str()));
        // Notes are the administrator's own.
        o.machines.iter_mut().for_each(|m| m.machine.notes.clear());
        let mine: Vec<String> = o.machines.iter().map(|m| m.machine.id.clone()).collect();
        o.sessions.retain(|s| s["machine_id"].as_str().is_some_and(|id| mine.iter().any(|m| m == id)));
        o.grants.clear();
        o.users.clear();
        o.install_url.clear();
        o.slow_after_pause_fix = None;
        o.role = "user".into();
        if !o.server_update.last_check_ok && o.server_update.last_check.is_some() {
            o.server_update.last_check_result = "failed".into();
        }
        o.user_name = r.read_store(|s| s.display_name(who));
    }
    o
}

pub fn overview(r: &Relay) -> Overview {
    let now = now_secs();
    let release = r.release_version();
    let sessions = r.session_list();
    let base = r.cfg.public_url.trim_end_matches('/').to_string();
    let (machines, grants, users) = r.read_store(|s| {
        let mut machines: Vec<MachineView> = s
            .machines
            .values()
            .map(|m| {
                let online = r.is_online(&m.id);
                let (updating, update_error) = r.forced_update_state(&m.id, online);
                MachineView {
                    machine: m.into(),
                    online,
                    sessions: sessions.iter().filter(|x| x.machine_id == m.id).count(),
                    up_to_date: release.as_ref().is_none_or(|v| !crate::version::is_newer(v, &m.version)),
                    owner_name: m.owner.as_ref().and_then(|o| s.users.get(o)).map(|u| u.name.clone()),
                    updating,
                    update_error,
                    stats: if online { r.agents.lock().unwrap().get(&m.id).and_then(|a| a.stats.clone()) } else { None },
                }
            })
            .collect();
        let mut users: Vec<UserView> = s
            .users
            .values()
            .map(|u| UserView {
                id: u.id.clone(),
                name: u.name.clone(),
                login: u.login.clone(),
                install_url: format!("{base}/install/{}", u.enroll_token),
                machines: s.machines.values().filter(|m| m.owner.as_deref() == Some(u.id.as_str())).map(|m| m.id.clone()).collect(),
            })
            .collect();
        users.sort_by_key(|u| u.name.to_lowercase());
        machines.sort_by(|a, b| b.online.cmp(&a.online).then(a.machine.name.to_lowercase().cmp(&b.machine.name.to_lowercase())));
        let mut grants: Vec<GrantView> = s
            .grants
            .values()
            .map(|g| GrantView {
                grant: g.clone(),
                url: r.grant_url(&g.token),
                machine_name: s.machines.get(&g.machine_id).map(|m| m.name.clone()).unwrap_or_else(|| "(removed)".into()),
                status: g.status(now).into(),
            })
            .collect();
        grants.sort_by(|a, b| b.grant.created.cmp(&a.grant.created));
        (machines, grants, users)
    });
    Overview {
        machines,
        grants,
        sessions: sessions.iter().map(|s| serde_json::to_value(s).unwrap()).collect(),
        install_url: r.install_url(),
        server_version: crate::VERSION.into(),
        release_version: release,
        now,
        role: "admin".into(),
        user_name: "Administrator".into(),
        users,
        server_update: r.update_status(),
        slow_after_pause_fix: (crate::install::tcp_stays_warm() == Some(false)).then(|| crate::install::TCP_WARM_FIX.to_string()),
    }
}

async fn api_overview(State(r): State<Shared>, headers: HeaderMap) -> Response {
    match who(&r, &headers) {
        Some(w) => Json(overview_for(&r, &w)).into_response(),
        None => unauthorized(),
    }
}

#[derive(Deserialize)]
struct NewUserReq {
    name: String,
    login: String,
    password: String,
}

async fn api_create_user(State(r): State<Shared>, headers: HeaderMap, Json(req): Json<NewUserReq>) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    match r.with_store(|s| s.create_user(&req.name, &req.login, &req.password, now_secs())) {
        Ok(u) => {
            let url = format!("{}/install/{}", r.cfg.public_url.trim_end_matches('/'), u.enroll_token);
            Json(json!({"id": u.id, "login": u.login, "install_url": url})).into_response()
        }
        Err(e) => bad(e),
    }
}

async fn api_delete_user(State(r): State<Shared>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    match r.with_store(|s| s.delete_user(&id)) {
        Ok(()) => {
            r.kill_sessions("Your access was removed by the administrator", |s| s.user.as_deref() == Some(id.as_str()));
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct PasswordReq {
    password: String,
}

async fn api_user_password(State(r): State<Shared>, headers: HeaderMap, Path(id): Path<String>, Json(req): Json<PasswordReq>) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    match r.with_store(|s| s.set_user_password(&id, &req.password)) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct OwnerReq {
    #[serde(default)]
    user_id: Option<String>,
}

async fn api_set_owner(State(r): State<Shared>, headers: HeaderMap, Path(id): Path<String>, Json(req): Json<OwnerReq>) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    let owner = req.user_id.filter(|u| !u.is_empty());
    match r.with_store(|s| s.assign(&id, owner.as_deref())) {
        Ok(()) => {
            // Someone who just lost this computer is disconnected from it.
            r.kill_sessions("This computer was moved to someone else", |s| {
                s.machine_id == id && s.user.is_some() && s.user != owner
            });
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct RenameReq {
    name: String,
}

async fn api_rename(State(r): State<Shared>, headers: HeaderMap, Path(id): Path<String>, Json(req): Json<RenameReq>) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    match r.with_store(|s| s.rename_machine(&id, &req.name)) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct NotesReq {
    notes: String,
}

async fn api_notes(State(r): State<Shared>, headers: HeaderMap, Path(id): Path<String>, Json(req): Json<NotesReq>) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    match r.with_store(|s| s.set_notes(&id, &req.notes)) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => bad(e),
    }
}

/// Small pictures of a computer's screens (admin, or the person it belongs to).
async fn api_preview(State(r): State<Shared>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let Some(who) = who(&r, &headers) else { return unauthorized() };
    if !r.read_store(|s| s.can_access(&who, &id)) {
        return (StatusCode::FORBIDDEN, "not your computer").into_response();
    }
    match r.preview(&id).await {
        Ok(p) => Json(p).into_response(),
        Err((code, msg)) => (code, msg).into_response(),
    }
}

/// Make a computer's helper install the published version now (administrator).
async fn api_update_machine(State(r): State<Shared>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    match r.force_update(&id) {
        Ok(v) => Json(json!({"ok": true, "version": v})).into_response(),
        Err(e) => bad(e),
    }
}

/// Update every online helper that is older than the published version.
async fn api_update_all(State(r): State<Shared>, headers: HeaderMap) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    if r.release_version().is_none() {
        return bad("No version is published for computers yet");
    }
    Json(json!({"ok": true, "updating": r.force_update_all()})).into_response()
}

async fn api_remove_machine(State(r): State<Shared>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    match r.with_store(|s| s.remove_machine(&id)) {
        Ok(_) => {
            r.kill_sessions("This computer was removed by the administrator", |s| s.machine_id == id);
            if let Some(a) = r.agents.lock().unwrap().remove(&id) {
                let _ = a.tx.send(AgentMsg::Rejected { reason: "removed by administrator".into() });
            }
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
pub struct GrantReq {
    pub machine_id: String,
    pub minutes: u64,
    pub view_only: bool,
    #[serde(default)]
    pub label: String,
}

async fn api_create_grant(State(r): State<Shared>, headers: HeaderMap, Json(req): Json<GrantReq>) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    match r.with_store(|s| s.create_grant(&req.machine_id, req.minutes, req.view_only, &req.label, now_secs())) {
        Ok(g) => Json(json!({"token": g.token, "url": r.grant_url(&g.token), "expires": g.expires})).into_response(),
        Err(e) => bad(e),
    }
}

async fn api_revoke(State(r): State<Shared>, headers: HeaderMap, Path(token): Path<String>) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    let changed = r.with_store(|s| s.revoke(&token));
    let ended = r.kill_sessions("The administrator withdrew this access", |s| s.grant.as_deref() == Some(token.as_str()));
    Json(json!({"ok": true, "withdrawn": changed, "sessions_ended": ended})).into_response()
}

async fn api_end_session(State(r): State<Shared>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    let n = r.kill_sessions("The administrator ended this session", |s| s.id == id);
    Json(json!({"ok": n > 0})).into_response()
}

/// Ask the server to look for a new release now (and install it).
async fn api_check_update(State(r): State<Shared>, headers: HeaderMap) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    if r.update_status().source.is_none() {
        return bad("Automatic updates are off. Set \"update_from_github\" in the server settings.");
    }
    r.set_update_status(|s| s.checking = true);
    r.update_now.notify_one();
    Json(json!({"ok": true})).into_response()
}

async fn api_rotate(State(r): State<Shared>, headers: HeaderMap) -> Response {
    if !is_admin(&r, &headers) {
        return unauthorized();
    }
    r.with_store(|s| s.rotate_enroll_token());
    Json(json!({"install_url": r.install_url()})).into_response()
}

/// Public info about a temporary link (used by the desktop app).
async fn api_guest_info(State(r): State<Shared>, Path(token): Path<String>) -> Response {
    let now = now_secs();
    let info = r.read_store(|s| {
        s.active_grant(&token, now).map(|g| {
            let m = s.machines.get(&g.machine_id);
            json!({
                "machine_id": g.machine_id,
                "name": m.map(|m| m.name.clone()).unwrap_or_default(),
                "os": m.map(|m| m.os.clone()).unwrap_or_default(),
                "view_only": g.view_only,
                "expires": g.expires,
                "label": g.label,
            })
        })
    });
    match info {
        Some(v) => Json(v).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "This link has expired or was withdrawn"}))).into_response(),
    }
}

// ------------------------------------------------------------------ WebSockets

async fn ws_agent(State(r): State<Shared>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(1 << 20).on_upgrade(move |socket| agent_loop(r, socket))
}

async fn agent_loop(r: Shared, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let first = match tokio::time::timeout(Duration::from_secs(15), stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<AgentMsg>(t.as_str()).ok(),
        _ => None,
    };
    let Some(AgentMsg::Register { enroll_token, machine_id, secret, hostname, os, version, target, monitors }) = first else {
        return;
    };
    let info = HostInfo { hostname, os, version: version.clone(), target, monitors };
    let result = r.with_store(|s| s.register(enroll_token.as_deref(), machine_id.as_deref(), secret.as_deref(), info, now_secs()));
    let (id, secret, name) = match result {
        Ok(v) => v,
        Err(e) => {
            let msg = AgentMsg::Rejected { reason: e.to_string() };
            let _ = sink.send(Message::Text(serde_json::to_string(&msg).unwrap().into())).await;
            return;
        }
    };
    let reply = AgentMsg::Registered { machine_id: id.clone(), secret, name: name.clone() };
    if sink.send(Message::Text(serde_json::to_string(&reply).unwrap().into())).await.is_err() {
        return;
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    let conn = random_token(8);
    if let Some(old) = r.agents.lock().unwrap().insert(id.clone(), AgentConn { tx: tx.clone(), conn: conn.clone(), version: version.clone(), stats: None }) {
        drop(old); // an older connection of the same machine; it will notice and stop
    }
    log::info!("'{name}' online ({version})");
    {
        // Back with the version it was asked to install: the update worked.
        let mut forced = r.forced_updates.lock().unwrap();
        if forced.get(&id).is_some_and(|f| f.version == version) {
            forced.remove(&id);
        }
    }
    if let Some(v) = r.release_version()
        && crate::version::is_newer(&v, &version)
    {
        let _ = tx.send(AgentMsg::UpdateAvailable { version: v, force: false });
    }
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(m) => {
                    let stop = matches!(m, AgentMsg::Rejected { .. });
                    if sink.send(Message::Text(serde_json::to_string(&m).unwrap().into())).await.is_err() || stop { break; }
                }
                None => break,
            },
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<AgentMsg>(t.as_str()) {
                    Ok(AgentMsg::Preview { req, shots }) => r.preview_arrived(&id, req, shots),
                    Ok(AgentMsg::UpdateFailed { version, error }) => r.update_failed(&id, version, error),
                    Ok(AgentMsg::Heartbeat { monitors, stats }) => {
                        if stats.is_some()
                            && let Some(a) = r.agents.lock().unwrap().get_mut(&id).filter(|a| a.conn == conn)
                        {
                            a.stats = stats;
                        }
                        let changed = r.read_store(|s| s.machines.get(&id).is_some_and(|m| m.monitors != monitors));
                        if changed {
                            r.with_store(|s| if let Some(m) = s.machines.get_mut(&id) { m.monitors = monitors; m.last_seen = now_secs(); });
                        } else if let Some(m) = r.store.lock().unwrap().machines.get_mut(&id) {
                            m.last_seen = now_secs();
                        }
                    }
                    _ => {}
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            }
        }
    }
    {
        let mut agents = r.agents.lock().unwrap();
        if agents.get(&id).is_some_and(|a| a.conn == conn) {
            agents.remove(&id);
        }
    }
    r.with_store(|s| {
        if let Some(m) = s.machines.get_mut(&id) {
            m.last_seen = now_secs();
        }
    });
    log::info!("'{name}' offline");
}

async fn ws_host_session(
    State(r): State<Shared>,
    Path(session): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let machine = q.get("machine").cloned().unwrap_or_default();
    let secret = q.get("secret").cloned().unwrap_or_default();
    let ok_secret = r.read_store(|s| {
        s.machines
            .get(&machine)
            .is_some_and(|m| crate::util::constant_time_eq(&m.secret_hash, &sha256_hex(secret.as_bytes())))
    });
    let pending = {
        let mut p = r.pending.lock().unwrap();
        match p.get(&session) {
            Some(x) if ok_secret && x.machine_id == machine => p.remove(&session),
            _ => None,
        }
    };
    let Some(pending) = pending else {
        return (StatusCode::FORBIDDEN, "unknown session").into_response();
    };
    ws.max_message_size(8 << 20).on_upgrade(move |socket| async move {
        let _ = pending.tx.send(socket);
    })
}

async fn ws_view(
    State(r): State<Shared>,
    Path(machine): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let token = q.get("token").cloned().unwrap_or_default();
    let Some(who) = r.read_store(|s| s.who(&token, now_secs())) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    // People only reach their own computers.
    let (allowed, name) = r.read_store(|s| (s.can_access(&who, &machine), s.display_name(&who)));
    if !allowed {
        return (StatusCode::FORBIDDEN, "not your computer").into_response();
    }
    let user = match who {
        Who::User(id) => Some(id),
        Who::Admin => None,
    };
    let view_only = q.get("view_only").is_some_and(|v| v == "1" || v == "true");
    ws.max_message_size(1 << 20).on_upgrade(move |socket| viewer_loop(r, socket, machine, view_only, None, user, name))
}

async fn ws_guest(State(r): State<Shared>, Path(token): Path<String>, ws: WebSocketUpgrade) -> Response {
    let now = now_secs();
    let grant = r.with_store(|s| {
        let g = s.grants.get_mut(&token).filter(|g| g.is_active(now))?;
        g.uses += 1;
        Some((g.machine_id.clone(), g.view_only, g.label.clone()))
    });
    let Some((machine, view_only, label)) = grant else {
        return (StatusCode::FORBIDDEN, "This link has expired or was withdrawn").into_response();
    };
    let who = if label.is_empty() { "Temporary link".to_string() } else { format!("Link: {label}") };
    ws.max_message_size(1 << 20).on_upgrade(move |socket| viewer_loop(r, socket, machine, view_only, Some(token), None, who))
}

async fn send_closed(sink: &mut futures_util::stream::SplitSink<WebSocket, Message>, reason: &str) {
    let m = SessionMsg::Closed { reason: reason.to_string() };
    let _ = sink.send(Message::Text(serde_json::to_string(&m).unwrap().into())).await;
    let _ = sink.close().await;
}

/// Should a message from the viewer reach the host?
pub fn viewer_message_allowed(text: &str, view_only: bool) -> bool {
    match serde_json::from_str::<SessionMsg>(text) {
        // Mode changes go through the relay's own check, never straight to the host.
        Ok(m) => {
            !(view_only && m.is_control())
                && !matches!(
                    m,
                    SessionMsg::Hello { .. } | SessionMsg::Closed { .. } | SessionMsg::SetViewOnly { .. } | SessionMsg::ViewMode { .. }
                )
        }
        Err(_) => false,
    }
}

/// May this text message from the host reach the viewer? Copied text never
/// goes to view-only viewers (the host holds it back too).
pub fn host_message_allowed(text: &str, view_only: bool) -> bool {
    !(view_only && text.trim_start().starts_with(r#"{"t":"clipboard""#))
}

async fn viewer_loop(
    r: Shared,
    viewer: WebSocket,
    machine: String,
    view_only: bool,
    grant: Option<String>,
    user: Option<String>,
    who: String,
) {
    let (mut vs, mut vr) = viewer.split();
    // A view-only temporary link stays view-only; everyone else may switch.
    let locked = grant.is_some() && view_only;
    let mut view_only = view_only;
    let (session_id, host, mut kill) = match r.open_session(&machine, view_only, grant, user, who).await {
        Ok(x) => x,
        Err(reason) => {
            send_closed(&mut vs, &reason).await;
            return;
        }
    };
    let mode = |v: bool| Message::Text(serde_json::to_string(&SessionMsg::ViewMode { view_only: v, locked }).unwrap().into());
    let _ = vs.send(mode(view_only)).await;
    let (mut hs, mut hr) = host.split();
    let view_only_now = Arc::new(std::sync::atomic::AtomicBool::new(view_only));
    let down_view_only = view_only_now.clone();
    // Pictures (host → viewer) flow in their own task: a big picture on its
    // way to a slow viewer never holds up mouse, keys and acks going back.
    let (to_viewer, mut to_viewer_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let mut down = tokio::spawn(async move {
        let reason = loop {
            tokio::select! {
                m = hr.next() => match m {
                    Some(Ok(msg @ (Message::Text(_) | Message::Binary(_)))) => {
                        if let Message::Text(t) = &msg
                            && !host_message_allowed(t.as_str(), down_view_only.load(std::sync::atomic::Ordering::Relaxed))
                        {
                            continue;
                        }
                        if vs.send(msg).await.is_err() { break String::new(); }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break "The remote computer ended the session".to_string(),
                    _ => {}
                },
                m = to_viewer_rx.recv() => match m {
                    Some(m) => if vs.send(m).await.is_err() { break String::new(); },
                    None => break String::new(),
                },
            }
        };
        (vs, reason)
    });
    let mut finished = None;
    let reason = loop {
        tokio::select! {
            m = vr.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(SessionMsg::SetViewOnly { view_only: want }) = serde_json::from_str::<SessionMsg>(t.as_str()) {
                        let next = want || locked;
                        if next != view_only {
                            view_only = next;
                            view_only_now.store(view_only, std::sync::atomic::Ordering::Relaxed);
                            if let Some(e) = r.sessions.lock().unwrap().get_mut(&session_id) {
                                e.view.view_only = view_only;
                            }
                            let m = SessionMsg::SetViewOnly { view_only };
                            if hs.send(Message::Text(serde_json::to_string(&m).unwrap().into())).await.is_err() {
                                break "The remote computer disconnected".to_string();
                            }
                        }
                        let _ = to_viewer.send(mode(view_only));
                        continue;
                    }
                    if viewer_message_allowed(t.as_str(), view_only) && hs.send(Message::Text(t)).await.is_err() {
                        break "The remote computer disconnected".to_string();
                    }
                }
                // Uploaded file pieces: control sessions only.
                Some(Ok(Message::Binary(b))) => {
                    if !view_only && crate::proto::FileChunk::decode(&b).is_some() && hs.send(Message::Binary(b)).await.is_err() {
                        break "The remote computer disconnected".to_string();
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break String::new(),
                _ => {}
            },
            done = &mut down => {
                let Ok((vs, reason)) = done else { break String::new() };
                finished = Some(vs);
                break reason;
            },
            _ = kill.changed() => {
                break kill.borrow().clone().unwrap_or_else(|| "Session ended".into());
            }
        }
    };
    r.sessions.lock().unwrap().remove(&session_id);
    let bye = SessionMsg::Closed { reason: "viewer left".into() };
    let _ = hs.send(Message::Text(serde_json::to_string(&bye).unwrap().into())).await;
    let _ = hs.close().await;
    drop(to_viewer);
    let vs = match finished {
        Some(vs) => Some(vs),
        None => match tokio::time::timeout(Duration::from_secs(5), &mut down).await {
            Ok(Ok((vs, _))) => Some(vs),
            _ => {
                down.abort();
                None
            }
        },
    };
    if let Some(mut vs) = vs
        && !reason.is_empty()
    {
        send_closed(&mut vs, &reason).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_text_only_flows_in_control_sessions() {
        let clip = serde_json::to_string(&SessionMsg::Clipboard { text: "secret".into() }).unwrap();
        assert!(!viewer_message_allowed(&clip, true));
        assert!(viewer_message_allowed(&clip, false));
        assert!(!host_message_allowed(&clip, true));
        assert!(host_message_allowed(&clip, false));
        let notice = serde_json::to_string(&SessionMsg::Notice { text: "hi".into() }).unwrap();
        assert!(host_message_allowed(&notice, true));
    }

    #[test]
    fn view_only_blocks_input_but_not_acks() {
        let input = r#"{"t":"input","event":{"k":"text","text":"rm -rf"}}"#;
        let ack = r#"{"t":"ack","seq":5}"#;
        let quality = serde_json::to_string(&SessionMsg::Quality { quality: crate::quality::Quality::default() }).unwrap();
        assert!(!viewer_message_allowed(input, true));
        assert!(viewer_message_allowed(input, false));
        assert!(viewer_message_allowed(ack, true));
        assert!(viewer_message_allowed(&quality, true));
        assert!(!viewer_message_allowed("not json", false));
        assert!(!viewer_message_allowed(r#"{"t":"closed","reason":"x"}"#, false), "viewer can't fake relay messages");
    }

    #[test]
    fn scripts_embed_server_and_token() {
        let sh = linux_script("https://rd.example.com", "abc");
        assert!(sh.contains("SERVER='https://rd.example.com'"));
        assert!(sh.contains("TOKEN='abc'"));
        assert!(sh.contains("install --server"));
        let ps = windows_script("https://rd.example.com", "abc");
        assert!(ps.contains("Tls12"), "Server 2016 needs TLS 1.2 switched on");
        assert!(ps.contains("RunAs"));
        assert!(ps.contains("farlink-host.exe"));
        assert!(ps.contains("Add-MpPreference"), "allows the Farlink folders in Defender");
        assert!(ps.contains("Test-Path $tmp"), "notices when antivirus removes the download");
        assert!(ps.contains("$LASTEXITCODE"), "does not claim success when the installer failed");
        assert!(ps.find("Done.").unwrap() > ps.find("Get-Service FarlinkHost").unwrap());
        assert!(windows_cmd("https://x.y", "t").contains("https://x.y/install/t/windows.ps1"));
    }

    #[test]
    fn path_segments_are_checked() {
        assert!(safe_segment("x86_64-pc-windows-gnu"));
        assert!(!safe_segment("../etc"));
        assert!(!safe_segment("a/b"));
        assert!(!safe_segment(""));
    }

    #[test]
    fn escaping() {
        assert_eq!(html_escape("<b>\"x\"&'"), "&lt;b&gt;&quot;x&quot;&amp;&#39;");
    }
}
