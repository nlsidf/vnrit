#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// Force mimalloc to return cached memory to the OS.
unsafe extern "C" {
    fn mi_collect(force: bool) -> ();
}

use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc as block_mpsc;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use futures_util::StreamExt;
use std::os::unix::net::UnixStream;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, Window};
use x11rb::protocol::shm::{self, Seg};
use x11rb::protocol::xtest;
use x11rb::protocol::xinput::{self, XIEventMask, EventMask};
use x11rb::protocol::Event;
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb_protocol::xauth::get_auth;
use std::os::unix::io::AsRawFd;

// ── webrtc-rs types ──
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceServer, RTCPeerConnectionIceEvent,
    RTCSessionDescription, RTCIceGatheringState, RTCPeerConnectionState,
    RTCIceCandidateInit, MediaEngine, register_default_interceptors, Registry,
    SettingEngine,
};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::{MediaStreamTrack, Track};
use webrtc::runtime;
use rtc_media::Sample;
use rtc::rtp_transceiver::rtp_sender::{
    RtpCodecKind, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters,
};
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS};
use rtc::statistics::StatsSelector;

// ── openh264 ──
use openh264::encoder::{Encoder, EncoderConfig, BitRate, FrameRate, UsageType, RateControlMode, IntraFramePeriod, Profile, Complexity};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;
use openh264_sys2::ENCODER_OPTION_BITRATE;


use bytes::Bytes;
use ice::network_type::NetworkType;
use rand::Rng;
use async_trait::async_trait;
use std::os::fd::IntoRawFd;

use vnrit_libyuv::{self, FilterMode, ImageSize, ArgbImage, I420Image, I420ImageMut};

// ── libblur: SIMD-accelerated fast blur (used for Y-plane unsharp mask) ──
use libblur::{self, BlurImageMut, FastBlurChannels, AnisotropicRadius, ThreadingPolicy};

/// Resize `buf` to `size` without zero-initialization, then call `write` with the mutable slice.
/// The write closure must write all `size` bytes before returning — reading uninit bytes is UB.
/// After `write` returns, the Vec is guaranteed to contain `size` initialized bytes.
fn with_resize_uninit(buf: &mut Vec<u8>, size: usize, write: impl FnOnce(&mut [u8])) {
    buf.clear();
    buf.reserve(size);
    // SAFETY: reserve guarantees capacity >= size. write() fills all bytes.
    unsafe { buf.set_len(size); }
    write(&mut buf[..size]);
}

/// Shared state passed via axum State to every WebSocket handler.
#[derive(Clone)]
struct ServerState {
    args: Arc<Args>,
    token: Option<String>,
    /// Pre-rendered HTML page (template substitution done once at startup).
    /// Arc avoids cloning the ~10-30KB HTML on every HTTP request.
    index_html: Arc<String>,
    /// Shared keycode cache (keysym→keycode), built once on first connection.
    keycode_cache: std::sync::OnceLock<std::sync::Arc<std::collections::HashMap<u32, u8>>>,
}

// X11 event opcodes used by XTest fake_input (standard X11 protocol values)
const X11_KEY_PRESS: u8 = 2;
const X11_KEY_RELEASE: u8 = 3;
const X11_BUTTON_PRESS: u8 = 4;
const X11_BUTTON_RELEASE: u8 = 5;
const X11_MOTION_NOTIFY: u8 = 6;

#[derive(Parser, Clone)]
#[command(
    name = "vnrit",
    version,
    about = "Lightweight X11 WebRTC streaming server (pure Rust)",
    long_about = "\
vnrit streams an X11 display to one or more browsers over WebRTC.

  1. Start the server:   vnrit --display :1
  2. Open the URL in a browser (printed on startup, default http://0.0.0.0:8080)
  3. Click to send keyboard/mouse events back to the X11 display.

The frontend supports touch-to-mouse translation (one-finger move,
two-finger scroll, tap = left click, long-press = right click).

Uses pure Rust: webrtc-rs for WebRTC, openh264 for H.264 encoding,
x11rb for X11 screen capture and input injection.
No GStreamer dependency.
",
    after_help = "\
══════════════════════════════════════════════════════════════════
                        V N R I T   G U I D E
══════════════════════════════════════════════════════════════════

─── CODEC ──────────────────────────────���────────────────────────

  H.264 (only) built-in via openh264 (Cisco OpenH264).
  Constrained Baseline profile, real-time screen content mode.

─── RECOMMENDED COMMAND ────────────────────────────────────────

  vnrit --height 720 --bitrate 500

  • 720p downscale (good clarity, low bandwidth)
  • 500 kbps bitrate (smooth GUI at ~3 MB/min)

─── BITRATE RECOMMENDATIONS ─────────────────────────────────────

  720p @ 24 fps:

    300 kbps    Low quality, usable for text terminals
    500 kbps    Good quality for GUI desktops (recommended)
    1000 kbps   High quality, default setting
    2000+ kbps  Near-lossless on static content

─── ADAPTIVE BITRATE ──────────────────────────────────────────

  --adaptive-bitrate enables dynamic bitrate adjustment based on
  WebRTC bandwidth estimation (TWCC). The encoder lowers bitrate
  on congested links and raises it when bandwidth improves.

  Rate changes are gated by 5-second minimum intervals and a
  50kbps deadband. IDR frames are only forced on drops >30%.

    vnrit --adaptive-bitrate              # enable, fixed max only
    vnrit --bitrate 2000 --adaptive-bitrate  # cap at 2000 kbps

─── STREAM SCALING ──────────────────────────────────────────────

  By default vnrit streams at the desktop's native resolution
  (e.g. 1920x1080). Use --height to downscale on the server side:

    vnrit --height 720       # stream at 720p (maintains aspect ratio)
    vnrit --height 480       # stream at 480p (low bandwidth)

─── INPUT (WebSocket Protocol) ──────────────────────────────────

  The frontend sends keyboard/mouse input as CSV lines over the
  same WebSocket used for WebRTC signaling:

    mouse,<x>,<y>,<button>,<pressed>
      x/y = absolute pixel coordinates
      button: 1=left, 2=middle, 3=right
      pressed: 1=down, 0=up
      Example:  mouse,800,600,1,1

    key,<keycode>,<pressed>
      keycode = X11 keysym (see /usr/include/X11/keysymdef.h)
      pressed: 1=down, 0=up
      Example:  key,65,1   (space bar press)

─── EXAMPLES ────────────────────────────────────────────────────

  vnrit --display :1 -p 8080 --height 720 --bitrate 500
  vnrit --display :0 --stun \"\"    # LAN only, no STUN
  vnrit --tcp-only --adaptive-bitrate  # NAT traversal + ABR

─── NOTES ───────────────────────────────────────────────────────

  - vnrit requires a running X11 server (Xvnc, Xvfb, or real X).
  - On Termux, it connects via the Unix socket at
    /data/data/com.termux/files/usr/tmp/.X11-unix/X<display>.
  - Each browser tab creates a separate WebRTC connection.
"
)]
struct Args {
    #[arg(
        long,
        default_value = ":1",
        help = "X11 display to capture (e.g. :0, :1)",
        long_help = "X11 display identifier to capture. Uses the standard X11 \
display format :<number>. On Termux the connection is made via a Unix socket \
at /data/data/com.termux/files/usr/tmp/.X11-unix/X<number>."
    )]
    display: String,

    #[arg(
        long,
        short = 'p',
        default_value = "8080",
        help = "HTTP/WebSocket listen port",
    )]
    port: u16,

    #[arg(
        long,
        default_value = "24",
        help = "Capture framerate in fps",
    )]
    framerate: i32,

    #[arg(
        long,
        default_value = "stun:stun.cloudflare.com:3478",
        help = "STUN server URL (set empty string to disable)"
    )]
    stun: String,

    #[arg(
        long,
        default_value = "1000",
        help = "Target bitrate in kbps",
    )]
    bitrate: i32,

    #[arg(
        long,
        default_value = "0",
        help = "Downscale stream height in pixels (0 = no scaling)",
        long_help = "If non-zero, the video stream is scaled down to the given height while \
maintaining aspect ratio. This reduces bandwidth and encoding CPU usage."
    )]
    height: i32,

    #[arg(
        long,
        help = "Authentication token (if set, all connections require this token)",
        long_help = "If set, all HTTP and WebSocket connections must include a 'token' query parameter \
or a 'token' cookie matching this value. The server sets a cookie on first successful \
authentication so subsequent requests (including the WebSocket upgrade) can reuse it."
    )]
    token: Option<String>,

    #[arg(
        long,
        default_value = "info",
        help = "Log level (off, error, warn, info, debug, trace)",
    )]
    log_level: String,

    #[arg(
        long,
        default_value = "false",
        help = "Use TCP-only ICE (disable UDP, useful when UDP is blocked)",
    )]
    tcp_only: bool,

    #[arg(
        long,
        default_value = "false",
        help = "Enable adaptive bitrate based on WebRTC bandwidth estimation",
    )]
    adaptive_bitrate: bool,

    #[arg(
        long,
        help = "Y-plane unsharp mask strength (0.0=off, 0.5=light, 1.0=medium, 2.0=strong) \
[default: auto=0.8 when --height is active, else 0.0]",
        long_help = "After scaling, applies unsharp masking to the Y (luminance) plane. \
Uses libblur stack_blur (SIMD O(1) blur) with integer USM. \
Only touches luma — chroma is preserved. \
Values above 1.5 may introduce visible halos. \
When omitted and --height is active, defaults to 0.8 automatically."
    )]
    enhance: Option<f32>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum SignalingMessage {
    #[serde(rename = "offer")]
    Offer { sdp: String },
    #[serde(rename = "answer")]
    Answer { sdp: String },
    #[serde(rename = "ice")]
    Ice { candidate: String, sdp_mline_index: u32 },
    #[serde(rename = "ready")]
    Ready,
}

struct CaptureState {
    conn: RustConnection,
    root: Window,
}

struct InputState {
    conn: RustConnection,
    root: Window,
    screen_w: u16,
    screen_h: u16,
    cursor_x: AtomicI32,
    cursor_y: AtomicI32,
    /// Packed (y << 16) | x of last sent cursor position for atomic compare-swap.
    /// Initialized to !0 (= no sent yet) so first send always goes through.
    last_sent_packed: AtomicU64,
    keycode_cache: std::sync::Arc<std::collections::HashMap<u32, u8>>,
    /// Track currently pressed keycodes so we can release them on disconnect,
    /// preventing "stuck key" on the X server (especially from virtual keyboard).
    pressed_keys: std::sync::Mutex<std::collections::HashSet<u8>>,
}

// ── ScreenCapture: SHM-accelerated X11 screen capture ──
//
// Uses MIT-SHM extension with server-allocated FD-based shared memory
// (shm::create_segment). The X server writes pixels directly into a
// shared memory buffer, avoiding per-frame socket transfer of pixel data.
//
// For a 1920×1080 display at 24 fps, this saves ~200 MB/s of X11 socket
// bandwidth (1920 × 1080 × 4 B × 24 fps = ~198 MB/s).
//
// The FD-based approach (create_segment + mmap) works in Termux proot
// because it uses mmap internally, unlike SysV IPC (shmget/shmat).
//
// Falls back to get_image if MIT-SHM extension is unavailable.
//
// ZPixmap format → pixel data is BGRA (4 bytes/pixel).

struct ShmScreenCapture {
    conn: Arc<CaptureState>,
    width: u16,
    height: u16,
    shmseg: Seg,
    // SAFETY: shm_ptr is a valid mmap'd region of shm_size bytes, owned by this struct.
    // It is only accessed from the single spawn_blocking capture task via capture().
    // Drop calls munmap which is safe as long as no concurrent access exists —
    // guaranteed because capture() and Drop run sequentially in the same task.
    shm_ptr: *mut u8,
    shm_size: usize,
    bpp: u8,
}

// Required because ShmScreenCapture is moved into spawn_blocking (crosses thread boundary).
// The *mut u8 is only accessed from one thread at a time — see SAFETY above.
unsafe impl Send for ShmScreenCapture {}
unsafe impl Sync for ShmScreenCapture {}

impl ShmScreenCapture {
    /// Try to create an SHM-accelerated capture. Returns None if SHM is
    /// not available (MIT-SHM extension missing from X server).
    fn try_new(capture: Arc<CaptureState>, width: u16, height: u16, depth: u8) -> Result<Option<Self>> {
        // Calculate bytes-per-pixel for ZPixmap
        // depth 24 → 4 bytes (32-bit padded), depth >24 → 4 bytes
        let bpp = if depth >= 24 { 4u8 } else { ((depth as u32 + 7) / 8) as u8 };
        let shm_size = (width as usize) * (height as usize) * (bpp as usize);

        // Query MIT-SHM version to verify availability
        let ver = match shm::query_version(&capture.conn) {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => reply,
                Err(e) => {
                    log::debug!("[shm] MIT-SHM reply error: {:?}, falling back to get_image", e);
                    return Ok(None);
                }
            },
            Err(e) => {
                log::debug!("[shm] MIT-SHM query failed: {}, falling back to get_image", e);
                return Ok(None);
            }
        };

        if ver.major_version == 0 && ver.minor_version == 0 {
            log::debug!("[shm] MIT-SHM extension missing, falling back to get_image");
            return Ok(None);
        }

        log::debug!("[shm] MIT-SHM v{}.{}, allocating {} bytes ({}x{}x{})",
            ver.major_version, ver.minor_version, shm_size, width, height, depth);

        let shmseg = capture.conn.generate_id()
            .context("failed to generate SHM seg ID")?;

        // Ask the X server to allocate shared memory and return a file descriptor
        let cookie = shm::create_segment(&capture.conn, shmseg, shm_size as u32, false)
            .context("SHM create_segment failed")?;
        let reply = cookie.reply()
            .context("SHM create_segment reply failed")?;

        let raw_fd = reply.shm_fd.into_raw_fd();

        // Map the FD into our address space
        let shm_ptr = unsafe {
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                shm_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                raw_fd,
                0,
            );
            if ptr == libc::MAP_FAILED {
                libc::close(raw_fd);
                return Err(anyhow::anyhow!("mmap failed for SHM segment: size={}", shm_size));
            }
            ptr as *mut u8
        };

        // Close fd — mmap keeps a reference to the underlying file
        unsafe { libc::close(raw_fd); }

        log::debug!("[shm] segment allocated at {:?} ({} bytes)", shm_ptr, shm_size);

        Ok(Some(ShmScreenCapture {
            conn: capture,
            width,
            height,
            shmseg,
            shm_ptr,
            shm_size,
            bpp,
        }))
    }

    /// Capture the root window and convert directly to I420, bypassing BGRA Vec.
    fn capture_to_i420(&self, i420_out: &mut Vec<u8>, out_w: u32, out_h: u32,
        needs_scaling: bool, tmp_argb: &mut Vec<u8>) -> Result<()>
    {
        let cookie = shm::get_image(
            &self.conn.conn,
            self.conn.root, // drawable
            0,        // x offset
            0,        // y offset
            self.width,
            self.height,
            !0,       // plane_mask = all planes
            2,        // format = ZPixmap
            self.shmseg,
            0,        // offset in shared memory
        ).context("SHM get_image failed")?;
        let _reply = cookie.reply().context("SHM get_image reply failed")?;

        // Read BGRA directly from shared memory and convert to I420 in one step.
        let size = (self.width as usize) * (self.height as usize) * (self.bpp as usize);
        // SAFETY: shm_ptr points to X server's shared memory, written by get_image.
        let bgra_slice = unsafe { std::slice::from_raw_parts(self.shm_ptr, size) };

        if needs_scaling {
            scale_bgra_direct(bgra_slice, self.width as u32, self.height as u32,
                out_w, out_h, i420_out, tmp_argb);
        } else {
            bgra_to_i420(bgra_slice, self.width as u32, self.height as u32, i420_out);
        }
        Ok(())
    }
}

impl Drop for ShmScreenCapture {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.shm_ptr as *mut libc::c_void, self.shm_size);
        }
        let _ = shm::detach(&self.conn.conn, self.shmseg);
        log::debug!("[shm] cleaned up segment {:?}", self.shm_ptr);
    }
}

/// Fallback screen capture using xproto::get_image (no SHM).
/// Used when MIT-SHM extension is not available.
struct FallbackCapture {
    conn: Arc<CaptureState>,
    width: u16,
    height: u16,
}

impl FallbackCapture {
    fn capture_to_i420(&self, i420_out: &mut Vec<u8>, out_w: u32, out_h: u32,
        needs_scaling: bool, tmp_argb: &mut Vec<u8>) -> Result<()>
    {
        let cookie = xproto::get_image(
            &self.conn.conn,
            xproto::ImageFormat::Z_PIXMAP,
            self.conn.root,
            0, 0,
            self.width, self.height,
            !0, // plane_mask = all planes
        ).context("get_image failed")?;
        let reply = cookie.reply().context("get_image reply failed")?;

        if needs_scaling {
            scale_bgra_direct(&reply.data, self.width as u32, self.height as u32,
                out_w, out_h, i420_out, tmp_argb);
        } else {
            bgra_to_i420(&reply.data, self.width as u32, self.height as u32, i420_out);
        }
        Ok(())
    }
}

/// Unified interface for both SHM-accelerated and fallback capture.
enum ScreenCapture {
    Shm(ShmScreenCapture),
    Fallback(FallbackCapture),
}

impl ScreenCapture {
    fn capture_to_i420(&self, i420_out: &mut Vec<u8>, out_w: u32, out_h: u32,
        needs_scaling: bool, tmp_argb: &mut Vec<u8>) -> Result<()>
    {
        match self {
            ScreenCapture::Shm(s) => s.capture_to_i420(i420_out, out_w, out_h, needs_scaling, tmp_argb),
            ScreenCapture::Fallback(f) => f.capture_to_i420(i420_out, out_w, out_h, needs_scaling, tmp_argb),
        }
    }
}

// ── Color conversion (libyuv SIMD-accelerated via vnrit_libyuv) ──
//
// X11 ZPixmap returns BGRA bytes: [B,G,R,A]. libyuv calls this "ARGB".
// No-scaling path:  BGRA → ARGBToI420 → I420 (SIMD, 1 pass)
// Scaling path:     BGRA → ARGBToI420 → I420 native → I420Scale → I420 output

/// Convert BGRA (X11 ZPixmap) to I420 planar YUV via libyuv SIMD.
/// Output buffer is reused (cleared and resized if needed).
fn bgra_to_i420(bgra: &[u8], width: u32, height: u32, out: &mut Vec<u8>) {
    let w = width as usize;
    let h = height as usize;
    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);
    let total = y_size + 2 * uv_size;
    with_resize_uninit(out, total, |buf| {
        let src = ArgbImage { data: bgra, stride: (width * 4) as usize };
        let (y_plane, rest) = buf.split_at_mut(y_size);
        let (u_plane, v_plane) = rest.split_at_mut(uv_size);
        let mut dst = I420ImageMut {
            y: y_plane, y_stride: width as usize,
            u: u_plane, u_stride: (width / 2) as usize,
            v: v_plane, v_stride: (width / 2) as usize,
        };
        let size = ImageSize::new(width as usize, height as usize);
        let _ = vnrit_libyuv::argb_to_i420(&src, &mut dst, size);
    });
}

/// Scale BGRA to target size via I420-domain pipeline:
///   1. BGRA → ARGBToI420 → native I420   (1.5 bytes/px intermediate)
///   2. I420Scale → target I420
/// Uses less memory bandwidth than ARGBScale path (1.5 vs 4 bytes/px intermediate).
/// temp holds the native-resolution I420 frame (reused across frames).
fn scale_bgra_direct(bgra: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32,
    i420_out: &mut Vec<u8>, temp: &mut Vec<u8>) {
    // Step 1: BGRA → I420 at native resolution
    let src_y_size = (src_w * src_h) as usize;
    let src_uv_size = ((src_w / 2) * (src_h / 2)) as usize;
    let native_i420_size = src_y_size + 2 * src_uv_size;
    with_resize_uninit(temp, native_i420_size, |t| {
        bgra_to_i420_into(bgra, src_w, src_h, t);
    });

    // Step 2: I420 scale to target resolution
    let dst_y_size = (dst_w * dst_h) as usize;
    let dst_uv_size = ((dst_w / 2) * (dst_h / 2)) as usize;
    with_resize_uninit(i420_out, dst_y_size + 2 * dst_uv_size, |out| {
        let (src_y, rest) = temp.split_at(src_y_size);
        let (src_u, src_v) = rest.split_at(src_uv_size);
        let src_img = I420Image {
            y: src_y, y_stride: src_w as usize,
            u: src_u, u_stride: (src_w / 2) as usize,
            v: src_v, v_stride: (src_w / 2) as usize,
        };
        let (dst_y, rest) = out.split_at_mut(dst_y_size);
        let (dst_u, dst_v) = rest.split_at_mut(dst_uv_size);
        let mut dst_img = I420ImageMut {
            y: dst_y, y_stride: dst_w as usize,
            u: dst_u, u_stride: (dst_w / 2) as usize,
            v: dst_v, v_stride: (dst_w / 2) as usize,
        };
        let src_size = ImageSize::new(src_w as usize, src_h as usize);
        let dst_size = ImageSize::new(dst_w as usize, dst_h as usize);
        let _ = vnrit_libyuv::i420_scale(&src_img, src_size, &mut dst_img, dst_size, FilterMode::Bilinear);
    });
}

/// Build motion-adaptation look-up table at compile time.
///
/// `table[mad] = 255 × exp(-mad/5)` approximated via recurrence
/// `v_{n+1} = v_n × 209 / 256` where 209 = floor(256 × e^{-1/5}).
const fn build_motion_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut v: u32 = 255;
    let mut i = 0usize;
    while i < 256 {
        t[i] = v as u8;
        v = v * 209 / 256;
        i += 1;
    }
    t
}

/// Motion-adaptation table: MAD → sharpening multiplier [0, 255].
/// MAD = 0 (still) → 255 (full); MAD = 25+ (fast motion) → ≈ 0.
const MOTION_TABLE: [u8; 256] = build_motion_table();

/// Local-sharpening table: |diff| → Q8 multiplier [256, 384].
///   [0,  8) → 256 (1.0)           dead zone
///   [8, 40) → 256..384 (1..1.5)   linear ramp
///   [40, ∞) → 384 (1.5)           saturate
const fn build_local_q8_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        t[i] = if i < 8 { 256 }
            else if i < 40 { 256 + ((i - 8) * 4) as u32 }
            else { 384 };
        i += 1;
    }
    t
}
const LOCAL_Q8_TABLE: [u32; 256] = build_local_q8_table();

/// Texture-adaptation table: prev_abs_diff × abs_diff → Q8 multiplier.
/// 0.75x when adjacent pixels have similar |diff| (texture region).
/// Uses 5-bit quantization: idx = value >> 3 → 32×32 = 4KB LUT.
const fn build_texture_table() -> [[u32; 32]; 32] {
    let mut t = [[256u32; 32]; 32];
    let mut p = 0usize;
    while p < 32 {
        let mut c = 0usize;
        while c < 32 {
            let prev_v = (p * 8 + 4) as u32;  // bucket midpoint [4, 252]
            let curr_v = (c * 8 + 4) as u32;
            let mx = if prev_v > curr_v { prev_v } else { curr_v };
            let mn = if prev_v <= curr_v { prev_v } else { curr_v };
            if mx >= 8 && mn >= (mx >> 1) {
                t[p][c] = 192;
            }
            c += 1;
        }
        p += 1;
    }
    t
}
const TEXTURE_TABLE: [[u32; 32]; 32] = build_texture_table();

/// Apply unsharp mask to the Y (luminance) plane of an I420 frame.
///
/// Features:
///   - **Motion adaptation** — MAD-based look-up table reduces strength
///     during motion (eliminates temporal flicker).
///   - **Local adaptation** — per-pixel edge strength (`|Y - blur(Y)|`)
///     boosts the amount on strong edges, leaves flat areas untouched.
///   - **Integer only** — no float division, no `exp`, no `sin`/`cos`.
///
/// # Memory layout: `blur_buf` is dual-purpose
///
/// ```text
/// blur_buf  [0 .. y_size)   = copy of current Y (for stack_blur)
///           [y_size .. 2×y_size) = previous frame Y (for MAD)
/// ```
///
/// This avoids a separate `prev` allocation.  `blur_buf` is `tmp_argb`
/// from the capture pipeline (already ≈3 MB), so the 2× expansion from
/// `y_size` to `2×y_size` never reallocates after the first frame.
fn apply_enhancement(i420: &mut [u8], w: usize, h: usize, strength: f32,
    blur_buf: &mut Vec<u8>, chroma_tick: &mut u32, last_chroma_boost: &mut u32) {
    if strength <= 0.0 || w < 3 || h < 3 {
        return;
    }
    let y_size = w * h;
    let total_req = 2 * y_size;

    // Compute UV sizes now (needed for chroma activity and later for flattening)
    let uv_size = (w / 2) * (h / 2);

    // (Re)size blur_buf to [blur_copy | prev]; on first call or resolution
    // change the new prev is seeded with current frame so MAD = 0.
    if blur_buf.len() != total_req {
        blur_buf.clear();
        blur_buf.reserve(total_req);
        // SAFETY: reserve guarantees capacity. Writes below fill all bytes.
        unsafe { blur_buf.set_len(total_req); }
        // First frame or resolution change: no valid prev → current = prev
        let y_plane = &i420[..y_size];
        blur_buf[y_size..total_req].copy_from_slice(y_plane);
    }

    // ── 0.  Chroma activity (recomputed every 8 frames) ─────────
    // Chroma changes much slower than framerate; amortize the UV traversal.
    let chroma_boost_q8 = if *chroma_tick == 0 {
        let boost = if uv_size > 0 && i420.len() >= y_size + 2 * uv_size {
            let mut chroma_sum: u64 = 0;
            for &p in &i420[y_size..y_size + 2 * uv_size] {
                chroma_sum += (p as i32 - 128).unsigned_abs() as u64;
            }
            let avg_chroma = (chroma_sum / (2 * uv_size as u64)) as u32;
            256u32 + (avg_chroma * 3).min(128)
        } else {
            256u32
        };
        *last_chroma_boost = boost;
        boost
    } else {
        *last_chroma_boost
    };
    *chroma_tick = (*chroma_tick + 1) & 7;

    let (blur_copy, prev) = blur_buf.split_at_mut(y_size);
    let y_plane = &mut i420[..y_size];

    // ── 1.  MAD × TV × Copy ─────────────────────────────────────
    // Merge MAD (motion) and TV (contrast) accumulation with the
    // mandatory Y → blur_copy copy.  No extra passes.
    //   MAD = Σ|y[i] - prev[i]|       → motion adaptation
    //   TV  = Σ|y[i] - y[i-1]|        → contrast adaptation
    let mut mad_sum: u64 = 0;
    let mut tv_sum: u64 = 0;
    let mut prev_y = y_plane[0];
    for i in 0..y_size {
        let diff = y_plane[i] as i32 - prev[i] as i32;
        mad_sum += diff.unsigned_abs() as u64;
        if i > 0 {
            tv_sum += (y_plane[i] as i32 - prev_y as i32).unsigned_abs() as u64;
        }
        prev_y = y_plane[i];
        blur_copy[i] = y_plane[i];
    }
    let mad = (mad_sum / y_size as u64) as usize;           // [0, 255]
    let avg_tv = (tv_sum / (y_size.saturating_sub(1)) as u64) as u32; // [0, 255]
    let motion_factor = MOTION_TABLE[mad.min(255)] as u32;  // [0, 255]

    // When motion_factor < 16 (< 6% strength), USM effect is imperceptible.
    // Skip stack_blur + per-pixel USM; just save Y as prev for next frame's MAD.
    if motion_factor < 16 {
        prev.copy_from_slice(y_plane);
        return;
    }

    // ── 2.  Stack blur (SIMD, O(1)) ────────────────────────────
    // Resolution-adaptive radius: 640px→1, 1280px→2, 1920px→3
    let blur_radius = ((w + 320) / 640).clamp(1, 3);
    {
        let mut img = BlurImageMut::borrow(
            blur_copy,
            w as u32, h as u32,
            FastBlurChannels::Plane,
        );
        let radius = AnisotropicRadius::new(blur_radius as u32);
        let _ = libblur::stack_blur(&mut img, radius, ThreadingPolicy::Single);
    }

    // ── 3.  USM with motion + contrast + local adaptation ──────
    //  base_amount = Q8  (256 = 1.0x)
    let base_amount = (strength * 256.0).clamp(0.0, 1023.0) as u32;

    //  Contrast boost  (Total Variation per pixel → Q8 multiplier)
    //    avg_tv ≈  2 (blurry/smooth) → boost = 384 (1.5x)
    //    avg_tv ≈ 43 (typical)       → boost = 256 (1.0x)
    //    avg_tv ≈ 85 (already sharp) → boost = 128 (0.5x)
    let contrast_boost_q8 = (384u32).saturating_sub(avg_tv * 3).max(128).min(512);
    let effective_base = ((base_amount * contrast_boost_q8 + 128) >> 8).min(1023);

    //  Chroma boost (mean |C - 128| → Q8 multiplier)
    //    avg_chroma ≈  0 (grayscale) → 256  (1.0×,  no boost)
    //    avg_chroma ≈ 40 (colorful)  → 376  (1.47×, near max)
    //    avg_chroma ≈ 85+            → 384  (1.5×,  saturate)
    let effective_base = ((effective_base * chroma_boost_q8 + 128) >> 8).min(1023);

    let mut prev_abs_diff = 0u32;
    let mut col = 0usize;

    // USM loop: write enhanced Y and save to prev simultaneously (eliminates separate copy pass).
    for ((yp, &blur), p) in y_plane.iter_mut().zip(blur_copy.iter()).zip(prev.iter_mut()) {
        let diff = *yp as i32 - blur as i32;
        let abs_diff = diff.unsigned_abs();

        // Local factor Q8: lookup from compile-time LUT.
        let local_q8 = LOCAL_Q8_TABLE[abs_diff as usize];

        // Texture factor Q8: lookup from 32×32 compile-time LUT.
        let texture_q8 = TEXTURE_TABLE[(prev_abs_diff >> 3) as usize][(abs_diff >> 3) as usize];

        // Combined multiplier: motion × local × texture  (Q8)
        let combined_q8 = (motion_factor * local_q8 + 128) >> 8;
        let combined_q8 = (combined_q8 * texture_q8 + 128) >> 8;
        // Final Q8 amount, clamped to safe range
        let amount = ((effective_base * combined_q8 + 128) >> 8).min(1023);

        let adj = (diff as i32 * amount as i32 + 128) >> 8;
        let enhanced = ((*yp as i32 + adj).clamp(0, 255)) as u8;
        *yp = enhanced;
        *p = enhanced;  // save to prev for next frame's MAD

        // Update row-aware prev_abs_diff for next pixel
        prev_abs_diff = if col > 0 { abs_diff } else { 0 };
        col = if col + 1 < w { col + 1 } else { 0 };
    }
    // ── prev is updated in-loop above, no separate copy_from_slice needed ──
}

/// BGRA → I420, writing into a pre-sized buffer (no resize).
/// Buffer must have capacity >= width * height * 3 / 2.
fn bgra_to_i420_into(bgra: &[u8], width: u32, height: u32, out: &mut [u8]) {
    let src = ArgbImage { data: bgra, stride: (width * 4) as usize };
    let y_size = (width * height) as usize;
    let uv_size = ((width / 2) * (height / 2)) as usize;
    let (y_plane, rest) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = rest.split_at_mut(uv_size);
    let mut dst = I420ImageMut {
        y: y_plane, y_stride: width as usize,
        u: u_plane, u_stride: (width / 2) as usize,
        v: v_plane, v_stride: (width / 2) as usize,
    };
    let size = ImageSize::new(width as usize, height as usize);
    let _ = vnrit_libyuv::argb_to_i420(&src, &mut dst, size);
}

// ── VideoEncoder: wraps openh264 ──

struct VideoEncoder {
    inner: Encoder,
    width: u32,
    height: u32,
    last_bitrate_bps: u32,
    last_adjust: std::time::Instant,
}

impl VideoEncoder {
    fn new(args: &Args, width: u32, height: u32) -> Result<Self> {
        let bitrate_bps = (args.bitrate as u32) * 1000;
        let framerate = args.framerate as f32;

        let intra_period = (args.framerate as u32) * 10; // GOP = 10× framerate

        let config = EncoderConfig::new()
            .num_threads(1)
            .bitrate(BitRate::from_bps(bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(framerate))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .complexity(Complexity::Low)
            .intra_frame_period(IntraFramePeriod::from_num_frames(intra_period))
            .profile(Profile::Baseline);

        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("failed to create openh264 encoder")?;

        log::info!("[encoder] 1 thread configured, {}kbps", bitrate_bps / 1000);

        Ok(VideoEncoder {
            inner: encoder,
            width,
            height,
            last_bitrate_bps: bitrate_bps,
            last_adjust: std::time::Instant::now(),
        })
    }

    /// Adjust encoder bitrate at runtime via openh264 SetOption (no recreate).
    /// Returns true if bitrate was actually changed.
    fn set_bitrate(&mut self, new_bps: u32) -> bool {
        const MIN_GAP: std::time::Duration = std::time::Duration::from_secs(5);
        if self.last_adjust.elapsed() < MIN_GAP { return false; }
        if (new_bps as i32 - self.last_bitrate_bps as i32).abs() < 50000 { return false; }
        let clamped = new_bps.clamp(100_000, 10_000_000);
        let mut val: i32 = clamped as i32;
        // SAFETY: set_option with ENCODER_OPTION_BITRATE is a well-defined
        // operation in openh264's public C API — the encoder handles mid-stream
        // bitrate changes safely without requiring re-initialization.
        unsafe {
            self.inner.raw_api().set_option(
                ENCODER_OPTION_BITRATE,
                std::ptr::addr_of_mut!(val).cast(),
            );
        }
        let old_bps = self.last_bitrate_bps;
        self.last_bitrate_bps = clamped;
        self.last_adjust = std::time::Instant::now();
        // Only force IDR on significant drops (>30%) to avoid bloating an
        // already-congested link. Small adjustments transition smoothly via
        // the encoder's internal rate control.
        if clamped < old_bps && (old_bps - clamped) > old_bps * 30 / 100 {
            self.inner.force_intra_frame();
            log::info!("[encoder] {}→{}kbps (IDR forced)", old_bps / 1000, clamped / 1000);
        } else {
            log::info!("[encoder] {}→{}kbps", old_bps / 1000, clamped / 1000);
        }
        true
    }

    fn encode(&mut self, i420: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let w = self.width as usize;
        let h = self.height as usize;
        let y_size = w * h;
        let uv_size = (w / 2) * (h / 2);
        let slices = YUVSlices::new(
            (&i420[..y_size], &i420[y_size..y_size + uv_size], &i420[y_size + uv_size..]),
            (w, h),
            (w, w / 2, w / 2),
        );
        let bitstream = self.inner
            .encode(&slices)
            .context("openh264 encode failed")?;
        out.clear();
        bitstream.write_vec(out);
        Ok(())
    }

    fn force_keyframe(&mut self) {
        self.inner.force_intra_frame();
    }
}

// ── WebrtcHandler: event handler for PeerConnection ──

struct WebrtcHandler {
    ice_tx: runtime::Sender<String>,
    gather_complete_tx: runtime::Sender<()>,
    connected_tx: runtime::Sender<()>,
    done: Arc<tokio::sync::Notify>,
    dc_tx: tokio::sync::watch::Sender<Option<Arc<dyn DataChannel>>>,
    lan_ip: String,
}

#[async_trait]
impl PeerConnectionEventHandler for WebrtcHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        // The UDP/TCP socket binds to 0.0.0.0:0 (all interfaces), but the host
        // candidate address becomes "0.0.0.0" which is unreachable. Replace it
        // with the LAN IP so the browser can reach us.
        let mut candidate = event.candidate.clone();
        if candidate.typ == webrtc::peer_connection::RTCIceCandidateType::Host
            && candidate.address == "0.0.0.0"
        {
            candidate.address = self.lan_ip.clone();
        }

        log::debug!("[ice] candidate: {} {}:{} ...", candidate.typ, candidate.address, candidate.port);

        if let Ok(init) = candidate.to_json() {
            let msg = serde_json::to_string(&SignalingMessage::Ice {
                candidate: init.candidate,
                sdp_mline_index: init.sdp_mline_index.unwrap_or(0) as u32,
            }).ok();
            if let Some(msg) = msg {
                if let Err(e) = self.ice_tx.try_send(msg) {
                    log::warn!("[ice] candidate send failed: {:?}", e);
                }
            }
        }
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        log::debug!("[ice] gathering state: {:?}", state);
        if state == RTCIceGatheringState::Complete {
            let _ = self.gather_complete_tx.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        log::info!("[pc] connection state: {:?}", state);
        match state {
            RTCPeerConnectionState::Connected => {
                let _ = self.connected_tx.try_send(());
            }
            RTCPeerConnectionState::Failed
            | RTCPeerConnectionState::Disconnected
            | RTCPeerConnectionState::Closed => {
                self.done.notify_one();
            }
            _ => {}
        }
    }

    async fn on_signaling_state_change(&self, state: webrtc::peer_connection::RTCSignalingState) {
        log::info!("[pc] signaling state: {:?}", state);
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        log::info!("[dc] incoming data channel (id={})", data_channel.id());
        // NOTE: handle_ws uses input_dc from create_data_channel directly, NOT this
        // watch channel value. The reference received here may be a different
        // Arc<dyn DataChannel> wrapping the same SCTP stream. On reconnect, we've
        // observed this reference arriving already in an exhausted state — poll()
        // returns None immediately. Sending to dc_tx as a fallback is harmless.
        let _ = self.dc_tx.send(Some(data_channel));
    }
}

/// Get all non-loopback LAN IPs + loopback fallback for local testing.
fn get_local_ips() -> Vec<String> {
    let mut ips: Vec<String> = match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces.into_iter()
            .filter(|iface| !iface.is_loopback())
            .map(|iface| iface.ip().to_string())
            .collect(),
        Err(_) => vec![],
    };
    // Always include loopback so localhost / browser-on-device works.
    if !ips.contains(&"127.0.0.1".to_string()) {
        ips.push("127.0.0.1".to_string());
    }
    ips
}

/// Format an IP address and port into a socket bind string.
/// IPv6 addresses must be wrapped in brackets (e.g. [::1]:0).
fn fmt_bind_addr(ip: &str, port: u16) -> String {
    if ip.contains(':') {
        format!("[{}]:{}", ip, port)
    } else {
        format!("{}:{}", ip, port)
    }
}

// ═══════════════════════════════════════════════════════════════
//  main() — server entry point
// ═══════════════════════════════════════════════════════════════

fn main() -> Result<()> {
    // Build a multi-threaded Tokio runtime with a limited blocking thread pool.
    // Without this cap, each stuck spawn_blocking task (e.g. PulseAudio read,
    // X11 reply) consumes a blocking thread forever, causing unbounded growth.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .max_blocking_threads(32)
        .enable_all()
        .build()?;
    rt.block_on(async {
    let args = Args::parse();

    // Init logging from --log-level arg (falls back to RUST_LOG env var).
    // Suppress noisy third-party library warnings (expected behavior, not actionable).
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(&args.log_level)
    )
    .filter(Some("rtc_ice"), log::LevelFilter::Error)
    .filter(Some("rtc::peer_connection"), log::LevelFilter::Error)
    .filter(Some("webrtc::peer_connection::driver"), log::LevelFilter::Off)
    .filter(Some("rtc_dtls"), log::LevelFilter::Error)
    .filter(Some("openh264"), log::LevelFilter::Error)
    .format_timestamp(None)
    .init();

    let token = args.token.clone();
    // Pre-render HTML once at startup (avoids template substitution on every request).
    let index_html = include_str!("index.html")
        .replace("{{STUN_SERVER}}", &args.stun)
        .replace("{{PAKO_JS}}", include_str!("pako_inflate.min.js"));
    let state = ServerState {
        args: Arc::new(args),
        token,
        index_html: Arc::new(index_html),
        keycode_cache: std::sync::OnceLock::new(),
    };

    // ── Signal handler: release stuck XTest keys before exit ──
    // When streaming vnrit's own display (e.g. `:1`), pressing Ctrl+C in the
    // browser injects Ctrl+C via XTest into the terminal, which sends SIGINT
    // to vnrit. Without cleanup the X server keeps the keys held, causing
    // keyboard repeat to flood the terminal with ^C after exit.
    // Covers SIGINT/Ctrl+C, SIGTERM/kill, SIGQUIT/Ctrl+\ and SIGHUP/hangup.
    use tokio::signal::unix::{signal, SignalKind};
    let display = state.args.display.clone();
    tokio::spawn(async move {
        let mut sigint = signal(SignalKind::interrupt()).unwrap();
        let mut sigterm = signal(SignalKind::terminate()).unwrap();
        let mut sigquit = signal(SignalKind::quit()).unwrap();
        let mut sighup = signal(SignalKind::hangup()).unwrap();

        tokio::select! {
            _ = sigint.recv() => log::info!("[signal] SIGINT received"),
            _ = sigterm.recv() => log::info!("[signal] SIGTERM received"),
            _ = sigquit.recv() => log::info!("[signal] SIGQUIT received"),
            _ = sighup.recv() => log::info!("[signal] SIGHUP received"),
        }

        log::info!("[signal] releasing stuck XTest keys...");
        let _ = tokio::task::spawn_blocking(move || {
            let Ok((conn, _)) = connect_to_display(&display) else { return };
            // Release all possible keycodes (unpressed ones are silently ignored)
            for kc in 8..=255u8 {
                let _ = xtest::fake_input(&conn, X11_KEY_RELEASE, kc, 0, 0, 0, 0, 0);
            }
            let _ = conn.flush();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }).await;
        log::info!("[signal] keys released, exiting");
        std::process::exit(0);
    });

    let addr = format!("0.0.0.0:{}", state.args.port);
    println!("vnrit listening on http://{}", addr);
    println!("  Display: {}", state.args.display);
    println!("  FPS    : {}", state.args.framerate);
    println!("  Bitrate: {} kbps", state.args.bitrate);
    if state.args.height > 0 {
        println!("  Scale  : {}p", state.args.height);
    } else {
        println!("  Scale  : native (no scaling)");
    }
    if state.args.stun.is_empty() {
        println!("  STUN   : disabled");
    } else {
        println!("  STUN   : {}", state.args.stun);
    }
    match &state.token {
        Some(t) => println!("  Auth token: {} (required)", t),
        None => println!("  Auth token: none (open access)"),
    }

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/ws", get(ws_handler))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
    })
}

async fn root_handler(State(state): State<ServerState>) -> Html<String> {
    Html((*state.index_html).clone())
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<ServerState>) -> impl IntoResponse {
    ws.on_upgrade(move |ws| handle_ws(ws, state))
}

/// Authentication middleware for token-based access control.
async fn auth_middleware(
    State(state): State<ServerState>,
    req: Request,
    next: Next,
) -> Result<Response, Response> {
    let expected_token = match &state.token {
        Some(t) => t.clone(),
        None => return Ok(next.run(req).await),
    };

    // Check query parameter: ?token=xxx
    let query_token = req.uri().query().and_then(|q| {
        for pair in q.split('&') {
            let mut parts = pair.splitn(2, '=');
            if parts.next() == Some("token") {
                return parts.next().map(|v| v.to_string());
            }
        }
        None
    });

    // Check Cookie header: Cookie: token=xxx
    let cookie_token = req
        .headers()
        .get("Cookie")
        .and_then(|c| c.to_str().ok())
        .and_then(|c| {
            for cookie in c.split(';') {
                let trimmed = cookie.trim();
                if let Some(val) = trimmed.strip_prefix("token=") {
                    return Some(val.to_string());
                }
            }
            None
        });

    let authenticated = query_token.as_deref() == Some(&expected_token)
        || cookie_token.as_deref() == Some(&expected_token);

    if !authenticated {
        return Err((
            StatusCode::UNAUTHORIZED,
            "unauthorized — provide ?token=<token> or Cookie: token=<token>",
        )
            .into_response());
    }

    let mut response = next.run(req).await;

    // If authenticated via query param, set a cookie so subsequent requests
    // (including WebSocket upgrade) are authenticated without the query param.
    if query_token.as_deref() == Some(&expected_token) {
        let cookie = format!(
            "token={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400",
            expected_token
        );
        if let Ok(hv) = cookie.parse() {
            response.headers_mut().insert(axum::http::header::SET_COOKIE, hv);
        }
    }

    Ok(response)
}

// ═══════════════════════════════════════════════════════════════
//  setup_x11_connection() — creates two connections (capture + input)
//  ═══════════════════════════════════════════════════════════════

/// Connect to an X11 display, trying standard method first then Termux socket.
fn connect_to_display(display: &str) -> Result<(RustConnection, usize)> {
    // Always construct the connection manually so we can set SO_RCVTIMEO
    // on the socket.  The X server is always on a local Unix socket or
    // Termux path — there is no remote display scenario.
    let display_num: u16 = display.trim_start_matches(':').split('.').next()
        .and_then(|s| s.parse().ok())
        .context("invalid display format")?;

    // Try the standard X11 socket path first, then fall back to Termux.
    let sock = format!("/tmp/.X11-unix/X{}", display_num);
    let unix_stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(_) => {
            let termux_sock = format!(
                "/data/data/com.termux/files/usr/tmp/.X11-unix/X{}", display_num
            );
            log::info!("[x11] connecting via Termux socket path: {}", termux_sock);
            UnixStream::connect(&termux_sock)
                .context("cannot connect to Termux X11 socket")?
        }
    };

    // Set 5-second receive timeout on all X11 sockets so that blocking
    // reply() calls (e.g. mit-shm get_image) don't hang forever.
    let fd = unix_stream.as_raw_fd();
    let tv = libc::timeval { tv_sec: 5, tv_usec: 0 };
    unsafe {
        libc::setsockopt(
            fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }
    log::info!("[x11] SO_RCVTIMEO=5s set on display {}", display);

    let (stream, (family, address)) = DefaultStream::from_unix_stream(unix_stream)
        .context("from_unix_stream failed")?;
    let (auth_name, auth_data) = get_auth(family, &address, display_num)
        .unwrap_or(None)
        .unwrap_or_else(|| (Vec::new(), Vec::new()));
    let conn = RustConnection::connect_to_stream_with_auth_info(
        stream, 0, auth_name, auth_data,
    ).context("connect_to_stream failed")?;
    log::info!("[x11] connected");
    Ok((conn, 0usize))
}

fn setup_x11_connections(display: &str, keycode_cache: std::sync::Arc<std::collections::HashMap<u32, u8>>) -> Result<(Arc<CaptureState>, Arc<InputState>, u16, u16, u8, RustConnection)> {
    log::info!("[x11] connecting to display {} (capture connection)", display);

    let (cap_conn, screen_num) = connect_to_display(display)?;
    let screen = &cap_conn.setup().roots[screen_num];
    let root = screen.root;
    let screen_width = screen.width_in_pixels;
    let screen_height = screen.height_in_pixels;
    let screen_depth = screen.root_depth;

    // Second connection for input injection (keyboard/mouse)
    log::info!("[x11] connecting to display {} (input connection)", display);
    let (inp_conn, _) = connect_to_display(display)?;

    // Verify XTest extension on input connection
    let xtest_cookie = xtest::get_version(&inp_conn, 2, 2)
        .context("XTest not available")?;
    xtest_cookie.reply().context("XTest query failed")?;

    // Get current pointer position on input connection
    let ptr = xproto::query_pointer(&inp_conn, root)
        .context("query_pointer failed")?
        .reply()
        .context("query_pointer reply failed")?;

    let setup = inp_conn.setup();

    log::info!(
        "[x11] connected, root=0x{:x}, pointer=({},{}), dims={}x{}, keycodes={}-{}",
        root, ptr.root_x, ptr.root_y,
        screen_width, screen_height,
        setup.min_keycode, setup.max_keycode
    );

    // Third connection for event-driven cursor tracking via XI2
    log::info!("[x11] connecting to display {} (xi2 event connection)", display);
    let (evt_conn, _) = connect_to_display(display)?;

    // Query XI2 version
    let xi_ver = xinput::xi_query_version(&evt_conn, 2, 0)
        .context("XI2 query_version failed")?
        .reply()
        .context("XI2 query_version reply failed")?;
    log::info!("[x11] XI2 version: {}.{}", xi_ver.major_version, xi_ver.minor_version);

    // Select XI_Motion events on root window for all master devices
    xinput::xi_select_events(&evt_conn, root, &[EventMask {
        deviceid: 1, // XIAllMasterDevices
        mask: vec![XIEventMask::MOTION],
    }]).context("XI2 select_events failed")?;
    evt_conn.flush()?;

    let capture_state = Arc::new(CaptureState {
        conn: cap_conn,
        root,
    });

    let input_state = Arc::new(InputState {
        conn: inp_conn,
        root,
        screen_w: screen_width,
        screen_h: screen_height,
        cursor_x: AtomicI32::new(ptr.root_x as i32),
        cursor_y: AtomicI32::new(ptr.root_y as i32),
        last_sent_packed: AtomicU64::new(u64::MAX),
        keycode_cache,
        pressed_keys: std::sync::Mutex::new(std::collections::HashSet::new()),
    });

    Ok((capture_state, input_state, screen_width, screen_height, screen_depth, evt_conn))
}

/// Try to auto-detect the default PulseAudio sink's monitor source for system audio capture.
/// Returns `Some("sink_name.monitor")` on success, `None` to fall back to default record source.
fn find_default_monitor() -> Option<String> {
    use libpulse_binding as pulse;
    use std::sync::mpsc as block_mpsc;

    let mut mainloop = pulse::mainloop::standard::Mainloop::new()?;
    let mut ctx = pulse::context::Context::new(&mainloop, "vnrit-monitor-detect")?;

    let (tx, rx) = block_mpsc::channel();

    ctx.set_state_callback(Some(Box::new(move || {
        let _ = tx.send(());
    })));

    if ctx.connect(None, pulse::context::FlagSet::NOFLAGS, None).is_err() {
        log::info!("[audio] PA connect failed");
        return None;
    }

    // Run mainloop until ready or timeout (~2s)
    for _ in 0..200 {
        if mainloop.iterate(false).is_error() { break; }
        if rx.try_recv().is_ok() && ctx.get_state() == pulse::context::State::Ready {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    ctx.set_state_callback(None);

    if ctx.get_state() != pulse::context::State::Ready {
        log::info!("[audio] PA context not ready, fallback to default source");
        return None;
    }

    // Query server info to get default sink name
    let (info_tx, info_rx) = block_mpsc::channel();
    let introspect = ctx.introspect();
    introspect.get_server_info(Box::new(move |info: &pulse::context::introspect::ServerInfo| {
        if let Some(ref name) = info.default_sink_name {
            let _ = info_tx.send(name.to_string());
        }
    }));

    for _ in 0..50 {
        if mainloop.iterate(false).is_error() { break; }
        if let Ok(name) = info_rx.try_recv() {
            let monitor = format!("{}.monitor", name);
            log::info!("[audio] detected default sink '{}', monitor '{}'", name, monitor);
            return Some(monitor);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    log::info!("[audio] failed to detect monitor source");
    None
}

// ═══════════════════════════════════════════════════════════════
//  handle_ws() — per‑client WebSocket + WebRTC handler
// ═══════════════════════════════════════════════════════════════

/// Resources created during the signaling phase that survive into the pipeline.
struct SignalingOutput {
    pc: Box<dyn PeerConnection>,
    handler: Arc<WebrtcHandler>,
    ice_rx: runtime::Receiver<String>,
    gather_complete_rx: runtime::Receiver<()>,
    connected_rx: runtime::Receiver<()>,
    dc_tx: tokio::sync::watch::Sender<Option<Arc<dyn DataChannel>>>,
    dc_rx: tokio::sync::watch::Receiver<Option<Arc<dyn DataChannel>>>,
    done: Arc<tokio::sync::Notify>,
    track: Arc<TrackLocalStaticSample>,
    audio_track: Arc<TrackLocalStaticSample>,
    audio_ssrc: u32,
    /// DataChannel from create_data_channel, returned directly to handle_ws.
    /// The on_data_channel callback provides a separate reference that may be
    /// invalid on reconnect; using this field avoids that race.
    input_dc: Option<Arc<dyn DataChannel>>,
}

/// Run the WebRTC signaling phase inside a Result so that all early exits
/// use `?` and the outer function performs cleanup once on error.
async fn run_signaling(
    in_rx: &mut mpsc::Receiver<Result<Message, axum::Error>>,
    out_tx: &mpsc::Sender<Message>,
    state: &ServerState,
    all_ips: &[String],
    tcp_addrs: Vec<String>,
    udp_addrs: Vec<String>,
) -> Result<SignalingOutput> {
    // ── Build MediaEngine (H.264 only) ──
    let mut media_engine = MediaEngine::default();
    let video_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .into(),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
        ..Default::default()
    };
    media_engine.register_codec(video_codec.clone(), RtpCodecKind::Video)
        .context("register H264 codec")?;

    // ── Register Opus audio codec ──
    let audio_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
            rtcp_feedback: vec![],
        },
        payload_type: 111,
        ..Default::default()
    };
    media_engine.register_codec(audio_codec.clone(), RtpCodecKind::Audio)
        .context("register Opus codec")?;

    let registry = register_default_interceptors(Registry::new(), &mut media_engine)
        .context("register default interceptors")?;

    // ── RTCConfiguration ──
    let ice_servers = if state.args.stun.is_empty() {
        vec![]
    } else {
        vec![RTCIceServer {
            urls: vec![state.args.stun.clone()],
            ..Default::default()
        }]
    };
    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(ice_servers)
        .build();

    // ── SettingEngine: relax ICE timeouts for mobile/unstable networks ──
    let mut settings = SettingEngine::default();
    settings.set_ice_timeouts(
        Some(std::time::Duration::from_secs(15)),  // disconnected (default 5s)
        Some(std::time::Duration::from_secs(60)),  // failed (default 25s)
        Some(std::time::Duration::from_secs(5)),   // keepalive (default 2s)
    );
    if state.args.tcp_only {
        settings.set_network_types(vec![NetworkType::Tcp4, NetworkType::Tcp6]);
    }

    // ── Create runtime channels for the handler ──
    let (ice_tx, ice_rx) = runtime::channel::<String>(256);
    let (gather_complete_tx, gather_complete_rx) = runtime::channel::<()>(1);
    let (connected_tx, connected_rx) = runtime::channel::<()>(1);
    let done = Arc::new(tokio::sync::Notify::new());
    let (dc_tx, dc_rx) = tokio::sync::watch::channel::<Option<Arc<dyn DataChannel>>>(None);

    // ── Build PeerConnection ──
    let lan_ip = all_ips[0].clone();
    log::info!("[ice] binding interfaces: {:?}", all_ips);
    let handler = Arc::new(WebrtcHandler {
        ice_tx,
        gather_complete_tx,
        connected_tx,
        done: done.clone(),
        dc_tx: dc_tx.clone(),
        lan_ip,
    });

    let rt = runtime::default_runtime()
        .context("no webrtc runtime available")?;

    let pc = Box::new(PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_setting_engine(settings)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(handler.clone())
        .with_runtime(rt)
        .with_udp_addrs(udp_addrs)
        .with_tcp_addrs(tcp_addrs)
        .build()
        .await
        .context("build PeerConnection")?);
    log::info!("[pc] PeerConnection created");

    // ── Post-pc operations (tracks, offer/answer) — close pc on error ──
    let v = match async {
        // ── Create video track ──
        let ssrc = rand::rng().random::<u32>();
        let rtp_codec = video_codec.rtp_codec.clone();

        let track = TrackLocalStaticSample::new(MediaStreamTrack::new(
            "video-stream".to_owned(),
            "video-track".to_owned(),
            "desktop".to_owned(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: rtp_codec.clone(),
                ..Default::default()
            }],
        )).context("create video track")?;
        let track = Arc::new(track);
        let track_local: Arc<dyn TrackLocal> = track.clone();
        pc.add_track(track_local).await
            .context("add video track")?;
        log::info!("[pc] video track added");

        // ── Create audio track ──
        let audio_ssrc = rand::rng().random::<u32>();
        let audio_rtp_codec = audio_codec.rtp_codec.clone();
        let audio_track = TrackLocalStaticSample::new(MediaStreamTrack::new(
            "audio-stream".to_owned(),
            "audio-track".to_owned(),
            "microphone".to_owned(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(audio_ssrc),
                    ..Default::default()
                },
                codec: audio_rtp_codec,
                ..Default::default()
            }],
        )).context("create audio track")?;
        let audio_track = Arc::new(audio_track);
        let audio_track_local: Arc<dyn TrackLocal> = audio_track.clone();
        pc.add_track(audio_track_local).await
            .context("add audio track")?;
        log::info!("[pc] audio track added, creating offer...");

        // Create input DataChannel BEFORE create_offer so the browser includes it in the answer SDP.
        // Return the DC directly to handle_ws, bypassing the on_data_channel callback
        // (which may provide a different, already-exhausted Arc<dyn DataChannel> on reconnect).
        let input_dc = match pc.create_data_channel("input", None).await {
            Ok(dc) => {
                log::info!("[dc] input channel created (id={})", dc.id());
                let _ = dc_tx.send(Some(dc.clone()));
                Some(dc)
            }
            Err(e) => {
                log::warn!("[dc] create_data_channel failed: {}", e);
                None
            }
        };

        // ── Create offer and send to browser ──
        let offer = pc.create_offer(None).await
            .context("create_offer")?;
        pc.set_local_description(offer).await
            .context("set_local_description")?;

        let local = pc.local_description().await
            .context("local_description returned None")?;
        let offer_msg = serde_json::to_string(&SignalingMessage::Offer {
            sdp: local.sdp.clone(),
        }).context("serialize offer")?;
        log::info!("[sdp] sending offer ({} bytes)", local.sdp.len());
        out_tx.try_send(Message::Text(offer_msg.into()))
            .context("send offer to WebSocket")?;

        // ── Receive browser's answer ──
        let answer_sdp = loop {
            match in_rx.recv().await {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(SignalingMessage::Answer { sdp }) = serde_json::from_str(&t) {
                        log::info!("[sdp] received answer ({} bytes)", sdp.len());
                        break sdp;
                    }
                }
                Some(Ok(Message::Close(_))) | None => {
                    anyhow::bail!("disconnected while waiting for answer");
                }
                _ => {}
            }
        };

        // ── Set remote description from answer ──
        let answer = RTCSessionDescription::answer(answer_sdp)
            .context("invalid answer SDP")?;
        pc.set_remote_description(answer).await
            .context("set_remote_description")?;
        log::info!("[sdp] remote description set");

        anyhow::Ok((track, audio_track, audio_ssrc, input_dc))
    }.await {
        Ok(v) => v,
        Err(e) => {
            let _ = pc.close().await;
            return Err(e);
        }
    };

    let (track, audio_track, audio_ssrc, input_dc) = v;

    Ok(SignalingOutput {
        pc,
        handler,
        ice_rx,
        gather_complete_rx,
        connected_rx,
        dc_tx,
        dc_rx,
        done,
        track,
        audio_track,
        audio_ssrc,
        input_dc,
    })
}

async fn handle_ws(ws: WebSocket, state: ServerState) {
    log::info!("[ws] client connected");

    // Pre-connection yield: give orphaned blocking threads from a previous
    // timed-out shutdown a chance to make progress (e.g. X11 reply() timeout
    // expiry, PA mainloop exit), so their resources are freed before we
    // allocate new ones.
    tokio::task::yield_now().await;
    std::thread::yield_now();
    // Force mimalloc to flush cached segments before this session allocates.
    // Without this, freed memory from the previous session sits in mimalloc
    // pools and the new session allocates on top of it, inflating RSS.
    unsafe { mi_collect(true); }

    // ── X11 connection setup (triple connections: capture + input + event) ──
    // Build or reuse the shared keycode cache (keysym→keycode mapping)
    let keycode_cache = match state.keycode_cache.get() {
        Some(cache) => cache.clone(),
        None => {
            // First-time initialization with proper error handling
            let (conn, _screen_num) = match connect_to_display(&state.args.display) {
                Ok(v) => v,
                Err(e) => {
                    log::error!("[x11] failed to connect for keycode cache: {:#}", e);
                    return;
                }
            };
            let setup = conn.setup();
            let first_kc = setup.min_keycode;
            let keycode_count = setup.max_keycode - setup.min_keycode + 1;
            let kbd = match xproto::get_keyboard_mapping(&conn, first_kc, keycode_count) {
                Ok(cookie) => match cookie.reply() {
                    Ok(reply) => reply,
                    Err(e) => {
                        log::error!("[x11] get_keyboard_mapping reply failed: {:#}", e);
                        return;
                    }
                },
                Err(e) => {
                    log::error!("[x11] get_keyboard_mapping request failed: {:#}", e);
                    return;
                }
            };
            let kpk = kbd.keysyms_per_keycode as usize;
            let mut m = std::collections::HashMap::new();
            for (i, chunk) in kbd.keysyms.chunks(kpk).enumerate() {
                let kc = first_kc + i as u8;
                for &ks in chunk {
                    if ks != 0 {
                        m.entry(ks).or_insert(kc);
                    }
                }
            }
            let cache = std::sync::Arc::new(m);
            // Ignore race: another task may have set it first
            let _ = state.keycode_cache.set(cache.clone());
            cache
        }
    };
    let (capture_state, input_state, native_w, native_h, depth, evt_conn) = match setup_x11_connections(&state.args.display, keycode_cache) {
        Ok(v) => v,
        Err(e) => {
            log::error!("[x11] FATAL: failed to connect: {:#}", e);
            return;
        }
    };

    // ── Determine output dimensions ──
    let (out_w, out_h) = if state.args.height > 0 {
        let h = state.args.height as u32;
        let w = (native_w as u32 * h) / native_h as u32;
        (w / 2 * 2, h / 2 * 2)
    } else {
        (native_w as u32, native_h as u32)
    };
    let needs_scaling = out_w != native_w as u32 || out_h != native_h as u32;

    log::info!("[capture] native={}x{} output={}x{} scaling={}",
        native_w, native_h, out_w, out_h, needs_scaling);

    // ── Spawn I/O task for WebSocket ──
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(256);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Message, axum::Error>>(256);
    let in_tx_task = in_tx.clone();

    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        use futures_util::SinkExt;
        let (mut ws_sink, mut ws_stream) = ws.split();
        loop {
            tokio::select! {
                outgoing = out_rx.recv() => {
                    match outgoing {
                        Some(msg) => {
                            // Compress non-signaling text messages > 512 bytes.
                            // Signaling (offer/answer/ice/ready) is always sent as
                            // raw Text frames for maximum browser compatibility.
                            let msg = match msg {
                                Message::Text(t) if t.len() > 512
                                    && !is_signaling_message(&t) => {
                                    compress_text(&t)
                                }
                                other => other,
                            };
                            if let Err(e) = ws_sink.send(msg).await {
                                log::debug!("[wsio] send error: {}", e);
                                break;
                            }
                        }
                        None => break,
                    }
                }
                incoming = ws_stream.next() => {
                    match incoming {
                        Some(Ok(msg)) => {
                            let msg = match msg {
                                Message::Binary(data) if data.len() > 1 && data[0] == 1 => {
                                    decompress_binary(data)
                                }
                                _ => msg,
                            };
                            if in_tx_task.send(Ok(msg)).await.is_err() {
                                break;
                            }
                        }
                        Some(Err(e)) => {
                            log::debug!("[wsio] recv error: {}", e);
                            break;
                        }
                        None => break,
                    }
                }
            }
        }
        log::debug!("[wsio] task ended");
    });

    // ── Wait for 'ready' message ──
    loop {
        match in_rx.recv().await {
            Some(Ok(Message::Text(t))) => {
                if let Ok(SignalingMessage::Ready) = serde_json::from_str(&t) {
                    break;
                }
            }
            Some(Ok(Message::Close(_))) | None => {
                log::debug!("[ws] disconnected before ready");
                tasks.shutdown().await;
                return;
            }
            _ => {}
        }
    }
    log::info!("[ws] ready received, creating WebRTC peer connection...");

    // ── Run signaling phase (wrapped in Result for unified cleanup) ──
    let all_ips = get_local_ips();
    let tcp_addrs: Vec<String> = all_ips.iter().map(|ip| fmt_bind_addr(ip, 0)).collect();
    let udp_addrs = if state.args.tcp_only {
        Vec::<String>::new()
    } else {
        all_ips.iter()
            .filter(|ip| *ip != "127.0.0.1" && *ip != "::1")
            .map(|ip| fmt_bind_addr(ip, 0))
            .collect()
    };

    let sig = match run_signaling(
        &mut in_rx, &out_tx, &state, &all_ips, tcp_addrs, udp_addrs,
    ).await {
        Ok(s) => s,
        Err(e) => {
            log::info!("[ws] signaling failed: {:#}", e);
            tasks.shutdown().await;
            return;
        }
    };

    // Destructure signaling output
    let pc = sig.pc;
    let handler = sig.handler;
    let mut ice_rx = sig.ice_rx;
    let mut gather_complete_rx = sig.gather_complete_rx;
    let mut connected_rx = sig.connected_rx;
    let _dc_tx = sig.dc_tx;
    let mut dc_rx = sig.dc_rx;
    let done = sig.done;
    let track = sig.track;
    let audio_track = sig.audio_track;
    let audio_ssrc = sig.audio_ssrc;
    let input_dc = sig.input_dc;

    // ── Send initial cursor position ──
    send_cursor_position(&out_tx, &input_state, native_w, native_h, out_w, out_h);

    // ── Create ScreenCapture (SHM-accelerated with fallback) ──
    let screen_capture = match ShmScreenCapture::try_new(capture_state.clone(), native_w, native_h, depth) {
        Ok(Some(shm)) => {
            log::info!("[capture] using MIT-SHM acceleration");
            ScreenCapture::Shm(shm)
        }
        Ok(None) => {
            log::info!("[capture] SHM unavailable, using get_image fallback");
            ScreenCapture::Fallback(FallbackCapture { conn: capture_state.clone(), width: native_w, height: native_h })
        }
        Err(e) => {
            log::info!("[capture] SHM init failed: {}, using get_image fallback", e);
            ScreenCapture::Fallback(FallbackCapture { conn: capture_state.clone(), width: native_w, height: native_h })
        }
    };

    // ── Create VideoEncoder ──
    let mut encoder = match VideoEncoder::new(&state.args, out_w, out_h) {
        Ok(e) => e,
        Err(err) => {
            log::info!("[encoder] failed to create: {:#}", err);
            tasks.shutdown().await;
            let _ = pc.close().await;
            return;
        }
    };
    log::info!("[encoder] created ({}x{}, {}kbps, {}fps)",
        out_w, out_h, state.args.bitrate, state.args.framerate);

    // ── Forward ICE candidates ──
    let ice_out_tx = out_tx.clone();
    tasks.spawn(async move {
        while let Some(candidate_msg) = ice_rx.recv().await {
            if ice_out_tx.send(Message::Text(candidate_msg.into())).await.is_err() {
                break;
            }
        }
    });

    // ── Wait for ICE gathering complete ──
    if tokio::select! {
        _ = gather_complete_rx.recv() => { false }
        _ = done.notified() => { true }
    } {
        log::debug!("[ice] connection ended during gathering, cleaning up");
        // async cleanup: only io_handle, ice_forward, pc exist at this point
        let _ = pc.close().await;
        drop(pc);
        drop(handler);
        tasks.shutdown().await;
        return;
    }
    log::debug!("[ice] gathering complete");

    // ── Wait for ICE connected state ──
    if tokio::select! {
        _ = connected_rx.recv() => { false }
        _ = done.notified() => { true }
    } {
        log::info!("[pc] connection failed before connected, cleaning up");
        let _ = pc.close().await;
        drop(pc);
        drop(handler);
        tasks.shutdown().await;
        return;
    }
    log::info!("[pc] connection established!");

    // ── Pipeline: 3-stage capture → encode → send ──
    let frame_duration = std::time::Duration::from_nanos(1_000_000_000 / state.args.framerate as u64);
    let track_ssrc = *track.ssrcs().await.first().unwrap_or(&0);
    if track_ssrc == 0 {
        log::info!("[pc] ERROR: no SSRC available for video track");
        let _ = pc.close().await;
        drop(pc);
        drop(handler);
        tasks.shutdown().await;
        return;
    }

    // When --height is active, auto-enable enhancement at 0.8 unless user explicitly set --enhance.
    let enhance = state.args.enhance
        .unwrap_or_else(|| if needs_scaling { 0.8 } else { 0.0 });
    log::info!("[pipeline] starting 3-stage pipeline (cap→enc→send), {} fps, enhance={}", state.args.framerate, enhance);

    // Capture flag before it's moved into the encoder task
    let adaptive_bitrate = state.args.adaptive_bitrate;
    let initial_bitrate_bps = (state.args.bitrate as u32) * 1000;

    // Pipeline channels
    let (yuv_tx, yuv_rx) = block_mpsc::sync_channel::<Bytes>(2);
    let (enc_tx, mut enc_rx) = tokio::sync::mpsc::channel::<Bytes>(2);

    // Shutdown signal — unified CancellationToken for all pipeline tasks
    let cancel = tokio_util::sync::CancellationToken::new();
    let cap_stop = cancel.clone();
    let cap_done = done.clone();
    let yuv_tx_cap = yuv_tx.clone();
    let frame_size = (out_w * out_h * 3 / 2) as usize;

    // ── Stage 1: Capture + Convert (spawn_blocking) ──
    tasks.spawn_blocking(move || {
        let mut last_raw: Option<Bytes> = None;
        let mut tmp_argb = Vec::new();
        let mut chroma_tick = 0u32;
        let mut last_chroma_boost = 256u32;
        loop {
            if cap_stop.is_cancelled() { break; }

            let frame_start = std::time::Instant::now();

            // Allocate I420 buffer — Vec::with_capacity + set_len avoids unnecessary
            // zero-initialization (capture_to_i420 fills every byte via libyuv).
            // Bytes::from takes ownership zero-copy for downstream.
            let mut i420 = Vec::with_capacity(frame_size);
            // SAFETY: capture_to_i420 fills every byte through with_resize_uninit
            // (clear → set_len → libyuv write). No byte is read before being written.
            unsafe { i420.set_len(frame_size); }

            if let Err(e) = screen_capture.capture_to_i420(&mut i420, out_w, out_h,
                needs_scaling, &mut tmp_argb)
            {
                log::info!("[capture] error: {:#}, repeating last frame", e);
                if let Some(ref last) = last_raw {
                    // Send the previous frame (zero-copy: Arc refcount++), keep last_raw intact.
                    if let Err(e) = yuv_tx_cap.try_send(last.clone()) {
                        if matches!(e, block_mpsc::TrySendError::Disconnected(_)) {
                            log::debug!("[capture] output channel closed, exiting");
                            cap_done.notify_one();
                            return;
                        }
                    }
                } else {
                    let elapsed = frame_start.elapsed();
                    if elapsed < frame_duration {
                        std::thread::sleep(frame_duration - elapsed);
                    }
                    continue;
                }
            } else {
                // Capture succeeded — apply Y-plane unsharp mask enhancement
                if enhance > 0.0 {
                    let y_size = out_w as usize * out_h as usize;
                    let uv_size = (out_w as usize / 2) * (out_h as usize / 2);
                    apply_enhancement(
                        &mut i420[..y_size + 2 * uv_size],
                        out_w as usize, out_h as usize,
                        enhance, &mut tmp_argb,
                        &mut chroma_tick, &mut last_chroma_boost,
                    );
                }
                // Zero-copy: Vec -> Bytes (takes ownership of Vec allocation)
                let frame = Bytes::from(i420);
                // Share via Arc: last_raw holds a clone (refcount++), send the original
                last_raw = Some(frame.clone());
                if let Err(e) = yuv_tx_cap.try_send(frame) {
                    match e {
                        block_mpsc::TrySendError::Full(_) => {
                            log::trace!("[capture] dropping frame (channel full)");
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        block_mpsc::TrySendError::Disconnected(_) => {
                            log::debug!("[capture] output channel closed, exiting");
                            cap_done.notify_one();
                            return;
                        }
                    }
                }
            }
            if cap_stop.is_cancelled() { return; }

            let elapsed = frame_start.elapsed();
            if elapsed < frame_duration {
                std::thread::sleep(frame_duration - elapsed);
            }
        }
        log::debug!("[capture] task ended");
    });

    // ── Stage 3: Encode (spawn_blocking, single-threaded) ──
    let enc_stop = cancel.clone();
    let enc_tx_clone = enc_tx.clone();
    let target_bps = Arc::new(AtomicU32::new(initial_bitrate_bps));
    let enc_bps = target_bps.clone();
    let enc_done = done.clone();
    let enc_args = state.args.clone();
    let (enc_w, enc_h) = (out_w, out_h);
    tasks.spawn_blocking(move || {
        let mut enc_buf = Vec::with_capacity(65536);
        let mut last_idr = std::time::Instant::now();
        let mut enc_failures = 0u32;
        let mut hard_failures = 0u32;
        const ENC_MAX_FAILURES: u32 = 10;
        const ENC_HARD_LIMIT: u32 = 3;
        const IDR_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
        loop {
            if enc_stop.is_cancelled() { break; }
            // Check for adaptive bitrate update
            let desired = enc_bps.load(Ordering::Relaxed);
            encoder.set_bitrate(desired);
            match yuv_rx.recv_timeout(std::time::Duration::from_millis(20)) {
                Ok(yuv) => {
                    enc_buf.clear();
                    if let Err(e) = encoder.encode(yuv.as_ref(), &mut enc_buf) {
                        enc_failures += 1;
                        hard_failures += 1;
                        log::error!("[encoder] error #{}/{} (hard={}/{}): {:#}",
                            enc_failures, ENC_MAX_FAILURES, hard_failures, ENC_HARD_LIMIT, e);
                        // Hard limit: if encoder keeps failing after multiple resets, give up
                        if hard_failures >= ENC_HARD_LIMIT {
                            log::error!("[encoder] hard limit reached, disconnecting");
                            enc_done.notify_one();
                            break;
                        }
                        if enc_failures >= ENC_MAX_FAILURES {
                            log::error!("[encoder] too many failures, resetting encoder");
                            // Give the encoder some breathing room, then recreate it
                            std::thread::sleep(std::time::Duration::from_millis(
                                enc_failures as u64 * 100));
                            encoder = match VideoEncoder::new(&enc_args, enc_w, enc_h) {
                                Ok(e) => e,
                                Err(_) => { enc_done.notify_one(); break; }
                            };
                            enc_failures = 0;
                            continue;
                        }
                        encoder.force_keyframe();
                        // Exponential backoff before retry
                        std::thread::sleep(std::time::Duration::from_millis(
                            enc_failures.min(10) as u64 * 100));
                        continue;
                    }
                    enc_failures = 0; // reset on success
                    // Copy encoded data into Bytes (exact size allocation).
                    let frame = Bytes::copy_from_slice(&enc_buf);
                    enc_buf.clear();
                    if enc_stop.is_cancelled() { return; }
                    match enc_tx_clone.try_send(frame) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            // 通道满，丢弃当前帧，强制 IDR
                            encoder.force_keyframe();
                            log::trace!("[encoder] dropping frame: channel full");
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                    }
                    // Force IDR on a wall-clock basis so timing is unaffected
                    // by encoder stalls or frame drops.
                    if last_idr.elapsed() >= IDR_INTERVAL {
                        encoder.force_keyframe();
                        last_idr = std::time::Instant::now();
                    }
                }
                Err(block_mpsc::RecvTimeoutError::Timeout) => continue,
                Err(block_mpsc::RecvTimeoutError::Disconnected) => {
                    enc_done.notify_one();
                    break;
                }
            }
        }
        log::debug!("[encoder] task ended");
    });

    // ── Stage 4: Send (async) ──
    let send_stop = cancel.clone();
    let send_done = done.clone();
    tasks.spawn(async move {
        loop {
            tokio::select! {
                Some(h264) = enc_rx.recv() => {
                    if h264.is_empty() { continue; }
                    if let Err(e) = track.sample_writer(track_ssrc).write_sample(&Sample {
                        data: h264,
                        duration: frame_duration,
                        ..Default::default()
                    }).await {
                        log::info!("[send] write_sample error: {}", e);
                        send_done.notify_one();
                        break;
                    }
                }
                _ = send_stop.cancelled() => { break; }
            }
        }
        log::debug!("[send] task ended");
    });

    // ═══════════════════════════════════════════════════════════
    //  Audio pipeline: PulseAudio → Opus → WebRTC (parallel to video)
    // ═══════════════════════════════════════════════════════════

    use libpulse_binding as pulse;

    // Auto-detect default sink monitor source for system audio capture
    let audio_source = tokio::task::spawn_blocking(find_default_monitor).await.unwrap_or(None);
    if let Some(ref src) = audio_source {
        log::info!("[audio] using monitor source: {}", src);
    } else {
        log::info!("[audio] no monitor found, falling back to default record source");
    }

    let audio_frame_duration = std::time::Duration::from_millis(20);

    // Audio channels — std sync_channel for blocking pipeline (recv_timeout available)
    let (pcm_tx, pcm_rx) = block_mpsc::sync_channel::<Vec<u8>>(8);
    let (opus_tx, mut opus_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);

    // PCM memory pool for zero-alloc audio capture
    use crossbeam_queue::ArrayQueue;
    const PCM_FRAME_BYTES: usize = 3840; // 20ms stereo 48kHz S16LE
    let pcm_pool = {
        let pool = ArrayQueue::new(8);
        for _ in 0..8 {
            let _ = pool.push(vec![0u8; PCM_FRAME_BYTES]);
        }
        Arc::new(pool)
    };

    // ── Audio Stage 1: PulseAudio capture (event-driven, with wakeup) ──
    let audio_cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let audio_cancelled_clone = audio_cancelled.clone();
    let audio_pcm_tx_thread = pcm_tx.clone();
    let pcm_pool_cap = pcm_pool.clone();
    let audio_source_thread = audio_source.clone();
    // PA mainloop pointer, protected by mutex for thread-safe wakeup.
    // pa_mainloop_wakeup() is explicitly documented as thread-safe by PulseAudio.
    // We only call wakeup() through the pointer, never drop() — safe even though Mainloop !Send.
    use std::sync::Mutex;
    struct PaMainloopPtr(Mutex<Option<*mut pulse::mainloop::standard::Mainloop>>);
    unsafe impl Send for PaMainloopPtr {}
    unsafe impl Sync for PaMainloopPtr {}
    let pa_mainloop_ptr = Arc::new(PaMainloopPtr(Mutex::new(None)));
    let pa_mainloop_ptr_clone = pa_mainloop_ptr.clone();
    tasks.spawn_blocking(move || {
        use pulse::mainloop::standard::IterateResult;

        let spec = pulse::sample::Spec {
            format: pulse::sample::Format::S16NE,
            channels: 2,
            rate: 48000,
        };
        if !spec.is_valid() {
            log::info!("[audio] invalid PulseAudio sample spec");
            return;
        }

        let mut mainloop = match pulse::mainloop::standard::Mainloop::new() {
            Some(m) => m,
            None => { log::error!("[audio] PA mainloop init failed"); return; }
        };

        // Store pointer for external wakeup (called from cleanup on any thread).
        *pa_mainloop_ptr_clone.0.lock().unwrap() = Some(&mut mainloop as *mut _);

        let mut ctx = match pulse::context::Context::new(&mainloop, "vnrit") {
            Some(c) => c,
            None => { log::error!("[audio] PA context init failed"); return; }
        };

        // Connect and wait for context to be ready
        if ctx.connect(None, pulse::context::FlagSet::NOFLAGS, None).is_err() {
            log::error!("[audio] PA connect failed");
            return;
        }
        for _ in 0..200 {
            if audio_cancelled_clone.load(std::sync::atomic::Ordering::Relaxed) { return; }
            let _ = mainloop.iterate(false);
            if ctx.get_state() == pulse::context::State::Ready { break; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if ctx.get_state() != pulse::context::State::Ready {
            log::error!("[audio] PA context not ready");
            *pa_mainloop_ptr_clone.0.lock().unwrap() = None;
            return;
        }

        // Create recording stream
        let dev = audio_source_thread.as_deref();
        let mut stream = match pulse::stream::Stream::new(&mut ctx, "audio-capture", &spec, None) {
            Some(s) => s,
            None => {
                log::error!("[audio] PA Stream::new failed");
                *pa_mainloop_ptr_clone.0.lock().unwrap() = None;
                return;
            }
        };

        // Connect for recording
        if stream.connect_record(dev, None, pulse::stream::FlagSet::NOFLAGS).is_err() {
            log::error!("[audio] PA connect_record failed");
            return;
        }

        log::info!("[audio] PulseAudio capture started (event-driven)");

        // Event-driven mainloop: iterate(true) blocks until PA events arrive.
        // After each event, drain available audio data with non-blocking peek().
        // PCM accumulation buffer: PA may deliver partial frames; accumulate until 3840 bytes ready.
        use std::collections::VecDeque;
        let mut pcm_accum = VecDeque::<u8>::with_capacity(PCM_FRAME_BYTES);
        while !audio_cancelled_clone.load(std::sync::atomic::Ordering::Relaxed) {
            match mainloop.iterate(true) {
                IterateResult::Quit(_) => break,
                _ => {}
            }
            // Drain all available data
            loop {
                match stream.peek() {
                    Ok(pulse::stream::PeekResult::Data(data)) => {
                        pcm_accum.extend(&data[..]);
                        let _ = stream.discard();
                        // Safety cap: if accumulator exceeds 2 frames, trim oldest data.
                        // Prevents unbounded growth from pathological PA fragment sizes.
                        const PCM_ACCUM_MAX: usize = PCM_FRAME_BYTES * 2;
                        if pcm_accum.len() > PCM_ACCUM_MAX {
                            let excess = pcm_accum.len() - PCM_ACCUM_MAX;
                            pcm_accum.drain(..excess);
                        }
                        // Emit complete frames from the accumulator
                        while pcm_accum.len() >= PCM_FRAME_BYTES {
                            let mut buf = match pcm_pool_cap.pop() {
                                Some(b) => b,
                                None => vec![0u8; PCM_FRAME_BYTES],
                            };
                            for (i, byte) in pcm_accum.drain(..PCM_FRAME_BYTES).enumerate() {
                                buf[i] = byte;
                            }
                            if audio_pcm_tx_thread.send(buf).is_err() {
                                log::debug!("[audio] PCM channel closed, exiting");
                                *pa_mainloop_ptr_clone.0.lock().unwrap() = None;
                                return;
                            }
                        }
                    }
                    _ => break, // Empty or Hole → no more data
                }
            }
        }

        // Clear PA mainloop pointer before exit (prevents dangling pointer in cleanup)
        *pa_mainloop_ptr_clone.0.lock().unwrap() = None;
        log::debug!("[audio] capture task ended");
    });

    // ── Audio Stage 2: Opus encode (spawn_blocking) ──
    let audio_enc_stop = cancel.clone();
    let audio_enc_done = done.clone();
    let audio_opus_tx = opus_tx.clone();
    let pcm_pool_enc = pcm_pool.clone();
    tasks.spawn_blocking(move || {
        let mut encoder = match opus::Encoder::new(48000, opus::Channels::Stereo, opus::Application::Audio) {
            Ok(e) => e,
            Err(e) => {
                log::info!("[audio] Opus encoder init failed: {}", e);
                return;
            }
        };
        let mut opus_out = Vec::with_capacity(4096);
        loop {
            if audio_enc_stop.is_cancelled() { break; }
            match pcm_rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(pcm) => {
                    // Safety: pcm comes from PulseAudio which always returns multiples of frame size
                    // (3840 bytes = 1920 i16 samples @ 20ms stereo 48kHz). Assert to prevent UB.
                    assert_eq!(pcm.len() % 2, 0, "PCM buffer length must be even for i16 samples");
                    let samples = unsafe {
                        std::slice::from_raw_parts(pcm.as_ptr() as *const i16, pcm.len() / 2)
                    };
                    opus_out.clear();
                    opus_out.reserve(4096);
                    // SAFETY: encoder.encode 写入前 n 字节，不读 buf，truncate 丢弃未写部分
                    unsafe { opus_out.set_len(4096); }
                    match encoder.encode(samples, &mut opus_out) {
                        Ok(n) => {
                            opus_out.truncate(n);
                            // Return PCM buffer to pool for reuse
                            let _ = pcm_pool_enc.push(pcm);
                            // Zero-copy: send Opus Vec directly
                            if audio_enc_stop.is_cancelled() { return; }
                            if audio_opus_tx.try_send(std::mem::take(&mut opus_out)).is_err() {
                                log::trace!("[audio] dropping Opus packet (channel full)");
                            }
                        }
                        Err(e) => {
                            log::info!("[audio] encode error: {}", e);
                            let _ = pcm_pool_enc.push(pcm);
                        }
                    }
                }
                Err(block_mpsc::RecvTimeoutError::Timeout) => continue,
                Err(block_mpsc::RecvTimeoutError::Disconnected) => {
                    audio_enc_done.notify_one();
                    break;
                }
            }
        }
        log::debug!("[audio] encode task ended");
    });

    // ── Audio Stage 3: Send Opus packets via WebRTC (async) ──
    let audio_send_stop = cancel.clone();
    let audio_send_done = done.clone();
    tasks.spawn(async move {
        loop {
            tokio::select! {
                Some(opus_packet) = opus_rx.recv() => {
                    if opus_packet.is_empty() { continue; }
                    // Zero-copy Vec→Bytes (moves heap allocation, no copy)
                    let data = Bytes::from(opus_packet);
                    if let Err(e) = audio_track.sample_writer(audio_ssrc).write_sample(&Sample {
                        data,
                        duration: audio_frame_duration,
                        ..Default::default()
                    }).await {
                        log::info!("[audio] write_sample error: {}", e);
                        audio_send_done.notify_one();
                        break;
                    }
                }
                _ = audio_send_stop.cancelled() => { break; }
            }
        }
        log::debug!("[audio] send task ended");
    });

    // ── XI2 event-driven cursor tracking ──
    // AsyncFd wraps the X11 connection fd for tokio select readiness.
    // When the fd becomes readable we poll for XI2 Motion events and
    // update the cursor position without any X11 RPC overhead.
    use tokio::io::unix::AsyncFd;
    use std::os::unix::io::FromRawFd;
    // Dup the fd so AsyncFd can own it independently of evt_conn
    let fd = evt_conn.stream().as_raw_fd();
    let dup_fd = unsafe { libc::dup(fd) };
    if dup_fd < 0 {
        log::error!("[x11] failed to dup event connection fd");
        // Clean up pipeline tasks and connections before returning
        // Signal PA mainloop and all async tasks to stop
        audio_cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
        {
            let mut guard = pa_mainloop_ptr.0.lock().unwrap();
            if let Some(ptr) = *guard {
                unsafe { (&mut *ptr).wakeup(); }
                *guard = None;
            }
        }
        cancel.cancel();
        // Drop channels before waiting for tasks — makes blocking recv exit
        drop(yuv_tx); drop(enc_tx); drop(pcm_tx); drop(opus_tx);
        // Wait for tasks to exit before releasing WebRTC resources
        tasks.shutdown().await;
        let _ = pc.close().await;
        drop(pc);
        drop(handler);
        unsafe { mi_collect(true); }
        return;
    }
    let evt_fd = match unsafe { AsyncFd::new(std::os::unix::io::OwnedFd::from_raw_fd(dup_fd)) } {
        Ok(fd) => fd,
        Err(e) => {
            log::error!("[x11] failed to create AsyncFd for event connection: {}", e);
            audio_cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            {
                let mut guard = pa_mainloop_ptr.0.lock().unwrap();
                if let Some(ptr) = *guard {
                    unsafe { (&mut *ptr).wakeup(); }
                    *guard = None;
                }
            }
            cancel.cancel();
            // Drop channels before waiting for tasks — makes blocking recv exit
            drop(yuv_tx); drop(enc_tx); drop(pcm_tx); drop(opus_tx);
            // Wait for tasks to exit before releasing WebRTC resources
            tasks.shutdown().await;
            let _ = pc.close().await;
            drop(pc);
            drop(handler);
            unsafe { mi_collect(true); }
            return;
        }
    };

    // ── Periodic stats timer (separate from cursor sync) ──
    // Runs at 200ms; every 3 ticks (~600ms) we collect RTT + adaptive bitrate.
    let mut stats_timer = time::interval(Duration::from_millis(200));

    // ── Input handling runs in main task (non-blocking) ──
    let scale_x = native_w as f64 / out_w as f64;
    let scale_y = native_h as f64 / out_h as f64;
    // Use the DataChannel from create_data_channel directly, NOT the one from
    // on_data_channel (which may be exhausted on reconnect). See on_data_channel.
    let mut dc_ref: Option<Arc<dyn DataChannel>> = input_dc;
    let mut stats_tick = 0u32;

    loop {
        tokio::select! {
            // DataChannel input (non-blocking — only fires when a message arrives)
            dc_event = async {
                if let Some(ref dc) = dc_ref {
                    dc.poll().await
                } else {
                    futures_util::future::pending::<Option<DataChannelEvent>>().await
                }
            } => {
                match dc_event {
                    Some(DataChannelEvent::OnClose) => {
                        log::info!("[dc] data channel closed by remote");
                        dc_ref = None;
                    }
                    Some(DataChannelEvent::OnMessage(msg)) => {
                        if let Ok(text) = std::str::from_utf8(&msg.data) {
                            if text.starts_with("mr,") || text.starts_with("ma,")
                                || text.starts_with("md,") || text.starts_with("mu,")
                                || text.starts_with("ms,") || text.starts_with("kd,")
                                || text.starts_with("ku,")
                            {
                                handle_input_message(&text, &input_state, scale_x, scale_y);
                                send_cursor_position(&out_tx, &input_state, native_w, native_h, out_w, out_h);
                            }
                        }
                    }
                    // poll() returning None means the channel is fully closed
                    // and will never emit more events. Clean up to stop polling.
                    None => {
                        log::info!("[dc] data channel exhausted, stopping poll");
                        dc_ref = None;
                    }
                    // Ignore other events (OnOpen, OnError, OnClosing, etc.)
                    _ => {}
                }
            }

            // Event-driven cursor sync via XI2 (no polling)
            _ = evt_fd.readable() => {
                let mut guard = match evt_fd.readable().await {
                    Ok(g) => g,
                    Err(e) => {
                        log::error!("[x11] XI2 event fd error: {}", e);
                        break;
                    }
                };
                guard.clear_ready();
                while let Ok(Some(event)) = evt_conn.poll_for_event() {
                    if let Event::XinputMotion(ev) = event {
                        // Fp1616 is 16.16 fixed-point → shift right 16 for integer pixel coords
                        let rx = (ev.root_x >> 16) as i32;
                        let ry = (ev.root_y >> 16) as i32;
                        input_state.cursor_x.store(rx, Ordering::Relaxed);
                        input_state.cursor_y.store(ry, Ordering::Relaxed);
                        send_cursor_position(&out_tx, &input_state, native_w, native_h, out_w, out_h);
                    }
                }
            }

            // Periodic stats collection (RTT + adaptive bitrate)
            _ = stats_timer.tick() => {
                stats_tick += 1;
                // Collect stats every ~600ms (200ms × 3)
                if stats_tick % 3 == 0 {
                    let now = std::time::Instant::now();
                    let report = pc.get_stats(now, StatsSelector::None).await;
                    let rtt_ms = report.candidate_pairs()
                        .find_map(|p| if p.current_round_trip_time > 0.0 { Some(p.current_round_trip_time * 1000.0) } else { None });
                    if let Some(rtt) = rtt_ms {
                        if out_tx.try_send(Message::Text(
                            serde_json::json!({"type":"stats","rtt_ms": rtt as u32}).to_string().into()
                        )).is_err() {
                            log::warn!("[stats] failed to send stats");
                        }
                    }
                    // Adaptive bitrate: TWCC estimation + packet loss back-off
                    if adaptive_bitrate {
                        let mut desired_bps = 0u32;
                        // Check TWCC target bitrate from outbound stats
                        for outbound in report.outbound_rtp_streams() {
                            let twcc_bps = outbound.target_bitrate as u32;
                            if twcc_bps > 0 { desired_bps = twcc_bps; break; }
                        }
                        // Check packet loss from inbound stats — reduce aggressively
                        for inbound in report.inbound_rtp_streams() {
                            let total = inbound.received_rtp_stream_stats.packets_received
                                + inbound.received_rtp_stream_stats.packets_lost.max(0) as u64;
                            if total > 100 {
                                let ratio = inbound.received_rtp_stream_stats.packets_lost as f64 / total as f64;
                                if ratio > 0.05 {
                                    // >5% loss → cut to 80% of current
                                    let cur = target_bps.load(Ordering::Relaxed);
                                    desired_bps = (cur as f64 * 0.8) as u32;
                                    log::debug!("[abr] loss={:.1}%, cutting to {}kbps", ratio * 100.0, desired_bps / 1000);
                                }
                            }
                            break;
                        }
                        if desired_bps > 0 {
                            let current_bps = target_bps.load(Ordering::Relaxed);
                            let new_bps = desired_bps.max(200_000).min(initial_bitrate_bps);
                            if (new_bps as i32 - current_bps as i32).abs() >= 50000 {
                                target_bps.store(new_bps, Ordering::Relaxed);
                                log::debug!("[abr] bitrate={}kbps (twcc={}kbps)", new_bps / 1000, desired_bps / 1000);
                            }
                        }
                    }
                }
            }

            // Handle incoming WebSocket messages (input + signaling)
            msg = in_rx.recv() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        // Fast path: check for input events first (most frequent)
                        if t.starts_with("mr,") || t.starts_with("ma,")
                            || t.starts_with("md,") || t.starts_with("mu,")
                            || t.starts_with("ms,") || t.starts_with("kd,")
                            || t.starts_with("ku,")
                        {
                            handle_input_message(&t, &input_state, scale_x, scale_y);
                            send_cursor_position(&out_tx, &input_state, native_w, native_h, out_w, out_h);
                        } else if let Ok(sig) = serde_json::from_str::<SignalingMessage>(&t) {
                            match sig {
                                SignalingMessage::Ice { candidate, sdp_mline_index } => {
                                    let init = RTCIceCandidateInit {
                                        candidate,
                                        sdp_mline_index: Some(sdp_mline_index as u16),
                                        ..Default::default()
                                    };
                                    let _ = pc.add_ice_candidate(init).await;
                                }
                                SignalingMessage::Offer { .. } => {
                                    log::debug!("[ws] unexpected duplicate offer, ignoring");
                                }
                                _ => {}
                            }
                        } else {
                            let preview: String = t.chars().take(100).collect();
                            log::debug!("[ws] unrecognized message: '{}'", preview);
                            let _ = out_tx.try_send(Message::Text(
                                serde_json::json!({"type":"error","message":"invalid message format"}).to_string().into()
                            ));
                        }
                    }
                    Some(Ok(Message::Close(_))) => {
                        log::debug!("[ws] client sent close");
                        break;
                    }
                    Some(Err(_)) | None => {
                        log::debug!("[ws] channel disconnected");
                        break;
                    }
                    _ => {}
                }
            }

            // Register DataChannel (fallback): dc_ref is set from input_dc (returned by
            // create_data_channel) right after signaling. The on_data_channel callback
            // may provide a separate Arc<dyn DataChannel> reference that has already
            // exhausted on reconnect — so we always prefer the create_data_channel one.
            // This branch only fires if dc_ref somehow remains None (shouldn't happen).
            _ = async {
                if dc_ref.is_some() {
                    futures_util::future::pending::<()>().await
                } else {
                    dc_rx.changed().await.ok();
                }
            } => {
                if let Some(dc) = &*dc_rx.borrow_and_update() {
                    log::info!("[dc] data channel ready for input");
                    dc_ref = Some(dc.clone());
                }
            }

            // Check peer connection state
            _ = done.notified() => {
                log::debug!("[loop] connection closed, exiting");
                break;
            }

            // Safety net: if all tasks have been cancelled (e.g., cancelled before done triggered),
            // exit the main loop rather than blocking forever on out_tx.closed() below.
            _ = cancel.cancelled() => {
                log::debug!("[loop] cancel triggered, exiting");
                break;
            }

            // Detect WebSocket disconnect: when the WS send task exits,
            // out_tx's receiver is dropped, and closed() resolves immediately.
            _ = out_tx.closed() => {
                log::debug!("[loop] WebSocket send channel closed, exiting");
                break;
            }
        }
    }

    // ── Phase 0: Immediately release pressed keys ──
    // Must happen before cleanup (which can take seconds), so a reconnecting
    // session doesn't inherit stuck modifier keys from the old one.
    {
        let keys = input_state.pressed_keys.lock().unwrap();
        for &kc in keys.iter() {
            let _ = xtest::fake_input(&input_state.conn, X11_KEY_RELEASE,
                kc, 0, input_state.root, 0, 0, 0);
        }
        if !keys.is_empty() {
            let _ = input_state.conn.flush();
        }
    }

    // ── CLEANUP ──
    log::info!("[cleanup] client disconnected, shutting down pipeline...");

    // ── Phase 1: Signal cancellation and wake blocking tasks ──
    // Cancel the main CancellationToken first so all async select! branches
    // that listen on cancel.cancelled() bail out immediately.
    audio_cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
    {
        let mut guard = pa_mainloop_ptr.0.lock().unwrap();
        if let Some(ptr) = *guard {
            unsafe { (&mut *ptr).wakeup(); }
            // Clear the pointer so wakeup() is never called again on a dangling ptr
            *guard = None;
        }
    }
    cancel.cancel();

    // ── Phase 2: Drop all channels first — this makes blocking recv tasks
    //   get Disconnected instantly and the async recv tasks see None,
    //   causing them to exit their loops. We drop channels BEFORE waiting
    //   for tasks so that tasks aren't stuck in recv() calls.
    drop(yuv_tx);
    drop(enc_tx);
    drop(pcm_tx);
    drop(opus_tx);

    // ── Phase 3: Wait for tasks to exit — Pipeline tasks check cancel token
    //   and should exit within ~100ms after cancel. 3s safety net for edge cases.
    if let Err(_) = tokio::time::timeout(Duration::from_secs(3), tasks.shutdown()).await {
        log::error!("[cleanup] shutdown timed out — some tasks may still be running");
    }

    // ── Phase 4: All background tasks have exited. Now it's safe to release
    //   WebRTC resources — no task is still calling write_sample() or
    //   holding Arc references to the tracks.
    // Do NOT call dc.close() — pc.close() handles DataChannel cleanup
    // internally via SCTP transport shutdown. Calling both causes the
    // driver to close the same channel twice, producing Disconnected errors.
    drop(dc_ref);
    let _ = pc.close().await;
    drop(pc);
    drop(handler);
    // track and audio_track are moved into send tasks, dropped when tasks exit above

    // Force mimalloc to return cached memory segments to the OS.
    // Per-session allocations (openh264 ref frames ~1.4MB, X11 buffers, audio)
    // are freed but mimalloc retains them in thread-local heaps. Without
    // explicit collection this manifests as a ~1.5MB/heap RSS increment.
    unsafe { mi_collect(true); }

    log::info!("[ws] cleanup complete");
}

// ═══════════════════════════════════════════════════════════════
//  Input handling — keyboard/mouse injection via XTest
// ═══════════════════════════════════════════════════════════════

fn handle_input_message(raw: &str, state: &InputState, scale_x: f64, scale_y: f64) {
    // Use split() iterator directly — no Vec allocation per input event.
    let mut fields = raw.split(',');

    let cmd = match fields.next() {
        Some(c) => c,
        None => return,
    };

    match cmd {
        "mr" => {
            let dx: i32 = match fields.next().and_then(|s| s.parse().ok()) { Some(v) => v, None => return };
            let dy: i32 = match fields.next().and_then(|s| s.parse().ok()) { Some(v) => v, None => return };
            let max_x = state.screen_w as i32 - 1;
            let max_y = state.screen_h as i32 - 1;
            let new_x = state.cursor_x.load(Ordering::Relaxed).saturating_add(dx).clamp(0, max_x);
            let new_y = state.cursor_y.load(Ordering::Relaxed).saturating_add(dy).clamp(0, max_y);
            state.cursor_x.store(new_x, Ordering::Relaxed);
            state.cursor_y.store(new_y, Ordering::Relaxed);
            let _ = xtest::fake_input(&state.conn, X11_MOTION_NOTIFY,
                0, 0, state.root, new_x as i16, new_y as i16, 0);
            let _ = state.conn.flush();
        }
        "ma" => {
            let raw_x: i32 = match fields.next().and_then(|s| s.parse().ok()) { Some(v) => v, None => return };
            let raw_y: i32 = match fields.next().and_then(|s| s.parse().ok()) { Some(v) => v, None => return };
            let new_x = ((raw_x as f64 * scale_x) as i32).clamp(0, state.screen_w as i32 - 1);
            let new_y = ((raw_y as f64 * scale_y) as i32).clamp(0, state.screen_h as i32 - 1);
            state.cursor_x.store(new_x, Ordering::Relaxed);
            state.cursor_y.store(new_y, Ordering::Relaxed);
            let _ = xtest::fake_input(&state.conn, X11_MOTION_NOTIFY,
                0, 0, state.root, new_x as i16, new_y as i16, 0);
            let _ = state.conn.flush();
        }
        "md" => {
            let btn: u8 = match fields.next() {
                Some("2") => 2,
                Some("3") => 3,
                _ => 1,
            };
            let cx = state.cursor_x.load(Ordering::Relaxed) as i16;
            let cy = state.cursor_y.load(Ordering::Relaxed) as i16;
            let _ = xtest::fake_input(&state.conn, X11_BUTTON_PRESS,
                btn, 0, state.root, cx, cy, 0);
            let _ = state.conn.flush();
        }
        "mu" => {
            let btn: u8 = match fields.next() {
                Some("2") => 2,
                Some("3") => 3,
                _ => 1,
            };
            let cx = state.cursor_x.load(Ordering::Relaxed) as i16;
            let cy = state.cursor_y.load(Ordering::Relaxed) as i16;
            let _ = xtest::fake_input(&state.conn, X11_BUTTON_RELEASE,
                btn, 0, state.root, cx, cy, 0);
            let _ = state.conn.flush();
        }
        "ms" => {
            let delta: f64 = match fields.next().and_then(|s| s.parse().ok()) { Some(v) => v, None => return };
            let steps = (delta.abs() / 40.0).round().clamp(1.0, 20.0) as u32;
            let btn = if delta > 0.0 { 5_u8 } else { 4_u8 };
            let cx = state.cursor_x.load(Ordering::Relaxed) as i16;
            let cy = state.cursor_y.load(Ordering::Relaxed) as i16;
            for _ in 0..steps {
                let _ = xtest::fake_input(&state.conn, X11_BUTTON_PRESS,
                    btn, 0, state.root, cx, cy, 0);
                let _ = xtest::fake_input(&state.conn, X11_BUTTON_RELEASE,
                    btn, 0, state.root, cx, cy, 0);
            }
            let _ = state.conn.flush();
        }
        "kd" => {
            let keysym = match fields.next() {
                Some(s) => code_to_keysym(s),
                None => return,
            };
            if keysym != 0 {
                let kc = find_keycode(state, keysym);
                if kc > 0 {
                    state.pressed_keys.lock().unwrap().insert(kc);
                    let cx = state.cursor_x.load(Ordering::Relaxed) as i16;
                    let cy = state.cursor_y.load(Ordering::Relaxed) as i16;
                    let _ = xtest::fake_input(&state.conn, X11_KEY_PRESS,
                        kc, 0, state.root, cx, cy, 0);
                    let _ = state.conn.flush();
                }
            }
        }
        "ku" => {
            let keysym = match fields.next() {
                Some(s) => code_to_keysym(s),
                None => return,
            };
            if keysym != 0 {
                let kc = find_keycode(state, keysym);
                if kc > 0 {
                    state.pressed_keys.lock().unwrap().remove(&kc);
                    let cx = state.cursor_x.load(Ordering::Relaxed) as i16;
                    let cy = state.cursor_y.load(Ordering::Relaxed) as i16;
                    let _ = xtest::fake_input(&state.conn, X11_KEY_RELEASE,
                        kc, 0, state.root, cx, cy, 0);
                    let _ = state.conn.flush();
                }
            }
        }
        _ => {}
    }
}

/// Send current cursor position to browser — reads from the in-memory
/// cursor_x/cursor_y (set by browser input or X11 sync).  Does NOT query
/// X11 itself; use sync_cursor_position for periodic X11 reads.
fn send_cursor_position(out_tx: &mpsc::Sender<Message>, state: &InputState,
    native_w: u16, native_h: u16, out_w: u32, out_h: u32)
{
    let x = state.cursor_x.load(Ordering::Relaxed);
    let y = state.cursor_y.load(Ordering::Relaxed);
    let packed = (x as u64) | ((y as u64) << 32);
    // Atomic swap: skip if the same position was already sent
    let prev = state.last_sent_packed.swap(packed, Ordering::Relaxed);
    if prev == packed {
        return;
    }
    // Scale from native X11 coordinates to encoded video coordinates
    let sx = if native_w > 0 { x as u64 * out_w as u64 / native_w as u64 } else { x as u64 };
    let sy = if native_h > 0 { y as u64 * out_h as u64 / native_h as u64 } else { y as u64 };
    // Format cursor JSON directly without serde_json::Value allocation
    let msg = format!(r#"{{"type":"cursor","x":{},"y":{}}}"#, sx, sy);
    let _ = out_tx.try_send(Message::Text(msg.into()));
}

fn find_keycode(s: &InputState, keysym: u32) -> u8 {
    s.keycode_cache.get(&keysym).copied().unwrap_or(0)
}

fn code_to_keysym(code: &str) -> u32 {
    // Perfect-hash static lookup map — compile-time generated, O(1), no hashing overhead.
    use phf::phf_map;
    static KEYMAP: phf::Map<&'static str, u32> = phf_map! {
        "Enter" => 0xff0d,
        "Backspace" => 0xff08,
        "Space" => 0x0020,
        "Tab" => 0xff09,
        "Escape" => 0xff1b,
        "ArrowUp" => 0xff52,
        "ArrowDown" => 0xff54,
        "ArrowLeft" => 0xff51,
        "ArrowRight" => 0xff53,
        "ShiftLeft" => 0xffe1,
        "ShiftRight" => 0xffe1,
        "ControlLeft" => 0xffe3,
        "ControlRight" => 0xffe3,
        "AltLeft" => 0xffe9,
        "AltRight" => 0xffe9,
        "MetaLeft" => 0xffeb,
        "MetaRight" => 0xffeb,
        "CapsLock" => 0xffe5,
        "Delete" => 0xffff,
        "Insert" => 0xff63,
        "Home" => 0xff50,
        "End" => 0xff57,
        "PageUp" => 0xff55,
        "PageDown" => 0xff56,
        "Minus" => 0x002d,
        "Equal" => 0x003d,
        "BracketLeft" => 0x005b,
        "BracketRight" => 0x005d,
        "Semicolon" => 0x003b,
        "Quote" => 0x0027,
        "Backquote" => 0x0060,
        "PrintScreen" => 0xff61,
        "ScrollLock" => 0xff14,
        "Pause" => 0xff13,
        "Break" => 0xff6b,
        "SysRq" => 0xff15,
        "NumLock" => 0xff7f,
        "Comma" => 0x002c,
        "Period" => 0x002e,
        "Slash" => 0x002f,
        "Backslash" => 0x005c,
        "IntlBackslash" => 0x005c,
        "Numpad0" => 0xffb0,
        "Numpad1" => 0xffb1,
        "Numpad2" => 0xffb2,
        "Numpad3" => 0xffb3,
        "Numpad4" => 0xffb4,
        "Numpad5" => 0xffb5,
        "Numpad6" => 0xffb6,
        "Numpad7" => 0xffb7,
        "Numpad8" => 0xffb8,
        "Numpad9" => 0xffb9,
        "NumpadEnter" => 0xff8d,
        "NumpadAdd" => 0xffab,
        "NumpadSubtract" => 0xffad,
        "NumpadMultiply" => 0xffaa,
        "NumpadDivide" => 0xffaf,
        "NumpadDecimal" => 0xffae,
    };

    if let Some(&keysym) = KEYMAP.get(code) {
        return keysym;
    }
    // Function keys: F1..F24
    if code.starts_with('F') && code.len() <= 4 {
        let n: u32 = code[1..].parse().unwrap_or(0);
        if (1..=24).contains(&n) {
            return 0xffbe + n - 1;
        }
    }
    // Digit keys: Digit0..Digit9
    if let Some(d) = code.strip_prefix("Digit") {
        if d.len() == 1 {
            let b = d.as_bytes()[0];
            if b.is_ascii_digit() {
                return b as u32;
            }
        }
    }
    // Letter keys: KeyA..KeyZ
    if let Some(c) = code.strip_prefix("Key") {
        if c.len() == 1 {
            let b = c.as_bytes()[0];
            if b.is_ascii_alphabetic() {
                return b as u32;
            }
        }
    }
    0
}

// ── WebSocket compression helpers ──
//
// Application-level deflate compression for text messages > 512 bytes.
// Protocol: first byte = 0 (uncompressed) or 1 (deflate-compressed), followed by payload.

/// Returns true if the JSON text is a WebRTC signaling message.
/// Signaling is always sent as uncompressed Text for browser compatibility.
fn is_signaling_message(text: &str) -> bool {
    text.starts_with("{\"type\":\"offer\"")
        || text.starts_with("{\"type\":\"answer\"")
        || text.starts_with("{\"type\":\"ice\"")
        || text.starts_with("{\"type\":\"ready\"")
}

/// Compress a text string with deflate and wrap as Binary with prefix byte 0x01.
fn compress_text(text: &str) -> Message {
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
    if encoder.write_all(text.as_bytes()).is_ok() {
        if let Ok(compressed) = encoder.finish() {
            if compressed.len() < text.len() {
                let mut buf = Vec::with_capacity(1 + compressed.len());
                buf.push(1);
                buf.extend_from_slice(&compressed);
                return Message::Binary(Bytes::from(buf));
            }
        }
    }
    // Compression didn't help — send uncompressed
    let mut buf = Vec::with_capacity(1 + text.len());
    buf.push(0);
    buf.extend_from_slice(text.as_bytes());
    Message::Binary(Bytes::from(buf))
}

/// Decompress a Binary message with prefix byte 0x01 back to Text.
fn decompress_binary(data: Bytes) -> Message {
    use flate2::read::DeflateDecoder;
    use std::io::Read;
    let mut decoder = DeflateDecoder::new(&data[1..]);
    let mut s = String::new();
    if decoder.read_to_string(&mut s).is_ok() {
        Message::Text(s.into())
    } else {
        // Decompression failed — try to interpret raw bytes as text
        Message::Text(String::from_utf8_lossy(&data[1..]).to_string().into())
    }
}
