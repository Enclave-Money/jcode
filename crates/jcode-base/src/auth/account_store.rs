use anyhow::Result;
use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

/// Runtime (process-local) active-account overrides, keyed by provider
/// prefix ("claude", "openai", ...). Lets `/account switch <label>` take
/// effect immediately without rewriting the provider auth file.
///
/// Centralized here so every provider shares one mechanism instead of
/// duplicating a `static ACTIVE_ACCOUNT_OVERRIDE` per module.
static RUNTIME_ACTIVE_OVERRIDES: LazyLock<RwLock<HashMap<&'static str, String>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

tokio::task_local! {
    /// The team member whose turn is currently running, if any.
    ///
    /// One daemon serves several people, and every one of their turns used to
    /// resolve to the same account, so a teammate spent the owner's quota. The
    /// bridge already knows who each connection belongs to (it sends
    /// `subscribe.user`, kept as `connection_user`), and each stored account
    /// already records the member who signed it in (`added_by`). This carries
    /// the first to the place that reads the second.
    ///
    /// Task-local rather than a global: `RUNTIME_ACTIVE_OVERRIDES` is
    /// process-wide, so one member switching accounts changed which account the
    /// NEXT member's turn used. A task-local cannot leak across concurrent
    /// turns, which is the property that matters here.
    static ACTING_MEMBER: Option<String>;
}

/// Run `turn` attributed to `member`, so credential resolution picks that
/// person's own account.
pub async fn with_acting_member<T>(
    member: Option<String>,
    turn: impl std::future::Future<Output = T>,
) -> T {
    ACTING_MEMBER.scope(member, turn).await
}

/// The member whose turn is running, or `None` outside one (the owner's own
/// TUI, background refreshes, tests).
pub fn acting_member() -> Option<String> {
    ACTING_MEMBER
        .try_with(|member| member.clone())
        .ok()
        .flatten()
}

pub fn set_runtime_active_override(prefix: &'static str, label: Option<String>) {
    if let Ok(mut overrides) = RUNTIME_ACTIVE_OVERRIDES.write() {
        match label {
            Some(label) => {
                overrides.insert(prefix, label);
            }
            None => {
                overrides.remove(prefix);
            }
        }
    }
}

pub fn runtime_active_override(prefix: &str) -> Option<String> {
    RUNTIME_ACTIVE_OVERRIDES
        .read()
        .ok()
        .and_then(|overrides| overrides.get(prefix).cloned())
}

/// Memorable, provider-independent account names. Keeping this list fixed makes
/// labels stable across restarts and gives the same ordinal account the same
/// animal for every provider (for example `claude-otter` and `openai-otter`).
const ACCOUNT_ANIMALS: &[&str] = &[
    "otter", "fox", "panda", "wolf", "owl", "lynx", "badger", "raven", "tiger", "koala", "falcon",
    "gecko", "bison", "heron", "moose", "orca", "rabbit", "yak", "zebra", "beaver", "cougar",
    "dolphin", "ibis", "jaguar", "lemur", "marten", "newt", "quail", "seal", "wombat", "alpaca",
    "penguin",
];

pub fn canonical_account_label(prefix: &str, index: usize) -> String {
    let animal = index
        .checked_sub(1)
        .and_then(|index| ACCOUNT_ANIMALS.get(index).copied());
    match animal {
        Some(animal) => format!("{prefix}-{animal}"),
        // Extremely large account sets remain unique without making the common
        // case less friendly.
        None => format!("{prefix}-animal-{index}"),
    }
}

pub fn next_account_label(prefix: &str, account_count: usize) -> String {
    canonical_account_label(prefix, account_count + 1)
}

pub fn login_target_label<T, F>(
    prefix: &str,
    requested: Option<&str>,
    active_label: Option<String>,
    accounts: &[T],
    label_of: F,
) -> String
where
    F: Fn(&T) -> &str + Copy,
{
    if let Some(requested) = requested
        .map(str::trim)
        .filter(|requested| !requested.is_empty())
    {
        if accounts
            .iter()
            .any(|account| label_of(account) == requested)
        {
            return requested.to_string();
        }
        return next_account_label(prefix, accounts.len());
    }

    active_label
        .or_else(|| {
            accounts
                .first()
                .map(|account| label_of(account).to_string())
        })
        .unwrap_or_else(|| canonical_account_label(prefix, 1))
}

pub fn active_account_label<T, F>(
    override_label: Option<String>,
    stored_active_label: Option<String>,
    accounts: &[T],
    label_of: F,
) -> Option<String>
where
    F: Fn(&T) -> &str + Copy,
{
    override_label.or(stored_active_label).or_else(|| {
        accounts
            .first()
            .map(|account| label_of(account).to_string())
    })
}

pub fn set_active_account<T, F>(
    label: &str,
    accounts: &[T],
    stored_active_label: &mut Option<String>,
    missing_message: &str,
    label_of: F,
) -> Result<()>
where
    F: Fn(&T) -> &str + Copy,
{
    if !accounts.iter().any(|account| label_of(account) == label) {
        anyhow::bail!(missing_message.replace("{}", label));
    }
    *stored_active_label = Some(label.to_string());
    Ok(())
}

/// Insert `account`, or replace the stored one with the same label.
///
/// `carry` runs as `carry(&existing, &mut incoming)` before a replacement, to
/// move fields that belong to the STORED record onto the incoming one. A
/// refresh rebuilds an account from credentials alone, so without this the
/// replacement silently dropped `added_by` — and a member's account stopped
/// being theirs a few hours after they signed in, which read as "no Claude
/// account for you" out of nowhere.
pub fn upsert_account<T, FGet, FSet, FCarry>(
    prefix: &str,
    accounts: &mut Vec<T>,
    stored_active_label: &mut Option<String>,
    account: T,
    label_of: FGet,
    set_label: FSet,
    carry: FCarry,
) -> String
where
    FGet: Fn(&T) -> &str + Copy,
    FSet: Fn(&mut T, String) + Copy,
    FCarry: Fn(&T, &mut T),
{
    let requested_label = label_of(&account).to_string();
    if let Some(existing) = accounts
        .iter_mut()
        .find(|existing| label_of(existing) == requested_label)
    {
        let mut account = account;
        carry(existing, &mut account);
        *existing = account;
        return requested_label;
    }

    let label = next_account_label(prefix, accounts.len());
    let mut account = account;
    set_label(&mut account, label.clone());
    accounts.push(account);

    if stored_active_label.is_none() || accounts.len() == 1 {
        *stored_active_label = Some(label.clone());
    }

    label
}

pub struct RelabelOutcome {
    pub changed: bool,
    pub canonical_override_label: Option<String>,
}

pub fn relabel_accounts<T, FGet, FSet>(
    prefix: &str,
    accounts: &mut [T],
    stored_active_label: &mut Option<String>,
    override_label: Option<String>,
    label_of: FGet,
    set_label: FSet,
) -> RelabelOutcome
where
    FGet: Fn(&T) -> &str + Copy,
    FSet: Fn(&mut T, String) + Copy,
{
    let label_map = accounts
        .iter()
        .enumerate()
        .map(|(index, account)| {
            (
                label_of(account).to_string(),
                canonical_account_label(prefix, index + 1),
            )
        })
        .collect::<Vec<_>>();
    let mut changed = false;

    for (account, (_, canonical_label)) in accounts.iter_mut().zip(label_map.iter()) {
        if label_of(account) != canonical_label {
            set_label(account, canonical_label.clone());
            changed = true;
        }
    }

    let desired_active = if accounts.is_empty() {
        None
    } else {
        stored_active_label
            .as_deref()
            .and_then(|label| {
                label_map
                    .iter()
                    .find(|(original, _)| original == label)
                    .map(|(_, canonical)| canonical.clone())
            })
            .or_else(|| {
                accounts
                    .first()
                    .map(|account| label_of(account).to_string())
            })
    };

    if *stored_active_label != desired_active {
        *stored_active_label = desired_active;
        changed = true;
    }

    let canonical_override_label = override_label.and_then(|override_label| {
        label_map
            .iter()
            .find(|(original, _)| original == &override_label)
            .and_then(|(_, canonical)| (override_label != *canonical).then(|| canonical.clone()))
    });

    RelabelOutcome {
        changed,
        canonical_override_label,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Account {
        label: String,
    }

    #[test]
    fn relabel_accounts_canonicalizes_labels_and_active_label() {
        let mut accounts = vec![
            Account {
                label: "default".to_string(),
            },
            Account {
                label: "other".to_string(),
            },
        ];
        let mut active = Some("other".to_string());

        let outcome = relabel_accounts(
            "openai",
            &mut accounts,
            &mut active,
            Some("default".to_string()),
            |account| account.label.as_str(),
            |account, label| account.label = label,
        );

        assert!(outcome.changed);
        assert_eq!(accounts[0].label, "openai-otter");
        assert_eq!(accounts[1].label, "openai-fox");
        assert_eq!(active.as_deref(), Some("openai-fox"));
        assert_eq!(
            outcome.canonical_override_label.as_deref(),
            Some("openai-otter")
        );
    }

    #[test]
    fn upsert_account_assigns_next_label_and_sets_initial_active() {
        let mut accounts = Vec::<Account>::new();
        let mut active = None;

        let label = upsert_account(
            "claude",
            &mut accounts,
            &mut active,
            Account {
                label: "ignored".to_string(),
            },
            |account| account.label.as_str(),
            |account, label| account.label = label,
            |_existing, _incoming| {},
        );

        assert_eq!(label, "claude-otter");
        assert_eq!(accounts[0].label, "claude-otter");
        assert_eq!(active.as_deref(), Some("claude-otter"));
    }

    #[test]
    fn account_labels_use_animals_and_stay_unique_after_the_named_pool() {
        assert_eq!(canonical_account_label("claude", 1), "claude-otter");
        assert_eq!(canonical_account_label("claude", 2), "claude-fox");
        assert_eq!(canonical_account_label("openai", 32), "openai-penguin");
        assert_eq!(canonical_account_label("openai", 33), "openai-animal-33");
    }
}
