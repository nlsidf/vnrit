#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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

use shiguredo_libyuv::{self, FilterMode, ImageSize, ArgbImage, I420Image, I420ImageMut};

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

// ── Color conversion (libyuv SIMD-accelerated via shiguredo_libyuv) ──
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
        let _ = shiguredo_libyuv::argb_to_i420(&src, &mut dst, size);
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
        let _ = shiguredo_libyuv::i420_scale(&src_img, src_size, &mut dst_img, dst_size, FilterMode::Bilinear);
    });
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
    let _ = shiguredo_libyuv::argb_to_i420(&src, &mut dst, size);
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
            .rate_control_mode(RateControlMode::Quality)
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Init logging from --log-level arg (falls back to RUST_LOG env var).
    // Suppress noisy third-party library warnings (expected behavior, not actionable).
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(&args.log_level)
    )
    .filter(Some("rtc_ice"), log::LevelFilter::Error)
    .filter(Some("rtc::peer_connection"), log::LevelFilter::Error)
    .filter(Some("rtc_dtls"), log::LevelFilter::Error)
    .filter(Some("openh264"), log::LevelFilter::Error)
    .format_timestamp(None)
    .init();

    let token = args.token.clone();
    let state = ServerState {
        args: Arc::new(args),
        token,
        keycode_cache: std::sync::OnceLock::new(),
    };

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
}

async fn root_handler(State(state): State<ServerState>) -> Html<String> {
    let html = include_str!("index.html")
        .replace("{{STUN_SERVER}}", &state.args.stun);
    Html(html)
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
    match RustConnection::connect(Some(display)) {
        Ok(v) => Ok(v),
        Err(e) => {
            log::info!("[x11] standard connect failed: {}, trying Termux socket path...", e);
            let display_num: u16 = display.trim_start_matches(':').split('.').next()
                .and_then(|s| s.parse().ok())
                .context("invalid display format")?;
            let sock = format!(
                "/data/data/com.termux/files/usr/tmp/.X11-unix/X{}", display_num
            );
            log::info!("[x11] connecting to {}", sock);
            let unix_stream = UnixStream::connect(&sock)
                .context("cannot connect to Termux X11 socket")?;
            let (stream, (family, address)) = DefaultStream::from_unix_stream(unix_stream)
                .context("from_unix_stream failed")?;
            let (auth_name, auth_data) = get_auth(family, &address, display_num)
                .unwrap_or(None)
                .unwrap_or_else(|| (Vec::new(), Vec::new()));
            let conn = RustConnection::connect_to_stream_with_auth_info(
                stream, 0, auth_name, auth_data,
            ).context("connect_to_stream failed")?;
            log::info!("[x11] connected via Termux socket path");
            Ok((conn, 0usize))
        }
    }
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

async fn handle_ws(ws: WebSocket, state: ServerState) {
    log::info!("[ws] client connected");

    // ── X11 connection setup (triple connections: capture + input + event) ──
    // Build or reuse the shared keycode cache (keysym→keycode mapping)
    let keycode_cache = state.keycode_cache.get_or_init(|| {
        let (conn, _screen_num) = connect_to_display(&state.args.display)
            .expect("failed to connect to X11 for keycode cache");
        let setup = conn.setup();
        let first_kc = setup.min_keycode;
        let keycode_count = setup.max_keycode - setup.min_keycode + 1;
        let kbd = xproto::get_keyboard_mapping(&conn, first_kc, keycode_count)
            .expect("get_keyboard_mapping failed")
            .reply()
            .expect("get_keyboard_mapping reply failed");
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
        std::sync::Arc::new(m)
    }).clone();
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

    let io_handle = tokio::spawn(async move {
        use futures_util::SinkExt;
        let (mut ws_sink, mut ws_stream) = ws.split();
        loop {
            tokio::select! {
                outgoing = out_rx.recv() => {
                    match outgoing {
                        Some(msg) => {
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
                io_handle.abort();
                let _ = io_handle.await;
                return;
            }
            _ => {}
        }
    }
    log::info!("[ws] ready received, creating WebRTC peer connection...");

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
    if let Err(e) = media_engine.register_codec(video_codec.clone(), RtpCodecKind::Video) {
        log::info!("[pc] failed to register H264 codec: {}", e);
        return;
    }

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
    if let Err(e) = media_engine.register_codec(audio_codec.clone(), RtpCodecKind::Audio) {
        log::info!("[pc] failed to register Opus codec: {}", e);
        return;
    }
    let registry = match register_default_interceptors(Registry::new(), &mut media_engine) {
        Ok(r) => r,
        Err(e) => {
            log::info!("[pc] failed to register default interceptors: {}", e);
            return;
        }
    };

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
    // In --tcp-only mode, restrict candidate gathering to TCP only.
    // Default (all four types) is already the webrtc-rs default.
    if state.args.tcp_only {
        settings.set_network_types(vec![NetworkType::Tcp4, NetworkType::Tcp6]);
    }

    // ── Create runtime channels for the handler ──
    let (ice_tx, mut ice_rx) = runtime::channel::<String>(256);
    let (gather_complete_tx, mut gather_complete_rx) = runtime::channel::<()>(1);
    let (connected_tx, mut connected_rx) = runtime::channel::<()>(1);
    let done = Arc::new(tokio::sync::Notify::new());
    let (dc_tx, mut dc_rx) = tokio::sync::watch::channel::<Option<Arc<dyn DataChannel>>>(None);

    // ── Build PeerConnection ──
    let all_ips = get_local_ips();
    // get_local_ips always includes 127.0.0.1; non-loopback IPs come first.
    let lan_ip = all_ips[0].clone();
    log::info!("[ice] binding interfaces: {:?}", all_ips);
    // Bind to ALL non-loopback IPs so each network interface gets its own
    // host candidate. STUN works on the default-route interface.
    // TCP binds include all IPs (loopback included) so browser-on-device works.
    let tcp_addrs: Vec<String> = all_ips.iter().map(|ip| fmt_bind_addr(ip, 0)).collect();
    // UDP binds exclude loopback — sending STUN from 127.0.0.1 to an
    // external server causes EINVAL (Invalid argument) on Linux/Android.
    let udp_addrs = if state.args.tcp_only {
        Vec::<String>::new()
    } else {
        all_ips.iter()
            .filter(|ip| *ip != "127.0.0.1" && *ip != "::1")
            .map(|ip| fmt_bind_addr(ip, 0))
            .collect()
    };
    let handler = Arc::new(WebrtcHandler {
        ice_tx,
        gather_complete_tx,
        connected_tx,
        done: done.clone(),
        dc_tx: dc_tx.clone(),
        lan_ip,
    });

    let input_dc_tx = dc_tx.clone();

    let rt = match runtime::default_runtime() {
        Some(r) => r,
        None => {
            log::info!("[pc] no webrtc runtime available");
            return;
        }
    };

    let pc = match PeerConnectionBuilder::new()
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
    {
        Ok(pc) => pc,
        Err(e) => {
            log::info!("[pc] build failed: {:#}", e);
            return;
        }
    };
    log::info!("[pc] PeerConnection created");

    // ── Create video track ──
    let ssrc = rand::rng().random::<u32>();
    let rtp_codec = video_codec.rtp_codec.clone();

    let track = match TrackLocalStaticSample::new(MediaStreamTrack::new(
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
    )) {
        Ok(t) => Arc::new(t),
        Err(e) => {
            log::info!("[pc] failed to create track: {}", e);
            return;
        }
    };
    let track_local: Arc<dyn TrackLocal> = track.clone();
    if let Err(e) = pc.add_track(track_local).await {
        log::info!("[pc] add_track (video) failed: {}", e);
        return;
    }
    log::info!("[pc] video track added");

    // ── Create audio track ──
    let audio_ssrc = rand::rng().random::<u32>();
    let audio_rtp_codec = audio_codec.rtp_codec.clone();
    let audio_track = match TrackLocalStaticSample::new(MediaStreamTrack::new(
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
    )) {
        Ok(t) => Arc::new(t),
        Err(e) => {
            log::info!("[pc] failed to create audio track: {}", e);
            return;
        }
    };
    let audio_track_local: Arc<dyn TrackLocal> = audio_track.clone();
    if let Err(e) = pc.add_track(audio_track_local).await {
        log::info!("[pc] add_track (audio) failed: {}", e);
        return;
    }
    log::info!("[pc] audio track added, creating offer...");

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
            return;
        }
    };
    log::info!("[encoder] created ({}x{}, {}kbps, {}fps)",
        out_w, out_h, state.args.bitrate, state.args.framerate);

    // ── Create offer and send to browser ──
    let offer = match pc.create_offer(None).await {
        Ok(o) => o,
        Err(e) => {
            log::info!("[pc] create_offer failed: {}", e);
            return;
        }
    };
    if let Err(e) = pc.set_local_description(offer).await {
        log::info!("[pc] set_local_description failed: {}", e);
        return;
    }

    if let Some(local) = pc.local_description().await {
        let offer_msg = match serde_json::to_string(&SignalingMessage::Offer {
            sdp: local.sdp.clone(),
        }) {
            Ok(m) => m,
            Err(e) => {
                log::error!("[sdp] failed to serialize offer: {}", e);
                return;
            }
        };
        log::info!("[sdp] sending offer ({} bytes)", local.sdp.len());
        if out_tx.try_send(Message::Text(offer_msg.into())).is_err() {
            log::info!("[ws] failed to send offer");
            return;
        }
    } else {
        log::info!("[pc] ERROR: no local description after create_offer");
        return;
    }

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
                log::info!("[ws] disconnected waiting for answer");
                return;
            }
            _ => {}
        }
    };

    // ── Set remote description from answer ──
    let answer = match RTCSessionDescription::answer(answer_sdp) {
        Ok(a) => a,
        Err(e) => {
            log::info!("[sdp] invalid answer SDP: {}", e);
            return;
        }
    };
    if let Err(e) = pc.set_remote_description(answer).await {
        log::info!("[pc] set_remote_description failed: {}", e);
        return;
    }
    log::info!("[sdp] remote description set");

    // ── Forward ICE candidates ──
    let ice_out_tx = out_tx.clone();
    let ice_forward = tokio::spawn(async move {
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
        drop(handler);
        let _ = ice_forward.await;
        io_handle.abort();
        let _ = io_handle.await;
        let _ = pc.close().await;
        return;
    }
    log::debug!("[ice] gathering complete");

    // ── Wait for ICE connected state ──
    if tokio::select! {
        _ = connected_rx.recv() => { false }
        _ = done.notified() => { true }
    } {
        log::info!("[pc] connection failed before connected, cleaning up");
        drop(handler);
        let _ = ice_forward.await;
        io_handle.abort();
        let _ = io_handle.await;
        let _ = pc.close().await;
        return;
    }
    log::info!("[pc] connection established!");

    // ── Create input DataChannel (server-initiated) ──
    match pc.create_data_channel("input", None).await {
        Ok(dc) => {
            log::info!("[dc] input channel created (id={})", dc.id());
            let _ = input_dc_tx.send(Some(dc));
        }
        Err(e) => log::warn!("[dc] create_data_channel failed: {}", e),
    }

    // ── Pipeline: 3-stage capture → encode → send ──
    let frame_duration = std::time::Duration::from_nanos(1_000_000_000 / state.args.framerate as u64);
    let track_ssrc = *track.ssrcs().await.first().unwrap_or(&0);
    if track_ssrc == 0 {
        log::info!("[pc] ERROR: no SSRC available for video track");
        return;
    }

    log::info!("[pipeline] starting 3-stage pipeline (cap→enc→send), {} fps", state.args.framerate);

    // Capture flag before it's moved into the encoder task
    let adaptive_bitrate = state.args.adaptive_bitrate;
    let initial_bitrate_bps = (state.args.bitrate as u32) * 1000;

    // Pipeline channels
    let (yuv_tx, yuv_rx) = block_mpsc::sync_channel::<Vec<u8>>(2);
    let (enc_tx, mut enc_rx) = tokio::sync::mpsc::channel::<Bytes>(2);

    // Shutdown signal — unified CancellationToken for all pipeline tasks
    let cancel = tokio_util::sync::CancellationToken::new();

    // I420 memory pool (1 write + 1 in transit + 1 spare)
    use crossbeam_queue::ArrayQueue;
    let i420_pool = {
        let pool = ArrayQueue::new(3);
        let i420_size = (out_w * out_h * 3 / 2) as usize;
        for _ in 0..3 {
            let _ = pool.push(vec![0u8; i420_size]);
        }
        Arc::new(pool)
    };

    // ── Stage 1: Capture + Convert (spawn_blocking) ──
    let cap_stop = cancel.clone();
    let yuv_tx_cap = yuv_tx.clone();
    let i420_pool_cap = i420_pool.clone();
    let cap_handle = tokio::task::spawn_blocking(move || {
        let mut last_raw: Option<Vec<u8>> = None;
        let mut tmp_argb = Vec::new();
        loop {
            if cap_stop.is_cancelled() { break; }

            let frame_start = std::time::Instant::now();

            // Pop I420 buffer from pool
            let mut i420 = match i420_pool_cap.pop() {
                Some(buf) => buf,
                None => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
            };

            if let Err(e) = screen_capture.capture_to_i420(&mut i420, out_w, out_h,
                needs_scaling, &mut tmp_argb)
            {
                log::info!("[capture] error: {:#}, repeating last frame", e);
                // O(1) pointer swap instead of ~3MB copy_from_slice
                if let Some(ref mut last) = last_raw {
                    std::mem::swap(&mut i420, last);
                } else {
                    let _ = i420_pool_cap.push(i420);
                    let elapsed = frame_start.elapsed();
                    if elapsed < frame_duration {
                        std::thread::sleep(frame_duration - elapsed);
                    }
                    continue;
                }
            } else {
                // Capture succeeded
                if let Some(ref mut last) = last_raw {
                    let stale = std::mem::replace(last, i420.clone());
                    let _ = i420_pool_cap.push(stale);
                } else {
                    last_raw = Some(i420.clone());
                }
            }
            if cap_stop.is_cancelled() { return; }

            // Send I420 downstream (try_send — drop on congestion)
            if let Err(e) = yuv_tx_cap.try_send(i420) {
                log::trace!("[capture] dropping frame (channel full)");
                if let block_mpsc::TrySendError::Full(v) = e {
                    let _ = i420_pool_cap.push(v);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
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
    let i420_pool_enc = i420_pool.clone();
    let enc_handle = tokio::task::spawn_blocking(move || {
        let mut enc_buf = Vec::with_capacity(65536);
        let mut last_idr = std::time::Instant::now();
        let mut enc_failures = 0u32;
        const ENC_MAX_FAILURES: u32 = 10;
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
                        log::error!("[encoder] error #{}/{}: {:#}", enc_failures, ENC_MAX_FAILURES, e);
                        if enc_failures >= ENC_MAX_FAILURES {
                            log::error!("[encoder] too many failures, aborting encoder task");
                            let _ = i420_pool_enc.push(yuv);
                            break;
                        }
                        encoder.force_keyframe();
                        let _ = i420_pool_enc.push(yuv);
                        continue;
                    }
                    enc_failures = 0; // reset on success
                    // Return I420 buffer to pool for reuse
                    let _ = i420_pool_enc.push(yuv);
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
                Err(block_mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        log::debug!("[encoder] task ended");
    });

    // ── Stage 4: Send (async) ──
    let send_stop = cancel.clone();
    let send_handle = tokio::spawn(async move {
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
    use libpulse_simple_binding as psimple;

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
    const PCM_FRAME_BYTES: usize = 3840; // 20ms stereo 48kHz S16LE
    let pcm_pool = {
        let pool = ArrayQueue::new(8);
        for _ in 0..8 {
            let _ = pool.push(vec![0u8; PCM_FRAME_BYTES]);
        }
        Arc::new(pool)
    };

    // ── Audio Stage 1: PulseAudio capture (spawn_blocking) ──
    let audio_cap_stop = cancel.clone();
    let audio_pcm_tx = pcm_tx.clone();
    let audio_source = audio_source.clone();
    let pcm_pool_cap = pcm_pool.clone();
    let audio_cap_handle = tokio::task::spawn_blocking(move || {
        let spec = pulse::sample::Spec {
            format: pulse::sample::Format::S16NE,
            channels: 2,
            rate: 48000,
        };
        if !spec.is_valid() {
            log::info!("[audio] invalid PulseAudio sample spec");
            return;
        }
        let dev = audio_source.as_deref();
        let pa = match psimple::Simple::new(
            None, "vnrit", pulse::stream::Direction::Record,
            dev, "audio-capture", &spec, None, None,
        ) {
            Ok(s) => s,
            Err(e) => {
                log::error!("[audio] PulseAudio init failed: {} — audio disabled", e);
                return;
            }
        };
        log::info!("[audio] PulseAudio capture started");

        loop {
            if audio_cap_stop.is_cancelled() { break; }
            // Pop PCM buffer from pool (no per-frame allocation)
            let mut buf = match pcm_pool_cap.pop() {
                Some(b) => b,
                None => {
                    log::trace!("[audio] PCM pool empty, dropping frame");
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
            };
            // PulseAudio Simple::read blocks up to 20ms (one frame).
            // If PA hangs (rare), the cancel token can't interrupt it, but
            // this only delays shutdown by at most one audio frame.
            if let Err(e) = pa.read(&mut buf) {
                log::info!("[audio] read error: {}", e);
                let _ = pcm_pool_cap.push(buf);
                break;
            }
            if audio_cap_stop.is_cancelled() {
                let _ = pcm_pool_cap.push(buf);
                return;
            }
            // Send PCM buffer downstream — no clone needed
            if audio_pcm_tx.send(buf).is_err() {
                return;
            }
        }
        log::debug!("[audio] capture task ended");
    });

    // ── Audio Stage 2: Opus encode (spawn_blocking) ──
    let audio_enc_stop = cancel.clone();
    let audio_opus_tx = opus_tx.clone();
    let pcm_pool_enc = pcm_pool.clone();
    let audio_enc_handle = tokio::task::spawn_blocking(move || {
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
                    opus_out.resize(4096, 0);
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
                Err(block_mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        log::debug!("[audio] encode task ended");
    });

    // ── Audio Stage 3: Send Opus packets via WebRTC (async) ──
    let audio_send_stop = cancel.clone();
    let audio_send_handle = tokio::spawn(async move {
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
        cancel.cancel();
        drop(yuv_tx); drop(enc_tx); drop(pcm_tx); drop(opus_tx);
        if let Err(e) = cap_handle.await { log::error!("[pipeline] capture task panicked: {:?}", e); }
        if let Err(e) = enc_handle.await { log::error!("[pipeline] encode task panicked: {:?}", e); }
        if let Err(e) = send_handle.await { log::error!("[pipeline] send task panicked: {:?}", e); }
        if let Err(e) = audio_cap_handle.await { log::error!("[pipeline] audio capture panicked: {:?}", e); }
        if let Err(e) = audio_enc_handle.await { log::error!("[pipeline] audio encode panicked: {:?}", e); }
        if let Err(e) = audio_send_handle.await { log::error!("[pipeline] audio send panicked: {:?}", e); }
        drop(handler);
        let _ = ice_forward.await;
        let _ = pc.close().await;
        io_handle.abort();
        let _ = io_handle.await;
        return;
    }
    let evt_fd = match unsafe { AsyncFd::new(std::os::unix::io::OwnedFd::from_raw_fd(dup_fd)) } {
        Ok(fd) => fd,
        Err(e) => {
            log::error!("[x11] failed to create AsyncFd for event connection: {}", e);
            // Clean up pipeline tasks and connections before returning
            cancel.cancel();
            drop(yuv_tx); drop(enc_tx); drop(pcm_tx); drop(opus_tx);
            if let Err(e) = cap_handle.await { log::error!("[pipeline] capture task panicked: {:?}", e); }
            if let Err(e) = enc_handle.await { log::error!("[pipeline] encode task panicked: {:?}", e); }
            if let Err(e) = send_handle.await { log::error!("[pipeline] send task panicked: {:?}", e); }
            if let Err(e) = audio_cap_handle.await { log::error!("[pipeline] audio capture panicked: {:?}", e); }
            if let Err(e) = audio_enc_handle.await { log::error!("[pipeline] audio encode panicked: {:?}", e); }
            if let Err(e) = audio_send_handle.await { log::error!("[pipeline] audio send panicked: {:?}", e); }
            drop(handler);
            let _ = ice_forward.await;
            let _ = pc.close().await;
            io_handle.abort();
            let _ = io_handle.await;
            return;
        }
    };

    // ── Periodic stats timer (separate from cursor sync) ──
    // Runs at 200ms; every 3 ticks (~600ms) we collect RTT + adaptive bitrate.
    let mut stats_timer = time::interval(Duration::from_millis(200));

    // ── Input handling runs in main task (non-blocking) ──
    let scale_x = native_w as f64 / out_w as f64;
    let scale_y = native_h as f64 / out_h as f64;
    let mut dc_ref: Option<Arc<dyn DataChannel>> = None;
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
                if let Some(DataChannelEvent::OnMessage(msg)) = dc_event {
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
                                    log::info!("[abr] loss={:.1}%, cutting to {}kbps", ratio * 100.0, desired_bps / 1000);
                                }
                            }
                            break;
                        }
                        if desired_bps > 0 {
                            let current_bps = target_bps.load(Ordering::Relaxed);
                            let new_bps = desired_bps.max(200_000).min(initial_bitrate_bps);
                            if (new_bps as i32 - current_bps as i32).abs() >= 50000 {
                                target_bps.store(new_bps, Ordering::Relaxed);
                                log::info!("[abr] bitrate={}kbps (twcc={}kbps)", new_bps / 1000, desired_bps / 1000);
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

            // Watch for DataChannel registration from on_data_channel handler.
            // Once registered, skip this branch to avoid polling dc_rx forever.
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

            // Detect WebSocket disconnect: when the WS send task exits,
            // out_tx's receiver is dropped, and closed() resolves immediately.
            _ = out_tx.closed() => {
                log::debug!("[loop] WebSocket send channel closed, exiting");
                break;
            }
        }
    }

    // ── CLEANUP ──
    log::info!("[cleanup] client disconnected, shutting down pipeline...");

    // Signal all pipeline tasks to stop via CancellationToken
    cancel.cancel();

    // Release all pressed keys on the X server to prevent stuck keys
    // (e.g. from virtual keyboard clicks where ku is lost on disconnect)
    {
        let keys = input_state.pressed_keys.lock().unwrap_or_else(|e| e.into_inner());
        for &kc in keys.iter() {
            let _ = xtest::fake_input(&input_state.conn, X11_KEY_RELEASE,
                kc, 0, input_state.root, 0, 0, 0);
        }
        if !keys.is_empty() {
            let _ = input_state.conn.flush();
        }
    }

    // Drop channel senders so receivers stop blocking
    drop(yuv_tx);
    drop(enc_tx);
    drop(pcm_tx);
    drop(opus_tx);

    // Wait for pipeline tasks to finish (log any panics)
    if let Err(e) = cap_handle.await { log::error!("[pipeline] capture task panicked: {:?}", e); }
    if let Err(e) = enc_handle.await { log::error!("[pipeline] encode task panicked: {:?}", e); }
    if let Err(e) = send_handle.await { log::error!("[pipeline] send task panicked: {:?}", e); }
    if let Err(e) = audio_cap_handle.await { log::error!("[pipeline] audio capture panicked: {:?}", e); }
    if let Err(e) = audio_enc_handle.await { log::error!("[pipeline] audio encode panicked: {:?}", e); }
    if let Err(e) = audio_send_handle.await { log::error!("[pipeline] audio send panicked: {:?}", e); }

    // ICE forward: drop handler to close ice channel, then await task completion
    drop(handler);
    let _ = ice_forward.await;

    // Close PeerConnection first (sends DTLS close alert), then the WebSocket.
    let _ = pc.close().await;

    // Finally, abort the WebSocket I/O task (no longer needed).
    io_handle.abort();
    let _ = io_handle.await;
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
                    state.pressed_keys.lock().unwrap_or_else(|e| e.into_inner()).insert(kc);
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
                    state.pressed_keys.lock().unwrap_or_else(|e| e.into_inner()).remove(&kc);
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
    match code {
        "Enter" => 0xff0d,
        "Backspace" => 0xff08,
        "Space" => 0x0020,
        "Tab" => 0xff09,
        "Escape" => 0xff1b,
        "ArrowUp" => 0xff52,
        "ArrowDown" => 0xff54,
        "ArrowLeft" => 0xff51,
        "ArrowRight" => 0xff53,
        "ShiftLeft" | "ShiftRight" => 0xffe1,
        "ControlLeft" | "ControlRight" => 0xffe3,
        "AltLeft" | "AltRight" => 0xffe9,
        "MetaLeft" | "MetaRight" => 0xffeb,
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
        "Backslash" | "IntlBackslash" => 0x005c,
        k if k.starts_with("Numpad") => match k {
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
            _ => return 0,
        },
        k if k.starts_with('F') && k.len() <= 4 => {
            let n: u32 = k[1..].parse().unwrap_or(0);
            if (1..=24).contains(&n) {
                0xffbe + n - 1
            } else {
                0
            }
        }
        "Digit0" | "Digit1" | "Digit2" | "Digit3" | "Digit4"
        | "Digit5" | "Digit6" | "Digit7" | "Digit8" | "Digit9" => {
            code.as_bytes()[5] as u32
        }
        _ => {
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
    }
}
