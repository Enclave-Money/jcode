//! `blaude council` — manage cross-model councils from the CLI.
//!
//! A thin front-end over [`jcode_storage::councils`]: parse the subcommand, do
//! the CRUD, persist, and print. The definitions and validation live in the
//! storage crate so the TUI `/model` picker can reuse them.

use anyhow::Result;

use jcode_storage::councils::{Councils, MAX_MEMBERS, MIN_MEMBERS};

use super::args::CouncilCommand;

pub(crate) fn run(cmd: CouncilCommand) -> Result<()> {
    match cmd {
        CouncilCommand::List { json } => list(json),
        CouncilCommand::Show { name } => show(&name),
        CouncilCommand::Create { name, models } => create(name, models),
        CouncilCommand::Rename { from, to } => rename(&from, to),
        CouncilCommand::SetMembers { name, models } => set_members(&name, models),
        CouncilCommand::Delete { name } => delete(&name),
        CouncilCommand::Run { name, prompt, keep } => super::council_run::run(&name, &prompt, keep),
    }
}

fn list(json: bool) -> Result<()> {
    let councils = Councils::load()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&councils)?);
        return Ok(());
    }
    if councils.is_empty() {
        println!(
            "No councils yet. Create one with:\n  \
             blaude council create <name> <model-a> <model-b> [model-c]\n\n\
             A council is {MIN_MEMBERS}–{MAX_MEMBERS} models blaude fans a prompt out to at once."
        );
        return Ok(());
    }
    println!("Councils ({}):", councils.len());
    for c in &councils.councils {
        println!("  {}  —  {}", c.name, c.members.join(", "));
    }
    Ok(())
}

fn show(name: &str) -> Result<()> {
    let councils = Councils::load()?;
    match councils.get(name) {
        Some(c) => {
            println!("{}", c.name);
            for m in &c.members {
                println!("  · {m}");
            }
            Ok(())
        }
        None => {
            anyhow::bail!("no council named “{name}” (see `blaude council list`)");
        }
    }
}

fn create(name: String, models: Vec<String>) -> Result<()> {
    let mut councils = Councils::load()?;
    let created = councils.create(&name, models)?;
    let summary = format!("{}  —  {}", created.name, created.members.join(", "));
    councils.save()?;
    println!("Created council {summary}");
    Ok(())
}

fn rename(from: &str, to: String) -> Result<()> {
    let mut councils = Councils::load()?;
    councils.rename(from, &to)?;
    councils.save()?;
    println!("Renamed council “{from}” → “{to}”");
    Ok(())
}

fn set_members(name: &str, models: Vec<String>) -> Result<()> {
    let mut councils = Councils::load()?;
    councils.set_members(name, models)?;
    let summary = councils
        .get(name)
        .map(|c| c.members.join(", "))
        .unwrap_or_default();
    councils.save()?;
    println!("Updated council “{name}” → {summary}");
    Ok(())
}

fn delete(name: &str) -> Result<()> {
    let mut councils = Councils::load()?;
    if councils.delete(name) {
        councils.save()?;
        println!("Deleted council “{name}”");
        Ok(())
    } else {
        anyhow::bail!("no council named “{name}” (see `blaude council list`)");
    }
}
