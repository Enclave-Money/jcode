//! `blaude council run` — the CLI front-end for a council deliberation.
//!
//! The orchestration (draft, critique, synthesize) lives in
//! [`jcode_storage::council_run`] so the interactive TUI reuses it; this file
//! loads the council, wires the production runner, and prints the deliberation.

use std::path::Path;

use anyhow::{Context, Result};

use jcode_storage::council_run::{
    Deliberation, MemberText, deliberate, git_head_sha, git_repo_root, spawn_member, truncate,
};
use jcode_storage::councils::Councils;

/// Load the council, resolve the repo, deliberate, and print the joint plan.
pub(crate) fn run(name: &str, prompt: &str, keep: bool) -> Result<()> {
    let councils = Councils::load()?;
    let council = councils
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("no council named “{name}” (see `blaude council list`)"))?
        .clone();

    // Council reads the code and hands back text, so it runs in any directory.
    // A git repository used to be required purely to host one worktree per
    // member, which failed a council run in a plain workspace.
    let repo_root = git_repo_root()
        .or_else(|_| std::env::current_dir())
        .context("locating the workspace directory")?;
    let base_sha = git_head_sha(&repo_root).unwrap_or_default();
    let exe = std::env::current_exe().context("locating the blaude binary")?;

    let from = if base_sha.is_empty() {
        String::new()
    } else {
        format!(" from {}", &base_sha[..base_sha.len().min(12)])
    };
    println!(
        "Council “{}”: {} models deliberating on “{}”{}…\n\
         (draft independently → critique each other → synthesize a joint plan)\n",
        council.name,
        council.members.len(),
        truncate(prompt, 60),
        from
    );

    let runner =
        |worktree: &Path, model: &str, prompt: &str| spawn_member(&exe, worktree, model, prompt);

    let d = deliberate(
        &repo_root,
        &base_sha,
        &council.members,
        prompt,
        keep,
        &runner,
        &|p| {
            if p.done {
                eprintln!(
                    "  {} {} finished {}",
                    if p.ok { "✓" } else { "✗" },
                    p.model,
                    p.phase
                );
            } else {
                eprintln!("  ⚖ {} {}…", p.model, p.phase);
            }
        },
    );
    report(&d);
    Ok(())
}

fn report(d: &Deliberation) {
    let show = |label: &str, items: &[MemberText]| {
        for m in items {
            println!("━━━ {label}: {} ━━━", m.model);
            match &m.text {
                Ok(t) if !t.trim().is_empty() => println!("{}", t.trim()),
                Ok(_) => println!("(no output)"),
                Err(e) => println!("⚠ failed: {e}"),
            }
            println!();
        }
    };

    show("draft", &d.drafts);
    show("critique", &d.critiques);

    println!("═══ joint plan (synthesized by {}) ═══", d.synthesizer);
    match &d.joint_plan {
        Ok(t) if !t.trim().is_empty() => println!("{}", t.trim()),
        Ok(_) => println!("(no joint plan produced)"),
        Err(e) => println!("⚠ synthesis failed: {e}"),
    }

    if !d.worktrees.is_empty() {
        println!("\nworktrees kept:");
        for wt in &d.worktrees {
            println!("  {}", wt.display());
        }
    }
}
