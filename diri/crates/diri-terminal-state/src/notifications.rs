//! Bounded OSC notification extraction, enabled only by the local Engine.
//! Content is data: it cannot acknowledge prompts or alter execution status.
//! OSC 52 clipboard writes ride the same scanner so a replayed prefix can
//! never rewrite the user's clipboard; reads are never answered.
use std::collections::VecDeque;

/// Other OSC payloads stay small; a copied selection may be long.
const OSC_PAYLOAD_LIMIT: usize = 8192;
/// Base64 bytes of one OSC 52 write, about 768 KiB of copied text.
const CLIPBOARD_PAYLOAD_LIMIT: usize = 1 << 20;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalNotification {
    pub title: String,
    pub body: String,
}

#[derive(Default)]
pub(crate) struct NotificationParser {
    state: u8,
    payload: Vec<u8>,
    pending: VecDeque<(String, TerminalNotification)>,
    pub ready: VecDeque<TerminalNotification>,
    /// Base64 payload of the newest OSC 52 clipboard write. Later writes
    /// replace it: only the last copy would survive on the clipboard anyway.
    pub clipboard: Option<String>,
    /// Exit status the Engine's `returnToLoginShell` wrapper reported for its
    /// agent ([`AGENT_EXIT_OSC`]), not yet taken.
    pub agent_exit: Option<i32>,
    /// `OSC 7501` program-status records.
    pub program: crate::program_status::ProgramStatusTable,
    /// Answers owed to the child, moved to the screen's replies after a feed.
    pub replies: Vec<u8>,
    /// Where the shell is between its `OSC 133` marks.
    pub shell: ShellPhase,
    /// Answer `OSC 7501` support queries and interpret nothing else: no
    /// alerts, clipboard writes, exit reports or status records. A remote
    /// Holder's mode; see [`crate::HeadlessScreen::answering_program_status_queries`].
    queries_only: bool,
}

/// Where an interactive shell is, from its `OSC 133` semantic prompt marks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShellPhase {
    /// No marks seen, or a full reset since.
    #[default]
    Unknown,
    /// At its prompt: a prompt was drawn, or the last command ended (`D`).
    Prompt,
    /// Running a command line (`C`), with no `D` yet.
    Running,
    /// A prompt drawn (`A`) while a command line still runs: the shell's own
    /// line reader asking for input on that command's behalf. fish marks its
    /// `read` builtin's prompt this way; with no foreground job, nothing else
    /// tells that question from an idle prompt.
    PromptInCommand,
}

impl ShellPhase {
    fn mark(self, mark: &str) -> Self {
        match mark.as_bytes().first() {
            Some(b'A') => match self {
                Self::Running | Self::PromptInCommand => Self::PromptInCommand,
                _ => Self::Prompt,
            },
            Some(b'C') => Self::Running,
            Some(b'D') => Self::Prompt,
            // `B` (input start) and unknown marks change nothing.
            _ => self,
        }
    }

    /// A shell reporting its directory (`OSC 7`) is at its own prompt: a
    /// `C` that came before belonged to whatever replaced it (`exec fish`).
    fn directory_reported(self) -> Self {
        match self {
            Self::Running => Self::Prompt,
            other => other,
        }
    }
}

/// Longest OSC payload a queries-only scanner keeps; anything longer cannot
/// be the support query and is skipped without buffering.
const QUERY_PAYLOAD_LIMIT: usize = 16;

/// `OSC 6973;agent-exit;<status> BEL`: the status the login-shell wrapper's
/// shell saw when the agent it launched returned (`$?`, or `$status` in
/// fish). Private to diri; other terminals ignore an unknown OSC. It carries
/// one integer and nothing the agent wrote, and like every product OSC here
/// it is only honoured in live output, never in replayed history.
pub const AGENT_EXIT_OSC: &str = "6973;agent-exit;";

impl NotificationParser {
    pub fn queries_only() -> Self {
        Self {
            queries_only: true,
            ..Self::default()
        }
    }

    /// A fresh parser in the same mode, for a terminal reset.
    pub fn reset_copy(&self) -> Self {
        Self {
            queries_only: self.queries_only,
            ..Self::default()
        }
    }

    pub fn reset_sequence(&mut self) {
        self.state = 0;
        self.payload.clear();
        self.pending.clear();
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        // No copy or bytewise scan for the common plain-output case.
        let mut index = 0;
        while index < bytes.len() {
            if self.state == 0 {
                // Plain output between escapes cannot start an OSC.
                let Some(offset) = memchr::memchr(0x1b, &bytes[index..]) else {
                    return;
                };
                index += offset;
            }
            let byte = bytes[index];
            index += 1;
            match self.state {
                0 => {
                    if byte == 0x1b {
                        self.state = 1;
                    }
                }
                1 => {
                    if byte == b'c' {
                        self.shell = ShellPhase::Unknown;
                        // RIS ends every program-status record; a soft reset
                        // (DECSTR) and screen switches keep them.
                        self.program
                            .end_program(crate::program_status::ProgramRemoval::Reset);
                    }
                    self.state = match byte {
                        b']' => 2,
                        b'P' | b'_' | b'^' | b'X' => 4,
                        0x1b => 1,
                        _ => 0,
                    };
                    self.payload.clear();
                }
                2 => match byte {
                    7 => {
                        self.finish();
                        self.state = 0;
                    }
                    0x1b => self.state = 3,
                    0x18 | 0x1a => {
                        self.payload.clear();
                        self.state = 0;
                    }
                    _ if self.payload.len() < self.payload_limit() => self.payload.push(byte),
                    _ => {
                        self.payload.clear();
                        self.state = 4;
                    }
                },
                3 => {
                    if byte == b'\\' {
                        self.finish();
                        self.state = 0;
                    } else {
                        self.payload.clear();
                        self.state = if byte == 0x1b { 1 } else { 0 };
                    }
                }
                4 => match byte {
                    7 | 0x18 | 0x1a => self.state = 0,
                    0x1b => self.state = 5,
                    _ => {}
                },
                5 => {
                    self.state = if byte == b'\\' {
                        0
                    } else if byte == 0x1b {
                        5
                    } else {
                        4
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    fn payload_limit(&self) -> usize {
        if self.queries_only {
            QUERY_PAYLOAD_LIMIT
        } else if self.payload.starts_with(b"52;") {
            CLIPBOARD_PAYLOAD_LIMIT
        } else {
            OSC_PAYLOAD_LIMIT
        }
    }

    fn finish(&mut self) {
        if self.queries_only {
            if self
                .payload
                .strip_prefix(crate::program_status::PREFIX.as_bytes())
                .is_some_and(|report| report == crate::program_status::QUERY.as_bytes())
            {
                self.replies
                    .extend_from_slice(crate::program_status::QUERY_REPLY);
            }
            return;
        }
        let Ok(payload) = std::str::from_utf8(&self.payload) else {
            return;
        };
        if let Some(status) = payload.strip_prefix(AGENT_EXIT_OSC) {
            if let Ok(status) = status.parse::<i32>()
                && (0..=255).contains(&status)
            {
                self.agent_exit = Some(status);
            }
            return;
        }
        if let Some(mark) = payload.strip_prefix("133;") {
            self.shell = self.shell.mark(mark);
            return;
        }
        if payload.starts_with("7;") {
            self.shell = self.shell.directory_reported();
            return;
        }
        if let Some(report) = payload.strip_prefix(crate::program_status::PREFIX) {
            if report == crate::program_status::QUERY {
                self.replies
                    .extend_from_slice(crate::program_status::QUERY_REPLY);
            } else {
                self.program.apply(report);
            }
            return;
        }
        if let Some(content) = payload.strip_prefix("52;") {
            // `52;<targets>;<base64>`. A `?` asks to read the clipboard and an
            // empty payload asks to clear it; a program may do neither.
            if let Some((_, data)) = content.split_once(';')
                && !data.is_empty()
                && data != "?"
            {
                self.clipboard = Some(data.to_owned());
            }
            return;
        }
        let notification = if let Some(body) = payload.strip_prefix("9;") {
            // OSC 9;4 is progress, never a desktop notification.
            if body.starts_with("4;") {
                return;
            }
            TerminalNotification {
                title: String::new(),
                body: clean(body, 1000),
            }
        } else if let Some(content) = payload.strip_prefix("777;notify;") {
            let (title, body) = content.split_once(';').unwrap_or((content, ""));
            TerminalNotification {
                title: clean(title, 160),
                body: clean(body, 1000),
            }
        } else if let Some(content) = payload.strip_prefix("99;") {
            let Some((metadata, text)) = content.split_once(';') else {
                return;
            };
            let value = |key: &str| metadata.split(':').find_map(|part| part.strip_prefix(key));
            // Only textual title/body delivery. Queries, icons, close commands,
            // encoded data and activation callbacks must never become alerts.
            if value("e=").is_some_and(|encoding| encoding != "0") {
                return;
            }
            let part = value("p=").unwrap_or("title");
            if !matches!(part, "title" | "body") {
                return;
            }
            let id = clean(value("i=").unwrap_or(""), 128);
            let mut notification = self
                .pending
                .iter()
                .position(|(key, _)| key == &id)
                .and_then(|index| self.pending.remove(index))
                .map(|(_, item)| item)
                .unwrap_or_default();
            if part == "body" {
                notification.body = clean(text, 1000);
            } else {
                notification.title = clean(text, 160);
            }
            if value("d=") == Some("0") {
                if self.pending.len() == 8 {
                    self.pending.pop_front();
                }
                self.pending.push_back((id, notification));
                return;
            }
            notification
        } else {
            return;
        };
        if notification.title.is_empty() && notification.body.is_empty() {
            return;
        }
        if self.ready.len() == 32 {
            self.ready.pop_front();
        }
        self.ready.push_back(notification);
    }
}

fn clean(text: &str, max: usize) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(max)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_replay_boundary_inside_a_read_delivers_only_complete_live_notifications() {
        let mut screen = crate::HeadlessScreen::new(80, 24).with_notifications();
        let old = b"\x1b]9;old\x07\x1b]9;partial";
        let live = b" remainder\x07\x1b]9;new\x07";
        let bytes: Vec<_> = old.iter().chain(live).copied().collect();
        screen.feed_with_history(&bytes, old.len());
        let messages = screen.take_notifications();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body, "new");
    }

    #[test]
    fn agent_exit_status_is_read_from_live_output_only() {
        let mut screen = crate::HeadlessScreen::new(80, 24).with_notifications();
        let old = b"\x1b]6973;agent-exit;1\x07";
        let live = b"before\x1b]6973;agent-exit;137\x07after";
        let bytes: Vec<_> = old.iter().chain(live).copied().collect();
        screen.feed_with_history(&bytes, old.len());
        assert_eq!(screen.take_agent_exit(), Some(137));
        assert_eq!(screen.take_agent_exit(), None);
        // Consumed, not drawn: the row holds only the text around it.
        assert!(screen.lines()[0].starts_with("beforeafter"));

        for junk in [
            &b"\x1b]6973;agent-exit;\x07"[..],
            b"\x1b]6973;agent-exit;-1\x07",
            b"\x1b]6973;agent-exit;256\x07",
            b"\x1b]6973;agent-exit;1; rm\x07",
        ] {
            screen.feed(junk);
            assert_eq!(screen.take_agent_exit(), None);
        }
        // Screens that never opted in (the remote Holder) ignore it.
        let mut holder = crate::HeadlessScreen::new(80, 24);
        holder.feed(b"\x1b]6973;agent-exit;0\x07");
        assert_eq!(holder.take_agent_exit(), None);
    }

    #[test]
    fn fragmented_osc_bel_st_and_progress() {
        let mut parser = NotificationParser::default();
        for byte in b"\x1b]777;notify;Tests;All green\x1b\\\x1b]9;Review ready\x07\x1b]9;4;1;80\x07"
        {
            parser.feed(&[*byte]);
        }
        assert_eq!(parser.ready.len(), 2);
        assert_eq!(
            parser.ready[0],
            TerminalNotification {
                title: "Tests".into(),
                body: "All green".into()
            }
        );
        assert_eq!(parser.ready[1].body, "Review ready");
    }
    #[test]
    fn kitty_multipart_and_control_messages() {
        let mut parser = NotificationParser::default();
        parser.feed(
            b"\x1b]99;i=abc:d=0;Build\x07\x1b]99;i=abc:p=body;Passed\x1b\\\x1b]99;p=?;query\x07",
        );
        assert_eq!(parser.ready.len(), 1);
        assert_eq!(
            parser.ready[0],
            TerminalNotification {
                title: "Build".into(),
                body: "Passed".into()
            }
        );
    }
    #[test]
    fn malformed_and_flooded_output_is_bounded() {
        let mut parser = NotificationParser::default();
        parser.feed(b"\x1b]9;");
        parser.feed(&vec![b'x'; 100_000]);
        assert!(parser.payload.is_empty());
        parser.feed(b"\x07\x1b]9;recovered\x07");
        assert_eq!(parser.ready.len(), 1);
        for _ in 0..1000 {
            parser.feed(b"\x1b]9;bounded\x07");
        }
        assert_eq!(parser.ready.len(), 32);
    }
    #[test]
    fn clipboard_writes_keep_the_newest_payload_and_never_answer_reads() {
        let mut parser = NotificationParser::default();
        parser.feed(b"\x1b]52;c;Zmlyc3Q=\x07\x1b]52;c;c2Vjb25k\x1b\\");
        parser.feed(b"\x1b]52;c;?\x07\x1b]52;c;\x07");
        assert_eq!(parser.clipboard.as_deref(), Some("c2Vjb25k"));
        assert!(parser.ready.is_empty());
    }

    #[test]
    fn a_long_clipboard_write_survives_the_notification_bound() {
        let mut parser = NotificationParser::default();
        let data = "QUJD".repeat(10_000);
        parser.feed(format!("\x1b]52;c;{data}\x07").as_bytes());
        assert_eq!(parser.clipboard.as_deref(), Some(data.as_str()));

        parser.clipboard = None;
        parser.feed(b"\x1b]52;c;");
        parser.feed(&vec![b'A'; CLIPBOARD_PAYLOAD_LIMIT + 1]);
        parser.feed(b"\x07");
        assert!(parser.clipboard.is_none());
    }

    #[test]
    fn a_replayed_clipboard_write_is_not_delivered() {
        let mut screen = crate::HeadlessScreen::new(80, 24).with_notifications();
        let old = b"\x1b]52;c;b2xk\x07";
        screen.feed_with_history(old, old.len());
        assert!(!screen.has_notifications());
        assert_eq!(screen.take_clipboard(), None);
        screen.feed(b"\x1b]52;c;bmV3\x07");
        assert!(screen.has_notifications());
        assert_eq!(screen.take_clipboard().as_deref(), Some("bmV3"));
        assert!(!screen.has_notifications());
    }

    #[test]
    fn osc_inside_another_string_is_not_a_notification() {
        let mut parser = NotificationParser::default();
        parser.feed(b"\x1bPignored\x1b]9;not an alert\x1b\\");
        assert!(parser.ready.is_empty());
    }

    #[test]
    fn program_status_is_live_only_and_answers_its_query() {
        use crate::ProgramState;
        let mut screen = crate::HeadlessScreen::new(80, 24).with_notifications();
        // History: a stale report and a query asked by a program long gone.
        let old = b"\x1b]7501;state=blocked\x07\x1b]7501;?\x07";
        let live = b"\x1b]7501;state=working:progress=5\x1b\\\x1b]7501;?\x1b\\\x1b[c";
        let bytes: Vec<_> = old.iter().chain(live).copied().collect();
        screen.feed_with_history(&bytes, old.len());
        let status = screen.program_status().unwrap();
        assert_eq!(status.state, ProgramState::Working);
        assert_eq!(status.progress, Some(5));
        let replies = screen.take_replies();
        // One answer for the live query, ahead of the device attributes.
        assert!(replies.starts_with(b"\x1b]7501;?\x1b\\"), "{replies:?}");
        assert_eq!(replies.windows(5).filter(|w| w == b"7501;").count(), 1);
        assert!(replies.len() > 9, "device attributes still answered");

        let generation = screen.program_status_generation();
        screen.feed(b"plain output");
        assert_eq!(screen.program_status_generation(), generation);
        assert!(screen.end_program_status(crate::ProgramRemoval::Foreground));
        assert_eq!(screen.program_status(), None);
        assert_eq!(
            screen.program_status_removal(),
            Some(crate::ProgramRemoval::Foreground)
        );
    }

    #[test]
    fn a_full_reset_clears_program_status() {
        let mut screen = crate::HeadlessScreen::new(80, 24).with_notifications();
        screen.feed(b"\x1b]7501;state=working\x07\x1b[!p\x1b[?1049h");
        assert!(screen.program_status().is_some(), "DECSTR and 1049 keep it");
        screen.feed(b"\x1bc");
        assert_eq!(screen.program_status(), None);
        assert_eq!(
            screen.program_status_removal(),
            Some(crate::ProgramRemoval::Reset)
        );
    }

    #[test]
    fn a_screen_without_notifications_ignores_program_status() {
        let mut screen = crate::HeadlessScreen::new(80, 24);
        screen.feed(b"\x1b]7501;state=working\x07\x1b]7501;?\x07");
        assert_eq!(screen.program_status(), None);
        assert!(screen.take_replies().is_empty());
    }

    /// fish 4.0.2's own marks, as it sent them on 2026-10-27: its `read`
    /// prompt is an `A` inside the command line's `C`…`D`.
    #[test]
    fn a_prompt_inside_a_running_command_is_the_shells_own_question() {
        use super::ShellPhase;
        let mut screen = crate::HeadlessScreen::new(80, 24).with_notifications();
        assert_eq!(screen.shell_phase(), ShellPhase::Unknown);
        let prompt = b"\x1b]7;file://host/private/tmp/statlab\x07\x1b]0;/p/t/statlab\x07\
            \x1b]133;A;special_key=1\x07giga@host /p/t/statlab> ";
        screen.feed(prompt);
        assert_eq!(screen.shell_phase(), ShellPhase::Prompt);
        screen.feed(
            b"read -P \"Proceed? [y/N] \" ans\r\n\
              \x1b]133;C;cmdline_url=read%20-P%20%22Proceed%3F%20%5By/N%5D%20%22%20ans\x07",
        );
        assert_eq!(screen.shell_phase(), ShellPhase::Running);
        screen.feed(b"\x1b]0;read /p/t/statlab\x07\r\x1b]133;A;special_key=1\x07Proceed? [y/N] ");
        assert_eq!(screen.shell_phase(), ShellPhase::PromptInCommand);
        // A second `read` in the same command line.
        screen.feed(b"y\r\n\x1b]133;A;special_key=1\x07Again? ");
        assert_eq!(screen.shell_phase(), ShellPhase::PromptInCommand);
        // Answered (or ^C, which ends it with `D;1`): the shell's prompt.
        screen.feed(b"y\r\n\x1b]133;D;0\x07\x1b]133;A;special_key=1\x07giga@host> ");
        assert_eq!(screen.shell_phase(), ShellPhase::Prompt);

        // `exec fish` from a shell that marked the command: the new shell
        // reports its directory before its first prompt.
        screen.feed(b"\x1b]133;C\x07\x1b]7;file://host/tmp\x07\x1b]133;A;special_key=1\x07> ");
        assert_eq!(screen.shell_phase(), ShellPhase::Prompt);

        // zsh and bash marks: `A`/`B` around the prompt, `C`, `D`.
        screen.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 1\r\n\x1b]133;C\x07");
        assert_eq!(screen.shell_phase(), ShellPhase::Running);
        screen.feed(b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        assert_eq!(screen.shell_phase(), ShellPhase::Prompt);

        screen.feed(b"\x1b]133;C\x07\x1bc");
        assert_eq!(screen.shell_phase(), ShellPhase::Unknown);
    }
}
