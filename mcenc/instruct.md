# 使用 Android NDK 实现硬件加速视频编码器 - 完整指南

本文档将指导你从零开始，使用 Android NDK 的 `AMediaCodec` API，构建一个可直接运行的 GStreamer 硬件编码元素。我们将以 H.264 为例，同时指出如何扩展到 H.265。整个实现将遵循 GStreamer 插件开发规范，确保稳定高效。

---

## 目录
1. [背景与设计目标](#1-背景与设计目标)
2. [核心 API 详解](#2-核心-api-详解)
3. [GStreamer 元素框架设计](#3-gstreamer-元素框架设计)
4. [详细实现步骤](#4-详细实现步骤)
    - 4.1 插件注册与元素定义
    - 4.2 状态管理与生命周期
    - 4.3 Caps 协商与属性设置
    - 4.4 核心编码循环（chain 函数）
    - 4.5 输出处理与 CSD 提取
    - 4.6 错误报告与日志
5. [完整代码框架](#5-完整代码框架)
6. [编译与部署](#6-编译与部署)
7. [性能优化指南](#7-性能优化指南)
8. [扩展至 H.265](#8-扩展至-h265)
9. [常见问题与调试技巧](#9-常见问题与调试技巧)
10. [总结](#10-总结)

---

## 1. 背景与设计目标

### 1.1 为什么需要自行实现？
- **性能最优**：直接与 `libmediandk.so` 交互，无 JNI 开销，可实现零拷贝路径。
- **灵活控制**：可定制编码参数、处理私有扩展、实现低延迟模式。
- **无外部依赖**：不依赖 `gst-plugins-bad` 中的 `androidmedia` 插件，适合嵌入式或定制 Android 系统。

### 1.2 设计目标
- **线程安全**：使用互斥锁保护共享状态。
- **错误健壮**：所有 MediaCodec 操作返回值均被检查，并通过 GStreamer 错误信号向上传递。
- **低延迟**：采用非阻塞轮询，最小化缓冲。
- **内存安全**：动态分配 CSD 缓存，避免固定大小溢出。
- **可扩展**：支持 H.264/H.265 切换。

---

## 2. 核心 API 详解

Android NDK 提供了 `AMediaCodec` 接口，所有函数均定义在以下头文件中：
```c
#include <media/NdkMediaCodec.h>
#include <media/NdkMediaFormat.h>
#include <media/NdkMediaError.h>
```

### 2.1 关键数据结构
- `AMediaCodec`：编码器实例句柄。
- `AMediaFormat`：键值对集合，用于配置参数。
- `AMediaCodecBufferInfo`：输出缓冲区信息（偏移、大小、时间戳、标志）。

### 2.2 生命周期函数
| 函数 | 作用 |
|------|------|
| `AMediaCodec_createEncoderByType(const char* mime)` | 根据 MIME 类型创建编码器（如 `"video/avc"`）。 |
| `AMediaCodec_configure(codec, format, surface, crypto, flags)` | 配置编码参数（宽高、码率、颜色格式等）。 |
| `AMediaCodec_start(codec)` | 启动编码器，开始处理队列。 |
| `AMediaCodec_stop(codec)` | 停止编码器，释放内部资源。 |
| `AMediaCodec_delete(codec)` | 销毁编码器实例。 |

### 2.3 数据交互函数
- **输入**：
  - `ssize_t AMediaCodec_dequeueInputBuffer(codec, timeoutUs)`：获取可用输入缓冲区索引。
  - `uint8_t* AMediaCodec_getInputBuffer(codec, idx, &size)`：获取缓冲区指针。
  - `media_status_t AMediaCodec_queueInputBuffer(codec, idx, offset, size, ptsUs, flags)`：提交数据。
- **输出**：
  - `ssize_t AMediaCodec_dequeueOutputBuffer(codec, &info, timeoutUs)`：获取已编码数据索引。
  - `uint8_t* AMediaCodec_getOutputBuffer(codec, idx, &size)`：获取输出数据指针。
  - `media_status_t AMediaCodec_releaseOutputBuffer(codec, idx, render)`：释放缓冲区。

### 2.4 格式变化通知
当编码器输出格式（如分辨率、CSD）发生变化时，`dequeueOutputBuffer` 会返回 `AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED`。此时应调用 `AMediaCodec_getOutputFormat` 获取新格式，并提取 CSD 数据。

### 2.5 错误码
- `AMEDIA_OK`：成功。
- `AMEDIACODEC_INFO_TRY_AGAIN_LATER`：暂时无可用缓冲区。
- `AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED`：格式已更改。
- 负值表示具体错误，需查阅 NDK 文档。

---

## 3. GStreamer 元素框架设计

### 3.1 元素结构
我们定义 `GstMcEnc` 结构体，包含：
- `GstElement` 基类。
- `GstPad *sinkpad, *srcpad`。
- 编码参数：`bitrate` (kbps), `framerate`, `width`, `height`。
- `AMediaCodec *codec` 及运行标志。
- `GMutex lock` 保护所有可变状态。
- `GByteArray *csd` 动态存储 CSD。
- `GQueue *output_queue` 或其他聚合缓冲区。
- `guint64 frame_count` 等。

### 3.2 Pad 模板
- **Sink Pad**：接受 `video/x-raw, format=NV12, width=[1,4096], height=[1,4096], framerate=[0/1,2147483647/1]`。
- **Src Pad**：输出 `video/x-h264, stream-format=byte-stream, alignment=au`。

### 3.3 状态转换
- `NULL → READY`：无操作。
- `READY → PAUSED`：预先分配资源（如创建互斥锁）。
- `PAUSED → PLAYING`：实际创建并启动编码器。
- 反向转换时停止并释放编码器。

### 3.4 属性
- `bitrate`：整型，单位 kbps，默认 2000。
- `framerate`：整型，默认 30。
- 可选 `profile`、`level` 等。

---

## 4. 详细实现步骤

### 4.1 插件注册与元素定义
```c
// gstmcenc.h
#ifndef __GST_MC_ENC_H__
#define __GST_MC_ENC_H__

#include <gst/gst.h>
#include <media/NdkMediaCodec.h>

G_BEGIN_DECLS

#define GST_TYPE_MC_ENC (gst_mc_enc_get_type())
#define GST_MC_ENC(obj) (G_TYPE_CHECK_INSTANCE_CAST((obj), GST_TYPE_MC_ENC, GstMcEnc))
#define GST_MC_ENC_CLASS(klass) (G_TYPE_CHECK_CLASS_CAST((klass), GST_TYPE_MC_ENC, GstMcEncClass))
#define GST_IS_MC_ENC(obj) (G_TYPE_CHECK_INSTANCE_TYPE((obj), GST_TYPE_MC_ENC))
#define GST_IS_MC_ENC_CLASS(klass) (G_TYPE_CHECK_CLASS_TYPE((klass), GST_TYPE_MC_ENC))

typedef struct _GstMcEnc GstMcEnc;
typedef struct _GstMcEncClass GstMcEncClass;

struct _GstMcEnc {
    GstElement parent;
    GstPad *sinkpad, *srcpad;

    /* properties */
    gint bitrate;      /* kbps */
    gint framerate;

    /* negotiated */
    gint width, height;
    GstCaps *out_caps;

    /* codec state */
    AMediaCodec *codec;
    gboolean codec_running;
    GMutex lock;

    /* CSD */
    GByteArray *csd;
    gboolean csd_sent;

    /* output aggregation */
    GQueue *out_queue;  /* of GstBuffer* */
};

struct _GstMcEncClass {
    GstElementClass parent_class;
};

GType gst_mc_enc_get_type(void);

G_END_DECLS

#endif
```

### 4.2 状态管理与生命周期
```c
static GstStateChangeReturn gst_mc_enc_change_state(GstElement *element, GstStateChange transition) {
    GstMcEnc *self = GST_MC_ENC(element);
    GstStateChangeReturn ret;

    switch (transition) {
        case GST_STATE_CHANGE_READY_TO_PAUSED:
            // 预备资源，但编码器在 PLAYING 时创建
            break;
        case GST_STATE_CHANGE_PAUSED_TO_PLAYING:
            if (!self->codec) {
                if (!gst_mc_enc_start_codec(self)) {
                    GST_ELEMENT_ERROR(self, RESOURCE, SETTINGS,
                        ("Failed to start hardware encoder"),
                        (NULL));
                    return GST_STATE_CHANGE_FAILURE;
                }
            }
            break;
        case GST_STATE_CHANGE_PLAYING_TO_PAUSED:
            // 暂停时停止编码器，但保留资源
            gst_mc_enc_stop_codec(self);
            break;
        case GST_STATE_CHANGE_PAUSED_TO_READY:
            // 彻底释放
            gst_mc_enc_release_codec(self);
            break;
        default:
            break;
    }

    ret = GST_ELEMENT_CLASS(gst_mc_enc_parent_class)->change_state(element, transition);
    return ret;
}
```

启动函数 `start_codec`：
```c
static gboolean gst_mc_enc_start_codec(GstMcEnc *self) {
    g_mutex_lock(&self->lock);

    if (self->codec) {
        g_mutex_unlock(&self->lock);
        return TRUE;
    }

    // 创建编码器
    self->codec = AMediaCodec_createEncoderByType("video/avc");
    if (!self->codec) {
        GST_WARNING_OBJECT(self, "Failed to create AMediaCodec encoder");
        g_mutex_unlock(&self->lock);
        return FALSE;
    }

    // 配置格式
    AMediaFormat *format = AMediaFormat_new();
    AMediaFormat_setString(format, AMEDIAFORMAT_KEY_MIME, "video/avc");
    AMediaFormat_setInt32(format, AMEDIAFORMAT_KEY_WIDTH, self->width);
    AMediaFormat_setInt32(format, AMEDIAFORMAT_KEY_HEIGHT, self->height);
    AMediaFormat_setInt32(format, AMEDIAFORMAT_KEY_BIT_RATE, self->bitrate * 1000);
    AMediaFormat_setInt32(format, AMEDIAFORMAT_KEY_FRAME_RATE, self->framerate);
    AMediaFormat_setInt32(format, AMEDIAFORMAT_KEY_I_FRAME_INTERVAL, 1);
    AMediaFormat_setInt32(format, "color-format", COLOR_FormatYUV420SemiPlanar); // NV12
    // 可选：设置 stride 等

    media_status_t status = AMediaCodec_configure(self->codec, format, NULL, NULL, AMEDIACODEC_CONFIGURE_FLAG_ENCODE);
    AMediaFormat_delete(format);

    if (status != AMEDIA_OK) {
        GST_ERROR_OBJECT(self, "AMediaCodec_configure failed: %d", status);
        AMediaCodec_delete(self->codec);
        self->codec = NULL;
        g_mutex_unlock(&self->lock);
        return FALSE;
    }

    status = AMediaCodec_start(self->codec);
    if (status != AMEDIA_OK) {
        GST_ERROR_OBJECT(self, "AMediaCodec_start failed: %d", status);
        AMediaCodec_delete(self->codec);
        self->codec = NULL;
        g_mutex_unlock(&self->lock);
        return FALSE;
    }

    self->codec_running = TRUE;
    self->csd = g_byte_array_new();
    self->csd_sent = FALSE;
    g_mutex_unlock(&self->lock);

    GST_DEBUG_OBJECT(self, "Encoder started: %dx%d, %dkbps, %dfps",
        self->width, self->height, self->bitrate, self->framerate);
    return TRUE;
}
```

停止函数：
```c
static void gst_mc_enc_stop_codec(GstMcEnc *self) {
    g_mutex_lock(&self->lock);
    if (self->codec && self->codec_running) {
        AMediaCodec_stop(self->codec);
        self->codec_running = FALSE;
    }
    // 清空输出队列
    g_queue_foreach(self->out_queue, (GFunc)gst_buffer_unref, NULL);
    g_queue_clear(self->out_queue);
    if (self->csd) {
        g_byte_array_unref(self->csd);
        self->csd = NULL;
    }
    self->csd_sent = FALSE;
    g_mutex_unlock(&self->lock);
}

static void gst_mc_enc_release_codec(GstMcEnc *self) {
    g_mutex_lock(&self->lock);
    if (self->codec) {
        if (self->codec_running) {
            AMediaCodec_stop(self->codec);
            self->codec_running = FALSE;
        }
        AMediaCodec_delete(self->codec);
        self->codec = NULL;
    }
    if (self->csd) {
        g_byte_array_unref(self->csd);
        self->csd = NULL;
    }
    self->csd_sent = FALSE;
    g_mutex_unlock(&self->lock);
}
```

### 4.3 Caps 协商与属性设置
Caps 事件处理：
```c
static gboolean gst_mc_enc_sink_event(GstPad *pad, GstObject *parent, GstEvent *event) {
    GstMcEnc *self = GST_MC_ENC(parent);
    gboolean ret = TRUE;

    switch (GST_EVENT_TYPE(event)) {
        case GST_EVENT_CAPS: {
            GstCaps *caps;
            gst_event_parse_caps(event, &caps);
            GstStructure *s = gst_caps_get_structure(caps, 0);
            gst_structure_get_int(s, "width", &self->width);
            gst_structure_get_int(s, "height", &self->height);
            gint fn, fd;
            if (gst_structure_get_fraction(s, "framerate", &fn, &fd) && fd > 0)
                self->framerate = fn / fd;
            // 此处不启动编码器，等待 PLAYING 状态
            // 但可以创建输出 caps
            if (self->out_caps) gst_caps_unref(self->out_caps);
            self->out_caps = gst_caps_new_simple("video/x-h264",
                "stream-format", G_TYPE_STRING, "byte-stream",
                "alignment", G_TYPE_STRING, "au",
                "width", G_TYPE_INT, self->width,
                "height", G_TYPE_INT, self->height,
                "framerate", GST_TYPE_FRACTION, self->framerate, 1,
                NULL);
            gst_pad_set_caps(self->srcpad, self->out_caps);
            gst_event_unref(event);
            return TRUE;
        }
        default:
            break;
    }
    return gst_pad_event_default(pad, parent, event);
}
```

属性设置（用于 bitrate/framerate）：
```c
static void gst_mc_enc_set_property(GObject *object, guint prop_id, const GValue *value, GParamSpec *pspec) {
    GstMcEnc *self = GST_MC_ENC(object);
    switch (prop_id) {
        case PROP_BITRATE:
            self->bitrate = g_value_get_int(value);
            // 若编码器已运行，可动态更新（见下文）
            break;
        case PROP_FRAMERATE:
            self->framerate = g_value_get_int(value);
            break;
        default:
            G_OBJECT_WARN_INVALID_PROPERTY_ID(object, prop_id, pspec);
            break;
    }
}
```

### 4.4 核心编码循环（chain 函数）
这是最核心的部分，需在持有锁的情况下操作编码器。

```c
static GstFlowReturn gst_mc_enc_chain(GstPad *pad, GstObject *parent, GstBuffer *buf) {
    GstMcEnc *self = GST_MC_ENC(parent);
    GstFlowReturn ret = GST_FLOW_OK;

    g_mutex_lock(&self->lock);

    if (!self->codec || !self->codec_running) {
        GST_WARNING_OBJECT(self, "Encoder not running, dropping buffer");
        gst_buffer_unref(buf);
        g_mutex_unlock(&self->lock);
        return GST_FLOW_NOT_NEGOTIATED;
    }

    // 映射输入数据
    GstMapInfo map;
    if (!gst_buffer_map(buf, &map, GST_MAP_READ)) {
        GST_ERROR_OBJECT(self, "Failed to map input buffer");
        gst_buffer_unref(buf);
        g_mutex_unlock(&self->lock);
        return GST_FLOW_ERROR;
    }

    // 获取 PTS（转换为微秒）
    gint64 pts_us = GST_CLOCK_TIME_IS_VALID(GST_BUFFER_PTS(buf)) ? GST_BUFFER_PTS(buf) / 1000 : -1;

    // 提交输入帧
    ssize_t input_idx = AMediaCodec_dequeueInputBuffer(self->codec, 10000); // 10ms 超时
    if (input_idx >= 0) {
        size_t buf_size;
        uint8_t *input_buf = AMediaCodec_getInputBuffer(self->codec, input_idx, &buf_size);
        if (input_buf && map.size <= buf_size) {
            memcpy(input_buf, map.data, map.size);
            media_status_t status = AMediaCodec_queueInputBuffer(self->codec, input_idx,
                0, map.size, pts_us, 0);
            if (status != AMEDIA_OK) {
                GST_ERROR_OBJECT(self, "queueInputBuffer failed: %d", status);
                ret = GST_FLOW_ERROR;
                goto done;
            }
        } else {
            GST_WARNING_OBJECT(self, "Input buffer too small or invalid");
            // 释放输入缓冲区
            AMediaCodec_queueInputBuffer(self->codec, input_idx, 0, 0, 0, AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM);
            ret = GST_FLOW_ERROR;
            goto done;
        }
    } else if (input_idx == AMEDIACODEC_INFO_TRY_AGAIN_LATER) {
        // 无可用输入缓冲区，丢弃此帧
        GST_DEBUG_OBJECT(self, "No input buffer available, dropping frame");
    } else {
        GST_ERROR_OBJECT(self, "dequeueInputBuffer error: %zd", input_idx);
        ret = GST_FLOW_ERROR;
        goto done;
    }

    // 提取所有输出
    gst_mc_enc_drain_output(self);

done:
    gst_buffer_unmap(buf, &map);
    gst_buffer_unref(buf);
    g_mutex_unlock(&self->lock);
    return ret;
}
```

### 4.5 输出处理与 CSD 提取
`drain_output` 函数负责从编码器取出所有已编码数据，并拼接 CSD。

```c
static void gst_mc_enc_drain_output(GstMcEnc *self) {
    while (TRUE) {
        AMediaCodecBufferInfo info;
        ssize_t output_idx = AMediaCodec_dequeueOutputBuffer(self->codec, &info, 10000);
        if (output_idx == AMEDIACODEC_INFO_TRY_AGAIN_LATER) {
            break; // 无更多输出
        } else if (output_idx == AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED) {
            // 格式变化，提取 CSD
            AMediaFormat *fmt = AMediaCodec_getOutputFormat(self->codec);
            if (fmt) {
                // 清空旧 CSD
                g_byte_array_set_size(self->csd, 0);
                // 提取 csd-0, csd-1
                void *data = NULL; size_t sz = 0;
                if (AMediaFormat_getBuffer(fmt, "csd-0", &data, &sz) && data && sz > 0) {
                    g_byte_array_append(self->csd, (const guint8*)data, sz);
                }
                if (AMediaFormat_getBuffer(fmt, "csd-1", &data, &sz) && data && sz > 0) {
                    g_byte_array_append(self->csd, (const guint8*)data, sz);
                }
                AMediaFormat_delete(fmt);
                self->csd_sent = FALSE;
                GST_INFO_OBJECT(self, "CSD size: %u", self->csd->len);
            }
            continue;
        } else if (output_idx < 0) {
            GST_ERROR_OBJECT(self, "dequeueOutputBuffer error: %zd", output_idx);
            break;
        }

        // 正常输出
        size_t buf_size;
        uint8_t *out_data = AMediaCodec_getOutputBuffer(self->codec, output_idx, &buf_size);
        if (out_data && info.size > 0) {
            // 构建 GstBuffer
            GstBuffer *out_buf = gst_buffer_new_and_alloc(info.size + (self->csd_sent ? 0 : self->csd->len));
            guint offset = 0;
            // 如果 CSD 尚未发送，先拷贝 CSD
            if (!self->csd_sent && self->csd->len > 0) {
                gst_buffer_fill(out_buf, 0, self->csd->data, self->csd->len);
                offset = self->csd->len;
                self->csd_sent = TRUE;
            }
            gst_buffer_fill(out_buf, offset, out_data + info.offset, info.size);
            // 设置时间戳
            GST_BUFFER_PTS(out_buf) = info.presentationTimeUs * 1000;
            GST_BUFFER_DTS(out_buf) = GST_BUFFER_PTS(out_buf);
            // 推送到 src pad
            gst_pad_push(self->srcpad, out_buf);
        } else {
            GST_WARNING_OBJECT(self, "Output buffer is empty or invalid");
        }

        AMediaCodec_releaseOutputBuffer(self->codec, output_idx, FALSE);
    }
}
```

### 4.6 错误报告与日志
- 使用 `GST_ERROR_OBJECT`, `GST_WARNING_OBJECT`, `GST_DEBUG_OBJECT` 输出日志。
- 在发生不可恢复错误时，调用 `GST_ELEMENT_ERROR` 并返回失败。

---

## 5. 完整代码框架

以下是整合后的核心文件，省略了部分标准 GObject 模板代码，但提供完整逻辑。

**gstmcenc.c** (完整)：
```c
#define _GNU_SOURCE
#include "gstmcenc.h"
#include <dlfcn.h>

GST_DEBUG_CATEGORY_STATIC(gst_mc_enc_debug);
#define GST_CAT_DEFAULT gst_mc_enc_debug

enum {
    PROP_0,
    PROP_BITRATE,
    PROP_FRAMERATE,
};

static GstStaticPadTemplate sink_template = GST_STATIC_PAD_TEMPLATE(
    "sink", GST_PAD_SINK, GST_PAD_ALWAYS,
    GST_STATIC_CAPS("video/x-raw,format=NV12,width=[1,4096],height=[1,4096],framerate=[0/1,2147483647/1]")
);
static GstStaticPadTemplate src_template = GST_STATIC_PAD_TEMPLATE(
    "src", GST_PAD_SRC, GST_PAD_ALWAYS,
    GST_STATIC_CAPS("video/x-h264,stream-format=byte-stream,alignment=au,width=[1,4096],height=[1,4096],framerate=[0/1,2147483647/1]")
);

#define parent_class gst_mc_enc_parent_class
G_DEFINE_TYPE_WITH_CODE(GstMcEnc, gst_mc_enc, GST_TYPE_ELEMENT,
    GST_DEBUG_CATEGORY_INIT(gst_mc_enc_debug, "mcenc", 0, "MediaCodec H.264 Encoder");
)

// 函数声明
static void gst_mc_enc_finalize(GObject *object);
static void gst_mc_enc_set_property(GObject *object, guint prop_id, const GValue *value, GParamSpec *pspec);
static void gst_mc_enc_get_property(GObject *object, guint prop_id, GValue *value, GParamSpec *pspec);
static GstStateChangeReturn gst_mc_enc_change_state(GstElement *element, GstStateChange transition);
static gboolean gst_mc_enc_sink_event(GstPad *pad, GstObject *parent, GstEvent *event);
static GstFlowReturn gst_mc_enc_chain(GstPad *pad, GstObject *parent, GstBuffer *buf);
static void gst_mc_enc_stop_codec(GstMcEnc *self);
static void gst_mc_enc_release_codec(GstMcEnc *self);
static gboolean gst_mc_enc_start_codec(GstMcEnc *self);
static void gst_mc_enc_drain_output(GstMcEnc *self);

// 初始化
static void gst_mc_enc_init(GstMcEnc *self) {
    self->bitrate = 2000;
    self->framerate = 30;
    self->width = self->height = 0;
    self->codec = NULL;
    self->codec_running = FALSE;
    self->out_caps = NULL;
    self->csd = g_byte_array_new();
    self->csd_sent = FALSE;
    self->out_queue = g_queue_new();
    g_mutex_init(&self->lock);

    self->sinkpad = gst_pad_new_from_static_template(&sink_template, "sink");
    gst_pad_set_event_function(self->sinkpad, gst_mc_enc_sink_event);
    gst_pad_set_chain_function(self->sinkpad, gst_mc_enc_chain);
    gst_element_add_pad(GST_ELEMENT(self), self->sinkpad);

    self->srcpad = gst_pad_new_from_static_template(&src_template, "src");
    gst_pad_use_fixed_caps(self->srcpad);
    gst_element_add_pad(GST_ELEMENT(self), self->srcpad);
}

static void gst_mc_enc_class_init(GstMcEncClass *klass) {
    GObjectClass *gobject_class = G_OBJECT_CLASS(klass);
    GstElementClass *element_class = GST_ELEMENT_CLASS(klass);

    gobject_class->finalize = gst_mc_enc_finalize;
    gobject_class->set_property = gst_mc_enc_set_property;
    gobject_class->get_property = gst_mc_enc_get_property;

    element_class->change_state = gst_mc_enc_change_state;

    g_object_class_install_property(gobject_class, PROP_BITRATE,
        g_param_spec_int("bitrate", "Bitrate", "Bitrate in kbps",
            1, 100000, 2000, G_PARAM_READWRITE | G_PARAM_STATIC_STRINGS));
    g_object_class_install_property(gobject_class, PROP_FRAMERATE,
        g_param_spec_int("framerate", "Framerate", "Encoding framerate in fps",
            1, 120, 30, G_PARAM_READWRITE | G_PARAM_STATIC_STRINGS));

    gst_element_class_set_static_metadata(element_class,
        "MediaCodec Hardware H.264 Encoder",
        "Codec/Encoder/Video",
        "Hardware H.264 encoding via Android NDK AMediaCodec",
        "Your Name <your@email.com>");

    gst_element_class_add_static_pad_template(element_class, &sink_template);
    gst_element_class_add_static_pad_template(element_class, &src_template);
}

static void gst_mc_enc_finalize(GObject *object) {
    GstMcEnc *self = GST_MC_ENC(object);
    gst_mc_enc_release_codec(self);
    if (self->out_caps) gst_caps_unref(self->out_caps);
    if (self->csd) g_byte_array_unref(self->csd);
    if (self->out_queue) {
        g_queue_foreach(self->out_queue, (GFunc)gst_buffer_unref, NULL);
        g_queue_free(self->out_queue);
    }
    g_mutex_clear(&self->lock);
    G_OBJECT_CLASS(parent_class)->finalize(object);
}

static void gst_mc_enc_set_property(GObject *object, guint prop_id, const GValue *value, GParamSpec *pspec) {
    GstMcEnc *self = GST_MC_ENC(object);
    switch (prop_id) {
        case PROP_BITRATE:
            self->bitrate = g_value_get_int(value);
            // 可在此添加动态更新逻辑
            break;
        case PROP_FRAMERATE:
            self->framerate = g_value_get_int(value);
            break;
        default:
            G_OBJECT_WARN_INVALID_PROPERTY_ID(object, prop_id, pspec);
            break;
    }
}

static void gst_mc_enc_get_property(GObject *object, guint prop_id, GValue *value, GParamSpec *pspec) {
    GstMcEnc *self = GST_MC_ENC(object);
    switch (prop_id) {
        case PROP_BITRATE:
            g_value_set_int(value, self->bitrate);
            break;
        case PROP_FRAMERATE:
            g_value_set_int(value, self->framerate);
            break;
        default:
            G_OBJECT_WARN_INVALID_PROPERTY_ID(object, prop_id, pspec);
            break;
    }
}

static GstStateChangeReturn gst_mc_enc_change_state(GstElement *element, GstStateChange transition) {
    GstMcEnc *self = GST_MC_ENC(element);
    GstStateChangeReturn ret;

    switch (transition) {
        case GST_STATE_CHANGE_READY_TO_PAUSED:
            // 准备资源，但编码器在 PLAYING 时创建
            break;
        case GST_STATE_CHANGE_PAUSED_TO_PLAYING:
            if (!self->codec) {
                if (!gst_mc_enc_start_codec(self)) {
                    GST_ELEMENT_ERROR(self, RESOURCE, SETTINGS,
                        ("Failed to start hardware encoder"),
                        (NULL));
                    return GST_STATE_CHANGE_FAILURE;
                }
            }
            break;
        case GST_STATE_CHANGE_PLAYING_TO_PAUSED:
            gst_mc_enc_stop_codec(self);
            break;
        case GST_STATE_CHANGE_PAUSED_TO_READY:
            gst_mc_enc_release_codec(self);
            break;
        default:
            break;
    }

    ret = GST_ELEMENT_CLASS(parent_class)->change_state(element, transition);
    return ret;
}

static gboolean gst_mc_enc_sink_event(GstPad *pad, GstObject *parent, GstEvent *event) {
    GstMcEnc *self = GST_MC_ENC(parent);
    switch (GST_EVENT_TYPE(event)) {
        case GST_EVENT_CAPS: {
            GstCaps *caps;
            gst_event_parse_caps(event, &caps);
            GstStructure *s = gst_caps_get_structure(caps, 0);
            gst_structure_get_int(s, "width", &self->width);
            gst_structure_get_int(s, "height", &self->height);
            gint fn, fd;
            if (gst_structure_get_fraction(s, "framerate", &fn, &fd) && fd > 0)
                self->framerate = fn / fd;
            if (self->out_caps) gst_caps_unref(self->out_caps);
            self->out_caps = gst_caps_new_simple("video/x-h264",
                "stream-format", G_TYPE_STRING, "byte-stream",
                "alignment", G_TYPE_STRING, "au",
                "width", G_TYPE_INT, self->width,
                "height", G_TYPE_INT, self->height,
                "framerate", GST_TYPE_FRACTION, self->framerate, 1,
                NULL);
            gst_pad_set_caps(self->srcpad, self->out_caps);
            gst_event_unref(event);
            return TRUE;
        }
        default:
            break;
    }
    return gst_pad_event_default(pad, parent, event);
}

static gboolean gst_mc_enc_start_codec(GstMcEnc *self) {
    g_mutex_lock(&self->lock);
    if (self->codec) {
        g_mutex_unlock(&self->lock);
        return TRUE;
    }

    self->codec = AMediaCodec_createEncoderByType("video/avc");
    if (!self->codec) {
        GST_WARNING_OBJECT(self, "Failed to create AMediaCodec encoder");
        g_mutex_unlock(&self->lock);
        return FALSE;
    }

    AMediaFormat *format = AMediaFormat_new();
    AMediaFormat_setString(format, AMEDIAFORMAT_KEY_MIME, "video/avc");
    AMediaFormat_setInt32(format, AMEDIAFORMAT_KEY_WIDTH, self->width);
    AMediaFormat_setInt32(format, AMEDIAFORMAT_KEY_HEIGHT, self->height);
    AMediaFormat_setInt32(format, AMEDIAFORMAT_KEY_BIT_RATE, self->bitrate * 1000);
    AMediaFormat_setInt32(format, AMEDIAFORMAT_KEY_FRAME_RATE, self->framerate);
    AMediaFormat_setInt32(format, AMEDIAFORMAT_KEY_I_FRAME_INTERVAL, 1);
    AMediaFormat_setInt32(format, "color-format", COLOR_FormatYUV420SemiPlanar);
    // 可选：设置 stride 和 slice-height
    // AMediaFormat_setInt32(format, "stride", self->width);
    // AMediaFormat_setInt32(format, "slice-height", self->height);

    media_status_t status = AMediaCodec_configure(self->codec, format, NULL, NULL, AMEDIACODEC_CONFIGURE_FLAG_ENCODE);
    AMediaFormat_delete(format);

    if (status != AMEDIA_OK) {
        GST_ERROR_OBJECT(self, "AMediaCodec_configure failed: %d", status);
        AMediaCodec_delete(self->codec);
        self->codec = NULL;
        g_mutex_unlock(&self->lock);
        return FALSE;
    }

    status = AMediaCodec_start(self->codec);
    if (status != AMEDIA_OK) {
        GST_ERROR_OBJECT(self, "AMediaCodec_start failed: %d", status);
        AMediaCodec_delete(self->codec);
        self->codec = NULL;
        g_mutex_unlock(&self->lock);
        return FALSE;
    }

    self->codec_running = TRUE;
    self->csd_sent = FALSE;
    if (self->csd) g_byte_array_set_size(self->csd, 0);
    GST_INFO_OBJECT(self, "Encoder started: %dx%d, %dkbps, %dfps",
        self->width, self->height, self->bitrate, self->framerate);
    g_mutex_unlock(&self->lock);
    return TRUE;
}

static void gst_mc_enc_stop_codec(GstMcEnc *self) {
    g_mutex_lock(&self->lock);
    if (self->codec && self->codec_running) {
        AMediaCodec_stop(self->codec);
        self->codec_running = FALSE;
    }
    // 清空输出队列
    g_queue_foreach(self->out_queue, (GFunc)gst_buffer_unref, NULL);
    g_queue_clear(self->out_queue);
    self->csd_sent = FALSE;
    g_mutex_unlock(&self->lock);
}

static void gst_mc_enc_release_codec(GstMcEnc *self) {
    g_mutex_lock(&self->lock);
    if (self->codec) {
        if (self->codec_running) {
            AMediaCodec_stop(self->codec);
            self->codec_running = FALSE;
        }
        AMediaCodec_delete(self->codec);
        self->codec = NULL;
    }
    if (self->csd) g_byte_array_set_size(self->csd, 0);
    self->csd_sent = FALSE;
    g_mutex_unlock(&self->lock);
}

static void gst_mc_enc_drain_output(GstMcEnc *self) {
    while (TRUE) {
        AMediaCodecBufferInfo info;
        ssize_t idx = AMediaCodec_dequeueOutputBuffer(self->codec, &info, 10000);
        if (idx == AMEDIACODEC_INFO_TRY_AGAIN_LATER) {
            break;
        } else if (idx == AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED) {
            AMediaFormat *fmt = AMediaCodec_getOutputFormat(self->codec);
            if (fmt) {
                // 清空 CSD
                g_byte_array_set_size(self->csd, 0);
                void *data = NULL; size_t sz = 0;
                if (AMediaFormat_getBuffer(fmt, "csd-0", &data, &sz) && data && sz > 0) {
                    g_byte_array_append(self->csd, (const guint8*)data, sz);
                }
                if (AMediaFormat_getBuffer(fmt, "csd-1", &data, &sz) && data && sz > 0) {
                    g_byte_array_append(self->csd, (const guint8*)data, sz);
                }
                AMediaFormat_delete(fmt);
                self->csd_sent = FALSE;
                GST_INFO_OBJECT(self, "CSD size: %u", self->csd->len);
            }
            continue;
        } else if (idx < 0) {
            GST_ERROR_OBJECT(self, "dequeueOutputBuffer error: %zd", idx);
            break;
        }

        // 处理输出
        size_t buf_size;
        uint8_t *out_data = AMediaCodec_getOutputBuffer(self->codec, idx, &buf_size);
        if (out_data && info.size > 0) {
            gsize total_size = info.size + (self->csd_sent ? 0 : self->csd->len);
            GstBuffer *out_buf = gst_buffer_new_and_alloc(total_size);
            guint offset = 0;
            if (!self->csd_sent && self->csd->len > 0) {
                gst_buffer_fill(out_buf, 0, self->csd->data, self->csd->len);
                offset = self->csd->len;
                self->csd_sent = TRUE;
            }
            gst_buffer_fill(out_buf, offset, out_data + info.offset, info.size);
            GST_BUFFER_PTS(out_buf) = info.presentationTimeUs * 1000;
            GST_BUFFER_DTS(out_buf) = GST_BUFFER_PTS(out_buf);
            gst_pad_push(self->srcpad, out_buf);
        } else {
            GST_WARNING_OBJECT(self, "Output buffer empty or invalid");
        }
        AMediaCodec_releaseOutputBuffer(self->codec, idx, FALSE);
    }
}

static GstFlowReturn gst_mc_enc_chain(GstPad *pad, GstObject *parent, GstBuffer *buf) {
    GstMcEnc *self = GST_MC_ENC(parent);
    GstFlowReturn ret = GST_FLOW_OK;

    g_mutex_lock(&self->lock);
    if (!self->codec || !self->codec_running) {
        GST_WARNING_OBJECT(self, "Encoder not running, dropping buffer");
        gst_buffer_unref(buf);
        g_mutex_unlock(&self->lock);
        return GST_FLOW_NOT_NEGOTIATED;
    }

    GstMapInfo map;
    if (!gst_buffer_map(buf, &map, GST_MAP_READ)) {
        GST_ERROR_OBJECT(self, "Failed to map buffer");
        gst_buffer_unref(buf);
        g_mutex_unlock(&self->lock);
        return GST_FLOW_ERROR;
    }

    gint64 pts_us = GST_CLOCK_TIME_IS_VALID(GST_BUFFER_PTS(buf)) ? GST_BUFFER_PTS(buf) / 1000 : -1;

    ssize_t input_idx = AMediaCodec_dequeueInputBuffer(self->codec, 10000);
    if (input_idx >= 0) {
        size_t buf_size;
        uint8_t *input_buf = AMediaCodec_getInputBuffer(self->codec, input_idx, &buf_size);
        if (input_buf && map.size <= buf_size) {
            memcpy(input_buf, map.data, map.size);
            media_status_t status = AMediaCodec_queueInputBuffer(self->codec, input_idx,
                0, map.size, pts_us, 0);
            if (status != AMEDIA_OK) {
                GST_ERROR_OBJECT(self, "queueInputBuffer failed: %d", status);
                ret = GST_FLOW_ERROR;
                goto done;
            }
        } else {
            GST_WARNING_OBJECT(self, "Input buffer too small or invalid");
            AMediaCodec_queueInputBuffer(self->codec, input_idx, 0, 0, 0, AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM);
            ret = GST_FLOW_ERROR;
            goto done;
        }
    } else if (input_idx == AMEDIACODEC_INFO_TRY_AGAIN_LATER) {
        GST_DEBUG_OBJECT(self, "No input buffer available, dropping frame");
    } else {
        GST_ERROR_OBJECT(self, "dequeueInputBuffer error: %zd", input_idx);
        ret = GST_FLOW_ERROR;
        goto done;
    }

    gst_mc_enc_drain_output(self);

done:
    gst_buffer_unmap(buf, &map);
    gst_buffer_unref(buf);
    g_mutex_unlock(&self->lock);
    return ret;
}

// 插件入口
static gboolean plugin_init(GstPlugin *plugin) {
    return gst_element_register(plugin, "mcenc", GST_RANK_PRIMARY + 100, GST_TYPE_MC_ENC);
}

GST_PLUGIN_DEFINE(
    GST_VERSION_MAJOR,
    GST_VERSION_MINOR,
    mcenc,
    "MediaCodec Hardware H.264 Encoder",
    plugin_init,
    "1.0",
    "LGPL",
    "GStreamer",
    "https://example.com"
)
```

---

## 6. 编译与部署

### 6.1 编译环境 (Termux/Android)
- 安装必要包：`gcc`, `pkg-config`, `gstreamer-dev`, `ndk-sysroot`。
- 编译命令：
```bash
gcc -shared -fPIC -o libgstmcenc.so gstmcenc.c \
    $(pkg-config --cflags --libs gstreamer-1.0) \
    -ldl -lmediandk -I$PREFIX/include -L/system/lib64
```
- 若 `-lmediandk` 失败，使用绝对路径：`/system/lib64/libmediandk.so`。

### 6.2 部署
```bash
cp libgstmcenc.so $PREFIX/lib/gstreamer-1.0/
gst-inspect-1.0 mcenc
```

### 6.3 验证
运行一个测试管道：
```bash
gst-launch-1.0 videotestsrc ! video/x-raw,format=NV12,width=320,height=240,framerate=15/1 ! mcenc bitrate=500 ! h264parse ! fakesink silent=false
```

---

## 7. 性能优化指南

### 7.1 减少内存拷贝
- 直接使用 `AMediaCodec` 的输入缓冲区，避免中间缓冲。
- 输出时使用 `gst_buffer_new_wrapped` 包装已有内存，但需注意生命周期。

### 7.2 异步处理
- 使用 `AMediaCodec` 的回调模式（`AMediaCodec_setCallback`）替代轮询，减少 CPU 占用。
- 在回调中唤醒 GStreamer 的流线程。

### 7.3 动态比特率调整
- 若编码器支持，可通过 `AMediaCodec_setParameters` 动态更改。
- 不支持时，可先停止再重新配置（伴随短暂中断）。

### 7.4 多帧提交
- 一次 `chain` 可提交多帧，但需注意时间戳连续性。
- 将 `out_queue` 合并输出，减少 pad 推送次数。

---

## 8. 扩展至 H.265

### 8.1 修改 MIME 类型
- 创建编码器：`AMediaCodec_createEncoderByType("video/hevc")`。
- 配置格式中设置 `AMEDIAFORMAT_KEY_MIME` 为 `"video/hevc"`。
- 输出 Caps 改为 `video/x-h265`，并调整 `stream-format` 为 `hvc1` 或 `hev1`（取决于封装）。

### 8.2 CSD 差异
H.265 的 CSD 通常包含多个 NAL（VPS, SPS, PPS）。在 `AMediaFormat` 中，它们可能以 `csd-0`、`csd-1`、`csd-2` 形式存在。提取后需按顺序拼接。

### 8.3 Payloader
下游应使用 `rtph265pay`。

### 8.4 设备支持检查
并非所有芯片支持 H.265 编码，建议先查询设备能力（通过 `MediaCodecList`），若不支持则优雅降级。

---

## 9. 常见问题与调试技巧

### 9.1 `AMediaCodec_configure` 失败 `-1010`
- 原因：颜色格式不兼容。可尝试其他格式如 `COLOR_FormatYUV420Flexible`。
- 使用 `AMediaCodec_createEncoderByType` 返回后，用 `AMediaCodec_getCodecInfo` 查询支持的格式。

### 9.2 输出码流无法解码
- 检查 CSD 是否完整且正确拼接。
- 确保 `stream-format=byte-stream`，且 `alignment=au`。

### 9.3 内存泄漏
- 确保每个 `AMediaCodec` 实例均调用 `delete`。
- 每个 GstBuffer 均正确 unref。

### 9.4 调试日志
- 启用 GStreamer 调试：`GST_DEBUG=mcenc:5 gst-launch-...`
- Android 日志：`adb logcat | grep mcenc`

---

## 10. 总结

通过本指南，你已掌握：
- 使用 `AMediaCodec` API 实现硬件 H.264 编码。
- 将其封装为 GStreamer 元素，具备完整的 Caps 协商、状态管理、错误处理。
- 编译、部署、测试方法。
- 性能优化与扩展 H.265。

此实现相比官方 `androidmedia` 插件，更轻量、可控，适合定制化需求。如需进一步完善，可添加动态参数、支持更多颜色格式、实现零拷贝输出等。祝你开发顺利！

---


