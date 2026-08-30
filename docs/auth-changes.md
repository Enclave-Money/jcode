# Auth and multiplayer changes

What changed in `blaude-agent`, why, and how to re-derive it after a rebase.
14 commits, 48 files, +2520 / −57, on top of `4c5c889`.

Upstream moved 168 commits in the 10 days before this work, so the ordering
below is by rebase difficulty: the product-only changes will never conflict,
and the upstream-owned hunks are small and individually described.

**Before rebasing at all:** this is a shallow clone (`.git/shallow`, grafted at
`7313a1c`). `git rebase` against upstream cannot work until
`git fetch --unshallow`.

---

## The decision that shaped everything

**The auth-cache threading work in the original plan was not done, deliberately.**
Three gate conditions fired; the reasoning is in `docs/auth-investigation.md`.
The short version: the caches are already keyed on `JCODE_HOME`, and under the
settled isolation model (separate Linux users, separate homes) home is
equivalent to account, so per-user processes get correct isolation with a
zero-line auth diff. 173 call sites of threading would have bought in-process
multi-tenancy, which the same settled decisions say is not wanted.

What was built instead is the isolation those decisions imply.

## 2A: pooling was NOT deleted, deliberately

The brief said delete rather than deprecate. I did not, and the reason is
sequencing rather than reluctance.

"Pooling" is not a subsystem. It is one property of the sign-in path:
`save_claude_login` **appends** a new account when the identity differs instead
of overwriting (`login_jobs.rs:298-310`). Deleting that property restores the
overwrite it replaced, and on a **shared-home** server — which is what
`blaude-gm-25c8` still is — a second teammate's sign-in would then clobber the
first. That is strictly worse than pooling, not better.

Under per-user homes the property becomes **inert on its own**: one Linux user
has one identity, so there is never a second account to append, and the append
branch simply never fires. Pooling does not need deleting; it needs the
isolation that makes it moot, which is what `provision-member.sh` provides.

So the ordering is: migrate the live server to per-user provisioning, confirm
each member signs in against their own home, and *then* delete the append
branch as dead code. Deleting it first breaks a working server.

**Migration burden when that happens: zero.** The live server's store holds one
account (`claude-otter`, the owner's own) and `team-tokens.json` holds one
member. There is no multi-member pool in existence to migrate.

---

## 1. Product code — no upstream equivalent, will not conflict

### `deploy/team-server/provision-member.sh` (new)
### `deploy/team-server/deprovision-member.sh` (new)
### `deploy/team-server/verify-isolation.sh` (new)

One Linux user per teammate: own home (0750), own `~/.jcode` (0700), own daemon
and bridge on their own socket, shared setgid project directory, membership of
a `blaude` group.

Two details are load-bearing and easy to lose in a rewrite:

- **`umask 002`**, or setgid inherits the group without the group *write* bit
  and the next teammate cannot edit what the last one wrote.
- **`core.sharedRepository=group`**, or one teammate's git objects are
  unwritable by the rest and git fails with a bare "Permission denied" that
  reads like a git bug.

`verify-isolation.sh` attempts the accesses that must fail rather than reading
permission bits, and reports unrunnable checks as SKIP so a vacuous pass cannot
hide a regression.

**Evidence:** run on the live server with two provisioned members. qa1 could not
read qa2's `auth.json` (positive control: qa2 could read its own); qa2 could
append to qa1's file at `664 qa1:blaude`; sockets were `600`. The two FAILs it
reported were true positives — the legacy single-user units predate
`JCODE_SERVER_MODE`.

### `crates/jcode-app-core/src/tool/write_queue.rs` (new)

Per-repo write serialisation. `docs/SWARM_ARCHITECTURE.md:307-310` rules Swarm
out for this: it is explicitly optimistic and lock-free, and detects conflicts
after the fact.

Mirrors the existing `REFRESH_LOCKS` idiom
(`crates/jcode-base/src/auth/refresh_coordinator.rs:19`), so it should look
familiar to upstream.

The lock wraps the **whole read-modify-write**, not the write syscall.
**Verified by negative control:** bypassing the lock in the concurrent-writers
test loses 7 of 8 increments (`"1"` instead of `"8"`).

Known gap, documented in the module header: `bash` tool writes do not pass
through the queue.

### `crates/jcode-harness-api-server/src/team_create_jobs.rs`

Adds `Environment=JCODE_SERVER_MODE=1` to both generated units.

---

## 2. Upstream-owned files — the hunks to re-derive

Each of these is small and independent. Re-derive in this order.

### a. `crates/jcode-provider-core/src/lib.rs` — new function

Adds `env_credential_fallback_allowed()` just above `enum CredentialMode`.
Reads `JCODE_SERVER_MODE` with the same truthiness rule as
`jcode-base/src/auth/mod.rs:162`. Pure addition; nothing else in the file moves.

### b. `crates/jcode-provider-anthropic-runtime/src/lib.rs` — one branch

**This is the security fix.** In `get_access_token`, the `Auto` arm fell through
to `direct_api_key()` on *any* OAuth failure, and `direct_api_key` reads
`ANTHROPIC_API_KEY` from the process environment
(`crates/jcode-base/src/provider/anthropic.rs:123-127`). One daemon serving a
team therefore let a single environment key satisfy whoever was missing
credentials, spending one person's quota on another's work.

The change adds one guard before the existing fallback. Explicitly pinned
API-key mode is untouched: it is a deliberate choice.

If upstream restructures `get_access_token`, re-apply as: *before any automatic
substitution of an environment-derived key, return the original OAuth error when
`env_credential_fallback_allowed()` is false.*

### c. `crates/jcode-provider-anthropic-runtime/src/lib.rs` — parser fix

`anthropic_recommended_model_from_error` cut the hint at the first `.`, which is
the decimal point in "opus 4.8". The tokens `["opus","4"]` then scored equally
against every `opus-4-x` id and `max_by_key` broke the tie by catalog order, so
a recommendation that said 4.8 returned 4.5. Now only a `.` that is **not
between two digits** ends the sentence.

This one is a genuine upstream bug and is worth offering upstream rather than
carrying.

### d. `crates/jcode-base/src/bus.rs` — new event

Adds `WriteQueued` struct and `BusEvent::WriteQueued`. Pure addition.

### e. `crates/jcode-protocol/src/wire.rs` — new event

Adds `ServerEvent::WriteQueued`. Pure addition next to `SessionsChanged`.

### f. `crates/jcode-app-core/src/server/client_lifecycle.rs` — one match arm

Forwards `BusEvent::WriteQueued` to the client on the existing bus-to-client
subscription. First arm in the existing `match bus_event`.

### g. `crates/jcode-app-core/src/tool/{edit,write,patch}.rs` — wrap the write

Each wraps its existing write span in `with_repo_write_lock`. `edit` needed a
small enum because its no-match path returns early and must run its fuzzy
retry *outside* the lock. `patch` takes the lock per file, not per batch, so a
large patch does not stall the team for its whole run.

### h. `crates/jcode-harness-api/src/events.rs`, `.../translate.rs` — new event

`ApiEvent::WriteQueued` plus its translation arm. Pure additions.

### i. `sdk/typescript/src/protocol.ts` — three additions

`sessions_changed`, `write_queued`, and `team_members.owner`. The first two also
need their tag in `KNOWN_EVENT_KINDS`.

### j. `crates/jcode-sdk/src/client.rs`, `.../sdk_tests/parity.rs`

`add_dir` and `install_skill` on the Rust client, plus their capability entries.
These existed in the TS SDK only, which the parity contract forbids.

---

## 3. Test repairs — likely to conflict, trivially

`cargo test --workspace` did not compile. Three local commits added fields to
shared types and updated only the production constructors:

| Field | Added by | Sites missed |
|---|---|---|
| `by_user` on 3 message types | `6aad329` | 48, in 18 files |
| `SessionInfo::last_active_ms` | `ad3ab53` | 3 |
| `SessionInfo::user_messages` | `b71eaba` | 3 |

All are `by_user: None` / `user_messages: None` / `last_active_ms: None` in test
fixtures. If a rebase reintroduces them, the fix is mechanical.

**CI would not have caught two of them:** they live in `crates/jcode-sdk/tests/`
and `crates/jcode-harness-api/examples/`, which `--lib --bins`
(`.github/workflows/ci.yml:227`) does not build. **Widening that step to
`--all-targets` is the durable fix and is not done here.**

One test also encoded an ordering the realtime work broke:
`client_session_tests/clear.rs` asserted `SessionId` was the first frame, but
`SessionsChanged` is a broadcast that can arrive anywhere. It now pins the
direct-reply ordering it actually means.

---

## 4. State of the suite

| Suite | Before | After |
|---|---|---|
| `cargo test --workspace --lib --bins` | **did not compile** | **3612 passed, 0 failed** |

Caveat worth carrying: test counts differ by invocation (5729 vs 3894 vs 3612 on
the same command across runs, and `--all-targets` runs a different set again).
Feature unification differs per invocation, so **pin the invocation before
gating anything on "tests pass"**.

`cargo fmt --check` reports **146 diffs on clean main**, concentrated in
`crates/jcode-harness-api-server/`. CI runs `cargo fmt --all -- --check`
(`ci.yml:591`), so that job is red independently of this work. Not touched:
reformatting 146 files would bury this diff and wreck the rebase surface.

---

## Appendix: the 1B call-site map

The number that decided against the threading work. Precise counts, `AuthStatus`
entry points only:

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

The caches themselves:

| Name | Type | Location | TTL | Keyed on |
|---|---|---|---|---|
| `AUTH_STATUS_CACHE` | `LazyLock<RwLock<Option<CachedAuthStatus>>>` | `auth/mod.rs:59` | 30s | `JCODE_HOME` |
| `AUTH_STATUS_FAST_CACHE` | same | `auth/mod.rs:61` | 60s | `JCODE_HOME` |
| `COMMAND_EXISTS_CACHE` | `LazyLock<Mutex<HashMap<String, bool>>>` | `auth/mod.rs:102` | none | command name |
| `GITHUB_TOKEN_CACHE` | `LazyLock<RwLock<Option<(String, Instant)>>>` | `auth/copilot.rs:13` | 300s | **nothing** |
| `RUNTIME_ACTIVE_OVERRIDES` | `LazyLock<RwLock<HashMap<&str, String>>>` | `auth/account_store.rs:11` | none | provider only |
| `REFRESH_LOCKS` | `LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>>` | `auth/refresh_coordinator.rs:19` | n/a | key — fine |

The last two are the ones to watch if in-process multi-tenancy is ever
revisited: `GITHUB_TOKEN_CACHE` holds an actual token keyed on nothing, and
`RUNTIME_ACTIVE_OVERRIDES` means one user's `/account switch` redirects the
next user's request.
