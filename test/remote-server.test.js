const test = require('node:test');
const assert = require('node:assert');
const path = require('path');
const { RemoteAuth, resolveStatic, deviceName, encodePacket, decodePacket, sanitizeSupport, remoteIndexHtml } = require('../src/main/remote-server');

function auth(state = {}) {
  let t = 1_000_000;
  let saves = 0;
  const a = new RemoteAuth(state, { save: () => saves++, now: () => t });
  return { a, state, advance: (ms) => (t += ms), saves: () => saves };
}

test('a 6-digit PIN is generated and can be regenerated', () => {
  const { a, state } = auth();
  assert.match(a.pin, /^\d{6}$/);
  assert.strictEqual(state.pin, a.pin);
  const seen = new Set([a.pin]);
  for (let i = 0; i < 5; i++) seen.add(a.newPin());
  assert.ok(seen.size > 1);
});

test('correct PIN pairs, issued token verifies, only its hash is stored', () => {
  const { a, state } = auth();
  assert.deepStrictEqual(a.checkPin(a.pin, '10.0.0.2'), { ok: true });
  const { token, client } = a.issueToken({ name: 'iPad · Safari', ip: '10.0.0.2' });
  assert.ok(token.length >= 40);
  assert.ok(!JSON.stringify(state).includes(token));
  assert.strictEqual(a.verifyToken(token, '10.0.0.3').id, client.id);
  assert.strictEqual(a.list()[0].ip, '10.0.0.3');
  assert.strictEqual(a.verifyToken(`${token}x`), null);
  assert.strictEqual(a.verifyToken(''), null);
  assert.strictEqual(a.verifyToken(undefined), null);
  assert.ok(!('hash' in a.list()[0]));
});

test('tokens survive a restart (persisted state) and can be revoked', () => {
  const { a, state } = auth();
  const one = a.issueToken({ name: 'A' });
  const two = a.issueToken({ name: 'B' });
  const again = new RemoteAuth(JSON.parse(JSON.stringify(state)));
  assert.ok(again.verifyToken(one.token));
  assert.strictEqual(again.revoke(one.client.id), 1);
  assert.strictEqual(again.verifyToken(one.token), null);
  assert.ok(again.verifyToken(two.token));
  assert.strictEqual(again.revoke('all'), 1);
  assert.strictEqual(again.verifyToken(two.token), null);
});

test('wrong PINs are rate limited per IP, then unlock after the lockout', () => {
  const { a, advance } = auth();
  const wrong = a.pin === '000000' ? '111111' : '000000';
  for (let i = 0; i < 4; i++) assert.strictEqual(a.checkPin(wrong, '1.1.1.1').ok, false);
  const fifth = a.checkPin(wrong, '1.1.1.1');
  assert.ok(fifth.retryAfter > 0);
  // Locked: even the right PIN is refused from that IP, but not from another.
  assert.strictEqual(a.checkPin(a.pin, '1.1.1.1').ok, false);
  assert.strictEqual(a.checkPin(a.pin, '2.2.2.2').ok, true);
  advance(5 * 60_000 + 1);
  assert.strictEqual(a.checkPin(a.pin, '1.1.1.1').ok, true);
});

test('many wrong PINs from many IPs pause pairing globally', () => {
  const { a, advance } = auth();
  const wrong = a.pin === '000000' ? '111111' : '000000';
  for (let i = 0; i < 30; i++) a.checkPin(wrong, `10.0.${i}.1`);
  assert.strictEqual(a.checkPin(a.pin, '10.9.9.9').ok, false);
  advance(61_000);
  assert.strictEqual(a.checkPin(a.pin, '10.9.9.9').ok, true);
});

test('static paths cannot escape the renderer folder', () => {
  const root = path.resolve('/srv/renderer');
  assert.strictEqual(resolveStatic(root, '/js/app.js'), path.join(root, 'js', 'app.js'));
  assert.strictEqual(resolveStatic(root, '/styles.css?v=2'), path.join(root, 'styles.css'));
  assert.strictEqual(resolveStatic(root, '/../main/main.js'), path.join(root, 'main', 'main.js'));
  for (const bad of ['/%2e%2e/%2e%2e/etc/passwd', '/js/..%2f..%2fpreload.js', '/.hidden', '/js/%00x.js', '/js\\..\\x', '/%E0%A4%A']) {
    const r = resolveStatic(root, bad);
    assert.ok(r === null || r.startsWith(root + path.sep), `${bad} -> ${r}`);
    if (r) assert.ok(!r.includes(`${path.sep}.`), `${bad} -> ${r}`);
  }
  assert.strictEqual(resolveStatic(root, '/js/%00x.js'), null);
  assert.strictEqual(resolveStatic(root, '/.hidden'), null);
});

test('device names from user agents', () => {
  assert.strictEqual(deviceName('Mozilla/5.0 (iPad; CPU OS 17_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Mobile/15E148 Safari/604.1'), 'iPad · Safari');
  assert.strictEqual(deviceName('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15', 'iPad'), 'iPad · Safari');
  assert.strictEqual(deviceName('Mozilla/5.0 (Linux; Android 14; SM-X710 Build/UP1A) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36'), 'SM-X710 · Chrome');
  assert.strictEqual(deviceName('Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Mobile Safari/537.36'), 'Android phone · Chrome');
  assert.strictEqual(deviceName('Mozilla/5.0 (iPhone; CPU iPhone OS 17_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/124 Mobile/15E148 Safari/604.1'), 'iPhone · Chrome');
  assert.strictEqual(deviceName(''), 'Browser');
});

test('video packets round-trip through the binary frame', () => {
  const data = Buffer.from([0, 0, 0, 1, 0x65, 1, 2, 3]);
  const f = decodePacket(encodePacket('SERIAL-1', { config: false, key: true, pts: 123456789, data }));
  assert.deepStrictEqual({ ...f, data: [...f.data] }, { id: 'SERIAL-1', config: false, key: true, pts: 123456789, data: [...data] });
  const c = decodePacket(encodePacket('x', { config: true, key: false, pts: 0, data: Buffer.alloc(0) }));
  assert.strictEqual(c.config, true);
  assert.strictEqual(c.data.length, 0);
});

test('viewer decoder support is sanitised', () => {
  assert.deepStrictEqual(sanitizeSupport({ h264: { any: 1, hw: 0 }, h265_10: { any: true, hw: true }, 'bad key!': { any: true }, av1: 'yes' }), {
    h264: { any: true, hw: false },
    h265_10: { any: true, hw: true },
  });
  assert.strictEqual(sanitizeSupport(null), null);
  assert.strictEqual(sanitizeSupport({}), null);
});

test('index.html gets the bridge, manifest and a CSP that allows the WebSocket and MSE', () => {
  const html = `<!doctype html><html lang="en"><head><meta http-equiv="Content-Security-Policy" content="default-src 'self'; script-src 'self'" /><title>XR MultiCast</title></head><body><script src="js/icons.js"></script><script src="js/app.js"></script></body></html>`;
  const out = remoteIndexHtml(html);
  assert.match(out, /<html lang="en" class="remote">/);
  assert.match(out, /connect-src 'self' ws: wss:/);
  assert.match(out, /media-src 'self' blob:/);
  assert.match(out, /rel="manifest"/);
  assert.match(out, /apple-mobile-web-app-capable/);
  assert.ok(out.indexOf('remote-bridge.js') < out.indexOf('icons.js'));
});

function fakeServer() {
  const { EventEmitter } = require('events');
  const { RemoteServer } = require('../src/main/remote-server');
  const streams = new EventEmitter();
  const support = new Map();
  streams.setViewerSupport = (id, s) => (s ? support.set(id, s) : support.delete(id));
  const store = { data: {}, settings: { remoteAllowControl: true }, save() {} };
  const rs = new RemoteServer({ store, dispatch: async () => null, streams, devices: new EventEmitter(), rendererDir: '/x', iconPath: '/x', localIps: () => [] });
  rs.server = { listening: true };
  const client = (subs = []) => {
    const sent = [];
    const c = { id: `c${rs.clients.size}`, authed: true, subs: new Set(subs), dropping: new Set(), ws: { readyState: 1, bufferedAmount: 0, send: (f) => sent.push(f) }, sent };
    rs.clients.add(c);
    return c;
  };
  const pkt = (id, kind, n) => streams.emit('packet', id, { config: kind === 'c', key: kind === 'k', pts: n, data: Buffer.from([n]) });
  const kinds = (c) => c.sent.map((f) => decodePacket(f)).map((p) => (p.config ? 'c' : p.key ? 'k' : 'd') + p.data[0]);
  return { rs, streams, support, client, pkt, kinds };
}

test('new subscribers get the cached config + current GOP straight away', () => {
  const { rs, client, pkt, kinds } = fakeServer();
  pkt('A', 'c', 0);
  pkt('A', 'k', 1);
  pkt('A', 'd', 2);
  pkt('A', 'k', 3);
  pkt('A', 'd', 4);
  const c = client();
  rs.subscribe(c, ['A', 'B']);
  assert.deepStrictEqual(kinds(c), ['c0', 'k3', 'd4']);
  pkt('A', 'd', 5);
  pkt('B', 'k', 6);
  assert.deepStrictEqual(kinds(c), ['c0', 'k3', 'd4', 'd5', 'k6']);
  // Unsubscribed streams are not sent.
  rs.subscribe(c, ['B']);
  pkt('A', 'd', 7);
  assert.strictEqual(kinds(c).length, 5);
});

test('a backed-up client skips delta frames until the next key frame', () => {
  const { client, pkt, kinds } = fakeServer();
  const slow = client(['A']);
  const fast = client(['A']);
  pkt('A', 'k', 1);
  slow.ws.bufferedAmount = 8 * 1024 * 1024;
  pkt('A', 'd', 2);
  slow.ws.bufferedAmount = 0;
  pkt('A', 'd', 3); // still dropping: its reference frame was skipped
  pkt('A', 'k', 4);
  pkt('A', 'd', 5);
  assert.deepStrictEqual(kinds(slow), ['k1', 'k4', 'd5']);
  assert.deepStrictEqual(kinds(fast), ['k1', 'd2', 'd3', 'k4', 'd5']);
});

test('stream restarts clear the GOP cache; viewers report decoder support', () => {
  const { rs, streams, support, client, pkt, kinds } = fakeServer();
  pkt('A', 'c', 0);
  pkt('A', 'k', 1);
  streams.emit('state', { id: 'A', state: 'starting' });
  const c = client();
  rs.subscribe(c, ['A']);
  assert.deepStrictEqual(kinds(c), []);
  rs.setSupport(c, { h264: { any: true, hw: true }, h265: { any: false, hw: false } });
  assert.deepStrictEqual(support.get(c.id).h265, { any: false, hw: false });
  rs.dropClient(c);
  assert.strictEqual(support.has(c.id), false);
});

test('control-only clients (support: false) do not constrain stream codecs', () => {
  const { rs, support, client } = fakeServer();
  const panel = client();
  rs.setSupport(panel, false);
  assert.strictEqual(support.has(panel.id), false);
  const viewer = client();
  rs.setSupport(viewer, undefined);
  assert.deepStrictEqual(support.get(viewer.id), { h264: { any: true, hw: true } });
  rs.setSupport(viewer, false);
  assert.strictEqual(support.has(viewer.id), false);
});
