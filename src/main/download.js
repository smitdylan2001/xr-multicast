const fs = require('fs');
const https = require('https');

/**
 * Download a URL to a file over HTTPS, following redirects.
 * @param {(p: {received: number, total: number}) => void} [onProgress]
 */
function download(url, dest, onProgress, redirects = 0) {
  return new Promise((resolve, reject) => {
    if (redirects > 10) return reject(new Error('Too many redirects'));
    if (!url.startsWith('https://')) return reject(new Error(`Refusing non-HTTPS download: ${url}`));
    https
      .get(url, { headers: { 'User-Agent': 'xr-multicast' } }, (res) => {
        if ([301, 302, 303, 307, 308].includes(res.statusCode) && res.headers.location) {
          res.resume();
          return resolve(download(new URL(res.headers.location, url).toString(), dest, onProgress, redirects + 1));
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`GET ${url} -> HTTP ${res.statusCode}`));
        }
        const total = Number(res.headers['content-length']) || 0;
        let received = 0;
        const tmp = `${dest}.part`;
        const out = fs.createWriteStream(tmp);
        res.on('data', (c) => {
          received += c.length;
          if (onProgress) onProgress({ received, total });
        });
        res.on('error', reject);
        res.pipe(out);
        out.on('finish', () => {
          out.close(() => {
            fs.renameSync(tmp, dest);
            resolve(dest);
          });
        });
        out.on('error', reject);
      })
      .on('error', reject);
  });
}

module.exports = { download };
