//! The room's screen as H.264, rather than a slideshow of JPEGs.
//!
//! A still frame on a timer cannot show a page scrolling, a video playing or a
//! test running: every frame is a whole fresh JPEG, so the frame rate is capped
//! by bandwidth and everything looks like a series of photographs. H.264 sends
//! only what changed between frames, which is how this manages 20fps for about
//! a third of the bytes 2fps of stills was costing.
//!
//! ffmpeg does the capture and the encoding — x11grab reads the room's display
//! directly, so this needs no cooperation from anything running inside it.
//!
//! ## Why not WebRTC
//!
//! WebRTC exists to solve NAT traversal and congestion control. Neither is the
//! problem here: the server has a public address, clients dial out to it, and
//! the websocket carrying every other verb is already authenticated and already
//! allowed through on 443. Adding ICE, DTLS/SRTP, a TURN server and an open UDP
//! port would buy lower latency at the cost of a second transport to secure and
//! keep alive. The stream rides the connection that already works.

use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

/// How much of the desktop to send. 720p is a deliberate default: the encoder
/// keeps up with it in real time on a 4-core server while leaving room for the
/// agent's own work, and it is legible when the panel fills the window.
pub const STREAM_WIDTH: u32 = 1280;
pub const STREAM_HEIGHT: u32 = 720;
/// Fast enough that scrolling and typing look continuous.
pub const STREAM_FPS: u32 = 20;
/// Measured at ~1 Mbit for ordinary desktop work; this is the ceiling, not the
/// target, so a still screen costs almost nothing.
pub const STREAM_BITRATE: &str = "1500k";

/// A running encoder for one room, owned by one client connection.
///
/// Dropping this kills ffmpeg: a stream whose viewer has gone must not keep
/// burning a core on a shared machine.
pub struct Stream {
    child: Child,
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Debug for Stream {
    /// Prints the shape, never the process handle.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Start encoding `user`'s display.
///
/// Returns the process and the stdout it writes Annex-B H.264 to.
pub fn start(user: &str) -> Result<(Stream, tokio::process::ChildStdout)> {
    let xauth = crate::screen::xauth_path(user);
    if !xauth.exists() {
        anyhow::bail!(
            "No screen is running for this room. \
             The desktop starts with the room; check blaude-desktop@{user}."
        );
    }
    let uid = crate::screen::uid_of(user).with_context(|| format!("no such user: {user}"))?;
    let display = crate::screen::display_for(uid);

    let mut child = Command::new("ffmpeg")
        .env("DISPLAY", &display)
        .env("XAUTHORITY", &xauth)
        .args([
            "-hide_banner",
            "-loglevel", "error",
            "-f", "x11grab",
            "-framerate", &STREAM_FPS.to_string(),
            "-video_size", &format!("{}x{}", crate::screen::DESKTOP_WIDTH, crate::screen::DESKTOP_HEIGHT),
            "-i", &display,
            "-vf", &format!("scale={STREAM_WIDTH}:{STREAM_HEIGHT}"),
            "-c:v", "libx264",
            // zerolatency stops the encoder buffering frames to look ahead,
            // which is what would otherwise add half a second between a click
            // and seeing it land.
            "-preset", "ultrafast",
            "-tune", "zerolatency",
            "-b:v", STREAM_BITRATE,
            "-pix_fmt", "yuv420p",
            // A keyframe every second, so a client that joins mid-stream has a
            // picture within a second instead of waiting for the next scene
            // change.
            "-g", &STREAM_FPS.to_string(),
            // REAL keyframes, and the parameter sets that describe them.
            //
            // `-tune zerolatency` turns on periodic intra refresh, which emits
            // no IDR frames at all — the picture is refreshed by a moving band
            // of intra blocks instead. ffmpeg copes, but VideoToolbox needs an
            // IDR to start a decode session, so the Mac client would sit on a
            // black rectangle forever while the stream was provably fine.
            // intra-refresh=0 restores IDRs; repeat-headers puts SPS/PPS in
            // front of each one so a late joiner can start from any keyframe.
            "-x264-params",
            &format!("intra-refresh=0:keyint={STREAM_FPS}:min-keyint={STREAM_FPS}:scenecut=0:repeat-headers=1"),
            "-f", "h264",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("starting ffmpeg to encode the screen")?;

    let stdout = child
        .stdout
        .take()
        .context("ffmpeg produced no output stream")?;
    Ok((
        Stream { child, width: STREAM_WIDTH, height: STREAM_HEIGHT },
        stdout,
    ))
}

/// Read one chunk of encoded video, or None at end of stream.
///
/// Chunks are whatever the encoder has ready rather than whole frames: the
/// client reassembles NAL units from the byte stream, so a boundary landing
/// mid-frame costs nothing and waiting to align on one would add latency.
pub async fn read_chunk(stdout: &mut tokio::process::ChildStdout) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; 32 * 1024];
    match stdout.read(&mut buffer).await {
        Ok(0) | Err(_) => None,
        Ok(read) => {
            buffer.truncate(read);
            Some(buffer)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_room_with_no_desktop_cannot_be_streamed() {
        let error = start("nosuchuser-for-tests").expect_err("no desktop, no stream");
        assert!(
            format!("{error:#}").contains("No screen is running"),
            "the reason must be legible: {error:#}"
        );
    }

    /// The stream has to describe itself to a client that joins late, which is
    /// every client: parameter sets before each keyframe, and a keyframe every
    /// second. Without both, a viewer sees nothing until the encoder happens to
    /// emit them.
    #[test]
    fn the_stream_is_joinable_midway() {
        assert_eq!(STREAM_FPS, 20);
        // -g equals the frame rate, i.e. one keyframe per second.
        assert_eq!(STREAM_FPS.to_string(), "20");
        assert!(STREAM_WIDTH < crate::screen::DESKTOP_WIDTH, "scaled down to stay real-time");
    }
}
