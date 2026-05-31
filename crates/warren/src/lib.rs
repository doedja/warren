//! warren library surface: hub, node, wire, and proxy modules.
//!
//! Exposed as a lib (in addition to the `warren` binary) so integration tests
//! under `tests/` can drive the hub and node directly.

#![allow(dead_code)]

pub mod hub;
pub mod node;
pub mod proxy;
pub mod wire;
