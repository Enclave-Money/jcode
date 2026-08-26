//! Bridge-global GitHub device-flow auth jobs.
//!
//! `connect_github` runs `gh auth login` (device flow) ON THIS RUNTIME, so
//! the credential lands where the agent actually shells out — the team VM
//! when connected to a team server, the local machine otherwise. The verb
//! replies with the one-time code + verification URL for the app to show;
//! `github_status` with a `job_id` polls the job, without one it reports the
//! runtime's current `gh auth status`.
//!
//! No client ids or secrets live anywhere in blaude: gh owns the OAuth app,
//! the token, and the git credential helper wiring. The app only ever sees
//! the user-facing code and the resulting login name.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// gh gives device codes ~15 minutes; keep the job a little shorter.
const OVERALL_TIMEOUT_SECS: u64 = 840;

fn jobs() -> &'static Mutex<HashMap<String, Value>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
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
    format!("gh-{:x}{:04x}", now_secs(), nanos & 0xffff)
}

fn update(job_id: &str, patch: impl FnOnce(&mut Value)) {
    if let Ok(mut map) = jobs().lock() {
        if let Some(rec) = map.get_mut(job_id) {
            patch(rec);
        }
    }
}

/// Current record for a job, if it exists.
pub fn status(job_id: &str) -> Option<Value> {
    jobs().lock().ok().and_then(|map| map.get(job_id).cloned())
}

fn gh_bin() -> Option<String> {
    for candidate in ["gh", "/usr/bin/gh", "/usr/local/bin/gh", "/opt/homebrew/bin/gh"] {
        let probe = std::process::Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if probe.map(|s| s.success()).unwrap_or(false) {
            return Some(candidate.to_string());
        }
    }
    None
}

/// The runtime's current GitHub identity: `{connected, login?}`.
pub fn account_status() -> Value {
    let Some(gh) = gh_bin() else {
        return json!({
            "done": true,
            "connected": false,
            "error": "GitHub CLI (gh) is not installed on this runtime",
        });
    };
    let output = std::process::Command::new(&gh)
        .args(["auth", "status", "--hostname", "github.com"])
        .output();
    match output {
        Ok(out) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            // "✓ Logged in to github.com account <login> (…)"
            let login = text
                .lines()
                .find(|line| line.contains("Logged in to github.com"))
                .and_then(|line| {
                    let mut parts = line.split_whitespace();
                    while let Some(word) = parts.next() {
                        if word == "account" {
                            return parts.next().map(str::to_string);
                        }
                    }
                    None
                });
            json!({
                "done": true,
                "connected": out.status.success() && login.is_some(),
                "login": login,
            })
        }
        Err(error) => json!({
            "done": true,
            "connected": false,
            "error": format!("gh auth status failed: {error}"),
        }),
    }
}

/// Start the device flow. Returns the job record once the one-time code is
/// known (or a finished record carrying the error).
pub async fn start() -> Value {
    let Some(gh) = gh_bin() else {
        return json!({
            "job_id": "",
            "done": true,
            "error": "GitHub CLI (gh) is not installed on this runtime — install it on the server to connect GitHub",
        });
    };
    let job_id = new_job_id();
    if let Ok(mut map) = jobs().lock() {
        map.insert(
            job_id.clone(),
            json!({
                "job_id": job_id,
                "stage": "Starting GitHub sign-in…",
                "done": false,
                "started_at": now_secs(),
            }),
        );
    }

    let spawn = tokio::process::Command::new(&gh)
        .args([
            "auth", "login",
            "--hostname", "github.com",
            "--git-protocol", "https",
            "--web",
            "--skip-ssh-key",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(error) => {
            update(&job_id, |rec| {
                rec["done"] = json!(true);
                rec["error"] = json!(format!("could not start gh: {error}"));
            });
            return status(&job_id).unwrap_or_default();
        }
    };

    // gh prints the code + URL on stderr; watch both streams anyway and keep
    // a tail for error reporting.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let watch_id = job_id.clone();
    let reader_id = job_id.clone();
    let watch = |id: String, stream: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>| async move {
        use tokio::io::AsyncBufReadExt;
        let Some(stream) = stream else { return };
        let mut lines = tokio::io::BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(code) = line.split("one-time code:").nth(1) {
                let code = code.trim().to_string();
                update(&id, |rec| {
                    rec["stage"] = json!("Waiting for you to approve in the browser…");
                    rec["user_code"] = json!(code);
                    rec["verification_uri"] = json!("https://github.com/login/device");
                });
            }
            update(&id, |rec| {
                let mut tail = rec["output_tail"].as_str().unwrap_or_default().to_string();
                tail.push_str(&line);
                tail.push('\n');
                let keep: String = tail
                    .chars()
                    .skip(tail.chars().count().saturating_sub(800))
                    .collect();
                rec["output_tail"] = json!(keep);
            });
        }
    };
    let out_task = tokio::spawn(watch(
        watch_id,
        stdout.map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
    ));
    let err_task = tokio::spawn(watch(
        reader_id,
        stderr.map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
    ));

    let waiter_id = job_id.clone();
    tokio::spawn(async move {
        let waited = tokio::time::timeout(
            Duration::from_secs(OVERALL_TIMEOUT_SECS),
            child.wait(),
        )
        .await;
        let _ = out_task.await;
        let _ = err_task.await;
        match waited {
            Ok(Ok(code)) if code.success() => {
                let account = account_status();
                update(&waiter_id, |rec| {
                    rec["done"] = json!(true);
                    rec["stage"] = json!("Connected.");
                    rec["connected"] = json!(true);
                    rec["login"] = account["login"].clone();
                });
            }
            Ok(Ok(_)) => {
                let tail = status(&waiter_id)
                    .and_then(|rec| rec["output_tail"].as_str().map(str::to_string))
                    .unwrap_or_default();
                update(&waiter_id, |rec| {
                    rec["done"] = json!(true);
                    rec["error"] = json!(format!(
                        "GitHub sign-in did not complete: {}",
                        tail.lines().last().unwrap_or("gh exited with an error")
                    ));
                });
            }
            Ok(Err(error)) => update(&waiter_id, |rec| {
                rec["done"] = json!(true);
                rec["error"] = json!(format!("gh failed: {error}"));
            }),
            Err(_) => update(&waiter_id, |rec| {
                rec["done"] = json!(true);
                rec["error"] = json!("GitHub sign-in timed out — start it again");
            }),
        }
    });

    // Reply once the one-time code is parsed (arrives within a second or
    // two); give up after 20s and let the poll surface whatever happened.
    for _ in 0..100 {
        if let Some(rec) = status(&job_id) {
            if rec["user_code"].as_str().is_some() || rec["done"].as_bool() == Some(true) {
                return rec;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    status(&job_id).unwrap_or_else(|| json!({"job_id": job_id, "done": false}))
}
