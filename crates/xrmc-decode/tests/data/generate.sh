#!/usr/bin/env bash
# Regenerates the synthetic test bitstreams (GStreamer with x264, x265 and VA-API AV1).
# The emulator_* files are real scrcpy recordings from an Android 15 emulator
# (emulator_1280x800.h264 is the first 28 frames of a longer capture).
set -euo pipefail
cd "$(dirname "$0")"

SRC="videotestsrc pattern=smpte num-buffers=20"
CAPS="width=320,height=240,framerate=30/1"

gst-launch-1.0 -q $SRC ! "video/x-raw,$CAPS,format=I420" \
  ! x264enc tune=zerolatency key-int-max=8 bframes=0 bitrate=500 \
  ! video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au ! filesink location=h264_baseline.h264
gst-launch-1.0 -q $SRC ! "video/x-raw,$CAPS,format=I420" \
  ! x264enc tune=zerolatency key-int-max=8 bframes=0 bitrate=500 \
  ! video/x-h264,profile=high,stream-format=byte-stream,alignment=au ! filesink location=h264_high.h264
gst-launch-1.0 -q videotestsrc pattern=smpte num-buffers=4 ! "video/x-raw,$CAPS,format=I420,colorimetry=(string)1:3:5:1" \
  ! x264enc tune=zerolatency key-int-max=8 bframes=0 bitrate=300 \
  ! video/x-h264,profile=high,stream-format=byte-stream,alignment=au ! filesink location=h264_fullrange_bt709.h264
gst-launch-1.0 -q $SRC ! "video/x-raw,$CAPS,format=I420" \
  ! x265enc tune=zerolatency key-int-max=8 bitrate=500 \
  ! video/x-h265,profile=main,stream-format=byte-stream,alignment=au ! filesink location=h265_main.h265
gst-launch-1.0 -q $SRC ! "video/x-raw,$CAPS,format=I420_10LE" \
  ! x265enc tune=zerolatency key-int-max=8 bitrate=500 \
  ! video/x-h265,profile=main-10,stream-format=byte-stream,alignment=au ! filesink location=h265_main10.h265
gst-launch-1.0 -q $SRC ! "video/x-raw,$CAPS,format=NV12" ! vaav1enc key-int-max=8 \
  ! av1parse ! video/x-av1,stream-format=obu-stream,alignment=tu ! filesink location=av1_8.obu
gst-launch-1.0 -q $SRC ! "video/x-raw,$CAPS,format=P010_10LE" ! vaav1enc key-int-max=8 \
  ! av1parse ! video/x-av1,stream-format=obu-stream,alignment=tu ! filesink location=av1_10.obu
