//! `blaude-tools` — the helper CLI. Post-pivot it does one thing: index the
//! repo graph and refresh the AGENTS.md / CLAUDE.md brief. Wallet, hop, and the
//! council now live in the self-contained `blaude` agent (the jcode fork).

mod brief;
mod prune;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "blaude-tools",
    about = "blaude helper CLI (the agent is the separate `blaude` binary)",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index the repo graph and refresh AGENTS.md / CLAUDE.md briefing
    Brief {
        /// Only report index freshness; do not re-index or rewrite files
        #[arg(long)]
        check: bool,
    },
    /// List dead-code candidates (functions the graph shows no callers for)
    #[command(visible_alias = "dead")]
    Prune {
        /// Emit JSON for agents instead of a human table
        #[arg(long)]
        json: bool,
        /// Print only the per-bucket counts, no per-symbol listings
        #[arg(long)]
        summary: bool,
    },
}

fn cmd_brief(check: bool) -> Result<()> {
    let r = brief::run(check)?;
    println!("repo: {}", r.root.display());
    for line in r.status.lines().filter(|l| !l.trim().is_empty()) {
        println!("  {line}");
    }
    if check {
        return Ok(());
    }
    if r.written.is_empty() {
        println!("briefing: already current");
    } else {
        println!(
            "briefing rewritten (CLI mode, no MCP): {}",
            r.written.join(", ")
        );
    }
    if !r.gitignored.is_empty() {
        println!("gitignored: {}", r.gitignored.join(", "));
    }
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Brief { check } => cmd_brief(check),
        Cmd::Prune { json, summary } => prune::run(json, summary),
    }
}
