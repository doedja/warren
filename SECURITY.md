# Security policy

warren routes your traffic through devices you run, so its security matters. If
you find a vulnerability, please report it privately first.

## Reporting

- Use GitHub's private vulnerability reporting: the **Security** tab on
  github.com/doedja/warren, then "Report a vulnerability". That keeps the report
  private until a fix ships.
- Do not open a public issue for a security bug.
- Please include what you observed, how to reproduce it, and the impact. A proof
  of concept helps.

You can expect an acknowledgement within a few days.

## Scope and threat model

warren is built for devices and accounts you control. Useful things to know when
judging severity:

- **The node link.** TLS is on by default: the device pins the hub's certificate
  fingerprint (carried in the join code), and each data connection carries a
  random per-dial nonce so a guessed connection id cannot hijack a tunnel.
  `--no-tls` drops to plaintext, by design, for a hub reached only over localhost
  or a private network (a tailnet, a LAN, a WireGuard mesh); do not use it on a
  public hub.
- **Enrollment.** A device proves possession of its ed25519 key with a signed,
  time-bounded challenge (replayed enrollments are rejected: each must carry a
  newer timestamp than the last accepted for that key). A device may not claim a
  name already approved for another key. An enrollment token auto-approves a new
  key; without a token the device waits for approval in the dashboard. Treat the
  token as a secret: anyone holding it can add a device to your pool. Revoking a
  key in the dashboard tears down its live link immediately, not just on its next
  reconnect.
- **The proxy.** Client access is a username and password. The admin dashboard
  is behind HTTP Basic auth (the admin token is the password). Put the admin
  port behind TLS (a reverse proxy or tunnel) when it is reachable from outside.
  State-changing admin calls (create/delete tokens, users, keys) also require an
  `X-Warren-Admin: 1` request header, which the dashboard sends automatically and
  which blocks cross-site (CSRF) requests. Driving those routes from a script?
  Add `-H "X-Warren-Admin: 1"` (read-only `GET`s do not need it). Note that proxy
  passwords and enrollment tokens are stored in `warren.db` in the clear, so
  treat that file (and its backups) as secret material.

## Good practice

- Use `--tls` for any hub reachable from the internet.
- Keep enrollment tokens and the admin token out of shared shell history; prefer
  the `WARREN_*` environment variables.
- Only add devices you own, and only proxy traffic you are allowed to send.
