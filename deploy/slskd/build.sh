#!/usr/bin/env bash
set -euo pipefail
# Rebuild the acquisition fork from a pinned upstream revision plus our patch.
root=$(cd "$(dirname "$0")/../.." && pwd)
source_dir="$root/target/slskd-source"
output_dir="$root/target/slskd-publish"
web_dir="$source_dir/src/web"
revision=e42a525d700d6dc343f316447803138b8ea2fbe3
slskd_git() {
  git -c "safe.directory=$source_dir" -C "$source_dir" "$@"
}
mkdir -p "$root/target"
exec 9>"$root/target/slskd-build.lock"
flock 9
if [[ ! -d "$source_dir/.git" ]]; then
  slskd_git init
  slskd_git remote add origin https://github.com/slskd/slskd.git
  slskd_git fetch --depth 1 origin "$revision"
  slskd_git checkout --detach FETCH_HEAD
fi
[[ "$(slskd_git rev-parse HEAD)" == "$revision" ]] || { echo 'Unexpected slskd source revision' >&2; exit 1; }
if slskd_git apply --unidiff-zero --reverse --check "$root/deploy/slskd/acquisition-concurrency.patch" >/dev/null 2>&1; then
  echo 'slskd acquisition patch already applied'
else
  slskd_git apply --unidiff-zero --check "$root/deploy/slskd/acquisition-concurrency.patch"
  slskd_git apply --unidiff-zero "$root/deploy/slskd/acquisition-concurrency.patch"
fi

# slskd serves its React UI from a separate wwwroot directory. dotnet publish
# only leaves a .gitkeep there, so build and bundle the matching UI explicitly.
(
  cd "$web_dir"
  npm ci
  npm run test-unattended
  npm run build
)

mkdir -p "$root/target/slskd-tmp"
export TMPDIR="$root/target/slskd-tmp" DOTNET_CLI_TELEMETRY_OPTOUT=1
dotnet test "$source_dir/tests/slskd.Tests.Unit" -c Release --filter FullyQualifiedName~ApiConcurrencyTests
mkdir -p "$output_dir"
[[ ! -L "$output_dir" ]] || { echo 'slskd publish output must not be a symlink' >&2; exit 1; }
find "$output_dir" -mindepth 1 -delete
dotnet publish "$source_dir/src/slskd/slskd.csproj" -c Release -r linux-x64 --self-contained true -p:PublishSingleFile=true -p:DebugType=none -o "$output_dir"
mkdir -p "$output_dir/wwwroot"
cp -a "$web_dir/build/." "$output_dir/wwwroot/"
test -s "$output_dir/wwwroot/index.html"
sha256sum "$output_dir/slskd"
