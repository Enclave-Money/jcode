//! Which daemon a connection talks to: the shared room, or the member's own.
//!
//! A team has two kinds of room, chosen when a chat is created:
//!
//! - **Shared** — everyone works in one checkout, on one desktop. A teammate's
//!   edit is simply there when you refresh. This is what a team is for.
//! - **Mine** — the member's own Unix user, home, checkout, port and desktop,
//!   so two people can build and TEST at the same time without one person's
//!   half-finished edit landing in the other's test run.
//!
//! Credentials alone did not need this: `acting_member` already routes a turn
//! to the right Claude account inside one daemon. Rooms need more than
//! credentials — a separate filesystem and a separate desktop — and those are
//! properties of a Unix user, so each room is a daemon running as that user.
//!
//! There is still ONE public door. The front bridge authenticates the bearer
//! token, resolves the room, and connects to that daemon's socket; the daemons
//! themselves are loopback-only and own nothing but their user's work.

use std::path::{Path, PathBuf};

/// The Unix user that owns the shared room.
///
/// Deliberately not the owner's account. "Shared" and "the owner's personal
/// user" being the same identity is the same conflation that let one member's
/// turn spend another's AI account: the team's things should not live in one
/// person's home.
pub const SHARED_USER: &str = "blaude-shared";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Room {
    /// One checkout and one desktop, for everyone.
    Shared,
    /// The member's own user, checkout and desktop.
    Mine,
}

impl Room {
    /// Parse the `room=` query parameter of the websocket URL.
    ///
    /// Anything unrecognised — including absent — is Shared. A client that has
    /// never heard of rooms therefore keeps working exactly as before, and a
    /// typo lands you with your team rather than somewhere private and
    /// confusing.
    pub fn from_query(query: Option<&str>) -> Room {
        let value = query
            .unwrap_or("")
            .split('&')
            .find_map(|pair| pair.strip_prefix("room="))
            .unwrap_or("");
        match value.trim().to_ascii_lowercase().as_str() {
            "mine" | "own" | "private" => Room::Mine,
            _ => Room::Shared,
        }
    }
}

/// The Unix username a member's own room runs as.
///
/// Read from `~/.jcode/member-users.json` (`{"email": "unixname"}`), written by
/// `deploy/team-server/provision-member.sh`. A member with no entry has no own
/// room yet, which is not an error — they get the shared room, because a team
/// server that refuses to talk to a member who has not been provisioned is
/// worse than one that seats them with everyone else.
pub fn unix_user_for(identity: Option<&str>, home_root: &Path) -> Option<String> {
    let identity = identity?;
    let raw = std::fs::read_to_string(home_root.join(".jcode/member-users.json")).ok()?;
    let map: serde_json::Value = serde_json::from_str(&raw).ok()?;
    map.get(identity)?.as_str().map(str::to_string)
}

/// The daemon socket this connection should be joined to.
///
/// `default_socket` is the front bridge's own daemon — what every connection
/// used before rooms existed, and still the answer when the request is for the
/// shared room on a server that has not been split into per-member users.
pub fn daemon_socket(
    room: Room,
    identity: Option<&str>,
    home_root: &Path,
    default_socket: &Path,
) -> PathBuf {
    let user = match room {
        Room::Shared => SHARED_USER.to_string(),
        Room::Mine => match unix_user_for(identity, home_root) {
            Some(user) => user,
            // Not provisioned: seat them in the shared room rather than
            // failing the connection.
            None => return default_socket.to_path_buf(),
        },
    };
    let socket = PathBuf::from("/home").join(&user).join(".jcode/jcode.sock");
    // A room whose daemon is not running must not black-hole the connection:
    // fall back to the door's own daemon, which is the shared room in the
    // un-split deployment.
    if socket.exists() {
        socket
    } else {
        default_socket.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_or_unknown_room_is_the_shared_one() {
        assert_eq!(Room::from_query(None), Room::Shared);
        assert_eq!(Room::from_query(Some("")), Room::Shared);
        assert_eq!(Room::from_query(Some("room=team")), Room::Shared);
        // A client from before rooms existed sends other parameters only.
        assert_eq!(Room::from_query(Some("token=abc")), Room::Shared);
    }

    #[test]
    fn the_private_room_is_recognised_by_its_spellings() {
        assert_eq!(Room::from_query(Some("room=mine")), Room::Mine);
        assert_eq!(Room::from_query(Some("room=MINE")), Room::Mine);
        assert_eq!(Room::from_query(Some("token=x&room=mine")), Room::Mine);
    }

    /// A member with no Unix user of their own is seated in the shared room.
    /// Refusing the connection would lock a not-yet-provisioned teammate out
    /// of a team they have legitimately joined.
    #[test]
    fn an_unprovisioned_member_falls_back_to_the_shared_room() {
        let temp = tempfile::TempDir::new().unwrap();
        let default = temp.path().join("default.sock");
        std::fs::write(&default, "").unwrap();

        let socket = daemon_socket(Room::Mine, Some("nobody@example.com"), temp.path(), &default);
        assert_eq!(socket, default);
    }

    #[test]
    fn a_member_map_resolves_a_unix_user() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".jcode")).unwrap();
        std::fs::write(
            temp.path().join(".jcode/member-users.json"),
            r#"{"akshay@enclave.money":"akshay2"}"#,
        )
        .unwrap();

        assert_eq!(
            unix_user_for(Some("akshay@enclave.money"), temp.path()).as_deref(),
            Some("akshay2")
        );
        assert_eq!(unix_user_for(Some("someone@else.com"), temp.path()), None);
        assert_eq!(unix_user_for(None, temp.path()), None);
    }

    /// A room whose daemon is not running must not black-hole the connection.
    #[test]
    fn a_room_with_no_running_daemon_falls_back() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".jcode")).unwrap();
        std::fs::write(
            temp.path().join(".jcode/member-users.json"),
            r#"{"a@b.com":"nosuchuser-for-tests"}"#,
        )
        .unwrap();
        let default = temp.path().join("default.sock");
        std::fs::write(&default, "").unwrap();

        // /home/nosuchuser-for-tests/... does not exist, so it falls back.
        assert_eq!(
            daemon_socket(Room::Mine, Some("a@b.com"), temp.path(), &default),
            default
        );
    }
}
