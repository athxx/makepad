//! Linux backend — WebKitGTK (libwebkit2gtk-4.1 / 4.0) driven over a
//! dynamically-loaded (`dlopen`) GTK / GDK / WebKit / GLib stack.
//!
//! # Windowing-compatibility finding (READ THIS FIRST)
//!
//! Makepad's Linux platform layer does **not** use GTK. It owns its native
//! surface directly as a raw **X11** window (`makepad_platform::os::linux::x11`,
//! via Xlib) or a raw **Wayland** surface
//! (`makepad_platform::os::linux::wayland`), selected at runtime by
//! `windowing_backend::detect_windowing_protocol()`. There is no `GtkWindow` or
//! `GtkWidget` to embed a `WebKitWebView` into.
//!
//! `WebKitWebView` is a `GtkWidget` and therefore *requires* a GTK container to
//! live in. The only way to composite it over a foreign, non-GTK native window
//! is to:
//!
//!   1. build our own top-level `GtkWindow` (GTK owns its own X11 window), put
//!      the `WebKitWebView` inside it, realize it, and
//!   2. **reparent** GTK's underlying X11 window to become a child of Makepad's
//!      X11 window (`XReparentWindow`), then position/size it with
//!      `XMoveResizeWindow` to track the widget `Area`.
//!
//! This XEmbed-style reparenting only works under **X11**. Under a Wayland
//! session there is no stable child-surface embedding primitive we can drive
//! from a foreign toolkit, so this backend degrades to "unsupported" there
//! (construction still succeeds and events still pump, but nothing is shown).
//! Running the app with `--linux-backend=x11` (or on an X11 session, or via
//! XWayland with a forced X11 backend) is required for a visible web view.
//!
//! We deliberately do **not** add a `gtk`/`webkit2gtk` crate dependency: the
//! platform crate loads every native lib through `dlopen`/`dlsym`
//! (`os::linux::module_loader::ModuleLoader`), and we match that convention with
//! a local loader here so the crate keeps building on machines without the
//! WebKitGTK dev headers. If WebKitGTK is not installed at runtime,
//! [`LinuxBackend::new`] still succeeds as an inert (no-op) backend rather than
//! failing the whole widget.
//!
//! All GTK/GLib calls MUST happen on the GTK main-loop thread. Makepad's X11
//! event loop is single-threaded and is the thread on which `new` /
//! `update_rect` / `eval_js` / `detach` are invoked (they take `&mut Cx`), so we
//! treat that thread as the GTK thread and pump the GTK main context from
//! `drain_events`. Script-message and load-changed signals fire on that same
//! thread, pushing onto a `Mutex`-guarded queue that `drain_events` empties.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]

use crate::backend::WebViewBackend;
use crate::bridge::{injection_script, BridgeMessage};
use crate::{Result, WebViewEvent, WebViewOptions};
use makepad_micro_serde::DeJson;
use makepad_platform::{Area, Cx};

use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int, c_uint, c_ulong};
use std::sync::{Arc, Mutex, OnceLock};

// The exact native call form page JS uses to reach Rust on WebKitGTK.
const NATIVE_POST_EXPR: &str = "window.webkit.messageHandlers.vsbridge.postMessage(%MSG%)";
// The script message handler name registered on the UserContentManager.
const BRIDGE_HANDLER_NAME: &str = "vsbridge";

// --- Opaque GObject/GTK/WebKit/X11 pointer aliases -------------------------

type gpointer = *mut c_void;
type GtkWidget = c_void;
type GtkWindow = c_void;
type GtkContainer = c_void;
type WebKitWebView = c_void;
type WebKitUserContentManager = c_void;
type WebKitUserScript = c_void;
type WebKitJavascriptResult = c_void;
type GError = c_void;
type GMainContext = c_void;
type JSCValue = c_void;
type GdkWindow = c_void;
type XDisplay = c_void;

// GtkWindowType::GTK_WINDOW_TOPLEVEL
const GTK_WINDOW_TOPLEVEL: c_int = 0;
// WebKitUserContentInjectedFrames::WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES
const WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES: c_int = 0;
// WebKitUserScriptInjectionTime::WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START
const WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START: c_int = 0;
// WebKitLoadEvent
const WEBKIT_LOAD_STARTED: c_int = 0;
const WEBKIT_LOAD_REDIRECTED: c_int = 1;
const WEBKIT_LOAD_COMMITTED: c_int = 2;
const WEBKIT_LOAD_FINISHED: c_int = 3;
// g_signal_connect / GConnectFlags::G_CONNECT_AFTER = 0 for plain connect
const G_CONNECT_DEFAULT: c_int = 0;

// GCallback is a bare function pointer; g_signal_connect_data takes it as such.
type GCallback = *const c_void;
type GClosureNotify = Option<unsafe extern "C" fn(data: gpointer, closure: *mut c_void)>;

// --- dlopen-loaded symbol table --------------------------------------------
//
// Matches the `os::linux::module_loader::ModuleLoader` pattern used across the
// platform crate (see gstreamer_sys.rs): dlopen each `.so`, resolve every
// function pointer once, store them in a struct. We re-implement a tiny loader
// locally because `ModuleLoader` is internal to the platform crate.

struct Loader(*mut c_void);
// SAFETY: the handle is only used to resolve symbols; all actual GTK calls are
// confined to the GTK/main thread. The Lib is stored in a thread-unsynchronized
// OnceLock guarded by our own single-thread discipline (see module docs).
unsafe impl Send for Loader {}
unsafe impl Sync for Loader {}

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}
const RTLD_LAZY: c_int = 0x0001;
const RTLD_GLOBAL: c_int = 0x0100;

impl Loader {
    fn open(names: &[&str]) -> Option<Loader> {
        for name in names {
            let cname = CString::new(*name).ok()?;
            // RTLD_GLOBAL so GObject type registrations are visible across the
            // separately-dlopened GTK/WebKit modules.
            let h = unsafe { dlopen(cname.as_ptr(), RTLD_LAZY | RTLD_GLOBAL) };
            if !h.is_null() {
                return Some(Loader(h));
            }
        }
        None
    }
    fn sym<F>(&self, name: &str) -> Option<F> {
        let cname = CString::new(name).ok()?;
        let p = unsafe { dlsym(self.0, cname.as_ptr()) };
        if p.is_null() {
            None
        } else {
            Some(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&p) })
        }
    }
}

/// Resolved WebKitGTK + GTK + GDK + GLib + X11 entry points.
#[allow(dead_code)]
struct Lib {
    // GLib / GObject
    g_signal_connect_data: unsafe extern "C" fn(
        instance: gpointer,
        detailed_signal: *const c_char,
        c_handler: GCallback,
        data: gpointer,
        destroy_data: GClosureNotify,
        connect_flags: c_int,
    ) -> c_ulong,
    g_object_ref: unsafe extern "C" fn(gpointer) -> gpointer,
    g_object_unref: unsafe extern "C" fn(gpointer),
    g_free: unsafe extern "C" fn(gpointer),
    g_main_context_iteration: unsafe extern "C" fn(ctx: *mut GMainContext, may_block: c_int) -> c_int,
    g_main_context_pending: unsafe extern "C" fn(ctx: *mut GMainContext) -> c_int,

    // GTK 3
    gtk_init_check: unsafe extern "C" fn(argc: *mut c_int, argv: *mut *mut *mut c_char) -> c_int,
    gtk_window_new: unsafe extern "C" fn(kind: c_int) -> *mut GtkWidget,
    gtk_container_add: unsafe extern "C" fn(container: *mut GtkContainer, widget: *mut GtkWidget),
    gtk_widget_show_all: unsafe extern "C" fn(widget: *mut GtkWidget),
    gtk_widget_hide: unsafe extern "C" fn(widget: *mut GtkWidget),
    gtk_widget_realize: unsafe extern "C" fn(widget: *mut GtkWidget),
    gtk_widget_get_window: unsafe extern "C" fn(widget: *mut GtkWidget) -> *mut GdkWindow,
    gtk_window_move: unsafe extern "C" fn(window: *mut GtkWindow, x: c_int, y: c_int),
    gtk_window_resize: unsafe extern "C" fn(window: *mut GtkWindow, w: c_int, h: c_int),
    gtk_widget_destroy: unsafe extern "C" fn(widget: *mut GtkWidget),

    // GDK (X11 backend) — get the XID of a realized GdkWindow, and the display.
    gdk_x11_window_get_xid: Option<unsafe extern "C" fn(window: *mut GdkWindow) -> c_ulong>,
    gdk_x11_display_get_xdisplay: Option<unsafe extern "C" fn(display: gpointer) -> *mut XDisplay>,

    // Raw Xlib — reparent/position GTK's X11 window under Makepad's X11 window.
    // Loaded directly from libX11 (not present in the platform crate's x11_sys
    // bindgen bindings), matching the platform's dlopen discipline.
    XReparentWindow: Option<
        unsafe extern "C" fn(dpy: *mut XDisplay, w: c_ulong, parent: c_ulong, x: c_int, y: c_int) -> c_int,
    >,
    XMoveResizeWindow: Option<
        unsafe extern "C" fn(
            dpy: *mut XDisplay,
            w: c_ulong,
            x: c_int,
            y: c_int,
            width: c_uint,
            height: c_uint,
        ) -> c_int,
    >,
    XMapWindow: Option<unsafe extern "C" fn(dpy: *mut XDisplay, w: c_ulong) -> c_int>,
    XUnmapWindow: Option<unsafe extern "C" fn(dpy: *mut XDisplay, w: c_ulong) -> c_int>,
    XFlush: Option<unsafe extern "C" fn(dpy: *mut XDisplay) -> c_int>,

    // WebKitGTK
    webkit_web_view_new: unsafe extern "C" fn() -> *mut GtkWidget,
    webkit_web_view_get_user_content_manager:
        unsafe extern "C" fn(view: *mut WebKitWebView) -> *mut WebKitUserContentManager,
    webkit_user_content_manager_register_script_message_handler:
        unsafe extern "C" fn(mgr: *mut WebKitUserContentManager, name: *const c_char) -> c_int,
    webkit_user_content_manager_add_script:
        unsafe extern "C" fn(mgr: *mut WebKitUserContentManager, script: *mut WebKitUserScript),
    webkit_user_script_new: unsafe extern "C" fn(
        source: *const c_char,
        injected_frames: c_int,
        injection_time: c_int,
        allow_list: *const *const c_char,
        block_list: *const *const c_char,
    ) -> *mut WebKitUserScript,
    webkit_web_view_load_uri: unsafe extern "C" fn(view: *mut WebKitWebView, uri: *const c_char),
    webkit_web_view_go_back: unsafe extern "C" fn(view: *mut WebKitWebView),
    webkit_web_view_go_forward: unsafe extern "C" fn(view: *mut WebKitWebView),
    webkit_web_view_get_uri: unsafe extern "C" fn(view: *mut WebKitWebView) -> *const c_char,

    // JS evaluation: 4.1 renamed run_javascript → evaluate_javascript. Prefer
    // the newer symbol, fall back to the legacy one.
    webkit_web_view_evaluate_javascript: Option<
        unsafe extern "C" fn(
            view: *mut WebKitWebView,
            script: *const c_char,
            length: isize,
            world_name: *const c_char,
            source_uri: *const c_char,
            cancellable: gpointer,
            callback: gpointer,
            user_data: gpointer,
        ),
    >,
    webkit_web_view_run_javascript: Option<
        unsafe extern "C" fn(
            view: *mut WebKitWebView,
            script: *const c_char,
            cancellable: gpointer,
            callback: gpointer,
            user_data: gpointer,
        ),
    >,

    // Reading the JS-message payload out of a script-message signal.
    // 4.1: the signal delivers a JSCValue* directly. 4.0: a
    // WebKitJavascriptResult* that must be unwrapped via get_js_value.
    webkit_javascript_result_get_js_value:
        Option<unsafe extern "C" fn(res: *mut WebKitJavascriptResult) -> *mut JSCValue>,
    jsc_value_to_string: Option<unsafe extern "C" fn(value: *mut JSCValue) -> *mut c_char>,
}

impl Lib {
    fn load() -> Option<Lib> {
        // WebKitGTK 4.1 (GTK3 + libsoup3) preferred, then 4.0 (libsoup2).
        let webkit = Loader::open(&[
            "libwebkit2gtk-4.1.so.0",
            "libwebkit2gtk-4.1.so",
            "libwebkit2gtk-4.0.so.37",
            "libwebkit2gtk-4.0.so",
        ])?;
        let gtk = Loader::open(&["libgtk-3.so.0", "libgtk-3.so"])?;
        let gdk = Loader::open(&["libgdk-3.so.0", "libgdk-3.so"])?;
        let gobject = Loader::open(&["libgobject-2.0.so.0", "libgobject-2.0.so"])?;
        let glib = Loader::open(&["libglib-2.0.so.0", "libglib-2.0.so"])?;
        // GDK's X11 symbols may live in libgtk/libgdk directly or a companion.
        let x11 = Loader::open(&["libX11.so.6", "libX11.so"]);

        Some(Lib {
            g_signal_connect_data: gobject.sym("g_signal_connect_data")?,
            g_object_ref: gobject.sym("g_object_ref")?,
            g_object_unref: gobject.sym("g_object_unref")?,
            g_free: glib.sym("g_free")?,
            g_main_context_iteration: glib.sym("g_main_context_iteration")?,
            g_main_context_pending: glib.sym("g_main_context_pending")?,

            gtk_init_check: gtk.sym("gtk_init_check")?,
            gtk_window_new: gtk.sym("gtk_window_new")?,
            gtk_container_add: gtk.sym("gtk_container_add")?,
            gtk_widget_show_all: gtk.sym("gtk_widget_show_all")?,
            gtk_widget_hide: gtk.sym("gtk_widget_hide")?,
            gtk_widget_realize: gtk.sym("gtk_widget_realize")?,
            gtk_widget_get_window: gtk.sym("gtk_widget_get_window")?,
            gtk_window_move: gtk.sym("gtk_window_move")?,
            gtk_window_resize: gtk.sym("gtk_window_resize")?,
            gtk_widget_destroy: gtk.sym("gtk_widget_destroy")?,

            gdk_x11_window_get_xid: gdk.sym("gdk_x11_window_get_xid"),
            gdk_x11_display_get_xdisplay: gdk.sym("gdk_x11_display_get_xdisplay"),

            XReparentWindow: x11.as_ref().and_then(|l| l.sym("XReparentWindow")),
            XMoveResizeWindow: x11.as_ref().and_then(|l| l.sym("XMoveResizeWindow")),
            XMapWindow: x11.as_ref().and_then(|l| l.sym("XMapWindow")),
            XUnmapWindow: x11.as_ref().and_then(|l| l.sym("XUnmapWindow")),
            XFlush: x11.as_ref().and_then(|l| l.sym("XFlush")),

            webkit_web_view_new: webkit.sym("webkit_web_view_new")?,
            webkit_web_view_get_user_content_manager: webkit
                .sym("webkit_web_view_get_user_content_manager")?,
            webkit_user_content_manager_register_script_message_handler: webkit
                .sym("webkit_user_content_manager_register_script_message_handler")?,
            webkit_user_content_manager_add_script: webkit
                .sym("webkit_user_content_manager_add_script")?,
            webkit_user_script_new: webkit.sym("webkit_user_script_new")?,
            webkit_web_view_load_uri: webkit.sym("webkit_web_view_load_uri")?,
            webkit_web_view_go_back: webkit.sym("webkit_web_view_go_back")?,
            webkit_web_view_go_forward: webkit.sym("webkit_web_view_go_forward")?,
            webkit_web_view_get_uri: webkit.sym("webkit_web_view_get_uri")?,

            webkit_web_view_evaluate_javascript: webkit.sym("webkit_web_view_evaluate_javascript"),
            webkit_web_view_run_javascript: webkit.sym("webkit_web_view_run_javascript"),

            webkit_javascript_result_get_js_value: webkit
                .sym("webkit_javascript_result_get_js_value"),
            jsc_value_to_string: webkit.sym("jsc_value_to_string"),
        })
    }
}

// Loaded once per process; None if WebKitGTK/GTK aren't installed.
fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(Lib::load).as_ref()
}

// --- Event queue shared with GTK signal callbacks --------------------------

type EventQueue = Arc<Mutex<Vec<WebViewEvent>>>;

/// Per-view context handed to GTK signal callbacks as their `user_data`. Leaked
/// (kept alive) for the lifetime of the web view; freed in [`LinuxBackend::detach`].
struct SignalCtx {
    queue: EventQueue,
    // Kept so `load-changed` can report the current URL even when the signal
    // itself doesn't carry it.
    lib: &'static Lib,
    view: *mut WebKitWebView,
}

// --- Signal callbacks (run on the GTK/main thread) -------------------------

// script-message-received::vsbridge (WebKitGTK 4.1 shape: JSCValue* arg).
unsafe extern "C" fn on_script_message_jsc(
    _mgr: *mut WebKitUserContentManager,
    value: *mut JSCValue,
    user_data: gpointer,
) {
    let ctx = &*(user_data as *const SignalCtx);
    let Some(to_string) = ctx.lib.jsc_value_to_string else {
        return;
    };
    let cstr = to_string(value);
    push_message_from_cstr(ctx, cstr);
    if !cstr.is_null() {
        (ctx.lib.g_free)(cstr as gpointer);
    }
}

// script-message-received::vsbridge (WebKitGTK 4.0 shape:
// WebKitJavascriptResult* arg).
unsafe extern "C" fn on_script_message_jsresult(
    _mgr: *mut WebKitUserContentManager,
    js_result: *mut WebKitJavascriptResult,
    user_data: gpointer,
) {
    let ctx = &*(user_data as *const SignalCtx);
    let (Some(get_val), Some(to_string)) = (
        ctx.lib.webkit_javascript_result_get_js_value,
        ctx.lib.jsc_value_to_string,
    ) else {
        return;
    };
    let value = get_val(js_result);
    if value.is_null() {
        return;
    }
    let cstr = to_string(value);
    push_message_from_cstr(ctx, cstr);
    if !cstr.is_null() {
        (ctx.lib.g_free)(cstr as gpointer);
    }
}

unsafe fn push_message_from_cstr(ctx: &SignalCtx, cstr: *mut c_char) {
    if cstr.is_null() {
        return;
    }
    let json = CStr::from_ptr(cstr).to_string_lossy().into_owned();
    // `window.vs.postMessage` sends a JSON-stringified BridgeMessage
    // ({kind, payload}); parse it, mirroring the Apple/Windows handlers.
    if let Ok(msg) = BridgeMessage::deserialize_json(&json) {
        if let Ok(mut q) = ctx.queue.lock() {
            q.push(WebViewEvent::Message(msg));
        }
    }
}

// load-changed(view, WebKitLoadEvent, user_data)
unsafe extern "C" fn on_load_changed(
    _view: *mut WebKitWebView,
    load_event: c_int,
    user_data: gpointer,
) {
    let ctx = &*(user_data as *const SignalCtx);
    let url = current_uri(ctx.lib, ctx.view);
    let ev = match load_event {
        WEBKIT_LOAD_STARTED => Some(WebViewEvent::LoadStarted { url }),
        WEBKIT_LOAD_FINISHED => Some(WebViewEvent::LoadFinished { url }),
        WEBKIT_LOAD_REDIRECTED | WEBKIT_LOAD_COMMITTED => None,
        _ => None,
    };
    if let Some(ev) = ev {
        if let Ok(mut q) = ctx.queue.lock() {
            q.push(ev);
        }
    }
}

// load-failed(view, WebKitLoadEvent, failing_uri, GError*, user_data) -> gboolean
unsafe extern "C" fn on_load_failed(
    _view: *mut WebKitWebView,
    _load_event: c_int,
    failing_uri: *const c_char,
    _error: *mut GError,
    user_data: gpointer,
) -> c_int {
    let ctx = &*(user_data as *const SignalCtx);
    let url = if failing_uri.is_null() {
        String::new()
    } else {
        CStr::from_ptr(failing_uri).to_string_lossy().into_owned()
    };
    if let Ok(mut q) = ctx.queue.lock() {
        q.push(WebViewEvent::LoadFailed {
            url,
            // WebKit's GError message would be richer; keep it terse and
            // dependency-free here.
            error: "load failed".to_string(),
        });
    }
    0 // FALSE: let WebKit show its default error page.
}

unsafe fn current_uri(lib: &Lib, view: *mut WebKitWebView) -> String {
    let p = (lib.webkit_web_view_get_uri)(view);
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

// --- The backend -----------------------------------------------------------

pub(crate) struct LinuxBackend {
    // None when WebKitGTK/GTK is unavailable, or when we could not embed (e.g.
    // Wayland): the backend then behaves like the unsupported no-op, but still
    // constructs and pumps an (empty) event queue.
    inner: Option<Inner>,
    queue: EventQueue,
    current_url: String,
}

#[allow(dead_code)]
struct Inner {
    lib: &'static Lib,
    // The GtkWindow hosting the WebKitWebView (GTK owns its own X11 window).
    gtk_window: *mut GtkWidget,
    web_view: *mut WebKitWebView,
    // The XID of the GtkWindow's realized X11 window, once known.
    web_xid: c_ulong,
    // The Xlib Display* GDK is talking to (for reparent/move/flush).
    xdisplay: *mut XDisplay,
    // Boxed signal context; kept alive for the view's lifetime, freed on detach.
    signal_ctx: *mut SignalCtx,
    // True once we successfully reparented into Makepad's X11 window.
    embedded: bool,
    // The Makepad X11 parent window we reparented under, if any.
    parent_xid: c_ulong,
}

impl LinuxBackend {
    fn init(&mut self, opts: &WebViewOptions) -> bool {
        let Some(lib) = lib() else {
            makepad_platform::log!("makepad-webview: WebKitGTK/GTK not available; web view inert");
            return false;
        };

        unsafe {
            // gtk_init_check is idempotent and safe to call repeatedly; it must
            // run on the GTK/main thread (== Makepad's X11 loop thread here).
            if (lib.gtk_init_check)(std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                makepad_platform::log!("makepad-webview: gtk_init_check failed; web view inert");
                return false;
            }

            let gtk_window = (lib.gtk_window_new)(GTK_WINDOW_TOPLEVEL);
            if gtk_window.is_null() {
                return false;
            }
            let web_view = (lib.webkit_web_view_new)();
            if web_view.is_null() {
                (lib.gtk_widget_destroy)(gtk_window);
                return false;
            }
            (lib.gtk_container_add)(gtk_window as *mut GtkContainer, web_view);

            // UserContentManager: register `vsbridge` + document-start injection.
            let ucm = (lib.webkit_web_view_get_user_content_manager)(web_view);
            let handler = CString::new(BRIDGE_HANDLER_NAME).unwrap();
            (lib.webkit_user_content_manager_register_script_message_handler)(ucm, handler.as_ptr());

            let source = CString::new(injection_script(NATIVE_POST_EXPR)).unwrap();
            let user_script = (lib.webkit_user_script_new)(
                source.as_ptr(),
                WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES,
                WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START,
                std::ptr::null(),
                std::ptr::null(),
            );
            if !user_script.is_null() {
                (lib.webkit_user_content_manager_add_script)(ucm, user_script);
            }

            // Signal context leaked into a raw box; freed in detach().
            let signal_ctx = Box::into_raw(Box::new(SignalCtx {
                queue: self.queue.clone(),
                lib,
                view: web_view,
            }));

            // JS → Rust: `script-message-received::vsbridge`. The callback
            // signature differs between 4.1 (JSCValue*) and 4.0
            // (WebKitJavascriptResult*); pick by which unwrap symbol exists.
            let signal_name = CString::new(format!(
                "script-message-received::{}",
                BRIDGE_HANDLER_NAME
            ))
            .unwrap();
            let cb: GCallback = if lib.webkit_javascript_result_get_js_value.is_some() {
                on_script_message_jsresult as *const c_void
            } else {
                on_script_message_jsc as *const c_void
            };
            (lib.g_signal_connect_data)(
                ucm,
                signal_name.as_ptr(),
                cb,
                signal_ctx as gpointer,
                None,
                G_CONNECT_DEFAULT,
            );

            // Load lifecycle.
            let load_changed = CString::new("load-changed").unwrap();
            (lib.g_signal_connect_data)(
                web_view,
                load_changed.as_ptr(),
                on_load_changed as *const c_void,
                signal_ctx as gpointer,
                None,
                G_CONNECT_DEFAULT,
            );
            let load_failed = CString::new("load-failed").unwrap();
            (lib.g_signal_connect_data)(
                web_view,
                load_failed.as_ptr(),
                on_load_failed as *const c_void,
                signal_ctx as gpointer,
                None,
                G_CONNECT_DEFAULT,
            );

            // Realize so GTK creates its X11 window; grab the XID + Display*.
            (lib.gtk_widget_realize)(gtk_window);
            let gdk_win = (lib.gtk_widget_get_window)(gtk_window);
            let (web_xid, xdisplay) = match lib.gdk_x11_window_get_xid {
                Some(get_xid) if !gdk_win.is_null() => {
                    let xid = get_xid(gdk_win);
                    // The Xlib Display* is best obtained from Makepad's global
                    // (same X connection matters for reparenting). We use
                    // Makepad's display below; keep GDK's only as a fallback.
                    (xid, std::ptr::null_mut())
                }
                _ => (0, std::ptr::null_mut()),
            };

            self.inner = Some(Inner {
                lib,
                gtk_window,
                web_view,
                web_xid,
                xdisplay,
                signal_ctx,
                embedded: false,
                parent_xid: 0,
            });

            // Load the initial URL.
            if !opts.url.is_empty() {
                self.load_uri(&opts.url);
                self.current_url = opts.url.clone();
            }
            true
        }
    }

    fn load_uri(&mut self, url: &str) {
        if let Some(inner) = &self.inner {
            if let Ok(curl) = CString::new(url) {
                unsafe { (inner.lib.webkit_web_view_load_uri)(inner.web_view, curl.as_ptr()) };
            }
        }
    }

    /// Resolve the reparent target: the Xlib `Display*` shared with Makepad and
    /// the X11 window id owning `target_window` (or, when `None`, the first
    /// mapped Makepad window). Returns `None` under Wayland or when no matching
    /// window exists.
    ///
    /// Reachable because `makepad-platform` re-exports its Linux OS module
    /// (`pub mod os;` → `pub use crate::os::linux::*;`). `get_xlib_app_global()`,
    /// `XlibApp.{display, window_map}` and `XlibWindow.{window, window_id}` are
    /// all already `pub`.
    fn makepad_x11_target(
        target_window: Option<makepad_platform::WindowId>,
    ) -> Option<(*mut XDisplay, c_ulong)> {
        // Under Wayland there is no XlibApp; `os::linux::wayland` is active
        // instead. We cannot statically know the protocol here, so guard the
        // access: if `DISPLAY` is unset (pure Wayland) skip. On XWayland
        // `DISPLAY` is set and this still works.
        if std::env::var_os("DISPLAY").is_none() {
            return None;
        }
        // SAFETY: on the X11 loop thread the global is initialized. If the app
        // is running Wayland this symbol still links but the app is a different
        // one; the DISPLAY guard above is the practical protection.
        //
        // NOTE: the exact import path is
        // `makepad_platform::os::linux::x11::xlib_app`. If a future refactor
        // makes `os` non-pub, see the report for the symbols to re-export.
        use makepad_platform::os::linux::x11::xlib_app::get_xlib_app_global;
        let app = get_xlib_app_global();
        if app.display.is_null() {
            return None;
        }
        // Prefer the X11 window that owns the driving widget's `Area`. Each
        // `window_map` value is a `*mut XlibWindow` carrying its `window_id`
        // and native `window` (XID).
        let mut chosen: Option<c_ulong> = None;
        for (&xid, &win_ptr) in app.window_map.iter() {
            if win_ptr.is_null() {
                continue;
            }
            // SAFETY: pointers in window_map are valid while their window lives;
            // we're on the X11 thread that mutates the map, so no concurrent
            // mutation.
            let win = unsafe { &*win_ptr };
            match target_window {
                Some(want) if win.window_id == want => {
                    chosen = win.window.or(Some(xid));
                    break;
                }
                Some(_) => {}
                None => {
                    // No specific target: take the first mapped window.
                    chosen = win.window.or(Some(xid));
                    break;
                }
            }
        }
        // If a target was requested but not found (window not yet mapped),
        // don't fall back to an arbitrary window — retry next frame instead.
        let parent = chosen?;
        Some((app.display as *mut XDisplay, parent))
    }

    /// Reparent GTK's X11 window under Makepad's X11 window (X11 only).
    fn try_embed(&mut self, target_window: Option<makepad_platform::WindowId>) {
        let Some(inner) = &mut self.inner else { return };
        if inner.embedded || inner.web_xid == 0 {
            return;
        }
        let Some((display, parent)) = Self::makepad_x11_target(target_window) else {
            // Wayland / no window yet: leave un-embedded (nothing shown). See
            // module-level windowing-compat note.
            return;
        };
        let (Some(reparent), Some(flush)) = (inner.lib.XReparentWindow, inner.lib.XFlush) else {
            return;
        };
        unsafe {
            reparent(display, inner.web_xid, parent, 0, 0);
            flush(display);
        }
        inner.xdisplay = display;
        inner.parent_xid = parent;
        inner.embedded = true;
    }
}

impl WebViewBackend for LinuxBackend {
    fn new(_cx: &mut Cx, opts: &WebViewOptions) -> Result<Self> {
        let mut backend = LinuxBackend {
            inner: None,
            queue: Arc::new(Mutex::new(Vec::new())),
            current_url: String::new(),
        };
        // Never hard-fail construction: if WebKitGTK is missing or embedding
        // isn't possible, we still return a usable (inert) handle so the widget
        // layer behaves uniformly, matching the crate's "graceful unsupported"
        // philosophy. Returning Err(Unsupported) would break otherwise-fine
        // apps merely because libwebkit2gtk isn't installed.
        backend.init(opts);
        Ok(backend)
    }

    fn set_url(&mut self, _cx: &mut Cx, url: &str) {
        if self.inner.is_none() {
            return;
        }
        if self.current_url != url {
            self.load_uri(url);
            self.current_url = url.to_string();
        }
    }

    fn update_rect(&mut self, cx: &mut Cx, area: Area, visible: bool) {
        // Ensure we've embedded (needs a Makepad window to exist; on first
        // frames it may not yet, so retry here each update until it succeeds).
        if visible {
            let target = cx.get_window_id_of(&area);
            self.try_embed(target);
        }
        let Some(inner) = &self.inner else { return };
        unsafe {
            if !visible || !area.is_valid(cx) {
                (inner.lib.gtk_widget_hide)(inner.gtk_window);
                if inner.embedded {
                    if let (Some(unmap), Some(flush)) = (inner.lib.XUnmapWindow, inner.lib.XFlush) {
                        unmap(inner.xdisplay, inner.web_xid);
                        flush(inner.xdisplay);
                    }
                }
                return;
            }

            // Position/size in native (device) pixels. Makepad's `Area::rect`
            // is in logical points; scale by the window dpi factor.
            let rect = area.rect(cx);
            let dpi = cx.get_dpi_factor_of(&area);
            let x = (rect.pos.x * dpi) as c_int;
            let y = (rect.pos.y * dpi) as c_int;
            let w = (rect.size.x * dpi).max(1.0) as c_uint;
            let h = (rect.size.y * dpi).max(1.0) as c_uint;

            (inner.lib.gtk_widget_show_all)(inner.gtk_window);

            if inner.embedded {
                // As a reparented child, position via raw X11 relative to the
                // Makepad parent window (GTK's window-manager move/resize won't
                // apply to an override child).
                if let (Some(move_resize), Some(map), Some(flush)) = (
                    inner.lib.XMoveResizeWindow,
                    inner.lib.XMapWindow,
                    inner.lib.XFlush,
                ) {
                    map(inner.xdisplay, inner.web_xid);
                    move_resize(inner.xdisplay, inner.web_xid, x, y, w, h);
                    flush(inner.xdisplay);
                }
            } else {
                // Un-embedded fallback: drive the toplevel GtkWindow directly
                // (appears as a separate OS window). Better than nothing when
                // reparenting is unavailable.
                (inner.lib.gtk_window_move)(inner.gtk_window as *mut GtkWindow, x, y);
                (inner.lib.gtk_window_resize)(inner.gtk_window as *mut GtkWindow, w as c_int, h as c_int);
            }
        }
    }

    fn history_go(&mut self, _cx: &mut Cx, delta: i32) {
        let Some(inner) = &self.inner else { return };
        // WebKitGTK exposes single-step go_back/go_forward; approximate a
        // multi-step delta by repeating (WebKit clamps at history bounds).
        unsafe {
            if delta < 0 {
                for _ in 0..(-delta) {
                    (inner.lib.webkit_web_view_go_back)(inner.web_view);
                }
            } else if delta > 0 {
                for _ in 0..delta {
                    (inner.lib.webkit_web_view_go_forward)(inner.web_view);
                }
            }
        }
    }

    fn eval_js(&mut self, _cx: &mut Cx, script: &str) {
        let Some(inner) = &self.inner else { return };
        let Ok(cscript) = CString::new(script) else {
            return;
        };
        unsafe {
            if let Some(eval) = inner.lib.webkit_web_view_evaluate_javascript {
                // WebKitGTK 4.1+ signature.
                eval(
                    inner.web_view,
                    cscript.as_ptr(),
                    -1, // length: -1 = NUL-terminated
                    std::ptr::null(), // world_name: default world
                    std::ptr::null(), // source_uri
                    std::ptr::null_mut(), // cancellable
                    std::ptr::null_mut(), // callback
                    std::ptr::null_mut(), // user_data
                );
            } else if let Some(run) = inner.lib.webkit_web_view_run_javascript {
                // Legacy WebKitGTK 4.0 signature (deprecated but present).
                run(
                    inner.web_view,
                    cscript.as_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
            }
        }
    }

    fn drain_events(&mut self) -> Vec<WebViewEvent> {
        // Pump the GTK main context so WebKit signals (script-message,
        // load-changed, load-failed) fire and enqueue onto `self.queue`. We run
        // on Makepad's event-loop thread, which we treat as the GTK thread.
        if let Some(inner) = &self.inner {
            unsafe {
                let ctx: *mut GMainContext = std::ptr::null_mut(); // default context
                let mut guard = 0;
                while (inner.lib.g_main_context_pending)(ctx) != 0 && guard < 10_000 {
                    (inner.lib.g_main_context_iteration)(ctx, 0 /* may_block=false */);
                    guard += 1;
                }
            }
        }
        match self.queue.lock() {
            Ok(mut q) => core::mem::take(&mut *q),
            Err(_) => Vec::new(),
        }
    }

    fn detach(&mut self, _cx: &mut Cx) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        unsafe {
            if inner.embedded {
                if let (Some(unmap), Some(flush)) = (inner.lib.XUnmapWindow, inner.lib.XFlush) {
                    unmap(inner.xdisplay, inner.web_xid);
                    flush(inner.xdisplay);
                }
            }
            (inner.lib.gtk_widget_hide)(inner.gtk_window);
            // Destroying the toplevel GtkWindow also destroys the contained
            // WebKitWebView and its GTK/X11 resources.
            (inner.lib.gtk_widget_destroy)(inner.gtk_window);
            // Free the leaked signal context now that no callback can fire.
            if !inner.signal_ctx.is_null() {
                drop(Box::from_raw(inner.signal_ctx));
            }
        }
    }
}
