#!/bin/sh
# The deployer helper's release binaries, and their SHA256SUMS, into
# packaging/out/:
#
#   packaging/out/ucore-deploy-VERSION-LABEL[.exe]
#   packaging/out/SHA256SUMS
#
#   sh packaging/build.sh [LABEL...]        # all five by default
#
# The platforms Espressif's tools run on. Linux is static musl, linked by zig
# (cargo zigbuild); macOS too, with the CoreFoundation link stub in
# packaging/macos/; Windows is a native MSVC build, so this runs on Windows
# (Git Bash) for a full set. zig 0.16.0, from UCORE_ZIG or PATH.
#
# A helper downloads the μCore source of the commit it was built from, so a
# release is built from a clean tree whose commit is on GitHub.
set -eu

DEPLOY=$(cd "$(dirname "$0")/.." && pwd)
cd "$DEPLOY"
OUT=packaging/out
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)

if [ -n "$(git status --porcelain -- ..)" ]; then
  echo "!! the μCore tree is dirty; a release helper is built from a commit"
  exit 1
fi
rev=$(git rev-parse HEAD)
if ! git branch -r --contains "$rev" 2>/dev/null | grep -q .; then
  echo "!! $rev is not pushed: the helper would download a source GitHub does not have"
  exit 1
fi

ZIG="${UCORE_ZIG:-zig}"
zig_have=$("$ZIG" version 2>/dev/null || echo none)
if [ "$zig_have" != "0.16.0" ]; then
  echo "!! zig $zig_have at '$ZIG'; this build is pinned to 0.16.0. Run with UCORE_ZIG=/path/to/zig-0.16.0/zig"
  exit 1
fi
export CARGO_ZIGBUILD_ZIG_PATH="$ZIG"
# The commit each helper downloads, said outright rather than left to the
# build script to notice.
export UCORE_DEPLOY_REV="$rev"

# label | rust target
TARGETS="
linux-amd64|x86_64-unknown-linux-musl
linux-arm64|aarch64-unknown-linux-musl
macos-arm64|aarch64-apple-darwin
macos-x86_64|x86_64-apple-darwin
windows-x86_64|x86_64-pc-windows-msvc
"

wanted=${*:-$(printf '%s\n' "$TARGETS" | sed -n 's/|.*//p')}
mkdir -p "$OUT"
rm -f "$OUT"/ucore-deploy-* "$OUT"/SHA256SUMS
for label in $wanted; do
  target=$(printf '%s\n' "$TARGETS" | sed -n "s/^$label|//p")
  [ -n "$target" ] || { echo "!! unknown label $label"; exit 1; }
  echo "== $label ($target)"
  exe=""
  case "$target" in
    *windows-msvc)
      cargo build --release --locked --target "$target"
      exe=".exe"
      ;;
    *-apple-darwin)
      fw="$DEPLOY/packaging/macos"
      command -v cygpath >/dev/null 2>&1 && fw=$(cygpath -m "$fw")
      env "CARGO_TARGET_$(echo "$target" | tr 'a-z-' 'A-Z_')_RUSTFLAGS=-C link-arg=-F$fw" \
        cargo zigbuild --release --locked --target "$target"
      ;;
    *)
      cargo zigbuild --release --locked --target "$target"
      ;;
  esac
  cp "target/$target/release/ucore-deploy$exe" "$OUT/ucore-deploy-$VERSION-$label$exe"
done

(cd "$OUT" && sha256sum ucore-deploy-"$VERSION"-* | sed 's/ \*/  /' > SHA256SUMS)
# Each must say so: a helper that downloads another commit's source is wrong.
have=$("$OUT/ucore-deploy-$VERSION-windows-x86_64.exe" --version 2>/dev/null || true)
case "$have" in *"$rev"*|"") ;; *) echo "!! the Windows helper says \"$have\", not $rev"; exit 1 ;; esac
echo "== $OUT (μCore $rev)"
ls -l "$OUT"
