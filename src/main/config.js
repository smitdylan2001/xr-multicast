// The scrcpy client/server protocol is internal and versioned: the server
// refuses to start unless the version argument matches its own exactly.
// If you bump this, re-run `npm run setup` and re-check stream-session.js
// against doc/develop.md of that scrcpy release.
const SCRCPY_VERSION = '5.0.1';

const SCRCPY_SERVER_URL = `https://github.com/Genymobile/scrcpy/releases/download/v${SCRCPY_VERSION}/scrcpy-server-v${SCRCPY_VERSION}`;

const PLATFORM_TOOLS_URLS = {
  win32: 'https://dl.google.com/android/repository/platform-tools-latest-windows.zip',
  darwin: 'https://dl.google.com/android/repository/platform-tools-latest-darwin.zip',
  linux: 'https://dl.google.com/android/repository/platform-tools-latest-linux.zip',
};

// Google's terms for Android SDK Platform-Tools; users accept these in-app before adb is downloaded.
const ADB_TERMS_URL = 'https://developer.android.com/studio/terms';

const REMOTE_SERVER_PATH = '/data/local/tmp/xrmc-scrcpy-server.jar';
const REMOTE_LIST_SERVER_PATH = '/data/local/tmp/xrmc-scrcpy-list.jar';

const DEFAULT_ADB_TCP_PORT = 5555;

const QUALITY_PRESETS = {
  low: { label: 'Low (1280px · 4 Mbps · 30 fps)', maxSize: 1280, bitRate: 4_000_000, maxFps: 30 },
  balanced: { label: 'Balanced (1920px · 8 Mbps · 60 fps)', maxSize: 1920, bitRate: 8_000_000, maxFps: 60 },
  high: { label: 'High (2560px · 16 Mbps · 60 fps)', maxSize: 2560, bitRate: 16_000_000, maxFps: 60 },
  max: { label: 'Native (full res · 24 Mbps · 72 fps)', maxSize: 0, bitRate: 24_000_000, maxFps: 72 },
};

// Codecs the app can request from the headset encoder and decode on the desktop.
const CODECS = {
  h264: { label: 'H.264', description: 'Works everywhere' },
  h265: { label: 'H.265 (HEVC)', description: '~30–50% less bandwidth than H.264 at the same quality' },
  av1: { label: 'AV1', description: 'Most efficient; hardware encoders are rare on headsets' },
};

// "Auto" order: H.265 first (mature low-latency hardware encoders on XR chips),
// then AV1, with H.264 as the universal fallback.
const AUTO_CODEC_ORDER = ['h265', 'av1', 'h264'];

const DEFAULT_SETTINGS = {
  requireUsbVerification: true,
  autoReconnectWifi: true,
  autoCastOnConnect: false,
  autoRetryStreams: true,
  usbQuality: 'high',
  wifiQuality: 'balanced',
  usbCodec: 'auto',
  wifiCodec: 'auto',
  tenBit: false,
  usbCustom: { maxSize: 2560, bitRateMbps: 20, maxFps: 60 },
  wifiCustom: { maxSize: 1920, bitRateMbps: 10, maxFps: 60 },
  singleEyeStream: false,
  defaultViewMode: 'corrected',
  scanPort: DEFAULT_ADB_TCP_PORT,
  gridColumns: 'auto',
  remoteEnabled: false,
  remotePort: 8787,
  remoteAllowControl: true,
};

module.exports = {
  SCRCPY_VERSION,
  SCRCPY_SERVER_URL,
  PLATFORM_TOOLS_URLS,
  ADB_TERMS_URL,
  REMOTE_SERVER_PATH,
  REMOTE_LIST_SERVER_PATH,
  DEFAULT_ADB_TCP_PORT,
  QUALITY_PRESETS,
  CODECS,
  AUTO_CODEC_ORDER,
  DEFAULT_SETTINGS,
};
