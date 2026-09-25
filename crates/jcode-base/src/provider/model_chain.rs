//! The user's model order: which (provider, account, model) serves a turn, and
//! where to go when one of them hits its limit.
//!
//! A user who only wants the frontier models ("GPT-6 Astra on my first OpenAI
//! account, then my second, then Fable 5.1 on Claude") lists exactly those.
//! A turn runs on the first entry that is not at a limit; when a limit hits,
//! the turn moves to the next entry. Nothing outside the list is ever used —
//! a model the user did not pick is not a fallback, it is a surprise.
//!
//! Limits are recorded per account, and per model where the provider says the
//! limit is model-specific, with the reset time when the provider gives one.
//! They persist next to the list, so a restart does not send the next turn
//! straight back into a wall it already hit.
//!
//! Both files live in the runtime home (`$JCODE_HOME`), so on a team server
//! each member's room keeps its own order.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// How long to treat a limit as active when the provider gave no reset time.
/// Long enough not to hammer an exhausted account every turn, short enough
/// that a transient block does not strand a preferred model for the day.
const DEFAULT_LIMIT_COOLDOWN_MS: u64 = 30 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainEntry {
    /// `claude` or `openai`.
    pub provider: String,
    /// The stored account label (`claude-otter`, `openai-fox`, ...).
    pub account: String,
    /// The model id, without a route prefix (`gpt-6-astra`).
    pub model: String,
}

impl ChainEntry {
    /// The explicit subscription route for this entry's model. OAuth routes
    /// are what a signed-in account is; an explicit prefix never silently
    /// falls through to another provider.
    pub fn model_spec(&self) -> Option<String> {
        match self.provider.as_str() {
            "claude" => Some(format!("claude-oauth:{}", self.model)),
            "openai" => Some(format!("openai-oauth:{}", self.model)),
            _ => None,
        }
    }

    /// Plain words for the user: "GPT-6 Astra on openai-fox".
    pub fn describe(&self) -> String {
        format!("{} on {}", self.model, self.account)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChain {
    #[serde(default)]
    pub entries: Vec<ChainEntry>,
}

/// A recorded limit. `model: None` means the whole account is at its limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitMark {
    pub provider: String,
    pub account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Milliseconds since the epoch at which the limit is expected to lift.
    pub until_ms: u64,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LimitFile {
    #[serde(default)]
    limits: Vec<LimitMark>,
}

/// What a provider error says about limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitScope {
    /// The whole account is out, every model on it.
    Account,
    /// Only this model is out on this account.
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitHit {
    pub scope: LimitScope,
    pub until_ms: u64,
}

// Serializes read-modify-write of the limit file across concurrent turns in
// one daemon. Other processes are covered by the atomic replace underneath.
static LIMIT_FILE_LOCK: Mutex<()> = Mutex::new(());

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn chain_path() -> Option<PathBuf> {
    jcode_storage::jcode_dir()
        .ok()
        .map(|dir| dir.join("model-chain.json"))
}

fn limits_path() -> Option<PathBuf> {
    jcode_storage::jcode_dir()
        .ok()
        .map(|dir| dir.join("model-limits.json"))
}

/// The configured order, or `None` when the user has not set one (in which
/// case every existing default — including same-provider account failover —
/// behaves exactly as before).
pub fn load() -> Option<ModelChain> {
    let path = chain_path()?;
    let chain: ModelChain = jcode_storage::read_json(&path).ok()?;
    let entries: Vec<ChainEntry> = chain
        .entries
        .into_iter()
        .filter(|entry| entry.model_spec().is_some() && !entry.account.trim().is_empty())
        .collect();
    (!entries.is_empty()).then_some(ModelChain { entries })
}

pub fn configured() -> bool {
    load().is_some()
}

/// Replace the order. An empty list removes it (back to default behaviour).
pub fn save(chain: &ModelChain) -> anyhow::Result<()> {
    let path = chain_path().ok_or_else(|| anyhow::anyhow!("no runtime home"))?;
    if chain.entries.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    } else {
        for entry in &chain.entries {
            if entry.model_spec().is_none() {
                anyhow::bail!(
                    "unsupported provider `{}` (use claude or openai)",
                    entry.provider
                );
            }
            if entry.account.trim().is_empty() || entry.model.trim().is_empty() {
                anyhow::bail!("every entry needs an account and a model");
            }
        }
        jcode_storage::write_json(&path, chain)
    }
}

fn load_limits() -> Vec<LimitMark> {
    let Some(path) = limits_path() else {
        return Vec::new();
    };
    let file: LimitFile = jcode_storage::read_json(&path).unwrap_or_default();
    let now = now_ms();
    file.limits
        .into_iter()
        .filter(|mark| mark.until_ms > now)
        .collect()
}

/// Every limit still in force.
pub fn active_limits() -> Vec<LimitMark> {
    load_limits()
}

/// Remember that `provider`/`account` (and `model`, when model-specific) is at
/// its limit until `until_ms`.
pub fn record_limit(
    provider: &str,
    account: &str,
    model: Option<&str>,
    until_ms: u64,
    reason: &str,
) {
    let _guard = LIMIT_FILE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(path) = limits_path() else {
        return;
    };
    let mut limits = load_limits();
    limits.retain(|mark| {
        !(mark.provider == provider && mark.account == account && mark.model.as_deref() == model)
    });
    limits.push(LimitMark {
        provider: provider.to_string(),
        account: account.to_string(),
        model: model.map(str::to_string),
        until_ms,
        reason: reason
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(300)
            .collect(),
    });
    if let Err(error) = jcode_storage::write_json(&path, &LimitFile { limits }) {
        crate::logging::warn(&format!("model chain: could not persist a limit: {error}"));
    }
}

/// When `entry` is at a limit, the time it lifts.
pub fn blocked_until(entry: &ChainEntry, limits: &[LimitMark]) -> Option<u64> {
    limits
        .iter()
        .filter(|mark| mark.provider == entry.provider && mark.account == entry.account)
        .filter(|mark| {
            mark.model
                .as_deref()
                .is_none_or(|model| same_model(model, &entry.model))
        })
        .map(|mark| mark.until_ms)
        .max()
}

fn same_model(a: &str, b: &str) -> bool {
    normalize_model(a) == normalize_model(b)
}

/// Whether two model ids name the same model, ignoring route prefixes,
/// `[1m]` and dot/dash spelling.
pub fn same_model_id(a: &str, b: &str) -> bool {
    same_model(a, b)
}

fn normalize_model(model: &str) -> String {
    let model = model.trim();
    let model = model
        .rsplit_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or(model);
    model
        .trim_end_matches("[1m]")
        .to_ascii_lowercase()
        .replace('.', "-")
}

/// The first entry, in the user's order, that is not at a limit. `after`
/// skips everything up to and including that index (the failed entry), so a
/// failover never re-picks the entry that just failed.
pub fn first_available(
    chain: &ModelChain,
    limits: &[LimitMark],
    after: Option<usize>,
) -> Option<(usize, ChainEntry)> {
    let start = after.map(|index| index + 1).unwrap_or(0);
    chain
        .entries
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, entry)| blocked_until(entry, limits).is_none())
        .map(|(index, entry)| (index, entry.clone()))
}

/// Where `provider`/`account`/`model` sits in the order, if it is listed.
pub fn position(
    chain: &ModelChain,
    provider: &str,
    account: Option<&str>,
    model: &str,
) -> Option<usize> {
    chain.entries.iter().position(|entry| {
        entry.provider == provider
            && account.is_none_or(|account| entry.account == account)
            && same_model(&entry.model, model)
    })
}

/// The earliest time anything in the order becomes usable again.
pub fn earliest_reset(chain: &ModelChain, limits: &[LimitMark]) -> Option<u64> {
    chain
        .entries
        .iter()
        .filter_map(|entry| blocked_until(entry, limits))
        .min()
}

/// Decide whether `error` (from a request for `model`) is a usage limit, and
/// whether it covers the account or just this model.
///
/// Only real limits count. Auth failures, context overflows and "overloaded"
/// capacity errors are not the account running out; they keep their existing
/// handling.
pub fn classify_limit(error: &str, model: &str) -> Option<LimitHit> {
    let lower = error.to_ascii_lowercase();
    let is_auth = lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("authentication");
    let is_capacity = lower.contains("overloaded") || lower.contains("529");
    let is_context = lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("prompt is too long")
        || lower.contains("413");
    if is_auth || is_capacity || is_context {
        return None;
    }
    let is_limit = lower.contains("usage limit")
        || lower.contains("usage_limit")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("429")
        || lower.contains("quota")
        || lower.contains("limit reached")
        || lower.contains("limit has been reached")
        || lower.contains("exceeded your")
        || lower.contains("weekly limit")
        || lower.contains("out of credits")
        || lower.contains("insufficient_quota");
    if !is_limit {
        return None;
    }

    let scope = if names_model(&lower, model) {
        LimitScope::Model
    } else {
        LimitScope::Account
    };
    let until_ms = parse_reset_ms(&lower).unwrap_or_else(|| now_ms() + DEFAULT_LIMIT_COOLDOWN_MS);
    Some(LimitHit { scope, until_ms })
}

/// Model-specific limits name the model or its family ("your weekly Fable
/// limit", "limit for gpt-6-astra"). An account-wide limit names neither.
fn names_model(lower_error: &str, model: &str) -> bool {
    let normalized = normalize_model(model);
    if normalized.is_empty() {
        return false;
    }
    if lower_error.contains(&normalized) || lower_error.contains(&normalized.replace('-', ".")) {
        return true;
    }
    // The family word: "fable" in claude-fable-5-1, "astra" in gpt-6-astra,
    // "opus"/"sonnet" in claude-opus-5-5. Version digits and vendor words are
    // too generic to count.
    normalized
        .split('-')
        .filter(|part| part.len() >= 4 && part.chars().all(|c| c.is_ascii_alphabetic()))
        .filter(|part| !matches!(*part, "claude" | "codex" | "turbo" | "preview" | "latest"))
        .any(|family| lower_error.contains(family))
}

/// Pull a reset time out of the provider's words. Covers the shapes both
/// runtimes produce: an epoch `resets_at`, "resets in 3h 20m", "retry after
/// 120 seconds", and "try again in 45m".
fn parse_reset_ms(lower: &str) -> Option<u64> {
    let now = now_ms();
    if let Some(at) = number_after(lower, "resets_at") {
        // Seconds or milliseconds since the epoch.
        let ms = if at > 100_000_000_000 { at } else { at * 1000 };
        if ms > now {
            return Some(ms);
        }
    }
    if let Some(seconds) = number_after(lower, "resets_in_seconds") {
        return Some(now + seconds * 1000);
    }
    for marker in [
        "resets in",
        "reset in",
        "try again in",
        "retry after",
        "retry in",
    ] {
        if let Some(index) = lower.find(marker) {
            if let Some(ms) = parse_duration_ms(&lower[index + marker.len()..]) {
                return Some(now + ms);
            }
        }
    }
    None
}

fn number_after(text: &str, key: &str) -> Option<u64> {
    let index = text.find(key)? + key.len();
    let digits: String = text[index..]
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    // Only a number that follows the key closely (`"resets_at": 1790000000`).
    let gap = text[index..].find(|c: char| c.is_ascii_digit())?;
    if gap > 4 {
        return None;
    }
    digits.parse().ok()
}

/// "3h 20m", "45m", "120 seconds", "2 days 4 hours", "30d 2h".
fn parse_duration_ms(text: &str) -> Option<u64> {
    let mut total_ms: u64 = 0;
    let mut matched = false;
    let mut chars = text.trim_start().chars().peekable();
    // Read at most a handful of number-unit pairs; stop at the first gap.
    for _ in 0..4 {
        while chars.peek().is_some_and(|c| *c == ' ' || *c == ',') {
            chars.next();
        }
        let digits: String = std::iter::from_fn(|| chars.next_if(|c| c.is_ascii_digit())).collect();
        if digits.is_empty() {
            break;
        }
        while chars.peek().is_some_and(|c| *c == ' ') {
            chars.next();
        }
        let unit: String =
            std::iter::from_fn(|| chars.next_if(|c| c.is_ascii_alphabetic())).collect();
        let value: u64 = digits.parse().ok()?;
        let unit_ms = match unit.as_str() {
            "d" | "day" | "days" => 86_400_000,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000,
            "m" | "min" | "mins" | "minute" | "minutes" => 60_000,
            "s" | "sec" | "secs" | "second" | "seconds" => 1000,
            _ => break,
        };
        total_ms = total_ms.saturating_add(value.saturating_mul(unit_ms));
        matched = true;
    }
    matched.then_some(total_ms)
}

/// "at 14:05" in the runtime's local time, for messages.
pub fn describe_time(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    match chrono::DateTime::from_timestamp(secs, 0) {
        Some(utc) => {
            let local = utc.with_timezone(&chrono::Local);
            let today = chrono::Local::now().date_naive();
            if local.date_naive() == today {
                local.format("%H:%M").to_string()
            } else {
                local.format("%b %-d, %H:%M").to_string()
            }
        }
        None => "later".to_string(),
    }
}

#[cfg(test)]
#[path = "model_chain_tests.rs"]
mod tests;
