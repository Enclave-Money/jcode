#!/usr/bin/env bash
# Install the harness-owned browser helper once, server-wide, at
# /opt/blaude-browser. Root-owned, world-readable; every room's daemon spawns
# it as the room user. Idempotent: re-running repairs an install without
# re-downloading if the browser is already present.
#
# Argument: a staging directory holding helper.js, detect.js, fill.js and
# package.json (defaults to the directory this script sits in).
set -u

STAGE="${1:-$(cd "$(dirname "$0")" && pwd)}"
DEST=/opt/blaude-browser
BROWSERS="$DEST/ms-playwright"

if ! command -v node >/dev/null 2>&1; then
  echo "browser-helper: node missing; installing" >&2
  sudo apt-get install -y -q nodejs npm >/dev/null 2>&1 || true
fi

sudo mkdir -p "$DEST"
for f in helper.js detect.js fill.js package.json; do
  if [ ! -f "$STAGE/$f" ]; then
    echo "browser-helper: staging file missing: $STAGE/$f" >&2
    exit 1
  fi
  sudo cp "$STAGE/$f" "$DEST/$f"
done

# Playwright itself (the npm package). --omit=dev keeps it to the runtime dep.
if [ ! -d "$DEST/node_modules/playwright" ]; then
  ( cd "$DEST" && sudo npm install --omit=dev --no-audit --no-fund >/tmp/browser-helper-npm.log 2>&1 ) || {
    echo "browser-helper: npm install failed"; tail -5 /tmp/browser-helper-npm.log >&2; exit 1; }
fi

# The matched Chromium build, into a shared world-readable path so every room
# reads the same browser. Skipped if already present (the expensive step).
if [ ! -d "$BROWSERS" ] || [ -z "$(ls -A "$BROWSERS" 2>/dev/null)" ]; then
  sudo mkdir -p "$BROWSERS"
  ( cd "$DEST" && sudo PLAYWRIGHT_BROWSERS_PATH="$BROWSERS" npx --yes playwright install chromium >/tmp/browser-helper-pw.log 2>&1 ) || {
    echo "browser-helper: playwright browser install failed"; tail -5 /tmp/browser-helper-pw.log >&2; exit 1; }
fi

# World-readable, nothing writable by a room user: a room runs it, never edits it.
sudo chmod -R a+rX "$DEST"
echo "BROWSER_HELPER_OK"
