# Dependency Security Triage

Last reviewed: 2026-09-03

This file tracks the current `cargo audit` findings for blaude and the intended remediation path.
It is not an allowlist. It is a triage record so advisories are visible and actionable.

## Current advisories

| Advisory | Crate | Dependency path | Affected area in blaude | Triage | Planned action |
|---|---|---|---|---|---|
| `RUSTSEC-2025-0141` | `bincode` | `syntect -> bincode` | Markdown/code highlighting in the TUI | Unmaintained transitive dependency. No direct exposure in the provider/auth flow. | Track `syntect` upgrades or replace `syntect` if upstream does not move off `bincode` soon. |
| `RUSTSEC-2024-0436` | `paste` | `tokenizers -> macro_rules_attribute -> paste` | Embedding/tokenizer support | Unmaintained transitive proc macro. | Upgrade or replace `tokenizers` when its dependency graph moves off `paste`. |
| `RUSTSEC-2026-0253` | `lru` 0.16.4 | `ratatui 0.30 -> ratatui-core 0.1 -> lru` | TUI rendering/cache internals | Potential use-after-free when `LruCache::pop()` panics. Not in auth/provider logic, but still ships in-process. The fixed `lru >=0.18.2` is outside `ratatui-core`'s current `^0.16` constraint. | Upgrade `ratatui` / `ratatui-image` together when the upstream graph permits `lru >=0.18.2`. |
| `RUSTSEC-2026-0141` | `lettre` | `jcode-notify-email -> lettre` | Notification email sending | Vulnerability applies to the Boring TLS backend hostname verification path. blaude's `lettre` dependency uses rustls/native-tls features, not `boring-tls`, so this is not believed exploitable in the current build. | Keep ignored in `scripts/security_preflight.sh`; remove ignore after `lettre` ships a patched release or if feature use changes. |
| `RUSTSEC-2026-0098` | `rustls-webpki` | `rustls` dependency stack | TLS certificate validation in rustls consumers | Name constraints for URI names incorrectly accepted. Transitive via TLS libraries. | Upgrade rustls/webpki stack when compatible releases are available. |
| `RUSTSEC-2026-0099` | `rustls-webpki` | `rustls` dependency stack | TLS certificate validation in rustls consumers | Name constraints accepted for wildcard certificates. Transitive via TLS libraries. | Upgrade rustls/webpki stack when compatible releases are available. |
| `RUSTSEC-2026-0104` | `rustls-webpki` | `rustls` dependency stack | TLS certificate revocation list parsing | Reachable panic in CRL parsing. Transitive via TLS libraries. | Upgrade rustls/webpki stack when compatible releases are available. |
| `RUSTSEC-2026-0049` | `rustls-webpki` | `rustls` dependency stack (`aws-smithy` rustls 0.21, `imap`/`rustls-connector` rustls 0.22) | TLS certificate revocation list handling | CRLs not considered authoritative by Distribution Point due to faulty matching logic. Transitive via the older rustls stacks; fix needs rustls-webpki >=0.103.10, which requires major bumps of the `aws-sdk`/`imap` stacks. | Upgrade rustls/webpki stack when compatible releases are available. |
| `RUSTSEC-2026-0206` | `rustybuzz` | `resvg` / TUI image stack | SVG/text rendering | Unmaintained warning in presentation code. | Upgrade the SVG rendering stack together. |
| `RUSTSEC-2026-0192` | `ttf-parser` | `fontdb`, `usvg` / `rustybuzz`, and `lopdf` | Font parsing/rendering and PDF extraction | Unmaintained warning across the rendering stack. | Upgrade or replace the font, SVG, and PDF rendering stack together. |
| yanked | `chacha20` 0.10.0 | `rand 0.10.2`, selected by `lopdf`, `tract`, and Azure SDK dependencies | cryptographic primitive dependency | The locked release is yanked; RustSec does not classify this as a vulnerability. The latest compatible `rand` still selects it. | Follow the upstream `rand` dependency and move as soon as it selects a non-yanked release. |

## Priority order

1. `rustls-webpki` TLS advisories via rustls stack
2. `lettre` if blaude ever enables `boring-tls`
3. `lru` via `ratatui`
4. `bincode`, `rustybuzz`, and `ttf-parser` maintenance migrations
5. `paste` and the yanked `chacha20` via transitive dependencies

## Notes

- None of the advisories above were introduced by the provider-auth refactor.
- `RUSTSEC-2025-0134` (`rustls-pemfile`) was removed from the TLS setup path on
  2026-09-03. The harness now uses rustls' maintained `rustls-pki-types` PEM
  APIs directly, with the existing end-to-end WSS test covering certificate
  and private-key loading.
- `RUSTSEC-2026-0190` (`anyhow`) and `RUSTSEC-2026-0221`
  (`event-listener`) were resolved by updating to 1.0.104 and 5.4.2
  respectively. The compatible lockfile refresh also moved `rand` to 0.10.2;
  that release still selects the yanked `chacha20` 0.10.0.
- `RUSTSEC-2026-0187` (`lopdf`) was resolved by the earlier move to
  `pdf-extract` 0.12.0 / `lopdf` 0.42.0; the stale open row was removed during
  this reconciliation.
- `RUSTSEC-2026-0258` (`h2` unbounded empty DATA frames) was found by the
  2026-09-03 audit and resolved by updating `h2` from 0.4.13 to 0.4.16.
- GitHub Dependabot reconciliation on 2026-09-03 updated `jsonwebtoken` to
  10.4.0, `tar` to 0.4.46, `rand` to 0.8.6, and `cmov` to 0.5.4. This removed
  the open Rust dependency alerts, including `RUSTSEC-2026-0097` for `rand`.
- The same reconciliation updated the TypeScript SDK's `fast-uri` to 3.1.7
  and the browser helper's Playwright to 1.55.1. `npm audit` reports zero
  vulnerabilities in both package trees.
- The jsonwebtoken 10.4 upgrade exposed an interoperability defect in its
  generic JOSE header parser: Clerk's native session token includes a numeric
  custom header, while the crate assumes every unknown value is a string. The
  provisioning API now parses only `alg`, `kid`, and `crit`, ignores unknown
  non-critical headers as JOSE specifies, and still delegates RS256 signature
  verification to jsonwebtoken with its AWS-LC backend selected explicitly.
  The RustCrypto backend was rejected because it introduces the unfixed
  `RUSTSEC-2023-0071` RSA timing advisory. Clerk-shaped numeric-header and
  live-verifier regression tests protect the production path.
- `RUSTSEC-2023-0086` (`lexical-core`) is no longer present in the lockfile as
  of the 2026-09-03 audit.
- The provider/auth hardening work should continue independently of these dependency upgrades.
- `RUSTSEC-2026-0217` (`tract-nnef` 0.21.10, integer overflow in the NNEF tensor
  parser) was resolved on 2026-07-30 by moving `jcode-embedding` to `tract` 0.23.
  The in-line `0.21.16` fix was unreachable: `tract-data 0.21.16` pins
  `half =2.4.1` while `naga` (via `vello` in `jcode-desktop2`) requires
  `half ^2.5`. The 0.23 line drops that pin. This mattered because the parser
  runs over a model downloaded at runtime rather than one shipped in the binary,
  so an ignore would not have been clearly safe. See #657.
- `RUSTSEC-2024-0320` (`yaml-rust`) was removed from the dependency graph on 2026-03-05 by trimming `syntect` features to built-in syntax/theme dumps instead of YAML loading.
- `RUSTSEC-2026-0194` / `RUSTSEC-2026-0195` (`quick-xml` 0.39.2): reached only through `wayland-scanner`, a build-time proc-macro in the desktop crate's winit stack. It parses trusted, vendored Wayland protocol XML during compilation and never touches untrusted input at runtime. Remediation is upstream: `wayland-scanner` needs to move to `quick-xml >= 0.41`. Triaged and ignored in `scripts/security_preflight.sh` on 2026-07-04.
- `scripts/security_preflight.sh` ignores the vulnerability advisories that are explicitly triaged above (`lettre` and `rustls-webpki`) so CI can remain actionable. New vulnerabilities still fail CI by default.
- Before changing dependency versions, run:
  - `cargo check`
  - `cargo test -j 1`
  - `scripts/security_preflight.sh`
