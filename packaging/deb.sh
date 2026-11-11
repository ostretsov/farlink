#!/bin/sh
# Builds the desktop app's Debian package.
#   packaging/deb.sh <farlink program> <amd64|arm64> <version> <output .deb>
# Needs only dpkg-deb and readelf (binutils). Icons: packaging/icons
# (regenerate with `make icons` when the icon code changes).
set -eu
bin=$1 arch=$2 version=$3 out=$4
here=$(cd "$(dirname "$0")" && pwd)
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
chmod 755 "$root"

install -D -m 755 "$bin" "$root/usr/bin/farlink"
install -D -m 644 "$here/farlink.desktop" "$root/usr/share/applications/farlink.desktop"
for s in 16 22 32 48 256; do
  install -D -m 644 "$here/icons/farlink-$s.png" "$root/usr/share/icons/hicolor/${s}x${s}/apps/farlink.png"
done

# Oldest glibc the program works with = newest symbol version it uses.
glibc=$(readelf -V "$bin" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)
size=$(du -sk "$root/usr" | cut -f1)
mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CONTROL
Package: farlink
Version: $version
Architecture: $arch
Maintainer: Farlink <noreply@farlink.invalid>
Installed-Size: $size
Section: net
Priority: optional
Depends: libc6 (>= $glibc), libgcc-s1, libx11-6, libx11-xcb1, libxcb1, libxcursor1, libxi6, libxrender1, libxkbcommon0, libxkbcommon-x11-0, libgl1 | libegl1
Recommends: policykit-1 | pkexec
Description: remote desktop viewer for a self-hosted Farlink server
 See and control your computers through your own Farlink server: one
 window per remote screen, file transfer, shared clipboard and temporary
 links. Starts hidden in the tray at login and updates itself from the
 server with newer versions of this package.
CONTROL

mkdir -p "$(dirname "$out")"
dpkg-deb --root-owner-group -Zxz --build "$root" "$out" >/dev/null
echo "$out"
