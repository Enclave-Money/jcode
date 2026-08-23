//! Shared `JCODE_HOME` sandbox for this crate's test binary.
//!
//! ## The problem
//!
//! Many server/agent tests build real `Agent`/`Session` values and let them
//! persist. A session save resolves its directory through
//! `storage::jcode_dir()`, which falls back to the developer's actual
//! `~/.jcode` when `JCODE_HOME` is unset. Tests that never set a sandbox home
//! therefore litter the real `~/.jcode/sessions` with throwaway transcripts.
//!
//! ## The fix
//!
//! The isolation mechanism already exists: `crate::env::set_var("JCODE_HOME",
//! ..)` mirrors into a per-thread override that the storage resolvers consult
//! before the process env var (see `jcode-core/src/env.rs` and `jcode_home_env`
//! in `jcode-storage/src/lib.rs`). Rather than repeat a tempdir block in every
//! offending test, the test binary installs one shared sandbox home *before the
//! first test runs* (see the `#[ctor]` below). The mechanism is untouched, so a
//! daemon build (even a dev build that enables `test-support`) still resolves
//! the real home exactly as before.
//!
//! Two properties make a single shared home the right shape here:
//!
//! * **Idempotent, so it cannot stomp a parallel test.** Every caller sets the
//!   same path, unlike per-test tempdirs racing on a process-global env var.
//!   Tests that need a *private* home keep setting their own; their per-thread
//!   override still takes precedence over this process-wide default.
//! * **Covers tokio worker threads.** The override is thread-local, so a
//!   `#[tokio::test(flavor = "multi_thread")]` body that persists from a runtime
//!   worker would resolve the real home on that thread. Installing the sandbox
//!   in the process env var too means worker threads, which have no override of
//!   their own, fall back to the sandbox rather than to `~/.jcode`.
//!
//! The directory lives under the system temp dir and is intentionally left in
//! place: it stays inspectable while debugging a failure, and the OS reclaims it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Install the shared sandbox home before the first test in this binary runs.
///
/// `#[ctor]` runs at binary load, i.e. before libtest spawns any test thread,
/// so the env var is in place for every test including ones that persist from
/// tokio worker threads. Gated on `cfg(test)` and backed by a dev-dependency,
/// so it exists only in this crate's own unit-test binary.
#[cfg(test)]
#[ctor::ctor(unsafe)]
fn install_shared_test_home() {
    use_shared_test_home();
}

/// Point this thread (and, as a fallback, the whole process) at the shared test
/// `JCODE_HOME` sandbox.
///
/// Idempotent and cheap. The `#[ctor]` above already calls this for the unit-test
/// binary; call it explicitly from integration tests or fixtures that need the
/// guarantee on a thread of their own.
pub fn use_shared_test_home() -> &'static Path {
    let home = shared_test_home();
    // Mirrors into the per-thread override *and* the process env var. The
    // latter is what tokio worker threads resolve through.
    crate::env::set_var("JCODE_HOME", home);
    home
}

/// Path of the shared per-process test home, creating it on first use.
pub fn shared_test_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("jcode-test-home-{}", std::process::id()));
        std::fs::create_dir_all(base.join("sessions"))
            .expect("create shared test JCODE_HOME sandbox");
        base
    })
    .as_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The developer's real `~/.jcode`, or `None` if it cannot be determined.
    fn real_jcode_home() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(".jcode"))
    }

    /// The invariant that matters: a thread that never set a home of its own
    /// still resolves somewhere other than the developer's real `~/.jcode`.
    ///
    /// Deliberately *not* an equality assertion against [`shared_test_home`].
    /// Tests run in parallel and many legitimately point the process-global
    /// `JCODE_HOME` at their own tempdir, so the exact sandbox observed here is
    /// racy. What must never be observed is the real home. This is the
    /// regression guard for `cargo test -p jcode-app-core` littering
    /// `~/.jcode/sessions`.
    #[test]
    fn default_home_is_never_the_real_jcode_home() {
        let dir = crate::storage::jcode_dir().expect("resolve jcode dir");
        if let Some(real) = real_jcode_home() {
            assert_ne!(
                dir, real,
                "test resolved the developer's real ~/.jcode; a test fixture \
                 unset JCODE_HOME instead of restoring a sandbox"
            );
        }
    }

    /// Same guarantee on a tokio multi-thread runtime's worker threads, which
    /// have no thread-local override of their own and so resolve through the
    /// process env var.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sandbox_applies_on_tokio_worker_threads() {
        let dir = tokio::task::spawn_blocking(|| crate::storage::jcode_dir().expect("resolve"))
            .await
            .expect("join");
        if let Some(real) = real_jcode_home() {
            assert_ne!(dir, real, "tokio worker thread resolved the real ~/.jcode");
        }
    }

    /// A thread that opts in explicitly gets exactly the shared sandbox, and
    /// its per-thread override wins over whatever other tests do to the env.
    #[test]
    fn explicit_opt_in_resolves_to_the_shared_sandbox() {
        let expected = use_shared_test_home();
        assert_eq!(
            crate::storage::jcode_dir().expect("resolve jcode dir"),
            expected
        );
    }
}
