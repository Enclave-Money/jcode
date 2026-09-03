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
//!
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
    // The authenticated identity completing this login (a team member's email,
    // or the owner's username). Stamped onto the pooled account as `added_by` so
    // a team can see which member each pooled subscription belongs to.
    member: Option<String>,
    state_tx: watch::Sender<String>,
    cancel: Arc<tokio::sync::Notify>,
}

fn jobs() -> &'static Mutex<HashMap<String, Job>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

// A pending login lives only in this process's memory, but the browser round
// trip takes ~30s — and the bridge can restart in that window (a desktop client
// replacing a stale build, a systemd bounce, a crash). A restarted bridge then
// answered `complete_login` with "no login job", stranding every sign-in. So
// the secret is ALSO written to disk (0600, short-lived) and recovered on a
// miss, making completion survive a restart.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedJob {
    job_id: String,
    verifier: String,
    state_param: String,
    redirect_uri: String,
    provider: String,
    member: Option<String>,
    started_secs: u64,
}

fn login_jobs_dir() -> Option<std::path::PathBuf> {
    let dir = jcode_base::storage::jcode_dir().ok()?.join("login-jobs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn persist_job(p: &PersistedJob) {
    let Some(dir) = login_jobs_dir() else { return };
    let path = dir.join(format!("{}.json", p.job_id));
    let Ok(json) = serde_json::to_string(p) else {
        return;
    };
    if std::fs::write(&path, json).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

fn load_persisted(job_id: &str) -> Option<PersistedJob> {
    let path = login_jobs_dir()?.join(format!("{job_id}.json"));
    let p: PersistedJob = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
    if now_secs().saturating_sub(p.started_secs) > LOGIN_TIMEOUT_SECS {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(p)
}

fn remove_persisted(job_id: &str) {
    if let Some(dir) = login_jobs_dir() {
        let _ = std::fs::remove_file(dir.join(format!("{job_id}.json")));
    }
}

/// Rebuild an in-memory Job from a disk record so set_state/finish/status work
/// after a bridge restart, and a concurrent second complete dedupes as busy.
fn reregister(p: &PersistedJob) {
    let record = json!({
        "job_id": p.job_id,
        "provider": p.provider,
        "state": "waiting_for_code",
        "started_at": p.started_secs,
    });
    let (state_tx, _) = watch::channel("waiting_for_code".to_string());
    jobs()
        .lock()
        .unwrap()
        .entry(p.job_id.clone())
        .or_insert(Job {
            record,
            verifier: p.verifier.clone(),
            state_param: p.state_param.clone(),
            redirect_uri: p.redirect_uri.clone(),
            provider: p.provider.clone(),
            member: p.member.clone(),
            state_tx,
            cancel: Arc::new(tokio::sync::Notify::new()),
        });
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
    // A sign-in lands in the DOOR's auth file, but turns run in a ROOM as that
    // room's Unix user, reading that user's own file. Without this the account
    // never reaches the place that needs it and every turn fails with "no
    // Claude account for you" moments after a successful sign-in.
    if state == "done" {
        let _ = crate::rooms::request_credential_sync(&crate::rooms::door_home());
    }
    {
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
    // Terminal: the on-disk secret is no longer needed.
    remove_persisted(job_id);
}

fn normalize(provider: &str) -> &'static str {
    if provider == "codex" || provider == "openai" {
        "codex"
    } else {
        "claude"
    }
}

/// Start a loopback-relay login. `redirect_uri` is the app's own loopback
/// listener (e.g. `http://localhost:49xxx/callback`). Returns the job id; the
/// `url` field is on the record immediately (the authorize link).
pub fn start(provider: &str, redirect_uri: &str, member: Option<&str>) -> String {
    let provider = normalize(provider).to_string();
    let member = member.map(str::to_string);
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
    let state = if provider == "codex" {
        state
    } else {
        verifier.clone()
    };

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
    // Persist the secret to disk so completion survives a bridge restart.
    persist_job(&PersistedJob {
        job_id: job_id.clone(),
        verifier: verifier.clone(),
        state_param: state.clone(),
        redirect_uri: redirect_uri.to_string(),
        provider: provider.clone(),
        member: member.clone(),
        started_secs: now_secs(),
    });
    jobs().lock().unwrap().insert(
        job_id.clone(),
        Job {
            record,
            verifier,
            state_param: state,
            redirect_uri: redirect_uri.to_string(),
            provider,
            member,
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
                    drop(table);
                    remove_persisted(&expire_id);
                }
            }
        }
    });

    job_id
}

/// Complete a waiting login: exchange the code/callback the app relayed for
/// tokens, in process, and save them to the daemon's account store.
pub async fn complete(job_id: &str, input: &str) {
    enum Src {
        Live((String, String, String, String, Option<String>)),
        Busy,
        Missing,
    }
    let src = {
        let table = jobs().lock().unwrap();
        match table.get(job_id) {
            Some(job) if job.record["state"] == "waiting_for_code" => Src::Live((
                job.verifier.clone(),
                job.state_param.clone(),
                job.redirect_uri.clone(),
                job.provider.clone(),
                job.member.clone(),
            )),
            Some(_) => Src::Busy, // already completing or terminal — don't re-run
            None => Src::Missing,
        }
    };
    let (verifier, state_param, redirect_uri, provider, member) = match src {
        Src::Live(v) => v,
        Src::Busy => return,
        Src::Missing => match load_persisted(job_id) {
            // The bridge restarted after the login started; recover the secret
            // from disk and re-register so the rest of this flow works normally.
            Some(p) => {
                reregister(&p);
                (
                    p.verifier,
                    p.state_param,
                    p.redirect_uri,
                    p.provider,
                    p.member,
                )
            }
            None => return,
        },
    };
    let input = input.trim().to_string();
    if input.is_empty() {
        finish(
            job_id,
            "failed",
            Some("no authorization code received".into()),
        );
        return;
    }
    set_state(job_id, "completing");

    let result: anyhow::Result<()> = async {
        if provider == "codex" {
            let tokens = oauth::exchange_openai_callback_input(
                &verifier,
                &input,
                &state_param,
                &redirect_uri,
            )
            .await?;
            // Identity-aware append (pooling), mirroring the Claude path.
            let requested = codex::login_target_label(None)?;
            let (label, _email) = oauth::save_openai_login(&tokens, &requested).await?;
            // Stamp the member, exactly as the Claude branch below does.
            // Skipping it left every OpenAI account unattributed, so the
            // accounts list could not tell a member's own sign-in from the
            // owner's and showed it to the wrong person.
            if let Some(member) = member.as_deref() {
                let _ = codex::set_account_added_by(&label, member);
            }
        } else {
            let tokens = oauth::exchange_claude_code(&verifier, &input, &redirect_uri).await?;
            // Identity-aware save: fetch the account's profile email and, when it
            // names a DIFFERENT Anthropic account than the active one, APPEND it
            // as a new pooled account instead of overwriting. This is what lets a
            // team pool every member's Claude subscription on the server — the
            // daemon's same-provider failover then rotates across the pool. Using
            // login_target_label + save_*_tokens_for_account (the old path) made
            // each member's sign-in clobber the previous one.
            let requested = claude::login_target_label(None)?;
            let (label, _email) = oauth::save_claude_login(&tokens, &requested).await?;
            if let Some(member) = member.as_deref() {
                let _ = claude::set_account_added_by(&label, member);
            }
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
    jobs()
        .lock()
        .unwrap()
        .get(job_id)
        .map(|job| job.record.clone())
}

pub fn cancel(job_id: &str) -> bool {
    let cancelled = {
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
    };
    // Clear the on-disk secret whether or not it was still in memory (a
    // restarted bridge only has the disk copy).
    remove_persisted(job_id);
    cancelled
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
