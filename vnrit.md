# vnrit — 纯 Rust X11 WebRTC 流媒体服务器

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**vnrit** 是一款使用纯 Rust 编写的 X11 桌面流媒体服务器，通过 **WebRTC** 将 X11 屏幕以低延迟、高画质的方式传输至浏览器，并支持键盘鼠标反向控制。  
项目**不依赖 GStreamer、FFmpeg 或任何系统编解码器**，所有核心组件均为 Rust 实现或安全封装，编译后二进制仅约 **6.6 MB**，资源占用极低。

---

##  功能特性一览

###  视频流
- **MIT‑SHM 零拷贝捕获** – 优先使用 X11 共享内存扩展，像素数据直接写入内存，免除 Socket 传输。
- **SIMD 色彩转换** – 使用 Google libyuv 的 ARM NEON / x86 SSE 加速 BGRA → I420 转换。
- **智能缩放** – 支持服务器端缩放（`--height`），在 YUV 域使用高质量 Catmull‑Rom 双三次插值，节省带宽。
- **实时图像增强** – 内置边缘保持的 Unsharp Mask，融合运动自适应、纹理/亮度/色度感知，所有计算均为整数 SIMD 指令，无浮点开销。
- **H.264 硬件级编码** – 采用 Cisco OpenH264 编码器，专为屏幕内容优化的实时模式（Screen Content RealTime），支持运行时动态码率调整。
- **自适应码率（ABR）** – 基于 WebRTC 的 TWCC 带宽估计和丢包率反馈，自动调节编码码率，适应网络波动。

### 音频流
- **PulseAudio 系统音频捕获** – 自动探测默认音频输出设备的监听源（Monitor），捕获桌面声音（如媒体播放、系统提示音）。
- **Opus 编码** – 48 kHz 立体声，20 ms 帧长，低延迟高压缩比。
- **内存池复用** – PCM 缓冲区循环利用，零分配。

###  输入转发
- **XTest 扩展注入** – 支持键盘（KeySym）、鼠标（绝对/相对移动、滚轮、按键）事件。
- **前端物理键码映射** – 浏览器发送 `KeyboardEvent.code`（如 `KeyA`、`Digit2`），后端通过预构建的 keysym→keycode 哈希表快速转换，布局无关。
- **触控手势** – 单指移动（鼠标拖动）、双指缩放、长按右键、双指滚动（滚轮模拟）。
- **虚拟键盘** – 内置多层（主键 / F 键 / 数字键）虚拟键盘，支持修饰键“点按暂态”、“长按锁定”、“双击锁定”，适合触屏设备。

### 前端界面
- **纯 HTML + CSS + JS** – 无需任何第三方框架，体积小巧。
- **WebRTC 播放器** – 基于 `RTCPeerConnection` 接收 H.264 + Opus 流，支持自动播放与音频延迟解锁。
- **CSS 光标叠加层** – 远程光标位置通过 WebSocket 独立同步，不编码进视频流，保持清晰且无编码延迟。
- **缩放/平移** – 前端 CSS 变换实现平滑缩放（Alt 键或按钮切换），缩放时远程光标自动保持在视野中央。
- **自适应布局** – 使用 ResizeObserver 动态适应容器大小，完美适配窗口/全屏变化。
- **连接状态监控** – 显示实时 RTT 延迟、连接状态、自动重连（指数退避）。
- **压缩信令** – WebSocket 消息支持 deflate 压缩（原生 DecompressionStream + pako 回退），节省流量。

###  运维与可靠性
- **令牌认证** – 可选 `--token`，支持 URL 参数或 Cookie 传递，防止未授权访问。
- **信号处理** – 捕获 SIGINT/SIGTERM/SIGQUIT/SIGHUP，自动释放所有 XTest 按住键，避免 X 服务器“卡键”。
- **资源隔离** – 每个客户端独立 PeerConnection，连接断开时彻底清理所有后台任务，释放内存（调用 `mi_collect` 回收 mimalloc 缓存）。
- **超时保护** – X11 Socket 设置 5 秒接收超时，WebRTC 协商 15 秒超时，避免死锁。

---

##  系统架构

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                            vnrit 服务器 (Rust)                             │
│                                                                             │
│  ┌─────────────┐   ┌─────────────┐   ┌─────────────┐   ┌─────────────┐  │
│  │  捕获阶段    │──▶│  转换阶段    │──▶│  编码阶段    │──▶│  发送阶段    │  │
│  │  (阻塞线程)  │   │  (阻塞线程)  │   │  (阻塞线程)  │   │  (异步任务)  │  │
│  │  MIT-SHM    │   │  libyuv     │   │  openh264   │   │  WebRTC     │  │
│  │  / get_image│   │  BGRA→I420 │   │  H.264      │   │  track      │  │
│  │  + 缩放     │   │  + 增强     │   │  码率控制   │   │  write_sample│  │
│  └─────────────┘   └─────────────┘   └─────────────┘   └─────────────┘  │
│         │                  │                  │                  │         │
│         └──────────────────┴──────────────────┴──────────────────┘         │
│                               有界通道 (容量4)                              │
│                                                                             │
│  ┌─────────────┐   ┌─────────────┐   ┌─────────────┐                      │
│  │  音频捕获    │──▶│  音频编码    │──▶│  音频发送    │                      │
│  │  (阻塞线程)  │   │  (阻塞线程)  │   │  (异步任务)  │                      │
│  │  PulseAudio │   │  Opus       │   │  WebRTC     │                      │
│  │  事件驱动    │   │  48kHz/stereo│  │  track      │                      │
│  └─────────────┘   └─────────────┘   └─────────────┘                      │
│                                                                             │
│  ┌─────────────┐   ┌─────────────┐   ┌─────────────┐                      │
│  │  信令服务    │   │  输入注入    │   │  光标追踪    │                      │
│  │  (axum)     │   │  XTest      │   │  XI2事件    │                      │
│  │  WebSocket  │   │  键盘/鼠标   │   │  异步fd     │                      │
│  └─────────────┘   └─────────────┘   └─────────────┘                      │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
                              ┌───────────────┐
                              │  浏览器前端    │
                              │  (HTML+JS)    │
                              │  WebRTC / WS  │
                              └───────────────┘
```

- **并行管道** – 捕获、转换、编码、发送四个阶段分别运行在不同线程，通过 `SyncChannel`（容量 4）连接，互不阻塞。
- **双 X11 连接** – 捕获与输入使用独立连接，避免互斥锁竞争。
- **事件驱动光标更新** – 通过 XI2 扩展订阅鼠标移动事件，配合 `AsyncFd` 在 Tokio 事件循环中高效读取，无需轮询。

---

##  视频管道详解

### 1. 捕获（Capture）
- **优先 MIT‑SHM** – 通过 `shm::create_segment` 获取共享内存 FD，`mmap` 后 X 服务器直接写入原始 BGRA 像素。  
  对于 1920×1080@24fps，节省约 200 MB/s 的 X11 Socket 流量。
- **降级方案** – 若 SHM 不可用，自动回退至 `xproto::get_image`，依然保证功能完整。
- **缩放融合** – 若 `--height` 指定缩放，则在捕获阶段同时进行缩放和色彩转换，避免在 I420 域额外遍历。

### 2. 转换与增强（Convert & Enhance）
- **色彩空间转换** – 使用 `vnrit_libyuv`（libyuv 绑定）的 `ARGBToI420`，单次 SIMD 调用完成 BGRA → YUV420。
- **缩放（如需）** – 采用 Catmull‑Rom（Mitchell‑Netravali）双三次插值，预计算权重表（Q12 定点），水平+垂直分步执行，并利用块缓存（tile）优化内存访问。
- **图像增强（Unsharp Mask）** – 在 Y 平面应用边缘保持的 USM，具体亮点：
  - **边缘保持**：使用梯度指导的混合权重，避免传统 USM 的光晕（halo）。
  - **运动自适应**：基于帧间 MAD（平均绝对差）查表动态调整强度，抑制运动闪烁。
  - **纹理自适应**：根据局部对比度（相邻像素差异）调节增益，避免过度锐化。
  - **全局亮度/色度自适应**：参考平均亮度和色度活跃度，自动调节整体锐度。
  - **全部整数 + SIMD**：核心循环使用 `i16x8` 批量处理，无浮点，性能极致。
- **Chroma 饱和度增强**：当亮度锐化后，自动提升 U/V 饱和度以补偿视觉弱化。

### 3. 编码（Encode）
- **OpenH264** 编码器配置：
  - 配置文件：`Constrained Baseline`（浏览器兼容性最佳）
  - 使用模式：`ScreenContentRealTime`（针对屏幕内容优化）
  - 复杂度：`Low`（平衡 CPU 与画质）
  - GOP：`10 × framerate`（例如 240 帧）
  - 码率控制：`Bitrate` 模式，支持动态调整。
- **动态码率调整**：通过 `set_option(ENCODER_OPTION_BITRATE)` 运行时修改，不重建编码器。
- **关键帧强制**：当码率大幅下降（>30%）或每 5 秒定期插入 IDR，确保随机访问和错误恢复。
- **错误恢复**：编码失败时重置编码器，并进入指数退避重试，防止崩溃。

### 4. 发送（Send）
- 使用 `webrtc-rs` 的 `TrackLocalStaticSample`，异步调用 `write_sample` 将 H.264 NAL 单元打包为 RTP 包发送。
- 帧持续时间固定（`1/framerate`），确保码流平稳。
- 零拷贝：编码器输出直接转为 `Bytes` 传送，无需额外复制。

---

##  音频管道详解

- **PulseAudio 捕获**：
  - 启动时自动调用 `find_default_monitor` 检测默认输出的监听源（`.monitor`）。
  - 若检测失败，退回到默认录音源（通常为麦克风）。
  - 使用事件驱动的主循环（`pa_mainloop_iterate(true)`）阻塞等待数据，而非轮询，CPU 占用极低。
  - PCM 数据为 48 kHz 立体声 S16LE，每帧 3840 字节（20 ms）。
- **PCM 内存池**：预分配 8 个缓冲区，循环使用，零分配。
- **Opus 编码**：使用 `audiopus` crate，编码器配置为 48 kHz 立体声、音频应用（适合音乐/语音），输出帧长 20 ms，码率自适应。
- **WebRTC 发送**：与视频相同，通过音频 `TrackLocalStaticSample` 发送 RTP 包。

---

##  输入处理链路

### 前端 → 后端协议
所有输入命令通过 **WebSocket** 或 **DataChannel**（优先）以 CSV 形式发送，格式稳定：

| 命令 | 格式 | 说明 |
|------|------|------|
| 鼠标相对移动 | `mr,dx,dy` | 增量移动（像素） |
| 鼠标绝对移动 | `ma,x,y` | 绝对坐标（视频坐标系） |
| 鼠标按下 | `md,button` | button=1/2/3（左/中/右） |
| 鼠标释放 | `mu,button` | 同上 |
| 滚轮 | `ms,delta` | delta>0 下滚，<0 上滚（单位：步进） |
| 键盘按下 | `kd,code` | `KeyboardEvent.code` 字符串 |
| 键盘释放 | `ku,code` | 同上 |

### 后端处理
- **鼠标**：收到 `ma`/`mr` 后调用 `xtest::fake_input` 发送 `X11_MOTION_NOTIFY`，并更新本地缓存位置。
- **键盘**：通过预构建的 `keysym→keycode` 映射表（首次连接时从 `GetKeyboardMapping` 构建），将 `code` 转换为 X11 键码，再通过 XTest 发送 `KeyPress`/`KeyRelease`。
- **修饰键状态跟踪**：记录所有按下的键码，连接断开时自动释放，防止 X 服务器卡键。

### 光标同步优化
- 服务器通过 XI2 事件实时追踪光标绝对位置，并通过 WebSocket 发送 `{type:"cursor", x, y}` 给前端。
- 前端收到后使用 **CSS transform** 将自定义光标叠加层移动到对应坐标，无需依赖视频帧。
- 前端本地输入时立即更新叠加层，并发送绝对/相对移动命令，形成闭环。

---

## 🔌 信令与 WebRTC 连接

### WebSocket 信令流程
1. 浏览器连接 `/ws` 并发送 `{"type":"ready"}`。
2. 服务器创建 `PeerConnection`，生成 Offer SDP，发送 `{"type":"offer","sdp":"..."}`。
3. 浏览器创建 Answer，发送 `{"type":"answer","sdp":"..."}`。
4. 双方交换 ICE 候选（`{"type":"ice","candidate":"...","sdp_mline_index":0}`）。
5. 连接建立后，视频/音频轨道自动开始传输。

### 数据通道（DataChannel）
- 服务器主动创建名为 `"input"` 的 DataChannel，用于承载输入命令（低延迟、可靠传输）。
- 浏览器在 `ondatachannel` 事件中接收该通道，之后所有输入优先走 DataChannel，降级到 WebSocket。
- DataChannel 关闭时自动回退，保持可用性。

### ICE 配置
- 支持 STUN 服务器（默认 `stun:stun.cloudflare.com:3478`），可禁用（`--stun ""`）。
- 支持 TCP-only 模式（`--tcp-only`），适用于 UDP 被限制的网络。
- ICE 超时时间放宽至 15s（断连）和 60s（失败），适应移动网络波动。

---

##  前端界面详解

### 视频渲染
- 使用 `<video>` 元素，`object-fit: contain` 保持比例。
- 通过 `RTCPeerConnection` 的 `ontrack` 将媒体流挂载到 video/audio 元素。
- 音频默认静音（`muted`），首次用户交互时取消静音并调用 `play()`，符合浏览器自动播放策略。

### 缩放与平移（前端 CSS 变换）
- 由 `zoomLevel`、`zoomPanX`、`zoomPanY` 控制 `#zoom-layer` 的 `transform: translate(X,Y) scale(Z)`。
- 触发方式：
  - 鼠标滚轮（Alt 键按住时缩放，否则滚动）。
  - 双指捏合（触控板/触摸屏）。
  - 工具栏 `+` 按钮（开启/关闭缩放模式，Alt 键替代）。
- 平移边界自动限制，防止超出视频区域。
- 缩放时智能跟踪远程光标：若光标移出视口，自动平移使其回中，提升浏览体验。

### 光标叠加层
- 独立 `div#cursor-overlay`，样式为圆形光晕，通过 `transform: translate(x,y)` 定位。
- 位置更新使用 **requestAnimationFrame** 批量处理，与视频帧同步。
- 当服务器光标位置与本地预测偏差较大时，启动 40ms 缓动动画（ease-out），平滑过渡。

### 自定义虚拟键盘
- **三层布局**：主键（QWERTY）、功能键（F1-F12 等）、数字小键盘。
- **修饰键行为**：
  - **点击**：按下并保持 300ms 内释放 → 视为暂态（用于组合键，如 Shift+A）。
  - **长按 300ms**：进入“锁定”状态（适合连续输入，如 Ctrl+C/V）。
  - **双击**：永久锁定（直到再次点击解锁），适合需要长时间按住修饰键的场景。
- **层切换**：底部按钮 `Fn` 和 `123` 切换层，再次点击返回主层。
- **关闭**：点击视频区域或底部 `▼` 按钮关闭键盘，同时释放所有修饰键。

### 网络状态与重连
- 连接状态指示灯（红/绿），RTT 延迟显示。
- 断开后自动重连，退避间隔从 1s 开始，每次翻倍，最大 30s。
- 会话计数器（`currentSession`）防止旧连接的回调干扰新连接。

### 消息压缩
- 信令消息（offer/answer/ice）以纯文本发送，确保兼容性。
- 普通文本消息（如光标位置）超过 512 字节时，使用 deflate 压缩并加上前缀字节 `0x01` 发送为 Binary。
- 前端首先尝试 `DecompressionStream`（Chrome/Firefox/Safari 原生支持），失败后回退到 `pako`，最后尝试直接 UTF-8 解码。

---

##  性能优化一览

| 优化技术 | 具体实现 |
|----------|----------|
| **零拷贝内存池** | 所有环节使用预分配 Vec，通过 `Bytes` 和自定义 `PooledBuf` 自动回收到池，稳态零分配。 |
| **SHM 共享内存** | 捕获阶段直接映射 X 服务器写入的内存，无数据复制。 |
| **SIMD 批处理** | 色彩转换（libyuv）、缩放权重计算、增强循环均使用 SIMD（i16x8 / i32x4）指令。 |
| **I420 域缩放** | 缩放仅在 YUV 平面进行（1.5 字节/像素），避免在 BGRA（4 字节/像素）上操作，节省内存带宽。 |
| **双 X11 连接** | 捕获和输入分离，避免锁竞争。 |
| **异步非阻塞 I/O** | 所有网络操作（WebSocket、WebRTC）均为异步，充分利用 Tokio 运行时。 |
| **有界通道** | 管道间使用 `SyncChannel`（容量 4），防止生产者过快占用内存，同时保证流水线并行。 |
| **`with_resize_uninit`** | 使用 `set_len` 跳过缓冲区零初始化（写入前全部覆盖），减少内存清零开销。 |
| **原子操作优化** | 使用 `Ordering::Relaxed` 适当放宽内存屏障，提升 ARM 性能。 |
| **强制 IDR 间隔** | 每 5 秒强制关键帧，保证随机访问，同时避免过多 I 帧导致码率尖峰。 |
| **动态码率死区** | ABR 调整阈值为 50 kbps，防止频繁抖动。 |
| **PCM 内存池** | 音频捕获与编码共用一个环形池，无分配。 |
| **事件驱动 PulseAudio** | 使用 `pa_mainloop_iterate(true)` 阻塞等待，而非轮询，降低 CPU。 |
| **XI2 事件驱动光标** | 利用 `AsyncFd` 监听 X11 事件，无轮询开销。 |
| **WebSocket 压缩** | 非信令消息采用 deflate 压缩，减少带宽。 |
| **前端 RAF 合并** | 所有 DOM 更新在单次 `requestAnimationFrame` 中完成，避免布局抖动。 |
| **LayoutCache 缓存** | 缓存 `getBoundingClientRect` 结果，减少强制回流。 |

---

## 🛠️ 构建与运行

### 依赖项
- **Rust 工具链**：1.82 或更高（推荐使用 `rustup`）。
- **CMake**：3.20+（用于编译 libyuv）。
- **X11 开发库**（Linux）：`libx11-dev`、`libxtst-dev`、`libxext-dev`（构建时链接）。
- **PulseAudio 开发库**（可选，但编译需要）：`libpulse-dev`。
- **编译目标**：支持 x86_64 和 ARM（包括 Android Termux）。

### 构建步骤

```bash
# 克隆仓库
git clone https://github.com/yourname/vnrit.git
cd vnrit

# 或手动构建
export CMAKE=$(which cmake)
cargo build --release
```
### 运行服务器

```bash
# 最基本用法（捕获 :1 显示器，端口 8080）
./target/release/vnrit --display :1

# 推荐远程访问配置（720p，500 kbps）
./target/release/vnrit --display :1 --height 720 --bitrate 500

# 启用自适应码率
./target/release/vnrit --adaptive-bitrate

# 开启调试日志
RUST_LOG=debug ./target/release/vnrit
```

浏览器访问 `http://<服务器IP>:8080` 即可。

---

##  命令行参数完整参考

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `--display` | String | `:1` | X11 显示编号（如 `:0` 或 `:1`） |
| `-p, --port` | u16 | `8080` | HTTP/WebSocket 监听端口 |
| `--framerate` | i32 | `24` | 捕获帧率（fps） |
| `--bitrate` | i32 | `1000` | 目标视频码率（kbps） |
| `--height` | i32 | `0` | 输出高度（像素），0 表示保持原始分辨率，宽度按比例缩放 |
| `--stun` | String | `stun:stun.cloudflare.com:3478` | STUN 服务器 URL，设为空字符串可禁用 |
| `--token` | Option\<String\> | `None` | 认证令牌，设置后所有连接需提供 |
| `--log-level` | String | `info` | 日志级别：`off`, `error`, `warn`, `info`, `debug`, `trace` |
| `--tcp-only` | bool | `false` | 仅使用 TCP ICE 候选（禁用 UDP） |
| `--adaptive-bitrate` | bool | `false` | 开启自适应码率（基于 TWCC + 丢包） |
| `--enhance` | Option\<f32\> | `None` | Y 平面 USM 强度（0.0~2.0），当 `--height` 有效时默认 0.8，否则 0.0 |

---

## 🧪 使用示例

### 1. 本地局域网（无 STUN）
```bash
vnrit --stun "" --display :0
```

### 2. 高画质远程桌面（2 Mbps）
```bash
vnrit --bitrate 2000 --height 1080
```

### 3. 低带宽移动访问（300 kbps，240p）
```bash
vnrit --height 240 --bitrate 300 --framerate 15
```

### 4. 带认证与自适应码率
```bash
vnrit --token "supersecret" --adaptive-bitrate
```
浏览器访问需携带 `?token=supersecret` 或设置 Cookie `token=supersecret`。

### 5. 调试模式（日志输出到终端）
```bash
RUST_LOG=debug vnrit --log-level debug
```

---

## 🔒 安全与认证

- **令牌认证**：通过 `--token` 启用。浏览器需在 URL 查询参数或 Cookie 中提供 `token` 字段。首次通过查询参数认证后，服务器会设置 `HttpOnly` Cookie，后续 WebSocket 升级自动携带，无需重复传递。
- **WebSocket 安全**：若部署在生产环境，建议将 vnrit 置于反向代理（如 Nginx）后，启用 HTTPS/WSS 加密传输。
- **X11 权限**：vnrit 需要访问 X11 显示，通常通过 `DISPLAY` 环境变量或 `--display` 参数指定。确保 X 服务器允许连接（例如 `xhost +` 或使用 Cookie 认证）。
- **输入注入**：XTest 扩展允许模拟键盘鼠标，因此建议只在可信网络中使用，或使用防火墙限制访问。

---

## 🚧 故障排查指南

### 连接失败
- **检查 X11 服务器**：`echo $DISPLAY`，确保 vnrit 使用的显示编号正确。
- **确认 XTest 扩展**：运行 `xdpyinfo | grep XTest`，若无输出则需要加载 XTest 模块（通常默认安装）。
- **防火墙**：确保端口（默认 8080）可被浏览器访问。
- **STUN 不可达**：若无法访问默认 STUN 服务器，可禁用（`--stun ""`）或更换为内网可用的 STUN/TURN。

### 视频卡顿/黑屏
- **编码器错误**：查看日志是否有 `[encoder] error`，可能是资源不足，尝试降低分辨率/码率。
- **网络带宽**：检查上行带宽是否满足当前码率，可启用 `--adaptive-bitrate` 自动调节。
- **帧率过高**：降低 `--framerate` 至 15 或 10。

### 键盘输入无效
- **键码映射**：确保 X11 键盘布局与前端物理键盘匹配（通常无需干预）。
- **修饰键卡住**：断开连接后服务器会自动释放，若仍卡住可手动执行 `xdotool key X` 恢复。

### 音频无声
- **确认 PulseAudio 运行**：`pactl info`。
- **检查默认输出监听源**：`pactl list sinks short`，若不存在 monitor，可能需要设置默认源为 `alsa_output` 等。
- **浏览器自动播放策略**：需用户点击页面才能启用音频，点击任意位置即可。

### 构建失败
- **CMake 找不到**：安装 cmake，并设置 `CMAKE` 环境变量。
- **libclang 缺失**：`apt install libclang-dev`（Debian）或 `pkg install libclang`（Termux）。
- **libyuv 下载超时**：配置 Git 镜像（见构建章节）。

---

##  开发与贡献

### 代码结构
- `src/main.rs` – 所有后端逻辑（X11 捕获、编码、WebRTC、输入处理）。
- `src/index.html` – 前端完整页面（包含 CSS 与 JavaScript）。
- `build.sh` – 构建脚本，自动设置环境变量。
- `Cargo.toml` – 依赖管理，使用 `[patch.crates-io]` 对部分 crate 进行了补丁（如 `audiopus_sys`）以适配 Termux。

### 贡献指南
1. 提交 Issue 描述问题或建议。
2. Fork 仓库，创建功能分支。
3. 确保代码通过 `cargo fmt` 和 `cargo clippy`。
4. 编写测试（若适用）。
5. 发起 Pull Request。

### 依赖管理
所有依赖均为纯 Rust 或安全封装，已尽量减小第三方库的版本锁定。部分核心库（如 `webrtc-rs`）为 Git 依赖，确保使用最新修复。

---

## 📄 许可证

本项目采用 **MIT 许可证**，可自由使用、修改、分发。详见 [LICENSE](LICENSE) 文件。

---

## 🌟 致谢

- [webrtc-rs](https://github.com/webrtc-rs/webrtc) – WebRTC 实现基石。
- [x11rb](https://github.com/psychon/x11rb) – X11 协议绑定。
- [openh264](https://github.com/cisco/openh264) – 视频编码引擎。
- [libyuv](https://chromium.googlesource.com/libyuv/libyuv) – 高性能色彩转换。
- [libpulse-binding](https://github.com/jnqnfe/pulse-binding-rust) – PulseAudio 绑定。
- [mimalloc](https://github.com/microsoft/mimalloc) – 内存分配器。

---

**vnrit** – 让您的 X11 桌面触手可及，无论身处何地。
