#!/bin/sh
# Installs into user locations only: ~/.local/bin and ~/.local/share.
# Uses the prebuilt binary beside this script if there is one, otherwise builds from source.
set -eu
cd "$(dirname "$0")"
if [ -x ./tailgate ]; then
    bin=./tailgate
else
    cargo build --release
    bin=target/release/tailgate
fi
share="$HOME/.local/share"
install -Dm755 "$bin" "$HOME/.local/bin/tailgate"
for s in 48 64 128 256 512; do
    install -Dm644 "assets/icons/tailgate-$s.png" "$share/icons/hicolor/${s}x${s}/apps/tailgate.png"
done
rm -f "$share/icons/hicolor/scalable/apps/tailgate.svg"   # the old icon, from 0.2.0 and earlier
mkdir -p "$share/applications"
sed "s|@BIN@|$HOME/.local/bin/tailgate|" assets/tailgate.desktop > "$share/applications/tailgate.desktop"
command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 >/dev/null 2>&1 || true
command -v update-desktop-database >/dev/null && update-desktop-database "$share/applications" >/dev/null 2>&1 || true
echo "Installed. Launch with: tailgate"
