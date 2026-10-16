//! Settings > General > Voice, the parts beyond the toggles: the speech
//! command a "Custom command" voice pipes readings to, what takes the spoken
//! answer (Diri, Wispr Flow, or any dictation app's shortcut), and that
//! shortcut's recorder.
use super::*;
use crate::store::{DictationShortcut, TalkAnswer};

#[derive(Default)]
pub(super) struct VoiceFieldsState {
    command: QueryEditor,
    command_active: bool,
    recording_shortcut: bool,
}

const ANSWER_OPTIONS: [TalkAnswer; 3] = [TalkAnswer::Diri, TalkAnswer::WisprFlow, TalkAnswer::Shortcut];

fn answer_label(answer: TalkAnswer) -> &'static str {
    match answer {
        TalkAnswer::Diri => t("settings.general.talk_answer_diri"),
        TalkAnswer::WisprFlow => t("settings.general.talk_answer_wispr"),
        TalkAnswer::Shortcut => t("settings.general.talk_answer_shortcut"),
    }
}

impl UtilitySurfaces {
    pub(super) fn deactivate_voice_fields(&mut self) {
        self.voice_fields.command_active = false;
        self.voice_fields.recording_shortcut = false;
    }

    /// Typing into the speech command, or recording the dictation shortcut.
    pub(super) fn handle_voice_fields_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.surface != Surface::Settings || self.settings_tab != SettingsTab::General {
            return false;
        }
        if self.voice_fields.recording_shortcut {
            self.record_dictation_shortcut(event, cx);
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        if !self.voice_fields.command_active {
            return false;
        }
        let key = &event.keystroke;
        match key.key.as_str() {
            "escape" | "enter" | "tab" => self.voice_fields.command_active = false,
            _ => {
                let Some(edit) = query_editor::edit_for(key) else {
                    return false;
                };
                let field = &mut self.voice_fields.command;
                match edit {
                    Edit::Local(local) => {
                        field.apply(local);
                    }
                    Edit::Clipboard(ClipboardEdit::Copy) => {
                        query_editor::copy_selection(field, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Cut) => {
                        query_editor::cut_selection(field, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Paste) => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                            field.insert(text.lines().next().unwrap_or_default());
                        }
                    }
                }
                let command = field.text().to_owned();
                self.update_prefs(move |prefs| prefs.talk_voice_command = command);
            }
        }
        cx.stop_propagation();
        cx.notify();
        true
    }

    /// Escape cancels, Delete clears, a bare modifier waits for the key.
    fn record_dictation_shortcut(&mut self, event: &KeyDownEvent, _cx: &mut Context<Self>) {
        let key = &event.keystroke;
        if key.key == "escape" {
            self.voice_fields.recording_shortcut = false;
            return;
        }
        if matches!(key.key.as_str(), "backspace" | "delete") && !key.modifiers.modified() {
            self.voice_fields.recording_shortcut = false;
            self.update_prefs(|prefs| prefs.talk_answer_shortcut = None);
            return;
        }
        if matches!(
            key.key.as_str(),
            "shift" | "control" | "ctrl" | "alt" | "platform" | "function" | "fn"
        ) {
            return;
        }
        #[cfg(target_os = "macos")]
        let key_code = gpui_macos::current_native_key_code(event);
        #[cfg(not(target_os = "macos"))]
        let key_code: Option<u16> = None;
        let Some(key_code) = key_code else {
            return;
        };
        self.voice_fields.recording_shortcut = false;
        let shortcut = DictationShortcut {
            label: key.unparse(),
            key_code,
        };
        self.update_prefs(move |prefs| prefs.talk_answer_shortcut = Some(shortcut));
    }

    pub(super) fn voice_command_field(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.settings_colors();
        let active = self.voice_fields.command_active;
        let saved = self.prefs.talk_voice_command.clone();
        let content = if active {
            query_label(&self.voice_fields.command)
        } else if saved.is_empty() {
            div()
                .text_color(colors.tertiary)
                .child(t("settings.general.talk_voice_command_placeholder"))
                .into_any_element()
        } else {
            div().child(saved.clone()).into_any_element()
        };
        settings_text_box("talk-voice-command", 240.0, active, colors)
            .font_family(SharedString::from(crate::fonts::mono_family()))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.deactivate_privacy_name();
                this.voice_fields.recording_shortcut = false;
                this.voice_fields.command = text_editor(&saved);
                this.voice_fields.command.select_all();
                this.voice_fields.command_active = true;
                this.roots_editor_active = false;
                this.include_editor_active = false;
                this.settings_search_active = false;
                this.focus.focus(window, cx);
                cx.notify();
            }))
            .child(content)
    }

    pub(super) fn dictation_shortcut_field(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.settings_colors();
        let recording = self.voice_fields.recording_shortcut;
        let label = if recording {
            t("settings.general.talk_shortcut_recording").to_owned()
        } else {
            self.prefs
                .talk_answer_shortcut
                .as_ref()
                .map(|shortcut| crate::commands::keystroke_label(&shortcut.label))
                .unwrap_or_else(|| t("settings.general.talk_shortcut_record").to_owned())
        };
        settings_text_box("talk-dictation-shortcut", 154.0, recording, colors)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, window, cx| {
                this.deactivate_privacy_name();
                this.voice_fields.command_active = false;
                this.voice_fields.recording_shortcut = true;
                this.settings_search_active = false;
                this.focus.focus(window, cx);
                cx.notify();
            }))
            .child(label)
    }

    pub(super) fn talk_answer_dropdown(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.settings_colors();
        let open = self.settings_menu == Some(SettingsMenu::TalkAnswer);
        let mut control = div()
            .relative()
            .min_w(px(154.0))
            .child(settings_select_button(
                answer_label(self.prefs.talk_answer),
                "talk-answer-dropdown",
                open,
                SettingsMenu::TalkAnswer,
                colors,
                cx,
            ));
        if open {
            control = control.child(self.settings_menu_host(cx));
        }
        control.into_any_element()
    }

    pub(super) fn talk_answer_options(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        #[cfg(target_os = "macos")]
        let wispr = crate::voice::wispr_flow_installed();
        #[cfg(not(target_os = "macos"))]
        let wispr = false;
        let mut options = div().p(px(4.0)).flex().flex_col();
        for (index, answer) in ANSWER_OPTIONS.into_iter().enumerate() {
            if answer == TalkAnswer::WisprFlow && !wispr {
                continue;
            }
            options = options.child(settings_choice_row(
                format!("talk-answer-option-{index}"),
                answer_label(answer),
                answer == self.prefs.talk_answer,
                colors,
                cx,
                move |this, cx| {
                    this.settings_menu = None;
                    this.update_prefs(move |prefs| prefs.talk_answer = answer);
                    cx.notify();
                },
            ));
        }
        options.into_any_element()
    }
}

/// The bordered box Settings text fields share.
fn settings_text_box(
    id: &'static str,
    width: f32,
    active: bool,
    colors: SemanticColors,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .debug_selector(move || id.into())
        .flex_none()
        .w(px(width))
        .h(px(26.0))
        .px(px(8.0))
        .rounded(px(Radius::BADGE))
        .border_1()
        .border_color(colors.primary.alpha(if active { 0.26 } else { 0.10 }))
        .bg(colors.primary.alpha(if active { 0.075 } else { 0.04 }))
        .flex()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_size(px(11.0))
        .text_color(colors.primary)
        .cursor(CursorStyle::IBeam)
}
