//! Platform backend contract. Each platform implements [`WebViewBackend`]; the
//! crate root binds `Backend::Native` to the right one at compile time via
//! `#[cfg]`, and `Backend::Cef` to [`cef`] when the `cef` feature is on.

use crate::{Result, WebViewEvent, WebViewOptions};
use makepad_platform::{Area, Cx};

pub(crate) trait WebViewBackend {
    fn new(cx: &mut Cx, opts: &WebViewOptions) -> Result<Self>
    where
        Self: Sized;

    fn set_url(&mut self, cx: &mut Cx, url: &str);

    /// Position/clip the overlay to `area`; `visible=false` detaches/hides it.
    fn update_rect(&mut self, cx: &mut Cx, area: Area, visible: bool);

    fn history_go(&mut self, cx: &mut Cx, delta: i32);

    /// Rust → JS: evaluate an arbitrary script in the page context.
    fn eval_js(&mut self, cx: &mut Cx, script: &str);

    /// Drain events produced since the last call (load state + inbound bridge
    /// messages). Called once per frame by [`crate::WebView::poll_events`].
    fn drain_events(&mut self) -> Vec<WebViewEvent>;

    fn detach(&mut self, cx: &mut Cx);
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) mod apple;
#[cfg(target_os = "android")]
pub(crate) mod android;
#[cfg(target_os = "windows")]
pub(crate) mod windows;
#[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
pub(crate) mod linux;
#[cfg(target_arch = "wasm32")]
pub(crate) mod web;

#[cfg(feature = "cef")]
pub(crate) mod cef;

pub(crate) mod unsupported;
