#!/usr/bin/env bash
set -euo pipefail

# Build script for vnrit — X11 WebRTC streaming server
#
# Prerequisites:
#   - Rust toolchain
#   - cmake (for shiguredo_libyuv build)
#   - PulseAudio development headers (for audio capture)
#   - X11 development headers (for x11rb)
#
# Usage:
#   ./build.sh              # debug build
#   ./build.sh --release    # release build
#   ./build.sh --check      # cargo check (debug)
#   ./build.sh --check --release  # cargo check (release)

PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$PROJECT_DIR"

# Set CMAKE env var — shiguredo_libyuv's build.rs uses it to skip cmake download
export CMAKE="${CMAKE:-$(command -v cmake || true)}"
if [ -z "$CMAKE" ]; then
    echo "Error: cmake not found. Install cmake first."
    echo "  Debian/Ubuntu: apt install cmake"
    echo "  Termux:        pkg install cmake"
    exit 1
fi

echo "==> vnrit build script"
echo "    cmake: $CMAKE"
echo "    target: $PROJECT_DIR"

CHECK_MODE=false
RELEASE_FLAG=""

case "${1:-}" in
    --check)
        CHECK_MODE=true
        shift
        if [ "${1:-}" = "--release" ]; then
            RELEASE_FLAG="--release"
            echo "    check: release"
        else
            echo "    check: debug"
        fi
        ;;
    --release)
        RELEASE_FLAG="--release"
        echo "    profile: release"
        ;;
    *)
        echo "    profile: debug"
        ;;
esac

if [ "$CHECK_MODE" = true ]; then
    echo "==> Running cargo check $RELEASE_FLAG ..."
    cargo check $RELEASE_FLAG
    echo "==> Check complete (exit code: $?)"
else
    cargo build $RELEASE_FLAG
    BIN_PATH="$PROJECT_DIR/target/${RELEASE_FLAG:+release}${RELEASE_FLAG:-debug}/vnrit"
    if [ -f "$BIN_PATH" ]; then
        echo "==> Build complete: $BIN_PATH"
        ls -lh "$BIN_PATH"
    else
        echo "==> Build failed"
        exit 1
    fi
fi
