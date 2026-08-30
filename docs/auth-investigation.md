# Auth investigation — per-user credentials in a shared harness

Investigated 2026-08-30 against `feat/api-ws-realtime` @ `4c5c889`.
Every claim below cites `file:line`. Where I could not determine something, it
says so.

---

## RECOMMENDATION: do not do the 2B threading work

**Stop conditions hit: three of them, independently.**

| Gate condition | Result |
|---|---|
| Call sites under 40 | **173 total / 85 production.** 2.1× over. |
| Mechanical parameter-threading | No — see "why it is not mechanical" below. |
| Caches already keyed per account | **Yes, keyed on `JCODE_HOME`** — and under the settled isolation model, home ≡ account. |
| Test suite runs and passes | **No — it did not compile.** Fixed in `f470ffb` to unblock; workspace results below. |

The short version: **the isolation you already decided on makes the cache
problem disappear without touching the cache.** You settled on separate Linux
users with separate home directories. The auth caches are already keyed on
`JCODE_HOME`, and the daemon socket already resolves per-uid. One process per
Linux user gets you correct isolation with a **zero-line** auth diff.

Threading an account handle through 85 production call sites buys you the
ability to multiplex users inside one process — which the settled decisions say
you do not want, because in-process multi-tenancy is exactly the "leaks
silently across processes" failure mode you rejected environment variables for.

---

## THE LIVE LEAK (read this first)

Two findings that are true of the **currently deployed** team server.

### 1. A global `ANTHROPIC_API_KEY` will serve any user whose own credentials are missing

You asked for certainty. Here is the code path.

`crates/jcode-base/src/auth/active_method.rs:94-95`:
```rust
let has_api_key =
    auth.anthropic.has_api_key || std::env::var("ANTHROPIC_API_KEY").is_ok();
```

`crates/jcode-base/src/auth/active_method.rs:103-110`:
```rust
let active = match forced {
    Some(kind) => kind,
    None if has_oauth => ActiveCredential::OAuth,
    None if has_api_key => ActiveCredential::ApiKey,   // <-- falls through to here
    None => return None,
};
```

A user with no OAuth credentials (`has_oauth == false`) and a global
`ANTHROPIC_API_KEY` in the process environment resolves to `ActiveCredential::ApiKey`
and runs on that key. There is no ownership check, no consent gate, no per-user
scoping.

Worse than cached: `std::env::var` is read **live at resolution time**, so it is
not even bounded by the 30s TTL. Every request re-reads it.

This is the exact failure you described, and it is real.

### 2. The team server today is one daemon, one Unix user, for everybody

`deploy/team-server/setup-team-server.sh:2` says it in its own first line:

> "Self-hosted blaude team server: one box, one daemon, every teammate's client
> connects over (w)ss with their own token."

The script is 132 lines and contains **no `useradd`, no `groupadd`, no setgid,
no `chown`**. It installs a systemd *user* service
(`setup-team-server.sh:95`) running `$BINARY api-bridge`
(`setup-team-server.sh:103`).

Teammates are distinguished **only by a bearer token** —
`~/.jcode/team-tokens.json` mapping email → token (`setup-team-server.sh:22`).
Every teammate's agent runs as the same Unix user, in the same home, against
the same `~/.jcode/auth.json`.

So on the deployed server, the `JCODE_HOME` cache key provides **zero**
separation between teammates: they all share one `JCODE_HOME`. The keying is
correct in principle and inert in this deployment.

**This is why pooling exists.** Pooling is not a feature bolted onto a working
multi-user system; it is the workaround that makes a single shared Unix account
usable by several people.

### Could not verify against a live server

1J asked for a live audit. **I could not run it.** There is no team server
configured on this Mac (`BlaudeTeamURL` is unset in `ai.blaude.native`), and
`gcloud compute instances list` fails with an expired refresh token
(non-interactive). To do the live half — `/etc/environment`, systemd drop-ins,
per-user home modes, the actual A-cannot-read-B test — I need either a running
server or `gcloud auth login` run interactively (`! gcloud auth login`).

Everything in this section is from provisioning code, not a live box.

---

## 1B — the caches, and the call-site count

### The caches

| Name | Type | Location | TTL | Keyed on |
|---|---|---|---|---|
| `AUTH_STATUS_CACHE` | `LazyLock<RwLock<Option<CachedAuthStatus>>>` | `auth/mod.rs:59` | 30s (`mod.rs:68`) | **`JCODE_HOME`** |
| `AUTH_STATUS_FAST_CACHE` | same | `auth/mod.rs:61` | 60s (`mod.rs:69`) | **`JCODE_HOME`** |
| `COMMAND_EXISTS_CACHE` | `LazyLock<Mutex<HashMap<String, bool>>>` | `auth/mod.rs:102` | none, per-process | command name |
| `GITHUB_TOKEN_CACHE` | `LazyLock<RwLock<Option<(String, Instant)>>>` | `auth/copilot.rs:13` | 300s (`copilot.rs:15`) | **nothing** |
| `RUNTIME_ACTIVE_OVERRIDES` | `LazyLock<RwLock<HashMap<&'static str, String>>>` | `auth/account_store.rs:11` | none | provider prefix only |
| `REFRESH_LOCKS` | `LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>>` | `auth/refresh_coordinator.rs:19` | n/a | key — fine |

**Corrections to the brief.** The brief said `AUTH_STATUS_CACHE` is "a single
global Option, not a map". The type is a single `Option`, but the tuple's third
element is the `JCODE_HOME` it was computed under:

`auth/mod.rs:57`:
```rust
type CachedAuthStatus = (AuthStatus, Instant, Option<std::ffi::OsString>);
```

and every read compares it (`mod.rs:275`, `mod.rs:304`, `mod.rs:312`,
`mod.rs:348`, `mod.rs:358`). The doc comment at `mod.rs:50-56` explains why:
issue #361, parallel provider tests observed each other's auth snapshots. So
upstream already hit this class of bug and fixed it — by keying on home.

**Two the brief did not know about, and they are worse:**

- `GITHUB_TOKEN_CACHE` (`copilot.rs:13`) caches an **actual token string**,
  keyed on nothing at all — not even `JCODE_HOME`. In a shared process,
  `cached_github_token()` (`copilot.rs:18`) serves user A's GitHub token to
  user B for 300s. This is a credential leak, not a status leak.
- `RUNTIME_ACTIVE_OVERRIDES` (`account_store.rs:11`) is the `/account switch`
  mechanism, keyed by provider prefix only. In a shared process, one user
  running `/account switch claude-otter` changes which account the *next*
  user's request uses (`account_store.rs:27-32`, consumed at `claude.rs:668`).

### The call-site count

This is the number you said decides the approach.

| Entry point | Sites |
|---|---|
| `AuthStatus::check()` | 11 |
| `AuthStatus::check_fast()` | 42 |
| `AuthStatus::check_fast_nonblocking()` | 3 |
| `AuthStatus::invalidate_cache()` | 117 |
| **Total** | **173** |
| — production | **85** |
| — test | **88** |

Production sites by area:

```
33  crates/jcode-tui/src/tui/app
12  crates/jcode-base/src/provider
12  crates/jcode-base/src/auth
 6  src/cli/commands
 6  src/cli
 5  crates/jcode-provider-doctor/src
 5  crates/jcode-app-core/src/server
 2  src/cli/auth_test
 2  crates/jcode-app-core/src
 1  src/cli/login
 1  crates/jcode-tui/src/tui/app/inline_interactive
 1  crates/jcode-harness-api-server/src
```

Against your calibration — "a dozen is an afternoon, eighty and I need to
reconsider the approach entirely" — **85 production sites is precisely the
"reconsider entirely" end.** Not marginally over. Over by design.

---

## 1E — why it is not mechanical

Three things stop this being parameter-threading.

**The TUI dominates and has no user identity.** 33 of 85 production sites are
in `jcode-tui`, mostly per-frame render paths. `check_fast_nonblocking` exists
specifically because probing on the render thread cost 40-55ms frames
(`mod.rs:325-339`). The TUI is single-user by construction — there is no
account handle in scope at a render call site, and inventing one means
threading identity into the render loop. That is restructuring.

**117 of the 173 are `invalidate_cache()`.** Invalidation is a different
problem from lookup. Today it is "clear the world" (`mod.rs:918-921`). Keyed
caches need "invalidate for whom" — and the honest answer at most of those 117
sites is "I don't know, I just changed a credential file". Most would degrade
to clearing everything anyway, so you would pay for 117 edits and keep the same
semantics.

**`AuthStatus` is a whole-process snapshot, not a per-account one.** It carries
every provider at once (`mod.rs:392-402` checks anthropic, jcode, openai,
openrouter, azure, bedrock, copilot). Keying it "per account" is a category
error: the type would need to become per-(user × provider), which changes the
type's meaning and every consumer, including `has_any_available()` and
`resolve_dual_credential_auth`.

**Estimate.** ~85 production signatures/call sites, ~88 test sites, plus a
semantic redesign of `AuthStatus` and of invalidation. This is not an
afternoon. It is a multi-day refactor against an upstream moving ~17
commits/day, in the single hottest file in the fork (`auth/mod.rs`, 66KB).
The rebase cost alone argues against it.

---

## 1F — the process model already does what you want

**`runtime_dir()` is per-Linux-user by construction.**

`crates/jcode-storage/src/lib.rs:103-120`:
- Linux: `$XDG_RUNTIME_DIR` (i.e. `/run/user/<uid>`)
- macOS: `$TMPDIR` (per-user)
- fallback: `temp_dir()/jcode-<euid>` (`lib.rs:122-129`)

The daemon socket is `runtime_dir()/blaude.sock`
(`crates/jcode-app-core/src/server/socket.rs:7-12`) and the harness API socket
is `runtime_dir()/jcode-api.sock`
(`crates/jcode-harness-api/src/sockets.rs:67-72`).

So if teammate A and teammate B are separate Linux users, they get separate
sockets, separate daemons, separate `JCODE_HOME`, separate account stores —
**with no code change at all.**

> **Corrected by the live audit** (see `multiplayer-investigation.md` §1J).
> The deployed server sets `JCODE_RUNTIME_DIR=/home/<user>/.jcode/runtime`
> explicitly, so `runtime_dir()` takes its *first* branch and never consults
> `$XDG_RUNTIME_DIR`; `/run/user/1000/` holds no blaude socket. The conclusion
> stands — the path is home-relative, so it is still per-user, and the sockets
> are `srw-------` (0600) on the inode, which the kernel enforces at
> `connect()`. But the mechanism is the explicit override, not XDG.

One harness process per Linux user is not a fallback. It is the grain of the
existing code. The current single-daemon deployment is what fights it.

**Cost of per-user processes.** I did not measure RSS — that needs a live
Linux box (see the 1J gap). Startup is a socket bind plus lazy statics; the
caches are `LazyLock`, so an idle process populates almost nothing. The real
cost I can see in code is duplicated model-catalog and config parsing per
process, not credentials.

The brief's `ensure_server_running` and `validate_reload_socket_path` **do not
exist** under those names. The nearest real things are
`socket.rs:237 spawn_server_notify` and `server/reload.rs:16 prepare_server_exec`.

---

## 1C — the real credential waterfall (Claude)

Traced in `crates/jcode-base/src/auth/claude.rs:569-641`. The brief's order was
wrong in four ways.

**Actual order in `load_credentials()`:**

1. Claude Code native credentials, consent-gated via
   `external_auth_source_allowed_for_path_cached` — `claude.rs:574-592`
2. `CLAUDE_CODE_OAUTH_TOKEN` env var, gated by `native_source_allowed()` —
   `claude.rs:600-607`
3. blaude's own `auth.json` via `load_jcode_credentials()` — `claude.rs:609-614`
4. OpenCode `auth.json`, consent-gated — `claude.rs:616-634`
5. **Fall back to the first expired candidate** — `claude.rs:636-638`
6. Otherwise bail — `claude.rs:640`

**Corrections:**

- `~/.jcode/auth.json` is **third**, not first. Claude Code's own credentials
  win over blaude's.
- **The macOS Keychain is deliberately NOT read here.** `claude.rs:596-599`
  says so explicitly: it can trigger an interactive unlock, so it is read once
  at import time and copied into `auth.json`.
- `~/.pi/agent/auth.json` does **not** appear in this function.
- **`ANTHROPIC_API_KEY` is not in this waterfall at all.** The OAuth path and
  the API-key path are separate mechanisms, joined at
  `active_method.rs:103-110` (see the leak section). The brief's mental model —
  env var at the end of one waterfall — is wrong in a way that matters: it is
  not a last resort, it is a *parallel* tier selected when OAuth is absent.

Step 5 is worth knowing about independently: resolution returns **expired**
credentials rather than failing, which will interact with any "fail cleanly
when the user has no credentials" requirement.

I traced Claude only. OpenAI/codex, Gemini, and Antigravity waterfalls are
**not** covered here.

---

## 1D — an identity type already exists, in two places

**`load_credentials_for_account(label: &str)`** — `claude.rs:644-659`. A
per-account credential resolver that already takes an explicit account handle.
The account-keyed path you wanted to build **partially exists**; what is missing
is that the hot paths call the global `load_credentials()` instead.

The handle is a **string label** (`"claude-otter"`), generated from a fixed
animal list (`account_store.rs:37-54`) so the same ordinal account gets the
same name across providers.

**`StoredMessage::by_user: Option<String>`** —
`crates/jcode-session-types/src/lib.rs:306-310`. Team identity that authored a
message, added by local commit `6aad329`. This is a *product-level* member
identity (email), distinct from the provider account label.

And in the login flow, `Job::member: Option<String>` —
`crates/jcode-harness-api-server/src/login_jobs.rs:37-40` — "a team member's
email, or the owner's username", stamped onto the account as `added_by`
(`login_jobs.rs:308-310`, `claude.rs:483`).

So there are already **two** identities in the system: a provider account label
and a team member email, and `added_by` is the existing join between them. If
you do per-user work, that join is the thing to build on — not a new type.

**`refresh_coordinator.rs`** keys its locks by string
(`refresh_coordinator.rs:19`, `HashMap<String, Arc<tokio::sync::Mutex<()>>>`),
so it does **not** assume a single active account. It is already
multi-account-safe.

---

## 1A — pooling (partial map)

I did not complete the full pooling map, because the gate answer landed first
and the finding below changes what "removal" even means.

**Pooling is the last step of the credential-provisioning flow, not a
subsystem.** `login_jobs.rs:298-310`:

```rust
let tokens = oauth::exchange_claude_code(&verifier, &input, &redirect_uri).await?;
// ... APPEND it as a new pooled account instead of overwriting. This is what
// lets a team pool every member's Claude subscription on the server — the
// daemon's same-provider failover then rotates across the pool.
let requested = claude::login_target_label(None)?;
let (label, _email) = oauth::save_claude_login(&tokens, &requested).await?;
if let Some(member) = member.as_deref() {
    let _ = claude::set_account_added_by(&label, member);
}
```

Removing pooling is therefore **not a deletion** — it is changing *where*
`save_claude_login` writes: from the one shared account store into that
teammate's own Linux user's `~/.jcode/auth.json`. The identity needed to do
that (`member`) is already threaded to this exact line.

**What this makes harder than expected:** pooling is load-bearing for
same-provider failover. `same_provider_account_failover` defaults to `true`
(`crates/jcode-config-types/src/lib.rs:1222, 1258`) and rotates across the
pooled accounts. Remove the pool and each user has exactly one account, so
same-provider rollover has nothing to roll over *to*. See 1I.

**Removing pooling does NOT break the team layer.** This gate condition does
not fire. `team_create_jobs.rs` and `team_access.rs` contain **zero**
references to the provider account store — no `anthropic_accounts`, no
`claude::`, no `codex::`, no `auth.json`, no `save_claude_login`. The two
layers are already independent, and commit `f8b0ade` made that explicit:

> "fix(team): a team server boots and works with no AI account [...] Verified
> on a fresh VM: owner and member both create sessions and each sees the
> other's, with no AI account anywhere."

It sets `JCODE_DEFERRED_AUTH_BOOTSTRAP` in the generated unit
(`team_create_jobs.rs`) so the daemon boots credential-less and fails
individual turns with a clear message instead of crash-looping.

Beware a naming collision when reading this code: **"account" means two
different things.** In `blaude_account.rs` and `team_access.rs` it is the
*Clerk sign-in identity* (`me()`, `identity()`, `sign_out()` —
`blaude_account.rs:144, 244, 248`). In `auth/claude.rs` and
`auth/account_store.rs` it is the *AI provider subscription*. Pooling concerns
only the second.

**Not determined:** what migration existing teams need — i.e. how to split an
already-pooled `auth.json` back out to per-person homes. That needs the rest of
1A and a live server to inspect.

---

## 1G — the provisioning path you thought was unresolved is largely built

`login_jobs.rs:1-16` documents a **loopback-relay OAuth flow** that already
solves "get a teammate's credentials onto the team server":

1. The Mac app opens a loopback listener **on its own machine**
2. Calls `start_*_login { redirect_uri: "http://localhost:<port>/callback" }`
3. The bridge generates PKCE + the authorize URL, keeps the verifier **in
   memory, never on the wire**, returns the URL
4. The teammate approves; the browser redirects to the app's own loopback
5. The app relays the code back via `complete_login { job_id, code }`
6. **The bridge exchanges the code for tokens in-process and saves them**

No localhost dependency on the server, no CLI subprocess, no code paste. It
survives a bridge restart by persisting the PKCE secret to disk at 0600
(`login_jobs.rs:50-65`).

Step 6 is the only part that needs to change: today it writes to the single
shared store. Per-user, it must write into the requesting member's Linux home.
`member` is already available there.

**Security trade-off of this path:** the bridge sees the authorization code and
the resulting tokens in its own memory. That is acceptable when the bridge runs
as the *same* user it writes credentials for; it is a privilege boundary
violation if one shared-root bridge writes into every user's home. That is the
real design question for 2C, and it is the argument for per-user bridges over a
shared one.

---

## 1I — model-limit rollover does not exist today

`crates/jcode-provider-core/src/failover.rs:69-151`,
`classify_failover_error_message`, returns one of three
`FailoverDecision`s (`failover.rs:32-37`). It buckets by string matching:

- context/size → `RetryNextProvider` (`failover.rs:72-91`)
- capacity shed, "overloaded" / 529 → `RetryNextProvider` (`failover.rs:99-105`)
- **rate/quota: "rate limit", "quota", "credit balance", "billing", "usage
  tier", 429, 402 → `RetryAndMarkUnavailable`** (`failover.rs:107-125`)
- auth/access, 401/403 → `RetryAndMarkUnavailable` (`failover.rs:127-148`)

**It cannot distinguish a model-specific limit from a session limit from a
generic rate limit.** All three land in the same bucket at `failover.rs:107`.
There is no model dimension in `FailoverDecision` at all.

The code *knows* the distinction exists but discards it — `failover.rs:96-98`:

> "the outage is model/pool-scoped (opus-5 can shed while fable-5 streams fine)"

That comment is on the 529 branch and is the only place model scope is
acknowledged. The 429/quota branch marks the whole **provider** unavailable.

There is a hook for switching: `failover_account(&self, reason: &str) ->
Option<String>` (`crates/jcode-provider-core/src/lib.rs:287`), described at
`lib.rs:285` as switching mid-stream and returning the new account label.

**Smallest change for automatic model-limit rollover:** add a model-scoped
variant to `FailoverDecision` and populate it from the limit response, then
drive `failover_account` from it. That is **harness** work — it needs the
provider error body, which never reaches the orchestration layer.

**But** it interacts with pooling removal: rollover needs more than one account
to roll to. Per-person credentials means one account per person, so
"switch account" becomes "switch provider" or "wait". Worth deciding before
building either.

---

## 1H, 1K, 1L, 1M, 1N, 1O — not investigated

I stopped at the gate rather than complete these. `docs/multiplayer-investigation.md`
and `docs/client-investigation.md` are **not written**.

Partial 1N data, since it bore on the gate:

### The test suite did not compile, in 21 places

`cargo test --workspace` failed to build across **21 sites in 21 files**,
all from local commits adding fields to shared types and updating only the
production constructors. `cargo build` stayed green throughout, which is why
none of it was noticed.

| Field | Added by | Missed sites | Fixed in |
|---|---|---|---|
| `StoredMessage/HistoryMessage/RenderedMessage::by_user` | `6aad329` | 48 (18 files) | `f470ffb`, `afd1205` |
| `SessionInfo::last_active_ms` | `ad3ab53` | 3 | `6162684` |
| `SessionInfo::user_messages` | `b71eaba` | 3 | `6162684` |

**CI would not have caught the last two.** They live in
`crates/jcode-sdk/tests/` and `crates/jcode-harness-api/examples/`, which
`--lib --bins` (`.github/workflows/ci.yml:227`) does not build. Only
`--all-targets` reaches them. Worth widening the CI step.

### Four tests fail for reasons unrelated to those fixes

After the build was repaired: **4818 passed, 4 failed** across 72 suites
(`--lib --bins`). None of the four touches `by_user`, `user_messages` or
`last_active_ms`.

**Coverage is not the same between invocations, so "the suite passes" depends
on how you ask.** Two runs of the same tree:

| Invocation | Suites | Tests | Failures |
|---|---|---|---|
| `--workspace --lib --bins` | 72 | 4822 | 4 |
| `--workspace --all-targets` | 88 | 3971 | 2 |

`--all-targets` builds *more* targets but ran ~850 *fewer* tests, and
`schema_snapshot` did not run in it at all (the two TS-SDK parity failures are
absent from that run, yet reproduce every time via
`cargo test -p jcode-harness-api --lib schema_snapshot`). Likewise
`jcode-base`'s 1313 lib tests — including the known cursor failure — did not
appear in either workspace run, but fail reliably under `-p jcode-base --lib`.

Feature unification differs per invocation, so neither command is complete
coverage on its own. Anyone gating work on "tests pass" here needs to pin the
invocation first; today CI's `--lib --bins` misses integration tests and
examples entirely.

**Two are a live realtime regression, and they matter to the product bar.**

`harness_api_tests/schema_snapshot.rs:205` and `:236`:

> `ApiEvent::sessions_changed is missing from sdk/typescript/src/protocol.ts`

`sessions_changed` was added by `23a8174` — *"push new chats over the socket
instead of polling"*. It reached Rust clients and neither of the other two: not
the TypeScript SDK, and not the embedded web client, whose dispatch handled 11
event kinds without it. On that client a teammate's new chat never appeared at
all until the page was reloaded.

The realtime work landed for one client and silently missed the others, and the
parity test that exists to catch exactly this had been failing invisibly behind
the compile error.

**Since resolved two ways.** `sessions_changed` was added to the TypeScript SDK,
and **the embedded web client was deleted outright** — it was a second,
half-featured client nobody asked for, and every new event meant maintaining a
third surface alongside the app and the SDK. Keeping the parity test honest for
two clients is tractable; three was the reason this slipped.

**The other two are pre-existing and lower stakes:**

- `sdk_tests/parity.rs:143` — `addDir` and `installSkill` exist in the TS SDK
  but are in neither `CAPABILITIES` nor `TS_ONLY`.
- `anthropic_tests.rs:1861` — server recommendation `'Opus 4.8'` maps to
  `claude-opus-4-5`, not `claude-opus-4-8`. The pooling commit `89b25a9`
  touched this file, but only to add `added_by` to fixtures (4 lines); it did
  not touch this assertion.

### `cargo fmt --check` fails on main

**146 diffs**, verified by stashing. CI runs `cargo fmt --all -- --check`
(`ci.yml:591`), so the Format job is red on main independently of any of this.
The drift is concentrated in local product code — nearly every file under
`crates/jcode-harness-api-server/`. Not touched here: reformatting 146 files
would bury an auth diff and wreck the rebase surface.
- `jcode-base` auth tests: 391 pass, **1 pre-existing failure** —
  `auth::tests::cursor_status_is_available_for_authenticated_cli_session`
  (`auth/tests.rs:794-814`). Inherited from upstream, not local: the test is
  byte-identical at the fork point and `probe_cursor_status`
  (`auth/mod.rs:1160-1180`) is byte-identical too. The test mocks a cursor CLI
  via `JCODE_CURSOR_CLI_PATH`, but the Full-mode probe never consults a CLI —
  it checks `has_cursor_native_auth()` (`auth/cursor.rs:97-99`: env/file/vscdb)
  and `has_cursor_api_key()` only. Upstream test/code mismatch. It fails on a
  machine without consented Cursor credentials and would pass on one with them.

---

## What I would do instead

1. **Fix the env-var fall-through** (`active_method.rs:103-110`). This is the
   actual vulnerability and it is a small, contained change: gate the
   `has_api_key` tier behind a server-mode flag defaulting off, so a user with
   no OAuth fails cleanly instead of silently borrowing a global key. This was
   2B's most valuable requirement and it does **not** need the threading work.
2. **Go per-Linux-user, per-process.** The socket layer already does this. The
   caches are already correct under it. Rewrite
   `deploy/team-server/setup-team-server.sh` — which today creates no users at
   all — to provision a user, a home, a shared setgid project dir, and a
   per-user bridge unit.
3. **Point `save_claude_login` at the requesting member's home** instead of the
   shared store. `member` is already in scope at `login_jobs.rs:308`.
4. **Leave the caches alone.** Revisit only if you ever decide you want
   in-process multi-tenancy, and note that decision contradicts the isolation
   rationale you already settled.

For a rebase in a few weeks: items 1 and 3 are ~single-hunk changes in
upstream-owned files; item 2 is a product file with no upstream equivalent.
That keeps the rebase surface near zero, which the 168-commits-in-10-days
upstream rate makes worth protecting.
