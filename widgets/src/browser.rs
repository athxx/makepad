use crate::{image::DrawImage, makepad_derive_widget::*, makepad_draw::*, widget::*};

script_mod! {
    use mod.prelude.widgets_internal.*

    mod.widgets.BrowserBackend = #(BrowserBackend::script_api(vm))
    mod.widgets.splat(mod.widgets.BrowserBackend)

    mod.widgets.BrowserLoad = #(BrowserLoad::script_api(vm))
    mod.widgets.splat(mod.widgets.BrowserLoad)

    mod.widgets.BrowserDispose = #(BrowserDispose::script_api(vm))
    mod.widgets.splat(mod.widgets.BrowserDispose)

    mod.widgets.BrowserBase = #(Browser::register_widget(vm))

    mod.widgets.Browser = set_type_default() do mod.widgets.BrowserBase{
        width: Fill
        height: Fill
    }
}

#[derive(Script, ScriptHook, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserBackend {
    #[pick]
    Native,
    CEF,
}

/// When the underlying web view is created and its first page load is issued.
/// See also the sibling [`BrowserDispose`] policy. Reads `load_delay_ms` only
/// when set to [`BrowserLoad::Deferred`].
#[derive(Script, ScriptHook, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserLoad {
    /// Lazy (default): create + load only when the widget first becomes
    /// visible. Fastest startup / lowest idle memory, slowest first open.
    #[pick]
    Lazy,
    /// Create + load immediately on startup. Fastest first open, at the cost
    /// of holding a web content process and issuing the request up front.
    Eager,
    /// Warm up `load_delay_ms` after startup: create + load in the background
    /// while still hidden. Balances startup cost against first-open latency.
    Deferred,
}

/// What happens to the web view once the widget becomes invisible again.
/// Reads `dispose_delay_ms` only when set to [`BrowserDispose::Deferred`].
#[derive(Script, ScriptHook, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserDispose {
    /// Keep the web view alive when hidden. Next show is instant; memory stays
    /// resident until the app exits.
    Keep,
    /// Destroy the web view if it stays hidden for `dispose_delay_ms`. Balances
    /// memory against re-open latency. Default.
    #[pick]
    Deferred,
    /// Destroy the web view as soon as it becomes hidden. Frees memory eagerly;
    /// the next show pays a full cold start.
    Immediate,
}

/// Actions emitted by a [`Browser`] widget, mirroring
/// [`makepad_webview::WebViewEvent`]: page load lifecycle plus inbound bridge
/// messages from page JS (`window.vs.postMessage`).
#[derive(Clone, Debug, Default)]
pub enum BrowserAction {
    #[default]
    None,
    LoadStarted { url: String },
    LoadFinished { url: String },
    LoadFailed { url: String, error: String },
    /// A structured message from the page's JS bridge.
    Message(makepad_webview::BridgeMessage),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
enum ActiveBrowserBackend {
    #[default]
    None,
    Native,
    CEF,
    Unsupported,
}

#[derive(Script, ScriptHook, Widget)]
pub struct Browser {
    #[uid]
    uid: WidgetUid,
    #[source]
    source: ScriptObjectRef,
    #[walk]
    walk: Walk,
    #[redraw]
    #[live]
    draw_bg: DrawImage,
    #[live(BrowserBackend::Native)]
    backend: BrowserBackend,
    /// When the web view is created + first loaded. See [`BrowserLoad`].
    #[live(BrowserLoad::Lazy)]
    load: BrowserLoad,
    /// Delay (ms) before background warm-up; used only when `load` is
    /// [`BrowserLoad::Deferred`].
    #[live]
    load_delay_ms: u64,
    /// What happens to the web view when hidden. See [`BrowserDispose`].
    #[live]
    dispose: BrowserDispose,
    /// Delay (ms) a hidden web view is kept before destruction; used only when
    /// `dispose` is [`BrowserDispose::Deferred`].
    #[live(30000)]
    dispose_delay_ms: u64,
    #[live]
    url: ArcStringMut,
    #[visible]
    #[live(true)]
    visible: bool,
    #[rust]
    texture: Option<Texture>,
    #[cfg(feature = "cef")]
    #[rust]
    cef_browser: Option<makepad_cef::Browser>,
    #[rust]
    pump_timer: Timer,
    /// Fires once after `load_delay_ms` for [`BrowserLoad::Deferred`] warm-up.
    #[rust]
    warmup_timer: Timer,
    /// Fires once after `dispose_delay_ms` for [`BrowserDispose::Deferred`].
    #[rust]
    dispose_timer: Timer,
    #[rust]
    init_error: Option<String>,
    #[rust]
    last_url: String,
    #[rust]
    active_backend: ActiveBrowserBackend,
    /// The unified cross-platform web view (Native backend). Created lazily on
    /// first draw with the Native backend and torn down on backend transition.
    /// The CEF path does not use this (it is migrated into the library in P4).
    #[rust]
    webview: Option<makepad_webview::WebView>,
    #[cfg(feature = "cef")]
    #[rust]
    pressed_buttons: MouseButton,
    #[cfg(feature = "cef")]
    #[rust]
    suppress_next_paste_shortcut: bool,
}

impl Browser {
    const PUMP_INTERVAL: f64 = 1.0 / 60.0;

    /// A stable identity for the platform-side overlay (used by the library's
    /// Apple backend to key its `SystemBrowserId`).
    fn webview_id(&self) -> u64 {
        self.uid.0
    }

    fn resolved_backend(&self) -> ActiveBrowserBackend {
        match self.backend {
            BrowserBackend::Native => {
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                {
                    return ActiveBrowserBackend::Native;
                }
                #[cfg(not(any(target_os = "macos", target_os = "ios")))]
                {
                    return ActiveBrowserBackend::Unsupported;
                }
            }
            BrowserBackend::CEF => {
                #[cfg(feature = "cef")]
                {
                    return ActiveBrowserBackend::CEF;
                }
                #[cfg(not(feature = "cef"))]
                {
                    return ActiveBrowserBackend::Unsupported;
                }
            }
        }
    }

    fn unsupported_backend_message(&self) -> &'static str {
        match self.backend {
            BrowserBackend::Native => {
                "Native browser backend is currently only implemented on macOS and iOS"
            }
            BrowserBackend::CEF => "CEF browser backend requires the `cef` feature",
        }
    }

    fn sync_backend_transition(&mut self, cx: &mut Cx, desired_backend: ActiveBrowserBackend) {
        if self.active_backend == desired_backend {
            return;
        }

        match self.active_backend {
            ActiveBrowserBackend::Native => {
                if let Some(mut webview) = self.webview.take() {
                    webview.detach(cx);
                }
            }
            ActiveBrowserBackend::CEF => {
                self.texture = None;
                #[cfg(feature = "cef")]
                {
                    self.cef_browser = None;
                }
            }
            ActiveBrowserBackend::None | ActiveBrowserBackend::Unsupported => {}
        }

        self.active_backend = desired_backend;
        if desired_backend != ActiveBrowserBackend::Unsupported {
            self.init_error = None;
        }
    }

    /// Create the Native web view (and issue its first `loadRequest`) if it
    /// does not yet exist. Idempotent and safe to call from any load-policy
    /// path (startup warm-up or first-visible draw). No-op unless the resolved
    /// backend is Native.
    fn ensure_created(&mut self, cx: &mut Cx) {
        if self.webview.is_some() {
            return;
        }
        if self.resolved_backend() != ActiveBrowserBackend::Native {
            return;
        }
        let url = self.url.as_ref().to_string();
        let opts = makepad_webview::WebViewOptions {
            url: url.clone(),
            backend: makepad_webview::Backend::Native,
            visible: self.visible,
            id: self.webview_id(),
        };
        match makepad_webview::WebView::new(cx, opts) {
            Ok(webview) => {
                self.webview = Some(webview);
                self.last_url.clear();
                self.last_url.push_str(&url);
            }
            Err(err) => {
                let message = err.to_string();
                log!("Browser widget initialization failed: {message}");
                self.init_error = Some(message);
            }
        }
    }

    fn sync_webview(&mut self, cx: &mut Cx) {
        self.ensure_created(cx);
        if self.init_error.is_some() && self.webview.is_none() {
            return;
        }
        let url = self.url.as_ref().to_string();

        let area = self.browser_area();
        let visible = self.visible && area.is_valid(cx);

        let Some(webview) = &mut self.webview else {
            return;
        };

        if self.last_url != url {
            webview.set_url(cx, &url);
            self.last_url.clear();
            self.last_url.push_str(&url);
        }

        webview.update_rect(cx, area, visible);
    }

    fn browser_area(&self) -> Area {
        self.draw_bg.area()
    }

    /// Drain queued events from the Native web view and re-emit them as
    /// [`BrowserAction`]s. Load events / inbound bridge messages arrive here
    /// once the platform backchannel is wired (P2); until then this is a no-op.
    fn poll_webview_events(&mut self, cx: &mut Cx) {
        let Some(webview) = &mut self.webview else {
            return;
        };
        let events = webview.poll_events();
        if events.is_empty() {
            return;
        }
        let uid = self.uid;
        for event in events {
            let action = match event {
                makepad_webview::WebViewEvent::LoadStarted { url } => {
                    BrowserAction::LoadStarted { url }
                }
                makepad_webview::WebViewEvent::LoadFinished { url } => {
                    BrowserAction::LoadFinished { url }
                }
                makepad_webview::WebViewEvent::LoadFailed { url, error } => {
                    BrowserAction::LoadFailed { url, error }
                }
                makepad_webview::WebViewEvent::Message(msg) => BrowserAction::Message(msg),
            };
            cx.widget_action(uid, action);
        }
    }

    #[cfg(feature = "cef")]
    fn browser_rect(&self, cx: &mut Cx) -> Option<Rect> {
        let area = self.browser_area();
        if area.is_valid(cx) {
            Some(area.rect(cx))
        } else {
            None
        }
    }

    #[cfg(feature = "cef")]
    fn dpi_factor(&self, cx: &mut Cx) -> f64 {
        let area = self.browser_area();
        if area.is_valid(cx) {
            cx.get_dpi_factor_of(&area).max(1.0)
        } else {
            1.0
        }
    }

    #[cfg(feature = "cef")]
    fn cef_position(&self, cx: &mut Cx, abs: Vec2d) -> Option<(i32, i32)> {
        let rect = self.browser_rect(cx)?;
        let dpi = self.dpi_factor(cx);
        let local = abs - rect.pos;
        Some((
            (local.x * dpi).round() as i32,
            (local.y * dpi).round() as i32,
        ))
    }

    #[cfg(feature = "cef")]
    fn cef_modifiers(modifiers: KeyModifiers, pressed_buttons: MouseButton) -> u32 {
        let mut out = makepad_cef::EVENTFLAG_NONE;
        if modifiers.shift {
            out |= makepad_cef::EVENTFLAG_SHIFT_DOWN;
        }
        if modifiers.control {
            out |= makepad_cef::EVENTFLAG_CONTROL_DOWN;
        }
        if modifiers.alt {
            out |= makepad_cef::EVENTFLAG_ALT_DOWN;
        }
        if modifiers.logo {
            out |= makepad_cef::EVENTFLAG_COMMAND_DOWN;
        }
        if pressed_buttons.is_primary() {
            out |= makepad_cef::EVENTFLAG_LEFT_MOUSE_BUTTON;
        }
        if pressed_buttons.is_middle() {
            out |= makepad_cef::EVENTFLAG_MIDDLE_MOUSE_BUTTON;
        }
        if pressed_buttons.is_secondary() {
            out |= makepad_cef::EVENTFLAG_RIGHT_MOUSE_BUTTON;
        }
        out
    }

    #[cfg(feature = "cef")]
    fn cef_mouse_button(button: Option<MouseButton>) -> i32 {
        match button {
            Some(button) if button.is_secondary() => makepad_cef::MOUSE_BUTTON_RIGHT,
            Some(button) if button.is_middle() => makepad_cef::MOUSE_BUTTON_MIDDLE,
            _ => makepad_cef::MOUSE_BUTTON_LEFT,
        }
    }

    #[cfg(feature = "cef")]
    fn windows_key_code(key_code: KeyCode) -> i32 {
        match key_code {
            KeyCode::Escape => 0x1B,
            KeyCode::Back => 0xA6,
            KeyCode::Backtick => 0xC0,
            KeyCode::Key0 => 0x30,
            KeyCode::Key1 => 0x31,
            KeyCode::Key2 => 0x32,
            KeyCode::Key3 => 0x33,
            KeyCode::Key4 => 0x34,
            KeyCode::Key5 => 0x35,
            KeyCode::Key6 => 0x36,
            KeyCode::Key7 => 0x37,
            KeyCode::Key8 => 0x38,
            KeyCode::Key9 => 0x39,
            KeyCode::Minus => 0xBD,
            KeyCode::Equals => 0xBB,
            KeyCode::Backspace => 0x08,
            KeyCode::Tab => 0x09,
            KeyCode::KeyQ => 0x51,
            KeyCode::KeyW => 0x57,
            KeyCode::KeyE => 0x45,
            KeyCode::KeyR => 0x52,
            KeyCode::KeyT => 0x54,
            KeyCode::KeyY => 0x59,
            KeyCode::KeyU => 0x55,
            KeyCode::KeyI => 0x49,
            KeyCode::KeyO => 0x4F,
            KeyCode::KeyP => 0x50,
            KeyCode::LBracket => 0xDB,
            KeyCode::RBracket => 0xDD,
            KeyCode::ReturnKey => 0x0D,
            KeyCode::KeyA => 0x41,
            KeyCode::KeyS => 0x53,
            KeyCode::KeyD => 0x44,
            KeyCode::KeyF => 0x46,
            KeyCode::KeyG => 0x47,
            KeyCode::KeyH => 0x48,
            KeyCode::KeyJ => 0x4A,
            KeyCode::KeyK => 0x4B,
            KeyCode::KeyL => 0x4C,
            KeyCode::Semicolon => 0xBA,
            KeyCode::Quote => 0xDE,
            KeyCode::Backslash => 0xDC,
            KeyCode::KeyZ => 0x5A,
            KeyCode::KeyX => 0x58,
            KeyCode::KeyC => 0x43,
            KeyCode::KeyV => 0x56,
            KeyCode::KeyB => 0x42,
            KeyCode::KeyN => 0x4E,
            KeyCode::KeyM => 0x4D,
            KeyCode::Comma => 0xBC,
            KeyCode::Period => 0xBE,
            KeyCode::Slash => 0xBF,
            KeyCode::Control => 0x11,
            KeyCode::Alt => 0x12,
            KeyCode::Shift => 0x10,
            KeyCode::Logo => 0x5B,
            KeyCode::Space => 0x20,
            KeyCode::Capslock => 0x14,
            KeyCode::F1 => 0x70,
            KeyCode::F2 => 0x71,
            KeyCode::F3 => 0x72,
            KeyCode::F4 => 0x73,
            KeyCode::F5 => 0x74,
            KeyCode::F6 => 0x75,
            KeyCode::F7 => 0x76,
            KeyCode::F8 => 0x77,
            KeyCode::F9 => 0x78,
            KeyCode::F10 => 0x79,
            KeyCode::F11 => 0x7A,
            KeyCode::F12 => 0x7B,
            KeyCode::PrintScreen => 0x2C,
            KeyCode::ScrollLock => 0x91,
            KeyCode::Pause => 0x13,
            KeyCode::Insert => 0x2D,
            KeyCode::Delete => 0x2E,
            KeyCode::Home => 0x24,
            KeyCode::End => 0x23,
            KeyCode::PageUp => 0x21,
            KeyCode::PageDown => 0x22,
            KeyCode::Numpad0 => 0x60,
            KeyCode::Numpad1 => 0x61,
            KeyCode::Numpad2 => 0x62,
            KeyCode::Numpad3 => 0x63,
            KeyCode::Numpad4 => 0x64,
            KeyCode::Numpad5 => 0x65,
            KeyCode::Numpad6 => 0x66,
            KeyCode::Numpad7 => 0x67,
            KeyCode::Numpad8 => 0x68,
            KeyCode::Numpad9 => 0x69,
            KeyCode::NumpadEquals => 0x92,
            KeyCode::NumpadSubtract => 0x6D,
            KeyCode::NumpadAdd => 0x6B,
            KeyCode::NumpadDecimal => 0x6E,
            KeyCode::NumpadMultiply => 0x6A,
            KeyCode::NumpadDivide => 0x6F,
            KeyCode::Numlock => 0x90,
            KeyCode::NumpadEnter => 0x0D,
            KeyCode::ArrowUp => 0x26,
            KeyCode::ArrowDown => 0x28,
            KeyCode::ArrowLeft => 0x25,
            KeyCode::ArrowRight => 0x27,
            KeyCode::Unknown => 0,
        }
    }

    #[cfg(feature = "cef")]
    fn key_char(key_code: KeyCode, shift: bool) -> Option<char> {
        match key_code {
            KeyCode::Backspace => Some('\u{8}'),
            KeyCode::Backtick => Some(if shift { '~' } else { '`' }),
            KeyCode::Key0 => Some(if shift { ')' } else { '0' }),
            KeyCode::Key1 => Some(if shift { '!' } else { '1' }),
            KeyCode::Key2 => Some(if shift { '@' } else { '2' }),
            KeyCode::Key3 => Some(if shift { '#' } else { '3' }),
            KeyCode::Key4 => Some(if shift { '$' } else { '4' }),
            KeyCode::Key5 => Some(if shift { '%' } else { '5' }),
            KeyCode::Key6 => Some(if shift { '^' } else { '6' }),
            KeyCode::Key7 => Some(if shift { '&' } else { '7' }),
            KeyCode::Key8 => Some(if shift { '*' } else { '8' }),
            KeyCode::Key9 => Some(if shift { '(' } else { '9' }),
            KeyCode::Minus => Some(if shift { '_' } else { '-' }),
            KeyCode::Equals => Some(if shift { '+' } else { '=' }),
            KeyCode::KeyQ => Some(if shift { 'Q' } else { 'q' }),
            KeyCode::KeyW => Some(if shift { 'W' } else { 'w' }),
            KeyCode::KeyE => Some(if shift { 'E' } else { 'e' }),
            KeyCode::KeyR => Some(if shift { 'R' } else { 'r' }),
            KeyCode::KeyT => Some(if shift { 'T' } else { 't' }),
            KeyCode::KeyY => Some(if shift { 'Y' } else { 'y' }),
            KeyCode::KeyU => Some(if shift { 'U' } else { 'u' }),
            KeyCode::KeyI => Some(if shift { 'I' } else { 'i' }),
            KeyCode::KeyO => Some(if shift { 'O' } else { 'o' }),
            KeyCode::KeyP => Some(if shift { 'P' } else { 'p' }),
            KeyCode::LBracket => Some(if shift { '{' } else { '[' }),
            KeyCode::RBracket => Some(if shift { '}' } else { ']' }),
            KeyCode::KeyA => Some(if shift { 'A' } else { 'a' }),
            KeyCode::KeyS => Some(if shift { 'S' } else { 's' }),
            KeyCode::KeyD => Some(if shift { 'D' } else { 'd' }),
            KeyCode::KeyF => Some(if shift { 'F' } else { 'f' }),
            KeyCode::KeyG => Some(if shift { 'G' } else { 'g' }),
            KeyCode::KeyH => Some(if shift { 'H' } else { 'h' }),
            KeyCode::KeyJ => Some(if shift { 'J' } else { 'j' }),
            KeyCode::KeyK => Some(if shift { 'K' } else { 'k' }),
            KeyCode::KeyL => Some(if shift { 'L' } else { 'l' }),
            KeyCode::Semicolon => Some(if shift { ':' } else { ';' }),
            KeyCode::Quote => Some(if shift { '"' } else { '\'' }),
            KeyCode::Backslash => Some(if shift { '|' } else { '\\' }),
            KeyCode::KeyZ => Some(if shift { 'Z' } else { 'z' }),
            KeyCode::KeyX => Some(if shift { 'X' } else { 'x' }),
            KeyCode::KeyC => Some(if shift { 'C' } else { 'c' }),
            KeyCode::KeyV => Some(if shift { 'V' } else { 'v' }),
            KeyCode::KeyB => Some(if shift { 'B' } else { 'b' }),
            KeyCode::KeyN => Some(if shift { 'N' } else { 'n' }),
            KeyCode::KeyM => Some(if shift { 'M' } else { 'm' }),
            KeyCode::Comma => Some(if shift { '<' } else { ',' }),
            KeyCode::Period => Some(if shift { '>' } else { '.' }),
            KeyCode::Slash => Some(if shift { '?' } else { '/' }),
            KeyCode::Tab => Some('\t'),
            KeyCode::ReturnKey => Some('\r'),
            KeyCode::Space => Some(' '),
            KeyCode::Numpad0 => Some('0'),
            KeyCode::Numpad1 => Some('1'),
            KeyCode::Numpad2 => Some('2'),
            KeyCode::Numpad3 => Some('3'),
            KeyCode::Numpad4 => Some('4'),
            KeyCode::Numpad5 => Some('5'),
            KeyCode::Numpad6 => Some('6'),
            KeyCode::Numpad7 => Some('7'),
            KeyCode::Numpad8 => Some('8'),
            KeyCode::Numpad9 => Some('9'),
            KeyCode::NumpadEquals => Some('='),
            KeyCode::NumpadSubtract => Some('-'),
            KeyCode::NumpadAdd => Some('+'),
            KeyCode::NumpadDecimal => Some('.'),
            KeyCode::NumpadMultiply => Some('*'),
            KeyCode::NumpadDivide => Some('/'),
            KeyCode::NumpadEnter => Some('\r'),
            _ => None,
        }
    }

    #[cfg(feature = "cef")]
    fn sends_char_on_keydown(key_code: KeyCode) -> bool {
        matches!(
            key_code,
            KeyCode::Backspace | KeyCode::Tab | KeyCode::ReturnKey | KeyCode::NumpadEnter
        )
    }

    #[cfg(feature = "cef")]
    fn char_event_data(text: &str) -> Option<(i32, u16)> {
        let mut chars = text.chars();
        let ch = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        let mut utf16 = text.encode_utf16();
        let unit = utf16.next()?;
        if utf16.next().is_some() {
            return None;
        }
        let windows_key_code = if ch.is_ascii_alphabetic() {
            ch.to_ascii_uppercase() as i32
        } else {
            unit as i32
        };
        Some((windows_key_code, unit))
    }

    #[cfg(feature = "cef")]
    fn key_event_modifiers(key_event: &KeyEvent) -> u32 {
        let mut modifiers = Self::cef_modifiers(key_event.modifiers, MouseButton::empty());
        if key_event.is_repeat {
            modifiers |= makepad_cef::EVENTFLAG_IS_REPEAT;
        }
        if matches!(
            key_event.key_code,
            KeyCode::Numpad0
                | KeyCode::Numpad1
                | KeyCode::Numpad2
                | KeyCode::Numpad3
                | KeyCode::Numpad4
                | KeyCode::Numpad5
                | KeyCode::Numpad6
                | KeyCode::Numpad7
                | KeyCode::Numpad8
                | KeyCode::Numpad9
                | KeyCode::NumpadEquals
                | KeyCode::NumpadSubtract
                | KeyCode::NumpadAdd
                | KeyCode::NumpadDecimal
                | KeyCode::NumpadMultiply
                | KeyCode::NumpadDivide
                | KeyCode::NumpadEnter
        ) {
            modifiers |= makepad_cef::EVENTFLAG_IS_KEY_PAD;
        }
        modifiers
    }

    #[cfg(feature = "cef")]
    fn send_mouse_move_internal(
        &mut self,
        cx: &mut Cx,
        abs: Vec2d,
        modifiers: KeyModifiers,
        mouse_leave: bool,
    ) {
        let Some((x, y)) = self.cef_position(cx, abs) else {
            return;
        };
        let cef_modifiers = Self::cef_modifiers(modifiers, self.pressed_buttons);
        if let Some(browser) = &mut self.cef_browser {
            if let Err(err) = browser.send_mouse_move(x, y, cef_modifiers, mouse_leave) {
                log!("Browser mouse move failed: {err}");
            }
        }
    }

    #[cfg(feature = "cef")]
    fn send_mouse_click_internal(
        &mut self,
        cx: &mut Cx,
        abs: Vec2d,
        modifiers: KeyModifiers,
        button: Option<MouseButton>,
        mouse_up: bool,
        click_count: i32,
    ) {
        let Some((x, y)) = self.cef_position(cx, abs) else {
            return;
        };
        let cef_modifiers = Self::cef_modifiers(modifiers, self.pressed_buttons);
        let cef_button = Self::cef_mouse_button(button);
        if let Some(browser) = &mut self.cef_browser {
            if let Err(err) = browser.send_mouse_click(
                x,
                y,
                cef_modifiers,
                cef_button,
                mouse_up,
                click_count.max(1),
            ) {
                log!("Browser mouse click failed: {err}");
            }
        }
    }

    #[cfg(feature = "cef")]
    fn send_mouse_wheel_internal(
        &mut self,
        cx: &mut Cx,
        abs: Vec2d,
        modifiers: KeyModifiers,
        delta: Vec2d,
    ) {
        let Some((x, y)) = self.cef_position(cx, abs) else {
            return;
        };
        let cef_modifiers = Self::cef_modifiers(modifiers, self.pressed_buttons)
            | makepad_cef::EVENTFLAG_PRECISION_SCROLLING_DELTA;
        if let Some(browser) = &mut self.cef_browser {
            if let Err(err) = browser.send_mouse_wheel(
                x,
                y,
                cef_modifiers,
                delta.x.round() as i32,
                delta.y.round() as i32,
            ) {
                log!("Browser mouse wheel failed: {err}");
            }
        }
    }

    #[cfg(feature = "cef")]
    fn update_ime_spot(&self, cx: &mut Cx, pos: Vec2d) {
        let area = self.browser_area();
        if area.is_valid(cx) {
            cx.show_text_ime(area, pos);
        }
    }

    #[cfg(feature = "cef")]
    fn ensure_browser(&mut self, _cx: &mut Cx2d, width: usize, height: usize, scale_factor: f32) {
        if self.cef_browser.is_none() && self.init_error.is_none() {
            match makepad_cef::Browser::new(self.url.as_ref(), width, height, scale_factor) {
                Ok(browser) => {
                    self.last_url = self.url.as_ref().to_string();
                    self.cef_browser = Some(browser);
                }
                Err(err) => {
                    let message = err.to_string();
                    log!("Browser widget initialization failed: {message}");
                    self.init_error = Some(message);
                }
            }
        }

        if let Some(browser) = &mut self.cef_browser {
            if let Err(err) = browser.resize(width, height, scale_factor) {
                let message = err.to_string();
                log!("Browser widget resize failed: {message}");
                self.init_error = Some(message);
                self.cef_browser = None;
            }
        }

        if let Some(browser) = &mut self.cef_browser {
            let url = self.url.as_ref();
            if self.last_url != url {
                if let Err(err) = browser.set_url(url) {
                    let message = err.to_string();
                    log!("Browser widget navigation failed: {message}");
                    self.init_error = Some(message);
                    self.cef_browser = None;
                } else {
                    self.last_url.clear();
                    self.last_url.push_str(url);
                }
            }
        }
    }

    #[cfg(feature = "cef")]
    fn pump_browser(&mut self, cx: &mut Cx) {
        makepad_cef::do_message_loop_work();
        let mut latest_frame = None;
        if let Some(browser) = &mut self.cef_browser {
            while let Some(frame) = browser.take_frame() {
                latest_frame = Some(frame);
            }
        }
        if let Some(frame) = latest_frame {
            self.apply_frame(cx, frame);
            self.redraw(cx);
        }
    }

    #[cfg(feature = "cef")]
    fn apply_frame(&mut self, cx: &mut Cx, frame: makepad_cef::Frame) {
        match &self.texture {
            Some(texture)
                if texture.get_format(cx).vec_width_height()
                    == Some((frame.width, frame.height)) =>
            {
                texture.set_data_u32(cx, frame.width, frame.height, frame.pixels);
            }
            _ => {
                self.texture = Some(Texture::new_with_format(
                    cx,
                    TextureFormat::VecBGRAu8_32 {
                        data: Some(frame.pixels),
                        width: frame.width,
                        height: frame.height,
                        updated: TextureUpdated::Full,
                    },
                ));
            }
        }
    }

    fn set_url_internal(&mut self, cx: &mut Cx, url: &str) {
        self.url.set(url);
        self.last_url.clear();
        match self.active_backend {
            ActiveBrowserBackend::Native => {
                if let Some(webview) = &mut self.webview {
                    webview.set_url(cx, url);
                    self.last_url.push_str(url);
                }
            }
            ActiveBrowserBackend::CEF =>
            {
                #[cfg(feature = "cef")]
                if let Some(browser) = &mut self.cef_browser {
                    if let Err(err) = browser.set_url(url) {
                        let message = err.to_string();
                        log!("Browser widget navigation failed: {message}");
                        self.init_error = Some(message);
                        self.cef_browser = None;
                    }
                }
            }
            ActiveBrowserBackend::None | ActiveBrowserBackend::Unsupported => {}
        }
        self.redraw(cx);
    }

    fn set_visible_internal(&mut self, cx: &mut Cx, visible: bool) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.on_shown(cx);
        } else {
            self.on_hidden(cx);
        }
        self.redraw(cx);
    }

    /// Called when the widget transitions to visible: cancel any pending
    /// deferred destruction so a quick hide/show round-trip stays instant.
    fn on_shown(&mut self, cx: &mut Cx) {
        cx.stop_timer(self.dispose_timer);
        self.dispose_timer = Timer::default();
    }

    /// Called when the widget transitions to hidden: hide the web view and
    /// apply the configured [`BrowserDispose`] policy.
    fn on_hidden(&mut self, cx: &mut Cx) {
        if self.active_backend != ActiveBrowserBackend::Native {
            return;
        }
        let area = self.draw_bg.area();
        if let Some(webview) = &mut self.webview {
            webview.update_rect(cx, area, false);
        }
        match self.dispose {
            BrowserDispose::Keep => {}
            BrowserDispose::Immediate => {
                self.destroy_webview(cx);
            }
            BrowserDispose::Deferred => {
                if self.webview.is_some() {
                    cx.stop_timer(self.dispose_timer);
                    self.dispose_timer =
                        cx.start_timeout(self.dispose_delay_ms as f64 / 1000.0);
                }
            }
        }
    }

    /// Tear down the Native web view and reset load state so a later show
    /// re-creates and reloads it from scratch.
    fn destroy_webview(&mut self, cx: &mut Cx) {
        cx.stop_timer(self.dispose_timer);
        self.dispose_timer = Timer::default();
        if let Some(mut webview) = self.webview.take() {
            webview.detach(cx);
        }
        self.last_url.clear();
    }
}

impl Widget for Browser {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, _scope: &mut Scope) {
        let desired_backend = self.resolved_backend();
        self.sync_backend_transition(cx, desired_backend);

        if let Event::Startup = event {
            self.pump_timer = cx.start_interval(Self::PUMP_INTERVAL);
            if desired_backend == ActiveBrowserBackend::Native {
                match self.load {
                    BrowserLoad::Lazy => {}
                    BrowserLoad::Eager => self.ensure_created(cx),
                    BrowserLoad::Deferred => {
                        self.warmup_timer =
                            cx.start_timeout(self.load_delay_ms as f64 / 1000.0);
                    }
                }
            }
        }

        if matches!(event, Event::Shutdown) {
            cx.stop_timer(self.warmup_timer);
            cx.stop_timer(self.dispose_timer);
            if let Some(mut webview) = self.webview.take() {
                webview.detach(cx);
            }
            #[cfg(feature = "cef")]
            {
                self.cef_browser = None;
            }
            return;
        }

        if self.warmup_timer.is_event(event).is_some() {
            self.warmup_timer = Timer::default();
            self.ensure_created(cx);
        }

        if self.dispose_timer.is_event(event).is_some() {
            self.dispose_timer = Timer::default();
            if !self.visible {
                self.destroy_webview(cx);
            }
        }

        if desired_backend == ActiveBrowserBackend::Unsupported {
            if self.init_error.is_none() {
                let message = self.unsupported_backend_message().to_string();
                log!("{message}");
                self.init_error = Some(message);
            }
            return;
        }

        if self.pump_timer.is_event(event).is_some() {
            if desired_backend == ActiveBrowserBackend::Native {
                self.poll_webview_events(cx);
            }
            #[cfg(feature = "cef")]
            if desired_backend == ActiveBrowserBackend::CEF {
                self.pump_browser(cx);
            }
        }

        if !self.visible {
            if desired_backend == ActiveBrowserBackend::Native {
                if let Some(webview) = &mut self.webview {
                    let area = self.draw_bg.area();
                    webview.update_rect(cx, area, false);
                }
            }
            return;
        }

        #[cfg(feature = "cef")]
        if desired_backend == ActiveBrowserBackend::CEF {
            match event.hits_with_capture_overload(cx, self.browser_area(), true) {
                Hit::KeyFocus(_) => {
                    if let Some(browser) = &mut self.cef_browser {
                        if let Err(err) = browser.set_focus(true) {
                            log!("Browser focus failed: {err}");
                        }
                    }
                    if let Some(rect) = self.browser_rect(cx) {
                        self.update_ime_spot(cx, rect.pos);
                    }
                }
                Hit::KeyFocusLost(_) => {
                    if let Some(browser) = &mut self.cef_browser {
                        if let Err(err) = browser.set_focus(false) {
                            log!("Browser blur failed: {err}");
                        }
                    }
                    cx.hide_text_ime();
                    self.suppress_next_paste_shortcut = false;
                }
                Hit::FingerDown(fe) => {
                    let button = fe.mouse_button().unwrap_or(MouseButton::PRIMARY);
                    self.pressed_buttons.insert(button);
                    cx.set_key_focus(self.browser_area());
                    if let Some(browser) = &mut self.cef_browser {
                        if let Err(err) = browser.set_focus(true) {
                            log!("Browser focus on pointer down failed: {err}");
                        }
                    }
                    self.update_ime_spot(cx, fe.abs);
                    self.send_mouse_move_internal(cx, fe.abs, fe.modifiers, false);
                    self.send_mouse_click_internal(
                        cx,
                        fe.abs,
                        fe.modifiers,
                        Some(button),
                        false,
                        fe.tap_count as i32,
                    );
                }
                Hit::FingerMove(fe) => {
                    self.send_mouse_move_internal(cx, fe.abs, fe.modifiers, false);
                }
                Hit::FingerUp(fe) => {
                    let button = fe.mouse_button().unwrap_or(MouseButton::PRIMARY);
                    self.send_mouse_move_internal(cx, fe.abs, fe.modifiers, false);
                    self.send_mouse_click_internal(
                        cx,
                        fe.abs,
                        fe.modifiers,
                        Some(button),
                        true,
                        fe.tap_count as i32,
                    );
                    self.pressed_buttons.remove(button);
                }
                Hit::FingerHoverIn(fe) | Hit::FingerHoverOver(fe) => {
                    self.send_mouse_move_internal(cx, fe.abs, fe.modifiers, false);
                }
                Hit::FingerHoverOut(fe) => {
                    self.send_mouse_move_internal(cx, fe.abs, fe.modifiers, true);
                }
                Hit::FingerScroll(fe) => {
                    self.send_mouse_wheel_internal(cx, fe.abs, fe.modifiers, fe.scroll);
                }
                Hit::KeyDown(key_event) => {
                    if self.suppress_next_paste_shortcut
                        && key_event.key_code == KeyCode::KeyV
                        && key_event.modifiers.is_primary()
                    {
                        self.suppress_next_paste_shortcut = false;
                    } else {
                        let modifiers = Self::key_event_modifiers(&key_event);
                        let windows_key_code = Self::windows_key_code(key_event.key_code);
                        let character = if key_event.modifiers.control
                            || key_event.modifiers.alt
                            || key_event.modifiers.logo
                        {
                            0
                        } else {
                            Self::key_char(key_event.key_code, key_event.modifiers.shift)
                                .map(|ch| ch as u16)
                                .unwrap_or(0)
                        };

                        if let Some(browser) = &mut self.cef_browser {
                            if let Err(err) = browser.send_key_event(
                                makepad_cef::KEY_EVENT_KEYDOWN,
                                modifiers,
                                windows_key_code,
                                windows_key_code,
                                character,
                                character,
                                false,
                            ) {
                                log!("Browser key down failed: {err}");
                            }
                            if character != 0
                                && !key_event.modifiers.control
                                && !key_event.modifiers.alt
                                && !key_event.modifiers.logo
                                && Self::sends_char_on_keydown(key_event.key_code)
                            {
                                if let Err(err) = browser.send_key_event(
                                    makepad_cef::KEY_EVENT_CHAR,
                                    modifiers,
                                    windows_key_code,
                                    windows_key_code,
                                    character,
                                    character,
                                    false,
                                ) {
                                    log!("Browser key char failed: {err}");
                                }
                            }
                        }
                    }
                }
                Hit::KeyUp(key_event) => {
                    let modifiers = Self::key_event_modifiers(&key_event);
                    let windows_key_code = Self::windows_key_code(key_event.key_code);
                    let character = if key_event.modifiers.control
                        || key_event.modifiers.alt
                        || key_event.modifiers.logo
                    {
                        0
                    } else {
                        Self::key_char(key_event.key_code, key_event.modifiers.shift)
                            .map(|ch| ch as u16)
                            .unwrap_or(0)
                    };

                    if let Some(browser) = &mut self.cef_browser {
                        if let Err(err) = browser.send_key_event(
                            makepad_cef::KEY_EVENT_KEYUP,
                            modifiers,
                            windows_key_code,
                            windows_key_code,
                            character,
                            character,
                            false,
                        ) {
                            log!("Browser key up failed: {err}");
                        }
                    }
                }
                Hit::TextInput(text_event) => {
                    let ime_pos = self
                        .browser_rect(cx)
                        .map(|rect| rect.pos)
                        .unwrap_or_default();
                    self.update_ime_spot(cx, ime_pos);
                    if text_event.was_paste {
                        self.suppress_next_paste_shortcut = true;
                    }

                    if let Some(browser) = &mut self.cef_browser {
                        let modifiers =
                            Self::cef_modifiers(cx.keyboard.modifiers(), MouseButton::empty());
                        if text_event.was_paste
                            || text_event.replace_last
                            || Self::char_event_data(&text_event.input).is_none()
                        {
                            if let Err(err) = browser.ime_commit_text(&text_event.input) {
                                log!("Browser text commit failed: {err}");
                            }
                        } else if let Some((windows_key_code, character)) =
                            Self::char_event_data(&text_event.input)
                        {
                            if let Err(err) = browser.send_key_event(
                                makepad_cef::KEY_EVENT_CHAR,
                                modifiers,
                                windows_key_code,
                                windows_key_code,
                                character,
                                character,
                                false,
                            ) {
                                log!("Browser char input failed: {err}");
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let desired_backend = self.resolved_backend();
        self.sync_backend_transition(cx, desired_backend);

        if !self.visible {
            if desired_backend == ActiveBrowserBackend::Native {
                if let Some(webview) = &mut self.webview {
                    let area = self.draw_bg.area();
                    webview.update_rect(cx, area, false);
                }
            }
            return DrawStep::done();
        }

        if desired_backend == ActiveBrowserBackend::Unsupported {
            if self.init_error.is_none() {
                let message = self.unsupported_backend_message().to_string();
                log!("{message}");
                self.init_error = Some(message);
            }
            self.draw_bg.draw_vars.empty_texture(0);
            self.draw_bg.draw_walk(cx, walk);
            return DrawStep::done();
        }

        match desired_backend {
            ActiveBrowserBackend::Native => {
                self.draw_bg.draw_vars.empty_texture(0);
                self.draw_bg.draw_walk(cx, walk);
                self.sync_webview(cx);
                DrawStep::done()
            }
            ActiveBrowserBackend::CEF => {
                #[cfg(feature = "cef")]
                {
                    let rect = cx.peek_walk_turtle(walk);
                    let dpi = cx.current_dpi_factor() as f32;
                    let width = (rect.size.x.max(1.0) * dpi as f64).round().max(1.0) as usize;
                    let height = (rect.size.y.max(1.0) * dpi as f64).round().max(1.0) as usize;

                    self.ensure_browser(cx, width, height, dpi);

                    if let Some(texture) = &self.texture {
                        self.draw_bg.draw_vars.set_texture(0, texture);
                    } else {
                        self.draw_bg.draw_vars.empty_texture(0);
                    }

                    self.draw_bg.draw_walk(cx, walk);
                    cx.add_nav_stop(self.draw_bg.area(), NavRole::TextInput, Inset::default());
                    DrawStep::done()
                }
                #[cfg(not(feature = "cef"))]
                {
                    DrawStep::done()
                }
            }
            ActiveBrowserBackend::None | ActiveBrowserBackend::Unsupported => DrawStep::done(),
        }
    }
}

impl BrowserRef {
    pub fn set_url(&self, cx: &mut Cx, url: &str) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_url_internal(cx, url);
        }
    }

    pub fn set_visible(&self, cx: &mut Cx, visible: bool) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_visible_internal(cx, visible);
        }
    }

    /// Rust → JS: deliver a structured bridge message to page listeners
    /// registered via `window.vs.onMessage(...)`. No-op unless the Native
    /// backend web view exists.
    pub fn post_to_js(&self, cx: &mut Cx, msg: &makepad_webview::BridgeMessage) {
        if let Some(mut inner) = self.borrow_mut() {
            if let Some(webview) = &mut inner.webview {
                webview.post_to_js(cx, msg);
            }
        }
    }

    /// Rust → JS: evaluate an arbitrary script in the page context. No-op
    /// unless the Native backend web view exists.
    pub fn eval_js(&self, cx: &mut Cx, script: &str) {
        if let Some(mut inner) = self.borrow_mut() {
            if let Some(webview) = &mut inner.webview {
                webview.eval_js(cx, script);
            }
        }
    }

    /// Read the [`BrowserAction`] emitted by this widget this frame, if any.
    pub fn action(&self, actions: &Actions) -> BrowserAction {
        if let Some(item) = actions.find_widget_action(self.widget_uid()) {
            item.cast()
        } else {
            BrowserAction::None
        }
    }
}
