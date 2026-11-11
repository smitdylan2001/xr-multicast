#!/usr/bin/env bash
# Prints offsetof() for the AVPacket/AVFrame fields src/ffmpeg/sys.rs relies on,
# compiled against the official headers of each supported FFmpeg release.
# Compare with the `layouts_match_ffmpeg_headers` unit test.
# Needs git, gcc and network access; nothing is installed.
set -euo pipefail
WORK=${1:-/tmp/xrmc-ffmpeg-headers}
mkdir -p "$WORK"
cd "$WORK"
cat > off.c <<'EOF'
#include <stdio.h>
#include <stddef.h>
#include "libavutil/frame.h"
#include "libavcodec/packet.h"
#define O(t, f) printf(" " #t "." #f "=%zu", offsetof(t, f))
int main(void) {
    printf("libavutil %d:", LIBAVUTIL_VERSION_MAJOR);
    O(AVPacket, pts); O(AVPacket, data); O(AVPacket, size); O(AVPacket, flags);
    O(AVFrame, linesize); O(AVFrame, width); O(AVFrame, height); O(AVFrame, format);
    O(AVFrame, pts); O(AVFrame, flags); O(AVFrame, color_range); O(AVFrame, colorspace);
    printf("\n");
    return 0;
}
EOF
for v in 5.1 6.0 6.1 7.0 7.1 8.0 8.1; do
  if [ ! -d "src-$v" ]; then
    git clone -q --depth 1 --branch "release/$v" --filter=blob:none --sparse https://github.com/FFmpeg/FFmpeg.git "src-$v"
    (cd "src-$v" && git sparse-checkout set libavutil libavcodec)
    printf '#define AV_HAVE_BIGENDIAN 0\n#define AV_HAVE_FAST_UNALIGNED 1\n' > "src-$v/libavutil/avconfig.h"
  fi
  gcc -w -I "src-$v" off.c -o "off-$v"
  printf 'FFmpeg %s ' "$v"
  "./off-$v"
done
