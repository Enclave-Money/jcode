//! Councils — blaude's cross-model panels, and their CRUD + persistence.
//!
//! A *council* is a named set of 2–3 models you can pick from `/model` in place
//! of a single model; when run, blaude fans the same prompt out to every member
//! and shows their proposals side by side (the fan-out itself lives in the agent
//! runtime — this module owns only the definitions and their storage).
//!
//! Kept deliberately self-contained and additive: it lives in its own file with
//! its own on-disk state (`~/.jcode/councils.json`) rather than extending an
//! upstream config struct, so pulling upstream jcode stays a clean merge.

use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{jcode_dir, read_json, write_json};

/// Fewest models a council can hold — a council of one is just a model.
pub const MIN_MEMBERS: usize = 2;
/// Most models a council can hold, for now (kept small so the fan-out and its
/// side-by-side display stay legible).
pub const MAX_MEMBERS: usize = 3;

/// One named council: a label plus the model ids it fans out to. Model ids are
/// whatever `/model` uses (e.g. `claude-opus-4-8`, `openai:gpt-5-codex`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Council {
    pub name: String,
    pub members: Vec<String>,
}

impl Council {
    /// Build a validated council. Fails on a blank name, too few / too many
    /// members, or duplicate members.
    pub fn new(name: impl Into<String>, members: Vec<String>) -> Result<Self> {
        let council = Council {
            name: name.into(),
            members,
        };
        council.validate()?;
        Ok(council)
    }

    fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("a council needs a name");
        }
        if self.members.len() < MIN_MEMBERS {
            bail!(
                "a council needs at least {MIN_MEMBERS} models (got {})",
                self.members.len()
            );
        }
        if self.members.len() > MAX_MEMBERS {
            bail!(
                "a council can hold at most {MAX_MEMBERS} models (got {})",
                self.members.len()
            );
        }
        if self.members.iter().any(|m| m.trim().is_empty()) {
            bail!("a council member cannot be blank");
        }
        // Duplicate members would just waste a slot on the same model.
        let mut seen = std::collections::HashSet::new();
        for m in &self.members {
            if !seen.insert(m.as_str()) {
                bail!("duplicate model in council: {m}");
            }
        }
        Ok(())
    }
}

/// The full set of saved councils. Names are unique, case-insensitively, so the
/// picker never shows two rows a user can't tell apart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Councils {
    #[serde(default)]
    pub councils: Vec<Council>,
}

impl Councils {
    /// The on-disk location, `~/.jcode/councils.json` (respecting `JCODE_HOME`).
    pub fn path() -> Result<PathBuf> {
        Ok(jcode_dir()?.join("councils.json"))
    }

    /// Load the saved councils, or an empty set if none have been created yet.
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }

    /// Persist to disk (atomically, via the storage crate's writer).
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    /// Load from an explicit path (an empty set if it doesn't exist). The seam
    /// `load`/`save` use, and what tests exercise so they never touch the real
    /// `~/.jcode` or race on the `JCODE_HOME` env var.
    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        read_json(path)
    }

    /// Persist to an explicit path (atomically).
    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        write_json(path, self)
    }

    pub fn is_empty(&self) -> bool {
        self.councils.is_empty()
    }

    pub fn len(&self) -> usize {
        self.councils.len()
    }

    /// Look a council up by name (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&Council> {
        self.councils
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.councils
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    // --- CRUD -------------------------------------------------------------

    /// Create a council. Fails if the name is already taken or the council is
    /// invalid (see [`Council::new`]).
    pub fn create(&mut self, name: impl Into<String>, members: Vec<String>) -> Result<&Council> {
        let council = Council::new(name, members)?;
        if self.position(&council.name).is_some() {
            bail!("a council named “{}” already exists", council.name);
        }
        self.councils.push(council);
        Ok(self.councils.last().expect("just pushed"))
    }

    /// Rename a council. Fails if `from` doesn't exist or `to` is taken by a
    /// different council.
    pub fn rename(&mut self, from: &str, to: impl Into<String>) -> Result<()> {
        let to = to.into();
        if to.trim().is_empty() {
            bail!("a council needs a name");
        }
        // Guard against colliding with a *different* council (a no-op rename to
        // the same name, modulo case, is fine).
        if let Some(existing) = self.position(&to) {
            if self.position(from) != Some(existing) {
                bail!("a council named “{to}” already exists");
            }
        }
        let idx = self
            .position(from)
            .ok_or_else(|| anyhow::anyhow!("no council named “{from}”"))?;
        self.councils[idx].name = to;
        Ok(())
    }

    /// Replace a council's members (validated). Fails if it doesn't exist.
    pub fn set_members(&mut self, name: &str, members: Vec<String>) -> Result<()> {
        let idx = self
            .position(name)
            .ok_or_else(|| anyhow::anyhow!("no council named “{name}”"))?;
        // Validate against a candidate before committing, so a bad update leaves
        // the existing council untouched.
        let candidate = Council::new(self.councils[idx].name.clone(), members)?;
        self.councils[idx] = candidate;
        Ok(())
    }

    /// Delete a council by name. Returns whether one was removed.
    pub fn delete(&mut self, name: &str) -> bool {
        match self.position(name) {
            Some(idx) => {
                self.councils.remove(idx);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members(ms: &[&str]) -> Vec<String> {
        ms.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_council_needs_two_to_three_distinct_named_models() {
        assert!(
            Council::new("", members(&["a", "b"])).is_err(),
            "blank name"
        );
        assert!(Council::new("solo", members(&["a"])).is_err(), "too few");
        assert!(
            Council::new("crowd", members(&["a", "b", "c", "d"])).is_err(),
            "too many"
        );
        assert!(
            Council::new("dupes", members(&["a", "a"])).is_err(),
            "duplicates"
        );
        assert!(Council::new("pair", members(&["a", "b"])).is_ok());
        assert!(Council::new("trio", members(&["a", "b", "c"])).is_ok());
    }

    #[test]
    fn crud_round_trips() {
        let mut cs = Councils::default();
        cs.create(
            "claude+codex",
            members(&["claude-opus-4-8", "openai:gpt-5-codex"]),
        )
        .unwrap();
        assert_eq!(cs.len(), 1);

        // Duplicate name (case-insensitive) is refused.
        assert!(cs.create("Claude+Codex", members(&["a", "b"])).is_err());

        // Update members (validated).
        cs.set_members(
            "claude+codex",
            members(&["claude-opus-4-8", "openai:gpt-5-codex", "gemini-3-pro"]),
        )
        .unwrap();
        assert_eq!(cs.get("claude+codex").unwrap().members.len(), 3);
        assert!(
            cs.set_members("claude+codex", members(&["only-one"]))
                .is_err(),
            "an invalid update is rejected and leaves the council intact"
        );
        assert_eq!(cs.get("claude+codex").unwrap().members.len(), 3);

        // Rename, then look up under the new name.
        cs.rename("claude+codex", "dream team").unwrap();
        assert!(cs.get("claude+codex").is_none());
        assert!(cs.get("DREAM TEAM").is_some(), "lookup is case-insensitive");

        // Delete.
        assert!(cs.delete("dream team"));
        assert!(!cs.delete("dream team"), "second delete is a no-op");
        assert!(cs.is_empty());
    }

    #[test]
    fn save_and_load_round_trip_through_disk() {
        // An explicit path (not JCODE_HOME) keeps this hermetic and off the
        // global env, so it never races the other storage tests.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("councils.json");

        let mut cs = Councils::load_from(&path).unwrap();
        assert!(cs.is_empty(), "nothing saved yet");
        cs.create("pair", members(&["a", "b"])).unwrap();
        cs.save_to(&path).unwrap();

        let reloaded = Councils::load_from(&path).unwrap();
        assert_eq!(reloaded, cs);
        assert!(reloaded.get("pair").is_some());
    }
}
