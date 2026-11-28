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
Then (desktop app only, relay `stun_port` ≠ 0) both sides try a **direct QUIC path** (`direct.rs`) and move
their messages onto it; the relay socket stays open as the authority (mode, end of session) and the
fallback whenever the direct path can't be made or breaks.

## Where things are (src/)

| File | What's in it | Typical reasons to edit |
|---|---|---|
| `proto.rs` | **Wire protocol**: `SessionMsg` (viewer⇄host JSON), `AgentMsg` (host⇄relay), `TileHeader` (binary pictures, magic `T`), `FileChunk` (binary file pieces, magic `F`), `MonitorInfo`, `InputEvent`, `Preview`/`PreviewShot` (small base64 JPEGs per screen, `AgentMsg::PreviewRequest`/`Preview`), `AgentMsg::UpdateAvailable { force }` / `UpdateFailed` (forced helper updates), `SessionMsg::Clipboard` (copied text, both ways, control only), `PointerMode`/`PointerShape`/`Pointer` (viewer draws the mouse pointer itself), `SessionMsg::Timing` (host → viewer once a second: capture/compress ms per picture + picture→ack round trip), `SessionMsg::ShellRun`/`ShellInput`/`ShellStop` → `ShellOutput`/`ShellDone` (remote shell, `is_shell_request`), `AgentMsg::OpenSession.shell` (relay: the viewer is the administrator) | Any new feature that crosses the network. JSON uses `#[serde(tag = "t", rename_all = "snake_case")]` — the browser viewer (`web/viewer.js`) relies on these exact names |
| `server.rs` | axum routes (`router()`), REST API `/api/*`, pages, WebSocket handlers, `viewer_loop` (the relay pipe + view-only/mode enforcement), `overview_for` (what admin vs person sees), `Relay::preview` + `GET /api/machines/{id}/preview` (asks the host over `/ws/agent`, 3 s cache, only answers from the asked machine count), `GET /api/machines/{id}/screenshot/{index}` (same path with `PreviewRequest.full`: one monitor at full size, lossless PNG via `codec::screenshot_image`, never cached; `/ws/agent` takes messages up to 24 MB for it), `POST /api/machines/{id}/notes` (admin; `Machine.notes`, hidden from people in `overview_for`), `Relay::force_update`/`force_update_all` + `POST /api/machines/{id}/update`, `/api/machines/update-all` (admin; `MachineView.updating`/`update_error`, kept in memory only) | API endpoints, permissions, relay behaviour |
| `grants.rs` | **Server state** `Store` (saved as `state.json`): admin password + sessions, `Machine`, `User` (people), `Grant` (temporary links), `Who` (admin/user), `can_access`, enrolment tokens (`enroll_created` = when the general install link was made → `Overview.install_created`, "made … ago" in the web admin) | Accounts, access rules, link rules. Pure logic, well unit-tested |
| `guard.rs` | `LoginGuard`: failed sign-ins per client address (10 / 15 min) and per sign-in name (30 / 15 min) → `/api/login` answers 429 without checking; `client_key` (IPv6 by /64). `server.rs` reads the address via `ClientAddr` (connect info; `X-Forwarded-For` only from a loopback proxy) and checks passwords outside the store lock, 2 at a time (`login_slots`) | Sign-in limits |
| `host.rs` | Helper agent loop (`run_agent`), `run_session`, `screen_loop`/`run_screen` (waits on viewer messages, not sleep: input is injected the moment it arrives and the screen is looked at again `INPUT_FOLLOW_UP` later; capture → prepare → diff → encode, after input the screen is watched closely for `INPUT_BURST` (`look_interval`: cheap change checks every 8 ms, pictures 16 ms apart); far viewers (`FlowControl::far`, round trip ≥ 40 ms) and mostly idle computers (`SpareCpu`: ≥ 4 cores, on below 40 % CPU, off above 70 %, via `sysstat::CpuLoad`) get lossless parts compressed harder (`MonitorEncoder::tight`); adaptive flow control `FlowControl` (2–8 pictures on the way, grows while the round trip stays low, shrinks when it rises; viewers ack on arrival via `net::ack_for`), idle pacing `IdleTracker`, `AutoPace` (`Quality::auto_fps`, default on: pictures per second start at 10, ×0.8 gap every 0.5 s while acks come back without queueing, ×1.5 when `FlowControl::take_pressure` says they piled up, 4–`max_fps` per second, gap ≥ 2× capture+compress; pictures after input at most twice as fast; reported as `Timing.pace_fps`), watched monitors; `PointerSender` sends pointer shape/position apart from pictures; `TimingStats` → `SessionMsg::Timing`, round trip from `FlowControl::take_rtt`; `HeldInput` = keys/buttons pressed and not let go, released by `release_held` when the direct path breaks, the session turns view-only or ends; `ViewerInput` = viewer messages from either path, `relay_only` ones are never taken from the direct path), `preview_worker`/`preview_shots` (screen previews on their own thread, desktop kept open 60 s), `FileLogger`, Windows `service` module (SYSTEM service, session spawn, SendSAS) | Streaming performance, host behaviour |
| `codec.rs` | `Frame` (RGBA), scaling, colour reduction, 64×64 tile diff → strips, encode (`Codec::Auto`: ≤256 *real* colours → lossless (reduced to the chosen colours), else JPEG via `jpeg-encoder` (greyscale → 1-channel JPEG; 16-bit only affects lossless parts); colours are reduced per part in `encode_part`, `prepare` only scales; strips encoded in parallel), `MonitorEncoder` (host side; `refine` re-sends rough JPEG parts sharp after `REFINE_AFTER` still time), `Canvas` (viewer side, `take_dirty` for partial texture uploads) | Picture quality / bandwidth |
| `palette.rs` | "Colour optimised" (`ColorDepth::Optimized`, the default): `PaletteLearner` samples captured pictures, builds a `Palette` of ≤256 of the screen's own colours (common ones exact, the rest by median cut), rebuilds every 4 s on a background thread and switches only when clearly better (the host then re-sends the screen once it is still); `Palette::map` → palette PNG for a part unless too many pixels are far from the palette (photos/video then go as full colour: JPEG in Auto, exact in PNG). Host-side only: viewers just decode PNGs | Colour approximation, palette size |
| `quality.rs` | `Quality` settings + presets (Fast/Balanced/Sharp), clamping; `auto_fps` (missing = on) makes `max_fps` only the upper limit | Quality options |
| `desktop/mod.rs` | `Desktop` trait (monitors, capture, input, `changed`, `pointer`/`pointer_image`/`set_pointer_in_picture`), `PointerImage`, `TestPattern` fake 2-monitor desktop (demo + tests), coordinate helpers, cursor drawing | New platform features |
| `desktop/x11.rs` | Ubuntu/X11: RandR, GetImage, XFixes cursor, XTest input, XDamage change detection | Linux capture/input |
| `desktop/windows.rs` | Windows: capture via DXGI (falls back to reused GDI DIB surfaces), pointer drawn with `DrawIconEx`, SendInput, follows the input desktop (lock screen/UAC; a switch restarts DXGI), Ctrl+Alt+Del, `cursor_picture` (pointer shape via black/white `DrawIconEx`) | Windows capture/input (only cross-compiled here, never run) |
| `desktop/dxgi.rs` | DXGI Desktop Duplication (`windows` crate): per-monitor `Output::poll` copies only dirty/moved rects into an RGBA frame; `changed()` is real on Windows | Windows capture performance |
| `direct.rs` | **Direct (peer-to-peer) path**: `DirectEndpoint` (one UDP socket, STUN via `stun.rs` against the relay + `extra_stun_servers` → `hard_nat` = LAN address only; throw-away rcgen cert per session, `Pinned` verifiers: mutual TLS on the fingerprints swapped through the relay), `establish` (both sides QUIC client *and* server to all peer addresses; the viewer opens the stream, the host confirms it), `Link` = same `Message`s framed on one bi stream (`LinkTx`/`LinkRx`). `DirectState` = per-session state machine used by `host::run_session` and `net::network_task`: `allowed` (`DirectInfo`) → `bound` (send `DirectOffer`) → `offer` → `linked` (send `PathSwitch{direct:true}` on the relay = last relay message of that direction, so nothing reorders) → `send`/`recv`; `broken` = back to relay (`PathSwitch{false}`, viewer sends `Refresh`, host forgets pictures in flight and lets go of held keys/buttons). Attempts are numbered (`DirectOffer.attempt`): only the viewer starts new ones (`retry` after `RETRY_AFTER` 5 s / 30 s / 120 s, following a failed attempt or a break); a newer offer makes the host drop its old path and follow; stale `Event`s are ignored; `no_answer` names a helper too old to answer. `candidates`: only answers from public STUN servers decide `hard_nat` (a relay on the LAN sees the LAN address). `why_not` = reason shown in the viewer. Keep-alive 1 s, `IDLE_TIMEOUT` 4 s, relay socket pinged every `RELAY_PING` | NAT traversal, path switching |
| `stun.rs` | Minimal STUN Binding (RFC 5389, XOR-MAPPED-ADDRESS): `query_many` (several servers from one socket), `hard_nat` (servers saw different addresses), `serve` (relay, UDP `ServerConfig.stun_port`, default 3478, `0` = no direct connections) | Address discovery |
| `tls.rs` | Built-in HTTPS for the relay: Let's Encrypt via `rustls-acme` (TLS-ALPN-01 on 443, cache in `<data>/acme`) or PEM files (any port), `TlsListener` for `axum::serve`, `run` (binds per `ServerConfig.tls`) | Certificates, ports |
| `sysstat.rs` | Host system info: `SystemStats` (CPU % since the last sample, cores, RAM, disks with free space, `user` signed in (Windows: console session via WTS, else any active one; `DOMAIN\user` for domain accounts; Linux: the helper's user), `joined` = Windows `Workgroup`/`Domain` via `NetGetJoinInformation`; `user_line` = the list line) sent in every `AgentMsg::Heartbeat { stats }` (first one right after connecting, then every 30 s; kept in relay memory on `AgentConn`, shown as `MachineView.stats` while online; `ifaces` = `NetIface` name + addresses (`getifaddrs` / `GetAdaptersAddresses`; no loopback, link-local or container/VM bridges: `container_link`) → System card, next to the name in the list (`list_addrs`) and in the list search (`matches_search`, online only); `net` = `NetRate` bytes/s in/out since the last sample (real cards only: Linux `/proc/net/dev` minus `/sys/devices/virtual/net`, Windows `GetIfTable2` hardware non-filter interfaces); `LoadHistory` on `Relay.load` keeps CPU/memory %/network of the last 10 min per machine (survives a helper restart) → `MachineView.load` → `viewer.rs` `line_chart` (two charts in the System card: CPU and memory, network with `rate_axis_max`)); `os_version` (Windows: registry, "Windows 11 Pro 23H2 (build 22631)"; Linux: os-release), used by `util::os_label` | Machine info in the list |
| `ping.rs` | Ctrl+Shift+click "ping" (`InputEvent::Ping`, control only): `render` draws the dot + 3 fading waves (1.1 s, premultiplied ARGB, radius from the monitor height); `show` runs it in a click-through overlay on its own thread (X11: override-redirect ARGB window when composited, else shaped window; Windows: layered `WS_EX_TRANSPARENT` window on the input desktop, never run on real Windows); `draw_into` for the demo desktop. Real-X test: `cargo test -- --ignored x11_ping` | Ping look/length |
| `clipboard.rs` | Shared copied text: `ClipboardSync` (`last` stops echoes, `\r\n`-tolerant), process-wide arboard clipboard (`system_get/set`), demo clipboard for tests, host `poll_loop` (500 ms, nothing while view-only). Host applies incoming text before forwarding later input; relay drops it both ways for view-only (`host_message_allowed`); viewer sends on Ctrl+V / window focus, browser on `paste` | Clipboard |
| `shell.rs` | Remote shell (administrator only): `ShellService` per session (`allowed` = `OpenSession.shell`; refuses otherwise, also on the direct path), one command at a time via `bash -c` / `cmd /C` / PowerShell (`Shell::here`, `-EncodedCommand` = `powershell_encoded`), in the session's folder; `cd` runs as `cd … && pwd` (`is_cd`, `script`) and the printed folder becomes the new one; output streamed (`pump`, `take_text` keeps UTF-8 whole), `ShellStop`/view-only/session end kill the whole process tree (`kill_tree`). Never logs or keeps command text | Shell behaviour |
| `files.rs` | Host-side file service: list folders, upload/download in chunks, safe names, never overwrite | File transfer |
| `net.rs` | Viewer networking: `Api` (blocking REST via ureq), `SessionHandle`/`SessionShared` (live session state, transfers, mode; `Latency` = host timing + decode + arrival→drawn (`drawn`, called from `update_textures`), `Route` = via relay (+ `why`) / direct (LAN or p2p) / demo; `network_task` drives `direct::DirectState`, acks before decoding; `ShellLog` + `shell_run`/`shell_input`/`shell_stop`), demo session (its files and shell are this computer's) | Desktop app ⇄ server |
| `viewer.rs` | **Desktop UI** (≈2000 lines). Sections, in order: colours/style (`TEXT_SCALE`: all fonts drawn 20 % bigger) · jobs · `OpenSession` · input mapping (`map_events`) · `ViewerApp` state + `sample_overview` · `size_main_window` (once at start, after the title bar is known: full screen height, width ≥ 40 % of the screen via `main_window_size`) · status bar (app update · relay · account + Sign out on the right) · open connections (Files-only sessions: `files_only`, no screen windows; shell sessions: `shell` = `ShellView`, own connection each, not replaced by Control) · `ShellView` + `shell_window` (admin "Shell" button in the details card; transcript = `net::ShellLog` (`MAX_SHELL_TEXT`, `clean_output` strips terminal codes), Up/Down history in memory only, Stop/Ctrl+C on an empty line, Ctrl+D ends input, Windows: cmd/PowerShell switch) · machine list (double-click = Control, right-click rename/notes) · rename dialog · `preview_step` (selected online computer without open session: pictures every 5 s, drawn in the screens card; until they come `loading_animation` runs over the computer's last fully loaded preview, faded: `preview_memory`, RAM only) · central/login/details/grant/screens/system cards (screen previews: double-click connects, right-click → "Take screenshot" (`save_screenshot`: PNG in the temp folder, from an open connection's picture, else asked from the helper at full size via `Api::screenshot`) or forget that window's place) · `notes_window` (own window from the list's right-click menu) · monitor windows · screenshot · `monitor_window` + `monitor_bar` (all picture settings: `quality_controls` popup; "Hide controls": `ViewerConfig.hide_controls`, `Reveal` brings them back over the picture after 1 s in the top 3 % of the window or on its title bar) · file browser (`FilesView`, `files_window`, `file_pane`: per side the path (click = type/paste a folder, Enter goes there) with a refresh icon at the right border (spinner while loading), then that side's Upload/Download row, then the list; remembers per side the last folder and "Show hidden files": `ViewerConfig.files_local` / `files_remote[machine id]`, `local_start` = remembered → home → root, remote `RemoteStart` = remembered → primary disk via `primary_disk`) · `Updater` (app self-update: start-up, hourly, "Check for updates" + "Restart" in the bottom bar; installed from the .deb → downloads the newer .deb, "Install" asks for the password; `app_exe()` = path read at start, since `current_exe` names `farlink.old` after an update) · widgets · tests | Anything visible in the app |
| `tray/` | System tray icon + menu (Open · Start at login · Quit): `icon.rs` draws the app icon in code (purple tile, white monitor, pointer; also the window icon; preview with `cargo run --example icon_preview -- DIR`), `linux.rs` (ksni StatusNotifierItem, retries while the desktop's tray comes up), `windows.rs` (Shell_NotifyIconW on a hidden window thread; never run on real Windows). Events wake egui; closing the main window hides it to the tray (`QUITTING` in `viewer.rs` really quits) | Tray |
| `autostart.rs` | "Start at login" for the app (`farlink --tray` = start hidden): XDG `~/.config/autostart/farlink.desktop` / `HKCU\…\Run`. Switched on once on the first start of a release build (`ViewerConfig.autostart_set_up`) | Autostart |
| `layout.rs` | Remembered window positions per machine+monitor (`layouts.json`) | Window placement |
| `config.rs` | Settings files (host/viewer/server), atomic JSON save | New settings |
| `install.rs` | Install plans as data (`Step`s): Windows service + firewall, Linux user service, server (writes `/etc/sysctl.d/60-farlink.conf` = `tcp_slow_start_after_idle=0` so pictures after a pause arrive in one round trip; `tcp_stays_warm` → `Overview.slow_after_pause_fix` warns in the web admin / app when it is still on; HTTPS itself: `--port`, auto cert on 443 or cert files; removes old Caddy site); `execute` | Installers |
| `server_update.rs` | Server follows GitHub Releases (`update_from_github`): hourly check, verify assets against `SHA256SUMS.txt`, replace own exe, publish computer programs, re-exec in place (loop guard `FARLINK_UPDATED_TO`). `UpdateStatus` (saved in `<data>/update-status.json`, sent in `Overview.server_update`: version, last check, last update), `POST /api/server/check-update` (admin) wakes the loop via `Relay.update_now`. Asset names must match `build.yml` | Server auto-update |
| `update.rs` | Self-update: check, download, SHA-256 verify, swap exe; `install_published` (forced: any version that differs); Debian package: `installed_from_deb` (app at `/usr/bin/farlink`), `download_deb` (to `~/.cache/farlink`), `install_deb` (`pkexec apt-get install`) | Updates |
| `keys.rs` | Key names ⇄ X11 keysyms / Windows VK codes (`name_for_keysym` the other way) | Keyboard |
| `keygrab.rs` | Alt+Tab for the remote computer: while Alt is held in a focused screen window (control, mouse on the picture) `AltGrab` grabs the keyboard on that window (X11 only) and forwards keys itself (`Forward`) until Alt is let go; the browser uses Keyboard Lock in full screen (Chrome/Edge). Real-X test: `cargo test -- --ignored x11_alt_tab` | Keyboard shortcuts the local desktop would take |
| `util.rs`, `version.rs` | Small helpers, CLI `Args`, version compare, `published_file` (app → file in `releases/<target>/`; `DEB_APP` = `farlink.deb`, Linux only) | — |
| `packaging/` | Desktop app `.deb`: `deb.sh` (dpkg-deb; glibc minimum read from the binary), `farlink.desktop` (`StartupWMClass=farlink` = the viewport `app_id`), `icons/` (PNGs from `make icons`) | Linux package |
| `web/` | Browser UI embedded with `include_str!`: `admin.html/js` (web admin + person view), `viewer.html/js` (in-browser viewer + files panel), `install.html`, `message.html`, `app.css`, `icon.svg` (header logo + favicon at `/assets/icon.svg`; same shapes/colours as `tray/icon.rs`, keep them in step) | Web UI (plain JS, no build step) |

Tests: unit tests live at the bottom of each module; `tests/end_to_end.rs` starts a real relay on a
random port with real/fake hosts and viewers (people access, links, files, view-only switching, clipboard,
updates, rename, screen previews).

## Commands

```sh
make test            # all tests (must stay green)
make check           # compile-check Linux + Windows (cross)
cargo test -- --ignored x11_capture   # real X11 capture on this machine (needs a display)
cargo test -- --ignored x11_alt_tab   # real keyboard grab + XTest Alt+Tab (needs a display)
cargo run --bin farlink -- --demo     # viewer with sample computers, no server
cargo run --bin farlink -- --demo --tray   # same, starting hidden in the tray
cargo run --bin farlink -- --screenshot shots/now.png   # renders ~120 frames, saves PNG, exits
FARLINK_SHOT_FILES=1 / FARLINK_SHOT_RENAME=1 / FARLINK_SHOT_PREVIEW=1 / FARLINK_SHOT_SHELL=1  (with --screenshot)  # file browser / rename dialog / preview (no session) / shell window (runs a few commands here)
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
* Security basics on the relay: every answer gets `security_headers` (CSP, no framing, no referrer, no-store);
  settings files are saved owner-only (`save_json`, keeps the old owner); the data dir is 0700; `/view/{machine}`
  reveals nothing without a sign-in. Keep these when adding routes or pages (no external scripts: CSP).
* People (`Who::User`) see only machines whose `owner` is them; admin-only API calls check `is_admin`.
  The remote shell is the administrator's only: `viewer_loop` computes `shell` (no grant, no person),
  `viewer_message_allowed(.., shell)` drops shell requests otherwise, and the helper refuses unless
  `OpenSession.shell` said so. Shell windows keep their transcript and Up/Down history in memory only.
  Renaming computers and notes are admin-only (notes are blanked in a person's overview).
* Adding a `SessionMsg` variant: update `is_control()` / `is_file_request()` / `is_shell_request()` if it touches the remote
  machine, `viewer_message_allowed` if the relay must not forward it, `host::relay_only` / `net::relay_message`
  if only the relay may send it (never taken from the direct path), and `web/viewer.js` if browsers use it.
* **On the direct path the relay sees nothing**: the host's own checks (view-only input in `run_screen`,
  files/clipboard in `ViewerInput`) are the enforcement there. Mode switches and `Closed` only ever come
  from the relay socket.
* Every WebSocket uses TCP_NODELAY (`connect_async_with_config(.., true)`, `tls::PlainListener` for plain HTTP,
  `tls.rs` accept loop). The relay forwards host→viewer in its own task so input/acks never queue behind pictures.
* The host's screen thread is blocking (`std::sync::mpsc` in, `tokio::mpsc` out); keep heavy work off
  the async runtime.
* egui is **0.37**: `egui::Panel::top/left/bottom(...).show(ui, ..)`, `App::ui(&mut self, ui, frame)`,
  `ui.close()` for menus, `Id::unique`. Check `~/.cargo/registry/src/*/egui-0.37.0/` before guessing APIs.
* Edition 2024 with let-chains. Dev profile has no debug info (disk on the dev machine is tight).
* Viewer vsync stays on: without it the app spins a whole CPU core while a session is open (measured), for ~10 ms.
* Windows code compiles via `make check` but has **never run on real Windows** — test there before
  claiming it works. Ubuntu is X11 only (no Wayland).
* Release layout the server expects: `releases/version.txt` + `releases/<rust-target>/<program>` (+ `farlink.deb` on Linux, built by `make dist`/`make deb`);
  `util::current_target()` must match the folder names (`x86_64-pc-windows-gnu`, not msvc).

## CI/CD

`.github/workflows/build.yml` (this folder is meant to be the root of the GitHub repo): tests on every
push/PR (Linux + Windows); release builds for Linux x86_64, Linux ARM64 (on Ubuntu 22.04 runners, so the programs need only glibc 2.35; `packaging/deb.sh` refuses newer), Windows x86_64 only on tags and
manual "Run workflow" (so pushing main + tag together doesn't build twice); pushing a tag
`vX.Y.Z` (must equal `version` in `Cargo.toml`) creates a GitHub Release with the programs, the ready
`farlink-X.Y.Z.tar.gz` (dist layout) and `SHA256SUMS.txt`.

Releasing: bump `version` in `Cargo.toml` → `cargo build` (updates Cargo.lock) → commit →
`git tag v0.2.0 && git push origin main v0.2.0`.
The project skill `.claude/skills/release` (`/release [patch|minor|major|X.Y.Z]`) does all of
this with the safety checks.
