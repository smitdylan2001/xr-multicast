//! Copies MediaCodec ByteBuffer output (any YUV 4:2:0 layout) into a [`Frame`].
//!
//! The layout comes from the output format: the `image-data` buffer (a
//! `MediaImage2` struct, set by Codec2 for raw video) when present, else
//! `color-format` + `stride` + `slice-height`. Pure Rust so it is unit-tested on
//! the host.

use crate::{ColorInfo, Frame, PixelFormat, Plane};

/// `MediaCodecInfo.CodecCapabilities` colour formats.
pub const COLOR_YUV420_PLANAR: i32 = 19;
pub const COLOR_YUV420_SEMIPLANAR: i32 = 21;
pub const COLOR_YUV420_PACKED_SEMIPLANAR: i32 = 39;
pub const COLOR_YUV_P010: i32 = 54;
pub const COLOR_YUV420_FLEXIBLE: i32 = 0x7F42_0888;
/// Qualcomm's tiled/aligned NV12 variants (`QOMX_COLOR_FormatYUV420PackedSemiPlanar32m`, ...),
/// which are plain NV12 with aligned strides in ByteBuffer mode.
pub const COLOR_QCOM_NV12_32M: i32 = 0x7FA3_0C04;

/// Where one plane's samples are in the buffer (bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneLayout {
    pub offset: usize,
    /// Bytes from one sample to the next in a row (1/2 for 8-bit planar/semi-planar).
    pub col_inc: usize,
    /// Bytes from one row to the next.
    pub row_inc: usize,
}

/// A 4:2:0 YUV buffer layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Full (allocated) picture size in samples.
    pub width: u32,
    pub height: u32,
    /// Significant bits per sample (8 or 10).
    pub bit_depth: u8,
    /// 2 when samples are 16-bit containers.
    pub bytes_per_sample: usize,
    /// 16-bit samples have their value in the high bits (P010).
    pub msb_aligned: bool,
    pub planes: [PlaneLayout; 3],
}

/// Visible rectangle inside the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crop {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}

fn u32_at(d: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(d.get(i..i + 4)?.try_into().ok()?))
}

/// Parses a `MediaImage2` (`image-data`): YUV with 3 planes and 2×2 chroma subsampling only.
pub fn from_media_image2(d: &[u8]) -> Option<Layout> {
    const MEDIA_IMAGE_TYPE_YUV: u32 = 1;
    if u32_at(d, 0)? != MEDIA_IMAGE_TYPE_YUV || u32_at(d, 4)? != 3 {
        return None;
    }
    let (width, height) = (u32_at(d, 8)?, u32_at(d, 12)?);
    let (depth, allocated) = (u32_at(d, 16)?, u32_at(d, 20)?);
    let bytes = match allocated {
        8 => 1,
        16 => 2,
        _ => return None,
    };
    if !(8..=10).contains(&depth) || width == 0 || height == 0 {
        return None;
    }
    let mut planes = [PlaneLayout {
        offset: 0,
        col_inc: 0,
        row_inc: 0,
    }; 3];
    for (i, p) in planes.iter_mut().enumerate() {
        let base = 24 + i * 20;
        let col = u32_at(d, base + 4)? as i32;
        let row = u32_at(d, base + 8)? as i32;
        let (hs, vs) = (u32_at(d, base + 12)?, u32_at(d, base + 16)?);
        let want = if i == 0 { 1 } else { 2 };
        if col <= 0 || row <= 0 || hs != want || vs != want {
            return None;
        }
        *p = PlaneLayout {
            offset: u32_at(d, base)? as usize,
            col_inc: col as usize,
            row_inc: row as usize,
        };
    }
    let semi = planes[1].col_inc == 2 * bytes;
    Some(Layout {
        width,
        height,
        bit_depth: depth as u8,
        bytes_per_sample: bytes,
        msb_aligned: bytes == 2 && semi,
        planes,
    })
}

/// Layout from `color-format`, `stride` and `slice-height` (no `image-data`).
pub fn from_color_format(
    color_format: i32,
    width: u32,
    height: u32,
    stride: u32,
    slice_height: u32,
) -> Option<Layout> {
    if width == 0 || height == 0 {
        return None;
    }
    let stride = stride.max(width) as usize;
    let slice = slice_height.max(height) as usize;
    let y = PlaneLayout {
        offset: 0,
        col_inc: 1,
        row_inc: stride,
    };
    let (planes, depth, bytes) = match color_format {
        COLOR_YUV420_PLANAR => {
            let c = stride / 2;
            let u = stride * slice;
            let v = u + c * slice.div_ceil(2);
            (
                [
                    y,
                    PlaneLayout {
                        offset: u,
                        col_inc: 1,
                        row_inc: c,
                    },
                    PlaneLayout {
                        offset: v,
                        col_inc: 1,
                        row_inc: c,
                    },
                ],
                8,
                1,
            )
        }
        COLOR_YUV420_SEMIPLANAR
        | COLOR_YUV420_PACKED_SEMIPLANAR
        | COLOR_YUV420_FLEXIBLE
        | COLOR_QCOM_NV12_32M => {
            let uv = stride * slice;
            (
                [
                    y,
                    PlaneLayout {
                        offset: uv,
                        col_inc: 2,
                        row_inc: stride,
                    },
                    PlaneLayout {
                        offset: uv + 1,
                        col_inc: 2,
                        row_inc: stride,
                    },
                ],
                8,
                1,
            )
        }
        COLOR_YUV_P010 => {
            // `stride` is in bytes for P010.
            let stride = stride.max(width as usize * 2);
            let uv = stride * slice;
            (
                [
                    PlaneLayout {
                        offset: 0,
                        col_inc: 2,
                        row_inc: stride,
                    },
                    PlaneLayout {
                        offset: uv,
                        col_inc: 4,
                        row_inc: stride,
                    },
                    PlaneLayout {
                        offset: uv + 2,
                        col_inc: 4,
                        row_inc: stride,
                    },
                ],
                10,
                2,
            )
        }
        _ => return None,
    };
    Some(Layout {
        width,
        height,
        bit_depth: depth,
        bytes_per_sample: bytes,
        msb_aligned: bytes == 2,
        planes,
    })
}

fn last_index(p: &PlaneLayout, x0: usize, y0: usize, w: usize, h: usize, bytes: usize) -> usize {
    p.offset + (y0 + h - 1) * p.row_inc + (x0 + w - 1) * p.col_inc + bytes
}

/// Copy the visible part of `buf` into a frame: NV12 / I420 for 8-bit, P010 (or I420P10) for 10-bit.
pub fn to_frame(buf: &[u8], l: &Layout, crop: Crop, color: ColorInfo, pts: u64) -> Option<Frame> {
    let (w, h) = (crop.width as usize, crop.height as usize);
    if w == 0 || h == 0 || crop.left + crop.width > l.width || crop.top + crop.height > l.height {
        return None;
    }
    let (x0, y0) = (crop.left as usize, crop.top as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (cx0, cy0) = (x0 / 2, y0 / 2);
    let b = l.bytes_per_sample;
    let [py, pu, pv] = l.planes;
    if last_index(&py, x0, y0, w, h, b) > buf.len()
        || last_index(&pu, cx0, cy0, cw, ch, b) > buf.len()
        || last_index(&pv, cx0, cy0, cw, ch, b) > buf.len()
    {
        return None;
    }
    let plane = |data: Vec<u8>, stride: usize, pw: usize, ph: usize| Plane {
        data,
        stride,
        width: pw as u32,
        height: ph as u32,
    };
    if b == 1 {
        let rows = |p: &PlaneLayout, x: usize, y: usize, ph: usize, bytes: usize| {
            let mut out = Vec::with_capacity(bytes * ph);
            for r in 0..ph {
                let s = p.offset + (y + r) * p.row_inc + x * p.col_inc;
                out.extend_from_slice(&buf[s..s + bytes]);
            }
            out
        };
        let y = if py.col_inc == 1 {
            rows(&py, x0, y0, h, w)
        } else {
            gather8(buf, &py, x0, y0, w, h)
        };
        let nv12 = pu.col_inc == 2
            && pv.col_inc == 2
            && pv.offset == pu.offset + 1
            && pu.row_inc == pv.row_inc;
        if nv12 {
            let uv = rows(&pu, cx0, cy0, ch, 2 * cw);
            return Some(Frame {
                width: w as u32,
                height: h as u32,
                format: PixelFormat::Nv12,
                color,
                planes: vec![plane(y, w, w, h), plane(uv, 2 * cw, cw, ch)],
                pts,
            });
        }
        let u = gather8(buf, &pu, cx0, cy0, cw, ch);
        let v = gather8(buf, &pv, cx0, cy0, cw, ch);
        return Some(Frame {
            width: w as u32,
            height: h as u32,
            format: PixelFormat::I420,
            color,
            planes: vec![
                plane(y, w, w, h),
                plane(u, cw, cw, ch),
                plane(v, cw, cw, ch),
            ],
            pts,
        });
    }
    // P010 (what MediaCodec gives for 10-bit): copy as is (values in the high 10 bits).
    let p010 = l.msb_aligned
        && l.bit_depth == 10
        && py.col_inc == 2
        && pu.col_inc == 4
        && pv.col_inc == 4
        && pv.offset == pu.offset + 2
        && pu.row_inc == pv.row_inc;
    if p010 {
        let rows = |p: &PlaneLayout, x: usize, y: usize, ph: usize, bytes: usize| {
            let mut out = Vec::with_capacity(bytes * ph);
            for r in 0..ph {
                let s = p.offset + (y + r) * p.row_inc + x * p.col_inc;
                out.extend_from_slice(&buf[s..s + bytes]);
            }
            out
        };
        return Some(Frame {
            width: w as u32,
            height: h as u32,
            format: PixelFormat::P010,
            color,
            planes: vec![
                plane(rows(&py, x0, y0, h, 2 * w), 2 * w, w, h),
                plane(rows(&pu, cx0, cy0, ch, 4 * cw), 4 * cw, cw, ch),
            ],
            pts,
        });
    }
    // Other 16-bit layouts → I420P10 (little-endian u16, 0..1023).
    let shift = if l.msb_aligned {
        16 - u32::from(l.bit_depth)
    } else {
        0
    };
    let down = u32::from(l.bit_depth).saturating_sub(10);
    let gather16 = |p: &PlaneLayout, x: usize, y: usize, pw: usize, ph: usize| {
        let mut out = Vec::with_capacity(pw * ph * 2);
        for r in 0..ph {
            let row = p.offset + (y + r) * p.row_inc + x * p.col_inc;
            for c in 0..pw {
                let i = row + c * p.col_inc;
                let v = u16::from_le_bytes([buf[i], buf[i + 1]]);
                let v = ((u32::from(v) >> shift) >> down).min(1023) as u16;
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        out
    };
    Some(Frame {
        width: w as u32,
        height: h as u32,
        format: PixelFormat::I420P10,
        color,
        planes: vec![
            plane(gather16(&py, x0, y0, w, h), 2 * w, w, h),
            plane(gather16(&pu, cx0, cy0, cw, ch), 2 * cw, cw, ch),
            plane(gather16(&pv, cx0, cy0, cw, ch), 2 * cw, cw, ch),
        ],
        pts,
    })
}

fn gather8(buf: &[u8], p: &PlaneLayout, x: usize, y: usize, pw: usize, ph: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(pw * ph);
    for r in 0..ph {
        let row = p.offset + (y + r) * p.row_inc + x * p.col_inc;
        out.extend((0..pw).map(|c| buf[row + c * p.col_inc]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image2(
        planes: [(u32, i32, i32, u32); 3],
        w: u32,
        h: u32,
        depth: u32,
        alloc: u32,
    ) -> Vec<u8> {
        let mut d = Vec::new();
        for v in [1u32, 3, w, h, depth, alloc] {
            d.extend_from_slice(&v.to_ne_bytes());
        }
        for (off, col, row, sub) in planes {
            d.extend_from_slice(&off.to_ne_bytes());
            d.extend_from_slice(&col.to_ne_bytes());
            d.extend_from_slice(&row.to_ne_bytes());
            d.extend_from_slice(&sub.to_ne_bytes());
            d.extend_from_slice(&sub.to_ne_bytes());
        }
        d.extend_from_slice(&[0; 20]);
        d
    }

    const C: ColorInfo = ColorInfo {
        bt709: true,
        full_range: false,
    };

    #[test]
    fn nv12_with_stride_and_crop() {
        // 6×4 picture in an 8-wide buffer, 4 rows; NV12.
        let (stride, rows) = (8usize, 4usize);
        let mut buf = vec![0u8; stride * rows * 3 / 2];
        for r in 0..rows {
            for c in 0..stride {
                buf[r * stride + c] = (r * 10 + c) as u8;
            }
        }
        for r in 0..rows / 2 {
            for c in 0..stride {
                buf[stride * rows + r * stride + c] = 100 + (r * 10 + c) as u8;
            }
        }
        let l = from_color_format(COLOR_YUV420_SEMIPLANAR, 8, 4, 8, 4).unwrap();
        let crop = Crop {
            left: 0,
            top: 0,
            width: 6,
            height: 4,
        };
        let f = to_frame(&buf, &l, crop, C, 7).unwrap();
        assert_eq!(f.format, PixelFormat::Nv12);
        assert_eq!(f.planes[0].data[..6], [0, 1, 2, 3, 4, 5]);
        assert_eq!(f.planes[0].data[6..12], [10, 11, 12, 13, 14, 15]);
        assert_eq!(f.planes[1].data[..6], [100, 101, 102, 103, 104, 105]);
        assert_eq!(f.planes[1].stride, 6);
        // Same buffer described by MediaImage2.
        let im = image2([(0, 1, 8, 1), (32, 2, 8, 2), (33, 2, 8, 2)], 8, 4, 8, 8);
        assert_eq!(from_media_image2(&im).unwrap(), l);
    }

    #[test]
    fn planar_and_nv21_become_i420() {
        let l = from_color_format(COLOR_YUV420_PLANAR, 4, 2, 4, 2).unwrap();
        let buf: Vec<u8> = (0..12).collect();
        let f = to_frame(
            &buf,
            &l,
            Crop {
                left: 0,
                top: 0,
                width: 4,
                height: 2,
            },
            C,
            0,
        )
        .unwrap();
        assert_eq!(f.format, PixelFormat::I420);
        assert_eq!(f.planes[1].data, [8, 9]);
        assert_eq!(f.planes[2].data, [10, 11]);
        // NV21 (V first).
        let im = image2([(0, 1, 4, 1), (9, 2, 4, 2), (8, 2, 4, 2)], 4, 2, 8, 8);
        let l = from_media_image2(&im).unwrap();
        let f = to_frame(
            &buf,
            &l,
            Crop {
                left: 0,
                top: 0,
                width: 4,
                height: 2,
            },
            C,
            0,
        )
        .unwrap();
        assert_eq!(f.format, PixelFormat::I420);
        assert_eq!(f.planes[1].data, [9, 11]);
        assert_eq!(f.planes[2].data, [8, 10]);
    }

    #[test]
    fn p010_stays_p010() {
        let l = from_color_format(COLOR_YUV_P010, 2, 2, 4, 2).unwrap();
        let buf: Vec<u8> = (0..12).collect();
        let crop = Crop {
            left: 0,
            top: 0,
            width: 2,
            height: 2,
        };
        let f = to_frame(&buf, &l, crop, C, 0).unwrap();
        assert_eq!(f.format, PixelFormat::P010);
        assert_eq!(f.planes[0].data, (0..8).collect::<Vec<u8>>());
        assert_eq!(f.planes[1].data, [8, 9, 10, 11]);
    }

    #[test]
    fn planar_16_bit_to_i420p10() {
        // Planar 16-bit, MSB-aligned (as a MediaImage2 would describe it).
        let im = image2([(0, 2, 4, 1), (8, 2, 2, 2), (10, 2, 2, 2)], 2, 2, 10, 16);
        let mut l = from_media_image2(&im).unwrap();
        l.msb_aligned = true;
        let s = |v: u16| (v << 6).to_le_bytes();
        let mut buf = Vec::new();
        for v in [1u16, 2, 3, 1023] {
            buf.extend_from_slice(&s(v));
        }
        buf.extend_from_slice(&s(512));
        buf.extend_from_slice(&s(600));
        let f = to_frame(
            &buf,
            &l,
            Crop {
                left: 0,
                top: 0,
                width: 2,
                height: 2,
            },
            C,
            0,
        )
        .unwrap();
        assert_eq!(f.format, PixelFormat::I420P10);
        let y: Vec<u16> = f.planes[0]
            .data
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(y, [1, 2, 3, 1023]);
        assert_eq!(f.planes[1].data, 512u16.to_le_bytes());
        assert_eq!(f.planes[2].data, 600u16.to_le_bytes());
    }

    #[test]
    fn rejects_short_buffers_and_bad_crops() {
        let l = from_color_format(COLOR_YUV420_SEMIPLANAR, 8, 4, 8, 4).unwrap();
        let crop = Crop {
            left: 0,
            top: 0,
            width: 8,
            height: 4,
        };
        assert!(to_frame(&[0; 10], &l, crop, C, 0).is_none());
        let big = Crop {
            left: 2,
            top: 0,
            width: 8,
            height: 4,
        };
        assert!(to_frame(&[0; 48], &l, big, C, 0).is_none());
        assert!(from_media_image2(&[0; 8]).is_none());
    }
}
