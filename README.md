# Farlink — your own remote desktop

Three programs that work together:

| Program | Runs on | What it does |
|---|---|---|
| **farlink-server** | your public server (Ubuntu/Debian) | Relay between you and your computers, web admin page, install links, temporary access links, update feed |
| **farlink-host** | every computer you want to reach (Windows 10/11, Windows Server 2016+, Ubuntu X11) | Shows the screens and accepts mouse/keyboard. It can **only be controlled** — it never controls other computers — and it only connects out to your server (no open ports needed) |
| **farlink** | your own computer | Desktop app: list of computers, one window per remote monitor, quality settings, view-only mode. `farlink mcp` lets Claude (or any MCP client) use your computers: see [5. Let Claude use your computers](#5-let-claude-use-your-computers-mcp) |

There is also a **web version**: open `https://your-domain` in any browser to manage computers, links and to view/control a computer without installing anything.

**Full step-by-step server installation guide: [INSTALL-SERVER.md](INSTALL-SERVER.md).**

### People with access to only their own computer

*People → Add person* (e.g. Anna) gives her a sign-in and a **personal install link**. A computer
installed with that link belongs to Anna: the admin page shows it in her row as *installed*, and when Anna
signs in (web or app) she sees and controls **only that computer** — no other computers, people or links.

### Files

In a control session press **Files**: a two-pane browser, *this computer* on the left, the remote
computer on the right. Select a file and press *Upload →* or *← Download* (files are never overwritten;
a copy gets “(2)” added). The browser version has a Files panel too. View-only sessions can't touch files.

### Light on the remote computer

While someone is connected the helper only captures the screens that are actually open in a viewer
window (closed/minimised windows or a hidden browser tab cost nothing), skips pictures that didn't change,
slows to 4 checks a second when the screen is idle, and on Ubuntu asks the X server what changed instead of
reading the screen over and over.

## 1. Set up the server (once)

1. Point a domain (for example `remote.example.com`) to your server.
2. Build the release (`./build-release.sh`) and copy the `dist` folder to the server.
3. On the server run:

   ```sh
   sudo ./farlink-server install --domain remote.example.com
   ```

   This installs the service, opens port 443 and publishes the programs found in `releases/`.
   The server speaks HTTPS itself: it gets a free Let's Encrypt certificate for the domain on first
   start and renews it on its own (no Caddy or nginx needed; port 80 is not used).

   Port 443 already taken by something else? Pick another port and give your own certificate
   (Let's Encrypt only checks domains on port 443):
   `sudo ./farlink-server install --domain remote.example.com --port 8443 --tls-cert fullchain.pem --tls-key privkey.pem`
   The administrator password is printed and saved in
   `/var/lib/farlink-server/initial-admin-password.txt`.

   Other HTTPS setups, in `/etc/farlink-server/config.json` or on the `serve` command line:
   your own certificate files (`--tls-cert fullchain.pem --tls-key key.pem`), or plain HTTP behind
   a proxy you already run (`--no-tls --bind 127.0.0.1:8470`).

## 2. Add a computer

Open `https://remote.example.com`, sign in, copy the **Add a computer** link and send it to the computer.

* **Windows**: click *Download installer*, open it, allow changes. It installs a background service
  (works at the sign-in screen, lock screen and UAC prompts), adds firewall rules and starts it.
* **Ubuntu (X11)**: paste the one-line command into a Terminal.

The computer appears in your list within seconds. It updates itself in the background.

## 3. Use it

* **Desktop app**: enter the server address and password once. Pick a computer → **Control** or **View only**.
  Every remote monitor opens in its own window; move/resize them as you like — next time they open in
  exactly the same places (remembered per computer and per monitor).
* **Picture quality**: Fast / Balanced / Sharp, or set picture size, pictures per second, compression
  and colours (full, 16-bit, 256 colours, grey) yourself. Changes apply immediately.
* **Temporary links**: in the web admin or the app choose *Temporary link*, decide *only watch* or
  *watch and control* and how long it works. The person opens the link in a browser or pastes it into the
  app. **Withdraw** ends their access and any open session immediately.

## 4. Updates

Build a new version (raise `version` in `Cargo.toml`, run `./build-release.sh`), copy `dist/releases`
to the server and run:

```sh
sudo farlink-server publish 0.2.0 ./releases --config /etc/farlink-server/config.json
```

Every connected computer downloads it, checks its fingerprint (SHA-256) and restarts itself — no questions asked.
The desktop app updates itself on its next start.

## 5. Let Claude use your computers (MCP)

`farlink mcp` is an [MCP](https://modelcontextprotocol.io) server built into the desktop app. With it,
Claude Code, Claude Desktop or any other MCP client can work on your computers through your relay:

| Tool | What it does |
|---|---|
| `list_computers` | Every computer: online or not, OS, who is signed in, IP addresses, screens, CPU/memory, notes. Optional `search` |
| `screenshot` | A picture of one screen, right now (scaled to 1280 px by default, `max_size` changes it) |
| `click`, `move_mouse`, `drag`, `scroll` | The mouse, in pixels of the last screenshot of that screen |
| `type_text`, `press_keys` | Typing, and keys/shortcuts such as `ctrl+c`, `alt+tab`, `win+r`, `ctrl+alt+del` |
| `run_command` | A shell command (bash on Linux, PowerShell or cmd on Windows) with its output and exit code. The folder is kept between calls. **Administrator only** |
| `list_files`, `read_file`, `write_file`, `make_folder` | Folders and text files on the remote computer |
| `upload_file`, `download_file` | Files between the remote computer and the computer the MCP server runs on |

It signs in like the app and goes through the relay's usual checks: a person (`--user anna`) only reaches
their own computers and never gets the shell. Files are never overwritten (a copy gets "(2)" added).
`--read-only` keeps only the tools that look: `list_computers`, `screenshot`, `list_files`, `read_file`,
`download_file`.

**Signing in** (the first one found is used):

1. `FARLINK_PASSWORD` in the environment: the administrator's password, or a person's together with
   `--user <sign-in name>` (or `FARLINK_USER`).
2. `FARLINK_TOKEN`: a session token.
3. The desktop app's own sign-in: sign in once in the app (same server) and nothing else is needed.
   Pressing *Sign out* in the app ends it, for the MCP server too.

The server address comes from `--server https://remote.example.com`, `FARLINK_SERVER`, or the app's settings.

### Claude Code

Signed in in the app already:

```sh
claude mcp add farlink -- farlink mcp
```

With a password instead (`--scope user` makes it available in every project):

```sh
claude mcp add farlink --scope user -e FARLINK_PASSWORD='your admin password' -- farlink mcp --server https://remote.example.com
```

Then ask, for example: *"List my Farlink computers"*, *"How much disk space is left on the office PC?"*,
*"Take a screenshot of Anna's laptop and close the error dialog"*. Run `claude mcp list` to see that it connects;
in a session `/mcp` shows its tools.

### Claude Desktop

Settings → Developer → *Edit Config* opens `claude_desktop_config.json`. Add:

```json
{
  "mcpServers": {
    "farlink": {
      "command": "farlink",
      "args": ["mcp", "--server", "https://remote.example.com"],
      "env": { "FARLINK_PASSWORD": "your admin password" }
    }
  }
}
```

On Windows put the full path in `command` (for example `C:\\Users\\you\\farlink.exe`). Restart Claude Desktop;
Farlink's tools appear under the tools (🔨) button. Leave out `env` if you are signed in in the app on that computer.

### Other MCP clients

Anything that runs a local (stdio) MCP server works: the command is `farlink mcp`, with the options and
environment above. Messages go one JSON-RPC message per line on stdin/stdout; errors and logs go to stderr
(`RUST_LOG=info` for more).

**Be careful:** this gives the assistant the same power over your computers as you have — someone may be
working on them. Clients ask before each tool call unless you allow it; keep that on for `run_command` and
the input tools, or use `--read-only`.

## Building

```sh
make            # release builds for Linux and Windows + the ready dist/ folder
make linux      # Linux x86_64 only        make windows   # Windows x86_64 only
make linux-arm64                           make package   # dist/ as farlink-<version>.tar.gz
make test       # all tests                make check     # compile-check Linux + Windows
make help       # list of everything
```

## Working on it, GitHub and automatic builds

This folder is a complete, self-contained project: you can keep working in it, copy it anywhere, or
make it its own GitHub repository:

```sh
git init && git add . && git commit -m "Farlink"
git remote add origin git@github.com:<you>/farlink.git && git push -u origin main
```

GitHub then builds and tests every push automatically (*Actions* tab; the programs are under each
run's *Artifacts*). To publish a version, set `version` in `Cargo.toml` (e.g. `0.2.0`), commit, and push
a matching tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

That builds Linux (x86_64 and ARM64) and Windows and creates a GitHub Release with every program, the
ready-to-install `farlink-0.2.0.tar.gz` and checksums. `AGENTS.md` is a map of the source code for
AI coding assistants (and people).

## For developers

```sh
cargo build                      # all three programs
cargo test                       # unit + end-to-end tests (real relay, helper and viewers)
cargo test -- --ignored          # also real X11 screen capture on this machine
./target/debug/farlink --demo    # the app with sample computers, no server needed
./target/debug/farlink-server serve --url http://127.0.0.1:8470 --data ./data   # local server
./target/debug/farlink-host run --demo --server http://127.0.0.1:8470 --token <install token>
```
