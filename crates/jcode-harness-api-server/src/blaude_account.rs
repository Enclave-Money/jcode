// The blaude account: who the PERSON is, backed by Clerk — separate from AI
// provider accounts (which are per-environment credentials). The identity
// lives on the user's own machine at ~/.jcode/blaude-account.json; sign-in
// runs Clerk's native email-code flow through the frontend API configured in
// ~/.jcode/clerk.env (CLERK_FRONTEND_API).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Serialize, Deserialize)]
pub struct BlaudeAccountInfo {
    pub email: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub name: String,
}

enum PendingKind {
    SignIn,
    SignUp,
}

struct Pending {
    kind: PendingKind,
    attempt_id: String,
    client_jwt: String,
    email: String,
    created_ms: u64,
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A sign-in attempt already waiting for a code from this address.
///
/// Every `start` asks Clerk to send a NEW code and RETIRES the previous
/// one, so any repeat — an automatic retry after a dropped connection, an
/// impatient second click — silently invalidated the code the person was
/// already typing ("Incorrect code" on a code they read correctly).
/// Repeats reuse the live attempt instead; only an explicit resend mints a
/// new code.
fn live_attempt_for(email: &str) -> Option<String> {
    const REUSE_WINDOW_MS: u64 = 10 * 60 * 1000;
    let now = epoch_ms();
    let reg = registry().lock().ok()?;
    reg.iter()
        .find(|(_, p)| p.email == email && now.saturating_sub(p.created_ms) < REUSE_WINDOW_MS)
        .map(|(id, _)| id.clone())
}

fn registry() -> &'static Mutex<HashMap<String, Pending>> {
    static REG: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The blaude Clerk instance's Frontend API host. PUBLIC by design (every
/// Clerk-backed web app ships it to the browser) — baking it in is what
/// lets a fresh machine sign in with zero config. clerk.env's
/// CLERK_FRONTEND_API (or the env var) overrides it; the SECRET key is a
/// different thing and never leaves servers.
const DEFAULT_FRONTEND_API: &str = "wanted-weasel-6110.clerk.accounts.dev";

fn fapi_base() -> Result<String, String> {
    let configured = std::env::var("CLERK_FRONTEND_API").ok().filter(|v| !v.trim().is_empty());
    let from_file = || {
        let raw = crate::team_access::home()
            .ok()
            .and_then(|h| std::fs::read_to_string(h.join("clerk.env")).ok())?;
        for line in raw.lines() {
            let mut parts = line.splitn(2, '=');
            if parts.next().map(str::trim) == Some("CLERK_FRONTEND_API") {
                let value = parts.next().unwrap_or("").trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
        None
    };
    let value = configured
        .or_else(from_file)
        .unwrap_or_else(|| DEFAULT_FRONTEND_API.to_string());
    Ok(if value.starts_with("http") {
        value.trim_end_matches('/').to_string()
    } else {
        format!("https://{}", value.trim_end_matches('/'))
    })
}

fn random_password() -> Result<String, String> {
    use std::io::Read;
    let mut buf = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| format!("no entropy source: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn account_path() -> Result<std::path::PathBuf, String> {
    crate::team_access::home()
        .map(|h| h.join("blaude-account.json"))
        .map_err(|e| e.to_string())
}

/// The Clerk client session token, persisted at sign-in so the runtime can
/// re-read the user later (that re-read is how a NEW invite reaches a
/// signed-in app — there are no invite emails). Owner-only file, separate
/// from blaude-account.json because that struct travels on the wire.
fn session_path() -> Result<std::path::PathBuf, String> {
    crate::team_access::home()
        .map(|h| h.join("blaude-session.json"))
        .map_err(|e| e.to_string())
}

fn save_client_jwt(jwt: &str) {
    if jwt.is_empty() {
        return;
    }
    if let Ok(path) = session_path() {
        let _ = crate::team_access::write_owner_only(
            &path,
            serde_json::json!({ "client_jwt": jwt }).to_string().as_bytes(),
        );
    }
}

fn load_client_jwt() -> Option<String> {
    let raw = std::fs::read_to_string(session_path().ok()?).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("client_jwt")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn me() -> Option<BlaudeAccountInfo> {
    let raw = std::fs::read_to_string(account_path().ok()?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Only ONE invite refresh may be in flight.
///
/// The persisted credential is Clerk's ROTATING client token: every request
/// returns a replacement and re-presenting a spent one is treated as token
/// theft — Clerk revokes the client's sessions permanently. Two concurrent
/// refreshes (a timer and a window-focus trigger firing together) therefore
/// destroyed the very session they were reading, after which
/// `/v1/client` answered 200 with `sessions: []` forever and no invite
/// could ever be delivered to a signed-in machine. Verified from the token
/// claims ({id, rotating_token}, no sid) and a live 401 on /v1/me.
fn refresh_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Re-read the signed-in user from Clerk and return any team stamped on it
/// since sign-in. Every outcome is logged: this runs on the bridge, whose
/// stdout lands in ~/Library/Logs/Blaude/bridge.log, and a silent `None`
/// made "you have no invite" and "I could not check" indistinguishable —
/// which is exactly how a broken invite path stayed invisible.
pub async fn refresh_team_invite() -> Option<Value> {
    let _guard = refresh_lock().lock().await;
    let base = match fapi_base() {
        Ok(base) => base,
        Err(e) => {
            eprintln!("blaude account: invite check skipped — {e}");
            return None;
        }
    };
    let Some(jwt) = load_client_jwt() else {
        eprintln!("blaude account: invite check skipped — no saved session (sign in again to receive invites)");
        return None;
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .build()
        .ok()?;
    let resp = match client
        .get(format!("{base}/v1/client?_is_native=1"))
        .header("Authorization", &jwt)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            eprintln!("blaude account: invite check failed to reach Clerk — {e}");
            return None;
        }
    };
    let status = resp.status();
    // Persist the replacement BEFORE parsing: dropping it here is what
    // leaves a spent token on disk for the next call to present.
    if let Some(rotated) = resp.headers().get("authorization").and_then(|v| v.to_str().ok()) {
        save_client_jwt(rotated);
    }
    let body: Value = match resp.json().await {
        Ok(body) => body,
        Err(e) => {
            eprintln!("blaude account: invite check got an unreadable reply (HTTP {status}) — {e}");
            return None;
        }
    };
    let sessions = response_obj(&body)
        .get("sessions")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    if sessions.is_empty() {
        eprintln!(
            "blaude account: invite check found NO ACTIVE SESSION (HTTP {status}) — this machine \
             cannot receive team invites until you sign out and sign in again"
        );
        return None;
    }
    let Some(user) = sessions.last().and_then(|s| s.get("user")).cloned() else {
        eprintln!("blaude account: invite check got a session without a user (HTTP {status})");
        return None;
    };
    let invite = user
        .get("public_metadata")
        .and_then(|m| m.get("blaude_team"))
        .filter(|t| t.get("ws_url").and_then(|u| u.as_str()).is_some_and(|u| !u.is_empty()))
        .cloned();
    match &invite {
        Some(t) => eprintln!(
            "blaude account: invite check found team {}",
            t.get("name").and_then(|n| n.as_str()).unwrap_or("(unnamed)")
        ),
        None => eprintln!("blaude account: invite check ok — no team on this account"),
    }
    invite
}

/// The email IS the user identifier. Everything that names a person —
/// hello identity, attribution, member rows — should prefer this.
pub fn identity() -> Option<String> {
    me().map(|a| a.email).filter(|e| !e.is_empty())
}

pub fn sign_out() -> Result<(), String> {
    let path = account_path()?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    if let Ok(session) = session_path() {
        let _ = std::fs::remove_file(session);
    }
    Ok(())
}

/// Clerk's error copy is already human ("Couldn't find your account.",
/// "…is incorrect."); surface it as-is when present.
fn clerk_error(body: &Value) -> Option<String> {
    let err = body.get("errors")?.as_array()?.first()?;
    err.get("long_message")
        .or_else(|| err.get("message"))?
        .as_str()
        .map(str::to_string)
}

async fn fapi_post(
    base: &str,
    path: &str,
    jwt: Option<&str>,
    form: &[(&str, &str)],
) -> Result<(Value, Option<String>), String> {
    fapi_send(reqwest::Method::POST, base, path, jwt, form).await
}

async fn fapi_send(
    method: reqwest::Method,
    base: &str,
    path: &str,
    jwt: Option<&str>,
    form: &[(&str, &str)],
) -> Result<(Value, Option<String>), String> {
    let client = reqwest::Client::new();
    let mut req = client
        .request(method, format!("{base}/v1/client/{path}?_is_native=1"))
        .form(form);
    if let Some(jwt) = jwt {
        req = req.header("Authorization", jwt);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("couldn't reach the sign-in service: {e}"))?;
    let jwt_out = resp
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("sign-in service sent a broken reply: {e}"))?;
    Ok((body, jwt_out))
}

/// The attempt object regardless of envelope shape: `response` on success,
/// `meta.client.sign_in` inside error bodies.
fn response_obj(body: &Value) -> Value {
    body.get("response").cloned().unwrap_or(Value::Null)
}

/// Start email-code sign-in for `email`: existing users get a sign-in
/// attempt, unknown ones a sign-up. Either way Clerk emails a code; the
/// returned pending id redeems it via `finish`.
pub async fn start(email: &str, resend: bool) -> Result<String, String> {
    let base = fapi_base()?;
    let email = email.trim().to_lowercase();
    if !email.contains('@') || email.len() < 5 {
        return Err("that doesn't look like an email address".into());
    }
    if !resend {
        if let Some(existing) = live_attempt_for(&email) {
            return Ok(existing);
        }
    }

    let (body, jwt) = fapi_post(&base, "sign_ins", None, &[("identifier", &email)]).await?;
    let jwt = jwt.unwrap_or_default();
    let not_found = body
        .get("errors")
        .and_then(|e| e.as_array())
        .and_then(|a| a.first())
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        == Some("form_identifier_not_found");

    if not_found {
        // New person: sign them UP with the same email-code ceremony.
        let (body, jwt2) =
            fapi_post(&base, "sign_ups", Some(&jwt), &[("email_address", &email)]).await?;
        if let Some(msg) = clerk_error(&body) {
            return Err(msg);
        }
        let attempt = response_obj(&body);
        let id = attempt
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("sign-up didn't start — try again")?
            .to_string();
        let jwt = jwt2.unwrap_or(jwt);
        let (body, _) = fapi_post(
            &base,
            &format!("sign_ups/{id}/prepare_verification"),
            Some(&jwt),
            &[("strategy", "email_code")],
        )
        .await?;
        if let Some(msg) = clerk_error(&body) {
            return Err(msg);
        }
        registry().lock().unwrap().insert(
            id.clone(),
            Pending {
                kind: PendingKind::SignUp,
                attempt_id: id.clone(),
                client_jwt: jwt,
                email,
                created_ms: epoch_ms(),
            },
        );
        return Ok(id);
    }

    if let Some(msg) = clerk_error(&body) {
        return Err(msg);
    }
    let attempt = response_obj(&body);
    let id = attempt
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("sign-in didn't start — try again")?
        .to_string();
    let email_address_id = attempt
        .get("supported_first_factors")
        .and_then(|v| v.as_array())
        .and_then(|factors| {
            factors.iter().find_map(|f| {
                (f.get("strategy").and_then(|s| s.as_str()) == Some("email_code"))
                    .then(|| f.get("email_address_id")?.as_str().map(str::to_string))
                    .flatten()
            })
        })
        .ok_or("this account can't sign in with an emailed code")?;
    let (body, _) = fapi_post(
        &base,
        &format!("sign_ins/{id}/prepare_first_factor"),
        Some(&jwt),
        &[("strategy", "email_code"), ("email_address_id", &email_address_id)],
    )
    .await?;
    if let Some(msg) = clerk_error(&body) {
        return Err(msg);
    }
    registry().lock().unwrap().insert(
        id.clone(),
        Pending {
            kind: PendingKind::SignIn,
            attempt_id: id.clone(),
            client_jwt: jwt,
            email,
            created_ms: epoch_ms(),
        },
    );
    Ok(id)
}

/// Redeem the emailed code; on success the identity is persisted and
/// returned, along with any team invitation riding on the Clerk user
/// (invitation public_metadata lands on the signed-up user). The pending
/// entry survives a wrong code (typos get retries).
pub async fn finish(
    pending_id: &str,
    code: &str,
) -> Result<(BlaudeAccountInfo, Option<Value>), String> {
    let base = fapi_base()?;
    let (path, jwt, email, is_signup) = {
        let reg = registry().lock().unwrap();
        let p = reg
            .get(pending_id)
            .ok_or("that sign-in expired — start over with your email")?;
        let path = match p.kind {
            PendingKind::SignIn => format!("sign_ins/{}/attempt_first_factor", p.attempt_id),
            PendingKind::SignUp => format!("sign_ups/{}/attempt_verification", p.attempt_id),
        };
        (path, p.client_jwt.clone(), p.email.clone(), matches!(p.kind, PendingKind::SignUp))
    };
    let code = code.trim();
    let (body, jwt_rotated) = fapi_post(
        &base,
        &path,
        Some(&jwt),
        &[("strategy", "email_code"), ("code", code)],
    )
    .await?;
    if let Some(msg) = clerk_error(&body) {
        return Err(msg);
    }
    // Persist the client session: later refreshes of the user (how a new
    // invite reaches this signed-in machine) authenticate with it.
    save_client_jwt(jwt_rotated.as_deref().unwrap_or(&jwt));
    let mut body = body;
    let mut attempt = response_obj(&body);
    let mut status = attempt.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string();
    // The Clerk instance may require a password field; a person signing in
    // by email code neither has nor wants one. Complete the sign-up with a
    // generated throwaway (never shown, never needed — email codes remain
    // the sign-in method).
    if is_signup && status == "missing_requirements" {
        let needs_password = attempt
            .get("missing_fields")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().any(|f| f.as_str() == Some("password")))
            .unwrap_or(false);
        if needs_password {
            let pw = random_password()?;
            let (body2, _) = fapi_send(
                reqwest::Method::PATCH,
                &base,
                &format!("sign_ups/{pending_id}"),
                Some(&jwt),
                &[("password", pw.as_str())],
            )
            .await?;
            if let Some(msg) = clerk_error(&body2) {
                return Err(msg);
            }
            body = body2;
            attempt = response_obj(&body);
            status = attempt.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string();
        }
    }
    if status != "complete" {
        return Err("that code didn't finish the sign-in — try again".into());
    }
    let user_id = if is_signup {
        attempt.get("created_user_id").and_then(|v| v.as_str()).unwrap_or("")
    } else {
        ""
    };
    // The richest user record rides the client's session list.
    let user = body
        .get("client")
        .and_then(|c| c.get("sessions"))
        .and_then(|s| s.as_array())
        .and_then(|s| s.last())
        .and_then(|s| s.get("user"))
        .cloned()
        .unwrap_or(Value::Null);
    let name = [
        user.get("first_name").and_then(|v| v.as_str()).unwrap_or(""),
        user.get("last_name").and_then(|v| v.as_str()).unwrap_or(""),
    ]
    .iter()
    .filter(|s| !s.is_empty())
    .cloned()
    .collect::<Vec<_>>()
    .join(" ");
    let info = BlaudeAccountInfo {
        email,
        user_id: user
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or(user_id)
            .to_string(),
        name,
    };
    let team_invite = user
        .get("public_metadata")
        .and_then(|m| m.get("blaude_team"))
        .filter(|t| t.get("ws_url").and_then(|u| u.as_str()).is_some_and(|u| !u.is_empty()))
        .cloned();
    let path = account_path()?;
    crate::team_access::write_owner_only(
        &path,
        &serde_json::to_vec(&info).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    registry().lock().unwrap().remove(pending_id);
    Ok((info, team_invite))
}
