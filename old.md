# vnrit — Pure Rust X11 WebRTC Streaming Server

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**vnrit** streams an X11 desktop to browsers over **WebRTC** with low-latency keyboard/mouse input forwarding. Built entirely in Rust — no GStreamer, no FFmpeg, no system codec dependencies.

```
┌──────────────────────────────────────────────────────────────────┐
│                        vnrit Server                              │
│                                                                  │
│  X11 Server ──→ SHM Capture ──→ libyuv I420 ──→ openh264 H.264  │
│  (Xvnc/Xvfb)     (MIT-SHM)        (SIMD ARM NEON)  (Screen RT)  │
│                                                                  │
│  PulseAudio ──→ Opus Encoder ───→ WebRTC                         │
│  (monitor src)     (libopus)       (webrtc-rs)                   │
│                                                                  │
│                        ↓                                         │
│  Browser ←──WebRTC─── WebSocket (signaling + input) → XTest      │
└──────────────────────────────────────────────────────────────────┘
```

## Features

- **Pure Rust** — zero GStreamer/FFmpeg dependency, ~6.6 MB release binary
- **WebRTC H.264** — openh264 encoder with screen content optimization
- **SIMD color conversion** — Google libyuv via ARM NEON
- **4-stage pipeline** — capture → convert → encode → send, fully parallel
- **Audio support** — PulseAudio → Opus (48kHz stereo, 20ms frames)
- **X11 input injection** — keyboard + mouse via XTest extension
- **MIT-SHM capture** — zero-copy shared memory screen capture
- **Browser cursor overlay** — CSS cursor synced separately, never encoded in video
- **Dual X11 connections** — separate sockets for capture and input (no lock contention)
- **Memory pool reuse** — zero per-frame allocations in steady state
- **Token authentication** — optional passwordless access control
- **Auto-reconnection** — exponential backoff (1s → 30s)
- **Touch-to-mouse** — tap, long-press, drag, scroll on touchscreens
- **Virtual keyboard** — on-screen keyboard for mobile/touch devices

## Quick Start

```bash
# Build
./build.sh --release

# Run (default: X11 :1, port 8080, 1000 kbps)
target/release/vnrit --display :1

# Recommended settings for remote access
target/release/vnrit --display :1 --height 720 --bitrate 500

# Open http://<host>:8080 in a browser
```

## Prerequisites

| Dependency | Purpose | Install |
|-----------|---------|---------|
| Rust 1.82+ | Compiler | `rustup` or system package |
| cmake 3.20+ | libyuv build | `apt install cmake` / `pkg install cmake` |
| X11 server | Display to capture | Xvnc, Xvfb, or real X server |
| PulseAudio | Audio capture (optional) | `pulseaudio` server running |

### Termux (Android)

```bash
pkg install rust cmake x11-repo tur-repo pulseaudio
```

The X11 socket path at `/data/data/com.termux/files/usr/tmp/.X11-unix/X<display>` is auto-detected.

## Build

```bash
# Using build script
./build.sh --release

# Manual
CMAKE=$(which cmake) cargo build --release
```

The `CMAKE` environment variable is required — `shiguredo_libyuv`'s build system
needs to find cmake on Android. Without it, the build.rs attempts to download
a prebuilt cmake binary, which fails on Termux.

### Git mirror for libyuv source

`shiguredo_libyuv` clones libyuv from `chromium.googlesource.com` during build.
If that's blocked on your network, configure a mirror:

```bash
git config --global url."https://gitee.com/zhang_wang_wu/libyuv".insteadOf \
  "https://chromium.googlesource.com/libyuv/libyuv"
```

## Usage

```
Usage: vnrit [OPTIONS]

Options:
      --display <DISPLAY>    X11 display to capture [default: :1]
  -p, --port <PORT>          HTTP/WebSocket listen port [default: 8080]
      --framerate <FPS>      Capture framerate [default: 24]
      --bitrate <KBPS>       Target bitrate in kbps [default: 1000]
      --height <PX>          Downscale height (0 = native) [default: 0]
      --stun <URL>           STUN server URL (empty to disable) [default: stun:stun.cloudflare.com:3478]
      --token <TOKEN>        Authentication token (optional)
      --log-level <LEVEL>    Log level: off, error, warn, info, debug, trace [default: warn]
  -h, --help                 Print help
```

### Examples

```bash
# Basic: stream :1 at native resolution
vnrit

# Stream at 720p, 500 kbps (recommended for remote access)
vnrit --height 720 --bitrate 500

# Higher quality for LAN
vnrit --bitrate 2000

# Custom display and port
vnrit --display :0 -p 9090

# With authentication
vnrit --token mysecret

# Debug logging
vnrit --log-level debug

# Disable STUN (LAN-only)
vnrit --stun ""
```

## Bitrate Guidelines (720p @ 24 fps)

| Bitrate | Quality | Use Case |
|---------|---------|----------|
| 300 kbps | Low | Text terminals, SSH-like |
| **500 kbps** | **Good** | **GUI desktops (recommended)** |
| 1000 kbps | High | Default, smooth desktop |
| 2000+ kbps | Near-lossless | Static content, reading |

## Architecture

### Video Pipeline

```
┌──────────┐    ┌──────────┐    ┌──────────┐    ┌──────────┐
│ Capture  │───→│ Convert  │───→│ Encode   │───→│ Send     │
│ SHM/X11  │    │ libyuv   │    │ openh264 │    │ WebRTC   │
│ BGRA     │    │ I420     │    │ H.264    │    │ track    │
└──────────┘    └──────────┘    └──────────┘    └──────────┘
  spawn_         spawn_          spawn_          async
  blocking       blocking        blocking
```

- **Capture**: X11 MIT-SHM extension reads screen pixels into shared memory (zero-copy). Falls back to `get_image` if SHM unavailable.
- **Convert**: libyuv SIMD converts BGRA → I420. Supports I420-scaling for `--height` downscale.
- **Encode**: openh264 H.264 encoder with `ScreenContentRealTime` profile and configurable bitrate.
- **Send**: Asynchronously writes encoded frames to webrtc-rs `TrackLocalStaticSample`.

All 4 stages run in parallel connected by bounded channels (capacity 4). Each stage holds its own pre-allocated buffer pool.

### Audio Pipeline

```
┌──────────────┐    ┌──────────┐    ┌──────────┐
│ PulseAudio   │───→│ Opus     │───→│ WebRTC   │
│ Simple API   │    │ Encoder  │    │ track    │
│ PCM S16LE    │    │ 48kHz    │    │ 20ms     │
│ 3840 B/frame │    │ stereo   │    │ frames   │
└──────────────┘    └──────────┘    └──────────┘
```

Audio is captured from the **default PulseAudio sink monitor** (system audio output). Falls back to default source (microphone) if monitor detection fails.

### Input Protocol

Commands are sent from the browser over the WebSocket as CSV:

| Command | Format | Description |
|---------|--------|-------------|
| Mouse move (relative) | `mr,dx,dy` | Relative cursor movement |
| Mouse move (absolute) | `ma,x,y` | Absolute cursor position |
| Mouse down | `md,button` | Button press (1=left, 2=middle, 3=right) |
| Mouse up | `mu,button` | Button release |
| Scroll | `ms,deltaY` | Scroll wheel |
| Key down | `kd,code` | `KeyboardEvent.code` (e.g. `KeyA`, `Digit2`) |
| Key up | `ku,code` | Key release |

Keycodes are physical (not character-based), so keyboard layout handling is done by the X server. Shift+2 produces `@` on a US layout, regardless of the browser's locale.

### WebSocket Signaling

```
Client → Server:  {"type":"ready"}
Server → Client:  {"type":"offer","sdp":"..."}
Client → Server:  {"type":"answer","sdp":"..."}
Both:             {"type":"ice","candidate":"...", "sdp_mline_index":0}
```

### Frontend

The built-in web UI (`src/index.html`) provides:

- **WebRTC video** via `RTCPeerConnection` with H.264/Opus
- **CSS cursor overlay** — synced to server cursor position, never encoded in video
- **Relative input throttling** — accumulated via `requestAnimationFrame`, not `setInterval`
- **ResizeObserver** — real-time container resize tracking (no layout thrash)
- **Touch-to-mouse**: tap, long-press, drag, scroll
- **Virtual keyboard**: main/func/num layers with modifier latching
- **Auto-reconnection** with exponential backoff
- **Negotiation watchdog**: 15s timeout

### Cancellation & Cleanup

All pipeline tasks share a `CancellationToken`. On disconnect:
1. `cancel.cancel()` signals all tasks
2. Channel senders are dropped, waking blocked receivers
3. Each task checks cancellation and exits cleanly
4. `ice_forward` task exits naturally when its sender is dropped (no `abort()`)
5. All `spawn_blocking` handles are awaited

## Performance Optimizations

| Technique | Detail |
|-----------|--------|
| Memory pool reuse | Pre-allocated Vecs per pipeline stage, no per-frame allocation |
| Zero-copy capture | MIT-SHM shared memory (no X11 socket transfer for pixels) |
| SIMD color conversion | libyuv ARGBToI420 + I420Scale via ARM NEON |
| Dual X11 connections | Separate sockets for capture and input (no mutex) |
| I420-domain scaling | Scale in YUV space (1.5 B/px vs 4 B/px for ARGB) |
| `with_resize_uninit` | Skip zero-initialization for buffers immediately overwritten |
| `SyncSender::send()` | Condition-variable-based blocking (no busy-wait) |
| `CancellationToken` | Unified cancellation for blocking + async tasks |
| Atomic memory ordering | `Release`/`Acquire` for ARM weak memory model |
| `Release`/`Acquire` | Correctness on ARM (phone) vs x86 |
| Repeat frame on error | If capture fails, repeat last frame (prevents decoder crash) |
| Force keyframe on error | If encode fails, reset encoder state immediately |
| `try_send` instead of `blocking_send` | No thread pool deadlock risk |

## Resource Usage

Measured on Snapdragon 835 (Adreno 540) at 720p 500 kbps:

| Metric | Value |
|--------|-------|
| Binary size | ~6.6 MB (release, stripped) |
| Memory (steady) | ~50-80 MB RSS |
| CPU (video, 24 fps) | 2-3 cores at ~1.5 GHz |
| CPU (audio, 20ms frames) | <5% of one core |
| Network bandwidth | ~500 kbps video + ~40 kbps audio |

## Troubleshooting

### Connection fails

1. Check X11 server is running: `echo $DISPLAY`
2. Confirm XTest extension: `xdpyinfo | grep XTest`
3. Try direct connection: `vnrit --display :0 --stun ""`

### No audio

1. Verify PulseAudio is running: `pactl info`
2. Check default sink has a monitor: `pactl list sinks short`
3. Set default source to monitor: `pactl set-default-source <sink>.monitor`

### Build errors

| Error | Fix |
|-------|-----|
| `cmake not found` | `apt install cmake` / `pkg install cmake` |
| `libclang not found` | `apt install libclang-dev` / `pkg install libclang` |
| `audiopus_sys build.rs` | Already patched in `vendor/` — no action needed |
| `chromium.googlesource.com timeout` | Configure git mirror (see Build section) |

## License

MIT
