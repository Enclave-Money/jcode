//! Bridge-global create-team provisioning jobs.
//!
//! `create_team` builds a real team server: a fresh cloud VM running the
//! blaude daemon + bridge behind wss, with an owner token and a pinned CA.
//! Provisioning takes minutes, so it runs as a process-global async job
//! (the `council_jobs` shape): the verb replies immediately with a status
//! record and any connection polls `team_create_status` until `done`.
//!
//! The job shells out to `gcloud` with the OWNER's local credentials — the
//! bridge runs on the owner's machine, which is exactly where their cloud
//! auth lives. The app never sees a cloud credential; it gets back only the
//! finished endpoint (ws_url + owner token + CA PEM).
//!
//! Cloud settings come from `~/.jcode/team-cloud.json`:
//!   { "project": …, "zone": …, "machine_type": …, "template_instance": … }
//! with defaults matching the existing hand-built team server, and
//! `template_instance` naming a VM whose `~/blaude-agent/target/release/
//! blaude` is copied onto new servers (cached locally after the first pull).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const OVERALL_TIMEOUT_SECS: u64 = 900;
const PORT: u16 = 7644;

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
    format!("tc-{:x}{:04x}", now_secs(), nanos & 0xffff)
}

fn set_stage(job_id: &str, stage: &str) {
    if let Ok(mut map) = jobs().lock() {
        if let Some(rec) = map.get_mut(job_id) {
            rec["stage"] = json!(stage);
        }
    }
}

fn finish_err(job_id: &str, message: String) {
    if let Ok(mut map) = jobs().lock() {
        if let Some(rec) = map.get_mut(job_id) {
            rec["done"] = json!(true);
            rec["error"] = json!(message);
        }
    }
}

fn finish_ok(job_id: &str, ws_url: &str, token: &str, ca_pem: &str) {
    if let Ok(mut map) = jobs().lock() {
        if let Some(rec) = map.get_mut(job_id) {
            rec["done"] = json!(true);
            rec["stage"] = json!("Ready.");
            rec["ws_url"] = json!(ws_url);
            rec["token"] = json!(token);
            rec["ca_pem"] = json!(ca_pem);
        }
    }
}

/// Current record for a job, if it exists.
pub fn status(job_id: &str) -> Option<Value> {
    jobs().lock().ok()?.get(job_id).cloned()
}

/// Start provisioning; returns the initial record immediately.
pub fn start(name: &str) -> Value {
    let job_id = new_job_id();
    let record = json!({
        "job_id": job_id,
        "name": name,
        "stage": "Checking Google Cloud…",
        "done": false,
    });
    if let Ok(mut map) = jobs().lock() {
        map.insert(job_id.clone(), record.clone());
    }
    let name = name.to_string();
    tokio::spawn(async move {
        let id = job_id.clone();
        let work = provision(job_id.clone(), name);
        match tokio::time::timeout(Duration::from_secs(OVERALL_TIMEOUT_SECS), work).await {
            Ok(Ok(())) => {}
            Ok(Err(message)) => finish_err(&id, message),
            Err(_) => finish_err(
                &id,
                "Ran out of time building the server. It may still finish — check your \
                 cloud console, or try again."
                    .into(),
            ),
        }
    });
    record
}

// ---------------------------------------------------------------------------
// cloud config

struct CloudCfg {
    project: String,
    zone: String,
    machine_type: String,
    template_instance: String,
}

fn cloud_cfg() -> CloudCfg {
    let mut cfg = CloudCfg {
        project: "enclave-money".into(),
        zone: "asia-south1-a".into(),
        machine_type: "e2-small".into(),
        template_instance: "blaude-india-1".into(),
    };
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home).join(".jcode/team-cloud.json");
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(s) = v["project"].as_str() {
                    cfg.project = s.into();
                }
                if let Some(s) = v["zone"].as_str() {
                    cfg.zone = s.into();
                }
                if let Some(s) = v["machine_type"].as_str() {
                    cfg.machine_type = s.into();
                }
                if let Some(s) = v["template_instance"].as_str() {
                    cfg.template_instance = s.into();
                }
            }
        }
    }
    cfg
}

fn gcloud_bin() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![
        "/opt/homebrew/bin/gcloud".into(),
        "/usr/local/bin/gcloud".into(),
        "/usr/bin/gcloud".into(),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(&home).join("google-cloud-sdk/bin/gcloud"));
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            candidates.push(dir.join("gcloud"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// process helpers

async fn run(bin: &PathBuf, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("could not run gcloud: {e}"))?;
    if let Some(text) = stdin {
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(text.as_bytes()).await;
            let _ = pipe.shutdown().await;
        }
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| format!("gcloud did not finish: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(if stderr.is_empty() { stdout } else { stderr })
    }
}

/// Retry a gcloud call while the fresh VM's SSH comes up (key propagation
/// takes ~30-60s on a brand-new instance).
async fn run_retry(
    bin: &PathBuf,
    args: &[&str],
    stdin: Option<&str>,
    tries: u32,
) -> Result<String, String> {
    let mut last = String::new();
    for attempt in 0..tries {
        match run(bin, args, stdin).await {
            Ok(out) => return Ok(out),
            Err(e) => last = e,
        }
        if attempt + 1 < tries {
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    }
    Err(last)
}

fn slugify(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    let slug = if slug.is_empty() { "team".into() } else { slug };
    slug.chars().take(20).collect()
}

// ---------------------------------------------------------------------------
// the provisioning sequence

async fn provision(job_id: String, name: String) -> Result<(), String> {
    let Some(gcloud) = gcloud_bin() else {
        return Err(
            "Google Cloud isn't set up on this Mac. Install the gcloud CLI and run \
             `gcloud auth login`, then try again."
                .into(),
        );
    };
    let cfg = cloud_cfg();

    // Authed at all? A cheap read that fails fast when logged out.
    run(
        &gcloud,
        &[
            "auth",
            "print-access-token",
            "--quiet",
        ],
        None,
    )
    .await
    .map_err(|e| format!("Google Cloud isn't signed in — run `gcloud auth login`. ({e})"))?;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let instance = format!("blaude-{}-{:04x}", slugify(&name), nanos & 0xffff);

    // The shared firewall rule for every created team: 7644 open to the
    // world, protected by wss + per-member bearer tokens (same posture as
    // the app's join flow). Creating it twice is fine — "already exists"
    // is success.
    set_stage(&job_id, "Creating the server…");
    if let Err(e) = run(
        &gcloud,
        &[
            "compute",
            "firewall-rules",
            "create",
            "blaude-team-7644",
            "--project",
            &cfg.project,
            "--allow",
            &format!("tcp:{PORT}"),
            "--target-tags",
            "blaude-team",
            "--source-ranges",
            "0.0.0.0/0",
            "--description",
            "blaude team servers (wss + bearer tokens)",
            "--quiet",
        ],
        None,
    )
    .await
    {
        if !e.contains("already exists") {
            return Err(format!("Could not open the team port: {e}"));
        }
    }

    run(
        &gcloud,
        &[
            "compute",
            "instances",
            "create",
            &instance,
            "--project",
            &cfg.project,
            "--zone",
            &cfg.zone,
            "--machine-type",
            &cfg.machine_type,
            "--image-family",
            "debian-12",
            "--image-project",
            "debian-cloud",
            "--tags",
            "blaude-team",
            "--quiet",
        ],
        None,
    )
    .await
    .map_err(|e| format!("Could not create the server: {e}"))?;

    let ip = run(
        &gcloud,
        &[
            "compute",
            "instances",
            "describe",
            &instance,
            "--project",
            &cfg.project,
            "--zone",
            &cfg.zone,
            "--format",
            "value(networkInterfaces[0].accessConfigs[0].natIP)",
        ],
        None,
    )
    .await
    .map_err(|e| format!("Could not read the server's address: {e}"))?;
    if ip.is_empty() {
        return Err("The server came up without a public address.".into());
    }

    // The Linux build of blaude, cached locally after the first pull from
    // the template server so later teams skip the double copy.
    set_stage(&job_id, "Copying blaude onto it…");
    let home = std::env::var("HOME").map_err(|_| "no HOME".to_string())?;
    let cache_dir = PathBuf::from(&home).join(".jcode/team-server-cache");
    let cache = cache_dir.join("blaude-linux-x86_64");
    if !cache.is_file() {
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("could not create the local cache: {e}"))?;
        run(
            &gcloud,
            &[
                "compute",
                "scp",
                &format!("{}:~/blaude-agent/target/release/blaude", cfg.template_instance),
                cache.to_str().unwrap_or_default(),
                "--project",
                &cfg.project,
                "--zone",
                &cfg.zone,
                "--quiet",
            ],
            None,
        )
        .await
        .map_err(|e| format!("Could not fetch the blaude server build: {e}"))?;
    }
    run_retry(
        &gcloud,
        &[
            "compute",
            "scp",
            cache.to_str().unwrap_or_default(),
            &format!("{instance}:~/blaude"),
            "--project",
            &cfg.project,
            "--zone",
            &cfg.zone,
            "--quiet",
        ],
        None,
        8,
    )
    .await
    .map_err(|e| format!("Could not copy blaude onto the server: {e}"))?;

    // TLS, tokens, and the two SYSTEM units — the same known-good layout as
    // the hand-built team server (bridge with native wss + no-spawn; daemon
    // with the forever-retry drop-in so it self-heals once an AI account
    // lands). Sent over stdin (`bash -s`) to dodge quoting entirely.
    set_stage(&job_id, "Securing it…");
    let setup = format!(
        r#"set -e
U=$(whoami)
H=$HOME
mkdir -p "$H/.jcode/tls" "$H/.jcode/runtime" "$H/team"
chmod +x "$H/blaude"
openssl req -x509 -newkey rsa:2048 -keyout "$H/.jcode/tls/key.pem" -out "$H/.jcode/tls/cert.pem" \
  -days 3650 -nodes -subj "/CN=blaude-team" \
  -addext "subjectAltName=IP:{ip}" \
  -addext "extendedKeyUsage=serverAuth" \
  -addext "keyUsage=digitalSignature,keyEncipherment" 2>/dev/null
[ -f "$H/.jcode/api-ws-token" ] || {{ openssl rand -hex 24 > "$H/.jcode/api-ws-token"; chmod 600 "$H/.jcode/api-ws-token"; }}
[ -f "$H/.jcode/team-tokens.json" ] || echo '{{}}' > "$H/.jcode/team-tokens.json"
sudo tee /etc/systemd/system/blaude-daemon.service >/dev/null <<UNIT
[Unit]
Description=blaude agent daemon (team server)
After=network-online.target
Wants=network-online.target

[Service]
User=$U
WorkingDirectory=$H/team
Environment=HOME=$H
Environment=JCODE_RUNTIME_DIR=$H/.jcode/runtime
Environment=JCODE_IDLE_TIMEOUT_SECS=0
ExecStart=$H/blaude --provider auto serve
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
UNIT
sudo mkdir -p /etc/systemd/system/blaude-daemon.service.d
sudo tee /etc/systemd/system/blaude-daemon.service.d/hardening.conf >/dev/null <<UNIT
[Unit]
StartLimitIntervalSec=0
[Service]
RestartSec=3
UNIT
sudo tee /etc/systemd/system/blaude-bridge.service >/dev/null <<UNIT
[Unit]
Description=blaude harness API bridge (team server)
After=network-online.target blaude-daemon.service
Wants=network-online.target blaude-daemon.service

[Service]
User=$U
Environment=HOME=$H
Environment=JCODE_RUNTIME_DIR=$H/.jcode/runtime
Environment=JCODE_BRIDGE_NO_SPAWN=1
Environment=JCODE_API_WS_BIND=0.0.0.0
Environment=JCODE_API_WS_TLS_CERT=$H/.jcode/tls/cert.pem
Environment=JCODE_API_WS_TLS_KEY=$H/.jcode/tls/key.pem
ExecStart=$H/blaude api-bridge
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
UNIT
sudo systemctl daemon-reload
sudo systemctl enable --now blaude-daemon.service blaude-bridge.service
echo SETUP_OK
"#
    );
    let out = run_retry(
        &gcloud,
        &[
            "compute",
            "ssh",
            &instance,
            "--project",
            &cfg.project,
            "--zone",
            &cfg.zone,
            "--command",
            "bash -s",
            "--quiet",
        ],
        Some(&setup),
        4,
    )
    .await
    .map_err(|e| format!("Could not set the server up: {e}"))?;
    if !out.contains("SETUP_OK") {
        return Err(format!("Server setup did not finish: {out}"));
    }

    // The wss door answering is the readiness signal (the daemon keeps
    // crash-retrying until an AI account lands — that's expected and the
    // bridge serves sign-in through it).
    set_stage(&job_id, "Starting it…");
    let mut up = false;
    for _ in 0..30 {
        if tokio::net::TcpStream::connect((ip.as_str(), PORT)).await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if !up {
        return Err(format!(
            "The server built but port {PORT} never answered at {ip}. Check the VM's logs."
        ));
    }

    let creds = run(
        &gcloud,
        &[
            "compute",
            "ssh",
            &instance,
            "--project",
            &cfg.project,
            "--zone",
            &cfg.zone,
            "--command",
            "cat ~/.jcode/api-ws-token && echo __CA__ && cat ~/.jcode/tls/cert.pem",
            "--quiet",
        ],
        None,
    )
    .await
    .map_err(|e| format!("Could not read the server's access token: {e}"))?;
    let mut parts = creds.splitn(2, "__CA__");
    let token = parts.next().unwrap_or_default().trim().to_string();
    let ca_pem = parts.next().unwrap_or_default().trim().to_string();
    if token.is_empty() || !ca_pem.contains("BEGIN CERTIFICATE") {
        return Err("The server is up but its credentials could not be read.".into());
    }

    finish_ok(
        &job_id,
        &format!("wss://{ip}:{PORT}/api"),
        &token,
        &ca_pem,
    );
    Ok(())
}
