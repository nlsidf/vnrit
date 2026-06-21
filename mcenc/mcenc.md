# mcenc – MediaCodec H.264 Hardware Encoder Plugin for GStreamer

`mcenc` is a GStreamer element that provides hardware-accelerated H.264 video encoding on Android devices. It leverages the Android NDK’s `AMediaCodec` API to access the device’s dedicated video encoder hardware (e.g., Adreno GPU) for low‑power, high‑performance encoding. This plugin is particularly useful in resource‑constrained environments like Termux on Android.

---

## Overview

- **Element Name**: `mcenc`  
- **Type**: `Codec/Encoder/Video`  
- **Rank**: `GST_RANK_PRIMARY + 100` (356) – high priority for automatic selection  
- **License**: LGPL  
- **Origin**: `https://opencode.ai`  

The plugin consists of two source files:
- **`gstmcenc.c`** – The GStreamer element implementation (pad handling, caps negotiation, state management, and dynamic loading of `libmediandk.so`).  
- **`mc_enc.c`** – A thin C wrapper around the NDK `AMediaCodec` API, providing simple functions to open, submit frames, encode, and close the encoder.

---

## Features

- Hardware H.264/AVC encoding (baseline/main/high profiles, device‑dependent).  
- Accepts NV12 (YUV 4:2:0 semi‑planar) video frames as input.  
- Outputs H.264 elementary stream in **byte‑stream** format (Annex B) with Access Unit alignment.  
- Supports adjustable **bitrate** (in kbps) and **framerate** (fps).  
- Dynamically loads `libmediandk.so` at runtime – no compile‑time dependency on Android NDK headers required.  
- Gracefully fails if the system lacks hardware encoding support.

---

## Usage in a GStreamer Pipeline

### Basic Pipeline

```bash
gst-launch-1.0 videotestsrc ! video/x-raw,format=NV12,width=640,height=480,framerate=30/1 ! mcenc bitrate=2000 framerate=30 ! h264parse ! rtph264pay ! udpsink host=127.0.0.1 port=5000
```

### Properties

| Property    | Type  | Range     | Default | Description                        |
|-------------|-------|-----------|---------|------------------------------------|
| `bitrate`   | int   | 1 – 100000| 5000    | Target bitrate in **kbps**         |
| `framerate` | int   | 1 – 120   | 30      | Encoding frame rate (fps)          |

These properties can be set at creation time or via `g_object_set()`.

### Input Caps

- `video/x-raw`
- `format` : `NV12`
- `width`  : 1 – 4096
- `height` : 1 – 4096
- `framerate` : any valid fraction

### Output Caps

- `video/x-h264`
- `stream-format` : `byte-stream`
- `alignment` : `au`
- `width`, `height`, `framerate` – same as input (fixed after negotiation)

> **Note**: The encoder may adjust the actual encoded width/height based on hardware constraints (e.g., to be multiple of 16). The output caps reflect the actual dimensions returned by the codec.

---

## Compilation

### Prerequisites (Termux / Android)

- GStreamer development libraries (`gstreamer-1.0`, `glib-2.0`)
- Android NDK sysroot (provides `media/NdkMediaCodec.h` – usually installed via `ndk-sysroot` package)
- `gcc` or `clang` with support for `-shared -fPIC`
- `pkg-config` for GStreamer flags

### Build Command

```bash
gcc -shared -fPIC -o libgstmcenc.so \
    mc_enc.c gstmcenc.c \
    $(pkg-config --cflags --libs gstreamer-1.0) \
    -ldl -lmediandk -I$PREFIX/include -L/system/lib64
```

If `-lmediandk` fails, try using the full path to the system library:
```bash
    /system/lib64/libmediandk.so
```

### Installation

Copy the resulting `libgstmcenc.so` to the GStreamer plugin directory:
```bash
cp libgstmcenc.so $PREFIX/lib/gstreamer-1.0/
```

Then verify with:
```bash
gst-inspect-1.0 mcenc
```

---

## How It Works

1. **Dynamic Loading** – `gstmcenc.c` uses `dlopen()` to load `libmediandk.so` at runtime, then resolves all `AMediaCodec_*` and `AMediaFormat_*` function pointers. This avoids a hard dependency on the NDK and allows the plugin to fail gracefully on non‑Android systems.

2. **Initialisation** – When the pipeline negotiates caps, the element receives the `GST_EVENT_CAPS` event, extracts width, height, and framerate, and calls `start_codec()`.

3. **Encoder Setup** – `start_codec()` calls `AMediaCodec_createEncoderByType("video/avc")`, configures it with the requested parameters (bitrate, framerate, i‑frame interval, NV12 colour format, etc.), and starts the codec.

4. **Frame Encoding** – In the `chain` function, each incoming buffer is mapped, and its PTS (in GST time) is converted to microseconds. The data (NV12) is submitted via `submit()`, which dequeues an input buffer, copies the frame data, and queues it to the encoder.

5. **Output Drain** – After submission, `drain_all()` repeatedly calls `drain_one()` to collect any available encoded output (H.264 NAL units). If the encoder produces CSD (Sequence Parameter Set / Picture Parameter Set) information, it is stored and prepended to the first output buffer.

6. **Output** – Encoded data is aggregated in an internal `carry_buf` (512 KB) and pushed downstream as a single GStreamer buffer with the correct PTS.

7. **Cleanup** – On state change to `READY` or finalization, the codec is stopped and deleted, and all internal resources are freed.

---

## Limitations & Caveats

- **Thread Safety** – The plugin is **not thread‑safe**. All GStreamer calls happen from the streaming thread, but internal state (e.g., `carry_buf`, `codec`) is not protected by locks. Avoid using the element in multi‑threaded contexts (e.g., multiple sink pads) without external serialisation.
- **CSD Buffer** – The internal CSD buffer is only 256 bytes. Some devices may produce larger CSD data, leading to truncation. In practice, most Android encoders generate CSD smaller than 256 bytes.
- **Error Handling** – The code lacks detailed error propagation. Failures during encoding may only produce warnings and drop frames.
- **Hardware Dependency** – Requires a device with a hardware H.264 encoder. Some devices may not support the exact parameters (e.g., bitrate, frame rate, resolution). If `configure()` fails, the element will not start.
- **Stride / Alignment** – Some encoders expect width/height aligned to 16 or 32 pixels. The plugin passes the original dimensions; if the encoder cannot handle them, configuration will fail.

---

## Integration with `vnrit`

This plugin is used by the [vnrit](https://github.com/nlsidf/vnrit) project to provide hardware‑accelerated encoding on Android (Termux). When `vnrit` is launched with `--codec h264`, it creates an instance of `mcenc`, sets the bitrate, and links it in the pipeline.

Example:
```bash
vnrit --codec h264 --height 720 --bitrate 500
```

If `mcenc` is correctly installed and the device supports hardware encoding, the pipeline will use it; otherwise, it falls back to `openh264enc` (software) or exits with an error.

---

## Troubleshooting

| Issue | Likely Cause | Suggested Fix |
|-------|--------------|---------------|
| `gst-inspect-1.0 mcenc` fails to find the element | Plugin not installed in GStreamer’s plugin path | Copy `libgstmcenc.so` to `$PREFIX/lib/gstreamer-1.0/` and rerun `gst-inspect-1.0` |
| `libmediandk.so not found` warning | System lacks NDK media library (only on non‑Android) | This is expected outside Android; the plugin will not work. |
| `configure failed: -1010` | Invalid colour format or unsupported resolution | Try smaller dimensions (e.g., 640×480) or a lower bitrate. |
| `dequeueOutputBuffer` errors | Encoder may have crashed or been killed by system | Restart the pipeline. Check device logs (`logcat`) for MediaCodec errors. |
| CSD not output | Some encoders emit CSD only via `INFO_OUTPUT_FORMAT_CHANGED` | Ensure you receive the format change event before submitting frames. |

---

## License & Credits

- **License**: LGPL (same as GStreamer).  
- **Author**: opencode (https://opencode.ai).  
- **Source**: Originally developed for use with Termux on Android.

---

## Future Improvements

Potential enhancements include:
- Support for HEVC (H.265) encoding.
- Add `profile` and `level` properties.
- Implement proper error signalling via `GError`.
- Add thread safety using `GMutex`.
- Provide dynamic bitrate adjustment based on network conditions.
- Increase CSD buffer size or allocate dynamically.

---

## References

- [GStreamer Plugin Development Guide](https://gstreamer.freedesktop.org/documentation/plugin-development/index.html)
- [Android NDK MediaCodec API](https://developer.android.com/ndk/reference/group/media)
- [OpenH264 Encoding with GStreamer](https://gstreamer.freedesktop.org/documentation/openh264/index.html)

---

*Document generated for the `mcenc` plugin. For issues or contributions, please refer to the original author’s repository or website.*
