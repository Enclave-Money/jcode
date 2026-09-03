//! Team access as harness verbs.
//!
//! The desktop app used to author `~/.jcode/team-tokens.json` and
//! `join-tickets.json` itself and POST to Clerk with a secret it read off
//! disk — the frontend writing the bridge's own authorization database
//! out-of-band. These are bridge operations now: the app asks over the
//! wire, and the bridge mints/writes/revokes. Clerk operations are relayed by
//! the provisioning service through a signed capability scoped to this team;
//! the Clerk backend key never lives on a team VM. Bearer tokens are returned
//! only on the owner-facing invite reply (local 0600 socket / token-gated WS —
//! the same trust boundary as the files).

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::ws;

pub(crate) fn home() -> Result<std::path::PathBuf> {
    // Same resolution as the WS door's stores: JCODE_HOME names the .jcode
    // directory itself (tests point it at a scratch dir; the door and the
    // access store must agree or claims read a different token file than
    // invites write).
    if let Ok(dir) = std::env::var("JCODE_HOME")
        && !dir.is_empty()
    {
        return Ok(std::path::PathBuf::from(dir));
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

/// Serialises every read-modify-write of the ticket and token stores. A
/// claim on the WS door, an invite RPC and the reconciler all edit these
/// files from concurrent tasks; lock-free rewrites can resurrect a redeemed
/// one-time ticket or silently drop a freshly minted one.
fn store_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

fn tickets_path() -> Result<std::path::PathBuf> {
    Ok(home()?.join("join-tickets.json"))
}

fn read_tickets() -> HashMap<String, Value> {
    tickets_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Email plus the bearer that admits it — what a redeemed ticket buys.
pub struct Grant {
    pub email: String,
    pub token: String,
}

/// How long an unclaimed ticket stays redeemable.
const TICKET_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn is_expired(record: &Value) -> bool {
    let created = record
        .get("created_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    created == 0 || now_ms().saturating_sub(created) > TICKET_TTL_MS
}

/// What an unredeemed ticket points at, WITHOUT redeeming it.
///
/// The join page needs the team's websocket URL and name to build the
/// `blaude://join` link that hands the app its team. Reading must not burn the
/// ticket: the app claims it moments later, and a browser preview burning it
/// first is exactly the bug that made invitation links stop working (da2e5ff).
///
/// Returns `(ws_url, team_name)`. Expired tickets return `None`, so a stale
/// link offers no button rather than a broken one.
pub fn peek_ticket(code: &str) -> Option<(String, String)> {
    if code.len() < 16 {
        return None;
    }
    let _guard = store_lock().lock().unwrap_or_else(|p| p.into_inner());
    let tickets = read_tickets();
    let record = tickets.get(code)?;
    if is_expired(record) {
        return None;
    }
    let ws_url = record.get("ws_url")?.as_str()?.to_string();
    let name = record
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Some((ws_url, name))
}

/// Redeem a join ticket for its bearer token.
///
/// The ticket is one-time, but MEMBERSHIP IS NOT A ONE-SHOT EVENT: a person
/// installs on a second Mac, reinstalls, wipes defaults, or quits between
/// the burn and the app persisting the token. Burning without replacing
/// stranded every one of those (the stamp kept pointing at a dead ticket
/// and each later sign-in 410'd), so a successful claim immediately mints a
/// REPLACEMENT ticket for the same person and re-stamps their account with
/// it. The credential in their account is therefore always live, and the
/// TTL restarts on every use.
pub fn claim_ticket(code: &str) -> Option<Grant> {
    if code.len() < 16 {
        return None;
    }
    let path = tickets_path().ok()?;
    let renewal = {
        let _guard = store_lock().lock().unwrap_or_else(|p| p.into_inner());
        let mut tickets = read_tickets();
        let entry = tickets.remove(code)?;
        // Persist the removal FIRST (atomically): a ticket that cannot be
        // burned must not be honored, or it stops being one-time.
        write_owner_only(&path, &serde_json::to_vec(&tickets).ok()?).ok()?;
        if is_expired(&entry) {
            return None;
        }
        let email = entry.get("email")?.as_str()?.to_string();
        let ws_url = entry
            .get("ws_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Mint the replacement under the SAME lock, so the account is never
        // left without a live ticket.
        let mut tickets = read_tickets();
        let fresh = format!("jt-{}", random_hex(16).ok()?);
        tickets.insert(
            fresh.clone(),
            json!({ "email": &email, "created_ms": now_ms(), "ws_url": &ws_url, "name": &name }),
        );
        write_owner_only(&path, &serde_json::to_vec(&tickets).ok()?).ok()?;
        (email, ws_url, fresh)
    };
    let (email, ws_url, fresh) = renewal;
    // A valid ticket IS the authorization: the owner minted it for this
    // email. Issue (or return) the member token at claim time — requiring a
    // pre-existing token made a revoke-then-rejoin (or any token-store loss)
    // burn the ticket and then 410, stranding the invitee.
    let token = issue_token(&email).ok()?;
    // Claiming an invitation is the moment someone becomes a member, so it is
    // the moment to build their own room. Without this a member only ever had
    // the shared one and "Mine" silently meant "Shared" for them forever.
    crate::rooms::request_member_provision(&email, &crate::rooms::door_home());
    if !ws_url.is_empty() {
        restamp_with_fresh_ticket(email.clone(), fresh);
    }
    Some(Grant { email, token })
}

/// Best-effort: point the account's stamp at the replacement ticket.
fn restamp_with_fresh_ticket(email: String, ticket: String) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        let _ = directory_call("stamp", &email, Some(&ticket)).await;
    });
}

/// Issue (or return the existing) bearer token for a member email.
pub fn issue_token(email: &str) -> Result<String> {
    let _guard = store_lock().lock().unwrap_or_else(|p| p.into_inner());
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
    let _guard = store_lock().lock().unwrap_or_else(|p| p.into_inner());
    let path = home()?.join("team-tokens.json");
    let mut tokens: HashMap<String, String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let removed_token = tokens.remove(email).is_some();
    if removed_token {
        write_owner_only(&path, &serde_json::to_vec(&tokens)?)?;
    }
    let tickets_file = tickets_path()?;
    let mut tickets = read_tickets();
    let before = tickets.len();
    tickets.retain(|_, v| v.get("email").and_then(|e| e.as_str()) != Some(email));
    let removed_tickets = tickets.len() != before;
    if removed_tickets {
        write_owner_only(&tickets_file, &serde_json::to_vec(&tickets)?)?;
    }
    if removed_token || removed_tickets {
        clear_team_metadata(email.to_string());
    }
    Ok(removed_token || removed_tickets)
}

/// Emails invited but not yet joined: an unredeemed ticket and no token.
pub fn pending_invites() -> Vec<String> {
    let members: std::collections::HashSet<String> = ws::team_tokens().keys().cloned().collect();
    let Ok(home) = home() else { return Vec::new() };
    let tickets: HashMap<String, Value> = std::fs::read_to_string(home.join("join-tickets.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    // An expired ticket is not a pending invitation — it can never be
    // redeemed, so showing it as "Invited" hides that the person needs a
    // fresh one.
    let mut pending: Vec<String> = tickets
        .values()
        .filter(|v| !is_expired(v))
        .filter_map(|v| v.get("email").and_then(|e| e.as_str()).map(str::to_string))
        .filter(|email| !members.contains(email))
        .collect();
    pending.sort();
    pending.dedup();
    pending
}

/// What this team is called, as the server knows it.
///
/// Written at provisioning time to `~/.jcode/team-name`. Empty when the server
/// predates this or was hand-built; clients then keep whatever name they
/// already had, so an older server never blanks out an existing label.
pub fn team_name() -> String {
    let Ok(dir) = jcode_storage::jcode_dir() else {
        return String::new();
    };
    std::fs::read_to_string(dir.join("team-name"))
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

/// Record this team's name so every client is told the same thing.
pub fn set_team_name(name: &str) -> std::io::Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(());
    }
    let dir = jcode_storage::jcode_dir()
        .map_err(|e| std::io::Error::other(format!("no jcode dir: {e}")))?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("team-name"), name)
}

/// The Linux user a member's agent runs as, if they have been provisioned one.
///
/// Written by `deploy/team-server/provision-member.sh` into
/// `~/.jcode/team-users.json` as `{email: linux_user}`. Absent for a server
/// that has not been migrated, and for the owner, who runs as themselves.
pub fn member_linux_user(email: &str) -> Option<String> {
    let dir = jcode_storage::jcode_dir().ok()?;
    let text = std::fs::read_to_string(dir.join("team-users.json")).ok()?;
    let map: serde_json::Value = serde_json::from_str(&text).ok()?;
    let user = map.get(email)?.as_str()?.trim().to_string();
    // Refuse anything that is not a plain username: this value becomes a path.
    if user.is_empty()
        || !user
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return None;
    }
    Some(user)
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
    let _guard = store_lock().lock().unwrap_or_else(|p| p.into_inner());
    let code = format!("jt-{}", random_hex(16)?);
    let path = tickets_path()?;
    let mut tickets = read_tickets();
    // A re-invite supersedes this person's older tickets: leaving them
    // redeemable means a stale link still works and the pending list keeps
    // showing dead invitations.
    tickets.retain(|_, v| v.get("email").and_then(|e| e.as_str()) != Some(email));
    tickets.insert(
        code.clone(),
        json!({ "email": email, "created_ms": now_ms(), "ws_url": ws_url, "name": team_name }),
    );
    write_owner_only(&path, &serde_json::to_vec(&tickets)?)?;
    Ok(code)
}

const DEFAULT_DIRECTORY_API: &str = "https://blaude-provision-api-860657606686.asia-south1.run.app";

fn directory_api() -> String {
    std::env::var("BLAUDE_PROVISION_API")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_DIRECTORY_API.to_string())
}

fn relay_token() -> Result<String, String> {
    let token = std::fs::read_to_string(
        home()
            .map_err(|error| error.to_string())?
            .join("team-relay-token"),
    )
    .map_err(|_| {
        "this team has no directory relay capability; ask the owner to recreate it".to_string()
    })?;
    let token = token.trim().to_string();
    if token.is_empty() {
        Err("this team's directory relay capability is empty".to_string())
    } else {
        Ok(token)
    }
}

async fn directory_call(action: &str, email: &str, ticket: Option<&str>) -> Result<Value, String> {
    let mut body = json!({ "email": email });
    if let Some(ticket) = ticket {
        body["ticket"] = json!(ticket);
    }
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|error| format!("could not build the directory client: {error}"))?
        .post(format!("{}/v1/team-directory/{action}", directory_api()))
        .bearer_auth(relay_token()?)
        .json(&body)
        .send()
        .await
        .map_err(|error| format!("the team directory service could not be reached: {error}"))?;
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .map_err(|error| format!("the team directory service sent an unreadable reply: {error}"))?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the team directory request was refused")
            .to_string())
    }
}

/// Deliver an invite. NEW address → a Clerk INVITATION: the person gets an
/// email, its link creates their account for exactly that address, the
/// redirect lands on the install/sign-in page, and the team metadata rides
/// onto the account — signing in in the app finishes the join. EXISTING
/// account → Clerk refuses invitations, so the team is stamped straight
/// onto the user and their signed-in app picks it up on its account watch.
/// Returns Ok(true) when an email went out, Ok(false) for a silent stamp.
async fn deliver_invite(email: &str, ticket: &str) -> Result<bool, String> {
    let response = directory_call("invite", email, Some(ticket)).await?;
    response
        .get("emailed")
        .and_then(Value::as_bool)
        .ok_or_else(|| "the team directory reply omitted its delivery result".to_string())
}

/// Closes the invited-but-signed-up-manually hole: someone who never opens
/// the invite email and just creates the account in the app never redeems
/// the invitation, so the metadata never lands. Sweep unredeemed tickets;
/// wherever an account now exists for an invited email without this ticket
/// stamped, stamp it (and retire the now-pointless invitation email).
pub async fn reconcile_invites() {
    let Ok(home) = home() else { return };
    let tickets: HashMap<String, Value> = std::fs::read_to_string(home.join("join-tickets.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    for (code, record) in tickets {
        // Stamping an expired ticket guarantees the invitee a 410 — worse
        // than not stamping, because it looks like a delivered invite.
        if is_expired(&record) {
            continue;
        }
        let Some(email) = record.get("email").and_then(|v| v.as_str()) else {
            continue;
        };
        // The relay returns false while no Clerk account exists; the pending
        // invitation remains the path in that case. It also avoids sending the
        // backend key to this VM merely to perform the lookup.
        let _ = directory_call("stamp", email, Some(&code)).await;
    }
}

/// Best-effort removal of the team stamp when a member is revoked, so
/// their next sign-in doesn't auto-join with a dead ticket.
fn clear_team_metadata(email: String) {
    // revoke() is sync; only detach the Clerk call when a runtime exists
    // (it always does on the ws path — this guards future callers).
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        let _ = directory_call("clear", &email, None).await;
    });
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
    // emailed=true → an invitation email went out (new address);
    // emailed=false with no error → existing account was stamped directly,
    // their signed-in app attaches the team on its account watch.
    let mut emailed = false;
    let mut email_error: Option<String> = None;
    if send_email {
        match deliver_invite(email, &ticket).await {
            Ok(sent) => emailed = sent,
            Err(error) => email_error = Some(error),
        }
    }
    Ok(json!({
        "email": email,
        "ws_url": ws_url,
        // The join link is the fallback credential: redeeming it mints the
        // bearer without an account.
        "token": "",
        "join_url": join_url,
        "emailed": emailed,
        "email_error": email_error,
    }))
}

#[cfg(test)]
mod ticket_tests {
    use super::*;

    /// Reading a ticket to build the invitation link must NEVER spend it.
    ///
    /// A browser preview burning the ticket is exactly the bug that made
    /// invitation links stop working (da2e5ff): the app claims the ticket
    /// moments after the page is opened, and a page that spent it left the
    /// invitee holding a dead link.
    #[test]
    fn peeking_a_ticket_does_not_burn_it() {
        let _lock = crate::jcode_home_test_lock();
        let temp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("JCODE_HOME", temp.path()) };

        let ticket = mint_join_ticket("who@example.com", "wss://team.example/api", "gm").unwrap();

        let first = peek_ticket(&ticket);
        assert_eq!(
            first,
            Some(("wss://team.example/api".to_string(), "gm".to_string())),
            "a fresh ticket must read back the team it points at"
        );
        assert_eq!(
            peek_ticket(&ticket),
            first,
            "reading must be repeatable — a page visit must not spend the ticket"
        );

        // The real claim still works after the page looked at it.
        assert!(
            claim_ticket(&ticket).is_some(),
            "the app must still claim a ticket a browser has read"
        );
        // And now it IS spent, so the read no longer resolves the old code.
        assert!(
            peek_ticket(&ticket).is_none(),
            "a claimed ticket must stop resolving"
        );
    }

    /// An unknown or malformed code must not resolve to a team.
    #[test]
    fn an_unknown_ticket_reads_as_nothing() {
        let _lock = crate::jcode_home_test_lock();
        let temp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("JCODE_HOME", temp.path()) };

        assert!(peek_ticket("jt-doesnotexist0000").is_none());
        assert!(
            peek_ticket("short").is_none(),
            "a too-short code is rejected outright"
        );
    }

    /// Team servers talk only to the scoped relay. This source guard prevents
    /// a future "quick fix" from restoring the Clerk backend key and direct
    /// instance-wide API calls on every VM.
    #[test]
    fn a_team_server_has_no_direct_clerk_backend_access() {
        let source = include_str!("team_access.rs");
        let body = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(!body.contains("api.clerk.com"));
        assert!(!body.contains("CLERK_SECRET_KEY"));
        assert!(body.contains("team-relay-token"));
    }
}
