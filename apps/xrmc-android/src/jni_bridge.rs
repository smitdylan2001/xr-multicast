//! Calls into the Java shell (`MainActivity.java`). Methods are called on the
//! activity object, which avoids the FindClass-from-a-native-thread class
//! loader pitfall. Java never calls into Rust: Rust polls `pollEvents()`.
//!
//! The VM and activity pointers are taken once, outside winit's event
//! callbacks: `AndroidApp` methods lock android-activity's state, and calling
//! the locking ones (e.g. `set_window_flags`) from inside a callback deadlocks.

use android_activity::AndroidApp;
use jni::objects::{JObject, JString};
use jni::{jni_sig, jni_str};

/// The Java methods Rust uses (all take no arguments).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `String codecList()`: `name|mime|hw|swOnly|lowLatency|tenBit|maxW|maxH` lines.
    CodecList,
    /// `String launchOptions()`: `key=value` lines from the launching intent.
    LaunchOptions,
    /// `String platformInfo()`: device, Android version, USB host support.
    PlatformInfo,
    /// `String pollEvents()`: USB / keyboard events since the last call, one per line.
    PollEvents,
    /// `void multicastOn()` / `multicastOff()`: Wi-Fi multicast lock for mDNS.
    MulticastOn,
    MulticastOff,
    /// `void goHome()`: move the task to the background (back button at the root).
    GoHome,
    /// `void rescanUsb()`: enumerate USB devices again.
    RescanUsb,
    /// `void keepScreenOn()` / `keepScreenOff()`: `FLAG_KEEP_SCREEN_ON` plus the casting
    /// foreground service (keeps connections alive when the app is in the background).
    KeepScreenOn,
    KeepScreenOff,
}

/// The Java VM and the activity object.
#[derive(Debug, Clone, Copy)]
pub struct Java {
    vm: *mut std::ffi::c_void,
    activity: jni::sys::jobject,
}

// SAFETY: both pointers stay valid while the activity exists (GameActivity keeps a
// global reference to the activity) and JNI handles may be used from any attached thread.
unsafe impl Send for Java {}
unsafe impl Sync for Java {}

impl Java {
    pub fn new(app: &AndroidApp) -> Java {
        Java {
            vm: app.vm_as_ptr(),
            activity: app.activity_as_ptr() as jni::sys::jobject,
        }
    }
}

fn with_activity<T>(
    java: Java,
    f: impl FnOnce(&mut jni::Env, &JObject) -> jni::errors::Result<T>,
) -> Result<T, String> {
    let vm = unsafe { jni::JavaVM::from_raw(java.vm.cast()) };
    let activity = java.activity;
    vm.attach_current_thread(|env| -> jni::errors::Result<T> {
        let activity = unsafe { env.as_cast_raw::<JObject>(&activity)? };
        let r = f(env, &activity);
        if r.is_err() && env.exception_check() {
            env.exception_describe();
            env.exception_clear();
        }
        r
    })
    .map_err(|e| e.to_string())
}

/// Call a `()Ljava/lang/String;` method.
pub fn call_string(java: Java, m: Method) -> Result<String, String> {
    with_activity(java, |env, activity| {
        let sig = jni_sig!("()Ljava/lang/String;");
        let v = match m {
            Method::CodecList => env.call_method(activity, jni_str!("codecList"), sig, &[]),
            Method::LaunchOptions => env.call_method(activity, jni_str!("launchOptions"), sig, &[]),
            Method::PlatformInfo => env.call_method(activity, jni_str!("platformInfo"), sig, &[]),
            Method::PollEvents => env.call_method(activity, jni_str!("pollEvents"), sig, &[]),
            _ => {
                return Err(jni::errors::Error::MethodNotFound {
                    name: format!("{m:?}"),
                    sig: "()Ljava/lang/String;".into(),
                });
            }
        }?
        .l()?;
        if v.is_null() {
            return Ok(String::new());
        }
        let s = env.cast_local::<JString>(v)?;
        s.try_to_string(env)
    })
}

/// Call a `()V` method.
pub fn call_void(java: Java, m: Method) -> Result<(), String> {
    with_activity(java, |env, activity| {
        let sig = jni_sig!("()V");
        match m {
            Method::MulticastOn => env.call_method(activity, jni_str!("multicastOn"), sig, &[]),
            Method::MulticastOff => env.call_method(activity, jni_str!("multicastOff"), sig, &[]),
            Method::GoHome => env.call_method(activity, jni_str!("goHome"), sig, &[]),
            Method::RescanUsb => env.call_method(activity, jni_str!("rescanUsb"), sig, &[]),
            Method::KeepScreenOn => env.call_method(activity, jni_str!("keepScreenOn"), sig, &[]),
            Method::KeepScreenOff => env.call_method(activity, jni_str!("keepScreenOff"), sig, &[]),
            _ => {
                return Err(jni::errors::Error::MethodNotFound {
                    name: format!("{m:?}"),
                    sig: "()V".into(),
                });
            }
        }?
        .v()
    })
}
