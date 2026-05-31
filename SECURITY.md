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

- **The node link.** Without `--tls` the hub-to-node link is plaintext, by
  design, for a hub reached only over localhost or a private network (a tailnet,
  a LAN, a WireGuard mesh). On a public hub, run with `--tls`: the device pins
  the hub's certificate fingerprint, and each data connection carries a random
  per-dial nonce so a guessed connection id cannot hijack a tunnel.
- **Enrollment.** A device proves possession of its ed25519 key with a signed,
  time-bounded challenge. An enrollment token auto-approves a new key; without a
  token the device waits for approval in the dashboard. Treat the token as a
  secret: anyone holding it can add a device to your pool.
- **The proxy.** Client access is a username and password. The admin dashboard
  is behind HTTP Basic auth (the admin token is the password). Put the admin
  port behind TLS (a reverse proxy or tunnel) when it is reachable from outside.

## Good practice

- Use `--tls` for any hub reachable from the internet.
- Keep enrollment tokens and the admin token out of shared shell history; prefer
  the `WARREN_*` environment variables.
- Only add devices you own, and only proxy traffic you are allowed to send.
