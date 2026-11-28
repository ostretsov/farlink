//! The MCP server against a real relay: a real helper (shell, files,
//! screenshots), a scripted host (the input that arrives), a person's
//! sign-in, and the `farlink mcp` program itself over stdin/stdout.

use farlink::config::{HostConfig, ServerConfig};
use farlink::host::{HostOptions, run_agent};
use farlink::mcp::{Login, Mcp};
use farlink::proto::{AgentMsg, InputEvent, MouseButton, SessionMsg};
use farlink::server::{Relay, Shared};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

struct TestServer {
    relay: Shared,
    url: String,
    password: String,
    admin: String,
    _dir: std::path::PathBuf,
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

async fn start_server() -> TestServer {
    let dir = farlink::util::unique_temp_dir("farlink-mcp-e2e");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let cfg = ServerConfig {
        public_url: url.clone(),
        bind: listener.local_addr().unwrap().to_string(),
        data_dir: dir.clone(),
        releases_dir: dir.join("releases"),
        stun_port: 0, // relay only: simpler, and the same for every test
        extra_stun_servers: Vec::new(),
        ..Default::default()
    };
    let (relay, pw) = Relay::new(cfg).unwrap();
    let password = pw.unwrap();
    tokio::spawn(farlink::server::serve(relay.clone(), listener));
    let (u, p) = (url.clone(), password.clone());
    let admin = blocking(move || farlink::net::Api::login(&u, "", &p).unwrap()).await;
    TestServer {
        relay,
        url,
        password,
        admin,
        _dir: dir,
    }
}

impl TestServer {
    fn api(&self) -> farlink::net::Api {
        farlink::net::Api {
            server: self.url.clone(),
            token: self.admin.clone(),
        }
    }
    fn mcp(&self) -> Mcp {
        Mcp::new(self.api(), true, false)
    }
    async fn online(&self) -> Vec<String> {
        let api = self.api();
        let ov = blocking(move || api.overview().unwrap()).await;
        ov.machines
            .iter()
            .filter(|m| m.online)
            .map(|m| m.machine.id.clone())
            .collect()
    }
}

/// A real helper with the demo desktop (its shell and files are this computer's).
async fn real_helper(s: &TestServer) -> String {
    let dir = farlink::util::unique_temp_dir("farlink-mcp-helper");
    let cfg = HostConfig {
        server_url: s.url.clone(),
        enroll_token: Some(s.relay.enroll_token()),
        machine_id: None,
        secret: None,
    };
    tokio::spawn(run_agent(
        dir.join("host.json"),
        cfg,
        HostOptions {
            demo: true,
            no_update: true,
        },
    ));
    for _ in 0..100 {
        if let Some(id) = s.online().await.into_iter().next() {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("helper never came online");
}

/// One tool call through the JSON-RPC front door: (text of the answer,
/// isError, the whole result).
fn call(m: &mut Mcp, tool: &str, args: Value) -> (String, bool, Value) {
    let req = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}});
    let r: Value = serde_json::from_str(&m.handle_line(&req.to_string()).unwrap()).unwrap();
    let result = r["result"].clone();
    let text: String = result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (text, result["isError"].as_bool().unwrap(), result)
}

fn ok(m: &mut Mcp, tool: &str, args: Value) -> String {
    let (t, err, _) = call(m, tool, args.clone());
    assert!(!err, "{tool} {args}: {t}");
    t
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_files_and_screenshots_on_a_real_helper() {
    let s = start_server().await;
    let id = real_helper(&s).await;
    let mut m = s.mcp();
    blocking(move || {
        let list = ok(&mut m, "list_computers", json!({}));
        assert!(
            list.contains(&format!("id: {id}")) && list.contains("online"),
            "{list}"
        );

        // Shell: output, exit code, standard input, the folder kept.
        let out = ok(
            &mut m,
            "run_command",
            json!({"computer": id, "command": "echo hi; echo oops >&2; exit 3"}),
        );
        // Standard output and error come on their own pipes: either may be first.
        let (output, tail) = out
            .split_once("[exit code 3]\n[folder: ")
            .unwrap_or_else(|| panic!("{out}"));
        let mut lines: Vec<&str> = output.lines().collect();
        lines.sort();
        assert_eq!(
            (lines, tail.is_empty()),
            (vec!["hi", "oops"], false),
            "{out}"
        );
        let out = ok(
            &mut m,
            "run_command",
            json!({"computer": id, "command": "tr a-z A-Z", "stdin": "abc\n"}),
        );
        assert!(out.starts_with("ABC\n"), "{out}");
        let out = ok(
            &mut m,
            "run_command",
            json!({"computer": id, "command": "cat"}),
        );
        assert!(
            out.starts_with("[exit code 0]\n[folder: "),
            "nobody types: cat ends at once: {out}"
        );
        let dir = farlink::util::unique_temp_dir("farlink-mcp-files")
            .canonicalize()
            .unwrap();
        let d = dir.display().to_string();
        ok(
            &mut m,
            "run_command",
            json!({"computer": id, "command": format!("cd '{d}'")}),
        );
        let out = ok(
            &mut m,
            "run_command",
            json!({"computer": id, "command": "pwd"}),
        );
        assert_eq!(out, format!("{d}\n[exit code 0]\n[folder: {d}]"));
        // Too slow: stopped, and the next command still runs.
        let out = ok(
            &mut m,
            "run_command",
            json!({"computer": id, "command": "echo started; sleep 30", "timeout_secs": 1}),
        );
        assert!(out.contains("started\n[stopped after 1 s]"), "{out}");
        let out = ok(
            &mut m,
            "run_command",
            json!({"computer": id, "command": "echo again"}),
        );
        assert!(out.starts_with("again\n"), "{out}");

        // Files: write (never overwrites), read, list, new folder.
        let out = ok(
            &mut m,
            "write_file",
            json!({"computer": id, "folder": d, "name": "a.txt", "content": "first\nпривет\n"}),
        );
        assert_eq!(out, format!("Wrote {d}/a.txt"));
        let out = ok(
            &mut m,
            "write_file",
            json!({"computer": id, "folder": d, "name": "a.txt", "content": "second"}),
        );
        assert!(
            out.contains("a (2).txt") && out.contains("was there already"),
            "{out}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "first\nпривет\n"
        );
        assert_eq!(
            ok(
                &mut m,
                "read_file",
                json!({"computer": id, "path": format!("{d}/a.txt")})
            ),
            "first\nпривет\n"
        );
        ok(
            &mut m,
            "make_folder",
            json!({"computer": id, "path": format!("{d}/sub")}),
        );
        assert!(dir.join("sub").is_dir());
        let (t, err, _) = call(
            &mut m,
            "make_folder",
            json!({"computer": id, "path": format!("{d}/a.txt/inside")}),
        );
        assert!(err, "{t}");
        let listing = ok(&mut m, "list_files", json!({"computer": id, "path": d}));
        assert!(
            listing.contains(&format!("Folder: {d}"))
                && listing.contains("a (2).txt")
                && listing.contains("[dir]  sub"),
            "{listing}"
        );
        let (t, err, _) = call(
            &mut m,
            "read_file",
            json!({"computer": id, "path": format!("{d}/missing.txt")}),
        );
        assert!(err && t.contains("does not exist"), "{t}");
        let (t, err, _) = call(
            &mut m,
            "read_file",
            json!({"computer": id, "path": format!("{d}/sub")}),
        );
        assert!(err && t.contains("folder"), "{t}");
        std::fs::write(dir.join("bin.dat"), [0xffu8, 0xfe, 0]).unwrap();
        let (t, err, _) = call(
            &mut m,
            "read_file",
            json!({"computer": id, "path": format!("{d}/bin.dat")}),
        );
        assert!(err && t.contains("not text"), "{t}");

        // Upload from "here" and download back.
        let here = farlink::util::unique_temp_dir("farlink-mcp-here");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(here.join("big.bin"), &data).unwrap();
        let out = ok(
            &mut m,
            "upload_file",
            json!({"computer": id, "local_path": here.join("big.bin"), "folder": d}),
        );
        assert!(out.ends_with(&format!("{d}/big.bin")), "{out}");
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), data);
        let back = here.join("back");
        std::fs::create_dir(&back).unwrap();
        let out = ok(
            &mut m,
            "download_file",
            json!({"computer": id, "path": format!("{d}/big.bin"), "local_folder": back}),
        );
        assert!(out.contains("300000 bytes"), "{out}");
        assert_eq!(std::fs::read(back.join("big.bin")).unwrap(), data);
        let (t, err, _) = call(
            &mut m,
            "upload_file",
            json!({"computer": id, "local_path": here.join("nope"), "folder": d}),
        );
        assert!(err && t.contains("not a file"), "{t}");

        // Screenshots: scaled, and their size is what clicks are measured in.
        let (t, err, result) = call(
            &mut m,
            "screenshot",
            json!({"computer": id, "screen": 2, "max_size": 640}),
        );
        assert!(!err, "{t}");
        assert!(
            t.contains("screen 2 of 2 (1280×1024), picture 640×512"),
            "{t}"
        );
        let img = &result["content"][0];
        assert_eq!(img["mimeType"], "image/jpeg");
        let jpeg = farlink::util::base64_decode(img["data"].as_str().unwrap()).unwrap();
        let f = farlink::codec::decode_image(&jpeg).unwrap();
        assert_eq!((f.width, f.height), (640, 512));
        let (t, err, _) = call(&mut m, "screenshot", json!({"computer": id, "screen": 5}));
        assert!(err && t.contains("no screen 5"), "{t}");
        let (t, err, _) = call(&mut m, "screenshot", json!({"computer": "no-such-pc"}));
        assert!(err && t.contains("No computer"), "{t}");
    })
    .await;
}

/// A scripted host: registers, then hands over its session socket.
async fn fake_host(
    s: &TestServer,
) -> (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
    String,
) {
    let ws_url = |p: &str| farlink::util::ws_url(&s.url, p);
    let (mut ws, _) = connect_async(ws_url("/ws/agent")).await.unwrap();
    let reg = AgentMsg::Register {
        enroll_token: Some(s.relay.enroll_token()),
        machine_id: None,
        secret: None,
        hostname: "SRV2016".into(),
        os: "Windows Server 2016".into(),
        version: farlink::VERSION.into(),
        target: "x86_64-pc-windows-gnu".into(),
        monitors: farlink::desktop::TestPattern::dual().monitors,
    };
    ws.send(Message::Text(serde_json::to_string(&reg).unwrap().into()))
        .await
        .unwrap();
    loop {
        let Some(Ok(Message::Text(t))) = ws.next().await else {
            panic!("agent socket closed")
        };
        if let Ok(AgentMsg::Registered {
            machine_id, secret, ..
        }) = serde_json::from_str(&t)
        {
            return (ws, machine_id, secret);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mouse_and_keys_arrive_where_the_screenshot_said() {
    let s = start_server().await;
    let (mut agent, id, secret) = fake_host(&s).await;
    let mut m = s.mcp();
    let mid = id.clone();
    let calls = tokio::task::spawn_blocking(move || {
        // No screenshot yet: coordinates are those of a default one (1600×900 → 1280×720).
        ok(
            &mut m,
            "click",
            json!({"computer": "SRV2016", "x": 639.5, "y": 359.5, "button": "right"}),
        );
        ok(
            &mut m,
            "press_keys",
            json!({"computer": mid, "keys": "ctrl+shift+escape ctrl+alt+del"}),
        );
        ok(
            &mut m,
            "type_text",
            json!({"computer": mid, "text": "hi\nthere"}),
        );
        ok(
            &mut m,
            "scroll",
            json!({"computer": mid, "screen": 2, "x": 0, "y": 0, "amount": -2}),
        );
        let (t, err, _) = call(
            &mut m,
            "press_keys",
            json!({"computer": mid, "keys": "ctrl+nope"}),
        );
        assert!(err && t.contains("Unknown key"), "{t}");
    });

    // The relay asks the host to open the session; it says hello.
    let session_id = loop {
        let Some(Ok(Message::Text(t))) = agent.next().await else {
            panic!("agent socket closed")
        };
        if let Ok(AgentMsg::OpenSession {
            session_id,
            view_only,
            ..
        }) = serde_json::from_str(&t)
        {
            assert!(!view_only, "a control session");
            break session_id;
        }
    };
    let url = farlink::util::ws_url(
        &s.url,
        &format!("/ws/host-session/{session_id}?machine={id}&secret={secret}"),
    );
    let (mut host, _) = connect_async(url).await.unwrap();
    let hello = SessionMsg::Hello {
        hostname: "SRV2016".into(),
        os: "Windows Server 2016".into(),
        version: farlink::VERSION.into(),
        monitors: farlink::desktop::TestPattern::dual().monitors,
        view_only: false,
    };
    host.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .unwrap();

    let mut got = Vec::new();
    let mut watched = None;
    while got.len() < 16 {
        let msg = tokio::time::timeout(Duration::from_secs(10), host.next())
            .await
            .expect("input never came")
            .unwrap()
            .unwrap();
        let Message::Text(t) = msg else { continue };
        match serde_json::from_str::<SessionMsg>(&t) {
            Ok(SessionMsg::Input { event }) => got.push(event),
            Ok(SessionMsg::Watch { monitors }) => watched = Some(monitors),
            _ => {}
        }
    }
    calls.await.unwrap();
    assert_eq!(
        watched,
        Some(vec![]),
        "no screens captured for the assistant"
    );
    let InputEvent::MouseMove { mon: 0, x, y } = got[0] else {
        panic!("{got:?}")
    };
    assert!(
        (x - 0.5).abs() < 0.001 && (y - 0.5).abs() < 0.001,
        "{x} {y}"
    );
    assert!(matches!(
        got[1],
        InputEvent::MouseButton {
            mon: 0,
            button: MouseButton::Right,
            down: true,
            ..
        }
    ));
    assert!(matches!(
        got[2],
        InputEvent::MouseButton {
            button: MouseButton::Right,
            down: false,
            ..
        }
    ));
    let keys: Vec<(String, bool)> = got[3..9]
        .iter()
        .map(|e| match e {
            InputEvent::Key { key, down } => (key.clone(), *down),
            other => panic!("{other:?}"),
        })
        .collect();
    let k = |n: &str, d| (n.to_string(), d);
    assert_eq!(
        keys,
        [
            k("Control", true),
            k("Shift", true),
            k("Escape", true),
            k("Escape", false),
            k("Shift", false),
            k("Control", false)
        ]
    );
    assert_eq!(got[9], InputEvent::CtrlAltDel);
    assert_eq!(got[10], InputEvent::Text { text: "hi".into() });
    assert_eq!(
        got[11],
        InputEvent::Key {
            key: "Enter".into(),
            down: true
        }
    );
    assert_eq!(
        got[13],
        InputEvent::Text {
            text: "there".into()
        }
    );
    // Screen 2 is 1280×1024: a default screenshot of it is that big too.
    let InputEvent::MouseMove { mon: 1, x, y } = got[14] else {
        panic!("{got:?}")
    };
    assert!(
        (x - 0.5 / 1280.0).abs() < 1e-6 && (y - 0.5 / 1024.0).abs() < 1e-6,
        "{x} {y}"
    );
    assert_eq!(got[15], InputEvent::Scroll { dx: 0.0, dy: -2.0 });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_gets_their_own_computers_and_no_shell() {
    let s = start_server().await;
    let (_agent, id, _secret) = fake_host(&s).await;
    let (base, admin) = (s.url.clone(), s.admin.clone());
    blocking(move || {
        ureq::post(&format!("{base}/api/users"))
            .header("Authorization", &format!("Bearer {admin}"))
            .send_json(json!({"name": "Anna", "login": "anna", "password": "anna-pass-1"}))
            .unwrap();
        let login = Login {
            server: base.clone(),
            user: "anna".into(),
            password: Some("anna-pass-1".into()),
            token: None,
        };
        let mut anna = Mcp::connect(login, false).unwrap();
        let names: Vec<String> = anna
            .tools()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(
            !names.contains(&"run_command".to_string())
                && names.contains(&"screenshot".to_string()),
            "{names:?}"
        );
        assert!(ok(&mut anna, "list_computers", json!({})).contains("No computers yet"));
        let (t, err, _) = call(&mut anna, "screenshot", json!({"computer": id}));
        assert!(
            err && t.contains("No computer"),
            "the administrator's computer is not hers: {t}"
        );
        let (t, err, _) = call(
            &mut anna,
            "run_command",
            json!({"computer": id, "command": "id"}),
        );
        assert!(err && t.contains("administrator"), "{t}");

        let bad = Login {
            server: base,
            user: String::new(),
            password: Some("wrong".into()),
            token: None,
        };
        assert!(Mcp::connect(bad, false).is_err());
    })
    .await;
}

/// The program itself: `farlink mcp` signs in from the environment and
/// answers on stdout, one message per line.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn farlink_mcp_speaks_json_rpc_on_stdio() {
    use std::io::{BufRead, BufReader, Write};
    let s = start_server().await;
    let (_agent, _id, _secret) = fake_host(&s).await;
    let (url, password) = (s.url.clone(), s.password.clone());
    blocking(move || {
        let empty = farlink::util::unique_temp_dir("farlink-mcp-cfg");
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_farlink"))
            .args(["mcp", "--server", &url, "--read-only"])
            .env("FARLINK_PASSWORD", &password)
            .env("FARLINK_VIEWER_DIR", &empty)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        // `before`: notifications sent ahead of the request (they get no answer).
        let mut ask = |before: &[&str], req: Value| -> Value {
            for n in before {
                writeln!(stdin, "{n}").unwrap();
            }
            writeln!(stdin, "{req}").unwrap();
            serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap()
        };
        let init = ask(&[], json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}));
        assert_eq!(init["result"]["serverInfo"]["version"], farlink::VERSION);
        let tools = ask(&[r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#], json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}));
        let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"list_computers") && !names.contains(&"click") && !names.contains(&"run_command"), "read-only: {names:?}");
        let list = ask(&[], json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_computers","arguments":{}}}));
        assert_eq!(list["id"], 2);
        assert!(list["result"]["content"][0]["text"].as_str().unwrap().contains("SRV2016"), "{list}");
        drop(ask);
        drop(stdin);
        assert!(child.wait().unwrap().success(), "ends when stdin ends");

        // Without any sign-in it explains what is missing.
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_farlink"))
            .args(["mcp", "--server", &url])
            .env_remove("FARLINK_PASSWORD")
            .env_remove("FARLINK_TOKEN")
            .env("FARLINK_VIEWER_DIR", &empty)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("FARLINK_PASSWORD"), "{}", String::from_utf8_lossy(&out.stderr));
    })
    .await;
}
