# Installing the Farlink server (web admin + relay)

The server is the one place everything connects to: your computers, your desktop app, the people you
give access to, and the web page. You install it once, on a small server on the internet with its own
domain name. After that, adding a computer is just "open a link on it".

Time needed: about 20 minutes.

---

## What you need

| | |
|---|---|
| **A server** | Any small cloud server (VPS) with **Ubuntu 24.04** or **Debian 12**, 1 CPU, 1 GB memory, 10 GB disk is enough. You need to be able to log in to it with `ssh` as a user who can use `sudo`. |
| **A domain name** | For example `remote.example.com`. You must be able to add a DNS record for it. |
| **Open ports** | 443 (HTTPS). Nothing else — port 80 is not used. The remote computers never need open ports. If 443 is already taken, see *Port 443 is already in use* below. |
| **The Farlink release folder** | Built once on a Linux computer (step 2). |

> Ubuntu 22.04 also works.

---

## Step 1 — Point your domain to the server

In your domain provider's control panel add a DNS record:

```
Type: A      Name: remote      Value: <your server's IP address>
```

(For `remote.example.com`.) It can take from a few minutes to an hour to work. Check it with
`ping remote.example.com`: it should show your server's IP.

## Step 2 — Build the release (on your Linux computer)

On a Linux computer with Rust installed, in the `app` folder:

```sh
rustup target add x86_64-pc-windows-gnu     # once
sudo apt install mingw-w64                  # once, for the Windows programs
./build-release.sh
```

This creates a `dist` folder:

```
dist/farlink-server                   the server program
dist/releases/version.txt             the version number
dist/releases/x86_64-unknown-linux-gnu/farlink, farlink-host
dist/releases/x86_64-pc-windows-gnu/farlink.exe, farlink-host.exe
```

## Step 3 — Copy it to the server

```sh
scp -r dist  you@<server-ip>:~/farlink
```

## Step 4 — Install

Log in to the server and run the installer with **your** domain:

```sh
ssh you@<server-ip>
cd ~/farlink
sudo ./farlink-server install --domain remote.example.com
```

Want to see what it will do first? Add `--dry-run` — it only prints the steps.

The installer:

1. copies the server to `/opt/farlink-server` and the programs for your computers to `/var/lib/farlink-server/releases`,
2. writes the settings to `/etc/farlink-server/config.json` (public address `https://remote.example.com`),
3. creates a `farlink` system user and a background service that starts with the server,
4. sets the server up to speak **HTTPS itself**: on first start it gets a free Let's Encrypt certificate
   for your domain and renews it on its own (no Caddy or nginx needed),
5. opens port 443 in the firewall (`ufw`).

### Port 443 is already in use

Let's Encrypt checks domains only on port 443, so on any other port the server can't get a certificate
by itself. Give it a certificate you already have (full chain + private key, PEM files the `farlink`
user can read) and pick the port:

```sh
sudo ./farlink-server install --domain remote.example.com --port 8443 \
     --tls-cert /etc/ssl/remote/fullchain.pem --tls-key /etc/ssl/remote/privkey.pem
```

The address becomes `https://remote.example.com:8443`. Restart the service after renewing the files
(`sudo systemctl restart farlink-server`).

If 443 is taken by your own web server (nginx, Caddy), you can instead let it proxy to Farlink: set
`"bind": "127.0.0.1:8470"` and `"tls": {"mode": "off"}` in `/etc/farlink-server/config.json` and point
the proxy at `127.0.0.1:8470` (WebSockets must be passed through).

At the end it says where the **administrator password** is:

```sh
sudo cat /var/lib/farlink-server/initial-admin-password.txt
```

## Step 5 — Sign in

Open `https://remote.example.com` in your browser. Leave *sign-in name* empty, type the administrator
password. Change it to your own right away:

```sh
sudo /opt/farlink-server/farlink-server set-password 'a-long-new-password' --config /etc/farlink-server/config.json
```

That's it — the web admin and the relay are running.

---

## Using it

**Add one of your computers.** On the web page, copy the link under *Add a computer* and open it on that
computer (Windows: download and run the installer; Ubuntu: paste one line into a Terminal). It appears in
*Computers* within seconds.

**Give someone (e.g. Anna) access to only her own computer.**
1. *People → Add person*: name `Anna`, sign-in name `anna`, a password.
2. You get **Anna's personal install link**. Open it on Anna's computer and install.
3. In *People*, Anna's row changes from *waiting for install* to her computer's name with *installed*.
4. Anna signs in at `https://remote.example.com` (or in the Farlink app) with `anna` and her password.
   She sees **only her computer** and can view and control it, and copy files. She can't see any other
   computer, people or links.
5. You can move any computer to a person (or back to *Administrator only*) with the *Belongs to* menu,
   reset her password, or remove her — her sessions end immediately.

**Temporary access** for a helper: *Temporary link* next to a computer → only watch or watch and
control → how long. *Withdraw* ends it immediately.

**Desktop app.** Download it from `https://remote.example.com/app`, enter the
server address and your sign-in.

---

## Updating everything

### Automatically from GitHub (recommended)

Install with `--github OWNER/REPO` (the repository whose *Releases* page has the Farlink builds):

```sh
sudo ./farlink-server install --domain remote.example.com --github you/farlink
```

(or add `"update_from_github": "you/farlink"` to `/etc/farlink-server/config.json` and restart).
The server then checks for a new release at start and every hour. When there is one it downloads the
programs, checks each against the release's `SHA256SUMS.txt`, replaces itself, restarts in a second,
and offers the new version to the computers, which update themselves within a minute. If anything
doesn't match, nothing is changed. See what it did: `journalctl -u farlink-server | grep updates`.

### By hand

Build a newer version on your Linux computer (raise `version` in `Cargo.toml`, run `./build-release.sh`),
copy `dist/releases` to the server and publish it:

```sh
scp -r dist/releases you@<server-ip>:~/farlink-new
ssh you@<server-ip>
sudo /opt/farlink-server/farlink-server publish 0.2.0 ~/farlink-new --config /etc/farlink-server/config.json
```

Every connected computer downloads it in the background within a minute, checks it and restarts — nobody
has to click anything. To update the server program itself, copy the new `farlink-server` over
`/opt/farlink-server/farlink-server` and run `sudo systemctl restart farlink-server`.

## Backup

Everything (computers, people, links, passwords as secure hashes) is one file:
`/var/lib/farlink-server/state.json`. Copy it somewhere safe from time to time.

---

## Troubleshooting

| Problem | What to do |
|---|---|
| The page doesn't open | Check DNS points to the server (`ping remote.example.com`) and the service runs: `systemctl status farlink-server`. |
| "Certificate" error in the browser | The certificate can only be fetched when the domain already points to the server and port 443 is open. Fix DNS/firewall, then `sudo systemctl restart farlink-server` and watch `journalctl -u farlink-server -f`. |
| Server won't start: "listening on 0.0.0.0:443" | Something else uses port 443 (`sudo ss -ltnp 'sport = :443'`). Stop it, or see *Port 443 is already in use*. |
| Logs | Server: `journalctl -u farlink-server -f`. Windows computers: `C:\ProgramData\Farlink\host.log`. Ubuntu computers: `~/.config/farlink/host.log`. |
| A computer shows *offline* | It is switched off, not connected to the internet, or (Ubuntu) nobody is signed in to the desktop. |
| Install link stopped working | You pressed *New link*. Copy the current one from the web page. |

## Removing the server

```sh
sudo systemctl disable --now farlink-server
sudo rm -rf /opt/farlink-server /var/lib/farlink-server /etc/farlink-server /etc/systemd/system/farlink-server.service
```
