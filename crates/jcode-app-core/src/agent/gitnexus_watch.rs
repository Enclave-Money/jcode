//! Keep the GitNexus code graph fresh while a session is active.
//!
//! Agents write to the working tree constantly, so the repo's code graph goes
//! stale the moment a turn edits a file. A debounced filesystem watcher — one
//! per repo, shared by every session in it — re-indexes shortly after writes
//! settle, so the next turn's briefing reflects the current code without the
//! user (or the agent) ever running `blaude brief` by hand.
//!
//! Ported from the terminal app's `term/src/watch.rs`, with two changes for the
//! daemon: the re-index shells out to `blaude brief` (which drives the gitnexus
//! toolchain) instead of an in-process call, and a process-global registry lets
//! callers `ensure_watching(dir)` idempotently at turn start.

use notify::{RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Default quiet period before a re-index fires. Long enough that an edit burst
/// (a multi-file refactor, a formatter pass, an agent's tool loop) collapses
/// into one re-index. Override with `BLAUDE_GRAPH_DEBOUNCE_SECS`.
const DEFAULT_DEBOUNCE_SECS: u64 = 20;

fn debounce() -> Duration {
    let secs = std::env::var("BLAUDE_GRAPH_DEBOUNCE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_DEBOUNCE_SECS);
    Duration::from_secs(secs)
}

/// How often the manager thread wakes to check debounce timers.
const TICK: Duration = Duration::from_secs(2);

/// Minimum gap between re-index *starts* for one repo, so a big graph never
/// rebuilds back-to-back. Override with `BLAUDE_GRAPH_COOLDOWN_SECS`.
const DEFAULT_COOLDOWN_SECS: u64 = 90;

fn cooldown() -> Duration {
    let secs = std::env::var("BLAUDE_GRAPH_COOLDOWN_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_COOLDOWN_SECS);
    Duration::from_secs(secs)
}

/// The pure debounce decision: re-index when writes have settled and none is
/// already running. Split out so it is testable without real time or fs.
fn should_refresh(since_last_change: Duration, debounce: Duration, indexing: bool) -> bool {
    !indexing && since_last_change >= debounce
}

/// Paths that never warrant a re-index. `.gitnexus` is critical: the re-index
/// writes there, so watching it would self-loop.
fn is_ignored(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some(".git") | Some(".gitnexus") | Some("target") | Some("node_modules")
        )
    })
}

/// Resolve symlinks so comparisons are stable — notably on macOS, where
/// FSEvents reports `/private/var/...` for a `/var/...` watched path.
fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// The git repository root containing `dir`, or `dir` itself if not in a repo.
fn repo_root(dir: &Path) -> PathBuf {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let root = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if root.is_empty() {
                canon(dir)
            } else {
                canon(Path::new(&root))
            }
        }
        _ => canon(dir),
    }
}

/// The repo's current HEAD sha, if it is a git repo.
fn git_head(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if output.status.success() {
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!sha.is_empty()).then_some(sha)
    } else {
        None
    }
}

/// The commit the current index was built from, from `.gitnexus/meta.json`'s
/// `lastCommit`. `None` if there is no index or it can't be read.
fn indexed_commit(root: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(root.join(".gitnexus/meta.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("lastCommit")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// True when the graph should be rebuilt: no index yet, or HEAD moved since the
/// last index. (A dirty tree without a commit is caught by the watcher's normal
/// change events; this is the baseline check for "opened a repo".)
fn is_stale(root: &Path) -> bool {
    match indexed_commit(root) {
        None => true,
        Some(indexed) => git_head(root).map(|head| head != indexed).unwrap_or(false),
    }
}

/// Run one re-index synchronously by shelling `blaude brief` in `root`. Uses the
/// running executable so the bundled binary is used, not whatever is on PATH.
fn run_reindex(root: &Path) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("blaude"));
    let status = std::process::Command::new(exe)
        .arg("brief")
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    // A quiet no-op here means the code graph silently never updates (missing
    // blaude-tools / node on this machine) — say so in the daemon log, once
    // per attempt, so a stale graph is diagnosable from the log alone.
    match status {
        Ok(s) if s.success() => {
            crate::logging::info(&format!("gitnexus re-index ok: {}", root.display()));
        }
        Ok(s) => {
            crate::logging::warn(&format!(
                "gitnexus re-index failed ({s}) in {} — is blaude-tools/node available?",
                root.display()
            ));
        }
        Err(e) => {
            crate::logging::warn(&format!(
                "gitnexus re-index could not start in {}: {e}",
                root.display()
            ));
        }
    }
}

struct WatchEntry {
    _watcher: notify::RecommendedWatcher,
}

fn registry() -> &'static Mutex<HashMap<PathBuf, WatchEntry>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, WatchEntry>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Ensure a debounced re-index watcher is running for the repo containing
/// `working_dir`. Idempotent and cheap to call every turn: the first call for a
/// repo starts the watcher and kicks a baseline re-index if the graph is stale
/// or missing; later calls are a hashmap hit and return immediately.
pub fn ensure_watching(working_dir: &str) {
    if working_dir.is_empty() {
        return;
    }
    let root = repo_root(Path::new(working_dir));

    {
        let reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        if reg.contains_key(&root) {
            return;
        }
    }

    // Start the platform watcher. If it can't be created, we simply have no
    // auto-refresh for this repo — never fatal.
    let (tx, rx) = channel();
    let mut watcher = match notify::recommended_watcher(move |res| {
        if let Ok(event) = res {
            let _ = tx.send(event);
        }
    }) {
        Ok(w) => w,
        Err(_) => return,
    };
    if watcher.watch(&root, RecursiveMode::Recursive).is_err() {
        return;
    }

    {
        let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        // Lost a race: another turn started the watcher first.
        if reg.contains_key(&root) {
            return;
        }
        reg.insert(root.clone(), WatchEntry { _watcher: watcher });
    }

    // Baseline: index a freshly-opened or stale repo now (the watcher only sees
    // future changes). Runs on its own thread so turn start never blocks.
    let baseline_root = root.clone();
    std::thread::spawn(move || {
        if is_stale(&baseline_root) {
            run_reindex(&baseline_root);
        }
    });

    // The manager folds fs events into a per-repo "last change" timer and fires
    // a re-index once writes settle.
    std::thread::spawn(move || manager_loop(rx, root));
}

fn manager_loop(rx: std::sync::mpsc::Receiver<notify::Event>, root: PathBuf) {
    let mut last_change: Option<Instant> = None;
    let mut last_start: Option<Instant> = None;
    let indexing = Arc::new(Mutex::new(false));
    loop {
        match rx.recv_timeout(TICK) {
            Ok(event) => {
                if event.paths.iter().any(|p| !is_ignored(p)) {
                    last_change = Some(Instant::now());
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        let Some(changed_at) = last_change else {
            continue;
        };
        let busy = *indexing.lock().unwrap_or_else(|p| p.into_inner());
        if !should_refresh(changed_at.elapsed(), debounce(), busy) {
            continue;
        }
        // Cooldown floor: don't restart too soon after the last re-index.
        if last_start.is_some_and(|t| t.elapsed() < cooldown()) {
            continue;
        }
        last_change = None;
        last_start = Some(Instant::now());
        *indexing.lock().unwrap_or_else(|p| p.into_inner()) = true;
        let idx = Arc::clone(&indexing);
        let reindex_root = root.clone();
        std::thread::spawn(move || {
            run_reindex(&reindex_root);
            *idx.lock().unwrap_or_else(|p| p.into_inner()) = false;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_waits_for_quiet_then_fires_once() {
        let d = Duration::from_secs(20);
        assert!(!should_refresh(Duration::from_secs(0), d, false));
        assert!(should_refresh(d, d, false));
        assert!(should_refresh(d + Duration::from_secs(1), d, false));
        // Never start a second re-index while one runs.
        assert!(!should_refresh(d + Duration::from_secs(1), d, true));
    }

    #[test]
    fn ignores_vcs_index_and_build_dirs() {
        assert!(is_ignored(Path::new("/repo/.git/index")));
        assert!(is_ignored(Path::new("/repo/.gitnexus/meta.json")));
        assert!(is_ignored(Path::new("/repo/target/debug/foo")));
        assert!(is_ignored(Path::new("/repo/node_modules/x/y.js")));
        assert!(!is_ignored(Path::new("/repo/src/main.rs")));
    }

    #[test]
    fn no_index_is_stale() {
        let dir = std::env::temp_dir().join(format!("gitnexus-watch-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // No .gitnexus/ → stale (needs a baseline index).
        assert!(is_stale(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
