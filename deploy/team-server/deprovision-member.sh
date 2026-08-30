#!/bin/bash
# Remove one teammate from a blaude team server.
#
#   sudo ./deprovision-member.sh <username> [--project DIR] [--purge-home]
#
# Stops their services, revokes their credentials, and hands their files in the
# shared project to the team rather than leaving them owned by a dead uid.
#
# By default the home directory is KEPT (renamed aside) so a mistaken removal is
# recoverable. --purge-home deletes it after shredding the credential store.
set -euo pipefail

GROUP="blaude"
PROJECT="/srv/blaude/project"
PURGE=0

usage() { echo "usage: sudo $0 <username> [--project DIR] [--purge-home]" >&2; exit 2; }

[ $# -ge 1 ] || usage
USER_NAME="$1"; shift
while [ $# -gt 0 ]; do
  case "$1" in
    --project) PROJECT="$2"; shift 2 ;;
    --purge-home) PURGE=1; shift ;;
    *) usage ;;
  esac
done

[ "$(id -u)" = "0" ] || { echo "error: must run as root (use sudo)" >&2; exit 1; }
id -u "$USER_NAME" >/dev/null 2>&1 || { echo "error: no such user: $USER_NAME" >&2; exit 1; }

HOME_DIR="$(getent passwd "$USER_NAME" | cut -d: -f6)"

echo "==> stopping services"
systemctl disable --now "blaude-bridge@$USER_NAME.service" 2>/dev/null || true
systemctl disable --now "blaude-daemon@$USER_NAME.service" 2>/dev/null || true
rm -f "/etc/systemd/system/blaude-daemon@$USER_NAME.service" \
      "/etc/systemd/system/blaude-bridge@$USER_NAME.service"
systemctl daemon-reload

echo "==> killing any surviving processes"
pkill -u "$USER_NAME" 2>/dev/null || true
sleep 1
pkill -9 -u "$USER_NAME" 2>/dev/null || true

echo "==> revoking credentials"
# Shred before unlink: the tokens are the sensitive part, and an unlinked file
# on a normal filesystem is still recoverable until its blocks are reused.
if [ -d "$HOME_DIR/.jcode" ]; then
  find "$HOME_DIR/.jcode" -maxdepth 1 -type f \
    \( -name 'auth.json' -o -name 'auth.bak' -o -name '*-auth.json' \
       -o -name 'api-ws-token' -o -name 'clerk.env' -o -name '*.env' \) \
    -exec shred -u {} + 2>/dev/null || true
  rm -rf "$HOME_DIR/.jcode/login-jobs" 2>/dev/null || true
fi

echo "==> reassigning their files in $PROJECT"
# Files owned by a removed uid become orphans displayed as a bare number, and
# a future user created with the same uid would silently inherit them. Give
# them to root:GROUP, group-writable, so the team keeps working on them.
if [ -d "$PROJECT" ]; then
  find "$PROJECT" -user "$USER_NAME" -exec chown root:"$GROUP" {} + 2>/dev/null || true
  find "$PROJECT" -group "$GROUP" -type f -exec chmod g+rw {} + 2>/dev/null || true
  find "$PROJECT" -group "$GROUP" -type d -exec chmod g+rwxs {} + 2>/dev/null || true
fi

echo "==> removing user"
gpasswd -d "$USER_NAME" "$GROUP" 2>/dev/null || true
if [ "$PURGE" = "1" ]; then
  userdel -r "$USER_NAME" 2>/dev/null || userdel "$USER_NAME"
  echo "    home purged"
else
  userdel "$USER_NAME"
  STAMP="$(date +%Y%m%d-%H%M%S)"
  if [ -d "$HOME_DIR" ]; then
    mv "$HOME_DIR" "${HOME_DIR}.removed-${STAMP}"
    chmod 0700 "${HOME_DIR}.removed-${STAMP}"
    echo "    home kept at ${HOME_DIR}.removed-${STAMP} (credentials already shredded)"
  fi
fi

echo
echo "deprovisioned: $USER_NAME"
echo "Their client can no longer reach this server. Revoke their team token"
echo "separately if it was issued outside this script."
