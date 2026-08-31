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
# The SHARED room's checkout: one copy the whole team edits together.
SHARED_PROJECT="/srv/blaude/project"
# Set per user below. The shared room works in SHARED_PROJECT; every other room
# works in its own clone, so two people can edit and TEST at the same time
# without one person's half-finished change landing in the other's test run.
PROJECT=""
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
# 1771 root:$DOOR_GROUP — the door (in that group) reaches every room, and the
# sticky bit stops one member's daemon deleting another's socket.
#
# The final 1 is execute-for-others, and it is load-bearing: a member must
# traverse this directory to open its OWN X cookie, and at 1770 it could not,
# so a browser the agent launched died with "Missing X server" while the
# door's capture kept working — a screen you could watch but never draw on.
# Traverse is not read: others still cannot LIST the directory, so no member
# can enumerate the rooms, and every file inside is owner+door only. Isolation
# rests on those per-file modes, which is why widening the directory is safe
# and adding members to $DOOR_GROUP would not be — that would hand every
# member every other member's cookie and socket.
# Browsers for testing, installed ONCE and shared read-only.
#
# The point of the room screen is to watch the app you just built actually
# run, so a headed browser has to launch as the member. Per-member installs
# would be the same ~400MB downloaded again for every person on the team, so
# they live in one world-readable path and every room's daemon is pointed at
# it. Headless still works; this only adds the ability to SEE it.
PLAYWRIGHT_PATH="/opt/ms-playwright"
if [ ! -d "$PLAYWRIGHT_PATH/chromium-"* ] 2>/dev/null; then
  echo "==> shared browsers for testing ($PLAYWRIGHT_PATH)"
  mkdir -p "$PLAYWRIGHT_PATH"
  PLAYWRIGHT_BROWSERS_PATH="$PLAYWRIGHT_PATH" npx --yes playwright@1.47.0 install --with-deps chromium >/dev/null 2>&1 \
    || echo "    (browser install failed — headless testing still works)"
  chmod -R a+rX "$PLAYWRIGHT_PATH"
  npm i -g playwright@1.47.0 >/dev/null 2>&1 || true
fi

echo "==> socket directory '$SOCKET_DIR'"
install -d -o root -g "$DOOR_GROUP" -m 1771 "$SOCKET_DIR"
printf 'd %s 1771 root %s -\n' "$SOCKET_DIR" "$DOOR_GROUP" > /etc/tmpfiles.d/blaude.conf

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
chown "$USER_NAME":"$DOOR_GROUP" "$HOME_DIR"

echo "==> credential store"
# 0770 owned by the member, group-owned by the DOOR.
#
# Sign-in happens at the door, so every credential lands there and has to be
# distributed to the room that will actually run the turn. 0700 made that
# impossible — the door could not even stat the directory. The door already
# holds every one of these credentials, so this leaks nothing to it; what it
# preserves is the boundary that matters, MEMBER to MEMBER, because no member
# is in the door group.
install -d -o "$USER_NAME" -g "$DOOR_GROUP" -m 0770 "$HOME_DIR/.jcode"
install -d -o "$USER_NAME" -g "$DOOR_GROUP" -m 0770 "$HOME_DIR/.jcode/runtime"
# The home itself must be traversable by the door to reach .jcode at all.
chgrp "$DOOR_GROUP" "$HOME_DIR"
chmod 0750 "$HOME_DIR"

# umask 002 so files this member creates in the shared project stay
# group-writable. Without it the setgid group is inherited but the group write
# bit is not, and the next teammate cannot edit what this one just wrote.
if ! grep -q "umask 002" "$HOME_DIR/.bashrc" 2>/dev/null; then
  echo "umask 002" >> "$HOME_DIR/.bashrc"
  chown "$USER_NAME":"$USER_NAME" "$HOME_DIR/.bashrc"
fi

# Where THIS room works.
#
# The shared room is the team's one checkout. A member's room gets its own
# clone of every repo in it: separate ports and separate desktops are not
# enough on their own, because both would still be serving the same files.
if [ "$USER_NAME" = "blaude-shared" ]; then
  PROJECT="$SHARED_PROJECT"
else
  PROJECT="$HOME_DIR/project"
fi

echo "==> shared project '$SHARED_PROJECT'"
install -d -o root -g "$GROUP" -m 2775 "$SHARED_PROJECT"
# setgid (the leading 2) makes new entries inherit the group, so a file one
# teammate creates is editable by the rest without anyone running chgrp.
chmod g+s "$SHARED_PROJECT"

if [ -d "$SHARED_PROJECT/.git" ]; then
  # Git writes objects with the creating user's umask. Without
  # core.sharedRepository=group, teammate A's commit leaves objects teammate B
  # cannot write, and B's next operation fails with a bare "Permission denied"
  # that looks like a git bug rather than a permissions one.
  git -C "$SHARED_PROJECT" config core.sharedRepository group
  find "$SHARED_PROJECT/.git" -type d -exec chmod g+rwxs {} + 2>/dev/null || true
  find "$SHARED_PROJECT/.git" -type f -exec chmod g+rw {} + 2>/dev/null || true
  chgrp -R "$GROUP" "$SHARED_PROJECT/.git" 2>/dev/null || true
fi

# A member's own copy of every repo the team is working on.
#
# Clone, not a worktree: a worktree shares only the .git objects, which for a
# real project is a rounding error next to node_modules — and worktrees are
# not wanted in this project. Dependencies are copied across on the first
# provision so the room can run the app immediately; after that each copy is
# the member's own to install into.
if [ "$USER_NAME" != "blaude-shared" ]; then
  install -d -o "$USER_NAME" -g "$USER_NAME" -m 0750 "$PROJECT"
  for repo in "$SHARED_PROJECT"/*/; do
    [ -d "$repo/.git" ] || continue
    name="$(basename "$repo")"
    dest="$PROJECT/$name"
    if [ ! -d "$dest/.git" ]; then
      echo "==> cloning '$name' into $USER_NAME's own copy"
      sudo -u "$USER_NAME" git clone -q "$repo" "$dest" 2>/dev/null || \
        { rm -rf "$dest"; cp -a "$repo" "$dest"; chown -R "$USER_NAME":"$USER_NAME" "$dest"; }
      # Warm the dependencies so the room can run the app straight away; a
      # fresh npm install of a real project is minutes the member should not
      # have to wait through before their first test.
      if [ -d "$repo/node_modules" ] && [ ! -d "$dest/node_modules" ]; then
        cp -a "$repo/node_modules" "$dest/node_modules" 2>/dev/null || true
        chown -R "$USER_NAME":"$USER_NAME" "$dest/node_modules" 2>/dev/null || true
      fi
    fi
  done
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

# --- the room's desktop -----------------------------------------------------
#
# A screen is only useful if it is the machine the agent works on, so the
# desktop runs HERE, as this room's user, with this room's checkout and this
# room's localhost. One display per room: two people testing at once must not
# be looking at, or clicking in, the same browser.
#
# The X authority file gets the same treatment as the socket: 0640 owned by the
# member and group-owned by the door, so the door can capture every room's
# screen and members can capture none. An `-ac` display would have been simpler
# and would let any local user screenshot any room.
DISPLAY_NUM=$((90 + (UID_NUM % 100)))
XAUTH="$SOCKET_DIR/$USER_NAME.Xauth"

cat > "/etc/systemd/system/blaude-desktop@$USER_NAME.service" <<UNIT
[Unit]
Description=blaude desktop ($USER_NAME, display :$DISPLAY_NUM)
After=network-online.target

[Service]
User=$USER_NAME
Group=$DOOR_GROUP
UMask=0007
Environment=HOME=$HOME_DIR
Environment=DISPLAY=:$DISPLAY_NUM
Environment=XAUTHORITY=$XAUTH
WorkingDirectory=$PROJECT
# A fresh cookie per start: a stale one silently denies every capture.
# chmod AFTER xauth, not before: `xauth add` REWRITES the file and resets it
# to 0600, silently undoing a chmod that ran first — leaving the door unable
# to read the cookie, and every capture failing with "unable to open X server".
ExecStartPre=/bin/sh -c 'rm -f $XAUTH; xauth -f $XAUTH add :$DISPLAY_NUM . $(head -c 16 /dev/urandom | od -An -tx1 | tr -d " \n"); chgrp $DOOR_GROUP $XAUTH; chmod 0640 $XAUTH'
ExecStart=/usr/bin/Xvfb :$DISPLAY_NUM -screen 0 1920x1080x24 -auth $XAUTH
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
UNIT

# A window manager, so windows have frames and can be arranged — a bare Xvfb
# renders unmanaged windows with no decoration and no stacking.
cat > "/etc/systemd/system/blaude-wm@$USER_NAME.service" <<UNIT
[Unit]
Description=blaude window manager ($USER_NAME)
After=blaude-desktop@$USER_NAME.service
Requires=blaude-desktop@$USER_NAME.service

[Service]
User=$USER_NAME
Group=$DOOR_GROUP
Environment=HOME=$HOME_DIR
Environment=DISPLAY=:$DISPLAY_NUM
Environment=XAUTHORITY=$XAUTH
# Xvfb takes a moment to accept connections; without the wait openbox exits
# and systemd restart-loops it against a display that is not up yet.
ExecStartPre=/bin/sh -c 'for i in \$(seq 1 50); do xdpyinfo -display :$DISPLAY_NUM >/dev/null 2>&1 && exit 0; sleep 0.2; done; exit 0'
ExecStart=/usr/bin/openbox
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
UNIT

# --- credential distribution ------------------------------------------------
#
# Sign-in happens at the DOOR, so every account lands in the door's auth file.
# Turns run in a ROOM, as that room's user, reading that user's own file — so
# without distribution the rooms have no credentials and no turn can run.
#
# Done by ROOT, on a path trigger, rather than by the door writing directly:
# the daemon owns ~/.jcode and re-tightens it to 0700 on startup, so a door
# write there is racing a process whose job is to close that door. Root can
# write and chown correctly and does not have to win a race.
#
# Who gets what preserves the isolation rooms exist for: the shared room gets
# every account (a turn there runs as whoever sent it, and `added_by` picks
# their account); a member's room gets only the accounts they added.
install -m 0755 /dev/stdin /usr/local/bin/blaude-sync-room-auth <<'SYNC'
#!/usr/bin/env python3
import json, os, pwd, shutil, subprocess, sys

DOOR_HOME = sys.argv[1] if len(sys.argv) > 1 else "/home/sumermalhotra"
SHARED = "blaude-shared"

def load(path):
    try:
        with open(path) as handle:
            return json.load(handle)
    except Exception:
        return None

auth = load(os.path.join(DOOR_HOME, ".jcode", "auth.json"))
if not auth:
    sys.exit(0)
accounts = auth.get("anthropic_accounts") or []
members = load(os.path.join(DOOR_HOME, ".jcode", "member-users.json")) or {}

targets = {SHARED: accounts}
for email, user in members.items():
    targets[user] = [a for a in accounts if a.get("added_by") == email]

for user, mine in targets.items():
    try:
        entry = pwd.getpwnam(user)
    except KeyError:
        continue
    store = os.path.join(entry.pw_dir, ".jcode")
    if not os.path.isdir(store):
        continue
    out = dict(auth)
    out["anthropic_accounts"] = mine
    if mine:
        out["active_anthropic_account"] = mine[0].get("label")
    else:
        out.pop("active_anthropic_account", None)
    path = os.path.join(store, "auth.json")
    tmp = path + ".tmp"
    with open(tmp, "w") as handle:
        json.dump(out, handle, indent=2)
    os.chmod(tmp, 0o600)
    os.chown(tmp, entry.pw_uid, entry.pw_gid)
    os.replace(tmp, path)
    print(f"synced {len(mine)} account(s) to {user}")
SYNC

cat > /etc/systemd/system/blaude-sync-room-auth.service <<UNIT
[Unit]
Description=distribute AI accounts from the door to each room

[Service]
Type=oneshot
ExecStart=/usr/local/bin/blaude-sync-room-auth $DOOR_HOME
UNIT

# A path unit, so a sign-in at the door reaches the rooms without anyone
# remembering to run anything.
cat > /etc/systemd/system/blaude-sync-room-auth.path <<UNIT
[Unit]
Description=watch the door's account store

[Path]
PathChanged=$DOOR_HOME/.jcode/auth.json
Unit=blaude-sync-room-auth.service

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable --now blaude-sync-room-auth.path >/dev/null 2>&1
systemctl start blaude-sync-room-auth.service >/dev/null 2>&1

systemctl daemon-reload
systemctl enable --now "blaude-daemon@$USER_NAME.service"
systemctl enable --now "blaude-desktop@$USER_NAME.service" "blaude-wm@$USER_NAME.service"

# The room's daemon needs to know which display to launch browsers into, and
# which port to serve on.
#
# Ports are system-wide, so two rooms cannot both have 3000: whoever started
# second would fail to bind, or worse, silently attach to the other room's
# server and show a teammate's build. PORT is what `next dev`, `vite` and most
# dev servers read, so setting it here means "npm run dev" just works in every
# room and lands somewhere different.
DEV_PORT=$((3000 + (UID_NUM % 100)))
mkdir -p "/etc/systemd/system/blaude-daemon@$USER_NAME.service.d"
cat > "/etc/systemd/system/blaude-daemon@$USER_NAME.service.d/display.conf" <<UNIT
[Service]
Environment=DISPLAY=:$DISPLAY_NUM
Environment=XAUTHORITY=$XAUTH
Environment=PORT=$DEV_PORT
Environment=PLAYWRIGHT_BROWSERS_PATH=$PLAYWRIGHT_PATH
UNIT
systemctl daemon-reload

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
echo "  port:      $PORT (bridge)"
echo "  display:   :$DISPLAY_NUM   dev server: $DEV_PORT"
echo "  sign-in:   this member signs in from their own client; their tokens"
echo "             land in $HOME_DIR/.jcode/auth.json and nowhere else."
echo
echo "verify isolation:  sudo ./verify-isolation.sh"
