//! Bridge-global provider login jobs (Claude and Codex/OpenAI).
//!
//! The desktop app used to shell the login CLI itself — a non-wire path
//! (process spawning + stdout scraping in the frontend). Login is now a
//! bridge job like council runs: `start_claude_login` / `start_codex_login`
//! replies with a job id, the authorize URL lands on the job record as soon
//! as the flow prints it, and any connection can await/status/cancel. The
//! child is the same `blaude login <provider> --no-browser --callback`
//! flow, so approval in the browser completes the exchange automatically.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

const LOGIN_TIMEOUT_SECS: u64 = 330;

#[derive(Clone)]
struct Job {
    record: Value,
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
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("lg-{:x}{:04x}", now_secs(), nanos & 0xffff)
}

fn set_field(job_id: &str, key: &str, value: Value) {
    let mut table = jobs().lock().unwrap();
    if let Some(job) = table.get_mut(job_id) {
        job.record[key] = value;
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

/// Start a login; returns the job id immediately. The `url` field appears
/// on the record as soon as the flow prints the authorize link (typically
/// well under a second). `provider` is the CLI login target: "claude" or
/// "codex" (the OpenAI/ChatGPT flow).
pub fn start(provider: &str) -> String {
    let provider = if provider == "codex" { "codex" } else { "claude" };
    let job_id = new_job_id();
    let record = json!({
        "job_id": job_id,
        "provider": provider,
        "state": "starting",
        "started_at": now_secs(),
    });
    let (state_tx, _) = watch::channel("starting".to_string());
    let cancel = Arc::new(tokio::sync::Notify::new());
    jobs().lock().unwrap().insert(
        job_id.clone(),
        Job { record, state_tx, cancel: cancel.clone() },
    );

    let id = job_id.clone();
    // Job records say "codex" (the product name); the CLI's login target
    // for that flow is "openai" (ChatGPT OAuth + Codex account store).
    let login_target = if provider == "codex" { "openai".to_string() } else { provider.to_string() };
    tokio::spawn(async move {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("blaude"));
        let mut command = tokio::process::Command::new(exe);
        command
            .arg("login")
            .arg(&login_target)
            .arg("--no-browser")
            .arg("--callback")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        command.kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                finish(&id, "failed", Some(format!("couldn't launch login: {error}")));
                return;
            }
        };

        // Scrape the authorize URL out of the flow's stderr as it streams.
        let mut stderr = child.stderr.take();
        let url_id = id.clone();
        let reader = tokio::spawn(async move {
            let Some(mut stderr) = stderr.take() else { return String::new() };
            let mut buffer: Vec<u8> = Vec::new();
            let mut url_sent = false;
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buffer.extend_from_slice(&chunk[..n]);
                        if !url_sent
                            && let Ok(text) = std::str::from_utf8(&buffer)
                            && let Some(start) = text.find("https://")
                        {
                            let url: String = text[start..]
                                .chars()
                                .take_while(|c| !c.is_whitespace())
                                .collect();
                            if url.len() > 40 {
                                set_field(&url_id, "url", json!(url));
                                set_field(&url_id, "state", json!("waiting_for_browser"));
                                url_sent = true;
                            }
                        }
                    }
                }
            }
            String::from_utf8_lossy(&buffer)
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .to_string()
        });

        let outcome = tokio::select! {
            _ = cancel.notified() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                ("cancelled", None)
            }
            status = child.wait() => match status {
                Ok(s) if s.success() => ("done", None),
                Ok(_) => ("failed", Some(String::new())),
                Err(error) => ("failed", Some(error.to_string())),
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS)) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                ("failed", Some(format!("sign-in not completed within {LOGIN_TIMEOUT_SECS}s")))
            }
        };
        let last_line = reader.await.unwrap_or_default();
        let error = match outcome {
            ("failed", Some(detail)) if detail.is_empty() => Some(last_line),
            ("failed", detail) => detail,
            _ => None,
        };
        finish(&id, outcome.0, error);
    });

    job_id
}

pub fn status(job_id: &str) -> Option<Value> {
    jobs().lock().unwrap().get(job_id).map(|job| job.record.clone())
}

pub fn cancel(job_id: &str) -> bool {
    let table = jobs().lock().unwrap();
    match table.get(job_id) {
        Some(job)
            if job.record["state"] == "starting"
                || job.record["state"] == "waiting_for_browser" =>
        {
            job.cancel.notify_one();
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
