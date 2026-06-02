# Changelog

All notable changes to warren. Format follows [Keep a Changelog]; the release
workflow publishes each version's section here as its GitHub release notes.

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/

## [Unreleased]

## [0.4.1] - 2026-06-02

### Added
- Hub log on the dashboard: a bounded in-memory ring of recent hub activity
  (enrollments, disconnects with their reason, errors), shown in a "Hub log"
  card and served at `/api/logs`. Filter by level (all / warn+ / errors) and a
  "copy shown" button.
- Each node's binary version is shown in the Live nodes table, so you can spot
  nodes that need updating.

### Fixed
- Node self-heal gap: the connect + enroll handshake (TCP/TLS dial, control
  stream open, Hello/HelloReply) is now bounded by a timeout, so a connection
  that comes up at the transport layer but never completes enrollment (hub
  mid-restart, half-open link) no longer wedges the node forever. It returns an
  error and the existing reconnect/backoff loop takes over, like the
  steady-state read already did. This was the cause of nodes going stuck for
  hours until a reinstall.
- Dashboard: the "How do I rotate it?" help is now a click-to-expand disclosure
  (the old hover tooltip never showed).
- Windows installer: a first install no longer prints a `schtasks` error when no
  scheduled task exists yet.

### Changed
- A node disconnect is now logged with its reason (dead-peer timeout, control
  connection lost, connection closed).
- Release notes are now generated from this changelog.
- Dashboard footer points to the Prometheus `/metrics` endpoint.

## [0.4.0] - 2026-06-02

### Added
- Protocol version tolerance: the hub accepts a range of node versions and gates
  newer messages per node, so an additive protocol bump no longer forces every
  node to reinstall at once.
- Node self-update: opt-in `--auto-update` (Unix) re-runs the installer when the
  hub reports a newer version; off by default (a notice is logged).
- Prometheus `/metrics` on the admin server (Basic auth, admin token): nodes
  online, bytes per node and per user, dial counts, fails, UDP associations.
- Seamless Termux/Android setup: the installer wires up the wake lock, a
  boot-restart script, and the battery-optimization prompt, then starts the node.
- tun2socks full-tunnel guide for routing a whole device through the pool.

### Changed
- Node exit IP is derived hub-side from the control-connection address (with a
  hub-side geo lookup), instead of a node-side IP-echo call that failed on some
  networks.
- UDP relay hardening: dual-stack (IPv4 + IPv6) node egress, a per-association
  idle timeout, and a per-second rate cap.

## [0.3.0] - 2026-06-02

### Added
- UDP support via SOCKS5 UDP ASSOCIATE: a client's UDP traffic (DNS, QUIC/HTTP3,
  WebRTC media) egresses through a node. New `UdpOpen` message + `UdpDatagram`
  frame; the hub runs one shared UDP relay, demuxed per client.

## [0.2.0] - 2026-06-02

### Changed
- The node-to-hub link is now one yamux-multiplexed connection (control stream
  plus one logical stream per request), replacing connection-per-request.

### Added
- Lean release profile, reconnect backoff with jitter, dead-peer read timeout on
  both sides, least-connections routing with a per-node concurrency cap, graceful
  drain on SIGTERM/SIGINT, and per-node success-rate + last-error on the dashboard.
- Dashboard refresh: a stat strip, setup stepper, version badge, and unified copy
  buttons.

[Unreleased]: https://github.com/doedja/warren/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/doedja/warren/releases/tag/v0.4.1
[0.4.0]: https://github.com/doedja/warren/releases/tag/v0.4.0
[0.3.0]: https://github.com/doedja/warren/releases/tag/v0.3.0
[0.2.0]: https://github.com/doedja/warren/releases/tag/v0.2.0
