#![cfg_attr(test, allow(clippy::items_after_test_module))]

pub use jcode_storage::*;

use anyhow::Result;
use serde::de::DeserializeOwned;
use std::path::Path;

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    jcode_storage::read_json_with_recovery_handler(path, |event| match event {
        jcode_storage::StorageRecoveryEvent::CorruptPrimary { path, error } => {
            crate::logging::warn(&format!(
                "Corrupt JSON at {}, trying backup: {}",
                path.display(),
                error
            ));
        }
        jcode_storage::StorageRecoveryEvent::RecoveredFromBackup { backup_path } => {
            crate::logging::info(&format!("Recovered from backup: {}", backup_path.display()));
        }
    })
}

#[cfg(any(test, feature = "test-support"))]
use std::sync::{Mutex, MutexGuard, OnceLock};

#[cfg(any(test, feature = "test-support"))]
pub fn test_env_lock() -> &'static Mutex<()> {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEST_ENV_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TEST_ENV_HELD_GUARD: std::cell::RefCell<Option<MutexGuard<'static, ()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Reentrant guard for the process-wide test serialization lock.
///
/// The same thread may nest `lock_test_env()` calls (for example a rendering
/// test that holds the lock and then builds an app, whose helpers also lock the
/// same shared state). A plain `Mutex` would self-deadlock on the second take,
/// and two globals with inconsistent acquisition order deadlock across threads.
/// Making this the single reentrant lock behind both env-state and render-state
/// test serialization removes both hazards. Only the outermost guard on a thread
/// actually holds the underlying mutex; nested guards are no-ops on drop.
#[cfg(any(test, feature = "test-support"))]
pub struct TestEnvGuard {
    // Not `Send`: the depth/held-guard bookkeeping is per-thread, so a guard
    // must be dropped on the same thread that created it.
    _not_send: std::marker::PhantomData<*const ()>,
}

#[cfg(any(test, feature = "test-support"))]
pub fn lock_test_env() -> TestEnvGuard {
    let depth = TEST_ENV_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    if depth == 0 {
        let guard = test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        TEST_ENV_HELD_GUARD.with(|slot| *slot.borrow_mut() = Some(guard));
    }
    TestEnvGuard {
        _not_send: std::marker::PhantomData,
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestEnvGuard {
    fn drop(&mut self) {
        let new_depth = TEST_ENV_DEPTH.with(|d| {
            let v = d.get().saturating_sub(1);
            d.set(v);
            v
        });
        if new_depth == 0 {
            TEST_ENV_HELD_GUARD.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
}

#[cfg(test)]
mod tests;
