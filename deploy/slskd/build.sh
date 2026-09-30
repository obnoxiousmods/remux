#!/usr/bin/env bash
set -euo pipefail
# Rebuild the acquisition fork from a pinned upstream revision plus our patch.
root=$(cd "$(dirname "$0")/../.." && pwd)
source_dir="$root/target/slskd-source"
output_dir="$root/target/slskd-publish"
revision=e42a525d700d6dc343f316447803138b8ea2fbe3
if [[ ! -d "$source_dir/.git" ]]; then
  git init "$source_dir"
  git -C "$source_dir" remote add origin https://github.com/slskd/slskd.git
  git -C "$source_dir" fetch --depth 1 origin "$revision"
  git -C "$source_dir" checkout --detach FETCH_HEAD
fi
[[ "$(git -C "$source_dir" rev-parse HEAD)" == "$revision" ]] || { echo 'Unexpected slskd source revision' >&2; exit 1; }
if git -C "$source_dir" apply --unidiff-zero --check "$root/deploy/slskd/acquisition-concurrency.patch"; then
  git -C "$source_dir" apply --unidiff-zero "$root/deploy/slskd/acquisition-concurrency.patch"
else
  git -C "$source_dir" apply --unidiff-zero --reverse --check "$root/deploy/slskd/acquisition-concurrency.patch"
fi
mkdir -p "$root/target/slskd-tmp"
export TMPDIR="$root/target/slskd-tmp" DOTNET_CLI_TELEMETRY_OPTOUT=1
 dotnet test "$source_dir/tests/slskd.Tests.Unit" -c Release --filter FullyQualifiedName~ApiConcurrencyTests
 dotnet publish "$source_dir/src/slskd/slskd.csproj" -c Release -r linux-x64 --self-contained true -p:PublishSingleFile=true -p:DebugType=none -o "$output_dir"
sha256sum "$output_dir/slskd"
