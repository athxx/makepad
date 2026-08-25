# makepad-webview

Unified cross-platform embedded web view + bidirectional JS bridge for Makepad / viso.

One API, backed by each platform's native web view:

| Platform | Native backend |
|----------|----------------|
| macOS / iOS | `WKWebView` |
| Windows | WebView2 |
| Linux | WebKitGTK |
| Android | `android.webkit.WebView` |
| Web (wasm) | `<iframe>` overlay |

An optional **CEF** backend (Chromium, rendered to a texture) is available behind the `cef` feature; the default `Native` backend needs no extra dependencies.

There are two ways to use it:

- **The `Browser` widget** (in `makepad-widgets`) — declare it in script, done. This is what most apps want.
- **The `WebView` handle** (this crate) — the low-level Rust API the widget is built on, for when you need to drive a web view yourself.

---

## 1. The `Browser` widget (recommended)

Declare a `Browser` in your script like any other widget. The Native backend maps to the system web view above; on macOS/iOS it is a real `WKWebView` overlaid on the Makepad surface.

```rust
use makepad_widgets::*;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    // ...
    browser := Browser{
        width: Fill
        height: Fill
        url: "https://example.com"
        // backend defaults to Native; set BrowserBackend.CEF to force CEF.
    }
}
```

Toggle it from Rust:

```rust
self.ui.browser(cx, ids!(browser)).set_visible(cx, true);  // show
self.ui.browser(cx, ids!(browser)).set_visible(cx, false); // hide
```

A full runnable example (button → open a page inside the app → back) lives at
[`examples/webview`](../../examples/webview).

### Load & dispose policy

By default a web view is created lazily the first time it becomes visible, and
kept in memory once created. That first open pays a **cold start** (the web
content process spins up + the page fetches over the network) — instant on a
desktop, but a visible ~1–2s white screen in the iOS simulator. Two policies
let you trade first-open speed against memory:

#### `load` — when the web view is created and first loaded

| Value | Behaviour |
|-------|-----------|
| `BrowserLoad.Lazy` *(default)* | Create + load the first time the widget becomes visible. Fastest startup, lowest idle memory, slowest first open. |
| `BrowserLoad.Eager` | Create + load immediately on app startup. Fastest first open, but holds a web content process and issues the request up front. |
| `BrowserLoad.Deferred` | Warm up `load_delay_ms` milliseconds after startup, in the background while still hidden. Balances startup cost against first-open latency. |

`load_delay_ms` (a `u64`) is read **only** when `load` is `Deferred`.

#### `dispose` — what happens when the widget is hidden

| Value | Behaviour |
|-------|-----------|
| `BrowserDispose.Keep` | Keep the web view alive when hidden. Next show is instant; memory stays resident until the app exits. |
| `BrowserDispose.Deferred` *(default)* | Destroy the web view if it stays hidden for `dispose_delay_ms` milliseconds. Re-showing within that window is instant and cancels the pending destruction. |
| `BrowserDispose.Immediate` | Destroy the web view as soon as it is hidden. Frees memory eagerly; the next show pays a full cold start. |

`dispose_delay_ms` (a `u64`, **default `30000`** = 30s) is read **only** when
`dispose` is `Deferred`.

> Time values are milliseconds. Destroying a `WKWebView` frees tens of MB but
> means the next open is a cold start again; keeping it alive costs that memory
> until the app exits. `Deferred` is the middle ground.

#### Examples

```rust
// Fastest possible open, memory-be-damned:
browser := Browser{
    url: "https://example.com"
    load: BrowserLoad.Eager
    dispose: BrowserDispose.Keep
}

// Warm up 1.5s after launch, drop it 30s after the user leaves:
browser := Browser{
    url: "https://example.com"
    load: BrowserLoad.Deferred
    load_delay_ms: 1500
    dispose: BrowserDispose.Deferred
    dispose_delay_ms: 30000
}

// Minimal memory, accept a cold start every time:
browser := Browser{
    url: "https://example.com"
    load: BrowserLoad.Lazy
    dispose: BrowserDispose.Immediate
}
```

### Opening a URL: in-app vs. the system browser

The `Browser` widget shows a page **inside your app**. It is a completely
different thing from [`viso-open`](../../../viso-ext/viso-open), which hands a
URI to the **operating system's default handler** (`NSWorkspace` / `UIApplication`
/ `Intent` / `xdg-open` / `start`) — that leaves your app and opens Safari,
the dialer, the mail client, etc.

| | `Browser` widget (this crate) | `viso-open` |
|---|---|---|
| Where the page shows | Inside your app window (a web view overlay) | The system browser (leaves your app) |
| Back to your app | Built-in — the page never leaves the app | User switches back manually |
| `window.vs` JS bridge | Yes | No |
| Good for | Embedded pages (like an in-app browser) | `tel:`, `mailto:`, deep links, external links |

To open a page **inside the app**, drive the `Browser` widget directly — set its
URL, then show it:

```rust
self.ui.browser(cx, ids!(browser)).set_url(cx, "https://example.com");
self.ui.browser(cx, ids!(browser)).set_visible(cx, true);
```

**Routing by scheme.** A single "open this URI" action often wants both: show
`http(s)` pages in-app, but let the OS handle `tel:` / `mailto:` / deep links.
There is no automatic bridge between the two — you fan out in your own click
handler:

```rust
fn open_uri(&mut self, cx: &mut Cx, uri: &str) {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        // Web page → show it inside the app.
        self.ui.browser(cx, ids!(browser)).set_url(cx, uri);
        self.ui.browser(cx, ids!(browser)).set_visible(cx, true);
        self.ui.redraw(cx);
    } else {
        // tel:/mailto:/deep link → hand off to the OS.
        let _ = viso_open::Uri::new(uri).open();
    }
}
```

`viso-open` must be called on the main UI thread; see its docs for `tel:` /
`mailto:` scheme handling and the `open_with_completion` variant.

### Reacting to page events

The widget emits `BrowserAction`s (mirroring `WebViewEvent`): `LoadStarted`,
`LoadFinished`, `LoadFailed`, and `Message` (a structured message from page JS
via the bridge). Handle them like any widget action.

### Backend availability

The `Native` backend is currently implemented for macOS and iOS. On other
platforms without the `cef` feature the widget renders an "unsupported backend"
message rather than a web view. Build with `--features cef` to use the CEF
backend where Native is unavailable.

---

## 2. The `WebView` handle (low-level)

If you are building your own widget or need direct control, use the `WebView`
type from this crate:

```rust
use makepad_webview::{WebView, WebViewOptions, Backend, WebViewEvent, BridgeMessage};

// Create (returns WebViewError::Unsupported if no backend exists here):
let mut webview = WebView::new(cx, WebViewOptions {
    url: "https://example.com".into(),
    backend: Backend::Native,
    visible: true,
    // Stable per-live-web-view id; the Apple backend keys its overlay on it.
    // Derive it from your owning widget's uid.
    id: my_widget_uid,
})?;

// Position/clip the overlay to a Makepad Area each draw; visible=false hides it:
webview.update_rect(cx, area, visible);

// Navigation:
webview.set_url(cx, "https://other.example");
webview.history_go(cx, -1); // back; +1 = forward

// Pump events once per frame (load lifecycle + inbound bridge messages):
for event in webview.poll_events() {
    match event {
        WebViewEvent::LoadStarted { url } => { /* ... */ }
        WebViewEvent::LoadFinished { url } => { /* ... */ }
        WebViewEvent::LoadFailed { url, error } => { /* ... */ }
        WebViewEvent::Message(msg) => { /* from page JS */ }
    }
}

// Tear down the native web view (frees its resources):
webview.detach(cx);
```

`WebViewOptions` implements `Default` (empty URL, `Backend::Native`, visible,
`id: 0`).

### The JS bridge (`window.vs`)

The bridge is bidirectional and dependency-free. Each backend injects a
document-start script exposing a `window.vs` global to page scripts.

**JS → Rust** — page code calls:

```js
window.vs.postMessage({ kind: "greeting", payload: "hello from the page" });
```

which arrives on the Rust side as `WebViewEvent::Message(BridgeMessage { kind, payload })`.

**Rust → JS** — send a structured message to `window.vs` listeners:

```rust
webview.post_to_js(cx, &BridgeMessage::new("ping", r#"{"n":1}"#));
// or run arbitrary script:
webview.eval_js(cx, "document.body.style.background = 'black'");
```

`BridgeMessage` is intentionally minimal: `kind` is a routing key and `payload`
is an opaque JSON string whose shape the two sides agree on. It is serialized
with `makepad-micro-serde`.

---

## Features

| Feature | Effect |
|---------|--------|
| *(default)* | Native backend only, no extra deps. |
| `cef` | Adds the CEF (Chromium) backend via `makepad-cef`. |

## Crate layout

- `src/lib.rs` — public API: `WebView`, `WebViewOptions`, `Backend`, `WebViewEvent`, `WebViewError`.
- `src/bridge.rs` — the `window.vs` protocol + `BridgeMessage` (pure logic, unit-tested on all targets).
- Platform backends are selected at compile time via `#[cfg]`.
