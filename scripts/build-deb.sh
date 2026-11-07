#!/usr/bin/env bash
# Builds an amd64 .deb for NUC Control into target/debian/.
# Layout: /usr/bin/{nuc-control,nuc-gui}, .desktop entry,
# /usr/share/doc/nuc-control/{README.md,usage/hardware docs}, copyright.
# Requires: cargo, gcc, dpkg-dev.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ $(dpkg --print-architecture) != amd64 ]]; then
	echo "This package script supports native amd64 builds only." >&2
	exit 1
fi

version=$(grep -m1 '^version' Cargo.toml | awk '{print $3}' | tr -d '"')
out=target/debian
pkgroot="$out/nuc-control_${version}_amd64"

echo "== cargo build --release"
cargo build --release --workspace

echo "== staging $pkgroot"
rm -rf "$pkgroot"
mkdir -p "$pkgroot/usr/bin" \
	"$pkgroot/usr/share/applications" \
	"$pkgroot/usr/share/doc/nuc-control" \
	"$pkgroot/DEBIAN"

install -m 755 target/release/nuc-control "$pkgroot/usr/bin/"
install -m 755 target/release/nuc-gui "$pkgroot/usr/bin/"
install -m 644 scripts/org.nuccontrol.gui.desktop "$pkgroot/usr/share/applications/"
install -m 644 README.md docs/hardware-capabilities.md docs/usage.md \
	docs/nuc-control-config.toml.example \
	"$pkgroot/usr/share/doc/nuc-control/"
install -m 644 LICENSE "$pkgroot/usr/share/doc/nuc-control/LICENSE"
install -m 644 debian-packaging/copyright "$pkgroot/usr/share/doc/nuc-control/copyright"

cat >"$pkgroot/DEBIAN/control" <<EOF
Package: nuc-control
Version: $version
Architecture: amd64
Maintainer: NUC Control project <mikeholp87@users.noreply.github.com>
Depends: libgtk-4-1 (>= 4.10), libadwaita-1-0 (>= 1.4), udisks2, power-profiles-daemon
Recommends: polkitd
Section: utils
Priority: optional
Homepage: https://github.com/mikeholp87/msi-control-linux
Description: MSI mini PC hardware & power monitor (read-first)
 Native GTK4/libadwaita GUI + CLI reporting sensors, CPU/GPU state, RAPL
 power, NVMe SMART health and power-profile state. Read-only by default;
 profile switching goes through the polkit-authorized power-profiles-daemon
 D-Bus API and is always verified by re-reading. Unavailable-capabilities
 data is reported honestly, never guessed; fan/thermal protections are
 never modified.
EOF

# Derive ABI requirements from the built binaries, including libc's minimum.
mkdir -p "$pkgroot/debian"
{
	printf 'Source: nuc-control\n\n'
	cat "$pkgroot/DEBIAN/control"
} >"$pkgroot/debian/control"
shlibs=$(cd "$pkgroot" && dpkg-shlibdeps -O -eusr/bin/nuc-control -eusr/bin/nuc-gui)
shlibs=${shlibs#shlibs:Depends=}
sed -i "s/^Depends: /Depends: $shlibs, /" "$pkgroot/DEBIAN/control"
rm -r "$pkgroot/debian"

# remove trailing whitespace that lintian dislikes in control
sed -i 's/[ \t]*$//' "$pkgroot/DEBIAN/control"

find "$pkgroot" -type d -exec chmod 755 {} +

echo "== dpkg-deb --build"
dpkg-deb --root-owner-group --build "$pkgroot" && dpkg-deb --info "$out/nuc-control_${version}_amd64.deb"
echo "built: $out/nuc-control_${version}_amd64.deb"
