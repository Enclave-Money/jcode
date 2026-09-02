//! Who is asking.
//!
//! The Mac sends a Clerk SESSION token — a short-lived RS256 JWT — and this
//! verifies it against Clerk's published keys. Verification is a local
//! signature check, so it costs no round trip and, more importantly, consumes
//! nothing.
//!
//! That last point is the whole reason the Mac must not send its saved Clerk
//! credential directly. The persisted one is Clerk's ROTATING client token:
//! every use returns a replacement and presenting a spent one is treated as
//! token theft, which permanently revokes the client's sessions. Sending it
//! here would burn a person's sign-in to create a VM.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::RwLock;

/// The person a request belongs to. Every provisioning action is attributed
/// to one, so a runaway bill has a name on it.
#[derive(Debug, Clone)]
pub struct Caller {
    pub subject: String,
    pub email: Option<String>,
}

impl Caller {
    /// For logs. Prefers the email because that is how people are named
    /// everywhere else in blaude.
    pub fn label(&self) -> &str {
        self.email.as_deref().unwrap_or(&self.subject)
    }
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    exp: usize,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    kid: String,
    n: String,
    e: String,
}

#[derive(Debug, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

/// Clerk's signing keys, cached.
///
/// Cached because a token arrives on every poll of a running create — and a
/// create polls for minutes. Refetched when an unknown `kid` shows up, which
/// is what a key rotation looks like from here, rather than on a timer that
/// would either be too slow on rotation or too chatty the rest of the time.
pub struct Verifier {
    jwks_url: String,
    http: reqwest::Client,
    keys: RwLock<Cache>,
    allowed_emails: Option<Vec<String>>,
}

struct Cache {
    keys: HashMap<String, DecodingKey>,
    fetched: Option<Instant>,
}

impl Verifier {
    pub fn new(jwks_url: String, allowed_emails: Option<Vec<String>>) -> Arc<Self> {
        Arc::new(Self {
            jwks_url,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("http client"),
            keys: RwLock::new(Cache {
                keys: HashMap::new(),
                fetched: None,
            }),
            allowed_emails: allowed_emails.map(|list| {
                list.into_iter()
                    .map(|e| e.trim().to_ascii_lowercase())
                    .filter(|e| !e.is_empty())
                    .collect()
            }),
        })
    }

    async fn key_for(&self, kid: &str) -> Result<DecodingKey, String> {
        if let Some(key) = self.keys.read().await.keys.get(kid) {
            return Ok(key.clone());
        }
        self.refresh().await?;
        self.keys
            .read()
            .await
            .keys
            .get(kid)
            .cloned()
            .ok_or_else(|| format!("no signing key {kid}"))
    }

    async fn refresh(&self) -> Result<(), String> {
        // One refetch per 30s at most, so a stream of tokens signed by a key
        // that genuinely does not exist cannot turn into a request flood at
        // Clerk.
        {
            let cache = self.keys.read().await;
            if cache.fetched.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
                return Ok(());
            }
        }
        let jwks: Jwks = self
            .http
            .get(&self.jwks_url)
            .send()
            .await
            .map_err(|e| format!("could not reach Clerk for signing keys: {e}"))?
            .json()
            .await
            .map_err(|e| format!("Clerk's signing keys were unreadable: {e}"))?;
        let mut cache = self.keys.write().await;
        cache.keys.clear();
        for jwk in jwks.keys {
            if let Ok(key) = DecodingKey::from_rsa_components(&jwk.n, &jwk.e) {
                cache.keys.insert(jwk.kid, key);
            }
        }
        cache.fetched = Some(Instant::now());
        Ok(())
    }

    /// Verify one `Authorization: Bearer <jwt>` header.
    pub async fn verify(&self, header: Option<&str>) -> Result<Caller, String> {
        let raw = header
            .and_then(|h| h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer ")))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "no sign-in was presented".to_string())?;

        let header = decode_header(raw).map_err(|e| format!("not a readable token: {e}"))?;
        let kid = header.kid.ok_or_else(|| "token names no signing key".to_string())?;
        let key = self.key_for(&kid).await?;

        let mut validation = Validation::new(Algorithm::RS256);
        // Clerk session tokens are minted for a browser-ish audience and the
        // set varies by instance, so the audience is not the check that
        // matters here — the signature and expiry are. Issuer is pinned by
        // the JWKS URL: only that instance's keys are ever loaded.
        validation.validate_aud = false;
        let token = decode::<Claims>(raw, &key, &validation)
            .map_err(|e| format!("sign-in was not valid: {e}"))?;

        let caller = Caller {
            subject: token.claims.sub,
            email: token.claims.email.map(|e| e.to_ascii_lowercase()),
        };
        let _ = token.claims.exp; // enforced by `decode`; named so it reads as deliberate.

        if let Some(allowed) = &self.allowed_emails {
            let ok = caller
                .email
                .as_ref()
                .is_some_and(|e| allowed.iter().any(|a| a == e));
            if !ok {
                return Err(format!("{} is not allowed to create team servers", caller.label()));
            }
        }
        Ok(caller)
    }
}
