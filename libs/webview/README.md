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

The `Native` backend is implemented for **macOS, iOS, Windows, Linux, Android,
and Web (wasm)** — every platform in the table at the top of this file. On any
other target the widget falls back to a no-op backend that renders nothing
(`WebViewError::Unsupported`); build with `--features cef` to force the CEF
backend there instead.

See [Developing on each platform](#developing-on-each-platform) below for the
prerequisites, build commands, and platform-specific notes for each.

---

## Developing on each platform

The Rust code is the **same on every platform** — you write the `Browser`
widget (or the `WebView` handle) once and it compiles everywhere. What differs
is the toolchain, the build/run command, and a few platform quirks (permissions,
signing, the JS bridge transport). This section walks through each.

Everything below uses the [`examples/webview`](../../examples/webview) app as
the concrete thing being built. It is a single button that opens
`https://google.com` inside an embedded web view — the smallest complete app
that exercises this crate. Swap `-p makepad-example-webview` for your own crate.

Desktop targets (macOS, Windows, Linux) build with plain `cargo`. Mobile and
wasm go through the [`cargo-makepad`](../../tools/cargo_makepad) helper, which
packages the app, generates the manifest/Info.plist, and handles install/run.
Install it once from the workspace root:

```bash
cargo install --path tools/cargo_makepad
# or run it in place without installing:
cargo run -p cargo-makepad -- <args>
```

### macOS

Native backend: **`WKWebView`**, overlaid on the Makepad surface by
`makepad-platform` itself. Nothing extra to install — the system web view ships
with the OS.

```bash
# Just run it like any Rust binary:
cargo run -p makepad-example-webview

# For a signed/bundled .app (needed for notarization, camera/mic prompts, etc.):
cargo makepad desktop run -p makepad-example-webview
```

Notes:
- No entitlement is required to load `https://` pages in a `WKWebView`.
- App Transport Security blocks plain `http://` by default — use `https://`, or
  add an ATS exception to the app's Info.plist if you must load cleartext.

### iOS

Same `WKWebView` backend as macOS. Build through `cargo-makepad`, which creates
the provisioning-signed `.ipa`/simulator bundle:

```bash
# one-time: add the iOS Rust targets
cargo makepad apple ios install-toolchain

# run on the booted simulator:
cargo makepad apple ios run-sim -p makepad-example-webview

# run on a real device (needs a provisioning profile — see below):
cargo makepad apple ios \
    --org=MyOrg --app=MyApp \
    run-device -p makepad-example-webview
```

Notes:
- A real device needs a provisioning profile. The simplest way to get one:
  create an empty app in Xcode with a matching **organisation** and **product**
  name, run it on the device once, then pass those exact names to `--org` /
  `--app`. Run `cargo makepad apple list` to see available certs/profiles/devices.
- The first web-view open in the simulator shows a ~1–2s white screen (cold
  start of the web content process). This is the simulator, not the code — use
  `BrowserLoad.Deferred`/`Eager` to warm it up (see [Load & dispose policy](#load--dispose-policy)).
- Cleartext `http://` is blocked by ATS just like macOS.

### Windows

Native backend: **WebView2** (`ICoreWebView2`), the system Edge/Chromium web
view. Reached through hand-rolled COM FFI — no `windows` crate dependency.

```bash
cargo run -p makepad-example-webview
# or, for the icon-autodetecting desktop build:
cargo makepad desktop run -p makepad-example-webview
```

Notes:
- WebView2 requires the **WebView2 Runtime** to be present. It ships with
  Windows 11 and current Windows 10; on older machines Microsoft's Evergreen
  installer provides it. If the runtime is missing, `WebView::new` returns
  `WebViewError::Backend(...)` — surface that to the user rather than assuming
  success.
- The web view runs in its own browser process (standard WebView2 architecture);
  it is torn down when you `detach`/dispose.

### Linux

Native backend: **WebKitGTK**. You need the GTK + WebKitGTK development
libraries installed.

```bash
# Debian/Ubuntu — install GTK/WebKitGTK and the other makepad deps:
cargo makepad linux apt-get-install-makepad-deps
# (or manually: sudo apt-get install libwebkit2gtk-4.1-dev libgtk-3-dev)

cargo run -p makepad-example-webview
```

Notes:
- The `-dev` packages are required at **build** time; the runtime `.so`s must be
  present on the target machine too.
- On Wayland vs X11 the overlay attaches the same way; no code change needed.

### Android

Native backend: **`android.webkit.WebView`**. Rust never touches the web view
directly — it calls thin helper methods on `MakepadActivity` over JNI, each of
which hops to the UI thread (mirroring the camera-preview overlay). The Java
side lives in
[`tools/cargo_makepad/.../MakepadActivity.java`](../../tools/cargo_makepad/src/android/java/dev/makepad/android/MakepadActivity.java);
you do not write any Java yourself — `cargo-makepad` compiles and packages it.

```bash
# one-time: download the SDK/NDK and add the Android Rust targets
cargo makepad android install-toolchain

# build + install + run on a connected device (adb):
cargo makepad android run -p makepad-example-webview \
    --package-name="dev.makepad.webview" \
    --app-label="WebView Demo"
```

Notes:
- **The app must declare the `INTERNET` permission** to load remote pages.
  `cargo-makepad` generates the manifest and already includes it (see
  `tools/cargo_makepad/src/android/mod.rs`); if you supply your own manifest,
  add `<uses-permission android:name="android.permission.INTERNET"/>` yourself.
- The JS bridge transport is an `@JavascriptInterface` object named `vsbridge`;
  page code still uses `window.vs.postMessage(...)` — the injected document-start
  script forwards it. This is invisible to your app code.
- Multiple `Browser`/`WebView`s coexist in the single app process, each keyed by
  its `id`. (This is the same-process model behind WeChat-style "multiple
  mini-programs in one app"; it does **not** give each web view its own OS task —
  that would need a multi-Activity/multi-process setup makepad does not have.)
- `--abi` selects target ABIs (default `aarch64`); pass e.g.
  `--abi=aarch64,armv7` for a multi-arch build. Add the same ABIs to
  `install-toolchain`.

### Web (wasm)

Native backend: an **`<iframe>` overlay** positioned over the wasm canvas. The
JS bridge uses `postMessage` between the host page and the iframe instead of a
native interface.

```bash
# one-time: install the wasm toolchain
cargo makepad wasm install-toolchain

# build + serve at http://localhost:8010
cargo makepad wasm run -p makepad-example-webview
```

Notes:
- Because it is a real `<iframe>`, the **same-origin policy and the target
  site's `X-Frame-Options` / `Content-Security-Policy: frame-ancestors` apply**.
  Many big sites (Google, most banks) send `X-Frame-Options: DENY` and simply
  refuse to render in an iframe — there is nothing this crate can do about that.
  Test with a page you control or one that permits framing.
- `eval_js` / `post_to_js` only reach a **same-origin** iframe; cross-origin
  frames can exchange `BridgeMessage`s via `postMessage` but cannot be scripted
  arbitrarily.
- Serve over HTTPS in production; mixed content (an `http://` iframe on an
  `https://` host) is blocked by the browser.

### The CEF backend (optional, any desktop platform)

Instead of the system web view you can embed **Chromium via CEF**, rendered to a
Makepad texture. This is opt-in behind the `cef` feature and needs the prebuilt
CEF binaries downloaded first:

```bash
# fetch the prebuilt CEF distribution for your host into local/cef-prebuilt/
./download_cef.sh

# build/run with the cef feature; force the backend in script with
# `backend: BrowserBackend.CEF` (or Backend::Cef via the handle):
cargo run -p makepad-example-webview --features cef
```

Use CEF when you need a consistent Chromium across all desktop OSes, texture-
level compositing, or a platform where no system web view is available.
See [`download_cef.sh --help`](../../download_cef.sh) for pinning a version,
channel, or cross-platform download.

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
