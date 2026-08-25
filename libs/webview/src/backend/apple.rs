//! Apple (macOS/iOS) backend.
//!
//! The actual WKWebView overlay lives in `makepad-platform`
//! (`os::apple::apple_webview`). Its `parent_view` can only be resolved on the
//! platform event loop's main thread, so this backend does **not** own any
//! native object — it drives the platform overlay entirely through `CxOsOp`s
//! pushed via [`Cx::system_browser`].
//!
//! P1 reproduces the pre-existing `Browser` widget behavior exactly (spawn /
//! set_url / update / detach / history). The JS bridge and load events (P2) are
//! added via new platform `CxOsOp`s and an event backchannel.

use crate::backend::WebViewBackend;
use crate::bridge::BridgeMessage;
use crate::{Result, WebViewEvent, WebViewOptions};
use makepad_micro_serde::DeJson;
use makepad_platform::os::apple_webview::{drain_system_browser_events, SystemBrowserEvent};
use makepad_platform::{Area, Cx, SystemBrowserId};

pub(crate) struct AppleBackend {
    id: SystemBrowserId,
    /// Raw id used to key the platform backchannel (matches `SystemBrowserId.0`).
    raw_id: u64,
    spawned: bool,
    current_url: String,
}

impl AppleBackend {
    fn browser_id(&self) -> SystemBrowserId {
        self.id
    }
}

impl WebViewBackend for AppleBackend {
    fn new(cx: &mut Cx, opts: &WebViewOptions) -> Result<Self> {
        let id = SystemBrowserId(makepad_live_id::LiveId(opts.id));
        let mut backend = Self {
            id,
            raw_id: opts.id,
            spawned: false,
            current_url: String::new(),
        };
        // Match the widget's prior semantics: spawn immediately with the url.
        cx.system_browser(backend.browser_id()).spawn(&opts.url);
        backend.spawned = true;
        backend.current_url = opts.url.clone();
        Ok(backend)
    }

    fn set_url(&mut self, cx: &mut Cx, url: &str) {
        if !self.spawned {
            cx.system_browser(self.browser_id()).spawn(url);
            self.spawned = true;
            self.current_url = url.to_string();
            return;
        }
        if self.current_url != url {
            cx.system_browser(self.browser_id()).set_url(url, false);
            self.current_url = url.to_string();
        }
    }

    fn update_rect(&mut self, cx: &mut Cx, area: Area, visible: bool) {
        if !self.spawned {
            return;
        }
        if visible && area.is_valid(cx) {
            cx.system_browser(self.browser_id()).update(area, true);
        } else {
            cx.system_browser(self.browser_id()).detach();
        }
    }

    fn history_go(&mut self, cx: &mut Cx, delta: i32) {
        if self.spawned {
            cx.system_browser(self.browser_id()).history_go(delta);
        }
    }

    fn eval_js(&mut self, cx: &mut Cx, script: &str) {
        if self.spawned {
            cx.system_browser(self.browser_id()).eval_js(script);
        }
    }

    fn drain_events(&mut self) -> Vec<WebViewEvent> {
        drain_system_browser_events(self.raw_id)
            .into_iter()
            .map(|event| match event {
                SystemBrowserEvent::LoadStarted { url } => WebViewEvent::LoadStarted { url },
                SystemBrowserEvent::LoadFinished { url } => WebViewEvent::LoadFinished { url },
                SystemBrowserEvent::LoadFailed { url, error } => {
                    WebViewEvent::LoadFailed { url, error }
                }
                SystemBrowserEvent::Message { json } => {
                    // The page posts an arbitrary JS object; upper layers agree
                    // on the {kind,payload} shape. Fall back to wrapping the raw
                    // JSON as the payload when it doesn't match.
                    let msg = BridgeMessage::deserialize_json(&json)
                        .unwrap_or_else(|_| BridgeMessage::new("raw", json));
                    WebViewEvent::Message(msg)
                }
            })
            .collect()
    }

    fn detach(&mut self, cx: &mut Cx) {
        if self.spawned {
            cx.system_browser(self.browser_id()).close();
            self.spawned = false;
        }
    }
}
