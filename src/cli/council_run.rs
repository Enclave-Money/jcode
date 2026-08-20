//! `blaude council run` — the CLI front-end for the cross-vendor fan-out.
//!
//! The orchestration (worktrees, diffs, the runner) lives in
//! [`jcode_storage::council_run`] so the interactive TUI council mode reuses it;
//! this file just loads the council, wires the production runner, and prints.

use std::path::Path;

use anyhow::{Context, Result};

use jcode_storage::council_run::{
    diff_file_count, fan_out, git_head_sha, git_repo_root, spawn_member, truncate, MemberOutcome,
};
use jcode_storage::councils::Councils;

/// Load the council, resolve the repo, fan the prompt out, and print proposals.
pub(crate) fn run(name: &str, prompt: &str, keep: bool) -> Result<()> {
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

    let runner =
        |worktree: &Path, model: &str, prompt: &str| spawn_member(&exe, worktree, model, prompt);

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
            println!("\n{} file(s) changed:", diff_file_count(&o.diff));
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
