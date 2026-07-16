#!/bin/sh
# launch.sh — MCPB bundle launcher for automatised-pipeline.
#
# Dispatches to the pre-built binary for the current platform.
# Called by the MCP host (Claude Desktop, Claude Code) as the server
# entry point when the .mcpb bundle is installed.
#
# Platform matrix (matches the release workflow target_tag names):
#   Darwin  arm64   → bin/macos-aarch64/automatised-pipeline
#   Linux   x86_64  → bin/linux-x86_64/automatised-pipeline
#   Linux   aarch64 → bin/linux-aarch64/automatised-pipeline
#
# Usage (by MCP host): ${__dirname}/launch.sh
# The MCP host sets __dirname to the extracted bundle root.

set -e

# Resolve bundle root: use __dirname if set by the MCP host, otherwise
# derive it from $0 (works when run directly for testing).
if [ -n "${__dirname:-}" ]; then
    BUNDLE_DIR="$__dirname"
else
    BUNDLE_DIR="$(cd "$(dirname "$0")" && pwd)"
fi

OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}-${ARCH}" in
    Darwin-arm64)
        BIN="$BUNDLE_DIR/bin/macos-aarch64/automatised-pipeline"
        ;;
    Linux-x86_64)
        BIN="$BUNDLE_DIR/bin/linux-x86_64/automatised-pipeline"
        ;;
    Linux-aarch64)
        BIN="$BUNDLE_DIR/bin/linux-aarch64/automatised-pipeline"
        ;;
    *)
        echo "automatised-pipeline: unsupported platform: ${OS}-${ARCH}" >&2
        echo "Supported: Darwin-arm64, Linux-x86_64, Linux-aarch64" >&2
        exit 1
        ;;
esac

if [ ! -f "$BIN" ]; then
    echo "automatised-pipeline: binary not found at: $BIN" >&2
    echo "The .mcpb bundle may be corrupt. Re-install from:" >&2
    echo "  https://github.com/cdeust/automatised-pipeline/releases" >&2
    exit 1
fi

if [ ! -x "$BIN" ]; then
    chmod +x "$BIN"
fi

exec "$BIN" "$@"
