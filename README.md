# warren

A residential proxy pool built from devices you own. One binary, two modes.

Install the `warren` agent on each PC, phone, or cheap Linux box. Each node dials
**out** to your hub and keeps one connection alive. Your apps point at the hub's
proxy endpoint; the hub routes each request back through a healthy node, which
makes the request from its own home/mobile (residential) IP. Nodes need **zero
inbound**: no Tailscale, no port-forwarding, no open proxy port. Works behind
CGNAT and on phones.

```
client ──HTTP CONNECT──► hub ──control + data conns (TCP)──► node ──► target
                          picks a healthy node, fails over    egress = node's
                                                              residential IP
```

> A warren is a network of interconnected burrows. Each node digs one burrow out
> to the hub; the hub is the warren clients enter through.

## Status

Working end to end and deployable. A client proxies HTTP CONNECT, SOCKS5, or
plain HTTP through the hub and out a node, with health-aware routing and
failover. Verified by integration tests and real `curl` runs through the binary.

How it works: the node opens one **control** connection to the hub (token-auth,
TLS optional). For each client request the hub sends a `Dial` to the node that
is freshest on that target host; the node dials the target from its own network,
opens a fresh **data** connection tagged with the request id, and the hub splices
client to data connection. No inbound connectivity needed on the node.

Done: HTTP CONNECT + SOCKS5 + plain-HTTP (auto-detected on one port) with auth;
opt-in TLS on the node link (self-signed cert + fingerprint pinning, persistent
across restarts); SQLite-backed enrollment tokens + proxy users; health-aware
routing with failover and per-target-host freshness; an admin API + web
dashboard; `node install` boot service (systemd / launchd / Windows); env-var
config; one-line installers for Linux/macOS/Windows; and a container image for
self-hosting the hub (any Docker/Compose host or Coolify).

Not pursued (see [`SPEC.md`](SPEC.md)): QUIC/WSS transport, superseded by the
connection-per-request model + TLS-over-TCP.

## Layout

```
warren/
├── SPEC.md                     design + wire protocol + milestones
├── Cargo.toml                  workspace
├── crates/
│   ├── warren-proto/           hub <-> node wire messages + postcard helpers
│   └── warren/                 the warren binary + lib: cli, hub, node, proxy, wire
│       └── tests/e2e.rs        end-to-end proxy + auth integration tests
└── docs/hardware.md            cheap low-power node boards (Indonesia sourcing)
```

## Build

Needs a Rust toolchain (`rustup`, stable >= 1.74).

```bash
cargo build --release   # binary at target/release/warren
cargo test              # unit + end-to-end tests
```

## Run it locally

Three terminals (or background the first two):

```bash
# 1. hub: node link on :7000, client proxy on :8000
warren hub --enroll-token secret --listen 0.0.0.0:7000 --proxy-listen 0.0.0.0:8000

# 2. node: dial the hub, become a residential exit
warren node run --hub <hub-host>:7000 --token secret --name livingroom

# 3. client: proxy HTTPS through the pool (HTTP CONNECT or SOCKS5, same port)
curl -x http://127.0.0.1:8000 https://api.ipify.org           # prints the node's IP
curl -x socks5h://127.0.0.1:8000 https://api.ipify.org        # SOCKS5, remote DNS
```

`warren enroll` prints a random token for `--enroll-token` / `--token`. Require
client auth with `--proxy-user U --proxy-pass P` on the hub, then
`curl -x http://U:P@host:8000 ...` (or `socks5h://U:P@host:8000`).

For a WAN deployment, add `--tls` to the hub (it prints a fingerprint) and join
nodes with `--tls --hub-fingerprint <fp>`. Install a node as a boot service with
`warren node install --hub ... --token ... [--tls --hub-fingerprint ...]`.

Admin dashboard: run the hub with `--admin-listen 0.0.0.0:9000 --admin-token <secret>`
for a web dashboard at that address, gated by HTTP Basic auth (any username, the
admin token as password). It **auto-refreshes** (live nodes, pending approvals,
approved keys, tokens, proxy users) and shows a **Connect** card with
copy-paste install + proxy commands. Pass `--public-node-addr HOST:7000` and
`--public-proxy-addr HOST:18080` so the card shows the real addresses; the
fingerprint is filled in automatically. Each enrollment token has a "copy
install" button that builds the full one-liner. Tokens/users live in the `--db`
SQLite file; `warren enroll --db <file>` also mints a token from the CLI.

## Install a node (one-liner)

Prebuilt binaries for Linux (x86_64/arm64 musl), macOS (x86_64/arm64), Windows
(x86_64) from [Releases](https://github.com/doedja/warren/releases/latest).

**Linux / macOS** ([`install.sh`](https://github.com/doedja/warren/blob/main/install.sh)):

```bash
# Mode B: pre-shared token, auto-approved
curl -fsSL https://raw.githubusercontent.com/doedja/warren/main/install.sh | sh -s -- \
  --hub HOST:7000 --token TOKEN --tls --hub-fingerprint FP

# Mode A: no token; node shows up as "pending", approve it in the dashboard
curl -fsSL https://raw.githubusercontent.com/doedja/warren/main/install.sh | sh -s -- --hub HOST:7000 --tls --hub-fingerprint FP
```

**Windows** ([`install.ps1`](https://github.com/doedja/warren/blob/main/install.ps1), elevated PowerShell):

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/doedja/warren/main/install.ps1))) `
    -Hub HOST:7000 -Token TOKEN -Tls -HubFingerprint FP
```

Both fetch the matching release binary and register a boot service (systemd /
launchd / Windows scheduled task). Run with no args to just install the binary.
**Uninstall**: `warren node uninstall` (or `install.ps1 -Uninstall` on Windows).
**Fallback** for other arches: `cargo install --git https://github.com/doedja/warren warren`.

The dashboard's Connect card and per-token "copy install" button hand you the
exact command with the host + fingerprint already filled in.

## Node identity + enrollment

Each node holds an ed25519 key (generated on first run, stored at the key file).
It proves possession by signing a timestamp; the hub trusts it once the pubkey is
approved. Two ways to approve:

- **Mode B (token):** node presents `--token`; the hub auto-approves its key.
- **Mode A (no token):** node appears as **pending** with a short code; approve
  it in the dashboard (or `DELETE` to deny). Approved keys are listed and can be
  revoked. No long-lived shared secret travels in Mode A.

## Deploy the hub

The hub is a single long-running process; run it on any always-on host (a VPS,
a home server, Docker/Compose, or a PaaS like Coolify). A minimal container:

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

Mount a volume at `/data` (persists the TLS cert + SQLite), set
`WARREN_ENROLL_TOKEN` / `WARREN_PROXY_USER` / `WARREN_PROXY_PASS` /
`WARREN_ADMIN_TOKEN` in the environment, and publish the node + proxy ports.
The admin port (9000) is plain HTTP, so it can sit behind a reverse proxy /
domain with TLS; the node link and proxy are raw TCP and are published directly.

## What is proxied (and what does not leak)

- HTTP CONNECT, SOCKS5, and plain-HTTP are all proxied (auto-detected on the
  proxy port). For plain HTTP the request is forwarded in origin form with
  `Connection: close`.
- HTTPS over CONNECT: fully proxied; the target sees the node's residential IP.
  The hub never sees content (it splices encrypted bytes); the node resolves DNS,
  so no DNS leak.
- Only apps pointed at the hub proxy use it (per-app, not whole-OS).
- UDP/QUIC from a client is not carried; clients normally fall back to TCP.
  Disable QUIC/WebRTC in a browser if you need a hard guarantee.

## Failover

Node-to-node and pool-to-pool, with the just-failed node excluded on retry. If
every pool is empty the request errors; it never silently falls back to a direct
(real-IP) connection. A last-resort direct path, if you want one, is wired
client-side on purpose.

## Hardware for always-on nodes

See [`docs/hardware.md`](docs/hardware.md). Short version: a used Amlogic STB
(B860H, ~Rp 100-200k, wired Ethernet) flashed with Armbian, or an Orange Pi Zero
2W/Zero 3, both run a node on ~1-5 W.

## License

Apache-2.0. Self-hosted, for devices you own.
