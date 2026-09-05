//! Bounded OSC notification extraction, enabled only by the local Engine.
//! Content is data: it cannot acknowledge prompts or alter execution status.
use std::collections::VecDeque;

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
}

impl NotificationParser {
    pub fn reset_sequence(&mut self) {
        self.state = 0;
        self.payload.clear();
        self.pending.clear();
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        // No copy or bytewise scan for the common plain-output case.
        if self.state == 0 && !bytes.contains(&0x1b) {
            return;
        }
        for &byte in bytes {
            match self.state {
                0 => {
                    if byte == 0x1b {
                        self.state = 1;
                    }
                }
                1 => {
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
                    _ if self.payload.len() < 8192 => self.payload.push(byte),
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

    fn finish(&mut self) {
        let Ok(payload) = std::str::from_utf8(&self.payload) else {
            return;
        };
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
    fn osc_inside_another_string_is_not_a_notification() {
        let mut parser = NotificationParser::default();
        parser.feed(b"\x1bPignored\x1b]9;not an alert\x1b\\");
        assert!(parser.ready.is_empty());
    }
}
