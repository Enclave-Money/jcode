//! Permission prompts over the harness API, via the safety files.
//!
//! The daemon's permission system is file-mediated by design: gated tools push
//! into `$JCODE_HOME/safety/queue.json` and poll for decisions that arrive in
//! `safety/history.json` (see jcode-base/src/safety.rs — the TUI overlay
//! answers through `record_permission_via_file` the same way). The bridge
//! therefore needs NO legacy-protocol change to support prompts: it watches
//! the queue and records decisions with identical file semantics, and the
//! `permissions` capability tells clients the round-trip is real.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct PendingPermission {
    pub id: String,
    pub action: String,
    pub description: String,
}

fn jcode_dir() -> PathBuf {
    std::env::var("JCODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".jcode")
        })
}

fn queue_path() -> PathBuf {
    jcode_dir().join("safety").join("queue.json")
}

fn history_path() -> PathBuf {
    jcode_dir().join("safety").join("history.json")
}

/// All pending permission requests, lenient about fields this build ignores.
pub fn pending() -> Vec<PendingPermission> {
    let Ok(raw) = std::fs::read_to_string(queue_path()) else {
        return Vec::new();
    };
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            let id = entry["id"].as_str()?.to_string();
            let action = entry["action"].as_str().unwrap_or("action").to_string();
            let mut description = entry["description"].as_str().unwrap_or("").to_string();
            if let Some(rationale) = entry["rationale"].as_str() {
                if !rationale.is_empty() {
                    if !description.is_empty() {
                        description.push_str(" — ");
                    }
                    description.push_str(rationale);
                }
            }
            Some(PendingPermission {
                id,
                action,
                description,
            })
        })
        .collect()
}

/// The recorded outcome for a request id, if any (newest wins).
pub fn decision_for(request_id: &str) -> Option<bool> {
    let raw = std::fs::read_to_string(history_path()).ok()?;
    let Value::Array(entries) = serde_json::from_str::<Value>(&raw).ok()? else {
        return None;
    };
    entries
        .iter()
        .rev()
        .find(|entry| entry["request_id"].as_str() == Some(request_id))
        .and_then(|entry| entry["approved"].as_bool())
}

/// Record a decision exactly the way `safety::record_permission_via_file`
/// does: drop from the queue, append to history. The daemon polls these files
/// while the gated tool waits.
pub fn record_decision(request_id: &str, approved: bool, note: Option<String>) -> Result<()> {
    let qp = queue_path();
    if let Some(parent) = qp.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut queue: Vec<Value> = std::fs::read_to_string(&qp)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    queue.retain(|entry| entry["id"].as_str() != Some(request_id));
    std::fs::write(&qp, serde_json::to_string_pretty(&queue)?)
        .with_context(|| format!("write {}", qp.display()))?;

    let hp = history_path();
    let mut history: Vec<Value> = std::fs::read_to_string(&hp)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let mut decision = json!({
        "request_id": request_id,
        "approved": approved,
        "decided_at": now_rfc3339(),
        "decided_via": "harness-api",
    });
    if let Some(note) = note {
        decision["message"] = Value::String(note);
    }
    history.push(decision);
    std::fs::write(&hp, serde_json::to_string_pretty(&history)?)
        .with_context(|| format!("write {}", hp.display()))?;
    Ok(())
}

fn now_rfc3339() -> String {
    // chrono-compatible UTC timestamp without pulling chrono into the bridge.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let micros = now.subsec_micros();
    let days = secs / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{micros:06}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days-since-epoch → (year, month, day); Howard Hinnant's civil_from_days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod permission_file_tests {
    use super::*;

    #[test]
    fn decision_moves_request_from_queue_to_history() {
        // JCODE_HOME is process-global: hold the crate-wide lock for the
        // whole test, or a ScopedJcodeHome test running in parallel swaps the
        // env var mid-write.
        let _guard = crate::jcode_home_test_lock();
        let dir = std::env::temp_dir().join(format!("jcode-perm-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("safety")).unwrap();
        let previous = std::env::var_os("JCODE_HOME");
        unsafe { std::env::set_var("JCODE_HOME", &dir) };
        std::fs::write(
            dir.join("safety/queue.json"),
            r#"[{"id":"req_1","action":"bash","description":"run tests","rationale":"verify","urgency":"medium","wait":true,"created_at":"2026-08-23T00:00:00Z"}]"#,
        )
        .unwrap();

        let pending = pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "req_1");
        assert!(pending[0].description.contains("verify"));

        record_decision("req_1", true, Some("always".into())).unwrap();
        assert!(super::pending().is_empty());
        assert_eq!(decision_for("req_1"), Some(true));
        // The stored decision must be readable as chrono RFC3339.
        let history = std::fs::read_to_string(dir.join("safety/history.json")).unwrap();
        assert!(history.contains("decided_via"), "{history}");
        assert!(history.contains("Z\""), "{history}");

        match previous {
            Some(value) => unsafe { std::env::set_var("JCODE_HOME", value) },
            None => unsafe { std::env::remove_var("JCODE_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timestamps_are_sane() {
        let stamp = now_rfc3339();
        assert!(stamp.starts_with("20"), "{stamp}");
        assert!(stamp.ends_with('Z'));
        assert_eq!(stamp.len(), "2026-08-23T01:02:03.123456Z".len());
    }
}
