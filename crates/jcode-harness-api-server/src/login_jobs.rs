//! Bridge-global provider login jobs (Claude and Codex/OpenAI).
//!
//! Sign-in is a loopback-relay OAuth flow that works whether the daemon is on
//! the user's Mac or on a remote team server, WITHOUT the daemon binding a
//! callback listener:
//!   1. The app opens a tiny loopback listener on ITS OWN machine and calls
//!      `start_*_login { redirect_uri: "http://localhost:<port>/callback" }`.
//!   2. The bridge (here) generates PKCE + the provider authorize URL pointing
//!      at that redirect, keeps the verifier IN MEMORY (never on the wire), and
//!      returns the URL.
//!   3. The user approves; the browser redirects to the app's loopback listener
//!      (reachable — it's the user's machine, not the server), which relays the
//!      code back via `complete_login { job_id, code }`.
//!   4. The bridge exchanges the code for tokens IN PROCESS and saves them into
//!      the daemon's own account store (so a team server's own agents use them).
//! No localhost dependency on the server, no CLI subprocess, no code-paste.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use jcode_base::auth::{claude, codex, oauth};
use serde_json::{Value, json};
use tokio::sync::watch;

/// A pending sign-in never completed by the user is dropped after this long.
const LOGIN_TIMEOUT_SECS: u64 = 600;

struct Job {
    record: Value,
    // In-memory only — the PKCE verifier and CSRF state MUST NOT cross the wire.
    verifier: String,
    state_param: String,
    redirect_uri: String,
    provider: String,
    state_tx: watch::Sender<String>,
    cancel: Arc<tokio::sync::Notify>,
}

fn jobs() -> &'static Mutex<HashMap<String, Job>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_job_id() -> String {
    // A process-global counter guarantees uniqueness even when two logins start
    // in the same nanosecond window (quick provider switch, a retry, or a test
    // suite sharing this global table).
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("lg-{:x}-{:x}", now_secs(), seq)
}

fn set_state(job_id: &str, state: &str) {
    let mut table = jobs().lock().unwrap();
    if let Some(job) = table.get_mut(job_id) {
        job.record["state"] = json!(state);
        let _ = job.state_tx.send(state.to_string());
    }
}

fn finish(job_id: &str, state: &str, error: Option<String>) {
    let mut table = jobs().lock().unwrap();
    if let Some(job) = table.get_mut(job_id) {
        job.record["state"] = json!(state);
        job.record["finished_at"] = json!(now_secs());
        if let Some(error) = error {
            job.record["error"] = json!(error);
        }
        let _ = job.state_tx.send(state.to_string());
    }
}

fn normalize(provider: &str) -> &'static str {
    if provider == "codex" || provider == "openai" { "codex" } else { "claude" }
}

/// Start a loopback-relay login. `redirect_uri` is the app's own loopback
/// listener (e.g. `http://localhost:49xxx/callback`). Returns the job id; the
/// `url` field is on the record immediately (the authorize link).
pub fn start(provider: &str, redirect_uri: &str) -> String {
    let provider = normalize(provider).to_string();
    let (verifier, challenge) = oauth::generate_pkce_public();
    // Claude and OpenAI differ in what the authorize `state` must carry:
    //   - Claude's CSRF convention makes the `state` the PKCE verifier itself;
    //     `exchange_claude_code` validates `callback_state == verifier`. A
    //     separate random state fails every exchange with "state mismatch".
    //   - OpenAI uses an independent random state, validated by
    //     `exchange_openai_callback_input(_, _, expected_state, _)`.
    let state = oauth::generate_state_public();
    let auth_url = if provider == "codex" {
        oauth::openai_auth_url_with_prompt(redirect_uri, &challenge, &state, Some("login"))
    } else {
        oauth::claude_auth_url(redirect_uri, &challenge, &verifier)
    };
    // Record the state the exchange will actually check against, so the stored
    // value matches the authorize URL for each provider.
    let state = if provider == "codex" { state } else { verifier.clone() };

    let job_id = new_job_id();
    let record = json!({
        "job_id": job_id,
        "provider": provider,
        "state": "waiting_for_code",
        "url": auth_url,
        "started_at": now_secs(),
    });
    let (state_tx, _) = watch::channel("waiting_for_code".to_string());
    let cancel = Arc::new(tokio::sync::Notify::new());
    jobs().lock().unwrap().insert(
        job_id.clone(),
        Job {
            record,
            verifier,
            state_param: state,
            redirect_uri: redirect_uri.to_string(),
            provider,
            state_tx,
            cancel: cancel.clone(),
        },
    );

    // Drop a never-completed pending sign-in so secrets don't linger forever.
    let expire_id = job_id.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel.notified() => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS)) => {
                let mut table = jobs().lock().unwrap();
                let still_waiting = table
                    .get(&expire_id)
                    .is_some_and(|job| job.record["state"] == "waiting_for_code");
                if still_waiting {
                    table.remove(&expire_id);
                }
            }
        }
    });

    job_id
}

/// Complete a waiting login: exchange the code/callback the app relayed for
/// tokens, in process, and save them to the daemon's account store.
pub async fn complete(job_id: &str, input: &str) {
    let (verifier, state_param, redirect_uri, provider) = {
        let table = jobs().lock().unwrap();
        match table.get(job_id) {
            Some(job) if job.record["state"] == "waiting_for_code" => (
                job.verifier.clone(),
                job.state_param.clone(),
                job.redirect_uri.clone(),
                job.provider.clone(),
            ),
            _ => return,
        }
    };
    let input = input.trim().to_string();
    if input.is_empty() {
        finish(job_id, "failed", Some("no authorization code received".into()));
        return;
    }
    set_state(job_id, "completing");

    let result: anyhow::Result<()> = async {
        if provider == "codex" {
            let tokens =
                oauth::exchange_openai_callback_input(&verifier, &input, &state_param, &redirect_uri)
                    .await?;
            let label = codex::login_target_label(None)?;
            oauth::save_openai_tokens_for_account(&tokens, &label)?;
        } else {
            let tokens = oauth::exchange_claude_code(&verifier, &input, &redirect_uri).await?;
            let label = claude::login_target_label(None)?;
            oauth::save_claude_tokens_for_account(&tokens, &label)?;
        }
        Ok(())
    }
    .await;

    match result {
        Ok(()) => finish(job_id, "done", None),
        Err(error) => finish(job_id, "failed", Some(format!("{error:#}"))),
    }
}

pub fn status(job_id: &str) -> Option<Value> {
    jobs().lock().unwrap().get(job_id).map(|job| job.record.clone())
}

pub fn cancel(job_id: &str) -> bool {
    let mut table = jobs().lock().unwrap();
    match table.get(job_id) {
        Some(job)
            if matches!(
                job.record["state"].as_str(),
                Some("starting") | Some("waiting_for_code")
            ) =>
        {
            job.cancel.notify_one();
            table.remove(job_id);
            true
        }
        _ => false,
    }
}

/// Wait until the job reaches a terminal state, then return its record.
pub async fn wait(job_id: &str) -> Option<Value> {
    let rx = {
        let table = jobs().lock().unwrap();
        table.get(job_id).map(|job| job.state_tx.subscribe())
    };
    if let Some(mut rx) = rx {
        loop {
            let state = rx.borrow().clone();
            if state == "done" || state == "failed" || state == "cancelled" {
                break;
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
    status(job_id)
}
