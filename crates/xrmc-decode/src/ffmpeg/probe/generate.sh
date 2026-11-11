#!/usr/bin/env bash
# Regenerates the hardware-probe key frames: one 128x128 SMPTE-bar frame per
# profile (GStreamer with x264, x265 and SVT-AV1). Embedded with include_bytes!.
set -euo pipefail
cd "$(dirname "$0")"
SRC="videotestsrc pattern=smpte num-buffers=1"
CAPS="width=128,height=128,framerate=30/1"
gst-launch-1.0 -q $SRC ! "video/x-raw,$CAPS,format=I420" ! x264enc tune=zerolatency bframes=0 bitrate=300 \
  ! video/x-h264,profile=high,stream-format=byte-stream,alignment=au ! filesink location=h264.h264
gst-launch-1.0 -q $SRC ! "video/x-raw,$CAPS,format=I420" ! x265enc tune=zerolatency bitrate=300 \
  ! video/x-h265,profile=main,stream-format=byte-stream,alignment=au ! filesink location=h265.h265
gst-launch-1.0 -q $SRC ! "video/x-raw,$CAPS,format=I420_10LE" ! x265enc tune=zerolatency bitrate=300 \
  ! video/x-h265,profile=main-10,stream-format=byte-stream,alignment=au ! filesink location=h265_10.h265
gst-launch-1.0 -q $SRC ! "video/x-raw,$CAPS,format=I420" ! svtav1enc \
  ! av1parse ! video/x-av1,stream-format=obu-stream,alignment=tu ! filesink location=av1.obu
gst-launch-1.0 -q $SRC ! "video/x-raw,$CAPS,format=I420_10LE" ! svtav1enc \
  ! av1parse ! video/x-av1,stream-format=obu-stream,alignment=tu ! filesink location=av1_10.obu
