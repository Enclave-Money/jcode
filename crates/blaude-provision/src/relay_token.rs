//! Signed, team-scoped capabilities for the Clerk directory relay.
//!
//! A team VM needs to deliver invitations and update the invited user's team
//! stamp. It must never receive the Clerk backend key: that key controls the
//! whole Clerk instance. Instead the provisioning service signs this narrow
//! capability, containing only the team's own URL and name. The VM can ask the
//! same service to operate on that one team's metadata, but cannot call Clerk
//! directly or forge a capability for another team.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const TOKEN_VERSION: &str = "v1";
const SIGNING_CONTEXT: &[u8] = b"blaude-clerk-directory-relay-v1\0";
const MINIMUM_KEY_BYTES: usize = 32;

static SIGNING_KEY: OnceLock<Vec<u8>> = OnceLock::new();

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RelayClaims {
    pub ws_url: String,
    pub team_name: String,
    pub issued_at: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Configure the process-wide relay signing key before serving requests.
pub fn configure_relay_signing_key(secret: &[u8]) -> Result<(), String> {
    let secret = secret
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    if secret.len() < MINIMUM_KEY_BYTES {
        return Err(format!(
            "the relay signing key must contain at least {MINIMUM_KEY_BYTES} bytes"
        ));
    }
    SIGNING_KEY
        .set(secret)
        .map_err(|_| "the relay signing key was configured more than once".to_string())
}

fn configured_key() -> Result<&'static [u8], String> {
    SIGNING_KEY
        .get()
        .map(Vec::as_slice)
        .ok_or_else(|| "the relay signing key is not configured".to_string())
}

fn sign_payload(key: &[u8], encoded_payload: &str) -> Result<Vec<u8>, String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|_| "the relay signing key is invalid".to_string())?;
    mac.update(SIGNING_CONTEXT);
    mac.update(encoded_payload.as_bytes());
    Ok(mac.finalize().into_bytes().to_vec())
}

fn encode_with_key(claims: &RelayClaims, key: &[u8]) -> Result<String, String> {
    let payload = serde_json::to_vec(claims)
        .map_err(|error| format!("could not encode relay claims: {error}"))?;
    let encoded_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
    let signature = sign_payload(key, &encoded_payload)?;
    let encoded_signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature);
    Ok(format!(
        "{TOKEN_VERSION}.{encoded_payload}.{encoded_signature}"
    ))
}

fn decode_with_key(token: &str, key: &[u8]) -> Result<RelayClaims, String> {
    let mut pieces = token.trim().split('.');
    let (Some(version), Some(payload), Some(signature), None) =
        (pieces.next(), pieces.next(), pieces.next(), pieces.next())
    else {
        return Err("the relay capability is malformed".to_string());
    };
    if version != TOKEN_VERSION {
        return Err("the relay capability version is not supported".to_string());
    }

    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| "the relay capability signature is malformed".to_string())?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|_| "the relay signing key is invalid".to_string())?;
    mac.update(SIGNING_CONTEXT);
    mac.update(payload.as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| "the relay capability signature is invalid".to_string())?;

    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| "the relay capability payload is malformed".to_string())?;
    let claims: RelayClaims = serde_json::from_slice(&payload)
        .map_err(|_| "the relay capability payload is unreadable".to_string())?;
    if claims.ws_url.trim().is_empty() || claims.team_name.chars().any(char::is_control) {
        return Err("the relay capability contains invalid team data".to_string());
    }
    if claims.issued_at > now_secs().saturating_add(300) {
        return Err("the relay capability was issued in the future".to_string());
    }
    Ok(claims)
}

pub fn mint_relay_token(ws_url: &str, team_name: &str) -> Result<String, String> {
    encode_with_key(
        &RelayClaims {
            ws_url: ws_url.to_string(),
            team_name: team_name.to_string(),
            issued_at: now_secs(),
        },
        configured_key()?,
    )
}

pub fn verify_relay_token(token: &str) -> Result<RelayClaims, String> {
    decode_with_key(token, configured_key()?)
}

#[cfg(test)]
mod tests {
    use super::{RelayClaims, decode_with_key, encode_with_key};

    fn claims() -> RelayClaims {
        RelayClaims {
            ws_url: "wss://34-93-93-41.sslip.io:443/api".to_string(),
            team_name: "Rabani's team".to_string(),
            issued_at: 1,
        }
    }

    #[test]
    fn capability_round_trips_and_is_team_scoped() {
        let token = encode_with_key(&claims(), b"0123456789abcdef0123456789abcdef")
            .expect("encode capability");
        assert_eq!(
            decode_with_key(&token, b"0123456789abcdef0123456789abcdef")
                .expect("decode capability"),
            claims()
        );
    }

    #[test]
    fn tampering_and_the_wrong_key_are_rejected() {
        let key = b"0123456789abcdef0123456789abcdef";
        let token = encode_with_key(&claims(), key).expect("encode capability");
        assert!(decode_with_key(&token, b"fedcba9876543210fedcba9876543210").is_err());

        let mut tampered = token.into_bytes();
        let index = tampered
            .iter()
            .position(|byte| *byte == b'.')
            .expect("version separator")
            + 2;
        tampered[index] = if tampered[index] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).expect("ASCII token");
        assert!(decode_with_key(&tampered, key).is_err());
    }
}
