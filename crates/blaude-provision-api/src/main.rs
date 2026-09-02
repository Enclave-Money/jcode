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

/// One shape for every refusal, so a client never has to guess whether a
/// failure was the caller's or ours.
fn refuse(code: StatusCode, message: String) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "error": message })))
}

async fn health() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "blaude-provision-api" }))
}

async fn create(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> impl IntoResponse {
    let caller = match app
        .verifier
        .verify(headers.get("authorization").and_then(|v| v.to_str().ok()))
        .await
    {
        Ok(c) => c,
        Err(e) => return refuse(StatusCode::UNAUTHORIZED, e),
    };
    let name = body.name.trim();
    if name.is_empty() {
        return refuse(StatusCode::BAD_REQUEST, "a team needs a name".into());
    }
    // The owner's email names them on the new server (attribution, their own
    // room). Clerk's default session token carries no email claim, so when
    // the token has none it is looked up from the verified subject — never
    // taken from the request body, which anyone can type anything into.
    let email = match caller.email.clone() {
        Some(e) => Some(e),
        None => auth::lookup_email(&caller.subject).await,
    };
    tracing::info!(
        caller = email.as_deref().unwrap_or(&caller.subject),
        team = name,
        "creating a team server"
    );
    let status = blaude_provision::start(name, body.region.as_deref(), email.as_deref());
    (StatusCode::ACCEPTED, Json(status))
}

async fn status(
    State(app): State<App>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = app
        .verifier
        .verify(headers.get("authorization").and_then(|v| v.to_str().ok()))
        .await
    {
        return refuse(StatusCode::UNAUTHORIZED, e);
    }
    match blaude_provision::status(&job_id) {
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
    match blaude_provision::delete_team(&body.ws_url).await {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => refuse(StatusCode::BAD_GATEWAY, e),
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
/// Place the mounted clerk.env where the engine looks for it.
///
/// The engine copies `~/.jcode/clerk.env` onto every new team server — that
/// file is how a server sends team invites. On a Mac it exists because the
/// owner set blaude up; in this container it exists only if the deploy
/// mounted it from Secret Manager. Without it teams still build, but their
/// invites silently never send, which is a miserable thing to discover a
/// week later.
fn prepare_clerk_env() {
    let mounted = std::path::Path::new("/secrets/clerk/env");
    if !mounted.exists() {
        tracing::warn!("no mounted clerk.env — new teams will not be able to send invites");
        return;
    }
    let dir = std::path::PathBuf::from(env_or("HOME", "/tmp")).join(".jcode");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("could not make {}: {e}", dir.display());
        return;
    }
    match std::fs::copy(mounted, dir.join("clerk.env")) {
        Ok(_) => tracing::info!("clerk.env in place; new teams can send invites"),
        Err(e) => tracing::warn!("could not place clerk.env: {e}"),
    }
}

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
            let _ = std::fs::write(dir.join("google_compute_engine.pub"), out.stdout);
            tracing::info!("ssh key ready at {}", private.display());
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Clerk's public keys for THIS instance. Pinning the URL is what pins the
    // issuer: no other instance's tokens can ever verify.
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

    prepare_ssh_key();
    prepare_clerk_env();

    let app = App {
        verifier: Verifier::new(jwks, allowed),
    };
    let router = Router::new()
        // NOT /healthz: Google's frontend reserves that path on run.app
        // domains and answers 404 itself, so the check "fails" while the
        // service is fine — which is worse than no check at all.
        .route("/v1/health", get(health))
        .route("/v1/teams", post(create))
        .route("/v1/teams/:job_id", get(status))
        .route("/v1/teams/delete", post(delete))
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
