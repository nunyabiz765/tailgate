#!/bin/bash
# Builds a .deb from a built Linux binary: package-deb.sh BINARY OUTDIR
# Needs dpkg-dev (dpkg-deb, dpkg-shlibdeps). The binary comes from the Debian 12 build and
# must ask for glibc 2.35 at most, so one .deb installs on Ubuntu 22.04 and 24.04 and the
# distros built on them (Zorin OS 17/18, Linux Mint 21/22, Pop!_OS 22.04/24.04).
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=$1; OUT=$(mkdir -p "$2" && cd "$2" && pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DEB="tailgate_${VERSION}_amd64.deb"

# Ubuntu 22.04 has glibc 2.35. Fail here rather than ship a .deb it can't run.
GLIBC_FLOOR=2.35
need=$(objdump -T "$BIN" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)
if [ "$(printf '%s\n%s\n' "$need" "$GLIBC_FLOOR" | sort -V | tail -1)" != "$GLIBC_FLOOR" ]; then
    echo "tailgate needs glibc $need, newer than $GLIBC_FLOOR (Ubuntu 22.04)" >&2; exit 1
fi

STAGE=$(mktemp -d); ROOT="$STAGE/root"
install -Dm755 "$BIN" "$ROOT/usr/bin/tailgate"
mkdir -p "$ROOT/usr/share/applications"
sed 's|@BIN@|tailgate|' assets/tailgate.desktop > "$ROOT/usr/share/applications/tailgate.desktop"
chmod 644 "$ROOT/usr/share/applications/tailgate.desktop"
for s in 48 64 128 256 512; do
    install -Dm644 "assets/icons/tailgate-$s.png" "$ROOT/usr/share/icons/hicolor/${s}x${s}/apps/tailgate.png"
done
install -Dm644 assets/tailgate.metainfo.xml "$ROOT/usr/share/metainfo/dev.centerpointlabs.tailgate.metainfo.xml"

DOC="$ROOT/usr/share/doc/tailgate"
install -Dm644 README.md "$DOC/README.md"
install -m644 NOTICE LICENSE-MIT LICENSE-APACHE "$DOC/"
python3 -I tools/third-party-licenses.py "$DOC/THIRD-PARTY-LICENSES.txt" x86_64-unknown-linux-gnu
chmod 644 "$DOC/THIRD-PARTY-LICENSES.txt"
cat > "$DOC/copyright" <<'EOF'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: tailgate
Source: https://github.com/nunyabiz765/tailgate

Files: *
Copyright: 2026 Center Point Labs by nicksosa.tech LLC
License: MIT or Apache-2.0
 Full texts in /usr/share/doc/tailgate/LICENSE-MIT and LICENSE-APACHE.
 Bundled fonts and Rust crates: /usr/share/doc/tailgate/THIRD-PARTY-LICENSES.txt
 and NOTICE.
EOF
chmod 644 "$DOC/copyright"

# Libraries the binary links, then the ones winit and Slint open at run time (X11 or
# Wayland, keyboard maps, OpenGL), which dpkg-shlibdeps can't see. Every name below is
# the same package on Ubuntu 22.04 and 24.04.
mkdir -p "$STAGE/debian"
printf 'Source: tailgate\n\nPackage: tailgate\nArchitecture: amd64\n' > "$STAGE/debian/control"
linked=$(cd "$STAGE" && dpkg-shlibdeps -O -e root/usr/bin/tailgate 2>/dev/null | sed -n 's/^shlibs:Depends=//p')
test -n "$linked" || { echo "dpkg-shlibdeps found nothing" >&2; exit 1; }
runtime="libx11-6, libx11-xcb1, libxcb1, libxcursor1, libxi6, libxrandr2, libxrender1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libwayland-egl1, libegl1, libgl1"

mkdir -p "$ROOT/DEBIAN"
cat > "$ROOT/DEBIAN/control" <<EOF
Package: tailgate
Version: $VERSION
Architecture: amd64
Maintainer: Center Point Labs by nicksosa.tech LLC <nick@centerpointlabs.dev>
Installed-Size: $(du -sk --exclude=DEBIAN "$ROOT" | cut -f1)
Depends: $linked, $runtime
Recommends: pkexec | policykit-1, curl, xdg-utils
Suggests: tailscale
Section: net
Priority: optional
Homepage: https://centerpointlabs.dev/services/tailgate/
Description: Desktop app for Tailscale and its Mullvad exit nodes
 Tailgate picks Tailscale exit nodes by city or country, turns Mullvad on
 and off, allows LAN access, lists your devices and what this machine
 serves, and checks what sites see, without the command line.
 .
 Tailscale itself isn't in the Ubuntu archive, so it isn't pulled in
 automatically. Install it from tailscale.com; Tailgate shows the steps on
 first launch. Not affiliated with Tailscale Inc. or Mullvad VPN AB.
EOF
# Tells the desktop to pick up the new menu entry and icon right away.
cat > "$ROOT/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
    command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
    command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true
fi
EOF
cp "$ROOT/DEBIAN/postinst" "$ROOT/DEBIAN/postrm"
sed -i 's/= configure/= remove/' "$ROOT/DEBIAN/postrm"
chmod 755 "$ROOT/DEBIAN/postinst" "$ROOT/DEBIAN/postrm"

dpkg-deb --root-owner-group -Zxz --build "$ROOT" "$OUT/$DEB"
dpkg-deb --info "$OUT/$DEB"
(cd "$OUT" && sha256sum "$DEB" > "$DEB.sha256" && cat "$DEB.sha256")
rm -rf "$STAGE"
