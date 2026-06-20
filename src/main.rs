#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
use std::sync::atomic::{AtomicI32, Ordering};
use anyhow::{Context, Result};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use clap::Parser;
use glib;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_sdp::SDPMessage;
use gstreamer_webrtc::{WebRTCSDPType, WebRTCSessionDescription};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use futures_util::StreamExt;
use std::os::unix::net::UnixStream;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self};
use x11rb::protocol::xtest;
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb_protocol::xauth::get_auth;

/// Shared state passed via axum State to every WebSocket handler.
/// Parsed once at startup, avoiding repeated work per connection.
#[derive(Clone)]
struct ServerState {
    args: Args,
    pulseaudio_available: bool,
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
    about = "Lightweight X11 WebRTC streaming server",
    long_about = "\
vnrit streams an X11 display to one or more browsers over WebRTC.

  1. Start the server:   vnrit --display :1
  2. Open the URL in a browser (printed on startup, default http://0.0.0.0:8080)
  3. Click to send keyboard/mouse events back to the X11 display.

The frontend supports touch-to-mouse translation (one-finger move,
two-finger scroll, tap = left click, long-press = right click).
",
    after_help = "\
══════════════════════════════════════════════════════════════════
                      D E T A I L E D   G U I D E
══════════════════════════════════════════════════════════════════

─── CODEC COMPARISON ────────────────────────────────────────────

  openh264 (default)   Cisco open-source H.264/AVC encoder.
                       Good balance of quality, speed and memory.
                       Uses constrained-baseline profile.

  h264                 Android MediaCodec hardware H.264 encoder
                       (via NDK AMediaCodec). Leverages the GPU
                       encoder (Adreno on Snapdragon) for lower CPU
                       usage and potentially better latency.
                       Use:  --codec h264

  vp8 / vp9            libvpx VP8/VP9 encoders.
                       Higher quality per bitrate but more memory
                       and CPU overhead than H.264 options on ARM.

  Measured memory (720p 500kbps, client connected):
    openh264  ~50 MB RSS
    h264      ~48 MB RSS   (hardware encoder)
    vp8       ~64 MB RSS

─── RECOMMENDED COMMAND ─────────────────────────────────────────

  vnrit --codec h264 --height 720 --bitrate 500

  This gives the best balance on Snapdragon 835:
    • Hardware H.264 encoding (lowest CPU + memory)
    • 720p downscale (good clarity, low bandwidth)
    • 500 kbps bitrate (smooth GUI at ~3 MB/min)

─── BITRATE RECOMMENDATIONS ─────────────────────────────────────

  720p @ 24 fps with recommended codec (openh264 / h264):

    300 kbps    Low quality, usable for text terminals
    500 kbps    Good quality for GUI desktops (recommended)
    1000 kbps   High quality, default setting
    2000+ kbps  Near-lossless on static content

  Higher framerates (--framerate 30/60) may require higher bitrate.

─── STREAM SCALING ──────────────────────────────────────────────

  By default vnrit streams at the desktop's native resolution
  (e.g. 1920×1080). Use --height to downscale on the server side:

    vnrit --height 720       # stream at 720p (maintains aspect ratio)
    vnrit --height 480       # stream at 480p  (low bandwidth)

  Scaling reduces bandwidth AND encoding CPU, which is valuable
  on ARM devices. Uses videoscale + capsfilter in the pipeline.

─── AUDIO ────────────────────────────────────────────────────────

  vnrit detects PulseAudio at startup and, if available, adds an
  audio pipeline: pulsesrc → audio/x-raw → opusenc → rtpopuspay
  → webrtcbin. The browser receives stereo Opus audio alongside
  the video stream.

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

  # Stream display :1 on port 8080 with defaults (openh264, 1Mbps)
  vnrit

  # Custom display and port
  vnrit --display :0 -p 9090

  # Hardware H.264 encoding, 720p stream, 500 kbps
  vnrit --codec h264 --height 720 --bitrate 500

  # VP9 codec, 30 fps, low bandwidth
  vnrit --codec vp9 --framerate 30 --bitrate 300

  # Full quality, no scaling, 2 Mbps
  vnrit --bitrate 2000

─── NOTES ───────────────────────────────────────────────────────

  - vnrit requires a running X11 server (Xvnc, Xvfb, or real X).
  - On Termux, it connects via the Unix socket at
    /data/data/com.termux/files/usr/tmp/.X11-unix/X<display>.
  - Audio requires PulseAudio running on the system.
  - Each browser tab creates a separate WebRTC connection: the
    pipeline is rebuilt per-client (no multi-viewer sharing yet).
  - Connect from multiple browsers simultaneously for multi-viewer.
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
        long_help = "TCP port for the HTTP server that serves the frontend page \
and the WebSocket endpoint (/ws). Both are on the same port."
    )]
    port: u16,

    #[arg(
        long,
        default_value = "openh264",
        help = "Video codec to use",
        long_help = "\
Video encoder codec. Supported values:

  openh264  Cisco H.264/AVC encoder (default).
            Best all-rounder on ARM: ~50 MB RSS with client.

  h264      Android MediaCodec hardware H.264 encoder.
            Uses the GPU's hardware video encoder block (e.g. Adreno 540).
            Slightly lower memory (~48 MB) and CPU usage than openh264.

  vp8       libvpx VP8 encoder. Higher memory (~64 MB).

  vp9       libvpx VP9 encoder. Higher memory, better compression.

All codecs use 24 fps by default and output via RTP to WebRTC."
    )]
    codec: String,

    #[arg(
        long,
        default_value = "24",
        help = "Capture framerate in fps",
        long_help = "Frames per second for X11 screen capture and encoding. \
Higher values (30, 60) give smoother motion but increase CPU and bandwidth. \
Lower values (10, 15) save bandwidth and CPU for mostly-static desktops."
    )]
    framerate: i32,

#[arg(
    long,
    default_value = "stun://stun.cloudflare.com:3478",
    help = "STUN server URL (set empty string to disable)"
)]
stun: String,

    #[arg(
        long,
        default_value = "1000",
        help = "Target bitrate in kbps",
        long_help = "Video encoder target bitrate in kilobits per second. \
At 720p 24 fps: 300=low, 500=good, 1000=high(default), 2000+=near-lossless."
    )]
    bitrate: i32,

    #[arg(
        long,
        default_value = "0",
        help = "Downscale stream height in pixels (0 = no scaling)",
        long_help = "\
If non-zero, the video stream is scaled down to the given height while \
maintaining aspect ratio. This reduces bandwidth and encoding CPU usage.

Examples: --height 720  produces a 720p stream
          --height 480  produces a 480p stream
          --height 0    uses the desktop's native resolution (default)

Uses GStreamer videoscale + capsfilter in the encoding pipeline."
    )]
    height: i32,
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
    conn: x11rb::rust_connection::RustConnection,
    root: xproto::Window,
    cursor_x: AtomicI32,
    cursor_y: AtomicI32,
    // Cached keyboard mapping (fetched once on start) to convert keysym → keycode
    keysyms: Vec<u32>,
    keysyms_per_keycode: u8,
    first_keycode: u8,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    gst::init().context("Failed to initialize GStreamer")?;

    // Check PulseAudio availability once at startup, not per-connection
    let pulseaudio_available = std::process::Command::new("pactl")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let state = ServerState { args, pulseaudio_available };

    let addr = format!("0.0.0.0:{}", state.args.port);
    println!("vnrit listening on http://{}", addr);
    println!("  Display: {}", state.args.display);
    println!("  Codec  : {}", state.args.codec);
    println!("  FPS    : {}", state.args.framerate);
    println!("  Bitrate: {} kbps", state.args.bitrate);
    if state.args.height > 0 {
        println!("  Scale  : {}p", state.args.height);
    } else {
        println!("  Scale  : native (no scaling)");
    }
    println!("  PulseAudio: {}", if state.pulseaudio_available { "yes" } else { "no" });

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/ws", get(ws_handler))
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

async fn handle_ws(ws: WebSocket, state: ServerState) {
    eprintln!("[ws] client connected");

    // ── Spawn a dedicated I/O task that owns the WebSocket ──
    // We use channels to communicate with it, avoiding mutex contention
    // between GStreamer callbacks (send) and the main loop (recv).
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(256);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Message, axum::Error>>(256);

    tokio::spawn(async move {
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
                            if in_tx.send(Ok(msg)).await.is_err() {
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

    // Wait for 'ready' message from browser
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

    eprintln!("[ws] ready received, creating pipeline...");

    let args = &state.args;
    let pa_available = state.pulseaudio_available;

    // ── Direct X11 connection (x11rb) — no xdotool process overhead ──
    // Every mouse move, click, scroll, and key event is sent straight to the
    // X server via XTest extension. No pipe, no string formatting, no IPC.
    eprintln!("[x11] connecting to display {}", args.display);

    // Try standard connection first (/tmp/.X11-unix/X{display}). On Termux,
    // /tmp may not exist — fall back to the actual Termux socket path.
    let (x11_conn, screen_num) = match RustConnection::connect(Some(&args.display)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[x11] standard connect failed: {}, trying Termux socket path...", e);
            let display_num: u16 = match args.display.trim_start_matches(':').split('.').next()
                .and_then(|s| s.parse().ok())
            {
                Some(n) => n,
                None => {
                    eprintln!("[x11] ERROR: invalid display '{}'", args.display);
                    return;
                }
            };
            let sock = format!("/data/data/com.termux/files/usr/tmp/.X11-unix/X{}", display_num);
            eprintln!("[x11] connecting to {}", sock);
            let unix_stream = match UnixStream::connect(&sock) {
                Ok(s) => s,
                Err(e2) => {
                    eprintln!("[x11] ERROR: cannot connect to {}: {}", sock, e2);
                    return;
                }
            };
            let (stream, (family, address)) = match DefaultStream::from_unix_stream(unix_stream) {
                Ok(v) => v,
                Err(e2) => {
                    eprintln!("[x11] ERROR: from_unix_stream: {}", e2);
                    return;
                }
            };
            let (auth_name, auth_data) = get_auth(family, &address, display_num)
                .unwrap_or(None)
                .unwrap_or_else(|| (Vec::new(), Vec::new()));
            match RustConnection::connect_to_stream_with_auth_info(stream, 0, auth_name, auth_data) {
                Ok(conn) => {
                    eprintln!("[x11] connected via Termux socket path");
                    (conn, 0usize)
                }
                Err(e2) => {
                    eprintln!("[x11] ERROR: connect_to_stream failed: {}", e2);
                    return;
                }
            }
        }
    };
    let screen = &x11_conn.setup().roots[screen_num];
    let root = screen.root;
    // Verify XTest extension is available
    let xtest_cookie = match xtest::get_version(&x11_conn, 2, 2) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[x11] ERROR: XTest extension not available: {}", e);
            return;
        }
    };
    if let Err(e) = xtest_cookie.reply() {
        eprintln!("[x11] ERROR: XTest query failed: {}", e);
        return;
    }
    // Get current pointer position for relative-move tracking
    let ptr = match xproto::query_pointer(&x11_conn, root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[x11] ERROR: query_pointer failed: {}", e);
            return;
        }
    };
    let ptr = match ptr.reply() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[x11] ERROR: query_pointer reply failed: {}", e);
            return;
        }
    };
    // Cache keyboard mapping for keysym → keycode conversion
    let setup = x11_conn.setup();
    let first_keycode = setup.min_keycode;
    let keycode_count = setup.max_keycode - setup.min_keycode + 1;
    let kbd = match xproto::get_keyboard_mapping(&x11_conn, first_keycode, keycode_count) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[x11] ERROR: get_keyboard_mapping failed: {}", e);
            return;
        }
    };
    let kbd = match kbd.reply() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[x11] ERROR: get_keyboard_mapping reply failed: {}", e);
            return;
        }
    };
    eprintln!("[x11] connected, root=0x{:x}, pointer=({},{}), keycodes={}-{}",
        root, ptr.root_x, ptr.root_y, first_keycode, setup.max_keycode);
    let state = Arc::new(Mutex::new(AppState {
        conn: x11_conn,
        root,
        cursor_x: AtomicI32::new(ptr.root_x as i32),
        cursor_y: AtomicI32::new(ptr.root_y as i32),
        keysyms: kbd.keysyms.to_vec(),
        keysyms_per_keycode: kbd.keysyms_per_keycode,
        first_keycode,
    }));

    let pipeline = gst::Pipeline::new();

    // ── webrtcbin ──
    let webrtcbin = gst::ElementFactory::make("webrtcbin")
        .name("webrtcbin")
        .build()
        .expect("failed to create webrtcbin");
    if !args.stun.is_empty() {
       eprintln!("[config] STUN server: {}", args.stun);
       webrtcbin.set_property_from_str("stun-server", &args.stun);
}else {
    eprintln!("[config] STUN disabled (using host candidates only)");
}
// 如果 args.stun 为空字符串，则不设置 stun-server，webrtcbin 将仅使用 host 候选
    pipeline.add(&webrtcbin).unwrap();

    // ── ximagesrc → videoconvert → encoder → payloader ──
    let ximagesrc = gst::ElementFactory::make("ximagesrc")
        .name("ximagesrc")
        .build()
        .unwrap();
    ximagesrc.set_property("display-name", &format!("{}", args.display));
    ximagesrc.set_property("use-damage", true);
    ximagesrc.set_property("show-pointer", false);

    let videoconvert = gst::ElementFactory::make("videoconvert").name("videoconvert").build().unwrap();
    let q1 = gst::ElementFactory::make("queue").name("vqueue").build().unwrap();
    // Minimize queue buffering to reduce latency: no time-based limit, max 1 buffer
    q1.set_property("max-size-time", 0u64);
    q1.set_property("max-size-buffers", 1u32);
    q1.set_property_from_str("leaky", "downstream"); // drop old frames on backlog

    let capsf = gst::ElementFactory::make("capsfilter").name("capsf").build().unwrap();
    let caps = gst::Caps::builder("video/x-raw")
        .field("framerate", gst::Fraction::new(args.framerate, 1))
        .build();
    capsf.set_property("caps", &caps);

    let encoder: gst::Element = match args.codec.as_str() {
        "vp8" => {
            let e = gst::ElementFactory::make("vp8enc").name("encoder").build().unwrap();
            e.set_property("target-bitrate", args.bitrate * 1000);
            e.set_property("deadline", 1i64);
            e.set_property("keyframe-max-dist", 240i32);
            e.set_property("min-force-key-unit-interval", 3_000_000_000u64);
            e
        }
        "vp9" => {
            let e = gst::ElementFactory::make("vp9enc").name("encoder").build().unwrap();
            e.set_property("target-bitrate", args.bitrate * 1000);
            e.set_property("deadline", 1i64);
            e.set_property("keyframe-max-dist", 240i32);
            e.set_property("min-force-key-unit-interval", 3_000_000_000u64);
            e
        }
        "h264" => {
            let e = gst::ElementFactory::make("mcenc").name("encoder").build().unwrap();
            e.set_property("bitrate", args.bitrate);
            e
        }
        _ => {
            let e = gst::ElementFactory::make("openh264enc").name("encoder").build().unwrap();
            e.set_property("bitrate", (args.bitrate * 1000) as u32);
            e.set_property_from_str("usage-type", "screen");
            e.set_property("gop-size", 240u32);
            e
        }
    };

    let pay_name = match args.codec.as_str() {
        "vp8" => "rtpvp8pay",
        "vp9" => "rtpvp9pay",
        _ => "rtph264pay",
    };
    let payloader = gst::ElementFactory::make(pay_name).name("payloader").build().unwrap();
	
    // ── 仅当使用 H.264 编码器时，每个 RTP 包都带 SPS/PPS ──
    if args.codec == "openh264" || args.codec == "h264" {
    payloader.set_property("config-interval", -1);
    }
    // ── Optional stream downscaling (--height, e.g. 720 for 720p) ──
    // Fewer pixels encoded = less CPU + less bandwidth, especially valuable on ARM.
    if args.height > 0 {
        let vs = gst::ElementFactory::make("videoscale").name("videoscale").build().unwrap();
        let sc = gst::ElementFactory::make("capsfilter").name("scale_capsf").build().unwrap();
        sc.set_property("caps", &gst::Caps::builder("video/x-raw")
            .field("height", args.height)
            .field("framerate", gst::Fraction::new(args.framerate, 1))
            .build());
        let ve = vec![&ximagesrc, &videoconvert, &q1, &capsf, &vs, &sc, &encoder, &payloader];
        pipeline.add_many(&ve).unwrap();
        gst::Element::link_many(&ve).unwrap();
    } else {
        let ve = vec![&ximagesrc, &videoconvert, &q1, &capsf, &encoder, &payloader];
        pipeline.add_many(&ve).unwrap();
        gst::Element::link_many(&ve).unwrap();
    }

    let vpad = webrtcbin.request_pad_simple("sink_%u").unwrap();
    payloader.static_pad("src").unwrap().link(&vpad).unwrap();

    // ── audio pipeline (optional — skip if PulseAudio not available) ──
    if pa_available {
        eprintln!("[audio] PulseAudio detected, adding audio pipeline");
        let pulsesrc = gst::ElementFactory::make("pulsesrc").name("pulsesrc").build().unwrap();
        pulsesrc.set_property("client-name", "vnrit");
        let aq = gst::ElementFactory::make("queue").name("aqueue").build().unwrap();
        let audioconv = gst::ElementFactory::make("audioconvert").name("audioconvert").build().unwrap();
        let acapsf = gst::ElementFactory::make("capsfilter").name("acapsf").build().unwrap();
        let acaps = gst::Caps::builder("audio/x-raw")
            .field("channels", 1i32)
            .field("rate", 48000i32)
            .build();
        acapsf.set_property("caps", &acaps);
        let opusenc = gst::ElementFactory::make("opusenc").name("opusenc").build().unwrap();
        let rtpopus = gst::ElementFactory::make("rtpopuspay").name("rtpopus").build().unwrap();

        let aelements = &[&pulsesrc, &aq, &audioconv, &acapsf, &opusenc, &rtpopus];
        pipeline.add_many(aelements).unwrap();
        gst::Element::link_many(aelements).unwrap();

        let apad = webrtcbin.request_pad_simple("sink_%u").unwrap();
        rtpopus.static_pad("src").unwrap().link(&apad).unwrap();
    } else {
        eprintln!("[audio] PulseAudio not available, skipping audio pipeline");
    }

    // ── Signal handlers: connect before pipeline plays ──
    let ws_neg = out_tx.clone();
    let ws_ice = out_tx.clone();

    // on-negotiation-needed
    webrtcbin.connect_closure(
        "on-negotiation-needed",
        false,
        glib::closure!(|wb: gst::Element| {
            eprintln!("[webrtc] on-negotiation-needed fired");
            let ws = ws_neg.clone();
            let wb2 = wb.clone();

            // Note: input is now sent via WebSocket, not data channel.
            // No data channel needed — just video (and optionally audio) media tracks.

            // Promise for create-offer
            let promise = gst::Promise::with_change_func(move |result| {
                eprintln!("[webrtc] create-offer promise resolved");
                if let Ok(Some(reply)) = result {
                    if let Ok(offer) = reply.get::<gstreamer_webrtc::WebRTCSessionDescription>("offer") {
                        let sdp_text = offer.sdp().as_text().unwrap().to_string();
                        eprintln!("[webrtc] offer SDP created ({} bytes)", sdp_text.len());

                        // --- CRITICAL: Set local description to trigger ICE gathering ---
                        let ws2 = ws.clone();
                        let set_promise = gst::Promise::with_change_func(move |_| {
                            eprintln!("[webrtc] local description set, ICE gathering should start now");
                            // Now send the offer to the browser (ICE candidates will follow)
                            let msg = serde_json::to_string(&SignalingMessage::Offer {
                                sdp: sdp_text,
                            })
                            .unwrap();
                            eprintln!("[webrtc] sending offer via WS");
                            let _ = ws2.try_send(Message::Text(msg.into()));
                            eprintln!("[webrtc] offer sent (queued)");
                        });
                        let _ = wb2.emit_by_name::<()>("set-local-description", &[&offer, &set_promise]);
                    } else {
                        eprintln!("[webrtc] reply.get('offer') failed");
                    }
                } else {
                    eprintln!("[webrtc] promise result: {:?}", result);
                }
            });
            let opts = gst::Structure::new_empty("options");
            wb.emit_by_name::<()>("create-offer", &[&opts, &promise]);
        }),
    );

    // on-ice-candidate
    webrtcbin.connect_closure(
        "on-ice-candidate",
        false,
        glib::closure!(|_: gst::Element, mline: u32, cand: String| {
            eprintln!("[webrtc] ICE candidate: mline={} candidate='{}'", mline, if cand.len() > 30 { &cand[..30] } else { &cand });
            let ws = ws_ice.clone();
            let msg = serde_json::to_string(&SignalingMessage::Ice {
                candidate: cand,
                sdp_mline_index: mline,
            })
            .unwrap();
            let _ = ws.try_send(Message::Text(msg.into()));
        }),
    );

    // Start
    pipeline.set_state(gst::State::Playing).unwrap();
    eprintln!("[ws] pipeline playing, waiting for answer...");

    // Push initial cursor position to browser (no polling needed — position is
    // pushed after every input event instead)
    send_cursor_position(&out_tx, &state);

    // ── ICE state logging via signal (no polling — fires once per state change) ──
    // We use notify::ice-connection-state so we only print when the state actually
    // changes, and a one-shot state-read after 2s for the initial state.
    {
        let wb = webrtcbin.clone();
        webrtcbin.connect_closure(
            "notify::ice-connection-state",
            false,
            glib::closure!(|_: gst::Element, _pspec: glib::ParamSpec| {
                let cs: gstreamer_webrtc::WebRTCICEConnectionState =
                    wb.property("ice-connection-state");
                let gs: gstreamer_webrtc::WebRTCICEGatheringState =
                    wb.property("ice-gathering-state");
                let ss: gstreamer_webrtc::WebRTCSignalingState =
                    wb.property("signaling-state");
                eprintln!("[ice] connection={:?} gathering={:?} signaling={:?}",
                    cs, gs, ss);
            }),
        );
        // Also read one-shot after initial delay so the browser gets a known starting state
        let wb_init = webrtcbin.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let cs: gstreamer_webrtc::WebRTCICEConnectionState =
                wb_init.property("ice-connection-state");
            let gs: gstreamer_webrtc::WebRTCICEGatheringState =
                wb_init.property("ice-gathering-state");
            let ss: gstreamer_webrtc::WebRTCSignalingState =
                wb_init.property("signaling-state");
            eprintln!("[ice] connection={:?} gathering={:?} signaling={:?}",
                cs, gs, ss);
        });
    }

    // ── Read messages (signaling + input via WebSocket) ──
    // Input was previously sent via WebRTC data channel (SCTP over DTLS).
    // Now it's sent directly over the WebSocket — much lower overhead on localhost.
    loop {
        let msg = in_rx.recv().await;

        match msg {
            Some(Ok(Message::Text(t))) => {
                // Try signaling first (answer / ICE)
                if let Ok(sig) = serde_json::from_str::<SignalingMessage>(&t) {
                    match sig {
                        SignalingMessage::Answer { sdp } => {
                            eprintln!("[ws] got answer ({} bytes SDP)", sdp.len());
                            if let Ok(sdp_msg) = SDPMessage::parse_buffer(sdp.as_bytes()) {
                                let answer =
                                    WebRTCSessionDescription::new(WebRTCSDPType::Answer, sdp_msg);
                                let set_promise = gst::Promise::new();
                                let _ = webrtcbin
                                    .emit_by_name::<()>("set-remote-description", &[&answer, &set_promise]);
                                eprintln!("[ws] answer set, streaming!");
                            }
                        }
                        SignalingMessage::Ice { candidate, sdp_mline_index } => {
                            eprintln!("[ws] got ICE from client: mline={} candidate='{}'", sdp_mline_index, if candidate.len() > 40 { &candidate[..40] } else { &candidate });
                            let (idx, cand): (&dyn glib::prelude::ToValue, &dyn glib::prelude::ToValue) =
                                (&sdp_mline_index, &candidate);
                            let args: [&dyn glib::prelude::ToValue; 2] = [idx, cand];
                            let _ = webrtcbin.emit_by_name::<()>("add-ice-candidate", &args);
                        }
                        _ => {
                            eprintln!("[ws] unexpected sig variant");
                        }
                    }
                } else {
                    // Not signaling → input message (mousemove_rel, mousedown, keydown, etc.)
                    handle_input_message(&t, &state);
                    // Push current cursor position to browser after every input.
                    // No polling needed — the browser tracks locally between updates,
                    // and this push corrects any drift (edge-clamping, lost messages, etc.).
                    send_cursor_position(&out_tx, &state);
                }
            }
            Some(Ok(Message::Close(_))) | None => break,
            _ => {}
        }
    }

    let _ = pipeline.set_state(gst::State::Null);
    eprintln!("[ws] client disconnected");
}

fn handle_input_message(raw: &str, state: &Arc<Mutex<AppState>>) {
    let parts: Vec<&str> = raw.split(',').collect();
    if parts.is_empty() {
        return;
    }

    // ── Direct X11 via XTest extension (no xdotool, no pipe, no IPC) ──
    let s = state.lock().unwrap();

    match parts[0] {
        "mr" if parts.len() >= 3 => {
            // mr,dx,dy — relative mouse move: update internal cursor, send absolute
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
            let btn = if delta > 0.0 { 5 } else { 4 };
            let _ = xtest::fake_input(&s.conn, X11_BUTTON_PRESS,
                btn, 0, s.root, s.cursor_x.load(Ordering::Relaxed) as i16, s.cursor_y.load(Ordering::Relaxed) as i16, 0);
            let _ = xtest::fake_input(&s.conn, X11_BUTTON_RELEASE,
                btn, 0, s.root, s.cursor_x.load(Ordering::Relaxed) as i16, s.cursor_y.load(Ordering::Relaxed) as i16, 0);
            let _ = s.conn.flush();
        }
        "kd" if parts.len() >= 2 => {
            // kd,code — key down (convert browser .code → X11 keycode via keysym)
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

/// Send current cursor position to browser via WebSocket.
/// Called after every input message and once on initial connection.
/// This replaces polling — the cursor position is pushed on every state change,
/// so the browser's overlay always stays in sync between local event updates.
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

/// Look up X11 keycode for a given keysym using cached keyboard mapping.
fn find_keycode(s: &AppState, keysym: u32) -> u8 {
    let kpk = s.keysyms_per_keycode as usize;
    for (i, chunk) in s.keysyms.chunks(kpk).enumerate() {
        for &ks in chunk {
            if ks == keysym {
                return s.first_keycode + i as u8;
            }
        }
    }
    0
}

/// Convert browser KeyboardEvent.code to X11 keysym (u32).
/// Returns 0 for unknown codes (event is silently dropped).
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
        "ShiftLeft" | "ShiftRight" => 0xffe1, // XK_Shift_L
        "ControlLeft" | "ControlRight" => 0xffe3, // XK_Control_L
        "AltLeft" | "AltRight" => 0xffe9, // XK_Alt_L
        "MetaLeft" | "MetaRight" => 0xffeb, // XK_Super_L
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
        "Comma" => 0x002c,
        "Period" => 0x002e,
        "Slash" => 0x002f,
        "IntlBackslash" => 0x005c,
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
            // "Digit5" → '5' → 0x0035 = XK_5
            code.as_bytes()[5] as u32
        }
        _ => {
            // "KeyA"-"KeyZ" → uppercase letter → XK_A-XK_Z
            if let Some(c) = code.strip_prefix("Key") {
                if c.len() == 1 {
                    let b = c.as_bytes()[0];
                    if b.is_ascii_alphabetic() {
                        return b as u32; // 'A' = 0x41 = XK_A
                    }
                }
            }
            0 // unknown
        }
    }
}
