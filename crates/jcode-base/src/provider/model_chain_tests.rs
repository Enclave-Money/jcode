use super::*;

fn entry(provider: &str, account: &str, model: &str) -> ChainEntry {
    ChainEntry {
        provider: provider.into(),
        account: account.into(),
        model: model.into(),
    }
}

/// The user's example: GPT-6 on the first OpenAI account, then the second,
/// then Fable 5.1 on Claude.
fn frontier_chain() -> ModelChain {
    ModelChain {
        entries: vec![
            entry("openai", "openai-fox", "gpt-6-astra"),
            entry("openai", "openai-otter", "gpt-6-astra"),
            entry("claude", "claude-fox", "claude-fable-5-1"),
        ],
    }
}

fn mark(provider: &str, account: &str, model: Option<&str>) -> LimitMark {
    LimitMark {
        provider: provider.into(),
        account: account.into(),
        model: model.map(str::to_string),
        until_ms: now_ms() + 60_000,
        reason: String::new(),
    }
}

#[test]
fn order_is_followed_and_limits_skip_forward() {
    let chain = frontier_chain();
    assert_eq!(first_available(&chain, &[], None).unwrap().0, 0);

    // First GPT-6 account spent: second account, same model.
    let limits = vec![mark("openai", "openai-fox", Some("gpt-6-astra"))];
    let (index, next) = first_available(&chain, &limits, None).unwrap();
    assert_eq!(index, 1);
    assert_eq!(next.account, "openai-otter");

    // Both spent: Claude Fable, never some other OpenAI model.
    let limits = vec![
        mark("openai", "openai-fox", Some("gpt-6-astra")),
        mark("openai", "openai-otter", None),
    ];
    let (index, next) = first_available(&chain, &limits, None).unwrap();
    assert_eq!(index, 2);
    assert_eq!(
        next.model_spec().as_deref(),
        Some("claude-oauth:claude-fable-5-1")
    );

    // Everything spent: nothing, rather than an unlisted model.
    let limits = vec![
        mark("openai", "openai-fox", None),
        mark("openai", "openai-otter", None),
        mark("claude", "claude-fox", Some("claude-fable-5-1")),
    ];
    assert!(first_available(&chain, &limits, None).is_none());
    assert!(earliest_reset(&chain, &limits).is_some());
}

#[test]
fn a_model_limit_does_not_block_other_models_on_the_account() {
    let chain = ModelChain {
        entries: vec![
            entry("claude", "claude-fox", "claude-fable-5-1"),
            entry("claude", "claude-fox", "claude-opus-5-5"),
        ],
    };
    let limits = vec![mark("claude", "claude-fox", Some("claude-fable-5-1"))];
    assert_eq!(first_available(&chain, &limits, None).unwrap().0, 1);
    // An account-wide limit blocks both.
    let limits = vec![mark("claude", "claude-fox", None)];
    assert!(first_available(&chain, &limits, None).is_none());
}

#[test]
fn failover_never_repicks_the_entry_that_just_failed() {
    let chain = frontier_chain();
    assert_eq!(first_available(&chain, &[], Some(0)).unwrap().0, 1);
    assert!(first_available(&chain, &[], Some(2)).is_none());
}

#[test]
fn classifier_separates_model_and_account_limits() {
    let fable = classify_limit(
        "Anthropic API error (429): {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"You've reached your weekly Fable limit.\"}}",
        "claude-fable-5-1",
    )
    .unwrap();
    assert_eq!(fable.scope, LimitScope::Model);

    let account = classify_limit(
        "Rate limited: The usage limit has been reached. Plan: pro. Resets in 3h 20m.",
        "gpt-6-astra",
    )
    .unwrap();
    assert_eq!(account.scope, LimitScope::Account);
    let expected = now_ms() + (3 * 60 + 20) * 60_000;
    assert!(account.until_ms.abs_diff(expected) < 5_000, "{account:?}");

    let astra = classify_limit(
        "Rate limited: usage limit reached for gpt-6-astra. Resets in 45m.",
        "openai-oauth:gpt-6-astra",
    )
    .unwrap();
    assert_eq!(astra.scope, LimitScope::Model);
}

#[test]
fn classifier_ignores_errors_that_are_not_limits() {
    for error in [
        "Anthropic API error (401): invalid x-api-key",
        "Anthropic API error (529): Overloaded",
        "prompt is too long: 250000 tokens > 200000 maximum",
        "connection reset by peer",
    ] {
        assert!(
            classify_limit(error, "claude-fable-5-1").is_none(),
            "{error}"
        );
    }
}

#[test]
fn reset_times_are_read_in_every_shape_the_runtimes_produce() {
    let now = now_ms();
    let at = (now / 1000) + 7200;
    let hit = classify_limit(&format!("usage_limit_reached \"resets_at\": {at}"), "m").unwrap();
    assert_eq!(hit.until_ms, at * 1000);
    let hit = classify_limit("429 rate limit, retry after 120 seconds", "m").unwrap();
    assert!(hit.until_ms.abs_diff(now + 120_000) < 5_000);
    let hit = classify_limit("usage limit reached. Resets in 30d 2h", "m").unwrap();
    assert!(hit.until_ms > now + 29 * 86_400_000);
    // No time given: a bounded cooldown, never forever.
    let hit = classify_limit("429 Too Many Requests", "m").unwrap();
    assert!(hit.until_ms.abs_diff(now + DEFAULT_LIMIT_COOLDOWN_MS) < 5_000);
}

#[test]
fn listed_models_match_regardless_of_route_prefix_or_dots() {
    let chain = frontier_chain();
    assert_eq!(
        position(
            &chain,
            "openai",
            Some("openai-otter"),
            "openai-oauth:gpt-6-astra"
        ),
        Some(1)
    );
    assert_eq!(
        position(&chain, "claude", None, "claude-fable-5.1"),
        Some(2)
    );
    assert_eq!(position(&chain, "claude", None, "claude-opus-5-5"), None);
}

#[test]
fn chain_and_limits_round_trip_through_the_runtime_home() {
    let _lock = crate::storage::lock_test_env();
    let dir = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("JCODE_HOME");
    unsafe { std::env::set_var("JCODE_HOME", dir.path()) };

    assert!(load().is_none());
    save(&frontier_chain()).unwrap();
    assert_eq!(load().unwrap(), frontier_chain());

    record_limit(
        "openai",
        "openai-fox",
        Some("gpt-6-astra"),
        now_ms() + 60_000,
        "limit\nsecond line",
    );
    record_limit(
        "openai",
        "openai-fox",
        Some("gpt-6-astra"),
        now_ms() + 120_000,
        "again",
    );
    let limits = active_limits();
    assert_eq!(limits.len(), 1, "same key replaces, not duplicates");
    assert_eq!(limits[0].reason, "again");
    // Expired marks are dropped on read.
    record_limit(
        "claude",
        "claude-fox",
        None,
        now_ms().saturating_sub(1),
        "old",
    );
    assert_eq!(active_limits().len(), 1);

    save(&ModelChain::default()).unwrap();
    assert!(
        load().is_none(),
        "an empty order restores default behaviour"
    );
    assert!(
        save(&ModelChain {
            entries: vec![entry("gemini", "g", "m")]
        })
        .is_err()
    );

    match previous {
        Some(value) => unsafe { std::env::set_var("JCODE_HOME", value) },
        None => unsafe { std::env::remove_var("JCODE_HOME") },
    }
}
