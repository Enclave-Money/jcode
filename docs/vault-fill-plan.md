# Vault Fill — coding plan

Status: **BUILT and proven end-to-end (1 Sep 2026), except the device-flow relay.**
Grounded in blaude-agent and blaude-native. Every claim below was checked against
the source, not remembered.

## What shipped

Layers 1–6 + the Mac app + tests are done and verified on the live team server:
the harness-owned room browser (Playwright over stdio), `fill_login` with the
approval flow over the in-memory stdin channel, index sync, the atomic
fill-and-submit, log redaction, and the full Mac app (settings, op integration,
approval card, allow list, audit). Proven with a fixture login in a real room:
silent allow-listed fill, asked+approved fill, deny, and shared-room refusal all
worked, the credential reached the fixture (positive control), and it leaked
nowhere — journald, room `.jcode`, audit, app.log, QA state, or the agent
transcript. Tests: helper 16, room_browser 4, translate 66 incl. a
non-vacuous credential canary; full workspace suite green.

**One architecture change from the plan below:** the credential rides the
EXISTING stdin request/response channel (in-memory, tool ↔ daemon ↔ bridge ↔
app), not the approvals queue file §2/§0 sketched — strictly better, because it
never touches disk. The bridge translates the fill-flavored stdin_request into a
clean `FillApproval` event and the app's `fill_credentials` back into a
stdin_response.

**Not built — §3 device-flow relay** (gh/vercel/gcloud). gh already ships
(`github_auth_jobs.rs`). vercel/gcloud need per-tool parsing of their real
interactive output (gcloud on a GCE VM prompts before the URL; vercel isn't
installed), which won't be shipped from guessed formats. The one remaining item.

---

## Original plan (below)

This is one deliverable; the build sequence at the end is dependency order for
correctness, not a staged release.

---

## 0. Where the plan conflicts with the code, and decisions made differently

Read this section first; everything after it incorporates these decisions.

**C1. The device-flow relay for GitHub already exists.**
`crates/jcode-harness-api-server/src/github_auth_jobs.rs`: `connect_github`
runs `gh auth login` device flow on the connected runtime, replies with the
one-time code + verification URL, `github_status` polls the job; the app
renders the code in AI accounts → Connections. The plan's §"Dev tools without
a browser" is therefore an extension, not a build: generalise this job runner
to `vercel login` and `gcloud auth login --no-launch-browser`, route the
code through the new approval notification instead of (only) the Connections
pane, and lift the owner-only restriction so it is per-member and lands in the
requesting member's room, not the door.

**C2. `ToolContext` has no teammate identity — the process is the identity.**
`crates/jcode-tool-core/src/lib.rs:103`: a tool sees session/message/call ids
and a working dir. Nothing says which teammate's turn this is. In a **mine**
room that is fine — the daemon runs as that member's Unix user and the mapping
`unix user → email` exists (`member-users.json`). In the **shared room**
(`blaude-shared`) there is no identity at fill time, and worse: the shared
room's screen is watched by everyone, so a fill there would type one member's
username/TOTP onto a display all members can view and leave a logged-in
session all members can drive. **Decision: `fill_login` and the device-flow
relay are refused in the shared room with result `unsupported_room`.** The
plan text ("a fill request … lands in their room") implicitly assumed this;
the code makes it a hard rule worth stating.

**C3. Automatic login-wall detection cannot see the agent's ad-hoc browsers.**
Today the agent browses via Bash + Playwright as the room user; the harness has
no view into those processes and must not grow one. Automatic detection
therefore lives **only inside the new harness-owned browser tool** (a
navigation that lands on a login-shaped page sets a `login_wall` hint in that
action's result). For the ad-hoc path the agent calls `fill_login` explicitly,
guided by the index. Consequence for the 80% goal: it is only reachable if the
agent's authed browsing moves onto the harness browser tool — the tool
description must steer the model there ("use browser for anything behind a
login"), and that steering is part of this feature, not an afterthought.

**C4. Fill must happen in the same browser the agent continues in.**
A fill typed into a harness-owned browser is useless if the agent's task
continues in a different Playwright context with a different cookie jar. The
harness browser tool is therefore not just "something that can type" — it is
the browser the agent uses for the whole authed task. `fill_login` is a
privileged action **on the browser session the tool already owns**. The
ad-hoc Bash path stays but cannot be filled; the tool result for `fill_login`
without an open harness browser session says exactly that.

**C5. The approval machinery already exists — reuse its shape, not its file.**
Permission prompts are file-mediated (`$JCODE_HOME/safety/queue.json` →
`safety/history.json`, `permissions.rs` watches and the ws loop pushes
`permissionRequest` / `permissionResolved` to clients; the app already renders
and answers them). Fill approvals follow the identical pattern but in their own
queue (`~/.jcode/approvals/queue.json` + `history.json`) because the payload is
richer (origin, username options, room, kind), the answer is richer (chosen
item, "always"), and entangling it with tool-permission semantics would make
both worse. Same watch mechanism, same select-loop arm shape, new event pair.

**C6. Redaction has three leak paths in the CURRENT code, not zero.**
(a) `HarnessService.hlog` logs full reply frames ("signin start reply: …");
(b) the QA dump writes app state to disk every second — any credential that
touches an `@Published` field lands in `state.json`; (c) the harness bridge
can log frames. The credential-carrying frame must be a dedicated type that is
redacted at the single choke point where frames are logged on each side, and
credential values must live only in function-local variables on the Mac —
never in `AppStore`. The CI test (§8) exists to catch all three.

**C7. localhost is shared between rooms; the browser helper must not listen.**
Verified: rooms are Unix users on one VM, no network namespaces. The
Playwright helper is a **child process of the room daemon speaking JSON-lines
over stdio** — no TCP, no Unix socket even; stdio cannot be connected to by
another user at all. Playwright's own CDP stays on its internal pipe
(`--remote-debugging-pipe` is Playwright's default launch transport), never a
port.

**C8. Node module resolution for the helper needs provisioning work.**
`/usr/local/bin/playwright` (global CLI) and `/opt/ms-playwright` browsers are
installed, but a helper script cannot `require('playwright')` from an
arbitrary path. Provisioning gains `/opt/blaude-browser/` — the helper script
plus a pinned `node_modules` with playwright, owned root, world-readable —
installed by `create_team` and `provision-member.sh` the same way ffmpeg was
added (embedded via the existing `install -m` / package-list mechanisms).
`PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright` is set by the daemon when
spawning the helper.

**C9. The Mac app is not sandboxed and deliberately avoids Keychain.**
No entitlements are configured in the project; spawning `/usr/local/bin/op`
(and `op` found via `PATH` / the standard install locations) is allowed.
`TeamTokenStore.swift` documents why Keychain is out. Everything this feature
stores on the Mac — allow list, audit log, settings — is non-secret and goes
in files under `~/Library/Application Support/Blaude/` (or UserDefaults for
plain toggles). The only secret flow is `op` → memory → websocket → gone.

**D1 (decision). "Approve always" is permanent until revoked, no expiry.**
The revocation surface ships in the same release and the audit view shows
every silent fill. A 30-day re-ask punishes the exact users who set it up
correctly; prompt-injection risk on an allow-listed origin is accepted in the
plan already. Revisit if audit review shows surprise fills.

**D2 (decision). The index is a file in the room home, not a tool call.**
`~/.jcode/vault-index.json`, written by the room daemon (which runs as the
room user, so no root and no sync-timer involvement — simpler than the
`blaude-sync-room-auth` shape, which exists because the *door* must write into
rooms; here the room writes its own file). It holds no secrets, the agent can
read and grep it, and it doubles as the tool's own lookup table.

**D3 (decision). Vault exposure defaults to NONE until the user selects.**
The settings sheet lists vaults with checkboxes, none pre-checked. A user who
wants everything ticks all; a user with a personal + work vault never
accidentally exposes the personal one. This is the only open question in the
plan where the safe default costs one click.

**D4 (decision). The harness browser session is ephemeral by construction.**
Fresh Playwright persistent-context in `~/.jcode/browser-session/<id>/`
(room-owned, 0700), deleted when the session closes or the daemon starts.
Nothing outlives the room process, satisfying "no harness-owned logged-in
browser that outlives the room" without inventing cookie hygiene rules.

---

## 1. Server-side browser tool for rooms (`browser` on Linux rooms)

**Repo: blaude-agent.**

### Process model

```
room daemon (blaude-daemon@<user>, runs as <user>)
  └─ browser-helper (node /opt/blaude-browser/helper.js)     [child, stdio JSON-lines]
       └─ Playwright chromium, headed on the room DISPLAY     [internal pipe]
```

- New crate module `jcode-app-core/src/tool/room_browser.rs` implementing the
  existing `BrowserProvider` trait (`browser.rs:105`) as a second provider,
  `room_playwright`. Provider selection: on Linux with a room display
  (`/run/blaude/$USER.Xauth` exists, same check `screen.rs::is_attached` uses)
  the room provider is chosen; otherwise the Firefox bridge provider as today.
  The `browser` tool name, schema, and action enum stay identical — the agent
  learns nothing new.
- The helper is spawned lazily on the first action, with `DISPLAY` and
  `XAUTHORITY` from the same derivations `screen.rs` uses, and
  `PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright`. One helper per daemon,
  restarted on crash (supervised the same way the video encoder now is:
  helper EOF is an event, not a silent stop).
- Lifetime: idle timeout (default 15 min without an action) closes the
  browser and helper; explicit `close` action too. Profile per D4.

### Action surface

Mirror the Firefox bridge's action enum exactly where it applies —
`status, list_tabs, new_tab, select_tab, get_active_tab, list_frames, open,
snapshot, get_content, interactables, click, type, fill_form, select, wait,
screenshot, eval, scroll, upload, press` — each mapped to the Playwright
equivalent in the helper. Plus:

- `fill_login` (§2) — the one action whose implementation the agent cannot
  observe or parameterise beyond `origin` and an optional `item_id`.
- Every navigation-ish result carries `login_wall: bool` (C3): the helper
  applies the detection heuristics (§2) after `open`, `click`, `wait`.

### Wire discipline

stdio JSON-lines only (C7). The helper never opens a listening socket; a test
asserts the helper process has no listening sockets after startup
(`ss -ltnp` in an integration check on the room image). Screenshots return
base64 through the same pipe.

---

## 2. `fill_login(origin)` end to end

### Detection (automatic + explicit)

In the helper, after each navigating action:
- URL heuristics: path matches `/(login|signin|sign-in|auth|sso|session)/i`,
  or a redirect chain ended on a different origin that matches.
- DOM heuristics: a visible `input[type=password]`, or a lone
  `input[autocomplete=username|email]` with a submit control (username-first
  flows), inside the top frame or any same-site iframe.

The result's `login_wall: true` plus `index_match: {username, has_totp}` (from
the index file) is what nudges the agent to call `fill_login`. Detection never
auto-fires the fill — the fill is always an explicit tool action, so the agent
remains the actor the audit line names.

### Event flow (all on the existing websocket, new event pair)

```
agent tool call fill_login(origin)
  → daemon writes approvals/queue.json entry {id, kind:"fill", origin,
      candidates:[{item_id, username, has_totp}], room, session_id, ts}
  → ws loop (new arm, same shape as permission_poll) pushes
      ApiEvent::FillApprovalRequest to every connected client of this room
  → Mac app: allow-listed origin? auto-answer. Otherwise notification
      (UNUserNotificationCenter, existing urgent path) + in-app sheet with
      Approve / Approve always / Deny and the item picker when >1 candidate
  → on approve: app runs `op item get <item_id> --fields username,password
      --reveal` (+ `--otp` when has_totp), builds ApiRequest::FillCredentials
      {approval_id, username, password, totp?}, sends on the member's
      authenticated connection, drops locals
  → daemon matches approval_id, records the DECISION (never the credential)
      in approvals/history.json, hands the credential to the helper as one
      `fill_and_submit` command, zeroes its copy
  → helper fills + submits (below), reports {outcome}
  → tool result to the agent: submitted | needs_human | unsupported_auth |
      denied | no_item | unsupported_room | timeout — nothing else
  → ApiEvent::FillApprovalResolved dismisses the prompt on other clients
```

Timeout: 120s from queue entry to credential or denial; expiry resolves as
`timeout` and dismisses everywhere. Deny answers instantly with `denied`; the
tool result text tells the agent not to retry (mirrors the existing
bridge-error copy pattern in `browser.rs`).

### Approval state machine (daemon side)

`queued → prompted → {approved, denied, expired}`; `approved → filling →
{submitted, failed(reason)}`. Persisted in `approvals/history.json` so a
daemon restart mid-fill resolves to `failed(restart)` rather than a stuck
prompt; the ws loop's queue watch re-pushes anything still `queued` on
reconnect, same as permission prompts survive today.

### Atomic fill-and-submit (helper)

One helper command performs: focus username field → fill → (multi-step: submit
/ press Enter, wait for password field, up to 2 steps) → fill password →
if TOTP field appears within 8s and a TOTP was provided, fill it → submit →
wait for navigation or network-idle → classify:

- Landed off the login page, no password field visible → `submitted`.
- CAPTCHA iframe (recaptcha/hcaptcha/turnstile selectors), SMS/push prompt
  text, or still on login with an error region → `needs_human` + screenshot
  (base64, capped 200KB) in the tool result.
- Passkey-only page (no password input, WebAuthn-triggering button matched)
  → `unsupported_auth`.

Fields are located per frame, including same-site iframes and open shadow
roots (Playwright pierces shadow DOM natively; iframes enumerated and matched
by the same heuristics). Credentials live in the helper only for the duration
of this one command; the helper's logging is `console.error` for diagnostics
and a lint in the helper build greps its source for any interpolation of the
credential variables into logs.

### Redaction (C6)

- New `ApiRequest::FillCredentials` decoded nowhere generic: the bridge's and
  daemon's frame logging redact by request name at the single logging choke
  point (one function each side; test in §8).
- Mac: credentials only in locals inside the approval handler; `hlog` never
  logs the fill request; nothing credential-shaped enters `AppStore`, so the
  QA dump cannot leak it.
- Tool results never echo values; `needs_human` screenshots are taken AFTER
  the password field is cleared or the page navigated (never mid-type — the
  helper explicitly blurs/clears before capturing on failure paths).

---

## 3. Device-flow relay (`gh`, `vercel`, `gcloud`)

**Repo: blaude-agent (generalise `github_auth_jobs.rs`), blaude-native (route
through the approval surface).**

- Rename/extend the job runner to `device_flow_jobs.rs` with a per-tool
  parser: `gh` (exists), `vercel login` (prints a URL with an embedded code),
  `gcloud auth login --no-launch-browser` (prints URL, waits for pasted
  verification code on stdin — the relay carries the teammate's pasted code
  back down the same approval channel, single-use so agent visibility is
  acceptable per the plan).
- New agent tool `connect_tool(tool)` available in mine rooms only (C2),
  running the CLI as the room user so the token lands in the room home
  (`~/.config/gh/hosts.yml`, `~/.local/share/com.vercel.cli`, gcloud ADC).
- The verification URL + code ride the same approvals queue with
  `kind:"device_flow"`; the Mac notification's Approve opens the URL in the
  teammate's local default browser and shows the code with a copy button
  (gh/vercel), or a paste-back field (gcloud). "Approve always" does not
  apply to device flows — each one is explicit by nature.
- Owner-only gating on the existing `connect_github` verb stays for the
  door/Connections pane; the new per-member path is the room tool.

---

## 4. Mac app work

**Repo: blaude-native.** All storage per C9.

- **Settings** (new section in the existing settings surface): master toggle
  "Fill logins from 1Password"; `op` detection (`op --version`, standard
  paths + `PATH`), install/sign-in guidance when missing (link to 1Password's
  CLI setting for Touch ID integration); vault list (`op vault list`) with
  checkboxes, default none (D3).
- **Index sync**: `op item list --vault <v> --format json` per selected vault
  → for each item, origin(s) from item URLs, username field, `has_totp` from
  the item's field types (no `--reveal`; list output holds no secrets) →
  `ApiRequest::VaultIndexSync {entries}` on the member's connection. Re-sync
  on: toggle-on, vault change, app focus at most hourly, and a manual button.
- **Approval UX**: `FillApprovalRequest` → allow-list check → silent
  auto-approve or notification + sheet (item picker when several candidates;
  choice remembered per origin in the allow-list file). Deny and timeout paths
  update the sheet and audit. Notification actions map to Approve / Deny;
  "Approve always" lives on the sheet only, so it is always a deliberate
  in-app act.
- **Allow list**: `~/Library/Application Support/Blaude/fill-allowlist.json`
  — `{origin: {item_id, since, last_used}}`; revocation UI lists origins with
  a remove button.
- **Audit view**: reads `fill-audit.jsonl` (§6), newest first: icon, origin
  or tool, room, outcome, timestamp, silent/prompted. No secrets by
  construction.
- **Wire**: `FillApprovalRequest`/`FillApprovalResolved` in `ApiEvent.swift`,
  `FillCredentials`, `VaultIndexSync`, `FillApprovalAnswer` in
  `ApiRequest.swift`, mirrored in `sdk/typescript/src/protocol.ts`.

---

## 5. Index sync path and file shape

Mac → `VaultIndexSync` on the member's ws connection → their room daemon
(runs as the room user) writes `~/.jcode/vault-index.json` 0600 (D2):

```json
{ "synced_at": "…", "entries": [
    { "origin": "vercel.com", "item_id": "op://…", "username": "sumer@…",
      "has_totp": true } ] }
```

The daemon also serves it to the helper for `index_match` hints. The shared
room never receives an index. Toggle-off sends an empty sync, which deletes
the file.

---

## 6. Audit record

Both sides, both append-only, no secrets:

- Room: `~/.jcode/fill-audit.jsonl` — `{ts, kind: fill|device_flow, origin?
  , tool?, item_id?, session_id, outcome, silent: bool}` (who = the room by
  identity; the daemon adds its unix user + mapped email).
- Mac: `~/Library/Application Support/Blaude/fill-audit.jsonl` — same shape
  plus `room`. The audit view reads this one; the room copy is for server-side
  forensics.

---

## 7. Tests

- **Helper unit tests** (node, in `/opt/blaude-browser` sources kept in-repo
  under `deploy/browser-helper/`): detection heuristics against saved HTML
  fixtures (username-first, iframe login, shadow-DOM login, CAPTCHA page,
  passkey-only page); fill state machine against a local fixture server.
- **Rust**: approval state machine transitions incl. restart-mid-fill;
  `unsupported_room` in shared; queue/history file semantics (mirroring the
  existing `rooms.rs`/`permissions` test style); redaction of
  `FillCredentials` at the frame-logging choke point (log a fill frame, assert
  the output contains the request name and no field values); device-flow
  parsers against captured CLI transcripts.
- **Swift**: allow-list round-trip incl. revocation; approval auto-answer
  path; index build from canned `op` JSON; nothing credential-shaped in
  `AppStore` (type-level: the credential struct is not `Codable`-exported and
  a QA-dump test asserts the dump of a session that performed a fill contains
  no canary).
- **CI canary test (the grep-style check the plan requires)**: an end-to-end
  fixture run with credential value `CANARY-9f3a-…` injected via a stubbed
  `op` shim; after a scripted fill against the fixture login server, grep the
  canary across: harness logs, bridge logs, tool results, session transcripts
  on disk, approvals history, both audit files, the Mac QA `state.json`, and
  the app log. Any hit fails. **Positive control**: the same grep against the
  fixture server's own access log MUST find the canary (the fill really
  happened), so a vacuous pass is impossible.

---

## 8. Dependency-ordered build sequence

1. **Provisioning**: `/opt/blaude-browser` (helper + pinned playwright),
   package additions; `create_team` + `provision-member.sh` (C8).
2. **Helper**: stdio protocol, action surface, detection, fill state machine,
   fixtures + unit tests. Testable standalone against the fixture server.
3. **Room browser provider** in `jcode-app-core` behind the existing `browser`
   tool; supervision + idle lifetime; provider selection.
4. **Approvals queue + ws events** in `jcode-harness-api-server` (new arm in
   the select loop, `FillApprovalRequest/Resolved`, `FillCredentials` +
   redaction choke points, timeout/restart semantics).
5. **Index sync**: `VaultIndexSync` request → room file; helper reads it.
6. **`fill_login` action** joining 2+4+5; audit lines both sides.
7. **Device-flow relay**: generalise `github_auth_jobs.rs`, add the
   `connect_tool` room tool, `kind:"device_flow"` approvals.
8. **Mac app**: wire types; settings + `op` integration + vault selection;
   approval sheet/notification + picker; allow list + revocation; audit view;
   index sync scheduling.
9. **CI canary test** last, because it needs every layer; the per-layer
   redaction tests land with their layers (4, 8).
10. **Steering**: browser tool description update (C3) and agent-facing
    result copy, then an end-to-end drill on a throwaway team server:
    scripted fill, kill-mid-fill, deny, timeout, shared-room refusal,
    device flow for all three CLIs.

---

## Open questions resolved in this plan

- "Approve always" permanence → D1 (permanent until revoked).
- Index as file vs tool call → D2 (file).
- Default vault exposure → D3 (none until selected).
