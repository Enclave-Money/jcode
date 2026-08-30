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

## 1K–1N — not covered

Not investigated: the team/session layer map (1K), workspace ownership analysis
(1L), the realtime polling audit (1M), or testing infrastructure beyond what
`auth-investigation.md` records (1N).

One 1M-adjacent finding did surface from the test suite and is recorded in
`auth-investigation.md`: `ApiEvent::sessions_changed` reaches Rust clients but
is absent from both the TypeScript SDK and the phone web client
(`phone.html:188-199`), so a teammate's new chat never appears on the phone
until reload. That is a live regression against the realtime bar.
