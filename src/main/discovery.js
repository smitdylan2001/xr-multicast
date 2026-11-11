const net = require('net');
const os = require('os');

/** IPv4 addresses of this computer on real (non-loopback) interfaces. */
function localIPv4s() {
  const result = [];
  for (const [name, addrs] of Object.entries(os.networkInterfaces())) {
    for (const a of addrs || []) {
      const v4 = a.family === 'IPv4' || a.family === 4;
      if (v4 && !a.internal && !a.address.startsWith('169.254.')) {
        result.push({ iface: name, address: a.address, netmask: a.netmask });
      }
    }
  }
  return result;
}

function ipToInt(ip) {
  return ip.split('.').reduce((acc, o) => ((acc << 8) | Number(o)) >>> 0, 0);
}

function intToIp(n) {
  return [24, 16, 8, 0].map((s) => (n >>> s) & 255).join('.');
}

/**
 * Hosts to probe. Networks bigger than /24 are clamped to the /24 around this
 * computer so a scan stays quick (~254 probes per interface).
 */
function candidateHosts() {
  const hosts = new Set();
  for (const { address, netmask } of localIPv4s()) {
    let mask = ipToInt(netmask || '255.255.255.0');
    if (mask < ipToInt('255.255.255.0')) mask = ipToInt('255.255.255.0');
    const self = ipToInt(address);
    const network = (self & mask) >>> 0;
    const broadcast = (network | (~mask >>> 0)) >>> 0;
    for (let h = network + 1; h < broadcast; h++) {
      if (h !== self) hosts.add(intToIp(h));
    }
  }
  return [...hosts];
}

function probe(host, port, timeout) {
  return new Promise((resolve) => {
    const sock = new net.Socket();
    let done = false;
    const finish = (ok) => {
      if (done) return;
      done = true;
      sock.destroy();
      resolve(ok);
    };
    sock.setTimeout(timeout);
    sock.once('connect', () => finish(true));
    sock.once('timeout', () => finish(false));
    sock.once('error', () => finish(false));
    sock.connect(port, host);
  });
}

/** Find hosts on the local subnet(s) with the adb TCP port open. */
async function scanForAdb(port = 5555, { timeout = 400, concurrency = 96 } = {}) {
  const hosts = candidateHosts();
  const open = [];
  let i = 0;
  async function worker() {
    while (i < hosts.length) {
      const h = hosts[i++];
      if (await probe(h, port, timeout)) open.push(h);
    }
  }
  await Promise.all(Array.from({ length: Math.min(concurrency, hosts.length) }, worker));
  return open;
}

module.exports = { localIPv4s, scanForAdb, probe };
