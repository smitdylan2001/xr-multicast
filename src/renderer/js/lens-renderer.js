'use strict';

// One WebGL2 compositor draws every stream tile into a single canvas (one GPU
// context for the whole window), turning each headset's raw panel image (two
// barrel-distorted eye views) into a flat, rectilinear "vision corrected" view.
//
// For every output pixel q (in units of half the eye height, centred on the
// lens axis) we sample the panel at
//     d = q · (1 + k1·r² + k2·r⁴) · s_channel,   r = |q|
// which inverts the barrel pre-distortion the headset compositor applied for
// its lenses. s_channel lets red/blue be scaled independently to cancel the
// lateral chromatic pre-correction (colour fringes).

(function () {
  const VS = `#version 300 es
in vec2 aPos;
out vec2 vPos;
void main() {
  vPos = aPos;
  gl_Position = vec4(aPos, 0.0, 1.0);
}`;

  const FS = `#version 300 es
precision highp float;
in vec2 vPos;
out vec4 outColor;

uniform sampler2D uTex;
uniform int uMode;          // 0 = raw passthrough, 1 = lens corrected
uniform vec4 uEyeRect;      // eye region in logical-frame UV (x, y, w, h), y down
uniform float uEyeAspect;   // eye region width / height in pixels
uniform float uOutAspect;   // output viewport width / height
uniform vec2 uCenter;       // lens centre offset inside the eye region (-1..1)
uniform float uK1;
uniform float uK2;
uniform float uZoom;
uniform vec3 uChroma;       // radial scale per channel
uniform int uRot;           // quarter turns clockwise applied to the source
uniform vec2 uFlip;         // 1 = mirror the logical (rotated) frame on that axis
uniform float uGrid;
uniform float uBlur;        // 0 = sharp; >0 = blurred + dimmed (e.g. while changing resolution)

const vec2 TAPS[12] = vec2[12](
  vec2(-0.326, -0.406), vec2(-0.840, -0.074), vec2(-0.696, 0.457), vec2(-0.203, 0.621),
  vec2(0.962, -0.195), vec2(0.473, -0.480), vec2(0.519, 0.767), vec2(0.185, -0.893),
  vec2(0.507, 0.064), vec2(0.896, 0.412), vec2(-0.322, -0.933), vec2(-0.792, -0.598));

vec2 toTex(vec2 l) {
  l = mix(l, 1.0 - l, uFlip);
  if (uRot == 1) return vec2(l.y, 1.0 - l.x);
  if (uRot == 2) return vec2(1.0 - l.x, 1.0 - l.y);
  if (uRot == 3) return vec2(1.0 - l.y, l.x);
  return l;
}

float inside(vec2 e) {
  vec2 s = step(vec2(0.0), e) * step(e, vec2(1.0));
  return s.x * s.y;
}

vec2 distortToEye(vec2 q, float cs) {
  float r2 = dot(q, q);
  float f = 1.0 + uK1 * r2 + uK2 * r2 * r2;
  vec2 d = q * f * cs;
  vec2 n = vec2(d.x / uEyeAspect, d.y) + uCenter;
  return n * 0.5 + 0.5; // 0..1 inside the eye region, y up
}

vec4 sampleEye(vec2 e) {
  vec2 c = clamp(e, 0.0, 1.0);
  vec2 l = uEyeRect.xy + vec2(c.x, 1.0 - c.y) * uEyeRect.zw;
  return texture(uTex, toTex(l)) * inside(e);
}

vec3 shade(vec2 p) {
  if (uMode == 0) {
    vec2 uv = clamp(p * 0.5 + 0.5, 0.0, 1.0);
    return texture(uTex, toTex(vec2(uv.x, 1.0 - uv.y))).rgb;
  }
  vec2 q = p * vec2(uOutAspect, 1.0) / uZoom;
  return vec3(
    sampleEye(distortToEye(q, uChroma.r)).r,
    sampleEye(distortToEye(q, uChroma.g)).g,
    sampleEye(distortToEye(q, uChroma.b)).b);
}

void main() {
  if (uBlur > 0.0) {
    vec3 acc = shade(vPos);
    vec2 radius = vec2(0.035 / max(uOutAspect, 0.2), 0.035) * uBlur;
    for (int i = 0; i < 12; i++) acc += shade(vPos + TAPS[i] * radius);
    outColor = vec4(acc / 13.0 * 0.55, 1.0);
    return;
  }
  vec3 col = shade(vPos);
  if (uGrid > 0.0 && uMode == 1) {
    vec2 q = vPos * vec2(uOutAspect, 1.0) / uZoom;
    vec2 g = q * 4.0;
    vec2 a = abs(fract(g + 0.5) - 0.5) / max(fwidth(g), vec2(1e-4));
    float line = 1.0 - clamp(min(a.x, a.y), 0.0, 1.0);
    vec2 ax = abs(q) / max(fwidth(q) * 1.5, vec2(1e-4));
    float axis = 1.0 - clamp(min(ax.x, ax.y), 0.0, 1.0);
    col = mix(col, vec3(0.15, 1.0, 0.65), max(line * 0.55, axis) * uGrid);
  }
  outColor = vec4(col, 1.0);
}`;

  const UNIFORMS = ['uTex', 'uMode', 'uEyeRect', 'uEyeAspect', 'uOutAspect', 'uCenter', 'uK1', 'uK2', 'uZoom', 'uChroma', 'uRot', 'uFlip', 'uGrid', 'uBlur'];
  const LAYOUT_RECTS = (quadRow) => {
    const qy = quadRow === 'bottom' ? 0.5 : 0;
    return {
      sbs: { left: [0, 0, 0.5, 1], right: [0.5, 0, 0.5, 1] },
      tb: { left: [0, 0, 1, 0.5], right: [0, 0.5, 1, 0.5] },
      quad: { left: [0, qy, 0.5, 0.5], right: [0.5, qy, 0.5, 0.5] },
      mono: { left: [0, 0, 1, 1], right: [0, 0, 1, 1] },
    };
  };

  function compile(gl, type, src) {
    const s = gl.createShader(type);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) {
      const log = gl.getShaderInfoLog(s);
      gl.deleteShader(s);
      throw new Error(`Shader compile failed: ${log}`);
    }
    return s;
  }

  function parseAspect(v, eyeAspect) {
    if (v === 'eye' || !v) return eyeAspect;
    const m = String(v).match(/^(\d+(?:\.\d+)?):(\d+(?:\.\d+)?)$/);
    return m ? Number(m[1]) / Number(m[2]) : eyeAspect;
  }

  /** Largest box with `aspect` centred inside `box` ({x, y, w, h} in framebuffer pixels). */
  function fit(box, aspect) {
    let w = box.w;
    let h = Math.round(box.w / aspect);
    if (h > box.h) {
      h = box.h;
      w = Math.round(box.h * aspect);
    }
    return { x: box.x + Math.round((box.w - w) / 2), y: box.y + Math.round((box.h - h) / 2), w, h };
  }

  /** One stream's latest frame, as a texture in the shared compositor context. */
  class VideoSurface {
    constructor(compositor) {
      this.c = compositor;
      this.frameW = 0;
      this.frameH = 0;
      this.tex = null;
      this.createTexture();
    }

    createTexture() {
      const gl = this.c.gl;
      this.tex = gl.createTexture();
      gl.bindTexture(gl.TEXTURE_2D, this.tex);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    }

    get hasFrame() {
      return this.frameW > 0;
    }

    /**
     * @param {VideoFrame | {source: TexImageSource, displayWidth: number, displayHeight: number}} frame
     *   A WebCodecs VideoFrame, or a wrapper around another texture source (e.g. an
     *   HTMLVideoElement fed by Media Source Extensions on tablets without WebCodecs).
     */
    upload(frame) {
      const gl = this.c.gl;
      if (this.c.lost) return;
      gl.bindTexture(gl.TEXTURE_2D, this.tex);
      gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, frame.source || frame);
      this.frameW = frame.displayWidth;
      this.frameH = frame.displayHeight;
      this.c.dirty = true;
    }

    /** See Compositor.analyze(). */
    analyze(p = {}, size = 96) {
      return this.c.analyze(this, p, size);
    }

    dispose() {
      this.c.surfaces.delete(this);
      if (!this.c.lost) this.c.gl.deleteTexture(this.tex);
      this.tex = null;
      this.c.dirty = true;
    }
  }

  class Compositor {
    /** @param {HTMLCanvasElement} canvas a transparent canvas covering the area tiles can appear in */
    constructor(canvas) {
      this.canvas = canvas;
      const gl = canvas.getContext('webgl2', {
        alpha: true,
        premultipliedAlpha: true,
        antialias: false,
        depth: false,
        stencil: false,
        powerPreference: 'high-performance',
        preserveDrawingBuffer: false,
      });
      if (!gl) throw new Error('WebGL2 is not available on this system.');
      this.gl = gl;
      this.surfaces = new Set();
      this.dirty = true;
      this.lost = false;
      this.init();
      canvas.addEventListener('webglcontextlost', (e) => {
        e.preventDefault();
        this.lost = true;
      });
      canvas.addEventListener('webglcontextrestored', () => {
        this.lost = false;
        this.init();
        for (const s of this.surfaces) {
          s.frameW = 0;
          s.frameH = 0;
          s.createTexture();
        }
        this.dirty = true;
      });
    }

    init() {
      const gl = this.gl;
      const prog = gl.createProgram();
      gl.attachShader(prog, compile(gl, gl.VERTEX_SHADER, VS));
      gl.attachShader(prog, compile(gl, gl.FRAGMENT_SHADER, FS));
      gl.linkProgram(prog);
      if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(prog));
      this.prog = prog;
      this.u = {};
      for (const n of UNIFORMS) this.u[n] = gl.getUniformLocation(prog, n);

      this.vao = gl.createVertexArray();
      gl.bindVertexArray(this.vao);
      const buf = gl.createBuffer();
      gl.bindBuffer(gl.ARRAY_BUFFER, buf);
      gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, -1, 1, 1, -1, 1, 1]), gl.STATIC_DRAW);
      const loc = gl.getAttribLocation(prog, 'aPos');
      gl.enableVertexAttribArray(loc);
      gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
      this.fbo = null;
      this.fboSize = null;
    }

    createSurface() {
      const s = new VideoSurface(this);
      this.surfaces.add(s);
      return s;
    }

    /** Resize the backing store to the canvas' CSS size × devicePixelRatio and clear it. */
    beginFrame() {
      const gl = this.gl;
      const dpr = window.devicePixelRatio || 1;
      const w = Math.max(1, Math.round(this.canvas.clientWidth * dpr));
      const h = Math.max(1, Math.round(this.canvas.clientHeight * dpr));
      if (w !== this.canvas.width || h !== this.canvas.height) {
        this.canvas.width = w;
        this.canvas.height = h;
      }
      this.dpr = dpr;
      this.bounds = this.canvas.getBoundingClientRect();
      gl.bindFramebuffer(gl.FRAMEBUFFER, null);
      gl.disable(gl.SCISSOR_TEST);
      gl.viewport(0, 0, w, h);
      gl.clearColor(0, 0, 0, 0);
      gl.clear(gl.COLOR_BUFFER_BIT);
    }

    /** CSS-pixel DOMRect → framebuffer rect (bottom-left origin), relative to this canvas. */
    toFb(r) {
      const b = this.bounds;
      const d = this.dpr;
      const x = Math.round((r.left - b.left) * d);
      const y = Math.round((b.bottom - r.bottom) * d);
      return { x, y, w: Math.round((r.right - b.left) * d) - x, h: Math.round((b.bottom - r.top) * d) - y };
    }

    /**
     * Draw one stream into a tile.
     * @param {VideoSurface} surface
     * @param {object} p correction params + { viewMode, grid, blur }
     * @param {DOMRect} rect tile video area (CSS px)
     * @param {DOMRect} clip visible area (CSS px), e.g. the scrolling grid's viewport
     */
    draw(surface, p, rect, clip) {
      if (this.lost || !surface.hasFrame) return null;
      const gl = this.gl;
      const box = this.toFb(rect);
      const c = this.toFb(clip);
      const sx = Math.max(box.x, c.x);
      const sy = Math.max(box.y, c.y);
      const sw = Math.min(box.x + box.w, c.x + c.w) - sx;
      const sh = Math.min(box.y + box.h, c.y + c.h) - sy;
      if (sw <= 0 || sh <= 0) return null;
      gl.enable(gl.SCISSOR_TEST);
      gl.scissor(sx, sy, sw, sh);
      const info = this.render(surface, p, box);
      gl.disable(gl.SCISSOR_TEST);
      return info;
    }

    /** Rotation, flip and texture binding for a surface. */
    setup(surface, p) {
      const gl = this.gl;
      gl.useProgram(this.prog);
      gl.bindVertexArray(this.vao);
      gl.activeTexture(gl.TEXTURE0);
      gl.bindTexture(gl.TEXTURE_2D, surface.tex);
      gl.uniform1i(this.u.uTex, 0);
      const rot = ((Math.round((Number(p.rotation) || 0) / 90) % 4) + 4) % 4;
      gl.uniform1i(this.u.uRot, rot);
      gl.uniform2f(this.u.uFlip, p.flipH ? 1 : 0, p.flipV ? 1 : 0);
      gl.uniform1f(this.u.uBlur, p.blur ? 1 : 0);
      gl.uniform1f(this.u.uGrid, p.grid ? 1 : 0);
      return { rot, LW: rot % 2 ? surface.frameH : surface.frameW, LH: rot % 2 ? surface.frameW : surface.frameH };
    }

    /**
     * Render a surface into `box` of the currently bound framebuffer.
     * layout should already be resolved ('sbs'|'tb'|'quad'|'mono'); 'auto' falls back to the frame aspect.
     * @returns {{layout:string, rotation:number, eyeW:number, eyeH:number}}
     */
    render(surface, p, box) {
      const gl = this.gl;
      const { rot, LW, LH } = this.setup(surface, p);
      const layout = ['sbs', 'tb', 'quad', 'mono'].includes(p.layout) ? p.layout : LW / LH >= 1.6 ? 'sbs' : 'mono';
      const rotation = rot * 90;

      if (p.viewMode === 'raw') {
        const vp = fit(box, LW / LH);
        gl.viewport(vp.x, vp.y, vp.w, vp.h);
        gl.uniform1i(this.u.uMode, 0);
        gl.uniform1f(this.u.uOutAspect, LW / LH);
        gl.drawArrays(gl.TRIANGLES, 0, 6);
        return { layout, rotation, eyeW: LW, eyeH: LH };
      }

      const rects = LAYOUT_RECTS(p.quadRow)[layout];
      const eyeW = LW * rects.left[2];
      const eyeH = LH * rects.left[3];
      const eyeAspect = eyeW / eyeH;
      const outAspect = parseAspect(p.outputAspect, eyeAspect);
      const sideBySide = layout === 'sbs' || layout === 'quad';

      gl.uniform1i(this.u.uMode, 1);
      gl.uniform1f(this.u.uEyeAspect, eyeAspect);
      gl.uniform1f(this.u.uOutAspect, outAspect);
      gl.uniform1f(this.u.uK1, Number(p.k1) || 0);
      gl.uniform1f(this.u.uK2, Number(p.k2) || 0);
      gl.uniform1f(this.u.uZoom, Math.max(0.1, Number(p.zoom) || 1));
      gl.uniform3f(this.u.uChroma, Number(p.chromaR) || 1, 1, Number(p.chromaB) || 1);

      const drawEye = (eye, vp) => {
        const r = rects[eye];
        // Lens centres sit symmetrically around the nose, so mirror X for the right eye.
        const cx = (Number(p.centerX) || 0) * (sideBySide && eye === 'right' ? -1 : 1);
        gl.viewport(vp.x, vp.y, vp.w, vp.h);
        gl.uniform4f(this.u.uEyeRect, r[0], r[1], r[2], r[3]);
        gl.uniform2f(this.u.uCenter, cx, Number(p.centerY) || 0);
        gl.drawArrays(gl.TRIANGLES, 0, 6);
      };

      if (p.viewMode === 'stereo' && layout !== 'mono') {
        // Keep each eye at most square so the pair doesn't become a thin letterbox.
        gl.uniform1f(this.u.uOutAspect, Math.min(outAspect, 1));
        const vp = fit(box, Math.min(outAspect, 1) * 2);
        const half = Math.floor(vp.w / 2);
        drawEye('left', { x: vp.x, y: vp.y, w: half, h: vp.h });
        drawEye('right', { x: vp.x + half, y: vp.y, w: vp.w - half, h: vp.h });
      } else {
        drawEye(p.eye === 'right' ? 'right' : 'left', fit(box, outAspect));
      }
      return { layout, rotation, eyeW: Math.round(eyeW), eyeH: Math.round(eyeH) };
    }

    /** Offscreen render target of the given size (reused). */
    target(w, h) {
      const gl = this.gl;
      const key = `${w}x${h}`;
      if (!this.fbo || this.fboSize !== key) {
        if (this.fbo) {
          gl.deleteFramebuffer(this.fbo);
          gl.deleteTexture(this.fboTex);
        }
        this.fboTex = gl.createTexture();
        gl.bindTexture(gl.TEXTURE_2D, this.fboTex);
        gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
        this.fbo = gl.createFramebuffer();
        gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
        gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, this.fboTex, 0);
        this.fboSize = key;
      }
      gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
      gl.disable(gl.SCISSOR_TEST);
      gl.viewport(0, 0, w, h);
      gl.clearColor(0, 0, 0, 1);
      gl.clear(gl.COLOR_BUFFER_BIT);
    }

    /** Read back the offscreen target, top row first. */
    readTarget(w, h) {
      const gl = this.gl;
      const px = new Uint8Array(w * h * 4);
      gl.readPixels(0, 0, w, h, gl.RGBA, gl.UNSIGNED_BYTE, px);
      gl.bindFramebuffer(gl.FRAMEBUFFER, null);
      const out = new Uint8ClampedArray(w * h * 4);
      for (let y = 0; y < h; y++) out.set(px.subarray((h - 1 - y) * w * 4, (h - y) * w * 4), y * w * 4);
      this.dirty = true; // the onscreen canvas needs a redraw after offscreen work
      return out;
    }

    /**
     * Render the logical frame (rotation/flip from p applied) into a small offscreen
     * target and detect its view grid. Cheap, but stalls on readPixels: call rarely.
     * @returns {{grid:{cols:number, rows:number, confidence:number}|null, mean:number}|null}
     */
    analyze(surface, p = {}, size = 96) {
      if (this.lost || !surface.hasFrame) return null;
      this.target(size, size);
      this.setup(surface, { ...p, blur: false, grid: false });
      this.gl.uniform1i(this.u.uMode, 0);
      this.gl.drawArrays(this.gl.TRIANGLES, 0, 6);
      const px = this.readTarget(size, size);
      const luma = new Float32Array(size * size);
      let sum = 0;
      for (let i = 0; i < size * size; i++) {
        const v = (0.299 * px[i * 4] + 0.587 * px[i * 4 + 1] + 0.114 * px[i * 4 + 2]) / 255;
        luma[i] = v;
        sum += v;
      }
      return { grid: window.LayoutDetect.detectGrid(luma, size, size), mean: sum / (size * size) };
    }

    /** PNG data URL of one stream rendered at w×h (no grid, no blur). */
    snapshot(surface, p, w, h) {
      if (this.lost || !surface.hasFrame) return null;
      this.target(w, h);
      this.render(surface, { ...p, grid: false, blur: false }, { x: 0, y: 0, w, h });
      const px = this.readTarget(w, h);
      const c = document.createElement('canvas');
      c.width = w;
      c.height = h;
      c.getContext('2d').putImageData(new ImageData(px, w, h), 0, 0);
      return c.toDataURL('image/png');
    }
  }

  window.Compositor = Compositor;
})();
