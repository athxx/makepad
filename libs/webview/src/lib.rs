//! `makepad-webview` — the single entry point for embedding web content in a
//! Makepad app, with a bidirectional JS bridge (`window.vs.*`).
//!
//! # Overview
//!
//! [`WebView`] is a platform-agnostic handle. Internally it holds one backend
//! selected at compile time:
//!
//! - [`Backend::Native`] → the system WebView overlay for the current platform
//!   (WKWebView / Android WebView / WebView2 / WebKitGTK / `<iframe>`), falling
//!   back to a no-op [`WebViewError::Unsupported`] on platforms without an
//!   implementation.
//! - [`Backend::Cef`] → CEF rendered to a texture, available only under the
//!   `cef` feature (otherwise unsupported).
//!
//! The widget layer drives it each frame: `update_rect` to position the
//! overlay, and `poll_events` to pump [`WebViewEvent`]s (load state + inbound
//! bridge messages) back out to script.
//!
//! On Apple, the overlay lives in `makepad-platform` (its `parent_view` can only
//! be resolved on the platform event loop's main thread), so the Apple backend
//! here is a thin wrapper driving it through `CxOsOp` via `Cx::system_browser`.
//! Every other platform owns its native objects directly inside this crate.

use makepad_platform::{Area, Cx};

mod backend;
pub mod bridge;

pub use bridge::BridgeMessage;

/// Which rendering strategy a [`WebView`] uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// System WebView overlay for the current platform.
    Native,
    /// CEF rendered to a texture (requires the `cef` feature).
    Cef,
}

impl Default for Backend {
    fn default() -> Self {
        Backend::Native
    }
}

/// Options for constructing a [`WebView`].
#[derive(Clone, Debug)]
pub struct WebViewOptions {
    pub url: String,
    pub backend: Backend,
    pub visible: bool,
    /// A stable identity for this web view, used by the Apple backend to key its
    /// platform-side overlay (`SystemBrowserId`). Callers should pass a value
    /// unique per live web view (e.g. derived from the owning widget's uid).
    /// Backends that own their native objects directly ignore this.
    pub id: u64,
}

impl Default for WebViewOptions {
    fn default() -> Self {
        Self {
            url: String::new(),
            backend: Backend::Native,
            visible: true,
            id: 0,
        }
    }
}

/// Errors surfaced when creating or driving a [`WebView`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebViewError {
    /// No backend is available for the requested `Backend` on this platform
    /// (or the required feature is disabled).
    Unsupported,
    /// The backend failed to create the underlying web view.
    Backend(String),
}

impl core::fmt::Display for WebViewError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WebViewError::Unsupported => {
                write!(f, "webview backend is not supported on this platform/feature")
            }
            WebViewError::Backend(msg) => write!(f, "webview backend error: {msg}"),
        }
    }
}

impl std::error::Error for WebViewError {}

pub type Result<T> = core::result::Result<T, WebViewError>;

/// Events pumped out of a [`WebView`] each frame.
#[derive(Clone, Debug)]
pub enum WebViewEvent {
    LoadStarted { url: String },
    LoadFinished { url: String },
    LoadFailed { url: String, error: String },
    /// Inbound structured message from page JS (`window.vs.postMessage`).
    Message(BridgeMessage),
}

/// A cross-platform web view + JS bridge handle.
pub struct WebView {
    inner: BackendImpl,
}

impl WebView {
    /// Create a web view. Returns [`WebViewError::Unsupported`] when no backend
    /// exists for the requested [`Backend`] on this platform.
    pub fn new(cx: &mut Cx, opts: WebViewOptions) -> Result<Self> {
        let inner = BackendImpl::new(cx, &opts)?;
        Ok(Self { inner })
    }

    pub fn set_url(&mut self, cx: &mut Cx, url: &str) {
        self.inner.set_url(cx, url);
    }

    /// Position/clip the overlay to `area`; `visible=false` hides/detaches it.
    pub fn update_rect(&mut self, cx: &mut Cx, area: Area, visible: bool) {
        self.inner.update_rect(cx, area, visible);
    }

    pub fn history_go(&mut self, cx: &mut Cx, delta: i32) {
        self.inner.history_go(cx, delta);
    }

    /// Rust → JS: inject and run `script` in the page context.
    pub fn eval_js(&mut self, cx: &mut Cx, script: &str) {
        self.inner.eval_js(cx, script);
    }

    /// Rust → JS: deliver a structured message to `window.vs.onMessage`
    /// listeners.
    pub fn post_to_js(&mut self, cx: &mut Cx, msg: &BridgeMessage) {
        let script = bridge::dispatch_script(msg);
        self.inner.eval_js(cx, &script);
    }

    /// Drain events since the last call (load state + inbound bridge messages).
    pub fn poll_events(&mut self) -> Vec<WebViewEvent> {
        self.inner.drain_events()
    }

    pub fn detach(&mut self, cx: &mut Cx) {
        self.inner.detach(cx);
    }
}

// --- Compile-time backend binding -----------------------------------------
//
// `BackendImpl` dispatches at runtime over the requested `Backend`, but each
// arm is bound at compile time to whatever the current target supports. The
// `Native` arm resolves to the platform overlay backend (or `unsupported`); the
// `Cef` arm resolves to the CEF backend under the `cef` feature (or
// `unsupported`).

use backend::WebViewBackend;

#[cfg(any(target_os = "macos", target_os = "ios"))]
type NativeBackend = backend::apple::AppleBackend;
#[cfg(target_os = "android")]
type NativeBackend = backend::android::AndroidBackend;
#[cfg(target_os = "windows")]
type NativeBackend = backend::windows::WindowsBackend;
#[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
type NativeBackend = backend::linux::LinuxBackend;
#[cfg(target_arch = "wasm32")]
type NativeBackend = backend::web::WebBackend;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "android",
    target_os = "windows",
    all(target_os = "linux", not(target_arch = "wasm32")),
    target_arch = "wasm32"
)))]
type NativeBackend = backend::unsupported::UnsupportedBackend;

#[cfg(feature = "cef")]
type CefBackend = backend::cef::CefBackend;
#[cfg(not(feature = "cef"))]
type CefBackend = backend::unsupported::UnsupportedBackend;

enum BackendImpl {
    Native(NativeBackend),
    Cef(CefBackend),
}

impl BackendImpl {
    fn new(cx: &mut Cx, opts: &WebViewOptions) -> Result<Self> {
        match opts.backend {
            Backend::Native => Ok(BackendImpl::Native(NativeBackend::new(cx, opts)?)),
            Backend::Cef => Ok(BackendImpl::Cef(CefBackend::new(cx, opts)?)),
        }
    }

    fn set_url(&mut self, cx: &mut Cx, url: &str) {
        match self {
            BackendImpl::Native(b) => b.set_url(cx, url),
            BackendImpl::Cef(b) => b.set_url(cx, url),
        }
    }
    fn update_rect(&mut self, cx: &mut Cx, area: Area, visible: bool) {
        match self {
            BackendImpl::Native(b) => b.update_rect(cx, area, visible),
            BackendImpl::Cef(b) => b.update_rect(cx, area, visible),
        }
    }
    fn history_go(&mut self, cx: &mut Cx, delta: i32) {
        match self {
            BackendImpl::Native(b) => b.history_go(cx, delta),
            BackendImpl::Cef(b) => b.history_go(cx, delta),
        }
    }
    fn eval_js(&mut self, cx: &mut Cx, script: &str) {
        match self {
            BackendImpl::Native(b) => b.eval_js(cx, script),
            BackendImpl::Cef(b) => b.eval_js(cx, script),
        }
    }
    fn drain_events(&mut self) -> Vec<WebViewEvent> {
        match self {
            BackendImpl::Native(b) => b.drain_events(),
            BackendImpl::Cef(b) => b.drain_events(),
        }
    }
    fn detach(&mut self, cx: &mut Cx) {
        match self {
            BackendImpl::Native(b) => b.detach(cx),
            BackendImpl::Cef(b) => b.detach(cx),
        }
    }
}
