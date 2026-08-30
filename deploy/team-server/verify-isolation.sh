#!/bin/bash
# Prove that teammates on this box are actually isolated, by ATTEMPTING the
# accesses that must fail rather than reading permission bits and inferring.
#
#   sudo ./verify-isolation.sh [--project DIR]
#
# Every check states what it tried and what happened. A check that cannot run
# is reported as SKIP, never as a pass: a vacuous pass is how an isolation
# regression hides.
set -uo pipefail

GROUP="blaude"
PROJECT="/srv/blaude/project"
[ "${1:-}" = "--project" ] && { PROJECT="$2"; shift 2; }

[ "$(id -u)" = "0" ] || { echo "error: must run as root (use sudo)" >&2; exit 1; }

PASS=0; FAIL=0; SKIP=0
ok()   { echo "  PASS  $1"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL  $1"; FAIL=$((FAIL+1)); }
skip() { echo "  SKIP  $1"; SKIP=$((SKIP+1)); }

MEMBERS=($(getent group "$GROUP" 2>/dev/null | cut -d: -f4 | tr ',' ' '))

echo "=== members of group '$GROUP': ${MEMBERS[*]:-(none)} ==="
echo

if [ "${#MEMBERS[@]}" -lt 2 ]; then
  echo "Fewer than two members. The cross-user checks below need two to mean"
  echo "anything, so they are SKIPPED rather than passed."
  echo
fi

A="${MEMBERS[0]:-}"
B="${MEMBERS[1]:-}"

echo "--- 1. credential isolation: A must not read B's tokens ---"
if [ -n "$A" ] && [ -n "$B" ]; then
  for f in auth.json api-ws-token clerk.env; do
    BHOME="$(getent passwd "$B" | cut -d: -f6)"
    TARGET="$BHOME/.jcode/$f"
    if [ ! -e "$TARGET" ]; then skip "$B has no $f yet"; continue; fi
    if sudo -u "$A" cat "$TARGET" >/dev/null 2>&1; then
      bad "$A CAN read $B's $f"
    else
      ok "$A cannot read $B's $f"
    fi
  done
  BHOME="$(getent passwd "$B" | cut -d: -f6)"
  if sudo -u "$A" ls "$BHOME/.jcode/" >/dev/null 2>&1; then
    bad "$A can list $B's ~/.jcode/"
  else
    ok "$A cannot list $B's ~/.jcode/"
  fi
else
  skip "cross-user credential read (need two members)"
fi

echo
echo "--- 2. shared project: A's new file must be writable by B ---"
if [ -n "$A" ] && [ -n "$B" ] && [ -d "$PROJECT" ]; then
  PROBE="$PROJECT/.isolation-probe.$$"
  if sudo -u "$A" bash -c "umask 002; echo from-$A > '$PROBE'" 2>/dev/null; then
    OWNER="$(stat -c '%U:%G %a' "$PROBE")"
    if sudo -u "$B" bash -c "echo also-$B >> '$PROBE'" 2>/dev/null; then
      ok "$B can append to $A's file ($OWNER)"
    else
      bad "$B CANNOT write $A's file ($OWNER) - setgid or umask is wrong"
    fi
    rm -f "$PROBE"
  else
    bad "$A cannot create files in $PROJECT"
  fi
else
  skip "shared-write check (need two members and $PROJECT)"
fi

echo
echo "--- 3. setgid on the shared project ---"
if [ -d "$PROJECT" ]; then
  MODE="$(stat -c '%a' "$PROJECT")"; PGRP="$(stat -c '%G' "$PROJECT")"
  case "$MODE" in
    2*) ok "$PROJECT is setgid (mode $MODE, group $PGRP)" ;;
    *)  bad "$PROJECT is NOT setgid (mode $MODE) - new files will not inherit the group" ;;
  esac
  [ "$PGRP" = "$GROUP" ] && ok "owned by group $GROUP" || bad "group is $PGRP, expected $GROUP"
else
  skip "$PROJECT does not exist"
fi

echo
echo "--- 4. no shared credentials in any unit environment ---"
HITS=0
for u in /etc/systemd/system/blaude-*.service; do
  [ -e "$u" ] || continue
  if grep -qE 'ANTHROPIC_API_KEY|CLAUDE_CODE_OAUTH_TOKEN|OPENAI_API_KEY|GEMINI_API_KEY|ANTHROPIC_AUTH_TOKEN' "$u"; then
    bad "$u sets a provider credential in its environment"
    HITS=$((HITS+1))
  fi
done
[ "$HITS" = "0" ] && ok "no provider credentials in any blaude unit"

echo
echo "--- 5. server mode is on (refuses to borrow an environment key) ---"
for u in /etc/systemd/system/blaude-*.service; do
  [ -e "$u" ] || continue
  grep -q 'JCODE_SERVER_MODE=1' "$u" \
    && ok "$(basename "$u") sets JCODE_SERVER_MODE=1" \
    || bad "$(basename "$u") is MISSING JCODE_SERVER_MODE=1"
done

echo
echo "--- 6. system-wide credential leaks ---"
for f in /etc/environment /etc/profile /etc/bash.bashrc; do
  [ -e "$f" ] || continue
  grep -qE 'ANTHROPIC_API_KEY|OPENAI_API_KEY|GEMINI_API_KEY|CLAUDE_CODE_OAUTH_TOKEN' "$f" \
    && bad "$f exports a provider credential" \
    || ok "$f is clean"
done
if ls /etc/profile.d/*.sh >/dev/null 2>&1; then
  grep -qlE 'ANTHROPIC_API_KEY|OPENAI_API_KEY|GEMINI_API_KEY' /etc/profile.d/*.sh 2>/dev/null \
    && bad "/etc/profile.d exports a provider credential" \
    || ok "/etc/profile.d is clean"
fi

echo
echo "--- 7. each member has their own daemon socket ---"
for m in "${MEMBERS[@]}"; do
  [ -n "$m" ] || continue
  MHOME="$(getent passwd "$m" | cut -d: -f6)"
  SOCK="$MHOME/.jcode/runtime/blaude.sock"
  if [ -S "$SOCK" ]; then
    SMODE="$(stat -c '%a %U' "$SOCK")"
    case "$SMODE" in
      6[0-7]0\ *) ok "$m has a private socket ($SMODE)" ;;
      *) bad "$m socket has permissive mode ($SMODE)" ;;
    esac
  else
    skip "$m has no running daemon socket"
  fi
done

echo
echo "======================================"
echo "  pass $PASS   fail $FAIL   skip $SKIP"
echo "======================================"
[ "$SKIP" -gt 0 ] && echo "NOTE: skips are not passes. Re-run with two provisioned members."
[ "$FAIL" -gt 0 ] && exit 1
exit 0
