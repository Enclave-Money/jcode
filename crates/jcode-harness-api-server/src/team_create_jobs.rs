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

const OVERALL_TIMEOUT_SECS: u64 = 1800;
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
/// Region shortcuts the app offers; anything else falls back to the
/// configured default zone.
fn zone_for_region(region: Option<&str>, default_zone: &str) -> String {
    match region.map(|r| r.to_ascii_lowercase()) {
        Some(r) if r.contains("india") => "asia-south1-a".into(),
        Some(r) if r.contains("singapore") => "asia-southeast1-a".into(),
        Some(r) if r.contains("europe") => "europe-west3-a".into(),
        Some(r) if r.contains("us") => "us-central1-a".into(),
        _ => default_zone.into(),
    }
}

pub fn start(name: &str, region: Option<&str>) -> Value {
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
    let region = region.map(str::to_string);
    tokio::spawn(async move {
        let id = job_id.clone();
        let work = provision(job_id.clone(), name, region);
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

async fn provision(job_id: String, name: String, region: Option<String>) -> Result<(), String> {
    let Some(gcloud) = gcloud_bin() else {
        return Err(
            "Google Cloud isn't set up on this Mac. Install the gcloud CLI and run \
             `gcloud auth login`, then try again."
                .into(),
        );
    };
    let cfg = cloud_cfg();
    // The template (binary source) stays in ITS zone; the new server goes to
    // the zone the user chose.
    let template_zone = cfg.zone.clone();
    let zone = zone_for_region(region.as_deref(), &cfg.zone);

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
            "blaude-team-web",
            "--project",
            &cfg.project,
            "--allow",
            "tcp:80,tcp:443",
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
            return Err(format!("Could not open the team ports: {e}"));
        }
        // The rule pre-existing is only success if it actually covers OUR
        // tag — a same-named rule targeting other tags leaves the new VM
        // firewalled shut while every step after reports success (it did,
        // live: the first created team built fully but was unreachable).
        let tags = run(
            &gcloud,
            &[
                "compute",
                "firewall-rules",
                "describe",
                "blaude-team-web",
                "--project",
                &cfg.project,
                "--format",
                "value(targetTags.list())",
            ],
            None,
        )
        .await
        .unwrap_or_default();
        if !tags.split([',', ';', ' ']).any(|t| t.trim() == "blaude-team") {
            let merged = if tags.trim().is_empty() {
                "blaude-team".to_string()
            } else {
                format!("{},blaude-team", tags.trim().replace(';', ","))
            };
            run(
                &gcloud,
                &[
                    "compute",
                    "firewall-rules",
                    "update",
                    "blaude-team-web",
                    "--project",
                    &cfg.project,
                    "--target-tags",
                    &merged,
                ],
                None,
            )
            .await
            .map_err(|e| format!("The team firewall rule exists but does not cover team servers, and updating it failed: {e}"))?;
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
            &zone,
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
            &zone,
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
    // A stale cache ships an old server build to brand-new teams forever;
    // re-pull daily.
    let cache_stale = cache
        .metadata()
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e.as_secs() > 86_400).unwrap_or(true))
        .unwrap_or(true);
    if cache_stale {
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
                &template_zone,
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
            &zone,
            "--quiet",
        ],
        None,
        8,
    )
    .await
    .map_err(|e| format!("Could not copy blaude onto the server: {e}"))?;

    // The email key rides along when the owner has one, so invites from the
    // new team send real emails from day one (the setup script moves it into
    // ~/.jcode and locks it down). Best-effort: no key just means invites
    // fall back to share-the-endpoint.
    let clerk = PathBuf::from(&home).join(".jcode/clerk.env");
    if clerk.is_file() {
        let _ = run_retry(
            &gcloud,
            &[
                "compute",
                "scp",
                clerk.to_str().unwrap_or_default(),
                &format!("{instance}:~/clerk.env"),
                "--project",
                &cfg.project,
                "--zone",
                &zone,
                "--quiet",
            ],
            None,
            3,
        )
        .await;
    }

    // TLS, tokens, and the two SYSTEM units — the same known-good layout as
    // the hand-built team server (bridge with native wss + no-spawn; daemon
    // with the forever-retry drop-in so it self-heals once an AI account
    // lands). Sent over stdin (`bash -s`) to dodge quoting entirely.
    set_stage(&job_id, "Securing it…");
    let domain = format!("{}.sslip.io", ip.replace('.', "-"));
    let setup = format!(
        r#"set -e
U=$(whoami)
H=$HOME
mkdir -p "$H/.jcode/tls" "$H/.jcode/runtime" "$H/team"
chmod +x "$H/blaude"
[ -f "$H/clerk.env" ] && {{ mv "$H/clerk.env" "$H/.jcode/clerk.env"; chmod 600 "$H/.jcode/clerk.env"; }}
# GitHub CLI: Connect GitHub (device flow) needs gh on the runtime.
if ! command -v gh >/dev/null; then
  curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg | sudo dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg 2>/dev/null
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" | sudo tee /etc/apt/sources.list.d/github-cli.list >/dev/null
  sudo apt-get update -q >/dev/null 2>&1 || true
  sudo apt-get install -y -q gh >/dev/null 2>&1 || true
fi
# TLS: a REAL Let's Encrypt cert on the VM's sslip.io name, so members join
# with zero CA files and zero browser warnings. Self-signed only as fallback
# (LE outage/rate limit) — the job then returns the CA for pinning.
DOMAIN="{domain}"
sudo apt-get update -q >/dev/null 2>&1 || true
sudo apt-get install -y -q certbot >/dev/null 2>&1 || true
TLS_MODE=selfsigned
if sudo certbot certonly --standalone -d "$DOMAIN" --non-interactive --agree-tos --register-unsafely-without-email >/dev/null 2>&1; then
  sudo install -o "$U" -g "$U" -m 600 "/etc/letsencrypt/live/$DOMAIN/fullchain.pem" "$H/.jcode/tls/cert.pem"
  sudo install -o "$U" -g "$U" -m 600 "/etc/letsencrypt/live/$DOMAIN/privkey.pem" "$H/.jcode/tls/key.pem"
  sudo mkdir -p /etc/letsencrypt/renewal-hooks/deploy
  sudo tee /etc/letsencrypt/renewal-hooks/deploy/blaude.sh >/dev/null <<HOOK
#!/bin/bash
install -o $U -g $U -m 600 "/etc/letsencrypt/live/$DOMAIN/fullchain.pem" "$H/.jcode/tls/cert.pem"
install -o $U -g $U -m 600 "/etc/letsencrypt/live/$DOMAIN/privkey.pem" "$H/.jcode/tls/key.pem"
systemctl restart blaude-bridge
HOOK
  sudo chmod +x /etc/letsencrypt/renewal-hooks/deploy/blaude.sh
  TLS_MODE=letsencrypt
else
  openssl req -x509 -newkey rsa:2048 -keyout "$H/.jcode/tls/key.pem" -out "$H/.jcode/tls/cert.pem" \
    -days 3650 -nodes -subj "/CN=blaude-team" \
    -addext "subjectAltName=IP:{ip},DNS:$DOMAIN" \
    -addext "extendedKeyUsage=serverAuth" \
    -addext "keyUsage=digitalSignature,keyEncipherment" 2>/dev/null
fi
echo "$TLS_MODE" > "$H/.jcode/tls-mode"
# Commits made by team agents need an identity; the owner can refine later.
git config --global user.name "blaude ({name})" 2>/dev/null || true
git config --global user.email "blaude-team@users.noreply.github.com" 2>/dev/null || true
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
Environment=JCODE_API_WS_PORT=443
Environment=JCODE_API_WS_TLS_CERT=$H/.jcode/tls/cert.pem
Environment=JCODE_API_WS_TLS_KEY=$H/.jcode/tls/key.pem
AmbientCapabilities=CAP_NET_BIND_SERVICE
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
            &zone,
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
        if tokio::net::TcpStream::connect((ip.as_str(), 443u16)).await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if !up {
        return Err(format!(
            "The server built but port 443 never answered at {ip}. Check the VM's logs."
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
            &zone,
            "--command",
            "cat ~/.jcode/api-ws-token && echo __MODE__ && cat ~/.jcode/tls-mode && echo __CA__ && cat ~/.jcode/tls/cert.pem",
            "--quiet",
        ],
        None,
    )
    .await
    .map_err(|e| format!("Could not read the server's access token: {e}"))?;
    let mut parts = creds.splitn(2, "__MODE__");
    let token = parts.next().unwrap_or_default().trim().to_string();
    let rest = parts.next().unwrap_or_default();
    let mut rest = rest.splitn(2, "__CA__");
    let tls_mode = rest.next().unwrap_or_default().trim().to_string();
    // A real certificate needs no pinning; return the CA only for the
    // self-signed fallback so members' transports pin it.
    let ca_pem = if tls_mode == "letsencrypt" {
        String::new()
    } else {
        rest.next().unwrap_or_default().trim().to_string()
    };
    // letsencrypt needs no pinning CA, so an empty ca_pem is the SUCCESS
    // shape there — only the self-signed fallback must hand one back.
    let ca_missing = tls_mode != "letsencrypt" && !ca_pem.contains("BEGIN CERTIFICATE");
    if token.is_empty() || ca_missing {
        return Err("The server is up but its credentials could not be read.".into());
    }

    finish_ok(
        &job_id,
        &format!("wss://{domain}:443/api"),
        &token,
        &ca_pem,
    );
    Ok(())
}
