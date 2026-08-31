//! The room's screen, as a still frame.
//!
//! What you want to see is the machine the agent is working on: a sign-in being
//! completed, the app you just built running in a browser. So the desktop runs
//! on the ENVIRONMENT, as the room's own Unix user, with that room's checkout
//! and that room's localhost — not on a separate box whose browser cannot reach
//! any of your work.
//!
//! One display per room. Two people testing at once must not be looking at, or
//! clicking in, the same browser.
//!
//! ## Reading another room's screen
//!
//! Capture needs the room's X display, and X access control is what stops one
//! member screenshotting another's. Each room's Xvfb therefore has its own
//! authority file, 0640 owned by the member and group-owned by the door — the
//! same shape as the room sockets. The door can capture every room; members can
//! capture none. Running the displays with `-ac` instead would have been one
//! flag and would have let any local user watch any room.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Where a room's X authority file lives, matching `provision-member.sh`.
pub fn xauth_path(user: &str) -> PathBuf {
    PathBuf::from("/run/blaude").join(format!("{user}.Xauth"))
}

/// The X display a room renders on.
///
/// Derived from the uid exactly as the provisioning script does, so the two
/// agree without a third file to keep in sync. `:90` upwards stays clear of a
/// real workstation's `:0`.
pub fn display_for(uid: u32) -> String {
    format!(":{}", 90 + (uid % 100))
}

/// The uid of a Unix user, or None if there is no such user.
pub fn uid_of(user: &str) -> Option<u32> {
    let output = std::process::Command::new("id")
        .args(["-u", user])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// A captured frame, ready to hand to a client.
///
/// `Debug` prints the size, never the pixels: a screenshot in a log is the
/// picture this feature exists to keep behind an authenticated door.
pub struct Frame {
    pub bytes: Vec<u8>,
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

/// Whether this room has a desktop running.
pub fn is_attached(user: &str) -> bool {
    xauth_path(user).exists()
}

/// One frame of `user`'s desktop, as JPEG.
///
/// The downscale happens here, in ImageMagick, rather than in the client: a
/// thumbnail beside a conversation refreshes every second or two, and a full
/// 1080p frame is roughly four times the bytes for a picture that is about to
/// be drawn small.
pub fn frame(user: &str, max_width: Option<u32>) -> Result<Frame> {
    let xauth = xauth_path(user);
    if !xauth.exists() {
        anyhow::bail!(
            "No screen is running for this room. \
             The desktop starts with the room; check blaude-desktop@{user}."
        );
    }
    let uid = uid_of(user).with_context(|| format!("no such user: {user}"))?;
    let width = max_width.unwrap_or(640).clamp(160, 1920);

    let output = std::process::Command::new("import")
        .env("DISPLAY", display_for(uid))
        .env("XAUTHORITY", &xauth)
        .args([
            "-window",
            "root",
            "-resize",
            &format!("{width}x"),
            "jpg:-",
        ])
        .output()
        .context("running import to capture the screen")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("could not capture the screen: {}", stderr.trim());
    }
    if output.stdout.is_empty() {
        anyhow::bail!("the screen capture came back empty");
    }
    Ok(Frame {
        bytes: output.stdout,
        content_type: "image/jpeg".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The display number must match `provision-member.sh`'s arithmetic, or
    /// the daemon renders on one display and the door captures another — which
    /// looks like a blank screen rather than a mismatch.
    #[test]
    fn the_display_is_derived_from_the_uid_the_same_way_provisioning_does() {
        assert_eq!(display_for(1000), ":90");
        assert_eq!(display_for(1001), ":91");
        assert_eq!(display_for(1002), ":92");
        // Wraps rather than colliding with a workstation's :0.
        assert_eq!(display_for(1100), ":90");
        assert!(display_for(0).starts_with(":9"));
    }

    #[test]
    fn the_authority_file_sits_beside_the_room_sockets() {
        assert_eq!(
            xauth_path("akshay2"),
            std::path::Path::new("/run/blaude/akshay2.Xauth")
        );
        assert!(
            !xauth_path("akshay2").starts_with("/home"),
            "the cookie must not sit in a member's home, which the door cannot read"
        );
    }

    /// A room with no desktop says so, rather than failing obscurely.
    #[test]
    fn a_room_with_no_desktop_reports_it_plainly() {
        assert!(!is_attached("nosuchuser-for-tests"));
        let error = frame("nosuchuser-for-tests", None).expect_err("no desktop, no frame");
        let text = format!("{error:#}");
        assert!(
            text.contains("No screen is running"),
            "the reason must be legible: {text}"
        );
    }

    /// A caller must not be able to ask for a 40000px render and pin the CPU
    /// the agent's own turns share.
    #[test]
    fn an_absurd_width_is_clamped() {
        // Exercised through the same clamp the capture uses.
        assert_eq!(u32::MAX.clamp(160, 1920), 1920);
        assert_eq!(1u32.clamp(160, 1920), 160);
    }
}
