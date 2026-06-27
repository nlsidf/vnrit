#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
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
use std::sync::Mutex;
use tokio::sync::mpsc;
use futures_util::StreamExt;
use std::os::unix::net::UnixStream;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, Window};
use x11rb::protocol::shm::{self, Seg};
use x11rb::protocol::xtest;
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb_protocol::xauth::get_auth;

// ── webrtc-rs types ──
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceServer, RTCPeerConnectionIceEvent,
    RTCSessionDescription, RTCIceGatheringState, RTCPeerConnectionState,
    RTCIceCandidateInit, MediaEngine, register_default_interceptors, Registry,
};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::{MediaStreamTrack, Track};
use webrtc::runtime;
use rtc_media::Sample;
use rtc::rtp_transceiver::rtp_sender::{
    RtpCodecKind, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters,
};
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_H264;

// ── openh264 ──
use openh264::encoder::{Encoder, EncoderConfig, BitRate, FrameRate, UsageType, RateControlMode, IntraFramePeriod, Profile};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;

// ── image scaling ──
use image::RgbaImage;

use bytes::Bytes;
use rand::Rng;
use async_trait::async_trait;
use std::os::fd::IntoRawFd;

/// Shared state passed via axum State to every WebSocket handler.
#[derive(Clone)]
struct ServerState {
    args: Arc<Args>,
    token: Option<String>,
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

─── NOTES ───────────────────────────────────────────────────────

  - vnrit requires a running X11 server (Xvnc, Xvfb, or real X).
  - On Termux, it connects via the Unix socket at
    /data/data/com.termux/files/usr/tmp/.X11-unix/X<display>.
  - No audio supported (video-only).
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

struct AppState {
    conn: RustConnection,
    root: Window,
    cursor_x: AtomicI32,
    cursor_y: AtomicI32,
    // O(1) keysym → keycode lookup, built on init
    keycode_cache: HashMap<u32, u8>,
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
    conn: Arc<Mutex<AppState>>,
    width: u16,
    height: u16,
    shmseg: Seg,
    shm_ptr: *mut u8,
    shm_size: usize,
    bpp: u8,
}

// Raw pointer access is Send+Sync for u8
unsafe impl Send for ShmScreenCapture {}
unsafe impl Sync for ShmScreenCapture {}

impl ShmScreenCapture {
    /// Try to create an SHM-accelerated capture. Returns None if SHM is
    /// not available (MIT-SHM extension missing from X server).
    fn try_new(conn: Arc<Mutex<AppState>>, width: u16, height: u16, depth: u8) -> Result<Option<Self>> {
        // Calculate bytes-per-pixel for ZPixmap
        // depth 24 → 4 bytes (32-bit padded), depth >24 → 4 bytes
        let bpp = if depth >= 24 { 4u8 } else { ((depth as u32 + 7) / 8) as u8 };
        let shm_size = (width as usize) * (height as usize) * (bpp as usize);

        let s = conn.lock().unwrap();

        // Query MIT-SHM version to verify availability
        let ver = match shm::query_version(&s.conn) {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => reply,
                Err(e) => {
                    eprintln!("[shm] MIT-SHM reply error: {:?}, falling back to get_image", e);
                    return Ok(None);
                }
            },
            Err(e) => {
                eprintln!("[shm] MIT-SHM query failed: {}, falling back to get_image", e);
                return Ok(None);
            }
        };

        if ver.major_version == 0 && ver.minor_version == 0 {
            eprintln!("[shm] MIT-SHM extension missing, falling back to get_image");
            return Ok(None);
        }

        eprintln!("[shm] MIT-SHM v{}.{}, allocating {} bytes ({}x{}x{})",
            ver.major_version, ver.minor_version, shm_size, width, height, depth);

        let shmseg = s.conn.generate_id()
            .context("failed to generate SHM seg ID")?;

        // Ask the X server to allocate shared memory and return a file descriptor
        let cookie = shm::create_segment(&s.conn, shmseg, shm_size as u32, false)
            .context("SHM create_segment failed")?;
        let reply = cookie.reply()
            .context("SHM create_segment reply failed")?;

        let raw_fd = reply.shm_fd.into_raw_fd();
        drop(s); // release X11 lock before mmap

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

        eprintln!("[shm] segment allocated at {:?} ({} bytes)", shm_ptr, shm_size);

        Ok(Some(ShmScreenCapture {
            conn,
            width,
            height,
            shmseg,
            shm_ptr,
            shm_size,
            bpp,
        }))
    }

    /// Capture the root window and return raw BGRA pixel data.
    /// The X server writes directly to shared memory; we memcpy out.
    fn capture(&self) -> Result<Vec<u8>> {
        let s = self.conn.lock().unwrap();
        let cookie = shm::get_image(
            &s.conn,
            s.root,   // drawable
            0,        // x offset
            0,        // y offset
            self.width,
            self.height,
            !0,       // plane_mask = all planes
            2,        // format = ZPixmap (hardcoded, matches xproto::ImageFormat::Z_PIXMAP)
            self.shmseg,
            0,        // offset in shared memory
        ).context("SHM get_image failed")?;
        let _reply = cookie.reply().context("SHM get_image reply failed")?;
        drop(s); // release lock before memcpy

        // Copy pixel data from shared memory (X server has written it by now)
        let size = (self.width as usize) * (self.height as usize) * (self.bpp as usize);
        let mut data = vec![0u8; size];
        unsafe {
            std::ptr::copy_nonoverlapping(self.shm_ptr, data.as_mut_ptr(), size);
        }
        Ok(data)
    }

    #[allow(dead_code)]
    fn dimensions(&self) -> (u16, u16) {
        (self.width, self.height)
    }
}

impl Drop for ShmScreenCapture {
    fn drop(&mut self) {
        unsafe {
            // Unmap the shared memory region
            libc::munmap(self.shm_ptr as *mut libc::c_void, self.shm_size);
        }
        // Detach the SHM segment from the X server
        if let Ok(s) = self.conn.lock() {
            let _ = shm::detach(&s.conn, self.shmseg);
        }
        eprintln!("[shm] cleaned up segment {:?}", self.shm_ptr);
    }
}

/// Fallback screen capture using xproto::get_image (no SHM).
/// Used when MIT-SHM extension is not available.
struct FallbackCapture {
    conn: Arc<Mutex<AppState>>,
    width: u16,
    height: u16,
}

impl FallbackCapture {
    fn capture(&self) -> Result<Vec<u8>> {
        let s = self.conn.lock().unwrap();
        let cookie = xproto::get_image(
            &s.conn,
            xproto::ImageFormat::Z_PIXMAP,
            s.root,
            0, 0,
            self.width, self.height,
            !0, // plane_mask = all planes
        ).context("get_image failed")?;
        let reply = cookie.reply().context("get_image reply failed")?;
        Ok(reply.data)
    }

    #[allow(dead_code)]
    fn dimensions(&self) -> (u16, u16) {
        (self.width, self.height)
    }
}

/// Unified interface for both SHM-accelerated and fallback capture.
enum ScreenCapture {
    Shm(ShmScreenCapture),
    Fallback(FallbackCapture),
}

impl ScreenCapture {
    fn capture(&self) -> Result<Vec<u8>> {
        match self {
            ScreenCapture::Shm(s) => s.capture(),
            ScreenCapture::Fallback(f) => f.capture(),
        }
    }

    #[allow(dead_code)]
    fn dimensions(&self) -> (u16, u16) {
        match self {
            ScreenCapture::Shm(s) => s.dimensions(),
            ScreenCapture::Fallback(f) => f.dimensions(),
        }
    }
}

// ── Color conversion ──

/// Scale BGRA image using the `image` crate.
/// Converts BGRA → RGBA for scaling (scaling is pixel-position-based,
/// channel order doesn't affect the interpolation result), then returns
/// RGBA data.
fn scale_bgra_image(bgra: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    // BGRA → RGBA: swap R (byte 2) and B (byte 0)
    let rgba: Vec<u8> = bgra
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0], p[3]])
        .collect();

    let img = RgbaImage::from_raw(src_w, src_h, rgba).expect("invalid image dimensions");
    let scaled = image::imageops::resize(&img, dst_w, dst_h, image::imageops::FilterType::CatmullRom);
    scaled.into_raw() // RGBA data
}

/// Convert RGBA pixel data to I420 (YUV 4:2:0) planar format.
/// Uses BT.601 full-range coefficients.
fn rgba_to_i420(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);
    let mut out = vec![0u8; y_size + 2 * uv_size];

    let (y_plane, rest) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = rest.split_at_mut(uv_size);

    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let r = rgba[i] as f32;
            let g = rgba[i + 1] as f32;
            let b = rgba[i + 2] as f32;

            let yy = (0.299 * r + 0.587 * g + 0.114 * b) as u8;
            y_plane[y * w + x] = yy;

            if x % 2 == 0 && y % 2 == 0 {
                let uu = (-0.1687 * r - 0.3313 * g + 0.5 * b + 128.0) as u8;
                let vv = (0.5 * r - 0.4187 * g - 0.0813 * b + 128.0) as u8;
                let uv_idx = (y / 2) * (w / 2) + (x / 2);
                u_plane[uv_idx] = uu;
                v_plane[uv_idx] = vv;
            }
        }
    }
    out
}

/// Convert BGRA pixel data directly to I420 (YUV 4:2:0) planar format.
/// Single-pass integer math — faster than two-pass BGRA→RGBA→I420.
/// Uses BT.601 full-range coefficients.
fn bgra_to_i420(bgra: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);
    let mut out = vec![0u8; y_size + 2 * uv_size];

    let (y_plane, rest) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = rest.split_at_mut(uv_size);

    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            // BGRA byte order: bgra[i]=B, [i+1]=G, [i+2]=R, [i+3]=A
            let b = bgra[i] as i32;
            let g = bgra[i + 1] as i32;
            let r = bgra[i + 2] as i32;

            // Y = (77*R + 150*G + 29*B) >> 8  (BT.601 full range)
            let yy = ((77 * r + 150 * g + 29 * b) >> 8).clamp(0, 255) as u8;
            y_plane[y * w + x] = yy;

            if x % 2 == 0 && y % 2 == 0 {
                // U = (-43*R - 85*G + 128*B) / 256 + 128
                let u_val = (128 * b - 43 * r - 85 * g) / 256 + 128;
                let v_val = (128 * r - 107 * g - 21 * b) / 256 + 128;
                let uv_idx = (y / 2) * (w / 2) + (x / 2);
                u_plane[uv_idx] = u_val.clamp(0, 255) as u8;
                v_plane[uv_idx] = v_val.clamp(0, 255) as u8;
            }
        }
    }
    out
}

// ── VideoEncoder: wraps openh264 ──

struct VideoEncoder {
    inner: Encoder,
    width: u32,
    height: u32,
}

impl VideoEncoder {
    fn new(args: &Args, width: u32, height: u32) -> Result<Self> {
        let bitrate_bps = (args.bitrate as u32) * 1000;
        let framerate = args.framerate as f32;

        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(framerate))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .intra_frame_period(IntraFramePeriod::from_num_frames(240))
            .profile(Profile::Baseline);

        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("failed to create openh264 encoder")?;

        Ok(VideoEncoder {
            inner: encoder,
            width,
            height,
        })
    }

    fn encode(&mut self, i420: &[u8]) -> Result<Vec<u8>> {
        let yuv = YUVBuffer::from_vec(i420.to_vec(), self.width as usize, self.height as usize);
        let bitstream = self.inner
            .encode(&yuv)
            .context("openh264 encode failed")?;
        Ok(bitstream.to_vec())
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
    done_tx: runtime::Sender<()>,
}

#[async_trait]
impl PeerConnectionEventHandler for WebrtcHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        eprintln!("[ice] candidate: {} ...", &event.candidate.address[..event.candidate.address.len().min(20)]);
        if let Ok(init) = event.candidate.to_json() {
            let msg = serde_json::to_string(&SignalingMessage::Ice {
                candidate: init.candidate,
                sdp_mline_index: init.sdp_mline_index.unwrap_or(0) as u32,
            }).unwrap();
            let _ = self.ice_tx.try_send(msg);
        }
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        eprintln!("[ice] gathering state: {:?}", state);
        if state == RTCIceGatheringState::Complete {
            let _ = self.gather_complete_tx.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        eprintln!("[pc] connection state: {:?}", state);
        match state {
            RTCPeerConnectionState::Connected => {
                let _ = self.connected_tx.try_send(());
            }
            RTCPeerConnectionState::Failed
            | RTCPeerConnectionState::Disconnected
            | RTCPeerConnectionState::Closed => {
                let _ = self.done_tx.try_send(());
            }
            _ => {}
        }
    }

    async fn on_signaling_state_change(&self, state: webrtc::peer_connection::RTCSignalingState) {
        eprintln!("[pc] signaling state: {:?}", state);
    }
}

// ═══════════════════════════════════════════════════════════════
//  main() — server entry point
// ═══════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let token = args.token.clone();
    let state = ServerState {
        args: Arc::new(args),
        token,
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
        response
            .headers_mut()
            .insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());
    }

    Ok(response)
}

// ═══════════════════════════════════════════════════════════════
//  setup_x11_connection() — factored out for reuse
// ═══════════════════════════════════════════════════════════════

fn setup_x11_connection(display: &str) -> Result<(Arc<Mutex<AppState>>, u16, u16, u8)> {
    eprintln!("[x11] connecting to display {}", display);

    let (x11_conn, screen_num) = match RustConnection::connect(Some(display)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[x11] standard connect failed: {}, trying Termux socket path...", e);
            let display_num: u16 = display.trim_start_matches(':').split('.').next()
                .and_then(|s| s.parse().ok())
                .context("invalid display format")?;
            let sock = format!(
                "/data/data/com.termux/files/usr/tmp/.X11-unix/X{}", display_num
            );
            eprintln!("[x11] connecting to {}", sock);
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
            eprintln!("[x11] connected via Termux socket path");
            (conn, 0usize)
        }
    };

    let screen = &x11_conn.setup().roots[screen_num];
    let root = screen.root;
    let screen_width = screen.width_in_pixels;
    let screen_height = screen.height_in_pixels;
    let screen_depth = screen.root_depth;
    let _ = screen; // explicitly end the borrow on x11_conn

    // Verify XTest extension
    let xtest_cookie = xtest::get_version(&x11_conn, 2, 2)
        .context("XTest not available")?;
    xtest_cookie.reply().context("XTest query failed")?;

    // Get current pointer position
    let ptr = xproto::query_pointer(&x11_conn, root)
        .context("query_pointer failed")?
        .reply()
        .context("query_pointer reply failed")?;

    // Cache keyboard mapping
    let setup = x11_conn.setup();
    let first_keycode = setup.min_keycode;
    let keycode_count = setup.max_keycode - setup.min_keycode + 1;
    let kbd = xproto::get_keyboard_mapping(&x11_conn, first_keycode, keycode_count)
        .context("get_keyboard_mapping failed")?
        .reply()
        .context("get_keyboard_mapping reply failed")?;

    eprintln!(
        "[x11] connected, root=0x{:x}, pointer=({},{}), dims={}x{}, keycodes={}-{}",
        root, ptr.root_x, ptr.root_y,
        screen_width, screen_height,
        first_keycode, setup.max_keycode
    );

    let keycode_cache = {
        let kpk = kbd.keysyms_per_keycode as usize;
        let mut m = HashMap::new();
        for (i, chunk) in kbd.keysyms.chunks(kpk).enumerate() {
            let kc = first_keycode + i as u8;
            for &ks in chunk {
                if ks != 0 {
                    m.entry(ks).or_insert(kc);
                }
            }
        }
        m
    };

    let state = Arc::new(Mutex::new(AppState {
        conn: x11_conn,
        root,
        cursor_x: AtomicI32::new(ptr.root_x as i32),
        cursor_y: AtomicI32::new(ptr.root_y as i32),
        keycode_cache,
    }));

    Ok((state, screen_width, screen_height, screen_depth))
}

// ═══════════════════════════════════════════════════════════════
//  handle_ws() — per‑client WebSocket + WebRTC handler
// ═══════════════════════════════════════════════════════════════

async fn handle_ws(ws: WebSocket, state: ServerState) {
    eprintln!("[ws] client connected");

    // ── X11 connection setup ──
    let (x11_state, native_w, native_h, depth) = match setup_x11_connection(&state.args.display) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[x11] FATAL: failed to connect: {:#}", e);
            return;
        }
    };

    // ── Determine output dimensions ──
    let (out_w, out_h) = if state.args.height > 0 {
        let h = state.args.height as u32;
        let w = (native_w as u32 * h) / native_h as u32;
        // Ensure even dimensions for I420
        (w / 2 * 2, h / 2 * 2)
    } else {
        (native_w as u32, native_h as u32)
    };
    let needs_scaling = out_w != native_w as u32 || out_h != native_h as u32;

    eprintln!("[capture] native={}x{} output={}x{} scaling={}",
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
                                eprintln!("[wsio] send error: {}", e);
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
                            eprintln!("[wsio] recv error: {}", e);
                            break;
                        }
                        None => break,
                    }
                }
            }
        }
        eprintln!("[wsio] task ended");
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
                eprintln!("[ws] disconnected before ready");
                return;
            }
            _ => {}
        }
    }
    eprintln!("[ws] ready received, creating WebRTC peer connection...");

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
    media_engine
        .register_codec(video_codec.clone(), RtpCodecKind::Video)
        .expect("failed to register H264 codec");
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)
        .expect("failed to register default interceptors");

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

    // ── Create runtime channels for the handler ──
    let (ice_tx, mut ice_rx) = runtime::channel::<String>(256);
    let (gather_complete_tx, mut gather_complete_rx) = runtime::channel::<()>(1);
    let (connected_tx, mut connected_rx) = runtime::channel::<()>(1);
    let (done_tx, mut done_rx) = runtime::channel::<()>(1);

    // ── Build PeerConnection ──
    let handler = Arc::new(WebrtcHandler {
        ice_tx,
        gather_complete_tx,
        connected_tx,
        done_tx,
    });

    let rt = runtime::default_runtime().expect("no webrtc runtime available");
    let pc = match PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_runtime(rt)
        .with_udp_addrs(vec![format!("{}:0", get_local_ip())])  
        .build()
        .await
    {
        Ok(pc) => pc,
        Err(e) => {
            eprintln!("[pc] build failed: {:#}", e);
            return;
        }
    };
    eprintln!("[pc] PeerConnection created");

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
            eprintln!("[pc] failed to create track: {}", e);
            return;
        }
    };
    let track_local: Arc<dyn TrackLocal> = track.clone();
    if let Err(e) = pc.add_track(track_local).await {
        eprintln!("[pc] add_track failed: {}", e);
        return;
    }
    eprintln!("[pc] video track added, creating offer...");

    // ── Send initial cursor position ──
    send_cursor_position(&out_tx, &x11_state);

    // ── Create ScreenCapture (SHM-accelerated with fallback) ──
    let screen_capture = match ShmScreenCapture::try_new(x11_state.clone(), native_w, native_h, depth) {
        Ok(Some(shm)) => {
            eprintln!("[capture] using MIT-SHM acceleration");
            ScreenCapture::Shm(shm)
        }
        Ok(None) => {
            eprintln!("[capture] SHM unavailable, using get_image fallback");
            ScreenCapture::Fallback(FallbackCapture { conn: x11_state.clone(), width: native_w, height: native_h })
        }
        Err(e) => {
            eprintln!("[capture] SHM init failed: {}, using get_image fallback", e);
            ScreenCapture::Fallback(FallbackCapture { conn: x11_state.clone(), width: native_w, height: native_h })
        }
    };

    // ── Create VideoEncoder ──
    let mut encoder = match VideoEncoder::new(&state.args, out_w, out_h) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("[encoder] failed to create: {:#}", err);
            return;
        }
    };
    eprintln!("[encoder] created ({}x{}, {}kbps, {}fps)",
        out_w, out_h, state.args.bitrate, state.args.framerate);

    // ── Create offer and send to browser ──
    let offer = match pc.create_offer(None).await {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[pc] create_offer failed: {}", e);
            return;
        }
    };
    if let Err(e) = pc.set_local_description(offer).await {
        eprintln!("[pc] set_local_description failed: {}", e);
        return;
    }

    // Send offer to browser via WebSocket
    if let Some(local) = pc.local_description().await {
        let offer_msg = serde_json::to_string(&SignalingMessage::Offer {
            sdp: local.sdp.clone(),
        }).unwrap();
        eprintln!("[sdp] sending offer ({} bytes)", local.sdp.len());
        if out_tx.try_send(Message::Text(offer_msg.into())).is_err() {
            eprintln!("[ws] failed to send offer");
            return;
        }
    } else {
        eprintln!("[pc] ERROR: no local description after create_offer");
        return;
    }

    // ── Receive browser's answer ──
    let answer_sdp = loop {
        match in_rx.recv().await {
            Some(Ok(Message::Text(t))) => {
                if let Ok(SignalingMessage::Answer { sdp }) = serde_json::from_str(&t) {
                    eprintln!("[sdp] received answer ({} bytes)", sdp.len());
                    break sdp;
                }
            }
            Some(Ok(Message::Close(_))) | None => {
                eprintln!("[ws] disconnected waiting for answer");
                return;
            }
            _ => {}
        }
    };

    // ── Set remote description from answer ──
    let answer = RTCSessionDescription::answer(answer_sdp)
        .expect("invalid answer SDP");
    if let Err(e) = pc.set_remote_description(answer).await {
        eprintln!("[pc] set_remote_description failed: {}", e);
        return;
    }
    eprintln!("[sdp] remote description set");

    // ── Forward ICE candidates from handler → WebSocket ──
    // Spawn a task that reads ice_rx and forwards to WebSocket
    let ice_out_tx = out_tx.clone();
    let ice_forward = tokio::spawn(async move {
        while let Some(candidate_msg) = ice_rx.recv().await {
            if ice_out_tx.try_send(Message::Text(candidate_msg.into())).is_err() {
                break;
            }
        }
    });

    // ── Wait for ICE gathering complete ──
    tokio::select! {
        _ = gather_complete_rx.recv() => {
            eprintln!("[ice] gathering complete");
        }
        _ = done_rx.recv() => {
            eprintln!("[ice] connection ended during gathering");
            return;
        }
    }

    // ── Wait for ICE connected state ──
    tokio::select! {
        _ = connected_rx.recv() => {
            eprintln!("[pc] connection established!");
        }
        _ = done_rx.recv() => {
            eprintln!("[pc] connection failed before connected");
            return;
        }
    }

    // ── Main capture + encode + send loop ──
    let frame_duration = std::time::Duration::from_millis(1000 / state.args.framerate as u64);
    let capture = screen_capture;
    let track_ssrc = *track.ssrcs().await.first().unwrap_or(&0);
    if track_ssrc == 0 {
        eprintln!("[pc] ERROR: no SSRC available for video track");
        return;
    }

    eprintln!("[loop] starting capture loop at {:?} intervals, ssrc={}", frame_duration, track_ssrc);

    let mut frame_count: u64 = 0;
    loop {
        let frame_start = std::time::Instant::now();

        // 1. Read WebSocket messages (input + signaling)
        //    Check for incoming messages with non-blocking-like approach
        //    (short timeout so we don't stall capture)
        loop {
            match in_rx.try_recv() {
                Ok(Ok(Message::Text(t))) => {
                    // Try signaling first
                    if let Ok(sig) = serde_json::from_str::<SignalingMessage>(&t) {
                        match sig {
                            SignalingMessage::Ice { candidate, sdp_mline_index } => {
                                eprintln!("[ws] ICE from client: mline={}", sdp_mline_index);
                                let init = RTCIceCandidateInit {
                                    candidate,
                                    sdp_mline_index: Some(sdp_mline_index as u16),
                                    ..Default::default()
                                };
                                let _ = pc.add_ice_candidate(init).await;
                            }
                            SignalingMessage::Offer { .. } => {
                                eprintln!("[ws] unexpected duplicate offer, ignoring");
                            }
                            _ => {}
                        }
                    } else {
                        // Input message
                        handle_input_message(&t, &x11_state);
                        send_cursor_position(&out_tx, &x11_state);
                    }
                }
                Ok(Ok(Message::Close(_))) => {
                    eprintln!("[ws] client sent close");
                    break;
                }
                Ok(Err(_)) => break,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    eprintln!("[ws] channel disconnected");
                    break;
                }
                _ => {}
            }
        }

        // 2. Check if connection is still alive
        match done_rx.try_recv() {
            Ok(()) => {
                eprintln!("[loop] connection closed, exiting");
                break;
            }
            Err(_) => {} // no message yet
        }

        // 3. Capture screen
        let frame_data = match capture.capture() {
            Ok(data) => data,
            Err(e) => {
                eprintln!("[capture] error: {:#}", e);
                tokio::time::sleep(frame_duration).await;
                continue;
            }
        };

        // 4. Scale if needed, then convert to I420 in one pass
        let i420 = if needs_scaling {
            // scale_bgra_image does BGRA→RGBA internally and returns RGBA
            let rgba = scale_bgra_image(&frame_data, native_w as u32, native_h as u32, out_w, out_h);
            rgba_to_i420(&rgba, out_w, out_h)
        } else {
            // Direct BGRA→I420: one pass, no intermediate allocation, integer math
            bgra_to_i420(&frame_data, out_w, out_h)
        };

        // 6. Encode to H.264
        let h264_data = match encoder.encode(&i420) {
            Ok(data) => data,
            Err(e) => {
                eprintln!("[encoder] error: {:#}", e);
                tokio::time::sleep(frame_duration).await;
                continue;
            }
        };

        // 7. Write sample to webrtc track
        if !h264_data.is_empty() {
            if let Err(e) = track.sample_writer(track_ssrc).write_sample(&Sample {
                data: Bytes::from(h264_data),
                duration: frame_duration,
                ..Default::default()
            }).await {
                eprintln!("[send] write_sample error: {}", e);
                break;
            }
        }

        // 8. Maintain target framerate
        frame_count += 1;
        let elapsed = frame_start.elapsed();
        if elapsed < frame_duration {
            tokio::time::sleep(frame_duration - elapsed).await;
        }

        // Periodic keyframe (every 240 frames ≈ 10s at 24fps)
        if frame_count % 240 == 0 {
            encoder.force_keyframe();
        }
    }

    // ── CLEANUP ──
    eprintln!("[cleanup] client disconnected, starting cleanup...");

    ice_forward.abort();
    let _ = ice_forward.await;

    io_handle.abort();
    let _ = io_handle.await;
    drop(out_tx);
    drop(in_tx);

    let _ = pc.close().await;
    eprintln!("[ws] cleanup complete");
}

// ═══════════════════════════════════════════════════════════════
//  Input handling (unchanged from GStreamer version)
// ═══════════════════════════════════════════════════════════════

fn handle_input_message(raw: &str, state: &Arc<Mutex<AppState>>) {
    let parts: Vec<&str> = raw.split(',').collect();
    if parts.is_empty() {
        return;
    }

    let s = state.lock().unwrap();

    match parts[0] {
        "mr" if parts.len() >= 3 => {
            // mr,dx,dy — relative mouse move
            let dx: i32 = parts[1].parse().unwrap_or(0);
            let dy: i32 = parts[2].parse().unwrap_or(0);
            let new_x = s.cursor_x.load(Ordering::Relaxed).saturating_add(dx).max(0);
            let new_y = s.cursor_y.load(Ordering::Relaxed).saturating_add(dy).max(0);
            s.cursor_x.store(new_x, Ordering::Relaxed);
            s.cursor_y.store(new_y, Ordering::Relaxed);
            let _ = xtest::fake_input(&s.conn, X11_MOTION_NOTIFY,
                0, 0, s.root,
                s.cursor_x.load(Ordering::Relaxed) as i16,
                s.cursor_y.load(Ordering::Relaxed) as i16,
                0);
            let _ = s.conn.flush();
        }
        "ma" if parts.len() >= 3 => {
            // ma,x,y — absolute mouse move
            let new_x = parts[1].parse::<i32>().unwrap_or(0).max(0);
            let new_y = parts[2].parse::<i32>().unwrap_or(0).max(0);
            s.cursor_x.store(new_x, Ordering::Relaxed);
            s.cursor_y.store(new_y, Ordering::Relaxed);
            let _ = xtest::fake_input(&s.conn, X11_MOTION_NOTIFY,
                0, 0, s.root,
                s.cursor_x.load(Ordering::Relaxed) as i16,
                s.cursor_y.load(Ordering::Relaxed) as i16,
                0);
            let _ = s.conn.flush();
        }
        "md" if parts.len() >= 2 => {
            // md,button — mouse button down
            let btn: u8 = match parts[1] {
                "2" => 2,
                "3" => 3,
                _ => 1,
            };
            let _ = xtest::fake_input(&s.conn, X11_BUTTON_PRESS,
                btn, 0, s.root, s.cursor_x.load(Ordering::Relaxed) as i16, s.cursor_y.load(Ordering::Relaxed) as i16, 0);
            let _ = s.conn.flush();
        }
        "mu" if parts.len() >= 2 => {
            // mu,button — mouse button up
            let btn: u8 = match parts[1] {
                "2" => 2,
                "3" => 3,
                _ => 1,
            };
            let _ = xtest::fake_input(&s.conn, X11_BUTTON_RELEASE,
                btn, 0, s.root, s.cursor_x.load(Ordering::Relaxed) as i16, s.cursor_y.load(Ordering::Relaxed) as i16, 0);
            let _ = s.conn.flush();
        }
        "ms" if parts.len() >= 2 => {
            // ms,deltaY — scroll wheel (click 4=up, 5=down)
            let delta: f64 = parts[1].parse().unwrap_or(0.0);
            let steps = (delta.abs() / 40.0).round().clamp(1.0, 20.0) as u32;
            let btn = if delta > 0.0 { 5_u8 } else { 4_u8 };
            let cx = s.cursor_x.load(Ordering::Relaxed) as i16;
            let cy = s.cursor_y.load(Ordering::Relaxed) as i16;
            for _ in 0..steps {
                let _ = xtest::fake_input(&s.conn, X11_BUTTON_PRESS,
                    btn, 0, s.root, cx, cy, 0);
                let _ = xtest::fake_input(&s.conn, X11_BUTTON_RELEASE,
                    btn, 0, s.root, cx, cy, 0);
            }
            let _ = s.conn.flush();
        }
        "kd" if parts.len() >= 2 => {
            // kd,code — key down
            let keysym = code_to_keysym(parts[1]);
            if keysym != 0 {
                let kc = find_keycode(&s, keysym);
                if kc > 0 {
                    let _ = xtest::fake_input(&s.conn, X11_KEY_PRESS,
                        kc, 0, s.root, s.cursor_x.load(Ordering::Relaxed) as i16, s.cursor_y.load(Ordering::Relaxed) as i16, 0);
                    let _ = s.conn.flush();
                }
            }
        }
        "ku" if parts.len() >= 2 => {
            let keysym = code_to_keysym(parts[1]);
            if keysym != 0 {
                let kc = find_keycode(&s, keysym);
                if kc > 0 {
                    let _ = xtest::fake_input(&s.conn, X11_KEY_RELEASE,
                        kc, 0, s.root, s.cursor_x.load(Ordering::Relaxed) as i16, s.cursor_y.load(Ordering::Relaxed) as i16, 0);
                    let _ = s.conn.flush();
                }
            }
        }
        _ => {}
    }
}

fn send_cursor_position(out_tx: &mpsc::Sender<Message>, state: &Arc<Mutex<AppState>>) {
    let s = state.lock().unwrap();
    let x = s.cursor_x.load(Ordering::Relaxed);
    let y = s.cursor_y.load(Ordering::Relaxed);
    drop(s);
    let msg = serde_json::to_string(&serde_json::json!({
        "type": "cursor",
        "x": x,
        "y": y
    })).unwrap();
    let _ = out_tx.try_send(Message::Text(msg.into()));
}

fn find_keycode(s: &AppState, keysym: u32) -> u8 {
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

/// Detect local IP address by creating a UDP socket and querying its local address.
/// Connects to Google DNS (8.8.8.8:80) to determine the routing interface,
/// without actually sending any data.
fn get_local_ip() -> String {
    let fallback = "127.0.0.1".to_string();
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                return addr.ip().to_string();
            }
        }
    }
    fallback
}
