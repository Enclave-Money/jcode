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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, crypto};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::RwLock;

/// The verified subject's primary email, from Clerk's Backend API.
///
/// The session token's claims usually stop at the subject id, and an email in
/// the REQUEST would be whatever the client felt like typing — so when the
/// token itself carries none, the answer comes from Clerk, keyed by the
/// subject that DID verify.
///
/// The secret key is read from the secret mounted only in this Cloud Run
/// service. The explicit User-Agent is not decoration: api.clerk.com
/// sits behind Cloudflare, which 403s some default client UAs, and that 403
/// decodes as an empty answer — a lookup that silently "finds nothing".
pub async fn lookup_email(subject: &str) -> Option<String> {
    let key = crate::directory::clerk_secret()?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("reqwest/0.12")
        .build()
        .ok()?;
    let user: serde_json::Value = client
        .get(format!("https://api.clerk.com/v1/users/{subject}"))
        .bearer_auth(&key)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let primary = user
        .get("primary_email_address_id")
        .and_then(|v| v.as_str());
    let addresses = user.get("email_addresses").and_then(|v| v.as_array())?;
    addresses
        .iter()
        .find(|a| primary.is_some() && a.get("id").and_then(|v| v.as_str()) == primary)
        .or_else(|| addresses.first())
        .and_then(|a| a.get("email_address"))
        .and_then(|v| v.as_str())
        .map(|e| e.to_ascii_lowercase())
}

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
    iss: String,
    sid: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    azp: Option<String>,
    exp: u64,
    #[serde(default)]
    nbf: Option<u64>,
}

/// Only the JOSE fields this service understands.
///
/// Clerk includes a numeric custom header in native session tokens. Starting
/// in jsonwebtoken 10.3, its public `Header` flattens every unknown field into
/// `HashMap<String, String>` and therefore rejects that valid number before it
/// can verify the signature. JOSE says unknown non-critical fields are to be
/// ignored, so parse the narrow contract here and let jsonwebtoken perform the
/// RS256 verification below.
#[derive(Debug, Deserialize)]
struct SessionHeader {
    alg: Algorithm,
    kid: Option<String>,
    #[serde(default)]
    crit: Vec<String>,
}

struct TokenParts<'a> {
    header: SessionHeader,
    encoded_claims: &'a str,
    signature: &'a str,
    signed: &'a str,
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
    issuer: String,
    http: reqwest::Client,
    keys: RwLock<Cache>,
    allowed_emails: Option<Vec<String>>,
    authorized_parties: Vec<String>,
}

struct Cache {
    keys: HashMap<String, DecodingKey>,
    fetched: Option<Instant>,
}

impl Verifier {
    pub fn new(
        jwks_url: String,
        allowed_emails: Option<Vec<String>>,
        authorized_parties: Vec<String>,
    ) -> Result<Arc<Self>, String> {
        let issuer = issuer_for_jwks(&jwks_url)?;
        Ok(Arc::new(Self {
            jwks_url,
            issuer,
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
            authorized_parties: authorized_parties
                .into_iter()
                .map(|party| party.trim().trim_end_matches('/').to_string())
                .filter(|party| !party.is_empty())
                .collect(),
        }))
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
            if cache
                .fetched
                .is_some_and(|t| t.elapsed() < Duration::from_secs(30))
            {
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
            .and_then(|h| {
                h.strip_prefix("Bearer ")
                    .or_else(|| h.strip_prefix("bearer "))
            })
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "no sign-in was presented".to_string())?;

        let parts = parse_token(raw).map_err(|e| format!("not a readable token: {e}"))?;
        if parts.header.alg != Algorithm::RS256 {
            return Err("sign-in did not use RS256".into());
        }
        if !parts.header.crit.is_empty() {
            return Err("sign-in uses unsupported critical JWT headers".into());
        }
        let kid = parts
            .header
            .kid
            .ok_or_else(|| "token names no signing key".to_string())?;
        let key = self.key_for(&kid).await?;

        let valid = crypto::verify(
            parts.signature,
            parts.signed.as_bytes(),
            &key,
            Algorithm::RS256,
        )
        .map_err(|e| format!("sign-in was not valid: {e}"))?;
        if !valid {
            return Err("sign-in was not valid: signature mismatch".into());
        }

        let claims: Claims = decode_json_segment(parts.encoded_claims)
            .map_err(|e| format!("sign-in claims were unreadable: {e}"))?;
        validate_registered_claims(&claims, &self.issuer)?;
        validate_session_claims(&claims, &self.authorized_parties)?;

        let caller = Caller {
            subject: claims.sub,
            email: claims.email.map(|e| e.to_ascii_lowercase()),
        };
        Ok(caller)
    }

    /// Apply the optional provisioning allowlist after the handler has filled
    /// in Clerk's primary email. Default Clerk session tokens usually contain
    /// only `sub`; enforcing this inside `verify` rejected every legitimate
    /// allowlisted caller before the Backend API lookup could run.
    pub fn ensure_allowed(&self, caller: &Caller) -> Result<(), String> {
        ensure_allowed_email(self.allowed_emails.as_deref(), caller)
    }
}

fn parse_token(raw: &str) -> Result<TokenParts<'_>, String> {
    let (encoded_header, rest) = raw
        .split_once('.')
        .ok_or_else(|| "expected three JWT segments".to_string())?;
    let (encoded_claims, signature) = rest
        .split_once('.')
        .ok_or_else(|| "expected three JWT segments".to_string())?;
    if encoded_header.is_empty()
        || encoded_claims.is_empty()
        || signature.is_empty()
        || signature.contains('.')
    {
        return Err("expected exactly three non-empty JWT segments".into());
    }
    let signed_len = encoded_header.len() + 1 + encoded_claims.len();
    let header = decode_json_segment(encoded_header)?;
    Ok(TokenParts {
        header,
        encoded_claims,
        signature,
        signed: &raw[..signed_len],
    })
}

fn decode_json_segment<T: DeserializeOwned>(encoded: &str) -> Result<T, String> {
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|error| format!("invalid base64url: {error}"))?;
    serde_json::from_slice(&decoded).map_err(|error| format!("invalid JSON: {error}"))
}

fn validate_registered_claims(claims: &Claims, issuer: &str) -> Result<(), String> {
    if claims.iss != issuer {
        return Err("sign-in came from the wrong Clerk instance".into());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch".to_string())?
        .as_secs();
    const CLOCK_SKEW: u64 = 60;
    if claims.exp.saturating_add(CLOCK_SKEW) < now {
        return Err("sign-in has expired".into());
    }
    if claims
        .nbf
        .is_some_and(|not_before| not_before > now.saturating_add(CLOCK_SKEW))
    {
        return Err("sign-in is not active yet".into());
    }
    Ok(())
}

fn issuer_for_jwks(jwks_url: &str) -> Result<String, String> {
    const SUFFIX: &str = "/.well-known/jwks.json";
    let url = jwks_url.trim().trim_end_matches('/');
    let issuer = url.strip_suffix(SUFFIX).ok_or_else(|| {
        format!("CLERK_JWKS_URL must end in {SUFFIX}, so the token issuer can be pinned")
    })?;
    if !issuer.starts_with("https://") || issuer.len() == "https://".len() {
        return Err("CLERK_JWKS_URL must be an https URL".into());
    }
    Ok(issuer.trim_end_matches('/').to_string())
}

fn validate_session_claims(claims: &Claims, authorized_parties: &[String]) -> Result<(), String> {
    if claims.sub.trim().is_empty() || claims.sid.trim().is_empty() || claims.iss.trim().is_empty()
    {
        return Err("sign-in was not a Clerk session token".into());
    }
    if let Some(party) = claims.azp.as_deref() {
        let normalized = party.trim_end_matches('/');
        if !authorized_parties
            .iter()
            .any(|allowed| allowed == normalized)
        {
            return Err("sign-in came from an unauthorized application origin".into());
        }
    }
    Ok(())
}

fn ensure_allowed_email(allowed: Option<&[String]>, caller: &Caller) -> Result<(), String> {
    let Some(allowed) = allowed else {
        return Ok(());
    };
    let ok = caller
        .email
        .as_ref()
        .is_some_and(|email| allowed.iter().any(|allowed| allowed == email));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{} is not allowed to create team servers",
            caller.label()
        ))
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::Algorithm;

    use super::{
        Caller, Claims, ensure_allowed_email, issuer_for_jwks, parse_token,
        validate_registered_claims, validate_session_claims,
    };

    fn claims() -> Claims {
        Claims {
            sub: "user_123".into(),
            iss: "https://clerk.example".into(),
            sid: "sess_123".into(),
            email: None,
            azp: None,
            exp: u64::MAX,
            nbf: None,
        }
    }

    #[test]
    fn clerk_numeric_custom_header_is_accepted() {
        let header = URL_SAFE_NO_PAD
            .encode(br#"{"alg":"RS256","kid":"clerk-key","typ":"JWT","v":1788445382}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"user_123"}"#);
        let raw = format!("{header}.{payload}.signature");
        let parsed = parse_token(&raw).expect("Clerk-shaped header should parse");
        assert_eq!(parsed.header.alg, Algorithm::RS256);
        assert_eq!(parsed.header.kid.as_deref(), Some("clerk-key"));
    }

    #[test]
    fn registered_claims_pin_issuer_and_time_window() {
        let mut value = claims();
        assert!(validate_registered_claims(&value, "https://clerk.example").is_ok());
        assert!(validate_registered_claims(&value, "https://other.example").is_err());

        value.exp = 0;
        assert!(validate_registered_claims(&value, "https://clerk.example").is_err());
    }

    #[test]
    fn allowlist_is_checked_after_email_resolution() {
        let allowed = vec!["owner@example.com".into()];
        let unresolved = Caller {
            subject: "user_123".into(),
            email: None,
        };
        assert!(ensure_allowed_email(Some(&allowed), &unresolved).is_err());

        let resolved = Caller {
            subject: "user_123".into(),
            email: Some("owner@example.com".into()),
        };
        assert!(ensure_allowed_email(Some(&allowed), &resolved).is_ok());
    }

    #[test]
    fn no_allowlist_accepts_any_verified_caller() {
        let caller = Caller {
            subject: "user_123".into(),
            email: None,
        };
        assert!(ensure_allowed_email(None, &caller).is_ok());
    }

    #[test]
    fn issuer_is_derived_from_the_exact_clerk_jwks_endpoint() {
        assert_eq!(
            issuer_for_jwks("https://wanted.example/.well-known/jwks.json").unwrap(),
            "https://wanted.example"
        );
        assert!(issuer_for_jwks("https://wanted.example/keys.json").is_err());
        assert!(issuer_for_jwks("http://wanted.example/.well-known/jwks.json").is_err());
    }

    #[test]
    fn native_session_without_authorized_party_is_accepted() {
        assert!(validate_session_claims(&claims(), &[]).is_ok());
    }

    #[test]
    fn browser_origin_must_be_explicitly_authorized() {
        let mut browser = claims();
        browser.azp = Some("https://app.example/".into());
        assert!(validate_session_claims(&browser, &[]).is_err());
        assert!(validate_session_claims(&browser, &["https://app.example".into()]).is_ok());
    }

    #[test]
    fn malformed_sessions_are_refused() {
        let mut missing_session = claims();
        missing_session.sid.clear();
        assert!(validate_session_claims(&missing_session, &[]).is_err());
    }
}
