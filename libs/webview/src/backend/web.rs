//! Web (`wasm32`) backend.
//!
//! Makepad on the web does **not** use `wasm-bindgen`/`web-sys`. It ships its
//! own hand-written JS↔wasm bridge: Rust pushes `FromWasm*` command structs into
//! a `FromWasmMsg` buffer (drained by `WasmWebBrowser.FromWasm*` handlers in
//! `platform/src/os/web/web.js`), and JS pushes `ToWasm*` structs back into a
//! `ToWasmMsg` buffer that reappears in Rust either as a hand-matched arm in
//! `Cx::process_to_wasm` or, for message ids the platform doesn't recognise, as
//! a generic [`Event::ToWasmMsg`] delivered to the app's event handler. This
//! backend follows that idiom exactly — no `web-sys`, no `wasm-bindgen`.
//!
//! # Why this backend does not own any native object
//!
//! An out-of-platform crate cannot synthesise a `FromWasm` on demand: the
//! `FromWasmMsg` buffer only exists for the duration of `Cx::process_to_wasm`.
//! The supported way to ask the JS runtime to do DOM work is to enqueue a
//! `CxOsOp` on `Cx::platform_ops`, which the platform drains in
//! `handle_platform_ops` and translates into `FromWasm*` messages. Makepad
//! already exposes a *generic* browser-overlay op family for exactly this —
//! [`Cx::system_browser`] (`SpawnSystemBrowser` / `UpdateSystemBrowser` /
//! `SetSystemBrowserUrl` / `SystemBrowserHistoryGo` / `DetachSystemBrowser` /
//! `CloseSystemBrowser`) — which the Apple backend already drives. We mirror
//! Apple here: the `<iframe>` overlay is owned by the platform/JS runtime and
//! keyed by [`WebViewOptions::id`], and this backend is a thin driver plus an
//! inbound event queue.
//!
//! The web arms of those ops (and the iframe DOM overlay + parent-window
//! `message` listener they need) are **not yet implemented in the platform
//! crate**; see the crate report / the "PLATFORM CRATE ADDITIONS" comment block
//! below for the precise list. Until they land the driver is a well-formed
//! no-op — nothing panics, `drain_events` simply yields whatever the host app
//! has fed into the shared queue (nothing, if the plumbing is absent).
//!
//! # Cross-origin honesty
//!
//! The child page runs in an `<iframe>`. The JS↔Rust bridge is carried over
//! `window.postMessage` between the iframe and the Makepad parent frame:
//!
//! * **Inbound (page → Rust):** the injected bridge posts
//!   `window.parent.postMessage({__vsbridge:true, data:<json>}, '*')`. For a
//!   **same-origin** iframe the parent can inject [`bridge::injection_script`]
//!   into `iframe.contentWindow` on every `load`, so the page gets `window.vs`
//!   for free. For a **cross-origin** iframe the parent **cannot** touch
//!   `contentWindow` (the browser's same-origin policy throws on any access),
//!   so injection is impossible; such a page only reaches Rust if it
//!   *cooperatively* calls `window.parent.postMessage({__vsbridge:true, ...})`
//!   itself. This limitation is fundamental to the web platform, not a TODO.
//!
//! * **Outbound (Rust → page):** [`eval_js`](WebBackend::eval_js) and
//!   [`history_go`](WebBackend::history_go) post a request into
//!   `iframe.contentWindow`. For a same-origin iframe the runtime can actually
//!   run the script / call `history.go`. For a cross-origin iframe neither is
//!   possible: script injection is blocked outright, and `history.go`/`src`
//!   changes on a cross-origin frame are subject to the same-origin policy and
//!   browser navigation rules — they are therefore **best-effort** and silently
//!   ignored by the browser when disallowed.

// The public web-bridge surface (`pump_event`, `web_injection_script`,
// `WEB_NATIVE_POST_EXPR`) is intended to be re-exported from the crate root so
// the host app can feed `Event`s in and obtain the same-origin injection
// script; `lib.rs` does not yet do so (see report), so within this
// `pub(crate)` module rustc cannot see those uses and the inbound ToWasm reader
// structs (populated by value from the wire) look unconstructed. Silence the
// resulting dead-code noise here rather than pretending the API is internal.
#![allow(dead_code)]

use crate::backend::WebViewBackend;
use crate::bridge::{self, BridgeMessage};
use crate::{Result, WebViewEvent, WebViewOptions};
use makepad_micro_serde::DeJson;
use makepad_platform::makepad_live_id::{live_id, LiveId};
use makepad_platform::{Area, Cx, Event};
use std::cell::RefCell;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// PLATFORM CRATE ADDITIONS (described, not applied — see report)
// ---------------------------------------------------------------------------
//
// This backend only *enqueues* the existing generic `CxSystemBrowser` ops and
// *reads* an inbound queue. To make it live end-to-end, the platform crate must
// grow the web-side implementation of those ops. Concretely:
//
// 1. `platform/src/os/web/web.rs`, in `Cx::handle_platform_ops`, add web arms
//    for the browser ops (they currently exist only for Apple):
//        CxOsOp::SpawnSystemBrowser{browser_id,url}       => from_wasm(FromWasmSpawnWebView{ id: browser_id.0, url })
//        CxOsOp::UpdateSystemBrowser{browser_id,area,visible} => resolve `area.rect(self)` (dvec2 pos/size in
//                                                                logical px) and from_wasm(FromWasmUpdateWebView{ id, x, y, w, h, visible })
//        CxOsOp::SetSystemBrowserUrl{browser_id,url,..}   => from_wasm(FromWasmSetWebViewUrl{ id, url })
//        CxOsOp::SystemBrowserHistoryGo{browser_id,delta} => from_wasm(FromWasmWebViewHistoryGo{ id, delta })
//        CxOsOp::CloseSystemBrowser{browser_id} | DetachSystemBrowser{browser_id}
//                                                         => from_wasm(FromWasmDetachWebView{ id })
//        CxOsOp::EvalSystemBrowserJs{browser_id,script}  => from_wasm(FromWasmWebViewEvalJs{ id, script })
//    (`EvalSystemBrowserJs` already EXISTS as a CxOsOp + `CxSystemBrowser::eval_js`;
//     only its web `handle_platform_ops` arm + the `Display` arm at cx_api.rs
//     ~line 546 (currently non-exhaustive) + the JS handler are missing.)
//
// 2. `platform/src/os/web/from_wasm.rs`, new `#[derive(FromWasm)]` structs.
//    IMPORTANT: the derive has NO `impl ToWasm/FromWasm for u64`, so a
//    `SystemBrowserId(LiveId(u64))` MUST be carried as two u32 halves
//    (`LiveId::to_lo_hi()` / `from_lo_hi()`), exactly like the video ids:
//        FromWasmSpawnWebView       { id_lo:u32, id_hi:u32, url:String }
//        FromWasmUpdateWebView      { id_lo:u32, id_hi:u32, x:f64, y:f64, w:f64, h:f64, visible:bool }
//        FromWasmSetWebViewUrl      { id_lo:u32, id_hi:u32, url:String }
//        FromWasmWebViewHistoryGo   { id_lo:u32, id_hi:u32, delta:f64 }
//        FromWasmWebViewEvalJs      { id_lo:u32, id_hi:u32, script:String }
//        FromWasmDetachWebView      { id_lo:u32, id_hi:u32 }
//
// 3. `platform/src/os/web/to_wasm.rs`, new `#[derive(ToWasm)]` structs — NOTE:
//    these are read here via `live_id!(..)` on the generic `Event::ToWasmMsg`
//    path, so the platform's `process_to_wasm` match does NOT need arms for
//    them; it only needs the struct definitions so JS can construct them and so
//    the derive registers the wire layout. Same u32 lo/hi rule for the id:
//        ToWasmWebViewLoadStart  { id_lo:u32, id_hi:u32, url:String }
//        ToWasmWebViewLoadFinish { id_lo:u32, id_hi:u32, url:String }
//        ToWasmWebViewLoadError  { id_lo:u32, id_hi:u32, url:String, error:String }
//        ToWasmWebViewBridgeMsg  { id_lo:u32, id_hi:u32, data:String }   // `data` = JSON of BridgeMessage
//
// 4. `platform/src/os/web/web.js`, new `WasmWebBrowser` methods + one-time
//    parent-window listener install (handler snippets are in the report):
//        FromWasmSpawnWebView(args)      -> create <iframe>, position off-screen,
//                                           src=args.url, id-keyed in this.webviews[args.id];
//                                           on first spawn install the single
//                                           window 'message' listener (see report).
//        FromWasmUpdateWebView(args)     -> style.left/top/width/height in CSS px
//                                           (logical px == what UpdateSystemBrowser
//                                           passes; DO NOT multiply by dpi_factor —
//                                           CSS px are already logical), display
//                                           none/block per args.visible, appended
//                                           over this.canvas.
//        FromWasmSetWebViewUrl(args)     -> iframe.src = args.url.
//        FromWasmWebViewHistoryGo(args)  -> iframe.contentWindow.postMessage(
//                                           {__vsbridge_cmd:'history', delta:args.delta}, '*')
//        FromWasmWebViewEvalJs(args)     -> same-origin: iframe.contentWindow.eval / inject
//                                           <script>; cross-origin: postMessage cmd (best-effort).
//        FromWasmDetachWebView(args)     -> remove/hide iframe, delete this.webviews[args.id].
//    The 'message' listener filters `e.data && e.data.__vsbridge`, finds the
//    source iframe (match e.source against each webview's contentWindow to
//    recover the id), then this.to_wasm.ToWasmWebViewBridgeMsg({id, data:JSON.stringify(e.data.data)})
//    and this.do_wasm_pump(). iframe.onload posts ToWasmWebViewLoadFinish, etc.
//    For same-origin iframes, iframe.onload also injects the bridge:
//        injection with native_post_expr =
//        "window.parent.postMessage({__vsbridge:true, data:%MSG%}, '*')"
//    (this crate produces that script via `bridge::injection_script`, but the
//    string must reach JS — pass it through FromWasmSpawnWebView if desired, or
//    re-derive it in JS; see report).
// ---------------------------------------------------------------------------

/// `native_post_expr` handed to [`bridge::injection_script`] for same-origin
/// iframes: the page-side `window.vs.postMessage` forwards to the Makepad parent
/// frame. Cross-origin pages cannot receive this injection (see module docs) and
/// must post the same envelope themselves.
pub const WEB_NATIVE_POST_EXPR: &str = "window.parent.postMessage({__vsbridge:true, data:%MSG%}, '*')";

thread_local! {
    /// Inbound events routed from [`Event::ToWasmMsg`] to the owning backend,
    /// keyed by [`WebViewOptions::id`]. The host app calls [`pump_event`] once
    /// per event; [`WebBackend::drain_events`] takes the per-id queue.
    ///
    /// A `thread_local` (not a global `Mutex`) is correct here: Makepad's wasm
    /// runtime is single-threaded on the main event loop, and all `Event`
    /// dispatch + `poll_events` happen on that same thread.
    static EVENT_QUEUES: RefCell<HashMap<u64, Vec<WebViewEvent>>> = RefCell::new(HashMap::new());
}

fn push_event(id: u64, ev: WebViewEvent) {
    EVENT_QUEUES.with(|q| q.borrow_mut().entry(id).or_default().push(ev));
}

fn take_events(id: u64) -> Vec<WebViewEvent> {
    EVENT_QUEUES.with(|q| {
        q.borrow_mut()
            .get_mut(&id)
            .map(core::mem::take)
            .unwrap_or_default()
    })
}

fn drop_queue(id: u64) {
    EVENT_QUEUES.with(|q| {
        q.borrow_mut().remove(&id);
    });
}

/// Feed a Makepad [`Event`] to the web webview backend(s).
///
/// The host app **must** call this from its event handler for every event
/// (or at least for every [`Event::ToWasmMsg`]) so inbound iframe bridge
/// messages and load events reach the owning [`WebView`] via
/// [`crate::WebView::poll_events`]. Events for unknown message ids or unknown
/// webview ids are ignored, so it is safe to call unconditionally.
///
/// This mirrors how other Makepad web features surface out-of-platform
/// `ToWasm*` messages: the platform's `process_to_wasm` doesn't know these
/// message ids, so it wraps them in [`Event::ToWasmMsg`] and hands them to the
/// app; we decode them here by `live_id`.
pub fn pump_event(event: &Event) {
    let Event::ToWasmMsg(tw) = event else {
        return;
    };
    match tw.id {
        id if id == live_id!(ToWasmWebViewLoadStart) => {
            let m = ToWasmWebViewLoadStart::from_ref(tw.as_ref());
            push_event(m.id, WebViewEvent::LoadStarted { url: m.url });
        }
        id if id == live_id!(ToWasmWebViewLoadFinish) => {
            let m = ToWasmWebViewLoadFinish::from_ref(tw.as_ref());
            push_event(m.id, WebViewEvent::LoadFinished { url: m.url });
        }
        id if id == live_id!(ToWasmWebViewLoadError) => {
            let m = ToWasmWebViewLoadError::from_ref(tw.as_ref());
            push_event(
                m.id,
                WebViewEvent::LoadFailed {
                    url: m.url,
                    error: m.error,
                },
            );
        }
        id if id == live_id!(ToWasmWebViewBridgeMsg) => {
            let m = ToWasmWebViewBridgeMsg::from_ref(tw.as_ref());
            // `data` is the JSON of a BridgeMessage; be lenient — a
            // non-cooperative cross-origin page may post malformed envelopes.
            if let Ok(msg) = BridgeMessage::deserialize_json(&m.data) {
                push_event(m.id, WebViewEvent::Message(msg));
            }
        }
        _ => {}
    }
}

// --- Inbound ToWasm decode structs ----------------------------------------
//
// These MIRROR the `#[derive(ToWasm)]` structs listed in the PLATFORM CRATE
// ADDITIONS block (they must live in the platform crate so JS can construct
// them and the derive registers the wire layout). They are decoded here off
// the generic `Event::ToWasmMsg` path via `ToWasmMsgRef`. We read the fields in
// declaration order using the same reader the derive would generate. If/when
// the platform structs land, the intent is to import them instead of these
// hand readers; kept local so this file compiles standalone today.

struct ToWasmWebViewLoadStart {
    id: u64,
    url: String,
}
struct ToWasmWebViewLoadFinish {
    id: u64,
    url: String,
}
struct ToWasmWebViewLoadError {
    id: u64,
    url: String,
    error: String,
}
struct ToWasmWebViewBridgeMsg {
    id: u64,
    data: String,
}

use makepad_platform::makepad_wasm_bridge::ToWasmMsgRef;

/// Read an `id_lo: u32, id_hi: u32` field pair (the two halves of a
/// `SystemBrowserId(LiveId)`) as consecutive `u32` reads and recombine — this is
/// exactly the wire the `ToWasm` derive emits for two adjacent `u32` fields, and
/// mirrors `LiveId::from_lo_hi`. (The derive has no `u64` support, so ids are
/// always split; see PLATFORM CRATE ADDITIONS.)
fn read_id_lo_hi(r: &mut ToWasmMsgRef) -> u64 {
    let lo = r.read_u32() as u64;
    let hi = r.read_u32() as u64;
    lo | (hi << 32)
}

impl ToWasmWebViewLoadStart {
    fn from_ref(mut r: ToWasmMsgRef) -> Self {
        Self {
            id: read_id_lo_hi(&mut r),
            url: r.read_string(),
        }
    }
}
impl ToWasmWebViewLoadFinish {
    fn from_ref(mut r: ToWasmMsgRef) -> Self {
        Self {
            id: read_id_lo_hi(&mut r),
            url: r.read_string(),
        }
    }
}
impl ToWasmWebViewLoadError {
    fn from_ref(mut r: ToWasmMsgRef) -> Self {
        Self {
            id: read_id_lo_hi(&mut r),
            url: r.read_string(),
            error: r.read_string(),
        }
    }
}
impl ToWasmWebViewBridgeMsg {
    fn from_ref(mut r: ToWasmMsgRef) -> Self {
        Self {
            id: read_id_lo_hi(&mut r),
            data: r.read_string(),
        }
    }
}

// --- Backend ---------------------------------------------------------------

pub(crate) struct WebBackend {
    /// Stable id keying the iframe overlay in the JS runtime and this crate's
    /// inbound event queue.
    id: u64,
    /// Platform-side `SystemBrowserId` used to drive the generic browser ops.
    browser_id: makepad_platform::SystemBrowserId,
    spawned: bool,
    current_url: String,
    visible: bool,
}

impl WebBackend {
    fn system_browser<'a>(&self, cx: &'a mut Cx) -> makepad_platform::CxSystemBrowser<'a> {
        cx.system_browser(self.browser_id)
    }
}

impl WebViewBackend for WebBackend {
    fn new(cx: &mut Cx, opts: &WebViewOptions) -> Result<Self> {
        let mut backend = Self {
            id: opts.id,
            browser_id: makepad_platform::SystemBrowserId(LiveId(opts.id)),
            spawned: false,
            current_url: String::new(),
            visible: opts.visible,
        };
        // Ensure a clean per-id queue for this instance.
        drop_queue(backend.id);
        // Ask the JS runtime to create the iframe overlay (src = url).
        backend.system_browser(cx).spawn(&opts.url);
        backend.spawned = true;
        backend.current_url = opts.url.clone();
        Ok(backend)
    }

    fn set_url(&mut self, cx: &mut Cx, url: &str) {
        if !self.spawned {
            self.system_browser(cx).spawn(url);
            self.spawned = true;
            self.current_url = url.to_string();
            return;
        }
        if self.current_url != url {
            // Update the iframe `src`. Cross-origin navigation is subject to the
            // browser's rules but setting `src` is always allowed.
            self.system_browser(cx).set_url(url, false);
            self.current_url = url.to_string();
        }
    }

    fn update_rect(&mut self, cx: &mut Cx, area: Area, visible: bool) {
        if !self.spawned {
            return;
        }
        self.visible = visible;
        // `UpdateSystemBrowser` carries the makepad `Area`; the platform's web
        // arm resolves it to a logical-px rect (`area.rect(cx)`) and forwards
        // CSS-px left/top/width/height to the iframe. When hidden or invalid we
        // detach so the iframe stops intercepting input and painting.
        if visible && area.is_valid(cx) {
            self.system_browser(cx).update(area, true);
        } else {
            self.system_browser(cx).update(area, false);
        }
    }

    fn history_go(&mut self, cx: &mut Cx, delta: i32) {
        if self.spawned {
            // Best-effort: for a same-origin iframe the runtime calls
            // `contentWindow.history.go(delta)`; for cross-origin the browser
            // enforces its navigation policy and may ignore it.
            self.system_browser(cx).history_go(delta);
        }
    }

    fn eval_js(&mut self, cx: &mut Cx, script: &str) {
        if !self.spawned {
            return;
        }
        // Rust → page. Delivered to the iframe via the generic
        // `CxSystemBrowser::eval_js` op (`CxOsOp::EvalSystemBrowserJs`), which
        // the web platform arm forwards to the iframe as `FromWasmWebViewEvalJs`.
        // Runs for same-origin iframes; for a cross-origin iframe the browser
        // blocks script injection outright, so the runtime downgrades to a
        // best-effort `postMessage` command that only a cooperating page can
        // honour (see module docs). Also the transport used by
        // `WebView::post_to_js` -> `bridge::dispatch_script`.
        self.system_browser(cx).eval_js(script);
    }

    fn drain_events(&mut self) -> Vec<WebViewEvent> {
        take_events(self.id)
    }

    fn detach(&mut self, cx: &mut Cx) {
        if self.spawned {
            self.system_browser(cx).close();
            self.spawned = false;
        }
        drop_queue(self.id);
    }
}

/// Convenience: the document-start bridge script to inject into **same-origin**
/// iframes so their page scripts get `window.vs.*`. Exposed for the JS runtime
/// / host to pass through to the iframe on `load`. Cross-origin iframes cannot
/// be injected (see module docs) and must post the `__vsbridge` envelope
/// themselves.
pub fn web_injection_script() -> String {
    bridge::injection_script(WEB_NATIVE_POST_EXPR)
}
