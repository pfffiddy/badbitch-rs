#!/usr/bin/env bash
# One-shot installer from a source checkout: build the .deb, install it (pulling apt
# dependencies), then run badbitch-setup (Ollama + model + optional CLIs + config).
#
#   git clone https://github.com/pfffiddy/badbitch-rs && cd badbitch-rs
#   ./scripts/install.sh
#
# Honors the same env knobs as badbitch-setup:
#   BADBITCH_MODEL=...        pull/configure a different model
#   BADBITCH_SKIP_MODEL=1     skip the (large) model download
#   BADBITCH_SKIP_SETUP=1     install the .deb only; don't run badbitch-setup
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

command -v cargo    >/dev/null || { echo "need a Rust toolchain (https://rustup.rs)"; exit 1; }
command -v dpkg-deb >/dev/null || { echo "need dpkg-deb (Debian/Ubuntu)"; exit 1; }

SUDO=""; [ "$(id -u)" = "0" ] || SUDO="sudo"

bash scripts/build-deb.sh

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
ARCH="$(dpkg --print-architecture)"
DEB="target/deb/badbitch_${VERSION}_${ARCH}.deb"

echo "[install] installing $DEB (with apt dependencies)…"
if command -v apt >/dev/null 2>&1; then
  $SUDO apt install -y "./$DEB"
else
  $SUDO dpkg -i "$DEB" || $SUDO apt-get -f install -y
fi

if [ "${BADBITCH_SKIP_SETUP:-0}" = "1" ]; then
  echo "[install] done (skipped badbitch-setup). Run 'badbitch-setup' yourself to finish."
else
  echo "[install] running badbitch-setup…"
  badbitch-setup
fi
