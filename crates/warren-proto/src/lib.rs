//! transport-agnostic wire messages between hub and node, serialized with
//! postcard; length-prefixed framing is done by the warren crate, not here.

use serde::{Deserialize, Serialize};

/// Protocol version. Bump on any breaking change to the message shapes OR the
/// transport framing. v4: the node<->hub link is now a single yamux-multiplexed
/// connection (one logical stream per request) instead of one TCP connection
/// per request. v5: adds UDP relay (SOCKS5 UDP ASSOCIATE) via `HubToNode::UdpOpen`
/// + the `UdpDatagram` frame, so a node can carry a client's UDP traffic.
pub const PROTOCOL_VERSION: u16 = 5;

/// Stable identity the hub assigns to a node at enrollment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

/// What the node tells the hub the moment its control stream opens.
///
/// The node proves possession of its ed25519 key by signing
/// `b"warren-node-auth" || pubkey || timestamp.to_le_bytes()`. Enrollment is
/// then: an approved pubkey, or a valid `token` (which auto-approves the key).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    /// ed25519 public key (32 bytes).
    pub pubkey: Vec<u8>,
    /// Optional enrollment token (Mode B: presenting it auto-approves the key).
    pub token: Option<String>,
    /// Unix seconds; signed, so a captured Hello cannot be replayed later.
    pub timestamp: u64,
    /// ed25519 signature over the auth message above (64 bytes).
    pub signature: Vec<u8>,
    pub node_name: String,
    pub platform: Platform,
    pub agent_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Platform {
    Linux,
    Windows,
    MacOs,
    Android,
    Other,
}

/// Hub's response to [`Hello`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HelloReply {
    Welcome {
        node_id: NodeId,
    },
    Reject {
        reason: String,
    },
    /// Key recorded as pending; an admin must approve it. `code` is a short
    /// human-friendly identifier (a prefix of the key fingerprint).
    Pending {
        code: String,
    },
}

/// Messages from hub to node on the control connection after the handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HubToNode {
    Dial {
        conn_id: u64,
        /// Random per-dial secret. The node must echo it in [`DataHello`] so the
        /// hub can prove the data connection came from the node it asked to
        /// dial, not from anyone who guessed `conn_id`.
        nonce: u64,
        host: String,
        port: u16,
    },
    Ping {
        nonce: u64,
    },
    /// Set up a UDP relay for a SOCKS5 UDP ASSOCIATE. Like [`Dial`] but the node
    /// opens a relay stream (tagged with `conn_id`/`nonce` in [`DataHello`]) that
    /// carries [`UdpDatagram`] frames instead of a raw TCP splice. Targets are
    /// per-datagram, so no host/port here. Added at the END to keep the existing
    /// variant discriminants stable.
    UdpOpen {
        conn_id: u64,
        nonce: u64,
    },
}

/// One UDP datagram carried over a relay stream. Hub->node: the destination to
/// send to. Node->hub: the source a reply came from. Length-prefixed by the
/// `warren` crate's framing, same as every other message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UdpDatagram {
    pub host: String,
    pub port: u16,
    pub data: Vec<u8>,
}

/// Messages from node to hub on the control connection after the handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeToHub {
    Pong {
        nonce: u64,
    },
    DialFailed {
        conn_id: u64,
        reason: String,
    },
    /// Node self-reports its public egress IP + geo (best-effort, periodic).
    /// Added at the END so older nodes (which never send it) stay wire-compatible
    /// with a newer hub; no PROTOCOL_VERSION bump needed.
    Info {
        public_ip: Option<String>,
        country: Option<String>,
        city: Option<String>,
    },
    /// Round-trip latency (ms) of the node's own internet check: a rough
    /// health/perf signal. A SEPARATE variant (not a field on Info) so it stays
    /// additive: v0.1.6 nodes already send the 3-field Info, and appending a
    /// field there would break decoding. Old nodes never send this; no bump.
    Latency {
        ms: u32,
    },
}

/// First frame sent by the node on a data connection to identify which
/// proxied connection this stream belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataHello {
    pub conn_id: u64,
    /// Echoes the `nonce` from the matching [`HubToNode::Dial`].
    pub nonce: u64,
}

/// First framed message on ANY node-to-hub connection. Tells the hub whether
/// this socket is the control connection or a data connection for a pending
/// dial.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Greeting {
    Control(Hello),
    Data(DataHello),
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Serialize a message to a `Vec<u8>` using postcard.
pub fn encode<T: serde::Serialize>(msg: &T) -> Vec<u8> {
    postcard::to_allocvec(msg).expect("postcard encode")
}

/// Deserialize a message from a byte slice using postcard.
pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_id_eq() {
        let a = NodeId("node-1".into());
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn test_protocol_version() {
        assert_eq!(PROTOCOL_VERSION, 5);
    }

    #[test]
    fn test_udp_datagram_roundtrip() {
        let d = UdpDatagram {
            host: "1.1.1.1".into(),
            port: 53,
            data: vec![0xde, 0xad, 0xbe, 0xef],
        };
        let decoded: UdpDatagram = decode(&encode(&d)).expect("decode");
        assert_eq!(d, decoded);
    }

    #[test]
    fn test_hello_roundtrip() {
        let original = Hello {
            protocol_version: PROTOCOL_VERSION,
            pubkey: vec![1u8; 32],
            token: Some("tok_abc".into()),
            timestamp: 1700000000,
            signature: vec![2u8; 64],
            node_name: "test-node".into(),
            platform: Platform::Linux,
            agent_version: "0.1.0".into(),
        };
        let encoded = encode(&original);
        let decoded: Hello = decode(&encoded).expect("decode should succeed");
        assert_eq!(original, decoded);
    }
}
