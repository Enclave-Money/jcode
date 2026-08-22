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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("bind websocket 127.0.0.1:{port}"))?;
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
fn authorize(request: &Request, token: &str) -> Result<(), ErrorResponse> {
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
    match presented {
        Some(presented) if constant_time_eq(presented.as_bytes(), token.as_bytes()) => Ok(()),
        Some(_) => Err(reject(401, "bad token")),
        None => Err(reject(401, "missing Authorization: Bearer <token>")),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn handle_ws_client(tcp: TcpStream, token: &str, legacy_socket: PathBuf) -> Result<()> {
    let ws = tokio_tungstenite::accept_hdr_async(tcp, |request: &Request, response: Response| {
        authorize(request, token).map(|()| response)
    })
    .await
    .context("websocket handshake")?;
    let (mut ws_write, mut ws_read) = ws.split();

    // The relay: WS text frames <-> an in-memory duplex carrying NDJSON, with
    // the standard bridge loop on the other end. One frame = one line.
    let (client_side, bridge_side) = tokio::io::duplex(1024 * 1024);
    let (bridge_read, bridge_write) = tokio::io::split(bridge_side);
    let core = tokio::spawn(handle_api_io(BufReader::new(bridge_read), bridge_write, legacy_socket));

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
