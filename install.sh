#!/usr/bin/env bash
# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0
#
# Install a2acli — the A2A CLI tool (Linux only; macOS users: brew install a2aproject/a2acli)
#
# Usage:
#   bash <(curl -fsSL https://raw.githubusercontent.com/a2aproject/a2a-rs/main/install.sh)
#
# Environment overrides:
#   A2A_CLI_VERSION              Install a specific version such as 0.1.5,
#                                v0.1.5, or a2a-cli-v0.1.5.
#   A2A_CLI_INSTALL_DIR          Directory to place the a2acli binary.
#   A2A_CLI_CARGO_MANIFEST_URL   Override the Cargo.toml URL used to resolve the latest version.
#   A2A_CLI_RELEASE_DOWNLOAD_BASE_URL  Override the release asset download base URL.

set -euo pipefail

REPO_OWNER="a2aproject"
REPO_NAME="a2a-rs"
RELEASE_PREFIX="a2a-cli-v"
DEFAULT_CARGO_MANIFEST_URL="https://raw.githubusercontent.com/${REPO_OWNER}/${REPO_NAME}/main/a2acli/Cargo.toml"
DEFAULT_RELEASE_DOWNLOAD_BASE_URL="https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/download"
TARGET_X86_64="x86_64-unknown-linux-gnu"
TARGET_ARM64="aarch64-unknown-linux-gnu"

usage() {
  cat <<'EOF'
Install a2acli from published GitHub release assets.

Usage:
  bash install.sh

Environment overrides:
  A2A_CLI_VERSION                    Install a specific version such as 0.1.5,
                                     v0.1.5, or a2a-cli-v0.1.5.
  A2A_CLI_INSTALL_DIR                Directory to place the a2acli binary.
  A2A_CLI_CARGO_MANIFEST_URL         Override the Cargo.toml URL used to resolve the latest version.
  A2A_CLI_RELEASE_DOWNLOAD_BASE_URL  Override the release asset download base URL.
EOF
}

log() {
  printf '==> %s\n' "$*" >&2
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

require_cmd() {
  have_cmd "$1" || die "$1 is required"
}

fetch_to_file() {
  local url="$1"
  local destination="$2"

  if have_cmd curl; then
    curl -fsSL --retry 3 "$url" -o "$destination"
    return
  fi

  if have_cmd wget; then
    wget -qO "$destination" "$url"
    return
  fi

  die "curl or wget is required"
}

fetch_text() {
  local url="$1"

  if have_cmd curl; then
    curl -fsSL --retry 3 "$url"
    return
  fi

  if have_cmd wget; then
    wget -qO- "$url"
    return
  fi

  die "curl or wget is required"
}

normalize_version() {
  local requested="$1"

  case "$requested" in
    "${RELEASE_PREFIX}"*)
      printf '%s\n' "${requested#${RELEASE_PREFIX}}"
      ;;
    v*)
      printf '%s\n' "${requested#v}"
      ;;
    *)
      printf '%s\n' "$requested"
      ;;
  esac
}

resolve_release_tag() {
  if [[ -n "${A2A_CLI_VERSION:-}" ]]; then
    local requested_version
    requested_version="$(normalize_version "$A2A_CLI_VERSION")"
    [[ -n "$requested_version" ]] || die "A2A_CLI_VERSION must not be empty"
    printf '%s\n' "${RELEASE_PREFIX}${requested_version}"
    return
  fi

  local manifest_url
  manifest_url="${A2A_CLI_CARGO_MANIFEST_URL:-$DEFAULT_CARGO_MANIFEST_URL}"

  local version
  version="$(fetch_text "$manifest_url" \
    | grep -m1 '^version[[:space:]]*=' \
    | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')"
  [[ -n "$version" ]] || die "failed to resolve the latest a2acli version from $manifest_url"
  printf '%s\n' "${RELEASE_PREFIX}${version}"
}

resolve_target() {
  [[ "$(uname -s)" == "Linux" ]] || die "this installer only supports Linux hosts; macOS users: brew install a2aproject/a2acli"

  case "$(uname -m)" in
    x86_64|amd64)
      printf '%s\n' "$TARGET_X86_64"
      ;;
    aarch64|arm64)
      printf '%s\n' "$TARGET_ARM64"
      ;;
    *)
      die "unsupported Linux architecture $(uname -m); supported targets are x86_64 and aarch64"
      ;;
  esac
}

resolve_install_dir() {
  if [[ -n "${A2A_CLI_INSTALL_DIR:-}" ]]; then
    printf '%s\n' "$A2A_CLI_INSTALL_DIR"
    return
  fi

  if [[ "$(id -u)" -eq 0 ]]; then
    printf '/usr/local/bin\n'
    return
  fi

  if [[ -n "${XDG_BIN_HOME:-}" ]]; then
    printf '%s\n' "$XDG_BIN_HOME"
    return
  fi

  printf '%s/.local/bin\n' "$HOME"
}

compute_sha256() {
  local path="$1"

  if have_cmd sha256sum; then
    sha256sum "$path" | awk '{print $1}'
    return
  fi

  if have_cmd shasum; then
    shasum -a 256 "$path" | awk '{print $1}'
    return
  fi

  if have_cmd openssl; then
    openssl dgst -sha256 "$path" | awk '{print $NF}'
    return
  fi

  die "sha256sum, shasum, or openssl is required"
}

cleanup() {
  if [[ -n "${work_dir:-}" && -d "${work_dir:-}" ]]; then
    rm -rf "$work_dir"
  fi
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  usage
  exit 0
fi

require_cmd tar
require_cmd mktemp
require_cmd install
require_cmd mv

release_tag="$(resolve_release_tag)"
version="${release_tag#${RELEASE_PREFIX}}"
target="$(resolve_target)"
install_dir="$(resolve_install_dir)"
release_download_base_url="${A2A_CLI_RELEASE_DOWNLOAD_BASE_URL:-$DEFAULT_RELEASE_DOWNLOAD_BASE_URL}"
release_dir_url="${release_download_base_url%/}/${release_tag}"
archive_name="a2acli-v${version}-${target}.tar.gz"
checksum_name="${archive_name}.sha256"
archive_url="${release_dir_url}/${archive_name}"
checksum_url="${release_dir_url}/${checksum_name}"

work_dir="$(mktemp -d)"
trap cleanup EXIT

archive_path="${work_dir}/${archive_name}"
checksum_path="${work_dir}/${checksum_name}"
extract_dir="${work_dir}/extract"

log "downloading ${archive_name}"
fetch_to_file "$archive_url" "$archive_path"

log "downloading ${checksum_name}"
fetch_to_file "$checksum_url" "$checksum_path"

expected_sha="$(awk 'NF { print $1; exit }' "$checksum_path" | tr '[:upper:]' '[:lower:]')"
actual_sha="$(compute_sha256 "$archive_path" | tr '[:upper:]' '[:lower:]')"
[[ -n "$expected_sha" ]] || die "checksum file ${checksum_name} did not contain a SHA-256 value"
[[ "$expected_sha" == "$actual_sha" ]] || die "downloaded archive checksum mismatch"

log "verified ${archive_name}"

mkdir -p "$extract_dir"
tar -xzf "$archive_path" -C "$extract_dir"

archive_stem="a2acli-v${version}-${target}"
binary_path="${extract_dir}/${archive_stem}/a2acli"
[[ -f "$binary_path" ]] || die "release archive did not contain ${archive_stem}/a2acli"

mkdir -p "$install_dir" || die "could not create ${install_dir}; rerun with sudo or set A2A_CLI_INSTALL_DIR"
[[ -w "$install_dir" ]] || die "could not write to ${install_dir}; rerun with sudo or set A2A_CLI_INSTALL_DIR"

destination="${install_dir}/a2acli"
staged_destination="$(mktemp "${install_dir}/.a2acli.tmp.XXXXXX")"

install -m 0755 "$binary_path" "$staged_destination"
mv "$staged_destination" "$destination"

log "installed a2acli ${version} to ${destination}"

case ":${PATH}:" in
  *":${install_dir}:"*)
    ;;
  *)
    warn "${install_dir} is not on PATH"
    warn "add it with: export PATH=\"${install_dir}:\$PATH\""
    ;;
esac
