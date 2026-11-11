'use strict';

// Compressed video -> VideoFrame using the browser's WebCodecs decoder
// (hardware accelerated where the GPU/driver supports it).
//
// Supported stream families (what scrcpy-server sends):
//   h264 / h265: Annex B NAL units; the config packet holds SPS/PPS (+VPS) and
//                is prepended to every key frame so the decoder can (re)start.
//   av1:         low-overhead OBU stream; the config packet is an av1C record
//                (used as the decoder description) or raw sequence-header OBUs.

(function () {
  const hex2 = (n) => n.toString(16).padStart(2, '0');

  const DEFAULT_CODEC = {
    h264: 'avc1.640033', // High, level 5.1
    h265: 'hvc1.1.6.L153.B0', // Main, level 5.1
    av1: 'av01.0.13M.08', // Main, level 5.1, 8-bit
  };

  /** Offsets of NAL unit payloads (just after each start code) in an Annex B buffer. */
  function nalOffsets(data) {
    const out = [];
    for (let i = 0; i + 3 < data.length; i++) {
      if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
        out.push(i + 3);
        i += 2;
      }
    }
    return out;
  }

  /** Strip emulation-prevention bytes (00 00 03 -> 00 00) from the first `max` bytes of a NAL unit. */
  function rbsp(data, start, max) {
    const out = [];
    let zeros = 0;
    for (let i = start; i < data.length && out.length < max; i++) {
      const b = data[i];
      if (zeros >= 2 && b === 3) {
        zeros = 0;
        continue;
      }
      zeros = b === 0 ? zeros + 1 : 0;
      out.push(b);
    }
    return out;
  }

  function h264Codec(data) {
    for (const off of nalOffsets(data)) {
      if ((data[off] & 0x1f) === 7 && off + 3 < data.length) {
        return `avc1.${hex2(data[off + 1])}${hex2(data[off + 2])}${hex2(data[off + 3])}`;
      }
    }
    return DEFAULT_CODEC.h264;
  }

  /** MSB-first bit reader with Exp-Golomb support, over an RBSP byte array. */
  function bitReader(bytes) {
    let pos = 0;
    const bit = () => {
      if (pos >= bytes.length * 8) throw new Error('SPS truncated');
      const v = (bytes[pos >> 3] >> (7 - (pos & 7))) & 1;
      pos++;
      return v;
    };
    const bits = (n) => {
      let v = 0;
      for (let i = 0; i < n; i++) v = v * 2 + bit();
      return v;
    };
    const ue = () => {
      let zeros = 0;
      while (bit() === 0 && zeros < 32) zeros++;
      return 2 ** zeros - 1 + bits(zeros);
    };
    return { bits, ue, skip: (n) => (pos += n) };
  }

  /** Luma bit depth from an HEVC SPS RBSP (starting after the 2-byte NAL header). */
  function h265BitDepth(b) {
    try {
      const r = bitReader(b);
      r.skip(4); // sps_video_parameter_set_id
      const maxSubLayersMinus1 = r.bits(3);
      r.skip(1);
      r.skip(96); // general profile_tier_level + level_idc
      const subProfile = [];
      const subLevel = [];
      for (let i = 0; i < maxSubLayersMinus1; i++) {
        subProfile.push(r.bits(1));
        subLevel.push(r.bits(1));
      }
      if (maxSubLayersMinus1 > 0) r.skip(2 * (8 - maxSubLayersMinus1));
      for (let i = 0; i < maxSubLayersMinus1; i++) {
        if (subProfile[i]) r.skip(88);
        if (subLevel[i]) r.skip(8);
      }
      r.ue(); // sps_seq_parameter_set_id
      if (r.ue() === 3) r.skip(1); // chroma_format_idc, separate_colour_plane_flag
      r.ue(); // pic_width_in_luma_samples
      r.ue(); // pic_height_in_luma_samples
      if (r.bits(1)) for (let i = 0; i < 4; i++) r.ue(); // conformance window
      return r.ue() + 8;
    } catch {
      return 8;
    }
  }

  /** RFC 6381 / ISO 14496-15 Annex E codec string from an HEVC SPS, plus the luma bit depth. */
  function h265Info(data) {
    for (const off of nalOffsets(data)) {
      if (((data[off] >> 1) & 0x3f) !== 33) continue; // SPS
      const b = rbsp(data, off + 2, 96);
      if (b.length < 13) break;
      const space = b[1] >> 6;
      const tier = (b[1] >> 5) & 1;
      let profile = b[1] & 0x1f;
      const compat = ((b[2] << 24) | (b[3] << 16) | (b[4] << 8) | b[5]) >>> 0;
      let reversed = 0;
      for (let i = 0; i < 32; i++) reversed = ((reversed << 1) | ((compat >>> i) & 1)) >>> 0;
      const constraints = b.slice(6, 12);
      while (constraints.length && constraints[constraints.length - 1] === 0) constraints.pop();
      const cons = constraints.map((c) => `.${c.toString(16).toUpperCase()}`).join('');
      const bitDepth = h265BitDepth(b);
      // Some hardware encoders write 10-bit streams labelled as Main (8-bit);
      // decoders pick their pipeline from this label, so correct it to Main 10.
      if (bitDepth > 8 && profile === 1) {
        profile = 2;
        reversed = 0x4;
      }
      return { codec: `hvc1.${['', 'A', 'B', 'C'][space]}${profile}.${reversed.toString(16).toUpperCase()}.${tier ? 'H' : 'L'}${b[12]}${cons}`, bitDepth };
    }
    return { codec: DEFAULT_CODEC.h265, bitDepth: 8 };
  }

  function av1BitDepth(rec) {
    return (rec[2] >> 5) & 1 ? 12 : (rec[2] >> 6) & 1 ? 10 : 8;
  }

  /** Codec string from an av1C record (AV1CodecConfigurationRecord). */
  function av1Codec(rec) {
    const profile = rec[1] >> 5;
    const level = rec[1] & 0x1f;
    const tier = rec[2] >> 7;
    const high = (rec[2] >> 6) & 1;
    const twelve = (rec[2] >> 5) & 1;
    const depth = twelve ? 12 : high ? 10 : 8;
    return `av01.${profile}.${String(level).padStart(2, '0')}${tier ? 'H' : 'M'}.${String(depth).padStart(2, '0')}`;
  }

  /** @returns {{codec: string, description?: Uint8Array, prepend: boolean, bitDepth: number}} */
  function describeConfig(family, data) {
    if (family === 'h265') return { ...h265Info(data), prepend: true };
    if (family === 'av1') {
      if (data.length >= 4 && data[0] === 0x81) return { codec: av1Codec(data), description: data, prepend: false, bitDepth: av1BitDepth(data) };
      return { codec: DEFAULT_CODEC.av1, prepend: true, bitDepth: 8 };
    }
    return { codec: h264Codec(data), prepend: true, bitDepth: 8 };
  }

  class StreamDecoder {
    constructor({ onFrame, onError, family = 'h264' }) {
      this.onFrame = onFrame;
      this.onError = onError || (() => {});
      this.family = family;
      this.decoder = null;
      this.codec = null;
      this.description = null;
      this.prefix = null;
      this.waitKey = true;
      this.errors = 0;
      this.hw = 'no-preference';
      this.dropped = 0;
      this.closed = false;
    }

    static get supported() {
      return typeof window.VideoDecoder === 'function';
    }

    setFamily(family) {
      if (!family || family === this.family) return;
      this.family = family;
      this.closeDecoder();
      this.codec = null;
      this.description = null;
      this.prefix = null;
      this.waitKey = true;
    }

    configure(codec) {
      this.closeDecoder();
      this.codec = codec;
      this.decoder = new VideoDecoder({
        output: (frame) => {
          if (this.closed) frame.close();
          else this.onFrame(frame);
        },
        error: (e) => this.handleError(e),
      });
      const cfg = { codec, optimizeForLatency: true, hardwareAcceleration: this.hw };
      if (this.description) cfg.description = this.description;
      this.decoder.configure(cfg);
      this.waitKey = true;
    }

    handleError(e) {
      if (this.closed) return;
      this.errors++;
      // If the hardware path keeps failing (driver limits on huge frames etc.), fall back to software.
      if (this.errors >= 2 && this.hw !== 'prefer-software') this.hw = 'prefer-software';
      this.onError(e, this.errors);
      if (this.codec && !this.closed) {
        try {
          this.configure(this.codec);
        } catch (err) {
          this.onError(err, this.errors);
        }
      }
    }

    push(isConfig, isKey, pts, data) {
      if (this.closed) return;
      if (isConfig) {
        const d = describeConfig(this.family, data);
        this.bitDepth = d.bitDepth;
        this.prefix = d.prepend ? data : null;
        this.description = d.description || null;
        if (d.codec !== this.codec || !this.decoder || this.decoder.state === 'closed' || d.description) this.configure(d.codec);
        else this.waitKey = true;
        return;
      }
      if ((!this.decoder || this.decoder.state === 'closed') && isKey) {
        // No config packet seen (some encoders put parameters in-band only).
        this.configure(DEFAULT_CODEC[this.family] || DEFAULT_CODEC.h264);
      }
      if (!this.decoder || this.decoder.state !== 'configured') return;
      if (this.waitKey && !isKey) {
        this.dropped++;
        return;
      }
      // Decoder falling behind: skip to the next key frame rather than build up latency.
      if (!isKey && this.decoder.decodeQueueSize > 24) {
        this.waitKey = true;
        this.dropped++;
        return;
      }
      let payload = data;
      if (isKey && this.prefix) {
        payload = new Uint8Array(this.prefix.length + data.length);
        payload.set(this.prefix, 0);
        payload.set(data, this.prefix.length);
      }
      try {
        this.decoder.decode(new EncodedVideoChunk({ type: isKey ? 'key' : 'delta', timestamp: pts, data: payload }));
        this.waitKey = false;
      } catch (e) {
        this.handleError(e);
      }
    }

    closeDecoder() {
      if (this.decoder && this.decoder.state !== 'closed') {
        try {
          this.decoder.close();
        } catch {
          /* ignore */
        }
      }
      this.decoder = null;
    }

    close() {
      this.closed = true;
      this.closeDecoder();
    }
  }

  const TEN_BIT_CODEC = {
    h265_10: 'hvc1.2.4.L153.B0', // Main 10, level 5.1
    av1_10: 'av01.0.13M.10', // Main, level 5.1, 10-bit
  };

  /** What this computer can decode: { h264: {any, hw}, h265, av1, h265_10, av1_10 } */
  async function probeDecoders() {
    const out = {};
    if (!StreamDecoder.supported) return out;
    for (const [family, codec] of Object.entries({ ...DEFAULT_CODEC, ...TEN_BIT_CODEC })) {
      const base = { codec, codedWidth: 2560, codedHeight: 1440, optimizeForLatency: true };
      const check = (cfg) => VideoDecoder.isConfigSupported(cfg).then((r) => !!r.supported, () => false);
      const [any, hw] = await Promise.all([check(base), check({ ...base, hardwareAcceleration: 'prefer-hardware' })]);
      out[family] = { any, hw };
    }
    return out;
  }

  window.StreamDecoder = StreamDecoder;
  window.probeDecoders = probeDecoders;
  window.codecFromConfig = (data, family = 'h264') => describeConfig(family, data).codec;
  window.describeVideoConfig = describeConfig;
})();
