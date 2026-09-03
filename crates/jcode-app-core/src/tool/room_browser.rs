//! The harness-owned browser for a blaude room.
//!
//! On a team server each room is a Unix user with its own X display. The agent
//! browses through a Playwright Chromium the DAEMON owns — a child process
//! spawned here, talking JSON-lines over stdio (never a socket: on a
//! shared-localhost box stdio is the one channel another room's user cannot
//! reach). The browser is headed on the room's display, so a person watching
//! the streamed screen sees exactly what the agent's browser does.
//!
//! The one privileged action is `fill_login`: it logs the teammate into a site
//! WITHOUT the agent ever seeing the password. The credential travels the
//! existing in-memory stdin-request channel (tool → daemon → bridge → Mac app
//! → back), is held only for the one fill, and is never written to disk or
//! returned to the model — the tool result is an outcome word, nothing else.

use super::{StdinInputRequest, ToolContext, ToolOutput};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};

/// The origin (scheme://host[:port]) of a URL, or None if it will not parse.
fn origin_of(url: &str) -> Option<String> {
    let scheme_end = url.find("://")?;
    let after = &url[scheme_end + 3..];
    let host = after.split('/').next().unwrap_or(after);
    if host.is_empty() {
        return None;
    }
    Some(format!("{}://{}", &url[..scheme_end], host))
}

/// A field off the raw tool input, by name.
fn field<'a>(input: &'a Value, key: &str) -> Option<&'a Value> {
    input.get(key).filter(|v| !v.is_null())
}
fn field_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    field(input, key).and_then(|v| v.as_str())
}

/// Where provisioning installs the helper and its browser.
fn helper_script() -> String {
    std::env::var("BLAUDE_BROWSER_HELPER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/opt/blaude-browser/helper.js".into())
}

/// The browser build that goes with the helper's OWN Playwright — derived from
/// the helper's directory, NOT the ambient `PLAYWRIGHT_BROWSERS_PATH`. The
/// daemon inherits a `PLAYWRIGHT_BROWSERS_PATH` pointing at a different,
/// older Playwright install (the global CLI's), and using it makes the helper
/// look for a browser build that install does not have — a launch failure that
/// looks like a missing browser. This path is a sibling of the helper script.
fn browsers_path() -> String {
    std::path::Path::new(&helper_script())
        .parent()
        .map(|dir| dir.join("ms-playwright").to_string_lossy().into_owned())
        .unwrap_or_else(|| "/opt/blaude-browser/ms-playwright".into())
}

/// The current Unix user's name, or None off a Unix host.
fn current_user() -> Option<String> {
    std::env::var("USER").ok().filter(|u| !u.is_empty()).or_else(|| {
        std::process::Command::new("id").arg("-un").output().ok().and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
    })
}

fn current_uid() -> Option<u32> {
    std::process::Command::new("id").arg("-u").output().ok().and_then(|o| {
        String::from_utf8_lossy(&o.stdout).trim().parse().ok()
    })
}

/// The room's X authority file, matching provision-member.sh and screen.rs.
fn xauth_path(user: &str) -> std::path::PathBuf {
    std::path::PathBuf::from("/run/blaude").join(format!("{user}.Xauth"))
}

/// The display the room renders on: `:90 + uid%100`, exactly as provisioning
/// derives it, so this and the capture agree without a third file to sync.
fn display_for(uid: u32) -> String {
    format!(":{}", 90 + (uid % 100))
}

/// Whether this process is a room daemon with a desktop — the only place the
/// room browser applies. A local runtime (someone's Mac) has neither, and
/// keeps the Firefox bridge.
pub fn is_room_runtime() -> bool {
    current_user().map(|u| xauth_path(&u).exists()).unwrap_or(false)
}

/// One running helper, owned for the life of the daemon. The browser is a
/// single page driven serially, so one helper and a mutex are enough.
struct Helper {
    stdin: ChildStdin,
    reader_lines: tokio::sync::mpsc::UnboundedReceiver<String>,
    _child: Child,
    next_id: u64,
}

/// Where the helper's stderr goes: one appended log per room, in the room's
/// own JCODE_HOME so a teammate can never read another room's browser trace.
fn log_file() -> Option<std::fs::File> {
    let dir = std::env::var("JCODE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".jcode"));
    let _ = std::fs::create_dir_all(&dir);
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("room-browser.log"))
        .ok()
}

impl Helper {
    async fn spawn() -> Result<Self> {
        let user = current_user().context("no Unix user for the room browser")?;
        let uid = current_uid().context("could not read the room uid")?;
        let xauth = xauth_path(&user);
        anyhow::ensure!(
            xauth.exists(),
            "no room display for {user}; the desktop starts with the room"
        );
        let profile = std::env::var("JCODE_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".jcode"))
            .join("browser-session");
        let _ = std::fs::create_dir_all(&profile);

        let mut child = tokio::process::Command::new("node")
            .arg(helper_script())
            .env("DISPLAY", display_for(uid))
            .env("XAUTHORITY", &xauth)
            .env("PLAYWRIGHT_BROWSERS_PATH", browsers_path())
            .env("BLAUDE_BROWSER_PROFILE", &profile)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The helper's own diagnostics — which page it acted on, why a
            // fill gave up — used to go to /dev/null, which made a sign-in
            // that silently did nothing impossible to explain from the
            // server. It never logs a credential (`cmd fill_and_submit
            // (redacted)`), so keeping it costs nothing.
            .stderr(log_file().map(Stdio::from).unwrap_or_else(Stdio::null))
            .kill_on_drop(true)
            .spawn()
            .context("spawning the room browser helper (is /opt/blaude-browser installed?)")?;

        let stdin = child.stdin.take().context("helper stdin")?;
        let stdout = child.stdout.take().context("helper stdout")?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        Ok(Helper { stdin, reader_lines: rx, _child: child, next_id: 1 })
    }

    /// Send one command and wait for its reply. Events (`{"event":...}`) are
    /// skipped; the reply is the line whose id matches.
    async fn call(&mut self, cmd: &str, args: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({ "id": id, "cmd": cmd, "args": args }).to_string();
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;

        // The helper answers one command at a time (serialized by the caller's
        // mutex), so the next id-matching line is ours.
        loop {
            let raw = tokio::time::timeout(std::time::Duration::from_secs(90), self.reader_lines.recv())
                .await
                .context("room browser timed out")?
                .context("room browser closed")?;
            let msg: Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if msg.get("event").is_some() {
                continue;
            }
            if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if msg.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                    return Ok(msg.get("result").cloned().unwrap_or(json!({})));
                }
                let err = msg.get("error").and_then(|v| v.as_str()).unwrap_or("browser error");
                anyhow::bail!("{err}");
            }
        }
    }
}

/// The daemon-wide helper handle.
fn helper_cell() -> &'static Mutex<Option<Helper>> {
    static CELL: std::sync::OnceLock<Mutex<Option<Helper>>> = std::sync::OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Run one helper command, spawning the helper if needed and respawning it
/// once if it had died.
async fn with_helper(cmd: &str, args: Value) -> Result<Value> {
    let cell = helper_cell();
    let mut guard = cell.lock().await;
    if guard.is_none() {
        *guard = Some(Helper::spawn().await?);
    }
    match guard.as_mut().unwrap().call(cmd, args.clone()).await {
        Ok(v) => Ok(v),
        Err(e) => {
            // A dead helper: drop it, respawn once, retry. A browser that
            // crashed (or whose pipe broke when the process went away) must not
            // wedge every future action behind a stale handle — that surfaces
            // as "Broken pipe" forever until the daemon restarts.
            let msg = e.to_string();
            let transport_dead = msg.contains("closed")
                || msg.contains("timed out")
                || msg.contains("Broken pipe")
                || msg.contains("os error 32")
                || msg.contains("not connected");
            if transport_dead {
                *guard = Some(Helper::spawn().await?);
                guard.as_mut().unwrap().call(cmd, args).await
            } else {
                Err(e)
            }
        }
    }
}

/// The index of logins the teammate exposed, written by the daemon on sync.
fn index_path() -> std::path::PathBuf {
    std::env::var("JCODE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".jcode"))
        .join("vault-index.json")
}

#[derive(serde::Deserialize, Clone)]
struct IndexEntry {
    origin: String,
    #[serde(default)]
    item_id: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    has_totp: bool,
}

fn index_candidates(origin: &str) -> Vec<IndexEntry> {
    index_candidates_in(&index_path(), origin)
}

/// The testable core: match against a specific index file.
fn index_candidates_in(path: &std::path::Path, origin: &str) -> Vec<IndexEntry> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let Some(entries) = doc.get("entries").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|e| serde_json::from_value::<IndexEntry>(e.clone()).ok())
        .filter(|e| e.origin.eq_ignore_ascii_case(origin))
        .collect()
}

/// Append one audit line to the room's log. Never a secret — outcome only.
fn audit(origin: &str, outcome: &str, item_id: &str) {
    let path = index_path().with_file_name("fill-audit.jsonl");
    let line = json!({
        "ts": chrono_now(),
        "kind": "fill",
        "origin": origin,
        "item_id": item_id,
        "outcome": outcome,
        "user": current_user().unwrap_or_default(),
    })
    .to_string();
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

fn chrono_now() -> String {
    // Seconds since epoch is enough for an audit line and needs no dep.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

/// The standard browser actions, forwarded to the helper. `input` is the raw
/// tool-call JSON, so this stays independent of the browser tool's private
/// input struct.
pub async fn execute(action: &str, input: &Value, ctx: &ToolContext) -> Result<ToolOutput> {
    match action {
        // The room browser is always installed and needs no per-machine setup,
        // unlike the Mac's Firefox bridge. Answer both as "ready" so the agent
        // does not treat a no-op as a failure.
        "setup" => Ok(ToolOutput::new("The room browser is ready — no setup needed.")
            .with_metadata(json!({ "ready": true }))),
        "status" => {
            let r = with_helper("status", json!({})).await.unwrap_or(json!({"ready": true}));
            Ok(ToolOutput::new(format!("Room browser ready: {}", r)).with_metadata(r))
        }
        "open" => {
            let url = field_str(input, "url").context("open needs a url")?;
            let r = with_helper("open", json!({ "url": url, "new_tab": field(input, "new_tab").cloned().unwrap_or(json!(false)) })).await?;
            Ok(login_aware_output("open", r))
        }
        "click" => forward("click", json!({ "selector": field(input, "selector"), "x": field(input, "x"), "y": field(input, "y") })).await,
        "type" => forward(
            "type",
            json!({ "selector": field(input, "selector"), "text": field(input, "text"), "clear": field(input, "clear"), "submit": field(input, "submit") }),
        )
        .await,
        "press" => forward("press", json!({ "key": field(input, "key") })).await,
        "wait" => forward("wait", json!({ "selector": field(input, "selector"), "timeout_ms": field(input, "timeout_ms") })).await,
        "screenshot" => {
            let r = with_helper("screenshot", json!({})).await?;
            Ok(ToolOutput::new("screenshot captured").with_metadata(r))
        }
        "get_content" | "snapshot" => {
            let r = with_helper("get_content", json!({})).await?;
            let text = r.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
            Ok(ToolOutput::new(text).with_metadata(r))
        }
        "eval" => forward("eval", json!({ "script": field(input, "script") })).await,
        "scroll" => forward("scroll", json!({ "position": field(input, "position") })).await,
        "list_tabs" => forward("list_tabs", json!({})).await,
        "new_tab" => forward("new_tab", json!({ "url": field(input, "url") })).await,
        "fill_login" => fill_login(input, ctx).await,
        other => anyhow::bail!("the room browser does not support action '{other}' yet"),
    }
}

async fn forward(cmd: &str, args: Value) -> Result<ToolOutput> {
    let r = with_helper(cmd, args).await?;
    Ok(login_aware_output(cmd, r))
}

/// Attach the login-wall hint to a navigating result so the model learns it can
/// call fill_login here.
fn login_aware_output(action: &str, result: Value) -> ToolOutput {
    let mut body = format!("{action} ok");
    if result.get("login_wall").and_then(|v| v.as_bool()) == Some(true) {
        let origin = result.get("origin").and_then(|v| v.as_str()).unwrap_or("");
        let has = !index_candidates(origin).is_empty();
        body = if has {
            format!("{action} ok — this is a login page for {origin}; call action='fill_login' to sign in without handling the password")
        } else {
            format!("{action} ok — login page ({origin}); no saved login for it")
        };
    }
    ToolOutput::new(body).with_metadata(result)
}

/// The privileged fill. The agent supplies only an origin; everything secret
/// happens off-model.
async fn fill_login(input: &Value, ctx: &ToolContext) -> Result<ToolOutput> {
    // The shared room is watched and driven by every member — a fill there
    // would type one member's credential onto everyone's screen. Refuse it.
    if current_user().as_deref() == Some("blaude-shared") {
        return Ok(ToolOutput::new(
            "fill_login is refused in the shared room: its screen is visible to every teammate. \
             Open this in your own room.",
        )
        .with_metadata(json!({ "outcome": "unsupported_room" })));
    }

    // Resolve the origin — and make sure the browser is actually THERE.
    //
    // Taking the origin from the argument alone asked the teammate to approve
    // a sign-in to a page that was not open. Two things went wrong with that.
    // The fill then raced the page load and silently did nothing while
    // reporting "Signed in." And, worse, the credential was typed into
    // whatever the browser did happen to be showing: approve site A, and if
    // the page was site B, site B got the password. Opening it first makes the
    // question and the page the same thing.
    let (origin, login_url) = match field_str(input, "url") {
        Some(u) => {
            with_helper("open", json!({ "url": u })).await?;
            live_page().await.unwrap_or_else(|| {
                (origin_of(u).unwrap_or_else(|| u.to_string()), u.to_string())
            })
        }
        None => live_page().await.unwrap_or_default(),
    };
    if origin.is_empty() {
        return Ok(fill_result("no_item", "No origin to sign in to. Open the login page first."));
    }

    let candidates = index_candidates(&origin);
    if candidates.is_empty() {
        audit(&origin, "no_item", "");
        return Ok(fill_result(
            "no_item",
            &format!("No saved login for {origin}. Ask the teammate how to proceed."),
        ));
    }

    // Ask the teammate (or their allow list) for approval + the credential,
    // over the in-memory stdin channel. The envelope is JSON so the Mac app
    // renders a proper approval, not a password box.
    let stdin_tx = ctx
        .stdin_request_tx
        .clone()
        .context("no client channel to approve the sign-in (is a person connected?)")?;
    let envelope = json!({
        "blaude_fill": {
            "origin": origin,
            "candidates": candidates.iter().map(|c| json!({
                "item_id": c.item_id, "username": c.username, "has_totp": c.has_totp,
            })).collect::<Vec<_>>(),
        }
    })
    .to_string();
    let (resp_tx, resp_rx) = oneshot::channel();
    stdin_tx
        .send(StdinInputRequest {
            request_id: format!("fill-{}", ctx.tool_call_id),
            prompt: envelope,
            is_password: true,
            response_tx: resp_tx,
        })
        .map_err(|_| anyhow::anyhow!("could not reach the client for approval"))?;

    let reply = match tokio::time::timeout(std::time::Duration::from_secs(120), resp_rx).await {
        Ok(Ok(s)) => s,
        _ => {
            audit(&origin, "timeout", "");
            return Ok(fill_result("timeout", "No response to the sign-in request."));
        }
    };
    let answer: Value = serde_json::from_str(&reply).unwrap_or(json!({}));
    if answer.get("denied").and_then(|v| v.as_bool()) == Some(true) {
        audit(&origin, "denied", "");
        return Ok(fill_result("denied", "The teammate declined the sign-in."));
    }
    if answer.get("needs_human").and_then(|v| v.as_bool()) == Some(true) {
        audit(&origin, "needs_human", "");
        return Ok(fill_result("needs_human", "The teammate will finish the sign-in on the screen."));
    }
    let username = answer.get("username").and_then(|v| v.as_str());
    let password = answer.get("password").and_then(|v| v.as_str());
    let item_id = answer.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
    let (Some(username), Some(password)) = (username, password) else {
        audit(&origin, "no_item", item_id);
        return Ok(fill_result("no_item", "No credential was provided."));
    };

    // The page can move while a person decides. Check it is still the origin
    // they were shown BEFORE typing anything: an approval is for one site, and
    // a navigation in between must void it, not redirect the credential.
    match live_origin().await {
        Some(now) if now == origin => {}
        other => {
            audit(&origin, "origin_changed", item_id);
            return Ok(fill_result(
                "origin_changed",
                &format!(
                    "The page moved to {} after {origin} was approved, so nothing was typed. \
                     Open {origin} again and ask.",
                    other.as_deref().unwrap_or("a blank page")
                ),
            ));
        }
    }

    // Hand the credential to the helper for the single atomic fill. This is the
    // only place it exists in this process, and it never leaves this scope.
    //
    // The approved origin and the login URL captured at approval time ride
    // along: the helper RE-NAVIGATES to that URL inside this one serialized
    // command before typing (dropping any script the agent injected while the
    // person decided — audit V2), and the fill machine refuses to type on any
    // origin but this one, even mid-flow after a redirect (audit V3).
    let mut fill_args = json!({
        "username": username,
        "password": password,
        "origin": origin,
        "url": login_url,
    });
    if let Some(totp) = answer.get("totp").and_then(|v| v.as_str()) {
        fill_args["totp"] = json!(totp);
    }
    let result = with_helper("fill_and_submit", fill_args).await?;
    let outcome = result.get("outcome").and_then(|v| v.as_str()).unwrap_or("needs_human").to_string();
    audit(&origin, &outcome, item_id);

    let message = match outcome.as_str() {
        "submitted" => "Signed in.".to_string(),
        "needs_human" => {
            let reason = result.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            format!("Could not finish automatically ({reason}); the teammate can complete it on the screen.")
        }
        "unsupported_auth" => "This site needs a passkey, which cannot be filled remotely.".to_string(),
        "origin_changed" => format!(
            "The page left {origin} before the credential could be typed, so nothing was sent."
        ),
        other => format!("Sign-in ended: {other}"),
    };
    // The result carries the outcome and reason ONLY — never the credential,
    // never the screenshot bytes back to the model.
    Ok(ToolOutput::new(message).with_metadata(json!({
        "outcome": outcome,
        "reason": result.get("reason").cloned().unwrap_or(Value::Null),
    })))
}

/// The origin of the page the room browser is ACTUALLY on, asked of the
/// browser rather than inferred from what the model passed in.
async fn live_origin() -> Option<String> {
    live_page().await.map(|(origin, _)| origin)
}

/// The live page's (origin, full URL). The URL is what the fill re-opens
/// after approval, so the credential is typed into a fresh document of the
/// page the person was actually shown.
async fn live_page() -> Option<(String, String)> {
    let hint = with_helper("detect_login", json!({})).await.ok()?;
    let url = hint.get("url").and_then(|v| v.as_str())?;
    Some((origin_of(url)?, url.to_string()))
}

fn fill_result(outcome: &str, message: &str) -> ToolOutput {
    ToolOutput::new(message).with_metadata(json!({ "outcome": outcome }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A credential is only ever released for the page the browser is ON.
    ///
    /// `fill_login` used to take the origin from the model's argument without
    /// opening it, so the teammate approved "sign in to A" while the browser
    /// sat on B — and B got the password. It also meant the fill raced the
    /// page load and silently did nothing while reporting success. Both are
    /// the same missing step: open it, then check it again after the human
    /// answers, because the page can move while they decide.
    #[test]
    fn the_approved_origin_must_be_the_live_one() {
        let src = include_str!("room_browser.rs");
        let body = src
            .split("async fn fill_login")
            .nth(1)
            .expect("fill_login exists");
        let ask = body.find("stdin_tx").expect("the approval is requested");
        let fill = body.find("fill_and_submit").expect("the fill happens");
        let open = body.find("\"open\"").expect("the page is opened");
        assert!(open < ask, "the page must be OPEN before a person is asked about it");
        let recheck = body[ask..fill].find("live_origin");
        assert!(
            recheck.is_some(),
            "the live origin must be re-checked between the approval and the typing"
        );
        assert!(
            body[ask..fill].contains("origin_changed"),
            "a page that moved after approval must refuse, not redirect the credential"
        );
        // Audit V2/V3: the helper must receive the approved origin and login
        // URL so it can re-navigate before typing and refuse mid-fill hops.
        // The fill_args json! block sits between the approval and the call.
        assert!(
            body[ask..fill].contains(r#""origin": origin"#)
                && body[ask..fill].contains(r#""url": login_url"#),
            "fill_and_submit must carry the approved origin and login url"
        );
    }

    #[test]
    fn origin_is_scheme_host_port() {
        assert_eq!(origin_of("https://vercel.com/login?x=1").as_deref(), Some("https://vercel.com"));
        assert_eq!(origin_of("http://localhost:3000/app").as_deref(), Some("http://localhost:3000"));
        assert_eq!(origin_of("https://a.b.com").as_deref(), Some("https://a.b.com"));
        assert_eq!(origin_of("not a url"), None);
    }

    #[test]
    fn browsers_path_follows_the_helper_not_the_ambient_env() {
        // The daemon inherits PLAYWRIGHT_BROWSERS_PATH pointing at a different
        // Playwright install; the helper's browser must come from beside the
        // helper script instead, or chromium fails to launch.
        assert_eq!(browsers_path(), "/opt/blaude-browser/ms-playwright");
    }

    #[test]
    fn the_display_matches_provisioning_arithmetic() {
        // Must equal provision-member.sh and screen.rs, or the browser draws on
        // one display and the capture reads another.
        assert_eq!(display_for(1000), ":90");
        assert_eq!(display_for(1002), ":92");
        assert_eq!(display_for(1100), ":90");
    }

    #[test]
    fn index_matches_by_origin_case_insensitively() {
        let path = std::env::temp_dir().join(format!("blaude-idx-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"entries":[
                {"origin":"https://vercel.com","item_id":"op://v/1","username":"a@b.com","has_totp":true},
                {"origin":"https://github.com","item_id":"op://v/2","username":"c@d.com","has_totp":false}
            ]}"#,
        )
        .unwrap();
        let hits = index_candidates_in(&path, "https://VERCEL.com");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item_id, "op://v/1");
        assert!(hits[0].has_totp);
        assert!(index_candidates_in(&path, "https://unknown.com").is_empty());
        // A missing index yields no candidates, never an error.
        assert!(index_candidates_in(std::path::Path::new("/no/such/index.json"), "x").is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
