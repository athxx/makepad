//! Windows backend: Microsoft Edge **WebView2** overlay (`ICoreWebView2`),
//! created as a child of the Makepad window's `HWND`.
//!
//! # Why this file is hand-rolled FFI
//!
//! The rest of the Makepad Windows platform uses the vendored `windows-rs`
//! bindings (re-exported as `makepad_platform::…::crate::windows`). Those
//! pre-generated bindings **do not include the WebView2 surface**
//! (`ICoreWebView2*`), and this crate (`makepad-webview`) does not depend on the
//! `windows` / `windows-core` crates at all (see `Cargo.toml`). So — exactly
//! like `platform/src/os/windows/dwrite_sys.rs` does for DirectWrite — we
//! hand-declare the *minimal* COM surface we need with raw `#[repr(C)]` vtables
//! and `extern "system"` fn pointers, and load the loader DLL dynamically. This
//! keeps `makepad-webview` free of any new Windows crate dependency.
//!
//! If a future refactor makes the `windows` crate a dependency of this crate,
//! this file should be ported to the `implement_com!` macro / `windows::core`
//! machinery for the callback objects (much less unsafe boilerplate). See the
//! report notes for the exact deps that would enable that.
//!
//! # Threading / event model
//!
//! WebView2 creation is asynchronous: `CreateCoreWebView2EnvironmentWithOptions`
//! calls back an `ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler`,
//! which in turn calls `CreateCoreWebView2Controller`, whose completion handler
//! finally hands us the `ICoreWebView2Controller` + `ICoreWebView2`. All of
//! these callbacks fire **on the UI thread** (the same thread that pumps the
//! Win32 message loop, i.e. the Makepad main thread), because WebView2 posts its
//! completions through the calling thread's message queue.
//!
//! Because the callbacks are UI-thread and so is `drain_events`, we could in
//! principle keep the queue non-`Sync`. But to mirror the platform's
//! backchannel patterns (e.g. `windows_media_engine_notify::MediaEngineNotifyState`
//! uses a `Mutex<Vec<..>>`) and to be robust if WebView2 ever marshals a
//! callback, we key a process-global `Mutex`-guarded registry by the webview's
//! `u64` id. The COM callback objects only carry that `u64`, never a borrowed
//! pointer back into `WindowsBackend`, which keeps their lifetime independent of
//! the Rust handle.

#![allow(non_snake_case)]
#![allow(non_camel_case_types)]
#![allow(dead_code)]

use crate::backend::WebViewBackend;
use crate::bridge::{self, BridgeMessage};
use crate::{Result, WebViewError, WebViewEvent, WebViewOptions};
use makepad_micro_serde::DeJson;
use makepad_platform::{Area, Cx};

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

// =============================================================================
// Minimal Win32 / COM ABI types (no `windows` crate dependency)
// =============================================================================

#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct HRESULT(i32);
impl HRESULT {
    #[inline]
    fn is_ok(self) -> bool {
        self.0 >= 0
    }
    #[inline]
    fn is_err(self) -> bool {
        self.0 < 0
    }
}

const S_OK: HRESULT = HRESULT(0);
const E_POINTER: HRESULT = HRESULT(0x8000_4003u32 as i32);
const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002u32 as i32);

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct GUID {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}
impl GUID {
    const fn from_u128(uuid: u128) -> Self {
        Self {
            data1: (uuid >> 96) as u32,
            data2: (uuid >> 80) as u16,
            data3: (uuid >> 64) as u16,
            data4: (uuid as u64).to_be_bytes(),
        }
    }
}

/// Win32 `RECT` (device pixels, relative to the parent client area).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RECT {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

type BOOL = i32;

/// A `*mut c_void` we treat as an opaque native handle (HWND / HMODULE / FARPROC).
type RawPtr = *mut c_void;

// --- Dynamic library loading + COM heap free (import libs always linked on Windows)
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> RawPtr;
    fn FreeLibrary(module: RawPtr) -> BOOL;
    fn GetProcAddress(module: RawPtr, name: *const u8) -> RawPtr;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoTaskMemFree(ptr: RawPtr);
}

/// UTF-16, NUL-terminated. Used for DLL names, URLs, scripts, options paths.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copy a `PWSTR`/`LPCWSTR` (`*const u16`, NUL-terminated) into a Rust `String`.
/// Returns `None` on a null pointer.
unsafe fn from_wide(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    Some(String::from_utf16_lossy(slice))
}

/// Copy a WebView2-allocated `PWSTR` into a `String`, then `CoTaskMemFree` it.
/// WebView2 out-string params (e.g. `get_Source`, `TryGetWebMessageAsString`)
/// are CoTaskMem-allocated and owned by the caller.
unsafe fn take_cotaskmem_wide(ptr: *mut u16) -> Option<String> {
    let s = from_wide(ptr);
    if !ptr.is_null() {
        CoTaskMemFree(ptr as RawPtr);
    }
    s
}

// =============================================================================
// IUnknown vtable prefix (shared by every COM interface / callback below)
// =============================================================================

#[repr(C)]
struct IUnknown_Vtbl {
    QueryInterface: unsafe extern "system" fn(
        this: RawPtr,
        iid: *const GUID,
        out: *mut RawPtr,
    ) -> HRESULT,
    AddRef: unsafe extern "system" fn(this: RawPtr) -> u32,
    Release: unsafe extern "system" fn(this: RawPtr) -> u32,
}

/// A thin owned COM interface pointer: a `*mut` whose first field is a `*const Vtbl`.
/// `Drop` calls `Release`. `Clone` calls `AddRef`. Modeled on `windows`'
/// refcounted interface wrappers but bespoke so we need no external crate.
struct ComPtr<V: 'static> {
    raw: *mut ComObj<V>,
}
#[repr(C)]
struct ComObj<V: 'static> {
    vtbl: *const V,
}
impl<V: 'static> ComPtr<V> {
    /// Take ownership of a raw interface pointer (does not AddRef).
    unsafe fn from_raw(raw: RawPtr) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            Some(Self {
                raw: raw as *mut ComObj<V>,
            })
        }
    }
    #[inline]
    fn as_raw(&self) -> RawPtr {
        self.raw as RawPtr
    }
    #[inline]
    unsafe fn vtbl(&self) -> &V {
        &*(*self.raw).vtbl
    }
    /// The IUnknown vtable prefix, valid for any COM interface.
    #[inline]
    unsafe fn unknown(&self) -> &IUnknown_Vtbl {
        &*((*self.raw).vtbl as *const IUnknown_Vtbl)
    }
}
impl<V: 'static> Clone for ComPtr<V> {
    fn clone(&self) -> Self {
        unsafe {
            (self.unknown().AddRef)(self.as_raw());
        }
        Self { raw: self.raw }
    }
}
impl<V: 'static> Drop for ComPtr<V> {
    fn drop(&mut self) {
        unsafe {
            (self.unknown().Release)(self.as_raw());
        }
    }
}

// =============================================================================
// WebView2 interface vtables (consumer side — only the slots we call).
//
// IIDs and slot order are from the WebView2 SDK headers (WebView2.h). The vtable
// layout must be complete up to the last slot we invoke; trailing slots may be
// omitted. IIDs below correspond to the stable 1.0.* interface versions.
// =============================================================================

// ICoreWebView2Environment — {b96d755e-0319-4e92-a296-23436f46a1fc}
const IID_ICoreWebView2Environment: GUID =
    GUID::from_u128(0xb96d755e_0319_4e92_a296_23436f46a1fc);
#[repr(C)]
struct ICoreWebView2Environment_Vtbl {
    base: IUnknown_Vtbl,
    CreateCoreWebView2Controller: unsafe extern "system" fn(
        this: RawPtr,
        parent_window: RawPtr, // HWND
        handler: RawPtr,       // ICoreWebView2CreateCoreWebView2ControllerCompletedHandler*
    ) -> HRESULT,
    // (CreateWebResourceResponse, get_BrowserVersionString, ...) not used.
}

// ICoreWebView2Controller — {4d00c0d1-9434-4eb6-8078-8697a560334f}
const IID_ICoreWebView2Controller: GUID =
    GUID::from_u128(0x4d00c0d1_9434_4eb6_8078_8697a560334f);
#[repr(C)]
struct ICoreWebView2Controller_Vtbl {
    base: IUnknown_Vtbl,
    get_IsVisible: unsafe extern "system" fn(this: RawPtr, value: *mut BOOL) -> HRESULT,
    put_IsVisible: unsafe extern "system" fn(this: RawPtr, value: BOOL) -> HRESULT,
    get_Bounds: unsafe extern "system" fn(this: RawPtr, bounds: *mut RECT) -> HRESULT,
    put_Bounds: unsafe extern "system" fn(this: RawPtr, bounds: RECT) -> HRESULT,
    get_ZoomFactor: unsafe extern "system" fn(this: RawPtr, value: *mut f64) -> HRESULT,
    put_ZoomFactor: unsafe extern "system" fn(this: RawPtr, value: f64) -> HRESULT,
    add_ZoomFactorChanged:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_ZoomFactorChanged: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    SetBoundsAndZoomFactor:
        unsafe extern "system" fn(this: RawPtr, bounds: RECT, zoom: f64) -> HRESULT,
    MoveFocus: unsafe extern "system" fn(this: RawPtr, reason: i32) -> HRESULT,
    add_MoveFocusRequested:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_MoveFocusRequested: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    add_GotFocus:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_GotFocus: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    add_LostFocus:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_LostFocus: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    add_AcceleratorKeyPressed:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_AcceleratorKeyPressed: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    get_ParentWindow: unsafe extern "system" fn(this: RawPtr, value: *mut RawPtr) -> HRESULT,
    put_ParentWindow: unsafe extern "system" fn(this: RawPtr, value: RawPtr) -> HRESULT,
    NotifyParentWindowPositionChanged: unsafe extern "system" fn(this: RawPtr) -> HRESULT,
    Close: unsafe extern "system" fn(this: RawPtr) -> HRESULT,
    get_CoreWebView2: unsafe extern "system" fn(this: RawPtr, value: *mut RawPtr) -> HRESULT,
}

// ICoreWebView2 — {76eceacb-0462-4d94-ac83-423a6793775e}
const IID_ICoreWebView2: GUID = GUID::from_u128(0x76eceacb_0462_4d94_ac83_423a6793775e);
#[repr(C)]
struct ICoreWebView2_Vtbl {
    base: IUnknown_Vtbl,
    get_Settings: unsafe extern "system" fn(this: RawPtr, value: *mut RawPtr) -> HRESULT,
    get_Source: unsafe extern "system" fn(this: RawPtr, uri: *mut *mut u16) -> HRESULT,
    Navigate: unsafe extern "system" fn(this: RawPtr, uri: *const u16) -> HRESULT,
    NavigateToString: unsafe extern "system" fn(this: RawPtr, html: *const u16) -> HRESULT,
    add_NavigationStarting:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_NavigationStarting: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    add_ContentLoading:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_ContentLoading: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    add_SourceChanged:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_SourceChanged: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    add_HistoryChanged:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_HistoryChanged: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    add_NavigationCompleted:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_NavigationCompleted: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    add_FrameNavigationStarting:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_FrameNavigationStarting: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    add_FrameNavigationCompleted:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_FrameNavigationCompleted: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    add_ScriptDialogOpening:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_ScriptDialogOpening: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    add_PermissionRequested:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_PermissionRequested: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    add_ProcessFailed:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_ProcessFailed: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    AddScriptToExecuteOnDocumentCreated: unsafe extern "system" fn(
        this: RawPtr,
        java_script: *const u16,
        handler: RawPtr, // ICoreWebView2AddScriptToExecuteOnDocumentCreatedCompletedHandler* (may be null)
    ) -> HRESULT,
    RemoveScriptToExecuteOnDocumentCreated:
        unsafe extern "system" fn(this: RawPtr, id: *const u16) -> HRESULT,
    ExecuteScript: unsafe extern "system" fn(
        this: RawPtr,
        java_script: *const u16,
        handler: RawPtr, // ICoreWebView2ExecuteScriptCompletedHandler* (may be null)
    ) -> HRESULT,
    CapturePreview:
        unsafe extern "system" fn(this: RawPtr, format: i32, stream: RawPtr, handler: RawPtr) -> HRESULT,
    Reload: unsafe extern "system" fn(this: RawPtr) -> HRESULT,
    PostWebMessageAsJson: unsafe extern "system" fn(this: RawPtr, message: *const u16) -> HRESULT,
    PostWebMessageAsString: unsafe extern "system" fn(this: RawPtr, message: *const u16) -> HRESULT,
    add_WebMessageReceived:
        unsafe extern "system" fn(this: RawPtr, handler: RawPtr, token: *mut i64) -> HRESULT,
    remove_WebMessageReceived: unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    add_DevToolsProtocolEventReceived: unsafe extern "system" fn(
        this: RawPtr,
        event_name: *const u16,
        handler: RawPtr,
        token: *mut i64,
    ) -> HRESULT,
    remove_DevToolsProtocolEventReceived:
        unsafe extern "system" fn(this: RawPtr, token: i64) -> HRESULT,
    GetDevToolsProtocolEventReceiver:
        unsafe extern "system" fn(this: RawPtr, event_name: *const u16, receiver: *mut RawPtr) -> HRESULT,
    get_BrowserProcessId: unsafe extern "system" fn(this: RawPtr, value: *mut u32) -> HRESULT,
    get_CanGoBack: unsafe extern "system" fn(this: RawPtr, value: *mut BOOL) -> HRESULT,
    get_CanGoForward: unsafe extern "system" fn(this: RawPtr, value: *mut BOOL) -> HRESULT,
    GoBack: unsafe extern "system" fn(this: RawPtr) -> HRESULT,
    GoForward: unsafe extern "system" fn(this: RawPtr) -> HRESULT,
    // (Stop, add_DocumentTitleChanged, ...) not used.
}

// ICoreWebView2WebMessageReceivedEventArgs — {0f99a40c-e962-4207-9e92-e3d542eff849}
#[repr(C)]
struct ICoreWebView2WebMessageReceivedEventArgs_Vtbl {
    base: IUnknown_Vtbl,
    get_Source: unsafe extern "system" fn(this: RawPtr, value: *mut *mut u16) -> HRESULT,
    get_WebMessageAsJson: unsafe extern "system" fn(this: RawPtr, value: *mut *mut u16) -> HRESULT,
    TryGetWebMessageAsString:
        unsafe extern "system" fn(this: RawPtr, value: *mut *mut u16) -> HRESULT,
}

// ICoreWebView2NavigationStartingEventArgs — {5b495469-e119-438a-9b18-7604f25f2e49}
#[repr(C)]
struct ICoreWebView2NavigationStartingEventArgs_Vtbl {
    base: IUnknown_Vtbl,
    get_Uri: unsafe extern "system" fn(this: RawPtr, value: *mut *mut u16) -> HRESULT,
    get_IsUserInitiated: unsafe extern "system" fn(this: RawPtr, value: *mut BOOL) -> HRESULT,
    get_IsRedirected: unsafe extern "system" fn(this: RawPtr, value: *mut BOOL) -> HRESULT,
    get_RequestHeaders: unsafe extern "system" fn(this: RawPtr, value: *mut RawPtr) -> HRESULT,
    get_Cancel: unsafe extern "system" fn(this: RawPtr, value: *mut BOOL) -> HRESULT,
    put_Cancel: unsafe extern "system" fn(this: RawPtr, value: BOOL) -> HRESULT,
    get_NavigationId: unsafe extern "system" fn(this: RawPtr, value: *mut u64) -> HRESULT,
}

// ICoreWebView2NavigationCompletedEventArgs — {30d68b7d-20d9-4752-a9ca-ec8448fbb5c1}
#[repr(C)]
struct ICoreWebView2NavigationCompletedEventArgs_Vtbl {
    base: IUnknown_Vtbl,
    get_IsSuccess: unsafe extern "system" fn(this: RawPtr, value: *mut BOOL) -> HRESULT,
    get_WebErrorStatus: unsafe extern "system" fn(this: RawPtr, value: *mut i32) -> HRESULT,
    get_NavigationId: unsafe extern "system" fn(this: RawPtr, value: *mut u64) -> HRESULT,
}

type EnvPtr = ComPtr<ICoreWebView2Environment_Vtbl>;
type ControllerPtr = ComPtr<ICoreWebView2Controller_Vtbl>;
type WebViewPtr = ComPtr<ICoreWebView2_Vtbl>;

// =============================================================================
// The WebView2Loader entry point, resolved dynamically.
// =============================================================================

/// `CreateCoreWebView2EnvironmentWithOptions(browserExecutableFolder,
///  userDataFolder, environmentOptions, environmentCreatedHandler)`.
type CreateEnvFn = unsafe extern "system" fn(
    browser_executable_folder: *const u16,
    user_data_folder: *const u16,
    environment_options: RawPtr,
    environment_created_handler: RawPtr,
) -> HRESULT;

/// Cache the resolved loader so we only `LoadLibrary` once per process.
struct Loader {
    create_env: CreateEnvFn,
}
// SAFETY: the fn pointer is a plain code address, valid process-wide.
unsafe impl Send for Loader {}
unsafe impl Sync for Loader {}

static LOADER: OnceLock<Option<Loader>> = OnceLock::new();

fn loader() -> Option<&'static Loader> {
    LOADER
        .get_or_init(|| unsafe {
            // Prefer the app-local `WebView2Loader.dll`; the OS loader also
            // resolves the machine-wide Evergreen runtime copy if it's on PATH.
            let name = to_wide("WebView2Loader.dll");
            let module = LoadLibraryW(name.as_ptr());
            if module.is_null() {
                return None;
            }
            let proc = GetProcAddress(
                module,
                b"CreateCoreWebView2EnvironmentWithOptions\0".as_ptr(),
            );
            if proc.is_null() {
                FreeLibrary(module);
                return None;
            }
            // Intentionally leak `module` for the process lifetime (the fn ptr
            // must stay valid); matches how DLLs are treated elsewhere.
            let create_env: CreateEnvFn = std::mem::transmute(proc);
            Some(Loader { create_env })
        })
        .as_ref()
}

// =============================================================================
// Process-global event queue keyed by webview id.
//
// COM callbacks fire on the UI thread and only know their `u64` id; they push
// events here. `drain_events` (also UI thread) collects them. Also stashes the
// controller/webview pointers produced by the async creation chain, since those
// arrive in a callback rather than a return value.
// =============================================================================

#[derive(Default)]
struct Slot {
    events: Vec<WebViewEvent>,
    /// Filled by the controller-completed callback once creation finishes.
    controller: Option<ControllerPtr>,
    webview: Option<WebViewPtr>,
    /// URL requested before the controller existed; navigated once ready.
    pending_url: Option<String>,
    /// Last-known desired bounds/visibility; applied once the controller exists.
    pending_bounds: Option<(RECT, bool)>,
    ready: bool,
}

// SAFETY: the whole registry is only ever touched on the UI thread in practice
// (WebView2 callbacks + the makepad main thread), and is additionally guarded by
// a Mutex. The ComPtrs are single-apartment but we never hand them across
// threads — they live and die on the UI thread. The Send bound the Mutex/HashMap
// require is satisfied by this wrapper which asserts that invariant.
struct SlotCell(Slot);
unsafe impl Send for SlotCell {}

static REGISTRY: OnceLock<Mutex<HashMap<u64, SlotCell>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<u64, SlotCell>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_slot<R>(id: u64, f: impl FnOnce(&mut Slot) -> R) -> R {
    let mut map = registry().lock().unwrap();
    let cell = map.entry(id).or_insert_with(|| SlotCell(Slot::default()));
    f(&mut cell.0)
}

fn push_event(id: u64, ev: WebViewEvent) {
    with_slot(id, |s| s.events.push(ev));
}

// =============================================================================
// Hand-rolled COM callback objects.
//
// Each is a heap `Box<Callback>` whose first field is a `*const Vtbl`, followed
// by a refcount and the `u64` id. `QueryInterface`/`AddRef`/`Release` are shared
// generic thunks; the one meaningful method (`Invoke`) is per-callback. We hand
// WebView2 a raw pointer to the boxed object; when its refcount hits zero we
// `Box::from_raw` and drop.
//
// TODO(windows-device): the `matches!(iid)` in QueryInterface only answers
// IUnknown + the callback's own IID. WebView2 in practice only ever queries
// those two, but this must be validated against a real WebView2 runtime; if a
// runtime QIs for IAgileObject (to marshal the callback) we'd need to add it or
// keep everything strictly single-threaded (which we do).
// =============================================================================

/// The invoke signature differs per callback, so each callback embeds its own
/// vtable type. They all share this header layout.
#[repr(C)]
struct CallbackHeader {
    vtbl: *const c_void,
    refcount: std::sync::atomic::AtomicU32,
    id: u64,
}

// --- generic IUnknown thunks -------------------------------------------------

unsafe extern "system" fn cb_query_interface(
    this: RawPtr,
    iid: *const GUID,
    out: *mut RawPtr,
    own_iid: &GUID,
) -> HRESULT {
    if iid.is_null() || out.is_null() {
        return E_POINTER;
    }
    let iid = &*iid;
    // IUnknown or the specific handler IID → hand back the same pointer.
    if *iid == IID_IUNKNOWN || iid == own_iid {
        cb_add_ref(this);
        *out = this;
        return S_OK;
    }
    *out = std::ptr::null_mut();
    E_NOINTERFACE
}

unsafe extern "system" fn cb_add_ref(this: RawPtr) -> u32 {
    let header = &*(this as *const CallbackHeader);
    header
        .refcount
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
        + 1
}

unsafe extern "system" fn cb_release_generic(this: RawPtr) -> u32 {
    let header = &*(this as *const CallbackHeader);
    let prev = header
        .refcount
        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    if prev == 1 {
        // Reconstruct the correct boxed type via a per-callback drop thunk stored
        // right after the header would be error-prone; instead each callback
        // constructor installs a concrete Release that boxes the right type. This
        // generic path is only used when the concrete type is statically known at
        // the call site (see macro below). Never reached directly.
        0
    } else {
        prev - 1
    }
}

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);

/// Build a concrete callback type with its own vtable and `Invoke`. The macro
/// generates: the `#[repr(C)]` vtable struct, the boxed object, the three
/// IUnknown thunks (with a correctly-typed `Release`), and a constructor that
/// returns a raw `*mut c_void` to pass to WebView2.
macro_rules! define_callback {
    (
        name: $name:ident,
        vtbl: $vtbl:ident,
        iid: $iid:expr,
        // Invoke signature after `this`: (arg_name: ArgTy, ...)
        invoke_args: ($($arg:ident : $arg_ty:ty),* $(,)?),
        // Body receives `id: u64` and the invoke args by value.
        invoke: |$id_bind:ident, $($body_arg:ident),*| $body:block
    ) => {
        #[repr(C)]
        struct $vtbl {
            base: IUnknown_Vtbl,
            Invoke: unsafe extern "system" fn(this: RawPtr, $($arg: $arg_ty),*) -> HRESULT,
        }

        #[repr(C)]
        struct $name {
            header: CallbackHeader,
        }

        impl $name {
            const IID: GUID = $iid;
            const VTBL: $vtbl = $vtbl {
                base: IUnknown_Vtbl {
                    QueryInterface: Self::query_interface,
                    AddRef: cb_add_ref,
                    Release: Self::release,
                },
                Invoke: Self::invoke,
            };

            /// Allocate a new callback, refcount 1, returning a raw pointer to
            /// pass to WebView2 (ownership transfers to the caller/WebView2).
            fn new(id: u64) -> RawPtr {
                let boxed = Box::new($name {
                    header: CallbackHeader {
                        vtbl: &Self::VTBL as *const $vtbl as *const c_void,
                        refcount: std::sync::atomic::AtomicU32::new(1),
                        id,
                    },
                });
                Box::into_raw(boxed) as RawPtr
            }

            unsafe extern "system" fn query_interface(
                this: RawPtr,
                iid: *const GUID,
                out: *mut RawPtr,
            ) -> HRESULT {
                cb_query_interface(this, iid, out, &Self::IID)
            }

            unsafe extern "system" fn release(this: RawPtr) -> u32 {
                let header = &*(this as *const CallbackHeader);
                let prev = header
                    .refcount
                    .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                if prev == 1 {
                    drop(Box::from_raw(this as *mut $name));
                    0
                } else {
                    prev - 1
                }
            }

            unsafe extern "system" fn invoke(this: RawPtr, $($arg: $arg_ty),*) -> HRESULT {
                let header = &*(this as *const CallbackHeader);
                let $id_bind = header.id;
                // The invoke args ($arg) are already in scope by name; the
                // `$body_arg` list just documents which ones the body consumes.
                $(let _ = &$body_arg;)*
                $body
                S_OK
            }
        }
    };
}

// --- environment-created: kicks off controller creation ---------------------
define_callback! {
    name: EnvironmentCompletedHandler,
    vtbl: EnvironmentCompletedHandler_Vtbl,
    // ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler
    iid: GUID::from_u128(0x4e8a3389_c9d8_4bd2_b6b5_124fee6cc14d),
    invoke_args: (result: HRESULT, environment: RawPtr),
    invoke: |id, result, environment| {
        if result.is_err() || environment.is_null() {
            push_event(id, WebViewEvent::LoadFailed {
                url: String::new(),
                error: format!("WebView2 environment creation failed (hr=0x{:08x})", result.0),
            });
            return S_OK;
        }
        // Borrow (don't take) the environment: WebView2 owns this reference.
        let env = &*(environment as *const ComObj<ICoreWebView2Environment_Vtbl>);
        let vtbl = &*env.vtbl;
        // Parent HWND recorded on the UI thread at `new()` time.
        let parent_hwnd = PARENT_HWND.with(|p| p.get());
        let controller_cb = ControllerCompletedHandler::new(id);
        let hr = (vtbl.CreateCoreWebView2Controller)(environment, parent_hwnd, controller_cb);
        if hr.is_err() {
            // WebView2 didn't take our callback; release it ourselves.
            let unk = &*(controller_cb as *const ComObj<IUnknown_Vtbl>);
            ((*unk.vtbl).Release)(controller_cb);
            push_event(id, WebViewEvent::LoadFailed {
                url: String::new(),
                error: format!("CreateCoreWebView2Controller failed (hr=0x{:08x})", hr.0),
            });
        }
    }
}

// --- controller-created: stash controller + webview, wire events ------------
define_callback! {
    name: ControllerCompletedHandler,
    vtbl: ControllerCompletedHandler_Vtbl,
    // ICoreWebView2CreateCoreWebView2ControllerCompletedHandler
    iid: GUID::from_u128(0x6c4819f3_c9b7_4260_8127_c9f5bde7f68c),
    invoke_args: (result: HRESULT, controller: RawPtr),
    invoke: |id, result, controller| {
        if result.is_err() || controller.is_null() {
            push_event(id, WebViewEvent::LoadFailed {
                url: String::new(),
                error: format!("WebView2 controller creation failed (hr=0x{:08x})", result.0),
            });
            return S_OK;
        }
        // Take ownership of the controller (creation handed us a +1 ref).
        cb_add_ref(controller); // one for us; WebView2 also holds one it owns.
        let controller_ptr = ControllerPtr::from_raw(controller).unwrap();

        // Fetch the ICoreWebView2 off the controller.
        let mut webview_raw: RawPtr = std::ptr::null_mut();
        let hr = (controller_ptr.vtbl().get_CoreWebView2)(controller_ptr.as_raw(), &mut webview_raw);
        let webview_ptr = if hr.is_ok() {
            WebViewPtr::from_raw(webview_raw)
        } else {
            None
        };

        if let Some(ref wv) = webview_ptr {
            wire_webview(id, wv);
        }

        // Publish, then apply any deferred url/bounds.
        let (pending_url, pending_bounds) = with_slot(id, |s| {
            s.controller = Some(controller_ptr.clone());
            s.webview = webview_ptr.as_ref().map(|p| p.clone());
            s.ready = true;
            (s.pending_url.take(), s.pending_bounds.take())
        });

        if let Some(ref wv) = webview_ptr {
            if let Some(url) = pending_url {
                let w = to_wide(&url);
                (wv.vtbl().Navigate)(wv.as_raw(), w.as_ptr());
            }
        }
        if let Some((bounds, visible)) = pending_bounds {
            (controller_ptr.vtbl().put_Bounds)(controller_ptr.as_raw(), bounds);
            (controller_ptr.vtbl().put_IsVisible)(controller_ptr.as_raw(), visible as BOOL);
        }
        // Keep our refs alive in the slot; drop the local (slot cloned them).
        drop(controller_ptr);
        drop(webview_ptr);
    }
}

// --- web message received: JS → Rust ----------------------------------------
define_callback! {
    name: WebMessageReceivedHandler,
    vtbl: WebMessageReceivedHandler_Vtbl,
    // ICoreWebView2WebMessageReceivedEventHandler
    iid: GUID::from_u128(0x57213f19_00e6_49fa_8e07_898ea01ecbd2),
    invoke_args: (_sender: RawPtr, args: RawPtr),
    invoke: |id, _sender, args| {
        if args.is_null() {
            return S_OK;
        }
        let a = &*(args as *const ComObj<ICoreWebView2WebMessageReceivedEventArgs_Vtbl>);
        let vtbl = &*a.vtbl;
        // Page JS calls `window.chrome.webview.postMessage(json)` where `json` is
        // the stringified BridgeMessage. `TryGetWebMessageAsString` yields that
        // string when postMessage was given a string; `get_WebMessageAsJson`
        // yields the JSON encoding for object payloads. The bridge stringifies to
        // a JSON *string*, so it arrives via TryGetWebMessageAsString as the raw
        // JSON text of the BridgeMessage.
        let mut out: *mut u16 = std::ptr::null_mut();
        let hr = (vtbl.TryGetWebMessageAsString)(args, &mut out);
        let json = if hr.is_ok() {
            take_cotaskmem_wide(out)
        } else {
            None
        };
        let json = match json {
            Some(j) if !j.is_empty() => j,
            _ => {
                // Fallback: object payload → JSON. `get_WebMessageAsJson` returns
                // the JSON-encoded value; for a JSON *string* that's the string
                // wrapped in quotes, so unwrap one level.
                let mut out2: *mut u16 = std::ptr::null_mut();
                let hr2 = (vtbl.get_WebMessageAsJson)(args, &mut out2);
                if hr2.is_err() { return S_OK; }
                match take_cotaskmem_wide(out2) {
                    Some(j) => j,
                    None => return S_OK,
                }
            }
        };
        if let Ok(msg) = BridgeMessage::deserialize_json(&json) {
            push_event(id, WebViewEvent::Message(msg));
        }
    }
}

// --- navigation starting: LoadStarted ---------------------------------------
define_callback! {
    name: NavigationStartingHandler,
    vtbl: NavigationStartingHandler_Vtbl,
    // ICoreWebView2NavigationStartingEventHandler
    iid: GUID::from_u128(0x9adbe429_f36d_432b_9ddc_f8881fbd76e3),
    invoke_args: (sender: RawPtr, args: RawPtr),
    invoke: |id, sender, args| {
        let url = navigation_start_url(sender, args);
        push_event(id, WebViewEvent::LoadStarted { url });
    }
}

// --- navigation completed: LoadFinished / LoadFailed ------------------------
define_callback! {
    name: NavigationCompletedHandler,
    vtbl: NavigationCompletedHandler_Vtbl,
    // ICoreWebView2NavigationCompletedEventHandler
    iid: GUID::from_u128(0xd33a35bf_1c49_4f98_93ab_006e0533fe1c),
    invoke_args: (sender: RawPtr, args: RawPtr),
    invoke: |id, sender, args| {
        // Current URL comes from the sender ICoreWebView2::get_Source.
        let url = current_source(sender).unwrap_or_default();
        let mut success: BOOL = 0;
        if !args.is_null() {
            let a = &*(args as *const ComObj<ICoreWebView2NavigationCompletedEventArgs_Vtbl>);
            let _ = ((*a.vtbl).get_IsSuccess)(args, &mut success);
        }
        if success != 0 {
            push_event(id, WebViewEvent::LoadFinished { url });
        } else {
            let mut status: i32 = 0;
            if !args.is_null() {
                let a = &*(args as *const ComObj<ICoreWebView2NavigationCompletedEventArgs_Vtbl>);
                let _ = ((*a.vtbl).get_WebErrorStatus)(args, &mut status);
            }
            push_event(id, WebViewEvent::LoadFailed {
                url,
                error: format!("navigation failed (WebErrorStatus={status})"),
            });
        }
    }
}

/// Read the target URL from a NavigationStarting args object.
unsafe fn navigation_start_url(_sender: RawPtr, args: RawPtr) -> String {
    if args.is_null() {
        return String::new();
    }
    let a = &*(args as *const ComObj<ICoreWebView2NavigationStartingEventArgs_Vtbl>);
    let mut out: *mut u16 = std::ptr::null_mut();
    let hr = ((*a.vtbl).get_Uri)(args, &mut out);
    if hr.is_err() {
        return String::new();
    }
    take_cotaskmem_wide(out).unwrap_or_default()
}

/// Read `ICoreWebView2::get_Source` off the event sender (the webview itself).
unsafe fn current_source(sender: RawPtr) -> Option<String> {
    if sender.is_null() {
        return None;
    }
    let wv = &*(sender as *const ComObj<ICoreWebView2_Vtbl>);
    let mut out: *mut u16 = std::ptr::null_mut();
    let hr = ((*wv.vtbl).get_Source)(sender, &mut out);
    if hr.is_err() {
        return None;
    }
    take_cotaskmem_wide(out)
}

/// Install the document-start bridge script + subscribe to events on a freshly
/// created ICoreWebView2. Runs on the UI thread inside the controller callback.
unsafe fn wire_webview(id: u64, wv: &WebViewPtr) {
    // 1) document-start bridge injection.
    let script = bridge::injection_script("window.chrome.webview.postMessage(%MSG%)");
    let wide = to_wide(&script);
    // Pass a null completion handler; we don't need the assigned script id.
    (wv.vtbl().AddScriptToExecuteOnDocumentCreated)(
        wv.as_raw(),
        wide.as_ptr(),
        std::ptr::null_mut(),
    );

    // 2) JS → Rust channel.
    let mut token: i64 = 0;
    let msg_cb = WebMessageReceivedHandler::new(id);
    (wv.vtbl().add_WebMessageReceived)(wv.as_raw(), msg_cb, &mut token);
    // add_* takes its own ref on the handler; drop our creation ref.
    release_raw(msg_cb);

    // 3) navigation started / completed → load events.
    let mut token2: i64 = 0;
    let nav_start_cb = NavigationStartingHandler::new(id);
    (wv.vtbl().add_NavigationStarting)(wv.as_raw(), nav_start_cb, &mut token2);
    release_raw(nav_start_cb);

    let mut token3: i64 = 0;
    let nav_done_cb = NavigationCompletedHandler::new(id);
    (wv.vtbl().add_NavigationCompleted)(wv.as_raw(), nav_done_cb, &mut token3);
    release_raw(nav_done_cb);

    // TODO(windows-device): store `token`/`token2`/`token3` and call the paired
    // `remove_*` in `detach()` for tidy teardown. Controller `Close()` tears the
    // whole thing down anyway, so leaking the tokens is safe but not pristine.
}

/// Release a raw COM/callback pointer via its IUnknown vtable prefix.
unsafe fn release_raw(ptr: RawPtr) {
    if ptr.is_null() {
        return;
    }
    let unk = &*(ptr as *const ComObj<IUnknown_Vtbl>);
    ((*unk.vtbl).Release)(ptr);
}

// The parent HWND is needed inside the environment-completed callback, which
// runs after `new()` returns. We stash it in a thread-local (callbacks run on
// the same UI thread that created the backend). Keyed per-thread is sufficient
// because creation is synchronous-to-first-message-pump on one thread.
//
// TODO(windows-device): if two WebViews are created before the first one's
// environment callback fires, this single-slot thread-local races. Promote to a
// per-id map in REGISTRY (store the parent HWND in `Slot`) once verified against
// a real runtime; left as a single cell to keep the common (one-at-a-time)
// creation path simple and observable.
thread_local! {
    static PARENT_HWND: std::cell::Cell<RawPtr> = std::cell::Cell::new(std::ptr::null_mut());
}

// =============================================================================
// Parent HWND resolution
// =============================================================================

/// Best-effort resolution of the parent Win32 `HWND` for `area`.
///
/// The correct path is `area → WindowId → HWND`, but the platform crate does
/// **not** expose an `HWND` for a `WindowId` today (see report): `CxWindow`
/// stores no handle, and the `Vec<D3d11Window>` that owns `win32_window.hwnd`
/// lives only in the event-loop closure. The one reachable list is
/// `Win32App.all_windows: Vec<HWND>` (via the pub `with_win32_app`), but it is
/// unkeyed by `WindowId`.
///
/// So we take the app's single/last top-level window as the parent, which is
/// correct for the overwhelmingly common single-window case. Multi-window needs
/// a platform-crate change (see report item 2).
fn resolve_parent_hwnd(_cx: &Cx, _area: Area) -> Option<RawPtr> {
    use makepad_platform::os::windows::win32_app::with_win32_app;
    // `HWND` here is `windows::…::HWND(pub *mut c_void)`; we never name the type,
    // only read its public `.0` raw pointer, so no `windows` dep is needed.
    with_win32_app(|app| app.all_windows.last().map(|hwnd| hwnd.0))
    // TODO(windows-device): resolve the *specific* window for `area` via
    //   area.draw_list_id() -> cx.draw_lists[id].draw_pass_id
    //   -> cx.get_pass_window_id(pass_id) -> WindowId
    // then map WindowId -> HWND. That final map requires the platform crate to
    // expose it (see report): e.g. store the HWND on `CxWindow` / add
    // `Cx::hwnd_from_window_id`, or key `Win32App.all_windows` by `WindowId`.
}

/// Compute device-pixel `RECT` bounds (relative to the parent client area) for
/// `area`. Makepad's `Area::rect`/`clipped_rect` are in the window's logical
/// coordinate space with the origin at the client top-left, which is what
/// WebView2 `put_Bounds` expects (device px).
///
/// TODO(windows-device): confirm DPI handling — WebView2 `Bounds` are in the
/// parent window's client coordinates in **physical** pixels. Makepad rects are
/// in its own DPI-scaled space; multiply by the window's dpi factor if a real
/// device shows the overlay offset/mis-sized. `Cx` exposes the dpi via the
/// window geom; wire it here once verified on hardware.
fn area_to_rect(cx: &Cx, area: Area) -> RECT {
    let r = area.clipped_rect(cx);
    RECT {
        left: r.pos.x as i32,
        top: r.pos.y as i32,
        right: (r.pos.x + r.size.x) as i32,
        bottom: (r.pos.y + r.size.y) as i32,
    }
}

// =============================================================================
// The backend
// =============================================================================

pub(crate) struct WindowsBackend {
    id: u64,
    current_url: String,
    /// True once creation was kicked off (regardless of async completion).
    spawned: bool,
}

impl WindowsBackend {
    /// Grab the controller pointer if creation has completed.
    fn controller(&self) -> Option<ControllerPtr> {
        with_slot(self.id, |s| s.controller.clone())
    }
    fn webview(&self) -> Option<WebViewPtr> {
        with_slot(self.id, |s| s.webview.clone())
    }
    fn is_ready(&self) -> bool {
        with_slot(self.id, |s| s.ready)
    }
}

impl WebViewBackend for WindowsBackend {
    fn new(cx: &mut Cx, opts: &WebViewOptions) -> Result<Self> {
        let loader = loader().ok_or_else(|| {
            WebViewError::Backend(
                "WebView2Loader.dll not found (install the Edge WebView2 Runtime)".to_string(),
            )
        })?;

        let parent = resolve_parent_hwnd(cx, Area::Empty).ok_or_else(|| {
            WebViewError::Backend("no parent HWND available for WebView2".to_string())
        })?;
        PARENT_HWND.with(|p| p.set(parent));

        // Initialize the slot and stash the initial url to navigate once ready.
        with_slot(opts.id, |s| {
            *s = Slot::default();
            if !opts.url.is_empty() {
                s.pending_url = Some(opts.url.clone());
            }
        });

        // Kick off async environment creation. Null browser folder → use the
        // installed Evergreen runtime; null user-data folder → default per-app
        // location. Null options → defaults.
        //
        // TODO(windows-device): a non-null user-data folder is recommended in
        // production (WebView2 otherwise picks a folder next to the exe, which
        // fails for read-only install dirs). Pass a per-app writable path here
        // once the app exposes one.
        let env_cb = EnvironmentCompletedHandler::new(opts.id);
        let hr = unsafe {
            (loader.create_env)(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                env_cb,
            )
        };
        if hr.is_err() {
            unsafe { release_raw(env_cb) };
            return Err(WebViewError::Backend(format!(
                "CreateCoreWebView2EnvironmentWithOptions failed (hr=0x{:08x})",
                hr.0
            )));
        }

        Ok(Self {
            id: opts.id,
            current_url: opts.url.clone(),
            spawned: true,
        })
    }

    fn set_url(&mut self, _cx: &mut Cx, url: &str) {
        if self.current_url == url && !url.is_empty() {
            return;
        }
        self.current_url = url.to_string();
        if let Some(wv) = self.webview() {
            let w = to_wide(url);
            unsafe { (wv.vtbl().Navigate)(wv.as_raw(), w.as_ptr()) };
        } else {
            // Not ready yet — navigate when the controller callback fires.
            with_slot(self.id, |s| s.pending_url = Some(url.to_string()));
        }
    }

    fn update_rect(&mut self, cx: &mut Cx, area: Area, visible: bool) {
        let show = visible && area.is_valid(cx);
        let rect = if show { area_to_rect(cx, area) } else { RECT::default() };
        if let Some(ctrl) = self.controller() {
            unsafe {
                if show {
                    (ctrl.vtbl().put_Bounds)(ctrl.as_raw(), rect);
                }
                (ctrl.vtbl().put_IsVisible)(ctrl.as_raw(), show as BOOL);
            }
        } else {
            // Defer until the controller exists.
            with_slot(self.id, |s| s.pending_bounds = Some((rect, show)));
        }
    }

    fn history_go(&mut self, _cx: &mut Cx, delta: i32) {
        let Some(wv) = self.webview() else { return };
        unsafe {
            // Match WKWebView/Apple semantics loosely: step one entry per unit.
            if delta < 0 {
                for _ in 0..(-delta) {
                    (wv.vtbl().GoBack)(wv.as_raw());
                }
            } else {
                for _ in 0..delta {
                    (wv.vtbl().GoForward)(wv.as_raw());
                }
            }
        }
    }

    fn eval_js(&mut self, _cx: &mut Cx, script: &str) {
        let Some(wv) = self.webview() else { return };
        let w = to_wide(script);
        unsafe {
            (wv.vtbl().ExecuteScript)(wv.as_raw(), w.as_ptr(), std::ptr::null_mut());
        }
    }

    fn drain_events(&mut self) -> Vec<WebViewEvent> {
        with_slot(self.id, |s| std::mem::take(&mut s.events))
    }

    fn detach(&mut self, _cx: &mut Cx) {
        // Close the controller (tears down the child window + browser) and drop
        // our owned refs so the COM objects release.
        if let Some(ctrl) = self.controller() {
            unsafe {
                let _ = (ctrl.vtbl().put_IsVisible)(ctrl.as_raw(), 0);
                let _ = (ctrl.vtbl().Close)(ctrl.as_raw());
            }
        }
        with_slot(self.id, |s| {
            s.controller = None;
            s.webview = None;
            s.ready = false;
            s.events.clear();
            s.pending_url = None;
            s.pending_bounds = None;
        });
        self.spawned = false;
    }
}

impl Drop for WindowsBackend {
    fn drop(&mut self) {
        if self.spawned {
            // Best-effort teardown without a Cx (Drop has none).
            if let Some(ctrl) = self.controller() {
                unsafe {
                    let _ = (ctrl.vtbl().Close)(ctrl.as_raw());
                }
            }
            registry().lock().unwrap().remove(&self.id);
        }
    }
}
