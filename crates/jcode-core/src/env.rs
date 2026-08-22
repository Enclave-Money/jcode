use std::ffi::OsStr;

/// Per-thread override for `JCODE_HOME`, active only under test builds.
///
/// `JCODE_HOME` is a process-global OS env var, so a `set_var` on one test
/// thread is instantly visible to every other thread. Under the default
/// (parallel) test runner that lets one test's home stomp another's mid-read —
/// e.g. a reload-reconnect test reads its sandbox home while a concurrent test
/// points `JCODE_HOME` somewhere else, and the read lands in the wrong dir.
///
/// The OS env var cannot be made per-thread, but blaude's *own* home
/// resolution can: every home write flows through [`set_var`]/[`remove_var`]
/// here, so we mirror `JCODE_HOME` into a thread-local that the storage
/// resolvers (`jcode_dir`, `app_config_dir`, `user_home_path`) consult first.
/// A thread that set its own home then reads its own home regardless of what
/// other threads do to the env var — no lock, no cross-test interference.
///
/// This is gated on `test-support`/`test` and never compiled into release
/// builds, where home resolution reads the env var directly as before. The
/// override mirrors the env exactly (set on write, cleared on remove), so a
/// thread that never set a home falls back to the env var, matching production.
#[cfg(any(test, feature = "test-support"))]
mod test_home_override {
    use std::cell::RefCell;
    use std::ffi::{OsStr, OsString};

    thread_local! {
        static HOME: RefCell<Option<OsString>> = const { RefCell::new(None) };
    }

    pub(super) fn set(value: &OsStr) {
        HOME.with(|home| *home.borrow_mut() = Some(value.to_os_string()));
    }

    pub(super) fn clear() {
        HOME.with(|home| *home.borrow_mut() = None);
    }

    /// The current thread's `JCODE_HOME` override, if it set one.
    pub fn get() -> Option<OsString> {
        HOME.with(|home| home.borrow().clone())
    }
}

/// The current thread's `JCODE_HOME` override (test builds only). Storage home
/// resolvers consult this before the process env var so parallel tests stay
/// isolated. Returns `None` on threads that never set a home.
#[cfg(any(test, feature = "test-support"))]
pub fn jcode_home_thread_override() -> Option<std::ffi::OsString> {
    test_home_override::get()
}

#[cfg(any(test, feature = "test-support"))]
fn is_jcode_home(key: &OsStr) -> bool {
    key == OsStr::new("JCODE_HOME")
}

/// Mutate the process environment for blaude runtime configuration.
///
/// Rust 2024 makes environment mutation unsafe because it can race with
/// concurrent environment access in foreign code. blaude intentionally mutates
/// process-local env vars to coordinate provider/runtime bootstrap before or
/// during task execution. We centralize that unsafety here so call sites remain
/// auditable.
pub fn set_var<K, V>(key: K, value: V)
where
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    // Mirror JCODE_HOME into the per-thread override so parallel tests resolve
    // their own home even when another thread changes the process env var.
    #[cfg(any(test, feature = "test-support"))]
    if is_jcode_home(key.as_ref()) {
        test_home_override::set(value.as_ref());
    }
    // SAFETY: blaude treats these mutations as process-global configuration.
    // They are a pre-existing design choice used throughout startup, auth,
    // provider bootstrap, tests, and self-dev flows. Centralizing the unsafe
    // operation here makes the Rust 2024 requirement explicit without
    // scattering unsafe blocks across hundreds of call sites.
    unsafe {
        std::env::set_var(key, value);
    }
}

/// Remove a process environment variable used by blaude runtime configuration.
pub fn remove_var<K>(key: K)
where
    K: AsRef<OsStr>,
{
    // Keep the per-thread override in lockstep with the env var it mirrors.
    #[cfg(any(test, feature = "test-support"))]
    if is_jcode_home(key.as_ref()) {
        test_home_override::clear();
    }
    // SAFETY: see `set_var` above; this is the corresponding centralized
    // removal operation for the same process-global configuration surface.
    unsafe {
        std::env::remove_var(key);
    }
}
