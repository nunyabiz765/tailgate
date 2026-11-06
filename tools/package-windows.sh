#!/bin/bash
# Packages a built Windows exe (run in Git Bash on Windows, or anywhere with zip):
#   package-windows.sh tailgate.exe OUTDIR
set -euo pipefail
cd "$(dirname "$0")/.."
EXE=$1; OUT=$(mkdir -p "$2" && cd "$2" && pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
NAME="tailgate-$VERSION-windows-x86_64"
STAGE=$(mktemp -d); DIR="$STAGE/Tailgate"; mkdir -p "$DIR"
"${PYTHON:-python3}" -I tools/third-party-licenses.py "$DIR/THIRD-PARTY-LICENSES.txt" x86_64-pc-windows-msvc
cp "$EXE" "$DIR/Tailgate.exe"
cp tools/dist/READ-ME-FIRST-windows.txt "$DIR/READ ME FIRST.txt"
cp README.md NOTICE LICENSE-MIT LICENSE-APACHE "$DIR/"
cp -r LICENSES "$DIR/"
rm -f "$OUT/$NAME.zip"
if command -v 7z >/dev/null; then (cd "$STAGE" && 7z a -tzip -mx=9 "$OUT/$NAME.zip" Tailgate >/dev/null)
else (cd "$STAGE" && zip -qr9 "$OUT/$NAME.zip" Tailgate); fi
(cd "$OUT" && sha256sum "$NAME.zip" | sed 's/ \*/  /' > "$NAME.zip.sha256" && cat "$NAME.zip.sha256")
rm -rf "$STAGE"
