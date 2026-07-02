#!/usr/bin/env bash
# Build a badbitch .deb from source. Output: target/deb/badbitch_<version>_<arch>.deb
# Requires: a Rust toolchain (cargo) and dpkg-deb. No fakeroot needed
# (dpkg-deb --root-owner-group sets root:root ownership without it).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
ARCH="$(dpkg --print-architecture)"
STAGE="target/deb/badbitch_${VERSION}_${ARCH}"

echo "[build-deb] cargo build --release (CLI + GUI)"
cargo build --release --bin badbitch
cargo build --release --features gui --bin badbitch-gui

echo "[build-deb] staging $STAGE"
rm -rf "$STAGE"
mkdir -p "$STAGE/DEBIAN" "$STAGE/usr/bin" "$STAGE/usr/share/doc/badbitch" \
         "$STAGE/usr/share/applications" "$STAGE/usr/share/icons/hicolor/scalable/apps"

install -m0755 target/release/badbitch          "$STAGE/usr/bin/badbitch"
install -m0755 target/release/badbitch-gui      "$STAGE/usr/bin/badbitch-gui"
install -m0755 packaging/badbitch-setup         "$STAGE/usr/bin/badbitch-setup"
install -m0644 README.md                        "$STAGE/usr/share/doc/badbitch/README.md"
install -m0644 LICENSE                           "$STAGE/usr/share/doc/badbitch/copyright"
install -m0644 packaging/badbitch-rs.desktop    "$STAGE/usr/share/applications/badbitch-rs.desktop"
install -m0644 packaging/badbitch-rs.svg        "$STAGE/usr/share/icons/hicolor/scalable/apps/badbitch-rs.svg"
install -m0755 packaging/postinst               "$STAGE/DEBIAN/postinst"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: badbitch
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Maintainer: pfffiddy <paul.foster.sou@gmail.com>
Depends: dnsutils, whois, libimage-exiftool-perl, graphviz, python3, ca-certificates, curl, libgl1, libxkbcommon0, libwayland-client0, libx11-6, libxcursor1, libxi6, libxrandr2
Recommends: pipx, docker.io
Description: Full-spectrum OSINT agent (Rust) + GUI
 badbitch-rs drives a local LLM via Ollama through a tool-calling loop to build
 OSINT dossiers on authorized targets from public records and openly accessible
 sources, then saves/exports the result (incl. Maltego/Graphviz/Neo4j).
 .
 Ships the 'badbitch' CLI and the 'badbitch-gui' desktop control panel (launcher
 in your applications menu). After install, run 'badbitch-setup' to install
 Ollama, pull the model, add the optional OSINT CLIs, and write a config.
EOF

echo "[build-deb] dpkg-deb --build"
dpkg-deb --root-owner-group --build "$STAGE"

OUT="target/deb/badbitch_${VERSION}_${ARCH}.deb"
echo "[build-deb] built: $OUT"
echo "Install with:  sudo apt install ./$OUT   (or: sudo dpkg -i ./$OUT && sudo apt -f install)"
