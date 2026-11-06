# AGENTS.md — guide for AI coding agents (and humans) working on Farlink

Farlink is a self-hosted remote desktop system written in Rust: **one crate, three programs, one
shared library**. Read this first, then jump straight to the file you need.

## The three programs

| Binary | Entry point | Runs on | Role |
|---|---|---|---|
| `farlink` | `src/main.rs` → `viewer::run` | admin's / person's PC | Desktop viewer (egui/eframe): machine list, one native window per remote monitor, quality, files, temporary links |
| `farlink-server` | `src/bin/farlink-server.rs` → `server` | public Linux server | Relay + web admin + install links + update feed + people/grants (axum) |
| `farlink-host` | `src/bin/farlink-host.rs` → `host` | every remote computer | Helper: dials out to the relay only, streams screens, injects input, files, self-update; Windows SYSTEM service |

Data flow: host keeps a control WebSocket `/ws/agent` to the relay → a viewer opens `/ws/view/{machine}`
(signed in) or `/ws/guest/{token}` (temporary link) → relay sends `OpenSession` to the host → host opens
`/ws/host-session/{id}` → relay pipes viewer ⇄ host, enforcing view-only, files and access rules.

## Where things are (src/)

| File | What's in it | Typical reasons to edit |
|---|---|---|
| `proto.rs` | **Wire protocol**: `SessionMsg` (viewer⇄host JSON), `AgentMsg` (host⇄relay), `TileHeader` (binary pictures, magic `T`), `FileChunk` (binary file pieces, magic `F`), `MonitorInfo`, `InputEvent` | Any new feature that crosses the network. JSON uses `#[serde(tag = "t", rename_all = "snake_case")]` — the browser viewer (`web/viewer.js`) relies on these exact names |
| `server.rs` | axum routes (`router()`), REST API `/api/*`, pages, WebSocket handlers, `viewer_loop` (the relay pipe + view-only/mode enforcement), `overview_for` (what admin vs person sees) | API endpoints, permissions, relay behaviour |
| `grants.rs` | **Server state** `Store` (saved as `state.json`): admin password + sessions, `Machine`, `User` (people), `Grant` (temporary links), `Who` (admin/user), `can_access`, enrolment tokens | Accounts, access rules, link rules. Pure logic, well unit-tested |
| `host.rs` | Helper agent loop (`run_agent`), `run_session`, `screen_loop` (capture → prepare → diff → encode, flow control, idle pacing `IdleTracker`, watched monitors), `FileLogger`, Windows `service` module (SYSTEM service, session spawn, SendSAS) | Streaming performance, host behaviour |
| `codec.rs` | `Frame` (RGBA), scaling, colour reduction, 64×64 tile diff → strips, JPEG/PNG encode, `MonitorEncoder` (host side), `Canvas` (viewer side) | Picture quality / bandwidth |
| `quality.rs` | `Quality` settings + presets (Fast/Balanced/Sharp), clamping | Quality options |
| `desktop/mod.rs` | `Desktop` trait (monitors, capture, input, `changed`), `TestPattern` fake 2-monitor desktop (demo + tests), coordinate helpers, cursor drawing | New platform features |
| `desktop/x11.rs` | Ubuntu/X11: RandR, GetImage, XFixes cursor, XTest input, XDamage change detection | Linux capture/input |
| `desktop/windows.rs` | Windows: GDI capture, SendInput, follows the input desktop (lock screen/UAC), Ctrl+Alt+Del | Windows capture/input (only cross-compiled here, never run) |
| `tls.rs` | Built-in HTTPS for the relay: Let's Encrypt via `rustls-acme` (TLS-ALPN-01 on 443, cache in `<data>/acme`) or PEM files (any port), `TlsListener` for `axum::serve`, `run` (binds per `ServerConfig.tls`) | Certificates, ports |
| `files.rs` | Host-side file service: list folders, upload/download in chunks, safe names, never overwrite | File transfer |
| `net.rs` | Viewer networking: `Api` (blocking REST via ureq), `SessionHandle`/`SessionShared` (live session state, transfers, mode), demo session | Desktop app ⇄ server |
| `viewer.rs` | **Desktop UI** (≈2000 lines). Sections, in order: colours/style · jobs · `OpenSession` · input mapping (`map_events`) · `ViewerApp` state + `sample_overview` · top bar · bottom bar (connections) · machine list (right-click rename) · rename dialog · central/login/details/grant/screens/quality cards · monitor windows · screenshot · `monitor_window` · file browser (`FilesView`, `files_window`) · widgets · tests | Anything visible in the app |
| `layout.rs` | Remembered window positions per machine+monitor (`layouts.json`) | Window placement |
| `config.rs` | Settings files (host/viewer/server), atomic JSON save | New settings |
| `install.rs` | Install plans as data (`Step`s): Windows service + firewall, Linux user service, server (HTTPS itself: `--port`, auto cert on 443 or cert files; removes old Caddy site); `execute` | Installers |
| `server_update.rs` | Server follows GitHub Releases (`update_from_github`): hourly check, verify assets against `SHA256SUMS.txt`, replace own exe, publish computer programs, re-exec in place (loop guard `FARLINK_UPDATED_TO`). Asset names must match `build.yml` | Server auto-update |
| `update.rs` | Self-update: check, download, SHA-256 verify, swap exe | Updates |
| `keys.rs` | Key names ⇄ X11 keysyms / Windows VK codes | Keyboard |
| `util.rs`, `version.rs` | Small helpers, CLI `Args`, version compare | — |
| `web/` | Browser UI embedded with `include_str!`: `admin.html/js` (web admin + person view), `viewer.html/js` (in-browser viewer + files panel), `install.html`, `message.html`, `app.css` | Web UI (plain JS, no build step) |

Tests: unit tests live at the bottom of each module; `tests/end_to_end.rs` starts a real relay on a
random port with real/fake hosts and viewers (people access, links, files, view-only switching,
updates, rename).

## Commands

```sh
make test            # all tests (must stay green)
make check           # compile-check Linux + Windows (cross)
cargo test -- --ignored x11_capture   # real X11 capture on this machine (needs a display)
cargo run --bin farlink -- --demo     # viewer with sample computers, no server
cargo run --bin farlink -- --screenshot shots/now.png   # renders ~120 frames, saves PNG, exits
FARLINK_SHOT_FILES=1 / FARLINK_SHOT_RENAME=1  (with --screenshot)  # screenshot the file browser / rename dialog
make all             # release builds Linux + Windows → dist/
```

Local end-to-end by hand:
```sh
cargo run --bin farlink-server -- serve --url http://127.0.0.1:8470 --bind 127.0.0.1:8470 --data ./data
cargo run --bin farlink-host -- run --demo --verbose --server http://127.0.0.1:8470 --token <token from the web page>
```

## Rules and gotchas

* **Never trust the viewer.** Permissions are enforced on the relay (`server.rs`: `who`, `can_access`,
  `viewer_message_allowed`, `viewer_loop`) *and* again on the host for files/input. A view-only
  temporary link is `locked`: it can never switch to control.
* People (`Who::User`) see only machines whose `owner` is them; admin-only API calls check `is_admin`.
  Renaming computers is admin-only.
* Adding a `SessionMsg` variant: update `is_control()` / `is_file_request()` if it touches the remote
  machine, `viewer_message_allowed` if the relay must not forward it, and `web/viewer.js` if browsers use it.
* The host's screen thread is blocking (`std::sync::mpsc` in, `tokio::mpsc` out); keep heavy work off
  the async runtime.
* egui is **0.37**: `egui::Panel::top/left/bottom(...).show(ui, ..)`, `App::ui(&mut self, ui, frame)`,
  `ui.close()` for menus, `Id::unique`. Check `~/.cargo/registry/src/*/egui-0.37.0/` before guessing APIs.
* Edition 2024 with let-chains. Dev profile has no debug info (disk on the dev machine is tight).
* Windows code compiles via `make check` but has **never run on real Windows** — test there before
  claiming it works. Ubuntu is X11 only (no Wayland).
* Release layout the server expects: `releases/version.txt` + `releases/<rust-target>/<program>`;
  `util::current_target()` must match the folder names (`x86_64-pc-windows-gnu`, not msvc).

## CI/CD

`.github/workflows/build.yml` (this folder is meant to be the root of the GitHub repo): tests on every
push/PR (Linux + Windows), release builds for Linux x86_64, Linux ARM64, Windows x86_64; pushing a tag
`vX.Y.Z` (must equal `version` in `Cargo.toml`) creates a GitHub Release with the programs, the ready
`farlink-X.Y.Z.tar.gz` (dist layout) and `SHA256SUMS.txt`.

Releasing: bump `version` in `Cargo.toml` → `cargo build` (updates Cargo.lock) → commit →
`git tag v0.2.0 && git push origin main v0.2.0`.
The project skill `.claude/skills/release` (`/release [patch|minor|major|X.Y.Z]`) does all of
this with the safety checks.
