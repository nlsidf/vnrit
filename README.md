# vnrit — Lightweight X11 WebRTC Streaming Server

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

vnrit streams an X11 desktop to one or more browsers over **WebRTC** with low-latency keyboard and mouse input forwarding. Designed for ARM Linux environments (Termux, Raspberry Pi, etc.) where hardware resources are constrained.

```
┌──────────────────────────────────────────────────────────────────┐
│  X11 Server  ──→  ximagesrc  ──→  videoconvert  ──→  encoder   │
│  (Xvnc/Xvfb)              GStreamer pipeline                    │
│                                                                │
│                     ┌── WebSocket (signaling + input)          │
│  Browser  ←──WebRTC──┤                                          │
│                     └── ICE/STUN/TURN (p2p media)              │
└──────────────────────────────────────────────────────────────────┘
```

## Features

- **WebRTC streaming** — low-latency video via `webrtcbin` with adaptive quality
- **Multiple codecs** — openh264 (default), Android MediaCodec H.264, VP8, VP9
- **Audio support** — Opus audio via PulseAudio (auto-detected)
- **Input forwarding** — keyboard + mouse injected directly via X11 XTest extension (no `xdotool`)
- **Touch-to-mouse** — trackpad mode: tap, long-press, drag, scroll — all on a touchscreen
- **Browser cursor overlay** — synced cursor position without encoding the cursor into video
- **Optional token-auth** — passwordless access control with cookie-based sessions

## Quick Start

```bash
# Recommended: hardware H.264, 720p, 500 kbps
vnrit --codec h264 --height 720 --bitrate 500

# Open the printed URL (default http://0.0.0.0:8080) in a browser.
# Tap/click to send mouse and keyboard events back.
```

## Installation

### Prerequisites

- **Rust** 1.70+ (`rustup` or system package)
- **GStreamer** 1.22+ with plugins:
  - `gstreamer`, `gst-plugins-base`, `gst-plugins-good`, `gst-plugins-bad`
  - `gst-plugins-ugly` (for x264enc, optional)
  - **openh264** (`gst-openh264` or system package)
  - VP8/VP9 support via `gst-plugins-good` (libvpx)
  - Android MediaCodec encoder (`mcenc` plugin, Termux only)
- **X11 server** (Xvnc, Xvfb, or real X display)
- **pkg-config** (for GStreamer build linkage)

### Build

```bash
git clone https://github.com/nlsidf/vnrit.git
cd vnrit
cargo build --release
./target/release/vnrit --help
```

### Termux (Android)

On Termux, install dependencies via apt:

```bash
pkg install rust gstreamer gst-plugins-base gst-plugins-good \
  gst-plugins-bad gst-plugins-ugly openh264 mcenc x11-repo \
  tur-repo pulseaudio
```

The X11 display connection uses the Unix socket at
`/data/data/com.termux/files/usr/tmp/.X11-unix/X<display>`. vnrit
auto-detects this path.

## Usage

```
Usage: vnrit [OPTIONS]

Options:
      --display <DISPLAY>    X11 display to capture [default: :1]
  -p, --port <PORT>          HTTP/WebSocket port [default: 8080]
      --codec <CODEC>        Video encoder: openh264, h264, vp8, vp9 [default: openh264]
      --framerate <FPS>      Capture framerate [default: 24]
      --bitrate <KBPS>       Target bitrate in kbps [default: 1000]
      --height <PX>          Downscale height (0 = native) [default: 0]
      --token <TOKEN>        Authentication token (optional, for access control)
  -h, --help                 Print detailed help
  -V, --version              Print version
```

### Examples

```bash
# Default: openh264 at desktop resolution, 1 Mbps
vnrit

# Recommended: hardware H.264, 720p, 500 kbps
vnrit --codec h264 --height 720 --bitrate 500

# Low bandwidth: VP9, 480p, 300 kbps
vnrit --codec vp9 --height 480 --bitrate 300

# High quality: no scaling, 2 Mbps
vnrit --bitrate 2000

# Custom display and port
vnrit --display :0 -p 9090

# With token authentication
vnrit --token mysecret

# Full setup with auth + recommended codec
vnrit --token abc123 --codec h264 --height 720 --bitrate 500
```

### Token Authentication

When `--token <TOKEN>` is specified, all HTTP and WebSocket connections must present the token.

**How it works:**

```
Browser → http://host:8080/?token=xxx    # initial visit with token
   ↓
Server: validates ?token=xxx, sets HttpOnly cookie
   ↓
Browser → ws://host:8080/ws?token=xxx    # WebSocket upgrade (or via cookie)
   ↓
Server: validates token → WebRTC streaming begins
```

- **First visit**: append `?token=<value>` to the URL
- **Subsequent visits**: the browser's cookie handles authentication automatically
- **No token**: server returns HTTP 401 Unauthorized
- **No `--token` specified**: server operates in open-access mode (no auth)

## Codec Comparison

Measured on Snapdragon 835 (Adreno 540) at 720p 500 kbps with a connected
client:

| Codec | Element | RSS | Type | Notes |
|-------|---------|-----|------|-------|
| openh264 | `openh264enc` | ~50 MB | Software H.264 (Cisco) | Default, good balance |
| **h264** | **`mcenc`** | **~48 MB** | **Hardware H.264** | **Lowest CPU/memory** |
| vp8 | `vp8enc` | ~64 MB | Software VP8 (libvpx) | Higher memory |
| vp9 | `vp9enc` | ~64 MB | Software VP9 (libvpx) | Better compression |

The hardware H.264 encoder (`mcenc`) uses the GPU's dedicated video encoding
block, consuming the least CPU and memory.

### Bitrate Guidelines (720p @ 24 fps)

| Bitrate | Quality | Use Case |
|---------|---------|----------|
| 300 kbps | Low | Text terminals, SSH-like |
| **500 kbps** | **Good** | **GUI desktops (recommended)** |
| 1000 kbps | High | Default, smooth desktop |
| 2000+ kbps | Near-lossless | Static content, reading |

## Architecture

### Pipeline

The GStreamer pipeline is constructed dynamically per WebRTC connection:

```
Video:
ximagesrc → videoconvert → queue → capsfilter
                                        ↓ (optional)
                              videoscale → capsfilter (--height)
                                        ↓
                              encoder → payloader → webrtcbin

Audio (if PulseAudio detected):
pulsesrc → audio/x-raw (mono/48kHz) → opusenc → rtpopuspay → webrtcbin
```

### Input Protocol

Keyboard and mouse events are sent from the browser to the server over the
same WebSocket used for WebRTC signaling. Messages are CSV lines for minimal
overhead:

| Command | Format | Description |
|---------|--------|-------------|
| Mouse move (relative) | `mr,dx,dy` | Relative cursor movement |
| Mouse move (absolute) | `ma,x,y` | Absolute cursor position |
| Mouse down | `md,button` | Button press (1=left, 2=middle, 3=right) |
| Mouse up | `mu,button` | Button release |
| Scroll | `ms,deltaY` | Scroll wheel (positive=down, negative=up) |
| Key down | `kd,code` | KeyboardEvent.code press |
| Key up | `ku,code` | KeyboardEvent.code release |

Input is injected directly into X11 via the **XTest extension** — no `xdotool`,
no subprocess, no string parsing overhead.

### Frontend

The built-in web UI provides:

- **WebRTC video rendering** via `RTCPeerConnection`
- **Browser cursor overlay** — a CSS-rendered cursor synced with the server position, so the system cursor (and its latency) is never encoded in the video
- **Input throttling** — relative mouse movements are accumulated and flushed at ~50fps to avoid flooding X11
- **Touch-to-mouse translation**:
  - One-finger slide → relative cursor move
  - Tap (<300ms) → left click
  - Long-press (>700ms) → right click
  - Long-press + vertical move → scroll
  - Double-tap (<400ms) + hold + move → drag selection
- **Keyboard forwarding** — all keyboard events mapped to X11 keysyms
- **Auto-reconnection** — exponential backoff (1s → 30s max)
- **Negotiation watchdog** — 15s timeout on WebRTC connection

### WebSocket Signaling

```
Client → Server:  {"type":"ready"}                          ready to connect
Server → Client:  {"type":"offer","sdp":"..."}              SDP offer
Client → Server:  {"type":"answer","sdp":"..."}             SDP answer
Both:             {"type":"ice","candidate":"...","sdp_mline_index":0}
```

After signaling completes, the WebSocket switches to carrying input CSV lines
and cursor position updates (`{"type":"cursor","x":<x>,"y":<y>}`).

## Security

### Token Authentication

`--token <TOKEN>` provides **passwordless access control** suitable for
internal/VPN networks:

| Measure | Detail |
|---------|--------|
| Cookie | `HttpOnly` + `SameSite=Lax`, 24h expiry |
| No server-side session | Stateless, no session leakage |
| Token in query param | Auto-converted to cookie on first successful auth |
| No token set | Server operates in open-access mode |

**Limitations:**
- Token is transmitted in plaintext on HTTP — use behind VPN or HTTPS for production
- Token is static — rotate by restarting with a new `--token` value
- No rate-limiting on auth attempts — use a long random token (>16 chars)
- Cookie `Secure` flag not set (HTTP-only environments)

> **Recommendation**: Pair with Tailscale/WireGuard VPN, or put behind nginx
> HTTPS reverse proxy for public-facing deployments.

## Notes

- Each browser tab creates a separate WebRTC pipeline (no multi-viewer
  sharing yet). Multiple viewers work simultaneously.
- Requires a running X11 server (Xvnc, Xvfb, or real X).
- On Termux, the X socket path is auto-detected.
- Audio requires PulseAudio running on the system.
- Stale wineserver processes with ESYNC on Termux can cause
  `virtual_setup_exception` crashes — clear them with `kill -9` if needed.
- Termux linker namespace restrictions require `LD_PRELOAD` tricks for
  GPU-accelerated encoding — see the [proton11 guide](https://github.com/nlsidf/proton11) for details.

## License

MIT
