//! The optional tmux-style prefix layer (Settings › Keyboard Shortcuts).
//!
//! Off by default: then no key is taken from a terminal program. On, the
//! prefix (⌃A unless the preferences say otherwise) arms the layer for one
//! keystroke, which runs a verb from [`BINDINGS`] instead of reaching the
//! terminal. The prefix itself never reaches the PTY; pressed twice it types
//! itself once. Escape, an unbound key or [`PREFIX_TIMEOUT`] cancel.
//!
//! The layer is a keystroke interceptor in the root view, so it runs before
//! any key binding and before the focused terminal's key handler. This module
//! is the pure decision; `RootView` carries out what it returns.

use std::time::{Duration, Instant};

use diri_ui::{Radius, SemanticColors, Typo};
use gpui::{AnyElement, FontWeight, Keystroke, div, prelude::*, px};

use crate::commands::CommandId;
use crate::i18n::{t, tf};

/// The prefix while the preferences name none: screen's and Harness's ⌃A.
pub(crate) const DEFAULT_PREFIX: &str = "ctrl-a";

/// An armed prefix gives up after this long without a second key.
pub(crate) const PREFIX_TIMEOUT: Duration = Duration::from_secs(2);

/// How long `prefix q` keeps the pane numbers up (tmux's
/// `display-panes-time` is one second; a little longer reads better).
pub(crate) const PANE_NUMBERS_TIME: Duration = Duration::from_millis(1800);

/// What the second key of a prefix sequence does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Verb {
    /// A command from the registry, exactly as the palette runs it.
    Command(CommandId),
    /// `x`: close the focused pane when it is a shell pane. An Agent's pane
    /// is its tab; it is never closed from here.
    KillPane,
}

/// One prefix-table entry.
pub(crate) struct Binding {
    /// The second key as GPUI names it (`key_char` for printable keys).
    pub key: &'static str,
    pub shift: bool,
    pub verb: Verb,
    /// Keeps the prefix armed after it runs, like tmux's `bind -r`, so a
    /// held resize keeps nudging.
    pub repeat: bool,
}

const fn bind(key: &'static str, verb: Verb) -> Binding {
    Binding {
        key,
        shift: false,
        verb,
        repeat: false,
    }
}

const fn command(key: &'static str, id: CommandId) -> Binding {
    bind(key, Verb::Command(id))
}

const fn resize(key: &'static str, id: CommandId) -> Binding {
    Binding {
        key,
        shift: true,
        verb: Verb::Command(id),
        repeat: true,
    }
}

/// The prefix table: tmux's defaults where tmux has one, Harness's
/// otherwise (`S` sync, `m`/`j` mark and join).
pub(crate) const BINDINGS: &[Binding] = &[
    command("%", CommandId::SplitPaneRight),
    command("\"", CommandId::SplitPaneBelow),
    command("left", CommandId::FocusPaneLeft),
    command("right", CommandId::FocusPaneRight),
    command("up", CommandId::FocusPaneUp),
    command("down", CommandId::FocusPaneDown),
    command("o", CommandId::FocusNextPane),
    command(";", CommandId::FocusPreviousPane),
    command("z", CommandId::TogglePaneZoom),
    bind("x", Verb::KillPane),
    resize("left", CommandId::PaneShrinkWidth),
    resize("right", CommandId::PaneGrowWidth),
    resize("up", CommandId::PaneShrinkHeight),
    resize("down", CommandId::PaneGrowHeight),
    command("q", CommandId::ShowPaneNumbers),
    command(" ", CommandId::NextLayout),
    command("!", CommandId::BreakPane),
    command("m", CommandId::MarkPane),
    command("j", CommandId::JoinPane),
    command("S", CommandId::ToggleSynchronizePanes),
    command("?", CommandId::ShowMultiplexerKeys),
];

/// One cheatsheet line: the keys after the prefix and what they do.
pub(crate) struct SheetRow {
    pub keys: &'static str,
    /// An i18n id: a command's own title where there is one.
    pub title: &'static str,
}

/// The cheatsheet, in reading groups. Built from [`BINDINGS`]: the test
/// `every_binding_is_on_the_cheatsheet` keeps them in step.
pub(crate) const SHEET: &[(&str, &[SheetRow])] = &[
    (
        "multiplexer.sheet.panes",
        &[
            SheetRow {
                keys: "%",
                title: "command.split_pane_right.title",
            },
            SheetRow {
                keys: "\"",
                title: "command.split_pane_below.title",
            },
            SheetRow {
                keys: "← → ↑ ↓",
                title: "multiplexer.focus_direction",
            },
            SheetRow {
                keys: "o  ;",
                title: "multiplexer.cycle_panes",
            },
            SheetRow {
                keys: "z",
                title: "command.toggle_pane_zoom.title",
            },
            SheetRow {
                keys: "x",
                title: "multiplexer.kill_pane",
            },
            SheetRow {
                keys: "⇧← ⇧→ ⇧↑ ⇧↓",
                title: "multiplexer.resize",
            },
            SheetRow {
                keys: "q",
                title: "command.show_pane_numbers.title",
            },
            SheetRow {
                keys: "S",
                title: "command.toggle_synchronize_panes.title",
            },
        ],
    ),
    (
        "multiplexer.sheet.layouts",
        &[SheetRow {
            keys: "Space",
            title: "command.next_layout.title",
        }],
    ),
    (
        "multiplexer.sheet.tabs",
        &[
            SheetRow {
                keys: "!",
                title: "command.break_pane.title",
            },
            SheetRow {
                keys: "m",
                title: "command.mark_pane.title",
            },
            SheetRow {
                keys: "j",
                title: "command.join_pane.title",
            },
        ],
    ),
    (
        "multiplexer.sheet.help",
        &[SheetRow {
            keys: "?",
            title: "command.show_multiplexer_keys.title",
        }],
    ),
];

/// What the root view does with a keystroke.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    /// Not the layer's: the key goes on as usual.
    Pass,
    /// Taken and dropped: the prefix arming, Escape, an unbound key.
    Swallow,
    /// The prefix pressed twice: let this one through, so it is typed.
    Literal,
    Run(Verb),
    /// A digit while the pane numbers are up: focus that pane (from 0).
    FocusNumber(usize),
}

#[derive(Default)]
pub(crate) struct PrefixLayer {
    armed_at: Option<Instant>,
    numbers: bool,
}

impl PrefixLayer {
    pub(crate) fn is_armed(&self) -> bool {
        self.armed_at.is_some()
    }

    pub(crate) fn cancel(&mut self) {
        self.armed_at = None;
    }

    /// `prefix q` put the numbers up: the next digit picks a pane.
    pub(crate) fn await_number(&mut self) {
        self.numbers = true;
    }

    /// Decides one keystroke. `prefix` is the parsed prefix, or `None`
    /// while the layer is off (then only the pane numbers take keys).
    pub(crate) fn key_down(
        &mut self,
        keystroke: &Keystroke,
        prefix: Option<&Keystroke>,
        now: Instant,
    ) -> Outcome {
        if is_modifier(&keystroke.key) {
            return Outcome::Pass;
        }
        if std::mem::take(&mut self.numbers) {
            if let Some(digit) = digit(keystroke) {
                return Outcome::FocusNumber(digit);
            }
            if keystroke.key == "escape" {
                return Outcome::Swallow;
            }
            // Any other key closes the numbers and is handled as usual.
        }
        let Some(prefix) = prefix else {
            self.armed_at = None;
            return Outcome::Pass;
        };
        let armed = self
            .armed_at
            .take()
            .is_some_and(|since| now.duration_since(since) <= PREFIX_TIMEOUT);
        let is_prefix = matches(keystroke, prefix);
        if !armed {
            if is_prefix {
                self.armed_at = Some(now);
                return Outcome::Swallow;
            }
            return Outcome::Pass;
        }
        if is_prefix {
            return Outcome::Literal;
        }
        match lookup(keystroke) {
            Some(binding) => {
                if binding.repeat {
                    self.armed_at = Some(now);
                }
                if binding.verb == Verb::Command(CommandId::ShowPaneNumbers) {
                    self.numbers = true;
                }
                Outcome::Run(binding.verb)
            }
            // Escape and unbound keys end the sequence and go nowhere.
            None => Outcome::Swallow,
        }
    }
}

fn is_modifier(key: &str) -> bool {
    matches!(
        key,
        "shift" | "control" | "alt" | "platform" | "function" | "capslock"
    )
}

fn digit(keystroke: &Keystroke) -> Option<usize> {
    let modifiers = &keystroke.modifiers;
    if modifiers.control || modifiers.alt || modifiers.platform {
        return None;
    }
    let key = keystroke.key.as_str();
    (key.len() == 1)
        .then(|| key.chars().next()?.to_digit(10))
        .flatten()
        .map(|digit| digit as usize)
}

/// The prefix, as the preferences spell it. It needs ⌃, ⌥ or ⌘: a bare
/// key would arm on every word typed. Anything else falls back to the
/// default rather than leaving the layer unreachable.
pub(crate) fn parse_prefix(source: &str) -> Keystroke {
    Keystroke::parse(source)
        .ok()
        .filter(|keystroke| {
            let modifiers = &keystroke.modifiers;
            (modifiers.control || modifiers.alt || modifiers.platform)
                && !keystroke.key.is_empty()
                && !keystroke.key.contains(char::is_whitespace)
        })
        .unwrap_or_else(|| Keystroke::parse(DEFAULT_PREFIX).expect("the default prefix parses"))
}

fn matches(keystroke: &Keystroke, prefix: &Keystroke) -> bool {
    keystroke.modifiers == prefix.modifiers && keystroke.key.eq_ignore_ascii_case(&prefix.key)
}

/// The binding for the key after the prefix. Printable keys match by the
/// character they type (`%` is shift-5 on one layout and a key of its own
/// on another); named keys (arrows) by name and shift.
fn lookup(keystroke: &Keystroke) -> Option<&'static Binding> {
    let modifiers = &keystroke.modifiers;
    if modifiers.control || modifiers.alt || modifiers.platform {
        return None;
    }
    let typed = keystroke
        .key_char
        .as_deref()
        .filter(|typed| !typed.is_empty() && !typed.chars().any(char::is_control));
    BINDINGS.iter().find(|binding| {
        let printable = binding.key.chars().count() == 1;
        if printable {
            match typed {
                Some(typed) => typed == binding.key,
                // No character reported (a synthesized key): its name,
                // shifted for a capital.
                None => {
                    let named = if modifiers.shift {
                        keystroke.key.to_uppercase()
                    } else {
                        keystroke.key.clone()
                    };
                    named == binding.key || (binding.key == " " && keystroke.key == "space")
                }
            }
        } else {
            keystroke.key == binding.key && modifiers.shift == binding.shift
        }
    })
}

/// How the prefix reads in the interface: `⌃A`.
pub(crate) fn prefix_label(source: &str) -> String {
    crate::commands::keystroke_label(source)
}

fn keycap(label: &str, colors: SemanticColors) -> gpui::Div {
    div()
        .flex_none()
        .min_w(px(22.0))
        .h(px(20.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::CHIP))
        .bg(colors.primary.alpha(0.07))
        .border_1()
        .border_color(colors.primary.alpha(0.10))
        .text_size(px(Typo::META.size))
        .font_weight(FontWeight::MEDIUM)
        .text_color(colors.primary)
        .child(label.to_owned())
}

/// `prefix ?`: every binding, generated from [`SHEET`] with the prefix the
/// preferences name. With the layer off it says where to turn it on.
pub(crate) fn cheatsheet(enabled: bool, prefix: &str, colors: SemanticColors) -> AnyElement {
    let prefix = prefix_label(prefix);
    let mut groups = div().flex().flex_col().gap(px(14.0));
    for (title, rows) in SHEET {
        let mut group = div().flex().flex_col().gap(px(5.0)).child(
            div()
                .text_size(px(Typo::META.size))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.tertiary)
                .child(t(title)),
        );
        for row in *rows {
            let mut keys = div().flex().items_center().gap(px(4.0)).w(px(150.0));
            for key in row.keys.split_whitespace() {
                keys = keys.child(keycap(key, colors));
            }
            group = group.child(
                div().flex().items_center().gap(px(10.0)).child(keys).child(
                    div()
                        .text_size(px(Typo::ROW.size))
                        .text_color(colors.secondary)
                        .child(t(row.title)),
                ),
            );
        }
        groups = groups.child(group);
    }
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .id("multiplexer-cheatsheet")
                .debug_selector(|| "multiplexer-cheatsheet".into())
                .w(px(460.0))
                .p(px(18.0))
                .flex()
                .flex_col()
                .gap(px(14.0))
                .rounded(px(Radius::PANEL))
                .bg(colors.floating_surface())
                .border_1()
                .border_color(colors.floating_stroke())
                .shadow_lg()
                .occlude()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(keycap(&prefix, colors))
                                .child(
                                    div()
                                        .text_size(px(Typo::TITLE.size))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(colors.primary)
                                        .child(t("multiplexer.sheet.title")),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(Typo::META.size))
                                .text_color(colors.tertiary)
                                .child(if enabled {
                                    tf("multiplexer.sheet.subtitle", &[("prefix", &prefix)])
                                } else {
                                    t("multiplexer.sheet.off").to_owned()
                                }),
                        ),
                )
                .child(groups),
        )
        .into_any_element()
}

/// While the prefix waits for its second key: a small keycap in the
/// window's bottom corner, so a swallowed ⌃A never looks like a lost key.
pub(crate) fn armed_mark(prefix: &str, colors: SemanticColors) -> AnyElement {
    div()
        .absolute()
        .bottom(px(10.0))
        .right(px(12.0))
        .child(
            div()
                .id("multiplexer-armed")
                .debug_selector(|| "multiplexer-armed".into())
                .role(gpui::Role::Status)
                .aria_label(tf(
                    "multiplexer.armed",
                    &[("prefix", &prefix_label(prefix))],
                ))
                .px(px(8.0))
                .py(px(3.0))
                .rounded(px(Radius::CHIP))
                .bg(colors.floating_surface())
                .border_1()
                .border_color(colors.floating_stroke())
                .shadow_md()
                .text_size(px(Typo::META.size))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.primary)
                .child(tf(
                    "multiplexer.armed",
                    &[("prefix", &prefix_label(prefix))],
                )),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(source: &str) -> Keystroke {
        let mut keystroke = Keystroke::parse(source).unwrap();
        // What macOS reports for a printable key: the character it typed.
        if keystroke.key_char.is_none()
            && !keystroke.modifiers.control
            && !keystroke.modifiers.platform
        {
            keystroke.key_char = match keystroke.key.as_str() {
                "space" => Some(" ".into()),
                k if k.chars().count() == 1 => Some(k.into()),
                _ => None,
            };
        }
        keystroke
    }

    fn typed(character: &str, base: &str) -> Keystroke {
        let mut keystroke = Keystroke::parse(base).unwrap();
        keystroke.key_char = Some(character.into());
        keystroke
    }

    #[test]
    fn the_prefix_is_swallowed_then_the_next_key_runs_a_verb() {
        let prefix = parse_prefix(DEFAULT_PREFIX);
        let mut layer = PrefixLayer::default();
        let now = Instant::now();
        assert_eq!(
            layer.key_down(&key("ctrl-a"), Some(&prefix), now),
            Outcome::Swallow
        );
        assert!(layer.is_armed());
        // `%` arrives as shift-5 with the character it types.
        assert_eq!(
            layer.key_down(&typed("%", "shift-5"), Some(&prefix), now),
            Outcome::Run(Verb::Command(CommandId::SplitPaneRight))
        );
        assert!(!layer.is_armed());
        // Unarmed, keys are the terminal's.
        assert_eq!(layer.key_down(&key("x"), Some(&prefix), now), Outcome::Pass);
    }

    #[test]
    fn prefix_twice_types_it_and_escape_or_timeout_cancel() {
        let prefix = parse_prefix(DEFAULT_PREFIX);
        let mut layer = PrefixLayer::default();
        let now = Instant::now();
        layer.key_down(&key("ctrl-a"), Some(&prefix), now);
        assert_eq!(
            layer.key_down(&key("ctrl-a"), Some(&prefix), now),
            Outcome::Literal
        );
        assert!(!layer.is_armed(), "the literal does not re-arm");

        layer.key_down(&key("ctrl-a"), Some(&prefix), now);
        // Modifiers on their own neither run nor cancel.
        assert_eq!(
            layer.key_down(&key("shift"), Some(&prefix), now),
            Outcome::Pass
        );
        assert!(layer.is_armed());
        assert_eq!(
            layer.key_down(&key("escape"), Some(&prefix), now),
            Outcome::Swallow
        );
        assert_eq!(layer.key_down(&key("z"), Some(&prefix), now), Outcome::Pass);

        layer.key_down(&key("ctrl-a"), Some(&prefix), now);
        let late = now + PREFIX_TIMEOUT + Duration::from_millis(1);
        assert_eq!(
            layer.key_down(&key("z"), Some(&prefix), late),
            Outcome::Pass,
            "a stale prefix is gone: z is typed"
        );
        // An unbound key ends the sequence without reaching the terminal.
        layer.key_down(&key("ctrl-a"), Some(&prefix), now);
        assert_eq!(
            layer.key_down(&key("y"), Some(&prefix), now),
            Outcome::Swallow
        );
        assert!(!layer.is_armed());
    }

    #[test]
    fn off_means_every_key_passes() {
        let mut layer = PrefixLayer::default();
        assert_eq!(
            layer.key_down(&key("ctrl-a"), None, Instant::now()),
            Outcome::Pass
        );
        assert!(!layer.is_armed());
    }

    #[test]
    fn a_custom_prefix_and_shifted_resizes_that_repeat() {
        let prefix = parse_prefix("ctrl-b");
        let mut layer = PrefixLayer::default();
        let now = Instant::now();
        assert_eq!(
            layer.key_down(&key("ctrl-a"), Some(&prefix), now),
            Outcome::Pass
        );
        layer.key_down(&key("ctrl-b"), Some(&prefix), now);
        assert_eq!(
            layer.key_down(&key("shift-right"), Some(&prefix), now),
            Outcome::Run(Verb::Command(CommandId::PaneGrowWidth))
        );
        assert!(layer.is_armed(), "resize keeps the prefix armed");
        assert_eq!(
            layer.key_down(&key("left"), Some(&prefix), now),
            Outcome::Run(Verb::Command(CommandId::FocusPaneLeft))
        );
        assert!(!layer.is_armed());
        // Unusable preferences fall back to ⌃A: no modifier, or no key.
        assert_eq!(parse_prefix("a"), parse_prefix(DEFAULT_PREFIX));
        assert_eq!(parse_prefix("not a key!!"), parse_prefix(DEFAULT_PREFIX));
        assert_eq!(parse_prefix("ctrl-"), parse_prefix(DEFAULT_PREFIX));
    }

    #[test]
    fn verbs_cover_the_tmux_table() {
        let prefix = parse_prefix(DEFAULT_PREFIX);
        let now = Instant::now();
        let cases = [
            (
                typed("\"", "shift-'"),
                Verb::Command(CommandId::SplitPaneBelow),
            ),
            (key("o"), Verb::Command(CommandId::FocusNextPane)),
            (key(";"), Verb::Command(CommandId::FocusPreviousPane)),
            (key("z"), Verb::Command(CommandId::TogglePaneZoom)),
            (key("x"), Verb::KillPane),
            (key("space"), Verb::Command(CommandId::NextLayout)),
            (typed("!", "shift-1"), Verb::Command(CommandId::BreakPane)),
            (key("m"), Verb::Command(CommandId::MarkPane)),
            (key("j"), Verb::Command(CommandId::JoinPane)),
            (
                typed("S", "shift-s"),
                Verb::Command(CommandId::ToggleSynchronizePanes),
            ),
            (
                typed("?", "shift-/"),
                Verb::Command(CommandId::ShowMultiplexerKeys),
            ),
        ];
        for (keystroke, verb) in cases {
            let mut layer = PrefixLayer::default();
            layer.key_down(&key("ctrl-a"), Some(&prefix), now);
            assert_eq!(
                layer.key_down(&keystroke, Some(&prefix), now),
                Outcome::Run(verb),
                "{keystroke:?}"
            );
        }
    }

    #[test]
    fn pane_numbers_take_one_digit_then_keys_flow_again() {
        let prefix = parse_prefix(DEFAULT_PREFIX);
        let mut layer = PrefixLayer::default();
        let now = Instant::now();
        layer.key_down(&key("ctrl-a"), Some(&prefix), now);
        assert_eq!(
            layer.key_down(&key("q"), Some(&prefix), now),
            Outcome::Run(Verb::Command(CommandId::ShowPaneNumbers))
        );
        assert_eq!(
            layer.key_down(&key("2"), Some(&prefix), now),
            Outcome::FocusNumber(2)
        );
        assert_eq!(layer.key_down(&key("2"), Some(&prefix), now), Outcome::Pass);
        // The palette's "Show pane numbers" works with the layer off, too.
        layer.await_number();
        assert_eq!(
            layer.key_down(&key("0"), None, now),
            Outcome::FocusNumber(0)
        );
        layer.await_number();
        assert_eq!(layer.key_down(&key("a"), None, now), Outcome::Pass);
    }

    #[test]
    fn every_binding_is_on_the_cheatsheet() {
        let titles: Vec<&str> = SHEET
            .iter()
            .flat_map(|(_, rows)| rows.iter().map(|row| row.title))
            .collect();
        for binding in BINDINGS {
            let title = match binding.verb {
                Verb::KillPane => "multiplexer.kill_pane".to_owned(),
                Verb::Command(CommandId::FocusPaneLeft)
                | Verb::Command(CommandId::FocusPaneRight)
                | Verb::Command(CommandId::FocusPaneUp)
                | Verb::Command(CommandId::FocusPaneDown) => "multiplexer.focus_direction".into(),
                Verb::Command(CommandId::FocusNextPane)
                | Verb::Command(CommandId::FocusPreviousPane) => "multiplexer.cycle_panes".into(),
                Verb::Command(_) if binding.repeat => "multiplexer.resize".into(),
                Verb::Command(id) => format!(
                    "command.{}.title",
                    crate::commands::command(id).stable_id.replace('-', "_")
                ),
            };
            assert!(titles.contains(&title.as_str()), "{title} is missing");
        }
    }

    /// A stand-in terminal: records every key that reaches its handler,
    /// the way the real one turns them into PTY bytes.
    struct Typing {
        focus: gpui::FocusHandle,
        typed: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }
    impl gpui::Render for Typing {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            let typed = self.typed.clone();
            div().size_full().track_focus(&self.focus).on_key_down(
                move |event: &gpui::KeyDownEvent, _, _| {
                    typed.borrow_mut().push(event.keystroke.unparse());
                },
            )
        }
    }

    /// The layer runs as a keystroke interceptor; a key it takes must never
    /// reach the focused view's key handler (the terminal, so the PTY).
    #[gpui::test]
    fn swallowed_keys_never_reach_the_focused_terminal(cx: &mut gpui::TestAppContext) {
        let typed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let record = typed.clone();
        let (view, cx) = cx.add_window_view(move |_, cx| Typing {
            focus: cx.focus_handle(),
            typed: record,
        });
        let ran = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let verbs = ran.clone();
        let layer = std::rc::Rc::new(std::cell::RefCell::new(PrefixLayer::default()));
        let _intercept = cx.update(|window, cx| {
            window.focus(&view.read(cx).focus.clone(), cx);
            cx.intercept_keystrokes(move |event, _, cx| {
                let prefix = parse_prefix(DEFAULT_PREFIX);
                match layer
                    .borrow_mut()
                    .key_down(&event.keystroke, Some(&prefix), Instant::now())
                {
                    Outcome::Pass | Outcome::Literal => {}
                    Outcome::Run(verb) => {
                        verbs.borrow_mut().push(verb);
                        cx.stop_propagation();
                    }
                    Outcome::Swallow | Outcome::FocusNumber(_) => cx.stop_propagation(),
                }
            })
        });
        cx.simulate_keystrokes("ctrl-a z a ctrl-a ctrl-a ctrl-a escape b ctrl-a y c");
        assert_eq!(
            *typed.borrow(),
            vec!["a", "ctrl-a", "b", "c"],
            "only plain keys and the doubled prefix are typed"
        );
        assert_eq!(
            *ran.borrow(),
            vec![Verb::Command(CommandId::TogglePaneZoom)]
        );
    }
}
