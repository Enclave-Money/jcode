//! Harness API bridge: exposes the stable versioned harness API on its own
//! Unix socket and translates to the internal (legacy) blaude protocol.
//!
//! Architecture (milestone 2 of docs/HARNESS_API_AND_DESKTOP_REWRITE.md):
//! - Listens on `~/.jcode/jcode-api.sock` (or `JCODE_API_SOCKET`).
//! - For each API client, dials the legacy daemon socket (`JCODE_SOCKET` or
//!   `~/.jcode/jcode.sock`) and speaks `subscribe`/`message`/... on its
//!   behalf.
//! - Translation is JSON-to-JSON so this crate does not depend on the heavy
//!   internal protocol types and cannot be broken by additive internal
//!   changes.
//!
//! This keeps the daemon untouched while the API surface stabilizes. Once
//! proven, the same translation can move in-process behind a `hello` sniff on
//! the main socket.

#[cfg(test)]
pub(crate) fn jcode_home_test_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub mod background_progress;
pub mod blaude_account;
pub mod council_jobs;
pub mod github_auth_jobs;
pub mod team_create_jobs;
pub mod login_jobs;
pub mod team_access;
pub mod permissions;
pub mod translate;
pub mod ws;

use anyhow::{Context, Result};
use jcode_harness_api::{API_VERSION_MAJOR, ApiEvent, ErrorCode, ServerFrame};
use serde_json::Value;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
// Unix sockets on Unix, named pipes on Windows, one API. Without this the
// bridge simply did not compile for Windows, so the SDK could not run there at
// all.
use jcode_transport::{Listener, Stream, WriteHalf};

/// Dial the legacy daemon and start a dedicated reader task that forwards
/// complete newline-delimited frames over an unbounded channel.
///
/// The reader lives OUTSIDE the connection's `select!` loop on purpose:
/// `AsyncBufReadExt::read_line` is not cancellation-safe (a dropped future
/// discards bytes already pulled out of the BufReader), and the loop's other
/// arms — a 900ms permission tick, every inbound client frame — fire
/// constantly. Reading a multi-MB history/state line straight off a select
/// arm therefore truncates and silently loses it. An mpsc receiver, by
/// contrast, IS cancel-safe, so the loop can await it freely.
async fn dial_legacy(
    legacy_socket: &std::path::Path,
) -> Option<(tokio::sync::mpsc::Receiver<String>, WriteHalf)> {
    let stream = Stream::connect(legacy_socket).await.ok()?;
    let (read, write) = stream.into_split();
    // BOUNDED, not unbounded: if the client stops draining (a slow or stalled
    // WebSocket peer), a full channel makes the reader task park on send,
    // which stops pulling from THIS client's daemon socket — per-connection
    // backpressure. An unbounded channel instead buffers the daemon's output
    // without limit and one slow client can OOM the shared bridge.
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(1024);
    tokio::spawn(async move {
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break, // EOF / error: dropping tx signals close
                Ok(_) => {
                    if tx.send(std::mem::take(&mut line)).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    Some((rx, write))
}

// Socket paths live in `jcode-harness-api` so clients and the bridge can never
// resolve different directories (they once did, and the desktop app could not
// connect as a result).
pub use jcode_harness_api::{api_socket_path, legacy_socket_path};

/// Largest single request frame accepted from an API client, in bytes.
///
/// `read_line` grows its buffer until it finds a newline, so a client that
/// never sends one makes the bridge allocate without bound: one connection can
/// exhaust the host's memory, and the bridge serves every client on the
/// machine. 16 MiB is far above any legitimate frame (the largest real one is a
/// message carrying base64 images) and far below a problem.
const MAX_FRAME_BYTES: u64 = 16 * 1024 * 1024;

/// Read one newline-delimited frame, refusing to buffer more than
/// `MAX_FRAME_BYTES`. Returns `Ok(0)` at end of stream, like `read_line`.
async fn read_frame<R>(reader: &mut R, line: &mut String) -> std::io::Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    line.clear();
    let mut limited = tokio::io::AsyncReadExt::take(reader, MAX_FRAME_BYTES);
    let read = limited.read_line(line).await?;
    // A full buffer with no terminator means the frame exceeded the cap (or is
    // exactly at it and unterminated); either way it cannot be trusted.
    if read as u64 == MAX_FRAME_BYTES && !line.ends_with('\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame exceeds {MAX_FRAME_BYTES} byte limit"),
        ));
    }
    Ok(read)
}

/// Run the bridge accept loop forever.
#[cfg(unix)]
pub(crate) struct InstanceLock {
    _file: std::fs::File,
    path: PathBuf,
}

#[cfg(unix)]
impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Deliberately do NOT unlink the lock file. The flock releases on fd
        // close regardless, and unlinking raced: a dying bridge deleted the
        // path while a NEWER bridge held the lock on that inode, so the next
        // spawner created a fresh inode, locked it, and TWO bridges ran with
        // split state (job tables, WS vs unix socket). A persistent empty
        // lock file is harmless.
        let _ = &self.path;
    }
}

/// Take the exclusive bridge lock beside the API socket, or report that a live
/// bridge already holds it. `flock` is released by the kernel when the holder
/// dies, so a crashed bridge never wedges the next one out.
#[cfg(unix)]
pub(crate) fn single_instance_lock(api_socket: &std::path::Path) -> Result<Option<InstanceLock>> {
    use std::os::fd::AsRawFd;

    let path = api_socket.with_extension("lock");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // Retry loop: if the inode we locked is no longer the inode at the
    // path (an old build's Drop unlinked it mid-race), the lock protects
    // nothing — lock the fresh inode instead.
    use std::os::unix::fs::MetadataExt;
    for _ in 0..3 {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open bridge lock {}", path.display()))?;
        let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if !taken {
            return Ok(None);
        }
        let locked_ino = file.metadata().map(|m| m.ino()).unwrap_or(0);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.ino() == locked_ino => {
                return Ok(Some(InstanceLock { _file: file, path }));
            }
            _ => continue,
        }
    }
    Ok(None)
}

pub async fn run_bridge(api_socket: PathBuf, legacy_socket: PathBuf) -> Result<()> {
    // Only one bridge may own the socket.
    //
    // This is the fix for clients seeing "disconnected: harness API stream
    // closed" at random: every desktop client spawned a bridge on demand, and
    // each new bridge unlinked the live socket and bound its own. The older
    // bridges kept running with their connected clients, but the *pathname*
    // now pointed at the newest one, and each reconnect churned the same way.
    // Whoever lost the race had its clients dropped. Refusing to start when a
    // live bridge holds the lock makes on-demand spawning idempotent, which is
    // what every caller already assumes.
    #[cfg(unix)]
    let _lock = match single_instance_lock(&api_socket)? {
        Some(lock) => lock,
        None => {
            eprintln!(
                "harness API bridge: another bridge already owns {}; exiting",
                api_socket.display()
            );
            return Ok(());
        }
    };
    // A stale socket file blocks bind on Unix. On Windows there is no file to
    // remove: the pipe namespace is not the filesystem. Safe to unlink here
    // only because we hold the exclusive lock above, so no live bridge owns it.
    #[cfg(unix)]
    let _ = std::fs::remove_file(&api_socket);
    if let Some(parent) = api_socket.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // `mut` only on Windows: the named-pipe listener republishes a pipe
    // instance on every accept, so accepting takes `&mut self`. Unix's
    // UnixListener::accept takes `&self`, and an unconditional `mut` there is
    // an unused_mut warning, so the binding is declared per platform rather
    // than warning on every build.
    #[cfg(windows)]
    let mut listener = Listener::bind(&api_socket)
        .with_context(|| format!("bind API socket {}", api_socket.display()))?;
    #[cfg(unix)]
    let listener = Listener::bind(&api_socket)
        .with_context(|| format!("bind API socket {}", api_socket.display()))?;
    // Restrict the socket to its owner, matching the daemon socket it fronts.
    //
    // Without this the bridge widens access to everything behind it: the
    // daemon socket is 0600, but a default-umask bind here produced 0755, so
    // any local user could drive sessions, read transcripts, and spend the
    // owner's provider tokens. A bridge must never be more permissive than
    // the thing it bridges to.
    //
    // Unix only: a Windows named pipe carries an ACL rather than a file mode,
    // and the transport applies it when publishing the pipe.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&api_socket, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict API socket {}", api_socket.display()))?;
    }
    // Identify this bridge to launchers: pid + which binary (path AND its
    // mtime at spawn). An orphaned bridge from an old build used to squat on
    // the socket forever — dead daemon, missing verbs — and every new app
    // trusted whatever answered. A launcher reads this file, compares the
    // exe identity against its own embedded binary, and replaces a bridge
    // that is not its own, current build (RuntimeLauncher.staleBridge).
    {
        let ident_path = api_socket.with_extension("ident.json");
        let exe = std::env::current_exe().unwrap_or_default();
        let exe_mtime_ms = std::fs::metadata(&exe)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let ident = serde_json::json!({
            "pid": std::process::id(),
            "exe": exe.to_string_lossy(),
            "exe_mtime_ms": exe_mtime_ms,
            "started_ms": started_ms,
        });
        let _ = std::fs::write(&ident_path, ident.to_string());
    }
    eprintln!(
        "harness API bridge: listening on {} -> {}",
        api_socket.display(),
        legacy_socket.display()
    );
    // Realtime clients speak the same NDJSON frames over WebSocket (one text
    // frame per line), token-guarded, loopback-only. `JCODE_API_WS_PORT=off`
    // disables it; a bind failure degrades to unix-socket-only rather than
    // killing the bridge (another runtime's bridge may hold the port).
    match ws::spawn_from_env(legacy_socket.clone()).await {
        Ok(Some(addr)) => eprintln!("harness API bridge: websocket on ws://{addr}/api"),
        Ok(None) => {}
        Err(error) => eprintln!("harness API bridge: websocket listener disabled: {error:#}"),
    }
    loop {
        let (stream, _) = listener.accept().await?;
        let legacy = legacy_socket.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_api_client(stream, legacy).await {
                eprintln!("harness API bridge: client ended: {error:#}");
            }
        });
    }
}

/// Best-effort revival of a dead daemon under a live bridge: run our own
/// binary's `server start` (the exact daemonize `api-bridge` does at startup),
/// at most once per 15s process-wide. Managed team servers own the daemon via
/// systemd (JCODE_BRIDGE_NO_SPAWN) — never self-heal there.
fn respawn_daemon_throttled() {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    let no_spawn = std::env::var("JCODE_BRIDGE_NO_SPAWN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if no_spawn {
        return;
    }
    static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    let last = LAST.get_or_init(|| Mutex::new(None));
    {
        let mut guard = match last.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        if let Some(at) = *guard {
            if at.elapsed() < Duration::from_secs(15) {
                return;
            }
        }
        *guard = Some(Instant::now());
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    eprintln!("harness API bridge: attempting daemon respawn");
    let _ = std::process::Command::new(exe)
        .args(["server", "start"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

async fn handle_api_client(stream: Stream, legacy_socket: PathBuf) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    // Local unix-socket clients are the machine owner by definition (0600).
    let identity = blaude_account::identity().or_else(|| std::env::var("USER").ok());
    handle_api_io(BufReader::new(read_half), write_half, legacy_socket, identity, true).await
}

/// Per-client bridge loop, generic over the client transport: the unix socket
/// path and the WebSocket relay (src/ws.rs, via an in-memory duplex) share it.
/// `is_owner` gates team-management verbs — members can attach and steer, but
/// never invite, enumerate, or revoke.
pub(crate) async fn handle_api_io<R, W>(
    reader: R,
    write_half: W,
    legacy_socket: PathBuf,
    identity: Option<String>,
    is_owner: bool,
) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut reader = reader;
    let mut write_half = write_half;
    let mut line = String::new();

    // 1. Handshake: first frame must be hello with a compatible version.
    read_frame(&mut reader, &mut line).await?;
    // A malformed first frame used to abort the task, closing the connection
    // with no reply at all: the client saw only an EOF and could not tell a
    // protocol mistake from a crashed bridge. Say what was wrong, then close.
    let hello: Value = match serde_json::from_str(line.trim()) {
        Ok(value) => value,
        Err(error) => {
            let frame = ServerFrame::event(ApiEvent::Error {
                code: ErrorCode::InvalidRequest,
                message: format!("first frame must be a JSON `hello`: {error}"),
            });
            write_json_line(&mut write_half, &frame).await?;
            return Ok(());
        }
    };
    let reply_to = hello["id"].as_u64().unwrap_or(0);
    let compatible = hello["req"] == "hello"
        && hello["min_version"].as_u64().unwrap_or(0) <= u64::from(API_VERSION_MAJOR)
        && hello["max_version"].as_u64().unwrap_or(0) >= u64::from(API_VERSION_MAJOR);
    if !compatible {
        let frame = ServerFrame::reply(
            reply_to,
            ApiEvent::Error {
                code: ErrorCode::UnsupportedVersion,
                message: format!(
                    "bridge speaks API v{API_VERSION_MAJOR}; this client asked for v{}..=v{}",
                    hello["min_version"].as_u64().unwrap_or(0),
                    hello["max_version"].as_u64().unwrap_or(0),
                ),
            },
        );
        write_json_line(&mut write_half, &frame).await?;
        return Ok(());
    }
    let hello_ok = ServerFrame::reply(
        reply_to,
        ApiEvent::HelloOk {
            identity: identity.clone(),
            version: API_VERSION_MAJOR,
            server: format!("jcode-harness-api-bridge/{}", env!("CARGO_PKG_VERSION")),
            capabilities: [
                "sessions",
                "streaming",
                "persisted_session_discovery",
                "runtime_info",
                "api_key_provisioning",
                "session_archive",
                "session_retention",
                "session_files",
                "permissions",
                "team_notes",
                "presence",
                "add_dir",
                "skill_install",
                "session_modes",
                "council_jobs",
                "login_jobs",
                "codex_login",
                "team_access",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        },
    );
    write_json_line(&mut write_half, &hello_ok).await?;

    // 2. Dial the legacy daemon for this client. A fresh team server may not
    //    have a bootable daemon yet (no provider credentials) — that must NOT
    //    kill the connection, or the bridge-local verbs that FIX it (the
    //    login jobs, invites) can never run. Serve bridge-only until the
    //    daemon appears, then upgrade in place on the next daemon-bound
    //    request.
    let (mut legacy_rx, mut legacy_write) = match dial_legacy(&legacy_socket).await {
        Some((rx, write)) => (Some(rx), Some(write)),
        None => {
            eprintln!(
                "harness API bridge: daemon unreachable; serving bridge-only verbs until it comes up"
            );
            // "Until it comes up" needs a mechanism: the daemon is spawned once
            // at bridge startup, so a daemon that dies later (crash, wedged
            // token refresh) stayed dead forever and every client saw an
            // eternal reconnect loop. Re-run our own binary's daemonize,
            // throttled so reconnect storms don't fork-bomb.
            respawn_daemon_throttled();
            (None, None)
        }
    };

    let mut state = translate::BridgeState::default();
    state.identity = identity;

    // 3. Pump both directions in one select loop so translation state stays
    //    single-threaded. A third branch watches the safety queue so
    //    permission prompts reach API clients (they are file-mediated by
    //    design — see src/permissions.rs).
    let mut api_line = String::new();
    let mut permission_poll = tokio::time::interval(std::time::Duration::from_millis(900));
    permission_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut announced_permissions: std::collections::HashSet<String> = Default::default();
    loop {
        tokio::select! {
            _ = permission_poll.tick() => {
                let Some(session_id) = state.session_id.clone() else { continue };
                let pending = tokio::task::block_in_place(permissions::pending);
                let current: std::collections::HashSet<String> =
                    pending.iter().map(|p| p.id.clone()).collect();
                for request in &pending {
                    if announced_permissions.insert(request.id.clone()) {
                        let frame = ServerFrame::event(ApiEvent::PermissionRequest {
                            session_id: session_id.clone(),
                            request_id: request.id.clone(),
                            tool_name: request.action.clone(),
                            description: request.description.clone(),
                        });
                        write_json_line(&mut write_half, &frame).await?;
                    }
                }
                let resolved: Vec<String> = announced_permissions
                    .iter()
                    .filter(|id| !current.contains(*id))
                    .cloned()
                    .collect();
                for id in resolved {
                    announced_permissions.remove(&id);
                    let approved = tokio::task::block_in_place(|| permissions::decision_for(&id));
                    let frame = ServerFrame::event(ApiEvent::PermissionResolved {
                        session_id: session_id.clone(),
                        request_id: id,
                        approved,
                    });
                    write_json_line(&mut write_half, &frame).await?;
                }
            }
            n = read_frame(&mut reader, &mut api_line) => {
                let n = match n {
                    Ok(n) => n,
                    // An oversized frame is unrecoverable: the stream is now
                    // mid-frame with no way to resynchronise. Report and close.
                    Err(error) => {
                        let frame = ServerFrame::event(ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message: error.to_string(),
                        });
                        write_json_line(&mut write_half, &frame).await?;
                        return Ok(());
                    }
                };
                if n == 0 { return Ok(()); }
                if api_line.trim().is_empty() { continue; }
                let request: Value = match serde_json::from_str(api_line.trim()) {
                    Ok(value) => value,
                    Err(error) => {
                        // No `reply_to`: the id lived in the frame that failed
                        // to parse, so there is nothing to correlate against.
                        let frame = ServerFrame::event(ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message: error.to_string(),
                        });
                        write_json_line(&mut write_half, &frame).await?;
                        continue;
                    }
                };
                // install_skill clones a repo — an async job the sync
                // translate machine can't run. Handle it here: the await
                // stalls only THIS connection (clients install on a spare
                // connection), and on success the daemon reloads its registry
                // so the skill is usable without a restart.
                // Council runs fan a prompt to 2-3 models and take minutes —
                // another async job the sync translate machine can't host.
                // The await stalls only THIS connection; clients run councils
                // on a spare connection.
                // Council runs are bridge-global JOBS, not per-connection
                // awaits: `run_council` replies immediately with a job id,
                // and any connection — including one from a relaunched app —
                // can await, poll, cancel, or list runs. Results persist in
                // ~/.jcode/council-runs/.
                if request["req"].as_str() == Some("run_council") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let name = request["name"].as_str().unwrap_or_default().to_string();
                    let prompt = request["prompt"].as_str().unwrap_or_default().to_string();
                    let cwd = request["working_dir"].as_str().map(str::to_string);
                    let tag = request["tag"].as_str().map(str::to_string);
                    let frame = if name.is_empty() || prompt.is_empty() {
                        ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message: "run_council needs `name` and `prompt`".into(),
                        })
                    } else {
                        let job_id = council_jobs::start(name, prompt, cwd, tag);
                        ServerFrame::reply(api_id, ApiEvent::CouncilStarted { job_id })
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("await_council") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let job_id = request["job_id"].as_str().unwrap_or_default().to_string();
                    let frame = match council_jobs::wait(&job_id).await {
                        Some(run) => ServerFrame::reply(api_id, ApiEvent::CouncilRun { run }),
                        None => ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::UnknownRequest,
                            message: format!("no council run `{job_id}`"),
                        }),
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("council_status") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let job_id = request["job_id"].as_str().unwrap_or_default();
                    let frame = match council_jobs::status(job_id) {
                        Some(run) => ServerFrame::reply(api_id, ApiEvent::CouncilRun { run }),
                        None => ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::UnknownRequest,
                            message: format!("no council run `{job_id}`"),
                        }),
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("cancel_council") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let job_id = request["job_id"].as_str().unwrap_or_default();
                    // Cancelling an already-finished (or unknown) job is a
                    // no-op, not an error: the client raced completion.
                    let _ = council_jobs::cancel(job_id);
                    let frame = ServerFrame::reply(api_id, ApiEvent::Ok);
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("start_claude_login") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let redirect = request["redirect_uri"].as_str().unwrap_or_default();
                    let job_id = login_jobs::start("claude", redirect, state.identity.as_deref());
                    let frame = ServerFrame::reply(api_id, ApiEvent::LoginStarted { job_id });
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("start_codex_login") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let redirect = request["redirect_uri"].as_str().unwrap_or_default();
                    let job_id = login_jobs::start("codex", redirect, state.identity.as_deref());
                    let frame = ServerFrame::reply(api_id, ApiEvent::LoginStarted { job_id });
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("complete_login") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let job_id = request["job_id"].as_str().unwrap_or_default().to_string();
                    let code = request["code"].as_str().unwrap_or_default().to_string();
                    login_jobs::complete(&job_id, &code).await;
                    let frame = match login_jobs::status(&job_id) {
                        Some(run) => ServerFrame::reply(api_id, ApiEvent::LoginRun { run }),
                        None => ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::UnknownRequest,
                            message: format!("no login job `{job_id}`"),
                        }),
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("login_status") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let job_id = request["job_id"].as_str().unwrap_or_default();
                    let frame = match login_jobs::status(job_id) {
                        Some(run) => ServerFrame::reply(api_id, ApiEvent::LoginRun { run }),
                        None => ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::UnknownRequest,
                            message: format!("no login job `{job_id}`"),
                        }),
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("await_login") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let job_id = request["job_id"].as_str().unwrap_or_default().to_string();
                    let frame = match login_jobs::wait(&job_id).await {
                        Some(run) => ServerFrame::reply(api_id, ApiEvent::LoginRun { run }),
                        None => ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::UnknownRequest,
                            message: format!("no login job `{job_id}`"),
                        }),
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("cancel_login") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let _ = login_jobs::cancel(request["job_id"].as_str().unwrap_or_default());
                    let frame = ServerFrame::reply(api_id, ApiEvent::Ok);
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                // Remove a stored OAuth account. This is bridge-local by design,
                // symmetric to the login jobs that WROTE the account: the daemon
                // reads the auth files per request, so deleting from them here is
                // enough — no notify, no daemon round-trip. `clear_api_key` only
                // ever touched API-KEY providers, so Sign-out on a Claude/Codex
                // OAuth account was a silent no-op until this verb existed.
                // Empty label clears every account for the provider.
                if request["req"].as_str() == Some("sign_out_account") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    // Destructive on the shared store — a team member's bearer
                    // token must never wipe the server's pooled accounts. The
                    // local app owns its unix socket (is_owner = true).
                    if !is_owner {
                        let frame = ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message: "removing accounts is owner-only".into(),
                        });
                        write_json_line(&mut write_half, &frame).await?;
                        continue;
                    }
                    let provider = request["provider"].as_str().unwrap_or_default();
                    let label = request["label"].as_str().unwrap_or_default();
                    let result: anyhow::Result<()> = (|| {
                        let is_openai =
                            matches!(provider, "openai" | "codex" | "openai-oauth" | "chatgpt");
                        match (is_openai, label.is_empty()) {
                            (true, true) => jcode_base::auth::codex::clear_accounts().map(|_| ()),
                            (true, false) => jcode_base::auth::codex::remove_account(label),
                            (false, true) => jcode_base::auth::claude::clear_accounts().map(|_| ()),
                            (false, false) => jcode_base::auth::claude::remove_account(label),
                        }
                    })();
                    jcode_base::auth::AuthStatus::invalidate_cache();
                    let frame = match result {
                        Ok(()) => ServerFrame::reply(api_id, ApiEvent::Ok),
                        Err(error) => ServerFrame::reply(
                            api_id,
                            ApiEvent::Error {
                                code: ErrorCode::InvalidRequest,
                                message: format!("{error:#}"),
                            },
                        ),
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                // Team management is owner-only: a member's bearer token must
                // never invite (and receive a minted credential), enumerate,
                // or revoke. The three verbs share one gate.
                if matches!(
                    request["req"].as_str(),
                    Some("invite_member") | Some("list_team_members") | Some("revoke_member")
                        | Some("create_team") | Some("team_create_status")
                        | Some("connect_github")
                        | Some("account_signin_start") | Some("account_signin_code")
                        | Some("account_signout")
                ) && !is_owner
                {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let frame = ServerFrame::reply(api_id, ApiEvent::Error {
                        code: ErrorCode::InvalidRequest,
                        message: "team management is owner-only".into(),
                    });
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("list_dirs") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
                    let home_canon = std::path::Path::new(&home)
                        .canonicalize()
                        .unwrap_or_else(|_| std::path::PathBuf::from(&home));
                    let requested = request["path"].as_str().unwrap_or(&home);
                    let canon = std::path::Path::new(requested)
                        .canonicalize()
                        .unwrap_or_else(|_| home_canon.clone());
                    // Stay under HOME: the picker browses the runtime's work
                    // area, not the whole machine.
                    let base = if canon.starts_with(&home_canon) { canon } else { home_canon };
                    let mut entries: Vec<serde_json::Value> = Vec::new();
                    if let Ok(read) = std::fs::read_dir(&base) {
                        for entry in read.flatten() {
                            let file_name = entry.file_name();
                            let entry_name = file_name.to_string_lossy();
                            if entry_name.starts_with('.') {
                                continue;
                            }
                            let p = entry.path();
                            if p.is_dir() {
                                entries.push(serde_json::json!({
                                    "name": entry_name,
                                    "is_repo": p.join(".git").exists(),
                                }));
                            }
                            if entries.len() >= 400 { break; }
                        }
                    }
                    entries.sort_by(|a, b| {
                        b["is_repo"].as_bool().cmp(&a["is_repo"].as_bool()).then(
                            a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")))
                    });
                    let frame = ServerFrame::reply(api_id, ApiEvent::Dirs {
                        path: base.display().to_string(),
                        entries: serde_json::Value::Array(entries),
                    });
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("connect_github") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let status = github_auth_jobs::start().await;
                    let frame = ServerFrame::reply(api_id, ApiEvent::GithubStatus { status });
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("github_status") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let status = match request["job_id"].as_str() {
                        Some(job_id) if !job_id.is_empty() => {
                            github_auth_jobs::status(job_id).unwrap_or_else(|| {
                                serde_json::json!({
                                    "done": true,
                                    "error": format!("no GitHub sign-in job `{job_id}`"),
                                })
                            })
                        }
                        _ => github_auth_jobs::account_status(),
                    };
                    let frame = ServerFrame::reply(api_id, ApiEvent::GithubStatus { status });
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("me_account") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let account = blaude_account::me()
                        .and_then(|a| serde_json::to_value(a).ok());
                    let frame = ServerFrame::reply(api_id, ApiEvent::BlaudeAccount { account });
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("account_signin_start") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let email = request["email"].as_str().unwrap_or_default();
                    let frame = match blaude_account::start(email).await {
                        Ok(pending_id) => {
                            ServerFrame::reply(api_id, ApiEvent::SigninPending { pending_id })
                        }
                        Err(message) => ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message,
                        }),
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("account_signin_code") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let pending = request["pending_id"].as_str().unwrap_or_default();
                    let code = request["code"].as_str().unwrap_or_default();
                    let frame = match blaude_account::finish(pending, code).await {
                        Ok(info) => ServerFrame::reply(api_id, ApiEvent::BlaudeAccount {
                            account: serde_json::to_value(info).ok(),
                        }),
                        Err(message) => ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message,
                        }),
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("account_signout") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let frame = match blaude_account::sign_out() {
                        Ok(()) => ServerFrame::reply(api_id, ApiEvent::BlaudeAccount { account: None }),
                        Err(message) => ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message,
                        }),
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("create_team") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let name = request["name"].as_str().unwrap_or_default().trim();
                    let frame = if name.is_empty() {
                        ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message: "create_team needs a name".into(),
                        })
                    } else {
                        let region = request["region"].as_str();
                        let status = team_create_jobs::start(name, region);
                        ServerFrame::reply(api_id, ApiEvent::TeamCreateStatus { status })
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("team_create_status") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let job_id = request["job_id"].as_str().unwrap_or_default();
                    let frame = match team_create_jobs::status(job_id) {
                        Some(status) => {
                            ServerFrame::reply(api_id, ApiEvent::TeamCreateStatus { status })
                        }
                        None => ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::UnknownRequest,
                            message: format!("no create-team job `{job_id}`"),
                        }),
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("invite_member") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let email = request["email"].as_str().unwrap_or_default().to_string();
                    let host = request["host"].as_str().unwrap_or("127.0.0.1").to_string();
                    let send_email = request["send_email"].as_bool().unwrap_or(false);
                    let frame = if email.is_empty() {
                        ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message: "invite_member needs `email`".into(),
                        })
                    } else {
                        match team_access::invite(&email, &host, send_email).await {
                            Ok(invite) => ServerFrame::reply(api_id, ApiEvent::MemberInvited { invite }),
                            Err(error) => ServerFrame::reply(api_id, ApiEvent::Error {
                                code: ErrorCode::Internal,
                                message: error.to_string(),
                            }),
                        }
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("list_team_members") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let frame = ServerFrame::reply(api_id, ApiEvent::TeamMembers {
                        emails: team_access::member_emails(),
                        pending: team_access::pending_invites(),
                    });
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("revoke_member") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let email = request["email"].as_str().unwrap_or_default();
                    let frame = match team_access::revoke(email) {
                        Ok(_) => ServerFrame::reply(api_id, ApiEvent::Ok),
                        Err(error) => ServerFrame::reply(api_id, ApiEvent::Error {
                            code: ErrorCode::Internal,
                            message: error.to_string(),
                        }),
                    };
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("list_council_runs") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let tag = request["tag"].as_str();
                    let frame = ServerFrame::reply(api_id, ApiEvent::CouncilRuns {
                        runs: council_jobs::list(tag),
                    });
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                }
                if request["req"].as_str() == Some("install_skill") {
                    let api_id = request["id"].as_u64().unwrap_or(0);
                    let url = request["url"].as_str().unwrap_or_default();
                    match translate::BridgeState::validate_skill_install(url) {
                        Err((code, message)) => {
                            let frame = ServerFrame::reply(api_id, ApiEvent::Error { code, message });
                            write_json_line(&mut write_half, &frame).await?;
                        }
                        Ok((clone_url, name, dest)) => {
                            let clone = tokio::time::timeout(
                                std::time::Duration::from_secs(120),
                                tokio::process::Command::new("git")
                                    .args(["clone", "--depth", "1", &clone_url])
                                    .arg(&dest)
                                    .output(),
                            )
                            .await;
                            let frame = match clone {
                                Ok(Ok(output)) if output.status.success() => {
                                    // No daemon yet? The skill is on disk; the
                                    // daemon reads the registry when it boots.
                                    if let Some(legacy) = legacy_write.as_mut() {
                                        let reload_id = state.next_legacy_request_id();
                                        let reload = serde_json::json!({
                                            "type": "reload_skills", "id": reload_id,
                                        });
                                        write_json_line(legacy, &reload).await?;
                                    }
                                    ServerFrame::reply(api_id, ApiEvent::Ok)
                                }
                                Ok(Ok(output)) => ServerFrame::reply(api_id, ApiEvent::Error {
                                    code: ErrorCode::Internal,
                                    message: format!(
                                        "git clone failed for {clone_url}: {}",
                                        String::from_utf8_lossy(&output.stderr).trim(),
                                    ),
                                }),
                                Ok(Err(error)) => ServerFrame::reply(api_id, ApiEvent::Error {
                                    code: ErrorCode::Internal,
                                    message: format!("couldn't run git: {error}"),
                                }),
                                Err(_) => {
                                    // A half-cloned tree would block retries.
                                    let _ = tokio::fs::remove_dir_all(&dest).await;
                                    ServerFrame::reply(api_id, ApiEvent::Error {
                                        code: ErrorCode::Internal,
                                        message: format!("git clone of {name} timed out after 120s"),
                                    })
                                }
                            };
                            write_json_line(&mut write_half, &frame).await?;
                        }
                    }
                    continue;
                }
                // Translation may inspect persisted session/archive files. Tell
                // Tokio before entering that synchronous region so it can keep
                // the accept loop and fresh-client handshakes scheduled.
                let outbound = tokio::task::block_in_place(|| {
                    state.api_request_to_legacy(&request)
                });
                for out in outbound {
                    match out {
                        translate::Outbound::Legacy(value) => {
                            // Only frames bound for the daemon need it up.
                            // File-backed verbs (councils, accounts listings)
                            // reply directly and must keep working while the
                            // daemon waits for its first AI account. If it was
                            // down when this connection opened, try ONE
                            // re-dial — credentials may have just landed via
                            // the login job and systemd brought it up — so
                            // the same connection upgrades in place.
                            if legacy_write.is_none() {
                                if let Some((rx, write)) = dial_legacy(&legacy_socket).await {
                                    legacy_rx = Some(rx);
                                    legacy_write = Some(write);
                                    eprintln!("harness API bridge: daemon is up — connection upgraded");
                                }
                            }
                            match legacy_write.as_mut() {
                                Some(legacy) => write_json_line(legacy, &value).await?,
                                None => {
                                    let api_id = request["id"].as_u64().unwrap_or(0);
                                    let frame = ServerFrame::reply(api_id, ApiEvent::Error {
                                        code: ErrorCode::Internal,
                                        message: "the agent daemon is not running (usually: no AI \
                                                  account yet). Add an account — sign-in works \
                                                  right now — and retry.".into(),
                                    });
                                    write_json_line(&mut write_half, &frame).await?;
                                }
                            }
                        }
                        translate::Outbound::Reply(frame) => {
                            write_json_line(&mut write_half, &frame).await?;
                        }
                    }
                }
            }
            // Cancel-safe: recv() never loses a partially-read frame the way
            // an inline read_line would when a sibling arm fires.
            received = async {
                legacy_rx
                    .as_mut()
                    .expect("branch guarded on legacy_rx.is_some()")
                    .recv()
                    .await
            }, if legacy_rx.is_some() => {
                let Some(legacy_line) = received else {
                    // The daemon closed its socket mid-connection (crash,
                    // restart, credential-less boot loop). This used to close
                    // the CLIENT connection too — every app saw "transport
                    // failed: stream closed", INCLUDING the login sheet whose
                    // bridge-local verbs never needed the daemon at all. That
                    // deadlocked recovery: a crash-looping daemon killed the
                    // very sign-in that would fix it. Degrade to bridge-only
                    // (exactly like a failed dial at connection setup), keep
                    // serving, and let the next daemon-bound request re-dial
                    // via the upgrade-in-place path above.
                    legacy_rx = None;
                    legacy_write = None;
                    respawn_daemon_throttled();
                    let frame = ServerFrame::event(ApiEvent::Error {
                        code: ErrorCode::Internal,
                        message: "daemon connection closed; reconnecting — sign-in and other \
                                  bridge verbs still work"
                            .into(),
                    });
                    write_json_line(&mut write_half, &frame).await?;
                    continue;
                };
                if legacy_line.trim().is_empty() { continue; }
                let event: Value = match serde_json::from_str(legacy_line.trim()) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                let frames = tokio::task::block_in_place(|| {
                    state.legacy_event_to_api(&event)
                });
                for frame in frames {
                    write_json_line(&mut write_half, &frame).await?;
                }
            }
        }
    }
}

async fn write_json_line<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: ?Sized + serde::Serialize,
{
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
#[path = "framing_tests.rs"]
mod framing_tests;

#[cfg(test)]
#[path = "login_jobs_tests.rs"]
mod login_jobs_tests;

#[cfg(all(test, unix))]
mod single_instance_tests {
    /// Two bridges must never both own the API socket.
    ///
    /// They used to: `run_bridge` unlinked whatever socket file was there and
    /// bound its own, so every on-demand spawn silently evicted the live
    /// bridge and its clients reported "harness API stream closed".
    #[test]
    fn a_second_bridge_cannot_take_the_socket() {
        let dir = std::env::temp_dir().join(format!("jcode-bridge-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("jcode-api.sock");

        let first = super::single_instance_lock(&socket).unwrap();
        assert!(first.is_some(), "the first bridge must take the lock");
        assert!(
            super::single_instance_lock(&socket).unwrap().is_none(),
            "a second bridge must be refused while the first is alive"
        );

        // Once the owner is gone the lock is available again, so a crashed
        // bridge never wedges its replacement out.
        drop(first);
        assert!(super::single_instance_lock(&socket).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(all(test, unix))]
mod socket_permission_tests {
    /// The API socket must never be more permissive than the daemon socket it
    /// fronts.
    ///
    /// This regressed once: `UnixListener::bind` applies the process umask, so
    /// the socket landed at 0755 while the daemon socket it bridges to is
    /// 0600. Every guarantee behind the daemon socket was then reachable by
    /// any local user, including reading transcripts and spending the owner's
    /// provider tokens.
    #[tokio::test]
    async fn the_api_socket_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("jcode-api-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let api_socket = dir.join("api.sock");
        let legacy_socket = dir.join("daemon.sock");

        let bridge_socket = api_socket.clone();
        let handle = tokio::spawn(async move {
            let _ = super::run_bridge(bridge_socket, legacy_socket).await;
        });

        // Wait for the bind, which happens before the accept loop.
        let mut mode = None;
        for _ in 0..100 {
            if let Ok(meta) = std::fs::metadata(&api_socket) {
                mode = Some(meta.permissions().mode() & 0o777);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            mode,
            Some(0o600),
            "API socket must be owner-only (0600); a wider mode exposes every \
             session behind the bridge to other local users"
        );
    }
}
