# warren

**Turn devices you already own into your own private residential proxy pool.**

[![CI](https://github.com/doedja/warren/actions/workflows/ci.yml/badge.svg)](https://github.com/doedja/warren/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/doedja/warren?sort=semver)](https://github.com/doedja/warren/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](#license)
![Platforms](https://img.shields.io/badge/platforms-linux%20%7C%20macos%20%7C%20windows-informational)

Run a tiny agent on any devices you control: a home PC, a cheap Raspberry Pi or
TV box, a spare phone, a VPS at a friend's place. Each one quietly dials out to
your hub and waits. You point your app, scraper, or browser at a single address,
and warren sends each request out through one of your devices, so the site sees
that device's ordinary home IP, not yours and not a flagged datacenter range.

The devices need nothing opened: no port-forwarding, no static IP, no fixed
hostname. It works behind home routers, CGNAT, and on phones, because the
devices only ever dial *out*.

It is yours end to end: no third-party proxy service, no bandwidth marketplace,
nobody else's traffic on your IPs. One small binary is both the hub and the node.

> A warren is a network of connected burrows. Each device digs one burrow out to
> the hub; the hub is the warren your apps enter through.

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

## Quick start

```bash
# 1. on a server: run the hub (proxy on :8000, devices connect on :7000)
warren hub --enroll-token secret --proxy-user me --proxy-pass pw \
  --listen 0.0.0.0:7000 --proxy-listen 0.0.0.0:8000

# 2. on a device you own: join the pool (one line; see Install below)
curl -fsSL https://raw.githubusercontent.com/doedja/warren/main/install.sh | sh -s -- \
  --hub SERVER:7000 --token secret

# 3. from anywhere: send traffic through your pool
curl -x http://me:pw@SERVER:8000 https://api.ipify.org   # prints your device's IP
```

That is the whole loop. Add more devices and the hub spreads requests across
them and skips any that go offline. For a public-facing hub, turn on TLS
(`--tls`) and use the dashboard below.

## What you get

- One proxy endpoint for a pool of your own devices, with automatic failover.
- **HTTP CONNECT, SOCKS5, and plain HTTP**, all on the same port, with auth.
- A **web dashboard** (live, auto-refreshing) to add devices, approve them, and
  copy ready-to-run install commands. See [the dashboard](#admin-dashboard).
- **Encrypted device link** (opt-in TLS, the device pins the hub's key).
- **No inbound** on devices; runs on Linux, macOS, Windows, and tiny ARM boxes.
- One static binary. No runtime, no database server (state is a local file).

## Install a device (node)

Prebuilt binaries for Linux (x86_64/arm64), macOS (x86_64/arm64), and Windows
(x86_64) are on the [Releases](https://github.com/doedja/warren/releases/latest) page.

**Linux / macOS** ([`install.sh`](https://github.com/doedja/warren/blob/main/install.sh)):

```bash
# with a token: the device is approved automatically
curl -fsSL https://raw.githubusercontent.com/doedja/warren/main/install.sh | sh -s -- \
  --hub HOST:7000 --token TOKEN --tls --hub-fingerprint FP

# without a token: the device shows up as "pending", you approve it in the dashboard
curl -fsSL https://raw.githubusercontent.com/doedja/warren/main/install.sh | sh -s -- --hub HOST:7000 --tls --hub-fingerprint FP
```

**Windows** ([`install.ps1`](https://github.com/doedja/warren/blob/main/install.ps1), elevated PowerShell):

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/doedja/warren/main/install.ps1))) `
    -Hub HOST:7000 -Token TOKEN -Tls -HubFingerprint FP
```

Both download the right binary and register a startup service (systemd, launchd,
or a Windows scheduled task). Run with no extra arguments to just install the
binary. To remove: `warren node uninstall` (or `install.ps1 -Uninstall`). No
prebuilt binary for your arch? Build it: `cargo install --git https://github.com/doedja/warren warren`.

The dashboard's Connect card hands you these exact commands with the address and
fingerprint already filled in.

## Admin dashboard

Run the hub with `--admin-listen 0.0.0.0:9000 --admin-token <secret>` for a web
dashboard, behind HTTP Basic auth (any username, the admin token as the
password). It refreshes itself and shows live devices, pending approvals,
approved keys, enrollment tokens, and proxy users, plus a **Connect** card with
copy-paste install and proxy commands. Pass `--public-node-addr HOST:7000` and
`--public-proxy-addr HOST:8000` so the card shows your real addresses. The admin
port is plain HTTP, so it can sit behind a domain with TLS.

## How devices are trusted

Each device makes an ed25519 key on first run and proves it owns that key when it
connects. It joins the pool one of two ways:

- **With a token:** present `--token`; the hub trusts the device's key right away.
- **Without a token:** the device appears as **pending** with a short code; you
  click approve in the dashboard. Nothing secret travels over the wire, and you
  can revoke any device's key later.

## Run the hub

The hub is one long-running process. Put it on any always-on host: a VPS, a home
server, or a container platform. A minimal image:

```dockerfile
FROM rust:1-bookworm AS build
RUN git clone https://github.com/doedja/warren /src && cd /src && cargo build --release --bin warren
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/warren /usr/local/bin/warren
ENTRYPOINT ["warren"]
CMD ["hub", "--tls", "--tls-cert-dir", "/data", "--db", "/data/warren.db", \
     "--listen", "0.0.0.0:7000", "--proxy-listen", "0.0.0.0:8000", "--admin-listen", "0.0.0.0:9000"]
```

Mount a volume at `/data` (it holds the TLS cert and the SQLite file), set
`WARREN_ENROLL_TOKEN`, `WARREN_PROXY_USER`, `WARREN_PROXY_PASS`, and
`WARREN_ADMIN_TOKEN` in the environment, and publish the device port (7000) and
proxy port. With `--tls` the hub prints a fingerprint; devices pin it.

## What is proxied (and what does not leak)

- HTTP CONNECT, SOCKS5, and plain HTTP, all auto-detected on the proxy port.
- HTTPS over CONNECT is end to end: the target sees the device's IP, the hub only
  relays encrypted bytes, and the device resolves DNS (so no DNS leak).
- Only apps you point at the proxy use it. It is per-app, not whole-machine.
- UDP and QUIC are not carried; clients fall back to TCP. Disable QUIC/WebRTC in
  a browser if you need a hard guarantee that nothing slips around the proxy.

## Failover

The hub tries the device freshest on the target host first and retries another if
one fails, skipping the failed one. If no device can serve the request it errors
out: it never quietly falls back to your real IP.

## Hardware for always-on devices

See [`docs/hardware.md`](docs/hardware.md). Short version: a used Amlogic TV box
(B860H, wired Ethernet) flashed with Armbian, or an Orange Pi Zero 2W / Zero 3,
each run a node on roughly 1 to 5 watts.

## Build from source

```bash
cargo build --release   # needs rustup, stable >= 1.74; binary at target/release/warren
cargo test              # unit + end-to-end tests
```

Design notes and the wire protocol are in [`SPEC.md`](SPEC.md). QUIC/WSS was
considered and dropped in favor of the simpler connection-per-request model plus
TLS over TCP.

## License

Apache-2.0. Self-hosted, for devices you own. Use it on networks and accounts you
have permission to use.
