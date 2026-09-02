//! Asking the provisioning service to build a team server.
//!
//! This file used to BE the provisioning: it shelled out to `gcloud` on the
//! owner's Mac. That only ever worked for one person. Everyone else would have
//! needed the gcloud CLI installed and a login with Compute Admin on a project
//! that is not theirs, so for them "Create a team" could not work at all — and
//! for that one person it broke about daily, because a human `gcloud auth
//! login` expires and nothing renews it.
//!
//! The work now happens in `blaude-provision`, running in a service that holds
//! the cloud credential (see `blaude-provision-api`). No user's machine has
//! one, which is the point.
//!
//! The app's contract is untouched: `create_team` still returns a job record
//! immediately and `team_create_status` still polls it. This mirrors the
//! service's record into a local one so the app never learns that the work
//! moved.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

/// Where the provisioning service lives.
///
/// Overridable so a developer can point at a local `blaude-provision-api`
/// without a rebuild, and so the endpoint can move without shipping a new app.
fn api_base() -> String {
    std::env::var("BLAUDE_PROVISION_API")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_API.to_string())
}

/// The deployed service. Replaced by the real Cloud Run URL at deploy time.
const DEFAULT_API: &str = "https://blaude-provision-api.run.app";

fn jobs() -> &'static Mutex<HashMap<String, Value>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn put(job_id: &str, record: Value) {
    if let Ok(mut map) = jobs().lock() {
        map.insert(job_id.to_string(), record);
    }
}

fn fail(job_id: &str, message: String) {
    if let Ok(mut map) = jobs().lock() {
        let rec = map.entry(job_id.to_string()).or_insert_with(|| json!({}));
        rec["job_id"] = json!(job_id);
        rec["done"] = json!(true);
        rec["error"] = json!(message);
    }
}

/// Current record for a job, if it exists.
pub fn status(job_id: &str) -> Option<Value> {
    jobs().lock().ok()?.get(job_id).cloned()
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        // Creating a team takes minutes, but every INDIVIDUAL call here is a
        // small one — start, or one poll. A short timeout keeps a wedged
        // network from looking like a wedged build.
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("could not make an HTTPS client: {e}"))
}

/// A local job id, used only until the service answers with its own.
fn local_job_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("local-{nanos}")
}

/// Start provisioning; returns the initial record immediately.
pub fn start(name: &str, region: Option<&str>) -> Value {
    let job_id = local_job_id();
    let record = json!({
        "job_id": job_id,
        "name": name,
        "stage": "Asking for a server…",
        "done": false,
    });
    put(&job_id, record.clone());

    let name = name.to_string();
    let region = region.map(str::to_string);
    let local_id = job_id.clone();
    tokio::spawn(async move {
        if let Err(message) = run(&local_id, name, region).await {
            fail(&local_id, message);
        }
    });
    record
}

async fn run(local_id: &str, name: String, region: Option<String>) -> Result<(), String> {
    let token = crate::blaude_account::session_token()
        .await
        .ok_or_else(|| {
            "Sign in to blaude first — creating a team server is tied to your account.".to_string()
        })?;
    let http = client()?;
    let base = api_base();

    let resp = http
        .post(format!("{base}/v1/teams"))
        .bearer_auth(&token)
        .json(&json!({ "name": name, "region": region }))
        .send()
        .await
        .map_err(|e| format!("Could not reach the blaude service that builds servers: {e}"))?;
    let code = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("The server-building service sent something unreadable: {e}"))?;
    if !code.is_success() {
        return Err(service_error(code, &body));
    }

    // From here the service owns the job; this only mirrors it so the app can
    // keep polling the one place it always has.
    let remote_id = body
        .get("job_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "The service started a build but did not name it.".to_string())?
        .to_string();
    let mut mirrored = body.clone();
    mirrored["job_id"] = json!(local_id);
    put(local_id, mirrored);

    // Poll until the service says done. The 2s cadence matches what the app
    // already does on top of this, so a stage change shows up about as fast as
    // it did when the work ran here.
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let resp = match http
            .get(format!("{base}/v1/teams/{remote_id}"))
            .bearer_auth(&token)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // A dropped poll is not a dropped build: the server is still
                // being made. Say so and keep looking.
                eprintln!("team create: poll failed, retrying — {e}");
                continue;
            }
        };
        let code = resp.status();
        let Ok(body) = resp.json::<Value>().await else {
            continue;
        };
        if !code.is_success() {
            return Err(service_error(code, &body));
        }
        let done = body.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut mirrored = body.clone();
        mirrored["job_id"] = json!(local_id);
        put(local_id, mirrored);
        if done {
            return Ok(());
        }
    }
}

/// Turn the service's refusal into something a person can act on.
fn service_error(code: reqwest::StatusCode, body: &Value) -> String {
    let detail = body
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("no reason given");
    if code == reqwest::StatusCode::UNAUTHORIZED {
        return format!("Your blaude sign-in was not accepted: {detail}");
    }
    format!("The service could not build the server ({code}): {detail}")
}

/// Delete a team server, by the ws_url the app holds for it.
pub async fn delete_team(ws_url: &str) -> Result<Value, String> {
    let token = crate::blaude_account::session_token()
        .await
        .ok_or_else(|| "Sign in to blaude first — deleting a team server is tied to your account.".to_string())?;
    let http = client()?;
    let resp = http
        .post(format!("{}/v1/teams/delete", api_base()))
        .bearer_auth(&token)
        // Deleting waits on the instance delete AND on releasing the address,
        // which retries — so this one call is allowed to be slow.
        .timeout(Duration::from_secs(180))
        .json(&json!({ "ws_url": ws_url }))
        .send()
        .await
        .map_err(|e| format!("Could not reach the blaude service that builds servers: {e}"))?;
    let code = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("The server-building service sent something unreadable: {e}"))?;
    if code.is_success() {
        Ok(body)
    } else {
        Err(service_error(code, &body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No cloud credential, and no cloud CLI, may be reachable from a user's
    /// machine. The whole reason provisioning moved is that requiring either
    /// meant only one person could ever create a team.
    #[test]
    fn the_client_never_touches_the_cloud_itself() {
        // Everything above the test module, minus comments — the test names
        // the very things it forbids, and the doc comment explains why they
        // are forbidden, so scanning the whole file matches itself.
        let src = include_str!("team_create_jobs.rs");
        let body = src.split("#[cfg(test)]").next().expect("module body");
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!code.contains("gcloud"), "the app's runtime must not shell out to gcloud");
        assert!(
            !code.contains("Command::new"),
            "the app's runtime must not spawn cloud tooling"
        );
    }

    /// The endpoint has to be overridable, or testing against a local service
    /// means editing and rebuilding the app.
    #[test]
    fn the_endpoint_can_be_pointed_somewhere_else() {
        // Read through the same accessor the code uses, without disturbing a
        // real environment: absent means the shipped default.
        unsafe { std::env::remove_var("BLAUDE_PROVISION_API") };
        assert_eq!(api_base(), DEFAULT_API);
        unsafe { std::env::set_var("BLAUDE_PROVISION_API", "http://127.0.0.1:8080/") };
        assert_eq!(api_base(), "http://127.0.0.1:8080");
        unsafe { std::env::remove_var("BLAUDE_PROVISION_API") };
    }
}
