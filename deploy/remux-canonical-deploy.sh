#!/usr/bin/env bash
set -Eeuo pipefail

readonly CANONICAL_SOURCE="/home/s/remux-server-wip"
readonly RELEASE_ROOT="/opt/remux/releases"
readonly CURRENT_LINK="/opt/remux/current"
readonly PREVIOUS_LINK="/opt/remux/previous"
readonly LOCK_FILE="/run/lock/remux-canonical-deploy.lock"
readonly NGINX_CONFIG="deploy/remux.obnoxious.lol.nginx.conf"
readonly NGINX_CONFIG_DEST="/etc/nginx/sites-enabled/remux.obnoxious.lol"

die() {
  printf 'remux deploy: %s\n' "$*" >&2
  exit 1
}

[[ $# -eq 0 ]] || die "this command takes no arguments"
[[ "$(pwd -P)" == "$CANONICAL_SOURCE" ]] || cd "$CANONICAL_SOURCE"
[[ "$(git rev-parse --show-toplevel)" == "$CANONICAL_SOURCE" ]] \
  || die "refusing to deploy outside $CANONICAL_SOURCE"
[[ -f "$NGINX_CONFIG" ]] || die "tracked nginx configuration is missing"

if [[ ! -e "$LOCK_FILE" ]]; then
  sudo install -o "$(id -un)" -g media -m 0660 /dev/null "$LOCK_FILE"
fi
exec 9<>"$LOCK_FILE"
flock -n 9 || die "another Remux deployment is already running"

[[ -z "$(git status --porcelain=v1 --untracked-files=all)" ]] \
  || die "canonical worktree is dirty; commit or remove every change before deploying"

readonly SOURCE_COMMIT="$(git rev-parse HEAD)"
readonly SOURCE_BRANCH="$(git branch --show-current)"
readonly BUILT_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
readonly RELEASE_NAME="${SOURCE_COMMIT}-${BUILT_AT//:/}"
readonly RELEASE_DIR="$RELEASE_ROOT/$RELEASE_NAME"
readonly STAGING_DIR="$RELEASE_ROOT/.staging-$RELEASE_NAME-$$"
readonly DASHBOARD_BUILD="target/dx/remux-dashboard/release/web/public"

printf 'Building Remux %s from %s\n' "$SOURCE_COMMIT" "$CANONICAL_SOURCE"
cargo build --locked --release -p remux-server --bin remux-server
dx build --release --package remux-dashboard --debug-symbols false

[[ "$(git rev-parse HEAD)" == "$SOURCE_COMMIT" ]] \
  || die "HEAD changed during the build"
[[ -z "$(git status --porcelain=v1 --untracked-files=all)" ]] \
  || die "worktree changed during the build"
[[ -x target/release/remux-server ]] || die "release server binary is missing"
[[ -f "$DASHBOARD_BUILD/index.html" ]] || die "dashboard index is missing"

readonly BINARY_SHA256="$(sha256sum target/release/remux-server | awk '{print $1}')"
readonly DASHBOARD_SHA256="$(sha256sum "$DASHBOARD_BUILD/index.html" | awk '{print $1}')"
manifest="$(mktemp)"
trap 'rm -f "$manifest"' EXIT
printf '%s\n' \
  "SOURCE_PATH=$CANONICAL_SOURCE" \
  "SOURCE_COMMIT=$SOURCE_COMMIT" \
  "SOURCE_BRANCH=$SOURCE_BRANCH" \
  "BUILT_AT=$BUILT_AT" \
  "BINARY_SHA256=$BINARY_SHA256" \
  "DASHBOARD_INDEX_SHA256=$DASHBOARD_SHA256" \
  >"$manifest"

sudo install -d -o root -g media -m 0755 "$RELEASE_ROOT" "$STAGING_DIR"
sudo install -o root -g media -m 0755 target/release/remux-server "$STAGING_DIR/remux-server"
sudo install -d -o root -g media -m 0755 "$STAGING_DIR/dashboard"
sudo cp -a "$DASHBOARD_BUILD/." "$STAGING_DIR/dashboard/"
sudo chown -R root:media "$STAGING_DIR/dashboard"
sudo chmod -R u=rwX,g=rX,o=rX "$STAGING_DIR/dashboard"
sudo install -o root -g media -m 0644 "$manifest" "$STAGING_DIR/manifest.env"

[[ "$(sha256sum "$STAGING_DIR/remux-server" | awk '{print $1}')" == "$BINARY_SHA256" ]] \
  || die "installed binary hash does not match the build"
[[ "$(sha256sum "$STAGING_DIR/dashboard/index.html" | awk '{print $1}')" == "$DASHBOARD_SHA256" ]] \
  || die "installed dashboard hash does not match the build"
sudo mv "$STAGING_DIR" "$RELEASE_DIR"

old_release="$(readlink -f "$CURRENT_LINK" 2>/dev/null || true)"
if [[ -n "$old_release" && "$old_release" == "$RELEASE_ROOT/"* && "$old_release" != "$RELEASE_DIR" ]]; then
  sudo ln -sfn "$old_release" "$PREVIOUS_LINK.new"
  sudo mv -Tf "$PREVIOUS_LINK.new" "$PREVIOUS_LINK"
fi
sudo ln -sfn "$RELEASE_DIR" "$CURRENT_LINK.new"
sudo mv -Tf "$CURRENT_LINK.new" "$CURRENT_LINK"

sudo install -o root -g root -m 0755 deploy/remux-verify-release.sh /usr/local/sbin/remux-verify-release
sudo install -o root -g root -m 0644 deploy/remux.service /etc/systemd/system/remux.service
sudo install -o root -g root -m 0644 deploy/remux-integrity.service /etc/systemd/system/remux-integrity.service
sudo install -o root -g root -m 0644 deploy/remux-integrity.timer /etc/systemd/system/remux-integrity.timer
sudo install -o root -g root -m 0644 "$NGINX_CONFIG" "$NGINX_CONFIG_DEST"
sudo nginx -t
sudo systemctl reload nginx
sudo systemctl daemon-reload
sudo /usr/local/sbin/remux-verify-release --release-only
sudo systemctl enable --now remux-integrity.timer
sudo systemctl restart remux.service

for _ in $(seq 1 30); do
  if sudo systemctl is-active --quiet remux.service \
    && curl -fsS --max-time 2 http://127.0.0.1:3008/system/info/public >/dev/null; then
    break
  fi
  sleep 1
done
sudo systemctl is-active --quiet remux.service || die "remux.service did not become active"
curl -fsS --max-time 5 http://127.0.0.1:3008/system/info/public >/dev/null \
  || die "public system endpoint did not become ready"
sudo /usr/local/sbin/remux-verify-release

printf 'DEPLOYED_COMMIT=%s\n' "$SOURCE_COMMIT"
printf 'DEPLOYED_RELEASE=%s\n' "$RELEASE_DIR"
printf 'DEPLOYED_BINARY_SHA256=%s\n' "$BINARY_SHA256"
printf 'DEPLOYED_DASHBOARD_SHA256=%s\n' "$DASHBOARD_SHA256"
