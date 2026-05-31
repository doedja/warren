# warren design spec

A residential proxy pool built from devices you own. One binary, two modes.
Nodes need zero inbound connectivity: they dial out to the hub and the hub
routes client traffic back through them, so the target site sees each node's
home/mobile (residential) IP. No Tailscale, no port-forwarding, no open proxy
port on the node.

> Metaphor: a warren is a network of interconnected burrows. Each node digs one
> burrow out to the hub; the hub is the warren that clients enter through.

## Implemented vs planned

Implemented: the control + on-demand-data-connection transport (plain TCP, with
opt-in TLS + self-signed cert fingerprint pinning, persistent cert); HTTP
CONNECT, SOCKS5, and plain-HTTP client proxies with auth (all auto-detected on
one port); SQLite-backed enrollment tokens + proxy users; health-aware routing
with failover and per-target-host freshness; an admin API + web dashboard; node
boot-service install (systemd/launchd); env-var config; and a Coolify deploy.

Not pursued: QUIC/WSS transport. The original muxed-QUIC design was replaced by
the connection-per-request model (simpler, deterministic, easy to test), and
opt-in TLS-over-TCP covers the security goal, so a separate QUIC/WSS carrier
adds complexity without a matching benefit for this model. Revisit only if the
transport moves back to single-connection multiplexing.

## Goals

- One static binary, `warren`, with `hub` and `node` modes.
- A node joins with a single command and an enrollment token; one more command
  installs it as a boot service. Leaving is one command.
- Nodes are outbound-only (works behind CGNAT, on phones, on locked-down LANs).
- The hub exposes one standard proxy endpoint (HTTP CONNECT + SOCKS5) with
  user/password auth. It load-balances, health-checks, and fails over across nodes.
- Hub state in a single SQLite file. The hub is one container on Coolify.
- Self-hosted, for devices you own. Not a bandwidth marketplace.

## Non-goals

- Not a general mesh VPN. We move proxied TCP, not arbitrary L3 traffic.
- Not a full Tailscale/Headscale replacement. The only thing we "mesh" is the
  node->hub control link.
- No silent direct-connection fallback. If the pool is empty the request errors;
  it never leaks the client's real IP.

## Topology

```
client ──HTTP/SOCKS proxy──► hub ──QUIC/TLS (or WSS:443)──► node ──► target site
                              │  picks a healthy node,            egress =
                              │  opens a stream "dial target",    node's
                              │  pipes bytes both ways            residential IP
        node keeps ONE persistent outbound control connection to the hub.
```

- The node opens a persistent connection to `hub:443` and keeps it alive.
- A client connects to the hub's proxy port and issues `CONNECT host:port`
  (HTTP) or a SOCKS5 connect.
- The hub authenticates the proxy-user, selects a node, opens a new multiplexed
  stream over that node's connection carrying a `Dial{host,port}` request.
- The node dials `host:port` from its own network and the hub splices the client
  socket to the node stream. The node never accepts inbound connections.

## Transport

Pluggable behind a `Transport` trait so the wire can change without touching hub
or node logic.

- **Primary: QUIC** (`quinn` + `rustls`). Native stream multiplexing (one stream
  per proxied connection), 0-RTT reconnect, and connection migration that
  survives a node changing IP (mobile roaming, DHCP renew) without dropping the
  pool membership. Runs on UDP/443.
- **Fallback: WSS over TCP/443** (`tokio-tungstenite`) with `yamux` for
  multiplexing, used automatically when UDP/QUIC is blocked. Same logical
  protocol, different carrier.

A node tries QUIC first, falls back to WSS. The hub serves both on 443.

## Wire protocol (control + data)

Length-prefixed messages, serialized with `serde` (CBOR/`postcard` on the wire;
defined in the `warren-proto` crate). Sketch:

- Node -> Hub on connect: `Hello { token, node_name, platform, version }`
- Hub -> Node: `Welcome { node_id }` or `Reject { reason }`
- Keepalive: `Ping` / `Pong` (also the passive liveness signal)
- Per proxied connection, Hub -> Node opens a stream: `Dial { host, port }`
- Node -> Hub on that stream: `DialOk` then raw bytes both ways, or `DialErr { reason }`
- Hub -> Node: `Close { reason }` for graceful drain

Each proxied connection is its own multiplexed stream; the control messages above
ride a dedicated control stream.

## Hub

- **Client-facing proxy**: HTTP CONNECT (`hyper`) + SOCKS5 (hand-rolled), one
  listen port, `user:password` auth backed by SQLite. HTTPS tunnels via CONNECT
  so the hub never sees content; the node resolves DNS, so no DNS leak.
- **Node registry**: in-memory live set (control connection + rolling health),
  persisted to SQLite for the node list, users, pools/tags, and stats.
- **Routing strategies**: round-robin (default), least-connections, random,
  sticky-by-client-IP. Optional pools/tags to group nodes (region/ISP) with an
  ordered fallback chain, like a pool cascade.
- **Health**: passive (control link alive + recent success/error rate) and active
  (periodic test fetch through the node). Unhealthy nodes are excluded; a failed
  `Dial` retries on the next healthy node, excluding the one that just failed.
- **Admin**: JSON API (`axum`) + a small embedded static web UI served by the
  same binary. No separate dashboard service.
- **Store**: SQLite (`rusqlite`/`sqlx`), one file on a mounted volume.

## Node

- `warren node --hub <url> --token <enroll>`: connect, authenticate, keep the
  control link alive, serve `Dial` requests by connecting out and piping bytes.
- `warren node install` / `uninstall`: register or remove a boot service
  (systemd unit on Linux, launchd plist on macOS, Scheduled Task/Service on
  Windows). The binary writes its own service definition.
- Minimal footprint: target is a few MB RAM idle. Runs on an Amlogic STB
  (B860H), Orange Pi Zero, a PC, or a phone via Termux (native Android app later).

## Enrollment and auth

- Hub generates enrollment tokens (`warren enroll --name ... [--ttl ...]`).
- A token authenticates a node on first connect; the hub issues a persistent
  node identity (a keypair / client cert) so reconnects do not need the token.
- Reusable and single-use tokens; ephemeral nodes auto-prune from the pool after
  a missed-keepalive window.
- Client/proxy users are separate from node identity: created in the hub
  (`user:password`, optional `requests_per_minute`).

## Security and abuse posture

- All node<->hub traffic is TLS (QUIC built-in, or WSS). Node identity is a
  per-node credential issued at enrollment.
- Framed and documented as a self-hosted tool for your own devices.
- The hub proxy port should sit behind the tailnet/VPN or a firewall allowlist,
  or rely on proxy-user auth + rate limits if exposed. The docs lead with the
  private option.

## Crate layout

```
warren/                 cargo workspace
  crates/
    warren-proto/       wire messages + shared types (serde)
    warren/             the `warren` binary: cli, hub, node, transport, proxy
```

Start as two crates. Split hub/node into their own crates only if they grow.

## Milestones

1. **Skeleton** (this scaffold): CLI (`hub`/`node`/`enroll`), proto message
   types, transport trait + stub, structured logging. Compiles, inert.
2. **One node end-to-end**: QUIC control link + enrollment; hub HTTP CONNECT
   proxy; `Dial` over a stream; bytes piped through a single node.
3. **Pool**: multiple nodes, routing strategies, passive+active health, failover
   with dead-node exclusion.
4. **Auth + protocols**: proxy-users (auth, rate limit), SOCKS5, pools/tags with
   fallback chains.
5. **Ops**: self-install services per OS; SQLite persistence; embedded admin UI +
   JSON API.
6. **Packaging**: GitHub releases (musl static for linux/arm64+amd64, Windows,
   macOS), one-line installer, Coolify compose for the hub at `warren.doedja.com`.
7. **Stretch**: native Android node app, WSS fallback hardening, metrics/export.

## Verifiable success criteria (per milestone)

- M1: `cargo build` is clean; `warren --help`, `warren hub --help`,
  `warren node --help` print usage; running each mode logs a startup line and
  exits/idles without panic.
- M2: with a hub up and one node enrolled, `curl -x http://hub:PORT
  https://api.ipify.org` returns the node's residential IP.
- M3: killing one node's process keeps requests served via another; the dead node
  is skipped within one health interval.
- M4: an unauthorized proxy request is rejected; SOCKS5 path returns the node IP;
  a pool with an empty primary cascades to the fallback pool.
- M5: `warren node install` survives a reboot (service auto-starts); hub state
  persists across a container restart.
- M6: a release binary runs on a freshly flashed B860H (Armbian) with no Rust
  toolchain present.
