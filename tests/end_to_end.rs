//! End-to-end tests: a real relay server on a local port, a host helper (or a
//! scripted fake host) and viewers talking to it over WebSockets.

use farlink::config::{HostConfig, ServerConfig};
use farlink::host::{HostOptions, run_agent};
use farlink::proto::{AgentMsg, InputEvent, SessionMsg, TileHeader};
use farlink::server::{Overview, Relay, Shared};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct TestServer {
    relay: Shared,
    url: String,
    admin: String,
    _dir: std::path::PathBuf,
}

async fn start_server() -> TestServer {
    let dir = farlink::util::unique_temp_dir("farlink-e2e");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    let cfg = ServerConfig { public_url: url.clone(), bind: addr.to_string(), data_dir: dir.clone(), releases_dir: dir.join("releases"), ..Default::default() };
    let (relay, pw) = Relay::new(cfg).unwrap();
    let pw = pw.expect("first start creates a password");
    tokio::spawn(farlink::server::serve(relay.clone(), listener));
    let server = url.clone();
    let admin = blocking(move || farlink::net::Api::login(&server, "", &pw).unwrap()).await;
    TestServer { relay, url, admin, _dir: dir }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

impl TestServer {
    fn ws(&self, path: &str) -> String {
        farlink::util::ws_url(&self.url, path)
    }
    fn api(&self) -> farlink::net::Api {
        farlink::net::Api { server: self.url.clone(), token: self.admin.clone() }
    }
    async fn overview(&self) -> Overview {
        let api = self.api();
        blocking(move || api.overview().unwrap()).await
    }
}

async fn send(ws: &mut Ws, m: &impl serde::Serialize) {
    ws.send(Message::Text(serde_json::to_string(m).unwrap().into())).await.unwrap();
}

/// Next JSON message (skipping pictures), with a timeout.
async fn next_json<T: serde::de::DeserializeOwned>(ws: &mut Ws) -> T {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("timed out").expect("closed").unwrap();
        // Mode notices from the relay are checked separately (next_mode).
        if let Message::Text(t) = msg
            && !t.contains(r#""t":"view_mode""#)
        {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

/// Next "current mode" notice from the relay: (view_only, locked).
async fn next_mode(ws: &mut Ws) -> (bool, bool) {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("timed out").expect("closed").unwrap();
        if let Message::Text(t) = msg
            && let Ok(SessionMsg::ViewMode { view_only, locked }) = serde_json::from_str(&t)
        {
            return (view_only, locked);
        }
    }
}

/// A scripted host that registers like the real helper does.
async fn fake_host(s: &TestServer) -> (Ws, String, String) {
    fake_host_with(s, &s.relay.enroll_token(), "SRV2016").await
}

async fn fake_host_with(s: &TestServer, token: &str, hostname: &str) -> (Ws, String, String) {
    let (mut ws, _) = connect_async(s.ws("/ws/agent")).await.unwrap();
    send(
        &mut ws,
        &AgentMsg::Register {
            enroll_token: Some(token.to_string()),
            machine_id: None,
            secret: None,
            hostname: hostname.into(),
            os: "Windows Server 2016".into(),
            version: farlink::VERSION.into(),
            target: "x86_64-pc-windows-gnu".into(),
            monitors: farlink::desktop::TestPattern::dual().monitors,
        },
    )
    .await;
    match next_json::<AgentMsg>(&mut ws).await {
        AgentMsg::Registered { machine_id, secret, .. } => (ws, machine_id, secret),
        other => panic!("not registered: {other:?}"),
    }
}

/// Fake host answers an OpenSession by opening its session socket.
async fn accept_session(s: &TestServer, agent: &mut Ws, id: &str, secret: &str) -> (Ws, bool) {
    let AgentMsg::OpenSession { session_id, view_only } = next_json::<AgentMsg>(agent).await else { panic!("expected open_session") };
    let url = s.ws(&format!("/ws/host-session/{session_id}?machine={id}&secret={secret}"));
    let (mut host, _) = connect_async(url).await.unwrap();
    send(
        &mut host,
        &SessionMsg::Hello {
            hostname: "SRV2016".into(),
            os: "Windows Server 2016".into(),
            version: "x".into(),
            monitors: farlink::desktop::TestPattern::dual().monitors,
            view_only,
        },
    )
    .await;
    (host, view_only)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_helper_streams_both_monitors_to_admin_viewer() {
    let s = start_server().await;
    let cfg_dir = farlink::util::unique_temp_dir("farlink-e2e-host");
    let cfg = HostConfig { server_url: s.url.clone(), enroll_token: Some(s.relay.enroll_token()), machine_id: None, secret: None };
    let cfg_path = cfg_dir.join("host.json");
    tokio::spawn(run_agent(cfg_path.clone(), cfg, HostOptions { demo: true, no_update: true }));

    // The helper enrols and shows up online.
    let mut machine = None;
    for _ in 0..50 {
        let o = s.overview().await;
        if let Some(m) = o.machines.iter().find(|m| m.online) {
            machine = Some(m.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let machine = machine.expect("helper never came online");
    assert_eq!(machine.machine.monitors.len(), 2);
    // It saved its identity and dropped the one-time install token.
    let mut saved: HostConfig = farlink::config::load_json(&cfg_path);
    for _ in 0..50 {
        if saved.is_enrolled() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        saved = farlink::config::load_json(&cfg_path);
    }
    assert!(saved.is_enrolled());
    assert!(saved.enroll_token.is_none());

    // Admin viewer connects and receives the hello plus pictures of both screens.
    let url = s.ws(&format!("/ws/view/{}?token={}&view_only=0", machine.machine.id, s.admin));
    let (mut viewer, _) = connect_async(url).await.unwrap();
    let hello: SessionMsg = next_json(&mut viewer).await;
    let SessionMsg::Hello { monitors, view_only, .. } = hello else { panic!("expected hello") };
    assert_eq!(monitors.len(), 2);
    assert!(!view_only);
    let mut q = farlink::quality::Quality::preset(farlink::quality::Preset::Fast);
    q.max_fps = 30;
    send(&mut viewer, &SessionMsg::Quality { quality: q }).await;

    let mut seen = std::collections::HashSet::new();
    let mut canvases: std::collections::HashMap<u8, farlink::codec::Canvas> = Default::default();
    while seen.len() < 2 {
        let msg = tokio::time::timeout(Duration::from_secs(20), viewer.next()).await.unwrap().unwrap().unwrap();
        if let Message::Binary(b) = msg {
            let (h, data) = TileHeader::decode(&b).unwrap();
            canvases.entry(h.monitor).or_default().apply(&h, data).unwrap();
            if h.is_last() {
                send(&mut viewer, &SessionMsg::Ack { seq: h.seq }).await;
            }
            seen.insert(h.monitor);
        }
    }
    // Pictures are scaled by the quality setting (Fast is smaller than the screen).
    let f0 = canvases[&0].frame.as_ref().unwrap();
    assert!(q.scale_pct < 100);
    assert_eq!((f0.width, f0.height), q.scaled_size(1600, 900));

    // The viewer asks to draw the pointer itself: shape and position come
    // through the relay as their own small messages.
    send(&mut viewer, &SessionMsg::PointerMode { local: true }).await;
    let (mut shape, mut pos) = (None, None);
    while shape.is_none() || pos.is_none() {
        let msg = tokio::time::timeout(Duration::from_secs(20), viewer.next()).await.unwrap().unwrap().unwrap();
        match msg {
            Message::Binary(b) => {
                if let Some((h, _)) = TileHeader::decode(&b)
                    && h.is_last()
                {
                    send(&mut viewer, &SessionMsg::Ack { seq: h.seq }).await;
                }
            }
            Message::Text(t) => match serde_json::from_str::<SessionMsg>(&t) {
                Ok(SessionMsg::PointerShape { id, .. }) => shape = Some(id),
                Ok(SessionMsg::Pointer { x, y, shape }) => pos = Some((x, y, shape)),
                _ => {}
            },
            _ => {}
        }
    }
    assert_eq!(pos, Some((100, 100, shape.unwrap())), "the demo pointer's start position");

    // The admin overview lists the live session.
    let o = s.overview().await;
    assert_eq!(o.sessions.len(), 1);
    assert_eq!(o.machines[0].sessions, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn view_only_links_block_input_and_can_be_withdrawn() {
    let s = start_server().await;
    let (mut agent, id, secret) = fake_host(&s).await;

    // Admin creates a view-only temporary link.
    let api = s.api();
    let id2 = id.clone();
    let link = blocking(move || api.create_grant(&id2, 60, true, "Support").unwrap()).await;
    let guest = farlink::net::parse_link(&link).expect("link parses");

    let guest_url = s.ws(&format!("/ws/guest/{}", guest.token));
    let guest_task = tokio::spawn(async move { connect_async(guest_url).await.unwrap().0 });
    let (mut host, view_only) = accept_session(&s, &mut agent, &id, &secret).await;
    assert!(view_only, "host is told the session is view-only");
    let mut viewer = guest_task.await.unwrap();
    let SessionMsg::Hello { .. } = next_json::<SessionMsg>(&mut viewer).await else { panic!() };

    // Mouse/keyboard from a view-only guest never reach the host; acks do.
    send(&mut viewer, &SessionMsg::Input { event: InputEvent::Text { text: "format c:".into() } }).await;
    send(&mut viewer, &SessionMsg::Input { event: InputEvent::Ping { mon: 0, x: 0.5, y: 0.5 } }).await;
    send(&mut viewer, &SessionMsg::Ack { seq: 42 }).await;
    let first: SessionMsg = next_json(&mut host).await;
    assert_eq!(first, SessionMsg::Ack { seq: 42 });

    // Pictures flow from host to guest untouched.
    let tile = farlink::proto::TileHeader {
        monitor: 1,
        format: farlink::proto::TileFormat::Png,
        flags: 1,
        frame_w: 64,
        frame_h: 64,
        x: 0,
        y: 0,
        w: 64,
        h: 64,
        seq: 1,
    }
    .encode(b"png-bytes");
    host.send(Message::Binary(tile.clone().into())).await.unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), viewer.next()).await.unwrap().unwrap().unwrap() {
            Message::Binary(b) => {
                assert_eq!(b.to_vec(), tile);
                break;
            }
            _ => continue,
        }
    }

    // Withdraw the link: the guest is disconnected with a reason.
    let api = s.api();
    let token = guest.token.clone();
    blocking(move || api.revoke(&token).unwrap()).await;
    let closed: SessionMsg = next_json(&mut viewer).await;
    match closed {
        SessionMsg::Closed { reason } => assert!(reason.contains("withdrew"), "{reason}"),
        other => panic!("expected closed, got {other:?}"),
    }
    // ...and the link no longer opens.
    assert!(connect_async(s.ws(&format!("/ws/guest/{}", guest.token))).await.is_err());
    let o = s.overview().await;
    assert_eq!(o.grants[0].status, "withdrawn");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_links_pass_input_and_expire() {
    let s = start_server().await;
    let (mut agent, id, secret) = fake_host(&s).await;
    let api = s.api();
    let id2 = id.clone();
    let link = blocking(move || api.create_grant(&id2, 30, false, "").unwrap()).await;
    let guest = farlink::net::parse_link(&link).unwrap();

    let guest_url = s.ws(&format!("/ws/guest/{}", guest.token));
    let guest_task = tokio::spawn(async move { connect_async(guest_url).await.unwrap().0 });
    let (mut host, view_only) = accept_session(&s, &mut agent, &id, &secret).await;
    assert!(!view_only);
    let mut viewer = guest_task.await.unwrap();
    let _hello: SessionMsg = next_json(&mut viewer).await;

    let click = SessionMsg::Input {
        event: InputEvent::MouseButton { mon: 1, x: 0.5, y: 0.5, button: farlink::proto::MouseButton::Left, down: true },
    };
    send(&mut viewer, &click).await;
    assert_eq!(next_json::<SessionMsg>(&mut host).await, click);
    let ping = SessionMsg::Input { event: InputEvent::Ping { mon: 1, x: 0.25, y: 0.75 } };
    send(&mut viewer, &ping).await;
    assert_eq!(next_json::<SessionMsg>(&mut host).await, ping, "pings reach the host");

    // Time passes: the link expires and the session is ended by the server.
    let token = guest.token.clone();
    s.relay.with_store(|st| st.grants.get_mut(&token).unwrap().expires = 1);
    assert_eq!(s.relay.expire_sessions(), 1);
    match next_json::<SessionMsg>(&mut viewer).await {
        SessionMsg::Closed { reason } => assert!(reason.contains("expired"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_machine_and_bad_credentials_are_refused() {
    let s = start_server().await;
    // Wrong admin token.
    assert!(connect_async(s.ws("/ws/view/nope?token=bad")).await.is_err());
    // Wrong password.
    let url = s.url.clone();
    assert!(blocking(move || farlink::net::Api::login(&url, "", "wrong")).await.is_err());
    // Bad enrolment token is rejected.
    let (mut ws, _) = connect_async(s.ws("/ws/agent")).await.unwrap();
    send(
        &mut ws,
        &AgentMsg::Register {
            enroll_token: Some("forged".into()),
            machine_id: None,
            secret: None,
            hostname: "x".into(),
            os: "x".into(),
            version: "0".into(),
            target: "t".into(),
            monitors: vec![],
        },
    )
    .await;
    assert!(matches!(next_json::<AgentMsg>(&mut ws).await, AgentMsg::Rejected { .. }));

    // Known but offline machine: viewer gets a clear reason.
    let (agent, id, _) = fake_host(&s).await;
    drop(agent);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (mut viewer, _) = connect_async(s.ws(&format!("/ws/view/{id}?token={}", s.admin))).await.unwrap();
    match next_json::<SessionMsg>(&mut viewer).await {
        SessionMsg::Closed { reason } => assert!(reason.contains("offline"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn published_updates_are_offered_and_verified() {
    let s = start_server().await;
    let target = "x86_64-pc-windows-gnu";
    let rel = s.relay.cfg.releases_dir.join(target);
    std::fs::create_dir_all(&rel).unwrap();
    std::fs::write(rel.join("farlink-host.exe"), b"new helper build").unwrap();
    std::fs::write(s.relay.cfg.releases_dir.join("version.txt"), "9.1.0\n").unwrap();

    let url = s.url.clone();
    let (info, older, same) = blocking(move || {
        let older = farlink::update::check(&url, "farlink-host", target, "0.1.0").unwrap();
        let same = farlink::update::check(&url, "farlink-host", target, "9.1.0").unwrap();
        let info = older.clone().unwrap();
        let bytes = farlink::update::download(&url, &info).unwrap();
        assert_eq!(bytes, b"new helper build");
        (info, older, same)
    })
    .await;
    assert!(older.is_some());
    assert!(same.is_none(), "no update when already current");
    assert_eq!(info.version, "9.1.0");
    assert_eq!(info.path, "/download/x86_64-pc-windows-gnu/farlink-host.exe");

    // Connected hosts with an older version are told to update.
    let (mut agent, _, _) = fake_host(&s).await;
    match next_json::<AgentMsg>(&mut agent).await {
        AgentMsg::UpdateAvailable { version, force } => {
            assert_eq!(version, "9.1.0");
            assert!(!force, "a normal offer");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_desktop_app_package_is_offered_to_linux_apps_only() {
    let s = start_server().await;
    let linux = "aarch64-unknown-linux-gnu";
    let rel = s.relay.cfg.releases_dir.join(linux);
    std::fs::create_dir_all(&rel).unwrap();
    std::fs::write(rel.join("farlink"), b"bare app").unwrap();
    std::fs::write(rel.join("farlink.deb"), b"app package").unwrap();
    std::fs::write(s.relay.cfg.releases_dir.join("version.txt"), "9.2.0\n").unwrap();

    let url = s.url.clone();
    blocking(move || {
        let deb = farlink::util::DEB_APP;
        let info = farlink::update::check(&url, deb, linux, "0.1.0").unwrap().expect("package offered");
        assert_eq!(info.path, "/download/aarch64-unknown-linux-gnu/farlink.deb");
        assert_eq!(farlink::update::download(&url, &info).unwrap(), b"app package");
        // The bare program is still there for apps not installed from the package.
        let bare = farlink::update::check(&url, "farlink", linux, "0.1.0").unwrap().unwrap();
        assert_eq!(farlink::update::download(&url, &bare).unwrap(), b"bare app");
        // No package for Windows, and no other file names through the download route.
        assert!(farlink::update::check(&url, deb, "x86_64-pc-windows-gnu", "0.1.0").unwrap().is_none());
        assert!(ureq::get(&format!("{url}/download/{linux}/farlink.deb.part")).call().is_err());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_forces_helper_updates_and_sees_how_they_went() {
    let s = start_server().await;
    let (mut agent, id, secret) = fake_host(&s).await;
    let overview = |s: &TestServer| {
        let api = farlink::net::Api { server: s.url.clone(), token: s.admin.clone() };
        blocking(move || api.overview().unwrap())
    };
    let machine = |o: &farlink::server::Overview| o.machines.iter().find(|m| m.machine.id == id).unwrap().clone();
    let post = |s: &TestServer, path: &str, token: &str| {
        let (url, token) = (format!("{}{path}", s.url), token.to_string());
        blocking(move || match ureq::post(&url).header("Authorization", format!("Bearer {token}")).send_empty() {
            Ok(mut r) => (200, r.body_mut().read_json::<serde_json::Value>().unwrap()),
            Err(ureq::Error::StatusCode(c)) => (c, serde_json::Value::Null),
            Err(e) => panic!("{e}"),
        })
    };
    let update_path = format!("/api/machines/{id}/update");
    let admin = farlink::net::Api { server: s.url.clone(), token: s.admin.clone() };

    // Nothing published yet: the server explains why.
    let a = admin.clone();
    let id2 = id.clone();
    let err = blocking(move || a.update_machine(&id2).unwrap_err().to_string()).await;
    assert!(err.contains("No version is published"), "{err}");

    // The helper reports its version; a newer one gets published.
    for _ in 0..50 {
        if s.relay.is_online(&id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let m = machine(&overview(&s).await);
    assert!(m.online);
    assert_eq!(m.machine.version, farlink::VERSION);
    assert!(m.up_to_date && !m.updating && m.update_error.is_none());
    std::fs::create_dir_all(&s.relay.cfg.releases_dir).unwrap();
    std::fs::write(s.relay.cfg.releases_dir.join("version.txt"), "99.0.0\n").unwrap();
    let m = machine(&overview(&s).await);
    assert!(!m.up_to_date, "older than the published version");

    // Only the administrator may force updates.
    assert_eq!(post(&s, &update_path, "not-a-session").await.0, 401);
    assert_eq!(post(&s, "/api/machines/update-all", "not-a-session").await.0, 401);

    // "Update": the helper is told to install it even if it would not on its own.
    let (code, v) = post(&s, &update_path, &s.admin).await;
    assert_eq!(code, 200);
    assert_eq!(v["version"], "99.0.0");
    match next_json::<AgentMsg>(&mut agent).await {
        AgentMsg::UpdateAvailable { version, force } => assert!(force && version == "99.0.0"),
        other => panic!("{other:?}"),
    }
    assert!(machine(&overview(&s).await).updating, "shown as updating");
    // Already being updated: "Update all" leaves it out.
    assert_eq!(post(&s, "/api/machines/update-all", &s.admin).await.1["updating"], 0);

    // The helper could not install it: the administrator sees why.
    send(&mut agent, &AgentMsg::UpdateFailed { version: "99.0.0".into(), error: "checksum mismatch".into() }).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let m = machine(&overview(&s).await);
    assert!(!m.updating);
    assert_eq!(m.update_error.as_deref(), Some("checksum mismatch"));
    // "Update all" tries it again.
    assert_eq!(post(&s, "/api/machines/update-all", &s.admin).await.1["updating"], 1);
    assert!(matches!(next_json::<AgentMsg>(&mut agent).await, AgentMsg::UpdateAvailable { force: true, .. }));

    // Next try works: the helper comes back with the new version, the error is gone.
    drop(agent);
    let (mut ws, _) = connect_async(s.ws("/ws/agent")).await.unwrap();
    send(
        &mut ws,
        &AgentMsg::Register {
            enroll_token: None,
            machine_id: Some(id.clone()),
            secret: Some(secret),
            hostname: "SRV2016".into(),
            os: "Windows Server 2016".into(),
            version: "99.0.0".into(),
            target: "x86_64-pc-windows-gnu".into(),
            monitors: vec![],
        },
    )
    .await;
    assert!(matches!(next_json::<AgentMsg>(&mut ws).await, AgentMsg::Registered { .. }));
    let m = machine(&overview(&s).await);
    assert_eq!(m.machine.version, "99.0.0");
    assert!(m.up_to_date && !m.updating && m.update_error.is_none());

    // Up to date: nothing to do.
    assert_eq!(post(&s, &update_path, &s.admin).await.0, 400);
    assert_eq!(post(&s, "/api/machines/update-all", &s.admin).await.1["updating"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn install_link_serves_ready_made_scripts() {
    let s = start_server().await;
    let token = s.relay.enroll_token();
    let base = s.url.clone();
    let (page, sh, ps, bad) = blocking(move || {
        let get = |p: &str| ureq::get(&format!("{base}{p}")).call().map(|mut r| r.body_mut().read_to_string().unwrap());
        (
            get(&format!("/install/{token}")).unwrap(),
            get(&format!("/install/{token}/linux.sh")).unwrap(),
            get(&format!("/install/{token}/windows.ps1")).unwrap(),
            get("/install/not-the-token/linux.sh").is_err(),
        )
    })
    .await;
    let base = s.url.clone();
    let (admin, js, css, expired) = blocking(move || {
        let get = |p: &str| ureq::get(&format!("{base}{p}")).call().unwrap().body_mut().read_to_string().unwrap();
        (get("/"), get("/assets/viewer.js"), get("/assets/app.css"), get("/g/0000"))
    })
    .await;
    assert!(admin.contains("Sign in") && admin.contains("Temporary links"));
    assert!(js.contains("new WebSocket"));
    assert!(css.contains("--accent"));
    assert!(expired.contains("expired or the access was withdrawn"));
    assert!(page.contains("Windows 10 / 11 / Server 2016+"));
    assert!(sh.contains(&format!("SERVER='{}'", s.url)));
    assert!(ps.contains("farlink-host.exe"));
    assert!(bad, "wrong install token gives an error");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_sees_and_controls_only_their_own_computer() {
    let s = start_server().await;
    // Admin adds Anna and gets her personal install link.
    let (base, admin) = (s.url.clone(), s.admin.clone());
    let created: serde_json::Value = blocking(move || {
        ureq::post(&format!("{base}/api/users"))
            .header("Authorization", &format!("Bearer {admin}"))
            .send_json(serde_json::json!({"name": "Anna", "login": "anna", "password": "anna-pass-1"}))
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap()
    })
    .await;
    let install_url = created["install_url"].as_str().unwrap().to_string();
    let anna_token = install_url.rsplit('/').next().unwrap().to_string();

    // Her install page says whose computer it becomes.
    let page = blocking(move || ureq::get(&install_url).call().unwrap().body_mut().read_to_string().unwrap()).await;
    assert!(page.contains("Anna's"), "install page names the owner");

    // One computer installed with her link, one with the general link.
    let (mut anna_agent, anna_pc, anna_secret) = fake_host_with(&s, &anna_token, "ANNA-PC").await;
    let (_office_agent, office_pc, _) = fake_host_with(&s, &s.relay.enroll_token(), "OFFICE").await;

    // The admin sees both, and that Anna's is installed and hers.
    let o = s.overview().await;
    assert_eq!(o.machines.len(), 2);
    let anna_view = o.users.iter().find(|u| u.login == "anna").unwrap();
    assert_eq!(anna_view.machines, vec![anna_pc.clone()]);
    assert_eq!(o.machines.iter().find(|m| m.machine.id == anna_pc).unwrap().owner_name.as_deref(), Some("Anna"));

    // Anna signs in: only her computer, nothing else.
    let url = s.url.clone();
    let anna = blocking(move || farlink::net::Api::login(&url, "anna", "anna-pass-1").unwrap()).await;
    let api = farlink::net::Api { server: s.url.clone(), token: anna.clone() };
    let mine = blocking(move || api.overview().unwrap()).await;
    assert_eq!(mine.role, "user");
    assert_eq!(mine.user_name, "Anna");
    assert_eq!(mine.machines.len(), 1);
    assert_eq!(mine.machines[0].machine.id, anna_pc);
    assert!(mine.users.is_empty() && mine.grants.is_empty() && mine.install_url.is_empty());

    // She cannot open the office computer or use admin functions.
    assert!(connect_async(s.ws(&format!("/ws/view/{office_pc}?token={anna}"))).await.is_err());
    let api = farlink::net::Api { server: s.url.clone(), token: anna.clone() };
    let office = office_pc.clone();
    assert!(blocking(move || api.create_grant(&office, 30, true, "")).await.is_err());

    // She controls her own computer.
    let url = s.ws(&format!("/ws/view/{anna_pc}?token={anna}"));
    let viewer_task = tokio::spawn(async move { connect_async(url).await.unwrap().0 });
    let (mut host, view_only) = accept_session(&s, &mut anna_agent, &anna_pc, &anna_secret).await;
    assert!(!view_only);
    let mut viewer = viewer_task.await.unwrap();
    let _hello: SessionMsg = next_json(&mut viewer).await;
    let key = SessionMsg::Input { event: InputEvent::Key { key: "Enter".into(), down: true } };
    send(&mut viewer, &key).await;
    assert_eq!(next_json::<SessionMsg>(&mut host).await, key);
    assert_eq!(s.overview().await.sessions[0]["who"], "Anna");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_go_both_ways_in_control_sessions_only() {
    use farlink::proto::FileChunk;
    let s = start_server().await;
    let cfg_dir = farlink::util::unique_temp_dir("farlink-e2e-files");
    let cfg = HostConfig { server_url: s.url.clone(), enroll_token: Some(s.relay.enroll_token()), machine_id: None, secret: None };
    tokio::spawn(run_agent(cfg_dir.join("host.json"), cfg, HostOptions { demo: true, no_update: true }));
    let mut id = None;
    for _ in 0..50 {
        if let Some(m) = s.overview().await.machines.iter().find(|m| m.online) {
            id = Some(m.machine.id.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let id = id.expect("online");
    let remote_dir = cfg_dir.join("remote");
    std::fs::create_dir_all(&remote_dir).unwrap();

    let (mut viewer, _) = connect_async(s.ws(&format!("/ws/view/{id}?token={}", s.admin))).await.unwrap();
    let _hello: SessionMsg = next_json(&mut viewer).await;
    send(&mut viewer, &SessionMsg::Watch { monitors: vec![] }).await; // no pictures needed

    // Upload a file in two pieces.
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 256) as u8).collect();
    let up = SessionMsg::FsUpload { req: 1, dir: remote_dir.display().to_string(), name: "report.bin".into(), size: data.len() as u64 };
    send(&mut viewer, &up).await;
    let (a, b) = data.split_at(200_000);
    viewer.send(Message::Binary(FileChunk { req: 1, last: false }.encode(a).into())).await.unwrap();
    viewer.send(Message::Binary(FileChunk { req: 1, last: true }.encode(b).into())).await.unwrap();
    let done = loop {
        if let SessionMsg::FsDone { req: 1, path, error } = next_json::<SessionMsg>(&mut viewer).await {
            break (path, error);
        }
    };
    assert!(done.1.is_none(), "{:?}", done.1);
    assert_eq!(std::fs::read(remote_dir.join("report.bin")).unwrap(), data);

    // List the folder and download the file back.
    send(&mut viewer, &SessionMsg::FsList { req: 2, path: remote_dir.display().to_string() }).await;
    loop {
        if let SessionMsg::FsListing { entries, .. } = next_json::<SessionMsg>(&mut viewer).await {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].size, 300_000);
            break;
        }
    }
    send(&mut viewer, &SessionMsg::FsDownload { req: 3, path: done.0.clone() }).await;
    let mut got = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), viewer.next()).await.unwrap().unwrap().unwrap() {
            Message::Binary(b) => {
                if let Some((c, d)) = FileChunk::decode(&b) {
                    assert_eq!(c.req, 3);
                    got.extend_from_slice(d);
                    if c.last {
                        break;
                    }
                }
            }
            _ => continue,
        }
    }
    assert_eq!(got, data);

    // A view-only session gets no file access at all.
    let (mut watcher, _) = connect_async(s.ws(&format!("/ws/view/{id}?token={}&view_only=1", s.admin))).await.unwrap();
    let _hello: SessionMsg = next_json(&mut watcher).await;
    send(&mut watcher, &SessionMsg::Watch { monitors: vec![] }).await;
    send(&mut watcher, &SessionMsg::FsList { req: 9, path: remote_dir.display().to_string() }).await;
    let answer = tokio::time::timeout(Duration::from_millis(1500), async {
        loop {
            if let Some(Ok(Message::Text(t))) = watcher.next().await
                && t.contains("fs_listing")
            {
                return;
            }
        }
    })
    .await;
    assert!(answer.is_err(), "view-only session must not list files");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_admin_renames_and_everyone_sees_the_new_name() {
    let s = start_server().await;
    let (base, admin) = (s.url.clone(), s.admin.clone());
    let created: serde_json::Value = blocking(move || {
        ureq::post(&format!("{base}/api/users"))
            .header("Authorization", &format!("Bearer {admin}"))
            .send_json(serde_json::json!({"name": "Anna", "login": "anna", "password": "anna-pass-1"}))
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap()
    })
    .await;
    let anna_token = created["install_url"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
    let (_agent, pc, _) = fake_host_with(&s, &anna_token, "ANNA-PC").await;

    // The admin renames it.
    let api = s.api();
    let id = pc.clone();
    blocking(move || api.rename(&id, "  Anna's laptop  ").unwrap()).await;
    assert_eq!(s.overview().await.machines[0].machine.name, "Anna's laptop");

    // Anna sees the new name, but may not rename it herself.
    let url = s.url.clone();
    let anna = blocking(move || farlink::net::Api::login(&url, "anna", "anna-pass-1").unwrap()).await;
    let api = farlink::net::Api { server: s.url.clone(), token: anna };
    let id = pc.clone();
    let (mine, refused) = blocking(move || (api.overview().unwrap(), api.rename(&id, "Mine now").is_err())).await;
    assert_eq!(mine.machines[0].machine.name, "Anna's laptop");
    assert!(refused, "only the administrator renames computers");

    // Empty names are refused; a temporary link shows the new name.
    let api = s.api();
    let id = pc.clone();
    let (empty_refused, link) = blocking(move || (api.rename(&id, "   ").is_err(), api.create_grant(&id, 30, true, "").unwrap())).await;
    assert!(empty_refused);
    let token = farlink::net::parse_link(&link).unwrap().token;
    let base = s.url.clone();
    let info: serde_json::Value =
        blocking(move || ureq::get(&format!("{base}/api/guest/{token}")).call().unwrap().body_mut().read_json().unwrap()).await;
    assert_eq!(info["name"], "Anna's laptop");
    assert_eq!(s.overview().await.machines[0].machine.name, "Anna's laptop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notes_are_the_admins_and_heartbeats_bring_load_and_disks() {
    let s = start_server().await;
    let (base, admin) = (s.url.clone(), s.admin.clone());
    let created: serde_json::Value = blocking(move || {
        ureq::post(&format!("{base}/api/users"))
            .header("Authorization", &format!("Bearer {admin}"))
            .send_json(serde_json::json!({"name": "Anna", "login": "anna", "password": "anna-pass-1"}))
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap()
    })
    .await;
    let anna_token = created["install_url"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
    let (mut agent, pc, _) = fake_host_with(&s, &anna_token, "ANNA-PC").await;
    assert_eq!(s.overview().await.machines[0].stats, None, "no stats before the first heartbeat");

    // The admin writes notes; they come back in the overview (the web admin reads the same).
    let api = s.api();
    let id = pc.clone();
    blocking(move || api.set_notes(&id, "Kitchen\r\nprinter on USB\n\n").unwrap()).await;
    assert_eq!(s.overview().await.machines[0].machine.notes, "Kitchen\nprinter on USB");

    // Anna sees her computer but not the notes, and may not write them.
    let url = s.url.clone();
    let anna = blocking(move || farlink::net::Api::login(&url, "anna", "anna-pass-1").unwrap()).await;
    let api = farlink::net::Api { server: s.url.clone(), token: anna };
    let id = pc.clone();
    let (mine, refused) = blocking(move || (api.overview().unwrap(), api.set_notes(&id, "mine").is_err())).await;
    assert_eq!(mine.machines[0].machine.notes, "");
    assert!(refused, "only the administrator writes notes");
    let api = s.api();
    let id = pc.clone();
    let too_long = "x".repeat(farlink::grants::MAX_NOTES + 1);
    assert!(blocking(move || api.set_notes(&id, &too_long).is_err()).await);

    // A heartbeat with stats shows up for both of them.
    let stats = farlink::sysstat::demo_stats();
    send(&mut agent, &AgentMsg::Heartbeat { monitors: farlink::desktop::TestPattern::dual().monitors, stats: Some(stats.clone()) }).await;
    let mut seen = None;
    for _ in 0..50 {
        seen = s.overview().await.machines[0].stats.clone();
        if seen.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(seen, Some(stats.clone()));
    let api = farlink::net::Api { server: s.url.clone(), token: mine_token(&s).await };
    let theirs = blocking(move || api.overview().unwrap()).await;
    assert_eq!(theirs.machines[0].stats, Some(stats));

    // Offline: no load shown any more, notes stay.
    drop(agent);
    let mut gone = false;
    for _ in 0..100 {
        let m = s.overview().await.machines[0].clone();
        if !m.online {
            assert_eq!(m.stats, None);
            assert_eq!(m.machine.notes, "Kitchen\nprinter on USB");
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(gone);
}

async fn mine_token(s: &TestServer) -> String {
    let url = s.url.clone();
    blocking(move || farlink::net::Api::login(&url, "anna", "anna-pass-1").unwrap()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn view_only_can_be_switched_during_a_session_unless_the_link_forbids_it() {
    let s = start_server().await;
    let (mut agent, id, secret) = fake_host(&s).await;

    // Administrator connects with control and switches to watching only.
    let url = s.ws(&format!("/ws/view/{id}?token={}", s.admin));
    let viewer_task = tokio::spawn(async move { connect_async(url).await.unwrap().0 });
    let (mut host, view_only) = accept_session(&s, &mut agent, &id, &secret).await;
    assert!(!view_only);
    let mut viewer = viewer_task.await.unwrap();
    assert_eq!(next_mode(&mut viewer).await, (false, false), "starts in control, switchable");

    send(&mut viewer, &SessionMsg::SetViewOnly { view_only: true }).await;
    assert_eq!(next_mode(&mut viewer).await, (true, false));
    assert_eq!(next_json::<SessionMsg>(&mut host).await, SessionMsg::SetViewOnly { view_only: true }, "helper is told");
    assert_eq!(s.overview().await.sessions[0]["view_only"], true, "admin page shows the new mode");
    // Input is now dropped by the relay; the ack still arrives.
    let key = SessionMsg::Input { event: InputEvent::Key { key: "Enter".into(), down: true } };
    send(&mut viewer, &key).await;
    send(&mut viewer, &SessionMsg::Ack { seq: 1 }).await;
    assert_eq!(next_json::<SessionMsg>(&mut host).await, SessionMsg::Ack { seq: 1 });

    // And back to control.
    send(&mut viewer, &SessionMsg::SetViewOnly { view_only: false }).await;
    assert_eq!(next_mode(&mut viewer).await, (false, false));
    assert_eq!(next_json::<SessionMsg>(&mut host).await, SessionMsg::SetViewOnly { view_only: false });
    send(&mut viewer, &key).await;
    assert_eq!(next_json::<SessionMsg>(&mut host).await, key);
    drop(viewer);
    drop(host);

    // A view-only temporary link can't be switched to control.
    let api = s.api();
    let id2 = id.clone();
    let link = blocking(move || api.create_grant(&id2, 60, true, "").unwrap()).await;
    let token = farlink::net::parse_link(&link).unwrap().token;
    let url = s.ws(&format!("/ws/guest/{token}"));
    let guest_task = tokio::spawn(async move { connect_async(url).await.unwrap().0 });
    let (mut host, _) = accept_session(&s, &mut agent, &id, &secret).await;
    let mut guest = guest_task.await.unwrap();
    assert_eq!(next_mode(&mut guest).await, (true, true), "locked");
    send(&mut guest, &SessionMsg::SetViewOnly { view_only: false }).await;
    assert_eq!(next_mode(&mut guest).await, (true, true), "still view-only");
    send(&mut guest, &key).await;
    send(&mut guest, &SessionMsg::Ack { seq: 2 }).await;
    assert_eq!(next_json::<SessionMsg>(&mut host).await, SessionMsg::Ack { seq: 2 }, "no switch and no input reached the helper");
}

/// Built-in HTTPS with certificate files: the web page and a WebSocket upgrade
/// work over TLS.
#[tokio::test]
async fn https_with_certificate_files() {
    use farlink::config::Tls;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = farlink::util::unique_temp_dir("farlink-tls");
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    std::fs::write(dir.join("cert.pem"), cert.cert.pem()).unwrap();
    std::fs::write(dir.join("key.pem"), cert.key_pair.serialize_pem()).unwrap();
    let tls = Tls::Files { cert: dir.join("cert.pem"), key: dir.join("key.pem") };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let url = format!("https://localhost:{port}");
    let cfg = ServerConfig {
        public_url: url.clone(),
        bind: format!("127.0.0.1:{port}"),
        data_dir: dir.clone(),
        releases_dir: dir.join("releases"),
        tls: tls.clone(),
        update_from_github: None,
    };
    let (relay, _) = Relay::new(cfg).unwrap();
    let tls_listener = farlink::tls::listen(listener, &tls, &dir).unwrap();
    tokio::spawn(farlink::server::serve(relay, tls_listener));

    // A client that trusts only our self-signed certificate.
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let client = Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );

    // A silent client must not block others (handshakes run separately).
    let _idle = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // The admin page over HTTPS.
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut tls_stream = tokio_rustls::TlsConnector::from(client.clone()).connect(name, tcp).await.unwrap();
    tls_stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut page = String::new();
    tls_stream.read_to_string(&mut page).await.unwrap();
    assert!(page.starts_with("HTTP/1.1 200"), "{}", &page[..page.len().min(200)]);

    // A WebSocket (what hosts and viewers use) over TLS.
    let (ws, resp) = tokio_tungstenite::connect_async_tls_with_config(
        format!("wss://localhost:{port}/ws/agent"),
        None,
        false,
        Some(tokio_tungstenite::Connector::Rustls(client)),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 101);
    drop(ws);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn bad_certificate_files_are_reported() {
    let dir = farlink::util::unique_temp_dir("farlink-tls-bad");
    std::fs::write(dir.join("cert.pem"), "not a certificate").unwrap();
    std::fs::write(dir.join("key.pem"), "not a key").unwrap();
    let err = farlink::tls::config_from_files(&dir.join("cert.pem"), &dir.join("key.pem")).unwrap_err();
    assert!(format!("{err:#}").contains("cert.pem"), "{err:#}");
    let _ = std::fs::remove_dir_all(dir);
}

/// A fake GitHub serving one release: `/repos/o/farlink/releases/latest`, the
/// program files and `SHA256SUMS.txt` (whose entry for `tamper` is wrong).
async fn fake_github(version: &str, tamper: Option<&'static str>) -> String {
    use axum::extract::Path as P;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let names = [
        "farlink-linux-x86_64",
        "farlink-host-linux-x86_64",
        "farlink-server-linux-x86_64",
        "farlink-linux-arm64",
        "farlink-host-linux-arm64",
        "farlink-windows-x86_64.exe",
        "farlink-host-windows-x86_64.exe",
    ];
    let body = |n: &str| format!("{n} {version}").into_bytes();
    let mut sums = String::new();
    for n in names {
        let hash = if Some(n) == tamper { "00".repeat(32) } else { farlink::util::sha256_hex(&body(n)) };
        sums.push_str(&format!("{hash}  {n}\n"));
    }
    let mut assets: Vec<_> =
        names.iter().map(|n| serde_json::json!({"name": n, "browser_download_url": format!("{base}/dl/{n}")})).collect();
    assets.push(serde_json::json!({"name": "SHA256SUMS.txt", "browser_download_url": format!("{base}/dl/SHA256SUMS.txt")}));
    let release = serde_json::json!({"tag_name": format!("v{version}"), "draft": false, "prerelease": false, "assets": assets});
    let version = version.to_string();
    let app = axum::Router::new()
        .route("/repos/o/farlink/releases/latest", axum::routing::get(move || async move { axum::Json(release) }))
        .route(
            "/dl/{name}",
            axum::routing::get(move |P(name): P<String>| {
                let (sums, version) = (sums.clone(), version.clone());
                async move { if name == "SHA256SUMS.txt" { sums.into_bytes() } else { format!("{name} {version}").into_bytes() } }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

#[tokio::test]
async fn server_updates_itself_and_publishes_from_github() {
    use farlink::server_update::{apply, fetch_latest};
    let api = fake_github("0.9.0", None).await;
    let dir = farlink::util::unique_temp_dir("farlink-selfupd");
    let (releases, exe) = (dir.join("releases"), dir.join("farlink-server"));
    std::fs::write(&exe, b"old server").unwrap();
    let (r, e) = (releases.clone(), exe.clone());
    let done = blocking(move || {
        let latest = fetch_latest(&api, "o/farlink").unwrap();
        assert_eq!(latest.version, "0.9.0");
        apply(&latest, "0.1.0", Some("0.1.0"), &r, Some((&e, "x86_64-unknown-linux-gnu"))).unwrap()
    })
    .await;
    assert!(done.server_replaced);
    assert_eq!(done.published.as_deref(), Some("0.9.0"));
    assert_eq!(std::fs::read(&exe).unwrap(), b"farlink-server-linux-x86_64 0.9.0");
    assert_eq!(std::fs::read_to_string(releases.join("version.txt")).unwrap().trim(), "0.9.0");
    for (t, f, asset) in [
        ("x86_64-unknown-linux-gnu", "farlink-host", "farlink-host-linux-x86_64"),
        ("aarch64-unknown-linux-gnu", "farlink", "farlink-linux-arm64"),
        ("x86_64-pc-windows-gnu", "farlink-host.exe", "farlink-host-windows-x86_64.exe"),
    ] {
        assert_eq!(std::fs::read(releases.join(t).join(f)).unwrap(), format!("{asset} 0.9.0").into_bytes());
    }
    assert!(!releases.join("x86_64-unknown-linux-gnu/farlink-server").exists(), "server is not offered to computers");

    // A tampered file: nothing at all changes (not the server, not the releases).
    let api = fake_github("1.0.0", Some("farlink-host-windows-x86_64.exe")).await;
    let (r, e) = (releases.clone(), exe.clone());
    let err = blocking(move || {
        let latest = fetch_latest(&api, "o/farlink").unwrap();
        apply(&latest, "0.9.0", Some("0.9.0"), &r, Some((&e, "x86_64-unknown-linux-gnu"))).unwrap_err()
    })
    .await;
    assert!(format!("{err:#}").contains("checksum"), "{err:#}");
    assert_eq!(std::fs::read(&exe).unwrap(), b"farlink-server-linux-x86_64 0.9.0");
    assert_eq!(std::fs::read_to_string(releases.join("version.txt")).unwrap().trim(), "0.9.0");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn overview_shows_relay_version_and_update_check_is_admin_only() {
    let ts = start_server().await;
    let (url, admin) = (ts.url.clone(), ts.admin.clone());
    blocking(move || {
        let get = |token: &str| -> serde_json::Value {
            ureq::get(format!("{url}/api/overview"))
                .header("Authorization", format!("Bearer {token}"))
                .call()
                .unwrap()
                .body_mut()
                .read_json()
                .unwrap()
        };
        let post = |token: &str| {
            match ureq::post(format!("{url}/api/server/check-update")).header("Authorization", format!("Bearer {token}")).send_empty() {
                Ok(r) => r.status().as_u16(),
                Err(ureq::Error::StatusCode(c)) => c,
                Err(e) => panic!("{e}"),
            }
        };
        let o = get(&admin);
        assert_eq!(o["server_version"], farlink::VERSION);
        assert_eq!(o["server_update"]["version"], farlink::VERSION);
        assert!(o["server_update"]["source"].is_null(), "no GitHub repository set");
        assert!(o["server_update"]["last_update"].is_null(), "first start is not an update");
        assert_eq!(post("not-a-session"), 401);
        assert_eq!(post(&admin), 400, "automatic updates are off: nothing to check");
    })
    .await;
    // Turned on: the request wakes the update loop and shows "checking".
    ts.relay.set_update_status(|s| s.source = Some("o/farlink".into()));
    let (url, admin) = (ts.url.clone(), ts.admin.clone());
    let checking = blocking(move || {
        let r = ureq::post(format!("{url}/api/server/check-update")).header("Authorization", format!("Bearer {admin}")).send_empty();
        assert_eq!(r.unwrap().status().as_u16(), 200);
        let o: serde_json::Value = ureq::get(format!("{url}/api/overview"))
            .header("Authorization", format!("Bearer {admin}"))
            .call()
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap();
        o["server_update"]["checking"].as_bool()
    })
    .await;
    assert_eq!(checking, Some(true));
    tokio::time::timeout(Duration::from_secs(1), ts.relay.update_now.notified()).await.expect("update loop woken");
}

/// Status code of a preview request with this sign-in token ("" = none).
async fn preview_status(s: &TestServer, machine: &str, token: &str) -> u16 {
    let url = format!("{}/api/machines/{machine}/preview", s.url);
    let token = token.to_string();
    blocking(move || {
        let mut req = ureq::get(&url);
        if !token.is_empty() {
            req = req.header("Authorization", &format!("Bearer {token}"));
        }
        match req.call() {
            Ok(r) => r.status().as_u16(),
            Err(ureq::Error::StatusCode(c)) => c,
            Err(e) => panic!("{e}"),
        }
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_helper_sends_previews_of_every_screen() {
    let s = start_server().await;
    let cfg_dir = farlink::util::unique_temp_dir("farlink-e2e-host");
    let cfg = HostConfig { server_url: s.url.clone(), enroll_token: Some(s.relay.enroll_token()), machine_id: None, secret: None };
    tokio::spawn(run_agent(cfg_dir.join("host.json"), cfg, HostOptions { demo: true, no_update: true }));
    let mut id = None;
    for _ in 0..50 {
        if let Some(m) = s.overview().await.machines.iter().find(|m| m.online) {
            id = Some(m.machine.id.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let id = id.expect("helper never came online");

    let api = s.api();
    let p = blocking(move || api.preview(&id).unwrap()).await;
    assert!(p.taken > 0);
    let frames = farlink::net::decode_preview(&p);
    let sizes: Vec<_> = frames.iter().map(|(i, f)| (*i, f.width, f.height)).collect();
    assert_eq!(sizes, vec![(0, 480, 270), (1, 480, 384)], "both demo screens, scaled down");
    // No viewing session was opened for it.
    assert!(s.overview().await.sessions.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn previews_follow_access_rules_and_are_shared_briefly() {
    let s = start_server().await;
    let (base, admin) = (s.url.clone(), s.admin.clone());
    let created: serde_json::Value = blocking(move || {
        ureq::post(&format!("{base}/api/users"))
            .header("Authorization", &format!("Bearer {admin}"))
            .send_json(serde_json::json!({"name": "Anna", "login": "anna", "password": "anna-pass-1"}))
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap()
    })
    .await;
    let anna_token = created["install_url"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
    let (mut anna_agent, anna_pc, _) = fake_host_with(&s, &anna_token, "ANNA-PC").await;
    let (mut office_agent, office_pc, _) = fake_host_with(&s, &s.relay.enroll_token(), "OFFICE").await;
    let url = s.url.clone();
    let anna = blocking(move || farlink::net::Api::login(&url, "anna", "anna-pass-1").unwrap()).await;

    // Nobody signed in, and Anna for someone else's computer: refused before the host is asked.
    assert_eq!(preview_status(&s, &office_pc, "").await, 401);
    assert_eq!(preview_status(&s, &office_pc, &anna).await, 403);
    assert_eq!(preview_status(&s, "no-such-pc", &s.admin).await, 403);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), office_agent.next()).await.is_err(),
        "the office computer was not asked for pictures"
    );

    // Anna's own computer: the relay asks it and passes the answer on.
    let api = farlink::net::Api { server: s.url.clone(), token: anna.clone() };
    let pc = anna_pc.clone();
    let request = tokio::spawn(async move { blocking(move || api.preview(&pc)).await });
    let AgentMsg::PreviewRequest { req } = next_json::<AgentMsg>(&mut anna_agent).await else { panic!("expected preview_request") };
    // Another computer can't answer in its place.
    let fake = farlink::proto::PreviewShot { index: 0, width: 1, height: 1, jpeg: "AAAA".into() };
    send(&mut office_agent, &AgentMsg::Preview { req, shots: vec![fake.clone(), fake] }).await;
    let shot = farlink::proto::PreviewShot { index: 0, width: 2, height: 2, jpeg: "/9j/".into() };
    tokio::time::sleep(Duration::from_millis(100)).await;
    send(&mut anna_agent, &AgentMsg::Preview { req, shots: vec![shot.clone()] }).await;
    let p = request.await.unwrap().unwrap();
    assert_eq!(p.shots, vec![shot.clone()], "the real computer's answer, not the impostor's");

    // Asked again right away (the admin looking too): served from the relay, the computer isn't bothered.
    let api = s.api();
    let pc = anna_pc.clone();
    let again = blocking(move || api.preview(&pc).unwrap()).await;
    assert_eq!(again.shots, vec![shot]);
    assert!(tokio::time::timeout(Duration::from_millis(300), anna_agent.next()).await.is_err(), "no second request");

    // Offline computers have no preview.
    drop(office_agent);
    for _ in 0..50 {
        if !s.relay.is_online(&office_pc) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(preview_status(&s, &office_pc, &s.admin).await, 503);
    let api = s.api();
    let err = blocking(move || api.preview(&office_pc).unwrap_err()).await;
    assert!(err.to_string().contains("offline"), "{err}");
}

/// Next copied text the viewer gets.
async fn next_clip(ws: &mut Ws) -> String {
    loop {
        if let SessionMsg::Clipboard { text } = next_json::<SessionMsg>(ws).await {
            return text;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copied_text_goes_both_ways_in_control_sessions_only() {
    use farlink::clipboard::{demo_get, demo_set};
    let s = start_server().await;
    let cfg_dir = farlink::util::unique_temp_dir("farlink-e2e-clipboard");
    let cfg = HostConfig { server_url: s.url.clone(), enroll_token: Some(s.relay.enroll_token()), machine_id: None, secret: None };
    tokio::spawn(run_agent(cfg_dir.join("host.json"), cfg, HostOptions { demo: true, no_update: true }));
    let mut id = None;
    for _ in 0..50 {
        if let Some(m) = s.overview().await.machines.iter().find(|m| m.online) {
            id = Some(m.machine.id.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let id = id.expect("online");
    let (mut viewer, _) = connect_async(s.ws(&format!("/ws/view/{id}?token={}", s.admin))).await.unwrap();
    let _hello: SessionMsg = next_json(&mut viewer).await;
    send(&mut viewer, &SessionMsg::Watch { monitors: vec![] }).await;

    let wait_remote = |want: &'static str| async move {
        for _ in 0..50 {
            if demo_get().as_deref() == Some(want) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    };

    // Viewer -> remote computer.
    send(&mut viewer, &SessionMsg::Clipboard { text: "copied on my PC ✓".into() }).await;
    assert!(wait_remote("copied on my PC ✓").await);
    // Remote computer -> viewer (what the viewer just sent is not echoed).
    demo_set("copied on the remote PC");
    assert_eq!(next_clip(&mut viewer).await, "copied on the remote PC");

    // Only watching: nothing in either direction.
    send(&mut viewer, &SessionMsg::SetViewOnly { view_only: true }).await;
    assert_eq!(next_mode(&mut viewer).await, (true, false));
    send(&mut viewer, &SessionMsg::Clipboard { text: "must not arrive".into() }).await;
    demo_set("secret while watching");
    let leaked = tokio::time::timeout(Duration::from_millis(1500), next_clip(&mut viewer)).await;
    assert!(leaked.is_err(), "view-only session got copied text: {leaked:?}");
    assert_eq!(demo_get().as_deref(), Some("secret while watching"));

    // Control again: only text copied from now on is shared.
    send(&mut viewer, &SessionMsg::SetViewOnly { view_only: false }).await;
    assert_eq!(next_mode(&mut viewer).await, (false, false));
    tokio::time::sleep(Duration::from_millis(800)).await;
    demo_set("after switching back");
    assert_eq!(next_clip(&mut viewer).await, "after switching back");
}
