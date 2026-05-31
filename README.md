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

Working end to end. A client can proxy HTTPS through the hub and out a node, with
round-robin selection and failover across nodes. Verified by an integration test
(`crates/warren/tests/e2e.rs`) and a real `curl` through the running binary.

How it works today: the node opens one **control** TCP connection to the hub
(token-authenticated). For each client `CONNECT`, the hub sends a `Dial` to a
node; the node dials the target from its own network, opens a fresh **data** TCP
connection to the hub tagged with the request id, and the hub splices client to
data connection. No inbound connectivity needed on the node.

Not yet done (see [`SPEC.md`](SPEC.md)): TLS on the node-hub link (plain TCP for
now, gated by the enrollment token), QUIC/WSS transport, SOCKS5 and plain-HTTP
proxying (CONNECT only today), SQLite persistence and the admin UI, and the
self-installing boot service (`node install` is a stub).

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

# 3. client: proxy HTTPS through the pool
curl -x http://127.0.0.1:8000 https://api.ipify.org      # prints the node's IP
```

`warren enroll` prints a random token to use for `--enroll-token` / `--token`.
Require client auth with `--proxy-user U --proxy-pass P` on the hub, then
`curl -x http://U:P@host:8000 ...`.

## What is proxied (and what does not leak)

- HTTPS over CONNECT: fully proxied; the target sees the node's residential IP.
  The hub never sees content (it splices encrypted bytes); the node resolves DNS,
  so no DNS leak.
- CONNECT only today. Plain-HTTP (absolute-URI) proxying and SOCKS5 are planned.
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
