use std::sync::{LazyLock, RwLock};

use jcode_provider_metadata::{is_safe_env_file_name, is_safe_env_key_name};

/// Fallback resolvers consulted by [`load_api_key_from_env_or_config`] after the
/// environment and config-file lookups fail. Higher-level crates register
/// resolvers at startup so this leaf crate does not need to depend on auth.
type ApiKeyFallbackResolver = fn(&str) -> Option<String>;

static API_KEY_FALLBACK_RESOLVERS: LazyLock<RwLock<Vec<ApiKeyFallbackResolver>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a fallback API-key resolver consulted when env/config lookups miss.
pub fn register_api_key_fallback_resolver(resolver: ApiKeyFallbackResolver) {
    API_KEY_FALLBACK_RESOLVERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(resolver);
}

fn resolve_api_key_fallback(env_key: &str) -> Option<String> {
    let resolvers = API_KEY_FALLBACK_RESOLVERS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for resolver in resolvers.iter() {
        if let Some(key) = resolver(env_key) {
            return Some(key);
        }
    }
    None
}

/// Characters that editors, terminals, and `cat` render invisibly but that
/// corrupt a credential when embedded in it. Rust's [`str::trim`] only removes
/// ASCII whitespace, so these survive a plain trim and silently break auth
/// (see GitHub issue #376). [`char::is_whitespace`] covers Unicode White_Space
/// (NBSP U+00A0, the en/em spaces U+2002-U+200A, line/paragraph separators,
/// etc.); the explicit cases below are zero-width characters and the BOM, which
/// are not classified as whitespace.
fn is_invisible_boundary_char(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '\u{200B}' // zero-width space
                | '\u{200C}' // zero-width non-joiner
                | '\u{200D}' // zero-width joiner
                | '\u{2060}' // word joiner
                | '\u{FEFF}' // BOM / zero-width no-break space
        )
}

/// Strip leading/trailing invisible (Unicode whitespace and zero-width)
/// characters and one optional layer of surrounding quotes from a loaded
/// secret or config value.
///
/// Exposed so other credential loaders (e.g. the Cursor key reader) can apply
/// the same sanitizing as [`load_api_key_from_env_or_config`].
pub fn sanitize_secret_value(raw: &str) -> &str {
    raw.trim_matches(is_invisible_boundary_char)
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches(is_invisible_boundary_char)
}

/// Sanitize a loaded value and surface a warning when Unicode invisible
/// characters were present, so the failure mode in issue #376 is no longer
/// silent. Returns `None` for values that are empty after sanitizing.
fn clean_loaded_value(raw: &str, env_key: &str) -> Option<String> {
    let cleaned = sanitize_secret_value(raw);
    if cleaned.is_empty() {
        return None;
    }
    // A plain ASCII trim is what we previously did; if it leaves a different
    // result than the Unicode-aware sanitize, hidden characters were stripped.
    let ascii_only = raw.trim().trim_matches('"').trim_matches('\'').trim();
    if ascii_only != cleaned {
        jcode_logging::warn(&format!(
            "Stripped Unicode invisible or non-ASCII whitespace characters from '{}' while loading credentials; verify the value contains no hidden characters",
            env_key
        ));
    }
    Some(cleaned.to_string())
}

/// Whether the runtime may read an AI credential from the PROCESS ENVIRONMENT.
///
/// False when `JCODE_EXPLICIT_ACCOUNTS_ONLY` is set. The blaude app sets it on
/// its local runtime so a turn uses only accounts a person added through the
/// app — never an `ANTHROPIC_API_KEY` / `OPENROUTER_API_KEY` that happens to
/// sit in the login environment. Without it, `--provider auto` silently picks
/// up an ambient key and bills it, with the account list still showing zero.
///
/// Credentials added through the app live in FILES (OAuth tokens in auth.json,
/// pasted keys in the config dir), which this never gates — only the direct
/// `std::env::var` reads are skipped, so the file lookups below still run.
pub fn ambient_env_credentials_allowed() -> bool {
    let env_says_no = std::env::var("JCODE_EXPLICIT_ACCOUNTS_ONLY")
        .ok()
        .map(|v| {
            let v = v.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        })
        .unwrap_or(false);
    if env_says_no {
        return false;
    }
    // The persisted form. A daemon outlives the app that launched it — the
    // next app launch reconnects to the running one — so a policy carried
    // only in the launch environment never reached a daemon that was already
    // up, and an ambient key kept being used after the app had switched to
    // explicit accounts. The bridge writes this file (see
    // `persist_explicit_accounts_policy`); the daemon checks it on every
    // credential lookup.
    !explicit_accounts_policy_path().is_some_and(|p| p.exists())
}

/// Where the explicit-accounts policy is persisted: a marker file in the
/// runtime's config dir, next to the pasted-key files it governs.
pub fn explicit_accounts_policy_path() -> Option<std::path::PathBuf> {
    jcode_storage::app_config_dir()
        .ok()
        .map(|d| d.join("explicit-accounts-only"))
}

/// Persist the policy so a daemon that is already running — or one started
/// later by something other than the app — honours it. Returns true when the
/// file was NEWLY created, which is the caller's cue to reload a running
/// daemon (its provider choice was made before the policy existed).
pub fn persist_explicit_accounts_policy() -> bool {
    let Some(path) = explicit_accounts_policy_path() else {
        return false;
    };
    if path.exists() {
        return false;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, b"1\n").is_ok()
}

pub fn load_api_key_from_env_or_config(env_key: &str, file_name: &str) -> Option<String> {
    if !is_safe_env_key_name(env_key) {
        jcode_logging::warn(&format!(
            "Ignoring invalid API key variable name '{}' while loading credentials",
            env_key
        ));
        return None;
    }
    if !is_safe_env_file_name(file_name) {
        jcode_logging::warn(&format!(
            "Ignoring invalid env file name '{}' while loading credentials",
            file_name
        ));
        return None;
    }

    if ambient_env_credentials_allowed()
        && let Ok(key) = std::env::var(env_key)
        && let Some(key) = clean_loaded_value(&key, env_key)
    {
        return Some(key);
    }

    let config_path = jcode_storage::app_config_dir().ok()?.join(file_name);
    jcode_storage::harden_secret_file_permissions(&config_path);
    let content = std::fs::read_to_string(config_path).ok()?;
    let prefix = format!("{}=", env_key);

    for line in content.lines() {
        if let Some(key) = line.strip_prefix(&prefix)
            && let Some(key) = clean_loaded_value(key, env_key)
        {
            return Some(key);
        }
    }

    if env_key == "ZHIPU_API_KEY" {
        if ambient_env_credentials_allowed()
            && let Ok(key) = std::env::var("ZAI_API_KEY")
            && let Some(key) = clean_loaded_value(&key, "ZAI_API_KEY")
        {
            return Some(key);
        }

        let legacy_prefix = "ZAI_API_KEY=";
        for line in content.lines() {
            if let Some(key) = line.strip_prefix(legacy_prefix)
                && let Some(key) = clean_loaded_value(key, "ZAI_API_KEY")
            {
                return Some(key);
            }
        }
    }

    if let Some(key) = resolve_api_key_fallback(env_key) {
        return Some(key);
    }

    None
}

pub fn load_env_value_from_env_or_config(env_key: &str, file_name: &str) -> Option<String> {
    if !is_safe_env_key_name(env_key) {
        jcode_logging::warn(&format!(
            "Ignoring invalid variable name '{}' while loading config value",
            env_key
        ));
        return None;
    }
    if !is_safe_env_file_name(file_name) {
        jcode_logging::warn(&format!(
            "Ignoring invalid env file name '{}' while loading config value",
            file_name
        ));
        return None;
    }

    if ambient_env_credentials_allowed()
        && let Ok(value) = std::env::var(env_key)
        && let Some(value) = clean_loaded_value(&value, env_key)
    {
        return Some(value);
    }

    load_env_value_from_config_file(env_key, file_name)
}

/// Load a value only from the saved env file under the blaude config dir,
/// ignoring the process environment.
///
/// [`load_env_value_from_env_or_config`] prefers the process env var, which is
/// correct for ambient configuration but wrong right after an explicit
/// `/login`: a stale env var inherited by a long-lived server process would
/// silently win over the credential the user just saved (issue #453). This
/// reader lets the auth-change path resolve what the file actually contains.
pub fn load_env_value_from_config_file(env_key: &str, file_name: &str) -> Option<String> {
    if !is_safe_env_key_name(env_key) {
        jcode_logging::warn(&format!(
            "Ignoring invalid variable name '{}' while loading config value",
            env_key
        ));
        return None;
    }
    if !is_safe_env_file_name(file_name) {
        jcode_logging::warn(&format!(
            "Ignoring invalid env file name '{}' while loading config value",
            file_name
        ));
        return None;
    }

    let config_path = jcode_storage::app_config_dir().ok()?.join(file_name);
    jcode_storage::harden_secret_file_permissions(&config_path);
    let content = std::fs::read_to_string(config_path).ok()?;
    let prefix = format!("{}=", env_key);

    for line in content.lines() {
        if let Some(value) = line.strip_prefix(&prefix)
            && let Some(value) = clean_loaded_value(value, env_key)
        {
            return Some(value);
        }
    }

    None
}

pub fn save_env_value_to_env_file(
    env_key: &str,
    file_name: &str,
    value: Option<&str>,
) -> anyhow::Result<()> {
    if !is_safe_env_key_name(env_key) {
        anyhow::bail!("Invalid variable name: {}", env_key);
    }
    if !is_safe_env_file_name(file_name) {
        anyhow::bail!("Invalid env file name: {}", file_name);
    }

    let config_dir = jcode_storage::app_config_dir()?;
    let file_path = config_dir.join(file_name);
    jcode_storage::upsert_env_file_value(&file_path, env_key, value)?;

    if let Some(value) = value {
        jcode_core::env::set_var(env_key, value);
    } else {
        jcode_core::env::remove_var(env_key);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&'static str]) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let saved = keys
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect::<Vec<_>>();
            for key in keys {
                jcode_core::env::remove_var(key);
            }
            Self { _lock: lock, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(value) => jcode_core::env::set_var(key, value),
                    None => jcode_core::env::remove_var(key),
                }
            }
        }
    }

    /// The whole point of the flag: with JCODE_EXPLICIT_ACCOUNTS_ONLY set, an
    /// AI key sitting in the process environment is NOT used — but a key in the
    /// config file (how the app stores an account a person added) still is.
    /// Both directions are asserted so the guard cannot pass vacuously.
    #[test]
    fn explicit_accounts_only_ignores_ambient_env_but_keeps_config_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::new(&[
            "JCODE_HOME",
            "OPENROUTER_API_KEY",
            "JCODE_EXPLICIT_ACCOUNTS_ONLY",
        ]);
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        // An ambient key in the environment, and NO config file yet.
        jcode_core::env::set_var("OPENROUTER_API_KEY", "sk-ambient-should-be-ignored");

        // Default (flag unset): the ambient key IS used — positive control that
        // the test can observe the ambient read at all.
        assert_eq!(
            load_api_key_from_env_or_config("OPENROUTER_API_KEY", "openrouter.env").as_deref(),
            Some("sk-ambient-should-be-ignored"),
            "without the flag, an ambient env key is used"
        );

        // Flag on: the ambient key is now invisible.
        jcode_core::env::set_var("JCODE_EXPLICIT_ACCOUNTS_ONLY", "1");
        assert_eq!(
            load_api_key_from_env_or_config("OPENROUTER_API_KEY", "openrouter.env"),
            None,
            "with the flag, an ambient env key must be ignored"
        );

        // The PERSISTED policy, with no env flag at all: this is what a
        // daemon started before the app switched policies actually reads.
        jcode_core::env::remove_var("JCODE_EXPLICIT_ACCOUNTS_ONLY");
        jcode_core::env::set_var("OPENROUTER_API_KEY", "sk-ambient-should-be-ignored");
        assert!(persist_explicit_accounts_policy(), "first persist creates the file");
        assert!(!persist_explicit_accounts_policy(), "second persist is a no-op");
        assert_eq!(
            load_api_key_from_env_or_config("OPENROUTER_API_KEY", "openrouter.env"),
            None,
            "the persisted policy alone must hide an ambient env key"
        );
        jcode_core::env::set_var("JCODE_EXPLICIT_ACCOUNTS_ONLY", "1");

        // A key added through the app lives in the config FILE — still found,
        // even with the flag on, so real accounts are untouched.
        save_env_value_to_env_file("OPENROUTER_API_KEY", "openrouter.env", Some("sk-from-the-app"))
            .expect("write config file");
        // save_env_value_to_env_file also sets the process env; clear it so the
        // only remaining source is the file.
        jcode_core::env::remove_var("OPENROUTER_API_KEY");
        assert_eq!(
            load_api_key_from_env_or_config("OPENROUTER_API_KEY", "openrouter.env").as_deref(),
            Some("sk-from-the-app"),
            "a key added through the app (config file) is still used under the flag"
        );
    }

    #[test]
    fn loads_api_key_from_env_before_config_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::new(&["JCODE_HOME", "JCODE_PROVIDER_ENV_TEST_KEY"]);
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        save_env_value_to_env_file(
            "JCODE_PROVIDER_ENV_TEST_KEY",
            "provider-env-test.env",
            Some("file-key"),
        )
        .expect("save file key");
        jcode_core::env::set_var("JCODE_PROVIDER_ENV_TEST_KEY", "env-key");

        assert_eq!(
            load_api_key_from_env_or_config("JCODE_PROVIDER_ENV_TEST_KEY", "provider-env-test.env")
                .as_deref(),
            Some("env-key")
        );
    }

    #[test]
    fn loads_and_removes_values_from_sandboxed_config_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::new(&["JCODE_HOME", "JCODE_PROVIDER_ENV_TEST_VALUE"]);
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        save_env_value_to_env_file(
            "JCODE_PROVIDER_ENV_TEST_VALUE",
            "provider-env-test.env",
            Some("file-value"),
        )
        .expect("save file value");

        jcode_core::env::remove_var("JCODE_PROVIDER_ENV_TEST_VALUE");
        assert_eq!(
            load_env_value_from_env_or_config(
                "JCODE_PROVIDER_ENV_TEST_VALUE",
                "provider-env-test.env"
            )
            .as_deref(),
            Some("file-value")
        );

        save_env_value_to_env_file(
            "JCODE_PROVIDER_ENV_TEST_VALUE",
            "provider-env-test.env",
            None,
        )
        .expect("remove file value");
        assert_eq!(
            load_env_value_from_env_or_config(
                "JCODE_PROVIDER_ENV_TEST_VALUE",
                "provider-env-test.env"
            ),
            None
        );
    }

    #[test]
    fn accepts_legacy_zai_key_for_zhipu() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::new(&["JCODE_HOME", "ZHIPU_API_KEY", "ZAI_API_KEY"]);
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        save_env_value_to_env_file("ZAI_API_KEY", "zai.env", Some("legacy-zai-key"))
            .expect("save legacy key");
        jcode_core::env::remove_var("ZAI_API_KEY");

        assert_eq!(
            load_api_key_from_env_or_config("ZHIPU_API_KEY", "zai.env").as_deref(),
            Some("legacy-zai-key")
        );
    }

    #[test]
    fn sanitize_strips_unicode_invisible_characters() {
        // Zero-width space, BOM, NBSP, en space around the value.
        assert_eq!(
            sanitize_secret_value("\u{200B}sk-key123\u{FEFF}"),
            "sk-key123"
        );
        assert_eq!(sanitize_secret_value("\u{00A0}sk-key\u{2002}"), "sk-key");
        // Quotes plus invisible padding both stripped.
        assert_eq!(
            sanitize_secret_value("\u{FEFF}\"sk-quoted\"\u{200B}"),
            "sk-quoted"
        );
        // Interior characters are preserved.
        assert_eq!(
            sanitize_secret_value("sk-mid\u{200B}dle"),
            "sk-mid\u{200B}dle"
        );
        // Empty after sanitize.
        assert_eq!(sanitize_secret_value("\u{200B}\u{FEFF}"), "");
    }

    #[test]
    fn loads_api_key_with_zero_width_space_from_config_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::new(&["JCODE_HOME", "JCODE_PROVIDER_FOO_API_KEY"]);
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        // Write an env file with a U+200B zero-width space prefixed onto the key,
        // mirroring issue #376's reproduction.
        let config_dir = jcode_storage::app_config_dir().expect("config dir");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::write(
            config_dir.join("provider-foo.env"),
            "JCODE_PROVIDER_FOO_API_KEY=\u{200B}sk-mykey123\n",
        )
        .expect("write env file");

        assert_eq!(
            load_api_key_from_env_or_config("JCODE_PROVIDER_FOO_API_KEY", "provider-foo.env")
                .as_deref(),
            Some("sk-mykey123")
        );
    }

    #[test]
    fn loads_api_key_with_invisible_chars_from_env_var() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::new(&["JCODE_HOME", "JCODE_PROVIDER_BAR_API_KEY"]);
        jcode_core::env::set_var("JCODE_HOME", temp.path());
        // NBSP + BOM padding around the env-provided key.
        jcode_core::env::set_var("JCODE_PROVIDER_BAR_API_KEY", "\u{00A0}sk-env-key\u{FEFF}");

        assert_eq!(
            load_api_key_from_env_or_config("JCODE_PROVIDER_BAR_API_KEY", "provider-bar.env")
                .as_deref(),
            Some("sk-env-key")
        );
    }
}
