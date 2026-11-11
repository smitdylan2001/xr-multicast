# Remote API (WebSocket)

The tablet/phone dashboard talks to the XR MultiCast hub over one WebSocket. The same API is available to your own software and hardware: a Raspberry Pi kiosk, a physical "cast all" button panel, a stage controller, an automated test rig, and so on.

> The protocol is versioned with the app. It is stable within a major version, but may change between major versions.

## Enabling

On the hub: **Settings → Remote access (tablets & phones)** → tick *Let tablets and phones…* → **Save**. Note the port (default `8787`) and the 6-digit **PIN**. *Allow remote control* decides whether paired clients can start and stop casts (see [Permissions](#permissions)).

Endpoint: `ws://<hub-ip>:<port>/ws`

Browsers must connect from the same origin. Clients that send no `Origin` header (most non-browser WebSocket libraries) are accepted.

## Message format

Text frames are JSON objects with a type field `t`. Binary frames (server → client only) carry video packets.

### 1. Hello and authentication

```jsonc
// server → client, right after connecting
{ "t": "hello", "needAuth": true }

// client → server: first time, pair with the PIN shown on the hub
{ "t": "pair", "pin": "123456", "hint": "Stage button panel", "support": false }
// server → client
{ "t": "paired", "token": "<long random token — store it>" }

// client → server: every later connection, use the token instead of the PIN
{ "t": "auth", "token": "<token>", "support": false }
// server → client on failure
{ "t": "auth-failed", "error": "This device is no longer paired. ..." }

// server → client once authenticated (after pair or auth)
{ "t": "ready", "clientId": "a1b2c3", "canControl": true, "name": "Stage button panel" }
```

- `hint` is the device name shown in the hub's paired-devices list.
- `support` tells the hub which video codecs you can decode:
  - `false`: control only, you never request video. **Use this for hardware controllers.** You then don't affect which codec the headsets stream in.
  - Omitted: assumed H.264-only.
  - Object such as `{ "h264": {"any": true, "hw": true}, "h265": {"any": true, "hw": true} }`: streams switch to a codec every viewer can decode.
- Wrong PINs are rate limited: 5 per IP, then a 5-minute lockout. Failures return `{ "t": "pair-failed", "error": "...", "retryAfter": <seconds> }`.
- Unauthenticated connections are closed after 10 minutes. Text messages are limited to 64 KB.

### 2. Calling methods

```jsonc
// client → server
{ "t": "call", "id": 1, "method": "stream:startAll", "args": [] }
// server → client
{ "t": "res", "id": 1, "ok": true, "value": [ { "id": "1WMHH8...", "ok": true } ] }
{ "t": "res", "id": 2, "ok": false, "error": "Headset is not connected." }
```

| Method | Args | Permission | Returns |
| --- | --- | --- | --- |
| `app:init` | – | view | brands, settings, current device snapshot, stream states, environment |
| `devices:refresh` | – | view | – (forces a device poll) |
| `stream:start` | `id`, `{ transport?: "usb"\|"wifi"\|"auto", restart?: bool, codec?: "h264"\|"h265"\|"av1" }` | control | – |
| `stream:stop` | `id` | control | – |
| `stream:startAll` | – | control | `[{ id, ok, error? }]` |
| `stream:stopAll` | – | control | – |
| `devices:scan` | – | control | `{ probed, connected, unverified, pendingAuth }` |
| `devices:connect` | `"192.168.1.50[:5555]"` | control | – |
| `devices:enableWireless` | `id` | control | `{ ip, port }` |
| `devices:disconnectWireless` | `id` | control | – |
| `devices:update` | `id`, `{ name?, color?, calibration?, transport?, viewMode? }` | control | updated record |
| `devices:encoders` | `id` | control | `{ live, encoders: [{ codec, name, hw }] }` |

`id` is a headset's hardware serial: the `id` field in device snapshots.

### 3. Events (server → client)

```jsonc
{ "t": "event", "event": "devices", "args": [ { "headsets": [ ... ], "adbError": null } ] }
{ "t": "event", "event": "state",   "args": [ { "id": "...", "state": "live", "transport": "wifi", "codec": "h264", "width": 1832, "height": 960, ... } ] }
{ "t": "event", "event": "notice",  "args": [ { "level": "ok" | "info" | "warn" | "error", "message": "..." } ] }
```

Each headset in `headsets` includes:
- `id`, `name`, `index` (the #number shown in the UI) and `color`
- `brand`, `model`, `variant`
- `usb` and `wifi` (adb serials or `null`) and `ip`
- `battery: { level, charging }`
- `status` (`online` | `offline` | `unauthorized` …), `trusted` and `canCast`

Stream `state` is one of `starting`, `live`, `reconnecting`, `error` or `stopped`.

### 4. Video (optional)

Send the ids of the streams you want to receive. Send the full list each time; it replaces the previous one.

```json
{ "t": "subscribe", "ids": ["1WMHH8...", "PA8110..."] }
```

You immediately get the latest codec config packet and the current group of pictures, then live packets, as **binary frames**:

```
u8   flags      bit0 = codec config packet, bit1 = key frame
u8   idLen
u8[] id         UTF-8 headset id
f64  pts        little-endian, microseconds
u8[] data       H.264/H.265: Annex B NAL units · AV1: OBUs (config packet = av1C record)
```

Prepend the config packet to key frames when (re)starting a decoder. Slow clients have delta frames dropped until the next key frame (about every 2 s). The codec of each stream is in its `state` event.

## Permissions

| Level | Who | Methods |
| --- | --- | --- |
| view | every paired client | `app:init`, `devices:refresh`, events, video |
| control | paired clients when *Allow remote control* is on | everything in the table above marked *control* |
| hub only | never remote | verifying/forgetting headsets, all settings, pairing management |

Verification over USB, the core trust step, can only happen at the hub.

## Minimal example (Node.js)

```js
// npm install ws
const WebSocket = require('ws');
const fs = require('fs');

const HUB = 'ws://192.168.1.20:8787/ws';
const TOKEN_FILE = './xrmc-token.txt';

const ws = new WebSocket(HUB);
let nextId = 1;
const pending = new Map();
const call = (method, ...args) =>
  new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ t: 'call', id, method, args }));
  });

ws.on('message', async (raw, isBinary) => {
  if (isBinary) return; // video packets, not used here
  const msg = JSON.parse(raw);
  if (msg.t === 'hello') {
    if (fs.existsSync(TOKEN_FILE)) ws.send(JSON.stringify({ t: 'auth', token: fs.readFileSync(TOKEN_FILE, 'utf8'), support: false }));
    else ws.send(JSON.stringify({ t: 'pair', pin: process.env.PIN, hint: 'Cast-all button', support: false }));
  } else if (msg.t === 'paired') {
    fs.writeFileSync(TOKEN_FILE, msg.token);
  } else if (msg.t === 'ready') {
    console.log(await call('stream:startAll'));
  } else if (msg.t === 'res') {
    const p = pending.get(msg.id);
    pending.delete(msg.id);
    if (p) (msg.ok ? p.resolve(msg.value) : p.reject(new Error(msg.error)));
  } else if (msg.t === 'event' && msg.event === 'state') {
    const s = msg.args[0];
    console.log(`${s.id}: ${s.state}`);
  }
});
```

Run it once with `PIN=123456 node cast-all.js` to pair. After that it authenticates with the saved token.
