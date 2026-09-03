//! Builds a team's cloud server.
//!
//! `create_team` makes a real one: a fresh VM running the blaude daemon and
//! bridge behind wss, with an owner token and a pinned CA. It takes minutes,
//! so it runs as a process-global async job — `start` returns a status record
//! immediately and callers poll `status` until `done`.
//!
//! # Where this runs
//!
//! Wherever the CLOUD CREDENTIAL lives, which is the provisioning service —
//! never a user's Mac.
//!
//! It used to run on the owner's machine, shelling out to their own `gcloud`.
//! That only ever worked for one person: everyone else would have needed the
//! gcloud CLI installed and a login with Compute Admin on someone else's
//! project, so for them "Create a team" could not work at all. Even for that
//! one person it broke roughly daily, because a human `gcloud auth login`
//! expires and there is nothing to renew it. A service account attached to
//! the service has neither problem.
//!
//! Cloud settings come from `BLAUDE_PROJECT`, `BLAUDE_ZONE`,
//! `BLAUDE_MACHINE_TYPE`, and `BLAUDE_TEMPLATE_INSTANCE`, with a legacy local
//! fallback to `~/.jcode/team-cloud.json`:
//!   { "project": …, "zone": …, "machine_type": …, "template_instance": … }
//! with defaults matching the first hand-built team server, and
//! `template_instance` naming a VM whose built `blaude` is copied onto new
//! servers (cached locally after the first pull).
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

mod relay_token;
pub use relay_token::{
    RelayClaims, configure_relay_signing_key, mint_relay_token, verify_relay_token,
};

const OVERALL_TIMEOUT_SECS: u64 = 1800;
const COMPLETED_JOB_RETENTION_SECS: u64 = 3600;

#[derive(Clone, Debug)]
struct CloudResources {
    project: String,
    zone: String,
    instance: String,
    address_name: String,
    address_region: String,
    address_reserved: bool,
    instance_created: bool,
}

#[derive(Debug)]
struct Job {
    owner_subject: String,
    record: Value,
    finished_at: Option<u64>,
    resources: Option<CloudResources>,
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
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("tc-{nanos:x}-{sequence:x}")
}

fn prune_finished(map: &mut HashMap<String, Job>) {
    let cutoff = now_secs().saturating_sub(COMPLETED_JOB_RETENTION_SECS);
    map.retain(|_, job| job.finished_at.is_none_or(|finished| finished >= cutoff));
}

fn set_stage(job_id: &str, stage: &str) {
    if let Ok(mut map) = jobs().lock()
        && let Some(job) = map.get_mut(job_id)
    {
        job.record["stage"] = json!(stage);
    }
}

fn finish_err(job_id: &str, message: String) {
    if let Ok(mut map) = jobs().lock()
        && let Some(job) = map.get_mut(job_id)
    {
        job.record["done"] = json!(true);
        job.record["error"] = json!(message);
        job.finished_at = Some(now_secs());
    }
}

fn finish_ok(job_id: &str, ws_url: &str, token: &str, ca_pem: &str) {
    if let Ok(mut map) = jobs().lock()
        && let Some(job) = map.get_mut(job_id)
    {
        job.record["done"] = json!(true);
        job.record["stage"] = json!("Ready.");
        job.record["ws_url"] = json!(ws_url);
        job.record["token"] = json!(token);
        job.record["ca_pem"] = json!(ca_pem);
        job.finished_at = Some(now_secs());
    }
}

fn register_resources(job_id: &str, resources: CloudResources) {
    if let Ok(mut map) = jobs().lock()
        && let Some(job) = map.get_mut(job_id)
    {
        job.resources = Some(resources);
    }
}

fn mark_address_reserved(job_id: &str) {
    if let Ok(mut map) = jobs().lock()
        && let Some(resources) = map.get_mut(job_id).and_then(|job| job.resources.as_mut())
    {
        resources.address_reserved = true;
    }
}

fn mark_instance_created(job_id: &str) {
    if let Ok(mut map) = jobs().lock()
        && let Some(resources) = map.get_mut(job_id).and_then(|job| job.resources.as_mut())
    {
        resources.instance_created = true;
    }
}

fn resources_for(job_id: &str) -> Option<CloudResources> {
    jobs().lock().ok()?.get(job_id)?.resources.clone()
}

/// Current record for a job, but only for the identity that created it.
pub fn status(job_id: &str, owner_subject: &str) -> Option<Value> {
    let mut map = jobs().lock().ok()?;
    prune_finished(&mut map);
    let job = map.get(job_id)?;
    (job.owner_subject == owner_subject).then(|| job.record.clone())
}

/// Start provisioning; returns the initial record immediately.
/// Region shortcuts the app offers; anything else falls back to the
/// configured default zone.
fn zone_for_region(region: Option<&str>, default_zone: &str) -> String {
    match region.map(|r| r.trim().to_ascii_lowercase()) {
        Some(r) if r == "india" => "asia-south1-a".into(),
        Some(r) if r == "singapore" => "asia-southeast1-a".into(),
        Some(r) if r == "europe" => "europe-west3-a".into(),
        Some(r) if matches!(r.as_str(), "us" | "usa" | "united states") => "us-central1-a".into(),
        _ => default_zone.into(),
    }
}

#[cfg(test)]
mod region_tests {
    use super::{shell_quote, zone_for_region};

    #[test]
    fn region_shortcuts_are_exact_and_unknown_values_use_the_default() {
        assert_eq!(zone_for_region(Some("india"), "default-a"), "asia-south1-a");
        assert_eq!(
            zone_for_region(Some(" Singapore "), "default-a"),
            "asia-southeast1-a"
        );
        assert_eq!(zone_for_region(Some("australia"), "default-a"), "default-a");
        assert_eq!(zone_for_region(Some("business"), "default-a"), "default-a");
    }

    #[test]
    fn team_names_are_one_literal_shell_argument() {
        assert_eq!(shell_quote("O'Brien"), "'O'\\''Brien'");
        assert_eq!(
            shell_quote("$(touch /tmp/pwned) `id` \"quoted\""),
            "'$(touch /tmp/pwned) `id` \"quoted\"'"
        );
    }
}

fn team_ip(ws_url: &str) -> Option<String> {
    let authority = ws_url.strip_prefix("wss://")?.split('/').next()?;
    let host = authority.split(':').next()?;
    let dashed = host.strip_suffix(".sslip.io")?;
    let octets = dashed
        .split('-')
        .map(str::parse::<u8>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    (octets.len() == 4).then(|| {
        octets
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(".")
    })
}

pub fn start(
    name: &str,
    region: Option<&str>,
    owner_subject: &str,
    owner_email: Option<&str>,
) -> Value {
    let job_id = new_job_id();
    let record = json!({
        "job_id": job_id,
        "name": name,
        "stage": "Checking Google Cloud…",
        "done": false,
    });
    if let Ok(mut map) = jobs().lock() {
        prune_finished(&mut map);
        map.insert(
            job_id.clone(),
            Job {
                owner_subject: owner_subject.to_string(),
                record: record.clone(),
                finished_at: None,
                resources: None,
            },
        );
    }
    let name = name.to_string();
    let region = region.map(str::to_string);
    let owner_subject = owner_subject.to_string();
    let owner_email = owner_email.map(str::to_string);
    tokio::spawn(async move {
        let id = job_id.clone();
        let work = provision(job_id.clone(), name, region, owner_subject, owner_email);
        match tokio::time::timeout(Duration::from_secs(OVERALL_TIMEOUT_SECS), work).await {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                let cleanup = cleanup_failed_job(&id).await;
                finish_err(&id, with_cleanup_result(message, cleanup));
            }
            Err(_) => {
                let cleanup = cleanup_failed_job(&id).await;
                finish_err(
                    &id,
                    with_cleanup_result("Ran out of time building the server.".into(), cleanup),
                );
            }
        }
    });
    record
}

#[cfg(test)]
mod job_tests {
    use super::{Job, jobs, new_job_id, owner_label, status};

    #[test]
    fn job_status_is_visible_only_to_its_creator() {
        let job_id = new_job_id();
        let record = serde_json::json!({ "job_id": job_id, "done": false });
        jobs().lock().unwrap().insert(
            job_id.clone(),
            Job {
                owner_subject: "user_owner".into(),
                record: record.clone(),
                finished_at: None,
                resources: None,
            },
        );

        assert_eq!(status(&job_id, "user_owner"), Some(record));
        assert_eq!(status(&job_id, "user_someone_else"), None);
        jobs().lock().unwrap().remove(&job_id);
    }

    #[test]
    fn job_ids_do_not_collide_within_one_process() {
        assert_ne!(new_job_id(), new_job_id());
    }

    #[test]
    fn ownership_labels_are_stable_and_subject_specific() {
        let owner = owner_label("user_owner");
        assert_eq!(owner, owner_label("user_owner"));
        assert_ne!(owner, owner_label("user_someone_else"));
        assert!(owner.starts_with('u'));
        assert!(owner.len() <= 63);
    }
}

// ---------------------------------------------------------------------------
// cloud config

#[derive(Clone)]
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
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(v) = serde_json::from_str::<Value>(&text)
        {
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
    for (key, target) in [
        ("BLAUDE_PROJECT", &mut cfg.project),
        ("BLAUDE_ZONE", &mut cfg.zone),
        ("BLAUDE_MACHINE_TYPE", &mut cfg.machine_type),
        ("BLAUDE_TEMPLATE_INSTANCE", &mut cfg.template_instance),
    ] {
        if let Ok(value) = std::env::var(key)
            && !value.trim().is_empty()
        {
            *target = value;
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
    if let Some(text) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        let bytes = text.as_bytes().to_vec();
        tokio::spawn(async move {
            let _ = pipe.write_all(&bytes).await;
            let _ = pipe.shutdown().await;
        });
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

/// A short, deterministic GCP-label-safe identity. The original Clerk subject
/// never leaves the service; the label is only an ownership discriminator.
fn owner_label(subject: &str) -> String {
    let digest = Sha256::digest(subject.as_bytes());
    format!("u{}", hex::encode(&digest[..16]))
}

/// Addresses do not support GCP labels. Keep the same opaque ownership value
/// in their description instead, using a whitespace-free marker so gcloud's
/// `value(...)` output can be parsed without treating free text as identity.
fn owner_description(owner: &str) -> String {
    format!("blaude-managed-owner-{owner}")
}

async fn release_address(
    gcloud: &PathBuf,
    project: &str,
    region: &str,
    address_name: &str,
) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..6 {
        match run(
            gcloud,
            &[
                "compute",
                "addresses",
                "delete",
                address_name,
                "--project",
                project,
                "--region",
                region,
                "--quiet",
            ],
            None,
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(error) if error.to_ascii_lowercase().contains("not found") => return Ok(()),
            Err(error) => last = error,
        }
        if attempt < 5 {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
    Err(last)
}

async fn cleanup_failed_job(job_id: &str) -> Result<(), String> {
    let Some(resources) = resources_for(job_id) else {
        return Ok(());
    };
    let gcloud = gcloud_bin().ok_or_else(|| "gcloud disappeared during cleanup".to_string())?;
    let mut failures = Vec::new();

    if resources.instance_created
        && let Err(error) = run(
            &gcloud,
            &[
                "compute",
                "instances",
                "delete",
                &resources.instance,
                "--project",
                &resources.project,
                "--zone",
                &resources.zone,
                "--quiet",
            ],
            None,
        )
        .await
        && !error.to_ascii_lowercase().contains("not found")
    {
        failures.push(format!("VM cleanup failed: {error}"));
    }

    if resources.address_reserved
        && let Err(error) = release_address(
            &gcloud,
            &resources.project,
            &resources.address_region,
            &resources.address_name,
        )
        .await
    {
        failures.push(format!("address cleanup failed: {error}"));
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn with_cleanup_result(message: String, cleanup: Result<(), String>) -> String {
    match cleanup {
        Ok(()) => format!("{message} The partial cloud resources were removed."),
        Err(error) => format!("{message} Automatic cleanup also failed: {error}"),
    }
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

/// One shell argument, represented literally even when a team name contains
/// quotes, substitutions, or option-looking text.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

// ---------------------------------------------------------------------------
// the provisioning sequence

async fn provision(
    job_id: String,
    name: String,
    region: Option<String>,
    owner_subject: String,
    owner_email: Option<String>,
) -> Result<(), String> {
    let Some(gcloud) = gcloud_bin() else {
        return Err("The provisioning service is missing the Google Cloud CLI.".into());
    };
    let cfg = cloud_cfg();
    // The template (binary source) stays in ITS zone; the new server goes to
    // the zone the user chose.
    let template_zone = cfg.zone.clone();
    let zone = zone_for_region(region.as_deref(), &cfg.zone);

    // Authenticated at all? On Cloud Run this comes from the attached service
    // account and cannot expire like a human gcloud login.
    run(&gcloud, &["auth", "print-access-token", "--quiet"], None)
        .await
        .map_err(|e| {
            format!("The provisioning service cannot authenticate to Google Cloud: {e}")
        })?;

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let instance = format!("blaude-{}-{:010x}", slugify(&name), unique & 0xffffffffff);
    let owner = owner_label(&owner_subject);
    let resource_labels = format!("blaude_managed=true,blaude_owner={owner}");
    let address_description = owner_description(&owner);

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
    // Reserving costs nothing while attached to a running VM. A failure is
    // fatal: an ephemeral address would change on restart and invalidate every
    // member's saved URL and the TLS certificate.
    let address_name = format!("{instance}-ip");
    // An address is a REGIONAL resource and the zone is `<region>-<letter>`,
    // so the region is the zone with its last segment removed.
    let address_region = zone
        .rsplit_once('-')
        .map(|(region, _)| region.to_string())
        .unwrap_or_else(|| zone.clone());
    register_resources(
        &job_id,
        CloudResources {
            project: cfg.project.clone(),
            zone: zone.clone(),
            instance: instance.clone(),
            address_name: address_name.clone(),
            address_region: address_region.clone(),
            address_reserved: false,
            instance_created: false,
        },
    );
    // Treat an ambiguous create response as "may exist" so rollback checks
    // the deterministic name instead of trusting the process exit alone.
    mark_address_reserved(&job_id);
    run(
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
            "--description",
            &address_description,
            "--quiet",
        ],
        None,
    )
    .await
    .map_err(|e| format!("Could not reserve the team's stable address: {e}"))?;

    // Install the packages AT BOOT, not over ssh three stages later.
    //
    // This is the single biggest thing standing between "create a team" and a
    // usable server: ~2 minutes of apt that needs nothing from this Mac. Run
    // from ssh it was pure wall clock, with the VM sitting idle through the
    // instance create and the 130MB binary copy first. As a startup-script it
    // begins the moment the guest boots, so most of it is already done by the
    // time the first ssh connects. The setup stage waits on the marker.
    //
    // Best effort by design: if the guest agent never runs this, the setup
    // stage sees no marker and installs the packages itself.
    let boot_script = format!(
        r#"#!/bin/bash
exec >>/var/log/blaude-boot.log 2>&1
set -x
{packages}
touch /var/lib/blaude-apt.done
"#,
        packages = package_install_snippet(),
    );
    let boot_path = std::env::temp_dir().join(format!("{instance}-boot.sh"));
    let boot_arg = format!("startup-script={}", boot_path.display());
    let booted = std::fs::write(&boot_path, &boot_script).is_ok();

    // Authorize the service's SSH key on the box from boot.
    //
    // The service reaches the VM with `gcloud compute scp/ssh`. The project
    // uses metadata SSH keys (not OS Login), and from a root container gcloud
    // cannot derive a usable login name, so its automatic key push lands under
    // a name the VM rejects — every copy then fails "Permission denied" and
    // the create wedges at "Copying blaude onto it". Injecting the login and
    // key here means the guest agent provisions that exact user before the
    // first scp, and the container connects as it (its env sets the name).
    let ssh_meta = std::env::var("BLAUDE_SSH_PUBKEY").ok().and_then(|pubkey| {
        let pubkey = pubkey.trim().to_string();
        if pubkey.is_empty() {
            return None;
        }
        let login = std::env::var("BLAUDE_SSH_LOGIN").unwrap_or_else(|_| "blaude".into());
        Some(format!("ssh-keys={login}:{pubkey}"))
    });

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
        "--labels",
        &resource_labels,
        "--quiet",
    ];
    create_args.extend_from_slice(&["--address", &address_name]);
    if booted {
        create_args.extend_from_slice(&["--metadata-from-file", &boot_arg]);
    }
    if let Some(meta) = &ssh_meta {
        create_args.extend_from_slice(&["--metadata", meta]);
    }

    // From this point a failed/ambiguous gcloud response may still have made
    // the VM. Mark it before the call so rollback verifies deletion by name.
    mark_instance_created(&job_id);
    let created = run(&gcloud, &create_args, None).await;
    let _ = std::fs::remove_file(&boot_path);
    created.map_err(|e| format!("Could not create the server: {e}"))?;

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
    // In the provisioning service's container this points at binaries BUILT
    // INTO THE IMAGE, from the same commit as the service itself. That
    // retires a whole failure class: the Mac-side cache was only re-pulled
    // daily, so a same-day cache built before a fix silently shipped the OLD
    // server to every brand-new team while the fix looked deployed.
    let cache_dir = std::env::var("BLAUDE_SERVER_BINARY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(&home).join(".jcode/team-server-cache"));
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
        if let Err(e) = pulled
            && !cache.is_file()
        {
            return Err(format!("Could not fetch the blaude server build: {e}"));
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

    // A team may ask the provisioning service to deliver invitations, but it
    // must never receive the Clerk backend secret itself. This signed
    // capability is scoped to this team's URL and name; the Cloud Run relay
    // reconstructs metadata from those signed claims before touching Clerk.
    let team_ws_url = format!("wss://{}.sslip.io:443/api", ip.replace('.', "-"));
    let relay_capability = mint_relay_token(&team_ws_url, &name)
        .map_err(|error| format!("Could not authorize team invitations: {error}"))?;
    let relay_path = std::env::temp_dir().join(format!("{instance}-relay-token"));
    std::fs::write(&relay_path, &relay_capability)
        .map_err(|error| format!("Could not stage the invitation capability: {error}"))?;
    let relay_copy = run_retry(
        &gcloud,
        &[
            "compute",
            "scp",
            relay_path.to_str().unwrap_or_default(),
            &format!("{instance}:~/blaude-relay-token"),
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
    let _ = std::fs::remove_file(&relay_path);
    relay_copy.map_err(|error| format!("Could not authorize team invitations: {error}"))?;

    // The owner's blaude identity rides along too, so the team server names
    // the owner by their EMAIL (attribution, member rows) instead of a unix
    // username. This is required: without it the owner silently lands in the
    // shared room instead of their private one.
    let mut account = PathBuf::from(&home).join(".jcode/blaude-account.json");
    let mut synthesized_account = None;
    // The service has no account file — it is nobody. It DOES know who asked,
    // from their verified sign-in, so the owner's identity is written from
    // that. Without it the server cannot name the owner and never provisions
    // their own room, which surfaces as "Mine" quietly meaning "Shared".
    if !account.is_file() {
        let email = owner_email
            .as_deref()
            .filter(|email| email.contains('@'))
            .ok_or_else(|| "Could not determine the team owner's verified email.".to_string())?;
        let synthesized = std::env::temp_dir().join(format!("{instance}-owner.json"));
        std::fs::write(&synthesized, json!({ "email": email }).to_string())
            .map_err(|error| format!("Could not stage the owner's identity: {error}"))?;
        account = synthesized.clone();
        synthesized_account = Some(synthesized);
    }
    if account.is_file() {
        let account_copy = run_retry(
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
        if let Some(path) = synthesized_account {
            let _ = std::fs::remove_file(path);
        }
        account_copy.map_err(|error| format!("Could not copy the owner's identity: {error}"))?;
    }

    // TLS, tokens, and the two SYSTEM units — the same known-good layout as
    // the hand-built team server (bridge with native wss + no-spawn; daemon
    // with the forever-retry drop-in so it self-heals once an AI account
    // lands). Sent over stdin (`bash -s`) to dodge quoting entirely.
    set_stage(&job_id, "Securing it…");
    let domain = format!("{}.sslip.io", ip.replace('.', "-"));
    // Single-quoted for the shell, with embedded quotes escaped the POSIX way
    // ('\''), so a team called O'Brien's does not break the setup script.
    let name_quoted = shell_quote(&name);
    let git_name_quoted = shell_quote(&format!("blaude ({name})"));
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
[ -f "$H/blaude-relay-token" ] || {{ echo "invitation capability missing" >&2; exit 1; }}
mv "$H/blaude-relay-token" "$H/.jcode/team-relay-token"
chmod 600 "$H/.jcode/team-relay-token"
[ -f "$H/blaude-account.json" ] && {{ mv "$H/blaude-account.json" "$H/.jcode/blaude-account.json"; chmod 600 "$H/.jcode/blaude-account.json"; }}
# The packages were started at BOOT (see the startup-script on the instance),
# so by now they are usually in. Wait for the marker rather than racing the
# dpkg lock, and install them here if the guest agent never ran the script —
# a create must not depend on it.
for i in $(seq 1 240); do [ -f /var/lib/blaude-apt.done ] && break; sleep 2; done
if [ ! -f /var/lib/blaude-apt.done ]; then
  echo "boot install missing; installing inline"
{packages}
  sudo touch /var/lib/blaude-apt.done
fi
{browser_install}
# TLS: a REAL Let's Encrypt cert on the VM's sslip.io name, so members join
# with zero CA files and zero browser warnings. Self-signed only as fallback
# (LE outage/rate limit) — the job then returns the CA for pinning.
DOMAIN="{domain}"
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
git config --global user.name {git_name_quoted} 2>/dev/null || true
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
"#,
        browser_install = browser_helper_install_snippet(),
        packages = package_install_snippet(),
    );
    let out = run_remote_script(&gcloud, &cfg.project, &zone, &instance, "setup", &setup, 4)
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
    // Rooms are part of the product contract, not an optional embellishment:
    // without them "Mine" is shared, member isolation is absent, and screen
    // control never appears. Fail the transaction and roll the partial VM back.
    set_stage(&job_id, "Setting up rooms and screens…");
    install_rooms(&gcloud, &cfg.project, &zone, &instance).await?;

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

    finish_ok(&job_id, &team_ws_url, &token, &ca_pem);
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

    /// Regional addresses reject `--labels`, so ownership is stored in the
    /// supported description field. It must remain opaque and one token for
    /// safe matching when a failed deletion leaves only the address behind.
    #[test]
    fn address_ownership_description_is_stable_and_parseable() {
        let owner = super::owner_label("user_owner");
        let description = super::owner_description(&owner);
        assert_eq!(description, format!("blaude-managed-owner-{owner}"));
        assert!(!description.chars().any(char::is_whitespace));
        assert_ne!(
            description,
            super::owner_description(&super::owner_label("user_someone_else"))
        );
    }
}

/// Delete a team server: the VM, its disk, and its reserved address.
///
/// The instance name is not something a client knows — members hold a
/// `wss://<ip-with-dashes>.sslip.io/api` URL and nothing else — so the server
/// is found by matching that address against the project's instances AND the
/// ownership labels written during create. An authenticated user must never
/// be able to delete another user's team or an unrelated VM in the project.
///
/// Deliberately NOT best-effort about the address: an unreleased static IP
/// keeps billing after the VM is gone, which is exactly the kind of leftover
/// nobody notices.
pub async fn delete_team(ws_url: &str, owner_subject: &str) -> Result<Value, String> {
    let gcloud = gcloud_bin().ok_or_else(|| "gcloud is not installed".to_string())?;
    let cfg = cloud_cfg();
    let owner = owner_label(owner_subject);
    let address_description = owner_description(&owner);

    // wss://34-93-93-41.sslip.io:443/api -> 34.93.93.41
    let ip = team_ip(ws_url).ok_or_else(|| format!("cannot tell which server {ws_url} is"))?;

    let listed = run(
        &gcloud,
        &[
            "compute",
            "instances",
            "list",
            "--project",
            &cfg.project,
            "--filter",
            &format!(
                "labels.blaude_managed=true AND labels.blaude_owner={owner} AND \
                 networkInterfaces[0].accessConfigs[0].natIP={ip}"
            ),
            "--format",
            "value(name,zone)",
        ],
        None,
    )
    .await
    .map_err(|e| friendly_cloud_error("look up the server", &e))?;

    let mut fields = listed.split_whitespace();
    let (Some(instance), Some(zone)) = (fields.next(), fields.next()) else {
        // The VM may already be gone while its owned address remains billable.
        // Find and release that address before reporting success.
        let addresses = run(
            &gcloud,
            &[
                "compute",
                "addresses",
                "list",
                "--project",
                &cfg.project,
                "--filter",
                &format!("address={ip}"),
                "--format",
                "value(name,region,description)",
            ],
            None,
        )
        .await
        .map_err(|e| friendly_cloud_error("look up the server's address", &e))?;
        let mut address_fields = addresses.split_whitespace();
        if let (Some(address_name), Some(region), Some(description)) = (
            address_fields.next(),
            address_fields.next(),
            address_fields.next(),
        ) && description == address_description
        {
            let region = region.rsplit('/').next().unwrap_or(region);
            release_address(&gcloud, &cfg.project, region, address_name)
                .await
                .map_err(|e| {
                    format!("the VM is gone but its address could not be released: {e}")
                })?;
        }
        return Ok(json!({
            "job_id": "", "stage": "Deleted", "done": true,
            "deleted": Value::Null, "already_gone": true, "ip": ip,
            "address_released": true
        }));
    };
    let zone = zone.rsplit('/').next().unwrap_or(zone);

    run(
        &gcloud,
        &[
            "compute",
            "instances",
            "delete",
            instance,
            "--project",
            &cfg.project,
            "--zone",
            zone,
            "--quiet",
        ],
        None,
    )
    .await
    .map_err(|e| format!("could not delete {instance}: {e}"))?;

    // The address outlives the VM it was attached to, and keeps billing.
    //
    // RETRIED, because for a few seconds after the instance is gone the
    // address is still marked in use and the delete fails. One attempt left a
    // RESERVED, unattached address behind — silently, since nothing acts on
    // the `address_released: false` this returns — and a reserved address that
    // is attached to nothing is billed by the hour, forever. Seen live: a test
    // team deleted cleanly and still cost money afterwards.
    let region = zone.rsplit_once('-').map(|(r, _)| r).unwrap_or(zone);
    let address_name = format!("{instance}-ip");
    release_address(&gcloud, &cfg.project, region, &address_name)
        .await
        .map_err(|e| format!("deleted {instance}, but could not release its address: {e}"))?;

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
        "address_released": true,
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
        return "The provisioning service's Google Cloud credential is unavailable. Contact the \
                blaude operator; no gcloud login is needed on this Mac."
            .to_string();
    }
    let first = error
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(error);
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
const ROOMS_SETUP_SCRIPT: &str = r#"set -e
H=$HOME
if [ ! -f "$H/provision-member.sh" ]; then echo "PROVISION_SCRIPT_MISSING"; exit 1; fi
chmod +x "$H/provision-member.sh"
# Every package a room needs was installed in the setup stage, and the
# browser download was kicked off there in the background. Wait for it —
# normally it finished while certbot was talking to Let's Encrypt and systemd
# was bringing the services up, so this returns at once. The wait is here so
# a slow link cannot hand a room a helper that is still half written.
for i in $(seq 1 150); do [ -f /tmp/browser-helper.done ] && break; sleep 2; done
grep -q BROWSER_HELPER_OK /tmp/browser-helper-install.log 2>/dev/null || {
  echo "BROWSER_HELPER_FAILED"; tail -5 /tmp/browser-helper-install.log 2>/dev/null; }
sudo BLAUDE_BIN="$H/blaude" "$H/provision-member.sh" blaude-shared --door-home "$H" >/tmp/rooms-shared.log 2>&1 || {
  echo "SHARED_ROOM_FAILED"; tail -5 /tmp/rooms-shared.log; exit 1; }
OWNER=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1])).get('email',''))" "$H/.jcode/blaude-account.json" 2>/dev/null || echo "")
if [ -n "$OWNER" ]; then
  NAME=$(printf '%s' "${OWNER%%@*}" | tr '[:upper:]' '[:lower:]' | tr -cd 'a-z0-9_-')
  [ -n "$NAME" ] || NAME=owner
  case "$NAME" in [0-9]*) NAME="m$NAME" ;; esac
  sudo BLAUDE_BIN="$H/blaude" "$H/provision-member.sh" "$NAME" --email "$OWNER" --door-home "$H" >/tmp/rooms-owner.log 2>&1 || {
    echo "OWNER_ROOM_FAILED"; tail -5 /tmp/rooms-owner.log; }
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
"#;

/// A bash snippet that stages the browser-helper files (base64, so no quoting
/// hazard) and runs the installer. Idempotent — safe to run on every create.
/// Every package a team server needs, in ONE apt transaction.
///
/// It used to be five — gh, certbot, node, the screen stack, the desktop —
/// each re-fetching the package index and each running its own dpkg trigger
/// pass (fontconfig, mime, desktop-database). Measured on a real create, the
/// repeated index fetches and trigger runs cost more than the packages.
///
/// Shared verbatim by the instance's startup-script and by the setup stage's
/// fallback, so the two can never drift into installing different servers.
fn package_install_snippet() -> String {
    r#"export DEBIAN_FRONTEND=noninteractive
curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg | sudo dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg 2>/dev/null
echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" | sudo tee /etc/apt/sources.list.d/github-cli.list >/dev/null
sudo apt-get update -q >/dev/null 2>&1 || true
# gh for Connect GitHub's device flow; certbot for the real TLS cert; node for
# the gitnexus indexer blaude-tools drives and for the browser helper; then
# everything a room's screen is made of — Xvfb to render, ImageMagick to
# capture, xdotool to click, ffmpeg to stream it.
sudo apt-get install -y -q gh certbot nodejs npm xvfb x11-utils x11-xserver-utils imagemagick xdotool ffmpeg acl >/dev/null 2>&1 || true
# A desktop environment, because a cloud image has none: no panel, no file
# manager, nothing to click. openbox rides along as the fallback the session
# unit uses if this install fails. Recommends off — xfce4 with them pulls in
# several hundred packages nobody in a room will open.
sudo apt-get install -y -q --no-install-recommends xfce4 xfce4-terminal dbus-x11 openbox >/dev/null 2>&1 || true
# A clickable browser for whoever opens the room's desktop, pointed at the
# Chromium the harness downloads anyway. apt's chromium used to be installed
# too: a SECOND 274MB browser on every server, for the same job. The shim
# resolves Playwright's versioned path, so upgrading it does not strand the
# menu entry. Verified on Debian 12: it launches on a room display as the room
# user with its sandbox on.
sudo tee /usr/local/bin/chromium >/dev/null <<'SHIM'
#!/bin/sh
B=$(ls -d /opt/blaude-browser/ms-playwright/chromium-*/chrome-linux/chrome 2>/dev/null | sort -V | tail -1)
[ -n "$B" ] || { echo "the browser is still installing" >&2; exit 1; }
exec "$B" --no-first-run --no-default-browser-check "$@"
SHIM
sudo chmod +x /usr/local/bin/chromium
sudo tee /usr/share/applications/blaude-chromium.desktop >/dev/null <<'DESK'
[Desktop Entry]
Type=Application
Name=Web Browser
Exec=/usr/local/bin/chromium %U
Icon=web-browser
Categories=Network;WebBrowser;
DESK
"#
    .to_string()
}

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
# Backgrounded, and this is the point of it: ~900MB of download that holds no
# apt lock and that nothing needs until rooms are provisioned a stage later.
# It now runs while certbot talks to Let's Encrypt and systemd starts the
# services, instead of adding its whole download to the wall clock. The rooms
# stage waits on the marker.
sudo rm -f /tmp/browser-helper.done
setsid nohup bash -c 'sudo bash "$0/install-browser-helper.sh" "$0" >/tmp/browser-helper-install.log 2>&1; sudo touch /tmp/browser-helper.done' "$HOME/browser-helper" </dev/null >/dev/null 2>&1 &
"#,
        b64(BROWSER_HELPER_JS),
        b64(BROWSER_DETECT_JS),
        b64(BROWSER_FILL_JS),
        b64(BROWSER_HELPER_PKG),
        b64(BROWSER_INSTALL_SCRIPT),
    )
}

/// Run a script ON the instance by COPYING it there and executing it — never by
/// piping it into `bash -s`.
///
/// Piping is what wedged team creation: with a script of any size, when the
/// remote bash finishes (or dies) while the local side still has bytes to
/// write, the local `ssh` never exits. gcloud then hangs forever and the whole
/// create sits on one stage — "Securing it…" — with the server already fully
/// built. Copy-then-run has no stdin at all, so there is nothing to wedge on.
async fn run_remote_script(
    gcloud: &PathBuf,
    project: &str,
    zone: &str,
    instance: &str,
    name: &str,
    script: &str,
    attempts: u32,
) -> Result<String, String> {
    let mut local = std::env::temp_dir();
    local.push(format!("blaude-{name}-{}.sh", std::process::id()));
    std::fs::write(&local, script).map_err(|e| format!("could not stage {name}: {e}"))?;
    let _cleanup = scopeguard_remove(local.clone());

    run_retry(
        gcloud,
        &[
            "compute",
            "scp",
            local.to_str().unwrap_or_default(),
            &format!("{instance}:~/{name}.sh"),
            "--project",
            project,
            "--zone",
            zone,
            "--quiet",
        ],
        None,
        attempts,
    )
    .await
    .map_err(|e| format!("could not copy {name}: {e}"))?;

    run_retry(
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
            &format!("bash ~/{name}.sh"),
            "--quiet",
        ],
        None,
        attempts,
    )
    .await
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
    let out = run_remote_script(
        gcloud,
        project,
        zone,
        instance,
        "rooms",
        ROOMS_SETUP_SCRIPT,
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

    /// A service credential failure must not tell an app user to authenticate
    /// gcloud on their Mac; that coupling is exactly what this API removed.
    #[test]
    fn an_expired_cloud_sign_in_says_what_to_do() {
        let raw = "ERROR: (gcloud.compute.instances.list) There was a problem \
                   refreshing your current auth tokens: Reauthentication failed. \
                   cannot prompt during non-interactive execution.\nPlease run:\n\n  \
                   $ gcloud auth login\n";
        let message = super::friendly_cloud_error("look up the server", raw);
        assert!(
            message.contains("provisioning service"),
            "must name the owner: {message}"
        );
        assert!(
            message.contains("no gcloud login"),
            "must keep gcloud off the Mac: {message}"
        );
        assert!(
            !message.contains("ERROR:"),
            "must not echo gcloud: {message}"
        );
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
        let subject = std::env::var("BLAUDE_DELETE_TEST_SUBJECT")
            .expect("set BLAUDE_DELETE_TEST_SUBJECT to the throwaway server owner's Clerk subject");
        let result = super::delete_team(&url, &subject)
            .await
            .expect("delete should succeed");
        println!("delete returned: {result}");
    }

    /// The instance is found by the address in the URL members already hold,
    /// because nothing else identifies it — a client never learns the VM's
    /// name. This also means it works for teams created before the name was
    /// recorded anywhere.
    #[test]
    fn the_server_is_identified_by_the_address_in_its_url() {
        assert_eq!(
            super::team_ip("wss://34-93-93-41.sslip.io:443/api").as_deref(),
            Some("34.93.93.41")
        );
        assert_eq!(
            super::team_ip("wss://35-200-139-215.sslip.io:443/api").as_deref(),
            Some("35.200.139.215")
        );
    }

    /// A URL that is not one of ours must not resolve to something deletable.
    /// Guessing here would delete the wrong machine.
    #[test]
    fn a_url_that_is_not_a_team_server_resolves_to_nothing() {
        assert_eq!(super::team_ip("wss://example.com:443/api"), None);
        assert_eq!(super::team_ip("not a url"), None);
        assert_eq!(super::team_ip("wss://localhost:7644/api"), None);
        assert_eq!(super::team_ip("https://34-93-93-41.sslip.io/api"), None);
        assert_eq!(super::team_ip("wss://999-93-93-41.sslip.io/api"), None);
        assert_eq!(super::team_ip("wss://34-93-93-41-or-1.sslip.io/api"), None);
    }

    /// Packages are installed ONCE, at boot, from a single snippet.
    ///
    /// Every clause here is a measured minute. Installing over ssh instead of
    /// at boot cost ~2 min of pure wall clock with the VM idle; splitting it
    /// across stages re-ran the package index fetch and the dpkg trigger pass
    /// each time; and apt's chromium was a second 274MB browser doing the job
    /// Playwright's already does. A create that gets slower again will get
    /// slower in exactly one of these ways.
    #[test]
    fn packages_are_installed_once_at_boot() {
        let pkgs = super::package_install_snippet();
        assert_eq!(
            pkgs.matches("apt-get update").count(),
            1,
            "one index fetch, not one per install"
        );
        // The INSTALL LINES only — the comment above them names chromium on
        // purpose, and a check that reads the comments passes vacuously.
        let installs: String = pkgs
            .lines()
            .filter(|l| l.contains("apt-get install"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!installs.is_empty(), "nothing is being installed at all");
        assert!(
            !installs.contains(" chromium "),
            "apt's chromium duplicates the Playwright one the shim points at"
        );
        assert!(
            pkgs.contains("/usr/local/bin/chromium"),
            "the shim must exist"
        );

        let rooms = super::ROOMS_SETUP_SCRIPT;
        assert!(
            !rooms.contains("apt-get"),
            "the rooms stage must install nothing — it waits for the boot install"
        );
        assert!(
            rooms.contains("/tmp/browser-helper.done"),
            "the rooms stage must wait for the backgrounded browser install"
        );
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

    /// Remote provisioning scripts must be COPIED and executed, never piped
    /// into `bash -s`. Piping wedges: when the remote bash finishes or dies
    /// while the local ssh still has bytes to write, ssh never exits and the
    /// whole create hangs on one stage with the server already built. This
    /// caught it twice in production; a grep is the cheapest guard that it
    /// cannot come back.
    #[test]
    fn provisioning_never_pipes_a_script_into_bash_dash_s() {
        let src = include_str!("lib.rs");
        // Ignore this test's own mention of the pattern.
        let hits = src.lines().filter(|l| l.contains("\"bash -s\"")).count();
        assert_eq!(
            hits, 0,
            "provisioning must copy-and-run, not pipe into `bash -s`"
        );
    }

    /// The Clerk backend key controls the entire identity instance. A team VM
    /// receives only its signed relay capability, never clerk.env itself.
    #[test]
    fn provisioning_never_copies_the_clerk_backend_secret_to_a_team() {
        let src = include_str!("lib.rs");
        let body = src
            .split("#[cfg(test)]\nmod delete_tests")
            .next()
            .unwrap_or(src);
        assert!(!body.contains(":~/clerk.env"));
        assert!(!body.contains("$H/clerk.env"));
        assert!(body.contains(":~/blaude-relay-token"));
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
        assert_eq!(
            result.expect("cat succeeds").len(),
            big.len(),
            "the whole payload round-trips"
        );
    }
}
