#!/bin/sh
# What aspic serves under /ucore-deploy/: the page, the runners, this
# version's helper binaries (from packaging/build.sh), and SHA256SUMS over the
# runners and binaries.
#
#   sh stage.sh OUT
#
# Refuses a dirty tree, so what is served is what is committed.
set -eu

[ "$#" -eq 1 ] || { echo "usage: sh stage.sh OUT" >&2; exit 2; }
DEPLOY=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$1"
OUT=$(cd "$1" && pwd)
cd "$DEPLOY"

if [ -n "$(git status --porcelain -- ..)" ]; then
  echo "!! the μCore tree is dirty; commit first" >&2
  exit 1
fi
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
# Windows PowerShell 5.1 reads a script without a BOM as ANSI.
if LC_ALL=C grep -q '[^[:print:][:space:]]' run.ps1; then
  echo "!! run.ps1 is not plain ASCII" >&2
  exit 1
fi
for label in linux-amd64 linux-arm64 macos-arm64 macos-x86_64 windows-x86_64.exe; do
  [ -f "packaging/out/ucore-deploy-$VERSION-$label" ] || { echo "!! no packaging/out/ucore-deploy-$VERSION-$label: run packaging/build.sh" >&2; exit 1; }
done

mkdir -p "$OUT"
rm -f "$OUT"/ucore-deploy-* "$OUT"/SHA256SUMS
cp run.sh run.ps1 web/index.html web/app.js web/style.css "$OUT"/
cp packaging/out/ucore-deploy-"$VERSION"-* "$OUT"/
(cd "$OUT" && sha256sum run.sh run.ps1 ucore-deploy-"$VERSION"-* | sed 's/ \*/  /' > SHA256SUMS)
echo "staged ucore-deploy $VERSION ($(git rev-parse --short HEAD)) in $OUT"
ls -l "$OUT"
