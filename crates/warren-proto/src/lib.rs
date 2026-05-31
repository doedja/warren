//! transport-agnostic wire messages between hub and node, serialized with
//! postcard; length-prefixed framing is done by the warren crate, not here.

use serde::{Deserialize, Serialize};

/// Protocol version. Bump on any breaking change to the message shapes.
pub const PROTOCOL_VERSION: u16 = 1;

/// Stable identity the hub assigns to a node at enrollment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

/// What the node tells the hub the moment its control stream opens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    /// Enrollment token (first connect) OR a previously issued node credential.
    pub token: String,
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
    Welcome { node_id: NodeId },
    Reject { reason: String },
}

/// Messages from hub to node on the control connection after the handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HubToNode {
    Dial {
        conn_id: u64,
        host: String,
        port: u16,
    },
    Ping {
        nonce: u64,
    },
    Drain {
        reason: String,
    },
}

/// Messages from node to hub on the control connection after the handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeToHub {
    Pong { nonce: u64 },
    DialFailed { conn_id: u64, reason: String },
}

/// First frame sent by the node on a data connection to identify which
/// proxied connection this stream belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataHello {
    pub conn_id: u64,
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
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn test_hello_roundtrip() {
        let original = Hello {
            protocol_version: 1,
            token: "tok_abc".into(),
            node_name: "test-node".into(),
            platform: Platform::Linux,
            agent_version: "0.1.0".into(),
        };
        let encoded = encode(&original);
        let decoded: Hello = decode(&encoded).expect("decode should succeed");
        assert_eq!(original, decoded);
    }
}
