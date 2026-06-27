# openh264 0.9.3 API 参考

## 项目结构

```
openh264 0.9.3          # Rust 绑定 (编译 OpenH264 C++ 库)
├── src/encoder.rs      # 编码器核心 (1249 行)
│   ├── Encoder         # 编码器主类型
│   ├── EncoderConfig   # 编码器配置 (builder 模式)
│   ├── EncodedBitStream # 编码输出
│   ├── FrameType       # 帧类型枚举
│   └── Layer / NAL 类型
├── src/formats/        # 媒体格式
│   ├── mod.rs          # 重新导出 YUVSource, YUVBuffer, YUVSlices, RGB 类型
│   └── yuv.rs          # YUVSource trait + YUVBuffer/YUVSlices 实现
├── src/error.rs        # Error 类型
├── src/time.rs         # Timestamp 类型
├── tests/encode.rs     # 编码集成测试
└── examples/           # examples
```

## 模块路径速查

| 类型 | 导入路径 |
|------|---------|
| `Encoder` | `openh264::encoder::Encoder` |
| `EncoderConfig` | `openh264::encoder::EncoderConfig` |
| `EncodedBitStream` | `openh264::encoder::EncodedBitStream` (encode() 返回值) |
| `FrameType` | `openh264::encoder::FrameType` |
| `BitRate` | `openh264::encoder::BitRate` |
| `FrameRate` | `openh264::encoder::FrameRate` |
| `RateControlMode` | `openh264::encoder::RateControlMode` |
| `UsageType` | `openh264::encoder::UsageType` |
| `Profile` | `openh264::encoder::Profile` |
| `Level` | `openh264::encoder::Level` |
| `Complexity` | `openh264::encoder::Complexity` |
| `QpRange` | `openh264::encoder::QpRange` |
| `IntraFramePeriod` | `openh264::encoder::IntraFramePeriod` |
| `YUVBuffer` | `openh264::formats::YUVBuffer` |
| `YUVSlices` | `openh264::formats::YUVSlices` |
| `YUVSource` (trait) | `openh264::formats::YUVSource` |
| `RgbSliceU8` | `openh264::formats::RgbSliceU8` |
| `RgbaSlice` | `openh264::formats::RgbaSlice` |
| `Timestamp` | `openh264::Timestamp` |
| `Error` | `openh264::Error` |
| `OpenH264API` | `openh264::OpenH264API` |

---

## 1. `Encoder`

**文件**: `src/encoder.rs:825`

```rust
pub struct Encoder { /* private fields */ }

impl Encoder {
    /// 使用默认配置创建编码器 (需要 feature = "source")
    #[cfg(feature = "source")]
    pub fn new() -> Result<Self, Error>;

    /// 使用自定义 API 来源 + 配置创建编码器
    pub fn with_api_config(api: OpenH264API, config: EncoderConfig) -> Result<Self, Error>;
}

// Send + Sync 均已实现
```

### 编码方法

```rust
impl Encoder {
    /// 编码一帧 YUV 数据
    pub fn encode<T: YUVSource>(&mut self, yuv_source: &T) -> Result<EncodedBitStream<'_>, Error>;

    /// 编码一帧 YUV 数据并指定时间戳
    pub fn encode_at<T: YUVSource>(
        &mut self,
        yuv_source: &T,
        timestamp: Timestamp,
    ) -> Result<EncodedBitStream<'_>, Error>;

    /// 强制下一个帧为 IDR 关键帧
    pub fn force_intra_frame(&mut self);
}
```

### 创建方式

```rust
use openh264::encoder::{Encoder, EncoderConfig};
use openh264::OpenH264API;

// 方式 1: 默认 (需要 "source" feature)
let encoder = Encoder::new()?;

// 方式 2: 自定义配置 (推荐)
let config = EncoderConfig::new()
    .bitrate(BitRate::from_bps(500_000))
    .max_frame_rate(FrameRate::from_hz(30.0));
let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)?;
```

---

## 2. `EncoderConfig`

**文件**: `src/encoder.rs:636`

```rust
pub struct EncoderConfig { /* private fields */ }

impl EncoderConfig {
    pub const fn new() -> Self;

    // ── Builder 方法 (所有方法返回 Self) ──

    // 码率控制
    pub const fn bitrate(mut self, bps: BitRate) -> Self;
    pub const fn max_frame_rate(mut self, value: FrameRate) -> Self;
    pub const fn rate_control_mode(mut self, value: RateControlMode) -> Self;

    // 使用场景
    pub const fn usage_type(mut self, value: UsageType) -> Self;

    // SPS/PPS 行为
    pub const fn sps_pps_strategy(mut self, value: SpsPpsStrategy) -> Self;

    // 编码控制
    pub const fn skip_frames(mut self, value: bool) -> Self;
    pub const fn debug(mut self, value: bool) -> Self;
    pub const fn max_slice_len(mut self, max_slice_len: u32) -> Self;

    // H.264 Profile 和 Level
    pub const fn profile(mut self, profile: Profile) -> Self;
    pub const fn level(mut self, level: Level) -> Self;

    // 质量/复杂度
    pub const fn complexity(mut self, complexity: Complexity) -> Self;
    pub const fn qp(mut self, value: QpRange) -> Self;
    pub const fn scene_change_detect(mut self, value: bool) -> Self;
    pub const fn adaptive_quantization(mut self, value: bool) -> Self;
    pub const fn background_detection(mut self, value: bool) -> Self;
    pub const fn long_term_reference(mut self, value: bool) -> Self;

    // GOP (Group of Pictures)
    pub const fn intra_frame_period(mut self, value: IntraFramePeriod) -> Self;

    // 线程
    pub const fn num_threads(mut self, threads: u16) -> Self;

    // VUI (色彩空间信息)
    pub const fn vui(mut self, config: VuiConfig) -> Self;
}
```

### 默认值

| 字段 | 默认值 |
|------|--------|
| `target_bitrate` | `BitRate::from_bps(120_000)` (120 kbps) |
| `max_frame_rate` | `FrameRate::from_hz(0.0)` |
| `rate_control_mode` | `RateControlMode::Quality` |
| `enable_skip_frame` | `true` |
| `usage_type` | `UsageType::CameraVideoRealTime` |
| `complexity` | `Complexity::Medium` |
| `qp` | `QpRange { min: 0, max: 51 }` |
| `scene_change_detect` | `true` |
| `adaptive_quantization` | `true` |
| `background_detection` | `true` |
| `long_term_reference` | `false` |
| `intra_frame_period` | `0` (auto) |
| `multiple_thread_idc` | `0` (auto) |
| `sps_pps_strategy` | `SpsPpsStrategy::ConstantId` |
| `data_format` | `videoFormatI420` |
| `vui` | `None` |

### 屏幕捕获推荐配置

```rust
let config = EncoderConfig::new()
    .bitrate(BitRate::from_bps(500_000))        // 500 kbps
    .max_frame_rate(FrameRate::from_hz(24.0))   // 24 fps
    .usage_type(UsageType::ScreenContentRealTime) // 屏幕内容优化
    .rate_control_mode(RateControlMode::Bitrate)  // 恒定码率
    .intra_frame_period(IntraFramePeriod::from_num_frames(240)) // 10s keyframe
    .profile(Profile::Baseline);                 // Baseline profile
```

---

## 3. `EncodedBitStream`

**文件**: `src/encoder.rs:1077`

```rust
pub struct EncodedBitStream<'a> {
    bit_stream_info: &'a SFrameBSInfo,
}

impl<'a> EncodedBitStream<'a> {
    // ── 查询 ──

    /// 帧类型
    pub const fn frame_type(&self) -> FrameType;

    /// 层数 (通常 2 层: layer 0=SPS/PPS, layer 1=视频数据)
    pub const fn num_layers(&self) -> usize;

    /// 获取指定层
    pub const fn layer(&self, i: usize) -> Option<Layer<'a>>;

    // ── 输出 ──

    /// 将所有 NAL 数据写入 Vec<u8> (包括 0x00 0x00 0x00 0x01 起始码)
    pub fn to_vec(&self) -> Vec<u8>;

    /// 将所有 NAL 数据追加到 Vec<u8>
    pub fn write_vec(&self, dst: &mut Vec<u8>);

    /// 将所有 NAL 数据写入实现了 Write 的类型
    pub fn write<T: std::io::Write>(&self, writer: &mut T) -> Result<(), Error>;

    // ── 底层 FFI ──
    pub const fn raw_info(&self) -> &'a SFrameBSInfo;
}
```

### `FrameType`

```rust
pub enum FrameType {
    Invalid,   // 编码器未就绪或参数无效
    IDR,       // IDR 帧 (关键帧) 0x65
    I,         // I 帧
    P,         // P 帧
    Skip,      // 跳过帧
    IPMixed,   // I + P 混合 (暂不支持)
}
```

### `Layer`

```rust
pub struct Layer<'a> { /* private */ }

impl<'a> Layer<'a> {
    /// NAL 单元数量
    pub const fn nal_count(&self) -> usize;

    /// 按索引获取 NAL 单元数据 (含 0x00 0x00 0x00 0x01 起始码)
    pub fn nal_unit(&self, i: usize) -> Option<&[u8]>;

    /// 是否为视频编码层
    pub const fn is_video(&self) -> bool;

    pub const fn raw_info(&self) -> &'a SLayerBSInfo;
}
```

### 典型输出结构

```
Layer 0 (非视频): SPS + PPS (2 个 NAL, 起始码 0x67 和 0x68)
Layer 1 (视频):   压缩后的帧数据 (1 个 NAL, IDR 起始码 0x65)
```

---

## 4. 编解码器配置类型

### `BitRate`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BitRate(u32);

impl BitRate {
    pub const fn from_bps(bps: u32) -> Self;  // bits per second
}
```

### `FrameRate`

```rust
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct FrameRate(f32);

impl FrameRate {
    pub const fn from_hz(hz: f32) -> Self;  // Hertz (frames per second)
}
```

### `RateControlMode`

```rust
#[derive(Copy, Clone, Debug, Default)]
pub enum RateControlMode {
    #[default] Quality,         // 质量优先
    Bitrate,                    // 码率优先 (推荐)
    Bufferbased,                // 缓冲区自适应
    Timestamp,                  // 时间戳自适应
    BitrateModePostSkip,        // 码率模式 + 后置跳帧
    Off,                        // 无码率控制
}
```

### `UsageType`

```rust
#[derive(Copy, Clone, Debug, Default)]
pub enum UsageType {
    #[default] CameraVideoRealTime,     // 摄像头实时视频
    ScreenContentRealTime,              // 屏幕内容实时 (推荐)
    CameraVideoNonRealTime,             // 摄像头非实时
    ScreenContentNonRealTime,           // 屏幕内容非实时
    InputContentTypeAll,                // 自适应
}
```

### `Profile`

```rust
#[derive(Copy, Clone, Debug)]
pub enum Profile {
    Baseline,           // 基础 (推荐)
    Main,               // 主要
    Extended,           // 扩展
    High,               // 高级
    High10, High422, High444,
    CAVLC444, ScalableBaseline, ScalableHigh,
}
```

### `Level`

```rust
#[derive(Copy, Clone, Debug)]
pub enum Level {
    Level_1_0, Level_1_B, Level_1_1, Level_1_2, Level_1_3,
    Level_2_0, Level_2_1, Level_2_2,
    Level_3_0, Level_3_1, Level_3_2,
    Level_4_0, Level_4_1, Level_4_2,
    Level_5_0, Level_5_1, Level_5_2,
}
```

### `Complexity`

```rust
#[derive(Debug, Default, Clone, Copy)]
pub enum Complexity {
    Low,
    #[default] Medium,
    High,
}
```

### `QpRange`

```rust
#[derive(Debug, Clone, Copy)]
pub struct QpRange { min: u8, max: u8 } // 0..=51

impl QpRange {
    pub const fn new(min: u8, max: u8) -> Self;  // panics 如果越界
}
impl Default for QpRange {
    fn default() -> Self { Self { min: 0, max: 51 } }
}
```

### `IntraFramePeriod`

```rust
#[derive(Debug, Clone, Copy, Default)]
pub struct IntraFramePeriod(u32);

impl IntraFramePeriod {
    pub const fn from_num_frames(frames: u32) -> Self;  // GOP 大小 (帧数)
    pub const fn auto() -> Self;                         // 自动
}
```

### `Timestamp`

```rust
#[repr(transparent)]
#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub struct Timestamp(u64);

impl Timestamp {
    pub const ZERO: Self = Self(0);
    pub const fn from_millis(ts: u64) -> Self;  // 从毫秒创建
    pub const fn as_millis(self) -> u64;
}
```

---

## 5. `YUVSource` Trait

**文件**: `src/formats/yuv.rs:6`

```rust
pub trait YUVSource {
    /// 获取帧的尺寸 (width, height)
    fn dimensions(&self) -> (usize, usize);

    /// 获取所有 plane 的 stride
    fn strides(&self) -> (usize, usize, usize);  // (y_stride, u_stride, v_stride)

    /// Y plane (亮度)
    fn y(&self) -> &[u8];

    /// U plane (色度蓝色差)
    fn u(&self) -> &[u8];

    /// V plane (色度红色差)
    fn v(&self) -> &[u8];

    // ── 默认实现 ──
    fn dimensions_i32(&self) -> (i32, i32) { ... }
    fn strides_i32(&self) -> (i32, i32, i32) { ... }
    fn rgb8_len(&self) -> usize { 0 }    // by default
    fn rgba8_len(&self) -> usize { 0 }
}
```

---

## 6. `YUVBuffer` (具体 I420 容器)

**文件**: `src/formats/yuv.rs:66`

```rust
/// 最常用的 I420 容器，自己持有数据
pub struct YUVBuffer {
    yuv: Vec<u8>,
    width: usize,
    height: usize,
}

impl YUVBuffer {
    /// 分配指定尺寸的零初始化 I420 缓冲区
    pub fn new(width: usize, height: usize) -> Self;

    /// 从已有的 I420 Vec<u8> 创建 (不复制，直接移动)
    pub fn from_vec(yuv: Vec<u8>, width: usize, height: usize) -> Self;

    /// 从 RGB 源转换创建
    pub fn from_rgb_source(rgb: impl RGBSource) -> Self;

    /// 从 RGB8 源转换创建 (快速路径)
    pub fn from_rgb8_source(rgb: impl RGB8Source) -> Self;

    /// 就地转换 RGB → YUV (复用缓冲区)
    pub fn read_rgb(&mut self, rgb: impl RGBSource);

    /// 就地转换 RGB8 → YUV (复用缓冲区, 快速)
    pub fn read_rgb8(&mut self, rgb: impl RGB8Source);
}

impl YUVSource for YUVBuffer {
    // strides: (width, width/2, width/2)
}
```

---

## 7. `YUVSlices` (引用的 I420 plane)

**文件**: `src/formats/yuv.rs:190`

```rust
/// 不持有数据的 I420 视图，只包含对已有数据的引用
#[derive(Clone, Copy, Debug)]
pub struct YUVSlices<'a> {
    dimensions: (usize, usize),
    yuv: (&'a [u8], &'a [u8], &'a [u8]),  // (y_plane, u_plane, v_plane)
    strides: (usize, usize, usize),
}

impl<'a> YUVSlices<'a> {
    /// 通过将 I420 数据的引用传入构造
    pub fn new(
        yuv: (&'a [u8], &'a [u8], &'a [u8]),  // (y, u, v) planes
        dimensions: (usize, usize),              // (width, height)
        strides: (usize, usize, usize),          // (y_stride, u_stride, v_stride)
    ) -> Self;
}

impl YUVSource for YUVSlices<'_> { ... }
```

### 使用 `YUVSlices` 避免复制

```rust
// 已有 I420 vec:
let i420: Vec<u8> = bgra_to_i420(&bgra, w, h);

// 使用 YUVSlices 避免复制到 YUVBuffer
let y_size = (w * h) as usize;
let uv_size = ((w/2) * (h/2)) as usize;
let slices = YUVSlices::new(
    (&i420[..y_size],
     &i420[y_size..y_size+uv_size],
     &i420[y_size+uv_size..]),
    (w as usize, h as usize),
    (w as usize, w as usize / 2, w as usize / 2),
);
let bitstream = encoder.encode(&slices)?;
```

> **注意**: `YUVSlices` 借用数据，需要确保 i420 变量在 encode 调用期间存活。

---

## 8. RGB Source 类型

**文件**: `src/formats/mod.rs`

```rust
/// 3 字节/像素的 RGB 源 (R, G, B)
pub struct RgbSliceU8<'a> { /* private */ }
impl RgbSliceU8<'_> {
    pub fn new(data: &[u8], dimensions: (u32, u32)) -> Self;
}

/// 4 字节/像素的 RGBA 源 (R, G, B, A 或 B, G, R, A — 取决于实现)
pub struct RgbaSlice<'a> { /* private */ }
impl RgbaSlice<'_> {
    pub fn new(data: &[u8], dimensions: (u32, u32)) -> Self;
}
```

> **注意**: `RgbaSlice` 期望的数据格式取决于 `RgbaSlice` 的实现。如果使用 `YUVBuffer::from_rgb_source()`，请确认输入格式。对于 BGRA 数据，推荐直接手动转换到 I420。

---

## 9. 推荐使用模式

### 创建编码器

```rust
use openh264::encoder::{
    Encoder, EncoderConfig, BitRate, FrameRate,
    UsageType, RateControlMode, IntraFramePeriod, Profile,
};
use openh264::formats::{YUVBuffer, YUVSource};
use openh264::OpenH264API;

let config = EncoderConfig::new()
    .bitrate(BitRate::from_bps(500_000))
    .max_frame_rate(FrameRate::from_hz(30.0))
    .usage_type(UsageType::ScreenContentRealTime)
    .rate_control_mode(RateControlMode::Bitrate)
    .intra_frame_period(IntraFramePeriod::from_num_frames(240))
    .profile(Profile::Baseline);

let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config)?;
```

### 编码一帧

```rust
// i420: Vec<u8> from BGRA→I420 conversion
let yuv = YUVBuffer::from_vec(i420, width, height);
let bitstream = encoder.encode(&yuv)?;

// 获取 H.264 bytes
let h264_data: Vec<u8> = bitstream.to_vec();

// 检查帧类型
match bitstream.frame_type() {
    FrameType::IDR => println!("Keyframe!"),
    FrameType::P   => println!("P-frame"),
    _ => {}
}
```

### 强制关键帧

```rust
// 在场景变化或每 N 帧后调用
encoder.force_intra_frame();

// 下一帧将被编码为 IDR
let bitstream = encoder.encode(&yuv)?;
assert_eq!(bitstream.frame_type(), FrameType::IDR);
```

---

## 10. 注意事项

1. **Feature "source"** — 需要 `openh264 = { version = "0.9", features = ["source"] }` 才能使用 `Encoder::new()`。编译时会构建 Cisco OpenH264 C++ 源码 (约 5-10 分钟，取决于机器)。

2. **尺寸限制** — 最大 3840x2160 (水平) 或 2160x3840 (垂直)，宽和高必须为偶数。

3. **编码器自动重初始化** — 当连续两帧的尺寸不同时，编码器会自动重新初始化。

4. **`to_vec()` vs `write_vec()`** — `to_vec()` 每次分配新 Vec；`write_vec(&mut dst)` 可复用缓冲区。

5. **`force_intra_frame()`** — 推荐在每个 GOP size 帧数后调用，或在检测到场景变化时调用。不会立即编码，而是标记下一帧为 IDR。

6. **`YUVBuffer::from_vec`** — 直接移动传入的 Vec，零复制。但 YUVBuffer 内部存储为连续 I420 数据 (Y + U + V 平面连续排列)，stride = width。

7. **`YUVSlices`** — 零分配，适用于已有 I420 数据的情况，避免 `from_vec` 的所有权转移。

8. **ScreenContentRealTime** — 屏幕内容编码模式，优化了文本和 UI 元素的编码效率。相比 CameraVideoRealTime，它对边缘和文字更清晰。

9. **`from_rgb_source` 的输入格式** — `YUVBuffer::from_rgb_source` 期望输入为 RGB (3 字节/像素)。BGRA 数据需要先转换为 RGB 或直接手动 BGRA→I420。

10. **NAL 起始码** — `to_vec()` 返回的数据包含 `0x00 0x00 0x00 0x01` 起始码，可直接送入 RTP 包或 webrtc-rs 的 `write_sample`。
