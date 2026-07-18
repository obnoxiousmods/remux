#!/usr/bin/env bash
set -Eeuo pipefail

readonly RELEASE_ROOT="/opt/remux/releases"
readonly CURRENT_LINK="/opt/remux/current"

die() {
  printf 'remux integrity: %s\n' "$*" >&2
  exit 1
}

release_only=false
if [[ "${1:-}" == "--release-only" ]]; then
  release_only=true
  shift
fi
[[ $# -eq 0 ]] || die "unknown argument"

release="$(readlink -f "$CURRENT_LINK" 2>/dev/null || true)"
[[ -n "$release" && "$release" == "$RELEASE_ROOT/"* ]] \
  || die "$CURRENT_LINK does not resolve inside $RELEASE_ROOT"
[[ -f "$release/manifest.env" ]] || die "release manifest is missing"
[[ -x "$release/remux-server" ]] || die "release binary is missing or not executable"
[[ -f "$release/dashboard/index.html" ]] || die "release dashboard is missing"

manifest_value() {
  local key="$1"
  sed -n "s/^${key}=//p" "$release/manifest.env" | tail -n 1
}

expected_binary="$(manifest_value BINARY_SHA256)"
expected_dashboard="$(manifest_value DASHBOARD_INDEX_SHA256)"
[[ "$expected_binary" =~ ^[0-9a-f]{64}$ ]] || die "manifest binary hash is invalid"
[[ "$expected_dashboard" =~ ^[0-9a-f]{64}$ ]] || die "manifest dashboard hash is invalid"
[[ "$(sha256sum "$release/remux-server" | awk '{print $1}')" == "$expected_binary" ]] \
  || die "release binary hash mismatch"
[[ "$(sha256sum "$release/dashboard/index.html" | awk '{print $1}')" == "$expected_dashboard" ]] \
  || die "release dashboard hash mismatch"

if [[ "$release_only" == false ]]; then
  systemctl is-active --quiet remux.service || die "remux.service is not active"
  pid="$(systemctl show remux.service -p MainPID --value)"
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || die "remux.service has no main PID"
  running_exe="$(readlink -f "/proc/$pid/exe" 2>/dev/null || true)"
  [[ "$running_exe" == "$release/remux-server" ]] \
    || die "running executable is $running_exe, expected $release/remux-server"
  [[ "$(sha256sum "/proc/$pid/exe" | awk '{print $1}')" == "$expected_binary" ]] \
    || die "running executable hash mismatch"
fi

printf 'remux integrity: ok release=%s commit=%s\n' \
  "$release" "$(manifest_value SOURCE_COMMIT)"
