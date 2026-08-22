# jcode-tui test flakiness: root causes and fixes

`cargo test -p jcode-tui --lib` historically failed 1-4 tests per run at the
default (parallel) thread count, with a varying set, while
`-- --test-threads=1` passed every test. This is a set of parallelism races on
**process-global state**, not logic bugs — each failing test passes in
isolation.

This document records the root causes found by looping the suite under load and
attributing each failure to the global it raced on, and the fixes applied. The
guiding principle throughout: **stop sharing the state across tests** (make it
per-test), or remove the wall-clock/order dependence, rather than adding another
lock.

## The reentrant serialization lock (pre-existing baseline)

`crate::storage::lock_test_env()` (in `jcode-base/src/storage.rs`) is a single
reentrant, poison-tolerant, `!Send` guard over one process `Mutex`. All in-crate
test serialization funnels through it: `App::new_for_test_harness` holds it for
the App's whole lifetime, `render_state_test_lock()` delegates to it, and cache
resets take it. This already serializes the ~810 App-harness tests. The residual
flakes below were tests that **escaped** that lock — either they read a global
the lock doesn't cover, or they depend on wall-clock time / host load.

## Root causes and fixes (by class)

### 1. Wall-clock pacing under CPU load — `stream_buffer.rs`
`test_remote_done_waits_for_paced_backlog_and_one_live_frame` was the dominant
flake. `StreamBuffer` initializes `last_reveal` at construction, so when a burst
starts after any idle gap the whole gap was banked as reveal budget and the
first chunk dumped up to a full 50ms step (~48 chars) at once. Under load the
test's setup gap exceeded that window and a short message revealed immediately,
failing `assert!(!buffer.is_empty())`. **This was a real product bug** (first
tokens after a quiet gap dumped instead of pacing). Fix: `begin_burst_if_idle`
snaps the pacing clock to now when the backlog transitions empty→non-empty.

### 2. `JCODE_HOME` cross-test env race — per-thread home override
`JCODE_HOME` is a process-global OS env var; a `set_var` on one test thread is
instantly visible to all threads, so a concurrent test could point it elsewhere
mid-read (e.g. `handle_post_connect_dispatches_reload_followup…` read its reload
context from the wrong dir). The OS var cannot be made per-thread, but blaude's
own home resolution can: `jcode_core::env::set_var`/`remove_var` now mirror
`JCODE_HOME` into a **thread-local override** (test builds only), and the storage
resolvers (`jcode_dir`/`app_config_dir`/`user_home_path`) consult it first. A
thread that set its own home reads its own home regardless of other threads.
Gated on the `test-support` feature (plumbed jcode-core → jcode-storage →
jcode-base → …); never compiled into release, where resolution reads the env var
directly as before.

### 3. Perf tier first-write-wins — deterministic Full under test
`perf::profile()` is a first-write-wins `OnceLock`; whichever test touched it
first under shifting host load could cache a Reduced/Minimal tier for the whole
process, flaking paced-streaming, redraw-cadence, and animation-policy
assertions. Fix: `FORCE_TEST_FULL_PROFILE` now **defaults to `true` in test
builds** (`cfg!(any(test, feature = "test-support"))`), so every test sees a
stable Full tier from the first call. Tier *detection* is still covered directly
via `compute_tier`/`synthetic_profile`. `redraw_interval` tests additionally
build an explicit synthetic-Full policy and pass an explicit `animation_on_screen`
so they depend on neither the perf global nor the shared idle-animation area.

### 4. Terminal-glyph env read — glyph-independent assertion
`render_system_message_uses_scheduled_task_card` compared the title against
`width_stable_system_title(...)`, which reads `TERM`/`TERM_PROGRAM` live; a
concurrent test flipping those between the render and the recomputation
mismatched the exact form. Fix: assert on the substring shared by both glyph
variants (`"scheduled task due"`).

### 5. Global `Bus` foreign events — session/provider-scoped filtering
`test_tui_openai_compatible_*` subscribe to the process-global `Bus` and panic on
`ProviderModelActivated`/`LoginCompleted`, assuming they own it — but other tests
and leaked background activation tasks publish to the same bus. Fix: scope every
assertion to the app's own session (`ProviderModelActivated.session_id`,
`UiActivity.session_id`) and provider (`LoginCompleted.provider == "Cerebras"`),
so foreign traffic is ignored instead of tripping a panic. (`Bus::global()` has
no per-app injection hook; `Bus::new_isolated_for_tests()` exists for tests that
can use it.)

## Result

At settled load the suite passes 2191/2191 per run. The `--test-threads=1`
workaround is no longer required for correctness.

## Measuring for regressions

Loop the prebuilt binary (build once with `--no-run`) rather than recompiling
each time. Avoid running a second full-suite instance concurrently — two test
processes contend enough to perturb the timing-sensitive cases and produce
non-panic process failures that look like flakes but are measurement artifacts.

```bash
CARGO_INCREMENTAL=0 cargo test -p jcode-tui --lib --no-run
BIN=$(ls -t target/debug/deps/jcode_tui-* | grep -v '\.d$' | head -1)
for i in $(seq 1 20); do "$BIN" --quiet 2>&1 | grep "test result:"; done
```
