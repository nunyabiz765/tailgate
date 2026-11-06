#!/bin/bash
# Packages a built Linux binary: package-linux.sh BINARY OUTDIR
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=$1; OUT=$(mkdir -p "$2" && cd "$2" && pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
NAME="tailgate-$VERSION-x86_64-linux"
STAGE=$(mktemp -d); DIR="$STAGE/$NAME"; mkdir -p "$DIR/assets"
python3 -I tools/third-party-licenses.py "$DIR/THIRD-PARTY-LICENSES.txt" x86_64-unknown-linux-gnu
cp "$BIN" "$DIR/tailgate"
cp install.sh README.md NOTICE LICENSE-MIT LICENSE-APACHE "$DIR/"
cp -r LICENSES "$DIR/"
cp assets/tailgate.svg assets/tailgate-256.png assets/tailgate-64.png assets/tailgate.desktop "$DIR/assets/"
chmod 755 "$DIR/tailgate" "$DIR/install.sh"
tar -C "$STAGE" --owner=0 --group=0 --sort=name -czf "$OUT/$NAME.tar.gz" "$NAME"
(cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256" && cat "$NAME.tar.gz.sha256")
rm -rf "$STAGE"
