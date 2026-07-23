#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// Force mimalloc to return cached memory to the OS.
unsafe extern "C" {
    fn mi_collect(force: bool) -> ();
}

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc as block_mpsc;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use clap::Parser;
use futures_util::StreamExt;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use wide::i16x8;
use wide::i32x4;
use wide::i32x8;
use wide::u8x16;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::shm::{self, Seg};
use x11rb::protocol::xinput::{self, EventMask, XIEventMask};
use x11rb::protocol::xproto::{self, Window};
use x11rb::protocol::xtest;
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb_protocol::xauth::get_auth;

// ── webrtc-rs types ──
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS};
use rtc::peer_connection::configuration::RTCOfferOptions;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::statistics::StatsSelector;
use rtc_media::Sample;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::{MediaStreamTrack, Track};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceGatheringState, RTCIceServer,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSessionDescription, Registry,
    SettingEngine, register_default_interceptors,
};
use webrtc::runtime;

// ── openh264 ──
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Profile,
    RateControlMode, UsageType,
};
use openh264::formats::YUVSlices;
use openh264_sys2::ENCODER_OPTION_BITRATE;

use async_trait::async_trait;
use bytes::Bytes;
use ice::network_type::NetworkType;
use tokio::task::JoinHandle;
use std::os::fd::IntoRawFd;

use vnrit_libyuv::{self, ArgbImage, FilterMode, I420ImageMut, ImageSize};

// ── libblur: SIMD-accelerated fast blur (used for Y-plane unsharp mask) ──
use libblur::{self, AnisotropicRadius, BlurImageMut, FastBlurChannels, ThreadingPolicy};

/// Resize `buf` to `size` without zero-initialization, then call `write` with the mutable slice.
/// The write closure must write all `size` bytes before returning — reading uninit bytes is UB.
/// After `write` returns, the Vec is guaranteed to contain `size` initialized bytes.
fn with_resize_uninit(buf: &mut Vec<u8>, size: usize, write: impl FnOnce(&mut [u8])) {
    buf.clear();
    buf.reserve(size);
    // SAFETY: reserve guarantees capacity >= size. write() fills all bytes.
    unsafe {
        buf.set_len(size);
    }
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
        help = "HTTP/WebSocket listen port"
    )]
    port: u16,

    #[arg(long, default_value = "24", help = "Capture framerate in fps")]
    framerate: i32,

    #[arg(
        long,
        default_value = "stun:stun.cloudflare.com:3478",
        help = "STUN server URL (set empty string to disable)"
    )]
    stun: String,

    #[arg(long, default_value = "1000", help = "Target bitrate in kbps")]
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
        help = "Log level (off, error, warn, info, debug, trace)"
    )]
    log_level: String,

    #[arg(
        long,
        default_value = "false",
        help = "Use TCP-only ICE (disable UDP, useful when UDP is blocked)"
    )]
    tcp_only: bool,

    #[arg(
        long,
        default_value = "false",
        help = "Enable adaptive bitrate based on WebRTC bandwidth estimation"
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
    Offer {
        sdp: String,
        #[serde(default)]
        renegotiation: Option<bool>,
    },
    #[serde(rename = "answer")]
    Answer { sdp: String },
    #[serde(rename = "ice")]
    Ice {
        candidate: String,
        sdp_mline_index: u32,
    },
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
    /// Pre-computed bicubic weights for Y-plane scaling (None if no scaling needed)
    h_weights: Option<BicubicWeights>,
    v_weights: Option<BicubicWeights>,
}

// Required because ShmScreenCapture is moved into spawn_blocking (crosses thread boundary).
// The *mut u8 is only accessed from one thread at a time — see SAFETY above.
unsafe impl Send for ShmScreenCapture {}
unsafe impl Sync for ShmScreenCapture {}

impl ShmScreenCapture {
    /// Try to create an SHM-accelerated capture. Returns None if SHM is
    /// not available (MIT-SHM extension missing from X server).
    fn try_new(
        capture: Arc<CaptureState>,
        width: u16,
        height: u16,
        depth: u8,
        out_w: u32,
        out_h: u32,
    ) -> Result<Option<Self>> {
        // Calculate bytes-per-pixel for ZPixmap
        // depth 24 → 4 bytes (32-bit padded), depth >24 → 4 bytes
        let bpp = if depth >= 24 {
            4u8
        } else {
            ((depth as u32 + 7) / 8) as u8
        };
        let shm_size = (width as usize) * (height as usize) * (bpp as usize);

        // Query MIT-SHM version to verify availability
        let ver = match shm::query_version(&capture.conn) {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => reply,
                Err(e) => {
                    log::debug!(
                        "[shm] MIT-SHM reply error: {:?}, falling back to get_image",
                        e
                    );
                    return Ok(None);
                }
            },
            Err(e) => {
                log::debug!(
                    "[shm] MIT-SHM query failed: {}, falling back to get_image",
                    e
                );
                return Ok(None);
            }
        };

        if ver.major_version == 0 && ver.minor_version == 0 {
            log::debug!("[shm] MIT-SHM extension missing, falling back to get_image");
            return Ok(None);
        }

        log::debug!(
            "[shm] MIT-SHM v{}.{}, allocating {} bytes ({}x{}x{})",
            ver.major_version,
            ver.minor_version,
            shm_size,
            width,
            height,
            depth
        );

        let shmseg = capture
            .conn
            .generate_id()
            .context("failed to generate SHM seg ID")?;

        // Ask the X server to allocate shared memory and return a file descriptor
        let cookie = shm::create_segment(&capture.conn, shmseg, shm_size as u32, false)
            .context("SHM create_segment failed")?;
        let reply = cookie.reply().context("SHM create_segment reply failed")?;

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
                return Err(anyhow::anyhow!(
                    "mmap failed for SHM segment: size={}",
                    shm_size
                ));
            }
            ptr as *mut u8
        };

        // Close fd — mmap keeps a reference to the underlying file
        unsafe {
            libc::close(raw_fd);
        }

        log::debug!(
            "[shm] segment allocated at {:?} ({} bytes)",
            shm_ptr,
            shm_size
        );

        // Pre-compute bicubic weights if scaling is needed
        let needs_scaling = out_w != width as u32 || out_h != height as u32;
        let (h_weights, v_weights) = if needs_scaling {
            (
                Some(build_bicubic_weights(width as usize, out_w as usize)),
                Some(build_bicubic_weights(height as usize, out_h as usize)),
            )
        } else {
            (None, None)
        };

        Ok(Some(ShmScreenCapture {
            conn: capture,
            width,
            height,
            shmseg,
            shm_ptr,
            shm_size,
            bpp,
            h_weights,
            v_weights,
        }))
    }

    /// Capture the root window and convert directly to I420, bypassing BGRA Vec.
    /// Returns (mad_sum, tv_sum, luma_sum) for the Y plane.
    fn capture_to_i420(
        &self,
        i420_out: &mut Vec<u8>,
        out_w: u32,
        out_h: u32,
        needs_scaling: bool,
        blur_copy: &mut [u8],
        prev: &[u8],
    ) -> Result<(u64, u64, u64)> {
        let cookie = shm::get_image(
            &self.conn.conn,
            self.conn.root, // drawable
            0,              // x offset
            0,              // y offset
            self.width,
            self.height,
            !0, // plane_mask = all planes
            2,  // format = ZPixmap
            self.shmseg,
            0, // offset in shared memory
        )
        .context("SHM get_image failed")?;
        let _reply = cookie.reply().context("SHM get_image reply failed")?;

        // Read BGRA directly from shared memory and convert to I420 in one step.
        let size = (self.width as usize) * (self.height as usize) * (self.bpp as usize);
        // SAFETY: shm_ptr points to X server's shared memory, written by get_image.
        let bgra_slice = unsafe { std::slice::from_raw_parts(self.shm_ptr, size) };

        let stats = if needs_scaling {
            let mut temp_buf = Vec::new();
            scale_bgra_direct(
                bgra_slice,
                self.width as u32,
                self.height as u32,
                out_w,
                out_h,
                i420_out,
                &mut temp_buf,
                self.h_weights.as_ref().unwrap(),
                self.v_weights.as_ref().unwrap(),
                blur_copy,
                prev,
            )
        } else {
            bgra_to_i420(bgra_slice, self.width as u32, self.height as u32, i420_out);
            let y_size = (self.width as usize) * (self.height as usize);
            let y = &i420_out[..y_size];
            blur_copy.copy_from_slice(y);
            let (mut mad, mut tv, mut luma) = (0u64, 0u64, 0u64);
            for i in 0..y_size {
                let yv = y[i] as u64;
                luma += yv;
                mad += (yv as i32 - prev[i] as i32).unsigned_abs() as u64;
                if i > 0 {
                    tv += (yv as i32 - y[i - 1] as i32).unsigned_abs() as u64;
                }
            }
            (mad, tv, luma)
        };
        Ok(stats)
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
    /// Pre-computed bicubic weights for Y-plane scaling (None if no scaling needed)
    h_weights: Option<BicubicWeights>,
    v_weights: Option<BicubicWeights>,
}

impl FallbackCapture {
    fn new(conn: Arc<CaptureState>, width: u16, height: u16, out_w: u32, out_h: u32) -> Self {
        let needs_scaling = out_w != width as u32 || out_h != height as u32;
        let (h_weights, v_weights) = if needs_scaling {
            (
                Some(build_bicubic_weights(width as usize, out_w as usize)),
                Some(build_bicubic_weights(height as usize, out_h as usize)),
            )
        } else {
            (None, None)
        };

        FallbackCapture {
            conn,
            width,
            height,
            h_weights,
            v_weights,
        }
    }

    /// Returns (mad_sum, tv_sum, luma_sum) for the Y plane.
    fn capture_to_i420(
        &self,
        i420_out: &mut Vec<u8>,
        out_w: u32,
        out_h: u32,
        needs_scaling: bool,
        blur_copy: &mut [u8],
        prev: &[u8],
    ) -> Result<(u64, u64, u64)> {
        let cookie = xproto::get_image(
            &self.conn.conn,
            xproto::ImageFormat::Z_PIXMAP,
            self.conn.root,
            0,
            0,
            self.width,
            self.height,
            !0, // plane_mask = all planes
        )
        .context("get_image failed")?;
        let reply = cookie.reply().context("get_image reply failed")?;

        let stats = if needs_scaling {
            let mut temp_buf = Vec::new();
            scale_bgra_direct(
                &reply.data,
                self.width as u32,
                self.height as u32,
                out_w,
                out_h,
                i420_out,
                &mut temp_buf,
                self.h_weights.as_ref().unwrap(),
                self.v_weights.as_ref().unwrap(),
                blur_copy,
                prev,
            )
        } else {
            bgra_to_i420(&reply.data, self.width as u32, self.height as u32, i420_out);
            let y_size = (self.width as usize) * (self.height as usize);
            let y = &i420_out[..y_size];
            blur_copy.copy_from_slice(y);
            let (mut mad, mut tv, mut luma) = (0u64, 0u64, 0u64);
            for i in 0..y_size {
                let yv = y[i] as u64;
                luma += yv;
                mad += (yv as i32 - prev[i] as i32).unsigned_abs() as u64;
                if i > 0 {
                    tv += (yv as i32 - y[i - 1] as i32).unsigned_abs() as u64;
                }
            }
            (mad, tv, luma)
        };
        Ok(stats)
    }
}

/// Unified interface for both SHM-accelerated and fallback capture.
enum ScreenCapture {
    Shm(ShmScreenCapture),
    Fallback(FallbackCapture),
}

impl ScreenCapture {
    /// Returns (mad_sum, tv_sum, luma_sum) for the Y plane.
    fn capture_to_i420(
        &self,
        i420_out: &mut Vec<u8>,
        out_w: u32,
        out_h: u32,
        needs_scaling: bool,
        blur_copy: &mut [u8],
        prev: &[u8],
    ) -> Result<(u64, u64, u64)> {
        match self {
            ScreenCapture::Shm(s) => {
                s.capture_to_i420(i420_out, out_w, out_h, needs_scaling, blur_copy, prev)
            }
            ScreenCapture::Fallback(f) => {
                f.capture_to_i420(i420_out, out_w, out_h, needs_scaling, blur_copy, prev)
            }
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
        let src = ArgbImage {
            data: bgra,
            stride: (width * 4) as usize,
        };
        let (y_plane, rest) = buf.split_at_mut(y_size);
        let (u_plane, v_plane) = rest.split_at_mut(uv_size);
        let mut dst = I420ImageMut {
            y: y_plane,
            y_stride: width as usize,
            u: u_plane,
            u_stride: (width / 2) as usize,
            v: v_plane,
            v_stride: (width / 2) as usize,
        };
        let size = ImageSize::new(width as usize, height as usize);
        let _ = vnrit_libyuv::argb_to_i420(&src, &mut dst, size);
    });
}

/// Bicubic (Catmull-Rom) weight table for one dimension.
/// w is flat: per output pixel, 4 consecutive Q12 weights for taps [t-1, t, t+1, t+2].
/// Layout: w[pixel * 4 + tap] for pixel in 0..dst_len, tap in 0..4.
/// w32 is the same weights pre-converted to i32 — eliminates per-row as i32
/// conversion in vertical SIMD splat (memory cost: ~11 KB extra for 720p).
struct BicubicWeights {
    base: Vec<i32>,
    w: Vec<i16>,
    w32: Vec<i32>,
}
// BicubicWeights is Send-safe: contains only primitive types and Vecs of primitive types
unsafe impl Send for BicubicWeights {}
/// Build Catmull-Rom weights: src_len → dst_len, Q12 fixed-point.
/// Mitchell-Netravali (B=1/3, C=1/3) bicubic weight table for one dimension.
/// Standard "high-quality" cubic — sharper than Bilinear, less ringing than Catmull-Rom.
/// base values are pre-clamped to [0, max(0, src_len - 4)] so that scaling functions
/// can skip the runtime boundary check.
fn build_bicubic_weights(src_len: usize, dst_len: usize) -> BicubicWeights {
    let step = (src_len as f64) / (dst_len as f64);
    let max_base = (src_len as i32).saturating_sub(4).max(0);
    let mut base = Vec::with_capacity(dst_len);
    let mut w = Vec::with_capacity(dst_len * 4);
    let mut w32 = Vec::with_capacity(dst_len * 4);
    for ox in 0..dst_len {
        let center = (ox as f64 + 0.5) * step - 0.5;
        let ix = center.floor() as i32;
        let frac = center - ix as f64;
        let base_ix = (ix - 1).max(0).min(max_base);
        for j in 0..4 {
            let t = (j as f64 - 1.0) - frac;
            let ta = t.abs();
            // Mitchell-Netravali: B=1/3, C=1/3
            let val = if ta >= 2.0 {
                0.0
            } else if ta >= 1.0 {
                let t2 = ta;
                // (-7/18)|t|³ + 2|t|² - (10/3)|t| + 16/9
                (-7.0 / 18.0) * t2 * t2 * t2 + 2.0 * t2 * t2 - (10.0 / 3.0) * t2 + 16.0 / 9.0
            } else {
                // (7/6)|t|³ - 2|t|² + 8/9
                (7.0 / 6.0) * ta * ta * ta - 2.0 * ta * ta + 8.0 / 9.0
            };
            let q12 = (val * 4096.0).round() as i16;
            w.push(q12);
            w32.push(q12 as i32);
        }
        base.push(base_ix);
    }
    BicubicWeights { base, w, w32 }
}
// Tile-based Catmull-Rom bicubic Y-plane downscaler with pre-computed weights.
//
// Processes the image in 256×256-pixel tiles.  Each tile performs horizontal
// then vertical scaling within its own local buffer, keeping intermediate data
// in L1/L2 cache.  Source rows are read directly (no tile_src copy).
// Q12 fixed-point, edge clamping, no new dependencies.
// Thread-local tile_horiz buffer — reused across tiles within each rayon
// worker to avoid per-tile allocation+zero-init.  Uses UnsafeCell (no runtime
// borrow-check overhead) — safe because each thread owns its instance and
// processes one tile at a time within the for_each closure.
thread_local! {
    static TILE_HORIZ: std::cell::UnsafeCell<Vec<u8>> =
        const { std::cell::UnsafeCell::new(Vec::new()) };
}
/// Returns (mad_sum, tv_sum, luma_sum) for the scaled Y plane:
///   - mad_sum  = Σ|final_y[i] - prev[i]|  (motion)
///   - tv_sum   = Σ|final_y[i] - final_y[i-1]|  (texture)
///   - luma_sum = Σ final_y[i]  (global luminance)
/// Also bilinearly scales U/V planes (fused to avoid separate libyuv passes).
fn bicubic_scale_y_with_weights(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst: &mut [u8],
    dst_w: usize,
    dst_h: usize,
    hw: &BicubicWeights,
    vw: &BicubicWeights,
    blur_copy: &mut [u8],
    prev: &[u8],
) -> (u64, u64, u64) {
    if src_w == dst_w && src_h == dst_h {
        if dst.len() >= src.len() {
            dst[..src.len()].copy_from_slice(src);
        }
        if blur_copy.len() >= src.len() {
            blur_copy[..src.len()].copy_from_slice(src);
        }
        // Compute MAD/TV/luma from direct copy (no scaling needed)
        let (mut mad, mut tv, mut luma) = (0u64, 0u64, 0u64);
        for i in 0..src_h * src_w {
            let y = src[i] as u64;
            luma += y;
            mad += (y as i32 - prev[i] as i32).unsigned_abs() as u64;
            if i > 0 {
                tv += (y as i32 - src[i - 1] as i32).unsigned_abs() as u64;
            }
        }
        return (mad, tv, luma);
    }
    const TILE_W: usize = 256;
    const TILE_H: usize = 128;
    let tiles_x = dst_w.div_ceil(TILE_W);
    let tiles_y = dst_h.div_ceil(TILE_H);
    // Use raw pointer address (usize is Sync) so the Fn closure (rayon for_each)
    // can safely partition dst into non-overlapping tile regions.
    let dst_addr = dst.as_mut_ptr() as usize;
    let dst_stride = dst_w;
    let bc_addr = blur_copy.as_mut_ptr() as usize;
    // Parallelize over ALL tiles using fold+reduce (per-thread accumulation,
    // no atomic contention).
    let total_tiles = tiles_x * tiles_y;
    let (mad, tv, luma) = (0..total_tiles)
        .into_par_iter()
        .fold(
            || (0u64, 0u64, 0u64),
            |(mut mad, mut tv, mut luma), tile_id| {
                let tx = tile_id % tiles_x;
                let ty = tile_id / tiles_x;
                let out_x0 = tx * TILE_W;
                let out_y0 = ty * TILE_H;
                let out_x1 = (out_x0 + TILE_W).min(dst_w);
                let out_y1 = (out_y0 + TILE_H).min(dst_h);
                let tile_w = out_x1 - out_x0;
                // Source row range for this tile
                let src_y0 = vw.base[out_y0] as usize;
                let src_y1 = (vw.base[out_y1 - 1] + 4) as usize;
                let tile_src_h = src_y1 - src_y0;
                // ── Horizontal pass: read directly from src ──
                // Reuse thread-local tile_horiz buffer (avoid per-tile alloc+zero-init).
                // SAFETY: TILE_HORIZ is per-thread; within a rayon for_each closure
                // each thread processes one tile at a time, so there is no concurrent
                // mutable access to the same UnsafeCell.
                let needed = tile_w * tile_src_h;
                let tile_horiz = unsafe {
                    let v = &mut *TILE_HORIZ.with(|b| b.get());
                    if v.capacity() < needed {
                        v.reserve(needed - v.capacity());
                    }
                    // Every byte is written by the horizontal pass below.
                    v.set_len(needed);
                    std::slice::from_raw_parts_mut(v.as_mut_ptr(), needed)
                };
                for ry in 0..tile_src_h {
                    let sr = &src[(src_y0 + ry) * src_w..];
                    let dr = &mut tile_horiz[ry * tile_w..];
                    // ── Horizontal pass: SIMD i32x4 (4 pixels/batch) ──
                    // Each pixel uses its own base index and 4 tap weights.
                    // Weights are in hw.w at stride-4 offsets: pixel j uses w[wo+j*4..wo+j*4+4].
                    let mut ox_local = 0usize;
                    while ox_local + 4 <= tile_w {
                        let ox_global = out_x0 + ox_local;
                        let wo = ox_global * 4;
                        let b0 = hw.base[ox_global] as usize;
                        let b1 = hw.base[ox_global + 1] as usize;
                        let b2 = hw.base[ox_global + 2] as usize;
                        let b3 = hw.base[ox_global + 3] as usize;
                        // Tap 0: w[wo..wo+12:4], sr[base_j + 0]
                        let s0 = i32x4::new([
                            sr[b0] as i32,
                            sr[b1] as i32,
                            sr[b2] as i32,
                            sr[b3] as i32,
                        ]);
                        let w0 = i32x4::new([
                            hw.w[wo] as i32,
                            hw.w[wo + 4] as i32,
                            hw.w[wo + 8] as i32,
                            hw.w[wo + 12] as i32,
                        ]);
                        let mut acc = s0 * w0;
                        // Tap 1: w[wo+1..wo+13:4], sr[base_j + 1]
                        let s1 = i32x4::new([
                            sr[b0 + 1] as i32,
                            sr[b1 + 1] as i32,
                            sr[b2 + 1] as i32,
                            sr[b3 + 1] as i32,
                        ]);
                        let w1 = i32x4::new([
                            hw.w[wo + 1] as i32,
                            hw.w[wo + 5] as i32,
                            hw.w[wo + 9] as i32,
                            hw.w[wo + 13] as i32,
                        ]);
                        acc = acc + s1 * w1;
                        // Tap 2: w[wo+2..wo+14:4], sr[base_j + 2]
                        let s2 = i32x4::new([
                            sr[b0 + 2] as i32,
                            sr[b1 + 2] as i32,
                            sr[b2 + 2] as i32,
                            sr[b3 + 2] as i32,
                        ]);
                        let w2 = i32x4::new([
                            hw.w[wo + 2] as i32,
                            hw.w[wo + 6] as i32,
                            hw.w[wo + 10] as i32,
                            hw.w[wo + 14] as i32,
                        ]);
                        acc = acc + s2 * w2;
                        // Tap 3: w[wo+3..wo+15:4], sr[base_j + 3]
                        let s3 = i32x4::new([
                            sr[b0 + 3] as i32,
                            sr[b1 + 3] as i32,
                            sr[b2 + 3] as i32,
                            sr[b3 + 3] as i32,
                        ]);
                        let w3 = i32x4::new([
                            hw.w[wo + 3] as i32,
                            hw.w[wo + 7] as i32,
                            hw.w[wo + 11] as i32,
                            hw.w[wo + 15] as i32,
                        ]);
                        acc = acc + s3 * w3 + i32x4::splat(2048);
                        let [r0, r1, r2, r3] = (acc >> 12_i32).to_array();
                        dr[ox_local] = r0.clamp(0, 255) as u8;
                        dr[ox_local + 1] = r1.clamp(0, 255) as u8;
                        dr[ox_local + 2] = r2.clamp(0, 255) as u8;
                        dr[ox_local + 3] = r3.clamp(0, 255) as u8;
                        ox_local += 4;
                    }
                    for ox_local in ox_local..tile_w {
                        let ox_global = out_x0 + ox_local;
                        let b = hw.base[ox_global] as usize;
                        let wo = ox_global * 4;
                        let s = (sr[b] as i32 * hw.w[wo] as i32
                            + sr[b + 1] as i32 * hw.w[wo + 1] as i32
                            + sr[b + 2] as i32 * hw.w[wo + 2] as i32
                            + sr[b + 3] as i32 * hw.w[wo + 3] as i32
                            + 2048)
                            >> 12;
                        dr[ox_local] = s.clamp(0, 255) as u8;
                    }
                }
                // ── Vertical pass: tile_horiz → dst (SIMD 4-col batches) ──
                // Also writes to blur_copy and accumulates MAD/TV/luma for mo­tion
                // adaptation (eliminates the separate MAD/TV/Copy loop).
                for oy in out_y0..out_y1 {
                    let b = (vw.base[oy] as usize) - src_y0;
                    let wo = oy * 4;
                    let (w0, w1, w2, w3) =
                        (vw.w32[wo], vw.w32[wo + 1], vw.w32[wo + 2], vw.w32[wo + 3]);
                    let wv0 = i32x4::splat(w0);
                    let wv1 = i32x4::splat(w1);
                    let wv2 = i32x4::splat(w2);
                    let wv3 = i32x4::splat(w3);
                    // SAFETY: Each tile writes to a unique (oy, ox) region.
                    let dst_base = dst_addr as *mut u8;
                    let dst_row = unsafe {
                        std::slice::from_raw_parts_mut(
                            dst_base.add(oy * dst_stride + out_x0),
                            tile_w,
                        )
                    };
                    let mut ox = 0usize;
                    while ox + 4 <= tile_w {
                        let s0 = i32x4::new([
                            tile_horiz[b * tile_w + ox] as i32,
                            tile_horiz[b * tile_w + ox + 1] as i32,
                            tile_horiz[b * tile_w + ox + 2] as i32,
                            tile_horiz[b * tile_w + ox + 3] as i32,
                        ]);
                        let s1 = i32x4::new([
                            tile_horiz[(b + 1) * tile_w + ox] as i32,
                            tile_horiz[(b + 1) * tile_w + ox + 1] as i32,
                            tile_horiz[(b + 1) * tile_w + ox + 2] as i32,
                            tile_horiz[(b + 1) * tile_w + ox + 3] as i32,
                        ]);
                        let s2 = i32x4::new([
                            tile_horiz[(b + 2) * tile_w + ox] as i32,
                            tile_horiz[(b + 2) * tile_w + ox + 1] as i32,
                            tile_horiz[(b + 2) * tile_w + ox + 2] as i32,
                            tile_horiz[(b + 2) * tile_w + ox + 3] as i32,
                        ]);
                        let s3 = i32x4::new([
                            tile_horiz[(b + 3) * tile_w + ox] as i32,
                            tile_horiz[(b + 3) * tile_w + ox + 1] as i32,
                            tile_horiz[(b + 3) * tile_w + ox + 2] as i32,
                            tile_horiz[(b + 3) * tile_w + ox + 3] as i32,
                        ]);
                        let acc = s0 * wv0 + s1 * wv1 + s2 * wv2 + s3 * wv3 + i32x4::splat(2048);
                        let [r0, r1, r2, r3] = (acc >> 12_i32).to_array();
                        dst_row[ox] = r0.clamp(0, 255) as u8;
                        dst_row[ox + 1] = r1.clamp(0, 255) as u8;
                        dst_row[ox + 2] = r2.clamp(0, 255) as u8;
                        dst_row[ox + 3] = r3.clamp(0, 255) as u8;
                        ox += 4;
                    }
                    for ox in ox..tile_w {
                        let s = (tile_horiz[b * tile_w + ox] as i32 * w0
                            + tile_horiz[(b + 1) * tile_w + ox] as i32 * w1
                            + tile_horiz[(b + 2) * tile_w + ox] as i32 * w2
                            + tile_horiz[(b + 3) * tile_w + ox] as i32 * w3
                            + 2048)
                            >> 12;
                        dst_row[ox] = s.clamp(0, 255) as u8;
                    }
                    // ── blur_copy + MAD/TV/luma (dst_row is hot in L1) ──
                    let bc_base = bc_addr as *mut u8;
                    let bc_row = unsafe {
                        std::slice::from_raw_parts_mut(
                            bc_base.add(oy * dst_stride + out_x0),
                            tile_w,
                        )
                    };
                    let prev_row = &prev[oy * dst_w + out_x0..][..tile_w];
                    let mut row_prev = dst_row[0];
                    for ox in 0..tile_w {
                        let y = dst_row[ox] as u32;
                        bc_row[ox] = y as u8;
                        mad += (y as i32 - prev_row[ox] as i32).unsigned_abs() as u64;
                        luma += y as u64;
                        // TV: first pixel of each tile misses the neighbor to its left
                        // from the previous tile (processed by another thread).  Error:
                        // ~0.3% for 720p — negligible for contrast adaptation.
                        if ox > 0 {
                            tv += (y as i32 - row_prev as i32).unsigned_abs() as u64;
                        }
                        row_prev = y as u8;
                    }
                }
                (mad, tv, luma)
            },
        )
        .reduce(
            || (0u64, 0u64, 0u64),
            |(m1, t1, l1), (m2, t2, l2)| (m1 + m2, t1 + t2, l1 + l2),
        );
    (mad, tv, luma)
}

/// Scale BGRA → I420 with bicubic Y + bilinear UV.
///  1. BGRA → native I420  (temp)
///  2. Y: bicubic (Catmull-Rom, 4-tap, Q12, tile-based for cache locality),
///     UV: bilinear (libyuv)
/// temp holds the native-resolution I420 frame (reused across frames).
/// blur_copy and prev are sections of tmp_argb for MAD fusion.
/// Returns (mad_sum, tv_sum, luma_sum).
fn scale_bgra_direct(
    bgra: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    i420_out: &mut Vec<u8>,
    temp: &mut Vec<u8>,
    h_weights: &BicubicWeights,
    v_weights: &BicubicWeights,
    blur_copy: &mut [u8],
    prev: &[u8],
) -> (u64, u64, u64) {
    // Step 1: BGRA → I420 at native resolution
    let src_y_size = (src_w * src_h) as usize;
    let src_uv_size = ((src_w / 2) * (src_h / 2)) as usize;
    let native_i420_size = src_y_size + 2 * src_uv_size;
    with_resize_uninit(temp, native_i420_size, |t| {
        bgra_to_i420_into(bgra, src_w, src_h, t);
    });

    // Step 2: Scale Y plane with bicubic, U+V with bilinear
    let dst_y_size = (dst_w * dst_h) as usize;
    let dst_uv_size = ((dst_w / 2) * (dst_h / 2)) as usize;
    // Resize i420_out inline (no closure — need to return stats)
    let total = dst_y_size + 2 * dst_uv_size;
    i420_out.clear();
    i420_out.reserve(total);
    unsafe {
        i420_out.set_len(total);
    }
    let out = &mut i420_out[..total];
    let (src_y, src_rest) = temp.split_at(src_y_size);
    let (src_u, src_v) = src_rest.split_at(src_uv_size);
    let (dst_y, dst_rest) = out.split_at_mut(dst_y_size);
    let (dst_u, dst_v) = dst_rest.split_at_mut(dst_uv_size);

    // Tile-based bicubic Y (fused MAD/TV/luma)
    let (mad, tv, luma) = bicubic_scale_y_with_weights(
        src_y,
        src_w as usize,
        src_h as usize,
        dst_y,
        dst_w as usize,
        dst_h as usize,
        h_weights,
        v_weights,
        blur_copy,
        prev,
    );

    // Bilinear UV via SIMD-accelerated libyuv (separate pass but fast)
    let uv_src_w = (src_w / 2) as usize;
    let uv_src_h = (src_h / 2) as usize;
    let uv_dst_w = (dst_w / 2) as usize;
    let uv_dst_h = (dst_h / 2) as usize;
    let uv_src_sz = ImageSize::new(uv_src_w, uv_src_h);
    let uv_dst_sz = ImageSize::new(uv_dst_w, uv_dst_h);
    let _ = vnrit_libyuv::scale_plane(
        src_u,
        uv_src_w,
        uv_src_sz,
        dst_u,
        uv_dst_w,
        uv_dst_sz,
        FilterMode::Bilinear,
    );
    let _ = vnrit_libyuv::scale_plane(
        src_v,
        uv_src_w,
        uv_src_sz,
        dst_v,
        uv_dst_w,
        uv_dst_sz,
        FilterMode::Bilinear,
    );
    (mad, tv, luma)
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

/// Texture-adaptation: prev_abs_diff × abs_diff → Q8 multiplier [192 or 256].
/// Returns 192 (0.75×) when adjacent |diff| values are similar and
/// both non-trivial — reduces gain in texture regions.
/// Branchless: uses only min/max/compare/shift — no `if`.
#[inline(always)]
fn texture_weight(prev: u32, curr: u32) -> u32 {
    let mx = prev.max(curr);
    let mn = prev.min(curr);
    // condition = mx >= 8 && mn >= mx/2
    let c = ((mx >= 8) as u32) & ((mn >= (mx >> 1)) as u32);
    // c==1 → 192, c==0 → 256
    256 - c * 64
}

/// Gradient-aware edge refine weight: (grad, diff) → weight [0, 192].
/// Branchless priority encoder: arithmetic select without `if`.
/// Values are ordered by descending priority:
///   g≥4 & d≥4 → 192 (extremely strong)
///   g≥2 & d≥2 → 128 (strong)
///   g≥1 & d≥1 → 64  (medium)
///   else      → 0   (flat)
#[inline(always)]
fn edge_refine_weight(grad: u32, diff: u32) -> u32 {
    let g = grad >> 3;
    let d = diff >> 3;
    let c3 = ((g >= 4) as u32) & ((d >= 4) as u32);
    let c2 = ((g >= 2) as u32) & ((d >= 2) as u32);
    let c1 = ((g >= 1) as u32) & ((d >= 1) as u32);
    // Priority: c3 > c2 > c1.  Arithmetic select avoids branches.
    c3 * 192 + c2 * (1 - c3) * 128 + c1 * (1 - c3) * (1 - c2) * 64
}

/// Merged per-pixel enhancement tables — co-located in memory for cache efficiency.
/// All tables are 256-entry indexed by u8 pixel values.
#[repr(C)]
struct EnhanceTables {
    /// |Y-blur| → blend weight [0, 128]. Flat areas smoothed toward blur.
    denoise: [u8; 256],
    /// edge_refine_weight → contrast boost [0, 32] (Q5).
    edge_boost: [u8; 256],
    /// |diff| → non-linear gain (Q8). Feature Self-Transform quadratic mapping.
    nonlinear: [u16; 256],
    /// Y → Q8 amount multiplier [128, 512]. Midtones peak at 2.0×.
    midtone: [u16; 256],
    /// Y_in → Y_out S-curve (fallback kept for validation; branchless path preferred).
    tone_curve: [u8; 256],
}

/// Branchless S-curve tone mapping: arithmetic computation, no table lookup.
/// Lifts shadows up to +6, compresses highlights down to -4, midtones pass through.
/// Same output as `build_tone_curve()` but executable in any context (no table needed).
#[inline(always)]
fn tone_curve_branchless(v: u8) -> u8 {
    let v = v as u32;
    let lift = (64u32.saturating_sub(v)) * 6 / 64; // +6 at v=0, 0 at v>=64
    let comp = v.saturating_sub(191) * 4 / 64; // 0 at v<=191, -4 at v=255
    (v + lift - comp) as u8
}

/// Edge contrast boost table: edge_refine_weight → boost amount [0, 32] (Q5).
///
/// After edge-preserving refinement, edges in blur_copy may be slightly
/// softened.  This table adds back a tiny contrast boost proportional to
/// the refine weight, restoring crispness without reintroducing halos.
const fn build_edge_boost_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0usize;
    while i < 256 {
        // Peak boost at moderate weight (real edge, after pullback)
        // Zero at weight=0 (flat) and weight=192 (full preserve, no need)
        t[i] = if i < 32 {
            0
        } else if i < 128 {
            ((i as u32 - 32) * 32 / 96) as u8
        }
        // 0 → 32
        else if i < 192 {
            (32 - (i as u32 - 128) * 32 / 64) as u8
        }
        // 32 → 0
        else {
            0
        };
        i += 1;
    }
    t
}

/// Spatial denoise weight table: |Y-blur| → blend weight [0, 128].
///
/// In flat areas (|diff| small), Y is blended toward the blur to suppress noise.
/// At edges (|diff| large), Y is preserved to keep detail.
/// Applied BEFORE edge-preserving refinement so the cleaner Y produces
/// a better base layer.
/// NOTE: Denoise is deliberately conservative — only truly flat pixels
/// (|diff| = 0) get smoothed.  Anything with even subtle texture is
/// preserved so the non-linear gain stage can enhance it naturally.
/// Over-denoising causes "plasticky" haze that kills fine detail.
const fn build_denoise_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0usize;
    while i < 256 {
        t[i] = if i < 1 { 128 }        // |diff| = 0: 50% blend, remove single-pixel noise
            else { 0 }; // |diff| ≥ 1: preserve all texture
        i += 1;
    }
    t
}

/// Non-linear gain table replacing the linear local_q8.
///
/// Inspired by MobileIE's Feature Self-Transform (quadratic mapping):
///   |diff| 0:     gain = 0     (dead zone — only exact flat pixels)
///   |diff| 1-2:   gain 0.25→0.5×  (subtle texture, gentle boost)
///   |diff| 3-5:   gain 0.5→1.0×   (fine detail, catch-up ramp)
///   |diff| 6-10:  gain 1.0→2.0×   (mid-detail boost — peak at |diff|=10)
///   |diff| 11-32: gain 2.0→1.0×   (roll-off, prevent halos)
///   |diff| 33+:   gain 1.0→0.5×   (tail, strong edge conservatism)
const fn build_nonlinear_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0usize;
    while i < 256 {
        t[i] = if i == 0 {
            0
        } else if i < 3 {
            (32 + (i - 1) as u32 * 32) as u16
        }
        // [32, 64]
        else if i < 6 {
            (128 + (i - 3) as u32 * 128 / 3) as u16
        }
        // [128, 256]
        else if i < 11 {
            (256 + (i - 6) as u32 * 256 / 5) as u16
        }
        // [256, 512]
        else if i < 33 {
            (512 - (i - 11) as u32 * 256 / 22) as u16
        }
        // [512, 256]
        else {
            let v = 256 - (i - 33) as u32 * 128 / 223;
            (if v < 128 { 128 } else { v }) as u16
        };
        i += 1;
    }
    t
}

/// Global luminance boost table: avg_y → Q8 multiplier [128, 448].
///
/// Inspired by MobileIE's channel attention (avg pool → sigmoid → scale):
///   avg Y < 32  (very dark):    boost 1.75×  — enhance shadow detail
///   avg Y 32-64 (dark):         boost 1.25×
///   avg Y 64-160 (normal):      boost 1.0×   — neutral
///   avg Y 160-192 (bright):     boost 0.85×  — prevent blowing
///   avg Y > 192 (very bright):  boost 0.5×   — minimal enhancement
const fn build_luma_boost_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0usize;
    while i < 256 {
        t[i] = if i < 32 {
            (256 + (32 - i as u32) * 192 / 32) as u16
        }
        // [448, 256]
        else if i < 64 {
            (256 + (64 - i as u32) * 192 / 32) as u16
        }
        // [256, 448]
        else if i < 160 {
            256u16
        }
        // 1.0×
        else if i < 192 {
            (256 - (i - 160) as u32 * 128 / 32) as u16
        }
        // [256, 128]
        else {
            let v = 128 - (i - 192) as u32 * 128 / 64;
            (if v > 128 { v } else { 128 }) as u16 // clamp(128)
        };
        i += 1;
    }
    t
}
const LUMA_BOOST_TABLE: [u16; 256] = build_luma_boost_table();

/// Midtone clarity boost table: Y → Q8 amount multiplier [128, 512].
///
/// Applies a visibility-weighted gain to the USM amount so that midtones
/// (where human vision is most sensitive) receive more enhancement while
/// shadows and highlights are handled more gently.
///   Y 0   (shadows):     128 (0.5×)
///   Y 96  (midtone edge):256 (1.0×)
///   Y 128 (midtones):    512 (2.0×)  ← peak
///   Y 160 (bright):      256 (1.0×)
///   Y 255 (highlights):  128 (0.5×)
const fn build_midtone_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0usize;
    while i < 256 {
        t[i] = if i < 96 {
            (128 + i as u32 * 128 / 96) as u16
        } else if i < 128 {
            (256 + (i - 96) as u32 * 256 / 32) as u16
        } else if i < 160 {
            (512 - (i - 128) as u32 * 256 / 32) as u16
        } else {
            (256 - (i - 160) as u32 * 128 / 95) as u16
        };
        i += 1;
    }
    t
}

/// Photographic S-curve tone mapping table: Y_in → Y_out [0, 255].
///
/// Subtly lifts shadows (up to +6 at Y=0) and compresses highlights
/// (up to -4 at Y=255) for a film-like tonal response. Midtones pass
/// through unchanged.  Applied after USM enhancement.
const fn build_tone_curve() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0usize;
    while i < 256 {
        t[i] = if i < 64 {
            // Shadow lift: +6 at Y=0, tapering to 0 at Y=64
            let lift = ((64 - i) as u32 * 6 / 64) as u8;
            (i as u8).saturating_add(lift)
        } else if i > 191 {
            // Highlight compression: -4 at Y=255, tapering to 0 at Y=191
            let comp = ((i - 191) as u32 * 4 / 64) as u8;
            (i as u8).saturating_sub(comp)
        } else {
            i as u8
        };
        i += 1;
    }
    t
}

const ENHANCE_TABLES: EnhanceTables = EnhanceTables {
    denoise: build_denoise_table(),
    edge_boost: build_edge_boost_table(),
    nonlinear: build_nonlinear_table(),
    midtone: build_midtone_table(),
    tone_curve: build_tone_curve(),
};

// ── SIMD enhancement helpers (i16x8, 8 pixels per batch) ──────────────

/// SIMD priority encoder: (grad, raw_diff) → weight {0, 64, 128, 192}.
#[inline(always)]
fn edge_refine_weight_simd(grad: i16x8, raw_diff: i16x8) -> i16x8 {
    let g = grad >> 3_i32;
    let d = raw_diff >> 3_i32;
    let s4 = i16x8::splat(4);
    let s2 = i16x8::splat(2);
    let s1 = i16x8::splat(1);
    let c3 = g.simd_ge(s4) & d.simd_ge(s4);
    let c2 = g.simd_ge(s2) & d.simd_ge(s2);
    let c1 = g.simd_ge(s1) & d.simd_ge(s1);
    let nc3 = !c3;
    let nc2 = !c2;
    (c3 & i16x8::splat(192)) | (nc3 & c2 & i16x8::splat(128)) | (nc3 & nc2 & c1 & i16x8::splat(64))
}

/// SIMD texture adaptation: (prev_abs_diff, abs_diff) → Q8 multiplier {192, 256}.
#[inline(always)]
fn texture_weight_simd(prev: i16x8, curr: i16x8) -> i16x8 {
    let mx = prev.max(curr);
    let mn = prev.min(curr);
    let c = mx.simd_ge(i16x8::splat(8)) & mn.simd_ge(mx >> 1_i32);
    i16x8::splat(256) - (c & i16x8::splat(64))
}

/// SIMD edge boost: weight {0,64,128,192} → boost {0,10,32,0}.
#[inline(always)]
fn edge_boost_simd(w: i16x8) -> i16x8 {
    let c64 = w.simd_eq(i16x8::splat(64));
    let c128 = w.simd_eq(i16x8::splat(128));
    (c64 & i16x8::splat(10)) | (c128 & i16x8::splat(32))
}

/// SIMD non-linear gain: |diff| → Q8 gain [0, 512].
/// 5 segments matched to `build_nonlinear_table()`.
/// SAFETY: All multiplications verified safe in i16 (max 222×128=28416 < 32767).
#[inline(always)]
fn nonlinear_gain_simd(d: i16x8) -> i16x8 {
    let z = i16x8::splat(0);
    // Segment 2: d ∈ [1, 2] → gain = 32×d
    let s2_pred = d.simd_ge(i16x8::splat(1)) & d.simd_le(i16x8::splat(2));
    let s2_val = d * i16x8::splat(32);
    // Segment 3: d ∈ [3, 5] → gain = 128 + (d-3)×43
    let s3_pred = d.simd_ge(i16x8::splat(3)) & d.simd_le(i16x8::splat(5));
    let s3_val = i16x8::splat(128) + (d - i16x8::splat(3)) * i16x8::splat(43);
    // Segment 4: d ∈ [6, 10] → gain = 256 + (d-6)×51, clamp 512
    let s4_pred = d.simd_ge(i16x8::splat(6)) & d.simd_le(i16x8::splat(10));
    let s4_val =
        (i16x8::splat(256) + (d - i16x8::splat(6)) * i16x8::splat(51)).min(i16x8::splat(512));
    // Segment 5: d ∈ [11, 32] → gain = 512 - (d-11)×12
    let s5_pred = d.simd_ge(i16x8::splat(11)) & d.simd_le(i16x8::splat(32));
    let s5_val = i16x8::splat(512) - (d - i16x8::splat(11)) * i16x8::splat(12);
    // Segment 6: d > 32 → gain = 256 - (d-33)×128/223, clamp 128
    let s6_pred = d.simd_gt(i16x8::splat(32));
    let s6_val = (i16x8::splat(256)
        - (d - i16x8::splat(33)) * i16x8::splat(128) / i16x8::splat(223))
    .max(i16x8::splat(128));
    // Blend: start with 0, layer each segment (non-overlapping by design)
    let m2 = s2_pred;
    let m3 = s3_pred;
    let m4 = s4_pred;
    let m5 = s5_pred;
    let m6 = s6_pred;
    (z & !m2) | (s2_val & m2) | (s3_val & m3) | (s4_val & m4) | (s5_val & m5) | (s6_val & m6)
}

/// SIMD midtone clarity boost: Y → Q8 multiplier [128, 512].
/// 4 segments matched to `build_midtone_table()`.
/// SAFETY: All multiplications verified safe in i16 (max 255×128=32640 < 32767).
#[inline(always)]
fn midtone_gain_simd(v: i16x8) -> i16x8 {
    // Segment 1: v < 96 → 128 + v×128/96
    let s1_pred = v.simd_lt(i16x8::splat(96));
    let s1_val = i16x8::splat(128) + v * i16x8::splat(128) / i16x8::splat(96);
    // Segment 2: v ∈ [96, 128) → 256 + (v-96)×8
    let s2_pred = v.simd_ge(i16x8::splat(96)) & v.simd_lt(i16x8::splat(128));
    let s2_val = i16x8::splat(256) + (v - i16x8::splat(96)) * i16x8::splat(8);
    // Segment 3: v ∈ [128, 160) → 512 - (v-128)×8
    let s3_pred = v.simd_ge(i16x8::splat(128)) & v.simd_lt(i16x8::splat(160));
    let s3_val = i16x8::splat(512) - (v - i16x8::splat(128)) * i16x8::splat(8);
    // Segment 4: v ≥ 160 → 256 - (v-160)×128/95
    let s4_pred = v.simd_ge(i16x8::splat(160));
    let s4_val = i16x8::splat(256) - (v - i16x8::splat(160)) * i16x8::splat(128) / i16x8::splat(95);
    (s1_val & s1_pred) | (s2_val & s2_pred) | (s3_val & s3_pred) | (s4_val & s4_pred)
}

/// SIMD tone curve: Y_in → Y_out with shadow lift + highlight compression.
/// Branchless arithmetic, no table needed.
#[inline(always)]
fn tone_curve_simd(v: i16x8) -> i16x8 {
    let lift = (i16x8::splat(64) - v).max(i16x8::splat(0)) * i16x8::splat(6) / i16x8::splat(64);
    let comp = (v - i16x8::splat(191)).max(i16x8::splat(0)) * i16x8::splat(4) / i16x8::splat(64);
    v + lift - comp
}

/// Apply unsharp mask to the Y (luminance) plane of an I420 frame.
///
/// Features:
///   - **Edge-preserving blur refinement** — guided-filter-inspired correction
///     pulls the box blur toward the original Y at strong edges, eliminating
///     the root cause of USM halos (blur bleeding across edges).
///   - **Motion adaptation** — MAD-based look-up table reduces strength
///     during motion (eliminates temporal flicker).
///   - **Non-linear gain curve** — FST-inspired quadratic mapping compresses
///     large |diff| values for natural-looking sharpening without halos.
///   - **Channel-attention-inspired global luma** — average luminance adjusts
///     overall enhancement strength (dark → boost, bright → reduce).
///   - **Chroma activity boost** — colorful frames get more enhancement.
///   - **Texture-adaptive** — 32×32 LUT reduces gain in texture regions
///     to avoid over-sharpening.
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
fn apply_enhancement(
    i420: &mut [u8],
    w: usize,
    h: usize,
    strength: f32,
    blur_buf: &mut Vec<u8>,
    chroma_tick: &mut u32,
    last_chroma_boost: &mut u32,
    mad_sum: u64,
    tv_sum: u64,
    luma_sum: u64,
) {
    if strength <= 0.0 || w < 3 || h < 3 {
        return;
    }
    let y_size = w * h;
    let total_req = 2 * y_size;

    let uv_size = (w / 2) * (h / 2);

    // (Re)size blur_buf to [blur_copy | prev] if needed (first frame/res change).
    // MAD/TV/luma are pre-computed by the tile scaling phase — no separate pass.
    if blur_buf.len() != total_req {
        blur_buf.clear();
        blur_buf.reserve(total_req);
        unsafe {
            blur_buf.set_len(total_req);
        }
        let y_plane = &i420[..y_size];
        // blur_copy section is already written by tile scaling if scaling was
        // needed; for a no-scaling transition or first frame, copy it here.
        // blur_buf[..y_size] may already be valid; overwrite to be safe.
        blur_buf[..y_size].copy_from_slice(y_plane);
        blur_buf[y_size..total_req].copy_from_slice(y_plane);
    }

    // ── 0.  Chroma activity ──
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

    // ── 1.  MAD/TV/luma are pre-computed by tile scaling ─────────
    // No separate MAD/TV/Copy pass — tile scaling already wrote blur_copy
    // and accumulated stats.  Compute derived values directly.
    let mad = (mad_sum / y_size as u64) as usize;
    let avg_tv = (tv_sum / (y_size.saturating_sub(1)) as u64) as u32;
    let avg_y = (luma_sum / y_size as u64) as usize;
    let motion_factor = MOTION_TABLE[mad.min(255)] as u32;

    // When motion_factor < 16 (< 6% strength), USM effect is imperceptible.
    // Skip stack_blur + per-pixel USM; just save Y as prev for next frame's MAD.
    if motion_factor < 16 {
        prev.copy_from_slice(y_plane);
        return;
    }

    // ── 2.  Stack blur (SIMD, O(1)) ────────────────────────────
    // Resolution-adaptive radius: 640px→2, 1280px→3, 1920px→4
    let blur_radius = ((w + 320) / 640).clamp(2, 4);
    {
        let mut img = BlurImageMut::borrow(blur_copy, w as u32, h as u32, FastBlurChannels::Plane);
        let radius = AnisotropicRadius::new(blur_radius as u32);
        let _ = libblur::stack_blur(&mut img, radius, ThreadingPolicy::Single);
    }

    // ── 2+3.  Denoise + edge refine + USM (merged single pass) ──
    // Originally three separate loops (denoise, refine, USM) that required
    // writing intermediate y2/boosted to y_plane/blur_copy and reading them
    // back.  Merging eliminates one full Y-plane write+read (≈ 4 MB/frame).
    //
    // Per-pixel dataflow within the merged loop:
    //   y (original) + b (stack-blurred)
    //     → denoise: y2 = blend(y, b, DENOISE_TABLE[diff])
    //     → refine: blended = blend(b, y2, edge_refine_weight)
    //               boosted = blended + boost(blended - y2)
    //     → USM: diff_usm = y2 - boosted → nonlinear_gain × motion × texture
    //            final_y = TONE_CURVE(y2 + adj)

    //  base_amount = Q8  (256 = 1.0x)
    let base_amount = (strength * 256.0).clamp(0.0, 1023.0) as u32;

    //  Contrast boost  (Total Variation per pixel → Q8 multiplier)
    let contrast_boost_q8 = (384u32).saturating_sub(avg_tv * 3).max(128).min(512);
    let effective_base = ((base_amount * contrast_boost_q8 + 128) >> 8).min(1023);

    //  Luma boost (global luminance → Q8 multiplier, channel attention inspired)
    let luma_boost_q8 = LUMA_BOOST_TABLE[avg_y.min(255)] as u32;
    let effective_base = ((effective_base * luma_boost_q8 + 128) >> 8).min(1023);

    //  Chroma boost (mean |C - 128| → Q8 multiplier)
    let effective_base = ((effective_base * chroma_boost_q8 + 128) >> 8).min(1023);

    let motion_factor_s = motion_factor as i16;
    let effective_base_s = effective_base as i16;
    let mut total_adj: u64 = 0;

    let mf_v = i16x8::splat(motion_factor_s);
    let eb_v = i16x8::splat(effective_base_s);

    // Function-level SIMD constants (created once per frame, not per row)
    let z = i16x8::splat(0);
    let o = i16x8::splat(1);
    let s16 = i16x8::splat(16);
    let s128 = i16x8::splat(128);
    let s255 = i16x8::splat(255);
    let s256 = i16x8::splat(256);
    let s1023 = i16x8::splat(1023);
    let s128_i32 = i32x8::splat(128);

    // ── SIMD: row-at-a-time, 8 pixels per i16x8 batch ──
    // Process each row independently so vgrad reads from the correct
    // previous-row y_plane and prev_abs_diff resets at row boundaries.
    for row in 0..h {
        let row_start = row * w;
        // carry_y2 for hgrad: first pixel uses last pixel of previous row
        let carry_in: i16 = if row == 0 {
            y_plane[0] as i16
        } else {
            // Previous row's last pixel (y_plane[row_start-1] = final_y of prev row)
            y_plane[row_start - 1] as i16
        };
        let mut carry_y2 = carry_in;
        let mut carry_abs_diff: i16 = 0; // resets at row start

        // ── SIMD: process 8-pixel chunks ──
        // ═══════════════════════════════════════════════════════════════════
        // i16 OVERFLOW SAFETY AUDIT (2026-07-09)
        // All i16 multiplications are categorized below.  The i16 range is
        // [-32768, 32767].  Any product exceeding this wraps silently in
        // release mode, causing black-pixel flickering in white areas.
        //
        // Protected by mul_widen → i32x8 (safe for any u8×u8 product):
        //   [1] y_v * (256 - w_d)    max 255×256 = 65280     — denoise blend
        //   [2] b_v * w_d            max 255×128 = 32640     — denoise blend
        //   [3] b_v * (256 - weight) max 255×256 = 65280     — edge blend
        //   [4] y2 * weight          max 255×192 = 48960     — edge blend
        //   [5] mf × texture_q8      max 255×256 = 65280     — combined_q8
        //   [6] eb × combined_q8     max 1023×255 = 260865   — amount (2×)
        //   [7] amount × midtone_q8  max 1023×512 = 523776   — amount update
        //   [8] diff × nonlinear     max 255×512 = 130560    — adj_a
        //   [9] adj_a × amount       max 511×1023 = 522753   — adj
        //
        // Verified safe in i16 (max product < 32767):
        //   [A] (edge_diff * boost)  max 255×32 = 8160       — boosted
        //   [B] nonlinear_gain_simd  max 222×128 = 28416     — piecewise
        //   [C] midtone_gain_simd    max 255×128 = 32640     — piecewise
        //   [D] tone_curve_simd       max 64×6 = 384         — S-curve
        //   [E] (o - raw_dc) * 128   max 1×128 = 128         — denoise weight
        //   [F] texture_weight       no multiplication       — cmp/select only
        //   [G] edge_refine_weight   no multiplication       — cmp/select only
        //   [H] edge_boost           no multiplication       — cmp/select only
        // ═══════════════════════════════════════════════════════════════════
        let simd_limit = row_start + (w & !7);
        let mut chunk = row_start;
        while chunk < simd_limit {
            // ── SIMD load + widen: u8 → i16x8 via u8x16 + from_u8x16_low ──
            // On ARM NEON: vld1q_u8 (from aligned stack) + uxtl = 2 insns, 8 pixels.
            // The u8x16::new(buf) is a compile-time transmute; the real work
            // is the 8-byte copy_from_slice which LLVM widens to a single ldr/str.
            let mut u8_buf = [0u8; 16];
            u8_buf[..8].copy_from_slice(&y_plane[chunk..chunk + 8]);
            let y_v = i16x8::from_u8x16_low(u8x16::new(u8_buf));

            u8_buf[..8].copy_from_slice(&blur_copy[chunk..chunk + 8]);
            let b_v = i16x8::from_u8x16_low(u8x16::new(u8_buf));

            // Above-row for vgrad (pre-compute to avoid duplication)
            let above_v = if row > 0 {
                u8_buf[..8].copy_from_slice(&y_plane[chunk - w..chunk - w + 8]);
                i16x8::from_u8x16_low(u8x16::new(u8_buf))
            } else {
                z // first row: vgrad = 0
            };

            // raw_diff = |y - b|
            let raw_diff = (y_v - b_v).abs(); // ── Denoise: blend Y→blur in flat areas ──
            let raw_dc = raw_diff.min(o);
            let w_d = (o - raw_dc) * s128;
            let y2 = i16x8::from_i32x8_saturate(
                (y_v.mul_widen(s256 - w_d) + b_v.mul_widen(w_d) + s128_i32) >> 8_i32,
            );

            // ── Edge refine ──
            let y2_arr = y2.to_array();
            // Shifted-lane trick: hgrad[j] = |y2_j - y2_{j-1}|
            let shifted_y2 = i16x8::new([
                carry_y2, y2_arr[0], y2_arr[1], y2_arr[2], y2_arr[3], y2_arr[4], y2_arr[5],
                y2_arr[6],
            ]);
            carry_y2 = y2_arr[7];
            let hgrad = (y2 - shifted_y2).abs();

            // vgrad: zero for first row, |y2 - above| for rest
            let vgrad = if row == 0 { z } else { (y2 - above_v).abs() };
            let grad = hgrad.max(vgrad);

            let weight = edge_refine_weight_simd(grad, raw_diff);

            // blended = blend(b, y2, weight) — safe via i32x8 widen
            let blended = i16x8::from_i32x8_saturate(
                (b_v.mul_widen(s256 - weight) + y2.mul_widen(weight) + s128_i32) >> 8_i32,
            );

            // Edge boost
            let boost = edge_boost_simd(weight);
            let edge_diff = y2 - blended;
            let boosted = (blended + ((edge_diff * boost + s16) >> 5_i32))
                .max(z)
                .min(s255);

            // ── USM ──
            let diff = y2 - boosted;
            let abs_diff = diff.abs();
            let nonlinear_gain = nonlinear_gain_simd(abs_diff);

            // Texture weight: shifted-lane for prev_abs_diff
            let ad_arr = abs_diff.to_array();
            let shifted_ad = i16x8::new([
                carry_abs_diff,
                ad_arr[0],
                ad_arr[1],
                ad_arr[2],
                ad_arr[3],
                ad_arr[4],
                ad_arr[5],
                ad_arr[6],
            ]);
            carry_abs_diff = ad_arr[7];
            let texture_q8 = texture_weight_simd(shifted_ad, abs_diff);

            // ── Overflow-safe multiplications via i32x8 widening ──
            // combined_q8 = (motion_factor × texture_q8 + 128) >> 8
            let combined_q8 =
                i16x8::from_i32x8_saturate((mf_v.mul_widen(texture_q8) + s128_i32) >> 8_i32);

            // amount = ((effective_base × combined_q8 + 128) >> 8).min(1023)
            let amount =
                i16x8::from_i32x8_saturate((eb_v.mul_widen(combined_q8) + s128_i32) >> 8_i32)
                    .min(s1023);

            // amount = ((amount × midtone_q8 + 128) >> 8).min(1023)
            let midtone_q8 = midtone_gain_simd(y2);
            let amount =
                i16x8::from_i32x8_saturate((amount.mul_widen(midtone_q8) + s128_i32) >> 8_i32)
                    .min(s1023);

            // adj_a = (diff × nonlinear_gain + 128) >> 8
            let adj_a =
                i16x8::from_i32x8_saturate((diff.mul_widen(nonlinear_gain) + s128_i32) >> 8_i32);

            // adj = (adj_a × amount + 128) >> 8
            let adj = i16x8::from_i32x8_saturate((adj_a.mul_widen(amount) + s128_i32) >> 8_i32);

            // Accumulate |adj| for chroma saturation boost
            let adj_arr = adj.to_array();
            for &a in &adj_arr {
                total_adj += a.unsigned_abs() as u64;
            }

            let enhanced = (y2 + adj).max(z).min(s255);

            // Tone curve with XOR select for grad >= 16 bypass
            let curve_val = tone_curve_simd(enhanced);
            let grad_mask = grad.simd_ge(i16x8::splat(16));
            // grad_mask: 0xFFFF for true, 0x0000 for false
            let final_y = enhanced ^ (grad_mask & (enhanced ^ curve_val));

            // Store 8 pixels
            let f_arr = final_y.to_array();
            for j in 0..8 {
                let fv = f_arr[j] as u8;
                y_plane[chunk + j] = fv;
                prev[chunk + j] = fv;
            }

            chunk += 8;
        }

        // ── Scalar tail: remaining < 8 pixels in this row ──
        for j in chunk..row_start + w {
            let i = j;
            let y = y_plane[i] as u32;
            let b = blur_copy[i] as u32;
            let raw_diff = (y as i32 - b as i32).unsigned_abs();

            // ── Denoise ──
            let w_d = ENHANCE_TABLES.denoise[raw_diff.min(255) as usize] as u32;
            let y2 = (y * (256 - w_d) + b * w_d + 128) >> 8;

            // ── Edge refine ──
            let hgrad = (y2 as i32 - carry_y2 as i32).unsigned_abs();
            let v_neighbor = if row > 0 { y_plane[i - w] as u32 } else { 0 };
            let vgrad = (row > 0) as u32 * (y2 as i32 - v_neighbor as i32).unsigned_abs();
            let grad = hgrad.max(vgrad);
            carry_y2 = y2 as i16;

            let weight = edge_refine_weight(grad, raw_diff);
            let blended = (b * (256 - weight) + y2 * weight + 128) >> 8;
            let boost = ENHANCE_TABLES.edge_boost[weight as usize] as u32;
            let edge_diff = (y2 as i32 - blended as i32) as i32;
            let boosted =
                (blended as i32 + ((edge_diff * boost as i32 + 16) >> 5)).clamp(0, 255) as u8;

            // ── USM ──
            let diff = y2 as i32 - boosted as i32;
            let abs_diff = diff.unsigned_abs();
            let nonlinear_gain = ENHANCE_TABLES.nonlinear[abs_diff as usize] as i32;
            let texture_q8 = texture_weight(carry_abs_diff as u32, abs_diff);
            carry_abs_diff = abs_diff as i16;
            let combined_q8 = (motion_factor * texture_q8 + 128) >> 8;
            let amount = ((effective_base * combined_q8 + 128) >> 8).min(1023);
            let midtone_q8 = ENHANCE_TABLES.midtone[y2 as usize] as u32;
            let amount = ((amount * midtone_q8 + 128) >> 8).min(1023);
            let adj = (((diff * nonlinear_gain + 128) >> 8) * amount as i32 + 128) >> 8;
            total_adj += adj.unsigned_abs() as u64;
            let enhanced = ((y2 as i32 + adj).clamp(0, 255)) as u8;

            let curve_val = tone_curve_branchless(enhanced);
            let mask = ((grad >= 16) as u8).wrapping_sub(1);
            let final_y = enhanced ^ (mask & (enhanced ^ curve_val));

            y_plane[i] = final_y;
            prev[i] = final_y;
        }
    }

    // ── 4.  Chroma saturation boost (every 8 frames, tied to chroma_tick) ──
    // When USM enhances luma detail, chroma feels desaturated by comparison.
    // Amortize the UV traversal by boosting only when chroma stats recompute.
    //   avg_adj = 0   → coeff = 256 (1.0×,  no change)
    //   avg_adj = 64  → coeff = 384 (1.5×,  moderate boost)
    //   avg_adj ≥ 128 → coeff = 448 (1.75×, saturate)
    if *chroma_tick == 0 && total_adj > 0 && uv_size > 0 {
        let avg_adj = (total_adj / y_size as u64) as u32;
        let sat_coeff = (256u32).saturating_add(avg_adj * 3 / 2).min(448);
        let (u_plane, v_plane) = i420[y_size..y_size + 2 * uv_size].split_at_mut(uv_size);
        for (u, v) in u_plane.iter_mut().zip(v_plane.iter_mut()) {
            // Branchless: (c - 128) × coeff / 256 + 128
            //   coeff = 256 → result = c (no change, sat_coeff wraps)
            let uc = ((*u as i32 - 128) * sat_coeff as i32 + 128) >> 8;
            let vc = ((*v as i32 - 128) * sat_coeff as i32 + 128) >> 8;
            *u = (128 + uc).clamp(0, 255) as u8;
            *v = (128 + vc).clamp(0, 255) as u8;
        }
    }
}

/// BGRA → I420, writing into a pre-sized buffer (no resize).
/// Buffer must have capacity >= width * height * 3 / 2.
fn bgra_to_i420_into(bgra: &[u8], width: u32, height: u32, out: &mut [u8]) {
    let src = ArgbImage {
        data: bgra,
        stride: (width * 4) as usize,
    };
    let y_size = (width * height) as usize;
    let uv_size = ((width / 2) * (height / 2)) as usize;
    let (y_plane, rest) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = rest.split_at_mut(uv_size);
    let mut dst = I420ImageMut {
        y: y_plane,
        y_stride: width as usize,
        u: u_plane,
        u_stride: (width / 2) as usize,
        v: v_plane,
        v_stride: (width / 2) as usize,
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
        if self.last_adjust.elapsed() < MIN_GAP {
            return false;
        }
        if (new_bps as i32 - self.last_bitrate_bps as i32).abs() < 50000 {
            return false;
        }
        let clamped = new_bps.clamp(100_000, 10_000_000);
        let mut val: i32 = clamped as i32;
        // SAFETY: set_option with ENCODER_OPTION_BITRATE is a well-defined
        // operation in openh264's public C API — the encoder handles mid-stream
        // bitrate changes safely without requiring re-initialization.
        unsafe {
            self.inner
                .raw_api()
                .set_option(ENCODER_OPTION_BITRATE, std::ptr::addr_of_mut!(val).cast());
        }
        let old_bps = self.last_bitrate_bps;
        self.last_bitrate_bps = clamped;
        self.last_adjust = std::time::Instant::now();
        // Only force IDR on significant drops (>30%) to avoid bloating an
        // already-congested link. Small adjustments transition smoothly via
        // the encoder's internal rate control.
        if clamped < old_bps && (old_bps - clamped) > old_bps * 30 / 100 {
            self.inner.force_intra_frame();
            log::info!(
                "[encoder] {}→{}kbps (IDR forced)",
                old_bps / 1000,
                clamped / 1000
            );
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
            (
                &i420[..y_size],
                &i420[y_size..y_size + uv_size],
                &i420[y_size + uv_size..],
            ),
            (w, h),
            (w, w / 2, w / 2),
        );
        let bitstream = self
            .inner
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
    /// Channel to signal the main loop that ICE restart is needed.
    recovery_tx: tokio::sync::mpsc::UnboundedSender<()>,
    /// If set, the deadline before which the connection should recover; None = stable.
    recovery_deadline: Arc<Mutex<Option<Instant>>>,
    /// Handle of the background recovery task; abort() on successful recovery.
    recovery_task_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Number of ICE restart attempts already made during this recovery session.
    recovery_attempts: Arc<Mutex<u32>>,
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

        log::debug!(
            "[ice] candidate: {} {}:{} ...",
            candidate.typ,
            candidate.address,
            candidate.port
        );

        if let Ok(init) = candidate.to_json() {
            let msg = serde_json::to_string(&SignalingMessage::Ice {
                candidate: init.candidate,
                sdp_mline_index: init.sdp_mline_index.unwrap_or(0) as u32,
            })
            .ok();
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
                // 连接恢复 → 取消恢复任务
                if let Some(handle) = self.recovery_task_handle.lock().unwrap().take() {
                    handle.abort();
                }
                *self.recovery_deadline.lock().unwrap() = None;
                *self.recovery_attempts.lock().unwrap() = 0;
                let _ = self.connected_tx.try_send(());
            }
            RTCPeerConnectionState::Disconnected => {
                // 启动 15s 容忍窗口
                let deadline = Instant::now() + Duration::from_secs(15);
                *self.recovery_deadline.lock().unwrap() = Some(deadline);
                *self.recovery_attempts.lock().unwrap() = 0;
                let recovery_tx = self.recovery_tx.clone();
                let recovery_deadline = self.recovery_deadline.clone();
                let handle_arc = self.recovery_task_handle.clone();
                let handle = tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(15)).await;
                    // 再次检查是否仍然需要恢复
                    if recovery_deadline.lock().unwrap().is_some() {
                        log::info!("[recovery] timeout reached, signaling main loop");
                        let _ = recovery_tx.send(());
                    }
                });
                *handle_arc.lock().unwrap() = Some(handle);
            }
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                // 彻底失败 → 直接触发全局关闭（保留原有逻辑）
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

/// Initiate an ICE restart by creating a new offer with `ice_restart=true` and sending
/// it to the browser via WebSocket.
async fn initiate_ice_restart(
    pc: &dyn PeerConnection,
    out_tx: &mpsc::Sender<Message>,
) -> Result<()> {
    let options = RTCOfferOptions { ice_restart: true };
    let offer = pc.create_offer(Some(options)).await?;
    pc.set_local_description(offer).await?;
    let local_desc = pc
        .local_description()
        .await
        .context("local_description returned None after ICE restart")?;
    let msg = serde_json::to_string(&SignalingMessage::Offer {
        sdp: local_desc.sdp,
        renegotiation: Some(true),
    })?;
    out_tx.send(Message::Text(msg.into())).await?;
    log::info!("[recovery] ICE restart offer sent");
    Ok(())
}

/// Get all non-loopback LAN IPs + loopback fallback for local testing.
fn get_local_ips() -> Vec<String> {
    let mut ips: Vec<String> = match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
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
            env_logger::Env::default().default_filter_or(&args.log_level),
        )
        .filter(Some("rtc_ice"), log::LevelFilter::Error)
        .filter(Some("rtc::peer_connection"), log::LevelFilter::Error)
        .filter(
            Some("webrtc::peer_connection::driver"),
            log::LevelFilter::Off,
        )
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
        use tokio::signal::unix::{SignalKind, signal};
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
                let Ok((conn, _)) = connect_to_display(&display) else {
                    return;
                };
                // Release all possible keycodes (unpressed ones are silently ignored)
                for kc in 8..=255u8 {
                    let _ = xtest::fake_input(&conn, X11_KEY_RELEASE, kc, 0, 0, 0, 0, 0);
                }
                let _ = conn.flush();
                std::thread::sleep(std::time::Duration::from_millis(50));
            })
            .await;
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
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
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
            response
                .headers_mut()
                .insert(axum::http::header::SET_COOKIE, hv);
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
    let display_num: u16 = display
        .trim_start_matches(':')
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .context("invalid display format")?;

    // Try the standard X11 socket path first, then fall back to Termux.
    let sock = format!("/tmp/.X11-unix/X{}", display_num);
    let unix_stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(_) => {
            let termux_sock = format!(
                "/data/data/com.termux/files/usr/tmp/.X11-unix/X{}",
                display_num
            );
            log::info!("[x11] connecting via Termux socket path: {}", termux_sock);
            UnixStream::connect(&termux_sock).context("cannot connect to Termux X11 socket")?
        }
    };

    // Set 5-second receive timeout on all X11 sockets so that blocking
    // reply() calls (e.g. mit-shm get_image) don't hang forever.
    let fd = unix_stream.as_raw_fd();
    let tv = libc::timeval {
        tv_sec: 5,
        tv_usec: 0,
    };
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }
    log::info!("[x11] SO_RCVTIMEO=5s set on display {}", display);

    let (stream, (family, address)) =
        DefaultStream::from_unix_stream(unix_stream).context("from_unix_stream failed")?;
    let (auth_name, auth_data) = get_auth(family, &address, display_num)
        .unwrap_or(None)
        .unwrap_or_else(|| (Vec::new(), Vec::new()));
    let conn = RustConnection::connect_to_stream_with_auth_info(stream, 0, auth_name, auth_data)
        .context("connect_to_stream failed")?;
    log::info!("[x11] connected");
    Ok((conn, 0usize))
}

fn setup_x11_connections(
    display: &str,
    keycode_cache: std::sync::Arc<std::collections::HashMap<u32, u8>>,
) -> Result<(
    Arc<CaptureState>,
    Arc<InputState>,
    u16,
    u16,
    u8,
    RustConnection,
)> {
    log::info!(
        "[x11] connecting to display {} (capture connection)",
        display
    );

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
    let xtest_cookie = xtest::get_version(&inp_conn, 2, 2).context("XTest not available")?;
    xtest_cookie.reply().context("XTest query failed")?;

    // Get current pointer position on input connection
    let ptr = xproto::query_pointer(&inp_conn, root)
        .context("query_pointer failed")?
        .reply()
        .context("query_pointer reply failed")?;

    let setup = inp_conn.setup();

    log::info!(
        "[x11] connected, root=0x{:x}, pointer=({},{}), dims={}x{}, keycodes={}-{}",
        root,
        ptr.root_x,
        ptr.root_y,
        screen_width,
        screen_height,
        setup.min_keycode,
        setup.max_keycode
    );

    // Third connection for event-driven cursor tracking via XI2
    log::info!(
        "[x11] connecting to display {} (xi2 event connection)",
        display
    );
    let (evt_conn, _) = connect_to_display(display)?;

    // Query XI2 version
    let xi_ver = xinput::xi_query_version(&evt_conn, 2, 0)
        .context("XI2 query_version failed")?
        .reply()
        .context("XI2 query_version reply failed")?;
    log::info!(
        "[x11] XI2 version: {}.{}",
        xi_ver.major_version,
        xi_ver.minor_version
    );

    // Select XI_Motion events on root window for all master devices
    xinput::xi_select_events(
        &evt_conn,
        root,
        &[EventMask {
            deviceid: 1, // XIAllMasterDevices
            mask: vec![XIEventMask::MOTION],
        }],
    )
    .context("XI2 select_events failed")?;
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

    Ok((
        capture_state,
        input_state,
        screen_width,
        screen_height,
        screen_depth,
        evt_conn,
    ))
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

    if ctx
        .connect(None, pulse::context::FlagSet::NOFLAGS, None)
        .is_err()
    {
        log::info!("[audio] PA connect failed");
        return None;
    }

    // Run mainloop until ready or timeout (~2s)
    for _ in 0..200 {
        if mainloop.iterate(false).is_error() {
            break;
        }
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
    introspect.get_server_info(Box::new(
        move |info: &pulse::context::introspect::ServerInfo| {
            if let Some(ref name) = info.default_sink_name {
                let _ = info_tx.send(name.to_string());
            }
        },
    ));

    for _ in 0..50 {
        if mainloop.iterate(false).is_error() {
            break;
        }
        if let Ok(name) = info_rx.try_recv() {
            let monitor = format!("{}.monitor", name);
            log::info!(
                "[audio] detected default sink '{}', monitor '{}'",
                name,
                monitor
            );
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
    /// Receiver for ICE restart recovery signals from the handler's background task.
    recovery_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
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
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: "".into(),
                },
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: "pli".into(),
                },
                RTCPFeedback {
                    typ: "ccm".into(),
                    parameter: "fir".into(),
                },
                RTCPFeedback {
                    typ: "goog-remb".into(),
                    parameter: "".into(),
                },
            ],
        },
        payload_type: 102,
        ..Default::default()
    };
    media_engine
        .register_codec(video_codec.clone(), RtpCodecKind::Video)
        .context("register H264 codec")?;

    // ── Register Opus audio codec ──
    let audio_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: "".into(),
                },
                RTCPFeedback {
                    typ: "transport-cc".into(),
                    parameter: "".into(),
                },
            ],
        },
        payload_type: 111,
        ..Default::default()
    };
    media_engine
        .register_codec(audio_codec.clone(), RtpCodecKind::Audio)
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
        Some(std::time::Duration::from_secs(15)), // disconnected (default 5s)
        Some(std::time::Duration::from_secs(60)), // failed (default 25s)
        Some(std::time::Duration::from_secs(5)),  // keepalive (default 2s)
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

    // ── Recovery channel: handler signals main loop when ICE restart is needed ──
    let (recovery_tx, recovery_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

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
        recovery_tx,
        recovery_deadline: Arc::new(Mutex::new(None)),
        recovery_task_handle: Arc::new(Mutex::new(None)),
        recovery_attempts: Arc::new(Mutex::new(0)),
    });

    let rt = runtime::default_runtime().context("no webrtc runtime available")?;

    let pc = Box::new(
        PeerConnectionBuilder::new()
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
            .context("build PeerConnection")?,
    );
    log::info!("[pc] PeerConnection created");

    // ── Post-pc operations (tracks, offer/answer) — close pc on error ──
    let v = match async {
        // ── Create video track ──
        let ssrc = rand::random::<u32>();
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
        ))
        .context("create video track")?;
        let track = Arc::new(track);
        let track_local: Arc<dyn TrackLocal> = track.clone();
        pc.add_track(track_local).await.context("add video track")?;
        log::info!("[pc] video track added");

        // ── Create audio track ──
        let audio_ssrc = rand::random::<u32>();
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
        ))
        .context("create audio track")?;
        let audio_track = Arc::new(audio_track);
        let audio_track_local: Arc<dyn TrackLocal> = audio_track.clone();
        pc.add_track(audio_track_local)
            .await
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
        let offer = pc.create_offer(None).await.context("create_offer")?;
        pc.set_local_description(offer)
            .await
            .context("set_local_description")?;

        let local = pc
            .local_description()
            .await
            .context("local_description returned None")?;
        let offer_msg = serde_json::to_string(&SignalingMessage::Offer {
            sdp: local.sdp.clone(),
            renegotiation: None,
        })
        .context("serialize offer")?;
        log::info!("[sdp] sending offer ({} bytes)", local.sdp.len());
        out_tx
            .try_send(Message::Text(offer_msg.into()))
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
        let answer = RTCSessionDescription::answer(answer_sdp).context("invalid answer SDP")?;
        pc.set_remote_description(answer)
            .await
            .context("set_remote_description")?;
        log::info!("[sdp] remote description set");

        anyhow::Ok((track, audio_track, audio_ssrc, input_dc))
    }
    .await
    {
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
        recovery_rx,
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
    unsafe {
        mi_collect(false);
    }

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
    let (capture_state, input_state, native_w, native_h, depth, evt_conn) =
        match setup_x11_connections(&state.args.display, keycode_cache) {
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

    log::info!(
        "[capture] native={}x{} output={}x{} scaling={}",
        native_w,
        native_h,
        out_w,
        out_h,
        needs_scaling
    );

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
        all_ips
            .iter()
            .filter(|ip| *ip != "127.0.0.1" && *ip != "::1")
            .map(|ip| fmt_bind_addr(ip, 0))
            .collect()
    };

    let sig = match run_signaling(&mut in_rx, &out_tx, &state, &all_ips, tcp_addrs, udp_addrs).await
    {
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
    let mut recovery_rx = sig.recovery_rx;

    // ── Send initial cursor position ──
    send_cursor_position(&out_tx, &input_state, native_w, native_h, out_w, out_h);

    // ── Create ScreenCapture (SHM-accelerated with fallback) ──
    let screen_capture = match ShmScreenCapture::try_new(
        capture_state.clone(),
        native_w,
        native_h,
        depth,
        out_w,
        out_h,
    ) {
        Ok(Some(shm)) => {
            log::info!("[capture] using MIT-SHM acceleration");
            ScreenCapture::Shm(shm)
        }
        Ok(None) => {
            log::info!("[capture] SHM unavailable, using get_image fallback");
            ScreenCapture::Fallback(FallbackCapture::new(
                capture_state.clone(),
                native_w,
                native_h,
                out_w,
                out_h,
            ))
        }
        Err(e) => {
            log::info!("[capture] SHM init failed: {}, using get_image fallback", e);
            ScreenCapture::Fallback(FallbackCapture::new(
                capture_state.clone(),
                native_w,
                native_h,
                out_w,
                out_h,
            ))
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
    log::info!(
        "[encoder] created ({}x{}, {}kbps, {}fps)",
        out_w,
        out_h,
        state.args.bitrate,
        state.args.framerate
    );

    // ── Forward ICE candidates ──
    let ice_out_tx = out_tx.clone();
    tasks.spawn(async move {
        while let Some(candidate_msg) = ice_rx.recv().await {
            if ice_out_tx
                .send(Message::Text(candidate_msg.into()))
                .await
                .is_err()
            {
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
    let frame_duration =
        std::time::Duration::from_nanos(1_000_000_000 / state.args.framerate as u64);
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
    let enhance = state
        .args
        .enhance
        .unwrap_or_else(|| if needs_scaling { 0.8 } else { 0.0 });
    log::info!(
        "[pipeline] starting 3-stage pipeline (cap→enc→send), {} fps, enhance={}",
        state.args.framerate,
        enhance
    );

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

    // I420 buffer pool: wraps a Vec<u8> and auto-returns to pool on Bytes drop.
    // Used with Bytes::from_owner() for zero-copy capture→encode.
    struct PooledBuf {
        buf: Vec<u8>,
        pool: Arc<crossbeam_queue::ArrayQueue<Vec<u8>>>,
    }
    impl AsRef<[u8]> for PooledBuf {
        fn as_ref(&self) -> &[u8] {
            self.buf.as_ref()
        }
    }
    impl Drop for PooledBuf {
        fn drop(&mut self) {
            let mut buf = std::mem::take(&mut self.buf);
            buf.clear();
            self.pool.push(buf).ok();
        }
    }

    // Pre-allocate 3 Vecs: channel depth 2 + 1 in-use guarantees the capture
    // thread never blocks on allocation while the encoder is two frames ahead.
    let pool: Arc<crossbeam_queue::ArrayQueue<Vec<u8>>> =
        Arc::new(crossbeam_queue::ArrayQueue::new(3));
    for _ in 0..3 {
        pool.push(Vec::with_capacity(frame_size)).ok();
    }

    // ── Stage 1: Capture + Convert (spawn_blocking) ──
    tasks.spawn_blocking(move || {
        let mut last_raw: Option<Bytes> = None;
        let mut tmp_argb = Vec::new();
        let mut chroma_tick = 0u32;
        let mut last_chroma_boost = 256u32;
        loop {
            if cap_stop.is_cancelled() {
                break;
            }

            let frame_start = std::time::Instant::now();

            // Pop a pre-allocated buffer from the pool; fall back to fresh
            // allocation only if the pool is empty (should not happen at
            // steady state).
            let mut i420 = pool.pop().unwrap_or_else(|| Vec::with_capacity(frame_size));
            // Pre-size the Vec so the prev-seed read below is valid.
            // The pool Vec has len=0 (cleared before push); set_len makes the
            // slice accessible. capture_to_i420 will re-size via with_resize_uninit.
            if i420.len() < frame_size {
                unsafe {
                    i420.set_len(frame_size);
                }
            }

            // Ensure tmp_argb is sized as [blur_copy | prev] for MAD fusion
            let y_size = (out_w * out_h) as usize;
            let total_req = 2 * y_size;
            if tmp_argb.len() != total_req {
                tmp_argb.clear();
                tmp_argb.reserve(total_req);
                unsafe {
                    tmp_argb.set_len(total_req);
                }
                // First frame or resolution change: seed prev with current frame
                let y_plane = &i420[..y_size];
                tmp_argb[y_size..total_req].copy_from_slice(y_plane);
            }
            let (blur_copy, prev) = tmp_argb.split_at_mut(y_size);

            match screen_capture.capture_to_i420(
                &mut i420,
                out_w,
                out_h,
                needs_scaling,
                blur_copy,
                prev,
            ) {
                Err(e) => {
                    log::info!("[capture] error: {:#}, repeating last frame", e);
                    // Return the failed buffer to pool (it was never filled)
                    i420.clear();
                    pool.push(i420).ok();
                    if let Some(ref last) = last_raw {
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
                }
                Ok((mad_sum, tv_sum, luma_sum)) => {
                    // Capture succeeded — apply Y-plane unsharp mask enhancement
                    if enhance > 0.0 {
                        let y_size = out_w as usize * out_h as usize;
                        let uv_size = (out_w as usize / 2) * (out_h as usize / 2);
                        apply_enhancement(
                            &mut i420[..y_size + 2 * uv_size],
                            out_w as usize,
                            out_h as usize,
                            enhance,
                            &mut tmp_argb,
                            &mut chroma_tick,
                            &mut last_chroma_boost,
                            mad_sum,
                            tv_sum,
                            luma_sum,
                        );
                    }
                    // Zero-copy: Bytes::from_owner wraps the pool Vec so that
                    // when the encoder drops the last Bytes reference, the Vec
                    // is automatically returned to the pool via PooledBuf::drop.
                    let pool_clone = pool.clone();
                    let frame = Bytes::from_owner(PooledBuf {
                        buf: i420,
                        pool: pool_clone,
                    });
                    last_raw = Some(frame.clone()); // cheap Arc increment
                    if let Err(e) = yuv_tx_cap.try_send(frame) {
                        match e {
                            block_mpsc::TrySendError::Full(_) => {
                                // frame dropped — Bytes::drop → PooledBuf::drop
                                // will return the Vec to pool automatically
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
            }
            if cap_stop.is_cancelled() {
                return;
            }

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
            if enc_stop.is_cancelled() {
                break;
            }
            // Check for adaptive bitrate update
            let desired = enc_bps.load(Ordering::Relaxed);
            encoder.set_bitrate(desired);
            match yuv_rx.recv_timeout(std::time::Duration::from_millis(20)) {
                Ok(yuv) => {
                    enc_buf.clear();
                    if let Err(e) = encoder.encode(yuv.as_ref(), &mut enc_buf) {
                        enc_failures += 1;
                        hard_failures += 1;
                        log::error!(
                            "[encoder] error #{}/{} (hard={}/{}): {:#}",
                            enc_failures,
                            ENC_MAX_FAILURES,
                            hard_failures,
                            ENC_HARD_LIMIT,
                            e
                        );
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
                                enc_failures as u64 * 100,
                            ));
                            encoder = match VideoEncoder::new(&enc_args, enc_w, enc_h) {
                                Ok(e) => e,
                                Err(_) => {
                                    enc_done.notify_one();
                                    break;
                                }
                            };
                            enc_failures = 0;
                            continue;
                        }
                        encoder.force_keyframe();
                        // Exponential backoff before retry
                        std::thread::sleep(std::time::Duration::from_millis(
                            enc_failures.min(10) as u64 * 100,
                        ));
                        continue;
                    }
                    enc_failures = 0; // reset on success
                    // Zero-copy: transfer encoder buffer ownership to Bytes,
                    // replacing enc_buf with an empty Vec for the next frame.
                    let frame = Bytes::from(std::mem::take(&mut enc_buf));
                    if enc_stop.is_cancelled() {
                        return;
                    }
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
                    if let Err(e) = track.sample_writer(track_ssrc, 102).write_sample(&Sample {
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
    let audio_source = tokio::task::spawn_blocking(find_default_monitor)
        .await
        .unwrap_or(None);
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
            None => {
                log::error!("[audio] PA mainloop init failed");
                return;
            }
        };

        // Store pointer for external wakeup (called from cleanup on any thread).
        *pa_mainloop_ptr_clone.0.lock().unwrap() = Some(&mut mainloop as *mut _);

        let mut ctx = match pulse::context::Context::new(&mainloop, "vnrit") {
            Some(c) => c,
            None => {
                log::error!("[audio] PA context init failed");
                return;
            }
        };

        // Connect and wait for context to be ready
        if ctx
            .connect(None, pulse::context::FlagSet::NOFLAGS, None)
            .is_err()
        {
            log::error!("[audio] PA connect failed");
            return;
        }
        for _ in 0..200 {
            if audio_cancelled_clone.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let _ = mainloop.iterate(false);
            if ctx.get_state() == pulse::context::State::Ready {
                break;
            }
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
        if stream
            .connect_record(dev, None, pulse::stream::FlagSet::NOFLAGS)
            .is_err()
        {
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
        let mut encoder =
            match opus::Encoder::new(48000, opus::Channels::Stereo, opus::Application::Audio) {
                Ok(e) => e,
                Err(e) => {
                    log::info!("[audio] Opus encoder init failed: {}", e);
                    return;
                }
            };
        let mut opus_out = Vec::with_capacity(4096);
        loop {
            if audio_enc_stop.is_cancelled() {
                break;
            }
            match pcm_rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(pcm) => {
                    // Safety: pcm comes from PulseAudio which always returns multiples of frame size
                    // (3840 bytes = 1920 i16 samples @ 20ms stereo 48kHz). Assert to prevent UB.
                    assert_eq!(
                        pcm.len() % 2,
                        0,
                        "PCM buffer length must be even for i16 samples"
                    );
                    let samples = unsafe {
                        std::slice::from_raw_parts(pcm.as_ptr() as *const i16, pcm.len() / 2)
                    };
                    opus_out.clear();
                    opus_out.reserve(4096);
                    // SAFETY: encoder.encode 写入前 n 字节，不读 buf，truncate 丢弃未写部分
                    unsafe {
                        opus_out.set_len(4096);
                    }
                    match encoder.encode(samples, &mut opus_out) {
                        Ok(n) => {
                            opus_out.truncate(n);
                            // Return PCM buffer to pool for reuse
                            let _ = pcm_pool_enc.push(pcm);
                            // Zero-copy: send Opus Vec directly
                            if audio_enc_stop.is_cancelled() {
                                return;
                            }
                            if audio_opus_tx
                                .try_send(std::mem::take(&mut opus_out))
                                .is_err()
                            {
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
                    if let Err(e) = audio_track.sample_writer(audio_ssrc, 111).write_sample(&Sample {
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
    use std::os::unix::io::FromRawFd;
    use tokio::io::unix::AsyncFd;
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
                unsafe {
                    (&mut *ptr).wakeup();
                }
                *guard = None;
            }
        }
        cancel.cancel();
        // Drop channels before waiting for tasks — makes blocking recv exit
        drop(yuv_tx);
        drop(enc_tx);
        drop(pcm_tx);
        drop(opus_tx);
        // Wait for tasks to exit before releasing WebRTC resources
        tasks.shutdown().await;
        let _ = pc.close().await;
        drop(pc);
        drop(handler);
        unsafe {
            mi_collect(false);
        }
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
                    unsafe {
                        (&mut *ptr).wakeup();
                    }
                    *guard = None;
                }
            }
            cancel.cancel();
            // Drop channels before waiting for tasks — makes blocking recv exit
            drop(yuv_tx);
            drop(enc_tx);
            drop(pcm_tx);
            drop(opus_tx);
            // Wait for tasks to exit before releasing WebRTC resources
            tasks.shutdown().await;
            let _ = pc.close().await;
            drop(pc);
            drop(handler);
            unsafe {
                mi_collect(false);
            }
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
                                SignalingMessage::Offer { sdp, renegotiation } => {
                                    if renegotiation.unwrap_or(false) {
                                        log::info!("[recovery] received renegotiation offer, creating answer");
                                        match RTCSessionDescription::offer(sdp) {
                                            Ok(offer) => {
                                                let _ = pc.set_remote_description(offer).await;
                                                if let Ok(answer) = pc.create_answer(None).await {
                                                    if pc.set_local_description(answer).await.is_ok() {
                                                        if let Some(local) = pc.local_description().await {
                                                            let answer_msg = SignalingMessage::Answer { sdp: local.sdp };
                                                            if let Ok(json) = serde_json::to_string(&answer_msg) {
                                                                let _ = out_tx.try_send(Message::Text(json.into()));
                                                                log::info!("[recovery] renegotiation answer sent");
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            Err(e) => log::warn!("[recovery] invalid renegotiation offer SDP: {e}"),
                                        }
                                    } else {
                                        log::debug!("[ws] unexpected duplicate offer, ignoring");
                                    }
                                }
                                SignalingMessage::Answer { sdp } => {
                                    let in_recovery = handler.recovery_deadline.lock().unwrap().is_some();
                                    if in_recovery {
                                        log::info!("[recovery] received renegotiation answer");
                                        match RTCSessionDescription::answer(sdp) {
                                            Ok(answer) => {
                                                if let Err(e) = pc.set_remote_description(answer).await {
                                                    log::warn!("[recovery] set_remote_description(answer) failed: {e}");
                                                } else {
                                                    log::info!("[recovery] renegotiation answer applied");
                                                    // 清理恢复状态
                                                    *handler.recovery_deadline.lock().unwrap() = None;
                                                    if let Some(task) = handler.recovery_task_handle.lock().unwrap().take() {
                                                        task.abort();
                                                    }
                                                }
                                            }
                                            Err(e) => log::warn!("[recovery] invalid renegotiation answer SDP: {e}"),
                                        }
                                    } else {
                                        log::debug!("[ws] unexpected answer, ignoring");
                                    }
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

            // ICE restart recovery signal from handler's background task.
            // Fires after 15s of Disconnected state.
            _ = recovery_rx.recv() => {
                if handler.recovery_deadline.lock().unwrap().is_some() {
                    log::info!("[recovery] initiating ICE restart from main loop");
                    if let Err(e) = initiate_ice_restart(&*pc, &out_tx).await {
                        log::warn!("[recovery] ice restart failed: {e}");
                        // 重试一次：若未达到 2 次，重新启动 15s 定时器
                        let mut attempts = handler.recovery_attempts.lock().unwrap();
                        *attempts += 1;
                        if *attempts < 2 && handler.recovery_deadline.lock().unwrap().is_some() {
                            log::info!("[recovery] scheduling retry 2/2 in 10s");
                            let recovery_tx_clone = handler.recovery_tx.clone();
                            let deadline_clone = handler.recovery_deadline.clone();
                            let handle_arc = handler.recovery_task_handle.clone();
                            let handle = tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_secs(10)).await;
                                if deadline_clone.lock().unwrap().is_some() {
                                    log::info!("[recovery] retry timeout reached, signaling main loop");
                                    let _ = recovery_tx_clone.send(());
                                }
                            });
                            *handle_arc.lock().unwrap() = Some(handle);
                        }
                    }
                }
            }
        }
    }

    // ── Phase 0: Immediately release pressed keys ──
    // Must happen before cleanup (which can take seconds), so a reconnecting
    // session doesn't inherit stuck modifier keys from the old one.
    {
        let keys = input_state.pressed_keys.lock().unwrap();
        for &kc in keys.iter() {
            let _ = xtest::fake_input(
                &input_state.conn,
                X11_KEY_RELEASE,
                kc,
                0,
                input_state.root,
                0,
                0,
                0,
            );
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
            unsafe {
                (&mut *ptr).wakeup();
            }
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

    // ── Phase 5: Release X11 resources ──
    // All pipeline tasks have exited — no captures or events are in flight.
    // evt_conn holds the X11 display fd (one per session); dropping it closes
    // the connection.  capture_state and input_state are the final Arc refs
    // held by handle_ws — dropping them frees the X11 connection inside.
    drop(evt_fd);
    drop(evt_conn);
    drop(capture_state);
    drop(input_state);

    // Force mimalloc to return cached memory segments to the OS.
    // Per-session allocations (openh264 ref frames ~1.4MB, X11 buffers, audio)
    // are freed but mimalloc retains them in thread-local heaps. Without
    // explicit collection this manifests as a ~1.5MB/heap RSS increment.
    unsafe {
        mi_collect(false);
    }

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
            let dx: i32 = match fields.next().and_then(|s| s.parse().ok()) {
                Some(v) => v,
                None => return,
            };
            let dy: i32 = match fields.next().and_then(|s| s.parse().ok()) {
                Some(v) => v,
                None => return,
            };
            let max_x = state.screen_w as i32 - 1;
            let max_y = state.screen_h as i32 - 1;
            let new_x = state
                .cursor_x
                .load(Ordering::Relaxed)
                .saturating_add(dx)
                .clamp(0, max_x);
            let new_y = state
                .cursor_y
                .load(Ordering::Relaxed)
                .saturating_add(dy)
                .clamp(0, max_y);
            state.cursor_x.store(new_x, Ordering::Relaxed);
            state.cursor_y.store(new_y, Ordering::Relaxed);
            let _ = xtest::fake_input(
                &state.conn,
                X11_MOTION_NOTIFY,
                0,
                0,
                state.root,
                new_x as i16,
                new_y as i16,
                0,
            );
            let _ = state.conn.flush();
        }
        "ma" => {
            let raw_x: i32 = match fields.next().and_then(|s| s.parse().ok()) {
                Some(v) => v,
                None => return,
            };
            let raw_y: i32 = match fields.next().and_then(|s| s.parse().ok()) {
                Some(v) => v,
                None => return,
            };
            let new_x = ((raw_x as f64 * scale_x) as i32).clamp(0, state.screen_w as i32 - 1);
            let new_y = ((raw_y as f64 * scale_y) as i32).clamp(0, state.screen_h as i32 - 1);
            state.cursor_x.store(new_x, Ordering::Relaxed);
            state.cursor_y.store(new_y, Ordering::Relaxed);
            let _ = xtest::fake_input(
                &state.conn,
                X11_MOTION_NOTIFY,
                0,
                0,
                state.root,
                new_x as i16,
                new_y as i16,
                0,
            );
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
            let _ = xtest::fake_input(&state.conn, X11_BUTTON_PRESS, btn, 0, state.root, cx, cy, 0);
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
            let _ = xtest::fake_input(
                &state.conn,
                X11_BUTTON_RELEASE,
                btn,
                0,
                state.root,
                cx,
                cy,
                0,
            );
            let _ = state.conn.flush();
        }
        "ms" => {
            let delta: f64 = match fields.next().and_then(|s| s.parse().ok()) {
                Some(v) => v,
                None => return,
            };
            let steps = (delta.abs() / 40.0).round().clamp(1.0, 20.0) as u32;
            let btn = if delta > 0.0 { 5_u8 } else { 4_u8 };
            let cx = state.cursor_x.load(Ordering::Relaxed) as i16;
            let cy = state.cursor_y.load(Ordering::Relaxed) as i16;
            for _ in 0..steps {
                let _ =
                    xtest::fake_input(&state.conn, X11_BUTTON_PRESS, btn, 0, state.root, cx, cy, 0);
                let _ = xtest::fake_input(
                    &state.conn,
                    X11_BUTTON_RELEASE,
                    btn,
                    0,
                    state.root,
                    cx,
                    cy,
                    0,
                );
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
                    let _ =
                        xtest::fake_input(&state.conn, X11_KEY_PRESS, kc, 0, state.root, cx, cy, 0);
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
                    let _ = xtest::fake_input(
                        &state.conn,
                        X11_KEY_RELEASE,
                        kc,
                        0,
                        state.root,
                        cx,
                        cy,
                        0,
                    );
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
fn send_cursor_position(
    out_tx: &mpsc::Sender<Message>,
    state: &InputState,
    native_w: u16,
    native_h: u16,
    out_w: u32,
    out_h: u32,
) {
    let x = state.cursor_x.load(Ordering::Relaxed);
    let y = state.cursor_y.load(Ordering::Relaxed);
    let packed = (x as u64) | ((y as u64) << 32);
    // Atomic swap: skip if the same position was already sent
    let prev = state.last_sent_packed.swap(packed, Ordering::Relaxed);
    if prev == packed {
        return;
    }
    // Scale from native X11 coordinates to encoded video coordinates
    let sx = if native_w > 0 {
        x as u64 * out_w as u64 / native_w as u64
    } else {
        x as u64
    };
    let sy = if native_h > 0 {
        y as u64 * out_h as u64 / native_h as u64
    } else {
        y as u64
    };
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
    use flate2::Compression;
    use flate2::write::DeflateEncoder;
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
