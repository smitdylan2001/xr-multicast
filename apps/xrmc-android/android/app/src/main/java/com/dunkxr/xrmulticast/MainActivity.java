package com.dunkxr.xrmulticast;

import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageManager;
import android.hardware.usb.UsbConstants;
import android.hardware.usb.UsbDevice;
import android.hardware.usb.UsbDeviceConnection;
import android.hardware.usb.UsbInterface;
import android.hardware.usb.UsbManager;
import android.media.MediaCodecInfo;
import android.media.MediaCodecInfo.CodecCapabilities;
import android.media.MediaCodecInfo.CodecProfileLevel;
import android.media.MediaCodecList;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.os.Bundle;
import android.util.Log;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.WindowManager;

import com.google.androidgamesdk.GameActivity;

import java.util.Locale;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentLinkedQueue;

/**
 * Thin Java shell around the Rust app (android-activity "game-activity" backend,
 * lib xrmc_android). Only what exists solely as Java API lives here: USB host
 * permission and file descriptors, MediaCodecList, the Wi-Fi multicast lock,
 * keyboard insets and moving the task to the back.
 *
 * Rust calls the public methods below on this object (src/jni_bridge.rs) and
 * polls {@link #pollEvents()}; Java never calls into Rust.
 */
public class MainActivity extends GameActivity {
    private static final String TAG = "xrmc-java";
    private static final String ACTION_USB_PERMISSION = "com.dunkxr.xrmulticast.USB_PERMISSION";

    private final ConcurrentLinkedQueue<String> events = new ConcurrentLinkedQueue<>();
    /** Open USB connections by device name (kept open so Rust's duplicated fd stays valid). */
    private final Map<String, UsbDeviceConnection> open = new ConcurrentHashMap<>();
    private final Map<String, String> serials = new ConcurrentHashMap<>();
    /** Devices we already asked permission for (no repeated prompts). */
    private final Set<String> asked = ConcurrentHashMap.newKeySet();
    private WifiManager.MulticastLock multicast;
    private boolean receiversRegistered;

    private final BroadcastReceiver usbReceiver = new BroadcastReceiver() {
        @Override
        public void onReceive(Context context, Intent intent) {
            String action = intent.getAction();
            UsbDevice device = intent.getParcelableExtra(UsbManager.EXTRA_DEVICE, UsbDevice.class);
            if (action == null) return;
            switch (action) {
                case ACTION_USB_PERMISSION:
                    if (device == null) return;
                    if (intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)) {
                        Log.i(TAG, "USB permission granted for " + label(device));
                        openDevice(device);
                    } else {
                        Log.w(TAG, "USB permission denied for " + label(device));
                        events.add("usb-denied\t" + clean(label(device)));
                    }
                    break;
                case UsbManager.ACTION_USB_DEVICE_ATTACHED:
                    if (device != null) Log.i(TAG, "USB attached: " + label(device));
                    scanUsb();
                    break;
                case UsbManager.ACTION_USB_DEVICE_DETACHED:
                    if (device != null) detached(device);
                    break;
                default:
                    break;
            }
        }
    };

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        WindowInsetsController insets = getWindow().getInsetsController();
        if (insets != null) {
            insets.hide(WindowInsets.Type.systemBars());
            insets.setSystemBarsBehavior(WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
        }
        // Soft keyboard height, so dialogs can move above it.
        getWindow().getDecorView().setOnApplyWindowInsetsListener((v, wi) -> {
            int ime = wi.isVisible(WindowInsets.Type.ime()) ? wi.getInsets(WindowInsets.Type.ime()).bottom : 0;
            events.add("ime\t" + ime);
            return v.onApplyWindowInsets(wi);
        });
        registerReceivers();
    }

    private void registerReceivers() {
        if (receiversRegistered) return;
        receiversRegistered = true;
        // Our own permission broadcast (sent through our PendingIntent): not exported.
        registerReceiver(usbReceiver, new IntentFilter(ACTION_USB_PERMISSION), Context.RECEIVER_NOT_EXPORTED);
        // Attach/detach are protected system broadcasts.
        IntentFilter sys = new IntentFilter();
        sys.addAction(UsbManager.ACTION_USB_DEVICE_ATTACHED);
        sys.addAction(UsbManager.ACTION_USB_DEVICE_DETACHED);
        registerReceiver(usbReceiver, sys, Context.RECEIVER_EXPORTED);
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        if (UsbManager.ACTION_USB_DEVICE_ATTACHED.equals(intent.getAction())) scanUsb();
    }

    @Override
    protected void onResume() {
        super.onResume();
        scanUsb();
    }

    @Override
    protected void onDestroy() {
        if (receiversRegistered) {
            try {
                unregisterReceiver(usbReceiver);
            } catch (IllegalArgumentException ignored) {
            }
            receiversRegistered = false;
        }
        for (UsbDeviceConnection c : open.values()) c.close();
        open.clear();
        multicastOff();
        super.onDestroy();
    }

    // ------------------------------------------------------------------ USB

    private UsbManager usb() {
        return (UsbManager) getSystemService(Context.USB_SERVICE);
    }

    /** ADB: vendor-specific interface class 0xff, subclass 0x42, protocol 0x01. */
    static boolean hasAdbInterface(UsbDevice d) {
        for (int i = 0; i < d.getInterfaceCount(); i++) {
            UsbInterface intf = d.getInterface(i);
            if (intf.getInterfaceClass() == UsbConstants.USB_CLASS_VENDOR_SPEC
                    && intf.getInterfaceSubclass() == 0x42 && intf.getInterfaceProtocol() == 0x01) {
                return true;
            }
        }
        return false;
    }

    private static String label(UsbDevice d) {
        String p = d.getProductName();
        String m = d.getManufacturerName();
        String name = p != null ? p : String.format(Locale.ROOT, "USB device %04x:%04x", d.getVendorId(), d.getProductId());
        return m != null && p != null && !p.startsWith(m) ? m + " " + p : name;
    }

    private static String clean(String s) {
        return s == null ? "" : s.replace('\t', ' ').replace('\n', ' ');
    }

    /** Ask for permission / open every ADB device that is plugged in. */
    public void rescanUsb() {
        runOnUiThread(this::scanUsb);
    }

    private void scanUsb() {
        UsbManager m = usb();
        if (m == null || !getPackageManager().hasSystemFeature(PackageManager.FEATURE_USB_HOST)) return;
        for (UsbDevice d : m.getDeviceList().values()) {
            if (!hasAdbInterface(d)) continue;
            if (open.containsKey(d.getDeviceName())) continue;
            if (m.hasPermission(d)) {
                openDevice(d);
            } else if (asked.add(d.getDeviceName())) {
                Log.i(TAG, "requesting USB permission for " + label(d));
                Intent i = new Intent(ACTION_USB_PERMISSION).setPackage(getPackageName());
                PendingIntent pi = PendingIntent.getBroadcast(this, 0, i,
                        PendingIntent.FLAG_MUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
                m.requestPermission(d, pi);
            }
        }
    }

    private synchronized void openDevice(UsbDevice d) {
        if (open.containsKey(d.getDeviceName())) return;
        if (!hasAdbInterface(d)) {
            events.add("usb-error\t" + clean(label(d)) + " has no ADB interface: enable USB debugging (developer mode) on the headset.");
            return;
        }
        UsbDeviceConnection c = usb().openDevice(d);
        if (c == null) {
            events.add("usb-error\tCould not open " + clean(label(d)) + ".");
            return;
        }
        String serial = null;
        try {
            serial = d.getSerialNumber();
        } catch (SecurityException e) {
            Log.w(TAG, "no serial for " + label(d), e);
        }
        if (serial == null || serial.isEmpty()) serial = c.getSerial();
        if (serial == null || serial.isEmpty()) serial = d.getDeviceName();
        open.put(d.getDeviceName(), c);
        serials.put(d.getDeviceName(), serial);
        Log.i(TAG, "opened " + label(d) + " serial " + serial + " fd " + c.getFileDescriptor());
        events.add("usb-attached\t" + c.getFileDescriptor() + "\t" + clean(serial) + "\t" + clean(label(d)));
    }

    private void detached(UsbDevice d) {
        String name = d.getDeviceName();
        asked.remove(name);
        UsbDeviceConnection c = open.remove(name);
        String serial = serials.remove(name);
        if (c != null) {
            events.add("usb-detached\t" + clean(serial) + "\t" + clean(label(d)));
            c.close();
        }
        Log.i(TAG, "USB detached: " + label(d));
    }

    // -------------------------------------------------------- called from Rust

    /** Events since the last call, one per line (tab-separated fields). */
    public String pollEvents() {
        StringBuilder sb = new StringBuilder();
        String e;
        while ((e = events.poll()) != null) sb.append(e).append('\n');
        return sb.toString();
    }

    /** Video decoders: {@code name|mime|hw|swOnly|lowLatency|tenBit|maxW|maxH} per line. */
    public String codecList() {
        StringBuilder sb = new StringBuilder();
        try {
            for (MediaCodecInfo info : new MediaCodecList(MediaCodecList.REGULAR_CODECS).getCodecInfos()) {
                if (info.isEncoder() || info.isAlias()) continue;
                for (String type : info.getSupportedTypes()) {
                    String t = type.toLowerCase(Locale.ROOT);
                    if (!t.equals("video/avc") && !t.equals("video/hevc") && !t.equals("video/av01")) continue;
                    CodecCapabilities caps = info.getCapabilitiesForType(type);
                    boolean lowLatency = caps.isFeatureSupported(CodecCapabilities.FEATURE_LowLatency);
                    boolean ten = false;
                    for (CodecProfileLevel pl : caps.profileLevels) {
                        if (t.equals("video/hevc") && (pl.profile == CodecProfileLevel.HEVCProfileMain10
                                || pl.profile == CodecProfileLevel.HEVCProfileMain10HDR10
                                || pl.profile == CodecProfileLevel.HEVCProfileMain10HDR10Plus)) ten = true;
                        if (t.equals("video/av01") && (pl.profile == CodecProfileLevel.AV1ProfileMain10
                                || pl.profile == CodecProfileLevel.AV1ProfileMain10HDR10
                                || pl.profile == CodecProfileLevel.AV1ProfileMain10HDR10Plus)) ten = true;
                    }
                    int mw = 0, mh = 0;
                    MediaCodecInfo.VideoCapabilities v = caps.getVideoCapabilities();
                    if (v != null) {
                        mw = v.getSupportedWidths().getUpper();
                        mh = v.getSupportedHeights().getUpper();
                    }
                    sb.append(info.getName()).append('|').append(t).append('|')
                            .append(info.isHardwareAccelerated() ? 1 : 0).append('|')
                            .append(info.isSoftwareOnly() ? 1 : 0).append('|')
                            .append(lowLatency ? 1 : 0).append('|')
                            .append(ten ? 1 : 0).append('|')
                            .append(mw).append('|').append(mh).append('\n');
                }
            }
        } catch (RuntimeException e) {
            Log.e(TAG, "MediaCodecList failed", e);
        }
        return sb.toString();
    }

    /** {@code key=value} lines from the launching intent (testing and kiosk setups). */
    public String launchOptions() {
        Intent i = getIntent();
        Bundle b = i != null ? i.getExtras() : null;
        if (b == null) return "";
        StringBuilder sb = new StringBuilder();
        for (String k : new String[] {"connect", "noverify", "demo", "castonstart", "show", "uiscale"}) {
            Object v = b.get(k);
            if (v != null) sb.append(k).append('=').append(v).append('\n');
        }
        return sb.toString();
    }

    public String platformInfo() {
        PackageManager pm = getPackageManager();
        boolean usbHost = pm.hasSystemFeature(PackageManager.FEATURE_USB_HOST);
        boolean vulkan11 = pm.hasSystemFeature(PackageManager.FEATURE_VULKAN_HARDWARE_VERSION, 0x00401000);
        return Build.MANUFACTURER + " " + Build.MODEL
                + " · Android " + Build.VERSION.RELEASE + " (API " + Build.VERSION.SDK_INT + ")"
                + " · Vulkan 1.1 " + (vulkan11 ? "yes" : "no")
                + " · USB host " + (usbHost ? "yes" : "no");
    }

    public synchronized void multicastOn() {
        WifiManager wifi = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);
        if (wifi == null) return;
        if (multicast == null) {
            multicast = wifi.createMulticastLock("xrmc-mdns");
            multicast.setReferenceCounted(false);
        }
        if (!multicast.isHeld()) multicast.acquire();
        Log.i(TAG, "multicast lock acquired");
    }

    public synchronized void multicastOff() {
        if (multicast != null && multicast.isHeld()) {
            multicast.release();
            Log.i(TAG, "multicast lock released");
        }
    }

    public void keepScreenOn() {
        runOnUiThread(() -> getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON));
    }

    public void keepScreenOff() {
        runOnUiThread(() -> getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON));
    }

    /** Back button with nothing left to close: leave the app running in the background. */
    public void goHome() {
        runOnUiThread(() -> moveTaskToBack(true));
    }
}
