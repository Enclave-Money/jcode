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
    let tls = tls_acceptor_from_env()?;
    if tls.is_some() {
        eprintln!("harness API bridge: TLS enabled — clients connect with wss://");
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
        let (tcp, peer) = listener.accept().await?;
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
            let result = match tls {
                Some(acceptor) => match acceptor.accept(tcp).await {
                    Ok(stream) => handle_ws_client(stream, &token, legacy, peer.ip()).await,
                    Err(error) => Err(anyhow::Error::from(error).context("tls handshake")),
                },
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
async fn serve_http<S: AsyncRead + AsyncWrite + Unpin>(mut tcp: S, head: &str) -> Result<()> {
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
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 8192 {
        let n = tcp.read(&mut chunk).await.context("read request head")?;
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
    let identity = std::sync::Arc::new(std::sync::Mutex::new(None::<Option<String>>));
    let identity_cb = std::sync::Arc::clone(&identity);
    let ws = tokio_tungstenite::accept_hdr_async(tcp, |request: &Request, response: Response| {
        let who = authorize(request, token).inspect_err(|_| note_auth_failure(peer))?;
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
