#!/bin/sh
# run.sh -- run the Breeze μCore deployer, and leave no program behind.
#
#   curl -fsSL https://aspic.salataputarica.hr.eu.org/ucore-deploy/run.sh | sh
#
# What it does, and nothing more:
#   * downloads this machine's deployer helper into a new temporary directory;
#   * checks it against SHA256SUMS, and runs nothing that does not match;
#   * runs it on this terminal: it opens its page in your browser, and keeps
#     going until you press Ctrl-C;
#   * deletes the directory again, whatever happened.
#
# The helper keeps your deployments, and the build tools it installs, in a
# folder of its own (~/.local/share/ucore-deploy, or ~/Library/Application
# Support/ucore-deploy on a Mac), so they are there next time. Nothing else is
# installed, and nothing is added to your PATH or your shell's settings.
#
# Copyright 2026 Monika. AGPL-3.0-or-later; see LICENSE.

# Everything is in functions, and `main` runs on the last line: piped into sh,
# the whole script has been read before anything runs.

BASE="${UCORE_DEPLOY_BASE:-https://aspic.salataputarica.hr.eu.org/ucore-deploy}"

die() {
  printf 'ucore-deploy: %s\n' "$1" >&2
  exit 1
}

# This machine, as the binaries are labelled.
label() {
  os=$(uname -s 2>/dev/null)
  arch=$(uname -m 2>/dev/null)
  case "$os" in
    Linux)
      case "$arch" in
        x86_64 | amd64) echo linux-amd64 ;;
        aarch64 | arm64) echo linux-arm64 ;;
        *) die "there is no deployer for Linux on $arch (Espressif's tools need x86_64 or arm64)" ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        arm64) echo macos-arm64 ;;
        x86_64) echo macos-x86_64 ;;
        *) die "there is no deployer for macOS on $arch" ;;
      esac
      ;;
    MINGW* | MSYS* | CYGWIN*)
      die "on Windows, in PowerShell: irm $BASE/run.ps1 | iex"
      ;;
    *) die "there is no deployer for ${os:-this system}" ;;
  esac
}

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    die "downloading needs curl or wget"
  fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  elif command -v sha256 >/dev/null 2>&1; then
    sha256 -q "$1"
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$1" | sed 's/.*= *//'
  else
    die "nothing here computes a SHA-256 (sha256sum, shasum, sha256 or openssl)"
  fi
}

main() {
  me=$(label) || exit 1
  dir=$(mktemp -d 2>/dev/null || mktemp -d -t ucore-deploy) || die "cannot make a temporary directory"
  trap 'rm -rf "$dir"' EXIT
  trap 'rm -rf "$dir"; exit 130' INT TERM HUP

  download "$BASE/SHA256SUMS" "$dir/SHA256SUMS" || die "cannot download $BASE/SHA256SUMS"
  # "HASH  ucore-deploy-VERSION-LABEL": this machine's line. One version is
  # served at a time.
  line=$(awk -v me="$me" '$2 ~ ("^ucore-deploy-[0-9][0-9.]*-" me "$")' "$dir/SHA256SUMS" | tail -n 1)
  [ -n "$line" ] || die "$BASE has no deployer for $me"
  want=${line%% *}
  name=${line##* }
  download "$BASE/$name" "$dir/ucore-deploy" || die "cannot download $BASE/$name"
  [ "$(sha256 "$dir/ucore-deploy")" = "$want" ] || die "$name does not match its SHA-256 in SHA256SUMS; nothing was run"
  chmod 700 "$dir/ucore-deploy"
  version=${name#ucore-deploy-}
  printf 'The μCore deployer %s for %s, checked against its SHA-256. Ctrl-C stops it; the program is deleted then.\n' "${version%-"$me"}" "$me"
  "$dir/ucore-deploy" "$@" </dev/null
}

main "$@"
