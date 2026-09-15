#!/bin/sh
# Install the native td binary and keep one owner credential per host.
set -eu

base_url="${TANDEM_INSTALL_BASE:-@@TANDEM_PUBLIC_URL@@}"
base_url="${base_url%/}"
bin_dir="${TANDEM_INSTALL_BIN_DIR:-${XDG_BIN_HOME:-$HOME/.local/bin}}"
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/td"
credentials="$config_dir/credentials"
target="${TANDEM_INSTALL_TARGET:-}"

say() { printf '%s\n' "$*" >&2; }
die() { say "install: $*"; exit 1; }
command -v curl >/dev/null 2>&1 || die "curl is required"

host="${base_url#*://}"
host="${host%%/*}"
if [ -z "$target" ]; then
  case "$(uname -s):$(uname -m)" in
    Linux:x86_64) target="x86_64-unknown-linux-gnu" ;;
    Darwin:arm64) target="aarch64-apple-darwin" ;;
    *) die "no native release binary for $(uname -s) $(uname -m)" ;;
  esac
fi

mkdir -p "$bin_dir"
tmp="$(mktemp "${TMPDIR:-/tmp}/td.XXXXXX")"
trap 'rm -f "$tmp"' EXIT HUP INT TERM
curl -fsSL "$base_url/dl/td-$target" -o "$tmp" ||
  die "cannot download td for $target from $base_url"
chmod 755 "$tmp"
mv "$tmp" "$bin_dir/td"
trap - EXIT HUP INT TERM

valid_owner_token() {
  case "$1" in tdmo_*) body="${1#tdmo_}" ;; *) return 1 ;; esac
  entropy="${body%%_*}"
  tag="${body#*_}"
  [ "$entropy" != "$body" ] && [ "${tag#*_}" = "$tag" ] || return 1
  [ "${#entropy}" -eq 64 ] && [ "${#tag}" -eq 64 ] || return 1
  case "$entropy$tag" in *[!0-9a-f]*) return 1 ;; esac
}

existing=""
if [ -f "$credentials" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    key="$(printf '%s' "${line%%=*}" | tr -d '[:space:]')"
    if [ "$key" = "$host" ]; then
      value="${line#*=}"
      value="$(printf '%s' "$value" | tr -d '[:space:]')"
      if valid_owner_token "$value"; then existing="$value"; break; fi
    fi
  done <"$credentials"
fi

if [ -n "$existing" ]; then
  verify_status="$(printf 'header = "Authorization: Bearer %s"\n' "$existing" |
    curl --config - -sS -o /dev/null -w '%{http_code}' -X POST \
      "$base_url/install/token/verify")" ||
    die "cannot verify the existing owner credential; credential file is unchanged"
  case "$verify_status" in
    204) ;;
    401) existing="" ;;
    *) die "owner credential verification answered HTTP $verify_status; credential file is unchanged" ;;
  esac
fi

if [ -z "$existing" ]; then
  response="$(curl -fsS -X POST "$base_url/install/token")" ||
    die "cannot obtain an owner credential from $base_url"
  token="$(printf '%s' "$response" | sed -n \
    's/^[[:space:]]*{"token"[[:space:]]*:[[:space:]]*"\([^"]*\)"}[[:space:]]*$/\1/p')"
  unset response
  valid_owner_token "$token" || die "$base_url/install/token returned an invalid owner credential"
  umask 077
  mkdir -p "$config_dir"
  credentials_tmp="$(mktemp "$config_dir/.credentials.XXXXXX")"
  trap 'rm -f "$credentials_tmp"' EXIT HUP INT TERM
  if [ -f "$credentials" ]; then
    while IFS= read -r line || [ -n "$line" ]; do
      key="$(printf '%s' "${line%%=*}" | tr -d '[:space:]')"
      [ "$key" = "$host" ] || printf '%s\n' "$line" >>"$credentials_tmp"
    done <"$credentials"
  fi
  printf '%s = %s\n' "$host" "$token" >>"$credentials_tmp"
  chmod 600 "$credentials_tmp"
  mv "$credentials_tmp" "$credentials"
  trap - EXIT HUP INT TERM
else
  chmod 600 "$credentials"
fi

say "installed $bin_dir/td"
say "owner credential for $host is available in $credentials"
say "td clone $host/<namespace>/<repository>"
