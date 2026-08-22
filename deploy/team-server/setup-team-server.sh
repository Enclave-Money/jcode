#!/bin/sh
# Self-hosted blaude team server: one box, one daemon, every teammate's
# client connects over (w)ss with their own token. This script installs the
# bridge as a service (launchd on macOS, systemd on Linux) with the team
# configuration: LAN/public bind, optional native TLS, token files in place.
#
#   ./setup-team-server.sh [--bind 0.0.0.0] [--port 7644] \
#       [--tls-cert /path/cert.pem --tls-key /path/key.pem] \
#       [--binary /path/to/blaude] [--dry-run]
#
# After install:
#   - the owner token is at ~/.jcode/api-ws-token
#   - member tokens go in ~/.jcode/team-tokens.json {"email": "token"}
#     (the blaude desktop app's Invite People writes this for you)
#   - teammates: File > Join Team with wss://<host>:<port>/api + token,
#     or open https://<host>:<port>/ on a phone
set -eu

BIND="0.0.0.0"
PORT="7644"
TLS_CERT=""
TLS_KEY=""
BINARY="${BLAUDE_BIN:-$(command -v blaude || true)}"
DRY_RUN=0

while [ $# -gt 0 ]; do
  case "$1" in
    --bind) BIND="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    --tls-cert) TLS_CERT="$2"; shift 2 ;;
    --tls-key) TLS_KEY="$2"; shift 2 ;;
    --binary) BINARY="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

[ -n "$BINARY" ] && [ -x "$BINARY" ] || {
  echo "error: blaude binary not found — pass --binary or set BLAUDE_BIN" >&2
  exit 1
}
if [ -n "$TLS_CERT" ] || [ -n "$TLS_KEY" ]; then
  [ -f "$TLS_CERT" ] && [ -f "$TLS_KEY" ] || {
    echo "error: --tls-cert and --tls-key must both point at PEM files" >&2
    exit 1
  }
elif [ "$BIND" != "127.0.0.1" ]; then
  echo "warning: binding $BIND without TLS — fine on a tailnet/VPN," >&2
  echo "warning: do NOT expose this to the public internet without --tls-cert/--tls-key" >&2
fi

OS="$(uname -s)"
if [ "$OS" = "Darwin" ]; then
  UNIT="$HOME/Library/LaunchAgents/team.blaude.bridge.plist"
  render_unit() {
    cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>team.blaude.bridge</string>
  <key>ProgramArguments</key><array>
    <string>$BINARY</string><string>api-bridge</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>JCODE_API_WS_BIND</key><string>$BIND</string>
    <key>JCODE_API_WS_PORT</key><string>$PORT</string>$( [ -n "$TLS_CERT" ] && printf '\n    <key>JCODE_API_WS_TLS_CERT</key><string>%s</string>\n    <key>JCODE_API_WS_TLS_KEY</key><string>%s</string>' "$TLS_CERT" "$TLS_KEY" )
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$HOME/Library/Logs/blaude-team-bridge.log</string>
  <key>StandardErrorPath</key><string>$HOME/Library/Logs/blaude-team-bridge.log</string>
</dict></plist>
PLIST
  }
  if [ "$DRY_RUN" = 1 ]; then
    echo "--- would write $UNIT:"
    render_unit
  else
    mkdir -p "$(dirname "$UNIT")"
    render_unit > "$UNIT"
    launchctl unload "$UNIT" 2>/dev/null || true
    launchctl load "$UNIT"
    echo "installed and started: $UNIT"
  fi
else
  UNIT="$HOME/.config/systemd/user/blaude-team-bridge.service"
  render_unit() {
    cat <<SERVICE
[Unit]
Description=blaude team bridge (harness API over websocket)
After=network.target

[Service]
ExecStart=$BINARY api-bridge
Environment=JCODE_API_WS_BIND=$BIND
Environment=JCODE_API_WS_PORT=$PORT$( [ -n "$TLS_CERT" ] && printf '\nEnvironment=JCODE_API_WS_TLS_CERT=%s\nEnvironment=JCODE_API_WS_TLS_KEY=%s' "$TLS_CERT" "$TLS_KEY" )
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
SERVICE
  }
  if [ "$DRY_RUN" = 1 ]; then
    echo "--- would write $UNIT:"
    render_unit
  else
    mkdir -p "$(dirname "$UNIT")"
    render_unit > "$UNIT"
    systemctl --user daemon-reload
    systemctl --user enable --now blaude-team-bridge
    echo "installed and started: $UNIT"
  fi
fi

SCHEME="ws"; PAGE_SCHEME="http"
[ -n "$TLS_CERT" ] && SCHEME="wss" && PAGE_SCHEME="https"
HOST_HINT="$(hostname 2>/dev/null || echo '<host>')"
echo
echo "Team endpoint:   $SCHEME://$HOST_HINT:$PORT/api"
echo "Phone client:    $PAGE_SCHEME://$HOST_HINT:$PORT/"
echo "Owner token:     ~/.jcode/api-ws-token (created on first start)"
echo "Member tokens:   ~/.jcode/team-tokens.json — or use the app's Invite People"
