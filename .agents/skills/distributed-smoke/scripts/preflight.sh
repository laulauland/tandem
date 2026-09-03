#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 SERVER_SSH CLIENT_A_SSH CLIENT_B_SSH" >&2
  exit 2
fi

server_host=$1
client_a_host=$2
client_b_host=$3

if [[ $server_host == "$client_a_host" || $server_host == "$client_b_host" || $client_a_host == "$client_b_host" ]]; then
  echo "server and clients must be three distinct SSH targets" >&2
  exit 1
fi

for tool in jj cargo curl ssh sha256sum; do
  command -v "$tool" >/dev/null || {
    echo "missing local tool: $tool" >&2
    exit 1
  }
done

for host in "$server_host" "$client_a_host" "$client_b_host"; do
  echo "checking $host"
  ssh -o BatchMode=yes -o ConnectTimeout=10 "$host" \
    'command -v tandem >/dev/null && command -v sha256sum >/dev/null && uname -a'
done

echo "distributed smoke preflight passed"
