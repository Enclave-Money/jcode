//! `blaude brief` — repo-structure briefing via GitNexus.
//!
//! Mechanism 2 of the thesis: stop the model rediscovering the call graph
//! every session. This is *code structure*, not conversational memory.
//!
//! ## Why this is a wrapper and not just `npx gitnexus analyze`
//!
//! `analyze` generates AGENTS.md / CLAUDE.md that instruct the agent to call
//! **MCP tools** (`impact()`, `query()`, `context()`, `detect_changes()`).
//! Those only exist after `gitnexus setup`, which we deliberately skip: 17 MCP
//! tool schemas cost tokens on every single turn, unmeasured against what they
//! save. An analyze-only briefing therefore points at tools that do not exist
//! — worse than no briefing, because the agent will try and fail.
//!
//! So we rewrite the generated block to reference the CLI commands, which
//! mirror the MCP tools exactly and cost nothing until invoked. Verified
//! working: `context`, `impact`, `query`, `trace`, `detect-changes`, `status`.
//!
//! GitNexus is PolyForm Noncommercial: invoked via npx/node, never vendored.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const START: &str = "<!-- gitnexus:start -->";
const END: &str = "<!-- gitnexus:end -->";

/// The runner. Prefer the pinned local `.gitnexus/run.cjs` (fast, version-
/// stable) and fall back to npx on first run.
enum Runner {
    Local(PathBuf),
    Npx,
}

fn runner(root: &Path) -> Runner {
    let local = root.join(".gitnexus/run.cjs");
    if local.is_file() {
        Runner::Local(local)
    } else {
        Runner::Npx
    }
}

fn run_gitnexus(root: &Path, args: &[&str], stream: bool) -> Result<String> {
    let mut cmd = match runner(root) {
        Runner::Local(p) => {
            let mut c = Command::new("node");
            c.arg(p);
            c
        }
        Runner::Npx => {
            let mut c = Command::new("npx");
            c.args(["--yes", "gitnexus@latest"]);
            c
        }
    };
    cmd.args(args).current_dir(root);

    if stream {
        let status = cmd.status().context("running gitnexus")?;
        if !status.success() {
            bail!("gitnexus {:?} failed ({status})", args);
        }
        Ok(String::new())
    } else {
        let out = cmd.output().context("running gitnexus")?;
        if !out.status.success() {
            bail!(
                "gitnexus {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// A held refresh lock. Dropping it releases the lock (removes the file).
/// GitNexus writes to a KuzuDB graph that must never see two concurrent
/// `analyze` runs — a real risk once blaude is multiplayer (many agents, one
/// shared checkout). Every `analyze` invocation goes through this lock.
pub struct RefreshLock {
    path: PathBuf,
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A lock older than this is treated as abandoned (the holder crashed) and
/// stolen. Generous: a full re-index of a large repo takes minutes.
const LOCK_STALE_SECS: u64 = 1800;

fn lock_path(root: &Path) -> PathBuf {
    root.join(".gitnexus/refresh.lock")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Is `pid` a live process? Used to steal a lock whose holder died.
fn pid_alive(pid: i32) -> bool {
    // kill(pid, 0): 0 => alive, ESRCH => gone, EPERM => alive but not ours.
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// Try to take the refresh lock without blocking. Returns `None` if another
/// live refresh already holds it (so the watcher can coalesce), stealing a
/// stale/abandoned lock first.
pub fn try_refresh_lock(root: &Path) -> Option<RefreshLock> {
    let path = lock_path(root);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            use std::io::Write as _;
            let _ = writeln!(f, "{}\n{}", std::process::id(), now_secs());
            Some(RefreshLock { path })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let held = fs::read_to_string(&path).unwrap_or_default();
            let mut lines = held.lines();
            let pid = lines.next().and_then(|l| l.trim().parse::<i32>().ok());
            let ts = lines.next().and_then(|l| l.trim().parse::<u64>().ok());
            // An unknown pid is NOT assumed dead, and an unparseable timestamp
            // falls back to the file's mtime — so a lock that was just created
            // but not yet written (create_new succeeds before the writeln) is
            // never stolen out from under its holder.
            let dead = pid.map(|p| !pid_alive(p)).unwrap_or(false);
            let stale = match ts {
                Some(ts) => now_secs().saturating_sub(ts) > LOCK_STALE_SECS,
                None => fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|age| age.as_secs() > LOCK_STALE_SECS)
                    .unwrap_or(false),
            };
            if stale || dead {
                let _ = fs::remove_file(&path);
                // One retry after stealing; give up if we lose the race.
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .ok()
                    .map(|mut f| {
                        use std::io::Write as _;
                        let _ = writeln!(f, "{}\n{}", std::process::id(), now_secs());
                        RefreshLock { path: path.clone() }
                    })
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

/// Take the refresh lock, waiting up to `timeout` for a concurrent refresh to
/// finish. For user-initiated `blaude brief`, which must actually run.
fn wait_refresh_lock(root: &Path, timeout: std::time::Duration) -> Option<RefreshLock> {
    let start = std::time::Instant::now();
    loop {
        if let Some(lock) = try_refresh_lock(root) {
            return Some(lock);
        }
        if start.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Run a read-only Cypher query against the repo's graph, returning GitNexus's
/// raw JSON. Always scoped with `--repo .` — the registry is global and a
/// council run leaves worktree checkouts of the same repo behind, so an
/// unqualified query can fail with "Multiple repositories indexed".
pub fn cypher(root: &Path, query: &str) -> Result<String> {
    run_gitnexus(root, &["cypher", "--repo", ".", query], false)
}

/// Whether GitNexus reports the index as behind the working tree. Cheap (one
/// `status` call, no indexing) — prune prints a hint when true.
pub fn is_index_stale(root: &Path) -> bool {
    run_gitnexus(root, &["status"], false)
        .map(|s| s.to_lowercase().contains("stale"))
        .unwrap_or(false)
}

/// Round to two significant figures and format compactly (`~1.1k`, `~54k`,
/// `~140k`). Briefing stats are baked into tracked AGENTS.md/CLAUDE.md, so an
/// exact count re-dirties them on every reindex; two sig figs holds steady
/// across the small deltas an auto-refresh produces.
pub fn human_count(n: u64) -> String {
    let rounded = round_sig2(n);
    if rounded >= 1000 {
        let k = rounded as f64 / 1000.0;
        if k >= 100.0 {
            format!("~{}k", k.round() as u64)
        } else if (k.fract()).abs() < f64::EPSILON {
            format!("~{}k", k as u64)
        } else {
            format!("~{k:.1}k")
        }
    } else {
        format!("~{rounded}")
    }
}

fn round_sig2(n: u64) -> u64 {
    if n < 100 {
        return n; // two sig figs of a <100 value is the value itself
    }
    let magnitude = (n as f64).log10().floor() as u32;
    let scale = 10u64.pow(magnitude - 1);
    ((n as f64 / scale as f64).round() as u64) * scale
}

/// The CLI-mode briefing body. Replaces GitNexus's MCP-assuming block.
pub fn cli_briefing(repo_name: &str, stats: &str) -> String {
    format!(
        r#"{START}
# Repo graph — GitNexus (CLI mode)

`{repo_name}` is indexed as a knowledge graph{stats}. Query it with the commands
below **instead of grepping** when you need structure: callers, callees, blast
radius, execution flows.

No MCP server is installed by design — these commands cost nothing until you run
one, whereas MCP tool schemas would cost tokens on every turn.

## Commands

Run from the repo root. All output is JSON.

| Need | Command |
|------|---------|
| What calls this? What does it call? | `node .gitnexus/run.cjs context <symbol> --repo .` |
| What breaks if I change this? | `node .gitnexus/run.cjs impact <symbol> --repo .` |
| Find code by concept, not by string | `node .gitnexus/run.cjs query "<concept>" --repo .` |
| How does A reach B? | `node .gitnexus/run.cjs trace <from> <to> --repo .` |
| What did my diff actually touch? | `node .gitnexus/run.cjs detect-changes --repo .` |
| Is the index current? | `node .gitnexus/run.cjs status` |
| Find dead / unused code | `blaude prune` |

`--repo .` is not optional: GitNexus keeps one global registry of indexed
checkouts, and two checkouts of the same repository — a `council` run creates a
git worktree per backend — make an unqualified command fail with *"Multiple
repositories indexed"*, as an unhandled stack trace rather than as JSON.

## Use it for

- **Before editing a shared symbol**, run `impact` and check the risk level.
  It reports the true blast radius from the call graph, not a guess.
- **Before committing**, `detect-changes` maps your diff to affected symbols and
  execution flows — it catches edits that reach further than intended.
- **When exploring unfamiliar code**, `query` returns flows ranked by relevance.
  It finds things a grep for the wrong noun would miss.
- **When asked to remove dead/unused code**, run `blaude prune`: it lists
  functions the call graph shows no callers for (candidates — confirm with
  `cargo check` before deleting).

## Refresh

blaude re-indexes in the background whenever an agent writes code, so this graph
stays current on its own. `blaude brief` re-briefs or repairs by hand if
`status` ever reports stale.
{END}"#
    )
}

/// Replace an existing gitnexus block, or append one.
fn splice(existing: &str, block: &str) -> String {
    if let (Some(s), Some(e)) = (existing.find(START), existing.find(END)) {
        let mut out = String::with_capacity(existing.len() + block.len());
        out.push_str(&existing[..s]);
        out.push_str(block);
        out.push_str(&existing[e + END.len()..]);
        return out;
    }
    // GitNexus wrote a start marker with no end marker: replace to EOF.
    if let Some(s) = existing.find(START) {
        let mut out = String::from(&existing[..s]);
        out.push_str(block);
        out.push('\n');
        return out;
    }
    if existing.trim().is_empty() {
        return format!("{block}\n");
    }
    format!("{}\n\n{block}\n", existing.trim_end())
}

/// Keep the 24MB index and generated skills out of git. GitNexus self-ignores
/// `.gitnexus/` with its own nested .gitignore, but the local skills dir it
/// installs is not covered.
fn ensure_gitignored(root: &Path) -> Result<Vec<String>> {
    let path = root.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut added = Vec::new();
    let mut body = existing.clone();
    for entry in [".gitnexus/", ".claude/skills/gitnexus/"] {
        let present = existing
            .lines()
            .any(|l| l.trim() == entry || l.trim() == entry.trim_end_matches('/'));
        if !present {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(entry);
            body.push('\n');
            added.push(entry.to_string());
        }
    }
    if !added.is_empty() {
        fs::write(&path, body)?;
    }
    Ok(added)
}

pub fn repo_root() -> Result<PathBuf> {
    repo_root_of(Path::new("."))
}

/// The checkout containing `dir`. Takes a directory rather than relying on the
/// process cwd, because a host with several sessions open is in several
/// directories at once — the app's graph pane has to describe the tab you are
/// looking at, not wherever the binary happened to be started.
pub fn repo_root_of(dir: &Path) -> Result<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!("not inside a git repository");
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
}

/// The repository's name, not the checkout's.
///
/// `council` runs agents inside git worktrees, and `blaude brief` may well be run
/// from one, so taking the name from the current directory would title the
/// briefing after a temporary worktree. `--git-common-dir` points at the main
/// repository's `.git` from anywhere, including a worktree.
fn repo_name(root: &Path) -> String {
    let from_common_dir = || -> Option<String> {
        let out = Command::new("git")
            .args(["rev-parse", "--git-common-dir"])
            .current_dir(root)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
        // Relative (plain `.git`) at the repo root; absolute from a worktree.
        let dir = if dir.is_relative() {
            root.join(dir)
        } else {
            dir
        };
        Some(dir.parent()?.file_name()?.to_string_lossy().into_owned())
    };
    from_common_dir()
        .or_else(|| root.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "repo".into())
}

/// Size of the indexed repo graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GraphStats {
    pub symbols: u64,
    pub relationships: u64,
    /// Execution flows, when GitNexus recorded them.
    pub flows: Option<u64>,
}

/// Graph stats from `.gitnexus/meta.json` — structured and reliable, unlike
/// scraping the analyze banner (which streams straight to the terminal).
///
/// Reads only what is already on disk: no `npx`, no network, no index run. That
/// is what lets the app show a graph pane on every frame for free, and report
/// "no index yet" instead of blocking on one.
pub fn graph_stats(root: &Path) -> Option<GraphStats> {
    let body = fs::read_to_string(root.join(".gitnexus/meta.json")).ok()?;
    let v = serde_json::from_str::<serde_json::Value>(&body).ok()?;
    let s = &v["stats"];
    let get = |k: &str| s.get(k).and_then(|x| x.as_u64());
    Some(GraphStats {
        symbols: get("nodes")?,
        relationships: get("edges")?,
        flows: get("processes"),
    })
}

fn parse_stats(root: &Path) -> String {
    let Some(stats) = graph_stats(root) else {
        return String::new();
    };
    let mut out = format!(
        " of {} symbols and {} relationships",
        human_count(stats.symbols),
        human_count(stats.relationships)
    );
    if let Some(flows) = stats.flows {
        out.push_str(&format!(" across {} execution flows", human_count(flows)));
    }
    out
}

pub struct BriefReport {
    pub root: PathBuf,
    pub status: String,
    pub written: Vec<String>,
    pub gitignored: Vec<String>,
}

/// Run the full briefing refresh.
pub fn run(check_only: bool) -> Result<BriefReport> {
    let root = repo_root()?;

    if check_only {
        let status = run_gitnexus(&root, &["status"], false)
            .unwrap_or_else(|e| format!("no index yet ({e})"));
        return Ok(BriefReport {
            root,
            status,
            written: Vec::new(),
            gitignored: Vec::new(),
        });
    }

    eprintln!("[blaude] indexing repo graph (first run downloads gitnexus)...");
    // Serialize against the background watcher's refreshes.
    let _lock = wait_refresh_lock(&root, std::time::Duration::from_secs(300));
    run_gitnexus(&root, &["analyze"], true)?;

    let status = run_gitnexus(&root, &["status"], false).unwrap_or_default();
    let block = cli_briefing(&repo_name(&root), &parse_stats(&root));

    // Both vendors get an identical brief — Codex reads AGENTS.md, Claude Code
    // reads CLAUDE.md. Identical context is a prerequisite for the council to
    // be a fair comparison rather than two differently-briefed agents.
    let mut written = Vec::new();
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let p = root.join(name);
        let existing = fs::read_to_string(&p).unwrap_or_default();
        let next = splice(&existing, &block);
        if next != existing {
            fs::write(&p, &next)?;
            written.push(name.to_string());
        }
    }

    let gitignored = ensure_gitignored(&root)?;

    Ok(BriefReport {
        root,
        status,
        written,
        gitignored,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn briefing_mentions_prune_and_auto_refresh_and_rounds_stats() {
        let b = cli_briefing("demo", &parse_stats(Path::new("/nonexistent")));
        // Dead-code path is advertised.
        assert!(b.contains("blaude prune"), "{b}");
        // Refresh section describes automatic reindexing, not a manual chore.
        assert!(b.contains("re-indexes in the background"), "{b}");
        // Still enforces scoping.
        assert!(b.contains("--repo ."), "{b}");
    }

    #[test]
    fn human_count_rounds_to_two_sig_figs() {
        assert_eq!(human_count(1084), "~1.1k");
        assert_eq!(human_count(3916), "~3.9k");
        assert_eq!(human_count(3950), "~4k");
        assert_eq!(human_count(54225), "~54k");
        assert_eq!(human_count(136890), "~140k");
        assert_eq!(human_count(95), "~95");
        assert_eq!(human_count(30), "~30");
        // Stability: nearby values collapse to the same label (the anti-churn
        // property the rounding exists for).
        assert_eq!(human_count(1084), human_count(1091));
        assert_eq!(human_count(54225), human_count(54480));
    }

    #[test]
    fn a_freshly_created_empty_lock_is_not_stolen() {
        // Reproduces the create_new-succeeds-before-writeln window: an empty
        // lock file with a current mtime must be treated as held, not stolen.
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".gitnexus")).unwrap();
        fs::write(d.path().join(".gitnexus/refresh.lock"), "").unwrap();
        assert!(
            try_refresh_lock(d.path()).is_none(),
            "an empty but freshly-created lock must not be stolen"
        );
    }

    #[test]
    fn refresh_lock_is_exclusive_and_steals_stale() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".gitnexus")).unwrap();
        let first = try_refresh_lock(d.path()).expect("first acquire");
        // A second concurrent acquire is refused.
        assert!(
            try_refresh_lock(d.path()).is_none(),
            "lock must be exclusive"
        );
        drop(first);
        // Released — acquirable again.
        assert!(
            try_refresh_lock(d.path()).is_some(),
            "lock must release on drop"
        );

        // A stale lock (old timestamp, our own live pid) is stolen.
        let path = d.path().join(".gitnexus/refresh.lock");
        fs::write(&path, format!("{}\n{}", std::process::id(), 0)).unwrap();
        assert!(
            try_refresh_lock(d.path()).is_some(),
            "a lock older than the stale timeout must be stolen"
        );
    }

    #[test]
    fn briefing_references_cli_not_mcp() {
        let b = cli_briefing("demo", " (10 nodes)");
        // The whole point: no MCP tool-call syntax should survive.
        assert!(!b.contains("impact({"));
        assert!(!b.contains("detect_changes()"));
        assert!(!b.contains("MCP tools to understand"));
        assert!(b.contains("node .gitnexus/run.cjs impact"));
        assert!(b.contains("demo"));
        assert!(b.starts_with(START) && b.ends_with(END));
    }

    #[test]
    fn every_query_command_names_its_repo() {
        // GitNexus resolves against a global registry of indexed checkouts, so
        // an unqualified command dies with "Multiple repositories indexed" the
        // moment a second checkout exists — and `council` creates a git worktree
        // per backend, so this project guarantees a second checkout. Without
        // `--repo .` the briefing sends the agent at a stack trace.
        let b = cli_briefing("demo", "");
        for cmd in ["context", "impact", "query", "trace", "detect-changes"] {
            let line = b
                .lines()
                .find(|l| l.contains(&format!("run.cjs {cmd}")))
                .unwrap_or_else(|| panic!("no line advertising {cmd}"));
            assert!(
                line.contains("--repo ."),
                "{cmd} is advertised without --repo: {line}"
            );
        }
        // `status` is the exception: it reports on every registered checkout and
        // takes no target, so qualifying it would hide exactly the duplication
        // this flag exists to survive.
        let status_line = b
            .lines()
            .find(|l| l.contains("run.cjs status"))
            .expect("no status line");
        assert!(!status_line.contains("--repo"));
    }

    #[test]
    fn parse_stats_reads_meta_json() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".gitnexus")).unwrap();
        fs::write(
            d.path().join(".gitnexus/meta.json"),
            r#"{"stats":{"nodes":344,"edges":827,"processes":30}}"#,
        )
        .unwrap();
        let s = parse_stats(d.path());
        // Rounded to two sig figs so tracked briefings stop churning.
        assert!(s.contains("~340 symbols"), "{s}");
        assert!(s.contains("~830 relationships"), "{s}");
        assert!(s.contains("~30 execution flows"), "{s}");
        // Missing meta must degrade to empty, never panic.
        let d2 = tempfile::tempdir().unwrap();
        assert_eq!(parse_stats(d2.path()), "");
    }

    #[test]
    fn splice_replaces_existing_gitnexus_block() {
        let existing =
            format!("# My notes\n\nkeep me\n\n{START}\nOLD MCP JUNK\n{END}\n\ntrailing stays\n");
        let out = splice(&existing, &format!("{START}\nNEW\n{END}"));
        assert!(out.contains("keep me"));
        assert!(out.contains("trailing stays"));
        assert!(out.contains("NEW"));
        assert!(!out.contains("OLD MCP JUNK"));
        assert_eq!(out.matches(START).count(), 1);
    }

    #[test]
    fn splice_handles_unterminated_block() {
        // GitNexus wrote a start marker but no end marker.
        let existing = format!("# Notes\nkeep\n\n{START}\nOLD TO EOF\n");
        let out = splice(&existing, &format!("{START}\nNEW\n{END}"));
        assert!(out.contains("keep"));
        assert!(!out.contains("OLD TO EOF"));
        assert!(out.contains("NEW"));
    }

    #[test]
    fn splice_appends_when_absent_and_preserves_user_content() {
        let out = splice("# Existing project notes\n", &format!("{START}\nB\n{END}"));
        assert!(out.starts_with("# Existing project notes"));
        assert!(out.contains(START));
    }

    #[test]
    fn splice_on_empty_file_has_no_leading_blank() {
        let out = splice("", &format!("{START}\nB\n{END}"));
        assert!(out.starts_with(START));
    }

    #[test]
    fn gitignore_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let added1 = ensure_gitignored(root).unwrap();
        assert_eq!(added1.len(), 2);
        let added2 = ensure_gitignored(root).unwrap();
        assert!(added2.is_empty(), "second run must add nothing");
        let body = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(body.matches(".gitnexus/").count(), 1);
    }

    #[test]
    fn gitignore_preserves_existing_entries() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(".gitignore"), "target/").unwrap();
        ensure_gitignored(d.path()).unwrap();
        let body = fs::read_to_string(d.path().join(".gitignore")).unwrap();
        assert!(body.contains("target/"));
        assert!(body.contains(".gitnexus/"));
    }
}
