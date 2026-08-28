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
    if removed_token || removed_tickets {
        clear_team_metadata(email.to_string());
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

/// Mint a one-time join ticket redeemable at /join/claim. The record also
/// carries the team's ws_url + name so the invite RECONCILER can rebuild
/// the account stamp for people who sign up without touching their email.
fn mint_join_ticket(email: &str, ws_url: &str, team_name: &str) -> Result<String> {
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
    tickets.insert(
        code.clone(),
        json!({ "email": email, "created_ms": now_ms, "ws_url": ws_url, "name": team_name }),
    );
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

/// The signed-up user's Clerk id for an email, if the account exists.
async fn find_user(client: &reqwest::Client, secret: &str, email: &str) -> Result<Option<Value>, String> {
    let response = client
        .get("https://api.clerk.com/v1/users")
        .query(&[("email_address", email)])
        .bearer_auth(secret)
        .send()
        .await
        .map_err(|e| format!("Clerk request failed: {e}"))?;
    let users: Value = response
        .json()
        .await
        .map_err(|e| format!("Clerk user lookup unreadable: {e}"))?;
    let list = users
        .as_array()
        .cloned()
        .or_else(|| users.get("data").and_then(|d| d.as_array()).cloned())
        .unwrap_or_default();
    Ok(list.first().cloned())
}

/// Merge the blaude_team stamp into a user's public_metadata.
async fn stamp_user(
    client: &reqwest::Client,
    secret: &str,
    user_id: &str,
    metadata: &Value,
) -> Result<(), String> {
    let response = client
        .patch(format!("https://api.clerk.com/v1/users/{user_id}/metadata"))
        .bearer_auth(secret)
        .json(&json!({ "public_metadata": metadata }))
        .send()
        .await
        .map_err(|e| format!("Clerk request failed: {e}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("Clerk refused the stamp (HTTP {})", response.status()))
    }
}

/// Deliver an invite. NEW address → a Clerk INVITATION: the person gets an
/// email, its link creates their account for exactly that address, the
/// redirect lands on the install/sign-in page, and the team metadata rides
/// onto the account — signing in in the app finishes the join. EXISTING
/// account → Clerk refuses invitations, so the team is stamped straight
/// onto the user and their signed-in app picks it up on its account watch.
/// Returns Ok(true) when an email went out, Ok(false) for a silent stamp.
async fn deliver_invite(email: &str, join_url: &str, metadata: &Value) -> Result<bool, String> {
    let Some(secret) = clerk_secret() else {
        return Err("no Clerk key at ~/.jcode/clerk.env".into());
    };
    let client = reqwest::Client::new();
    // Retire older invitations first: their emails carry stale tickets.
    revoke_pending_invitations(&client, &secret, email).await;
    if let Some(user) = find_user(&client, &secret, email).await? {
        if let Some(id) = user["id"].as_str() {
            stamp_user(&client, &secret, id, metadata).await?;
            return Ok(false);
        }
    }
    let response = client
        .post("https://api.clerk.com/v1/invitations")
        .bearer_auth(&secret)
        .json(&json!({
            "email_address": email,
            "redirect_url": join_url,
            "public_metadata": metadata,
        }))
        .send()
        .await
        .map_err(|e| format!("Clerk request failed: {e}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(true);
    }
    if status.as_u16() == 422 {
        // Raced a signup between lookup and invitation — stamp instead.
        if let Some(user) = find_user(&client, &secret, email).await? {
            if let Some(id) = user["id"].as_str() {
                stamp_user(&client, &secret, id, metadata).await?;
                return Ok(false);
            }
        }
    }
    Err(format!("Clerk invitation failed (HTTP {status})"))
}

/// Closes the invited-but-signed-up-manually hole: someone who never opens
/// the invite email and just creates the account in the app never redeems
/// the invitation, so the metadata never lands. Sweep unredeemed tickets;
/// wherever an account now exists for an invited email without this ticket
/// stamped, stamp it (and retire the now-pointless invitation email).
pub async fn reconcile_invites() {
    let Some(secret) = clerk_secret() else { return };
    let Ok(home) = home() else { return };
    let tickets: HashMap<String, Value> = std::fs::read_to_string(home.join("join-tickets.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let client = reqwest::Client::new();
    for (code, record) in tickets {
        let (Some(email), Some(ws_url)) = (
            record.get("email").and_then(|v| v.as_str()),
            record.get("ws_url").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let Ok(Some(user)) = find_user(&client, &secret, email).await else {
            continue; // no account yet — the invitation email still covers them
        };
        let stamped = user
            .pointer("/public_metadata/blaude_team/ticket")
            .and_then(|v| v.as_str());
        if stamped == Some(code.as_str()) {
            continue;
        }
        let name = record.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let metadata =
            json!({ "blaude_team": { "name": name, "ws_url": ws_url, "ticket": code } });
        if let Some(id) = user["id"].as_str() {
            if stamp_user(&client, &secret, id, &metadata).await.is_ok() {
                revoke_pending_invitations(&client, &secret, email).await;
            }
        }
    }
}

/// Best-effort removal of the team stamp when a member is revoked, so
/// their next sign-in doesn't auto-join with a dead ticket.
fn clear_team_metadata(email: String) {
    let Some(secret) = clerk_secret() else { return };
    // revoke() is sync; only detach the Clerk call when a runtime exists
    // (it always does on the ws path — this guards future callers).
    let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
    handle.spawn(async move {
        let client = reqwest::Client::new();
        let Ok(response) = client
            .get("https://api.clerk.com/v1/users")
            .query(&[("email_address", email.as_str())])
            .bearer_auth(&secret)
            .send()
            .await
        else {
            return;
        };
        let Ok(users) = response.json::<Value>().await else { return };
        let list = users
            .as_array()
            .cloned()
            .or_else(|| users.get("data").and_then(|d| d.as_array()).cloned())
            .unwrap_or_default();
        if let Some(id) = list.first().and_then(|u| u["id"].as_str()) {
            let _ = client
                .patch(format!("https://api.clerk.com/v1/users/{id}/metadata"))
                .bearer_auth(&secret)
                // null deletes the key under Clerk's merge semantics.
                .json(&json!({ "public_metadata": { "blaude_team": null } }))
                .send()
                .await;
        }
    });
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
    let ws_url = ws_endpoint(host);
    let name = team_name.unwrap_or("");
    let ticket = mint_join_ticket(email, &ws_url, name)?;
    let port = std::env::var("JCODE_API_WS_PORT").unwrap_or_else(|_| "7644".into());
    let join_url = format!("{}://{host}:{port}/join?ticket={ticket}", http_scheme());
    let metadata = json!({ "blaude_team": {
        "name": name,
        "ws_url": ws_url,
        "ticket": ticket,
    }});
    // emailed=true → an invitation email went out (new address);
    // emailed=false with no error → existing account was stamped directly,
    // their signed-in app attaches the team on its account watch.
    let mut emailed = false;
    let mut email_error: Option<String> = None;
    if send_email {
        match deliver_invite(email, &join_url, &metadata).await {
            Ok(sent) => emailed = sent,
            Err(error) => email_error = Some(error),
        }
    }
    Ok(json!({
        "email": email,
        "ws_url": ws_endpoint(host),
        // The join link is the fallback credential: redeeming it mints the
        // bearer without an account.
        "token": "",
        "join_url": join_url,
        "emailed": emailed,
        "email_error": email_error,
    }))
}
