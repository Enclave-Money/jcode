#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cargo_exec="$repo_root/scripts/cargo_exec.sh"

run_cargo() {
  (cd "$repo_root" && "$cargo_exec" "$@")
}

echo "=== Fast test loop (library + primary jcode binary) ==="
# The default product feature set includes the local ONNX embedding stack, AWS
# Bedrock SDK, and PDF extraction. Those integrations have dedicated/full-suite
# coverage, but compiling them on every inner-loop test adds hundreds of crates
# and substantial peak RSS. Keep the fast loop minimal unless explicitly
# overridden with JCODE_DEV_FEATURE_PROFILE=default/full.
export JCODE_DEV_FEATURE_PROFILE="${JCODE_DEV_FEATURE_PROFILE:-minimal}"
echo "Feature profile: $JCODE_DEV_FEATURE_PROFILE"

# Only the primary `jcode` binary contains unit tests. `test_api` and
# `jcode-harness` are executable smoke tools with no #[test] functions, so
# `--bins` needlessly builds and links two additional copies of the full graph.
run_cargo test --lib --bin jcode "$@"

echo ""
release_binary="$repo_root/target/release/jcode"
newer_source=""
if [[ -x "$release_binary" ]]; then
  newer_source=$(find \
    "$repo_root/src" "$repo_root/crates" \
    "$repo_root/Cargo.toml" "$repo_root/Cargo.lock" \
    -type f -newer "$release_binary" -print -quit)
fi
if [[ -x "$release_binary" && -z "$newer_source" ]]; then
  echo "=== Startup regression check (release binary) ==="
  "$repo_root/scripts/check_startup_budget.sh" "$release_binary"
  echo ""
elif [[ -x "$release_binary" ]]; then
  echo "Skipping startup regression check: target/release/jcode is older than the source"
  echo "Build it first with: cargo build --release"
  echo ""
else
  echo "Skipping startup regression check: build release first with cargo build --release"
  echo ""
fi

echo "For full coverage, run: scripts/test_e2e.sh"
