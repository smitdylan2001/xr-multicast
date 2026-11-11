#!/usr/bin/env bash
# Prints offsetof() for every FFmpeg struct field src/ffmpeg/sys.rs relies on
# (AVPacket, AVFrame, and the AVCodecContext fields used for hardware decoding:
# opaque, get_format, hw_device_ctx written; profile, sw_pix_fmt read in
# get_format), compiled against the official headers of
# each supported FFmpeg release. Compare with the `layouts_match_ffmpeg_headers`
# and `codec_context_offsets_match_ffmpeg_headers` unit tests.
#
# The native (x86_64 Linux) build is compiled and run. If `zig` is available
# ($ZIG or on PATH) the same table is also compiled for Windows x64, macOS arm64
# and Linux aarch64 (read from the generated assembly; nothing is run), which
# checks the LLP64 / Apple ABIs.
#
# Needs git, gcc and network access; nothing is installed.
# Usage: check-ffmpeg-layouts.sh [work dir]
set -euo pipefail
WORK=${1:-/tmp/xrmc-ffmpeg-headers}
mkdir -p "$WORK"
cd "$WORK"

# name|expression, one per line (kept in the order of the unit tests).
FIELDS='libavcodec major|LIBAVCODEC_VERSION_MAJOR
libavutil major|LIBAVUTIL_VERSION_MAJOR
AVPacket.pts|offsetof(AVPacket, pts)
AVPacket.data|offsetof(AVPacket, data)
AVPacket.size|offsetof(AVPacket, size)
AVPacket.flags|offsetof(AVPacket, flags)
AVFrame.linesize|offsetof(AVFrame, linesize)
AVFrame.width|offsetof(AVFrame, width)
AVFrame.height|offsetof(AVFrame, height)
AVFrame.format|offsetof(AVFrame, format)
AVFrame.pts|offsetof(AVFrame, pts)
AVFrame.flags|offsetof(AVFrame, flags)
AVFrame.color_range|offsetof(AVFrame, color_range)
AVFrame.colorspace|offsetof(AVFrame, colorspace)
AVCodecContext.opaque|offsetof(AVCodecContext, opaque)
AVCodecContext.width|offsetof(AVCodecContext, width)
AVCodecContext.pix_fmt|offsetof(AVCodecContext, pix_fmt)
AVCodecContext.profile|offsetof(AVCodecContext, profile)
AVCodecContext.get_format|offsetof(AVCodecContext, get_format)
AVCodecContext.sw_pix_fmt|offsetof(AVCodecContext, sw_pix_fmt)
AVCodecContext.hw_frames_ctx|offsetof(AVCodecContext, hw_frames_ctx)
AVCodecContext.hw_device_ctx|offsetof(AVCodecContext, hw_device_ctx)
AVCodecContext.extra_hw_frames|offsetof(AVCodecContext, extra_hw_frames)
AVCodecHWConfig.pix_fmt|offsetof(AVCodecHWConfig, pix_fmt)
AVCodecHWConfig.methods|offsetof(AVCodecHWConfig, methods)
AVCodecHWConfig.device_type|offsetof(AVCodecHWConfig, device_type)
sizeof(AVCodecHWConfig)|sizeof(AVCodecHWConfig)
AVBufferRef.data|offsetof(AVBufferRef, data)
AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX|AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX
AV_PIX_FMT_NONE|AV_PIX_FMT_NONE
AV_PROFILE_HEVC_MAIN|XP(HEVC_MAIN)
AV_PROFILE_HEVC_MAIN_10|XP(HEVC_MAIN_10)
AV_PROFILE_HEVC_MAIN_STILL_PICTURE|XP(HEVC_MAIN_STILL_PICTURE)'

{
  echo '#include <stdio.h>'
  echo '#include <stddef.h>'
  echo '#include "libavutil/frame.h"'
  echo '#include "libavutil/hwcontext.h"'
  echo '#include "libavcodec/avcodec.h"'
  echo '#ifdef AV_PROFILE_HEVC_MAIN'
  echo '#define XP(n) AV_PROFILE_##n'
  echo '#else'
  echo '#define XP(n) FF_PROFILE_##n'
  echo '#endif'
  echo 'const int xrmc_values[] = {'
  while IFS='|' read -r _ expr; do echo "  (int)($expr),"; done <<< "$FIELDS"
  echo '};'
  echo '#ifndef XRMC_NO_MAIN'
  echo 'int main(void) { for (size_t i = 0; i < sizeof xrmc_values / sizeof *xrmc_values; i++) printf("%d\n", xrmc_values[i]); return 0; }'
  echo '#endif'
} > off.c

print_table() { # values on stdin, one per line
  paste -d '=' <(cut -d'|' -f1 <<< "$FIELDS") - | sed 's/^/  /'
}

ZIG=${ZIG:-$(command -v zig || true)}

for v in 5.1 6.0 6.1 7.0 7.1 8.0 8.1; do
  if [ ! -d "src-$v" ]; then
    git clone -q --depth 1 --branch "release/$v" --filter=blob:none --sparse https://github.com/FFmpeg/FFmpeg.git "src-$v"
    (cd "src-$v" && git sparse-checkout set libavutil libavcodec)
    printf '#define AV_HAVE_BIGENDIAN 0\n#define AV_HAVE_FAST_UNALIGNED 1\n' > "src-$v/libavutil/avconfig.h"
  fi
  gcc -w -I "src-$v" off.c -o "off-$v"
  echo "FFmpeg $v (x86_64-linux):"
  "./off-$v" | print_table
  if [ -n "$ZIG" ]; then
    for target in x86_64-windows-gnu aarch64-macos aarch64-linux-gnu; do
      "$ZIG" cc -target "$target" -w -DXRMC_NO_MAIN -I "src-$v" -S -o "off-$v-$target.s" off.c
      vals=$(awk '/^_?xrmc_values:/{f=1;next} f&&/\.(long|word)[ \t]/{v=$2; if (v>2147483647) v-=4294967296; print v; next} f{exit}' "off-$v-$target.s")
      if [ "$vals" = "$("./off-$v")" ]; then
        echo "  $target: identical"
      else
        echo "  $target: DIFFERENT"
        print_table <<< "$vals"
      fi
    done
  fi
done
