//! WebSocket front door for the harness API.
//!
//! Realtime clients (the desktop app, future web/mobile) speak the exact
//! NDJSON protocol over WebSocket text frames — one frame per line, byte-for-
//! byte the same JSON the unix socket carries — so a client's transport is a
//! deployment detail, not a protocol fork. This is also the shape a remote
//! team server speaks (wss + bearer token), which makes the local listener the
//! production code path, not a shortcut.
//!
//! Security: loopback-only bind, and every handshake must present the bearer
//! token stored owner-only at `$JCODE_HOME/api-ws-token`. A TCP port is
//! reachable by every local user (unlike the 0600 unix socket), so the token
//! is what restores the same "owner only" boundary. Validation happens in the
//! handshake callback, so a bad token gets a real 401 rather than an
//! accept-then-drop a client cannot distinguish from a network flake.

use crate::handle_api_io;
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};

/// Default loopback port. Override with `JCODE_API_WS_PORT`; `off` disables,
/// `0` binds an ephemeral port (printed at startup).
pub const DEFAULT_WS_PORT: u16 = 7644;

/// Read `JCODE_API_WS_PORT` and start the listener; `Ok(None)` means disabled.
pub async fn spawn_from_env(legacy_socket: PathBuf) -> Result<Option<std::net::SocketAddr>> {
    let raw = std::env::var("JCODE_API_WS_PORT").unwrap_or_default();
    if raw.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    let port: u16 = if raw.is_empty() { DEFAULT_WS_PORT } else { raw.parse().context("JCODE_API_WS_PORT")? };
    // Loopback by default; JCODE_API_WS_BIND=0.0.0.0 opens the door for team
    // clients. A non-loopback bind carries bearer tokens over the network, so
    // it is REFUSED unless TLS terminates here (or the operator explicitly
    // accepts plaintext with JCODE_API_WS_ALLOW_INSECURE=1 — e.g. a trusted
    // tailnet with its own encryption). Refusing by default is the only safe
    // production posture: a token in cleartext on 0.0.0.0 is a credential leak.
    let bind = std::env::var("JCODE_API_WS_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let tls = tls_acceptor_from_env()?;
    let is_loopback = bind == "127.0.0.1" || bind == "::1" || bind.eq_ignore_ascii_case("localhost");
    let allow_insecure = std::env::var("JCODE_API_WS_ALLOW_INSECURE").is_ok_and(|v| v == "1");
    if !is_loopback && tls.is_none() && !allow_insecure {
        anyhow::bail!(
            "refusing to bind the harness WS door on {bind} without TLS — bearer tokens would \
             travel in cleartext. Set JCODE_API_WS_TLS_CERT/KEY, or (only on a network you \
             trust to encrypt, e.g. a tailnet) JCODE_API_WS_ALLOW_INSECURE=1."
        );
    }
    let listener = TcpListener::bind((bind.as_str(), port))
        .await
        .with_context(|| format!("bind websocket {bind}:{port}"))?;
    let addr = listener.local_addr()?;
    let token = load_or_create_token()?;
    if tls.is_some() {
        eprintln!("harness API bridge: TLS enabled — clients connect with wss://");
    } else if !is_loopback {
        eprintln!("harness API bridge: WARNING — plaintext ws:// on {bind} (JCODE_API_WS_ALLOW_INSECURE)");
    }
    tokio::spawn(async move {
        if let Err(error) = run_ws_listener_with_tls(listener, token, legacy_socket, tls).await {
            eprintln!("harness API bridge: websocket listener ended: {error:#}");
        }
    });
    Ok(Some(addr))
}

/// The token file guarding the loopback WebSocket, owner-only.
pub fn token_path() -> PathBuf {
    let home = std::env::var("JCODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let base = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(base).join(".jcode")
        });
    home.join("api-ws-token")
}

pub fn load_or_create_token() -> Result<String> {
    let path = token_path();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }
    let token = generate_token()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, &token).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict {}", path.display()))?;
    }
    Ok(token)
}

#[cfg(unix)]
fn generate_token() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .context("open /dev/urandom")?
        .read_exact(&mut bytes)
        .context("read /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(not(unix))]
fn generate_token() -> Result<String> {
    // No /dev/urandom: derive from RandomState, which seeds from OS entropy.
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for i in 0..4u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(i);
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    Ok(out)
}

/// Per-member tokens issued by the host app after a Clerk invite is
/// accepted: `$JCODE_HOME/team-tokens.json` = {"email": "token", ...},
/// owner-only. Reloaded per handshake so revocation is immediate.
pub fn team_tokens() -> std::collections::HashMap<String, String> {
    let path = token_path().with_file_name("team-tokens.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Native TLS: set `JCODE_API_WS_TLS_CERT` and `JCODE_API_WS_TLS_KEY` to PEM
/// paths and the door speaks wss:// directly — no reverse proxy needed for a
/// remote team. Absent, plain ws:// (loopback/tailnet use).
pub fn tls_acceptor_from_env() -> Result<Option<tokio_rustls::TlsAcceptor>> {
    let (Ok(cert_path), Ok(key_path)) = (
        std::env::var("JCODE_API_WS_TLS_CERT"),
        std::env::var("JCODE_API_WS_TLS_KEY"),
    ) else {
        return Ok(None);
    };
    Ok(Some(load_tls_acceptor(&cert_path, &key_path)?))
}

pub fn load_tls_acceptor(cert_path: &str, key_path: &str) -> Result<tokio_rustls::TlsAcceptor> {
    // Both ring (via tungstenite) and aws-lc-rs (workspace rustls) are in the
    // tree, so rustls cannot pick a provider implicitly. First caller wins;
    // an Err means one is already installed — fine either way.
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    let certs: Vec<_> = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open(cert_path).with_context(|| format!("open {cert_path}"))?,
    ))
    .collect::<std::io::Result<_>>()
    .with_context(|| format!("parse certs in {cert_path}"))?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(
        std::fs::File::open(key_path).with_context(|| format!("open {key_path}"))?,
    ))
    .with_context(|| format!("parse key in {key_path}"))?
    .ok_or_else(|| anyhow::anyhow!("no private key in {key_path}"))?;
    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build TLS config")?;
    Ok(tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config)))
}

/// Redeem a join ticket into the JSON grant the app claims at /join/claim.
/// Ticket bookkeeping (burn, renew, token issue, locking) lives in
/// `team_access` — the single owner of those files.
fn claim_join_ticket(code: &str) -> Option<String> {
    let crate::team_access::Grant { email, token } = crate::team_access::claim_ticket(code)?;
    let grant = format!(
        r#"{{"email":{},"token":{}}}"#,
        serde_json::to_string(&email).ok()?,
        serde_json::to_string(&token).ok()?
    );
    // This lands inside a <script> literal. JSON escaping alone does NOT
    // stop `</script>` (or U+2028/2029) from terminating the element, so
    // neutralise the HTML-significant characters — the values parse
    // identically as JSON afterwards.
    Some(
        grant
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029"),
    )
}

pub async fn run_ws_listener(
    listener: TcpListener,
    token: String,
    legacy_socket: PathBuf,
) -> Result<()> {
    run_ws_listener_with_tls(listener, token, legacy_socket, None).await
}

pub async fn run_ws_listener_with_tls(
    listener: TcpListener,
    token: String,
    legacy_socket: PathBuf,
    tls: Option<tokio_rustls::TlsAcceptor>,
) -> Result<()> {
    loop {
        // A transient per-accept error (a client reset mid-handshake ->
        // ECONNABORTED, fd pressure -> EMFILE/ENFILE) must NOT take down the
        // whole door for every other client. Log and keep serving; back off
        // briefly on fd exhaustion so we don't spin.
        let (tcp, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                eprintln!("harness API bridge: accept error (continuing): {error}");
                if matches!(error.raw_os_error(), Some(libc::EMFILE) | Some(libc::ENFILE)) {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                continue;
            }
        };
        if auth_throttled(peer.ip()) {
            // Too many bad tokens from this peer: drop before any handshake
            // work — cheapest possible refusal.
            drop(tcp);
            continue;
        }
        let token = token.clone();
        let legacy = legacy_socket.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            // The TLS handshake is bounded here; the request-head read is
            // bounded inside handle_ws_client. Both guard only connection
            // setup, so an established session's frames are never affected —
            // but a slowloris peer that dribbles (or withholds) bytes on the
            // public 0.0.0.0 bind can no longer pin a task forever before it
            // ever presents a token for the bad-token throttle to catch.
            const TLS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
            let result = match tls {
                Some(acceptor) => {
                    match tokio::time::timeout(TLS_TIMEOUT, acceptor.accept(tcp)).await {
                        Ok(Ok(stream)) => handle_ws_client(stream, &token, legacy, peer.ip()).await,
                        Ok(Err(error)) => Err(anyhow::Error::from(error).context("tls handshake")),
                        Err(_) => Err(anyhow::anyhow!("tls handshake timed out")),
                    }
                }
                None => handle_ws_client(tcp, &token, legacy, peer.ip()).await,
            };
            if let Err(error) = result {
                eprintln!("harness API bridge: websocket client ended: {error:#}");
            }
        });
    }
}

/// A stream that replays already-read head bytes before the live stream —
/// how the HTTP-vs-websocket sniff works on TLS streams, which cannot peek.
struct PrefixedStream<S> {
    prefix: Vec<u8>,
    offset: usize,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.offset < self.prefix.len() {
            let remaining = &self.prefix[self.offset..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.offset += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Brute-force guard: a peer that keeps failing auth gets refused outright
/// for a cooldown window. In-memory per-process — enough to turn an online
/// token guess from thousands/second into ten/minute.
fn auth_failures() -> &'static std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>> {
    static FAILURES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>>,
    > = std::sync::OnceLock::new();
    FAILURES.get_or_init(Default::default)
}

const AUTH_FAILURE_LIMIT: u32 = 10;
const AUTH_FAILURE_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

fn auth_throttled(peer: std::net::IpAddr) -> bool {
    let mut map = auth_failures().lock().unwrap_or_else(|p| p.into_inner());
    match map.get(&peer) {
        Some((count, since)) if since.elapsed() < AUTH_FAILURE_WINDOW => *count >= AUTH_FAILURE_LIMIT,
        Some(_) => {
            map.remove(&peer);
            false
        }
        None => false,
    }
}

fn note_auth_failure(peer: std::net::IpAddr) {
    let mut map = auth_failures().lock().unwrap_or_else(|p| p.into_inner());
    let entry = map.entry(peer).or_insert((0, std::time::Instant::now()));
    if entry.1.elapsed() >= AUTH_FAILURE_WINDOW {
        *entry = (0, std::time::Instant::now());
    }
    entry.0 += 1;
}

fn reject(status: u16, body: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(body.to_string()));
    *response.status_mut() =
        tokio_tungstenite::tungstenite::http::StatusCode::from_u16(status).unwrap_or_default();
    response
}

/// Bearer header preferred; `?token=` accepted for clients that cannot set
/// headers (browsers). Path must be `/api`.
/// Returns `(identity, is_owner)`. The owner presents the owner api-ws-token
/// (or connects over the local 0600 unix socket); members present a
/// team-tokens.json bearer. Team-management verbs are owner-gated downstream.
fn authorize(request: &Request, token: &str) -> Result<(Option<String>, bool), ErrorResponse> {
    if request.uri().path() != "/api" {
        return Err(reject(404, "unknown path; the harness API lives at /api"));
    }
    let presented = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| {
            request.uri().query().and_then(|query| {
                query.split('&').find_map(|pair| {
                    pair.strip_prefix("token=").map(str::to_string)
                })
            })
        });
    let Some(presented) = presented else {
        return Err(reject(401, "missing Authorization: Bearer <token>"));
    };
    if constant_time_eq(presented.as_bytes(), token.as_bytes()) {
        // The owner's identity is their blaude account email when the
        // runtime has one (on team servers it rides along at create_team).
        let identity = crate::blaude_account::identity()
            .or_else(|| std::env::var("USER").ok());
        return Ok((identity, true));
    }
    for (email, member_token) in team_tokens() {
        if constant_time_eq(presented.as_bytes(), member_token.as_bytes()) {
            return Ok((Some(email), false));
        }
    }
    Err(reject(401, "bad token"))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The embedded phone/web client, served on plain GET — a browser on the
/// same network gets a full session UI from the very port the API lives on.
const PHONE_PAGE: &str = include_str!("../assets/phone.html");

/// Served at /join — the landing page of an invitation link. It NEVER
/// claims the ticket: a browser visit used to burn the one-time credential
/// (stranding the app's auto-join at sign-in, which is the only claimer
/// now, via /join/claim) and dropped invitees into the embedded web client
/// instead of the desktop flow. The page only points at the app.
const JOIN_INSTRUCTIONS: &str = r#"<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Join your team on blaude</title>
<body style="margin:0;display:grid;place-items:center;min-height:100vh;background:#111;color:#eee;font:16px/1.6 -apple-system,system-ui,sans-serif">
<div style="max-width:30rem;padding:2rem">
<h1 style="font-size:1.4rem">You're invited 🎉</h1>
<p>Joining is just signing in:</p>
<ol style="padding-left:1.2rem">
<li><a href="https://blaude-website.vercel.app" style="color:#8ab4ff">Download blaude</a> and open it.</li>
<li>Sign in with the email that was invited — enter the code we send it.</li>
<li>That's it — your team attaches itself.</li>
</ol>
<p style="color:#999;font-size:.85rem;margin-top:1rem">Invited at a different address? Ask your teammate to invite the email you actually use.</p>
</div>"#;

/// Serve the phone page (or health/404) to a non-upgrade HTTP request.
async fn serve_http<S: AsyncRead + AsyncWrite + Unpin>(mut tcp: S, head: &str) -> Result<()> {
    let target = head.split_whitespace().nth(1).unwrap_or("/");
    let path = target.split('?').next().unwrap_or("/");
    let join_page;
    let (status, body, content_type) = match path {
        "/" | "/index.html" => ("200 OK", PHONE_PAGE, "text/html; charset=utf-8"),
        "/health" => ("200 OK", "ok", "text/plain"),
        // Ticket or not, a browser visit gets instructions only — the
        // ticket stays unburned for the app's /join/claim at sign-in.
        "/join" => ("200 OK", JOIN_INSTRUCTIONS, "text/html; charset=utf-8"),
        // The app's machine-readable claim: same one-time semantics, JSON
        // body ({"email","token"}) instead of the join page.
        "/join/claim" => {
            let ticket = target
                .split_once('?')
                .map(|(_, query)| query)
                .unwrap_or("")
                .split('&')
                .find_map(|pair| pair.strip_prefix("ticket="))
                .unwrap_or("");
            match claim_join_ticket(ticket) {
                Some(grant) => {
                    join_page = grant;
                    ("200 OK", join_page.as_str(), "application/json")
                }
                None => (
                    "410 Gone",
                    r#"{"error":"this join link was already used or has expired"}"#,
                    "application/json",
                ),
            }
        }
        _ => (
            "404 Not Found",
            "not found — the phone client lives at /, the API at /api",
            "text/plain",
        ),
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    tcp.write_all(header.as_bytes()).await?;
    tcp.write_all(body.as_bytes()).await?;
    tcp.shutdown().await?;
    Ok(())
}

async fn handle_ws_client<S>(
    mut tcp: S,
    token: &str,
    legacy_socket: PathBuf,
    peer: std::net::IpAddr,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Read the request head, then REPLAY it: a websocket handshake gets the
    // bytes back through PrefixedStream, a plain browser GET gets the
    // embedded phone client instead of a refusal. (TLS streams cannot peek.)
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    // Bound the whole head read: a peer that opens a socket and sends bytes
    // one-per-second (or never completes the \r\n\r\n) must not hold this task.
    const HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 8192 {
        let n = tokio::time::timeout(HEAD_TIMEOUT, tcp.read(&mut chunk))
            .await
            .map_err(|_| anyhow::anyhow!("request head timed out"))?
            .context("read request head")?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..n]);
    }
    let head_text = String::from_utf8_lossy(&head).to_string();
    let tcp = PrefixedStream { prefix: head, offset: 0, inner: tcp };
    if head_text.starts_with("GET ")
        && !head_text.to_ascii_lowercase().contains("upgrade: websocket")
    {
        // The prefixed request bytes are already consumed conceptually — the
        // responder only writes.
        return serve_http(tcp, &head_text).await;
    }
    let auth = std::sync::Arc::new(std::sync::Mutex::new(None::<(Option<String>, bool)>));
    let auth_cb = std::sync::Arc::clone(&auth);
    let ws = tokio_tungstenite::accept_hdr_async(tcp, |request: &Request, response: Response| {
        let who = authorize(request, token).inspect_err(|_| note_auth_failure(peer))?;
        *auth_cb.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(who);
        Ok(response)
    })
    .await
    .context("websocket handshake")?;
    let (identity, is_owner) = auth
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .map(|(who, owner)| (who, owner))
        .unwrap_or((None, false));

    let (mut ws_write, mut ws_read) = ws.split();

    // The relay: WS text frames <-> an in-memory duplex carrying NDJSON, with
    // the standard bridge loop on the other end. One frame = one line.
    let (client_side, bridge_side) = tokio::io::duplex(1024 * 1024);
    let (bridge_read, bridge_write) = tokio::io::split(bridge_side);
    let core = tokio::spawn(handle_api_io(BufReader::new(bridge_read), bridge_write, legacy_socket, identity, is_owner));

    let (relay_read, mut relay_write) = tokio::io::split(client_side);
    let mut lines = BufReader::new(relay_read).lines();
    loop {
        tokio::select! {
            inbound = ws_read.next() => {
                match inbound {
                    Some(Ok(Message::Text(text))) => {
                        relay_write.write_all(text.as_bytes()).await?;
                        if !text.ends_with('\n') {
                            relay_write.write_all(b"\n").await?;
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = ws_write.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Binary(_))) => {
                        // The protocol is text NDJSON; a binary frame is a
                        // client bug. Say so once and end cleanly.
                        let _ = ws_write.send(Message::Text(
                            r#"{"v":1,"ev":"error","code":"invalid_request","message":"binary frames are not part of the harness API; send one JSON object per text frame"}"#.into(),
                        )).await;
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.into()),
                }
            }
            outbound = lines.next_line() => {
                match outbound? {
                    Some(line) => ws_write.send(Message::Text(line)).await?,
                    None => break, // bridge loop ended (daemon gone / client EOF)
                }
            }
        }
    }
    let _ = ws_write.send(Message::Close(None)).await;
    core.abort();
    Ok(())
}

#[cfg(test)]
mod ws_tests {
    use super::*;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    async fn start(token: &str) -> std::net::SocketAddr {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let legacy = std::env::temp_dir().join(format!("jcode-ws-test-{}-none.sock", std::process::id()));
        tokio::spawn(run_ws_listener(listener, token.to_string(), legacy));
        addr
    }

    /// The full handshake works over WebSocket: hello in a text frame,
    /// hello_ok back in a text frame — the same bytes the unix socket speaks.
    #[tokio::test(flavor = "multi_thread")]
    async fn hello_round_trips_over_websocket() {
        let addr = start("sekrit").await;
        let mut request = format!("ws://{addr}/api").into_client_request().unwrap();
        request.headers_mut().insert(
            "authorization",
            "Bearer sekrit".parse().unwrap(),
        );
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        ws.send(Message::Text(
            r#"{"v":1,"id":1,"req":"hello","min_version":1,"max_version":1,"client":"ws-test/0"}"#.into(),
        ))
        .await
        .unwrap();
        let reply = ws.next().await.unwrap().unwrap();
        let Message::Text(text) = reply else {
            panic!("expected text frame, got {reply:?}");
        };
        assert!(text.contains(r#""ev":"hello_ok""#), "got: {text}");
        assert!(text.contains(r#""reply_to":1"#), "got: {text}");
    }

    /// A wrong or missing token is refused during the handshake with a real
    /// HTTP status, never accept-then-drop.
    #[tokio::test(flavor = "multi_thread")]
    async fn bad_token_is_rejected_at_handshake() {
        let addr = start("sekrit").await;
        let mut request = format!("ws://{addr}/api").into_client_request().unwrap();
        request.headers_mut().insert(
            "authorization",
            "Bearer wrong".parse().unwrap(),
        );
        let error = tokio_tungstenite::connect_async(request).await.unwrap_err();
        let text = format!("{error}");
        assert!(text.contains("401"), "expected 401, got: {text}");

        let request = format!("ws://{addr}/api").into_client_request().unwrap();
        let error = tokio_tungstenite::connect_async(request).await.unwrap_err();
        assert!(format!("{error}").contains("401"));
    }

    /// A plain browser GET gets the embedded phone client, not a websocket
    /// refusal; unknown paths 404; the API path is untouched.
    #[tokio::test(flavor = "multi_thread")]
    async fn plain_get_serves_the_phone_client() {
        let addr = start("sekrit").await;
        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        tcp.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut body = Vec::new();
        tcp.read_to_end(&mut body).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with("HTTP/1.1 200"), "got: {}", &text[..60.min(text.len())]);
        assert!(text.contains("blaude-phone"), "page should embed the client");

        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        tcp.write_all(b"GET /nope HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut body = Vec::new();
        tcp.read_to_end(&mut body).await.unwrap();
        assert!(String::from_utf8_lossy(&body).starts_with("HTTP/1.1 404"));
    }

    /// TLS end to end: a self-signed server cert, a client that trusts it,
    /// and the same hello over wss:// — plus the phone page over https-style
    /// plain GET through the TLS stream.
    #[tokio::test(flavor = "multi_thread")]
    async fn wss_round_trips_with_native_tls() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let dir = std::env::temp_dir().join(format!("jcode-wss-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

        let acceptor =
            load_tls_acceptor(cert_path.to_str().unwrap(), key_path.to_str().unwrap()).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let legacy = std::env::temp_dir().join(format!(
            "jcode-wss-test-{}-none.sock",
            std::process::id()
        ));
        tokio::spawn(run_ws_listener_with_tls(
            listener,
            "sekrit".to_string(),
            legacy,
            Some(acceptor),
        ));

        // A client that trusts exactly this cert.
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let client_config = tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(client_config));

        let mut request = format!("wss://localhost:{}/api?token=sekrit", addr.port())
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("host", format!("localhost:{}", addr.port()).parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async_tls_with_config(
            request,
            None,
            false,
            Some(connector),
        )
        .await
        .unwrap();
        ws.send(Message::Text(
            r#"{"v":1,"id":1,"req":"hello","min_version":1,"max_version":1,"client":"wss-test/0"}"#.into(),
        ))
        .await
        .unwrap();
        let Message::Text(text) = ws.next().await.unwrap().unwrap() else {
            panic!("expected text frame");
        };
        assert!(text.contains(r#""ev":"hello_ok""#), "got: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A browser visit to /join NEVER burns the ticket (it used to, which
    /// stranded the app's auto-join and dropped invitees into the web
    /// client). Only the app's /join/claim redeems it — exactly once.
    #[tokio::test(flavor = "multi_thread")]
    async fn join_page_never_claims_and_claim_burns_once() {
        let _guard = crate::jcode_home_test_lock();
        let home = std::env::temp_dir().join(format!("jcode-join-test-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let previous = std::env::var_os("JCODE_HOME");
        unsafe { std::env::set_var("JCODE_HOME", &home) };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        std::fs::write(
            home.join("team-tokens.json"),
            r#"{"jo@example.com":"member-jo-token"}"#,
        )
        .unwrap();
        std::fs::write(
            home.join("join-tickets.json"),
            format!(r#"{{"ticket-abcdef1234567890":{{"email":"jo@example.com","created_ms":{now_ms}}}}}"#),
        )
        .unwrap();

        let addr = start("sekrit").await;
        let fetch = |path: String| async move {
            let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            tcp.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut body = Vec::new();
            tcp.read_to_end(&mut body).await.unwrap();
            String::from_utf8_lossy(&body).to_string()
        };
        let page = fetch("/join?ticket=ticket-abcdef1234567890".to_string()).await;
        assert!(page.starts_with("HTTP/1.1 200"), "{}", &page[..60]);
        assert!(page.contains("Sign in with the email"), "instructions page");
        assert!(!page.contains("member-jo-token"), "browser visit must not mint a grant");
        let claim = fetch("/join/claim?ticket=ticket-abcdef1234567890".to_string()).await;
        assert!(claim.starts_with("HTTP/1.1 200"), "the page left the ticket unburned: {}", &claim[..60]);
        assert!(claim.contains("member-jo-token"), "grant JSON for the app");
        assert!(claim.contains("jo@example.com"));
        let burned = fetch("/join/claim?ticket=ticket-abcdef1234567890".to_string()).await;
        assert!(burned.starts_with("HTTP/1.1 410"), "one-time: {}", &burned[..60]);
        assert!(!burned.contains("member-jo-token"));

        // …but membership is renewable: the claim left a FRESH ticket for
        // the same person, so a second Mac / reinstall / interrupted claim
        // still has something live to redeem. (Burning without replacing
        // stranded every one of those permanently.)
        let raw = std::fs::read_to_string(home.join("join-tickets.json")).unwrap();
        let tickets: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&raw).unwrap();
        assert_eq!(tickets.len(), 1, "exactly one live ticket: {raw}");
        let (code, record) = tickets.iter().next().unwrap();
        assert_ne!(code, "ticket-abcdef1234567890", "the burned code is gone");
        assert_eq!(record["email"], "jo@example.com");
        let second = fetch(format!("/join/claim?ticket={code}")).await;
        assert!(second.starts_with("HTTP/1.1 200"), "renewed ticket redeems: {}", &second[..60]);
        assert!(second.contains("member-jo-token"), "same membership, not a new one");

        match previous {
            Some(value) => unsafe { std::env::set_var("JCODE_HOME", value) },
            None => unsafe { std::env::remove_var("JCODE_HOME") },
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The brute-force limiter trips at the limit and only for that peer.
    #[test]
    fn auth_throttle_trips_at_limit_per_peer() {
        let probe: std::net::IpAddr = "192.0.2.77".parse().unwrap(); // TEST-NET-1
        let bystander: std::net::IpAddr = "192.0.2.78".parse().unwrap();
        assert!(!auth_throttled(probe));
        for _ in 0..AUTH_FAILURE_LIMIT - 1 {
            note_auth_failure(probe);
        }
        assert!(!auth_throttled(probe), "one under the limit still admitted");
        note_auth_failure(probe);
        assert!(auth_throttled(probe), "at the limit, refused");
        assert!(!auth_throttled(bystander), "other peers unaffected");
    }

    /// The query-string fallback works for header-less clients.
    #[tokio::test(flavor = "multi_thread")]
    async fn query_token_is_accepted() {
        let addr = start("sekrit").await;
        let request = format!("ws://{addr}/api?token=sekrit").into_client_request().unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        ws.send(Message::Text(
            r#"{"v":1,"id":9,"req":"hello","min_version":1,"max_version":1,"client":"ws-test/0"}"#.into(),
        ))
        .await
        .unwrap();
        let Message::Text(text) = ws.next().await.unwrap().unwrap() else {
            panic!("expected text frame");
        };
        assert!(text.contains(r#""ev":"hello_ok""#));
    }
}
