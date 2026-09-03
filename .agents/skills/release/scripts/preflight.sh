#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 || ! $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]]; then
  echo "usage: $0 X.Y.Z" >&2
  exit 2
fi

release_version=$1
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../../../.." && pwd)
cd "$repo_root"

actual_root=$(jj root)
if [[ $actual_root != "$repo_root" ]]; then
  echo "expected jj root $repo_root, got $actual_root" >&2
  exit 1
fi

package_version=$(cargo metadata --no-deps --format-version 1 | python3 -c \
  'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')
if [[ $package_version != "$release_version" ]]; then
  echo "Cargo.toml version is $package_version, expected $release_version" >&2
  exit 1
fi

description=$(jj log -r @ --no-graph -T 'description.first_line()')
if [[ $description != "chore(release): v$release_version" ]]; then
  echo "working-copy description must be: chore(release): v$release_version" >&2
  exit 1
fi

if [[ -n $(jj log -r 'conflicts()' --no-graph -T 'commit_id ++ "\n"') ]]; then
  echo "release graph contains unresolved conflicts" >&2
  exit 1
fi

changed=$(jj diff -r @ --name-only)
while IFS= read -r path; do
  [[ -z $path || $path == Cargo.toml || $path == Cargo.lock ]] && continue
  echo "release revision contains non-version change: $path" >&2
  exit 1
done <<< "$changed"

if ! jj git push --help | grep -q -- '--tag'; then
  echo "this jj cannot push tags; upgrade jj before publishing" >&2
  exit 1
fi

python3 scripts/check_docs.py
cargo test
cargo publish --dry-run

echo "release preflight passed for v$release_version"
