// XR MultiCast lens compositor (WGSL port of src/renderer/js/lens-renderer.js).
//
// One draw per tile, inside egui's render pass (or an offscreen pass for snapshots).
// The frame arrives as YUV planes and is converted to RGB here.
//
// For every output pixel q (in units of half the eye height, centred on the lens
// axis) the panel is sampled at
//     d = q · (1 + k1·r² + k2·r⁴) · s_channel,   r = |q|
// which inverts the barrel pre-distortion the headset compositor applied for its
// lenses. s_channel scales red/blue independently to cancel the lateral chromatic
// pre-correction (colour fringes).

struct Tile {
    quad: vec4<f32>,    // drawn box in NDC: x0, y0 (top), x1, y1 (bottom)
    vp0: vec4<f32>,     // (left) eye viewport in framebuffer px: x, y (top-left origin), w, h
    vp1: vec4<f32>,     // right eye viewport (stereo only)
    eye0: vec4<f32>,    // eye region in logical-frame UV (x, y, w, h), y down
    eye1: vec4<f32>,
    center: vec4<f32>,  // lens centre offset of eye0 (xy) and eye1 (zw), -1..1
    lens: vec4<f32>,    // k1, k2, zoom, eye aspect (w / h in pixels)
    chroma: vec4<f32>,  // radial scale r, g, b; output aspect (w / h)
    look: vec4<f32>,    // flip x, flip y, grid (0/1), blur (0/1)
    mode: vec4<u32>,    // 0 = raw / 1 = lens corrected, quarter turns clockwise, stereo (0/1), interleaved UV: NV12 / P010 (0/1)
    yuv_r: vec4<f32>,   // rgb = (dot(yuv_r, s), dot(yuv_g, s), dot(yuv_b, s)), s = (y, u, v, 1)
    yuv_g: vec4<f32>,
    yuv_b: vec4<f32>,
    tex: vec4<f32>,     // sample scale (10-bit in R16 / RG16), chroma uv scale x, y, 1 = linearise output (sRGB target)
};

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_u: texture_2d<f32>;
@group(0) @binding(2) var tex_v: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
@group(1) @binding(0) var<uniform> t: Tile;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let c = corners[vi];
    var out: VsOut;
    out.pos = vec4<f32>(mix(t.quad.x, t.quad.z, c.x), mix(t.quad.y, t.quad.w, c.y), 0.0, 1.0);
    return out;
}

const TAPS = array<vec2<f32>, 12>(
    vec2<f32>(-0.326, -0.406), vec2<f32>(-0.840, -0.074), vec2<f32>(-0.696, 0.457), vec2<f32>(-0.203, 0.621),
    vec2<f32>(0.962, -0.195), vec2<f32>(0.473, -0.480), vec2<f32>(0.519, 0.767), vec2<f32>(0.185, -0.893),
    vec2<f32>(0.507, 0.064), vec2<f32>(0.896, 0.412), vec2<f32>(-0.322, -0.933), vec2<f32>(-0.792, -0.598),
);

// YUV → RGB at a texture coordinate (luma UV; chroma is sampled at its own resolution).
fn rgb_at(uv: vec2<f32>) -> vec3<f32> {
    let y = textureSampleLevel(tex_y, samp, uv, 0.0).r;
    let cuv = uv * t.tex.yz;
    let u = textureSampleLevel(tex_u, samp, cuv, 0.0);
    let v = textureSampleLevel(tex_v, samp, cuv, 0.0);
    let cr = select(v.r, u.g, t.mode.w == 1u);
    let s = vec4<f32>(vec3<f32>(y, u.r, cr) * t.tex.x, 1.0);
    return clamp(vec3<f32>(dot(t.yuv_r, s), dot(t.yuv_g, s), dot(t.yuv_b, s)), vec3<f32>(0.0), vec3<f32>(1.0));
}

// Logical (rotated + flipped) frame UV → texture UV.
fn to_tex(l0: vec2<f32>) -> vec2<f32> {
    let l = mix(l0, 1.0 - l0, t.look.xy);
    switch t.mode.y {
        case 1u: { return vec2<f32>(l.y, 1.0 - l.x); }
        case 2u: { return vec2<f32>(1.0 - l.x, 1.0 - l.y); }
        case 3u: { return vec2<f32>(1.0 - l.y, l.x); }
        default: { return l; }
    }
}

fn inside(e: vec2<f32>) -> f32 {
    let s = step(vec2<f32>(0.0), e) * step(e, vec2<f32>(1.0));
    return s.x * s.y;
}

fn distort_to_eye(q: vec2<f32>, cs: f32, center: vec2<f32>) -> vec2<f32> {
    let r2 = dot(q, q);
    let f = 1.0 + t.lens.x * r2 + t.lens.y * r2 * r2;
    let d = q * f * cs;
    let n = vec2<f32>(d.x / t.lens.w, d.y) + center;
    return n * 0.5 + 0.5; // 0..1 inside the eye region, y up
}

fn sample_eye(e: vec2<f32>, rect: vec4<f32>) -> vec3<f32> {
    let c = clamp(e, vec2<f32>(0.0), vec2<f32>(1.0));
    let l = rect.xy + vec2<f32>(c.x, 1.0 - c.y) * rect.zw;
    return rgb_at(to_tex(l)) * inside(e);
}

fn shade(p: vec2<f32>, rect: vec4<f32>, center: vec2<f32>) -> vec3<f32> {
    if t.mode.x == 0u {
        let uv = clamp(p * 0.5 + 0.5, vec2<f32>(0.0), vec2<f32>(1.0));
        return rgb_at(to_tex(vec2<f32>(uv.x, 1.0 - uv.y)));
    }
    let q = p * vec2<f32>(t.chroma.w, 1.0) / t.lens.z;
    if t.chroma.x == t.chroma.y && t.chroma.z == t.chroma.y {
        return sample_eye(distort_to_eye(q, t.chroma.y, center), rect);
    }
    return vec3<f32>(
        sample_eye(distort_to_eye(q, t.chroma.x, center), rect).r,
        sample_eye(distort_to_eye(q, t.chroma.y, center), rect).g,
        sample_eye(distort_to_eye(q, t.chroma.z, center), rect).b,
    );
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

fn finish(c: vec3<f32>) -> vec4<f32> {
    if t.tex.w > 0.5 {
        return vec4<f32>(srgb_to_linear(c), 1.0);
    }
    return vec4<f32>(c, 1.0);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Stereo draws both eyes with one quad; pick the eye from the fragment position.
    let right = t.mode.z == 1u && in.pos.x >= t.vp1.x;
    let vp = select(t.vp0, t.vp1, right);
    let rect = select(t.eye0, t.eye1, right);
    let center = select(t.center.xy, t.center.zw, right);
    let rel = (in.pos.xy - vp.xy) / vp.zw;
    let p = vec2<f32>(rel.x * 2.0 - 1.0, 1.0 - rel.y * 2.0); // -1..1 over the eye viewport, y up
    let out_aspect = t.chroma.w;

    if t.look.w > 0.0 {
        var acc = shade(p, rect, center);
        let radius = vec2<f32>(0.035 / max(out_aspect, 0.2), 0.035) * t.look.w;
        for (var i = 0; i < 12; i++) {
            acc += shade(p + TAPS[i] * radius, rect, center);
        }
        return finish(acc / 13.0 * 0.55);
    }

    var col = shade(p, rect, center);
    if t.look.z > 0.0 && t.mode.x == 1u {
        let q = p * vec2<f32>(out_aspect, 1.0) / t.lens.z;
        // Screen-space derivatives of q are constant per eye viewport; computed analytically
        // (instead of fwidth) so the stereo seam doesn't produce a false line.
        let fq = vec2<f32>(2.0 * out_aspect / (vp.z * t.lens.z), 2.0 / (vp.w * t.lens.z));
        let g = q * 4.0;
        let a = abs(fract(g + 0.5) - 0.5) / max(fq * 4.0, vec2<f32>(1e-4));
        let line = 1.0 - clamp(min(a.x, a.y), 0.0, 1.0);
        let ax = abs(q) / max(fq * 1.5, vec2<f32>(1e-4));
        let axis = 1.0 - clamp(min(ax.x, ax.y), 0.0, 1.0);
        col = mix(col, vec3<f32>(0.15, 1.0, 0.65), max(line * 0.55, axis) * t.look.z);
    }
    return finish(col);
}
