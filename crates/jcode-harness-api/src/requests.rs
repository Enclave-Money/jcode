//! Client-to-server requests: the curated stable surface.

use serde::{Deserialize, Serialize};

/// Curated request surface. Internally-tagged on `"req"`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "req", rename_all = "snake_case")]
pub enum ApiRequest {
    /// Version negotiation. Must be the first frame on a connection.
    Hello {
        min_version: u32,
        max_version: u32,
        /// Client name and version, e.g. "jcode-desktop2/0.1.0".
        client: String,
    },

    /// List sessions visible to this client.
    ListSessions {
        /// Include sessions the user archived through this API.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        include_archived: bool,
    },

    /// List saved councils (cross-model panels) and their member model ids.
    ListCouncils,

    /// Create a council (2–3 unique model ids, unique name). Replies with
    /// the updated `councils` list.
    CreateCouncil { name: String, models: Vec<String> },

    /// Delete a council by name. Replies with the updated `councils` list.
    DeleteCouncil { name: String },

    /// Start a council run: fan `prompt` out to every member of the named
    /// council in `working_dir`. Replies immediately with `council_started`
    /// carrying a job id — the run itself is a bridge-global job that
    /// survives the requesting connection. `tag` is an opaque client
    /// binding (e.g. a chat id) echoed back in run records.
    RunCouncil {
        name: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
    },

    /// Block until the council job reaches a terminal state, then reply
    /// with its full `council_run` record. Safe to call from any
    /// connection, any number of times, including after a client restart.
    AwaitCouncil { job_id: String },

    /// One council job's current record (state + output when finished).
    CouncilStatus { job_id: String },

    /// Kill a running council job's child process and mark it cancelled.
    CancelCouncil { job_id: String },

    /// All known council runs (in-flight and persisted finished ones),
    /// optionally filtered to a client `tag`.
    ListCouncilRuns {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
    },

    /// Set the session's work mode: "auto" | "plan" | "ask" | "manual".
    /// Daemon-owned: the mode persists on the session, applies to every
    /// attached client, and its guardrail text is injected server-side
    /// into each turn — clients carry no reminder wording.
    SetWorkMode { session_id: String, mode: String },

    /// Start a Claude OAuth sign-in as a bridge job (loopback-relay flow).
    /// `redirect_uri` is the CLIENT's own loopback listener
    /// (`http://localhost:<port>/callback`); the bridge mints the authorize URL
    /// pointing there and keeps the PKCE verifier server-side. Replies with
    /// `login_started { job_id }`; the record gains `url` immediately, and the
    /// client relays the code via `complete_login`. Works for a remote server.
    StartClaudeLogin { redirect_uri: String },

    /// Start a Codex/OpenAI (ChatGPT) OAuth sign-in — same loopback-relay
    /// lifecycle as `start_claude_login`.
    StartCodexLogin { redirect_uri: String },

    /// Complete a waiting login with the code/callback the client's loopback
    /// listener caught. The bridge exchanges it for tokens server-side.
    CompleteLogin { job_id: String, code: String },

    /// One login job's record: `{ job_id, provider, state, url?, error? }`
    /// with state waiting_for_code | completing | done | failed | cancelled.
    LoginStatus { job_id: String },

    /// Block until the login job reaches a terminal state.
    AwaitLogin { job_id: String },

    /// Cancel a login job (kills the callback listener).
    CancelLogin { job_id: String },

    /// Invite a team member: the BRIDGE mints the bearer token, writes its
    /// own authorization files, and (optionally) sends the Clerk email —
    /// the app never touches token files or the Clerk secret. `host` is
    /// the address members dial. The reply carries the member token for
    /// the owner's invite blob (same trust boundary as the local socket).
    InviteMember {
        email: String,
        host: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        send_email: bool,
    },

    /// Member emails from the bridge's authorization store — never tokens.
    ListTeamMembers,

    /// Revoke a member's access; immediate (the WS door reloads per handshake).
    RevokeMember { email: String },

    /// Provision a team server in the owner's cloud as an async job — the
    /// reply and every poll carry a `TeamCreateStatus` record. Owner-only:
    /// it creates billed infrastructure.
    CreateTeam {
        name: String,
        /// Region shortcut ("india", "singapore", "europe", "us"); the
        /// server maps it to a concrete zone, defaulting when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
    },

    /// Poll a create-team provisioning job.
    TeamCreateStatus { job_id: String },

    /// List directories on THIS runtime's filesystem (scoped under $HOME) —
    /// the server-side folder picker behind team workspaces. `path` absent
    /// = $HOME.
    ListDirs {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },

    /// Connect this runtime's GitHub identity via gh's device flow — the
    /// reply is a `GithubStatus` record carrying the one-time code to enter
    /// at github.com/login/device. The credential lands on THIS runtime (the
    /// team VM when connected to a team). Owner-only on team servers.
    ConnectGithub,

    /// Poll a GitHub sign-in job (`job_id`), or report the runtime's
    /// current GitHub account when `job_id` is absent.
    GithubStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_id: Option<String>,
    },

    /// The blaude ACCOUNT (who the person is — Clerk-backed), distinct from
    /// AI provider accounts. Reply is a `BlaudeAccount` event; absent = not
    /// signed in. Identity lives on the runtime this connection reaches.
    MeAccount,

    /// Start blaude sign-in for `email`: Clerk emails a one-time code
    /// (signing UP unknown addresses). Reply: `SigninPending { pending_id }`.
    AccountSigninStart { email: String },

    /// Redeem the emailed code; on success the identity persists on this
    /// runtime and the reply is the signed-in `BlaudeAccount`.
    AccountSigninCode { pending_id: String, code: String },

    /// Forget the stored blaude identity. Reply: empty `BlaudeAccount`.
    AccountSignout,

    /// List the stored provider accounts (labels and emails only — token
    /// material never crosses the wire). `provider` currently: "claude".
    ListAccounts { provider: String },

    /// Make a stored account the active one for its provider. The daemon
    /// reads auth state per token fetch, so the switch applies to the next
    /// turn without a restart.
    SetActiveAccount { provider: String, label: String },

    /// Install a skill from a github.com/owner/repo URL into
    /// `~/.jcode/skills/<repo>` and reload the daemon's skill registry, so
    /// the skill is usable without a restart. Connection-scoped, no session
    /// required; clients should send it on a spare connection (the clone
    /// stalls the pipe it runs on).
    InstallSkill { url: String },

    /// Grant the session an extra working directory (the wire form of the
    /// TUI's `/add-dir`): recorded in session metadata and surfaced in the
    /// session context. Adding a directory it already has is an Ok no-op.
    AddDir { session_id: String, path: String },

    /// Reversibly hide a session from the default list. Its transcript remains
    /// on disk and can be restored at any time.
    ArchiveSession { session_id: String },

    /// Put an archived session back in the default list.
    RestoreSession { session_id: String },

    /// Configure automatic archival of inactive sessions. `None` disables it.
    SetRetentionPolicy {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        archive_after_days: Option<u32>,
    },

    /// Create a new session (optionally in a working directory) and attach.
    CreateSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
    },

    /// Attach to an existing session and subscribe to its event stream.
    AttachSession { session_id: String },

    /// Detach from the currently attached session.
    DetachSession { session_id: String },

    /// Send a user message to the attached session.
    SendMessage {
        session_id: String,
        content: String,
        /// (media_type, base64_data) pairs.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<(String, String)>,
        /// Persist the message as context without starting a model turn.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        no_reply: bool,
        /// Skill to activate for this and subsequent turns, resolved against
        /// the daemon's skill registry (names arrive via the `skills` event).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_skill: Option<String>,
        /// Client-side working-mode reminder injected for this turn (the
        /// Shift+Tab plan/ask/manual texts) — same slot the TUI uses.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_reminder: Option<String>,
    },

    /// Cancel the in-flight generation.
    Cancel { session_id: String },

    /// Post a human-only team note: persisted in the transcript (role
    /// "note"), broadcast to every attached client, never model context.
    SendTeamNote { session_id: String, content: String },

    /// Inject a message at the next safe point without cancelling.
    SoftInterrupt {
        session_id: String,
        content: String,
        /// `(media_type, base64_data)` pairs, e.g. `("image/png", "...")` —
        /// same shape as `SendMessage.images`; mid-turn follow-ups can carry
        /// screenshots too.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<(String, String)>,
        #[serde(default)]
        urgent: bool,
    },

    /// Fetch conversation history.
    GetHistory { session_id: String },

    /// Fetch the tail of *any* session's conversation, attached or not.
    ///
    /// `GetHistory` can only answer for the session this connection is
    /// attached to, because it is routed through the daemon's attachment.
    /// A client showing several sessions at once (a switcher, an overview, a
    /// dashboard) needs a glance at the others without attaching to each in
    /// turn, which would disturb the very sessions it is trying to preview.
    /// Served from the stored record and capped to the last `limit` messages.
    PeekSession {
        session_id: String,
        /// Messages to return from the end. Defaults to a small tail.
        #[serde(default)]
        limit: Option<u32>,
    },

    /// Clear conversation history.
    Clear { session_id: String },

    /// Rewind history to the given 1-based message index.
    Rewind {
        session_id: String,
        message_index: usize,
    },

    /// Reply to a `PermissionRequest` event.
    PermissionResponse {
        session_id: String,
        request_id: String,
        decision: PermissionDecision,
    },

    /// List the models this session can switch to.
    ///
    /// A client that cannot enumerate models cannot offer a model picker, so
    /// it is stuck on whatever the daemon defaulted to. Served from the
    /// catalog the daemon already reports on attach.
    ListModels { session_id: String },

    /// Provider routes and active runtime identity for the attached session.
    GetRuntimeInfo { session_id: String },

    /// Persist an API-key credential in jcode's owner-only provider store and
    /// notify the daemon to reload it. OAuth tokens are intentionally excluded.
    SetApiKey { provider: String, api_key: String },

    /// Remove a previously persisted API-key credential.
    ClearApiKey { provider: String },

    /// Read one UTF-8 file under the session working directory.
    ReadFile {
        session_id: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes: Option<u64>,
    },

    /// Find files by case-insensitive path substring under the session root.
    FindFiles {
        session_id: String,
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },

    /// Search UTF-8 files for a literal text string.
    SearchText {
        session_id: String,
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },

    /// Read safe filesystem metadata for a path under the session root.
    FileStatus { session_id: String, path: String },

    /// Switch the session to a different model.
    ///
    /// `model` is an id from `ListModels`, e.g. `claude-opus-5`. A route
    /// suffix like `claude-opus-4-6[1m]` selects a specific context variant.
    SetModel { session_id: String, model: String },

    /// Set how much the model deliberates before answering.
    ///
    /// The cost/quality dial: `minimal`, `low`, `medium`, `high`, `xhigh`, or
    /// `max`, depending on what the provider supports. Providers that do not
    /// support it answer with an error rather than silently ignoring it.
    SetReasoningEffort { session_id: String, effort: String },

    /// Summarize the transcript so far, freeing context.
    ///
    /// Without this a long-lived client eventually hits the context limit and
    /// has no recourse but to clear the conversation and lose everything.
    Compact { session_id: String },

    /// Set a session's title, or clear it to restore the generated one.
    RenameSession {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },

    /// Restore the history that the last `Rewind` removed.
    ///
    /// `Rewind` is destructive, so without an undo a client cannot offer it
    /// safely: a mis-click costs the user their conversation.
    RewindUndo { session_id: String },

    /// Drop soft interrupts that have been queued but not yet delivered.
    ///
    /// The counterpart to `SoftInterrupt`: a client that lets a user queue a
    /// follow-up must also let them take it back before it lands.
    CancelSoftInterrupts { session_id: String },

    /// Liveness check.
    Ping,

    /// Forward-compatibility catch-all. Servers reply with an error frame.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    AllowAlways,
    Deny,
}
