#!/usr/bin/env bash
# macOS / Linux counterpart of prepare-bundle.ps1.
# Downloads a portable Node.js matching the build host and assembles the
# OpenClaw npm runtime + node binary into desktop/src-tauri/resources/.
#
# Prereq: openclaw-runtime2/node_modules/openclaw must already exist
#         (CI runs `npm install openclaw@latest` in openclaw-runtime2/ first,
#          which fetches the platform-correct native binaries).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME_ROOT="$ROOT/openclaw-runtime2"
RUNTIME="$RUNTIME_ROOT/node_modules/openclaw"
RES="$ROOT/desktop/src-tauri/resources"
PORT="$ROOT/portable-node"
NODE_VER="${NODE_VERSION:-$(node -v | sed 's/^v//')}"

echo "==> Pre-flight"
[ -d "$RUNTIME" ] || { echo "ERROR: $RUNTIME missing — run 'npm install openclaw@latest' in openclaw-runtime2/ first"; exit 1; }
[ -d "$RUNTIME/dist/control-ui" ] || { echo "ERROR: $RUNTIME/dist/control-ui missing (unexpected npm layout)"; exit 1; }
echo "    OpenClaw runtime: $RUNTIME"
echo "    Node version to bundle: $NODE_VER"

OS="$(uname -s)"; ARCH="$(uname -m)"
case "$OS" in
  Darwin)
    if [ "$ARCH" = "arm64" ]; then NA=arm64; else NA=x64; fi
    PKG="node-v${NODE_VER}-darwin-${NA}"; TAR="${PKG}.tar.gz" ;;
  Linux)
    PKG="node-v${NODE_VER}-linux-x64"; TAR="${PKG}.tar.xz" ;;
  *) echo "ERROR: unsupported OS '$OS' (use prepare-bundle.ps1 on Windows)"; exit 1 ;;
esac

echo "==> Acquire portable Node ($PKG)"
mkdir -p "$PORT"; cd "$PORT"
if [ ! -d "$PKG" ]; then
  echo "    downloading https://nodejs.org/dist/v${NODE_VER}/${TAR}"
  curl -fsSLO "https://nodejs.org/dist/v${NODE_VER}/${TAR}"
  tar xf "$TAR"
fi

echo "==> Assemble $RES"
mkdir -p "$RES/node"
cp "$PORT/$PKG/bin/node" "$RES/node/node"
chmod +x "$RES/node/node"
echo "    node: $(du -m "$RES/node/node" | cut -f1) MB"

# openclaw package + its hoisted sibling deps (npm flat layout)
rsync -a --delete --exclude='.git' --exclude='test' --exclude='__screenshots__' "$RUNTIME/" "$RES/openclaw/"
rsync -a --delete --exclude='openclaw' --exclude='.git' "$RUNTIME_ROOT/node_modules/" "$RES/openclaw/node_modules/"

echo "==> Done. resources total: $(du -sm "$RES" | cut -f1) MB"
echo "    Next: (cd desktop && pnpm tauri build)"
