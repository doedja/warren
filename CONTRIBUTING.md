# Contributing to warren

Thanks for taking a look. warren is one Rust workspace with two crates:
`warren-proto` (the wire messages) and `warren` (the `hub` and `node` binary).

## Build and test

```bash
cargo build --release     # stable Rust >= 1.74; binary at target/release/warren
cargo test                # unit tests plus the end-to-end tests under crates/warren/tests
cargo clippy --all-targets
cargo fmt --all
```

CI runs `cargo fmt --all --check`, clippy, and the test suite. Run all four
locally before opening a pull request, since a formatting miss alone fails CI.

## Trying a change end to end

The fastest loop is the test harness in `crates/warren/tests/e2e.rs`: it boots a
hub, a node, and a target on ephemeral ports and drives real proxy traffic
through them. Add a test there for any behavior change to the routing, auth, or
data path.

To run it by hand: start a hub with `cargo run -- hub --proxy-user me
--proxy-pass pw` (it prints a join token), join a node with `cargo run -- node
run --hub 127.0.0.1:7000 --token <printed>`, then
`curl -x http://me:pw@127.0.0.1:8000 https://api.ipify.org`.

## Pull requests

- Keep changes focused. One concern per pull request is easier to review.
- Update the README or the dashboard text when behavior or flags change.
- If you change a wire message in `warren-proto`, bump `PROTOCOL_VERSION`. The
  hub rejects a node speaking a different version.
- Do not commit secrets, real tokens, or `.db`/key files.

## Writing style for docs

Plain and direct. Short sentences. No marketing filler. Match the tone of the
existing README.

## Security

Do not file security issues in public. See [SECURITY.md](SECURITY.md).
