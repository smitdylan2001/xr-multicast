# XR MultiCast for Android (`xrmc-android`)

The phone or tablet is the hub. Headsets plug into it over USB (USB host / OTG) or are reached over Wi-Fi. The app runs its own ADB client (pure Rust, no Google `adb`), starts scrcpy-server on each headset, and decodes the video itself with MediaCodec. No PC is needed.

The UI is the same egui app as the desktop (`apps/xrmc-desktop`, used as a library). It runs on GameActivity with Vulkan through wgpu, with OpenGL ES as a fallback.

## Build

```bash
apps/xrmc-android/build.sh                               # release APK for arm64-v8a + x86_64
apps/xrmc-android/build.sh release --abi arm64-v8a       # phones and tablets only (24 MB)
apps/xrmc-android/build.sh release --abi x86_64 --fast   # emulator, quicker Rust build
```

The build script runs these steps:

1. cargo-ndk builds `libxrmc_android.so` (plus `libc++_shared.so`) into `android/app/src/main/jniLibs`.
2. It copies scrcpy-server v5.0.1 into the APK assets and `build/icon.png` in as the launcher icon.
3. It runs `./gradlew assembleRelease`, signed with the local debug key.
4. It copies the APK to `dist-native/android/`.

Requirements:

- Rust with cargo-ndk 4.
- Android SDK 36, NDK r27 and JDK 17. The script sources `~/xrmc-spikes/env.sh`, or the file set in `$XRMC_ANDROID_ENV`.
- scrcpy-server, taken from `$XRMC_SCRCPY_SERVER` or the repository's `vendor/`.

Rust builds into `$CARGO_TARGET_DIR`, by default `~/.cache/xrmc-target-android`.

The app needs Android 13+ (minSdk 33) and Vulkan 1.1. Use your own keystore for distribution.

## Layout

| Path | What |
|---|---|
| `src/lib.rs` | `android_main`: logging, the codec list, launch options, extracting scrcpy-server to app storage, `HubPaths` (key and settings in internal storage), eframe start. |
| `src/platform.rs` | `Platform` for Android: USB file descriptors to `AdbRegistry::add_usb_fd` and `remove`, the multicast lock while scanning, keep-screen-on and the foreground service, back button, keyboard height, notices. |
| `src/jni_bridge.rs` | Calls into `MainActivity` on the activity object. Rust polls `pollEvents()`; Java never calls into Rust. |
| `src/ime.rs` | Soft-keyboard bridge (winit 0.30 drops GameActivity text events). It sets a URI keyboard for addresses. |
| `android/…/MainActivity.java` | `UsbManager` enumeration, permission requests, `openDevice` → fd, attach/detach receivers, `MediaCodecList` → codec list, multicast lock, window insets. |
| `android/…/CastService.java` | Foreground service (`connectedDevice`) while streams are shown. Without it, Android cuts a background app's network after a few seconds. |
| `android/…/res/xml/device_filter.xml` | Opens the app when an ADB-capable headset is plugged in. It matches any ADB interface (class 0xff/0x42/0x01) plus known XR vendor IDs. |

Decoding lives in `crates/xrmc-decode/src/android/`:

- NDK `AMediaCodec`, synchronous, low-latency keys.
- Pictures are picked up as soon as they are decoded.
- `MediaImage2` and stride layouts are converted to NV12/I420/P010.
- SPS and AV1 size parsing.
- H.264 SPS "no reordering" rewrite.

## Testing on emulators (no headset needed)

1. Start two AVDs on private ports:
   - Hub: `emulator -avd xrmc_api35 -port 5570 -read-only -no-snapshot -gpu swiftshader_indirect -no-window`
   - "Headset": the same command with `-port 5572`.
2. Install the APK on the hub: `adb -s emulator-5570 install -r -g dist-native/android/xr-multicast-*-universal-release.apk`.
3. Launch it pointed at the other emulator's adbd:

   ```bash
   adb -s emulator-5570 shell am start -n com.dunkxr.xrmulticast/.MainActivity \
     --es connect 10.0.2.2:5573 --ez noverify true --ez castonstart true
   ```

4. Read the logs with `adb -s emulator-5570 logcat -s xrmc xrmc-java`. Every 10 s the decoder logs fps and MediaCodec latency.

Launch options (intent extras):

| Extra | Effect |
|---|---|
| `connect` | Comma-separated addresses to connect to. |
| `noverify` | Allow casting without USB verification (test devices added by IP). |
| `castonstart` | Cast every castable headset once. |
| `demo N` | N synthetic headsets. |
| `show codecs\|settings\|add-ip\|…` | Open a dialog. |
| `uiscale` | Interface size. |
| `decoder c2.android.avc.decoder,…` | MediaCodec decoders to prefer. |

## Known gaps

- USB OTG with real headsets is untested: the emulator has no USB host. The permission flow, fd hand-over, detach handling and the RSA prompt on the headset are wired up but unverified.
- Decoding copies frames to the CPU. Zero-copy is a TODO in `crates/xrmc-decode/src/android/mod.rs`: `AImageReader` → `AHardwareBuffer` → Vulkan import with YCbCr conversion.
- mDNS uses the core's `mdns-sd` with a Wi-Fi multicast lock held while scanning. It can't be tested on the emulator's NAT network.
- The Android 11+ wireless-debugging pairing (TLS) isn't supported. Use "Enable Wi-Fi" over USB, or `adb tcpip`-style port 5555.
- A display density change only takes effect after the activity is recreated. This is a winit 0.30 limitation; rotation and resizing work.
- There's no static bold system font on newer Android, so bold text uses `DroidSans-Bold` where present.
- Casting continues in the background (foreground service), and frames are decoded even though nothing is shown.
