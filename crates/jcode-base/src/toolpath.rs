//! PATH widening for child processes spawned by a GUI-launched daemon.
//!
//! launchd hands app bundles the minimal `/usr/bin:/bin:/usr/sbin:/sbin`, so
//! anything the daemon shells out to (`npx`-based MCP servers, `blaude-tools`,
//! the node runtime behind the code-graph re-index) fails with "No such file
//! or directory" even though the same command works in the user's terminal.
//! These helpers append the well-known user tool dirs — never prepend, so a
//! command resolvable on the original PATH keeps resolving to the same binary.

use std::path::PathBuf;

/// Directories where user-installed tool runners (npx, uvx, node) commonly
/// live but which the minimal PATH of a GUI-launched daemon lacks, filtered
/// to the ones that exist on this machine.
pub fn existing_tool_dirs() -> Vec<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(std::path::Path::new(&home).join(".local").join("bin"));
    }
    candidates.retain(|dir| dir.is_dir());
    candidates
}

/// Append `extra` directories to a PATH string, skipping ones already present.
/// Append — never prepend — so a command resolvable on the original PATH keeps
/// resolving to the same binary.
pub fn widen_path(current: &str, extra: &[PathBuf]) -> String {
    let mut parts: Vec<PathBuf> = std::env::split_paths(current).collect();
    for dir in extra {
        if !parts.iter().any(|p| p == dir) {
            parts.push(dir.clone());
        }
    }
    match std::env::join_paths(&parts) {
        Ok(joined) => joined.to_string_lossy().into_owned(),
        Err(_) => current.to_string(),
    }
}

/// This process's own PATH, widened with the existing tool dirs. What a child
/// that needs user-installed tools should get as its PATH.
pub fn widened_env_path() -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    widen_path(&current, &existing_tool_dirs())
}

/// Find `name` on the widened PATH, if it exists anywhere there.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let widened = widened_env_path();
    std::env::split_paths(&widened)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widen_path_appends_missing_dirs_without_reordering() {
        let extra = [PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/bin")];
        let widened = widen_path("/bin:/usr/bin", &extra);
        assert_eq!(widened, "/bin:/usr/bin:/opt/homebrew/bin");
    }
}
