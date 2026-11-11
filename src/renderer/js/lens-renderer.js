'use strict';

// WebGL2 renderer that turns the raw headset panel image (two barrel-distorted
// eye views) into a flat, rectilinear "vision corrected" view.
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

void main() {
  vec2 uv = vPos * 0.5 + 0.5;
  if (uMode == 0) {
    outColor = vec4(texture(uTex, toTex(vec2(uv.x, 1.0 - uv.y))).rgb, 1.0);
    return;
  }

  vec2 q = vPos * vec2(uOutAspect, 1.0) / uZoom;
  vec3 col;
  col.r = sampleEye(distortToEye(q, uChroma.r)).r;
  col.g = sampleEye(distortToEye(q, uChroma.g)).g;
  col.b = sampleEye(distortToEye(q, uChroma.b)).b;

  if (uGrid > 0.0) {
    vec2 g = q * 4.0;
    vec2 a = abs(fract(g + 0.5) - 0.5) / max(fwidth(g), vec2(1e-4));
    float line = 1.0 - clamp(min(a.x, a.y), 0.0, 1.0);
    vec2 ax = abs(q) / max(fwidth(q) * 1.5, vec2(1e-4));
    float axis = 1.0 - clamp(min(ax.x, ax.y), 0.0, 1.0);
    col = mix(col, vec3(0.15, 1.0, 0.65), max(line * 0.55, axis) * uGrid);
  }
  outColor = vec4(col, 1.0);
}`;

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

  class LensRenderer {
    constructor(canvas) {
      this.canvas = canvas;
      const gl = canvas.getContext('webgl2', {
        alpha: false,
        antialias: false,
        depth: false,
        desynchronized: true,
        powerPreference: 'high-performance',
        preserveDrawingBuffer: false,
      });
      if (!gl) throw new Error('WebGL2 is not available on this system.');
      this.gl = gl;

      const prog = gl.createProgram();
      gl.attachShader(prog, compile(gl, gl.VERTEX_SHADER, VS));
      gl.attachShader(prog, compile(gl, gl.FRAGMENT_SHADER, FS));
      gl.linkProgram(prog);
      if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(prog));
      this.prog = prog;

      this.u = {};
      for (const n of ['uTex', 'uMode', 'uEyeRect', 'uEyeAspect', 'uOutAspect', 'uCenter', 'uK1', 'uK2', 'uZoom', 'uChroma', 'uRot', 'uFlip', 'uGrid']) {
        this.u[n] = gl.getUniformLocation(prog, n);
      }

      this.vao = gl.createVertexArray();
      gl.bindVertexArray(this.vao);
      const buf = gl.createBuffer();
      gl.bindBuffer(gl.ARRAY_BUFFER, buf);
      gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, -1, 1, 1, -1, 1, 1]), gl.STATIC_DRAW);
      const loc = gl.getAttribLocation(prog, 'aPos');
      gl.enableVertexAttribArray(loc);
      gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);

      this.tex = gl.createTexture();
      gl.bindTexture(gl.TEXTURE_2D, this.tex);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);

      this.frameW = 0;
      this.frameH = 0;
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
      const gl = this.gl;
      gl.bindTexture(gl.TEXTURE_2D, this.tex);
      gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, frame.source || frame);
      this.frameW = frame.displayWidth;
      this.frameH = frame.displayHeight;
    }

    needsResize() {
      const dpr = window.devicePixelRatio || 1;
      const w = Math.max(1, Math.round(this.canvas.clientWidth * dpr));
      const h = Math.max(1, Math.round(this.canvas.clientHeight * dpr));
      return w !== this.canvas.width || h !== this.canvas.height;
    }

    resize() {
      const dpr = window.devicePixelRatio || 1;
      const w = Math.max(1, Math.round(this.canvas.clientWidth * dpr));
      const h = Math.max(1, Math.round(this.canvas.clientHeight * dpr));
      if (w !== this.canvas.width || h !== this.canvas.height) {
        this.canvas.width = w;
        this.canvas.height = h;
      }
    }

    fit(aspect) {
      const W = this.canvas.width;
      const H = this.canvas.height;
      let w = W;
      let h = Math.round(W / aspect);
      if (h > H) {
        h = H;
        w = Math.round(H * aspect);
      }
      return { x: Math.round((W - w) / 2), y: Math.round((H - h) / 2), w, h };
    }

    /** Rotation, flip and texture binding shared by draw() and analyze(). */
    setup(p) {
      const gl = this.gl;
      gl.useProgram(this.prog);
      gl.bindVertexArray(this.vao);
      gl.activeTexture(gl.TEXTURE0);
      gl.bindTexture(gl.TEXTURE_2D, this.tex);
      gl.uniform1i(this.u.uTex, 0);
      const rot = ((Math.round((Number(p.rotation) || 0) / 90) % 4) + 4) % 4;
      gl.uniform1i(this.u.uRot, rot);
      gl.uniform2f(this.u.uFlip, p.flipH ? 1 : 0, p.flipV ? 1 : 0);
      return { rot, LW: rot % 2 ? this.frameH : this.frameW, LH: rot % 2 ? this.frameW : this.frameH };
    }

    /**
     * @param {object} p correction params + { viewMode: 'corrected'|'stereo'|'raw', grid: 0|1 }.
     *   layout should already be resolved ('sbs'|'tb'|'quad'|'mono'); 'auto' falls back to the frame aspect.
     * @returns {{layout:string, rotation:number, eyeW:number, eyeH:number}|null}
     */
    draw(p) {
      const gl = this.gl;
      this.resize();
      gl.bindFramebuffer(gl.FRAMEBUFFER, null);
      gl.viewport(0, 0, this.canvas.width, this.canvas.height);
      gl.clearColor(0, 0, 0, 1);
      gl.clear(gl.COLOR_BUFFER_BIT);
      if (!this.hasFrame) return null;

      const { rot, LW, LH } = this.setup(p);
      const layout = ['sbs', 'tb', 'quad', 'mono'].includes(p.layout) ? p.layout : LW / LH >= 1.6 ? 'sbs' : 'mono';
      const rotation = rot * 90;

      if (p.viewMode === 'raw') {
        const vp = this.fit(LW / LH);
        gl.viewport(vp.x, vp.y, vp.w, vp.h);
        gl.uniform1i(this.u.uMode, 0);
        gl.drawArrays(gl.TRIANGLES, 0, 6);
        return { layout, rotation, eyeW: LW, eyeH: LH };
      }

      const qy = p.quadRow === 'bottom' ? 0.5 : 0;
      const rects = {
        sbs: { left: [0, 0, 0.5, 1], right: [0.5, 0, 0.5, 1] },
        tb: { left: [0, 0, 1, 0.5], right: [0, 0.5, 1, 0.5] },
        quad: { left: [0, qy, 0.5, 0.5], right: [0.5, qy, 0.5, 0.5] },
        mono: { left: [0, 0, 1, 1], right: [0, 0, 1, 1] },
      }[layout];
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
      gl.uniform1f(this.u.uGrid, p.grid ? 1 : 0);

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
        const vp = this.fit(Math.min(outAspect, 1) * 2);
        const half = Math.floor(vp.w / 2);
        drawEye('left', { x: vp.x, y: vp.y, w: half, h: vp.h });
        drawEye('right', { x: vp.x + half, y: vp.y, w: vp.w - half, h: vp.h });
      } else {
        drawEye(p.eye === 'right' ? 'right' : 'left', this.fit(outAspect));
      }
      return { layout, rotation, eyeW: Math.round(eyeW), eyeH: Math.round(eyeH) };
    }

    /**
     * Render the logical frame (rotation/flip from p applied) into a small offscreen
     * target and detect its view grid. Cheap, but stalls on readPixels: call rarely.
     * @returns {{grid:{cols:number, rows:number, confidence:number}|null, mean:number}|null}
     */
    analyze(p = {}, size = 96) {
      if (!this.hasFrame) return null;
      const gl = this.gl;
      if (!this.fbo || this.fboSize !== size) {
        if (this.fbo) {
          gl.deleteFramebuffer(this.fbo);
          gl.deleteTexture(this.fboTex);
        }
        this.fboTex = gl.createTexture();
        gl.bindTexture(gl.TEXTURE_2D, this.fboTex);
        gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, size, size, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
        this.fbo = gl.createFramebuffer();
        gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
        gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, this.fboTex, 0);
        this.fboSize = size;
        this.fboPixels = new Uint8Array(size * size * 4);
      }
      gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
      gl.viewport(0, 0, size, size);
      this.setup(p);
      gl.uniform1i(this.u.uMode, 0);
      gl.drawArrays(gl.TRIANGLES, 0, 6);
      gl.readPixels(0, 0, size, size, gl.RGBA, gl.UNSIGNED_BYTE, this.fboPixels);
      gl.bindFramebuffer(gl.FRAMEBUFFER, null);

      const px = this.fboPixels;
      const luma = new Float32Array(size * size);
      let sum = 0;
      for (let y = 0; y < size; y++) {
        // readPixels rows start at the bottom; flip so row 0 is the top of the frame.
        const src = (size - 1 - y) * size * 4;
        for (let x = 0; x < size; x++) {
          const o = src + x * 4;
          const v = (0.299 * px[o] + 0.587 * px[o + 1] + 0.114 * px[o + 2]) / 255;
          luma[y * size + x] = v;
          sum += v;
        }
      }
      return { grid: window.LayoutDetect.detectGrid(luma, size, size), mean: sum / (size * size) };
    }

    dispose() {
      const gl = this.gl;
      gl.deleteTexture(this.tex);
      if (this.fbo) {
        gl.deleteFramebuffer(this.fbo);
        gl.deleteTexture(this.fboTex);
      }
      gl.deleteProgram(this.prog);
      gl.deleteVertexArray(this.vao);
      const ext = gl.getExtension('WEBGL_lose_context');
      if (ext) ext.loseContext();
    }
  }

  window.LensRenderer = LensRenderer;
})();
