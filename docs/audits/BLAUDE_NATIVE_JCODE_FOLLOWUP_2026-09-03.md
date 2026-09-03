# Blaude native + jcode follow-up audit

Date: 2026-09-03

Repositories reviewed from these starting points:

- `Enclave-Money/jcode`, working tree based on `origin/master` commit
  `681558e357c5b2f5efd75790f69c6820083dc95e` (the local branch is named
  `feat/api-ws-realtime`, but its base commit is exactly `origin/master`).
- `Enclave-Money/blaude-native`, `main` commit
  `f87f5f04afa863b332ef6232764c57597e227080`.

The follow-up Rust fixes were committed as `34c1520`; dependency remediation
and SDK parity fixes followed as `07350c3`, the Clerk JWT interoperability fix
as `3c154f1`, and the audited crypto-backend selection as `cc919a1`. A final
dependency and dead-code cleanup was committed as `9402d89`. The corresponding
native release commit is `6ed9654`; the website manifest commit is `dc1136d`.
Cloud Run was redeployed from that final jcode state and the native app was
packaged as 0.2.99. The exact production and release acceptance evidence is
recorded at the end of this report.

## Architecture established before changing code

`blaude-native` is the macOS presentation and lifecycle layer. It launches and
talks to the Rust runtime, stores per-team connection details, and renders the
protocol. It does not provision Google Cloud resources itself.

The Rust runtime in `jcode` serves both the local bridge and each remote team.
For team creation, the local runtime authenticates the signed-in user and calls
the Rust `blaude-provision-api` service over HTTPS. Cloud Run performs the GCE
operations with an attached service account. A new team then exposes the same
authenticated websocket protocol the native app uses locally.

The security boundary is therefore:

1. Native app -> local Rust bridge.
2. Local bridge -> Cloud Run provisioning API using a short-lived Clerk
   session JWT.
3. Cloud Run -> GCE using its attached Google service account.
4. Team server -> a narrow Cloud Run directory relay using a signed,
   team-scoped capability.

The Clerk backend key and Google permissions stay in Cloud Run. Neither is
copied to a team VM or required on an end user's Mac.

## Requested-work ledger

| Item | Result from this audit |
|---|---|
| Use only explicitly added AI accounts | Confirmed in code and tests. Ambient provider environment variables are refused; the earlier positive production observation was a turn failing because the fresh team had no configured AI account. |
| Storage cleanup | Historical cleanup cannot be reconstructed from Git. The only large artifact created by this pass, a 782 MiB GitNexus index, was removed after analysis. |
| Rust provisioning API | Confirmed, hardened, rebuilt, and exercised against the live Cloud Run service. The app-side create path does not require a local `gcloud` login. |
| Delete team VMs | Confirmed after the production lifecycle test: no blaude team instance, boot disk, reserved address, or shared firewall rule remained. Cloud Run and its supporting service infrastructure intentionally remain. |
| Rabani's multiplayer reports | Realtime roster/message paths, message badging, explicit no-account guidance, first-run notification permission, and `~/workspace` defaults are present and tested/built. The earlier two-user observation was not repeated with two live identities in this pass. |
| Escape/interrupt | The one-request-per-keypress and cancellation path is covered by code/tests. A live AI turn was not available for a fresh manual interrupt retest because no disposable team was given a user AI account. |
| Thinking collapsed by default | Confirmed in the native state/rendering changes and successful app build. |
| Screen/vault/1Password audit | Confirmed the previously remediated critical paths and completed additional fixes listed below. TOTP stays on the Mac and passes RFC 6238 SHA-1 vectors. |
| Concision/dead code | Removed `brief::refresh_index` plus compiler-confirmed stale desktop actions, settings glue, layout wrappers, constants, and duplicate workspace helpers. Reused the tested Wayland MIME selector in production and removed test-only helpers with no callers. The intentional binary/cdylib module duplication is documented at its include boundary; no speculative graph-based bulk deletion was performed. |
| Claude Code version drift | The 2.1.259 claim and drift tests are present. |

## Additional defects found and fixed

### Authentication and secrets

- Removed the global Clerk backend credential from team VM provisioning.
  Team servers now receive an HMAC-signed capability scoped to their own team
  name and websocket URL and call a narrow directory relay in Cloud Run.
- Hardened Clerk JWT validation: exact HTTPS issuer pinning, required `sub`,
  `iss`, and `sid`, `nbf` validation, RS256-only decoding, and optional
  authorized-party enforcement.
- Made the provisioning email allowlist fail closed. A missing allowlist now
  aborts deployment unless public access is explicitly requested.
- Included untracked files in secret scanning and made the preflight scanner
  compatible with macOS Bash 3.2. Previously, the unavailable `mapfile`
  command could make the scan crash while the wrapper still printed success.
- Removed a secret-shaped fake AWS key literal from a test without weakening
  the redaction assertion.

### Provisioning correctness

- Quoted user-controlled team names before every generated shell command,
  including the generated Git author name.
- Replaced unsupported GCE address `--labels` use with an ownership marker in
  the address description. Deletion validates that marker before releasing an
  address; instance deletion remains label-gated.
- Added rollback coverage so failed creates do not leave a billable instance,
  boot disk, or reserved IP.
- Excluded `.gitnexus`, build output, packages, app assets, and other unrelated
  trees from the Cloud Build source context. The final upload returned to
  26.8 MiB after the audit index had briefly inflated it to about 805 MiB.

### Runtime and native client

- Serialized screen-input operations per room to remove the input/capture
  race identified by the earlier audit.
- Prevented shared rooms from exposing or filling private vault indexes.
- Pinned the downloaded 1Password CLI archive and binary to verified SHA-256
  digests for both supported architectures.
- Kept TOTP seed material in the native process; only generated codes cross
  the runtime boundary.
- Corrected team-token storage tests and comments to match reality:
  per-team tokens currently use `UserDefaults`, not Keychain.
- Updated `h2` from 0.4.13 to 0.4.16 to resolve
  `RUSTSEC-2026-0258` (unbounded empty DATA frame handling).
- Removed the unmaintained `rustls-pemfile` dependency in favor of rustls'
  maintained `rustls-pki-types` PEM API, and updated `anyhow` and
  `event-listener` to releases that resolve their RustSec unsoundness warnings.

### Dependency and protocol follow-up

- Reconciled all ten open GitHub Dependabot alerts. Patched versions are
  `jsonwebtoken` 10.4.0, `tar` 0.4.46, `rand` 0.8.6, `cmov` 0.5.4,
  `fast-uri` 3.1.7, and Playwright 1.55.1. GitHub closed every alert from the
  updated dependency graph; none was dismissed.
- Corrected a real TypeScript SDK schema drift found by its parity tests. The
  stable public SDK no longer advertises private native bridge requests, and
  its event/request tag lists now exactly match the Rust public API enums.
- The first post-upgrade production lifecycle test found that jsonwebtoken's
  generic header parser rejects Clerk's numeric custom JOSE header. The API
  now parses only the understood `alg`, `kid`, and `crit` fields, ignores
  unknown non-critical fields, and delegates RS256 verification to
  jsonwebtoken's explicitly selected AWS-LC backend. It independently enforces
  issuer, expiry, not-before, session identity, and authorized-party checks. A
  Clerk-shaped regression test covers the exact numeric-header case. The
  RustCrypto backend was tested and rejected because its RSA implementation
  carries the unfixed `RUSTSEC-2023-0071` timing advisory.

## Verification evidence

- Full Rust workspace run during this pass: 1,349 passed, 29 ignored.
- Post-cleanup affected-package matrix: 4,129 passed, 124 ignored across
  desktop (library and binary targets), TUI, setup hints, telemetry, transport,
  provider runtime, harness, and provisioning. This includes a redirected
  ANSI wire test with `NO_COLOR=1`, so its color assertions no longer depend
  on the invoking shell.
- Focused post-hardening Rust run: 156 passed, 1 ignored across
  `blaude-provision`, `blaude-provision-api`, and the harness.
- Fast Rust suite: 238 passed.
- Strict all-target Clippy with warnings denied across desktop, TUI, SDK,
  math, setup hints, telemetry, transport, provider runtime, harness, and both
  provisioning crates: passed.
- `cargo fmt --all -- --check`, `git diff --check`, and deployment/preflight
  shell syntax checks: passed.
- Security preflight with `cargo-audit` 0.22.2: passed with no untriaged
  vulnerabilities. The remaining maintenance/unsoundness warnings are tracked
  in `docs/SECURITY_DEPENDENCIES.md`.
- TypeScript SDK: build plus 40 tests passed; browser helper: 19 tests passed.
  `npm audit --audit-level=low` reports zero vulnerabilities in both trees.
- GitHub Dependabot: ten open alerts before reconciliation, zero afterward.
- BlaudeKit: 48 passed, 0 failed, including RFC 6238 TOTP vectors, transport
  lifecycle, wire snapshots, per-team token migration, and workspace state.
- macOS Debug app build: succeeded. The only output was an SDK metadata
  warning, not a compiler or linker failure.
- Rust/Swift wire protocol parity check: exact match.
- Live production lifecycle before the final dependency-only rebuild:
  authenticated create reached ready, TLS websocket and protocol hello
  succeeded, a real 64,797-byte desktop JPEG arrived, authenticated delete
  succeeded, and the final GCE scan was empty.

## Remaining risk and unfinished validation

1. Provisioning job state and background tasks live in one Cloud Run process.
   `max-instances=1` and no CPU throttling make the present design work, but a
   process restart can still lose a job record and leave resources requiring
   reconciliation. A durable queue/job store is the appropriate next
   infrastructure step.
2. Directory relay capabilities are long-lived. They cannot grant access to a
   different team or reveal the Clerk key, but a compromised team can continue
   inviting or stamping users into itself until its VM is removed or the
   signing key is rotated. Expiry/revocation and rate limiting remain useful
   defense-in-depth work.
3. Team tokens remain in `UserDefaults` while the development app is ad-hoc
   signed. Another process running as the same macOS user can read them. Move
   them to Keychain after stable Developer ID signing is established.
4. This pass did not repeat a live interrupt during an AI turn or the realtime
   flow with two separately signed-in humans. Those are release-level manual
   checks, not claims established by the automated suite.
5. RustSec reports six non-failing maintenance, unsoundness, or yanked
   dependency warnings after this pass removed `rustls-pemfile` and updated
   `anyhow` and `event-listener` to fixed releases. The six remaining paths
   require upstream ecosystem migrations and are tracked with their
   remediation order in `docs/SECURITY_DEPENDENCIES.md`.
6. Release 0.2.99 is ad-hoc signed because this Mac has no Developer ID
   Application certificate or notarization identity. Its bytes and embedded
   signatures were verified, but users will still receive the macOS Gatekeeper
   warning until Developer ID signing is configured.

## Production verification

- Revision `blaude-provision-api-00011-r8s`, image digest
  `sha256:d5aaec6e5d55303ee119f1f7fa7c2646ec080d94f8e7efe3a402f583c04c2991`,
  was the first fully accepted hardened revision. It was subsequently
  superseded by the post-cleanup deployment below.
- `/v1/health` returned HTTP 200 with the expected service response.
- An unauthenticated, structurally valid create returned HTTP 401 with
  `no sign-in was presented`.
- The full disposable lifecycle passed on this exact revision: authenticated
  create, two session-token renewals during provisioning, TLS/websocket
  authentication, protocol hello, a real 64,779-byte desktop JPEG,
  authenticated delete, and delete confirmation.
- Revision 00011 had no severity-ERROR Cloud Run log entries after the test.
- Independent GCE scans found no blaude instance, disk, or reserved address.
  The shared `blaude-team-web` firewall rule created during the test was
  deleted and its absence was confirmed.

### Post-cleanup revision 00012

- Revision `blaude-provision-api-00012-hh7`, image digest
  `sha256:25119a89fd4e963fca48e8e721c73002c12974ec816303f115b320171f7cc813`,
  was built from jcode commit `9402d89` and serves 100% of Cloud Run traffic.
- `/v1/health` returned HTTP 200. A structurally valid unauthenticated create
  returned the expected HTTP 401 with `no sign-in was presented`.
- The full application path passed on this exact revision: the signed-in local
  runtime started and polled a disposable create job through the provisioning
  API, the job reached ready, public TLS and authenticated websocket hello
  succeeded, and a real 64,754-byte desktop frame arrived.
- Delete through the same local-runtime/API path completed successfully.
  Independent post-delete scans found no managed instance, disk, reserved
  address, or acceptance IP, and the shared firewall rule was removed.
- Revision 00012 had no severity-ERROR Cloud Run log entries after the full
  lifecycle acceptance test.

## Final source and native release verification

- `Enclave-Money/jcode`: production and embedded-runtime code commit `9402d89`;
  both `master` and `feat/api-ws-realtime` contain it and are advanced together
  with this docs-only evidence update.
- `Enclave-Money/blaude-native`: release commit `6ed9654`.
- `Enclave-Money/blaude-website`: release manifest commit `dc1136d`.
- Public artifact:
  `https://b3ujehubdmouneya.public.blob.vercel-storage.com/dmg/blaude-0.2.99-arm64.dmg`.
- Artifact length: 57,674,304 bytes; SHA-256:
  `89fd34043527428cd72199442932e15a4cb9a37b0cb38fd615751147abe2b43a`.
- The public artifact was downloaded again and compared byte-for-byte with the
  locally accepted DMG.
- The accepted DMG mounted successfully, reported bundle version 0.2.99,
  passed deep signature verification, contained a universal x86_64/arm64 app
  executable with arm64 helpers, and embedded the clean-provenance runtime
  `blaude v0.77.1-dev (9402d89)`.
- The production release API reports 0.2.99 with the exact URL, byte length,
  and digest above; `/download` redirects to that artifact. The explicit
  production deployment is ready and aliased at
  `https://blaude-website.vercel.app`.

## Post-audit stale-team UX follow-up

On 2026-09-04, a GCE-deleted Banubear server exposed a native cleanup gap. The
VM was absent, but its URL remained in the app's `BlaudeSavedTeamsJSON`, and
the Environments sheet hid both recovery actions while that row was active.
Reinstalling the bundle correctly preserved those preferences, so it could not
clear the dead entry by itself.

- `Enclave-Money/blaude-native` commit `14be75e` makes Forget and Delete
  available on the active team row. Forget safely switches to This Mac, removes
  URL-scoped and legacy credentials, clears the legacy fallback URL, and marks
  the exact claimed invitation ticket declined so stale Clerk metadata cannot
  attach the same dead server again. A genuinely new ticket remains eligible.
- Delete continues to use the authenticated provisioning API. Its existing
  idempotent `already_gone` result now provides a reachable reconciliation path
  for servers removed outside the app, after which the same local cleanup runs.
- The full macOS target built successfully and all 48 BlaudeKit tests passed.
  A debug-only QA command reproduced Banubear as the active saved team, invoked
  the exact Forget store path, and confirmed `runtimeLabel = Local`, team mode
  false, an empty saved-team list, and absence of both current and legacy team
  URL keys.
- Release 0.2.100 was mounted and verified: deep signature validation passed,
  the app executable is universal x86_64/arm64, helpers are arm64, and the
  embedded runtime reports `blaude v0.77.1-dev (9402d89)`.
- `Enclave-Money/blaude-website` commit `33071cc` publishes the 57,677,124-byte
  artifact with SHA-256
  `df06a10195bfef582c789f6f2713525f98d6a74bda58bf2635f37f6672ddcecf`.
  Production deployment `dpl_Ec3JqM7AS1gHnYTVd5WWWKcDttKL` is ready;
  `/api/releases/latest` and `/download` both resolve to 0.2.100.
- The production 0.2.100 bundle is installed and running locally. Banubear is
  absent from the saved environment list and the app is back on This Mac.
