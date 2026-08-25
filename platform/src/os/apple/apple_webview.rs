#[cfg(target_os = "macos")]
use crate::window::WindowId;
use crate::{
    makepad_math::Rect,
    os::apple::{
        apple_classes::get_apple_class_global,
        apple_sys::*,
        apple_util::{nsstring_to_string, str_to_nsstring},
    },
    thread::SignalToUI,
};
use makepad_objc_sys::{class, msg_send, sel};
use std::{
    collections::HashMap,
    sync::Mutex,
};

#[link(name = "WebKit", kind = "framework")]
unsafe extern "C" {}

// -- JS bridge event backchannel ------------------------------------------------
//
// WKWebView delegates run on the main thread but outside a `Cx` borrow, so we
// cannot hand events directly to the event loop. Instead each delegate pushes
// into this global queue keyed by `browser_id` (== `LiveId.0`) and wakes the UI
// via `SignalToUI`. The `makepad-webview` backend drains it through
// `drain_system_browser_events`.

#[derive(Clone, Debug)]
pub enum SystemBrowserEvent {
    LoadStarted { url: String },
    LoadFinished { url: String },
    LoadFailed { url: String, error: String },
    Message { json: String },
}

static WEBVIEW_EVENTS: Mutex<Option<HashMap<u64, Vec<SystemBrowserEvent>>>> = Mutex::new(None);

fn push_system_browser_event(browser_id: u64, event: SystemBrowserEvent) {
    if let Ok(mut guard) = WEBVIEW_EVENTS.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        map.entry(browser_id).or_default().push(event);
    }
    SignalToUI::set_ui_signal();
}

/// Drain and return all pending bridge events for a browser. Called by the
/// `makepad-webview` Apple backend from the UI thread.
pub fn drain_system_browser_events(browser_id: u64) -> Vec<SystemBrowserEvent> {
    if let Ok(mut guard) = WEBVIEW_EVENTS.lock() {
        if let Some(map) = guard.as_mut() {
            if let Some(events) = map.remove(&browser_id) {
                return events;
            }
        }
    }
    Vec::new()
}

// -- JS bridge injection --------------------------------------------------------

const BRIDGE_HANDLER_NAME: &str = "vsbridge";

/// The document-start script that establishes `window.vs`. JS calls
/// `window.vs.postMessage(json)` which forwards to the native handler; native
/// code calls `window.vs.__dispatch(json)` to deliver a message to JS listeners.
fn bridge_injection_script() -> String {
    let send_call = "window.webkit.messageHandlers.vsbridge.postMessage(json)";
    format!(
        "(function(){{\
            if (window.vs && window.vs.__installed) return;\
            var listeners = [];\
            window.vs = {{\
                __installed: true,\
                postMessage: function(msg){{\
                    var json = (typeof msg === 'string') ? msg : JSON.stringify(msg);\
                    {send_call};\
                }},\
                onMessage: function(cb){{ listeners.push(cb); }},\
                __dispatch: function(json){{\
                    var msg; try {{ msg = JSON.parse(json); }} catch(e) {{ msg = json; }}\
                    for (var i=0;i<listeners.length;i++){{ try {{ listeners[i](msg); }} catch(e){{}} }}\
                }}\
            }};\
        }})();"
    )
}

/// Install the `window.vs` bridge onto a fresh `WKWebViewConfiguration`, wiring
/// the JS→Rust message handler and the navigation delegate. Returns the retained
/// `(script_handler, nav_delegate)` — WebKit holds them unowned, so the caller
/// must keep them alive and release them in `cleanup`.
unsafe fn install_bridge(config: ObjcId, browser_id: u64) -> (ObjcId, ObjcId) {
    let classes = get_apple_class_global();

    let controller: ObjcId = msg_send![config, userContentController];

    // document-start user script -> window.vs
    let source = str_to_nsstring(&bridge_injection_script());
    let user_script: ObjcId = msg_send![class!(WKUserScript), alloc];
    // injectionTime = WKUserScriptInjectionTimeAtDocumentStart (0), forMainFrameOnly = NO
    let user_script: ObjcId = msg_send![
        user_script,
        initWithSource: source
        injectionTime: 0u64
        forMainFrameOnly: NO
    ];
    let () = msg_send![controller, addUserScript: user_script];

    // JS -> Rust handler
    let handler: ObjcId = msg_send![classes.webview_script_message_handler, alloc];
    let handler: ObjcId = msg_send![handler, init];
    (*handler).set_ivar("browser_id", browser_id);
    let name = str_to_nsstring(BRIDGE_HANDLER_NAME);
    let () = msg_send![controller, addScriptMessageHandler: handler name: name];

    // navigation (load) events
    let nav_delegate: ObjcId = msg_send![classes.webview_navigation_delegate, alloc];
    let nav_delegate: ObjcId = msg_send![nav_delegate, init];
    (*nav_delegate).set_ivar("browser_id", browser_id);

    (handler, nav_delegate)
}

unsafe fn delegate_browser_id(this: &Object) -> u64 {
    *this.get_ivar::<u64>("browser_id")
}

unsafe fn web_view_url_string(web_view: ObjcId) -> String {
    let url: ObjcId = msg_send![web_view, URL];
    if url == nil {
        return String::new();
    }
    let abs: ObjcId = msg_send![url, absoluteString];
    if abs == nil {
        String::new()
    } else {
        nsstring_to_string(abs)
    }
}

unsafe fn ns_error_description(error: ObjcId) -> String {
    if error == nil {
        return String::new();
    }
    let desc: ObjcId = msg_send![error, localizedDescription];
    if desc == nil {
        String::new()
    } else {
        nsstring_to_string(desc)
    }
}

unsafe fn evaluate_js(web_view: ObjcId, script: &str) {
    if web_view == nil {
        return;
    }
    let js = str_to_nsstring(script);
    let () = msg_send![web_view, evaluateJavaScript: js completionHandler: nil];
}

// -- WKScriptMessageHandler (JS -> Rust) ---------------------------------------

pub fn define_wk_script_message_handler() -> *const Class {
    extern "C" fn did_receive_script_message(
        this: &Object,
        _: Sel,
        _controller: ObjcId,
        message: ObjcId,
    ) {
        unsafe {
            let browser_id = delegate_browser_id(this);
            let body: ObjcId = msg_send![message, body];
            if body == nil {
                return;
            }
            let is_string: BOOL = msg_send![body, isKindOfClass: class!(NSString)];
            if is_string != YES {
                return;
            }
            let json = nsstring_to_string(body);
            push_system_browser_event(browser_id, SystemBrowserEvent::Message { json });
        }
    }

    let superclass = class!(NSObject);
    let mut decl = ClassDecl::new("MakepadWKScriptMessageHandler", superclass).unwrap();
    unsafe {
        decl.add_method(
            sel!(userContentController: didReceiveScriptMessage:),
            did_receive_script_message as extern "C" fn(&Object, Sel, ObjcId, ObjcId),
        );
        if let Some(protocol) = Protocol::get("WKScriptMessageHandler") {
            decl.add_protocol(protocol);
        }
    }
    decl.add_ivar::<u64>("browser_id");
    decl.register()
}

// -- WKNavigationDelegate (load events) ----------------------------------------

pub fn define_wk_navigation_delegate() -> *const Class {
    extern "C" fn did_start(this: &Object, _: Sel, web_view: ObjcId, _nav: ObjcId) {
        unsafe {
            let browser_id = delegate_browser_id(this);
            let url = web_view_url_string(web_view);
            push_system_browser_event(browser_id, SystemBrowserEvent::LoadStarted { url });
        }
    }

    extern "C" fn did_finish(this: &Object, _: Sel, web_view: ObjcId, _nav: ObjcId) {
        unsafe {
            let browser_id = delegate_browser_id(this);
            let url = web_view_url_string(web_view);
            push_system_browser_event(browser_id, SystemBrowserEvent::LoadFinished { url });
        }
    }

    extern "C" fn did_fail(this: &Object, _: Sel, web_view: ObjcId, _nav: ObjcId, error: ObjcId) {
        unsafe {
            let browser_id = delegate_browser_id(this);
            let url = web_view_url_string(web_view);
            let error = ns_error_description(error);
            push_system_browser_event(browser_id, SystemBrowserEvent::LoadFailed { url, error });
        }
    }

    extern "C" fn did_fail_provisional(
        this: &Object,
        _: Sel,
        web_view: ObjcId,
        _nav: ObjcId,
        error: ObjcId,
    ) {
        unsafe {
            let browser_id = delegate_browser_id(this);
            let url = web_view_url_string(web_view);
            let error = ns_error_description(error);
            push_system_browser_event(browser_id, SystemBrowserEvent::LoadFailed { url, error });
        }
    }

    let superclass = class!(NSObject);
    let mut decl = ClassDecl::new("MakepadWKNavigationDelegate", superclass).unwrap();
    unsafe {
        decl.add_method(
            sel!(webView: didStartProvisionalNavigation:),
            did_start as extern "C" fn(&Object, Sel, ObjcId, ObjcId),
        );
        decl.add_method(
            sel!(webView: didFinishNavigation:),
            did_finish as extern "C" fn(&Object, Sel, ObjcId, ObjcId),
        );
        decl.add_method(
            sel!(webView: didFailNavigation: withError:),
            did_fail as extern "C" fn(&Object, Sel, ObjcId, ObjcId, ObjcId),
        );
        decl.add_method(
            sel!(webView: didFailProvisionalNavigation: withError:),
            did_fail_provisional as extern "C" fn(&Object, Sel, ObjcId, ObjcId, ObjcId),
        );
        if let Some(protocol) = Protocol::get("WKNavigationDelegate") {
            decl.add_protocol(protocol);
        }
    }
    decl.add_ivar::<u64>("browser_id");
    decl.register()
}

fn make_request(url: &str) -> Option<ObjcId> {
    unsafe {
        let url_string = str_to_nsstring(url);
        if url_string == nil {
            return None;
        }
        let ns_url: ObjcId = msg_send![class!(NSURL), URLWithString: url_string];
        if ns_url == nil {
            return None;
        }
        let request: ObjcId = msg_send![class!(NSURLRequest), requestWithURL: ns_url];
        if request == nil {
            None
        } else {
            Some(request)
        }
    }
}

fn history_go(web_view: ObjcId, delta: i32) {
    if web_view == nil || delta == 0 {
        return;
    }
    unsafe {
        for _ in 0..delta.unsigned_abs() {
            if delta < 0 {
                let can_go_back: BOOL = msg_send![web_view, canGoBack];
                if can_go_back == YES {
                    let () = msg_send![web_view, goBack];
                }
            } else {
                let can_go_forward: BOOL = msg_send![web_view, canGoForward];
                if can_go_forward == YES {
                    let () = msg_send![web_view, goForward];
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct MacosSystemBrowser {
    browser_id: u64,
    current_url: String,
    attached_window: Option<WindowId>,
    host_view: ObjcId,
    web_view: ObjcId,
    script_handler: ObjcId,
    nav_delegate: ObjcId,
}

#[cfg(target_os = "macos")]
impl MacosSystemBrowser {
    pub(crate) fn new(browser_id: u64, url: &str) -> Self {
        let mut browser = Self {
            browser_id,
            current_url: String::new(),
            attached_window: None,
            host_view: nil,
            web_view: nil,
            script_handler: nil,
            nav_delegate: nil,
        };
        browser.ensure_web_view();
        browser.set_url(url, false);
        browser
    }

    fn ensure_web_view(&mut self) {
        if self.web_view != nil {
            return;
        }
        unsafe {
            let config: ObjcId = msg_send![class!(WKWebViewConfiguration), new];
            let (handler, nav_delegate) = install_bridge(config, self.browser_id);
            let web_view: ObjcId = msg_send![class!(WKWebView), alloc];
            let web_view: ObjcId = msg_send![web_view, initWithFrame: NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize { width: 1.0, height: 1.0 }
            } configuration: config];
            if web_view != nil {
                let () = msg_send![web_view, setNavigationDelegate: nav_delegate];
                let () = msg_send![web_view, setHidden: YES];
                self.web_view = web_view;
                self.script_handler = handler;
                self.nav_delegate = nav_delegate;
            }
        }
    }

    fn ensure_host_view(&mut self) {
        if self.host_view != nil {
            return;
        }
        unsafe {
            let host_view: ObjcId = msg_send![class!(NSView), alloc];
            let host_view: ObjcId = msg_send![host_view, initWithFrame: NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize { width: 1.0, height: 1.0 }
            }];
            if host_view != nil {
                let () = msg_send![host_view, setWantsLayer: YES];
                let layer: ObjcId = msg_send![host_view, layer];
                if layer != nil {
                    let () = msg_send![layer, setMasksToBounds: YES];
                }
                let () = msg_send![host_view, setHidden: YES];
                self.host_view = host_view;
            }
        }
    }

    fn ensure_attached(&mut self, window_id: WindowId, parent_view: ObjcId) {
        self.ensure_web_view();
        self.ensure_host_view();
        if self.web_view == nil || self.host_view == nil {
            return;
        }
        unsafe {
            let host_super_view: ObjcId = msg_send![self.host_view, superview];
            if self.attached_window != Some(window_id) || host_super_view != parent_view {
                let () = msg_send![self.host_view, removeFromSuperview];
                let () = msg_send![parent_view, addSubview: self.host_view];
                self.attached_window = Some(window_id);
            }

            let web_super_view: ObjcId = msg_send![self.web_view, superview];
            if web_super_view != self.host_view {
                let () = msg_send![self.web_view, removeFromSuperview];
                let () = msg_send![self.host_view, addSubview: self.web_view];
            }
        }
    }

    pub(crate) fn update(
        &mut self,
        window_id: WindowId,
        parent_view: ObjcId,
        unclipped_rect: Rect,
        clipped_rect: Rect,
        visible: bool,
    ) {
        self.ensure_attached(window_id, parent_view);
        unsafe {
            if self.host_view == nil || self.web_view == nil {
                return;
            }
            let (host_rect, web_view_rect, is_visible) =
                clipped_browser_layout(unclipped_rect, clipped_rect, visible);
            let host_frame = NSRect {
                origin: NSPoint {
                    x: host_rect.pos.x,
                    y: host_rect.pos.y,
                },
                size: NSSize {
                    width: host_rect.size.x.max(0.0),
                    height: host_rect.size.y.max(0.0),
                },
            };
            let web_view_frame = NSRect {
                origin: NSPoint {
                    x: web_view_rect.pos.x,
                    y: web_view_rect.pos.y,
                },
                size: NSSize {
                    width: web_view_rect.size.x.max(0.0),
                    height: web_view_rect.size.y.max(0.0),
                },
            };
            let () = msg_send![self.host_view, setFrame: host_frame];
            let () = msg_send![self.web_view, setFrame: web_view_frame];
            let () = msg_send![self.web_view, setHidden: if is_visible { NO } else { YES }];
            let () = msg_send![self.host_view, setHidden: if is_visible { NO } else { YES }];
        }
    }

    pub(crate) fn detach(&mut self) {
        unsafe {
            if self.web_view != nil {
                let () = msg_send![self.web_view, removeFromSuperview];
                let () = msg_send![self.web_view, setHidden: YES];
            }
            if self.host_view != nil {
                let () = msg_send![self.host_view, removeFromSuperview];
                let () = msg_send![self.host_view, setHidden: YES];
            }
        }
        self.attached_window = None;
    }

    pub(crate) fn set_url(&mut self, url: &str, _replace: bool) {
        if self.current_url == url {
            return;
        }
        self.ensure_web_view();
        let Some(request) = make_request(url) else {
            return;
        };
        unsafe {
            if self.web_view != nil {
                let () = msg_send![self.web_view, loadRequest: request];
                self.current_url.clear();
                self.current_url.push_str(url);
            }
        }
    }

    pub(crate) fn history_go(&mut self, delta: i32) {
        history_go(self.web_view, delta);
    }

    pub(crate) fn eval_js(&mut self, script: &str) {
        unsafe {
            evaluate_js(self.web_view, script);
        }
    }

    pub(crate) fn cleanup(&mut self) {
        unsafe {
            if self.web_view != nil {
                let () = msg_send![self.web_view, stopLoading];
                let () = msg_send![self.web_view, setNavigationDelegate: nil];
                let config: ObjcId = msg_send![self.web_view, configuration];
                if config != nil {
                    let controller: ObjcId = msg_send![config, userContentController];
                    if controller != nil {
                        let name = str_to_nsstring(BRIDGE_HANDLER_NAME);
                        let () = msg_send![controller, removeScriptMessageHandlerForName: name];
                    }
                }
            }
            if self.script_handler != nil {
                let () = msg_send![self.script_handler, release];
                self.script_handler = nil;
            }
            if self.nav_delegate != nil {
                let () = msg_send![self.nav_delegate, release];
                self.nav_delegate = nil;
            }
        }
        drain_system_browser_events(self.browser_id);
        self.detach();
    }
}

#[cfg(target_os = "macos")]
fn clipped_browser_layout(
    unclipped_rect: Rect,
    clipped_rect: Rect,
    visible: bool,
) -> (Rect, Rect, bool) {
    let web_view_rect = Rect {
        pos: unclipped_rect.pos - clipped_rect.pos,
        size: unclipped_rect.size,
    };
    let is_visible = visible && clipped_rect.size.x > 0.0 && clipped_rect.size.y > 0.0;
    (clipped_rect, web_view_rect, is_visible)
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::clipped_browser_layout;
    use crate::makepad_math::{dvec2, Rect};

    #[test]
    fn keeps_native_browser_anchored_when_top_is_clipped() {
        let unclipped_rect = Rect {
            pos: dvec2(26.0, 262.0),
            size: dvec2(298.0, 420.0),
        };
        let clipped_rect = Rect {
            pos: dvec2(26.0, 262.0),
            size: dvec2(298.0, 319.0),
        };

        let (host_rect, web_view_rect, is_visible) =
            clipped_browser_layout(unclipped_rect, clipped_rect, true);

        assert_eq!(host_rect, clipped_rect);
        assert_eq!(web_view_rect.pos, dvec2(0.0, 0.0));
        assert_eq!(web_view_rect.size, unclipped_rect.size);
        assert!(is_visible);
    }

    #[test]
    fn offsets_native_browser_inside_clip_when_bottom_is_clipped() {
        let unclipped_rect = Rect {
            pos: dvec2(26.0, 120.0),
            size: dvec2(298.0, 420.0),
        };
        let clipped_rect = Rect {
            pos: dvec2(26.0, 170.0),
            size: dvec2(298.0, 370.0),
        };

        let (_, web_view_rect, _) = clipped_browser_layout(unclipped_rect, clipped_rect, true);

        assert_eq!(web_view_rect.pos, dvec2(0.0, -50.0));
    }

    #[test]
    fn hides_when_clip_is_empty() {
        let unclipped_rect = Rect {
            pos: dvec2(26.0, 262.0),
            size: dvec2(298.0, 420.0),
        };
        let clipped_rect = Rect {
            pos: dvec2(26.0, 262.0),
            size: dvec2(0.0, 0.0),
        };

        let (_, _, is_visible) = clipped_browser_layout(unclipped_rect, clipped_rect, true);

        assert!(!is_visible);
    }
}

#[cfg(target_os = "ios")]
pub(crate) struct IosSystemBrowser {
    browser_id: u64,
    current_url: String,
    web_view: ObjcId,
    script_handler: ObjcId,
    nav_delegate: ObjcId,
}

#[cfg(target_os = "ios")]
impl IosSystemBrowser {
    pub(crate) fn new(browser_id: u64, url: &str) -> Self {
        let mut browser = Self {
            browser_id,
            current_url: String::new(),
            web_view: nil,
            script_handler: nil,
            nav_delegate: nil,
        };
        browser.ensure_web_view();
        browser.set_url(url, false);
        browser
    }

    fn ensure_web_view(&mut self) {
        if self.web_view != nil {
            return;
        }
        unsafe {
            let config: ObjcId = msg_send![class!(WKWebViewConfiguration), new];
            let (handler, nav_delegate) = install_bridge(config, self.browser_id);
            let web_view: ObjcId = msg_send![class!(WKWebView), alloc];
            let web_view: ObjcId = msg_send![web_view, initWithFrame: NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize { width: 1.0, height: 1.0 }
            } configuration: config];
            if web_view != nil {
                let () = msg_send![web_view, setNavigationDelegate: nav_delegate];
                let () = msg_send![web_view, setHidden: YES];
                self.web_view = web_view;
                self.script_handler = handler;
                self.nav_delegate = nav_delegate;
            }
        }
    }

    pub(crate) fn update(&mut self, parent_view: ObjcId, rect: Rect, visible: bool) {
        self.ensure_web_view();
        unsafe {
            if self.web_view == nil {
                return;
            }
            let super_view: ObjcId = msg_send![self.web_view, superview];
            if super_view != parent_view {
                let () = msg_send![self.web_view, removeFromSuperview];
                let () = msg_send![parent_view, addSubview: self.web_view];
            }
            let () = msg_send![parent_view, bringSubviewToFront: self.web_view];
            let frame = NSRect {
                origin: NSPoint {
                    x: rect.pos.x,
                    y: rect.pos.y,
                },
                size: NSSize {
                    width: rect.size.x.max(0.0),
                    height: rect.size.y.max(0.0),
                },
            };
            let () = msg_send![self.web_view, setFrame: frame];
            let () = msg_send![self.web_view, setHidden: if visible { NO } else { YES }];
        }
    }

    pub(crate) fn detach(&mut self) {
        unsafe {
            if self.web_view != nil {
                let () = msg_send![self.web_view, removeFromSuperview];
                let () = msg_send![self.web_view, setHidden: YES];
            }
        }
    }

    pub(crate) fn set_url(&mut self, url: &str, _replace: bool) {
        if self.current_url == url {
            return;
        }
        self.ensure_web_view();
        let Some(request) = make_request(url) else {
            return;
        };
        unsafe {
            if self.web_view != nil {
                let () = msg_send![self.web_view, loadRequest: request];
                self.current_url.clear();
                self.current_url.push_str(url);
            }
        }
    }

    pub(crate) fn history_go(&mut self, delta: i32) {
        history_go(self.web_view, delta);
    }

    pub(crate) fn eval_js(&mut self, script: &str) {
        unsafe {
            evaluate_js(self.web_view, script);
        }
    }

    pub(crate) fn cleanup(&mut self) {
        unsafe {
            if self.web_view != nil {
                let () = msg_send![self.web_view, stopLoading];
                let () = msg_send![self.web_view, setNavigationDelegate: nil];
                let config: ObjcId = msg_send![self.web_view, configuration];
                if config != nil {
                    let controller: ObjcId = msg_send![config, userContentController];
                    if controller != nil {
                        let name = str_to_nsstring(BRIDGE_HANDLER_NAME);
                        let () = msg_send![controller, removeScriptMessageHandlerForName: name];
                    }
                }
            }
            if self.script_handler != nil {
                let () = msg_send![self.script_handler, release];
                self.script_handler = nil;
            }
            if self.nav_delegate != nil {
                let () = msg_send![self.nav_delegate, release];
                self.nav_delegate = nil;
            }
        }
        drain_system_browser_events(self.browser_id);
        self.detach();
    }
}
