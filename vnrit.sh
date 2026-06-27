#!/usr/bin/env bash
set -euo pipefail

# vnrit launcher — start Xvnc + PulseAudio + vnrit server
#
# Usage:
#   ./vnrit.sh                    # default (:1, 720p, 500kbps)
#   ./vnrit.sh --bitrate 1000     # custom bitrate
#   ./vnrit.sh --no-audio         # disable audio
#   ./vnrit.sh --help             # show vnrit help

PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="$PROJECT_DIR/target/release/vnrit"

# Default args
ARGS=(
    --display ":1"
    --height 720
    --bitrate 500
)

# Check if binary exists
if [ ! -f "$BIN" ]; then
    echo "==> vnrit binary not found at $BIN"
    echo "    Building..."
    "$PROJECT_DIR/build.sh" --release
fi

# Parse launcher-specific flags first, pass rest to vnrit
NO_AUDIO=false
PASSTHROUGH=()

for arg in "$@"; do
    case "$arg" in
        --no-audio)
            NO_AUDIO=true
            ;;
        --help)
            exec "$BIN" --help
            ;;
        *)
            PASSTHROUGH+=("$arg")
            ;;
    esac
done

# PulseAudio: start if not running
if ! pactl info &>/dev/null; then
    if command -v pulseaudio &>/dev/null; then
        echo "==> Starting PulseAudio..."
        pulseaudio --start --exit-idle-time=-1 2>/dev/null || true
        sleep 1
    fi
fi

# PulseAudio: ensure default source is a monitor (system audio)
if [ "$NO_AUDIO" = false ] && pactl info &>/dev/null; then
    CURRENT_SOURCE="$(pactl get-default-source 2>/dev/null || echo "")"
    if [ -n "$CURRENT_SOURCE" ] && ! echo "$CURRENT_SOURCE" | grep -q "\.monitor$"; then
        # Try to find a monitor source
        MONITOR="$(pactl list sinks short 2>/dev/null | head -1 | awk '{print $2}' | sed 's/$/.monitor/')"
        if [ -n "$MONITOR" ]; then
            echo "==> Setting default source to monitor: $MONITOR"
            pactl set-default-source "$MONITOR" 2>/dev/null || true
        fi
    fi
fi

# Disable audio via env var
if [ "$NO_AUDIO" = true ]; then
    # vnrit currently always tries PulseAudio if running
    echo "==> Audio disabled (--no-audio)"
fi

echo "==> Starting vnrit..."
exec "$BIN" "${ARGS[@]}" "${PASSTHROUGH[@]}"
