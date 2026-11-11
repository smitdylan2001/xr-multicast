//! Soft-keyboard bridge: keeps the focused egui `TextEdit` in sync with
//! GameActivity's IME text state (winit 0.30 drops GameActivity `TextEvent`s,
//! so typing on the soft keyboard would otherwise not reach egui).

use android_activity::AndroidApp;
use android_activity::input::{ImeOptions, InputType, TextInputAction, TextInputState, TextSpan};
use eframe::egui;
use xrmc_desktop::platform::TextKind;

#[derive(Default)]
pub struct ImeBridge {
    focused: Option<egui::Id>,
    last: String,
}

impl ImeBridge {
    pub fn sync(
        &mut self,
        app: &AndroidApp,
        ctx: &egui::Context,
        resp: &egui::Response,
        value: &mut String,
        kind: TextKind,
    ) {
        if resp.has_focus() {
            if self.focused != Some(resp.id) {
                // Focus gained: keyboard type, then seed the IME with the current value.
                self.focused = Some(resp.id);
                let input = match kind {
                    TextKind::Address => {
                        InputType::TYPE_CLASS_TEXT | InputType::TYPE_TEXT_VARIATION_URI
                    }
                    TextKind::Text => {
                        InputType::TYPE_CLASS_TEXT | InputType::TYPE_TEXT_FLAG_CAP_WORDS
                    }
                };
                app.set_ime_editor_info(input, TextInputAction::Done, ImeOptions::empty());
                self.push(app, value);
                return;
            }
            let state = app.text_input_state();
            if state.text != self.last {
                // The IME edited the text (soft keyboard, autocorrect).
                *value = state.text.clone();
                self.last = state.text;
                let n = value.chars().count();
                if let Some(mut te) = egui::TextEdit::load_state(ctx, resp.id) {
                    te.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                        egui::text::CCursor::new(n),
                    )));
                    te.store(ctx, resp.id);
                }
                ctx.request_repaint();
            } else if *value != self.last {
                // egui edited the text (hardware keys, `adb shell input`): tell the IME.
                self.push(app, value);
            }
        } else if self.focused == Some(resp.id) {
            self.focused = None;
        }
    }

    fn push(&mut self, app: &AndroidApp, value: &str) {
        self.last = value.to_owned();
        let n = value.chars().count();
        app.set_text_input_state(TextInputState {
            text: value.to_owned(),
            selection: TextSpan { start: n, end: n },
            compose_region: None,
        });
    }
}
