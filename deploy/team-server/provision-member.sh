#!/bin/bash
# Provision one teammate on a blaude team server as their own Linux user.
#
#   sudo ./provision-member.sh <username> [--project /srv/blaude/project]
#
# Each teammate gets:
#   - their own Linux user and home, so the harness resolves THEIR credentials
#     (credential lookup is home-relative; that is the whole mechanism)
#   - membership of the shared `blaude` group
#   - their own daemon + bridge, on their own socket under their own home
#   - write access to the shared project directory, via setgid + group perms
#
# What this replaces: one daemon running as one Unix user, with every
# teammate's agent sharing one ~/.jcode/auth.json. That arrangement has no
# boundary between people at all, which is why account pooling existed.
#
# Idempotent: safe to re-run for an existing member.
set -euo pipefail

GROUP="blaude"
# A SECOND group, holding only the public door. Room sockets are group-owned by
# it so the door can connect to every room while members cannot connect to each
# other's. Using the shared `blaude` group for that would defeat the isolation
# entirely, since every member is in it.
DOOR_GROUP="blaude-door"
SOCKET_DIR="/run/blaude"
PROJECT="/srv/blaude/project"
PORT_BASE=7700
# Where the PUBLIC door runs. Its ~/.jcode/member-users.json maps a member's
# email to the Unix user their own room runs as; the door reads it to decide
# which daemon a `?room=mine` connection is joined to. Defaults to the home of
# whoever invoked sudo, which is the account running the door today.
DOOR_HOME="${SUDO_USER:+/home/$SUDO_USER}"
DOOR_HOME="${DOOR_HOME:-$HOME}"
# The member's blaude identity (their email). Without it the member cannot be
# mapped to this Unix user and their own room is unreachable.
EMAIL=""

usage() { echo "usage: sudo $0 <username> [--email addr] [--project DIR] [--port-base N] [--door-home DIR]" >&2; exit 2; }

[ $# -ge 1 ] || usage
USER_NAME="$1"; shift
while [ $# -gt 0 ]; do
  case "$1" in
    --project) PROJECT="$2"; shift 2 ;;
    --port-base) PORT_BASE="$2"; shift 2 ;;
    --email) EMAIL="$2"; shift 2 ;;
    --door-home) DOOR_HOME="$2"; shift 2 ;;
    *) usage ;;
  esac
done

[ "$(id -u)" = "0" ] || { echo "error: must run as root (use sudo)" >&2; exit 1; }
case "$USER_NAME" in
  ''|*[!a-z0-9_-]*) echo "error: '$USER_NAME' is not a valid Linux username" >&2; exit 1 ;;
esac

BINARY="${BLAUDE_BIN:-/usr/local/bin/blaude}"
[ -x "$BINARY" ] || { echo "error: blaude binary not found at $BINARY (set BLAUDE_BIN)" >&2; exit 1; }

echo "==> groups '$GROUP' (all members) and '$DOOR_GROUP' (the door only)"
getent group "$GROUP" >/dev/null || groupadd "$GROUP"
getent group "$DOOR_GROUP" >/dev/null || groupadd "$DOOR_GROUP"

# The socket directory, recreated on every boot because /run is a tmpfs.
#
# 1770 root:$DOOR_GROUP — each room's daemon (which runs with that group)
# creates its own socket here, the door connects to all of them, and nothing
# else can even list the directory. The sticky bit is what stops one member's
# daemon deleting another's socket, since they share the group.
echo "==> socket directory '$SOCKET_DIR'"
install -d -o root -g "$DOOR_GROUP" -m 1770 "$SOCKET_DIR"
printf 'd %s 1770 root %s -\n' "$SOCKET_DIR" "$DOOR_GROUP" > /etc/tmpfiles.d/blaude.conf

# The DOOR itself must be in the door group, or it cannot reach any room. The
# door is whoever owns $DOOR_HOME.
DOOR_OWNER="$(stat -c %U "$DOOR_HOME" 2>/dev/null || true)"
if [ -n "$DOOR_OWNER" ]; then
  usermod -aG "$DOOR_GROUP" "$DOOR_OWNER"
  echo "==> door '$DOOR_OWNER' added to '$DOOR_GROUP'"
fi

echo "==> user '$USER_NAME'"
if id -u "$USER_NAME" >/dev/null 2>&1; then
  echo "    exists; ensuring group membership"
else
  useradd --create-home --shell /bin/bash "$USER_NAME"
fi
usermod -aG "$GROUP" "$USER_NAME"

HOME_DIR="$(getent passwd "$USER_NAME" | cut -d: -f6)"

# 0750 not 0755: a teammate's home is not world-readable. The group bit lets
# group-owned tooling traverse; nothing outside the team can.
chmod 0750 "$HOME_DIR"
chown "$USER_NAME":"$USER_NAME" "$HOME_DIR"

echo "==> credential store"
install -d -o "$USER_NAME" -g "$USER_NAME" -m 0700 "$HOME_DIR/.jcode"
install -d -o "$USER_NAME" -g "$USER_NAME" -m 0700 "$HOME_DIR/.jcode/runtime"

# umask 002 so files this member creates in the shared project stay
# group-writable. Without it the setgid group is inherited but the group write
# bit is not, and the next teammate cannot edit what this one just wrote.
if ! grep -q "umask 002" "$HOME_DIR/.bashrc" 2>/dev/null; then
  echo "umask 002" >> "$HOME_DIR/.bashrc"
  chown "$USER_NAME":"$USER_NAME" "$HOME_DIR/.bashrc"
fi

echo "==> shared project '$PROJECT'"
install -d -o root -g "$GROUP" -m 2775 "$PROJECT"
# setgid (the leading 2) makes new entries inherit the group, so a file one
# teammate creates is editable by the rest without anyone running chgrp.
chmod g+s "$PROJECT"

if [ -d "$PROJECT/.git" ]; then
  # Git writes objects with the creating user's umask. Without
  # core.sharedRepository=group, teammate A's commit leaves objects teammate B
  # cannot write, and B's next operation fails with a bare "Permission denied"
  # that looks like a git bug rather than a permissions one.
  git -C "$PROJECT" config core.sharedRepository group
  find "$PROJECT/.git" -type d -exec chmod g+rwxs {} + 2>/dev/null || true
  find "$PROJECT/.git" -type f -exec chmod g+rw {} + 2>/dev/null || true
  chgrp -R "$GROUP" "$PROJECT/.git" 2>/dev/null || true
fi

# A stable per-member port, derived from the uid so re-running is idempotent.
UID_NUM="$(id -u "$USER_NAME")"
PORT=$((PORT_BASE + (UID_NUM % 200)))

echo "==> systemd units (daemon + bridge) for '$USER_NAME' on port $PORT"
cat > "/etc/systemd/system/blaude-daemon@$USER_NAME.service" <<UNIT
[Unit]
Description=blaude agent daemon ($USER_NAME)
After=network-online.target
Wants=network-online.target

[Service]
User=$USER_NAME
# The DOOR's group, so the socket this daemon creates is reachable by the door
# and by nobody else. Files in the shared project still land in the '$GROUP'
# group, because that directory is setgid.
Group=$DOOR_GROUP
WorkingDirectory=$PROJECT
# 0007: the socket comes out rw for the user and the door group, and closed to
# everyone else. 0002 would have left it world-readable.
UMask=0007
Environment=HOME=$HOME_DIR
Environment=JCODE_RUNTIME_DIR=$HOME_DIR/.jcode/runtime
# Explicit, so the daemon and the door agree on the path by construction
# rather than both deriving it and hoping they match.
Environment=JCODE_SOCKET=$SOCKET_DIR/$USER_NAME.sock
# The daemon restricts its socket to owner-only AFTER binding, so a chmod from
# here would race it and lose. This tells the daemon to open the socket to the
# door's group itself — the door can reach every room, members can reach none
# but their own.
Environment=JCODE_SOCKET_GROUP=$DOOR_GROUP
Environment=JCODE_IDLE_TIMEOUT_SECS=0
Environment=JCODE_DEFERRED_AUTH_BOOTSTRAP=1
# This process serves exactly one person, but the flag stays on: it also
# refuses to borrow an API key that happens to be in the environment, which is
# the behaviour we want everywhere on a team box.
Environment=JCODE_SERVER_MODE=1
ExecStart=$BINARY --provider auto serve
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
UNIT

cat > "/etc/systemd/system/blaude-bridge@$USER_NAME.service" <<UNIT
[Unit]
Description=blaude harness API bridge ($USER_NAME)
After=network-online.target blaude-daemon@$USER_NAME.service
Wants=network-online.target blaude-daemon@$USER_NAME.service

[Service]
User=$USER_NAME
UMask=0002
Environment=HOME=$HOME_DIR
Environment=JCODE_RUNTIME_DIR=$HOME_DIR/.jcode/runtime
Environment=JCODE_SOCKET=$SOCKET_DIR/$USER_NAME.sock
Environment=JCODE_BRIDGE_NO_SPAWN=1
Environment=JCODE_SERVER_MODE=1
# LOOPBACK, not 0.0.0.0. There is ONE public door (:443); it authenticates the
# bearer token and forwards to the right member's bridge here. Binding these
# publicly would put a lightly-guarded port per member on the internet, need a
# firewall rule per member, and could not use the team's certificate — which
# is issued for the single hostname.
Environment=JCODE_API_WS_BIND=127.0.0.1
Environment=JCODE_API_WS_PORT=$PORT
ExecStart=$BINARY api-bridge
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable --now "blaude-daemon@$USER_NAME.service" "blaude-bridge@$USER_NAME.service"

# Map the member's email to this Unix user, so the public door can route their
# `?room=mine` connections here. Written with python rather than jq (which the
# image does not ship) and merged, never overwritten — re-provisioning one
# member must not forget the others.
if [ -n "$EMAIL" ]; then
  MAP="$DOOR_HOME/.jcode/member-users.json"
  install -d -m 0700 "$DOOR_HOME/.jcode"
  python3 - "$MAP" "$EMAIL" "$USER_NAME" <<'PY'
import json, os, sys
path, email, user = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    with open(path) as handle:
        data = json.load(handle)
    if not isinstance(data, dict):
        data = {}
except Exception:
    data = {}
data[email] = user
tmp = path + ".tmp"
with open(tmp, "w") as handle:
    json.dump(data, handle, indent=2, sort_keys=True)
os.replace(tmp, path)
PY
  DOOR_OWNER="$(stat -c %U "$DOOR_HOME")"
  chown "$DOOR_OWNER":"$DOOR_OWNER" "$MAP" 2>/dev/null || true
  chmod 0600 "$MAP"
  echo "==> mapped $EMAIL -> $USER_NAME in $MAP"
fi

echo
echo "provisioned: $USER_NAME"
echo "  home:      $HOME_DIR         (0750, .jcode 0700)"
echo "  project:   $PROJECT          (2775, group $GROUP)"
echo "  port:      $PORT"
echo "  sign-in:   this member signs in from their own client; their tokens"
echo "             land in $HOME_DIR/.jcode/auth.json and nowhere else."
echo
echo "verify isolation:  sudo ./verify-isolation.sh"
