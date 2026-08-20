//! Council deliberation — shared by the `blaude council run` CLI and the
//! interactive TUI council mode.
//!
//! A council does not just fan a task out and let you keep the better patch. Its
//! members **deliberate**, in three stages:
//!
//!   1. Draft — each model independently drafts a plan for the task, blind to
//!      the others, in its own git worktree off the same commit.
//!   2. Critique — each model reads every draft and says what is strongest in
//!      each and what to combine.
//!   3. Synthesize — one member merges the drafts and critiques into a single
//!      joint plan that takes the best from all of them.
//!
//! Each model call runs in an isolated worktree so nothing touches the user's
//! working tree; the deliberation itself is text (plans and critiques), and the
//! result is one joint plan. The orchestration is pure over a `runner` closure
//! so it can be unit-tested with a stub; the production runner ([`spawn_member`])
//! shells out to `blaude run --json`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// One model's text contribution to a stage (a draft or a critique).
#[derive(Debug)]
pub struct MemberText {
    pub model: String,
    pub text: Result<String>,
}

/// The full result of a council deliberation.
#[derive(Debug)]
pub struct Deliberation {
    /// Stage 1: each member's independent plan.
    pub drafts: Vec<MemberText>,
    /// Stage 2: each member's critique of all the drafts.
    pub critiques: Vec<MemberText>,
    /// Which model synthesized the joint plan (the first member).
    pub synthesizer: String,
    /// Stage 3: the joint plan built from the best of the drafts + critiques.
    pub joint_plan: Result<String>,
    /// Worktrees kept for inspection (when `keep`), one per member.
    pub worktrees: Vec<PathBuf>,
}

/// Run the three-stage deliberation. Pure over `runner` (model, worktree,
/// prompt) so tests can stub the model call. Worktrees are removed afterwards
/// unless `keep`.
pub fn deliberate(
    repo_root: &Path,
    base_sha: &str,
    members: &[String],
    task: &str,
    keep: bool,
    runner: &(dyn Fn(&Path, &str, &str) -> Result<String> + Sync),
) -> Deliberation {
    // One worktree per member, off the base commit, reused across stages so a
    // member's stages share a cwd. Setup failures leave that slot `None`.
    let worktrees: Vec<Option<PathBuf>> = members
        .iter()
        .enumerate()
        .map(|(i, model)| {
            let wt = worktree_path(repo_root, i, model);
            add_worktree(repo_root, &wt, base_sha).ok().map(|_| wt)
        })
        .collect();

    // Stage 1: independent drafts.
    let drafts = run_stage(members, &worktrees, runner, |_model| draft_prompt(task));

    // Stage 2: each member critiques all drafts.
    let drafts_block = format_block(&drafts);
    let critiques = run_stage(members, &worktrees, runner, |model| {
        critique_prompt(task, model, &drafts_block)
    });

    // Stage 3: the first member synthesizes the joint plan.
    let critiques_block = format_block(&critiques);
    let synthesizer = members.first().cloned().unwrap_or_default();
    let joint_plan = match worktrees.first().and_then(|w| w.as_ref()) {
        Some(wt) => runner(
            wt,
            &synthesizer,
            &synthesis_prompt(task, &drafts_block, &critiques_block),
        ),
        None => Err(anyhow::anyhow!("no worktree for the synthesizer")),
    };

    let kept: Vec<PathBuf> = if keep {
        worktrees.iter().flatten().cloned().collect()
    } else {
        for wt in worktrees.iter().flatten() {
            let _ = remove_worktree(repo_root, wt);
        }
        Vec::new()
    };

    Deliberation {
        drafts,
        critiques,
        synthesizer,
        joint_plan,
        worktrees: kept,
    }
}

/// Run one stage across all members in parallel (each in its own worktree),
/// building the per-member prompt with `prompt_for`.
fn run_stage(
    members: &[String],
    worktrees: &[Option<PathBuf>],
    runner: &(dyn Fn(&Path, &str, &str) -> Result<String> + Sync),
    prompt_for: impl Fn(&str) -> String + Sync,
) -> Vec<MemberText> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = members
            .iter()
            .enumerate()
            .map(|(i, model)| {
                let wt = worktrees[i].clone();
                let prompt_for = &prompt_for;
                scope.spawn(move || {
                    let text = match wt {
                        Some(wt) => runner(&wt, model, &prompt_for(model)),
                        None => Err(anyhow::anyhow!("worktree setup failed")),
                    };
                    MemberText {
                        model: model.clone(),
                        text,
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("council stage thread panicked"))
            .collect()
    })
}

// --- prompts -------------------------------------------------------------

fn draft_prompt(task: &str) -> String {
    format!(
        "You are one member of a council of AI models working a task together. \
         Independently draft a concise, concrete plan to accomplish the task \
         below. Do not modify any files; output only your plan.\n\nTASK:\n{task}"
    )
}

fn critique_prompt(task: &str, model: &str, drafts: &str) -> String {
    format!(
        "You are member \"{model}\" of a council. Below are every member's \
         independent plans for the same task. Critique them: name the strongest \
         idea in each plan, call out gaps or risks, and say which parts should be \
         combined into the best overall approach. Do not modify any files; output \
         only your critique.\n\nTASK:\n{task}\n\nPLANS:\n{drafts}"
    )
}

fn synthesis_prompt(task: &str, drafts: &str, critiques: &str) -> String {
    format!(
        "You are the council's synthesizer. Using the members' plans and their \
         critiques of each other, produce a single joint plan that takes the best \
         idea from each, resolves the disagreements, and is ready to act on. \
         Output only the final joint plan.\n\nTASK:\n{task}\n\nPLANS:\n{drafts}\n\n\
         CRITIQUES:\n{critiques}"
    )
}

/// Render a stage's outputs as labelled blocks for the next stage's prompt.
fn format_block(texts: &[MemberText]) -> String {
    texts
        .iter()
        .map(|m| {
            let body = match &m.text {
                Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
                Ok(_) => "(no output)".to_string(),
                Err(e) => format!("(failed: {e})"),
            };
            format!("### {}\n{}", m.model, body)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

// --- the production runner ------------------------------------------------

/// Spawn `blaude run --json --model <model> <prompt>` in `worktree` (where `exe`
/// is the current blaude binary) and return the model's text answer.
pub fn spawn_member(exe: &Path, worktree: &Path, model: &str, prompt: &str) -> Result<String> {
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
    let json_start = stdout.find('{').unwrap_or(0);
    let report: serde_json::Value = serde_json::from_str(stdout[json_start..].trim())
        .with_context(|| format!("parsing run report for {model}"))?;
    Ok(report
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string())
}

// --- git helpers ---------------------------------------------------------

/// The git repository root containing `dir` — used by the TUI, whose process cwd
/// may not be the session's working directory.
pub fn git_repo_root_from(dir: &Path) -> Result<PathBuf> {
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

/// The current HEAD commit SHA of `repo_root`.
pub fn git_head_sha(repo_root: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .context("reading HEAD")?;
    if !out.status.success() {
        bail!("the repository has no commits yet — a council needs a base commit");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn worktree_path(repo_root: &Path, index: usize, model: &str) -> PathBuf {
    repo_root
        .join(".git")
        .join("blaude-councils")
        .join(format!("{}-{index}", sanitize(model)))
}

fn add_worktree(repo_root: &Path, worktree: &Path, base_sha: &str) -> Result<()> {
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

fn remove_worktree(repo_root: &Path, worktree: &Path) -> Result<()> {
    let out = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .current_dir(repo_root)
        .output()
        .context("git worktree remove")?;
    if !out.status.success() {
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
pub fn sanitize(model: &str) -> String {
    model
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Truncate to `max` chars with an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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
    }

    #[test]
    fn deliberation_runs_draft_then_critique_then_synthesis() {
        let repo = init_repo();
        let root = repo.path().to_path_buf();
        let base = git_head_sha(&root).unwrap();
        let members = vec!["alpha".to_string(), "beta".to_string()];

        // The stub returns a tag per stage, and the critique/synthesis prompts
        // must have seen the earlier stages' outputs (they carry "### alpha").
        let calls = Mutex::new(Vec::<String>::new());
        let runner = |_wt: &Path, model: &str, prompt: &str| -> Result<String> {
            calls.lock().unwrap().push(model.to_string());
            if prompt.contains("output only your plan") {
                Ok(format!("{model} draft"))
            } else if prompt.contains("only your critique") {
                assert!(prompt.contains("### alpha"), "critique sees drafts");
                Ok(format!("{model} critique"))
            } else {
                assert!(prompt.contains("### alpha"), "synthesis sees drafts");
                assert!(prompt.contains("draft"), "synthesis sees draft text");
                Ok("JOINT PLAN".to_string())
            }
        };

        let d = deliberate(&root, &base, &members, "do the thing", false, &runner);
        assert_eq!(d.drafts.len(), 2);
        assert_eq!(d.critiques.len(), 2);
        assert_eq!(d.synthesizer, "alpha");
        assert_eq!(d.joint_plan.unwrap(), "JOINT PLAN");
        assert!(
            d.drafts
                .iter()
                .all(|m| m.text.as_ref().unwrap().contains("draft"))
        );
        // 2 drafts + 2 critiques + 1 synthesis = 5 model calls.
        assert_eq!(calls.lock().unwrap().len(), 5);
        // Worktrees cleaned up (not kept).
        let wt_dir = root.join(".git").join("blaude-councils");
        let leftover = std::fs::read_dir(&wt_dir)
            .map(|rd| rd.flatten().filter(|e| e.path().is_dir()).count())
            .unwrap_or(0);
        assert_eq!(leftover, 0);
    }

    #[test]
    fn a_failed_member_does_not_sink_the_deliberation() {
        let repo = init_repo();
        let root = repo.path().to_path_buf();
        let base = git_head_sha(&root).unwrap();
        let members = vec!["ok".to_string(), "bad".to_string()];
        let runner = |_wt: &Path, model: &str, _p: &str| -> Result<String> {
            if model == "bad" {
                bail!("model exploded")
            } else {
                Ok(format!("{model} output"))
            }
        };
        let d = deliberate(&root, &base, &members, "task", false, &runner);
        // The bad member's draft is an error, but the deliberation still yields
        // drafts, critiques, and a joint plan (synthesizer is the ok member).
        assert_eq!(d.drafts.len(), 2);
        assert!(d.drafts.iter().any(|m| m.text.is_err()));
        assert_eq!(d.synthesizer, "ok");
        assert!(d.joint_plan.is_ok());
    }
}
