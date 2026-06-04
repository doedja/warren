# Changelog

All notable changes to warren. Format follows [Keep a Changelog]; the release
workflow publishes each version's section here as its GitHub release notes.

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/

## [0.4.8] - 2026-06-04

### Fixed

- Hub log no longer floods with `rustls ... Illegal SNI extension` warnings.
  Internet scanners probing the exposed node port present the host IP as the TLS
  SNI; rustls warned on every one. The warning was harmless (the SNI is ignored,
  node auth is the app-layer enroll token) but buried real warnings in the
  dashboard log card. That one rustls target is now filtered below WARN.
- Dashboard no longer shows a healthy node as unhealthy next to a contradictory
  "100%" success rate. Two fixes: (1) the node Health counter now expires a run
  of failures after 60s with no new failure (a successful dial still resets it
  instantly), so a node that stopped failing or went idle recovers instead of
  sticking red until its next success, and transient dead-target timeouts no
  longer pin an otherwise-fine node. (2) the Success column floors to one decimal
  instead of rounding, so "100%" shows only when every dial succeeded. The
  expired health count also feeds routing order and the `/metrics` gauge.

### Changed

- Node auto-update is now staggered across the fleet. A hub redeploy announces
  the new version to every `--auto-update` node at once; previously they all
  re-ran the installer and restarted together, so the whole pool blinked out.
  Each node now waits a random delay (up to 5 min) before updating and keeps
  serving until it fires, so updates roll one node at a time (version tolerance
  keeps the not-yet-updated nodes routable). Debounced so a repeated announce
  does not stack timers. Single-node setups are unaffected (nothing to stagger).

### Security

- The admin/`/metrics` Basic-auth token is now compared in constant time, so it
  cannot be recovered by timing how long a wrong token takes to reject.

## [0.4.7] - 2026-06-03

### Fixed
- Unix `--auto-update` could take a node down instead of upgrading it. The node
  launched `install.sh` as a child process, and the installer stops the
  warren-node service to swap the binary, which killed the still-running
  installer (its parent died) before the swap finished: service stopped, binary
  not replaced, node offline until a manual reinstall. The updater now runs
  detached from the service: a systemd transient unit on Linux (its own cgroup,
  independent of the warren-node unit's restart), or a new session via `setsid`
  elsewhere (so launchd's process-group teardown does not reach it). This mirrors
  the Windows detached-helper design added in 0.4.6. Node-side only; reinstall a
  node once to pick up the fixed updater.

## [0.4.6] - 2026-06-03

### Fixed
- A node that reconnected over a half-open link (the old TCP connection never
  sent a FIN, common on Windows) was silently dropped from the routable set even
  though its new connection was alive. The hub keyed a node only by its stable id
  and removed every entry with that id, so when the dead-peer timer reaped the
  stale duplicate connection (~75s after the node had already reconnected) it
  evicted the live reconnected entry too. The node stayed "ghost-connected" (TCP
  up, replying to pings) but invisible on the dashboard and carrying no traffic
  until a restart. Each connection now carries a unique sequence: a reconnect
  displaces the old entry on enroll, and the stale reap removes only its own
  connection, never the live one. Hub-only fix, no protocol change.

### Added
- Windows nodes support opt-in `--auto-update`. The running `.exe` is locked, so
  a detached PowerShell helper waits for the node process to exit, then re-runs
  `install.ps1` to swap the binary and restart the service. Off by default, like
  the unix path; `install.ps1` gained an `-AutoUpdate` switch and the dashboard
  auto-update toggle now applies to the Windows install command too.
- CI gained an MSRV (Rust 1.74) build job and a `cargo audit` job, so the
  declared minimum toolchain and dependency advisories are both enforced.

## [0.4.5] - 2026-06-02

### Fixed
- Windows node install failed with "The task XML contains a value which is
  incorrectly formatted or out of range" (the restart Interval `PT15S` is below
  the Task Scheduler 1-minute minimum, and hand-written task XML is order-
  sensitive). The startup task is now built with PowerShell's ScheduledTask
  cmdlets, which generate valid XML: restart-on-failure every 1 minute, no
  execution time limit, run-on-batteries, SYSTEM at boot. If PowerShell is
  unavailable it falls back to the flat `schtasks` boot task, so install never
  hard-fails.

## [0.4.4] - 2026-06-02

### Added
- Node groups via enrollment tokens. The token's name is now a group label:
  every node that joins with a given token belongs to that group, and a client
  can route through the whole group with the proxy username `user-group-NAME`
  (case-insensitive, no fallback if the group is empty), alongside the existing
  `+device`, `-region-`, and `-session-` selectors. Pure hub-side routing, no
  protocol change, existing nodes unaffected.
- Dashboard: each node shows its group as a badge next to its name; each
  enrollment token has a "rename" button (renaming relabels the group; connected
  nodes pick up the new label on their next reconnect) and a "copy group cmd"
  button that yields the ready `user-group-NAME` curl.

### Changed
- Enrollment-token names are now validated on create and rename (the name
  doubles as the `user-group-NAME` routing selector). Names are restricted to
  `[A-Za-z0-9._-]` and may not contain the reserved routing markers
  (`-region-`, `-session-`, `-group-`), so a name can no longer break the copied
  group curl or be silently swallowed by `parse_route`. The API rejects an
  invalid or empty name with 400.

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

[Unreleased]: https://github.com/doedja/warren/compare/v0.4.8...HEAD
[0.4.8]: https://github.com/doedja/warren/releases/tag/v0.4.8
[0.4.7]: https://github.com/doedja/warren/releases/tag/v0.4.7
[0.4.6]: https://github.com/doedja/warren/releases/tag/v0.4.6
[0.4.5]: https://github.com/doedja/warren/releases/tag/v0.4.5
[0.4.4]: https://github.com/doedja/warren/releases/tag/v0.4.4
[0.4.3]: https://github.com/doedja/warren/releases/tag/v0.4.3
[0.4.2]: https://github.com/doedja/warren/releases/tag/v0.4.2
[0.4.1]: https://github.com/doedja/warren/releases/tag/v0.4.1
[0.4.0]: https://github.com/doedja/warren/releases/tag/v0.4.0
[0.3.0]: https://github.com/doedja/warren/releases/tag/v0.3.0
[0.2.0]: https://github.com/doedja/warren/releases/tag/v0.2.0
