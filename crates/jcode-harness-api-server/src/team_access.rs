//! Team access as harness verbs.
//!
//! The desktop app used to author `~/.jcode/team-tokens.json` and
//! `join-tickets.json` itself and POST to Clerk with a secret it read off
//! disk — the frontend writing the bridge's own authorization database
//! out-of-band. These are bridge operations now: the app asks over the
//! wire, the bridge mints/writes/revokes and talks to Clerk. Bearer
//! tokens are returned only on the owner-facing invite reply (local
//! 0600 socket / token-gated WS — the same trust boundary as the files).

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::ws;

pub(crate) fn home() -> Result<std::path::PathBuf> {
    // Same resolution as the WS door's stores: JCODE_HOME names the .jcode
    // directory itself (tests point it at a scratch dir; the door and the
    // access store must agree or claims read a different token file than
    // invites write).
    if let Ok(dir) = std::env::var("JCODE_HOME") {
        if !dir.is_empty() {
            return Ok(std::path::PathBuf::from(dir));
        }
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(std::path::PathBuf::from(home).join(".jcode"))
}

pub(crate) fn write_owner_only(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn random_hex(bytes: usize) -> Result<String> {
    use std::io::Read;
    let mut buffer = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .context("open /dev/urandom")?
        .read_exact(&mut buffer)
        .context("read /dev/urandom")?;
    Ok(buffer.iter().map(|b| format!("{b:02x}")).collect())
}

/// Issue (or return the existing) bearer token for a member email.
pub fn issue_token(email: &str) -> Result<String> {
    let path = home()?.join("team-tokens.json");
    let mut tokens: HashMap<String, String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    if let Some(existing) = tokens.get(email) {
        return Ok(existing.clone());
    }
    let token = format!("member-{}", random_hex(24)?);
    tokens.insert(email.to_string(), token.clone());
    write_owner_only(&path, &serde_json::to_vec(&tokens)?)?;
    Ok(token)
}

/// Remove a member's bearer token AND any unredeemed join tickets; the WS
/// door reloads per handshake, so revocation is immediate. Works on full
/// members and pending invitations alike.
pub fn revoke(email: &str) -> Result<bool> {
    let path = home()?.join("team-tokens.json");
    let mut tokens: HashMap<String, String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let removed_token = tokens.remove(email).is_some();
    if removed_token {
        write_owner_only(&path, &serde_json::to_vec(&tokens)?)?;
    }
    let tickets_path = home()?.join("join-tickets.json");
    let mut tickets: HashMap<String, Value> = std::fs::read_to_string(&tickets_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let before = tickets.len();
    tickets.retain(|_, v| v.get("email").and_then(|e| e.as_str()) != Some(email));
    let removed_tickets = tickets.len() != before;
    if removed_tickets {
        write_owner_only(&tickets_path, &serde_json::to_vec(&tickets)?)?;
    }
    Ok(removed_token || removed_tickets)
}

/// Emails invited but not yet joined: an unredeemed ticket and no token.
pub fn pending_invites() -> Vec<String> {
    let members: std::collections::HashSet<String> =
        ws::team_tokens().keys().cloned().collect();
    let Ok(home) = home() else { return Vec::new() };
    let tickets: HashMap<String, Value> = std::fs::read_to_string(home.join("join-tickets.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let mut pending: Vec<String> = tickets
        .values()
        .filter_map(|v| v.get("email").and_then(|e| e.as_str()).map(str::to_string))
        .filter(|email| !members.contains(email))
        .collect();
    pending.sort();
    pending.dedup();
    pending
}

/// Member emails only — never their tokens.
pub fn member_emails() -> Vec<String> {
    let mut emails: Vec<String> = ws::team_tokens().keys().cloned().collect();
    emails.sort();
    emails
}

/// Mint a one-time join ticket redeemable at the bridge's /join page.
fn mint_join_ticket(email: &str) -> Result<String> {
    let code = format!("jt-{}", random_hex(16)?);
    let path = home()?.join("join-tickets.json");
    let mut tickets: HashMap<String, Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    tickets.insert(code.clone(), json!({ "email": email, "created_ms": now_ms }));
    write_owner_only(&path, &serde_json::to_vec(&tickets)?)?;
    Ok(code)
}

fn clerk_secret() -> Option<String> {
    let raw = std::fs::read_to_string(home().ok()?.join("clerk.env")).ok()?;
    for line in raw.lines() {
        let mut parts = line.splitn(2, '=');
        if parts.next()?.trim() == "CLERK_SECRET_KEY" {
            let value = parts.next()?.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

async fn send_clerk_invitation(
    email: &str,
    redirect_url: &str,
    metadata: &Value,
) -> Result<(), String> {
    let Some(secret) = clerk_secret() else {
        return Err("no Clerk key at ~/.jcode/clerk.env — share the join link manually".into());
    };
    let client = reqwest::Client::new();
    let response = create_invitation(&client, &secret, email, redirect_url, metadata).await?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    if status.as_u16() == 422 {
        // Clerk refuses invitations for already-registered addresses; access
        // is still issued — the join link is the path for existing users.
        return Err(
            "that address already has an account, so no email was sent — share the join link instead"
                .into(),
        );
    }
    if status.as_u16() == 400 && body.contains("duplicate_record") {
        // A pending invitation already exists — and its email may carry a
        // STALE redirect (an old host, a burned ticket). Counting that as
        // success silently leaves the teammate holding a dead link (it did,
        // live). A re-invite means "send a working link": revoke the old
        // invitation and create a fresh one with THIS redirect.
        revoke_pending_invitations(&client, &secret, email).await;
        let retry = create_invitation(&client, &secret, email, redirect_url, metadata).await?;
        if retry.status().is_success() {
            return Ok(());
        }
        return Err(format!(
            "Clerk invitation failed after revoking the old one (HTTP {})",
            retry.status()
        ));
    }
    Err(format!("Clerk invitation failed (HTTP {status})"))
}

async fn create_invitation(
    client: &reqwest::Client,
    secret: &str,
    email: &str,
    redirect_url: &str,
    metadata: &Value,
) -> Result<reqwest::Response, String> {
    client
        .post("https://api.clerk.com/v1/invitations")
        .bearer_auth(secret)
        // public_metadata lands on the signed-up user, so the app can join
        // the team the moment sign-in completes — no links, no pasting.
        .json(&json!({
            "email_address": email,
            "redirect_url": redirect_url,
            "public_metadata": metadata,
        }))
        .send()
        .await
        .map_err(|e| format!("Clerk request failed: {e}"))
}

/// Best-effort revoke of every pending invitation for `email` — the prelude
/// to re-inviting with a fresh link.
async fn revoke_pending_invitations(client: &reqwest::Client, secret: &str, email: &str) {
    let Ok(response) = client
        .get("https://api.clerk.com/v1/invitations?status=pending")
        .bearer_auth(secret)
        .send()
        .await
    else {
        return;
    };
    let Ok(items) = response.json::<serde_json::Value>().await else {
        return;
    };
    let list = items
        .as_array()
        .cloned()
        .or_else(|| items.get("data").and_then(|d| d.as_array()).cloned())
        .unwrap_or_default();
    for item in list {
        if item["email_address"].as_str() == Some(email) {
            if let Some(id) = item["id"].as_str() {
                let _ = client
                    .post(format!("https://api.clerk.com/v1/invitations/{id}/revoke"))
                    .bearer_auth(secret)
                    .send()
                    .await;
            }
        }
    }
}

/// True when the WS door terminates TLS — the scheme handed to members must
/// match, or a wss listener rejects a ws:// URL and (worse) a bearer token
/// would travel in cleartext.
fn tls_enabled() -> bool {
    std::env::var("JCODE_API_WS_TLS_CERT").is_ok_and(|v| !v.is_empty())
        && std::env::var("JCODE_API_WS_TLS_KEY").is_ok_and(|v| !v.is_empty())
}

fn ws_scheme() -> &'static str {
    if tls_enabled() { "wss" } else { "ws" }
}

fn http_scheme() -> &'static str {
    if tls_enabled() { "https" } else { "http" }
}

fn ws_endpoint(host: &str) -> String {
    let port = std::env::var("JCODE_API_WS_PORT").unwrap_or_else(|_| "7644".into());
    format!("{}://{host}:{port}/api", ws_scheme())
}

/// The whole invite operation: token + ticket + (optionally) the Clerk
/// email. `host` is the address members should dial (the app passes its
/// LAN address; loopback for same-machine testing). The scheme is derived
/// from whether the door terminates TLS, so a token never rides cleartext.
pub async fn invite(
    email: &str,
    host: &str,
    send_email: bool,
    team_name: Option<&str>,
) -> Result<Value> {
    // No token yet: an invitation is a TICKET, and membership begins when
    // it is redeemed (claim mints the bearer). Until then the person shows
    // as pending, which is what "invited" means.
    let ticket = mint_join_ticket(email)?;
    let port = std::env::var("JCODE_API_WS_PORT").unwrap_or_else(|_| "7644".into());
    let join_url = format!("{}://{host}:{port}/join?ticket={ticket}", http_scheme());
    let metadata = json!({ "blaude_team": {
        "name": team_name.unwrap_or(""),
        "ws_url": ws_endpoint(host),
        "ticket": ticket,
    }});
    let mut emailed = false;
    let mut email_error: Option<String> = None;
    if send_email {
        match send_clerk_invitation(email, &join_url, &metadata).await {
            Ok(()) => emailed = true,
            Err(error) => email_error = Some(error),
        }
    }
    Ok(json!({
        "email": email,
        "ws_url": ws_endpoint(host),
        // The join link is the credential: redeeming it mints the bearer.
        "token": "",
        "join_url": join_url,
        "emailed": emailed,
        "email_error": email_error,
    }))
}
