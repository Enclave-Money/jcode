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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
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
    // clients on a trusted network (tailnet/LAN) — still token-guarded per
    // member. Public internet exposure needs TLS in front; refuse is not
    // possible to detect here, so the operator owns that call.
    let bind = std::env::var("JCODE_API_WS_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let listener = TcpListener::bind((bind.as_str(), port))
        .await
        .with_context(|| format!("bind websocket {bind}:{port}"))?;
    let addr = listener.local_addr()?;
    let token = load_or_create_token()?;
    tokio::spawn(async move {
        if let Err(error) = run_ws_listener(listener, token, legacy_socket).await {
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

pub async fn run_ws_listener(
    listener: TcpListener,
    token: String,
    legacy_socket: PathBuf,
) -> Result<()> {
    loop {
        let (tcp, _peer) = listener.accept().await?;
        let token = token.clone();
        let legacy = legacy_socket.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_ws_client(tcp, &token, legacy).await {
                eprintln!("harness API bridge: websocket client ended: {error:#}");
            }
        });
    }
}

fn reject(status: u16, body: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(body.to_string()));
    *response.status_mut() =
        tokio_tungstenite::tungstenite::http::StatusCode::from_u16(status).unwrap_or_default();
    response
}

/// Bearer header preferred; `?token=` accepted for clients that cannot set
/// headers (browsers). Path must be `/api`.
fn authorize(request: &Request, token: &str) -> Result<Option<String>, ErrorResponse> {
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
        return Ok(std::env::var("USER").ok());
    }
    for (email, member_token) in team_tokens() {
        if constant_time_eq(presented.as_bytes(), member_token.as_bytes()) {
            return Ok(Some(email));
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

/// Serve the phone page (or health/404) to a non-upgrade HTTP request.
async fn serve_http(mut tcp: TcpStream, head: &str) -> Result<()> {
    // Consume the request bytes we peeked (single read is plenty for a GET).
    let mut sink = [0u8; 8192];
    let _ = tcp.read(&mut sink).await;
    let path = head.split_whitespace().nth(1).unwrap_or("/");
    let path = path.split('?').next().unwrap_or("/");
    let (status, body, content_type) = match path {
        "/" | "/index.html" => ("200 OK", PHONE_PAGE, "text/html; charset=utf-8"),
        "/health" => ("200 OK", "ok", "text/plain"),
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

async fn handle_ws_client(tcp: TcpStream, token: &str, legacy_socket: PathBuf) -> Result<()> {
    // Sniff without consuming: a websocket handshake proceeds untouched, a
    // plain browser GET gets the embedded phone client instead of a refusal.
    let mut head = [0u8; 1024];
    let peeked = tcp.peek(&mut head).await.context("peek request head")?;
    let head_text = String::from_utf8_lossy(&head[..peeked]).to_string();
    if head_text.starts_with("GET ")
        && !head_text.to_ascii_lowercase().contains("upgrade: websocket")
    {
        return serve_http(tcp, &head_text).await;
    }
    let identity = std::sync::Arc::new(std::sync::Mutex::new(None::<Option<String>>));
    let identity_cb = std::sync::Arc::clone(&identity);
    let ws = tokio_tungstenite::accept_hdr_async(tcp, |request: &Request, response: Response| {
        let who = authorize(request, token)?;
        *identity_cb.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(who);
        Ok(response)
    })
    .await
    .context("websocket handshake")?;
    let identity = identity
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .flatten();
    let (mut ws_write, mut ws_read) = ws.split();

    // The relay: WS text frames <-> an in-memory duplex carrying NDJSON, with
    // the standard bridge loop on the other end. One frame = one line.
    let (client_side, bridge_side) = tokio::io::duplex(1024 * 1024);
    let (bridge_read, bridge_write) = tokio::io::split(bridge_side);
    let core = tokio::spawn(handle_api_io(BufReader::new(bridge_read), bridge_write, legacy_socket, identity));

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
