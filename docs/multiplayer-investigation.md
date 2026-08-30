# Multiplayer investigation — live audit of the team server

Audited 2026-08-30 against the running team server `blaude-gm-25c8`
(asia-south1-a, e2-small, 34.93.93.41), via `gcloud compute ssh
--tunnel-through-iap`. Read-only: nothing on the server was modified.

Covers **1J**. 1K–1N are not covered — see the end.

---

## 1J — credential leak audit

### Headline: there is nothing to leak *between* users, because there are no users

**The isolation question is unanswerable as posed.** The brief asked whether
user A can read user B's `~/.jcode/` or `~/.claude/`. I could not test it, and
the reason is the finding:

```
$ awk -F: '$3>=1000 && $3<65534 {print $1, $3, $6, $7}' /etc/passwd
sumermalhotra 1000 /home/sumermalhotra /bin/bash
```

**One non-system user.** Every teammate's agent runs as uid 1000. A and B are
not two users who might read each other's credentials — they are the *same*
Unix user sharing one `auth.json` outright. There is no boundary to breach.

```
$ ps -eo pid,user,etime,cmd | grep blaude
16953 sumermalhotra 14:45:37 /home/sumermalhotra/blaude --provider auto serve
16954 sumermalhotra 14:45:37 /home/sumermalhotra/blaude api-bridge
```

One daemon, one bridge, both uid 1000, up ~15h. Teammates are separated only by
a bearer token in `~/.jcode/team-tokens.json`.

Confirmed absent, box-wide:

| Settled design element | Present? |
|---|---|
| One Linux user per teammate | **No** — only uid 1000 |
| Per-teammate home directories | **No** — only `/home/sumermalhotra` |
| Shared group | **No** — non-system groups are only `google-sudoers`, `docker`, `lxd`, and the user's own private group |
| Group-owned project dir | **No** |
| setgid on the project dir | **No** — `find /home /srv /opt /workspace -type d -perm -2000` returns nothing |

### What is actually configured correctly

This part is good news and worth recording, because the code-level risks I
flagged in `auth-investigation.md` are **not** live here.

**No credential environment variables anywhere.** Scanned for
`ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, `OPENAI_API_KEY`,
`GEMINI_API_KEY`, `GOOGLE_API_KEY`, `OPENROUTER_API_KEY`, `CURSOR_API_KEY`,
`GITHUB_TOKEN`, and generic `API_KEY` / `_TOKEN`:

| Location | Result |
|---|---|
| `/etc/environment` | clean |
| `/etc/profile`, `/etc/profile.d/*` | clean |
| `/etc/bash.bashrc`, `/etc/zsh` | clean |
| `/etc/systemd/`, `/run/systemd/`, `/lib/systemd/system/` | clean |
| `~/.config/systemd/` | clean (no units exist) |
| Docker | no containers running |

And the decisive test — the **actual process environments**:

```
$ sudo cat /proc/16953/environ | tr '\0' '\n' | grep -E 'ANTHROPIC|CLAUDE|OPENAI|GEMINI|API_KEY|TOKEN'
  NONE
$ sudo cat /proc/16954/environ | ...
  NONE
```

So the `active_method.rs:103-110` env-var fall-through documented in
`auth-investigation.md` is a **latent** vulnerability here, not an active one.
Nothing would currently fall through to a global key, because no global key
exists. That could change the moment someone exports one for convenience —
which is exactly why the fix is still worth making.

**File modes are correct.**

```
drwx------ 17  ~/.jcode/
-rw-------     ~/.jcode/auth.json
-rw-------     ~/.jcode/api-ws-token
-rw-------     ~/.jcode/clerk.env
-rw-------     ~/.jcode/join-tickets.json
-rw-------     ~/.jcode/blaude-account.json
-rw-------     ~/.jcode/team-tokens.json
srw-------     ~/.jcode/runtime/blaude.sock
srw-------     ~/.jcode/runtime/blaude-debug.sock
srw-------     ~/.jcode/runtime/jcode-api.sock
```

Credentials 0600 inside a 0700 directory, and the unix sockets are **0600 on
the socket inode itself** — the strongest form, since the kernel checks
permission at `connect()`. `~/.jcode/runtime/` and `~/.jcode/login-jobs/` are
0755, but the 0700 parent blocks traversal, so that is cosmetic rather than
exploitable. `~/.jcode/login-jobs/` is empty — no stranded PKCE secrets.

`~/.claude/` does not exist on the server.

**TLS is properly configured.** The bridge listens on `0.0.0.0:443`, publicly
reachable. Probed from outside the box:

```
subject=CN=34-93-93-41.sslip.io
issuer=C=US, O=Let's Encrypt, CN=YE2
Protocol: TLSv1.3   Cipher: TLS_AES_256_GCM_SHA384
Verify return code: 0 (ok)
```

Plain HTTP to the same port is refused. So member bearer tokens are not
crossing the internet in cleartext. `setup-team-server.sh:56-59` warns about
exactly this case; the deployment did the right thing.

### Deployment drift from the documented script

**`setup-team-server.sh` is not what is deployed.**

- `~/.config/systemd/user/` **does not exist**. The script's whole install path
  (`setup-team-server.sh:95-122`) never ran.
- Both processes have **PPID 1** — started detached and adopted by init, not by
  a systemd user unit.
- `JCODE_RUNTIME_DIR=/home/sumermalhotra/.jcode/runtime` is set explicitly in
  the daemon environment. The script sets no such variable.

Provisioning is evidently done by `team_create_jobs.rs`, not by the checked-in
script. Anyone reading the script to understand the deployment will be wrong
about how it starts, where its sockets live, and how to restart it.

**This corrects a claim in `auth-investigation.md`.** I inferred from
`jcode-storage/src/lib.rs:103-120` that sockets land in `$XDG_RUNTIME_DIR`
(`/run/user/<uid>`, 0700, kernel-enforced). On this box they do **not**:
`JCODE_RUNTIME_DIR` takes the first branch and points at a home-relative path,
and `/run/user/1000/` contains only `gnupg` and `systemd`. The per-user
isolation conclusion still holds — home-relative is, if anything, more aligned
with the home-relative credential rationale — but the mechanism is the explicit
override, not XDG.

### Migration burden for removing pooling: effectively zero

```
anthropic_accounts: 1
  label=claude-otter  email=sumermalhotra1998@gmail.com  added_by=sumer@enclave.money
active: claude-otter

team-tokens.json members: ['sumermalhotra1998@gmail.com']
```

One pooled account — the owner's own — and one team member. **There is no
multi-member pool in existence to migrate.** Whatever 2A does, no teammate
currently loses an account.

Note the two identities visible in one record: the Anthropic account's own
email (personal) and `added_by` (the work identity that signed it in). That
`added_by` join is the existing link between provider account and team member,
as described in `auth-investigation.md` §1D.

### Other observations

- Daemon cwd is `~/team`, which is **empty** and not a git repo.
  `~/ghostmotion-website` (0755, user-owned, no setgid) sits separately in
  home. Which of these is meant to be "the shared project" is not evident from
  the box.
- `~/.jcode/runtime/durable-state/swarm/` exists — relevant to 1H, which I did
  not investigate.
- `/home/sumermalhotra` itself is 0755. Harmless with one user; it would matter
  the moment a second Linux user exists.

### Verdict

No credential leak is currently possible **between teammates via the
filesystem or environment**, because the filesystem and environment are
correctly locked down. The exposure is architectural: every teammate executes
as the same Unix user against the same credential store, so "leak" is not the
right word — there is simply no separation, by design, today.

Nothing here needs an emergency fix. Everything here needs 2C.

---

## 1K — team and session layer

### Identity already exists at the bridge

This is the most useful fact in this section, because it is the hook everything
else hangs off.

`crates/jcode-harness-api-server/src/ws.rs:354` `authorize()` maps a presented
bearer to `(identity, is_owner)`:

- **Owner** — presents the `api-ws-token`, or connects over the local 0600 unix
  socket. Identity is their blaude account email
  (`ws.rs:378-380`), falling back to `$USER`.
- **Member** — presents a token from `team-tokens.json`; identity is the email
  that token is filed under (`ws.rs:382-386`).
- Comparison is constant-time (`ws.rs:389-395`), so tokens are not guessable by
  timing.

That identity is then carried into the connection handler
(`ws.rs:514`, `ws.rs:527`), so **every request already knows which human it
belongs to**. Nothing new needs inventing to route a member to their own Linux
user: the value is in hand at the moment the socket authenticates.

### How a client routes to a session

`translate.rs` holds one `session_id` per bridge connection
(`translate.rs:90`). Attach sets it (`translate.rs:297-301`); requests naming a
different session are rejected or re-routed (`translate.rs:323-325`,
`translate.rs:472-476`). So the bridge connection *is* the session binding, and
one connection sees one session at a time.

### Where a shared account, home, or process is assumed

| Assumption | Where | Note |
|---|---|---|
| One daemon socket for everyone | `ws.rs:527` passes a single `legacy_socket` to every connection | Every member's traffic is translated onto the same daemon |
| One credential store | pooling writes to the daemon's own account store, `login_jobs.rs:298-310` | See `auth-investigation.md` §1A |
| One `~/.jcode` | `permissions.rs:22-34` resolves the safety queue and history under one `jcode_dir()` | Permission prompts are therefore team-global, not per member |
| One `~/.jcode/login-jobs` | `login_jobs.rs:68` | Pending sign-in secrets share a directory |
| One Unix user | `deploy/team-server/setup-team-server.sh`, and the live box | Now addressed by `provision-member.sh` |

The permissions one is worth calling out: because the safety queue is a single
file-mediated store under one home (`permissions.rs:30`), a permission prompt
raised by one member's agent is visible to, and answerable by, the whole team.
Under per-user processes that store becomes per-user automatically, since it is
home-relative. That is another thing the isolation work fixes for free.

### Realtime path (1M)

Audited for polling in the session, presence and team layer. One real instance:

**Permission prompts are polled, not pushed.** `lib.rs:459`:

```rust
let mut permission_poll = tokio::time::interval(Duration::from_millis(900));
```

The comment at `lib.rs:456-457` explains why: permission prompts are
"file-mediated by design", so the bridge polls the on-disk queue rather than
receiving an event. Everything else on this path is push: sessions, messages,
presence and team notes all arrive as `ServerEvent`s forwarded to clients.

Against the stated bar this qualified as an architectural gap rather than a
tuning problem.

**Fixed.** My first idea — publish on the bus the way other subsystems do — is
wrong, and worth recording so nobody tries it again: the queue is written by the
**daemon** and read by the **bridge**, which are separate processes, so an
in-process bus cannot carry it. The filesystem is the only channel available, so
the fix is to watch it. `permissions::watch_queue` turns the poll into an
OS-delivered event, with the interval kept at 30s purely as a safety net (1s if
a watch cannot be established, so a platform without working file events still
delivers prompts).

It watches the directory rather than the file: `queue.json` is replaced by
rename, and a watch pinned to the old inode goes deaf after the first write.

The other `tokio::time::sleep` calls in that crate are retry backoff and
settle delays, not polling loops.

**Fan-out under per-user processes.** With one harness process per member, a
message from A's process must still reach B's client. The seam is the bridge:
today one bridge fans out to all connections because they all translate onto
one daemon socket (`ws.rs:527`). Per-user daemons break that, so the bridge
becomes the join point and must subscribe to every member's daemon rather than
one. That is the specific place a future implementer will be tempted to
substitute a poll, and it must not be.

---

## 1L — workspace ownership, decided

Two options were weighed. **Option 1 chosen**: files owned by whoever creates
them, shared through a setgid group.

**Option 2 (one workspace owned by a service account, Linux users only for
credential scoping) does not actually work.** Credential resolution is
home-relative, so the agent must run *as* the individual to find their
credentials. If it runs as the individual, the files it creates are owned by
the individual, not the service account. Getting service-account ownership
anyway needs a setuid helper or idmapped mounts on every write. That is a lot
of moving parts to make ownership uniform, and uniform ownership was never the
goal, only the means.

Worked through against the criteria asked for:

| Criterion | Option 1 (chosen) | Option 2 |
|---|---|---|
| Ownership churn | Mixed owners, one group. Cosmetic. | Uniform, but only via a helper on every write |
| Git | Works with `core.sharedRepository=group`. Without it, A's objects are unwritable by B. | Uniform, but the helper must also run for git's own writes |
| Commit authorship | From git config, unaffected either way | Same |
| Backup | One tree | One tree |
| User removal | Files must be reassigned or they orphan to a bare uid | Nothing to reassign |
| Blast radius | A compromised session can write the shared tree | Identical: same tree, same group |
| Write queue | Unaffected; the queue keys on repo path | Unaffected |

Option 1's one real cost is orphaned files on user removal, and that is
scripted: `deprovision-member.sh` reassigns them to `root:blaude` and keeps
them group-writable.

Option 2's blast radius is not actually smaller, which is the argument that
usually motivates it. Both models put every member's agent in the same
directory with write access. Uniform ownership hides who wrote what without
preventing anything.

**Reversal note:** if home-relative credential resolution ever stops being the
mechanism, Option 2 becomes viable and the ownership churn argument flips.

---

## 1N — testing

Covered in `auth-investigation.md`, including the 21 construction sites that
stopped the workspace suite compiling and the four failures that were hiding
behind that break.

---

## Known gaps

- **1H (Swarm)** is answered in `auth-investigation.md`, not here: Swarm is
  explicitly optimistic and lock-free (`SWARM_ARCHITECTURE.md:307-310`), so it
  cannot serialise writes. Per-repo serialisation was built instead.
- **Permission-prompt polling** is **fixed**: a filesystem watch replaced the
  900ms poll, since the daemon and bridge are separate processes and the
  filesystem is the only channel between them.
- **Bridge fan-out across per-user daemons** is identified as the seam but not
  implemented. Today's bridge assumes one daemon socket (`ws.rs:527`).
- **`sessions_changed` on the web clients** was a live realtime regression and
  **is fixed** in the TypeScript SDK, along with `write_queued`. The embedded
  web client that also missed it was **deleted** rather than fixed.
- The live audit above could not test cross-user credential reads on the
  production box, because it has one user. That was tested separately with two
  provisioned members and is recorded in the `provision-member.sh` commit.
