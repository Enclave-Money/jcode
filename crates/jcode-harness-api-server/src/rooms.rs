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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Room {
    /// One checkout and one desktop, for everyone. The default everywhere:
    /// an unknown room, an unprovisioned member and an older client all land
    /// with the team rather than somewhere private and confusing.
    #[default]
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

/// Where a room's daemon listens.
///
/// Deliberately NOT inside the member's home. A member's `~/.jcode` is 0700
/// because it holds their Claude tokens, so the door could not reach a socket
/// there; opening that directory up so it could would hand every member read
/// access to every other member's credentials. The sockets therefore live in
/// one directory of their own, each owned by its member with an ACL granting
/// exactly the door's user, so the door can connect and other members cannot.
/// (A shared gid on the daemon was audit R1: every agent subprocess inherited
/// it, which handed each member's agent every other room's socket.)
///
/// `provision-member.sh` sets `JCODE_SOCKET` to this exact path, so the daemon
/// and the door agree by construction rather than by both guessing.
pub fn room_socket(user: &str) -> PathBuf {
    PathBuf::from("/run/blaude").join(format!("{user}.sock"))
}

/// The home of whoever runs the door, where the member map lives.
pub fn door_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
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

/// Where a room's work happens.
///
/// The shared room is the team's one checkout; a member's room is their own
/// clone, so two people can edit and test at once. This is the fallback the
/// bridge uses when a client creates a session without naming a directory —
/// it used to fall back to the BRIDGE's own $HOME, which put every room's work
/// in the door's home and quietly undid per-room checkouts.
pub fn project_dir(room: Room, identity: Option<&str>, home_root: &Path) -> Option<PathBuf> {
    let user = match room {
        Room::Shared => return Some(PathBuf::from("/srv/blaude/project")),
        Room::Mine => unix_user_for(identity, home_root)?,
    };
    // Deliberately NOT checked with is_dir(): a member's home is 0750 and the
    // door is not in their group, so the door cannot stat it — the check was
    // always false and silently disabled per-room checkouts entirely. The
    // DAEMON runs as the member and can see its own directory, and being in
    // the map is what says it was provisioned.
    Some(PathBuf::from("/home").join(user).join("project"))
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
    // The safe fallback for a room we cannot seat directly.
    //
    // NEVER the door's own daemon on a split server: it runs as the door user,
    // who holds every room's X cookie, every member and owner bearer, the TLS
    // key, and every member's OAuth tokens. Routing an unprovisioned member —
    // or one whose room daemon is briefly down — there hands their agent all
    // of it. The shared room's daemon runs as an UNPRIVILEGED room user
    // (blaude-shared), so seat them there instead. The door's daemon is the
    // fallback ONLY on an un-split server, where no room sockets exist and it
    // is the single daemon everyone already shares.
    let shared = room_socket(SHARED_USER);
    let user = match room {
        Room::Shared => SHARED_USER.to_string(),
        Room::Mine => match unix_user_for(identity, home_root) {
            Some(user) => user,
            // Not provisioned yet: the shared room, never the door.
            None => return fallback_socket(shared.exists(), &shared, default_socket),
        },
    };
    let socket = room_socket(&user);
    if socket.exists() {
        socket
    } else {
        fallback_socket(shared.exists(), &shared, default_socket)
    }
}

/// Where to seat a connection we cannot route to its own room's daemon.
fn fallback_socket(shared_exists: bool, shared: &Path, default_socket: &Path) -> PathBuf {
    if shared_exists {
        shared.to_path_buf()
    } else {
        default_socket.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R2: on a split server (the shared room socket exists) the fallback is
    /// the shared room's unprivileged daemon, never the door's own — the door
    /// user holds every bearer, X cookie, TLS key and OAuth token. Only an
    /// un-split server (no room sockets) falls back to the door.
    #[test]
    fn fallback_prefers_the_shared_room_over_the_door() {
        let shared = Path::new("/run/blaude/blaude-shared.sock");
        let door = Path::new("/run/blaude/door-own.sock");
        assert_eq!(fallback_socket(true, shared, door), shared);
        assert_eq!(fallback_socket(false, shared, door), door);
    }

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

        let socket = daemon_socket(
            Room::Mine,
            Some("nobody@example.com"),
            temp.path(),
            &default,
        );
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

    /// The socket must not live in the member's home: `~/.jcode` is 0700
    /// because it holds their Claude tokens, and opening it so the door could
    /// reach a socket there would expose every member's credentials to every
    /// other member.
    #[test]
    fn a_rooms_socket_is_outside_the_members_home() {
        let socket = room_socket("akshay2");
        assert_eq!(socket, std::path::Path::new("/run/blaude/akshay2.sock"));
        assert!(
            !socket.starts_with("/home"),
            "a room socket must never sit inside a member's home: {socket:?}"
        );
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

#[cfg(test)]
mod project_tests {
    use super::*;

    /// The shared room is the team's one checkout.
    #[test]
    fn the_shared_room_works_in_the_team_checkout() {
        let temp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            project_dir(Room::Shared, None, temp.path()),
            Some(std::path::PathBuf::from("/srv/blaude/project"))
        );
    }

    /// An unprovisioned member has no directory of their own, and the caller
    /// then falls back rather than inventing one — creating a session in a
    /// path that does not exist fails in a much more confusing way.
    #[test]
    fn an_unmapped_member_has_no_project_directory() {
        let temp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            project_dir(Room::Mine, Some("nobody@example.com"), temp.path()),
            None
        );
        assert_eq!(project_dir(Room::Mine, None, temp.path()), None);
    }

    /// Being in the member map is what says a member was provisioned — the
    /// door cannot check the directory itself.
    ///
    /// A member's home is 0750 and the door is not in their group, so an
    /// `is_dir()` guard here is ALWAYS false and silently sends every member's
    /// work to the door's home instead of their own checkout. Seen live.
    #[test]
    fn a_mapped_member_gets_their_own_directory_without_the_door_stat_ing_it() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".jcode")).unwrap();
        std::fs::write(
            temp.path().join(".jcode/member-users.json"),
            r#"{"a@b.com":"akshay2"}"#,
        )
        .unwrap();
        assert_eq!(
            project_dir(Room::Mine, Some("a@b.com"), temp.path()),
            Some(std::path::PathBuf::from("/home/akshay2/project")),
            "the path must not depend on the door being able to see it"
        );
    }
}

/// Ask for the door's accounts to be redistributed to the rooms.
///
/// Sign-in lands every account in the DOOR's auth file, but turns run in a
/// ROOM as that room's user, reading that user's own file. Distribution is
/// done by a root-owned unit (`blaude-sync-room-auth`) watching the door's
/// file, because the daemon owns `~/.jcode` and re-tightens it to 0700 on
/// startup — a door write there is racing a process whose job is to close it.
///
/// So this only touches the watched file, and the path unit does the rest.
/// Returns whether the nudge succeeded, not whether rooms were written.
/// Ask for this member's own room to be built.
///
/// Provisioning a room means creating a Unix user, a checkout and a desktop,
/// which needs root — so the door cannot do it. It appends the email to a
/// queue a root path-unit watches, exactly as the credential sync does. Until
/// that runs the member has no own room and `?room=mine` falls back to the
/// shared daemon, so the queue is what stops "Mine" quietly meaning "Shared"
/// forever for everyone who joined by invitation.
///
/// Append-only and deduplicated: the runner is free to be slow or to fail and
/// retry, and a member claiming a second ticket must not queue twice.
pub fn request_member_provision(email: &str, home_root: &Path) -> bool {
    let email = email.trim().to_lowercase();
    if email.is_empty() || !email.contains('@') {
        return false;
    }
    let dir = home_root.join(".jcode");
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let path = dir.join("provision-queue");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == email) {
        // Already queued. Touch it so a runner that died mid-queue is woken
        // again rather than leaving this member waiting forever.
        return std::fs::write(&path, existing.as_bytes()).is_ok();
    }
    let next = format!("{existing}{email}\n");
    std::fs::write(&path, next).is_ok()
}

pub fn request_credential_sync(home_root: &Path) -> bool {
    let path = home_root.join(".jcode/auth.json");
    let Ok(contents) = std::fs::read(&path) else {
        return false;
    };
    // Rewriting identical bytes is enough for PathChanged to fire, and cannot
    // corrupt the store the way an edit could.
    std::fs::write(&path, contents).is_ok()
}

#[cfg(test)]
mod provision_queue_tests {
    use super::*;

    #[test]
    fn a_member_is_queued_once_however_often_they_claim() {
        let temp = tempfile::tempdir().expect("temp");
        assert!(request_member_provision(
            "Akshay@Enclave.money",
            temp.path()
        ));
        assert!(request_member_provision(
            "akshay@enclave.money",
            temp.path()
        ));
        let queue =
            std::fs::read_to_string(temp.path().join(".jcode/provision-queue")).expect("queue");
        assert_eq!(
            queue.lines().filter(|l| !l.trim().is_empty()).count(),
            1,
            "a second claim must not queue a second room: {queue}"
        );
        // Normalised, so the runner does not create two users for one person.
        assert_eq!(queue.trim(), "akshay@enclave.money");
    }

    #[test]
    fn two_members_both_get_queued() {
        let temp = tempfile::tempdir().expect("temp");
        request_member_provision("a@b.com", temp.path());
        request_member_provision("c@d.com", temp.path());
        let queue =
            std::fs::read_to_string(temp.path().join(".jcode/provision-queue")).expect("queue");
        let lines: Vec<&str> = queue.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines, vec!["a@b.com", "c@d.com"]);
    }

    #[test]
    fn nonsense_is_not_queued() {
        let temp = tempfile::tempdir().expect("temp");
        assert!(!request_member_provision("", temp.path()));
        assert!(!request_member_provision("not-an-email", temp.path()));
        assert!(!temp.path().join(".jcode/provision-queue").exists());
    }
}
