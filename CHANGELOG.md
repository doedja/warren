# Changelog

All notable changes to warren. Format follows [Keep a Changelog]; the release
workflow publishes each version's section here as its GitHub release notes.

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/

## [Unreleased]

## [0.4.3] - 2026-06-02

### Fixed
- macOS node install no longer requires (and must not use) `sudo`. A launchd
  LaunchAgent is per-user: when the installer ran it under `sudo`, the root
  `launchctl load` targeted the wrong domain, so the agent silently never
  started and the node never appeared on the hub. `install.sh` now installs the
  service as the user on macOS, and `node install` loads into the user's GUI
  domain (`gui/<uid>`).
- macOS install run as root anyway (`sudo sh install.sh`, or a root shell) now
  resolves the real login user (`SUDO_USER`, else the console session owner),
  bootstraps into that user's domain, and `chown`s the plist back to them so no
  root-owned agent is left behind and later upgrades/uninstalls work without
  sudo.
- macOS (re)install rides out the launchd `bootout`/`bootstrap` race
  (KeepAlive teardown settles asynchronously and an immediate bootstrap can fail
  with EIO): it retries, then falls back to the legacy `load -w`.

### Changed
- Windows startup task is now defined from a task XML so it carries
  restart-on-failure (parity with systemd `Restart=always` / launchd
  `KeepAlive`: a crashed node recovers instead of staying down until reboot), no
  execution time limit (the flat form inherited the 72h default), and
  run-on-batteries. `install.ps1` checks for an elevated shell up front and
  fails with a clear message instead of a cryptic `schtasks` access error.
- CI now also compiles (build + clippy) on macOS and Windows, so the
  platform-specific service-install paths are checked on their real targets.

## [0.4.2] - 2026-06-02

### Added
- Release assets now ship a `sha256` checksum next to each archive, and both
  installers (`install.sh`, `install.ps1`) verify the download against it before
  extracting or running anything. Fails closed; `WARREN_SKIP_VERIFY=1` bypasses.
  Because auto-update re-runs the installer, self-updates are verified too.

### Changed
- Dashboard is wider (max 1200px) so the Live nodes table fits all columns
  (including the new Version) without crowding on a normal desktop.

### Fixed
- Node auto-update is now actually reachable: `--auto-update` passed to the
  installer is baked into the boot service (it was previously dropped), and a
  self-update re-runs with the flag so it stays on across an upgrade. Still
  opt-in and off by default. Append `--auto-update` to the install line to
  enable it (Unix only).

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

[Unreleased]: https://github.com/doedja/warren/compare/v0.4.3...HEAD
[0.4.3]: https://github.com/doedja/warren/releases/tag/v0.4.3
[0.4.2]: https://github.com/doedja/warren/releases/tag/v0.4.2
[0.4.1]: https://github.com/doedja/warren/releases/tag/v0.4.1
[0.4.0]: https://github.com/doedja/warren/releases/tag/v0.4.0
[0.3.0]: https://github.com/doedja/warren/releases/tag/v0.3.0
[0.2.0]: https://github.com/doedja/warren/releases/tag/v0.2.0
