//! CEF backend (feature = "cef").
//!
//! CEF renders the page to an offscreen texture rather than as a native
//! overlay, so unlike the other backends it needs the widget to feed it a size,
//! DPI scale, input events, and to blit its frames. Wiring all of that (plus the
//! JS bridge via `on_process_message_received` / process messages) is P4 in the
//! rollout; until then this backend compiles under the `cef` feature but reports
//! [`WebViewError::Unsupported`] so `Backend::Cef` is honestly wired end to end.

use crate::backend::WebViewBackend;
use crate::{Result, WebViewError, WebViewEvent, WebViewOptions};
use makepad_platform::{Area, Cx};

pub(crate) struct CefBackend;

impl WebViewBackend for CefBackend {
    fn new(_cx: &mut Cx, _opts: &WebViewOptions) -> Result<Self> {
        // TODO(P4): wrap makepad-cef (render-to-texture + input + JS bridge).
        Err(WebViewError::Unsupported)
    }
    fn set_url(&mut self, _cx: &mut Cx, _url: &str) {}
    fn update_rect(&mut self, _cx: &mut Cx, _area: Area, _visible: bool) {}
    fn history_go(&mut self, _cx: &mut Cx, _delta: i32) {}
    fn eval_js(&mut self, _cx: &mut Cx, _script: &str) {}
    fn drain_events(&mut self) -> Vec<WebViewEvent> {
        Vec::new()
    }
    fn detach(&mut self, _cx: &mut Cx) {}
}
