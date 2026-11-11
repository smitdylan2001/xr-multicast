const fs = require('fs');
const path = require('path');
const { DEFAULT_SETTINGS } = require('./config');

// Small JSON-file store for verified ("trusted") headsets and app settings.
class Store {
  constructor(file) {
    this.file = file;
    this.data = { headsets: {}, settings: { ...DEFAULT_SETTINGS } };
    try {
      const parsed = JSON.parse(fs.readFileSync(file, 'utf8'));
      this.data.headsets = parsed.headsets || {};
      this.data.settings = { ...DEFAULT_SETTINGS, ...(parsed.settings || {}) };
      if (parsed.remote && typeof parsed.remote === 'object') this.data.remote = parsed.remote;
    } catch {
      /* first run or unreadable file */
    }
    this._timer = null;
  }

  save() {
    clearTimeout(this._timer);
    this._timer = setTimeout(() => this.flush(), 200);
  }

  flush() {
    clearTimeout(this._timer);
    fs.mkdirSync(path.dirname(this.file), { recursive: true });
    const tmp = `${this.file}.tmp`;
    fs.writeFileSync(tmp, JSON.stringify(this.data, null, 2));
    fs.renameSync(tmp, this.file);
  }

  get settings() {
    return this.data.settings;
  }

  updateSettings(patch) {
    Object.assign(this.data.settings, patch);
    this.save();
    return this.data.settings;
  }

  getHeadset(serial) {
    return this.data.headsets[serial] || null;
  }

  allHeadsets() {
    return Object.values(this.data.headsets);
  }

  upsertHeadset(serial, patch) {
    const cur = this.data.headsets[serial] || { serial };
    this.data.headsets[serial] = { ...cur, ...patch, serial };
    this.save();
    return this.data.headsets[serial];
  }

  removeHeadset(serial) {
    delete this.data.headsets[serial];
    this.save();
  }
}

module.exports = { Store };
