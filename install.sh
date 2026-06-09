#!/usr/bin/env bash

install_bz() (
  set -eo pipefail

  arch="$(uname -m)"
  case "$arch" in
    x86_64 | amd64)
      arch="x86_64"
      ;;
    arm64 | aarch64)
      arch="arm64"
      ;;
    *)
      echo >&2 "Unsupported architecture: $arch"
      exit 1
      ;;
  esac

  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  case "$os" in
    darwin)
      os="macos"
      ;;
    linux)
      ;;
    *)
      echo >&2 "Unsupported OS: $os"
      exit 1
      ;;
  esac

  tempfile="$(mktemp "${TMPDIR:-/tmp}/bz.XXXXX")"
  cleanup() { rm -f "$tempfile"; }
  trap cleanup EXIT

  release="${1:-latest}"
  artifact="bz-${os}-${arch}"
  latest_binary_url="$(
    curl -fsSL "https://api.github.com/repos/buildbuddy-io/bz/releases/${release}" |
      perl -nle 'if (/"browser_download_url":\s*"(.*?/'"${artifact}"')"/) { print $1 }'
  )"

  if [[ -z "$latest_binary_url" ]]; then
    echo >&2 "Could not find release artifact '$artifact' in release '$release'"
    exit 1
  fi

  echo >&2 "Downloading $latest_binary_url"
  curl -fSL "$latest_binary_url" -o "$tempfile"
  chmod 0755 "$tempfile"

  echo >&2 "Will install bz to /usr/local/bin - this may ask for your password."
  sudo mv "$tempfile" /usr/local/bin/bz
  trap - EXIT
  echo >&2 "bz installed successfully."
)

install_bz "$@"
