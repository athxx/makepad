// A minimal example: the window shows a single button. Clicking it hides the
// landing screen and reveals a full-window embedded WebView (the Native
// backend maps to the system web view on each platform). A "Back" button
// returns to the landing screen and hides the WebView again.
//
// This mirrors the "open a web page inside the app" flow (like a WeChat
// mini-program / official-account in-app browser), built on the `Browser`
// widget from `makepad-webview`.

pub use makepad_widgets;

use makepad_widgets::*;

app_main!(App);

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    startup() do #(App::script_component(vm)){
        ui: Root{
            main_window := Window{
                window.inner_size: vec2(960, 720)
                body +: {
                    root := View{
                        width: Fill
                        height: Fill
                        flow: Overlay

                        // Landing screen: just a title and one button.
                        landing := View{
                            width: Fill
                            height: Fill
                            flow: Down
                            spacing: 20
                            align: Center

                            Label{
                                text: "Embedded WebView demo"
                                draw_text.color: #xfff
                                draw_text.text_style: theme.font_bold{font_size: 20}
                            }

                            Label{
                                text: "Click the button to open a web page inside the app."
                                draw_text.color: #x9fb0d7
                                draw_text.text_style.font_size: 12
                            }

                            open_button := Button{
                                text: "Open WebView"
                            }
                        }

                        // WebView screen: hidden until the button is clicked.
                        webview_screen := View{
                            width: Fill
                            height: Fill
                            flow: Down
                            visible: false

                            top_bar := View{
                                width: Fill
                                height: Fit
                                flow: Right
                                spacing: 10
                                padding: Inset{top: 8 bottom: 8 left: 10 right: 10}
                                align: Align{y: 0.5}
                                draw_bg.color: #x17191d
                                show_bg: true

                                back_button := Button{
                                    text: "< Back"
                                }

                                Label{
                                    text: "https://google.com"
                                    draw_text.color: #x9fb0d7
                                    draw_text.text_style.font_size: 11
                                }
                            }

                            browser := Browser{
                                width: Fill
                                height: Fill
                                backend: BrowserBackend.Native
                                url: "https://google.com"
                                visible: false

                                // Warm up the web view 1.5s after startup so
                                // the first open is instant instead of cold.
                                load: BrowserLoad.Deferred
                                load_delay_ms: 1500

                                // Keep it alive for 30s after being hidden;
                                // re-opening within that window is instant.
                                dispose: BrowserDispose.Deferred
                                dispose_delay_ms: 30000
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Script, ScriptHook)]
pub struct App {
    #[live]
    ui: WidgetRef,
}

impl App {
    /// Show either the landing screen or the WebView screen. The `Browser`
    /// widget's own visibility is toggled explicitly so the platform web view
    /// is attached/detached alongside the screen it lives in.
    fn show_webview(&mut self, cx: &mut Cx, show: bool) {
        self.ui.view(cx, ids!(landing)).set_visible(cx, !show);
        self.ui.view(cx, ids!(webview_screen)).set_visible(cx, show);
        self.ui.browser(cx, ids!(browser)).set_visible(cx, show);
        self.ui.redraw(cx);
    }
}

impl MatchEvent for App {
    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions) {
        if self.ui.button(cx, ids!(open_button)).clicked(actions) {
            self.show_webview(cx, true);
        }
        if self.ui.button(cx, ids!(back_button)).clicked(actions) {
            self.show_webview(cx, false);
        }
    }
}

impl AppMain for App {
    fn script_mod(vm: &mut ScriptVm) -> ScriptValue {
        crate::makepad_widgets::script_mod(vm);
        self::script_mod(vm)
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        self.match_event(cx, event);
        self.ui.handle_event(cx, event, &mut Scope::empty());
    }
}
