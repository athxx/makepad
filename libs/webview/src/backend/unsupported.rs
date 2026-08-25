//! Fallback backend for platforms/features with no implementation. Construction
//! returns [`WebViewError::Unsupported`]; every other method is a no-op so that
//! callers can hold a `WebView` uniformly without target-specific branching.

use crate::backend::WebViewBackend;
use crate::{Result, WebViewError, WebViewEvent, WebViewOptions};
use makepad_platform::{Area, Cx};

// Used as the fallback backend only on some target/feature combinations; on
// others (e.g. macOS + cef) both real backends exist and this is unreferenced.
#[allow(dead_code)]
pub(crate) struct UnsupportedBackend;

impl WebViewBackend for UnsupportedBackend {
    fn new(_cx: &mut Cx, _opts: &WebViewOptions) -> Result<Self> {
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
