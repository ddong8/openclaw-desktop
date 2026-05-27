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
# Start from a clean slate so stale state from a partial previous run can't
# poison rsync's --delete pass (this bit us on macOS BSD rsync, which left the
# 2nd-rsync dest with only tokenjuice/ + .package-lock.json).
rm -rf "$RES/node" "$RES/openclaw"
mkdir -p "$RES/node"
cp "$PORT/$PKG/bin/node" "$RES/node/node"
chmod +x "$RES/node/node"
echo "    node: $(du -m "$RES/node/node" | cut -f1) MB"

# Copy the openclaw package — including any bundled node_modules inside it.
# (openclaw 2026.5.22 ships npm-shrinkwrap.json in its tarball, which makes
# npm install most deps INSIDE openclaw/node_modules/ rather than hoisting them
# to the top-level openclaw-runtime2/node_modules/ — so the bulk of runtime
# deps actually rides on this first rsync, NOT the second.)
rsync -a \
  --exclude='/.git' \
  --exclude='/test' \
  --exclude='/__screenshots__' \
  "$RUNTIME/" "$RES/openclaw/"
echo "    openclaw package copied ($(du -sm "$RES/openclaw" | cut -f1) MB), node_modules entries inside: $(ls "$RES/openclaw/node_modules" 2>/dev/null | wc -l | tr -d ' ')"

# Merge in anything hoisted to the top level (older openclaw versions hoist;
# newer ones bundle). No --delete: we want UNION of both layouts, not replace.
# Anchored exclude /openclaw so we don't recurse into the openclaw package
# itself (it's already at $RES/openclaw, not under node_modules).
mkdir -p "$RES/openclaw/node_modules"
rsync -a --ignore-existing \
  --exclude='/openclaw' \
  --exclude='/.git' \
  "$RUNTIME_ROOT/node_modules/" "$RES/openclaw/node_modules/"
echo "    after hoisted merge: $(ls "$RES/openclaw/node_modules" | wc -l | tr -d ' ') entries in node_modules/"

# Verify the critical runtime deps actually landed. If json5 is missing the
# embedded Node will crash with ERR_MODULE_NOT_FOUND on first `config patch`
# call — fail the build loudly here instead of shipping a broken bundle.
for dep in json5 tokenjuice @mistralai/mistralai; do
  if [ ! -d "$RES/openclaw/node_modules/$dep" ]; then
    echo "ERROR: required dep '$dep' missing from $RES/openclaw/node_modules/"
    echo "       top-level node_modules entries actually present:"
    ls "$RES/openclaw/node_modules/" | sed 's/^/         /'
    exit 1
  fi
done
echo "    verified critical deps: json5, tokenjuice, @mistralai/mistralai"

# Strip compile-time-only files (TS decls + sourcemaps). Node never loads these
# at runtime — not a feature cut — and it shortens deep SDK paths for Windows.
echo "==> Strip .d.ts / .map (compile-time only; runtime unaffected)"
find "$RES/openclaw" -type f \( -name '*.map' -o -name '*.d.ts' -o -name '*.d.cts' -o -name '*.d.mts' \) -delete 2>/dev/null || true

# Drop nested @mistralai duplicates. pi-coding-agent keeps its own copy whose
# very long Mistral SDK filenames break Windows NSIS MAX_PATH. The top-level
# @mistralai (same 2.2.1, pinned via overrides) resolves for it via normal Node
# module lookup → zero functional impact.
TOP_MISTRAL="$RES/openclaw/node_modules/@mistralai"
find "$RES/openclaw" -type d -name '@mistralai' 2>/dev/null | while read -r d; do
  if [ "$d" != "$TOP_MISTRAL" ]; then echo "    drop nested $d"; rm -rf "$d"; fi
done

echo "==> Done. resources total: $(du -sm "$RES" | cut -f1) MB"
echo "    Next: (cd desktop && pnpm tauri build)"
