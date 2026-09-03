#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 4 || $3 != -- ]]; then
  echo "usage: $0 EVIDENCE_DIR LABEL -- COMMAND [ARG ...]" >&2
  exit 2
fi

evidence_dir=$1
label=$2
shift 3

if [[ ! $label =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
  echo "label must contain only letters, digits, dot, underscore, or dash" >&2
  exit 2
fi

mkdir -p "$evidence_dir"
output_path="$evidence_dir/$label.log"
metadata_path="$evidence_dir/$label.meta"

if [[ -e $output_path || -e $metadata_path ]]; then
  echo "refusing to overwrite existing evidence for $label" >&2
  exit 1
fi

started_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
command_name=${1##*/}

set +e
"$@" 2>&1 | tee "$output_path"
command_status=${PIPESTATUS[0]}
set -e

finished_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
{
  echo "label=$label"
  echo "command=$command_name"
  echo "started_at=$started_at"
  echo "finished_at=$finished_at"
  echo "exit_code=$command_status"
  echo "output_sha256=$(sha256sum "$output_path" | cut -d' ' -f1)"
} > "$metadata_path"

exit "$command_status"
