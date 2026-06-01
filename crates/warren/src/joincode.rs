//! Join code: one opaque string that carries everything a device needs to join
//! the pool (hub address, TLS on/off, pinned fingerprint, enrollment token), so
//! `warren node run --join <code>` replaces four separate flags. The hub builds
//! it (shown in the dashboard); the node decodes it. One encoder, one decoder.

use anyhow::{anyhow, Result};
use base64::Engine;

const PREFIX: &str = "warren1.";

/// Decoded contents of a join code.
pub struct Join {
    pub hub: String,
    pub tls: bool,
    pub fingerprint: Option<String>,
    pub token: Option<String>,
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Pack a join code. Fields are newline-joined then base64url-encoded; none of
/// them (host:port, "0"/"1", hex fingerprint, alnum token) contain a newline.
pub fn encode(hub: &str, tls: bool, fingerprint: Option<&str>, token: Option<&str>) -> String {
    let payload = format!(
        "{}\n{}\n{}\n{}",
        hub,
        if tls { "1" } else { "0" },
        fingerprint.unwrap_or(""),
        token.unwrap_or("")
    );
    format!("{PREFIX}{}", b64().encode(payload.as_bytes()))
}

/// Parse a join code produced by [`encode`].
pub fn decode(code: &str) -> Result<Join> {
    let body = code
        .trim()
        .strip_prefix(PREFIX)
        .ok_or_else(|| anyhow!("not a warren join code (expected it to start with {PREFIX})"))?;
    let bytes = b64()
        .decode(body)
        .map_err(|_| anyhow!("malformed join code"))?;
    let s = String::from_utf8(bytes).map_err(|_| anyhow!("malformed join code"))?;
    let parts: Vec<&str> = s.split('\n').collect();
    if parts.len() != 4 {
        return Err(anyhow!("malformed join code"));
    }
    let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
    Ok(Join {
        hub: parts[0].to_string(),
        tls: parts[1] == "1",
        fingerprint: opt(parts[2]),
        token: opt(parts[3]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_full() {
        let code = encode(
            "hub.example.com:7000",
            true,
            Some("c8145bad"),
            Some("tok123"),
        );
        let j = decode(&code).unwrap();
        assert_eq!(j.hub, "hub.example.com:7000");
        assert!(j.tls);
        assert_eq!(j.fingerprint.as_deref(), Some("c8145bad"));
        assert_eq!(j.token.as_deref(), Some("tok123"));
    }

    #[test]
    fn roundtrip_plain_no_token() {
        let code = encode("127.0.0.1:7000", false, None, None);
        let j = decode(&code).unwrap();
        assert_eq!(j.hub, "127.0.0.1:7000");
        assert!(!j.tls);
        assert!(j.fingerprint.is_none());
        assert!(j.token.is_none());
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("not-a-code").is_err());
        assert!(decode("warren1.@@@").is_err());
    }
}
