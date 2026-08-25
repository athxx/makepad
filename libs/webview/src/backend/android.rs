//! Android backend.
//!
//! Drives an `android.webkit.WebView` overlay laid on top of Makepad's render
//! surface. Unlike Apple — where the overlay lives entirely in
//! `makepad-platform` and this crate is a thin `CxOsOp` wrapper — Android has no
//! platform-side web view, so this backend owns the native object directly and
//! reaches Java through JNI, mirroring the conventions already used by the
//! platform crate's camera-preview overlay (`to_java_attach_camera_preview` &
//! friends in `os::linux::android::android_jni`).
//!
//! ## Threading
//!
//! Every `android.webkit.WebView` method must run on the Android **UI thread**;
//! Makepad's Rust code runs on its own render thread. We therefore never touch a
//! `WebView` object from Rust. Instead we call thin helper methods on the
//! `MakepadActivity` (`makepadWebViewCreate`, `makepadWebViewLoadUrl`, …) via
//! JNI; each helper hops onto the UI thread with `runOnUiThread` and manages the
//! real `WebView` there — exactly how the camera preview overlay works today.
//! See the "Required platform additions" note at the bottom of this file for the
//! Java-side methods and JNI callbacks this backend expects.
//!
//! ## Bridge
//!
//! The `@JavascriptInterface` object is installed Java-side under the name
//! [`BRIDGE_INTERFACE`]; page JS reaches Rust via
//! `window.vsbridge.postMessage(...)`. The document-start injection script
//! (which establishes `window.vs`) is produced by
//! [`crate::bridge::injection_script`] and re-run on every navigation by the
//! Java `WebViewClient` (`onPageStarted` → `evaluateJavascript`). Inbound
//! messages and load-lifecycle events arrive back on the UI thread through JNI
//! callbacks (`Java_dev_makepad_android_MakepadNative_onWebView*`), which push
//! into a process-wide queue keyed by the web view's `id`; `drain_events` pulls
//! from that queue on the render thread.

use crate::backend::WebViewBackend;
use crate::bridge::{self, BridgeMessage};
use crate::{Result, WebViewError, WebViewEvent, WebViewOptions};
use makepad_platform::{Area, Cx};

use makepad_android_state::{get_activity, get_java_vm};
use makepad_jni_sys as jni_sys;
// The platform crate re-exports these JNI helper macros at its crate root via
// `#[macro_export]`; we use the same ones the platform's own Android code uses
// so our call sites stay byte-for-byte consistent with it.
use makepad_platform::{call_void_method, get_utf_str};

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Mutex, OnceLock};

/// Name of the `@JavascriptInterface` object installed on the page. Page JS
/// calls `window.vsbridge.postMessage(json)`; the bridge's document-start script
/// forwards `window.vs.postMessage(...)` through it (see `injection_script`
/// below).
const BRIDGE_INTERFACE: &str = "vsbridge";

// --- Inbound event queue ---------------------------------------------------
//
// The `@JavascriptInterface` callback and the `WebViewClient` load callbacks run
// on the Android UI thread, while `drain_events` runs on Makepad's render
// thread. We bridge the two with a process-wide queue keyed by the web view's
// `id` (the `WebViewOptions::id` the widget layer passes in), mirroring how the
// platform layer keys its own Java→Rust backchannels by a `u64` id.

static EVENT_QUEUES: OnceLock<Mutex<HashMap<u64, Vec<WebViewEvent>>>> = OnceLock::new();

fn event_queues() -> &'static Mutex<HashMap<u64, Vec<WebViewEvent>>> {
    EVENT_QUEUES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Push an event for web view `id`. Called from the UI thread (JNI callbacks).
fn push_event(id: u64, event: WebViewEvent) {
    if let Ok(mut map) = event_queues().lock() {
        map.entry(id).or_default().push(event);
    }
}

/// Take all queued events for web view `id`. Called from the render thread.
fn take_events(id: u64) -> Vec<WebViewEvent> {
    if let Ok(mut map) = event_queues().lock() {
        if let Some(queue) = map.get_mut(&id) {
            return core::mem::take(queue);
        }
    }
    Vec::new()
}

/// Drop the entire queue for `id` once the web view is gone.
fn drop_queue(id: u64) {
    if let Ok(mut map) = event_queues().lock() {
        map.remove(&id);
    }
}

// --- Backend ---------------------------------------------------------------

pub(crate) struct AndroidBackend {
    /// Stable identity shared with the Java side and used to key the inbound
    /// event queue.
    id: u64,
    /// Whether the native `WebView` has been created Java-side yet.
    created: bool,
    /// Last url we asked the WebView to load (avoids redundant `loadUrl`s).
    current_url: String,
}

impl AndroidBackend {
    /// The document-start user script establishing `window.vs`, wired so that
    /// `window.vs.postMessage(obj)` reaches Rust through the `@JavascriptInterface`
    /// object. The Java `WebViewClient` evaluates this on every `onPageStarted`.
    fn injection_script() -> String {
        // `window.vsbridge.postMessage(%MSG%)` — the interface method takes the
        // already-stringified JSON that `injection_script` binds to `%MSG%`.
        bridge::injection_script(&format!(
            "window.{}.postMessage(%MSG%)",
            BRIDGE_INTERFACE
        ))
    }
}

impl WebViewBackend for AndroidBackend {
    fn new(cx: &mut Cx, opts: &WebViewOptions) -> Result<Self> {
        // The activity must exist before we can attach anything to its view
        // hierarchy. If it doesn't, there is no window to host the overlay.
        if get_activity().is_null() {
            return Err(WebViewError::Backend(
                "android activity is not available yet".to_string(),
            ));
        }

        let mut backend = Self {
            id: opts.id,
            created: false,
            current_url: String::new(),
        };

        // Ensure a clean queue for this id (a previous view with the same id
        // may have left events behind).
        drop_queue(backend.id);

        let injection = Self::injection_script();
        unsafe {
            to_java_webview_create(
                backend.id,
                BRIDGE_INTERFACE,
                &injection,
                &opts.url,
                opts.visible,
            );
        }
        backend.created = true;
        backend.current_url = opts.url.clone();

        let _ = cx;
        Ok(backend)
    }

    fn set_url(&mut self, cx: &mut Cx, url: &str) {
        let _ = cx;
        if !self.created {
            return;
        }
        if self.current_url != url {
            unsafe {
                to_java_webview_load_url(self.id, url);
            }
            self.current_url = url.to_string();
        }
    }

    fn update_rect(&mut self, cx: &mut Cx, area: Area, visible: bool) {
        if !self.created {
            return;
        }

        if !visible || !area.is_valid(cx) {
            unsafe {
                to_java_webview_update_rect(self.id, 0, 0, 0, 0, false);
            }
            return;
        }

        // Makepad hands us a logical (dp) rect; the native view hierarchy is in
        // physical pixels. Convert the same way the platform's camera-preview
        // op does (`layout_rect_to_physical_pixels`): clip to the visible area,
        // then scale by the area's dpi factor.
        let rect = area.clipped_rect(cx);
        let dpi = cx.get_dpi_factor_of(&area);
        let left = (rect.pos.x * dpi) as i32;
        let top = (rect.pos.y * dpi) as i32;
        let right = ((rect.pos.x + rect.size.x) * dpi) as i32;
        let bottom = ((rect.pos.y + rect.size.y) * dpi) as i32;

        unsafe {
            to_java_webview_update_rect(self.id, left, top, right, bottom, true);
        }
    }

    fn history_go(&mut self, cx: &mut Cx, delta: i32) {
        let _ = cx;
        if self.created && delta != 0 {
            // Java side loops `goBack`/`goForward` `|delta|` times, matching the
            // WKWebView `history_go` semantics.
            unsafe {
                to_java_webview_history_go(self.id, delta);
            }
        }
    }

    fn eval_js(&mut self, cx: &mut Cx, script: &str) {
        let _ = cx;
        if self.created {
            unsafe {
                to_java_webview_eval_js(self.id, script);
            }
        }
    }

    fn drain_events(&mut self) -> Vec<WebViewEvent> {
        take_events(self.id)
    }

    fn detach(&mut self, cx: &mut Cx) {
        let _ = cx;
        if self.created {
            unsafe {
                to_java_webview_detach(self.id);
            }
            self.created = false;
        }
        drop_queue(self.id);
    }
}

impl Drop for AndroidBackend {
    fn drop(&mut self) {
        // Belt-and-suspenders: if the widget dropped us without an explicit
        // `detach`, still tear down the Java-side view and free the queue.
        if self.created && !get_activity().is_null() {
            unsafe {
                to_java_webview_detach(self.id);
            }
            self.created = false;
        }
        drop_queue(self.id);
    }
}

// --- Rust → Java (JNI) ------------------------------------------------------
//
// These mirror the platform crate's `to_java_*` helpers (see
// `os::linux::android::android_jni`): attach the current thread to the JVM, then
// call a `void` helper method on the activity. Every helper hops to the UI
// thread Java-side before touching the real `WebView`.

/// Attach the current (render) thread to the JVM and return its `JNIEnv`.
///
/// Reimplemented locally (rather than calling the platform's private
/// `attach_jni_env`) so this crate only depends on the stable
/// `makepad_android_state` public API. Byte-for-byte identical to the platform's
/// own implementation.
unsafe fn attach_jni_env() -> *mut jni_sys::JNIEnv {
    let mut env: *mut jni_sys::JNIEnv = std::ptr::null_mut();
    let attach_current_thread = (**get_java_vm()).AttachCurrentThread.unwrap();
    let res = attach_current_thread(get_java_vm(), &mut env, std::ptr::null_mut());
    assert!(res == 0);
    env
}

/// Build a Java `String` from a Rust `&str`. Returns null on interior-NUL.
unsafe fn new_jstring(env: *mut jni_sys::JNIEnv, value: &str) -> jni_sys::jstring {
    match CString::new(value) {
        Ok(c) => (**env).NewStringUTF.unwrap()(env, c.as_ptr()),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Create the native `WebView`, enable JavaScript, install the
/// `@JavascriptInterface` object under `interface_name`, arm the `WebViewClient`
/// to (re)inject `injection` at document-start, then `loadUrl(url)`.
unsafe fn to_java_webview_create(
    id: u64,
    interface_name: &str,
    injection: &str,
    url: &str,
    visible: bool,
) {
    let env = attach_jni_env();
    let interface_name = new_jstring(env, interface_name);
    let injection = new_jstring(env, injection);
    let url = new_jstring(env, url);
    call_void_method!(
        env,
        get_activity(),
        "makepadWebViewCreate",
        "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;Z)V",
        id as jni_sys::jlong,
        interface_name,
        injection,
        url,
        visible as jni_sys::jboolean as std::ffi::c_uint
    );
}

unsafe fn to_java_webview_load_url(id: u64, url: &str) {
    let env = attach_jni_env();
    let url = new_jstring(env, url);
    call_void_method!(
        env,
        get_activity(),
        "makepadWebViewLoadUrl",
        "(JLjava/lang/String;)V",
        id as jni_sys::jlong,
        url
    );
}

unsafe fn to_java_webview_update_rect(
    id: u64,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    visible: bool,
) {
    let env = attach_jni_env();
    call_void_method!(
        env,
        get_activity(),
        "makepadWebViewUpdateRect",
        "(JIIIIZ)V",
        id as jni_sys::jlong,
        left,
        top,
        right,
        bottom,
        visible as jni_sys::jboolean as std::ffi::c_uint
    );
}

unsafe fn to_java_webview_history_go(id: u64, delta: i32) {
    let env = attach_jni_env();
    call_void_method!(
        env,
        get_activity(),
        "makepadWebViewHistoryGo",
        "(JI)V",
        id as jni_sys::jlong,
        delta
    );
}

unsafe fn to_java_webview_eval_js(id: u64, script: &str) {
    let env = attach_jni_env();
    let script = new_jstring(env, script);
    call_void_method!(
        env,
        get_activity(),
        "makepadWebViewEvalJs",
        "(JLjava/lang/String;)V",
        id as jni_sys::jlong,
        script
    );
}

unsafe fn to_java_webview_detach(id: u64) {
    let env = attach_jni_env();
    call_void_method!(
        env,
        get_activity(),
        "makepadWebViewDetach",
        "(J)V",
        id as jni_sys::jlong
    );
}

// --- Java → Rust (JNI callbacks) -------------------------------------------
//
// These run on the Android UI thread. They mirror the platform crate's existing
// `Java_dev_makepad_android_MakepadNative_*` natives (e.g. `onHttpResponse`).
// Each pushes a `WebViewEvent` into the queue keyed by the web view's `id`;
// `AndroidBackend::drain_events` collects them on the render thread.
//
// The corresponding `native` method declarations must be added to
// `MakepadNative.java` (see "Required platform additions" below).

/// Page JS called `window.vsbridge.postMessage(json)`. `json_message` is the
/// serialized `BridgeMessage` produced by the bridge's `postMessage`
/// (`JSON.stringify(msg)`), i.e. an object with `kind` and `payload` fields.
#[no_mangle]
extern "C" fn Java_dev_makepad_android_MakepadNative_onWebViewMessage(
    env: *mut jni_sys::JNIEnv,
    _: jni_sys::jobject,
    id: jni_sys::jlong,
    json_message: jni_sys::jstring,
) {
    if json_message.is_null() {
        return;
    }
    let json = unsafe { get_utf_str!(env, json_message) }.to_string();
    // The bridge transports a `{kind, payload}` object; parse it back into a
    // `BridgeMessage`. Malformed payloads are dropped rather than surfaced.
    if let Ok(msg) = BridgeMessage::deserialize_json(&json) {
        push_event(id as u64, WebViewEvent::Message(msg));
    }
}

#[no_mangle]
extern "C" fn Java_dev_makepad_android_MakepadNative_onWebViewLoadStarted(
    env: *mut jni_sys::JNIEnv,
    _: jni_sys::jobject,
    id: jni_sys::jlong,
    url: jni_sys::jstring,
) {
    let url = if url.is_null() {
        String::new()
    } else {
        unsafe { get_utf_str!(env, url) }.to_string()
    };
    push_event(id as u64, WebViewEvent::LoadStarted { url });
}

#[no_mangle]
extern "C" fn Java_dev_makepad_android_MakepadNative_onWebViewLoadFinished(
    env: *mut jni_sys::JNIEnv,
    _: jni_sys::jobject,
    id: jni_sys::jlong,
    url: jni_sys::jstring,
) {
    let url = if url.is_null() {
        String::new()
    } else {
        unsafe { get_utf_str!(env, url) }.to_string()
    };
    push_event(id as u64, WebViewEvent::LoadFinished { url });
}

#[no_mangle]
extern "C" fn Java_dev_makepad_android_MakepadNative_onWebViewLoadFailed(
    env: *mut jni_sys::JNIEnv,
    _: jni_sys::jobject,
    id: jni_sys::jlong,
    url: jni_sys::jstring,
    error: jni_sys::jstring,
) {
    let url = if url.is_null() {
        String::new()
    } else {
        unsafe { get_utf_str!(env, url) }.to_string()
    };
    let error = if error.is_null() {
        String::new()
    } else {
        unsafe { get_utf_str!(env, error) }.to_string()
    };
    push_event(id as u64, WebViewEvent::LoadFailed { url, error });
}

// Bring the micro-serde trait into scope for `BridgeMessage::deserialize_json`.
use makepad_micro_serde::DeJson;

// ---------------------------------------------------------------------------
// Required platform additions (device-side; NOT made in this crate)
// ---------------------------------------------------------------------------
//
// TODO(android-device): This backend calls Java helper methods that do not yet
// exist on `MakepadActivity`, and JNI callbacks whose `native` declarations do
// not yet exist on `MakepadNative`. They must be added to the platform's Android
// support (in `tools/cargo_makepad/src/android/java/dev/makepad/android/`)
// before this compiles/links on-device:
//
//   MakepadActivity.java — each method does `runOnUiThread` and manages a
//   per-id `WebView` inside a dedicated overlay `FrameLayout` (mirror the
//   existing `mCameraPreviewOverlay` machinery):
//     void makepadWebViewCreate(long id, String interfaceName,
//                               String injectionScript, String url, boolean visible)
//       - new WebView(this); settings.setJavaScriptEnabled(true);
//       - addJavascriptInterface(new Object(){
//             @JavascriptInterface public void postMessage(String json){
//                 MakepadNative.onWebViewMessage(id, json); } }, interfaceName);
//       - setWebViewClient(new WebViewClient(){
//             onPageStarted(v,url,fav){ v.evaluateJavascript(injectionScript,null);
//                                       MakepadNative.onWebViewLoadStarted(id,url); }
//             onPageFinished(v,url){ MakepadNative.onWebViewLoadFinished(id,url); }
//             onReceivedError(...){ MakepadNative.onWebViewLoadFailed(id,url,desc); } });
//       - add to overlay FrameLayout; loadUrl(url); set visibility from `visible`.
//     void makepadWebViewLoadUrl(long id, String url)
//     void makepadWebViewUpdateRect(long id, int l, int t, int r, int b, boolean visible)
//       - position/size via FrameLayout.LayoutParams (leftMargin/topMargin, w/h)
//         in physical px, and setVisibility(visible?VISIBLE:GONE).
//     void makepadWebViewHistoryGo(long id, int delta)
//       - loop goBack()/goForward() |delta| times (respecting canGoBack/Forward).
//     void makepadWebViewEvalJs(long id, String script)  -> evaluateJavascript.
//     void makepadWebViewDetach(long id)
//       - overlay.removeView(webView); webView.destroy(); forget the id.
//
//   MakepadNative.java — declare the four callbacks this file exports:
//     public static native void onWebViewMessage(long id, String json);
//     public static native void onWebViewLoadStarted(long id, String url);
//     public static native void onWebViewLoadFinished(long id, String url);
//     public static native void onWebViewLoadFailed(long id, String url, String error);
