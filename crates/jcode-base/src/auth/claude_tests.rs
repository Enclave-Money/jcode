use super::*;
use std::ffi::OsString;

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &std::path::Path) -> Self {
        let previous = std::env::var_os(key);
        crate::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous {
            crate::env::set_var(self.key, previous);
        } else {
            crate::env::remove_var(self.key);
        }
    }
}

#[test]
fn jcode_auth_file_default_is_empty() {
    let auth = JcodeAuthFile::default();
    assert!(auth.anthropic_accounts.is_empty());
    assert!(auth.active_anthropic_account.is_none());
}

#[test]
fn jcode_auth_file_roundtrip() {
    let auth = JcodeAuthFile {
        anthropic_accounts: vec![AnthropicAccount {
            label: "work".to_string(),
            access: "acc_123".to_string(),
            refresh: "ref_456".to_string(),
            expires: 9999999999999,
            email: None,
            scopes: Vec::new(),
            added_by: None,
            subscription_type: Some("max".to_string()),
        }],
        active_anthropic_account: Some("work".to_string()),
        anthropic: None,
    };

    let json = serde_json::to_string_pretty(&auth).unwrap();
    let parsed: JcodeAuthFile = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.anthropic_accounts.len(), 1);
    assert_eq!(parsed.anthropic_accounts[0].label, "work");
    assert_eq!(parsed.anthropic_accounts[0].access, "acc_123");
    assert_eq!(parsed.active_anthropic_account, Some("work".to_string()));
}

#[test]
fn jcode_path_respects_jcode_home() {
    let _lock = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().unwrap();
    let _home = EnvVarGuard::set("JCODE_HOME", temp.path());

    assert_eq!(jcode_path().unwrap(), temp.path().join("auth.json"));
    assert_eq!(
        claude_code_path().unwrap(),
        temp.path()
            .join("external")
            .join(".claude")
            .join(".credentials.json")
    );
    assert_eq!(
        opencode_path().unwrap(),
        temp.path()
            .join("external")
            .join(".local")
            .join("share")
            .join("opencode")
            .join("auth.json")
    );
}

#[test]
fn load_auth_file_renames_existing_labels_to_animal_scheme() {
    let _lock = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().unwrap();
    let _home = EnvVarGuard::set("JCODE_HOME", temp.path());
    set_active_account_override(None);

    let auth_path = temp.path().join("auth.json");
    std::fs::write(
        &auth_path,
        r#"{
            "anthropic_accounts": [
                {
                    "label": "personal",
                    "access": "acc_personal",
                    "refresh": "ref_personal",
                    "expires": 1000
                },
                {
                    "label": "work",
                    "access": "acc_work",
                    "refresh": "ref_work",
                    "expires": 2000
                }
            ],
            "active_anthropic_account": "work"
        }"#,
    )
    .unwrap();

    let auth = load_auth_file().unwrap();
    assert_eq!(
        auth.anthropic_accounts
            .iter()
            .map(|account| account.label.as_str())
            .collect::<Vec<_>>(),
        vec!["claude-otter", "claude-fox"]
    );
    assert_eq!(auth.active_anthropic_account.as_deref(), Some("claude-fox"));
}

#[test]
fn jcode_auth_file_multi_account() {
    let auth = JcodeAuthFile {
        anthropic_accounts: vec![
            AnthropicAccount {
                label: "personal".to_string(),
                access: "acc_personal".to_string(),
                refresh: "ref_personal".to_string(),
                expires: 1000,
                scopes: Vec::new(),
                added_by: None,
                subscription_type: Some("pro".to_string()),
                email: None,
            },
            AnthropicAccount {
                label: "work".to_string(),
                access: "acc_work".to_string(),
                refresh: "ref_work".to_string(),
                expires: 2000,
                email: None,
                scopes: Vec::new(),
                added_by: None,
                subscription_type: Some("max".to_string()),
            },
        ],
        active_anthropic_account: Some("work".to_string()),
        anthropic: None,
    };

    let json = serde_json::to_string(&auth).unwrap();
    let parsed: JcodeAuthFile = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.anthropic_accounts.len(), 2);
    assert_eq!(parsed.active_anthropic_account, Some("work".to_string()));
}

#[test]
fn jcode_auth_file_legacy_migration_format() {
    let legacy_json = r#"{
        "anthropic": {
            "access": "legacy_acc",
            "refresh": "legacy_ref",
            "expires": 12345
        }
    }"#;
    let parsed: JcodeAuthFile = serde_json::from_str(legacy_json).unwrap();
    assert!(parsed.anthropic_accounts.is_empty());
    assert!(parsed.anthropic.is_some());
}

#[test]
fn anthropic_account_no_subscription_type() {
    let json = r#"{
        "label": "test",
        "access": "acc",
        "refresh": "ref",
        "expires": 0
    }"#;
    let account: AnthropicAccount = serde_json::from_str(json).unwrap();
    assert_eq!(account.label, "test");
    assert!(account.subscription_type.is_none());
    assert!(account.email.is_none());
}

#[test]
fn anthropic_account_email_serialized_when_present() {
    let account = AnthropicAccount {
        label: "test".to_string(),
        access: "acc".to_string(),
        refresh: "ref".to_string(),
        expires: 0,
        email: Some("user@example.com".to_string()),
        scopes: Vec::new(),
        added_by: None,
        subscription_type: Some("max".to_string()),
    };
    let json = serde_json::to_string(&account).unwrap();
    assert!(json.contains("email"));
    assert!(json.contains("user@example.com"));
}

#[test]
fn anthropic_account_email_omitted_when_none() {
    let account = AnthropicAccount {
        label: "test".to_string(),
        access: "acc".to_string(),
        refresh: "ref".to_string(),
        expires: 0,
        email: None,
        scopes: Vec::new(),
        added_by: None,
        subscription_type: Some("max".to_string()),
    };
    let json = serde_json::to_string(&account).unwrap();
    assert!(!json.contains("\"email\""));
}

#[test]
fn anthropic_account_subscription_type_serialized_when_present() {
    let account = AnthropicAccount {
        label: "test".to_string(),
        access: "acc".to_string(),
        refresh: "ref".to_string(),
        expires: 0,
        email: None,
        scopes: Vec::new(),
        added_by: None,
        subscription_type: Some("max".to_string()),
    };
    let json = serde_json::to_string(&account).unwrap();
    assert!(json.contains("subscription_type"));
    assert!(json.contains("max"));
}

#[test]
fn anthropic_account_subscription_type_omitted_when_none() {
    let account = AnthropicAccount {
        label: "test".to_string(),
        access: "acc".to_string(),
        refresh: "ref".to_string(),
        expires: 0,
        scopes: Vec::new(),
        added_by: None,
        subscription_type: None,
        email: None,
    };
    let json = serde_json::to_string(&account).unwrap();
    assert!(!json.contains("subscription_type"));
}

#[test]
fn update_account_profile_sets_email() {
    let mut auth = JcodeAuthFile::default();
    auth.anthropic_accounts.push(AnthropicAccount {
        label: "test".to_string(),
        access: "acc".to_string(),
        refresh: "ref".to_string(),
        expires: 1,
        email: None,
        scopes: Vec::new(),
        added_by: None,
        subscription_type: None,
    });

    if let Some(account) = auth
        .anthropic_accounts
        .iter_mut()
        .find(|a| a.label == "test")
    {
        account.email = Some("user@example.com".to_string());
    }

    assert_eq!(
        auth.anthropic_accounts[0].email.as_deref(),
        Some("user@example.com")
    );
}

#[test]
fn is_max_subscription_pro_is_false() {
    // This tests the logic directly since we can't mock the file
    let sub_type = Some("pro".to_string());
    let is_max = match sub_type {
        Some(t) => t != "pro",
        None => true,
    };
    assert!(!is_max);
}

#[test]
fn is_max_subscription_max_is_true() {
    let sub_type = Some("max".to_string());
    let is_max = match sub_type {
        Some(t) => t != "pro",
        None => true,
    };
    assert!(is_max);
}

#[test]
fn is_max_subscription_unknown_is_true() {
    let sub_type: Option<String> = None;
    let is_max = match sub_type {
        Some(t) => t != "pro",
        None => true,
    };
    assert!(is_max);
}

#[test]
fn claude_code_credentials_format() {
    let json = r#"{
        "claudeAiOauth": {
            "accessToken": "at_12345",
            "refreshToken": "rt_67890",
            "expiresAt": 9999999999999,
            "subscriptionType": "max"
        }
    }"#;
    let file: CredentialsFile = serde_json::from_str(json).unwrap();
    let oauth = file.claude_ai_oauth.unwrap();
    assert_eq!(oauth.access_token, "at_12345");
    assert_eq!(oauth.refresh_token, "rt_67890");
    assert_eq!(oauth.expires_at, 9999999999999);
    assert_eq!(oauth.subscription_type, Some("max".to_string()));
}

#[test]
fn claude_code_credentials_no_subscription() {
    let json = r#"{
        "claudeAiOauth": {
            "accessToken": "at",
            "refreshToken": "rt",
            "expiresAt": 0
        }
    }"#;
    let file: CredentialsFile = serde_json::from_str(json).unwrap();
    let oauth = file.claude_ai_oauth.unwrap();
    assert!(oauth.subscription_type.is_none());
}

#[test]
fn claude_code_credentials_missing_oauth() {
    let json = r#"{}"#;
    let file: CredentialsFile = serde_json::from_str(json).unwrap();
    assert!(file.claude_ai_oauth.is_none());
}

#[cfg(unix)]
#[test]
fn load_claude_code_credentials_does_not_change_external_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().expect("tempdir");
    let _home = EnvVarGuard::set("JCODE_HOME", temp.path());

    let path = claude_code_path().expect("claude code path");
    std::fs::create_dir_all(path.parent().unwrap()).expect("create dir");
    std::fs::write(
        &path,
        r#"{"claudeAiOauth":{"accessToken":"at","refreshToken":"rt","expiresAt":4102444800000}}"#,
    )
    .expect("write file");
    std::fs::set_permissions(
        path.parent().unwrap(),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("set dir perms");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("set file perms");

    let _ = load_claude_code_credentials().expect("load external claude creds");

    let dir_mode = std::fs::metadata(path.parent().unwrap())
        .expect("stat dir")
        .permissions()
        .mode()
        & 0o777;
    let file_mode = std::fs::metadata(&path)
        .expect("stat file")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(dir_mode, 0o755);
    assert_eq!(file_mode, 0o644);
}

#[test]
fn opencode_credentials_format() {
    let json = r#"{
        "anthropic": {
            "access": "oc_acc",
            "refresh": "oc_ref",
            "expires": 1234567890
        }
    }"#;
    let auth: OpenCodeAuth = serde_json::from_str(json).unwrap();
    let anthropic = auth.anthropic.unwrap();
    assert_eq!(anthropic.access, "oc_acc");
    assert_eq!(anthropic.refresh, "oc_ref");
    assert_eq!(anthropic.expires, 1234567890);
}

#[test]
fn opencode_credentials_no_anthropic() {
    let json = r#"{}"#;
    let auth: OpenCodeAuth = serde_json::from_str(json).unwrap();
    assert!(auth.anthropic.is_none());
}

#[test]
fn active_account_override_roundtrip() {
    set_active_account_override(Some("test-override".to_string()));
    assert_eq!(
        get_active_account_override(),
        Some("test-override".to_string())
    );
    set_active_account_override(None);
    assert_eq!(get_active_account_override(), None);
}

#[test]
fn parse_blob_accepts_wrapped_file_form() {
    let json = r#"{
        "claudeAiOauth": {
            "accessToken": "at_file",
            "refreshToken": "rt_file",
            "expiresAt": 9999999999999,
            "subscriptionType": "max",
            "scopes": ["user:inference", "user:profile"]
        }
    }"#;
    let creds = parse_claude_code_credentials_blob(json).expect("parse wrapped");
    assert_eq!(creds.access_token, "at_file");
    assert_eq!(creds.refresh_token, "rt_file");
    assert_eq!(creds.expires_at, 9999999999999);
    assert_eq!(creds.subscription_type, Some("max".to_string()));
    assert_eq!(creds.scopes, vec!["user:inference", "user:profile"]);
}

#[test]
fn parse_blob_accepts_bare_keychain_form_with_numeric_expiry() {
    // The macOS Keychain stores a bare OAuth object (no claudeAiOauth wrapper).
    let json = r#"{
        "accessToken": "sk-ant-oat01-abc",
        "refreshToken": "sk-ant-ort01-xyz",
        "expiresAt": 4102444800000
    }"#;
    let creds = parse_claude_code_credentials_blob(json).expect("parse bare numeric");
    assert_eq!(creds.access_token, "sk-ant-oat01-abc");
    assert_eq!(creds.refresh_token, "sk-ant-ort01-xyz");
    assert_eq!(creds.expires_at, 4102444800000);
}

#[test]
fn parse_blob_accepts_rfc3339_string_expiry() {
    // Some Keychain blobs store expiresAt as an RFC 3339 timestamp string.
    let json = r#"{
        "accessToken": "at",
        "refreshToken": "rt",
        "expiresAt": "2027-02-18T07:00:00.000Z"
    }"#;
    let creds = parse_claude_code_credentials_blob(json).expect("parse rfc3339");
    let expected = chrono::DateTime::parse_from_rfc3339("2027-02-18T07:00:00.000Z")
        .unwrap()
        .timestamp_millis();
    assert_eq!(creds.expires_at, expected);
    assert!(creds.expires_at > 0);
}

#[test]
fn parse_blob_accepts_space_delimited_scope_string() {
    let json = r#"{
        "accessToken": "at",
        "refreshToken": "rt",
        "expiresAt": 1,
        "scopes": "user:inference user:profile"
    }"#;
    let creds = parse_claude_code_credentials_blob(json).expect("parse scope string");
    assert_eq!(creds.scopes, vec!["user:inference", "user:profile"]);
}

#[test]
fn parse_blob_missing_expiry_defaults_to_zero() {
    let json = r#"{ "accessToken": "at", "refreshToken": "rt" }"#;
    let creds = parse_claude_code_credentials_blob(json).expect("parse no expiry");
    assert_eq!(creds.expires_at, 0);
}

#[test]
fn parse_blob_rejects_empty_token() {
    let json = r#"{ "accessToken": "", "refreshToken": "" }"#;
    assert!(parse_claude_code_credentials_blob(json).is_err());
}

#[test]
fn parse_blob_rejects_empty_input() {
    assert!(parse_claude_code_credentials_blob("").is_err());
    assert!(parse_claude_code_credentials_blob("   ").is_err());
}

#[test]
fn env_token_credentials_parse_json_blob() {
    let _lock = crate::storage::lock_test_env();
    let _guard = EnvStringGuard::set(
        "CLAUDE_CODE_OAUTH_TOKEN",
        r#"{"accessToken":"at_env","refreshToken":"rt_env","expiresAt":4102444800000}"#,
    );
    let creds = load_claude_code_env_credentials().expect("env creds");
    assert_eq!(creds.access_token, "at_env");
    assert_eq!(creds.refresh_token, "rt_env");
    assert_eq!(creds.expires_at, 4102444800000);
}

#[test]
fn env_token_credentials_parse_bare_token() {
    let _lock = crate::storage::lock_test_env();
    let _guard = EnvStringGuard::set("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-bareToken");
    let creds = load_claude_code_env_credentials().expect("bare env creds");
    assert_eq!(creds.access_token, "sk-ant-oat01-bareToken");
    assert!(creds.refresh_token.is_empty());
    assert_eq!(creds.expires_at, 0);
}

#[test]
fn env_token_absent_yields_none() {
    let _lock = crate::storage::lock_test_env();
    let _guard = EnvStringGuard::remove("CLAUDE_CODE_OAUTH_TOKEN");
    assert!(load_claude_code_env_credentials().is_none());
}

/// Live macOS-only check against a real `Claude Code-credentials` Keychain item.
/// Ignored by default (mutates/reads the user Keychain). Run with:
///   cargo test -p jcode-base --lib auth::claude::tests::live_keychain -- --ignored --nocapture
#[cfg(target_os = "macos")]
#[test]
#[ignore = "live: reads the real macOS Keychain"]
fn live_keychain_native_credentials_detected_and_parsed() {
    let _lock = crate::storage::lock_test_env();
    let _guard = EnvStringGuard::remove("CLAUDE_CODE_OAUTH_TOKEN");

    assert!(
        native_credentials_present(),
        "expected a 'Claude Code-credentials' Keychain item to be present"
    );
    let creds = load_native_credentials().expect("load native creds from Keychain");
    assert!(
        !creds.access_token.trim().is_empty(),
        "expected a non-empty access token from the Keychain blob"
    );
}

/// Like `EnvVarGuard` but sets/removes string values (not just paths).
struct EnvStringGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvStringGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        crate::env::set_var(key, value);
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        crate::env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvStringGuard {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous {
            crate::env::set_var(self.key, previous);
        } else {
            crate::env::remove_var(self.key);
        }
    }
}

/// One daemon serves several teammates. A turn must resolve to the account
/// belonging to the person whose turn it is, not to whichever account the
/// process last made active.
///
/// Before this, every member's turn used the same account, so a teammate spent
/// the owner's Claude subscription and quota on their own work with nothing
/// saying so.
#[tokio::test]
async fn a_members_turn_uses_their_own_account() {
    let _lock = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().unwrap();
    let _home = EnvVarGuard::set("JCODE_HOME", temp.path());

    let account = |label: &str, token: &str, member: &str| AnthropicAccount {
        label: label.into(),
        access: token.into(),
        refresh: String::new(),
        expires: i64::MAX,
        email: None,
        subscription_type: Some("max".into()),
        scopes: vec!["user:inference".into()],
        added_by: Some(member.into()),
    };
    upsert_account(account("claude-otter", "OWNER-TOKEN", "owner@example.com")).unwrap();
    upsert_account(account("claude-fox", "MEMBER-TOKEN", "member@example.com")).unwrap();

    // Positive control: with no acting member this resolves to the active
    // account, so a failure below is the member routing and not a broken fixture.
    let shared = load_credentials().unwrap();
    assert_eq!(shared.access_token, "OWNER-TOKEN");

    let mine =
        crate::auth::account_store::with_acting_member(Some("member@example.com".into()), async {
            load_credentials()
        })
        .await
        .unwrap();
    assert_eq!(
        mine.access_token, "MEMBER-TOKEN",
        "the member's turn must use the account THEY signed in"
    );

    let owners =
        crate::auth::account_store::with_acting_member(Some("owner@example.com".into()), async {
            load_credentials()
        })
        .await
        .unwrap();
    assert_eq!(owners.access_token, "OWNER-TOKEN");
}

/// A teammate with no account of their own must FAIL, not quietly fall through
/// to someone else's subscription. That fall-through is the whole bug.
#[tokio::test]
async fn a_member_without_an_account_fails_instead_of_borrowing_one() {
    let _lock = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().unwrap();
    let _home = EnvVarGuard::set("JCODE_HOME", temp.path());

    upsert_account(AnthropicAccount {
        label: "claude-otter".into(),
        access: "OWNER-TOKEN".into(),
        refresh: String::new(),
        expires: i64::MAX,
        email: None,
        subscription_type: Some("max".into()),
        scopes: vec!["user:inference".into()],
        added_by: Some("owner@example.com".into()),
    })
    .unwrap();

    let error = crate::auth::account_store::with_acting_member(
        Some("newcomer@example.com".into()),
        async { load_credentials() },
    )
    .await
    .expect_err("a member with no account must not silently use the owner's");
    let error = format!("{error:#}");
    assert!(
        !error.contains("OWNER-TOKEN"),
        "the other account's token must never leak into the error: {error}"
    );
    assert!(
        error.contains("newcomer@example.com"),
        "the error should name who is missing an account: {error}"
    );
}

/// A second member signing in an account someone else already claimed must not
/// take it over.
///
/// Sign-in reuses an existing entry when the Anthropic profile email matches,
/// so without this guard the newcomer's stamp would overwrite the first
/// person's `added_by` and leave THEM unable to run a turn at all.
#[test]
fn signing_in_a_claimed_account_does_not_steal_it() {
    let _lock = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().unwrap();
    let _home = EnvVarGuard::set("JCODE_HOME", temp.path());

    upsert_account(AnthropicAccount {
        label: "claude-otter".into(),
        access: "OWNER-TOKEN".into(),
        refresh: String::new(),
        expires: i64::MAX,
        email: Some("shared@example.com".into()),
        subscription_type: Some("max".into()),
        scopes: vec!["user:inference".into()],
        added_by: Some("owner@example.com".into()),
    })
    .unwrap();

    let error = set_account_added_by("claude-otter", "newcomer@example.com")
        .expect_err("a claimed account must not be reassigned");
    assert!(
        format!("{error:#}").contains("owner@example.com"),
        "the error should name who already holds it: {error:#}"
    );

    // And the owner still owns it, so their turns keep working.
    assert_eq!(
        list_accounts().unwrap()[0].added_by.as_deref(),
        Some("owner@example.com")
    );

    // Re-stamping for the SAME member stays a no-op success, so an ordinary
    // re-login does not start failing.
    set_account_added_by("claude-otter", "owner@example.com").unwrap();
}

/// A refresh rewrites the stored token, so it must be written back to the
/// account it came FROM. The label used to come from the process-wide active
/// account, so refreshing a teammate's expiring token would have overwritten
/// the owner's stored credentials with the teammate's.
#[tokio::test]
async fn a_refresh_targets_the_acting_members_own_account() {
    let _lock = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().unwrap();
    let _home = EnvVarGuard::set("JCODE_HOME", temp.path());

    let account = |label: &str, token: &str, member: &str| AnthropicAccount {
        label: label.into(),
        access: token.into(),
        refresh: format!("{token}-REFRESH"),
        expires: i64::MAX,
        email: None,
        subscription_type: Some("max".into()),
        scopes: vec!["user:inference".into()],
        added_by: Some(member.into()),
    };
    upsert_account(account("claude-otter", "OWNER", "owner@example.com")).unwrap();
    upsert_account(account("claude-fox", "MEMBER", "member@example.com")).unwrap();
    // The server's active account is the owner's — the value that used to be
    // used for everyone's refresh.
    crate::auth::claude::set_active_account("claude-otter").unwrap();

    // Positive control: outside a member's turn the active account is correct.
    assert_eq!(
        crate::auth::claude::refresh_target_label().as_deref(),
        Some("claude-otter")
    );

    let target = crate::auth::account_store::with_acting_member(
        Some("member@example.com".into()),
        async { crate::auth::claude::refresh_target_label() },
    )
    .await;
    assert_eq!(
        target.as_deref(),
        Some("claude-fox"),
        "the member's refresh must rewrite THEIR account, not the active one"
    );
}

/// A token refresh rebuilds the account from credentials alone, and the store
/// replaces the record wholesale. Without carrying `added_by` across, a
/// member's account stopped being theirs a few hours after they signed in —
/// their next turn failed with "No Claude account for you" for no visible
/// reason, and the per-member routing decayed silently on a live server.
#[tokio::test]
async fn a_token_refresh_does_not_un_claim_the_member_who_signed_in() {
    let _lock = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().unwrap();
    let _home = EnvVarGuard::set("JCODE_HOME", temp.path());

    upsert_account(AnthropicAccount {
        label: "claude-otter".into(),
        access: "OLD".into(),
        refresh: "OLD-REFRESH".into(),
        expires: 1,
        email: Some("member@example.com".into()),
        subscription_type: Some("max".into()),
        scopes: vec!["user:inference".into()],
        added_by: Some("member@example.com".into()),
    })
    .unwrap();
    crate::auth::claude::set_account_added_by("claude-otter", "member@example.com").unwrap();

    // A refresh: same label, new tokens, and no idea who owns it.
    upsert_account(AnthropicAccount {
        label: "claude-otter".into(),
        access: "REFRESHED".into(),
        refresh: "NEW-REFRESH".into(),
        expires: i64::MAX,
        email: Some("member@example.com".into()),
        subscription_type: Some("max".into()),
        scopes: vec!["user:inference".into()],
        added_by: None,
    })
    .unwrap();

    // The refresh must have landed (positive control) AND kept the claim.
    let creds = crate::auth::account_store::with_acting_member(
        Some("member@example.com".into()),
        async { load_credentials() },
    )
    .await
    .expect("the member still owns this account after a refresh");
    assert_eq!(creds.access_token, "REFRESHED", "the refresh must apply");
}
