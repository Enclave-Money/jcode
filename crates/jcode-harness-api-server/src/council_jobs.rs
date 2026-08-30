//! Bridge-global council jobs.
//!
//! A council run fans a prompt to several models and deliberates for
//! minutes. Tying it to the requesting connection (the first design) meant
//! an app relaunch, a dropped socket, or a closed chat silently killed or
//! orphaned the run. Jobs instead live in a process-global table: `run`
//! replies immediately with a job id, any connection can `await`/`status`/
//! `cancel` it, and finished records persist to `~/.jcode/council-runs/`
//! so results survive both app and bridge restarts.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::watch;

const RUN_TIMEOUT_SECS: u64 = 1200;
/// Newest finished records kept on disk; older ones are pruned.
const KEEP_RECORDS: usize = 50;

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

fn runs_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".jcode/council-runs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_job_id() -> String {
    // Time + a pinch of entropy; collision would need two starts in the
    // same nanosecond, and the table insert would still keep both distinct
    // callers honest via the later duplicate check.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("cr-{:x}{:04x}", now_secs(), nanos & 0xffff)
}

/// Start a run; returns the job id immediately.
pub fn start(
    name: String,
    prompt: String,
    working_dir: Option<String>,
    tag: Option<String>,
) -> String {
    let job_id = new_job_id();
    let record = json!({
        "job_id": job_id,
        "name": name,
        "tag": tag,
        "state": "running",
        "prompt": prompt,
        "started_at": now_secs(),
    });
    let (state_tx, _) = watch::channel("running".to_string());
    let cancel = Arc::new(tokio::sync::Notify::new());
    jobs().lock().unwrap().insert(
        job_id.clone(),
        Job {
            record: record.clone(),
            state_tx: state_tx.clone(),
            cancel: cancel.clone(),
        },
    );

    let id = job_id.clone();
    tokio::spawn(async move {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("blaude"));
        let dir = runs_dir();
        let out_path = dir.as_ref().map(|d| d.join(format!("{id}.out")));
        let err_path = dir.as_ref().map(|d| d.join(format!("{id}.err")));
        let mut command = tokio::process::Command::new(exe);
        command.arg("council").arg("run").arg(&name).arg(&prompt);
        if let Some(cwd) = &working_dir {
            command.current_dir(cwd);
        }
        // Output streams to files so the child stays wait()-able (and thus
        // killable) — wait_with_output() would consume it.
        if let (Some(out), Some(err)) = (&out_path, &err_path) {
            if let (Ok(o), Ok(e)) = (std::fs::File::create(out), std::fs::File::create(err)) {
                command.stdout(o).stderr(e);
            }
        }
        // Belt and braces: if the bridge process dies, children die too.
        command.kill_on_drop(true);

        let (state, error) = match command.spawn() {
            Err(error) => (
                "failed".to_string(),
                Some(format!("couldn't launch council: {error}")),
            ),
            Ok(mut child) => {
                tokio::select! {
                    _ = cancel.notified() => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        ("cancelled".to_string(), None)
                    }
                    status = child.wait() => match status {
                        Ok(s) if s.success() => ("done".to_string(), None),
                        Ok(_) => {
                            let err = err_path
                                .as_ref()
                                .and_then(|p| std::fs::read_to_string(p).ok())
                                .unwrap_or_default();
                            ("failed".to_string(), Some(err.trim().to_string()))
                        }
                        Err(error) => ("failed".to_string(), Some(error.to_string())),
                    },
                    _ = tokio::time::sleep(std::time::Duration::from_secs(RUN_TIMEOUT_SECS)) => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        ("failed".to_string(), Some(format!("council run timed out after {RUN_TIMEOUT_SECS}s")))
                    }
                }
            }
        };

        let output = if state == "done" {
            out_path
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
        } else {
            None
        };
        finish(&id, &state, output, error);
    });

    job_id
}

fn finish(job_id: &str, state: &str, output: Option<String>, error: Option<String>) {
    let mut table = jobs().lock().unwrap();
    if let Some(job) = table.get_mut(job_id) {
        job.record["state"] = json!(state);
        job.record["finished_at"] = json!(now_secs());
        if let Some(output) = output {
            job.record["output"] = json!(output);
        }
        if let Some(error) = error {
            job.record["error"] = json!(error);
        }
        persist(&job.record);
        let _ = job.state_tx.send(state.to_string());
    }
}

fn persist(record: &Value) {
    let Some(dir) = runs_dir() else { return };
    let Some(job_id) = record["job_id"].as_str() else {
        return;
    };
    let path = dir.join(format!("{job_id}.json"));
    if let Ok(text) = serde_json::to_string_pretty(record) {
        let _ = std::fs::write(path, text);
    }
    prune(&dir);
}

fn prune(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut records: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if records.len() <= KEEP_RECORDS {
        return;
    }
    records.sort_by_key(|(modified, _)| *modified);
    for (_, path) in records.iter().take(records.len() - KEEP_RECORDS) {
        let stem = path.with_extension("");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(stem.with_extension("out"));
        let _ = std::fs::remove_file(stem.with_extension("err"));
    }
}

/// Current record for one job — memory first, then the disk archive
/// (runs from a previous bridge instance).
pub fn status(job_id: &str) -> Option<Value> {
    if let Some(job) = jobs().lock().unwrap().get(job_id) {
        return Some(job.record.clone());
    }
    let dir = runs_dir()?;
    let text = std::fs::read_to_string(dir.join(format!("{job_id}.json"))).ok()?;
    let mut record: Value = serde_json::from_str(&text).ok()?;
    // A record persisted as "running" belongs to a dead bridge — its child
    // died with that process.
    if record["state"] == "running" {
        record["state"] = json!("failed");
        record["error"] = json!("bridge restarted while the council was deliberating");
    }
    Some(record)
}

/// Kill a running job. Returns false when the job is unknown or already
/// terminal.
pub fn cancel(job_id: &str) -> bool {
    let table = jobs().lock().unwrap();
    match table.get(job_id) {
        Some(job) if job.record["state"] == "running" => {
            job.cancel.notify_one();
            true
        }
        _ => false,
    }
}

/// Wait until the job leaves "running", then return its record.
pub async fn wait(job_id: &str) -> Option<Value> {
    let rx = {
        let table = jobs().lock().unwrap();
        match table.get(job_id) {
            Some(job) if job.record["state"] == "running" => Some(job.state_tx.subscribe()),
            _ => None,
        }
    };
    if let Some(mut rx) = rx {
        while rx.borrow().as_str() == "running" {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
    status(job_id)
}

/// All known runs (memory + disk), newest first, optionally tag-filtered.
pub fn list(tag: Option<&str>) -> Vec<Value> {
    let mut by_id: HashMap<String, Value> = HashMap::new();
    if let Some(dir) = runs_dir()
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|x| x == "json")
                && let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(mut record) = serde_json::from_str::<Value>(&text)
            {
                if record["state"] == "running" {
                    record["state"] = json!("failed");
                    record["error"] = json!("bridge restarted while the council was deliberating");
                }
                if let Some(id) = record["job_id"].as_str() {
                    by_id.insert(id.to_string(), record);
                }
            }
        }
    }
    for job in jobs().lock().unwrap().values() {
        if let Some(id) = job.record["job_id"].as_str() {
            by_id.insert(id.to_string(), job.record.clone());
        }
    }
    let mut runs: Vec<Value> = by_id
        .into_values()
        .filter(|r| tag.is_none_or(|t| r["tag"].as_str() == Some(t)))
        .collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r["started_at"].as_u64().unwrap_or(0)));
    runs
}
