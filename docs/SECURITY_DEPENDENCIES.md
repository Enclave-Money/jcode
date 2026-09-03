# Dependency Security Triage

Last reviewed: 2026-09-03

This file tracks the current `cargo audit` findings for blaude and the intended remediation path.
It is not an allowlist. It is a triage record so advisories are visible and actionable.

## Current advisories

| Advisory | Crate | Dependency path | Affected area in blaude | Triage | Planned action |
|---|---|---|---|---|---|
| `RUSTSEC-2025-0141` | `bincode` | `syntect -> bincode` | Markdown/code highlighting in the TUI | Unmaintained transitive dependency. No direct exposure in the provider/auth flow. | Track `syntect` upgrades or replace `syntect` if upstream does not move off `bincode` soon. |
| `RUSTSEC-2024-0436` | `paste` | `ratatui -> paste`, `tokenizers -> paste`, `tract-* -> paste` | TUI rendering, tokenizers, embedding/model support | Widely transitive. Not isolated to one module. | Prefer upstream dependency upgrades before any local workaround. Re-evaluate after bumping `ratatui`, `tokenizers`, and `tract-*`. |
| `RUSTSEC-2026-0253` | `lru` | `ratatui -> lru` | TUI rendering/cache internals | Potential use-after-free when `LruCache::pop()` panics. Not in auth/provider logic, but still ships in-process. | Upgrade `ratatui` / `ratatui-image` together once compatible. |
| `RUSTSEC-2026-0141` | `lettre` | `jcode-notify-email -> lettre` | Notification email sending | Vulnerability applies to the Boring TLS backend hostname verification path. blaude's `lettre` dependency uses rustls/native-tls features, not `boring-tls`, so this is not believed exploitable in the current build. | Keep ignored in `scripts/security_preflight.sh`; remove ignore after `lettre` ships a patched release or if feature use changes. |
| `RUSTSEC-2026-0098` | `rustls-webpki` | `rustls` dependency stack | TLS certificate validation in rustls consumers | Name constraints for URI names incorrectly accepted. Transitive via TLS libraries. | Upgrade rustls/webpki stack when compatible releases are available. |
| `RUSTSEC-2026-0099` | `rustls-webpki` | `rustls` dependency stack | TLS certificate validation in rustls consumers | Name constraints accepted for wildcard certificates. Transitive via TLS libraries. | Upgrade rustls/webpki stack when compatible releases are available. |
| `RUSTSEC-2026-0104` | `rustls-webpki` | `rustls` dependency stack | TLS certificate revocation list parsing | Reachable panic in CRL parsing. Transitive via TLS libraries. | Upgrade rustls/webpki stack when compatible releases are available. |
| `RUSTSEC-2026-0049` | `rustls-webpki` | `rustls` dependency stack (`aws-smithy` rustls 0.21, `imap`/`rustls-connector` rustls 0.22) | TLS certificate revocation list handling | CRLs not considered authoritative by Distribution Point due to faulty matching logic. Transitive via the older rustls stacks; fix needs rustls-webpki >=0.103.10, which requires major bumps of the `aws-sdk`/`imap` stacks. | Upgrade rustls/webpki stack when compatible releases are available. |
| `RUSTSEC-2026-0187` | `lopdf` | `jcode-pdf -> pdf-extract 0.8.2 -> lopdf 0.34` | PDF text extraction (`/pdf`, image/PDF reads) | Stack overflow parsing deeply nested PDF objects. Only reached when extracting text from a (potentially malicious) PDF the user opens; not in the auth/provider/network path. `pdf-extract 0.8.2` pins `lopdf 0.34`, so it cannot be bumped to the fixed `>=0.42` without an upstream `pdf-extract` release. | Upgrade once `pdf-extract` ships a release depending on `lopdf >=0.42`; remove the ignore then. |
| `RUSTSEC-2025-0134` | `rustls-pemfile` | TLS dependency stack | PEM parsing at TLS setup boundaries | Unmaintained warning, not a reported vulnerability. | Move to `rustls-pki-types` PEM APIs as direct dependants permit. |
| `RUSTSEC-2026-0206` | `rustybuzz` | `resvg` / TUI image stack | SVG/text rendering | Unmaintained warning in presentation code. | Upgrade the SVG rendering stack together. |
| `RUSTSEC-2026-0192` | `ttf-parser` | font and SVG rendering stack | Font parsing/rendering | Unmaintained warning in presentation code. | Upgrade the font/rendering stack together. |
| `RUSTSEC-2026-0190` | `anyhow` 1.0.100 | broad direct dependency | Error downcasting | Unsound `Error::downcast_mut()` warning. The affected API is not intentionally used, but the crate is pervasive. | Upgrade `anyhow` after verifying the workspace's MSRV and full test matrix. |
| `RUSTSEC-2026-0221` | `event-listener` 5.4.1 | async dependency stack | async synchronization | Unsoundness warning involving custom `!Send` tags. | Upgrade the async dependency stack when a compatible fixed release resolves in the lockfile. |
| yanked | `chacha20` 0.10.0 | transitive cryptography dependency | cryptographic primitive dependency | The locked release is yanked; RustSec does not classify this as a vulnerability. | Follow the upstream dependency that selects it and move to a non-yanked release. |

## Priority order

1. `rustls-webpki` TLS advisories via rustls stack
2. `anyhow` and `event-listener` unsoundness warnings because they are broad in the graph
3. `lettre` if blaude ever enables `boring-tls`
4. `lru` via `ratatui`
5. `bincode`, `rustls-pemfile`, `rustybuzz`, and `ttf-parser` maintenance migrations
6. `paste` and the yanked `chacha20` via multiple transitive dependencies

## Notes

- None of the advisories above were introduced by the provider-auth refactor.
- `RUSTSEC-2026-0258` (`h2` unbounded empty DATA frames) was found by the
  2026-09-03 audit and resolved by updating `h2` from 0.4.13 to 0.4.16.
- GitHub Dependabot reconciliation on 2026-09-03 updated `jsonwebtoken` to
  10.4.0, `tar` to 0.4.46, `rand` to 0.8.6, and `cmov` to 0.5.4. This removed
  the open Rust dependency alerts, including `RUSTSEC-2026-0097` for `rand`.
- The same reconciliation updated the TypeScript SDK's `fast-uri` to 3.1.7
  and the browser helper's Playwright to 1.55.1. `npm audit` reports zero
  vulnerabilities in both package trees.
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
