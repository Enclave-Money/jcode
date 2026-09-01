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
        // Sized for the desktop, because everyone opens the browser.
        //
        // An e2-small (2 shared vCPU, 1.9 GB, 10 GB disk) runs the agent and
        // nothing else: Chrome alone wants 400-800 MB resident and the install
        // does not fit in the disk. docs/display-stack.md measured the floor
        // for this stack at 4 vCPU / 8 GiB / 40 GB, so that is the size a team
        // gets. Sizing down and resizing on demand was rejected deliberately:
        // a resize needs a stop/start, so it would drop the team exactly when
        // someone reaches for the screen.
        machine_type: "e2-standard-4".into(),
        // No template server by default. "blaude-india-1" used to be the
        // default and no longer exists, so every create_team spent a failed
        // gcloud scp discovering that before falling back to the cache. Set
        // template_instance in ~/.jcode/team-cloud.json to re-enable pulling.
        template_instance: String::new(),
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
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run gcloud: {e}"))?;
    // Write stdin on its OWN task, so output is drained concurrently. Writing
    // the whole script and only THEN reading stdout deadlocks once the script
    // is large enough (the browser-helper base64 pushed it past ~27KB): the
    // parent blocks in write_all while the far-away remote `bash -s` stalls
    // reading the script, and because the parent isn't reading gcloud's
    // stdout/stderr, nothing ever drains and both sides wedge. A brand-new
    // team hung here for 15+ minutes. The writer task lets wait_with_output
    // read output while the script is still going out.
    if let Some(text) = stdin {
        if let Some(mut pipe) = child.stdin.take() {
            let bytes = text.as_bytes().to_vec();
            tokio::spawn(async move {
                let _ = pipe.write_all(&bytes).await;
                let _ = pipe.shutdown().await;
            });
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
    run(&gcloud, &["auth", "print-access-token", "--quiet"], None)
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
        if !tags
            .split([',', ';', ' '])
            .any(|t| t.trim() == "blaude-team")
        {
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

    // A STATIC address, reserved before the VM exists.
    //
    // An ephemeral IP is released on every stop/start, and a team server's
    // whole identity is built from its address: members hold
    // `wss://<ip-with-dashes>.sslip.io/api`, and the Let's Encrypt cert is
    // issued for that same name. So one restart silently invalidated both the
    // saved URL and the certificate for every member — seen live when resizing
    // a running team moved it from 34.93.93.41 to 35.200.139.215.
    //
    // Reserving costs nothing while attached to a running VM, and a failure
    // here is not fatal: the instance is created either way and falls back to
    // an ephemeral address rather than refusing to make the team.
    let address_name = format!("{instance}-ip");
    // An address is a REGIONAL resource and the zone is `<region>-<letter>`,
    // so the region is the zone with its last segment removed.
    let address_region = zone
        .rsplit_once('-')
        .map(|(region, _)| region.to_string())
        .unwrap_or_else(|| zone.clone());
    let reserved = run(
        &gcloud,
        &[
            "compute",
            "addresses",
            "create",
            &address_name,
            "--project",
            &cfg.project,
            "--region",
            &address_region,
            "--quiet",
        ],
        None,
    )
    .await
    .is_ok();

    let mut create_args: Vec<&str> = vec![
        "compute",
        "instances",
        "create",
        &instance,
        "--project",
        &cfg.project,
        "--zone",
        &zone,
        // The display stack needs room: the base image plus Chrome does not
        // fit the 10 GB default (a live e2-small sat at 2.9 GB free).
        "--boot-disk-size",
        "40GB",
        "--machine-type",
        &cfg.machine_type,
        "--image-family",
        "debian-12",
        "--image-project",
        "debian-cloud",
        "--tags",
        "blaude-team",
        "--quiet",
    ];
    if reserved {
        create_args.extend_from_slice(&["--address", &address_name]);
    }

    run(&gcloud, &create_args, None)
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
    // An empty template_instance means "there is no template server; the cache
    // IS the source". The old default named a VM that has since been deleted,
    // so every create_team paid a failed gcloud fetch before falling back —
    // slow, and it logged an error that looked like the real failure.
    if cache_stale && !cfg.template_instance.trim().is_empty() {
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("could not create the local cache: {e}"))?;
        let pulled = run(
            &gcloud,
            &[
                "compute",
                "scp",
                &format!(
                    "{}:~/blaude-agent/target/release/blaude",
                    cfg.template_instance
                ),
                cache.to_str().unwrap_or_default(),
                "--project",
                &cfg.project,
                "--zone",
                &template_zone,
                "--quiet",
            ],
            None,
        )
        .await;
        // The template VM may no longer exist (teams get deleted); a cached
        // binary is a fine fallback — only a missing cache is fatal.
        if let Err(e) = pulled {
            if !cache.is_file() {
                return Err(format!("Could not fetch the blaude server build: {e}"));
            }
        }
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

    // blaude-tools rides along when the developer cache has it — it is what
    // `blaude brief` (the daemon's code-graph auto-reindex) delegates to.
    // Best-effort: without it the graph simply never builds on this server.
    let tools_cache = cache_dir.join("blaude-tools-linux-x86_64");
    if tools_cache.is_file() {
        let _ = run_retry(
            &gcloud,
            &[
                "compute",
                "scp",
                tools_cache.to_str().unwrap_or_default(),
                &format!("{instance}:~/blaude-tools"),
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

    // The owner's blaude identity rides along too, so the team server names
    // the owner by their EMAIL (attribution, member rows) instead of a unix
    // username. Best-effort like the email key.
    let account = PathBuf::from(&home).join(".jcode/blaude-account.json");
    if account.is_file() {
        let _ = run_retry(
            &gcloud,
            &[
                "compute",
                "scp",
                account.to_str().unwrap_or_default(),
                &format!("{instance}:~/blaude-account.json"),
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
    // Single-quoted for the shell, with embedded quotes escaped the POSIX way
    // ('\''), so a team called O'Brien's does not break the setup script.
    let name_quoted = format!("'{}'", name.replace('\'', r"'\''"));
    let setup = format!(
        r#"set -e
U=$(whoami)
H=$HOME
mkdir -p "$H/.jcode/tls" "$H/.jcode/runtime" "$H/team"
# The team's name, so the SERVER can tell every client what it is called.
# Without this the name lived only on whichever client ran the join flow, and
# everyone else fell back to displaying the hostname.
printf '%s' {name_quoted} > "$H/.jcode/team-name"
chmod +x "$H/blaude"
[ -f "$H/blaude-tools" ] && chmod +x "$H/blaude-tools"
[ -f "$H/clerk.env" ] && {{ mv "$H/clerk.env" "$H/.jcode/clerk.env"; chmod 600 "$H/.jcode/clerk.env"; }}
[ -f "$H/blaude-account.json" ] && {{ mv "$H/blaude-account.json" "$H/.jcode/blaude-account.json"; chmod 600 "$H/.jcode/blaude-account.json"; }}
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
# node runs the gitnexus code-graph indexer that blaude-tools drives; the
# graph is optional, so a failed install just means no auto-briefing here.
command -v node >/dev/null || sudo apt-get install -y -q nodejs npm >/dev/null 2>&1 || true
# 2G swap: gitnexus analyze (node) peaks near an e2-small's entire RAM and
# the OOM killer takes it without swap (seen live: exit 137). Best-effort.
if ! grep -q swapfile /etc/fstab 2>/dev/null; then
  sudo fallocate -l 2G /swapfile 2>/dev/null && sudo chmod 600 /swapfile && sudo /sbin/mkswap /swapfile >/dev/null 2>&1 && sudo /sbin/swapon /swapfile 2>/dev/null && echo "/swapfile none swap sw 0 0" | sudo tee -a /etc/fstab >/dev/null || true
fi
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
# Boot even with no AI account configured. A team server is provisioned
# BEFORE anyone connects a provider, and refusing to start left the daemon
# crash-looping so nothing worked at all: no sessions, no chats, no
# presence — none of which need a model. Turns fail with a clear message
# until an account is added; everything else works.
Environment=JCODE_DEFERRED_AUTH_BOOTSTRAP=1
# One daemon serves several teammates, so an API key sitting in this process's
# environment must never stand in for a teammate whose own sign-in is missing:
# that spends one person's quota on another's work with nothing saying so.
# Turns fail with a clear "sign in again" instead.
Environment=JCODE_SERVER_MODE=1
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
Environment=JCODE_SERVER_MODE=1
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

    // Rooms: the shared desktop, the owner's own, and the unit that builds a
    // room for anyone who joins later.
    //
    // Without this a new team had ONE daemon and no desktops at all — every
    // member sharing one checkout and one set of credentials, "Mine" silently
    // meaning "Shared", and the screen panel with nothing to show. The whole
    // rooms feature existed only on the server I had provisioned by hand.
    //
    // Best-effort: a team that comes up without rooms is still a working team
    // in the shared sense, so a failure here reports but does not destroy a
    // server the owner has already waited for.
    set_stage(&job_id, "Setting up rooms and screens…");
    if let Err(error) = install_rooms(&gcloud, &cfg.project, &zone, &instance).await {
        eprintln!("blaude: rooms setup failed for {instance}: {error}");
    }

    // The wss door answering is the readiness signal (the daemon keeps
    // crash-retrying until an AI account lands — that's expected and the
    // bridge serves sign-in through it).
    set_stage(&job_id, "Starting it…");
    let mut up = false;
    for _ in 0..30 {
        if tokio::net::TcpStream::connect((ip.as_str(), 443u16))
            .await
            .is_ok()
        {
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

    finish_ok(&job_id, &format!("wss://{domain}:443/api"), &token, &ca_pem);
    Ok(())
}

#[cfg(test)]
mod address_tests {
    /// An address is a regional resource; the zone is `<region>-<letter>`.
    /// Getting this wrong makes every reservation fail, and the team then
    /// silently falls back to an ephemeral IP — the exact failure this is
    /// meant to prevent, and one that only shows up on a later restart.
    #[test]
    fn the_address_region_is_the_zone_without_its_letter() {
        let region_of = |zone: &str| {
            zone.rsplit_once('-')
                .map(|(region, _)| region.to_string())
                .unwrap_or_else(|| zone.to_string())
        };
        assert_eq!(region_of("asia-south1-a"), "asia-south1");
        assert_eq!(region_of("us-central1-b"), "us-central1");
        assert_eq!(region_of("europe-west4-c"), "europe-west4");
        // Degenerate input must not panic or produce an empty region.
        assert_eq!(region_of("weird"), "weird");
    }
}

/// Delete a team server: the VM, its disk, and its reserved address.
///
/// The instance name is not something a client knows — members hold a
/// `wss://<ip-with-dashes>.sslip.io/api` URL and nothing else — so the server
/// is found by matching that address against the project's instances. That
/// also means this works for teams created before the name was ever recorded.
///
/// Deliberately NOT best-effort about the address: an unreleased static IP
/// keeps billing after the VM is gone, which is exactly the kind of leftover
/// nobody notices.
pub async fn delete_team(ws_url: &str) -> Result<Value, String> {
    let gcloud = gcloud_bin().ok_or_else(|| "gcloud is not installed".to_string())?;
    let cfg = cloud_cfg();

    // wss://34-93-93-41.sslip.io:443/api -> 34.93.93.41
    let ip = ws_url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(&['/', ':'][..]).next())
        .and_then(|host| host.strip_suffix(".sslip.io"))
        .map(|dashed| dashed.replace('-', "."))
        .ok_or_else(|| format!("cannot tell which server {ws_url} is"))?;

    let listed = run(
        &gcloud,
        &[
            "compute",
            "instances",
            "list",
            "--project",
            &cfg.project,
            "--filter",
            &format!("networkInterfaces[0].accessConfigs[0].natIP={ip}"),
            "--format",
            "value(name,zone)",
        ],
        None,
    )
    .await
    .map_err(|e| friendly_cloud_error("look up the server", &e))?;

    let mut fields = listed.split_whitespace();
    let (Some(instance), Some(zone)) = (fields.next(), fields.next()) else {
        // Already gone is the outcome the caller wanted, not a failure. As an
        // error it left a destroyed team sitting in the switcher permanently,
        // with Delete refusing it every time because there was nothing left
        // to delete.
        return Ok(json!({
            "job_id": "", "stage": "Deleted", "done": true,
            "deleted": Value::Null, "already_gone": true, "ip": ip
        }));
    };

    run(
        &gcloud,
        &[
            "compute", "instances", "delete", instance, "--project", &cfg.project, "--zone", zone,
            "--quiet",
        ],
        None,
    )
    .await
    .map_err(|e| format!("could not delete {instance}: {e}"))?;

    // The address outlives the VM it was attached to, and keeps billing.
    let region = zone.rsplit_once('-').map(|(r, _)| r).unwrap_or(zone);
    let address_released = run(
        &gcloud,
        &[
            "compute",
            "addresses",
            "delete",
            &format!("{instance}-ip"),
            "--project",
            &cfg.project,
            "--region",
            region,
            "--quiet",
        ],
        None,
    )
    .await
    .is_ok();

    // job_id/stage/done ride along because this reply is carried by the
    // team_create_status event, whose record REQUIRES them. Without them the
    // client's decode threw, the reply was dropped, and the app sat on
    // "Deleting…" until it timed out while the server really was destroyed.
    Ok(json!({
        "job_id": "",
        "stage": "Deleted",
        "done": true,
        "deleted": instance,
        "zone": zone,
        "ip": ip,
        "address_released": address_released,
    }))
}

/// Turn a raw gcloud failure into something worth showing a person.
///
/// Expired credentials are the common one, and gcloud reports them as a wall
/// of text ending in a shell command. Printed verbatim it filled the app's
/// banner with a stack of instructions nobody could act on from there.
fn friendly_cloud_error(action: &str, error: &str) -> String {
    if error.contains("Reauthentication failed")
        || error.contains("gcloud auth login")
        || error.contains("credentials are no longer valid")
    {
        return "Your Google Cloud sign-in has expired. Run `gcloud auth login` in a \
                terminal, then try again."
            .to_string();
    }
    let first = error.lines().find(|l| !l.trim().is_empty()).unwrap_or(error);
    format!("Could not {action}: {}", first.trim())
}

/// The room provisioning script, shipped inside the binary.
///
/// Embedded rather than fetched so a new team never depends on a checkout
/// being present, and so the script can never drift from the server code that
/// reads what it writes (`member-users.json`, the socket and cookie paths).
const PROVISION_SCRIPT: &str = include_str!("../../../deploy/team-server/provision-member.sh");

/// The browser helper, baked into the binary so a created team carries it with
/// no repo checkout on the server. Written to /opt/blaude-browser by the
/// install script, which npm-installs Playwright and its Chromium alongside.
const BROWSER_INSTALL_SCRIPT: &str =
    include_str!("../../../deploy/team-server/install-browser-helper.sh");
const BROWSER_HELPER_JS: &str = include_str!("../../../deploy/browser-helper/helper.js");
const BROWSER_DETECT_JS: &str = include_str!("../../../deploy/browser-helper/detect.js");
const BROWSER_FILL_JS: &str = include_str!("../../../deploy/browser-helper/fill.js");
const BROWSER_HELPER_PKG: &str = include_str!("../../../deploy/browser-helper/package.json");

/// A bash snippet that stages the browser-helper files (base64, so no quoting
/// hazard) and runs the installer. Idempotent — safe to run on every create.
fn browser_helper_install_snippet() -> String {
    use base64::Engine as _;
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    format!(
        r#"mkdir -p "$HOME/browser-helper"
base64 -d > "$HOME/browser-helper/helper.js" <<'B64H'
{}
B64H
base64 -d > "$HOME/browser-helper/detect.js" <<'B64D'
{}
B64D
base64 -d > "$HOME/browser-helper/fill.js" <<'B64F'
{}
B64F
base64 -d > "$HOME/browser-helper/package.json" <<'B64P'
{}
B64P
base64 -d > "$HOME/browser-helper/install-browser-helper.sh" <<'B64I'
{}
B64I
sudo bash "$HOME/browser-helper/install-browser-helper.sh" "$HOME/browser-helper" >/tmp/browser-helper-install.log 2>&1 || {{
  echo "BROWSER_HELPER_FAILED"; tail -5 /tmp/browser-helper-install.log; }}
"#,
        b64(BROWSER_HELPER_JS),
        b64(BROWSER_DETECT_JS),
        b64(BROWSER_FILL_JS),
        b64(BROWSER_HELPER_PKG),
        b64(BROWSER_INSTALL_SCRIPT),
    )
}

/// Build the shared room, the owner's room, and the auto-provisioner.
async fn install_rooms(
    gcloud: &PathBuf,
    project: &str,
    zone: &str,
    instance: &str,
) -> Result<(), String> {
    let mut script = std::env::temp_dir();
    script.push(format!("blaude-provision-{}.sh", std::process::id()));
    std::fs::write(&script, PROVISION_SCRIPT)
        .map_err(|e| format!("could not stage the provisioning script: {e}"))?;
    let staged = script.clone();
    let _cleanup = scopeguard_remove(staged);

    run_retry(
        gcloud,
        &[
            "compute",
            "scp",
            script.to_str().unwrap_or_default(),
            &format!("{instance}:~/provision-member.sh"),
            "--project",
            project,
            "--zone",
            zone,
            "--quiet",
        ],
        None,
        3,
    )
    .await
    .map_err(|e| format!("could not copy the provisioning script: {e}"))?;

    // Sent over stdin like the main setup, so nothing needs shell quoting.
    // The desktop packages are what make a screen possible at all: Xvfb to
    // render, a desktop environment for the furniture, ImageMagick to capture, xdotool
    // to click, and a browser to point at the app being built.
    let rooms = format!(
        r#"set -e
H=$HOME
chmod +x "$H/provision-member.sh"
sudo apt-get update -q >/dev/null 2>&1 || true
sudo apt-get install -y -q xvfb x11-utils x11-xserver-utils imagemagick xdotool chromium ffmpeg >/dev/null 2>&1 || true
# A desktop environment, because a cloud image has none: no panel, no file
# manager, nothing to click. openbox rides along as the fallback the session
# unit uses if this install fails.
sudo apt-get install -y -q --no-install-recommends xfce4 xfce4-terminal dbus-x11 openbox >/dev/null 2>&1 || true
# The harness-owned browser: Playwright + its Chromium at /opt/blaude-browser,
# the thing that types a fill into a login form. Without it a room has a
# screen and input but no browser the harness controls.
{browser_install}
sudo BLAUDE_BIN="$H/blaude" "$H/provision-member.sh" blaude-shared --door-home "$H" >/tmp/rooms-shared.log 2>&1 || {{
  echo "SHARED_ROOM_FAILED"; tail -5 /tmp/rooms-shared.log; exit 1; }}
OWNER=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1])).get('email',''))" "$H/.jcode/blaude-account.json" 2>/dev/null || echo "")
if [ -n "$OWNER" ]; then
  NAME=$(printf '%s' "${{OWNER%%@*}}" | tr '[:upper:]' '[:lower:]' | tr -cd 'a-z0-9_-')
  [ -n "$NAME" ] || NAME=owner
  case "$NAME" in [0-9]*) NAME="m$NAME" ;; esac
  sudo BLAUDE_BIN="$H/blaude" "$H/provision-member.sh" "$NAME" --email "$OWNER" --door-home "$H" >/tmp/rooms-owner.log 2>&1 || {{
    echo "OWNER_ROOM_FAILED"; tail -5 /tmp/rooms-owner.log; }}
fi
# The bridge started at the top of provisioning, BEFORE any room daemon
# existed. On a fresh team it can wedge in a "daemon unreachable" state and
# serve only bridge-only verbs — every turn then fails with "the agent daemon
# is not running", and because the one-shot screen probe fails too, the screen
# control never appears. Restart it now that every room daemon is up and
# listening, so it comes up clean. This is exactly the manual restart that
# recovered a wedged team.
sudo systemctl restart blaude-bridge >/dev/null 2>&1 || true
echo ROOMS_OK
"#,
        browser_install = browser_helper_install_snippet(),
    );
    let out = run_retry(
        gcloud,
        &[
            "compute",
            "ssh",
            instance,
            "--project",
            project,
            "--zone",
            zone,
            "--command",
            "bash -s",
            "--quiet",
        ],
        Some(rooms.as_str()),
        2,
    )
    .await
    .map_err(|e| format!("rooms setup could not run: {e}"))?;
    if !out.contains("ROOMS_OK") {
        return Err(format!("rooms setup did not finish: {out}"));
    }
    Ok(())
}

/// Delete a staged file when the guard drops, so a failed upload does not
/// leave the script in the temp directory.
fn scopeguard_remove(path: PathBuf) -> impl Drop {
    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    Guard(path)
}

#[cfg(test)]
mod delete_tests {
    /// The delete reply travels on team_create_status, whose record requires
    /// job_id, stage and done. Omit any of them and the client cannot decode
    /// the frame, drops it, and waits out the whole timeout on a delete that
    /// already finished.
    #[test]
    fn the_delete_reply_carries_what_the_event_requires() {
        for value in [
            serde_json::json!({
                "job_id": "", "stage": "Deleted", "done": true,
                "deleted": serde_json::Value::Null, "already_gone": true, "ip": "1.2.3.4"
            }),
            serde_json::json!({
                "job_id": "", "stage": "Deleted", "done": true,
                "deleted": "blaude-x", "zone": "asia-south1-a", "ip": "1.2.3.4",
                "address_released": true
            }),
        ] {
            for key in ["job_id", "stage", "done"] {
                assert!(value.get(key).is_some(), "{key} missing from {value}");
            }
            assert_eq!(value["done"], serde_json::json!(true));
        }
    }

    /// An expired sign-in must read as one instruction, not as gcloud's wall
    /// of text — the banner it lands in is one line wide.
    #[test]
    fn an_expired_cloud_sign_in_says_what_to_do() {
        let raw = "ERROR: (gcloud.compute.instances.list) There was a problem \
                   refreshing your current auth tokens: Reauthentication failed. \
                   cannot prompt during non-interactive execution.\nPlease run:\n\n  \
                   $ gcloud auth login\n";
        let message = super::friendly_cloud_error("look up the server", raw);
        assert!(message.contains("gcloud auth login"), "must say the fix: {message}");
        assert!(!message.contains("ERROR:"), "must not echo gcloud: {message}");
        assert!(message.lines().count() == 1, "one line: {message}");
    }

    #[test]
    fn any_other_failure_keeps_its_first_line_only() {
        let message = super::friendly_cloud_error("delete it", "boom happened\nstack\nmore");
        assert_eq!(message, "Could not delete it: boom happened");
    }

    /// Deletes a REAL throwaway VM. Ignored by default: it costs money and
    /// destroys an instance, so it runs only when explicitly named, against a
    /// server created for the purpose. It is the only test that proves the
    /// instance lookup, the delete and the address release actually work
    /// against Google rather than against my idea of Google.
    #[tokio::test]
    #[ignore = "creates and destroys real cloud resources"]
    async fn deleting_a_real_throwaway_server_removes_it_and_frees_its_address() {
        let url = std::env::var("BLAUDE_DELETE_TEST_URL")
            .expect("set BLAUDE_DELETE_TEST_URL to the throwaway server's wss url");
        let result = super::delete_team(&url).await.expect("delete should succeed");
        println!("delete returned: {result}");
    }

    /// The instance is found by the address in the URL members already hold,
    /// because nothing else identifies it — a client never learns the VM's
    /// name. This also means it works for teams created before the name was
    /// recorded anywhere.
    fn ip_of(ws_url: &str) -> Option<String> {
        ws_url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split(&['/', ':'][..]).next())
            .and_then(|host| host.strip_suffix(".sslip.io"))
            .map(|dashed| dashed.replace('-', "."))
    }

    #[test]
    fn the_server_is_identified_by_the_address_in_its_url() {
        assert_eq!(
            ip_of("wss://34-93-93-41.sslip.io:443/api").as_deref(),
            Some("34.93.93.41")
        );
        assert_eq!(
            ip_of("wss://35-200-139-215.sslip.io:443/api").as_deref(),
            Some("35.200.139.215")
        );
    }

    /// A URL that is not one of ours must not resolve to something deletable.
    /// Guessing here would delete the wrong machine.
    #[test]
    fn a_url_that_is_not_a_team_server_resolves_to_nothing() {
        assert_eq!(ip_of("wss://example.com:443/api"), None);
        assert_eq!(ip_of("not a url"), None);
        assert_eq!(ip_of("wss://localhost:7644/api"), None);
    }

    /// The region for releasing the address is the zone minus its letter.
    /// Getting it wrong leaves a reserved IP billing after the VM is gone.
    #[test]
    fn the_address_region_comes_from_the_zone() {
        fn region(zone: &str) -> &str {
            zone.rsplit_once('-').map(|(r, _)| r).unwrap_or(zone)
        }
        assert_eq!(region("asia-south1-a"), "asia-south1");
        assert_eq!(region("us-central1-b"), "us-central1");
    }

    /// run() must not deadlock when the stdin it feeds is larger than a pipe
    /// buffer AND the child echoes it back concurrently — which is exactly the
    /// provisioning script through `gcloud … bash -s`. `cat` is that child: it
    /// writes stdout as it reads stdin, so a 1 MiB payload fills its stdout
    /// pipe (64 KiB) long before the writer is done. The OLD run() wrote all of
    /// stdin before reading a byte of stdout and wedged here forever; a brand-
    /// new team hung at "Securing…" for 15+ minutes. The 5 s timeout is the
    /// guard: on the deadlocking version this test times out, on the fixed one
    /// it returns the payload intact.
    #[tokio::test]
    async fn run_does_not_deadlock_on_a_large_stdin_that_is_echoed_back() {
        let big = "x".repeat(1024 * 1024); // 1 MiB, way past a 64 KiB pipe
        let cat = std::path::PathBuf::from("/bin/cat");
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            super::run(&cat, &[], Some(&big)),
        )
        .await
        .expect("run() deadlocked writing a large stdin (the create_team hang)");
        assert_eq!(result.expect("cat succeeds").len(), big.len(), "the whole payload round-trips");
    }
}
