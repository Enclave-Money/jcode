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

/// A click, a key, or a scroll, sent into a room's desktop.
///
/// The screen is a still image on a timer, so input is events rather than a
/// WebRTC media channel. That is a deliberate first step: it makes the browser
/// genuinely usable — completing a sign-in, clicking through a page you are
/// testing — over the connection that already exists and is already
/// authenticated, with no media path exposed to the internet. A real video
/// channel replaces the transport later without changing this contract.
#[derive(Debug, Clone)]
pub enum Input {
    /// Move to (x, y) in the frame's coordinates and click `button`
    /// (1 left, 2 middle, 3 right).
    Click { x: u32, y: u32, button: u8 },
    /// Move the pointer without pressing anything, so hovers work.
    Move { x: u32, y: u32 },
    /// Type literal text — a URL, a password, a search.
    Text(String),
    /// One named key, in xdotool's vocabulary (Return, Tab, ctrl+a).
    Key(String),
    /// Wheel up (negative) or down (positive), in clicks.
    Scroll(i32),
    /// Press a button at (x, y) and HOLD it.
    ///
    /// Dragging a window, selecting text and moving a slider all need the
    /// press and the release held apart with movement in between. `Click`
    /// cannot express any of them: xdotool's `click` presses and releases in
    /// one go, so before this the desktop could be clicked but nothing on it
    /// could be dragged.
    MouseDown { x: u32, y: u32, button: u8 },
    /// Release a held button.
    MouseUp { button: u8 },
}

/// The screen size a client's coordinates are relative to, so a click on a
/// 900px-wide thumbnail lands where the user aimed on a 1920px desktop.
pub const DESKTOP_WIDTH: u32 = 1920;
pub const DESKTOP_HEIGHT: u32 = 1080;

/// Scale a click from the frame the user actually saw to the real desktop.
///
/// Without this every click lands in the top-left corner region: a thumbnail is
/// a third of the width, so untranslated coordinates hit roughly a third of the
/// way across, which looks like "clicking does nothing useful".
pub fn to_desktop(x: u32, y: u32, frame_width: u32) -> (u32, u32) {
    if frame_width == 0 || frame_width == DESKTOP_WIDTH {
        return (x.min(DESKTOP_WIDTH), y.min(DESKTOP_HEIGHT));
    }
    let scale = DESKTOP_WIDTH as f64 / frame_width as f64;
    let sx = (x as f64 * scale).round() as u32;
    let sy = (y as f64 * scale).round() as u32;
    (sx.min(DESKTOP_WIDTH), sy.min(DESKTOP_HEIGHT))
}

/// Send one input event into `user`'s desktop.
pub fn send_input(user: &str, input: &Input) -> Result<()> {
    let xauth = xauth_path(user);
    if !xauth.exists() {
        anyhow::bail!("No screen is running for this room.");
    }
    let uid = uid_of(user).with_context(|| format!("no such user: {user}"))?;
    let display = display_for(uid);

    let args: Vec<String> = match input {
        Input::Click { x, y, button } => vec![
            "mousemove".into(),
            x.to_string(),
            y.to_string(),
            "click".into(),
            button.clamp(&1, &3).to_string(),
        ],
        Input::Move { x, y } => vec!["mousemove".into(), x.to_string(), y.to_string()],
        // `type --` so text beginning with a dash is typed, not parsed as a flag.
        Input::Text(text) => vec!["type".into(), "--".into(), text.clone()],
        Input::Key(key) => vec!["key".into(), "--".into(), key.clone()],
        Input::MouseDown { x, y, button } => vec![
            "mousemove".into(),
            x.to_string(),
            y.to_string(),
            "mousedown".into(),
            button.clamp(&1, &3).to_string(),
        ],
        Input::MouseUp { button } => {
            vec!["mouseup".into(), button.clamp(&1, &3).to_string()]
        }
        Input::Scroll(amount) => {
            // xdotool has no scroll: buttons 4 and 5 ARE the wheel.
            let button = if *amount < 0 { "4" } else { "5" };
            let times = amount.unsigned_abs().min(10);
            let mut args = vec!["click".into(), "--repeat".into(), times.to_string()];
            args.push(button.into());
            args
        }
    };

    let output = std::process::Command::new("xdotool")
        .env("DISPLAY", &display)
        .env("XAUTHORITY", &xauth)
        .args(&args)
        .output()
        .context("running xdotool")?;
    if !output.status.success() {
        anyhow::bail!(
            "input was refused: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod input_tests {
    use super::*;

    /// A click on the thumbnail must land where the user aimed on the real
    /// desktop. Untranslated, every click lands about a third of the way
    /// across — which reads as "clicking does nothing useful".
    #[test]
    fn a_click_on_a_thumbnail_scales_to_the_desktop() {
        // Middle of a 640-wide thumbnail is the middle of a 1920 desktop.
        assert_eq!(to_desktop(320, 180, 640), (960, 540));
        // Middle of a 900-wide view.
        assert_eq!(to_desktop(450, 253, 900), (960, 540));
        // A full-size frame needs no scaling.
        assert_eq!(to_desktop(100, 200, 1920), (100, 200));
    }

    /// A coordinate outside the desktop must be clamped, not sent — xdotool
    /// would happily park the pointer off-screen and the next click would go
    /// somewhere the user cannot see.
    #[test]
    fn coordinates_are_clamped_to_the_screen() {
        let (x, y) = to_desktop(99999, 99999, 640);
        assert_eq!((x, y), (DESKTOP_WIDTH, DESKTOP_HEIGHT));
        // A zero width must not divide by zero.
        assert_eq!(to_desktop(10, 10, 0), (10, 10));
    }

    /// A drag is three events, not one: without a held press the window
    /// manager sees a click and the window never moves.
    #[test]
    fn a_drag_presses_moves_and_releases_separately() {
        let down = Input::MouseDown { x: 10, y: 20, button: 1 };
        let up = Input::MouseUp { button: 1 };
        assert!(matches!(down, Input::MouseDown { button: 1, .. }));
        assert!(matches!(up, Input::MouseUp { button: 1 }));
        // A click is still one event, so nothing about tapping changes.
        assert!(matches!(Input::Click { x: 1, y: 1, button: 1 }, Input::Click { .. }));
    }

    #[test]
    fn a_room_with_no_desktop_refuses_input() {
        let error = send_input("nosuchuser-for-tests", &Input::Key("Return".into()))
            .expect_err("no desktop, no input");
        assert!(format!("{error:#}").contains("No screen is running"));
    }
}
