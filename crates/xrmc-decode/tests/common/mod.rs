//! Helpers for the decode tests: split recorded bitstreams into scrcpy-style
//! packets (one config packet with the parameter sets / sequence header, then one
//! access unit per packet with the parameter sets removed), and sample pixels.

#![allow(dead_code)]

use xrmc_decode::{Frame, PixelFormat};

pub fn data(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[derive(Debug, Clone)]
pub struct Packet {
    pub data: Vec<u8>,
    pub key: bool,
    pub config: bool,
}

fn nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let mut end = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
            while end > s && data[end - 1] == 0 {
                end -= 1;
            }
            &data[s..end]
        })
        .collect()
}

/// Split an Annex B H.264 (`hevc = false`) or H.265 stream into scrcpy packets.
pub fn annexb_packets(data: &[u8], hevc: bool) -> Vec<Packet> {
    let nal_type = |n: &[u8]| {
        if hevc {
            (n[0] >> 1) & 0x3f
        } else {
            n[0] & 0x1f
        }
    };
    let is_vcl = |t: u8| if hevc { t < 32 } else { (1..=5).contains(&t) };
    let is_ps = |t: u8| {
        if hevc {
            (32..=34).contains(&t)
        } else {
            t == 7 || t == 8
        }
    };
    let is_idr = |t: u8| if hevc { (16..=21).contains(&t) } else { t == 5 };
    // Non-VCL NALs that start a new access unit when they follow a VCL NAL.
    let starts_au = |t: u8| {
        if hevc {
            (32..=35).contains(&t) || t == 39
        } else {
            (6..=9).contains(&t)
        }
    };
    let first_slice = |n: &[u8]| {
        let hdr = if hevc { 2 } else { 1 };
        n.get(hdr).is_some_and(|b| b & 0x80 != 0)
    };

    let mut aus: Vec<Vec<&[u8]>> = Vec::new();
    let mut cur: Vec<&[u8]> = Vec::new();
    let mut cur_has_vcl = false;
    for n in nals(data).into_iter().filter(|n| !n.is_empty()) {
        let t = nal_type(n);
        let new_au = cur_has_vcl && ((is_vcl(t) && first_slice(n)) || starts_au(t));
        if new_au {
            aus.push(std::mem::take(&mut cur));
            cur_has_vcl = false;
        }
        cur_has_vcl |= is_vcl(t);
        cur.push(n);
    }
    if !cur.is_empty() {
        aus.push(cur);
    }

    let annexb = |list: &[&[u8]]| -> Vec<u8> {
        list.iter()
            .flat_map(|n| [&[0, 0, 0, 1][..], n].concat())
            .collect()
    };
    let mut out = Vec::new();
    let mut last_config = Vec::new();
    for au in aus {
        let (ps, rest): (Vec<&[u8]>, Vec<&[u8]>) = au.into_iter().partition(|n| is_ps(nal_type(n)));
        if !ps.is_empty() {
            let config = annexb(&ps);
            if config != last_config {
                out.push(Packet {
                    data: config.clone(),
                    key: false,
                    config: true,
                });
                last_config = config;
            }
        }
        if rest.iter().any(|n| is_vcl(nal_type(n))) {
            let key = rest.iter().any(|n| is_idr(nal_type(n)));
            out.push(Packet {
                data: annexb(&rest),
                key,
                config: false,
            });
        }
    }
    out
}

fn obus(mut data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let h = data[0];
        let hdr = 1 + usize::from(h & 4 != 0);
        assert!(h & 2 != 0, "OBU without size field");
        let (mut size, mut n) = (0usize, 0);
        loop {
            let b = data[hdr + n];
            size |= usize::from(b & 0x7f) << (7 * n);
            n += 1;
            if b & 0x80 == 0 {
                break;
            }
        }
        let len = hdr + n + size;
        out.push(((h >> 3) & 0xf, &data[..len]));
        data = &data[len..];
    }
    out
}

/// Split a low-overhead AV1 OBU stream into scrcpy packets. The sequence header
/// becomes the config packet (as an `av1C` record if `av1c`, else raw OBUs) and is
/// removed from the key temporal units.
pub fn av1_packets(data: &[u8], av1c: bool, ten_bit: bool) -> Vec<Packet> {
    let mut tus: Vec<Vec<(u8, &[u8])>> = Vec::new();
    for (t, obu) in obus(data) {
        if t == 2 || tus.is_empty() {
            tus.push(Vec::new());
        }
        tus.last_mut().unwrap().push((t, obu));
    }
    let mut out = Vec::new();
    let mut last_seq: Vec<u8> = Vec::new();
    for tu in tus {
        let seq: Vec<u8> = tu
            .iter()
            .filter(|(t, _)| *t == 1)
            .flat_map(|(_, b)| b.iter().copied())
            .collect();
        let key = !seq.is_empty();
        if key && seq != last_seq {
            let config = if av1c {
                [
                    &[0x81, 0x08, if ten_bit { 0x4c } else { 0x0c }, 0x00][..],
                    &seq,
                ]
                .concat()
            } else {
                seq.clone()
            };
            out.push(Packet {
                data: config,
                key: false,
                config: true,
            });
            last_seq = seq;
        }
        let rest: Vec<u8> = tu
            .iter()
            .filter(|(t, _)| *t != 1)
            .flat_map(|(_, b)| b.iter().copied())
            .collect();
        out.push(Packet {
            data: rest,
            key,
            config: false,
        });
    }
    out
}

/// (Y, U, V) at a luma position, normalised to 8 bits.
pub fn yuv_at(f: &Frame, x: u32, y: u32) -> (u8, u8, u8) {
    let (cx, cy) = ((x / 2) as usize, (y / 2) as usize);
    let (x, y) = (x as usize, y as usize);
    let p = &f.planes;
    match f.format {
        PixelFormat::I420 => (
            p[0].data[y * p[0].stride + x],
            p[1].data[cy * p[1].stride + cx],
            p[2].data[cy * p[2].stride + cx],
        ),
        PixelFormat::Nv12 => (
            p[0].data[y * p[0].stride + x],
            p[1].data[cy * p[1].stride + 2 * cx],
            p[1].data[cy * p[1].stride + 2 * cx + 1],
        ),
        PixelFormat::I420P10 => {
            let s = |pl: &xrmc_decode::Plane, x: usize, y: usize| {
                let i = y * pl.stride + 2 * x;
                let v = u16::from_le_bytes([pl.data[i], pl.data[i + 1]]);
                assert!(v < 1024, "10-bit sample out of range: {v}");
                (v >> 2) as u8
            };
            (s(&p[0], x, y), s(&p[1], cx, cy), s(&p[2], cx, cy))
        }
    }
}

/// 100 % SMPTE colour bars (BT.601, limited range) as GStreamer's videotestsrc draws them.
pub const SMPTE_BARS: [(&str, (u8, u8, u8)); 7] = [
    ("white", (235, 128, 128)),
    ("yellow", (210, 16, 146)),
    ("cyan", (170, 166, 16)),
    ("green", (145, 54, 34)),
    ("magenta", (106, 202, 222)),
    ("red", (81, 90, 240)),
    ("blue", (41, 240, 110)),
];

/// Assert that the top part of the frame shows the seven SMPTE bars.
pub fn assert_smpte(f: &Frame) {
    for (i, (name, want)) in SMPTE_BARS.iter().enumerate() {
        let x = ((2 * i as u32 + 1) * f.width) / 14;
        let got = yuv_at(f, x, f.height / 4);
        let close = |a: u8, b: u8| (i32::from(a) - i32::from(b)).abs() <= 10;
        assert!(
            close(got.0, want.0) && close(got.1, want.1) && close(got.2, want.2),
            "{name} bar at x={x}: got {got:?}, want {want:?}"
        );
    }
}

/// Check plane geometry for a frame.
pub fn assert_geometry(f: &Frame, w: u32, h: u32, format: PixelFormat) {
    assert_eq!((f.width, f.height, f.format), (w, h, format));
    let bpp = if format == PixelFormat::I420P10 { 2 } else { 1 };
    let expect: Vec<(u32, u32, usize)> = match format {
        PixelFormat::Nv12 => vec![(w, h, 1), (w.div_ceil(2), h.div_ceil(2), 2)],
        _ => vec![
            (w, h, bpp),
            (w.div_ceil(2), h.div_ceil(2), bpp),
            (w.div_ceil(2), h.div_ceil(2), bpp),
        ],
    };
    assert_eq!(f.planes.len(), expect.len());
    for (p, (pw, ph, b)) in f.planes.iter().zip(expect) {
        assert_eq!((p.width, p.height), (pw, ph));
        assert!(p.stride >= pw as usize * b);
        assert!(p.data.len() >= p.stride * (ph as usize - 1) + pw as usize * b);
    }
}
