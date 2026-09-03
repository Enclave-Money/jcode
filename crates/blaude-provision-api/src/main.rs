//! The service that builds team servers.
//!
//! It exists so that no user's machine needs a cloud credential. Provisioning
//! used to shell out to `gcloud` on the owner's Mac, which meant "Create a
//! team" worked for exactly one person — everyone else would have needed the
//! gcloud CLI and Compute Admin on a project that is not theirs. And it broke
//! for that one person about daily, because a human `gcloud auth login`
//! expires and nothing renews it.
//!
//! Here the credential is a service account attached to the deployment. On
//! Cloud Run that means no key file at all: the metadata server mints tokens,
//! and they cannot lapse the way a person's login does.
//!
//! Callers present a Clerk session token, so every VM created has a person's
//! name attached to it.

mod auth;
mod directory;

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use auth::Verifier;

#[derive(Clone)]
struct App {
    verifier: Arc<Verifier>,
    directory: Arc<directory::Directory>,
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    name: String,
    #[serde(default)]
    region: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeleteBody {
    ws_url: String,
}

#[derive(Debug, Deserialize)]
struct DirectoryBody {
    email: String,
    #[serde(default)]
    ticket: Option<String>,
}

/// One shape for every refusal, so a client never has to guess whether a
/// failure was the caller's or ours.
fn refuse(code: StatusCode, message: String) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "error": message })))
}

fn validate_team_name(raw: &str) -> Result<&str, &'static str> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("a team needs a name");
    }
    if name.chars().count() > 80 {
        return Err("a team name cannot be longer than 80 characters");
    }
    if name.chars().any(char::is_control) {
        return Err("a team name cannot contain control characters");
    }
    Ok(name)
}

async fn health() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "blaude-provision-api" }))
}

async fn create(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> impl IntoResponse {
    let mut caller = match app
        .verifier
        .verify(headers.get("authorization").and_then(|v| v.to_str().ok()))
        .await
    {
        Ok(c) => c,
        Err(e) => return refuse(StatusCode::UNAUTHORIZED, e),
    };
    let name = match validate_team_name(&body.name) {
        Ok(name) => name,
        Err(message) => return refuse(StatusCode::BAD_REQUEST, message.into()),
    };
    // The owner's email names them on the new server (attribution, their own
    // room). Clerk's default session token carries no email claim, so when
    // the token has none it is looked up from the verified subject — never
    // taken from the request body, which anyone can type anything into.
    caller.email = match caller.email.clone() {
        Some(e) => Some(e),
        None => auth::lookup_email(&caller.subject).await,
    };
    if let Err(error) = app.verifier.ensure_allowed(&caller) {
        return refuse(StatusCode::FORBIDDEN, error);
    }
    let Some(email) = caller.email.as_deref() else {
        return refuse(
            StatusCode::BAD_GATEWAY,
            "Your verified Clerk account did not resolve to an email address. Try signing in again."
                .into(),
        );
    };
    tracing::info!(caller = email, team = name, "creating a team server");
    let status =
        blaude_provision::start(name, body.region.as_deref(), &caller.subject, Some(email));
    (StatusCode::ACCEPTED, Json(status))
}

async fn status(
    State(app): State<App>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> impl IntoResponse {
    let caller = match app
        .verifier
        .verify(headers.get("authorization").and_then(|v| v.to_str().ok()))
        .await
    {
        Ok(caller) => caller,
        Err(error) => return refuse(StatusCode::UNAUTHORIZED, error),
    };
    match blaude_provision::status(&job_id, &caller.subject) {
        Some(v) => (StatusCode::OK, Json(v)),
        None => refuse(StatusCode::NOT_FOUND, format!("no job {job_id}")),
    }
}

async fn delete(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<DeleteBody>,
) -> impl IntoResponse {
    let caller = match app
        .verifier
        .verify(headers.get("authorization").and_then(|v| v.to_str().ok()))
        .await
    {
        Ok(c) => c,
        Err(e) => return refuse(StatusCode::UNAUTHORIZED, e),
    };
    tracing::info!(caller = caller.label(), server = %body.ws_url, "deleting a team server");
    match blaude_provision::delete_team(&body.ws_url, &caller.subject).await {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => refuse(StatusCode::BAD_GATEWAY, e),
    }
}

fn relay_claims(headers: &HeaderMap) -> Result<blaude_provision::RelayClaims, String> {
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "no team capability was presented".to_string())?;
    blaude_provision::verify_relay_token(token)
}

async fn directory_invite(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<DirectoryBody>,
) -> impl IntoResponse {
    let claims = match relay_claims(&headers) {
        Ok(claims) => claims,
        Err(error) => return refuse(StatusCode::UNAUTHORIZED, error),
    };
    let Some(ticket) = body.ticket.as_deref() else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "an invite ticket is required".into(),
        );
    };
    match app.directory.invite(&claims, &body.email, ticket).await {
        Ok(emailed) => (StatusCode::OK, Json(json!({ "emailed": emailed }))),
        Err(error) => refuse(StatusCode::BAD_GATEWAY, error),
    }
}

async fn directory_stamp(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<DirectoryBody>,
) -> impl IntoResponse {
    let claims = match relay_claims(&headers) {
        Ok(claims) => claims,
        Err(error) => return refuse(StatusCode::UNAUTHORIZED, error),
    };
    let Some(ticket) = body.ticket.as_deref() else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "a replacement ticket is required".into(),
        );
    };
    match app.directory.stamp(&claims, &body.email, ticket).await {
        Ok(stamped) => (StatusCode::OK, Json(json!({ "stamped": stamped }))),
        Err(error) => refuse(StatusCode::BAD_GATEWAY, error),
    }
}

async fn directory_clear(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<DirectoryBody>,
) -> impl IntoResponse {
    let claims = match relay_claims(&headers) {
        Ok(claims) => claims,
        Err(error) => return refuse(StatusCode::UNAUTHORIZED, error),
    };
    match app.directory.clear(&claims, &body.email).await {
        Ok(cleared) => (StatusCode::OK, Json(json!({ "cleared": cleared }))),
        Err(error) => refuse(StatusCode::BAD_GATEWAY, error),
    }
}

/// Put the mounted SSH key where `gcloud compute ssh` looks for it.
///
/// Provisioning copies files onto the new VM and runs scripts there, so it
/// needs a key. Left to itself gcloud generates one on first use and registers
/// it with the project — but this filesystem is ephemeral, so every cold start
/// would mint another key and push it, growing the project's key list without
/// bound and paying a registration round trip on each first request. The
/// deploy script creates ONE key in Secret Manager and mounts it; this copies
/// it into place with the permissions ssh insists on.
///
/// Best effort: if the mount is absent (a local run, say) gcloud falls back to
/// generating one, which is fine for a single process that is not restarting.
fn prepare_ssh_key() {
    use std::os::unix::fs::PermissionsExt;
    let mounted = std::path::Path::new("/secrets/ssh/key");
    if !mounted.exists() {
        tracing::info!("no mounted ssh key; gcloud will generate one on first use");
        return;
    }
    let home = std::path::PathBuf::from(env_or("HOME", "/tmp"));
    let dir = home.join(".ssh");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("could not make {}: {e}", dir.display());
        return;
    }
    let private = dir.join("google_compute_engine");
    if let Err(e) = std::fs::copy(mounted, &private) {
        tracing::warn!("could not place the ssh key: {e}");
        return;
    }
    // 0600, or ssh refuses the key outright and every scp fails with a
    // permissions complaint that reads nothing like "the key is world
    // readable".
    let _ = std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600));

    // gcloud wants the public half beside it. Derive rather than mount, so
    // there is one secret to rotate instead of two that can disagree.
    match std::process::Command::new("ssh-keygen")
        .arg("-y")
        .arg("-f")
        .arg(&private)
        .output()
    {
        Ok(out) if out.status.success() => {
            let pubkey = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let _ = std::fs::write(dir.join("google_compute_engine.pub"), &pubkey);
            // The engine reads these to authorize this exact key, as this
            // exact user, in the new VM's metadata at create time.
            let login = env_or("BLAUDE_SSH_LOGIN", "blaude");
            unsafe {
                std::env::set_var("BLAUDE_SSH_PUBKEY", &pubkey);
                // gcloud in metadata mode takes the remote username from the
                // process login name; as root it cannot pick a usable one.
                // Pin it so scp/ssh connect as `login`, matching the metadata.
                std::env::set_var("USER", &login);
                std::env::set_var("LOGNAME", &login);
            }
            tracing::info!("ssh key ready at {} (login {login})", private.display());
        }
        Ok(out) => tracing::warn!(
            "could not derive the public key: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => tracing::warn!("ssh-keygen is missing: {e}"),
    }
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn load_relay_signing_key() -> Result<Vec<u8>, String> {
    if let Ok(value) = std::env::var("BLAUDE_RELAY_SIGNING_KEY")
        && !value.trim().is_empty()
    {
        return Ok(value.into_bytes());
    }
    std::fs::read("/secrets/relay/key")
        .map_err(|error| format!("the mounted team relay signing key is missing: {error}"))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Clerk's public keys for THIS instance. The verifier derives and checks
    // the exact issuer from this URL as a separate claim check.
    let jwks = std::env::var("CLERK_JWKS_URL").unwrap_or_default();
    if jwks.trim().is_empty() {
        eprintln!(
            "CLERK_JWKS_URL is not set. Without it every request would be refused, so this \
             would start and then serve nothing. Set it to \
             https://<your-clerk-frontend-api>/.well-known/jwks.json"
        );
        std::process::exit(2);
    }
    // Unset means "anyone signed in to our Clerk instance", which is the right
    // default for a private instance. Set it to lock provisioning down further.
    let allowed = std::env::var("BLAUDE_PROVISION_ALLOWED_EMAILS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.split(',').map(str::to_string).collect::<Vec<_>>());
    if let Some(list) = &allowed {
        tracing::info!("provisioning limited to {} account(s)", list.len());
    }
    // Native Clerk session tokens have no `azp`. If a browser session token
    // is ever accepted here, its origin must be deliberately allowlisted.
    let authorized_parties = std::env::var("BLAUDE_CLERK_AUTHORIZED_PARTIES")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.split(',').map(str::to_string).collect::<Vec<_>>())
        .unwrap_or_default();

    let relay_key = load_relay_signing_key().unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(2);
    });
    blaude_provision::configure_relay_signing_key(&relay_key).unwrap_or_else(|error| {
        eprintln!("could not configure the team directory relay: {error}");
        std::process::exit(2);
    });
    let directory = directory::Directory::load().unwrap_or_else(|error| {
        eprintln!("could not configure the Clerk directory relay: {error}");
        std::process::exit(2);
    });

    prepare_ssh_key();

    let verifier = Verifier::new(jwks, allowed, authorized_parties).unwrap_or_else(|error| {
        eprintln!("could not configure Clerk token verification: {error}");
        std::process::exit(2);
    });
    let app = App {
        verifier,
        directory: Arc::new(directory),
    };
    let router = Router::new()
        // NOT /healthz: Google's frontend reserves that path on run.app
        // domains and answers 404 itself, so the check "fails" while the
        // service is fine — which is worse than no check at all.
        .route("/v1/health", get(health))
        .route("/v1/teams", post(create))
        .route("/v1/teams/:job_id", get(status))
        .route("/v1/teams/delete", post(delete))
        .route("/v1/team-directory/invite", post(directory_invite))
        .route("/v1/team-directory/stamp", post(directory_stamp))
        .route("/v1/team-directory/clear", post(directory_clear))
        .with_state(app);

    // Cloud Run hands the port in; 8080 is its default and a sane local one.
    let port = env_or("PORT", "8080");
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("could not listen on {addr}: {e}"));
    tracing::info!("blaude-provision-api listening on {addr}");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server");
}

#[cfg(test)]
mod request_tests {
    use super::validate_team_name;

    #[test]
    fn team_names_are_trimmed_and_bounded() {
        assert_eq!(validate_team_name("  Rabani's team  "), Ok("Rabani's team"));
        assert!(validate_team_name("   ").is_err());
        assert!(validate_team_name(&"x".repeat(81)).is_err());
        assert!(validate_team_name("line one\nline two").is_err());
    }
}
