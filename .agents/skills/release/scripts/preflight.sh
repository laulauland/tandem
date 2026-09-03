#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 || ! $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ || (${2:-} != "" && ${2:-} != --workspace) ]]; then
  echo "usage: $0 X.Y.Z [--workspace]" >&2
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

python3 scripts/check_workspace.py --version "$release_version"
package_order=$(python3 scripts/check_workspace.py --version "$release_version" --print-publish-order)
packages=()
while IFS= read -r package; do packages+=("$package"); done <<< "$package_order"
package_args=()
for package in "${packages[@]}"; do package_args+=(-p "$package"); done

# A local graph/packaging qualification, not a release candidate or authorization.
# Packaging the packages together lets Cargo resolve unpublished local members.
if [[ ${2:-} == --workspace ]]; then
  cargo fmt --all --check
  python3 scripts/check_docs.py
  cargo test --workspace
  cargo package "${package_args[@]}" --allow-dirty --no-verify --offline
  echo "non-publishing workspace preflight passed for v$release_version"
  exit 0
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
cargo fmt --all --check
cargo test --workspace
cargo package "${package_args[@]}" --allow-dirty --no-verify

# Individual dry-runs of dependent packages require the matching dependency
# versions to be indexed already. Perform them in publication order during the
# separately authorized crates.io phase, not while preparing a new stack.

echo "release preflight passed for v$release_version"
