# Runner mode — design

Design only. Nothing here is implemented.

Apple builds run on a developer's own Mac. Blaude.app is already installed
there with Xcode and signing certificates configured, which makes that Mac the
obvious host for build and test jobs. This document works through whether it
should be, and what would have to be true.

**The short version:** a runner is worth building for simulator and test jobs,
as a separate opt-in helper binary. Signed device builds of *other people's*
code cannot be made safe on a personal Mac, and the design says so rather than
pretending otherwise. See "The honest answer" below.

---

## 1. The architectural tension

`blaude-native` is a pure frontend. A build runner is a long-lived local
process that accepts work from a server and executes it. That is a local
backend by any reading.

### The rule already has exactly one exception

`BlaudeKit/Sources/BlaudeKit/Runtime/LoopbackCallback.swift` binds a local
listener today. It exists for the OAuth loopback relay: the app opens a port on
its own machine, the provider redirects the browser to it, the app relays the
code onward (`crates/jcode-harness-api-server/src/login_jobs.rs:1-16`).

That exception has a shape worth naming, because it is what makes it
acceptable:

- **short-lived** — it exists for one sign-in and closes
- **user-initiated** — it opens because the user pressed Sign in
- **single-purpose** — it accepts one kind of payload and does one thing
- **accepts nothing from other people** — only the user's own browser reaches it

A build runner inverts every one of those. It is long-lived, idle-waiting,
general-purpose, and its entire job is to accept and execute work submitted by
*other people*. It is not the same kind of thing, and "the app already listens
on a port" is not a precedent that covers it.

### Options

**Option A: an explicit exception in blaude-native.** Ship the runner inside
the app. Cheapest to build; the app already has the Xcode environment and the
team connection. But it makes "pure frontend" false in a way that will not stay
contained: once the app has a job executor, the next thing that needs a local
process goes there too, and the rule that keeps this codebase honest is gone.
Also ties the runner's lifecycle to a GUI app the user may quit.

**Option B: a separate helper binary shipped alongside the app.** The app stays
a frontend; a sibling executable does the work, run as a LaunchAgent, opt-in.

There is already a precedent for exactly this packaging: `blaude-tools` sits
next to the `blaude` binary in the app bundle and is invoked as a sibling
(`src/cli/dispatch.rs:1602-1615`). The bundle is already a place where helper
executables live.

**Option C: put it in `blaude-tools`, built from `~/workspace/blaude`.** Reuses
the existing sibling binary and its distribution. But `blaude-tools` is a
grab-bag utility invoked synchronously for short tasks, and its build source is
the *old terminal app* repo, which is kept alive mainly because it still builds
this binary. Adding a long-lived networked daemon to it puts new, security
sensitive, long-lived code in the least-maintained of the four repos.

### Recommendation: Option B

A separate helper binary, `blaude-runner`, shipped in the app bundle beside
`blaude` and `blaude-tools`, run as an opt-in LaunchAgent.

Reasoning, in order of weight:

1. It keeps the frontend rule true. The rule's value is that it stops business
   logic accreting in the app; an exception for a job executor is the exact
   exception that would kill it.
2. Its lifecycle is independent of the GUI. A build should not die because
   someone quit the app, and the app should not have to stay open to be a good
   citizen of the team.
3. It fails separately. A crashing runner takes down builds, not the user's
   editor.
4. It can be sandboxed as a unit. A helper process can be given a restrictive
   profile and its own working root; the GUI app cannot, because it legitimately
   needs broad access.
5. The packaging already exists. Distribution, signing, and notarisation of a
   sibling binary in the bundle is a solved problem here.

The cost is a second updater path and a second thing to notarise. That is real
but bounded, and it is the price of not making the frontend a backend.

---

## 2. The runner

### Registration

The runner authenticates to the team server as its owner, using the same
member identity the app already holds. It is not a separate principal: a
runner is "this person's Mac", and jobs it accepts are billed and attributed to
that person's team membership.

It registers over the existing websocket rather than a new channel, which keeps
the realtime bar intact and avoids a second auth surface. This needs new harness
verbs; **none of this exists today** — there is no runner or worker concept
anywhere in `jcode-harness-api` or `jcode-harness-api-server`.

```
runner_register  { runner_id, owner, capabilities, labels }   -> runner_registered
runner_heartbeat { runner_id, state, current_job_id? }        -> ack (with lease)
job_offer        { job_id, kind, repo_ref, requirements }     (server -> runner)
job_accept       { job_id, runner_id }                        -> job_lease
job_progress     { job_id, seq, stream, chunk }               (runner -> server)
job_result       { job_id, status, artefacts[], summary }
job_release      { job_id, reason }
runner_unregister{ runner_id }
```

### Advertised capabilities

Gathered once at start and re-gathered when Xcode changes:

| Capability | Source |
|---|---|
| Xcode version and build | `xcodebuild -version` |
| SDKs | `xcodebuild -showsdks` |
| Simulators | `xcrun simctl list devices available --json` |
| Signing identities | `security find-identity -v -p codesigning` |
| Provisioning profiles | `~/Library/MobileDevice/Provisioning Profiles` |
| Hardware | arch, core count, free disk |

**Signing identities are advertised as presence, never as content.** The server
learns "this runner can sign with a Developer ID" and the team name, never the
certificate or key. Job matching needs the former; nothing needs the latter.

### Job kinds

The split matters for the security section, so it is part of the model rather
than a deployment detail:

- `simulator-build` — build for a simulator SDK. No signing identity needed.
- `test` — `xcodebuild test` against a simulator. No signing identity needed.
- `archive-signed` — a real signed build. **Requires the owner's identity.**

### Opt-in and visibility

Off by default. Turning it on is a deliberate act in the app's settings, with
the honest sentence attached: turning this on runs other people's code on this
Mac. While enabled, a menu-bar item shows the current state and the running
job, and one click pauses the runner after the current job or kills it now.
`launchctl unload` is always the hard off switch and is documented as such.

---

## 3. Sandboxing: what can and cannot be guaranteed

### The honest answer, first

**An Xcode build is arbitrary code execution by design.** Build phases run
shell scripts. Swift Package Manager plugins and macros execute at build time.
A test target runs whatever the test code says. There is no "just build it"
mode that is not also "run their code".

So the question is never "can a build be made inert" — it cannot. It is only
"what can that code reach".

And for signed builds there is a second, harder truth. Signing requires
`codesign` to use a private key in the login keychain. Any process that can
invoke `codesign` with your identity can sign arbitrary code as you. You can
narrow *what* it signs; you cannot let it sign at all without granting the
capability to sign. Keychain ACLs prompt once and are then commonly set to
"always allow" for `codesign`, at which point the gate is gone.

**Therefore: `archive-signed` jobs containing another person's code cannot be
made safe on your personal Mac, and this design does not claim they can.** The
options are to run only your own signed builds, or to move signed builds to a
dedicated machine or VM holding a dedicated certificate whose compromise costs
you a revocation rather than your identity.

Simulator and test jobs are a different matter: they need no signing identity,
so they can be confined meaningfully.

### The mechanisms, assessed

**`sandbox-exec` (SBPL).** Deprecated by Apple since 10.10 and still shipping
and still used by major applications. Kernel-enforced, per-process, allows a
deny-by-default profile with a writable working root. In practice an Xcode
build touches enough of the system (toolchains, caches, `DerivedData`,
simulator runtime, `xcodebuild`'s own IPC) that a workable profile is
permissive on reads and narrow on writes. That asymmetry is the realistic
target: **stop the job writing outside its root, accept that it can read much
of the system.** Being deprecated, it can break in a macOS release, so it
cannot be the only layer.

**A per-job Unix user.** Real, kernel-enforced, well-understood: a separate
`_blauderunner` account cannot read your home directory. This is the strongest
practical layer and it composes with the sandbox profile. Two costs: Xcode's
first run per user needs its own setup, and — the important one — that user
cannot reach your signing keychain, which is the whole reason `archive-signed`
cannot be served this way. That is a feature for the two safe job kinds and a
hard stop for the third.

**Containers.** Not available. macOS cannot containerise macOS; Docker on a Mac
runs a Linux VM and cannot run Xcode. This option does not exist and should not
appear in a plan.

**Virtual machines.** `Virtualization.framework` runs macOS guests on Apple
silicon, licence-limited to two guest VMs per host. This is the only mechanism
that genuinely isolates a signed build, because the guest can hold its own
certificate. Costs are real: tens of GB per image, Xcode installed per image,
slow cold start, and the guest still needs credentials provisioned into it.

### Recommended posture

| Job kind | Confinement | Honest guarantee |
|---|---|---|
| `simulator-build`, `test` | dedicated Unix user + `sandbox-exec` profile + working root on a separate volume or directory | Cannot read your home or write outside its root. Can read much of the system. Can use network unless denied. |
| `archive-signed` (own code) | same, plus keychain access to a signing identity | Your code, your key. Fine. |
| `archive-signed` (others' code) | **refuse** | Cannot be made safe here. Route to a dedicated host or VM with its own certificate. |

Network egress should default to denied for `test` jobs and be an explicit
per-job grant, because "the tests need the network" and "the tests exfiltrate
your files" are the same capability.

---

## 4. Sleep, offline, and the job state machine

The Mac will sleep, close its lid, and lose network mid-build. The design has
to assume that is normal, not exceptional.

### States

```
queued ──offer──> offered ──accept──> leased ──start──> running
                     │                   │                 │
                     │ (no accept)       │ (lease expiry)  ├── uploading ──> succeeded
                     v                   v                 │
                  queued              queued               ├──> failed      (terminal)
                                                           └──> abandoned ──> queued
```

- **queued** — server holds it; no runner owns it.
- **offered** — sent to a runner; short fuse (~10s) then re-queued.
- **leased** — runner owns it under a lease with an expiry.
- **running** — executing; the runner streams output and renews the lease by
  heartbeat.
- **uploading** — build finished, artefacts transferring.
- **succeeded / failed** — terminal; `failed` includes compile errors, which are
  a legitimate result and must not be retried.
- **abandoned** — lease expired without a result; re-queued, attempt count
  incremented.

### Leases, not timeouts, distinguish dead from slow

A slow runner and a dead runner look identical if you measure job duration, and
build durations vary by an order of magnitude. So the server must never infer
death from elapsed job time.

- The runner heartbeats every 10s. The lease is 60s and each heartbeat renews it.
- Heartbeats continue during long silent compiles, so a 40-minute build with no
  output is plainly alive.
- Lease expiry, and only lease expiry, means dead. The job goes `abandoned` and
  back to `queued`.
- Heartbeats ride the existing websocket. A dropped socket is not itself death:
  the lease survives a reconnect within its window, which covers a Wi-Fi hop or
  a network change.

### Sleep

- Hold a power assertion for the duration of a job (`caffeinate -i`
  equivalently via `IOPMAssertion`). This prevents idle sleep.
- **It does not prevent lid-close sleep on battery.** Nothing does. So that case
  must be handled, not prevented.
- On `NSWorkspace.willSleepNotification`, the runner sends `job_release` with
  reason `sleeping` so the server can re-queue immediately rather than waiting
  out a 60s lease.
- On wake, the runner re-registers and reconciles: any job it still believes it
  owns is checked against the server, and if the server re-queued it, the local
  work is killed rather than allowed to finish and report late.

### Checkpointing, honestly

**An Xcode build is not resumable mid-flight.** There is no checkpoint to take;
a job interrupted at 90% restarts from the beginning.

What can be preserved is *incremental state*: a per-repo persistent
`DerivedData` makes the retry much cheaper than a cold build. That is a
performance mitigation, not a correctness one, and the design should not
describe it as resumption.

Consequently:

- Jobs must be **idempotent**, because a re-queued job may run twice: once on a
  runner that vanished and later reported, and once on its replacement.
- Results are accepted only from the runner that currently holds the lease. A
  late result from a superseded lease is logged and discarded.
- Output is streamed with a monotonic `seq` per job so a partial log from an
  abandoned attempt can be discarded cleanly on retry.

### Retry policy

- `abandoned` retries up to 3 times, then goes `failed` with "no runner
  completed this job".
- `failed` from a non-zero build exit does **not** retry. A compile error is an
  answer.
- Infrastructure failures (runner disk full, Xcode missing, simulator failed to
  boot) are a distinct `failed_infrastructure` and re-queue to a *different*
  runner, so one broken Mac cannot fail the team's jobs repeatedly.

---

## 5. What blaude-native does here

Almost nothing, which is the point. The client:

- shows the runner's state and the current job in the menu bar
- offers the opt-in toggle and the two off switches (pause after current, kill
  now)
- surfaces job results and logs like any other server-pushed event

The runner itself is a separate process that talks to the harness directly. If
the app is closed, builds keep running.

---

## 6. Open questions for the owner

1. Do you want `archive-signed` for other people's code at all? If yes, that is
   a dedicated-host or VM project, not a runner-on-your-Mac project, and should
   be scoped separately.
2. Should a runner accept jobs from anyone on the team, or only from an
   allowlist? The security posture above is "other people's code runs here", and
   an allowlist is the cheapest meaningful control.
3. Network egress default for `test` jobs: denied with an explicit per-job
   grant is the safe default, but it will break any test suite that hits the
   network, which is common. Your call on which side to err.

**Stop here.** Do not implement until this is read.
