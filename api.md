# webrtc-rs 0.20.0-rc.1 API 参考

## 项目结构

```
webrtc = "0.20.0-rc.1"      # Meta-crate (Tokio-SansIO 混合层)
│                             #  PeerConnection trait 提供 async 接口
│                             #  内部使用 rtc Sans-I/O 核心 + tokio I/O
├── rtc (0.20.0-rc.1)        # Sans-I/O 核心 (纯协议逻辑, 不绑定 I/O)
│   ├── peer_connection       #   连接管理 (不含网络 I/O)
│   ├── statistics            #   统计信息 (RTCStatsReport)
│   ├── data_channel          #   数据通道
│   ├── media_stream          #   媒体流
│   ├── rtp_transceiver       #   RTP 收发器
│   └── interceptor           #   拦截器 (NACK/TWCC/报告)
├── rtc-media                 # Sample 等媒体类型
├── rtc-interceptor           # 拦截器实现 (NACK responder, TWCC sender)
└── rtc-shared                # SystemInstant, Error 等共享类型
```

## 模块路径速查

| 类型 | 导入路径 |
|------|---------|
| `PeerConnection` (trait) | `webrtc::peer_connection::PeerConnection` |
| `PeerConnectionBuilder` | `webrtc::peer_connection::PeerConnectionBuilder` |
| `PeerConnectionEventHandler` (trait) | `webrtc::peer_connection::PeerConnectionEventHandler` |
| `TrackLocalStaticSample` | `webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample` |
| `TrackLocal` (trait) | `webrtc::media_stream::track_local::TrackLocal` |
| `RTCSessionDescription` | `webrtc::peer_connection::RTCSessionDescription` |
| `RTCSdpType` | `webrtc::peer_connection::RTCSdpType` |
| `RTCIceCandidateInit` | `webrtc::peer_connection::RTCIceCandidateInit` |
| `RTCIceCandidate` | `webrtc::peer_connection::RTCIceCandidate` |
| `RTCIceGatheringState` | `webrtc::peer_connection::RTCIceGatheringState` |
| `RTCPeerConnectionState` | `webrtc::peer_connection::RTCPeerConnectionState` |
| `RTCPeerConnectionIceEvent` | `webrtc::peer_connection::RTCPeerConnectionIceEvent` |
| `RTCConfigurationBuilder` | `webrtc::peer_connection::RTCConfigurationBuilder` |
| `RTCIceServer` | `webrtc::peer_connection::RTCIceServer` |
| `MediaEngine` | `webrtc::peer_connection::MediaEngine` |
| `MIME_TYPE_H264` | `rtc::peer_connection::configuration::media_engine::MIME_TYPE_H264` |
| `MediaStreamTrack` | `rtc::media_stream::MediaStreamTrack` |
| `Sample` | `rtc::media::Sample` |
| `Registry` | `rtc::interceptor::Registry` |
| `register_default_interceptors` | `rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors` |
| `RTCRtpCodec` | `rtc::rtp_transceiver::rtp_sender::RTCRtpCodec` |
| `RTCRtpCodecParameters` | `rtc::rtp_transceiver::rtp_sender::RTCRtpCodecParameters` |
| `RTCRtpEncodingParameters` | `rtc::rtp_transceiver::rtp_sender::RTCRtpEncodingParameters` |
| `RTCRtpCodingParameters` | `rtc::rtp_transceiver::rtp_sender::RTCRtpCodingParameters` |
| `RtpCodecKind` | `rtc::rtp_transceiver::rtp_sender::RtpCodecKind` |
| `PayloadType` (= `u8`) | `rtc::rtp_transceiver::rtp_sender::PayloadType` |
| `runtime::{Sender, Receiver, channel, interval, default_runtime}` | `webrtc::runtime` |

---

## 1. `PeerConnection` Trait

**文件**: `webrtc-0.20.0-rc.1/src/peer_connection/mod.rs:260`

```rust
pub trait PeerConnection: Send + Sync + 'static {
    async fn close(&self) -> Result<()>;

    async fn create_offer(&self, options: Option<RTCOfferOptions>) -> Result<RTCSessionDescription>;
    async fn create_answer(&self, options: Option<RTCAnswerOptions>) -> Result<RTCSessionDescription>;

    async fn set_local_description(&self, desc: RTCSessionDescription) -> Result<()>;
    async fn local_description(&self) -> Option<RTCSessionDescription>;
    async fn current_local_description(&self) -> Option<RTCSessionDescription>;
    async fn pending_local_description(&self) -> Option<RTCSessionDescription>;

    async fn set_remote_description(&self, desc: RTCSessionDescription) -> Result<()>;
    async fn remote_description(&self) -> Option<RTCSessionDescription>;
    async fn current_remote_description(&self) -> Option<RTCSessionDescription>;
    async fn pending_remote_description(&self) -> Option<RTCSessionDescription>;

    async fn add_ice_candidate(&self, candidate: RTCIceCandidateInit) -> Result<()>;

    async fn add_track(&self, track: Arc<dyn TrackLocal>) -> Result<Arc<dyn RtpSender>>;
    async fn remove_track(&self, sender: &Arc<dyn RtpSender>) -> Result<()>;

    // DataChannels
    async fn create_data_channel(&self, label: &str, options: Option<RTCDataChannelInit>) -> Arc<dyn DataChannel>;

    // 其他
    async fn get_configuration(&self) -> RTCConfiguration;
    async fn set_configuration(&self, configuration: RTCConfiguration) -> Result<()>;
    async fn get_stats(&self) -> Result<Stats>;
    // ...
}
```

### 典型使用流程 (Offer/Answer)

```rust
// 接收浏览器 offer → 构造 RTCSessionDescription
let offer = RTCSessionDescription::offer(offer_sdp)?;
// 或直接构造:
let offer = RTCSessionDescription {
    sdp_type: RTCSdpType::Offer,
    sdp: offer_sdp,
};

// 设置远端描述
pc.set_remote_description(offer).await?;

// 创建应答
let answer = pc.create_answer(None).await?;

// 设置本地描述
pc.set_local_description(answer).await?;

// 获取本地 SDP (发送给浏览器)
let local = pc.local_description().await.unwrap();
```

---

## 2. `PeerConnectionBuilder`

**文件**: `webrtc-0.20.0-rc.1/src/peer_connection/mod.rs:115`

```rust
impl PeerConnectionBuilder {
    pub fn new() -> Self;

    pub fn with_configuration(mut self, configuration: RTCConfiguration) -> Self;
    pub fn with_media_engine(mut self, media_engine: MediaEngine) -> Self;
    pub fn with_setting_engine(mut self, setting_engine: SettingEngine) -> Self;
    pub fn with_interceptor_registry<P>(self, registry: Registry<P>) -> PeerConnectionBuilder<A, P>;
    pub fn with_runtime(mut self, runtime: Arc<dyn Runtime>) -> Self;
    pub fn with_handler(mut self, handler: Arc<dyn PeerConnectionEventHandler>) -> Self;
    pub fn with_udp_addrs(mut self, udp_addrs: Vec<A>) -> Self;   // A: ToSocketAddrs
    pub fn with_tcp_addrs(mut self, tcp_addrs: Vec<A>) -> Self;   // A: ToSocketAddrs
    pub async fn build(self) -> Result<impl PeerConnection>;
}
```

### UDP 与 TCP

- **`with_udp_addrs`**: 指定 ICE UDP 候选地址。格式 `"ip:port"`，port=0 表示系统分配。
  当前项目使用 `vec![format!("{}:0", get_local_ip())]`。
- **`with_tcp_addrs`**: 指定 ICE TCP 候选地址（可选）。通常用于防火墙限制 UDP 的环境。
  TCP 候选会增加连接延迟，但能穿透只允许 TCP 出站的网络。
  不加则只使用 UDP。`"ip:port"` 格式同上。

### 完整创建示例

```rust
use rtc::interceptor::Registry;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
};
use webrtc::runtime::{self, default_runtime};

let mut media_engine = MediaEngine::default();
// ... 注册 codec ...
let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

let config = RTCConfigurationBuilder::new()
    .with_ice_servers(vec![RTCIceServer {
        urls: vec!["stun:stun.l.google.com:19302".to_string()],
        ..Default::default()
    }])
    .build();

let rt = default_runtime().ok_or_else(|| anyhow!("no runtime found"))?;
let handler = Arc::new(MyHandler { /* ... */ });

let pc = PeerConnectionBuilder::new()
    .with_configuration(config)
    .with_media_engine(media_engine)
    .with_interceptor_registry(registry)
    .with_handler(handler)
    .with_runtime(rt)
    .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
    .build()
    .await?;
```

---

## 3. `PeerConnectionEventHandler` Trait

**文件**: `webrtc-0.20.0-rc.1/src/peer_connection/mod.rs:86`

所有方法都有默认空实现, 只需要覆盖需要的。

```rust
#[async_trait::async_trait]
pub trait PeerConnectionEventHandler: Send + Sync + 'static {
    async fn on_negotiation_needed(&self) {}
    async fn on_ice_candidate(&self, _event: RTCPeerConnectionIceEvent) {}
    async fn on_ice_candidate_error(&self, _event: RTCPeerConnectionIceErrorEvent) {}
    async fn on_signaling_state_change(&self, _state: RTCSignalingState) {}
    async fn on_ice_connection_state_change(&self, _state: RTCIceConnectionState) {}
    async fn on_ice_gathering_state_change(&self, _state: RTCIceGatheringState) {}
    async fn on_connection_state_change(&self, _state: RTCPeerConnectionState) {}
    async fn on_data_channel(&self, _data_channel: Arc<dyn DataChannel>) {}
    async fn on_track(&self, _track: Arc<dyn TrackRemote>) {}
}
```

### 典型实现

```rust
#[derive(Clone)]
struct Handler {
    gather_complete_tx: runtime::Sender<()>,
    done_tx: runtime::Sender<()>,
    connected_tx: runtime::Sender<()>,
    ice_tx: mpsc::Sender<String>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            let msg = serde_json::to_string(&SignalingMessage::Ice {
                candidate: init.candidate,
                sdp_mline_index: init.sdp_mline_index.unwrap_or(0) as u32,
            }).unwrap();
            let _ = self.ice_tx.try_send(msg);
        }
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gather_complete_tx.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
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
}
```

---

## 4. `TrackLocalStaticSample`

**文件**: `webrtc-0.20.0-rc.1/src/media_stream/track_local/static_sample.rs:22`

```rust
pub struct TrackLocalStaticSample { /* private */ }

impl TrackLocalStaticSample {
    /// 创建静态采样轨道
    pub fn new(track: MediaStreamTrack) -> Result<Self>;

    /// 获取 SSRC 列表
    pub async fn ssrcs(&self) -> Vec<SSRC>;

    /// 获取 SampleWriter (推荐使用)
    pub fn sample_writer(&self, ssrc: SSRC) -> SampleWriter;

    /// 直接写入采样 (需要 extensions 参数)
    pub async fn write_sample(
        &self,
        ssrc: SSRC,
        sample: &Sample,
        extensions: &[rtp::extension::HeaderExtension],
    ) -> Result<()>;
}

/// SampleWriter: 简化版的采样写入器
pub struct SampleWriter<'a> { /* private */ }

impl SampleWriter<'_> {
    pub async fn write_sample(self, sample: &Sample) -> Result<()>;
}
```

### 使用示例

```rust
// 创建轨道
let ssrc = rand::random::<u32>();
let track: Arc<TrackLocalStaticSample> = Arc::new(
    TrackLocalStaticSample::new(MediaStreamTrack::new(
        "stream-id".to_string(),
        "track-id".to_string(),
        "label".to_string(),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec: video_codec.rtp_codec.clone(),
            ..Default::default()
        }],
    ))?
);

// 添加到 PeerConnection
pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await?;

// 发送采样 (推荐使用 sample_writer)
let ssrc = *track.ssrcs().await.first().unwrap();
track.sample_writer(ssrc).write_sample(&Sample {
    data: Bytes::from(encoded_data),
    duration: Duration::from_millis(33), // ~30fps
    ..Default::default()
}).await?;
```

---

## 5. `MediaStreamTrack`

**文件**: `rtc-0.20.0-rc.1/src/media_stream/track.rs:209`

```rust
pub type MediaStreamId = String;       // rtc::media_stream::MediaStreamId
pub type MediaStreamTrackId = String;  // rtc::media_stream::track::MediaStreamTrackId

impl MediaStreamTrack {
    pub fn new(
        stream_id: MediaStreamId,       // → String
        track_id: MediaStreamTrackId,  // → String
        label: String,
        kind: RtpCodecKind,
        codings: Vec<RTCRtpEncodingParameters>,
    ) -> Self;
}
```

---

## 6. 编解码器配置类型

**文件**: `rtc-0.20.0-rc.1/src/rtp_transceiver/rtp_sender/`

```rust
/// 编解码器种类
pub enum RtpCodecKind {
    Unspecified = 0,
    Audio = 1,
    Video = 2,
}

/// PayloadType = u8
pub type PayloadType = u8;

/// RTP Codec 描述
pub struct RTCRtpCodec {
    pub mime_type: String,
    pub clock_rate: u32,
    pub channels: u16,
    pub sdp_fmtp_line: String,
    pub rtcp_feedback: Vec<RTCPFeedback>,
}

/// RTP Codec 参数 (包含 payload type)
pub struct RTCRtpCodecParameters {
    pub rtp_codec: RTCRtpCodec,
    pub payload_type: PayloadType,  // = u8
}

/// RTP 编码参数
pub struct RTCRtpCodingParameters {
    pub rid: RtpStreamId,
    pub ssrc: Option<SSRC>,          // = Option<u32>
    pub rtx: Option<RTCRtpRtxParameters>,
    pub fec: Option<RTCRtpFecParameters>,
}

/// RTP 编码参数 (完整)
pub struct RTCRtpEncodingParameters {
    pub rtp_coding_parameters: RTCRtpCodingParameters,
    pub active: bool,
    pub codec: RTCRtpCodec,       // ← 注意: 是 RTCRtpCodec, 不是 RTCRtpCodecParameters
    pub max_bitrate: u32,
    pub max_framerate: Option<f64>,
    pub scale_resolution_down_by: Option<f64>,
}
```

---

## 7. `Sample` 结构体

**文件**: `rtc-media-0.20.0-rc.1/src/lib.rs:14`

```rust
pub struct Sample {
    pub data: Bytes,                    // 编码后的媒体数据 (bitstream)
    pub timestamp: SystemInstant,       // 采样生成时的墙上时间
    pub duration: Duration,             // 采样时长
    pub packet_timestamp: u32,          // RTP 包时间戳
    pub prev_dropped_packets: u16,      // 丢弃包数
    pub prev_padding_packets: u16,      // 填充包数
}

impl Default for Sample {
    fn default() -> Self {
        Sample {
            data: Bytes::new(),
            timestamp: SystemInstant::now(),  // ← 默认使用当前时间
            duration: Duration::from_secs(0),
            packet_timestamp: 0,
            prev_dropped_packets: 0,
            prev_padding_packets: 0,
        }
    }
}
```

> **注意**: `SystemInstant` 来自 `shared::time::SystemInstant` (rtc-shared crate)。
> `Default::default()` 会自动设置 `timestamp: SystemInstant::now()`。

---

## 8. SDP 类型

### `RTCSessionDescription`

**文件**: `rtc-0.20.0-rc.1/src/peer_connection/sdp/session_description.rs:158`

```rust
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct RTCSessionDescription {
    #[serde(rename = "type")]
    pub sdp_type: RTCSdpType,
    pub sdp: String,
}

impl RTCSessionDescription {
    pub fn offer(sdp: String) -> Result<Self>;    // 构造 offer
    pub fn answer(sdp: String) -> Result<Self>;   // 构造 answer
    pub fn pranswer(sdp: String) -> Result<Self>; // 构造 provisional answer
    pub fn unmarshal(&self) -> Result<SessionDescription>; // 解析 SDP
}
```

### `RTCSdpType`

```rust
#[derive(Default, Debug, PartialEq, Eq, Copy, Clone, Serialize, Deserialize)]
pub enum RTCSdpType {
    #[default] Unspecified,
    #[serde(rename = "offer")]   Offer,
    #[serde(rename = "answer")]  Answer,
    #[serde(rename = "pranswer")] Pranswer,
    #[serde(rename = "rollback")] Rollback,
}
```

---

## 9. ICE 类型

### `RTCIceCandidate`

**文件**: `rtc-0.20.0-rc.1/src/peer_connection/transport/ice/candidate.rs:75`

```rust
pub struct RTCIceCandidate {
    pub id: String,
    pub foundation: String,
    pub priority: u32,
    pub address: String,
    pub protocol: RTCIceProtocol,
    pub port: u16,
    pub typ: RTCIceCandidateType,
    pub component: u16,
    pub related_address: String,
    pub related_port: u16,
    pub tcp_type: RTCIceTcpCandidateType,
    pub relay_protocol: RTCIceServerTransportProtocol,
    pub url: Option<String>,
}

impl RTCIceCandidate {
    /// 转为 RTCIceCandidateInit (适合序列化传输)
    pub fn to_json(&self) -> Result<RTCIceCandidateInit>;
}
```

### `RTCIceCandidateInit`

```rust
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RTCIceCandidateInit {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    #[serde(rename = "sdpMLineIndex")]
    pub sdp_mline_index: Option<u16>,
    pub username_fragment: Option<String>,
    pub url: Option<String>,
}
```

### `RTCPeerConnectionIceEvent`

**文件**: `rtc-0.20.0-rc.1/src/peer_connection/event/ice_event.rs:75`

```rust
pub struct RTCPeerConnectionIceEvent {
    pub candidate: RTCIceCandidate,  // ← 不是 Option! 始终有值
    pub url: String,                 // STUN/TURN URL
}
```

---

## 10. 状态枚举

### `RTCIceGatheringState`

```rust
pub enum RTCIceGatheringState {
    Unspecified,
    New,
    Gathering,
    Complete,
}
```

### `RTCPeerConnectionState`

```rust
pub enum RTCPeerConnectionState {
    Unspecified,
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}
```

---

## 11. `MediaEngine` 与 Codec 注册

**文件**: `rtc-0.20.0-rc.1/src/peer_connection/configuration/media_engine.rs:286`

```rust
pub struct MediaEngine { /* private fields */ }

impl MediaEngine {
    /// 注册默认编解码器 (Opus + H264 + VP8 + VP9 + G722 + PCMU + PCMA)
    pub fn register_default_codecs(&mut self) -> Result<()>;

    /// 注册单个编解码器
    pub fn register_codec(
        &mut self,
        codec: RTCRtpCodecParameters,
        typ: RtpCodecKind,
    ) -> Result<()>;
}
```

### 配置 H264 Codec 示例

```rust
let video_codec = RTCRtpCodecParameters {
    rtp_codec: RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),  // "video/H264"
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line:
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            .into(),
        rtcp_feedback: vec![],
    },
    payload_type: 102,
    ..Default::default()
};
media_engine.register_codec(video_codec, RtpCodecKind::Video)?;
```

---

## 12. `RTCConfigurationBuilder`

**文件**: `rtc-0.20.0-rc.1/src/peer_connection/configuration/mod.rs:471`

```rust
pub struct RTCConfigurationBuilder { /* private */ }

impl RTCConfigurationBuilder {
    pub fn new() -> Self;
    pub fn with_ice_servers(mut self, ice_servers: Vec<RTCIceServer>) -> Self;
    pub fn with_ice_transport_policy(mut self, policy: RTCIceTransportPolicy) -> Self;
    pub fn with_bundle_policy(mut self, policy: RTCBundlePolicy) -> Self;
    pub fn with_rtcp_mux_policy(mut self, policy: RTCRtcpMuxPolicy) -> Self;
    pub fn with_peer_identitys(mut self, identity: String) -> Self;
    pub fn with_certificates(mut self, certs: Vec<RTCCertificate>) -> Self;
    pub fn with_ice_candidate_pool_size(mut self, size: u8) -> Self;
    pub fn build(self) -> RTCConfiguration;
}
```

```rust
pub struct RTCIceServer {
    pub urls: Vec<String>,          // STUN/TURN URL 列表
    pub username: String,           // TURN 认证用户名
    pub credential: String,         // TURN 认证密码
    pub credential_type: RTCIceCredentialType,
}
```

---

## 13. `runtime` 模块

**文件**: `webrtc-0.20.0-rc.1/src/runtime/mod.rs`

```rust
// 类型别名 (根据 feature 决定使用 tokio 或 smol)
pub type Interval = TokioInterval;  // or SmolInterval
pub type Sender<T> = TokioSender<T>;
pub type Receiver<T> = TokioReceiver<T>;

// 创建 channel
pub fn channel<T>(size: usize) -> (Sender<T>, Receiver<T>);

// 创建定时器
pub fn interval(duration: Duration) -> Interval;
impl Interval {
    pub async fn tick(&mut self) -> Instant;
}

// 获取默认 runtime
pub fn default_runtime() -> Option<Arc<dyn Runtime>>;  // feature = "runtime-tokio" 时总是 Some

// Runtime trait
pub trait Runtime: Send + Sync + Debug + 'static {
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>);
    fn wrap_udp_socket(&self, socket: UdpSocket) -> Box<dyn AsyncUdpSocket>;
}
```

> **注意**: `runtime::channel` 和 `runtime::interval` 返回的是 webrtc-rs 封装的类型,
> 不是标准的 tokio channel/timer。`Receiver<T>` 的 `.recv().await` 返回 `Option<T>`。

---

## 16. Statistics API

```rust
use rtc::statistics::{StatsSelector, report::RTCStatsReport};

// 获取统计 (需要在事件循环或定时任务中调用)
let report: RTCStatsReport = pc.get_stats(std::time::Instant::now(), StatsSelector::default()).await;
```

### 主要统计类型

| 方法 | 返回 | 说明 |
|------|------|------|
| `report.peer_connection()` | `Option<&RTCPeerConnectionStats>` | 连接级统计 (连接时长, ICE 重启次数) |
| `report.transport()` | `Option<&RTCTransportStats>` | 传输层 (RTT, ICE 角色, TLS 指纹, DTLS 版本) |
| `report.inbound_rtp_streams()` | `impl Iterator<Item = &RTCInboundRtpStreamStats>` | 入站流 (丢包数/率, 抖动, 码率, FIR/SLI/NACK 计数) |
| `report.outbound_rtp_streams()` | `impl Iterator<Item = &RTCOutboundRtpStreamStats>` | 出站流 (已发送字节/包数, 重传数, 目标码率, NACK/FIR) |
| `report.data_channels()` | `impl Iterator<Item = &RTCDataChannelStats>` | DataChannel (已收/发消息数, 字节数, 当前缓冲) |
| `report.candidate_pairs()` | `impl Iterator<Item = &RTCIceCandidatePairStats>` | ICE 候选对 (是否连通, 优先级, RTT, 总收/发字节) |

### 使用示例: 获取出站 RTT

```rust
if let Some(transport) = report.transport() {
    if let Some(rtt) = transport.current_round_trip_time {
        log::info!("[stats] RTT: {}ms", rtt * 1000.0);
    }
}
```

---

## 17. 重要注意事项 (更新)## 14. `Interceptor` 与 Registry

```rust
use rtc::interceptor::{Registry, NoopInterceptor};
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;

// 创建默认拦截器 (NACK, TWCC, 等)
let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

// PeerConnectionBuilder 泛型:
// PeerConnectionBuilder<A: ToSocketAddrs, I = NoopInterceptor>
// 调用 with_interceptor_registry 后变更为: PeerConnectionBuilder<A, Registry<P>>
```

---

## 15. 完整连接建立流程

```rust
// 1. 创建 MediaEngine 并注册 codec
let mut media_engine = MediaEngine::default();
media_engine.register_codec(video_codec, RtpCodecKind::Video)?;
let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

// 2. 创建 RTCConfiguration
let config = RTCConfigurationBuilder::new()
    .with_ice_servers(vec![RTCIceServer {
        urls: vec!["stun:stun.l.google.com:19302".to_string()],
        ..Default::default()
    }])
    .build();

// 3. 创建 channel
let (done_tx, done_rx) = runtime::channel::<()>(1);
let (gather_complete_tx, gather_complete_rx) = runtime::channel::<()>(1);
let (connected_tx, connected_rx) = runtime::channel::<()>(1);

// 4. 创建 handler
let handler = Arc::new(Handler { gather_complete_tx, done_tx: done_tx.clone(), connected_tx });

// 5. 获取 runtime
let rt = runtime::default_runtime().unwrap();

// 6. 创建 PeerConnection
let pc = PeerConnectionBuilder::new()
    .with_configuration(config)
    .with_media_engine(media_engine)
    .with_interceptor_registry(registry)
    .with_handler(handler)
    .with_runtime(rt.clone())
    .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
    .build().await?;

// 7. 添加视频轨道
let track: Arc<TrackLocalStaticSample> = Arc::new(
    TrackLocalStaticSample::new(MediaStreamTrack::new(
        "stream-id", "track-id", "label",
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(rand::random()),
                ..Default::default()
            },
            codec: video_codec.rtp_codec.clone(),
            ..Default::default()
        }],
    )?)?
);
pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await?;

// 8. 接收浏览器 offer → set_remote_description
let offer = RTCSessionDescription::offer(offer_sdp)?;
pc.set_remote_description(offer).await?;

// 9. create_answer → set_local_description
let answer = pc.create_answer(None).await?;
pc.set_local_description(answer).await?;

// 10. 等待 ICE gathering complete
gather_complete_rx.recv().await;

// 11. 发送 answer 给浏览器
if let Some(local) = pc.local_description().await {
    // local.sdp_type, local.sdp
}

// 12. 等待连接建立
connected_rx.recv().await;

// 13. 开始发送视频帧
let ssrc = *track.ssrcs().await.first().unwrap();
let mut ticker = runtime::interval(Duration::from_millis(33));
loop {
    ticker.tick().await;
    let data = capture_and_encode_frame();
    track.sample_writer(ssrc).write_sample(&Sample {
        data: Bytes::from(data),
        duration: Duration::from_millis(33),
        ..Default::default()
    }).await?;
}

// 14. 清理
pc.close().await?;
```

---

## 16. 重要注意事项

1. **`RTCRtpEncodingParameters.codec` 是 `RTCRtpCodec` 类型**
   (不是 `RTCRtpCodecParameters`)，从 `video_codec.rtp_codec` 获取。

2. **`Sample` 构造时可使用 `..Default::default()`**
   会自动设置 `timestamp: SystemInstant::now()`。

3. **`write_sample` 推荐使用 `sample_writer(ssrc).write_sample(&sample).await`**
   而不是直接调用 `track.write_sample(ssrc, sample, extensions)`。

4. **`RTCPeerConnectionIceEvent.candidate` 不是 `Option`**
   始终是 `RTCIceCandidate`。使用 `candidate.to_json()` 转为 `RTCIceCandidateInit`。

5. **`RTCIceCandidateInit.sdp_mline_index: Option<u16>`**

6. **`runtime::channel` 的 `Receiver.recv()` 返回 `Option<T>`**
   通信关闭时返回 `None`。

7. **无 OpenSSL 依赖**
   webrtc-rs 使用 `ring` + `rustls` 纯 Rust TLS 栈。

8. **`(dev-dependencies)` 中可能包含 tokio 老版本**
   运行时注意 cargo 依赖解析问题。

9. **`PeerConnectionBuilder` 泛型**
   `PeerConnectionBuilder<A: ToSocketAddrs, I = NoopInterceptor>`。
   调用 `with_interceptor_registry` 后类型变为 `PeerConnectionBuilder<A, Registry<P>>`。
