#!/usr/bin/env bash
set -Eeuo pipefail

readonly RELEASE_ROOT="/opt/remux/releases"
readonly CURRENT_LINK="/opt/remux/current"
readonly PREVIOUS_LINK="/opt/remux/previous"
readonly LOCK_FILE="/run/lock/remux-canonical-deploy.lock"

exec 9<>"$LOCK_FILE"
flock -n 9 || { echo "another Remux deployment is already running" >&2; exit 1; }
current="$(readlink -f "$CURRENT_LINK")"
previous="$(readlink -f "$PREVIOUS_LINK")"
[[ "$current" == "$RELEASE_ROOT/"* && "$previous" == "$RELEASE_ROOT/"* && "$current" != "$previous" ]]

sudo /usr/local/sbin/remux-verify-release --release-only
sudo ln -sfn "$previous" "$CURRENT_LINK.new"
sudo mv -Tf "$CURRENT_LINK.new" "$CURRENT_LINK"
sudo ln -sfn "$current" "$PREVIOUS_LINK.new"
sudo mv -Tf "$PREVIOUS_LINK.new" "$PREVIOUS_LINK"
sudo /usr/local/sbin/remux-verify-release --release-only
sudo systemctl restart remux.service
sudo /usr/local/sbin/remux-verify-release
