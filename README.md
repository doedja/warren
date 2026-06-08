# warren

**Turn devices you already own into your own private residential proxy pool.**

[![CI](https://github.com/doedja/warren/actions/workflows/ci.yml/badge.svg)](https://github.com/doedja/warren/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/doedja/warren?display_name=tag)](https://github.com/doedja/warren/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](#license)
![Platforms](https://img.shields.io/badge/platforms-linux%20%7C%20macos%20%7C%20windows-informational)

Run a tiny agent on any device you control: a home PC, a cheap Raspberry Pi or
TV box, a spare phone, a VPS at a friend's place. Each one dials out to your hub
and waits. You point your app, scraper, or browser at a single address, and
warren sends each request out through one of your devices, so the site sees that
device's ordinary home IP, not yours and not a flagged datacenter range.

The devices need nothing opened: no port-forwarding, no static IP, no fixed
hostname. They only ever dial *out*, so home routers, CGNAT, and phones all
work. It is yours end to end: no third-party proxy service, no bandwidth
marketplace, nobody else's traffic on your IPs. One small binary is both the hub
and the node.

> A warren is a network of connected burrows. Each device digs one burrow out to
> the hub; the hub is the warren your apps enter through.

## Set it up: two steps

**1. On a server, install warren and start the hub.** No secrets to invent: it
generates a proxy login and an enrollment token, turns on TLS, and prints a
one-paste **join code** for your devices (give it the address devices reach it on
so the code is complete).

```bash
curl -fsSL https://raw.githubusercontent.com/doedja/warren/main/install.sh | sh && warren hub --public-node-addr SERVER:7000
```

It prints something like:

```
warren hub is up.
  proxy login:   warren / 7f3a9d2e1b8c...
  enroll token:  k7f3a9d2e1b8...
  fingerprint:   c8145bad9e7f...
  join a device: warren node run --join warren1.aGVsbG8...
```

This runs in the foreground for a quick try. For an always-on server, run it as a
container ([compose example](examples/docker-compose.yml)) or behind a service.

**2. On each device you own, join the pool.** Paste the join code (it carries the
address, token, TLS, and fingerprint, so there are no flags to fill in). The
dashboard's token list gives you the right line per OS.

Linux / macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/doedja/warren/main/install.sh | sh -s -- --join warren1.aGVsbG8...
```

Windows (elevated PowerShell):

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/doedja/warren/main/install.ps1))) -Join warren1.aGVsbG8...
```

That is the setup. No VPN to mesh, no proxy to configure on each device, no
rotator to bolt on. Add more devices and the hub spreads requests across them and
skips any that drop. Now point any app at the proxy with the printed login:

```bash
curl -x http://warren:PASSWORD@SERVER:8000 https://api.ipify.org
```

A plain username uses the whole pool and fails over automatically. Add a device
name to send traffic out one device, like picking a single exit node:

```bash
curl -x http://warren+phone:PASSWORD@SERVER:8000 https://api.ipify.org
```

Or use a **sticky session**: `user-session-KEY` keeps a sequence of requests on
the same device while it stays healthy (for multi-step flows that need a stable
IP). Any key works; reuse it across requests:

```bash
curl -x http://warren-session-ab12:PASSWORD@SERVER:8000 https://api.ipify.org
```

Or route by **region**: `user-region-COUNTRY` leaves through a device whose
location matches (e.g. `warren-region-Indonesia`), with no fallback to other
regions if none match. The hub determines each device's location from its exit
IP, so a device cannot spoof its region:

```bash
curl -x http://warren-region-Indonesia:PASSWORD@SERVER:8000 https://api.ipify.org
```

Or route by **group**: every node that enrolled with the same token is a group,
named after that token. `user-group-NAME` leaves through any node in the group
(no fallback if it is empty), so one token can stand for "my home boxes" or "the
US batch". Rename a token on the dashboard to relabel its group:

```bash
curl -x http://warren-group-residential:PASSWORD@SERVER:8000 https://api.ipify.org
```

The dashboard's Live nodes shows each device's **exit IP, location, health, and
success rate**.

### Route a whole device through the pool (tun2socks)

The examples above are per-app. To send a **whole machine or phone** out through
the pool (including UDP, QUIC, and WebRTC), point a tun2socks tool at warren's
SOCKS5 endpoint, which does UDP ASSOCIATE. With
[hev-socks5-tunnel](https://github.com/heiher/hev-socks5-tunnel):

```yaml
# config.yaml
tunnel:
  name: tun0
  mtu: 8500
socks5:
  address: SERVER
  port: 8000
  username: warren
  password: PASSWORD
  udp: udp        # carry UDP over the SOCKS5 UDP associate
```

Then bring up the tun device and route traffic into it (OS-specific). Now every
connection from that device leaves through one of your nodes on its home IP, not
just what you point at the proxy. (`examples/tun2socks.md` has the full walk-through.)

## Why one binary

The usual way to build this from your own devices is a stack: a mesh VPN
(Tailscale or Headscale) so the hub can reach devices stuck behind home routers,
a proxy on every device (3proxy or gost) to make the requests, a rotator in front
(something like Rota) to spread load and fail over, and a pile of scripts to wire
it together. Each piece is its own install, its own config, its own way to break.
The phones still need someone to tap "allow VPN" by hand.

warren is that whole stack collapsed into one binary. The device dials out, so
there is no mesh VPN and no port-forwarding to set up. The same binary is both
the proxy and the rotator. You install one thing, it joins the pool, done.

## How it works

- **Devices dial out.** Each node opens one outbound connection to the hub and
  keeps it alive. Nothing listens on the device.
- **The hub is the front door.** Your app talks to the hub's proxy (HTTP or
  SOCKS5, with a username and password). The hub picks a healthy device.
- **The device makes the request.** It connects to the target from its own
  network, so the target sees its home IP. The hub just shuttles bytes; for
  HTTPS it never even sees the content.

```
your app ──HTTP/SOCKS5──► hub ──one outbound link per device──► your device ──► the site
                          picks a healthy device, retries another  egress = device's home IP
```

## The dashboard does the typing

Start the hub with `--admin-listen 0.0.0.0:9000 --admin-token SECRET` and open it
in a browser (log in with any username and the admin token as the password). It
shows your live devices, and a **Connect** card with a ready-to-run install line
for new devices, address and token (and, if you use TLS, the fingerprint) already
filled in. Copy, paste on the device, done. It also lists devices waiting for
approval, your proxy users, and enrollment tokens, each labeled with what it does.

The admin server also exposes **Prometheus metrics** at `/metrics` (HTTP Basic,
any username + the admin token as the password, so Prometheus `basic_auth`):
nodes online, bytes per node and per user, dial counts, and active UDP
associations, for scraping into Grafana or alerting.

## What you get

- One proxy endpoint for a pool of your own devices, with automatic failover.
- **Pick the pool, one device, a region, a group, or a sticky session.**
  `user:pass` auto-picks a healthy device; `user+name:pass` pins one named
  device; `user-region-COUNTRY:pass` picks by location; `user-group-NAME:pass`
  picks any node that joined with the token named NAME; `user-session-K:pass`
  keeps a session on one device while it stays healthy.
- **HTTP CONNECT, SOCKS5, and plain HTTP** on one port, with auth, plus **UDP**
  via SOCKS5 UDP ASSOCIATE (DNS, QUIC, WebRTC media) out the same device.
- A **live web dashboard** to add devices, approve them, copy install commands,
  and see each device's current exit IP and location.
- **Encrypted device link** (TLS on by default; the device pins the hub's key).
- **No inbound** on devices; runs on Linux, macOS, Windows, and tiny ARM boxes.
- One static binary. No runtime, no database server (state is a local file).

## Install details

Prebuilt binaries for Linux (x86_64/arm64), macOS (x86_64/arm64), and Windows
(x86_64) are on the [Releases](https://github.com/doedja/warren/releases/latest)
page. The `install.sh` line above downloads the right one and registers a boot
service (systemd, launchd, or a Windows scheduled task). Run it with no extra
arguments to just drop in the binary.

Both installers verify the download against the `sha256` published with each
release before extracting or running it, and abort on a mismatch (set
`WARREN_SKIP_VERIFY=1` to bypass, not recommended).

**Windows** (elevated PowerShell):

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/doedja/warren/main/install.ps1))) `
    -Join warren1.aGVsbG8...
```

**Clean uninstall** (stops the service, deletes the node key, removes the binary):

```bash
# if warren is on PATH:
warren node uninstall
# otherwise:
curl -fsSL https://raw.githubusercontent.com/doedja/warren/main/install.sh | sh -s -- --uninstall
```

On Windows: `install.ps1 -Uninstall`. Revoke the device's key in the dashboard too
if you want the hub to forget it. No prebuilt binary for your arch? Build it:
`cargo install --git https://github.com/doedja/warren warren`.

### On a phone (Android, via Termux)

A spare Android phone can be a node. Install [Termux](https://termux.dev) from
F-Droid (not the Play Store version, which is outdated), then run the installer
with your join code:

```bash
pkg install curl
curl -fsSL https://raw.githubusercontent.com/doedja/warren/main/install.sh | sh -s -- --join warren1.YOUR_JOIN_CODE
```

The installer drops the arm64 binary, **starts the node, takes a CPU wake lock,
writes a boot-restart script, and opens the battery-optimization screen** for you.
Two one-time taps finish it (Android requires these by hand, no script can do them):

- Install **Termux:Boot** and **Termux:API** from F-Droid and open each once. They
  enable the boot-restart and the wake lock the installer set up.
- Accept the **battery-optimization exemption** dialog the installer opened.

After that the node survives sleep and reboot with no further steps. Logs are in
`~/warren-node.log`; a phone kept on power is steadiest, and the node reconnects on
its own when the link drops. (Termux has no systemd, so `warren node install`
does not apply; this path replaces it.)

### Managing the node

The installer registers a boot service, so the node **starts on reboot** and
restarts if it crashes (systemd `Restart=always`, launchd `KeepAlive`). It also
reconnects to the hub on its own if the link drops. To control it by hand:

| OS | start / restart | status / logs |
|----|-----------------|---------------|
| Linux | `systemctl restart warren-node` | `journalctl -u warren-node -f` |
| macOS | `launchctl kickstart -k gui/$(id -u)/com.warren.node` | `log show --predicate 'process == "warren"'` |
| Windows | `schtasks /Run /TN warren-node` (runs at startup; not auto-restarted on crash) | Task Scheduler |
| Termux | re-run `warren node run --join <code>` | terminal output |

Or just run it in the foreground anywhere: `warren node run --join <code>`.

**Auto-update (opt-in, off by default).** Append `--auto-update` to the install
line and the node will re-run the installer to upgrade itself when the hub
reports a newer version (Unix only; on Windows the running exe is locked):

```sh
curl -fsSL https://raw.githubusercontent.com/doedja/warren/main/install.sh | sh -s -- --join <code> --auto-update
```

It is off by default on purpose: a silent binary swap across a fleet means the
hub (and this repo) effectively gets to run code on every node, and a bad
release would propagate everywhere at once with no canary. Version tolerance
means a node that is a version behind keeps serving, so leaving it off and
reinstalling by hand is the safe default. Turn it on per node once you trust the
release flow.

## Running the hub on the public internet

**TLS is on by default.** The hub generates a self-signed cert (persisted next to
the db, so the fingerprint stays stable across restarts) and devices pin its
fingerprint, so a device can confirm it reached *your* real hub. The join code
carries the fingerprint, so you never type it. Only drop TLS with `--no-tls` for a
hub reached only over localhost or a tailnet.

A device makes an ed25519 key on first run and proves it owns that key when it
connects. With a token (in the join code) the hub trusts the device right away;
without one it appears as **pending** and you approve it in the dashboard. You can
revoke any device's key later, and nothing secret travels over the wire.

### As a container

A [`Dockerfile`](Dockerfile) is at the repo root (it builds the hub and runs it
with TLS, a `/data` volume for the cert and state, and the device, proxy, and
admin ports). [`examples/docker-compose.yml`](examples/docker-compose.yml) wires
it up end to end:

```bash
WARREN_PROXY_USER=me WARREN_PROXY_PASS=secret WARREN_ADMIN_TOKEN=admin \
  docker compose -f examples/docker-compose.yml up --build
```

Set `WARREN_ENROLL_TOKEN`, `WARREN_PROXY_USER`, `WARREN_PROXY_PASS`, and
`WARREN_ADMIN_TOKEN` in the environment (omit the enroll token to have the hub
mint one on first run, printed in the logs).

## Good to know

- **What is proxied:** HTTP CONNECT, SOCKS5, and plain HTTP, auto-detected on one
  port. HTTPS over CONNECT is end to end: the target sees the device's IP, the hub
  only relays encrypted bytes, and the device resolves DNS (no DNS leak). It is
  per-app, not whole-machine, so only what you point at the proxy uses it.
- **UDP, too.** SOCKS5 UDP ASSOCIATE is supported, so UDP datagrams (DNS, QUIC /
  HTTP3, and WebRTC media) egress through a device as well. The relay shares the
  proxy's host:port on UDP, so a client points at the same address. Caveats: the
  client must actually route its UDP through the proxy (native SOCKS5-UDP apps or
  a system tun2socks do; browsers bypass SOCKS for WebRTC by default, the usual
  "WebRTC leak"); egress targets may be IPv4 or IPv6; one association per client IP.
- **How a device is chosen:** round-robin across the pool, biased toward devices
  that are healthy, least-loaded, and recently succeeded on the target host. A
  device that fails three dials in a row is skipped until it recovers. `user+name`
  overrides this and pins one named device.
- **Failover:** the hub tries the freshest device first and retries another if one
  fails. If no device can serve, it errors out; it never quietly falls back to
  your real IP. (A pinned `user+name` request fails rather than using a different
  device.)

## Commands

One binary, three subcommands. Run any with `--help` for the full list.

**`warren hub`** runs the control plane and the client-facing proxy.

| flag | default | what it does |
|------|---------|--------------|
| `--listen` | `0.0.0.0:7000` | address devices dial in on (one multiplexed link) |
| `--proxy-listen` | `0.0.0.0:8000` | address apps send proxy traffic to |
| `--public-node-addr` | none | the address devices reach the hub on; set it so the printed/dashboard join code is complete |
| `--enroll-token` | auto | join secret; if omitted and none exists, one is generated and printed |
| `--proxy-user` / `--proxy-pass` | auto | a proxy login; if omitted and none exists, one is generated and printed |
| `--admin-listen` / `--admin-token` | off | serve the dashboard; log in with any username + the token |
| `--no-tls` | off | turn TLS OFF (plaintext); only for localhost or a tailnet |
| `--tls-cert-dir` | next to `--db` | where the TLS cert lives (so the fingerprint is stable) |
| `--db` | `warren.db` | SQLite file (tokens + proxy users + device keys) |
| `--public-proxy-addr` | none | proxy address shown in the dashboard's commands |

Most of these also read an env var (`WARREN_ENROLL_TOKEN`, `WARREN_PROXY_USER`,
`WARREN_PROXY_PASS`, `WARREN_ADMIN_LISTEN`, `WARREN_ADMIN_TOKEN`, `WARREN_DB`,
`WARREN_TLS_CERT_DIR`, `WARREN_PUBLIC_NODE_ADDR`, `WARREN_PUBLIC_PROXY_ADDR`),
which is how the container image is configured. `--listen`, `--proxy-listen`, and
`--no-tls` are flags only.

**`warren node run --join <code>`** runs the agent on a device. The join code (from
the hub's startup output or the dashboard) fills in everything below; or pass the
flags yourself.

| flag | what it does |
|------|--------------|
| `--join` | one-paste join code; fills in hub address, token, TLS, and fingerprint |
| `--hub HOST:7000` | the hub address, if you are not using `--join` |
| `--token` | join automatically (Mode B); omit to wait for dashboard approval (Mode A) |
| `--name` | the device's name in the pool (defaults to its hostname); this is the name used in `user+name`, and must be unique (a second device claiming a name already in use is rejected) |
| `--tls` `--hub-fingerprint FP` | use TLS and pin the hub (carried by `--join`) |
| `--insecure` | with `--tls`, skip fingerprint pinning. Dev only; do not use against a real hub |
| `--key-file` | where the device keeps its identity key (default `~/.warren/node.key`) |
| `--auto-update` | when the hub reports a newer version, re-run the installer to upgrade this node (Unix only; off by default, a notice is logged instead) |

**`warren node install ...`** takes the same flags as `node run` and registers a
boot service (systemd / launchd / Windows task) so the device rejoins on reboot.
**`warren node uninstall`** does a clean removal: stops the service, deletes the
key, removes the binary.

**`warren enroll --name device`** (on the hub) mints a fresh enrollment token and
prints it. Use it to rotate: mint a new one, hand it out, then delete the old
token in the dashboard.

## Build from source

```bash
# needs rustup, stable >= 1.88; binary lands at target/release/warren
cargo build --release
# unit + end-to-end tests
cargo test
```

## License

Apache-2.0. Self-hosted, for devices you own. Use it on networks and accounts you
have permission to use.
