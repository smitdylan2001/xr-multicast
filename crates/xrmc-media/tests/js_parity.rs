//! Differential tests against the Electron app's JavaScript reference.
//!
//! The fixtures are produced by running the JS (`decoder.js`,
//! `mse-decoder.js`) with node: see `tests/fixtures/generate.js`.

use xrmc_media::{Fmp4Muxer, VideoCodec, describe_config, fmp4, h264};

fn unhex(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn records(text: &str) -> impl Iterator<Item = Vec<&str>> {
    text.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.split(' ').collect())
}

#[test]
fn h264_sps_matches_js() {
    let mut n = 0;
    for f in records(include_str!("fixtures/h264-sps.txt")) {
        let data = unhex(f[0]);
        let d = describe_config(VideoCodec::H264, &data);
        assert_eq!(d.codec, f[1], "{}", f[0]);
        let cfg = h264::parse_config(&data).unwrap();
        assert_eq!(cfg.codec, f[1]);
        let info = cfg.info;
        let expect: Vec<u32> = f[2..7].iter().map(|v| v.parse().unwrap()).collect();
        assert_eq!(
            [
                info.width,
                info.height,
                info.profile.into(),
                info.level.into(),
                info.bit_depth_luma.into()
            ],
            expect[..],
            "{}",
            f[0]
        );
        assert_eq!(d.dimensions, Some((info.width, info.height)));
        assert_eq!(u32::from(d.bit_depth), expect[4]);
        n += 1;
    }
    assert!(n >= 100);
}

#[test]
fn h265_codec_strings_match_js() {
    let mut n = 0;
    for f in records(include_str!("fixtures/h265-sps.txt")) {
        let d = describe_config(VideoCodec::H265, &unhex(f[0]));
        assert_eq!(d.codec, f[1], "{}", f[0]);
        assert_eq!(d.bit_depth.to_string(), f[2], "{}", f[0]);
        assert_eq!(
            d.dimensions,
            Some((f[3].parse().unwrap(), f[4].parse().unwrap())),
            "{}",
            f[0]
        );
        n += 1;
    }
    assert!(n >= 100);
}

#[test]
fn fmp4_boxes_are_byte_identical_to_js() {
    let (mut inits, mut frags, mut samples) = (0, 0, 0);
    for f in records(include_str!("fixtures/fmp4.txt")) {
        match f[0] {
            "init" => {
                let cfg = h264::parse_config(&unhex(f[1])).unwrap();
                assert_eq!(
                    fmp4::init_segment(&cfg),
                    unhex(f[2]),
                    "init segment for {}",
                    f[1]
                );
                inits += 1;
            }
            "fragment" => {
                let frag = fmp4::fragment(
                    f[1].parse().unwrap(),
                    f[2].parse().unwrap(),
                    f[3].parse().unwrap(),
                    &unhex(f[5]),
                    f[4] == "1",
                );
                assert_eq!(frag, unhex(f[6]));
                frags += 1;
            }
            "sample" => {
                assert_eq!(
                    fmp4::to_avcc_sample(&unhex(f[1])),
                    unhex(f[2]),
                    "sample {}",
                    f[1]
                );
                samples += 1;
            }
            _ => {}
        }
    }
    assert!(inits > 0 && frags > 0 && samples > 0);
}

/// Replays packet sequences through [`Fmp4Muxer`] and compares its output
/// with what the JS `MseDecoder.push()` appended to its SourceBuffer.
#[test]
fn fmp4_muxer_session_is_byte_identical_to_js() {
    struct Push {
        config: bool,
        key: bool,
        pts: u64,
        data: Vec<u8>,
        expected: Vec<Vec<u8>>,
    }
    let mut sessions: Vec<(String, Vec<Push>)> = Vec::new();
    for f in records(include_str!("fixtures/fmp4.txt")) {
        match f[0] {
            "session" => sessions.push((f[1].to_owned(), Vec::new())),
            "push" => sessions.last_mut().unwrap().1.push(Push {
                config: f[1] == "1",
                key: f[2] == "1",
                pts: f[3].parse().unwrap(),
                data: unhex(f[4]),
                expected: Vec::new(),
            }),
            "out" => sessions
                .last_mut()
                .unwrap()
                .1
                .last_mut()
                .unwrap()
                .expected
                .push(unhex(f[1])),
            _ => {}
        }
    }
    assert_eq!(sessions.len(), 2);
    for (name, pushes) in sessions {
        let mut m = Fmp4Muxer::new();
        let mut produced = 0;
        for (i, p) in pushes.iter().enumerate() {
            let out = m.push(p.config, p.key, p.pts, &p.data).unwrap();
            let got: Vec<Vec<u8>> = out
                .init_segment
                .into_iter()
                .chain(out.media_segment)
                .collect();
            assert_eq!(got, p.expected, "session {name}, packet {i}");
            produced += got.len();
        }
        assert!(produced > 2, "session {name} produced too little");
    }
}
