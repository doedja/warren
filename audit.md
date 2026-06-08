# warren Repository Audit

**Date:** 2026-06-08  
**Version audited:** v0.4.8 (`main`, synced with `origin/main`)  
**Auditor scope:** Full repository — architecture, code quality, security, dependencies, testing, CI/CD, documentation, and operations.

---

## Executive Summary

warren is a well-engineered, self-hosted residential proxy pool: one Rust binary that runs as either a **hub** (control plane + client-facing proxy) or a **node** (outbound-only egress agent on devices you own). The codebase is compact (~6,500 lines of Rust), actively maintained (v0.4.8 released 2026-06-04), and shows deliberate security thinking in the node–hub link, enrollment flow, and installer supply chain.

**Overall assessment: production-ready for its stated threat model** (devices and accounts you control), with clear gaps that operators should understand: plaintext credential storage, no built-in TLS on the admin surface, in-memory metering, and no brute-force rate limiting.

| Area | Rating | Notes |
|------|--------|-------|
| Architecture | Strong | Clean separation of proto/binary; yamux multiplexing; minimal deps |
| Code quality | Strong | Clippy-clean, well-commented, focused modules |
| Security (node link) | Strong | TLS + fingerprint pin, ed25519 auth, per-dial nonces |
| Security (proxy/admin) | Adequate | Basic auth works; passwords at rest are plaintext |
| Testing | Good | 46 tests, 4 e2e paths; no fuzz/load tests |
| CI/CD | Strong | fmt, clippy, audit, MSRV, cross-platform builds, checksum releases |
| Documentation | Excellent | README, SECURITY.md, CHANGELOG, inline design notes |
| Operability | Good | Docker, installers, dashboard; admin TLS is operator responsibility |

---

## 1. Project Overview

| Item | Value |
|------|-------|
| Language | Rust 2021 edition |
| License | Apache-2.0 |
| MSRV (declared) | 1.88 (`Cargo.toml` `rust-version`) |
| Workspace crates | `warren-proto` (wire messages), `warren` (binary + lib) |
| Current version | 0.4.8 |
| Platforms | Linux (x86_64/arm64), macOS (x86_64/arm64), Windows (x86_64), Termux/Android |
| Persistence | SQLite (`warren.db`) — tokens, proxy users, node keys, pending nodes |
| External services | ip-api.com (geo lookup, plaintext HTTP), GitHub Releases (installers) |

**Purpose:** Turn owned devices into a private residential proxy pool. Devices dial *out* to the hub (no inbound ports). Clients use HTTP CONNECT, SOCKS5, or plain HTTP on one port, with routing via proxy username conventions (`user+device`, `user-session-KEY`, `user-region-COUNTRY`, `user-group-NAME`).

---

## 2. Architecture

### 2.1 High-level data flow

```
Client ──HTTP/SOCKS5──► Hub (proxy :8000)
                          │
                          ├── picks healthy node (routing + failover)
                          │
                          └── yamux link ──► Node ──TCP/UDP──► Target
                               (TLS :7000)      (residential egress IP)
```

### 2.2 Module map

| Module | Responsibility |
|--------|----------------|
| `warren-proto` | Postcard-serialized messages; `PROTOCOL_VERSION = 6`, `MIN_PROTOCOL_VERSION = 4` |
| `hub.rs` | Node registry, proxy handling, routing, UDP relay, admin API (~2,200 LOC) |
| `node.rs` | Outbound agent, service install (systemd/launchd/schtasks), auto-update |
| `mux.rs` | Yamux client/server drivers over one TLS/plain connection |
| `store.rs` | SQLite CRUD for tokens, users, node keys |
| `identity.rs` | ed25519 node keypair + signed enrollment |
| `tls.rs` | Self-signed hub cert, fingerprint pinning |
| `joincode.rs` | One-paste enrollment codes (base64url payload) |
| `proxy.rs` / `socks5.rs` | Client-facing protocol parsers |
| `admin_ui.rs` | Embedded single-page dashboard (~400 LOC HTML/CSS/JS) |

### 2.3 Design strengths

- **Outbound-only nodes** eliminate NAT/port-forward complexity.
- **Yamux multiplexing** replaces one-TCP-connection-per-request; stream open is cheap.
- **Protocol version range** (`MIN..=CURRENT`) allows additive bumps without fleet-wide reinstall.
- **Per-dial nonce** on data streams prevents `conn_id` guessing attacks.
- **Health-aware routing** with host-specific failure tracking, in-flight caps (64/node), and sticky sessions (10 min TTL).
- **Graceful node shutdown** with in-flight drain budget (20s) and staggered auto-update (0–5 min jitter).

### 2.4 Architectural concerns

| Concern | Severity | Detail |
|---------|----------|--------|
| `hub.rs` size | Low | Single ~2,200-line file mixes proxy, admin API, routing, UDP relay, and metrics. Harder to navigate than the rest of the codebase. |
| In-memory state | Medium | Byte counters, sticky sessions, UDP associations, and node stats (partially) live only in RAM — lost on hub restart. |
| No hub clustering | Info | One SQLite file, one hub process; scaling is vertical + more nodes, not multi-hub. |

---

## 3. Security Audit

Threat model is documented in `SECURITY.md`: devices and accounts you control; enrollment tokens and admin tokens are secrets; TLS on the node link for internet-facing hubs.

### 3.1 Node–hub link (Strong)

| Control | Implementation |
|---------|----------------|
| Transport encryption | TLS on by default (`rustls` + `ring`); self-signed cert persisted for stable fingerprint |
| Certificate trust | Node pins SHA256 fingerprint via `--hub-fingerprint` or join code; `--insecure` is dev-only |
| Node identity | ed25519 keypair; `Hello` signs `warren-node-auth \|\| pubkey \|\| timestamp` |
| Replay protection | 120-second timestamp skew window |
| Stream binding | Random per-dial `nonce` echoed in `DataHello` |
| Enrollment | Valid token → auto-approve; else pending + dashboard approval; approved pubkey whitelist |
| Protocol gate | Rejects nodes outside `[MIN_PROTOCOL_VERSION, PROTOCOL_VERSION]` |

### 3.2 Client proxy (Adequate)

| Control | Status |
|---------|--------|
| Authentication | HTTP Basic / SOCKS5 user-pass when users exist in DB |
| Routing in username | Parsed after auth; reserved markers blocked at user creation |
| Fail-closed routing | Pinned device/region/group with no match → 502, no silent fallback |
| UDP amplification guard | 2,000 datagrams/sec per association; bounded queue (1024); client IP lock on first datagram |

**Gaps:**

- **Proxy passwords stored plaintext** in SQLite (`store.rs` `proxy_users.password`). Anyone with DB file access has all credentials.
- **Proxy password check is not constant-time** — SQL `username = ? AND password = ?` comparison. Admin token comparison *is* constant-time (`ct_eq` in `hub.rs`), but proxy auth is not.
- **No rate limiting** on proxy auth failures or enrollment attempts. A public proxy port is vulnerable to credential stuffing.
- **No egress ACL on nodes** — an enrolled node dials any `host:port` the hub requests. Compromised node = open residential egress (by design for a proxy, but worth stating).

### 3.3 Admin dashboard & API (Adequate with operator duties)

| Control | Status |
|---------|--------|
| Authentication | HTTP Basic; any username + admin token as password |
| Token comparison | Constant-time (`ct_eq`) since v0.4.8 |
| API surface | REST for nodes, tokens, users, pending approval, metrics, logs |
| Secret exposure in API | `/api/info` returns proxy password for copy-paste commands (gated by same admin auth) |

**Gaps:**

- **No TLS on admin port** — `axum` serves plain HTTP. `SECURITY.md` and `docker-compose.yml` correctly say to put `:9000` behind a reverse proxy or tunnel; this is operator responsibility, not enforced.
- **No CSRF tokens** — dashboard uses `fetch()` with browser-stored Basic auth credentials; same-origin policy helps, but cross-site issues depend on browser behavior.
- **No audit log** for admin actions (token create/delete, node approve/revoke).

### 3.4 Join codes & installers

| Control | Status |
|---------|--------|
| Join code format | `warren1.` + base64url(`hub\ntls\nfingerprint\ntoken`) — **not encrypted**, only encoded |
| Installer integrity | `install.sh` / `install.ps1` verify SHA256 from release assets; fail closed |
| Bypass | `WARREN_SKIP_VERIFY=1` disables verification (documented, not recommended) |
| Auto-update | Opt-in; re-fetches from `raw.githubusercontent.com`; staggered restarts (v0.4.8) |

**Gaps:**

- Possession of a join code = possession of enrollment token + hub address. Treat join codes like passwords.
- Auto-update trusts GitHub + TLS to GitHub; checksum mitigates tampered binaries but not a compromised release pipeline.

### 3.5 Privacy & third parties

- **Geo lookup:** Hub calls `ip-api.com:80` (plaintext HTTP) with node IPs. Node also self-reports via same service periodically.
- **No telemetry** to vendor; all state is local except these optional geo calls.

### 3.6 Platform-specific notes

- **Windows node service** runs as `SYSTEM` with highest privileges — standard for always-on agents but high blast radius if binary is compromised.
- **Node key file** gets `0600` on Unix; no Windows ACL hardening documented.
- **Termux path** uses wake locks and boot scripts; relies on user installing Termux:Boot/API.

---

## 4. Dependencies

**Direct runtime dependencies (warren):** `tokio`, `axum`, `rusqlite`, `rustls`/`tokio-rustls`, `yamux`, `ed25519-dalek`, `rcgen`, `postcard`, `clap`, `tracing`, and utilities (`anyhow`, `serde`, `base64`, `sha2`, `hex`, `rand`, `futures`, `tokio-util`).

### 4.1 `cargo audit` (2026-06-08)

```
RUSTSEC-2023-0089: atomic-polyfill 1.0.3 — unmaintained
  └── heapless → postcard → warren-proto
```

No critical or high CVEs in the lockfile. One **unmaintained** transitive advisory via `postcard`/`heapless`. Low practical risk for this use case, but worth tracking if `postcard` offers an updated path.

### 4.2 Dependency choices (notable)

- **`ring` over `aws-lc-rs`** for TLS — avoids cmake in Docker builds (documented in `Cargo.toml`).
- **`rusqlite` bundled** — ships SQLite amalgamation; predictable builds, larger binary.
- **No HTTP client crate** — hand-rolled HTTP for ip-api.com and proxy parsing keeps deps small.

---

## 5. Code Quality

### 5.1 Static analysis (local run, 2026-06-08)

| Check | Result |
|-------|--------|
| `cargo test` | **46 passed** (42 unit + 4 e2e) |
| `cargo clippy --all-targets -- -D warnings` | **Clean** |
| `cargo fmt --check` | Not run locally; enforced in CI |

### 5.2 Code style & maintainability

**Strengths:**

- Extensive inline comments explain *why*, not just what — especially around concurrency, yamux driver lifecycle, and edge cases (stale TCP half-open reconnects, auto-update detachment).
- Pure functions extracted for testability (`route_rank`, `effective_fails`, `parse_route`, `should_schedule_update`).
- Validation at data-entry boundaries (`validate_token_name`, username reserved-character checks).
- Release profile: LTO, `codegen-units = 1`, `strip = true` — good for single static binaries.

**Weaknesses:**

- `hub.rs` concentration — admin routes, proxy logic, UDP relay, and Prometheus formatting could be split.
- Some `Mutex::lock().unwrap()` in async contexts (geo lookup, node info) — acceptable for short critical sections but could panic on poison.
- `postcard::encode` uses `.expect()` in `warren-proto` — infallible for in-memory types, but panics on encode failure.

### 5.3 Documentation drift

| Doc | Says | Reality |
|-----|------|---------|
| `README.md` Build section | "stable >= 1.74" | `Cargo.toml` declares `rust-version = "1.88"` |
| `CONTRIBUTING.md` | "stable Rust >= 1.74" | CI MSRV job builds on **1.88** |
| `CHANGELOG.md` v0.4.6 | "MSRV (Rust 1.74)" | Superseded by 1.88 floor (noted in CI comments: `time` crate) |

Recommend aligning README/CONTRIBUTING with 1.88.

---

## 6. Testing

### 6.1 Coverage by area

| Area | Unit tests | E2E tests |
|------|------------|-----------|
| Routing (`parse_route`, `route_rank`, health) | Yes | Device pinning |
| Identity (ed25519 sign/verify) | Yes | — |
| Join codes | Yes | — |
| Store validation | Yes | — |
| SOCKS5 / proxy parsing | Yes | — |
| Wire framing | Yes | — |
| Admin `ct_eq` | Yes | — |
| Node backoff/drain/auto-update | Yes | — |
| Full proxy path | — | HTTP CONNECT round-trip |
| Proxy auth | — | 407 on bad creds |
| UDP ASSOCIATE | — | Yes |

### 6.2 Gaps

- **No fuzz tests** for SOCKS5, HTTP parser, join code decoder, or postcard frames.
- **No TLS integration tests** in e2e (all e2e runs `--no-tls`).
- **No load/concurrency tests** — routing under many simultaneous dials, yamux backpressure, UDP flood behavior.
- **No admin API tests** — auth, token CRUD, approve/deny flows untested in automation.
- **No test for region/group/session routing** in e2e (only device pinning).

---

## 7. CI/CD & Release Engineering

### 7.1 CI (`.github/workflows/ci.yml`)

| Job | Purpose |
|-----|---------|
| `check` (ubuntu) | `fmt --check`, clippy `-D warnings`, build, test |
| `cross-compile` (macOS, Windows) | clippy + build on native targets |
| `msrv` | Build on Rust **1.88** |
| `audit` | `cargo audit` |

### 7.2 Release (`.github/workflows/release.yml`)

- Triggered on `v*` tags.
- Builds 5 targets: `linux-x86_64`, `linux-arm64` (musl), `macos-x86_64`, `macos-arm64`, `windows-x86_64`.
- Publishes `.tar.gz` + `.sha256` per asset.
- CHANGELOG section auto-published as release notes.

### 7.3 Gaps

- **No signed releases** (GPG/minisign) — integrity relies on HTTPS + SHA256 in installer.
- **No Docker image publish** — Dockerfile exists; compose builds locally.
- **No automated e2e in CI with TLS** or multi-node scenarios.

---

## 8. Documentation & Operator Experience

### 8.1 Strengths

- **README** is thorough: setup, routing syntax, tun2socks, Termux, auto-update risks, flag tables.
- **SECURITY.md** — clear reporting path and threat-model notes.
- **CONTRIBUTING.md** — concrete dev loop (e2e harness, protocol version bumps).
- **CHANGELOG** — detailed, includes security fixes with rationale.
- **Examples** — `docker-compose.yml`, `tun2socks.md`.

### 8.2 Gaps

- No runbook for **hub backup/restore** (SQLite + TLS cert directory).
- No guidance on **rotating** proxy passwords or enrollment tokens at scale.
- No **capacity planning** doc (connections per node, hub memory, SQLite limits).
- Dashboard is English-only, embedded in Rust string (no i18n).

---

## 9. Operational Considerations

| Topic | Behavior |
|-------|----------|
| Hub restart | Nodes reconnect automatically; sessions/meters reset |
| Node revoke | `delete_node` removes DB key; live connection stays until disconnect |
| TLS cert rotation | Changing cert dir or deleting cert changes fingerprint — nodes must re-join |
| Windows node | Scheduled task; auto-restart on failure (PowerShell path); fallback schtasks lacks restart |
| Logging | `tracing` to stdout + 500-line ring buffer for dashboard |
| Metrics | Prometheus text at `/metrics` (admin-gated): nodes online, bytes, dials, fails, UDP assocs |
| Database | Single-file SQLite; no migrations framework — schema created with `CREATE TABLE IF NOT EXISTS` |

---

## 10. Findings & Recommendations

### Critical

*None identified for the documented self-hosted threat model.*

### High

| # | Finding | Recommendation |
|---|---------|----------------|
| H1 | Admin API served over plain HTTP | Always terminate TLS in front of `:9000` on public networks; consider built-in TLS or documenting a Caddy/nginx snippet in compose |
| H2 | Proxy passwords stored and compared in plaintext | Hash passwords at rest (argon2/bcrypt); use constant-time compare of hash |

### Medium

| # | Finding | Recommendation |
|---|---------|----------------|
| M1 | No brute-force rate limiting on proxy or admin auth | Add per-IP or per-username lockout / exponential backoff |
| M2 | Join codes encode secrets without encryption | Document clearly; consider optional encryption with hub-side key for multi-tenant scenarios |
| M3 | In-memory byte/session state lost on restart | Persist counters to SQLite or document as ephemeral |
| M4 | MSRV documented as 1.74, actual floor is 1.88 | Update README and CONTRIBUTING |
| M5 | Revoked node keys remain connected until timeout | Optionally push disconnect on revoke, or document expected delay (~75s dead-peer timeout) |

### Low

| # | Finding | Recommendation |
|---|---------|----------------|
| L1 | `hub.rs` monolith | Split into `routing.rs`, `admin.rs`, `udp_relay.rs` |
| L2 | `atomic-polyfill` unmaintained advisory | Watch `postcard` updates; consider alternative serializer long-term |
| L3 | Geo lookup over plaintext HTTP | Use HTTPS endpoint or make geo opt-in |
| L4 | No e2e TLS tests | Add one e2e with fingerprint pinning |
| L5 | No fuzz testing | Fuzz SOCKS5/HTTP/joincode parsers |
| L6 | UDP byte metering not per-user | `handle_udp_associate` ignores `base_user` for metering |

---

## 11. Strengths (Summary)

1. **Coherent product vision** — collapses VPN + per-device proxy + rotator into one binary.
2. **Security-conscious node link** — fingerprint pinning, signed enrollment, nonces, version gating.
3. **Installer supply chain** — checksum verification before execute.
4. **Routing sophistication** — health tiers, host-aware failover, sticky sessions, region/group selectors.
5. **Production hardening details** — staggered fleet updates, graceful drain, stale-connection reconnect fix (v0.4.6), constant-time admin auth (v0.4.8).
6. **CI discipline** — clippy as errors, MSRV enforcement, cross-platform compile, dependency audit.
7. **Documentation quality** — rare for a project this size; honest about auto-update risks and TLS operator duties.

---

## 12. Verification Commands

Commands run during this audit:

```bash
cargo test                    # 46 passed
cargo clippy --all-targets --all-features -- -D warnings   # clean
cargo audit                   # 1 unmaintained warning (atomic-polyfill)
```

---

## Appendix A: File Inventory

| Path | Role |
|------|------|
| `crates/warren/src/*.rs` | Core implementation (17 files, ~6,514 LOC) |
| `crates/warren-proto/src/lib.rs` | Wire protocol |
| `crates/warren/tests/e2e.rs` | Integration tests |
| `install.sh` / `install.ps1` | Platform installers with checksum verify |
| `Dockerfile` | Hub container image |
| `examples/docker-compose.yml` | Reference deployment |
| `.github/workflows/ci.yml` | CI pipeline |
| `.github/workflows/release.yml` | Release binaries |

---

## Appendix B: Protocol Summary

- **Transport:** Length-prefixed postcard frames (max 1 MiB).
- **Multiplexing:** Yamux; control stream + one stream per proxied flow.
- **Current version:** 6 (UDP relay v5+, `HubVersion` self-update hint v6+).
- **Minimum accepted node version:** 4.

---

*End of audit.*