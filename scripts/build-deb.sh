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

echo "[build-deb] cargo build --release --bin badbitch"
cargo build --release --bin badbitch

echo "[build-deb] staging $STAGE"
rm -rf "$STAGE"
mkdir -p "$STAGE/DEBIAN" "$STAGE/usr/bin" "$STAGE/usr/share/doc/badbitch"

install -m0755 target/release/badbitch        "$STAGE/usr/bin/badbitch"
install -m0755 packaging/badbitch-setup        "$STAGE/usr/bin/badbitch-setup"
install -m0644 README.md                       "$STAGE/usr/share/doc/badbitch/README.md"
install -m0644 LICENSE                          "$STAGE/usr/share/doc/badbitch/copyright"
install -m0755 packaging/postinst              "$STAGE/DEBIAN/postinst"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: badbitch
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Maintainer: pfffiddy <paul.foster.sou@gmail.com>
Depends: dnsutils, whois, libimage-exiftool-perl, graphviz, python3, ca-certificates, curl
Recommends: pipx, docker.io
Description: Full-spectrum OSINT agent (Rust)
 badbitch-rs drives a local LLM via Ollama through a tool-calling loop to build
 OSINT dossiers on authorized targets from public records and openly accessible
 sources, then saves/exports the result (incl. Maltego/Graphviz).
 .
 After install, run 'badbitch-setup' to install Ollama, pull the model, add the
 optional OSINT CLIs (sherlock, holehe, theHarvester), and write a config.
EOF

echo "[build-deb] dpkg-deb --build"
dpkg-deb --root-owner-group --build "$STAGE"

OUT="target/deb/badbitch_${VERSION}_${ARCH}.deb"
echo "[build-deb] built: $OUT"
echo "Install with:  sudo apt install ./$OUT   (or: sudo dpkg -i ./$OUT && sudo apt -f install)"
