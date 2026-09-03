use super::box_utils::render_rounded_box;
use super::changelog::get_unseen_changelog_entries;
use super::{TuiState, dim_color, header_name_color, shorten_model_name};
use crate::auth::{AuthState, AuthStatus};
use crate::tui::color_support::rgb;
#[cfg(test)]
use crate::tui::ui::status_support::semver;
use ratatui::prelude::*;
#[cfg(test)]
use std::sync::OnceLock;

#[cfg(test)]
fn unseen_changelog_entries_override() -> &'static std::sync::Mutex<Option<Vec<String>>> {
    static OVERRIDE: OnceLock<std::sync::Mutex<Option<Vec<String>>>> = OnceLock::new();
    OVERRIDE.get_or_init(|| std::sync::Mutex::new(None))
}

/// How many unseen release-note entries there are (sizes the welcome pad).
pub(super) fn unseen_changelog_len() -> usize {
    unseen_changelog_entries().len()
}

fn unseen_changelog_entries() -> Vec<String> {
    #[cfg(test)]
    {
        if let Ok(guard) = unseen_changelog_entries_override().lock()
            && let Some(entries) = guard.clone()
        {
            return entries;
        }
    }
    get_unseen_changelog_entries().clone()
}

#[cfg(test)]
pub(crate) fn set_unseen_changelog_entries_override_for_tests(entries: Option<Vec<String>>) {
    let mut guard = unseen_changelog_entries_override()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = entries;
}

pub(crate) fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().chain(chars).collect(),
    }
}

/// Compact form of a full build version string: `v0.25.19-dev (abc1234, dirty)`
/// becomes `v0.25.19-dev`. Used for the per-line server/client version labels.
#[cfg(test)]
fn compact_version_label(version: &str) -> String {
    let trimmed = version.trim();
    match trimmed.split_once(" (") {
        Some((head, _)) => head.trim().to_string(),
        None => trimmed.to_string(),
    }
}

fn format_model_name(short: &str, provider_name: &str) -> String {
    if short.contains('/') {
        // Slashed model ids (e.g. `nvidia/nemotron-...`) are served by the
        // OpenRouter slot, which also fronts direct OpenAI-compatible profiles
        // such as NVIDIA NIM or DeepSeek. Label the line with the active
        // provider's display name instead of hard-coding "OpenRouter" so the
        // header matches the profile the user actually selected.
        let label = {
            let trimmed = provider_name.trim();
            if trimmed.is_empty() {
                "OpenRouter".to_string()
            } else {
                trimmed.to_string()
            }
        };
        return format!("{}: {}", label, short);
    }
    if short.contains("opus") {
        if short.contains("4.5") {
            return "Claude 4.5 Opus".to_string();
        }
        return "Claude Opus".to_string();
    }
    if short.contains("sonnet") {
        if short.contains("3.5") {
            return "Claude 3.5 Sonnet".to_string();
        }
        return "Claude Sonnet".to_string();
    }
    if short.contains("haiku") {
        return "Claude Haiku".to_string();
    }
    if short.starts_with("gpt") {
        // Only the numeric GPT families (gpt-4o, gpt-5.2-codex, ...) have a
        // curated form. Other gpt-prefixed ids (gpt-oss-120b) fall through to
        // the generic prettifier instead of producing "GPT-oss120b".
        let rest = short.trim_start_matches("gpt");
        if rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_digit()) {
            return format_gpt_name(short);
        }
    }
    short.to_string()
}

fn format_gpt_name(short: &str) -> String {
    let rest = short.trim_start_matches("gpt");
    if rest.is_empty() {
        return "GPT".to_string();
    }

    if let Some(idx) = rest.find("codex") {
        let version = &rest[..idx];
        if version.is_empty() {
            return "GPT Codex".to_string();
        }
        return format!("GPT-{} Codex", version);
    }

    format!("GPT-{}", rest)
}

/// Generic fallback for model ids with no curated pretty name: title-case the
/// hyphen/underscore segments (`claude-fable-5` -> `Claude Fable 5`). Date or
/// snapshot suffixes (6+ digit runs) are dropped, vowel-less short segments are
/// treated as acronyms (`glm` -> `GLM`), and parameter sizes are uppercased
/// (`70b` -> `70B`). Placeholder labels with spaces/ellipses pass through.
fn prettify_model_id(model: &str) -> String {
    if model.contains(' ') || model.contains('…') || model.contains('/') {
        return model.to_string();
    }

    fn is_acronym(part: &str) -> bool {
        // Well-known initialisms that contain vowels and would otherwise be
        // title-cased as words.
        const KNOWN: &[&str] = &["oss", "ai", "moe", "vl", "it", "fp8", "awq", "exp"];
        if KNOWN.contains(&part.to_ascii_lowercase().as_str()) {
            return true;
        }
        // Short, all-alphabetic, and vowel-less segments read as initialisms:
        // glm, gpt, qwq, llm. Anything with a vowel (pro, max, mini, fable)
        // reads as a word and gets normal title-casing.
        part.len() <= 4
            && part.chars().all(|c| c.is_ascii_alphabetic())
            && !part
                .chars()
                .any(|c| matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u' | 'y'))
    }

    fn is_param_size(part: &str) -> bool {
        // 70b / 8x7b / 32k style size or context markers.
        part.len() >= 2
            && part
                .chars()
                .last()
                .is_some_and(|c| matches!(c.to_ascii_lowercase(), 'b' | 'm' | 'k'))
            && part[..part.len() - 1]
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.' || c == 'x')
            && part.chars().any(|c| c.is_ascii_digit())
    }

    let parts: Vec<String> = model
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        // Drop date/snapshot suffixes like 20241022.
        .filter(|part| !(part.len() >= 6 && part.chars().all(|c| c.is_ascii_digit())))
        .map(|part| {
            if is_acronym(part) || is_param_size(part) {
                return part.to_uppercase();
            }
            let mut chars = part.chars();
            match chars.next() {
                Some(first) if first.is_ascii_alphabetic() => {
                    first.to_uppercase().chain(chars).collect::<String>()
                }
                Some(first) => first.to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    if parts.is_empty() {
        model.to_string()
    } else {
        parts.join(" ")
    }
}

/// Final display name for the header model line: curated pretty names first
/// (Claude 4.5 Opus, GPT-5.2 Codex), generic title-cased prettification otherwise.
fn header_model_display_name(model: &str, provider_name: &str) -> String {
    let raw = model.trim();

    // Claude family ids ("claude-opus-4-6", "claude-3-5-sonnet-latest",
    // "claude-haiku-4.5") render as "Claude <version> <Family>" for any
    // version, instead of only the hardcoded 3.5/4.5 cases.
    if raw.starts_with("claude") {
        for family in ["opus", "sonnet", "haiku"] {
            if raw.contains(family) {
                let family_pretty = capitalize(family);
                let version = claude_version_segment(raw, family);
                return match version {
                    Some(version) => format!("Claude {} {}", version, family_pretty),
                    None => format!("Claude {}", family_pretty),
                };
            }
        }
    }

    // GPT ids are formatted from the raw segments ("gpt-5.1-codex-max" ->
    // "GPT-5.1 Codex Max") rather than the legacy mashed short form, which
    // produced "GPT-5.1codexmax"-style names.
    if let Some(rest) = raw.strip_prefix("gpt-")
        && rest.starts_with(|c: char| c.is_ascii_digit())
    {
        let mut segments = rest.split('-');
        let version = segments.next().unwrap_or_default();
        let mut name = format!("GPT-{}", version);
        for segment in segments {
            if segment.is_empty() {
                continue;
            }
            let pretty = prettify_model_id(segment);
            name.push(' ');
            name.push_str(&pretty);
        }
        return name;
    }

    let short_model = shorten_model_name(raw);
    let curated = format_model_name(&short_model, provider_name);
    if curated == short_model {
        // No curated pretty name matched; title-case the raw model id
        // instead of showing the mangled short form (`claudefable5`).
        prettify_model_id(raw)
    } else {
        curated
    }
}

/// Extract the version from a Claude model id, e.g. "claude-opus-4-6" -> "4.6",
/// "claude-3-5-sonnet-latest" -> "3.5", "claude-haiku-4.5" -> "4.5". Snapshot
/// dates (6+ digit runs) are ignored.
fn claude_version_segment(raw: &str, family: &str) -> Option<String> {
    let digits: Vec<&str> = raw
        .split(['-', '_'])
        .filter(|part| *part != family)
        .filter(|part| {
            !part.is_empty()
                && part.len() < 6
                && part.chars().all(|c| c.is_ascii_digit() || c == '.')
                && part.chars().any(|c| c.is_ascii_digit())
        })
        .collect();
    match digits.as_slice() {
        [] => None,
        [single] => Some(single.to_string()),
        [major, minor, ..] => Some(format!(
            "{}.{}",
            major.trim_matches('.'),
            minor.trim_matches('.')
        )),
    }
}

fn auth_dot_color(state: AuthState) -> Color {
    match state {
        AuthState::Available => rgb(100, 200, 100),
        AuthState::Expired => rgb(255, 200, 100),
        AuthState::NotConfigured => rgb(80, 80, 80),
    }
}

#[cfg(test)]
fn auth_dot_char(state: AuthState) -> &'static str {
    match state {
        AuthState::Available => "●",
        AuthState::Expired => "◐",
        AuthState::NotConfigured => "○",
    }
}

/// Authoritative active credential per dual-auth provider, resolved by the app
/// from the live provider/remote server. `None` entries mean "unknown, fall
/// back to the cached `AuthStatus` + env heuristic".
#[derive(Clone, Copy, Default)]
pub(super) struct ActiveCredentialOverrides {
    anthropic: Option<crate::auth::ActiveCredential>,
    openai: Option<crate::auth::ActiveCredential>,
}

impl ActiveCredentialOverrides {
    fn from_app(app: &dyn TuiState) -> Self {
        Self {
            anthropic: app.active_dual_credential(jcode_provider_core::ActiveProvider::Claude),
            openai: app.active_dual_credential(jcode_provider_core::ActiveProvider::OpenAI),
        }
    }

    fn get(
        &self,
        provider: jcode_provider_core::ActiveProvider,
    ) -> Option<crate::auth::ActiveCredential> {
        match provider {
            jcode_provider_core::ActiveProvider::Claude => self.anthropic,
            jcode_provider_core::ActiveProvider::OpenAI => self.openai,
            _ => None,
        }
    }
}

/// Configured providers with their full labels, in display order.
#[cfg(test)]
fn auth_full_specs(
    auth: &AuthStatus,
    active: ActiveCredentialOverrides,
) -> Vec<(String, AuthState)> {
    fn provider_label(name: &str, state: AuthState, method: Option<&str>) -> String {
        match (state, method) {
            (AuthState::NotConfigured, _) => name.to_string(),
            (_, Some(method)) if !method.is_empty() => format!("{}({})", name, method),
            _ => name.to_string(),
        }
    }

    // The auth list is a credential *inventory* (what is configured), while
    // the provider tag above reports the *active* route. When both credentials
    // are configured, mark the active one with `*` so the two surfaces read as
    // one consistent story ("oauth*+key" = both configured, OAuth in use)
    // instead of an ambiguous "oauth+key" that looks like both are being used
    // at once.
    fn dual_method_label(
        provider: jcode_provider_core::ActiveProvider,
        auth: &AuthStatus,
        active: ActiveCredentialOverrides,
    ) -> Option<&'static str> {
        use crate::auth::{ActiveCredential, resolve_dual_credential_auth};
        let runtime_provider = std::env::var("JCODE_RUNTIME_PROVIDER").ok();
        let resolved = resolve_dual_credential_auth(provider, auth, runtime_provider.as_deref())?;
        // Prefer the app's authoritative answer over the env heuristic.
        let active = active.get(provider).unwrap_or(resolved.active);
        Some(match (resolved.has_oauth, resolved.has_api_key) {
            (true, true) => match active {
                ActiveCredential::OAuth => "oauth*+key",
                ActiveCredential::ApiKey => "oauth+key*",
            },
            (true, false) => "oauth",
            (false, true) => "key",
            (false, false) => return None,
        })
    }

    // Which accounts are signed in, so `blaude` shows at a glance what you're
    // logged into — "claude ✓" alone doesn't answer "which account(s)?".
    fn account_suffix(labels: Vec<String>) -> String {
        match labels.len() {
            0 => String::new(),
            1 => format!(" — {}", truncate_accounts(&labels, 28)),
            n => format!(" — {n} accounts: {}", truncate_accounts(&labels, 34)),
        }
    }
    fn truncate_accounts(labels: &[String], max: usize) -> String {
        let joined = labels.join(", ");
        if joined.chars().count() <= max {
            joined
        } else {
            let head: String = joined.chars().take(max.saturating_sub(1)).collect();
            format!("{head}…")
        }
    }
    let claude_accounts: Vec<String> = crate::auth::claude::list_accounts()
        .map(|accs| {
            accs.into_iter()
                .map(|a| a.email.unwrap_or(a.label))
                .collect()
        })
        .unwrap_or_default();
    let codex_accounts: Vec<String> = crate::auth::codex::list_accounts()
        .map(|accs| accs.into_iter().map(|a| a.label).collect())
        .unwrap_or_default();

    let anthropic_label = format!(
        "{}{}",
        provider_label(
            "anthropic",
            auth.anthropic.state,
            dual_method_label(jcode_provider_core::ActiveProvider::Claude, auth, active),
        ),
        account_suffix(claude_accounts)
    );

    let openai_label = format!(
        "{}{}",
        provider_label(
            "openai",
            auth.openai,
            dual_method_label(jcode_provider_core::ActiveProvider::OpenAI, auth, active),
        ),
        account_suffix(codex_accounts)
    );

    let gemini_label = if auth.gemini != AuthState::NotConfigured {
        provider_label("gemini", auth.gemini, Some("oauth"))
    } else {
        provider_label("gemini", auth.gemini, None)
    };

    vec![
        (anthropic_label, auth.anthropic.state),
        ("openrouter".to_string(), auth.openrouter),
        (openai_label, auth.openai),
        (provider_label("cursor", auth.cursor, None), auth.cursor),
        (provider_label("copilot", auth.copilot, None), auth.copilot),
        (gemini_label, auth.gemini),
        (
            provider_label("antigravity", auth.antigravity, None),
            auth.antigravity,
        ),
    ]
}

/// Vertical auth inventory: one line per provider. Configured providers get
/// green/yellow dots; unconfigured ones get a dim hollow dot so they read as
/// available-to-add without cluttering the `/login` heading.
#[cfg(test)]
pub(super) fn build_auth_status_lines(
    auth: &AuthStatus,
    active: ActiveCredentialOverrides,
) -> Vec<Line<'static>> {
    let specs = auth_full_specs(auth, active);
    // Only list providers the user actually has credentials for. When nothing
    // is configured at all, fall back to the full list so the `/login` heading
    // still shows what can be added.
    let configured: Vec<_> = specs
        .iter()
        .filter(|(_, state)| *state != AuthState::NotConfigured)
        .cloned()
        .collect();
    let shown = if configured.is_empty() {
        specs
    } else {
        configured
    };
    shown
        .into_iter()
        .map(|(label, state)| {
            Line::from(vec![
                Span::styled(
                    auth_dot_char(state),
                    Style::default().fg(auth_dot_color(state)),
                ),
                Span::styled(format!(" {}", label), Style::default().fg(dim_color())),
            ])
        })
        .collect()
}

fn header_provider_auth_tag(
    name: &str,
    auth: &AuthStatus,
    active: ActiveCredentialOverrides,
) -> &'static str {
    let runtime_provider = std::env::var("JCODE_RUNTIME_PROVIDER").ok();

    // Anthropic and OpenAI share one credential-resolution source of truth so
    // the header tag never drifts from the info widget / model-switch line. We
    // route through the canonical ActiveProvider rather than matching display
    // strings, which is how this surface previously broke (name == "claude"
    // never matched a "anthropic"-only arm and the tag silently vanished).
    if let Some(provider) = jcode_provider_core::parse_provider_hint(name) {
        use crate::auth::{ActiveCredential, resolve_dual_credential_auth};
        match resolve_dual_credential_auth(provider, auth, runtime_provider.as_deref()) {
            Some(resolved) => {
                // The app's live answer wins over the env heuristic; the env
                // var is frequently absent in the TUI client process.
                let credential = active.get(provider).unwrap_or(resolved.active);
                // Report exactly the credential the next request will use. The
                // "both configured" inventory now lives in the auth status line
                // (`oauth*+key`), so this tag never claims two credentials at
                // once -- that ambiguity is how "Claude OAuth" and "API key"
                // used to contradict each other across surfaces.
                return match credential {
                    ActiveCredential::OAuth => "oauth",
                    ActiveCredential::ApiKey => "api-key",
                };
            }
            // Provider recognized but no credentials configured: no tag.
            None if matches!(
                provider,
                jcode_provider_core::ActiveProvider::Claude
                    | jcode_provider_core::ActiveProvider::OpenAI
            ) =>
            {
                return "";
            }
            None => {}
        }
    }

    match name {
        "copilot" => {
            if auth.copilot_has_api_token {
                "oauth"
            } else {
                ""
            }
        }
        "openrouter" | "openai-compatible" => "api-key",
        other
            if crate::provider_catalog::resolve_openai_compatible_profile_selection(other)
                .is_some()
                || crate::provider_catalog::openai_compatible_profile_id_for_display_name(
                    other,
                )
                .is_some() =>
        {
            "api-key"
        }
        _ => "",
    }
}

fn header_provider_label(
    provider_name: &str,
    auth: &AuthStatus,
    active: ActiveCredentialOverrides,
) -> String {
    let trimmed = provider_name.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let name = trimmed.to_lowercase();
    let auth_tag = header_provider_auth_tag(&name, auth, active);
    if auth_tag.is_empty() {
        name
    } else {
        format!("{}:{}", auth_tag, name)
    }
}

fn abbreviate_home(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.display().to_string();
        if path == home_str {
            return "~".to_string();
        }
        if let Some(rest) = path.strip_prefix(&home_str) {
            return format!("~{}", rest);
        }
    }
    path.to_string()
}

#[cfg(test)]
fn truncate_to_width(text: &str, width: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }

    let mut truncated = text
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
fn choose_header_candidate(width: usize, candidates: Vec<String>) -> String {
    let mut last_non_empty = String::new();
    for candidate in candidates
        .into_iter()
        .filter(|candidate| !candidate.trim().is_empty())
    {
        if candidate.chars().count() <= width {
            return candidate;
        }
        last_non_empty = candidate;
    }

    truncate_to_width(&last_non_empty, width)
}

#[cfg(test)]
fn semver_core() -> String {
    semver()
        .split('-')
        .next()
        .unwrap_or_else(semver)
        .to_string()
}

#[cfg(test)]
fn semver_minor() -> String {
    let core = semver_core();
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() >= 2 {
        format!("{}.{}", parts[0], parts[1])
    } else {
        core
    }
}

#[cfg(test)]
fn version_display_candidates() -> Vec<String> {
    let full = format!("blaude {}", semver());
    let core = format!("blaude {}", semver_core());
    let minor = format!("blaude {}", semver_minor());
    let shortest = semver_minor();
    vec![full, core, minor, shortest]
}

#[cfg(test)]
fn configured_auth_count(auth: &AuthStatus) -> usize {
    [
        auth.jcode,
        auth.anthropic.state,
        auth.openrouter,
        auth.azure,
        auth.openai,
        auth.cursor,
        auth.copilot,
        auth.gemini,
        auth.antigravity,
        auth.google,
    ]
    .into_iter()
    .filter(|state| *state != AuthState::NotConfigured)
    .count()
}

/// The constant top-of-screen header: identity + model + cwd on line one,
/// accounts + subscription limits on line two, a dim rule under it. Rendered
/// pinned above the transcript every frame (never scrolls), so the trivia the
/// old floating side boxes carried has one stable home.
pub(super) fn build_pinned_header(app: &dyn TuiState, width: u16) -> Vec<Line<'static>> {
    let w = width as usize;
    let auth = app.auth_status();
    let active = ActiveCredentialOverrides::from_app(app);
    let mut lines: Vec<Line> = Vec::new();

    // Line 1: ✳ blaude [self-dev] · <model> · <cwd (branch)>
    {
        let mut spans = vec![
            Span::styled(
                "✳ ".to_string(),
                Style::default().fg(jcode_tui_style::theme::header_icon_color()),
            ),
            Span::styled(
                "blaude".to_string(),
                Style::default().fg(header_name_color()).bold(),
            ),
        ];
        let mut used = 8usize;
        if app.is_canary() {
            spans.push(Span::styled(
                " self-dev".to_string(),
                Style::default().fg(dim_color()),
            ));
            used += 9;
        }
        let model = app.provider_model();
        let nice_model = header_model_display_name(&model, &app.provider_name());
        if !nice_model.is_empty() && !model.trim().is_empty() {
            let seg = format!(" · {nice_model}");
            if used + seg.chars().count() <= w {
                used += seg.chars().count();
                spans.push(Span::styled(
                    " · ".to_string(),
                    Style::default().fg(dim_color()),
                ));
                spans.push(Span::styled(
                    nice_model,
                    Style::default().fg(rgb(255, 150, 200)).bold(),
                ));
            }
        }
        if let Some(dir) = app.working_dir() {
            let mut text = abbreviate_home(&dir);
            if let Some(branch) = app.git_branch() {
                text = format!("{text} ({branch})");
            }
            let seg = format!(" · {text}");
            if used + seg.chars().count() <= w {
                spans.push(Span::styled(seg, Style::default().fg(dim_color())));
            }
        }
        lines.push(Line::from(spans));
    }

    // Line 2: accounts + limits, trimmed from the right when narrow.
    {
        let mut segments: Vec<(String, Style)> = Vec::new();

        let claude_accounts: Vec<String> = crate::auth::claude::list_accounts()
            .map(|accs| {
                accs.into_iter()
                    .map(|a| a.email.unwrap_or(a.label))
                    .collect()
            })
            .unwrap_or_default();
        if !claude_accounts.is_empty() {
            let dot_style = Style::default().fg(auth_dot_color(auth.anthropic.state));
            segments.push(("● ".to_string(), dot_style));
            segments.push((
                format!("claude: {}", claude_accounts.join(", ")),
                Style::default().fg(dim_color()),
            ));
        }
        let codex_accounts: Vec<String> = crate::auth::codex::list_accounts()
            .map(|accs| accs.into_iter().map(|a| a.label).collect())
            .unwrap_or_default();
        if !codex_accounts.is_empty() {
            let dot_style = Style::default().fg(auth_dot_color(auth.openai));
            segments.push(("  ● ".to_string(), dot_style));
            segments.push((
                format!("codex: {}", codex_accounts.join(", ")),
                Style::default().fg(dim_color()),
            ));
        }
        let _ = active;

        if let Some(info) = app.info_widget_data().usage_info.as_ref().filter(|info| {
            info.available
                && !matches!(
                    info.provider,
                    crate::tui::info_widget::UsageProvider::Copilot
                        | crate::tui::info_widget::UsageProvider::CostBased
                )
        }) {
            let five_left =
                100u8.saturating_sub((info.five_hour * 100.0).round().clamp(0.0, 100.0) as u8);
            let week_left =
                100u8.saturating_sub((info.seven_day * 100.0).round().clamp(0.0, 100.0) as u8);
            let mut text = format!("  ·  5h {five_left}% left");
            if let Some(reset) = info
                .five_hour_resets_at
                .as_deref()
                .map(crate::usage::format_reset_time)
            {
                text.push_str(&format!(" ({reset})"));
            }
            text.push_str(&format!("  ·  wk {week_left}% left"));
            if let Some(reset) = info
                .seven_day_resets_at
                .as_deref()
                .map(crate::usage::format_reset_time)
            {
                text.push_str(&format!(" ({reset})"));
            }
            let color = if five_left < 15 || week_left < 15 {
                rgb(255, 170, 100)
            } else {
                dim_color()
            };
            segments.push((text, Style::default().fg(color)));
        }

        if segments.is_empty() {
            segments.push((
                "/login to add a Claude or Codex account".to_string(),
                Style::default().fg(dim_color()),
            ));
        }

        // Trim whole segments from the right until the line fits.
        let mut spans: Vec<Span> = Vec::new();
        let mut used = 0usize;
        for (text, style) in segments {
            let n = text.chars().count();
            if used + n > w {
                break;
            }
            used += n;
            spans.push(Span::styled(text, style));
        }
        lines.push(Line::from(spans));
    }

    // Line 3: a dim rule separating the header from the transcript.
    lines.push(Line::from(Span::styled(
        "─".repeat(w.min(500)),
        Style::default().fg(rgb(60, 60, 66)),
    )));

    lines
}

#[cfg(test)]
pub(super) fn build_persistent_header(app: &dyn TuiState, width: u16) -> Vec<Line<'static>> {
    let auth = app.auth_status();
    let active = ActiveCredentialOverrides::from_app(app);
    build_persistent_header_with_auth(app, width, &auth, active)
}

fn build_persistent_header_with_auth(
    app: &dyn TuiState,
    width: u16,
    auth: &AuthStatus,
    active: ActiveCredentialOverrides,
) -> Vec<Line<'static>> {
    let model = app.provider_model();
    let nice_model = header_model_display_name(&model, &app.provider_name());
    let align = Alignment::Left;
    let mut lines: Vec<Line> = Vec::new();
    let w = width as usize;

    let is_canary = app.is_canary();
    let is_remote = app.is_remote_mode();
    let server_update = app.server_update_available() == Some(true);
    let client_update = app.client_update_available();
    let mut status_items: Vec<&str> = Vec::new();
    if app.is_replay() {
        status_items.push("replay");
    } else if is_remote {
        status_items.push("client");
    }
    if server_update {
        status_items.push("srv↑");
    }
    if client_update {
        status_items.push("cli↑");
    }
    if let Some(badge) = crate::perf::profile().tier.badge() {
        status_items.push(badge);
    }

    // Server/client codenames and per-binary version labels moved out of the
    // welcome: the pinned header carries identity and `/status` keeps the
    // plumbing detail. A version mismatch still surfaces via the srv↑ /
    // cli↑ badges on the first line.

    // First line: `jcode` (+ `self-dev` when running a dev/canary build),
    // followed by any remaining status badges rendered dimly.
    {
        let mut spans = vec![
            Span::styled(
                "✳ ".to_string(),
                Style::default().fg(jcode_tui_style::theme::header_icon_color()),
            ),
            Span::styled(
                "blaude".to_string(),
                Style::default().fg(header_name_color()).bold(),
            ),
        ];
        if is_canary {
            spans.push(Span::styled(
                " self-dev".to_string(),
                Style::default().fg(dim_color()),
            ));
        }
        if !status_items.is_empty() {
            spans.push(Span::styled(
                format!(" · {}", status_items.join(" · ")),
                Style::default().fg(dim_color()),
            ));
        }
        lines.push(Line::from(spans).alignment(align));
    }

    // Single model line: dim active-route method on the left, styled model
    // name in the middle, dim upstream/hint detail after. This used to be a
    // second, unstyled line in the secondary header duplicating the model name.
    let model_is_placeholder = {
        let trimmed = model.trim();
        trimmed.is_empty()
            || trimmed == "connected"
            || trimmed.ends_with('…')
            || trimmed.starts_with("connecting")
    };
    let provider_label = if model_is_placeholder {
        String::new()
    } else {
        header_provider_label(&app.provider_name(), auth, active)
    };
    let upstream = if model_is_placeholder {
        None
    } else {
        app.upstream_provider()
    };
    let mut model_spans: Vec<Span> = Vec::new();
    let mut model_line_len = nice_model.chars().count();
    // Keep a little headroom below the full width so the line never
    // wraps when the render area subtracts side margins.
    let fit_width = w.saturating_sub(4);
    if !model_is_placeholder && !nice_model.is_empty() {
        let hint = "/model to switch · ";
        if model_line_len + hint.chars().count() <= fit_width {
            model_line_len += hint.chars().count();
            model_spans.push(Span::styled(
                hint.to_string(),
                Style::default().fg(dim_color()),
            ));
        }
    }
    if !provider_label.is_empty() {
        let prefix = format!("{} · ", provider_label);
        if model_line_len + prefix.chars().count() <= fit_width {
            model_line_len += prefix.chars().count();
            model_spans.push(Span::styled(prefix, Style::default().fg(dim_color())));
        }
    }
    model_spans.push(Span::styled(
        nice_model.clone(),
        // Match the info widget's model accent (pink, bold) instead of plain
        // white so the model reads as a distinct, styled element.
        Style::default().fg(rgb(255, 150, 200)).bold(),
    ));
    if let Some(upstream) = upstream.as_deref() {
        let suffix = format!(" via {}", upstream);
        if model_line_len + suffix.chars().count() <= fit_width {
            model_spans.push(Span::styled(suffix, Style::default().fg(dim_color())));
        }
    }
    if !nice_model.is_empty() {
        lines.push(Line::from(model_spans).alignment(align));
    }

    lines
}

#[cfg(test)]
pub(crate) fn build_header_lines(app: &dyn TuiState, width: u16) -> Vec<Line<'static>> {
    let auth = app.auth_status();
    let active = ActiveCredentialOverrides::from_app(app);
    build_header_lines_with_auth(app, width, &auth, active)
}

fn build_header_lines_with_auth(
    app: &dyn TuiState,
    width: u16,
    auth: &AuthStatus,
    active: ActiveCredentialOverrides,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let align = ratatui::layout::Alignment::Left;
    let w = width as usize;

    // Accounts, limits, and the working directory live in the pinned
    // header now; the welcome stays a quiet transcript entry.
    let _ = (auth, active);

    let mcps = app.mcp_servers();
    if !mcps.is_empty() {
        const MAX_MCPS: usize = 4;
        let shown: Vec<String> = mcps
            .iter()
            .take(MAX_MCPS)
            .map(|(name, count)| {
                if *count > 0 {
                    format!("{} ({} tools)", name, count)
                } else {
                    format!("{} (…)", name)
                }
            })
            .collect();
        let mut mcp_text = format!("mcp: {}", shown.join(", "));
        if mcps.len() > MAX_MCPS {
            mcp_text.push_str(&format!(" +{} more", mcps.len() - MAX_MCPS));
        }
        if mcp_text.chars().count() > w {
            mcp_text = format!("mcp: {} servers", mcps.len());
        }
        lines.push(
            Line::from(Span::styled(mcp_text, Style::default().fg(dim_color()))).alignment(align),
        );
    }

    lines.push(Line::from(""));
    lines
}

/// Build the "Updates" rounded box (unseen release notes) so it can be
/// rendered inside the top padding above the header. `max_lines` bounds the
/// total height including the box borders; entries beyond the budget are
/// collapsed into a "…N more" line. Returns an empty vec when there are no
/// unseen entries or the budget/width is too small for a box.
pub(super) fn build_updates_box_lines(width: u16, max_lines: usize) -> Vec<Line<'static>> {
    let w = width as usize;
    if w <= 20 || max_lines < 3 {
        return Vec::new();
    }
    let new_entries = unseen_changelog_entries();
    if new_entries.is_empty() {
        return Vec::new();
    }

    // Budget for content lines inside the box (borders take 2 lines).
    let content_budget = (max_lines - 2).min(8);
    let has_more = new_entries.len() > content_budget;
    let display_count = if has_more {
        content_budget.saturating_sub(1)
    } else {
        new_entries.len()
    };

    let mut content: Vec<Line> = Vec::new();
    for entry in new_entries.iter().take(display_count) {
        content.push(Line::from(Span::styled(
            format!("• {}", entry),
            Style::default().fg(dim_color()),
        )));
    }
    if has_more {
        content.push(Line::from(Span::styled(
            format!(
                "  …{} more · /changelog to see all",
                new_entries.len() - display_count
            ),
            Style::default().fg(dim_color()),
        )));
    }
    if content.is_empty() {
        return Vec::new();
    }

    render_rounded_box(
        "Updates",
        content,
        w.saturating_sub(2),
        Style::default().fg(dim_color()),
    )
    .into_iter()
    .map(|line| line.alignment(Alignment::Left))
    .collect()
}

/// Build both header sections from one authentication snapshot. Credential
/// discovery can touch several files on Windows, so the render path must not
/// repeat it for the persistent and secondary portions of the same frame.
pub(super) fn build_header_sections(
    app: &dyn TuiState,
    width: u16,
) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    let auth = app.auth_status();
    let active = ActiveCredentialOverrides::from_app(app);
    (
        build_persistent_header_with_auth(app, width, &auth, active),
        build_header_lines_with_auth(app, width, &auth, active),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthState, AuthStatus, ProviderAuth};
    use crate::message::Message;
    use crate::provider::{EventStream, Provider};
    use crate::tool::Registry;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::OnceLock;

    struct MockProvider;

    #[async_trait]
    impl Provider for MockProvider {
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[crate::message::ToolDefinition],
            _system: &str,
            _resume_session_id: Option<&str>,
        ) -> Result<EventStream> {
            Err(anyhow::anyhow!(
                "Mock provider should not be used for streaming completions in ui header tests"
            ))
        }

        fn name(&self) -> &str {
            "mock"
        }

        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(MockProvider)
        }
    }

    fn ensure_test_jcode_home_if_unset() {
        static TEST_HOME: OnceLock<std::path::PathBuf> = OnceLock::new();

        if std::env::var_os("JCODE_HOME").is_some() {
            return;
        }

        let path = TEST_HOME.get_or_init(|| {
            let path = std::env::temp_dir().join(format!("jcode-test-home-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&path);
            path
        });
        crate::env::set_var("JCODE_HOME", path);
    }

    fn create_test_app() -> crate::tui::app::App {
        ensure_test_jcode_home_if_unset();

        let provider: Arc<dyn Provider> = Arc::new(MockProvider);
        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        let registry = rt.block_on(Registry::new(provider.clone()));
        crate::tui::app::App::new_for_test_harness(provider, registry)
    }

    #[test]
    fn left_aligned_mode_keeps_persistent_header_left_aligned() {
        let mut app = create_test_app();
        app.set_centered(false);

        let lines = build_persistent_header(&app, 80);
        let non_empty: Vec<&Line<'_>> = lines
            .iter()
            .filter(|line| !line.spans.iter().all(|span| span.content.trim().is_empty()))
            .collect();

        assert!(!non_empty.is_empty(), "expected persistent header lines");
        assert!(
            non_empty
                .iter()
                .all(|line| line.alignment == Some(Alignment::Left)),
            "persistent header should be left aligned: {non_empty:?}"
        );
    }

    #[test]
    fn left_aligned_mode_keeps_secondary_header_left_aligned() {
        let mut app = create_test_app();
        app.set_centered(false);

        // The secondary header is minimal now (accounts/limits live in the
        // pinned header); check alignment across both sections instead.
        let (persistent, secondary) = build_header_sections(&app, 80);
        let lines: Vec<Line<'static>> = persistent.into_iter().chain(secondary).collect();
        let non_empty: Vec<&Line<'_>> = lines
            .iter()
            .filter(|line| !line.spans.iter().all(|span| span.content.trim().is_empty()))
            .collect();

        assert!(!non_empty.is_empty(), "expected header detail lines");
        assert!(
            non_empty
                .iter()
                .all(|line| line.alignment == Some(Alignment::Left)),
            "header detail lines should be left aligned: {non_empty:?}"
        );
    }

    #[test]
    fn combined_header_sections_match_individual_builders() {
        let app = create_test_app();
        let (persistent, secondary) = build_header_sections(&app, 80);

        assert_eq!(persistent, build_persistent_header(&app, 80));
        assert_eq!(secondary, build_header_lines(&app, 80));
    }

    #[test]
    fn version_display_candidates_compact_for_narrow_width() {
        let rendered = choose_header_candidate(8, version_display_candidates());
        // Version-agnostic: at width 8 only the bare minor semver fits.
        assert_eq!(rendered, semver_minor());
    }

    fn rendered_header_lines(app: &crate::tui::app::App, width: u16) -> Vec<String> {
        build_persistent_header(app, width)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn persistent_header_omits_server_client_plumbing() {
        let mut app = create_test_app();
        app.set_remote_server_identity_for_tests(
            Some("blazing"),
            Some("🔥"),
            Some("v0.14.2-dev (old1234)"),
            Some("session_fox_1705012345678"),
        );

        let lines = rendered_header_lines(&app, 120);
        assert!(
            lines
                .iter()
                .all(|line| !line.contains("server:") && !line.contains("client:")),
            "server/client plumbing lines moved to /status; the welcome stays quiet: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("blaude")),
            "identity line survives: {lines:?}"
        );
    }

    #[test]
    fn pinned_header_carries_model_accounts_and_limits_home() {
        let app = create_test_app();
        let lines = build_pinned_header(&app, 100);
        assert_eq!(lines.len(), 3, "identity, accounts/limits, rule");

        let text =
            |line: &Line<'_>| -> String { line.spans.iter().map(|s| s.content.as_ref()).collect() };
        let first = text(&lines[0]);
        assert!(
            first.contains("blaude"),
            "line one is the identity line: {first}"
        );
        let second = text(&lines[1]);
        assert!(
            !second.trim().is_empty(),
            "line two always says something — accounts or the /login hint: {second}"
        );
        let rule = text(&lines[2]);
        assert!(
            rule.chars().all(|c| c == '─') && !rule.is_empty(),
            "line three is the separator rule: {rule}"
        );
    }

    #[test]
    fn prettify_model_id_title_cases_unknown_models() {
        assert_eq!(prettify_model_id("claude-fable-5"), "Claude Fable 5");
        assert_eq!(prettify_model_id("grok-code-fast-1"), "Grok Code Fast 1");
        assert_eq!(prettify_model_id("kimi_k2"), "Kimi K2");
        assert_eq!(
            prettify_model_id("gemini-3-pro-preview"),
            "Gemini 3 Pro Preview"
        );
        assert_eq!(prettify_model_id("deepseek-chat"), "Deepseek Chat");
        assert_eq!(
            prettify_model_id("mistral-large-2411"),
            "Mistral Large 2411"
        );
        assert_eq!(prettify_model_id("o3-mini"), "O3 Mini");
        // Vowel-less short segments read as acronyms.
        assert_eq!(prettify_model_id("glm-4.6"), "GLM 4.6");
        assert_eq!(prettify_model_id("qwq-32b"), "QWQ 32B");
        // Parameter sizes are uppercased.
        assert_eq!(prettify_model_id("llama-3.3-70b"), "Llama 3.3 70B");
        assert_eq!(prettify_model_id("mixtral-8x7b"), "Mixtral 8X7B");
        // Long digit runs (snapshot dates) are dropped.
        assert_eq!(
            prettify_model_id("claude-fable-5-20260101"),
            "Claude Fable 5"
        );
        // Placeholders and slashed ids pass through untouched.
        assert_eq!(prettify_model_id("loading session…"), "loading session…");
        assert_eq!(
            prettify_model_id("deepseek/deepseek-chat"),
            "deepseek/deepseek-chat"
        );
        // Degenerate inputs survive.
        assert_eq!(prettify_model_id(""), "");
        assert_eq!(prettify_model_id("-"), "-");
    }

    #[test]
    fn header_model_display_name_sweeps_real_model_catalog() {
        // End-to-end through shorten_model_name + format_model_name +
        // prettify_model_id, over the model ids blaude actually routes.
        let cases = [
            // Anthropic
            ("claude-opus-4-5-20251101", "Claude 4.5 Opus"),
            ("claude-opus-4.6", "Claude 4.6 Opus"),
            ("claude-opus-4-8", "Claude 4.8 Opus"),
            ("claude-sonnet-4-5", "Claude 4.5 Sonnet"),
            ("claude-sonnet-4", "Claude 4 Sonnet"),
            ("claude-3-5-sonnet-latest", "Claude 3.5 Sonnet"),
            ("claude-haiku-4-5", "Claude 4.5 Haiku"),
            ("claude-fable-5", "Claude Fable 5"),
            // OpenAI
            ("gpt-5.2-codex", "GPT-5.2 Codex"),
            ("gpt-5.1-codex-max", "GPT-5.1 Codex Max"),
            ("gpt-5.3-codex-spark", "GPT-5.3 Codex Spark"),
            ("gpt-5-mini", "GPT-5 Mini"),
            ("gpt-5.1-chat-latest", "GPT-5.1 Chat Latest"),
            ("gpt-4o", "GPT-4o"),
            ("gpt-4o-mini", "GPT-4o Mini"),
            ("gpt-oss-120b", "GPT OSS 120B"),
            ("o3-mini", "O3 Mini"),
            ("o4-mini", "O4 Mini"),
            // Google
            ("gemini-3-pro-preview", "Gemini 3 Pro Preview"),
            ("gemini-2.5-flash", "Gemini 2.5 Flash"),
            // xAI / Moonshot / Zhipu / DeepSeek / Minimax
            ("grok-code-fast-1", "Grok Code Fast 1"),
            ("kimi-k2.5", "Kimi K2.5"),
            ("kimi-k2p5-turbo", "Kimi K2p5 Turbo"),
            ("glm-4.6", "GLM 4.6"),
            ("deepseek-v4-flash", "Deepseek V4 Flash"),
            ("minimax-m2.7", "Minimax M2.7"),
            // Meta / Mistral / Qwen / community
            ("llama-3.3-70b", "Llama 3.3 70B"),
            ("mixtral-8x7b", "Mixtral 8X7B"),
            ("devstral-medium-2507", "Devstral Medium 2507"),
            ("qwen3-coder-plus", "Qwen3 Coder Plus"),
            ("composer-1.5", "Composer 1.5"),
            ("llama-3.1-8b-instant", "Llama 3.1 8B Instant"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                header_model_display_name(input, ""),
                expected,
                "model id {input:?}"
            );
        }

        // Slashed ids keep the provider label form.
        assert_eq!(
            header_model_display_name("deepseek/deepseek-chat", "OpenRouter"),
            "OpenRouter: deepseek/deepseek-chat"
        );
        // Placeholders pass through untouched.
        assert_eq!(
            header_model_display_name("loading session…", ""),
            "loading session…"
        );
        assert_eq!(header_model_display_name("connected", ""), "Connected");
    }

    #[test]
    fn compact_version_label_strips_hash_suffix() {
        assert_eq!(
            compact_version_label("v0.25.19-dev (7e261bcc, dirty)"),
            "v0.25.19-dev"
        );
        assert_eq!(compact_version_label("v0.25.19 (abc1234)"), "v0.25.19");
        assert_eq!(compact_version_label(" v0.25.19 "), "v0.25.19");
    }

    #[test]
    fn configured_auth_count_includes_non_model_auth_surfaces() {
        let auth = AuthStatus {
            jcode: AuthState::Available,
            anthropic: ProviderAuth {
                state: AuthState::Expired,
                has_oauth: true,
                oauth_state: AuthState::Expired,
                has_api_key: false,
            },
            azure: AuthState::Available,
            google: AuthState::Available,
            ..AuthStatus::default()
        };

        assert_eq!(configured_auth_count(&auth), 4);
    }

    #[test]
    fn header_provider_auth_tag_reports_active_credential_for_openai() {
        let _guard = crate::storage::lock_test_env();
        let prev = std::env::var_os("JCODE_RUNTIME_PROVIDER");
        crate::env::remove_var("JCODE_RUNTIME_PROVIDER");
        let auth = AuthStatus {
            openai: AuthState::Available,
            openai_has_oauth: true,
            openai_has_api_key: true,
            ..AuthStatus::default()
        };

        // Auto mode prefers OAuth; the tag must report only the credential in
        // use (the auth inventory line carries the "both configured" detail).
        assert_eq!(
            header_provider_auth_tag("openai", &auth, ActiveCredentialOverrides::default()),
            "oauth"
        );
        if let Some(value) = prev {
            crate::env::set_var("JCODE_RUNTIME_PROVIDER", value);
        }
    }

    #[test]
    fn header_provider_auth_tag_prefers_app_resolved_credential_over_env() {
        let _guard = crate::storage::lock_test_env();
        let prev = std::env::var_os("JCODE_RUNTIME_PROVIDER");
        // The TUI client usually does not inherit JCODE_RUNTIME_PROVIDER, so the
        // env heuristic would answer "oauth" here; the app's resolution must win.
        crate::env::remove_var("JCODE_RUNTIME_PROVIDER");
        let both = AuthStatus {
            anthropic: ProviderAuth {
                // `state` must be set alongside the credential booleans:
                // `build_auth_status_lines` filters `NotConfigured` providers out
                // and falls back to the full "no credentials" list (issue #654).
                state: AuthState::Available,
                has_oauth: true,
                oauth_state: AuthState::Available,
                has_api_key: true,
            },
            ..AuthStatus::default()
        };
        let overrides = ActiveCredentialOverrides {
            anthropic: Some(crate::auth::ActiveCredential::ApiKey),
            openai: None,
        };
        assert_eq!(
            header_provider_auth_tag("anthropic", &both, overrides),
            "api-key"
        );
        let rendered = build_auth_status_lines(&both, overrides)
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            rendered.contains("anthropic(oauth+key*)"),
            "rendered: {rendered}"
        );

        if let Some(value) = prev {
            crate::env::set_var("JCODE_RUNTIME_PROVIDER", value);
        }
    }

    #[test]
    fn header_provider_auth_tag_honors_runtime_selection_and_oauth_first() {
        let _guard = crate::storage::lock_test_env();
        let prev = std::env::var_os("JCODE_RUNTIME_PROVIDER");

        let both = AuthStatus {
            anthropic: ProviderAuth {
                has_oauth: true,
                has_api_key: true,
                ..Default::default()
            },
            ..AuthStatus::default()
        };

        // Explicit API-key selection wins even when OAuth is available.
        crate::env::set_var("JCODE_RUNTIME_PROVIDER", "claude-api");
        assert_eq!(
            header_provider_auth_tag("anthropic", &both, ActiveCredentialOverrides::default()),
            "api-key"
        );

        // Explicit OAuth selection.
        crate::env::set_var("JCODE_RUNTIME_PROVIDER", "claude");
        assert_eq!(
            header_provider_auth_tag("anthropic", &both, ActiveCredentialOverrides::default()),
            "oauth"
        );

        // Auto (unset) prefers OAuth when both credentials are present.
        crate::env::remove_var("JCODE_RUNTIME_PROVIDER");
        assert_eq!(
            header_provider_auth_tag("anthropic", &both, ActiveCredentialOverrides::default()),
            "oauth"
        );

        // The "claude" display name resolves to the same Anthropic tagging.
        assert_eq!(
            header_provider_auth_tag("claude", &both, ActiveCredentialOverrides::default()),
            "oauth"
        );
        crate::env::set_var("JCODE_RUNTIME_PROVIDER", "claude-api");
        assert_eq!(
            header_provider_auth_tag("claude", &both, ActiveCredentialOverrides::default()),
            "api-key"
        );
        crate::env::remove_var("JCODE_RUNTIME_PROVIDER");

        // Auto falls back to the API key when no OAuth credential exists.
        let api_only = AuthStatus {
            anthropic: ProviderAuth {
                has_oauth: false,
                has_api_key: true,
                ..Default::default()
            },
            ..AuthStatus::default()
        };
        assert_eq!(
            header_provider_auth_tag("anthropic", &api_only, ActiveCredentialOverrides::default()),
            "api-key"
        );

        if let Some(value) = prev {
            crate::env::set_var("JCODE_RUNTIME_PROVIDER", value);
        } else {
            crate::env::remove_var("JCODE_RUNTIME_PROVIDER");
        }
    }

    #[test]
    fn build_persistent_header_prefers_configured_model_during_remote_connect() {
        let _guard = crate::storage::lock_test_env();
        let prev_model = std::env::var_os("JCODE_MODEL");
        let prev_provider = std::env::var_os("JCODE_PROVIDER");
        crate::env::set_var("JCODE_MODEL", "gpt-5.4");
        crate::env::set_var("JCODE_PROVIDER", "openai");

        let app = crate::tui::app::App::new_for_remote(None);
        let lines = build_persistent_header(&app, 80);
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.contains("GPT-5.4"));
        assert!(!rendered.contains("connecting to server…"));

        if let Some(prev_model) = prev_model {
            crate::env::set_var("JCODE_MODEL", prev_model);
        } else {
            crate::env::remove_var("JCODE_MODEL");
        }
        if let Some(prev_provider) = prev_provider {
            crate::env::set_var("JCODE_PROVIDER", prev_provider);
        } else {
            crate::env::remove_var("JCODE_PROVIDER");
        }
    }

    #[test]
    fn build_header_lines_omits_placeholder_provider_label_when_unknown() {
        // Reads model/provider env-derived state: without the env lock, the
        // sibling test that sets JCODE_MODEL=gpt-5.4 mid-flight leaks into this
        // render and the "loading session…" placeholder never appears. The
        // startup-phase label is also only rendered when no model hint is
        // known, so neutralize JCODE_MODEL/JCODE_PROVIDER for the duration
        // ("unknown" also suppresses the shared test home's config
        // default_model fallback, which another test may have persisted).
        let _guard = crate::storage::lock_test_env();
        let prev_model = std::env::var_os("JCODE_MODEL");
        let prev_provider = std::env::var_os("JCODE_PROVIDER");
        crate::env::set_var("JCODE_MODEL", "unknown");
        crate::env::remove_var("JCODE_PROVIDER");

        let mut app = crate::tui::app::App::new_for_remote(None);
        app.set_remote_startup_phase(crate::tui::app::RemoteStartupPhase::LoadingSession);

        // The model line lives in the persistent header now; the startup phase
        // label renders there without a bogus "(unknown)" provider tag.
        let lines = build_persistent_header(&app, 80);
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        if let Some(prev_model) = prev_model {
            crate::env::set_var("JCODE_MODEL", prev_model);
        } else {
            crate::env::remove_var("JCODE_MODEL");
        }
        if let Some(prev_provider) = prev_provider {
            crate::env::set_var("JCODE_PROVIDER", prev_provider);
        } else {
            crate::env::remove_var("JCODE_PROVIDER");
        }

        assert!(rendered.contains("loading session…"), "{rendered}");
        assert!(!rendered.contains("(unknown)"));
        assert!(!rendered.contains("(remote)"));
    }

    #[test]
    fn build_header_lines_hides_secondary_placeholder_during_brief_connecting_phase() {
        // Same env sensitivity as the placeholder test above: JCODE_MODEL /
        // JCODE_PROVIDER mutations from sibling tests change what renders.
        let _guard = crate::storage::lock_test_env();
        let app = crate::tui::app::App::new_for_remote(None);

        let lines = build_header_lines(&app, 80);
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(
            !rendered.contains("connecting to server…"),
            "brief connecting placeholder should not render the secondary detail line"
        );
        assert!(!rendered.contains("(remote)"));
    }

    #[test]
    fn auth_status_lines_show_all_providers_with_state_dots() {
        let auth = AuthStatus {
            anthropic: ProviderAuth {
                state: AuthState::Expired,
                has_oauth: true,
                oauth_state: AuthState::Expired,
                has_api_key: false,
            },
            openai: AuthState::Available,
            openai_has_oauth: false,
            openai_has_api_key: true,
            ..AuthStatus::default()
        };

        let rendered = build_auth_status_lines(&auth, ActiveCredentialOverrides::default())
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("anthropic(oauth)"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("openai(key)"), "rendered: {rendered}");
        // Providers the user has no credentials for stay out of the header.
        assert!(!rendered.contains("openrouter"), "rendered: {rendered}");
        assert!(!rendered.contains("copilot"), "rendered: {rendered}");
        assert!(!rendered.contains("○"), "rendered: {rendered}");
    }

    #[test]
    fn auth_status_lines_list_all_providers_when_nothing_configured() {
        let lines =
            build_auth_status_lines(&AuthStatus::default(), ActiveCredentialOverrides::default());
        assert!(
            !lines.is_empty(),
            "all providers should be listed: {lines:?}"
        );
    }

    #[test]
    fn auth_status_line_marks_active_credential_when_both_configured() {
        let _guard = crate::storage::lock_test_env();
        let prev = std::env::var_os("JCODE_RUNTIME_PROVIDER");
        let auth = AuthStatus {
            anthropic: ProviderAuth {
                state: AuthState::Available,
                has_oauth: true,
                oauth_state: AuthState::Available,
                has_api_key: true,
            },
            ..AuthStatus::default()
        };

        let rendered_with = |runtime: Option<&str>| {
            match runtime {
                Some(value) => crate::env::set_var("JCODE_RUNTIME_PROVIDER", value),
                None => crate::env::remove_var("JCODE_RUNTIME_PROVIDER"),
            }
            build_auth_status_lines(&auth, ActiveCredentialOverrides::default())
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        // Auto prefers OAuth: the star must sit on oauth, matching the header
        // provider tag's active-route answer.
        let rendered = rendered_with(None);
        assert!(
            rendered.contains("anthropic(oauth*+key)"),
            "rendered: {rendered}"
        );

        // Pinning the API key moves the star, keeping both surfaces consistent.
        let rendered = rendered_with(Some("claude-api"));
        assert!(
            rendered.contains("anthropic(oauth+key*)"),
            "rendered: {rendered}"
        );

        match prev {
            Some(value) => crate::env::set_var("JCODE_RUNTIME_PROVIDER", value),
            None => crate::env::remove_var("JCODE_RUNTIME_PROVIDER"),
        }
    }

    #[test]
    fn format_model_name_labels_slashed_models_with_active_provider() {
        // Regression for issue #329: a NVIDIA NIM model must be labeled with the
        // active provider's display name, not the fixed "OpenRouter" aggregator.
        assert_eq!(
            format_model_name("nvidia/nemotron-3-super-120b-a12b", "NVIDIA NIM"),
            "NVIDIA NIM: nvidia/nemotron-3-super-120b-a12b"
        );
        // The public aggregator still reads "OpenRouter".
        assert_eq!(
            format_model_name("anthropic/claude-sonnet-4", "OpenRouter"),
            "OpenRouter: anthropic/claude-sonnet-4"
        );
        // Missing provider name falls back to "OpenRouter" rather than an empty label.
        assert_eq!(
            format_model_name("deepseek/deepseek-chat", ""),
            "OpenRouter: deepseek/deepseek-chat"
        );
        // Non-slashed models are unaffected by the provider label.
        assert_eq!(
            format_model_name("claude-opus-4-6", "OpenRouter"),
            "Claude Opus"
        );
    }
}
