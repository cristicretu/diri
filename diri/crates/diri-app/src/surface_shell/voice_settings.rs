//! Settings > Voice, the parts beyond the toggles: the speech
//! command a "Custom command" voice pipes readings to, what takes the spoken
//! answer (Diri, Wispr Flow, or any dictation app's shortcut), and that
//! shortcut's recorder.
use super::*;
use crate::store::{DictationShortcut, TalkAnswer};
use gpui::{Modifiers, Subscription};

#[derive(Default)]
pub(super) struct VoiceFieldsState {
    command: QueryEditor,
    recording_shortcut: bool,
    /// Swallows key bindings while a shortcut is being recorded, so ⌘K is
    /// captured instead of opening the palette.
    recording_guard: Option<Subscription>,
    /// Which recorded modifier is showing its Left / Right / Any menu.
    side_menu: Option<usize>,
    /// Keeps the chord from running while it is recorded. On macOS this is
    /// a session event tap when Accessibility is already granted, and a
    /// local monitor otherwise.
    #[cfg(target_os = "macos")]
    capture: Option<crate::macos::shortcut_capture::Capture>,
    gesture: DictationGesture,
}

const ANSWER_OPTIONS: [TalkAnswer; 3] = [TalkAnswer::Diri, TalkAnswer::WisprFlow, TalkAnswer::Shortcut];

fn tabs_label(all: bool) -> &'static str {
    t(if all {
        "settings.general.talk_tabs_all"
    } else {
        "settings.general.talk_tabs_picked"
    })
}

fn answer_label(answer: TalkAnswer) -> &'static str {
    match answer {
        TalkAnswer::Diri => t("settings.general.talk_answer_diri"),
        TalkAnswer::WisprFlow => t("settings.general.talk_answer_wispr"),
        TalkAnswer::Shortcut => t("settings.general.talk_answer_shortcut"),
    }
}

impl UtilitySurfaces {
    pub(super) fn deactivate_voice_fields(&mut self) {
        self.active_field.release(SettingsField::VoiceCommand);
        self.stop_dictation_recording();
    }

    pub(crate) fn is_recording_dictation_shortcut(&self) -> bool {
        self.voice_fields.recording_shortcut
    }

    /// Recording belongs to the Voice page. Once that page is gone, the
    /// capture must not keep taking keys.
    fn dictation_recorder_visible(&self) -> bool {
        self.surface == Surface::Settings && self.settings_tab == SettingsTab::Voice
    }

    /// Typing into the speech command, or recording the dictation shortcut.
    pub(super) fn handle_voice_fields_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.surface != Surface::Settings || self.settings_tab != SettingsTab::Voice {
            return false;
        }
        if self.voice_fields.side_menu.is_some() && event.keystroke.key == "escape" {
            self.voice_fields.side_menu = None;
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        if self.voice_fields.recording_shortcut {
            if !event.is_held {
                self.record_dictation_keystroke(&event.keystroke, cx);
            }
            cx.stop_propagation();
            return true;
        }
        if !self.active_field.is(SettingsField::VoiceCommand) {
            return false;
        }
        let key = &event.keystroke;
        match key.key.as_str() {
            "escape" | "enter" | "tab" => self.active_field.release(SettingsField::VoiceCommand),
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

    /// A bare modifier is not a key-down. Left and right Option arrive as
    /// flags-changed with different hardware codes; record that on release
    /// when no other key completed the shortcut.
    pub(super) fn dictation_modifiers_changed(
        &mut self,
        event: &gpui::ModifiersChangedEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.surface != Surface::Settings
            || self.settings_tab != SettingsTab::Voice
            || !self.voice_fields.recording_shortcut
        {
            return false;
        }
        let (key_code, flags) = physical_key();
        let outcome = self
            .voice_fields
            .gesture
            .on_flags(event.modifiers, key_code, flags);
        self.apply_dictation_outcome(outcome, cx);
        cx.stop_propagation();
        cx.notify();
        true
    }

    fn arm_dictation_recording(&mut self, cx: &mut Context<Self>) {
        self.stop_dictation_recording();
        self.voice_fields.recording_shortcut = true;
        let entity = cx.entity();
        // Stopping propagation here skips bindings and key listeners alike,
        // so the key is recorded from the interceptor itself.
        self.voice_fields.recording_guard = Some(cx.intercept_keystrokes(move |event, _window, cx| {
            if !entity.read(cx).voice_fields.recording_shortcut {
                return;
            }
            cx.stop_propagation();
            let keystroke = event.keystroke.clone();
            entity.update(cx, |this, cx| this.record_dictation_keystroke(&keystroke, cx));
        }));
        #[cfg(target_os = "macos")]
        if let Some((capture, mut events)) = crate::macos::shortcut_capture::begin() {
            self.voice_fields.capture = Some(capture);
            cx.spawn(async move |this, cx| {
                while let Some(event) = events.recv().await {
                    if this
                        .update(cx, |this, cx| this.note_captured_key(event, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
    }

    fn record_dictation_keystroke(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) {
        if !self.dictation_recorder_visible() {
            self.stop_dictation_recording();
            cx.notify();
            return;
        }
        let (key_code, flags) = physical_key();
        let outcome =
            self.voice_fields
                .gesture
                .on_key(&keystroke.key, keystroke.modifiers, key_code, flags);
        self.apply_dictation_outcome(outcome, cx);
        cx.notify();
    }

    #[cfg(target_os = "macos")]
    fn note_captured_key(
        &mut self,
        event: crate::macos::shortcut_capture::CapturedKey,
        cx: &mut Context<Self>,
    ) {
        if !self.voice_fields.recording_shortcut {
            return;
        }
        if !self.dictation_recorder_visible() {
            self.stop_dictation_recording();
            cx.notify();
            return;
        }
        let modifiers = modifiers_from_bits(event.flags);
        let outcome = match route_captured(event.flags_changed, event.code, event.letters) {
            CapturedRoute::Flags => {
                if event.repeat {
                    return;
                }
                self.voice_fields
                    .gesture
                    .on_flags(modifiers, Some(event.code), event.flags)
            }
            CapturedRoute::Key { name, code } => self
                .voice_fields
                .gesture
                .on_key(&name, modifiers, Some(code), event.flags),
        };
        self.apply_dictation_outcome(outcome, cx);
        cx.notify();
    }

    pub(super) fn stop_dictation_recording(&mut self) {
        self.voice_fields.recording_shortcut = false;
        self.voice_fields.recording_guard = None;
        self.voice_fields.side_menu = None;
        #[cfg(target_os = "macos")]
        {
            self.voice_fields.capture = None;
        }
        self.voice_fields.gesture = DictationGesture::default();
    }

    fn apply_dictation_outcome(&mut self, outcome: GestureOutcome, cx: &mut Context<Self>) {
        match outcome {
            GestureOutcome::Listening => return,
            GestureOutcome::Cancel => self.stop_dictation_recording(),
            GestureOutcome::Clear => {
                self.stop_dictation_recording();
                self.update_prefs(|prefs| prefs.talk_answer_shortcut = None);
            }
            GestureOutcome::Record(shortcut) => {
                self.stop_dictation_recording();
                self.update_prefs(move |prefs| prefs.talk_answer_shortcut = Some(shortcut));
            }
        }
        cx.notify();
    }

    pub(super) fn voice_command_field(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.settings_colors();
        let active = self.active_field.is(SettingsField::VoiceCommand);
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
                this.stop_dictation_recording();
                this.voice_fields.command = text_editor(&saved);
                this.voice_fields.command.select_all();
                this.active_field = SettingsField::VoiceCommand;
                this.focus.focus(window, cx);
                cx.notify();
            }))
            .child(content)
    }

    pub(super) fn dictation_shortcut_field(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.settings_colors();
        let recording = self.voice_fields.recording_shortcut;
        let live = recording
            .then(|| self.voice_fields.gesture.preview())
            .flatten();
        let saved = self.prefs.talk_answer_shortcut.is_some();
        let begin = cx.listener(|this, _, window, cx| {
            this.deactivate_privacy_name();
            this.active_field.release(SettingsField::VoiceCommand);
            this.arm_dictation_recording(cx);
            this.active_field.release(SettingsField::Search);
            this.focus.focus(window, cx);
            cx.notify();
        });
        // The saved chord is a row of menus, not a clipped text box, so the
        // Left / Right / Any list can open past the row.
        if !recording && live.is_none() && saved {
            return div()
                .id("dictation-shortcut-sides")
                .flex()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .on_click(begin)
                .child(self.dictation_shortcut_parts(colors, cx))
                .child(self.dictation_shortcut_clear(colors, cx))
                .into_any_element();
        }
        let child = if let Some(label) = live {
            div()
                .child(dictation_shortcut_label(&label))
                .into_any_element()
        } else {
            div()
                .text_color(if recording {
                    colors.primary
                } else {
                    colors.tertiary
                })
                .child(if recording {
                    t("settings.general.talk_shortcut_recording")
                } else {
                    t("settings.general.talk_shortcut_record")
                })
                .into_any_element()
        };
        settings_text_box("talk-dictation-shortcut", 236.0, recording, colors)
            .cursor_pointer()
            .on_click(begin)
            .child(child)
            .into_any_element()
    }

    /// Each modifier opens a Left / Right / Any menu. The other key, and the
    /// gaps around these menus, start a new recording.
    fn dictation_shortcut_parts(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(shortcut) = self.prefs.talk_answer_shortcut.clone() else {
            return div().into_any_element();
        };
        let parts: Vec<String> = shortcut.label.split('-').map(str::to_owned).collect();
        let mut row = div().flex().items_center().gap(px(4.0));
        for (index, part) in parts.iter().enumerate() {
            if index > 0 {
                row = row.child(div().text_color(colors.tertiary).child("+"));
            }
            row = row.child(self.dictation_side_menu(index, part, colors, cx));
        }
        row.into_any_element()
    }

    fn dictation_side_menu(
        &self,
        index: usize,
        token: &str,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let open = self.voice_fields.side_menu == Some(index);
        let current = token_side(token).flatten();
        let mut control = div()
            .relative()
            .child(self.dictation_side_button(index, &part_symbol(token), open, colors, cx));
        if open {
            let mut options = div().p(px(4.0)).flex().flex_col();
            if token_side(token).is_some() {
                options = options
                    .child(self.dictation_side_choice(index, None, current.is_none(), colors, cx))
                    .child(self.dictation_side_choice(
                        index,
                        Some(KeySide::Left),
                        current == Some(KeySide::Left),
                        colors,
                        cx,
                    ))
                    .child(self.dictation_side_choice(
                        index,
                        Some(KeySide::Right),
                        current == Some(KeySide::Right),
                        colors,
                        cx,
                    ));
            }
            options = options.child(settings_choice_row(
                format!("dictation-side-{index}-remove"),
                t("settings.general.talk_side_remove"),
                false,
                colors,
                cx,
                move |this, cx| {
                    this.voice_fields.side_menu = None;
                    this.remove_dictation_part(index);
                    cx.stop_propagation();
                    cx.notify();
                },
            ));
            control = control.child(settings_dropdown(options, 148.0, colors));
        }
        control.into_any_element()
    }

    fn dictation_side_button(
        &self,
        index: usize,
        label: &str,
        open: bool,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .id(SharedString::from(format!("dictation-side-{index}")))
            .h(px(26.0))
            .px(px(8.0))
            .rounded(px(Radius::BADGE))
            .border_1()
            .border_color(colors.primary.alpha(if open { 0.20 } else { 0.10 }))
            .bg(colors.primary.alpha(if open { 0.08 } else { 0.04 }))
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.voice_fields.side_menu = if this.voice_fields.side_menu == Some(index) {
                    None
                } else {
                    Some(index)
                };
                this.settings_menu = None;
                cx.stop_propagation();
                cx.notify();
            }))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(colors.primary)
                    .child(label.to_owned()),
            )
            .child(sf_symbol_weighted(
                if open { "chevron.up" } else { "chevron.down" },
                8.0,
                SymbolWeight::Semibold,
                colors.tertiary,
            ))
            .into_any_element()
    }

    fn dictation_side_choice(
        &self,
        index: usize,
        side: Option<KeySide>,
        selected: bool,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let label = match side {
            None => t("settings.general.talk_side_any"),
            Some(KeySide::Left) => t("settings.general.talk_side_left"),
            Some(KeySide::Right) => t("settings.general.talk_side_right"),
        };
        settings_choice_row(
            format!("dictation-side-{index}-{:?}", side),
            label,
            selected,
            colors,
            cx,
            move |this, cx| {
                this.voice_fields.side_menu = None;
                this.set_dictation_side(index, side);
                cx.stop_propagation();
                cx.notify();
            },
        )
        .into_any_element()
    }

    fn dictation_shortcut_clear(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .id("clear-dictation-shortcut")
            .size(px(26.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Radius::BADGE))
            .cursor_pointer()
            .hover(|style| style.bg(colors.primary.alpha(0.07)))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, _, cx| {
                this.voice_fields.side_menu = None;
                this.stop_dictation_recording();
                this.update_prefs(|prefs| prefs.talk_answer_shortcut = None);
                cx.stop_propagation();
                cx.notify();
            }))
            .child(sf_symbol("xmark", 10.0, colors.tertiary))
            .into_any_element()
    }

    fn set_dictation_side(&mut self, index: usize, side: Option<KeySide>) {
        let Some(shortcut) = self.prefs.talk_answer_shortcut.clone() else {
            return;
        };
        let Some(next) = shortcut_with_side(&shortcut, index, side) else {
            return;
        };
        self.update_prefs(move |prefs| prefs.talk_answer_shortcut = Some(next));
    }

    fn remove_dictation_part(&mut self, index: usize) {
        let Some(shortcut) = self.prefs.talk_answer_shortcut.clone() else {
            return;
        };
        match shortcut_without_part(&shortcut, index) {
            None => {
                self.update_prefs(|prefs| prefs.talk_answer_shortcut = None);
            }
            Some(next) => {
                self.update_prefs(move |prefs| prefs.talk_answer_shortcut = Some(next));
            }
        }
    }

    /// Which tabs are read: every tab, or only the ones turned on with a
    /// tab's speaker or ⌘K. Switching starts over from no exceptions.
    pub(super) fn talk_tabs_dropdown(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.settings_colors();
        let open = self.settings_menu == Some(SettingsMenu::TalkTabs);
        let mut control = div()
            .relative()
            .min_w(px(154.0))
            .child(settings_select_button(
                tabs_label(self.prefs.talk_reads_by_default),
                "talk-tabs-dropdown",
                open,
                SettingsMenu::TalkTabs,
                colors,
                cx,
            ));
        if open {
            control = control.child(self.settings_menu_host(cx));
        }
        control.into_any_element()
    }

    pub(super) fn talk_tabs_options(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut options = div().p(px(4.0)).flex().flex_col();
        for (index, all) in [true, false].into_iter().enumerate() {
            options = options.child(settings_choice_row(
                format!("talk-tabs-option-{index}"),
                tabs_label(all),
                all == self.prefs.talk_reads_by_default,
                colors,
                cx,
                move |this, cx| {
                    this.settings_menu = None;
                    if all != this.prefs.talk_reads_by_default {
                        this.update_prefs(move |prefs| {
                            prefs.talk_reads_by_default = all;
                            prefs.talk_exceptions.clear();
                        });
                    }
                    cx.notify();
                },
            ));
        }
        options.into_any_element()
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModifierKind {
    Control,
    Alt,
    Shift,
    Command,
    Function,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeySide {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HeldModifier {
    code: u16,
    kind: ModifierKind,
    side: Option<KeySide>,
}

impl HeldModifier {
    fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            0x3B => Self { code, kind: ModifierKind::Control, side: Some(KeySide::Left) },
            0x3E => Self { code, kind: ModifierKind::Control, side: Some(KeySide::Right) },
            0x3A => Self { code, kind: ModifierKind::Alt, side: Some(KeySide::Left) },
            0x3D => Self { code, kind: ModifierKind::Alt, side: Some(KeySide::Right) },
            0x38 => Self { code, kind: ModifierKind::Shift, side: Some(KeySide::Left) },
            0x3C => Self { code, kind: ModifierKind::Shift, side: Some(KeySide::Right) },
            0x37 => Self { code, kind: ModifierKind::Command, side: Some(KeySide::Left) },
            0x36 => Self { code, kind: ModifierKind::Command, side: Some(KeySide::Right) },
            0x3F => Self { code, kind: ModifierKind::Function, side: None },
            _ => return None,
        })
    }

    /// The device bits tell the two sides apart, so letting go of one
    /// Option while the other is held is a release. Without them, only the
    /// logical flag is known.
    fn is_down(self, modifiers: Modifiers, flags: u64) -> bool {
        if let Some((left, right)) = device_bits(self.kind)
            && flags & (left | right) != 0
        {
            return match self.side {
                Some(KeySide::Left) => flags & left != 0,
                Some(KeySide::Right) => flags & right != 0,
                None => true,
            };
        }
        match self.kind {
            ModifierKind::Control => modifiers.control,
            ModifierKind::Alt => modifiers.alt,
            ModifierKind::Shift => modifiers.shift,
            ModifierKind::Command => modifiers.platform,
            ModifierKind::Function => modifiers.function,
        }
    }
}

#[derive(Default)]
struct DictationGesture {
    held: Vec<HeldModifier>,
}

#[derive(Debug)]
enum GestureOutcome {
    Listening,
    Cancel,
    Clear,
    Record(DictationShortcut),
}

impl DictationGesture {
    fn on_flags(&mut self, modifiers: Modifiers, key_code: Option<u16>, flags: u64) -> GestureOutcome {
        if let Some(held) = key_code.and_then(HeldModifier::from_code)
            && held.is_down(modifiers, flags)
        {
            if !self.held.iter().any(|item| item.code == held.code) {
                self.held.push(held);
            }
            return GestureOutcome::Listening;
        }
        if !modifiers.modified() && !self.held.is_empty() {
            return GestureOutcome::Record(shortcut_from_held(&self.held));
        }
        GestureOutcome::Listening
    }

    fn preview(&self) -> Option<String> {
        if self.held.is_empty() {
            None
        } else {
            Some(shortcut_from_held(&self.held).label)
        }
    }

    fn on_key(
        &mut self,
        key: &str,
        modifiers: Modifiers,
        key_code: Option<u16>,
        flags: u64,
    ) -> GestureOutcome {
        if key == "escape" {
            return GestureOutcome::Cancel;
        }
        if matches!(key, "backspace" | "delete") && !modifiers.modified() {
            return GestureOutcome::Clear;
        }
        if is_modifier_name(key) || key_code.is_some_and(|code| HeldModifier::from_code(code).is_some())
        {
            return GestureOutcome::Listening;
        }
        let Some(key_code) = key_code else {
            return GestureOutcome::Listening;
        };
        GestureOutcome::Record(DictationShortcut {
            label: label_for_key(key, modifiers, &self.held, flags),
            key_code,
        })
    }
}

fn shortcut_from_held(held: &[HeldModifier]) -> DictationShortcut {
    let key_code = held.last().map(|item| item.code).unwrap_or_default();
    let label = held
        .iter()
        .copied()
        .map(modifier_token)
        .collect::<Vec<_>>()
        .join("-");
    DictationShortcut { label, key_code }
}

fn label_for_key(key: &str, modifiers: Modifiers, held: &[HeldModifier], flags: u64) -> String {
    let mut tokens = modifier_tokens(held, modifiers, flags);
    tokens.push(key);
    tokens.join("-")
}

/// Every held key is kept, so left Option and right Option can both be in
/// the shortcut. A modifier that is down but was not seen as its own key
/// is added once, from the flags.
fn modifier_tokens(held: &[HeldModifier], modifiers: Modifiers, flags: u64) -> Vec<&'static str> {
    let mut tokens: Vec<&str> = held.iter().copied().map(modifier_token).collect();
    for (down, kind) in [
        (modifiers.function, ModifierKind::Function),
        (modifiers.control, ModifierKind::Control),
        (modifiers.alt, ModifierKind::Alt),
        (modifiers.platform, ModifierKind::Command),
        (modifiers.shift, ModifierKind::Shift),
    ] {
        if down && !held.iter().any(|item| item.kind == kind) {
            tokens.push(sided_token(kind, held, flags));
        }
    }
    tokens
}

fn sided_token(kind: ModifierKind, held: &[HeldModifier], flags: u64) -> &'static str {
    let side = held
        .iter()
        .rev()
        .find(|item| item.kind == kind)
        .and_then(|item| item.side)
        .or_else(|| side_from_flags(kind, flags));
    modifier_token(HeldModifier {
        code: 0,
        kind,
        side,
    })
}

fn modifier_token(held: HeldModifier) -> &'static str {
    match (held.kind, held.side) {
        (ModifierKind::Control, Some(KeySide::Left)) => "leftctrl",
        (ModifierKind::Control, Some(KeySide::Right)) => "rightctrl",
        (ModifierKind::Control, None) => "ctrl",
        (ModifierKind::Alt, Some(KeySide::Left)) => "leftalt",
        (ModifierKind::Alt, Some(KeySide::Right)) => "rightalt",
        (ModifierKind::Alt, None) => "alt",
        (ModifierKind::Shift, Some(KeySide::Left)) => "leftshift",
        (ModifierKind::Shift, Some(KeySide::Right)) => "rightshift",
        (ModifierKind::Shift, None) => "shift",
        (ModifierKind::Command, Some(KeySide::Left)) => "leftcmd",
        (ModifierKind::Command, Some(KeySide::Right)) => "rightcmd",
        (ModifierKind::Command, None) => "cmd",
        (ModifierKind::Function, _) => "fn",
    }
}

/// The left and right device bits macOS sets in an event's modifier flags.
fn device_bits(kind: ModifierKind) -> Option<(u64, u64)> {
    Some(match kind {
        ModifierKind::Control => (0x1, 0x2000),
        ModifierKind::Alt => (0x20, 0x40),
        ModifierKind::Shift => (0x2, 0x4),
        ModifierKind::Command => (0x8, 0x10),
        ModifierKind::Function => return None,
    })
}

fn side_from_flags(kind: ModifierKind, flags: u64) -> Option<KeySide> {
    let (left, right) = device_bits(kind)?;
    match (flags & left != 0, flags & right != 0) {
        (true, false) => Some(KeySide::Left),
        (false, true) => Some(KeySide::Right),
        _ => None,
    }
}

fn is_modifier_name(key: &str) -> bool {
    matches!(
        key,
        "shift" | "control" | "ctrl" | "alt" | "platform" | "function" | "fn"
    )
}

fn physical_key() -> (Option<u16>, u64) {
    #[cfg(target_os = "macos")]
    {
        gpui_macos::current_physical_key()
            .map(|(code, flags)| (Some(code), flags))
            .unwrap_or((None, 0))
    }
    #[cfg(not(target_os = "macos"))]
    {
        (None, 0)
    }
}

/// How the recorded shortcut reads in the field, with a plus between keys.
fn dictation_shortcut_label(label: &str) -> String {
    label.split('-').map(part_symbol).collect::<Vec<_>>().join(" + ")
}

fn part_symbol(part: &str) -> String {
    sided_symbol(part)
        .or_else(|| generic_symbol(part).map(str::to_owned))
        .unwrap_or_else(|| key_symbol(part))
}

/// `None` is either physical key. `None` returned from the function means
/// this token is not a modifier with a left and a right.
fn token_side(token: &str) -> Option<Option<KeySide>> {
    Some(match token {
        "leftcmd" | "leftalt" | "leftshift" | "leftctrl" => Some(KeySide::Left),
        "rightcmd" | "rightalt" | "rightshift" | "rightctrl" => Some(KeySide::Right),
        "cmd" | "alt" | "shift" | "ctrl" => None,
        _ => return None,
    })
}

fn token_with_side(token: &str, side: Option<KeySide>) -> Option<&'static str> {
    let kind = token_kind(token)?;
    if kind == ModifierKind::Function {
        return None;
    }
    Some(modifier_token(HeldModifier {
        code: 0,
        kind,
        side,
    }))
}

fn shortcut_with_side(
    shortcut: &DictationShortcut,
    index: usize,
    side: Option<KeySide>,
) -> Option<DictationShortcut> {
    let parts: Vec<String> = shortcut.label.split('-').map(str::to_owned).collect();
    let previous = parts.get(index)?.clone();
    let next = token_with_side(&previous, side)?;
    let kind = token_kind(&previous);
    let mut kept = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i == index {
            kept.push(next.to_owned());
        } else if side.is_none() && kind.is_some_and(|kind| token_kind(part) == Some(kind)) {
            continue;
        } else {
            kept.push(part.clone());
        }
    }
    Some(DictationShortcut {
        label: kept.join("-"),
        key_code: key_code_after_edit(shortcut.key_code, &previous, &kept),
    })
}

/// `None` means nothing is left, so the shortcut is cleared.
fn shortcut_without_part(shortcut: &DictationShortcut, index: usize) -> Option<DictationShortcut> {
    let mut parts: Vec<String> = shortcut.label.split('-').map(str::to_owned).collect();
    let Some(removed) = parts.get(index).cloned() else {
        return Some(shortcut.clone());
    };
    parts.remove(index);
    if parts.is_empty() {
        return None;
    }
    Some(DictationShortcut {
        label: parts.join("-"),
        key_code: key_code_after_edit(shortcut.key_code, &removed, &parts),
    })
}

fn key_code_after_edit(key_code: u16, edited: &str, parts: &[String]) -> u16 {
    let code_kind = HeldModifier::from_code(key_code).map(|held| held.kind);
    // The code is the edited part when that part is the modifier it names,
    // or when it is the one key that is not a modifier.
    let code_was_edited = match token_kind(edited) {
        Some(kind) => code_kind == Some(kind),
        None => code_kind.is_none(),
    };
    if !code_was_edited {
        return key_code;
    }
    parts
        .iter()
        .rev()
        .find_map(|part| key_code_for_token(part))
        .unwrap_or(key_code)
}

fn token_kind(token: &str) -> Option<ModifierKind> {
    Some(match token {
        "leftctrl" | "rightctrl" | "ctrl" => ModifierKind::Control,
        "leftalt" | "rightalt" | "alt" => ModifierKind::Alt,
        "leftshift" | "rightshift" | "shift" => ModifierKind::Shift,
        "leftcmd" | "rightcmd" | "cmd" => ModifierKind::Command,
        "fn" => ModifierKind::Function,
        _ => return None,
    })
}

fn key_code_for_token(token: &str) -> Option<u16> {
    Some(match token {
        "leftctrl" | "ctrl" => 0x3B,
        "rightctrl" => 0x3E,
        "leftalt" | "alt" => 0x3A,
        "rightalt" => 0x3D,
        "leftshift" | "shift" => 0x38,
        "rightshift" => 0x3C,
        "leftcmd" | "cmd" => 0x37,
        "rightcmd" => 0x36,
        "fn" => 0x3F,
        _ => return None,
    })
}

#[cfg(target_os = "macos")]
fn modifiers_from_bits(flags: u64) -> Modifiers {
    Modifiers {
        shift: flags & 0x0002_0000 != 0,
        control: flags & 0x0004_0000 != 0,
        alt: flags & 0x0008_0000 != 0,
        platform: flags & 0x0010_0000 != 0,
        function: flags & 0x0080_0000 != 0,
    }
}

#[cfg(any(test, target_os = "macos"))]
enum CapturedRoute {
    Flags,
    Key { name: String, code: u16 },
}

/// A letter typed with both Command keys sometimes arrives as a modifier
/// event or a flags change. If it names a key, record that key.
#[cfg(any(test, target_os = "macos"))]
fn route_captured(flags_changed: bool, code: u16, letters: Option<char>) -> CapturedRoute {
    // Both Command keys down can report the letter on the command key's code.
    if HeldModifier::from_code(code).is_some() {
        if let Some(letter) = letters.filter(|letter| letter.is_ascii_alphanumeric()) {
            let name = letter.to_ascii_lowercase();
            let code = ansi_code_for_char(name).unwrap_or(code);
            return CapturedRoute::Key {
                name: name.to_string(),
                code,
            };
        }
        return CapturedRoute::Flags;
    }
    if flags_changed && letters.is_none_or(|letter| !letter.is_ascii_alphanumeric()) {
        return CapturedRoute::Flags;
    }
    let name = ansi_key_name(code)
        .map(str::to_owned)
        .or_else(|| {
            letters
                .filter(|letter| letter.is_ascii_alphanumeric())
                .map(|letter| letter.to_ascii_lowercase().to_string())
        })
        .or_else(|| special_key_name(code).map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    CapturedRoute::Key { name, code }
}

#[cfg(any(test, target_os = "macos"))]
fn special_key_name(code: u16) -> Option<&'static str> {
    Some(match code {
        0x35 => "escape",
        0x33 => "backspace",
        0x75 => "delete",
        0x24 | 0x4C => "enter",
        0x30 => "tab",
        0x31 => "space",
        0x7B => "left",
        0x7C => "right",
        0x7D => "down",
        0x7E => "up",
        0x73 => "home",
        0x77 => "end",
        0x74 => "pageup",
        0x79 => "pagedown",
        0x7A => "f1",
        0x78 => "f2",
        0x63 => "f3",
        0x76 => "f4",
        0x60 => "f5",
        0x61 => "f6",
        0x62 => "f7",
        0x64 => "f8",
        0x65 => "f9",
        0x6D => "f10",
        0x67 => "f11",
        0x6F => "f12",
        0x69 => "f13",
        0x6B => "f14",
        0x71 => "f15",
        0x6A => "f16",
        0x40 => "f17",
        0x4F => "f18",
        0x50 => "f19",
        0x5A => "f20",
        // Named, not typed, because `-` separates the label's parts.
        0x1B => "minus",
        0x18 => "=",
        0x21 => "[",
        0x1E => "]",
        0x2A => "\\",
        0x29 => ";",
        0x27 => "'",
        0x2B => ",",
        0x2F => ".",
        0x2C => "/",
        0x32 => "`",
        _ => return None,
    })
}

/// Hardware key codes are positions, so Option+O stays `o` instead of Ó.
#[cfg(any(test, target_os = "macos"))]
fn ansi_key_name(code: u16) -> Option<&'static str> {
    Some(match code {
        0x00 => "a",
        0x0B => "b",
        0x08 => "c",
        0x02 => "d",
        0x0E => "e",
        0x03 => "f",
        0x05 => "g",
        0x04 => "h",
        0x22 => "i",
        0x26 => "j",
        0x28 => "k",
        0x25 => "l",
        0x2E => "m",
        0x2D => "n",
        0x1F => "o",
        0x23 => "p",
        0x0C => "q",
        0x0F => "r",
        0x01 => "s",
        0x11 => "t",
        0x20 => "u",
        0x09 => "v",
        0x0D => "w",
        0x07 => "x",
        0x10 => "y",
        0x06 => "z",
        0x1D => "0",
        0x12 => "1",
        0x13 => "2",
        0x14 => "3",
        0x15 => "4",
        0x17 => "5",
        0x16 => "6",
        0x1A => "7",
        0x1C => "8",
        0x19 => "9",
        _ => return None,
    })
}

#[cfg(any(test, target_os = "macos"))]
fn ansi_code_for_char(letter: char) -> Option<u16> {
    let letter = letter.to_ascii_lowercase();
    Some(match letter {
        'a' => 0x00,
        'b' => 0x0B,
        'c' => 0x08,
        'd' => 0x02,
        'e' => 0x0E,
        'f' => 0x03,
        'g' => 0x05,
        'h' => 0x04,
        'i' => 0x22,
        'j' => 0x26,
        'k' => 0x28,
        'l' => 0x25,
        'm' => 0x2E,
        'n' => 0x2D,
        'o' => 0x1F,
        'p' => 0x23,
        'q' => 0x0C,
        'r' => 0x0F,
        's' => 0x01,
        't' => 0x11,
        'u' => 0x20,
        'v' => 0x09,
        'w' => 0x0D,
        'x' => 0x07,
        'y' => 0x10,
        'z' => 0x06,
        '0' => 0x1D,
        '1' => 0x12,
        '2' => 0x13,
        '3' => 0x14,
        '4' => 0x15,
        '5' => 0x17,
        '6' => 0x16,
        '7' => 0x1A,
        '8' => 0x1C,
        '9' => 0x19,
        _ => return None,
    })
}

fn sided_symbol(part: &str) -> Option<String> {
    let side = token_side(part)??;
    let kind = token_kind(part)?;
    let symbol = generic_symbol(modifier_token(HeldModifier {
        code: 0,
        kind,
        side: None,
    }))?;
    let side = match side {
        KeySide::Left => t("settings.general.talk_side_left"),
        KeySide::Right => t("settings.general.talk_side_right"),
    };
    Some(format!("{side} {symbol}"))
}

fn generic_symbol(part: &str) -> Option<&'static str> {
    Some(match part {
        "fn" => "fn",
        "ctrl" => "⌃",
        "alt" => "⌥",
        "shift" => "⇧",
        "cmd" => "⌘",
        _ => return None,
    })
}

fn key_symbol(key: &str) -> String {
    match key {
        "backspace" => "⌫".into(),
        "delete" => "⌦".into(),
        "enter" => "↩".into(),
        "tab" => "⇥".into(),
        "escape" => "⎋".into(),
        "space" => "Space".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "minus" => "-".into(),
        "pageup" => "⇞".into(),
        "pagedown" => "⇟".into(),
        "home" => "↖".into(),
        "end" => "↘".into(),
        other => other.to_ascii_uppercase(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(alt: bool, command: bool) -> Modifiers {
        Modifiers {
            alt,
            platform: command,
            ..Modifiers::default()
        }
    }

    fn recorded(outcome: GestureOutcome) -> DictationShortcut {
        match outcome {
            GestureOutcome::Record(shortcut) => shortcut,
            other => panic!("expected a recorded shortcut, got {other:?}"),
        }
    }

    #[test]
    fn releasing_right_option_records_that_key() {
        let mut gesture = DictationGesture::default();
        assert!(matches!(
            gesture.on_flags(mods(true, false), Some(0x3D), 0),
            GestureOutcome::Listening
        ));
        let shortcut = recorded(gesture.on_flags(Modifiers::default(), Some(0x3D), 0));
        assert_eq!(shortcut.key_code, 0x3D);
        assert_eq!(shortcut.label, "rightalt");
        assert_eq!(shortcut.modifiers(), vec!["alt-right"]);
        assert_eq!(dictation_shortcut_label(&shortcut.label), "Right ⌥");
    }

    #[test]
    fn left_and_right_option_stay_distinct() {
        let mut left = DictationGesture::default();
        left.on_flags(mods(true, false), Some(0x3A), 0);
        let left = recorded(left.on_flags(Modifiers::default(), Some(0x3A), 0));
        let mut right = DictationGesture::default();
        right.on_flags(mods(true, false), Some(0x3D), 0);
        let right = recorded(right.on_flags(Modifiers::default(), Some(0x3D), 0));
        assert_eq!(left.key_code, 0x3A);
        assert_eq!(left.modifiers(), vec!["alt-left"]);
        assert_eq!(dictation_shortcut_label(&left.label), "Left ⌥");
        assert_ne!(left, right);
    }

    #[test]
    fn a_chord_keeps_the_option_side_from_the_held_key() {
        let mut gesture = DictationGesture::default();
        gesture.on_flags(mods(true, false), Some(0x3D), 0);
        let shortcut = recorded(gesture.on_key("d", mods(true, false), Some(2), 0));
        assert_eq!(shortcut.key_code, 2);
        assert_eq!(shortcut.label, "rightalt-d");
        assert_eq!(shortcut.modifiers(), vec!["alt-right"]);
        assert_eq!(dictation_shortcut_label(&shortcut.label), "Right ⌥ + D");
    }

    #[test]
    fn device_flags_distinguish_option_when_the_hold_was_missed() {
        let mut gesture = DictationGesture::default();
        let shortcut = recorded(gesture.on_key("d", mods(true, false), Some(2), 0x20));
        assert_eq!(shortcut.label, "leftalt-d");
        let mut other = DictationGesture::default();
        let other = recorded(other.on_key("d", mods(true, false), Some(2), 0x40));
        assert_eq!(other.label, "rightalt-d");
    }

    #[test]
    fn two_modifiers_record_when_the_last_one_is_released() {
        let mut gesture = DictationGesture::default();
        gesture.on_flags(mods(false, true), Some(0x37), 0);
        gesture.on_flags(mods(true, true), Some(0x3D), 0);
        assert!(matches!(
            gesture.on_flags(mods(true, false), Some(0x37), 0),
            GestureOutcome::Listening
        ));
        let shortcut = recorded(gesture.on_flags(Modifiers::default(), Some(0x3D), 0));
        assert_eq!(shortcut.key_code, 0x3D);
        assert_eq!(shortcut.label, "leftcmd-rightalt");
        assert_eq!(shortcut.modifiers(), vec!["cmd-left", "alt-right"]);
    }

    #[test]
    fn escape_cancels_and_a_missing_key_code_keeps_listening() {
        let mut gesture = DictationGesture::default();
        assert!(matches!(
            gesture.on_key("escape", Modifiers::default(), Some(53), 0),
            GestureOutcome::Cancel
        ));
        let mut gesture = DictationGesture::default();
        assert!(matches!(
            gesture.on_key("d", mods(true, false), None, 0x40),
            GestureOutcome::Listening
        ));
    }

    #[test]
    fn keys_are_shown_with_a_plus_as_they_are_held() {
        let mut gesture = DictationGesture::default();
        assert_eq!(gesture.preview(), None);
        gesture.on_flags(mods(true, false), Some(0x3D), 0);
        assert_eq!(
            gesture.preview().as_deref().map(dictation_shortcut_label),
            Some("Right ⌥".to_owned())
        );
        gesture.on_flags(mods(true, true), Some(0x37), 0);
        assert_eq!(
            gesture.preview().as_deref().map(dictation_shortcut_label),
            Some("Right ⌥ + Left ⌘".to_owned())
        );
    }

    #[test]
    fn a_modifier_can_be_set_to_left_right_or_any() {
        let shortcut = DictationShortcut {
            label: "leftcmd-d".into(),
            key_code: 2,
        };
        let any = shortcut_with_side(&shortcut, 0, None).expect("cmd has a side");
        assert_eq!(any.label, "cmd-d");
        assert_eq!(any.key_code, 2);
        let right = shortcut_with_side(&any, 0, Some(KeySide::Right)).expect("cmd has a side");
        assert_eq!(right.label, "rightcmd-d");
        let shortcut = DictationShortcut {
            label: "rightalt".into(),
            key_code: 0x3D,
        };
        let any = shortcut_with_side(&shortcut, 0, None).expect("option has a side");
        assert_eq!(any.label, "alt");
        assert_eq!(any.key_code, 0x3A);
        let left = shortcut_with_side(&any, 0, Some(KeySide::Left)).expect("option has a side");
        assert_eq!(left.label, "leftalt");
        assert_eq!(left.key_code, 0x3A);
    }

    #[test]
    fn both_option_keys_stay_until_one_is_set_to_any() {
        let mut gesture = DictationGesture::default();
        gesture.on_flags(mods(true, false), Some(0x3A), 0);
        gesture.on_flags(mods(true, false), Some(0x3D), 0);
        let shortcut = recorded(gesture.on_flags(Modifiers::default(), Some(0x3D), 0));
        assert_eq!(shortcut.label, "leftalt-rightalt");
        let any = shortcut_with_side(&shortcut, 0, None).expect("option has a side");
        assert_eq!(any.label, "alt");
        assert_eq!(dictation_shortcut_label(&any.label), "⌥");
    }

    #[test]
    fn removing_the_last_part_clears_the_shortcut() {
        let shortcut = DictationShortcut {
            label: "leftalt-o".into(),
            key_code: 0x1F,
        };
        let left = shortcut_without_part(&shortcut, 1).expect("the option remains");
        assert_eq!(left.label, "leftalt");
        // The key code has to follow, or the helper still presses Option+O.
        assert_eq!(left.key_code, 0x3A);
        assert!(shortcut_without_part(&left, 0).is_none());
    }

    #[test]
    fn a_letter_with_both_command_keys_is_still_a_letter() {
        // The command key's own code, but the character is the letter they hit.
        match route_captured(false, 0x37, Some('d')) {
            CapturedRoute::Key { name, code } => {
                assert_eq!(name, "d");
                assert_eq!(code, 0x02);
            }
            CapturedRoute::Flags => panic!("letter was treated as a modifier"),
        }
        match route_captured(true, 0x02, Some('d')) {
            CapturedRoute::Key { name, code } => {
                assert_eq!(name, "d");
                assert_eq!(code, 0x02);
            }
            CapturedRoute::Flags => panic!("letter was treated as a modifier"),
        }
        assert!(matches!(
            route_captured(true, 0x36, None),
            CapturedRoute::Flags
        ));
    }

    #[test]
    fn an_unsided_shortcut_keeps_each_key() {
        assert_eq!(dictation_shortcut_label("cmd-alt-d"), "⌘ + ⌥ + D");
    }

    #[test]
    fn releasing_one_option_while_the_other_is_held_is_a_release() {
        let mut gesture = DictationGesture::default();
        // Device bits: left Option 0x20, right Option 0x40.
        gesture.on_flags(mods(true, false), Some(0x3D), 0x40);
        gesture.on_flags(mods(true, false), Some(0x3A), 0x60);
        // Right Option goes up while left is still down.
        assert!(matches!(
            gesture.on_flags(mods(true, false), Some(0x3D), 0x20),
            GestureOutcome::Listening
        ));
        let shortcut = recorded(gesture.on_flags(Modifiers::default(), Some(0x3A), 0));
        assert_eq!(shortcut.label, "rightalt-leftalt");
        assert_eq!(shortcut.key_code, 0x3A);
    }

    #[test]
    fn function_and_punctuation_keys_are_named() {
        for (code, name) in [(0x60, "f5"), (0x2C, "/"), (0x1B, "minus")] {
            match route_captured(false, code, None) {
                CapturedRoute::Key { name: got, .. } => assert_eq!(got, name),
                CapturedRoute::Flags => panic!("{name} was treated as a modifier"),
            }
        }
        assert_eq!(dictation_shortcut_label("cmd-minus"), "⌘ + -");
        assert_eq!(dictation_shortcut_label("ctrl-f5"), "⌃ + F5");
    }
}
