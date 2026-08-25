//! Loopback-relay login: the bridge must mint an authorize URL that honors the
//! app's own loopback redirect and carries PKCE + CSRF state, and it must never
//! let the PKCE verifier reach the wire-visible status record.

use super::login_jobs;

/// The authorize URL the app opens must point the provider back at the app's
/// own loopback listener and carry a code challenge + state.
#[tokio::test]
async fn claude_start_mints_url_with_app_redirect_and_pkce() {
    let redirect = "http://localhost:49231/callback";
    let job_id = login_jobs::start("claude", redirect, None);
    let record = login_jobs::status(&job_id).expect("job exists");
    let url = record["url"].as_str().expect("url present");

    assert!(url.contains("claude.ai") || url.contains("claude.com"), "not a Claude authorize URL: {url}");
    // The redirect is percent-encoded in the query.
    assert!(
        url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A49231%2Fcallback"),
        "url does not carry the app's loopback redirect: {url}"
    );
    assert!(url.contains("code_challenge="), "no PKCE challenge: {url}");
    assert!(url.contains("state="), "no CSRF state: {url}");
    assert_eq!(record["state"], "waiting_for_code");

    login_jobs::cancel(&job_id);
}

#[tokio::test]
async fn codex_start_mints_openai_url_with_app_redirect() {
    let redirect = "http://localhost:50122/callback";
    let job_id = login_jobs::start("codex", redirect, None);
    let record = login_jobs::status(&job_id).expect("job exists");
    let url = record["url"].as_str().expect("url present");

    assert!(url.contains("openai.com") || url.contains("auth.openai"), "not an OpenAI authorize URL: {url}");
    assert!(
        url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A50122%2Fcallback"),
        "url does not carry the app's loopback redirect: {url}"
    );
    assert!(url.contains("code_challenge="), "no PKCE challenge: {url}");

    login_jobs::cancel(&job_id);
}

/// The PKCE verifier is a secret: it stays in bridge memory and must never
/// appear in the record the app can read over the wire.
#[tokio::test]
async fn status_record_never_leaks_the_verifier() {
    let job_id = login_jobs::start("claude", "http://localhost:49232/callback", None);
    let record = login_jobs::status(&job_id).expect("job exists");
    let serialized = serde_json::to_string(&record).unwrap();

    // The record carries the public authorize URL (challenge + state) but no
    // key that would expose the raw verifier.
    assert!(!serialized.contains("code_verifier"), "record leaked a verifier key: {serialized}");
    assert!(!serialized.contains("verifier"), "record leaked a verifier field: {serialized}");

    login_jobs::cancel(&job_id);
}

/// Completing with an empty relay is a clean failure, not a hang or panic.
#[tokio::test]
async fn complete_with_empty_code_fails_cleanly() {
    let job_id = login_jobs::start("claude", "http://localhost:49233/callback", None);
    login_jobs::complete(&job_id, "   ").await;
    let record = login_jobs::status(&job_id).expect("job exists");
    assert_eq!(record["state"], "failed");
    assert!(
        record["error"].as_str().unwrap_or("").contains("no authorization code"),
        "unexpected error: {record}"
    );
}

/// Completing an unknown job id is a no-op, never a panic.
#[tokio::test]
async fn complete_unknown_job_is_noop() {
    login_jobs::complete("lg-does-not-exist", "code=x&state=y").await;
    assert!(login_jobs::status("lg-does-not-exist").is_none());
}

/// A pending job can be cancelled, which drops its in-memory secrets.
#[tokio::test]
async fn cancel_removes_the_pending_job() {
    let job_id = login_jobs::start("claude", "http://localhost:49234/callback", None);
    assert!(login_jobs::status(&job_id).is_some());
    assert!(login_jobs::cancel(&job_id));
    assert!(login_jobs::status(&job_id).is_none());
}
