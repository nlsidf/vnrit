#include <media/NdkMediaCodec.h>
#include <media/NdkMediaFormat.h>
#include <media/NdkMediaError.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

#define LOG_TAG "mc_enc"
#define LOG(...) fprintf(stderr, LOG_TAG ": " __VA_ARGS__)

typedef struct {
    AMediaCodec *codec;
    int width;
    int height;
} McEncoder;

#define COLOR_NV12 21

McEncoder* mc_enc_open(int width, int height, int bitrate_kbps, int framerate) {
    McEncoder *enc = calloc(1, sizeof(McEncoder));
    if (!enc) return NULL;

    enc->width = width;
    enc->height = height;

    enc->codec = AMediaCodec_createEncoderByType("video/avc");
    if (!enc->codec) {
        LOG("Failed to create encoder\n");
        free(enc);
        return NULL;
    }

    AMediaFormat *fmt = AMediaFormat_new();
    AMediaFormat_setString(fmt, AMEDIAFORMAT_KEY_MIME, "video/avc");
    AMediaFormat_setInt32(fmt, AMEDIAFORMAT_KEY_WIDTH, width);
    AMediaFormat_setInt32(fmt, AMEDIAFORMAT_KEY_HEIGHT, height);
    AMediaFormat_setInt32(fmt, AMEDIAFORMAT_KEY_BIT_RATE, bitrate_kbps * 1000);
    AMediaFormat_setInt32(fmt, AMEDIAFORMAT_KEY_FRAME_RATE, framerate);
    AMediaFormat_setInt32(fmt, AMEDIAFORMAT_KEY_I_FRAME_INTERVAL, 10);
    AMediaFormat_setInt32(fmt, "color-format", COLOR_NV12);
    AMediaFormat_setInt32(fmt, "stride", width);
    AMediaFormat_setInt32(fmt, "slice-height", height);
    AMediaFormat_setInt32(fmt, "latency", 1);
    AMediaFormat_setInt32(fmt, AMEDIAFORMAT_KEY_PUSH_BLANK_BUFFERS_ON_STOP, 1);

    media_status_t status = AMediaCodec_configure(
        enc->codec, fmt, NULL, NULL, AMEDIACODEC_CONFIGURE_FLAG_ENCODE
    );
    AMediaFormat_delete(fmt);

    if (status != AMEDIA_OK) {
        LOG("Configure failed: %d\n", status);
        AMediaCodec_delete(enc->codec);
        free(enc);
        return NULL;
    }

    status = AMediaCodec_start(enc->codec);
    if (status != AMEDIA_OK) {
        LOG("Start failed: %d\n", status);
        AMediaCodec_delete(enc->codec);
        free(enc);
        return NULL;
    }

    LOG("Encoder opened: %dx%d, %dkbps, %dfps\n", width, height, bitrate_kbps, framerate);
    return enc;
}

int mc_enc_submit(McEncoder *enc, const uint8_t *nv12_data, int size, int64_t pts_us) {
    if (!enc || !enc->codec) return -1;

    ssize_t idx = AMediaCodec_dequeueInputBuffer(enc->codec, 10000);
    if (idx < 0) {
        if (idx == AMEDIACODEC_INFO_TRY_AGAIN_LATER) return 0;
        LOG("dequeueInputBuffer error: %zd\n", idx);
        return -1;
    }

    size_t buf_size;
    uint8_t *buf = AMediaCodec_getInputBuffer(enc->codec, idx, &buf_size);
    if (!buf) {
        LOG("getInputBuffer failed\n");
        return -1;
    }

    if ((size_t)size > buf_size) {
        LOG("Frame too large: %d > %zu\n", size, buf_size);
        size = buf_size;
    }

    memcpy(buf, nv12_data, size);

    media_status_t status = AMediaCodec_queueInputBuffer(
        enc->codec, idx, 0, size, pts_us, 0
    );
    if (status != AMEDIA_OK) {
        LOG("queueInputBuffer failed: %d\n", status);
        return -1;
    }

    return 1;
}

static int drain_one(McEncoder *enc, uint8_t *out_buf, int out_cap,
                     int64_t *out_pts_us, uint32_t *out_flags) {
    if (!enc || !enc->codec) return -1;

    AMediaCodecBufferInfo info;
    ssize_t idx = AMediaCodec_dequeueOutputBuffer(enc->codec, &info, 10000);  // 10ms

    if (idx == AMEDIACODEC_INFO_TRY_AGAIN_LATER) return 0;
    if (idx == AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED) {
        AMediaFormat *fmt = AMediaCodec_getOutputFormat(enc->codec);
        if (fmt) {
            int32_t w, h;
            if (AMediaFormat_getInt32(fmt, AMEDIAFORMAT_KEY_WIDTH, &w) &&
                AMediaFormat_getInt32(fmt, AMEDIAFORMAT_KEY_HEIGHT, &h)) {
                enc->width = w;
                enc->height = h;
            }
            AMediaFormat_delete(fmt);
        }
        return -2;
    }
    if (idx < 0) {
        LOG("dequeueOutputBuffer error: %zd\n", idx);
        return -1;
    }

    size_t buf_size;
    uint8_t *buf = AMediaCodec_getOutputBuffer(enc->codec, idx, &buf_size);
    if (!buf) {
        LOG("getOutputBuffer failed\n");
        AMediaCodec_releaseOutputBuffer(enc->codec, idx, false);
        return -1;
    }

    int data_size = info.size;
    if (data_size > out_cap) data_size = out_cap;

    memcpy(out_buf, buf + info.offset, data_size);

    if (out_pts_us) *out_pts_us = info.presentationTimeUs;
    if (out_flags) *out_flags = info.flags;

    AMediaCodec_releaseOutputBuffer(enc->codec, idx, false);
    return data_size;
}

// Submit a frame and drain all available output.
// Returns total bytes written to out_buf, or 0/negative on no-data/error.
// The presentation time (in microseconds) of the returned data is written to *out_pts_us.
int mc_enc_encode(McEncoder *enc, const uint8_t *frame, int frame_size,
                  int64_t pts_us, uint8_t *out_buf, int out_cap,
                  int64_t *out_pts_us) {
    // Submit current frame
    int ret = mc_enc_submit(enc, frame, frame_size, pts_us);
    if (ret < 0) return ret;

    // Drain all available output (may include output from previous submits)
    int total = 0;
    int64_t first_pts = -1;
    while (total < out_cap) {
        int64_t op;
        uint32_t flags;
        int n = drain_one(enc, out_buf + total, out_cap - total, &op, &flags);
        if (n == -2) continue;
        if (n <= 0) break;
        if (first_pts < 0) first_pts = op;
        total += n;
        if (flags & AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM) break;
    }

    if (out_pts_us) *out_pts_us = first_pts;
    return total;
}

void mc_enc_close(McEncoder *enc) {
    if (!enc) return;
    if (enc->codec) {
        AMediaCodec_stop(enc->codec);
        AMediaCodec_delete(enc->codec);
    }
    LOG("Encoder closed\n");
    free(enc);
}
