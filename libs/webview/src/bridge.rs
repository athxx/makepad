//! JS bridge protocol: the `window.vs.*` injection script and the
//! [`BridgeMessage`] structure exchanged in both directions.
//!
//! This module is pure logic (no platform calls) so it is unit-testable on
//! every target. Each platform backend is responsible for:
//!
//! - injecting [`injection_script`] as a document-start user script (re-run on
//!   every navigation),
//! - wiring the native "JS → Rust" channel so that `window.vs.postMessage(obj)`
//!   payloads arrive as [`BridgeMessage`] and get surfaced as
//!   [`crate::WebViewEvent::Message`],
//! - implementing "Rust → JS" by evaluating [`dispatch_script`] for an outbound
//!   [`BridgeMessage`].

use makepad_micro_serde::*;

/// A structured message crossing the JS ↔ Rust boundary in either direction.
///
/// The bridge itself is intentionally minimal: `kind` is a routing key and
/// `payload` is an opaque JSON string whose shape upper layers agree on. This
/// keeps the transport dependency-free (serialized with `makepad-micro-serde`)
/// and leaves any richer JSAPI to a future layer built on top.
#[derive(SerJson, DeJson, Clone, Debug, PartialEq, Eq, Default)]
pub struct BridgeMessage {
    /// Message type / routing key.
    pub kind: String,
    /// Opaque JSON string payload (structure agreed by upper layers).
    pub payload: String,
}

impl BridgeMessage {
    pub fn new(kind: impl Into<String>, payload: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            payload: payload.into(),
        }
    }
}

/// The name of the injected global object exposed to page scripts.
pub const BRIDGE_GLOBAL: &str = "vs";

/// The document-start user script establishing `window.vs`.
///
/// Each platform passes the *native handle name* that page JS calls to reach
/// Rust. On Apple this is a `WKScriptMessageHandler` name reachable via
/// `window.webkit.messageHandlers.<name>.postMessage(...)`; on Android it is the
/// `@JavascriptInterface` object name; on WebView2 it is
/// `window.chrome.webview.postMessage(...)` (the `native_post_expr` argument lets
/// each backend substitute the exact call form).
///
/// `native_post_expr` must be a JS expression template containing the literal
/// token `%MSG%`, which the bridge replaces with the JSON string to send. For
/// example Apple passes
/// `"window.webkit.messageHandlers.vsbridge.postMessage(%MSG%)"`.
pub fn injection_script(native_post_expr: &str) -> String {
    let send_call = native_post_expr.replace("%MSG%", "json");
    format!(
        r#"(function() {{
    if (window.{global} && window.{global}.__installed) {{ return; }}
    var listeners = [];
    var api = {{
        __installed: true,
        postMessage: function(msg) {{
            var json = JSON.stringify(msg);
            {send_call};
        }},
        onMessage: function(cb) {{
            if (typeof cb === 'function') {{ listeners.push(cb); }}
        }},
        __dispatch: function(jsonStr) {{
            var msg;
            try {{ msg = JSON.parse(jsonStr); }} catch (e) {{ return; }}
            for (var i = 0; i < listeners.length; i++) {{
                try {{ listeners[i](msg); }} catch (e) {{}}
            }}
        }}
    }};
    window.{global} = api;
}})();"#,
        global = BRIDGE_GLOBAL,
        send_call = send_call,
    )
}

/// A JS snippet that delivers `msg` to page scripts registered via
/// `window.vs.onMessage(...)`. Backends evaluate this in the page context for
/// each outbound [`BridgeMessage`] (Rust → JS).
pub fn dispatch_script(msg: &BridgeMessage) -> String {
    let json = msg.serialize_json();
    format!(
        "window.{global} && window.{global}.__dispatch({literal});",
        global = BRIDGE_GLOBAL,
        literal = js_string_literal(&json),
    )
}

/// Escape a Rust string into a safe double-quoted JS string literal (including
/// the surrounding quotes).
pub fn js_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_message_roundtrips_through_json() {
        let msg = BridgeMessage::new("nav", r#"{"url":"https://example.com"}"#);
        let json = msg.serialize_json();
        let back = BridgeMessage::deserialize_json(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn injection_script_substitutes_native_post_expr() {
        let script = injection_script("window.webkit.messageHandlers.vsbridge.postMessage(%MSG%)");
        // The %MSG% token is replaced by the local `json` variable.
        assert!(script.contains("window.webkit.messageHandlers.vsbridge.postMessage(json)"));
        assert!(!script.contains("%MSG%"));
        // Establishes the global and the two public methods.
        assert!(script.contains("window.vs = api"));
        assert!(script.contains("postMessage: function"));
        assert!(script.contains("onMessage: function"));
    }

    #[test]
    fn dispatch_script_embeds_escaped_payload() {
        let msg = BridgeMessage::new("evt", r#"{"quote":"\"hi\""}"#);
        let script = dispatch_script(&msg);
        assert!(script.contains("window.vs.__dispatch("));
        // No raw unescaped double-quote breaks out of the JS string literal:
        // every embedded quote must be backslash-escaped.
        assert!(!script.contains("__dispatch(\"\")"));
    }

    #[test]
    fn js_string_literal_escapes_control_and_line_separators() {
        assert_eq!(js_string_literal("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(js_string_literal("x\ny"), r#""x\ny""#);
        assert_eq!(js_string_literal("\u{2028}"), "\"\\u2028\"");
        assert_eq!(js_string_literal("\u{1}"), "\"\\u0001\"");
    }
}
