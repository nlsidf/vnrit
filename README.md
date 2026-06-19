# vnrit — Lightweight X11 WebRTC Streaming Server

vnrit streams an X11 desktop to one or more browsers over **WebRTC**, with
low-latency keyboard and mouse input forwarding. Designed for ARM Linux
environments (Termux, Raspberry Pi, etc.) where hardware resources are
constrained.

```
┌───────────────────────────────────────────────────────────────┐
│  X11 Server  ──→  ximagesrc  ──→  videoconvert  ──→  encoder  │
│  (Xvnc/Xvfb)             GStreamer pipeline              │
│                                                            │
│                     ┌── WebSocket (signaling + input)      │
│  Browser  ←──WebRTC──┤                                     │
│                     └── ICE/STUN/TURN (p2p media)          │
└───────────────────────────────────────────────────────────────┘
```

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
git clone <repo-url> && cd vnrit
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
      --codec <CODEC>        Video encoder: openh264, h264, vp8, vp9
                              [default: openh264]
      --framerate <FPS>      Capture framerate [default: 24]
      --bitrate <KBPS>       Target bitrate in kbps [default: 1000]
      --height <PX>          Downscale height (0 = native) [default: 0]
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
```

## Codec Comparison

Measured on Snapdragon 835 (Adreno 540) at 720p 500 kbps with a connected
client:

| Codec | Element | RSS | Type |
|-------|---------|-----|------|
| openh264 | `openh264enc` | ~50 MB | Software H.264 (Cisco) |
| **h264** | **`mcenc`** | **~48 MB** | **Hardware H.264 (MediaCodec)** |
| vp8 | `vp8enc` | ~64 MB | Software VP8 (libvpx) |
| vp9 | `vp9enc` | ~64 MB | Software VP9 (libvpx) |

The hardware H.264 encoder (`mcenc`) uses the GPU's dedicated video encoding
block, consuming the least CPU and memory. It is the recommended option on
devices that support it.

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
ximagesrc → videoconvert → queue → capsfilter
                                        ↓ (optional)
                              videoscale → capsfilter (--height)
                                        ↓
                              encoder → payloader → webrtcbin
```

If PulseAudio is detected, an audio pipeline is also added:

```
pulsesrc → audio/x-raw → opusenc → rtpopuspay → webrtcbin
```

### Input Protocol

Keyboard and mouse events are sent from the browser to the server over the
same WebSocket used for WebRTC signaling. Messages are CSV lines:

```
mouse,<x>,<y>,<button>,<pressed>
  x/y     = absolute pixel coordinates on the display
  button  = 1 (left), 2 (middle), 3 (right)
  pressed = 1 (down), 0 (up)

key,<keycode>,<pressed>
  keycode = X11 keysym (see /usr/include/X11/keysymdef.h)
  pressed = 1 (down), 0 (up)
```

### Frontend

The built-in web UI (`src/index.html`) provides:
- WebRTC video rendering via `RTCPeerConnection`
- Touch-to-mouse translation: one-finger move, two-finger scroll, tap = click,
  long-press = right-click
- Keyboard input via `onkeydown`/`onkeyup`
- Auto-reconnection on disconnect

## WebSocket Signaling

The signaling protocol uses JSON messages over the same WebSocket:

```
Client → Server:  {"type":"ready"}                        # ready to connect
Server → Client:  {"type":"offer","sdp":"..."}             # SDP offer
Client → Server:  {"type":"answer","sdp":"..."}            # SDP answer
Both:             {"type":"ice","candidate":"...","sdp_mline_index":0}  # ICE candidates
```

After signaling completes, the WebSocket continues to carry input CSV lines.

## Notes

- Each browser tab creates a separate WebRTC pipeline (no multi-viewer
  sharing yet). Multiple viewers work simultaneously.
- Requires a running X11 server (Xvnc, Xvfb, or real X).
- On Termux, the X socket path is auto-detected.
- Audio requires PulseAudio running on the system.
- Stale wineserver processes with ESYNC on Termux can cause
  `virtual_setup_exception` crashes — clear them with `kill -9` if needed.

## License

MIT
