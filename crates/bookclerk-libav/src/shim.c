/*
 * A small, stable C surface over the pinned FFmpeg build.
 *
 * Rust never sees an FFmpeg struct: the layouts of AVCodecContext,
 * AVFormatContext and AVFrame change between FFmpeg releases, so every
 * field access happens here, compiled against the same headers as the
 * libraries. Rust calls the bc_* functions below and reads only the plain
 * structs this file defines (see src/ffi.rs, which mirrors them).
 *
 * Decoding is single-threaded (the media worker's confinement allows no
 * extra threads, and FFmpeg is built without pthreads). Every input is
 * untrusted: sizes are checked before they are used, and every failure
 * returns a negative code instead of aborting.
 */

#include <limits.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/avutil.h>
#include <libavutil/channel_layout.h>
#include <libavutil/common.h>
#include <libavutil/mem.h>

/* Codec numbers shared with src/ffi.rs (enum Codec). */
#define BC_CODEC_AC3 1
#define BC_CODEC_EAC3 2
#define BC_CODEC_WMAV1 3
#define BC_CODEC_WMAV2 4
#define BC_CODEC_WMAPRO 5
#define BC_CODEC_WMALOSSLESS 6
#define BC_CODEC_AMRNB 7
#define BC_CODEC_AMRWB 8

/* Results shared with src/ffi.rs. */
#define BC_OK 0
#define BC_AGAIN 1
#define BC_EOF 2
#define BC_ERR_ARGS -1
#define BC_ERR_NOMEM -2
#define BC_ERR_UNSUPPORTED -3
#define BC_ERR_DECODE -4
#define BC_ERR_DEMUX -5
#define BC_ERR_LAYOUT -6

/* Limits that keep a hostile stream from asking for unbounded memory. */
#define BC_MAX_CHANNELS 8
#define BC_MAX_RATE 768000
#define BC_MAX_EXTRADATA (1 << 20)
#define BC_MAX_FRAME_SAMPLES (1 << 20)
#define BC_IO_BUFFER 65536

/* The demuxers the build enables; nothing else may be probed. */
static const char BC_FORMATS[] = "asf,ac3,eac3,amr,amrnb,amrwb,mov,mp4,m4a,3gp,3g2,mj2";
/* The decoders the build enables. */
static const char BC_CODECS[] = "ac3,eac3,wmav1,wmav2,wmapro,wmalossless,amrnb,amrwb";

/* A track's decoding parameters, as the demuxer found them. */
typedef struct bc_codec_config {
    int32_t codec;
    int32_t sample_rate;
    int32_t channels;
    int32_t block_align;
    int64_t bit_rate;
    const uint8_t *extradata;
    size_t extradata_len;
} bc_codec_config;

/* A demuxed track: its config plus timing. */
typedef struct bc_track {
    bc_codec_config config;
    int32_t time_base_num;
    int32_t time_base_den;
    /* In time_base units; -1 when unknown. */
    int64_t duration;
    int64_t start;
} bc_track;

/* One decoded frame, interleaved 32-bit float clamped to -1..1, valid until
 * the next call. */
typedef struct bc_frame {
    const float *samples;
    int32_t frames;
    int32_t channels;
    int32_t sample_rate;
    /* Speaker positions in FFmpeg's native order (WAVE bit numbering), one
     * bit per channel in interleaved order; 0 when the decoder gives none. */
    uint64_t channel_mask;
} bc_frame;

typedef struct bc_dec {
    AVCodecContext *ctx;
    AVPacket *pkt;
    AVFrame *frame;
    float *pcm;
    size_t pcm_cap;
} bc_dec;

typedef int (*bc_read_fn)(void *opaque, uint8_t *buf, int len);
typedef int64_t (*bc_seek_fn)(void *opaque, int64_t offset, int whence);

typedef struct bc_fmt {
    AVFormatContext *ctx;
    AVIOContext *io;
    AVPacket *pkt;
    int stream;
    bc_track track;
} bc_fmt;

static enum AVCodecID to_av_codec(int32_t codec) {
    switch (codec) {
    case BC_CODEC_AC3: return AV_CODEC_ID_AC3;
    case BC_CODEC_EAC3: return AV_CODEC_ID_EAC3;
    case BC_CODEC_WMAV1: return AV_CODEC_ID_WMAV1;
    case BC_CODEC_WMAV2: return AV_CODEC_ID_WMAV2;
    case BC_CODEC_WMAPRO: return AV_CODEC_ID_WMAPRO;
    case BC_CODEC_WMALOSSLESS: return AV_CODEC_ID_WMALOSSLESS;
    case BC_CODEC_AMRNB: return AV_CODEC_ID_AMR_NB;
    case BC_CODEC_AMRWB: return AV_CODEC_ID_AMR_WB;
    default: return AV_CODEC_ID_NONE;
    }
}

static int32_t from_av_codec(enum AVCodecID id) {
    switch (id) {
    case AV_CODEC_ID_AC3: return BC_CODEC_AC3;
    case AV_CODEC_ID_EAC3: return BC_CODEC_EAC3;
    case AV_CODEC_ID_WMAV1: return BC_CODEC_WMAV1;
    case AV_CODEC_ID_WMAV2: return BC_CODEC_WMAV2;
    case AV_CODEC_ID_WMAPRO: return BC_CODEC_WMAPRO;
    case AV_CODEC_ID_WMALOSSLESS: return BC_CODEC_WMALOSSLESS;
    case AV_CODEC_ID_AMR_NB: return BC_CODEC_AMRNB;
    case AV_CODEC_ID_AMR_WB: return BC_CODEC_AMRWB;
    default: return 0;
    }
}

/* Keeps FFmpeg's own log quiet: the worker reports errors itself. */
static void bc_quiet(void) { av_log_set_level(AV_LOG_QUIET); }

void bc_dec_close(bc_dec *dec) {
    if (!dec) return;
    avcodec_free_context(&dec->ctx);
    av_packet_free(&dec->pkt);
    av_frame_free(&dec->frame);
    free(dec->pcm);
    free(dec);
}

int bc_dec_open(const bc_codec_config *config, bc_dec **out) {
    if (!config || !out) return BC_ERR_ARGS;
    *out = NULL;
    bc_quiet();
    enum AVCodecID id = to_av_codec(config->codec);
    if (id == AV_CODEC_ID_NONE) return BC_ERR_UNSUPPORTED;
    if (config->sample_rate < 0 || config->sample_rate > BC_MAX_RATE) return BC_ERR_ARGS;
    if (config->channels < 0 || config->channels > BC_MAX_CHANNELS) return BC_ERR_ARGS;
    if (config->block_align < 0 || config->bit_rate < 0) return BC_ERR_ARGS;
    if (config->extradata_len > BC_MAX_EXTRADATA) return BC_ERR_ARGS;
    if (config->extradata_len > 0 && !config->extradata) return BC_ERR_ARGS;
    const AVCodec *codec = avcodec_find_decoder(id);
    if (!codec) return BC_ERR_UNSUPPORTED;

    bc_dec *dec = calloc(1, sizeof(*dec));
    if (!dec) return BC_ERR_NOMEM;
    dec->ctx = avcodec_alloc_context3(codec);
    dec->pkt = av_packet_alloc();
    dec->frame = av_frame_alloc();
    if (!dec->ctx || !dec->pkt || !dec->frame) {
        bc_dec_close(dec);
        return BC_ERR_NOMEM;
    }
    AVCodecContext *ctx = dec->ctx;
    ctx->sample_rate = config->sample_rate;
    if (config->channels > 0) av_channel_layout_default(&ctx->ch_layout, config->channels);
    ctx->block_align = config->block_align;
    ctx->bit_rate = config->bit_rate;
    ctx->thread_count = 1;
    if (config->extradata_len > 0) {
        ctx->extradata = av_mallocz(config->extradata_len + AV_INPUT_BUFFER_PADDING_SIZE);
        if (!ctx->extradata) {
            bc_dec_close(dec);
            return BC_ERR_NOMEM;
        }
        memcpy(ctx->extradata, config->extradata, config->extradata_len);
        ctx->extradata_size = (int)config->extradata_len;
    }
    if (avcodec_open2(ctx, codec, NULL) < 0) {
        bc_dec_close(dec);
        return BC_ERR_UNSUPPORTED;
    }
    *out = dec;
    return BC_OK;
}

int bc_dec_send(bc_dec *dec, const uint8_t *data, size_t len) {
    if (!dec) return BC_ERR_ARGS;
    if (len > INT_MAX - AV_INPUT_BUFFER_PADDING_SIZE) return BC_ERR_ARGS;
    if (len == 0) return BC_ERR_DECODE;
    if (!data) return BC_ERR_ARGS;
    av_packet_unref(dec->pkt);
    /* Decoders may read past the end; FFmpeg's padding contract. */
    if (av_new_packet(dec->pkt, (int)len) < 0) return BC_ERR_NOMEM;
    memcpy(dec->pkt->data, data, len);
    int ret = avcodec_send_packet(dec->ctx, dec->pkt);
    av_packet_unref(dec->pkt);
    if (ret == AVERROR(EAGAIN)) return BC_AGAIN;
    if (ret == AVERROR(ENOMEM)) return BC_ERR_NOMEM;
    if (ret < 0) return BC_ERR_DECODE;
    return BC_OK;
}

/* Reads sample `i` of channel `c` from a frame in any FFmpeg sample layout. */
static int bc_sample(const AVFrame *f, int planar, enum AVSampleFormat fmt, int channels, int i,
                     int c, float *value) {
    int idx = planar ? i : i * channels + c;
    const uint8_t *plane = planar ? f->extended_data[c] : f->extended_data[0];
    switch (fmt) {
    case AV_SAMPLE_FMT_FLT:
    case AV_SAMPLE_FMT_FLTP: *value = ((const float *)plane)[idx]; return 1;
    case AV_SAMPLE_FMT_DBL:
    case AV_SAMPLE_FMT_DBLP: *value = (float)((const double *)plane)[idx]; return 1;
    case AV_SAMPLE_FMT_S16:
    case AV_SAMPLE_FMT_S16P: *value = (float)((const int16_t *)plane)[idx] / 32768.0f; return 1;
    case AV_SAMPLE_FMT_S32:
    case AV_SAMPLE_FMT_S32P:
        *value = (float)((double)((const int32_t *)plane)[idx] / 2147483648.0);
        return 1;
    case AV_SAMPLE_FMT_U8:
    case AV_SAMPLE_FMT_U8P:
        *value = ((float)((const uint8_t *)plane)[idx] - 128.0f) / 128.0f;
        return 1;
    default: return 0;
    }
}

int bc_dec_receive(bc_dec *dec, bc_frame *out) {
    if (!dec || !out) return BC_ERR_ARGS;
    memset(out, 0, sizeof(*out));
    int ret = avcodec_receive_frame(dec->ctx, dec->frame);
    if (ret == AVERROR(EAGAIN)) return BC_AGAIN;
    if (ret == AVERROR_EOF) return BC_EOF;
    if (ret == AVERROR(ENOMEM)) return BC_ERR_NOMEM;
    if (ret < 0) return BC_ERR_DECODE;
    AVFrame *f = dec->frame;
    int channels = f->ch_layout.nb_channels;
    int rate = f->sample_rate;
    int frames = f->nb_samples;
    if (channels < 1 || channels > BC_MAX_CHANNELS || rate < 1 || rate > BC_MAX_RATE ||
        frames < 0 || frames > BC_MAX_FRAME_SAMPLES) {
        av_frame_unref(f);
        return BC_ERR_LAYOUT;
    }
    enum AVSampleFormat fmt = (enum AVSampleFormat)f->format;
    int planar = av_sample_fmt_is_planar(fmt);
    if (planar && channels > AV_NUM_DATA_POINTERS && !f->extended_data) {
        av_frame_unref(f);
        return BC_ERR_LAYOUT;
    }
    size_t need = (size_t)frames * (size_t)channels;
    if (need > dec->pcm_cap) {
        float *grown = realloc(dec->pcm, need * sizeof(float));
        if (!grown) {
            av_frame_unref(f);
            return BC_ERR_NOMEM;
        }
        dec->pcm = grown;
        dec->pcm_cap = need;
    }
    for (int i = 0; i < frames; i++) {
        for (int c = 0; c < channels; c++) {
            float v = 0.0f;
            if (!bc_sample(f, planar, fmt, channels, i, c, &v)) {
                av_frame_unref(f);
                return BC_ERR_LAYOUT;
            }
            /* Some decoders overshoot full scale; NaN becomes silence. */
            if (v != v) v = 0.0f;
            if (v > 1.0f) v = 1.0f;
            if (v < -1.0f) v = -1.0f;
            dec->pcm[(size_t)i * (size_t)channels + (size_t)c] = v;
        }
    }
    uint64_t mask = 0;
    if (f->ch_layout.order == AV_CHANNEL_ORDER_NATIVE &&
        av_popcount64(f->ch_layout.u.mask) == channels)
        mask = f->ch_layout.u.mask;
    av_frame_unref(f);
    out->channel_mask = mask;
    out->samples = dec->pcm;
    out->frames = frames;
    out->channels = channels;
    out->sample_rate = rate;
    return BC_OK;
}

/* Signals end of stream so a decoder with delay (the WMA codecs) gives
 * up the frames it still holds; bc_dec_receive then returns them and ends
 * with BC_EOF. A second drain before a flush is harmless. */
int bc_dec_drain(bc_dec *dec) {
    if (!dec) return BC_ERR_ARGS;
    int ret = avcodec_send_packet(dec->ctx, NULL);
    if (ret == AVERROR_EOF) return BC_OK;
    if (ret == AVERROR(ENOMEM)) return BC_ERR_NOMEM;
    if (ret < 0) return BC_ERR_DECODE;
    return BC_OK;
}

void bc_dec_flush(bc_dec *dec) {
    if (dec && dec->ctx) avcodec_flush_buffers(dec->ctx);
}

void bc_fmt_close(bc_fmt *fmt) {
    if (!fmt) return;
    avformat_close_input(&fmt->ctx);
    if (fmt->io) {
        av_freep(&fmt->io->buffer);
        avio_context_free(&fmt->io);
    }
    av_packet_free(&fmt->pkt);
    free(fmt);
}

int bc_fmt_open(void *opaque, bc_read_fn read, bc_seek_fn seek, bc_fmt **out) {
    if (!read || !seek || !out) return BC_ERR_ARGS;
    *out = NULL;
    bc_quiet();
    bc_fmt *fmt = calloc(1, sizeof(*fmt));
    if (!fmt) return BC_ERR_NOMEM;
    unsigned char *buffer = av_malloc(BC_IO_BUFFER);
    fmt->pkt = av_packet_alloc();
    if (!buffer || !fmt->pkt) {
        av_free(buffer);
        bc_fmt_close(fmt);
        return BC_ERR_NOMEM;
    }
    fmt->io = avio_alloc_context(buffer, BC_IO_BUFFER, 0, opaque, read, NULL, seek);
    if (!fmt->io) {
        av_free(buffer);
        bc_fmt_close(fmt);
        return BC_ERR_NOMEM;
    }
    AVFormatContext *ctx = avformat_alloc_context();
    if (!ctx) {
        bc_fmt_close(fmt);
        return BC_ERR_NOMEM;
    }
    ctx->pb = fmt->io;
    ctx->flags |= AVFMT_FLAG_CUSTOM_IO;
    ctx->format_whitelist = av_strdup(BC_FORMATS);
    ctx->codec_whitelist = av_strdup(BC_CODECS);
    /* No protocols are built; an empty list also refuses any a file names. */
    ctx->protocol_whitelist = av_strdup("");
    if (!ctx->format_whitelist || !ctx->codec_whitelist || !ctx->protocol_whitelist) {
        avformat_free_context(ctx);
        bc_fmt_close(fmt);
        return BC_ERR_NOMEM;
    }
    /* avformat_open_input frees ctx on failure. */
    if (avformat_open_input(&ctx, NULL, NULL, NULL) < 0) {
        bc_fmt_close(fmt);
        return BC_ERR_DEMUX;
    }
    fmt->ctx = ctx;
    if (avformat_find_stream_info(ctx, NULL) < 0) {
        bc_fmt_close(fmt);
        return BC_ERR_DEMUX;
    }
    int stream = av_find_best_stream(ctx, AVMEDIA_TYPE_AUDIO, -1, -1, NULL, 0);
    if (stream < 0) {
        bc_fmt_close(fmt);
        return BC_ERR_UNSUPPORTED;
    }
    AVStream *st = ctx->streams[stream];
    AVCodecParameters *par = st->codecpar;
    int32_t codec = from_av_codec(par->codec_id);
    if (codec == 0) {
        bc_fmt_close(fmt);
        return BC_ERR_UNSUPPORTED;
    }
    if (par->extradata_size < 0 || par->extradata_size > BC_MAX_EXTRADATA ||
        st->time_base.num <= 0 || st->time_base.den <= 0) {
        bc_fmt_close(fmt);
        return BC_ERR_DEMUX;
    }
    for (unsigned i = 0; i < ctx->nb_streams; i++) {
        if ((int)i != stream) ctx->streams[i]->discard = AVDISCARD_ALL;
    }
    fmt->stream = stream;
    fmt->track.config.codec = codec;
    fmt->track.config.sample_rate = par->sample_rate;
    fmt->track.config.channels = par->ch_layout.nb_channels;
    fmt->track.config.block_align = par->block_align;
    fmt->track.config.bit_rate = par->bit_rate;
    fmt->track.config.extradata = par->extradata;
    fmt->track.config.extradata_len = (size_t)par->extradata_size;
    fmt->track.time_base_num = st->time_base.num;
    fmt->track.time_base_den = st->time_base.den;
    fmt->track.duration = st->duration == AV_NOPTS_VALUE ? -1 : st->duration;
    fmt->track.start = st->start_time == AV_NOPTS_VALUE ? 0 : st->start_time;
    *out = fmt;
    return BC_OK;
}

int bc_fmt_track(const bc_fmt *fmt, bc_track *out) {
    if (!fmt || !out) return BC_ERR_ARGS;
    *out = fmt->track;
    return BC_OK;
}

int bc_fmt_read(bc_fmt *fmt, const uint8_t **data, size_t *len, int64_t *pts, int64_t *duration) {
    if (!fmt || !data || !len || !pts || !duration) return BC_ERR_ARGS;
    *data = NULL;
    *len = 0;
    for (;;) {
        av_packet_unref(fmt->pkt);
        int ret = av_read_frame(fmt->ctx, fmt->pkt);
        if (ret == AVERROR_EOF) return BC_EOF;
        if (ret == AVERROR(ENOMEM)) return BC_ERR_NOMEM;
        if (ret < 0) return BC_ERR_DEMUX;
        if (fmt->pkt->stream_index != fmt->stream || fmt->pkt->size <= 0) continue;
        *data = fmt->pkt->data;
        *len = (size_t)fmt->pkt->size;
        *pts = fmt->pkt->pts != AV_NOPTS_VALUE ? fmt->pkt->pts : fmt->pkt->dts;
        *duration = fmt->pkt->duration > 0 ? fmt->pkt->duration : 0;
        return BC_OK;
    }
}

int bc_fmt_seek(bc_fmt *fmt, int64_t ts) {
    if (!fmt) return BC_ERR_ARGS;
    if (avformat_seek_file(fmt->ctx, fmt->stream, INT64_MIN, ts, ts, 0) < 0) return BC_ERR_DEMUX;
    return BC_OK;
}
