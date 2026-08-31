//! The workspace's screen, as a still frame.
//!
//! What the user wants to see is the machine the agent is working on: sign-ins
//! being completed, a browser under test, a desktop app being driven. This
//! module is the authenticated path to that picture.
//!
//! ## Why a proxy and not a direct connection
//!
//! The desktop runs on a display box (see `docs/display-stack.md` for why it
//! cannot share the team server's 0.5-vCPU e2-small). That box must NOT be
//! reachable from the internet — a published desktop is a remote shell with a
//! mouse. So the client never talks to it: it asks the team server, over the
//! same authenticated websocket it already uses, and the team server fetches
//! the frame across the internal network.
//!
//! That keeps one door and one credential. It also keeps the encode off the
//! team server: the display box renders and compresses, and this only moves
//! the bytes.

use anyhow::{Context, Result};

/// Where the display box serves frames, e.g. `http://10.160.0.15:8080/frame`.
///
/// Absent means no screen is attached to this workspace, which is the normal
/// state for a team that has not asked for one — not an error to shout about.
const SCREEN_URL_VAR: &str = "JCODE_SCREEN_URL";

/// A captured frame, ready to hand to a client.
///
/// `Debug` prints the size, never the pixels: a screenshot in a log or a test
/// failure is the picture this feature exists to keep behind an authenticated
/// door.
pub struct Frame {
    /// Image bytes, already compressed by the display box.
    pub bytes: Vec<u8>,
    /// MIME type as reported by the display box (`image/jpeg`, `image/png`).
    pub content_type: String,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

pub fn screen_url() -> Option<String> {
    std::env::var(SCREEN_URL_VAR)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Whether this workspace has a screen at all, so the client can show or hide
/// the control instead of offering a button that always fails.
pub fn is_attached() -> bool {
    screen_url().is_some()
}

/// Fetch one frame from the display box.
///
/// Deliberately a still image per request rather than a stream: a thumbnail
/// beside a conversation wants a picture every second or two, and a still
/// costs the team server nothing to relay. A real video path (WebRTC) is a
/// separate thing and belongs directly between the viewer and the display
/// box, once that box is exposed safely.
pub async fn frame(max_width: Option<u32>) -> Result<Frame> {
    let base = screen_url().context(
        "No screen is attached to this workspace. \
         Set JCODE_SCREEN_URL on the server to the display box's frame endpoint.",
    )?;
    // The downscale happens on the display box: shipping a 1080p PNG so the
    // client can shrink it wastes the link and the team server's CPU, and the
    // measured downscaled frame is ~5.5 KB against ~24 KB full size.
    let url = match max_width {
        Some(width) => format!("{base}?w={width}"),
        None => base,
    };
    let client = reqwest::Client::builder()
        // A wedged display box must not hold a client's request open: a stale
        // thumbnail is fine, a hung UI is not.
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("building the screen client")?;
    let response = client
        .get(&url)
        .send()
        .await
        .context("the display box did not answer")?;
    if !response.status().is_success() {
        anyhow::bail!("the display box answered {}", response.status());
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = response
        .bytes()
        .await
        .context("reading the frame")?
        .to_vec();
    if bytes.is_empty() {
        anyhow::bail!("the display box returned an empty frame");
    }
    Ok(Frame {
        bytes,
        content_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_screen_url_means_no_screen_attached() {
        let _lock = crate::jcode_home_test_lock();
        unsafe { std::env::remove_var(SCREEN_URL_VAR) };
        assert!(!is_attached(), "a workspace with no display box has no screen");
        assert!(screen_url().is_none());
    }

    /// An empty or whitespace value is a MISCONFIGURED server, not a screen.
    /// Treating it as attached would offer a monitor button that can only
    /// ever fail.
    #[test]
    fn a_blank_screen_url_is_not_a_screen() {
        let _lock = crate::jcode_home_test_lock();
        unsafe { std::env::set_var(SCREEN_URL_VAR, "   ") };
        assert!(!is_attached());
        unsafe { std::env::set_var(SCREEN_URL_VAR, "http://10.0.0.5:8080/frame") };
        assert!(is_attached(), "a real URL attaches a screen");
        unsafe { std::env::remove_var(SCREEN_URL_VAR) };
    }

    #[tokio::test]
    async fn fetching_without_a_screen_says_so_instead_of_failing_obscurely() {
        let _lock = crate::jcode_home_test_lock();
        unsafe { std::env::remove_var(SCREEN_URL_VAR) };
        let error = frame(None).await.expect_err("no screen means no frame");
        let text = format!("{error:#}");
        assert!(
            text.contains("No screen is attached"),
            "the reason must be legible to the person reading it: {text}"
        );
    }
}
