//! Node identity: an ed25519 keypair the node uses to authenticate to the hub.
//!
//! Enrollment proves possession of the key by signing
//! `b"warren-node-auth" || pubkey || timestamp.to_le_bytes()`. The hub then
//! either recognizes the pubkey as approved, or auto-approves it on a valid
//! enrollment token. The signed timestamp bounds replay of a captured Hello.

use anyhow::{Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

const AUTH_DOMAIN: &[u8] = b"warren-node-auth";

pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    /// Load the keypair from `path`, generating and saving a new one if absent.
    pub fn load_or_create(path: &str) -> Result<Identity> {
        if std::path::Path::new(path).exists() {
            let bytes = std::fs::read(path).with_context(|| format!("read key {path}"))?;
            let arr: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .context("bad key file (expected 32 bytes)")?;
            Ok(Identity {
                signing: SigningKey::from_bytes(&arr),
            })
        } else {
            let signing = SigningKey::generate(&mut rand::rngs::OsRng);
            if let Some(dir) = std::path::Path::new(path).parent() {
                std::fs::create_dir_all(dir).ok();
            }
            std::fs::write(path, signing.to_bytes())
                .with_context(|| format!("write key {path}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
            Ok(Identity { signing })
        }
    }

    pub fn pubkey(&self) -> Vec<u8> {
        self.signing.verifying_key().to_bytes().to_vec()
    }

    /// ed25519 signature over the auth message for `timestamp`.
    pub fn sign_auth(&self, timestamp: u64) -> Vec<u8> {
        let msg = auth_message(&self.pubkey(), timestamp);
        self.signing.sign(&msg).to_bytes().to_vec()
    }
}

fn auth_message(pubkey: &[u8], timestamp: u64) -> Vec<u8> {
    let mut m = Vec::with_capacity(AUTH_DOMAIN.len() + pubkey.len() + 8);
    m.extend_from_slice(AUTH_DOMAIN);
    m.extend_from_slice(pubkey);
    m.extend_from_slice(&timestamp.to_le_bytes());
    m
}

/// Hub side: verify a node's enrollment signature for the given timestamp.
pub fn verify_auth(pubkey: &[u8], timestamp: u64, signature: &[u8]) -> bool {
    let pk: [u8; 32] = match pubkey.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let vk = match VerifyingKey::from_bytes(&pk) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let sig: [u8; 64] = match signature.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };
    vk.verify_strict(
        &auth_message(pubkey, timestamp),
        &Signature::from_bytes(&sig),
    )
    .is_ok()
}

/// Full hex of a pubkey (the stable node key id).
pub fn fingerprint(pubkey: &[u8]) -> String {
    hex::encode(pubkey)
}

/// Short human-friendly code (first 8 hex chars of the fingerprint).
pub fn short_code(pubkey: &[u8]) -> String {
    let n = pubkey.len().min(4);
    hex::encode(&pubkey[..n])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_then_verify_roundtrips() {
        let dir = std::env::temp_dir().join(format!("warren-key-{}", std::process::id()));
        let path = dir.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);
        let id = Identity::load_or_create(&path).unwrap();
        let ts = 1_700_000_000;
        let sig = id.sign_auth(ts);
        assert!(verify_auth(&id.pubkey(), ts, &sig));
        // Wrong timestamp fails.
        assert!(!verify_auth(&id.pubkey(), ts + 1, &sig));
        // Persisted: reloading yields the same pubkey.
        let id2 = Identity::load_or_create(&path).unwrap();
        assert_eq!(id.pubkey(), id2.pubkey());
        let _ = std::fs::remove_file(&path);
    }
}
