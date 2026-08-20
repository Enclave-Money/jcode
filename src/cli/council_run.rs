//! `blaude council run` — the cross-vendor fan-out.
//!
//! The council's differentiator: send one prompt to every member model at once,
//! each working in its **own git worktree** off the current HEAD, then show what
//! each proposed as a diff. Isolation is per-process and per-worktree — every
//! member is a separate `blaude run` invocation with its own model and cwd — so
//! two models editing the same files never collide, and each proposal is a clean
//! patch you can compare and cherry-pick.
//!
//! The git/worktree/diff orchestration is a pure function over a *runner*
//! closure so it can be unit-tested with a stub instead of real model calls.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use jcode_storage::councils::Councils;

/// What one council member produced.
#[derive(Debug)]
pub struct MemberOutcome {
    pub model: String,
    /// The agent's text answer, or the error if the run failed.
    pub result: Result<String>,
    /// The unified diff of its edits against the base commit (empty if none).
    pub diff: String,
    /// Where its worktree is, when kept for inspection.
    pub worktree: Option<PathBuf>,
}

/// The production entry point: load the council, resolve the repo, fan out.
pub fn run(name: &str, prompt: &str, keep: bool) -> Result<()> {
    let councils = Councils::load()?;
    let council = councils
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("no council named “{name}” (see `blaude council list`)"))?
        .clone();

    let repo_root = git_repo_root().context(
        "`council run` needs a git repository — it isolates each model's edits in a worktree",
    )?;
    let base_sha = git_head_sha(&repo_root)?;
    let exe = std::env::current_exe().context("locating the blaude binary")?;

    println!(
        "Council “{}”: fanning “{}” out to {} models from {}…\n",
        council.name,
        truncate(prompt, 60),
        council.members.len(),
        &base_sha[..base_sha.len().min(12)]
    );

    // Production runner: a fresh `blaude run --json --model <m>` per member, cwd
    // set to that member's worktree so its edits land there.
    let runner = |worktree: &Path, model: &str, prompt: &str| -> Result<String> {
        run_member_process(&exe, worktree, model, prompt)
    };

    let outcomes = fan_out(
        &repo_root,
        &base_sha,
        &council.members,
        prompt,
        keep,
        &runner,
    );
    report(&outcomes);
    Ok(())
}

/// Run every member against `prompt`, each in its own detached worktree off
/// `base_sha`. Worktrees are removed afterwards unless `keep`. Pure over
/// `runner`, so a test can substitute a stub for the model call.
pub fn fan_out(
    repo_root: &Path,
    base_sha: &str,
    members: &[String],
    prompt: &str,
    keep: bool,
    runner: &(dyn Fn(&Path, &str, &str) -> Result<String> + Sync),
) -> Vec<MemberOutcome> {
    // Each member on its own thread: the runner blocks (a subprocess), so
    // threads give real parallelism without pulling in an async runtime here.
    std::thread::scope(|scope| {
        let handles: Vec<_> = members
            .iter()
            .enumerate()
            .map(|(i, model)| {
                scope.spawn(move || run_one(repo_root, base_sha, i, model, prompt, keep, runner))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("member thread panicked"))
            .collect()
    })
}

fn run_one(
    repo_root: &Path,
    base_sha: &str,
    index: usize,
    model: &str,
    prompt: &str,
    keep: bool,
    runner: &(dyn Fn(&Path, &str, &str) -> Result<String> + Sync),
) -> MemberOutcome {
    let worktree = repo_root
        .join(".git")
        .join("blaude-councils")
        .join(format!("{}-{index}", sanitize(model)));

    // Set up the isolated worktree; a setup failure is the member's result.
    if let Err(e) = add_worktree(repo_root, &worktree, base_sha) {
        return MemberOutcome {
            model: model.to_string(),
            result: Err(e),
            diff: String::new(),
            worktree: None,
        };
    }

    let result = runner(&worktree, model, prompt);
    // Capture the diff regardless of whether the run reported an error — a model
    // may have edited files before failing, and that partial work is worth
    // seeing.
    let diff = worktree_diff(&worktree).unwrap_or_default();

    let kept = if keep {
        Some(worktree.clone())
    } else {
        let _ = remove_worktree(repo_root, &worktree);
        None
    };

    MemberOutcome {
        model: model.to_string(),
        result,
        diff,
        worktree: kept,
    }
}

/// Spawn `blaude run --json --model <model> <prompt>` in `worktree` and return
/// the agent's text answer.
fn run_member_process(exe: &Path, worktree: &Path, model: &str, prompt: &str) -> Result<String> {
    let out = Command::new(exe)
        .arg("run")
        .arg("--json")
        .arg("--model")
        .arg(model)
        .arg(prompt)
        .current_dir(worktree)
        .output()
        .with_context(|| format!("spawning blaude run for {model}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "model {model} run failed ({}): {}",
            out.status,
            truncate(err.trim(), 300)
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The report is a JSON object with a `text` field; be lenient about any
    // leading non-JSON banner lines by scanning for the first `{`.
    let json_start = stdout.find('{').unwrap_or(0);
    let report: serde_json::Value = serde_json::from_str(stdout[json_start..].trim())
        .with_context(|| format!("parsing run report for {model}"))?;
    Ok(report
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string())
}

fn report(outcomes: &[MemberOutcome]) {
    for (i, o) in outcomes.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("━━━ {} ━━━", o.model);
        match &o.result {
            Ok(text) if !text.trim().is_empty() => println!("{}", text.trim()),
            Ok(_) => println!("(no text answer)"),
            Err(e) => println!("⚠ failed: {e}"),
        }
        if o.diff.trim().is_empty() {
            println!("\n(no file changes)");
        } else {
            let files = diff_file_count(&o.diff);
            println!("\n{} file(s) changed:", files);
            println!("{}", o.diff.trim_end());
        }
        if let Some(wt) = &o.worktree {
            println!("\nworktree kept at {}", wt.display());
        }
    }
    println!(
        "\nPick the proposal you want and apply it in your tree, or re-run with \
         --keep to inspect the worktrees."
    );
}

// --- git helpers ---------------------------------------------------------

fn git_repo_root() -> Result<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!("not inside a git repository");
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
}

fn git_head_sha(repo_root: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .context("reading HEAD")?;
    if !out.status.success() {
        bail!("the repository has no commits yet — `council run` needs a base commit");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn add_worktree(repo_root: &Path, worktree: &Path, base_sha: &str) -> Result<()> {
    // A stale worktree from a previous run would make `add` fail; clear it.
    let _ = remove_worktree(repo_root, worktree);
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let out = Command::new("git")
        .args(["worktree", "add", "--detach", "--force"])
        .arg(worktree)
        .arg(base_sha)
        .current_dir(repo_root)
        .output()
        .context("git worktree add")?;
    if !out.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The member's edits as a unified diff against the worktree's base checkout,
/// including new files (staged with `add -A` first so untracked files show).
fn worktree_diff(worktree: &Path) -> Result<String> {
    let add = Command::new("git")
        .args(["add", "-A"])
        .current_dir(worktree)
        .output()
        .context("git add -A")?;
    if !add.status.success() {
        bail!(
            "git add failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        );
    }
    let out = Command::new("git")
        .args(["diff", "--cached"])
        .current_dir(worktree)
        .output()
        .context("git diff")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn remove_worktree(repo_root: &Path, worktree: &Path) -> Result<()> {
    let out = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .current_dir(repo_root)
        .output()
        .context("git worktree remove")?;
    if !out.status.success() {
        // Fall back to a plain directory removal + prune so we never leak.
        let _ = std::fs::remove_dir_all(worktree);
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(repo_root)
            .output();
    }
    Ok(())
}

// --- small pure helpers --------------------------------------------------

/// A filesystem-safe fragment of a model id (`openai:gpt-5` -> `openai-gpt-5`).
fn sanitize(model: &str) -> String {
    model
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Count the files in a unified diff by its `diff --git` headers.
fn diff_file_count(diff: &str) -> usize {
    diff.lines()
        .filter(|l| l.starts_with("diff --git "))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn git(args: &[&str], dir: &Path) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(&["init", "-q"], p);
        git(&["config", "user.email", "t@t"], p);
        git(&["config", "user.name", "t"], p);
        git(&["config", "commit.gpgsign", "false"], p);
        std::fs::write(p.join("README.md"), "base\n").unwrap();
        git(&["add", "-A"], p);
        git(&["commit", "-qm", "base"], p);
        dir
    }

    #[test]
    fn sanitize_makes_a_safe_fragment() {
        assert_eq!(sanitize("openai:gpt-5-codex"), "openai-gpt-5-codex");
        assert_eq!(sanitize("claude-opus-4-8"), "claude-opus-4-8");
    }

    #[test]
    fn diff_file_count_reads_the_headers() {
        let d = "diff --git a/x b/x\n+a\ndiff --git a/y b/y\n+b\n";
        assert_eq!(diff_file_count(d), 2);
        assert_eq!(diff_file_count(""), 0);
    }

    #[test]
    fn fan_out_isolates_each_member_and_captures_its_diff() {
        let repo = init_repo();
        let root = repo.path().to_path_buf();
        let base = git_head_sha(&root).unwrap();
        let members = vec!["model-a".to_string(), "model-b".to_string()];

        // Stub runner: each "model" writes a distinct file into its worktree,
        // proving the worktrees are isolated and diffs are captured per member.
        let calls = AtomicUsize::new(0);
        let runner = |wt: &Path, model: &str, prompt: &str| -> Result<String> {
            calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(wt.join(format!("{model}.txt")), prompt).unwrap();
            Ok(format!("{model} did it"))
        };

        let outcomes = fan_out(&root, &base, &members, "hello", false, &runner);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(outcomes.len(), 2);
        for o in &outcomes {
            assert!(o.result.as_ref().unwrap().contains("did it"));
            // Each member's diff shows only its own new file.
            assert!(
                o.diff.contains(&format!("{}.txt", o.model)),
                "diff: {}",
                o.diff
            );
            let other = if o.model == "model-a" {
                "model-b"
            } else {
                "model-a"
            };
            assert!(
                !o.diff.contains(&format!("{other}.txt")),
                "leaked: {}",
                o.diff
            );
            assert_eq!(diff_file_count(&o.diff), 1);
        }
        // Worktrees removed (not kept), so the councils dir is empty/gone.
        let wt_dir = root.join(".git").join("blaude-councils");
        let leftover = std::fs::read_dir(&wt_dir)
            .map(|rd| rd.flatten().filter(|e| e.path().is_dir()).count())
            .unwrap_or(0);
        assert_eq!(leftover, 0, "worktrees should be cleaned up");
    }

    #[test]
    fn a_kept_worktree_is_reported_and_left_on_disk() {
        let repo = init_repo();
        let root = repo.path().to_path_buf();
        let base = git_head_sha(&root).unwrap();
        let members = vec!["keep-me".to_string()];
        let runner = |wt: &Path, _m: &str, _p: &str| -> Result<String> {
            std::fs::write(wt.join("out.txt"), "x").unwrap();
            Ok("done".into())
        };
        let outcomes = fan_out(&root, &base, &members, "p", true, &runner);
        let wt = outcomes[0].worktree.as_ref().expect("kept worktree");
        assert!(wt.exists(), "kept worktree should remain on disk");
        // Clean up.
        let _ = remove_worktree(&root, wt);
    }

    #[test]
    fn a_member_whose_run_errors_still_yields_an_outcome() {
        let repo = init_repo();
        let root = repo.path().to_path_buf();
        let base = git_head_sha(&root).unwrap();
        let members = vec!["boom".to_string()];
        let runner = |_wt: &Path, _m: &str, _p: &str| -> Result<String> { bail!("kaboom") };
        let outcomes = fan_out(&root, &base, &members, "p", false, &runner);
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].result.is_err());
    }
}
