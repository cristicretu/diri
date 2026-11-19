//! The subset of a desktop terminal attachment needed to test a composer.
//! Consume the actual Grid and Modes frames sent to the renderer.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use diri_proto::frames::{Frame, FrameCodec, FrameType};
use diri_proto::terminal_input::{
    KeyAction, KeyEvent, KeyboardState, Modifiers, encode_action, paste,
};
use serde_json::json;

pub struct Composer {
    stream: UnixStream,
    codec: FrameCodec,
    pub rows: Vec<String>,
    keyboard: Option<KeyboardState>,
    bracketed: Option<bool>,
}

impl Composer {
    pub fn attach(mut stream: UnixStream, id: &str) -> Self {
        writeln!(
            stream,
            "{}",
            json!({"attach": id, "enhancedKeyboard": true})
        )
        .unwrap();
        Self {
            stream,
            codec: FrameCodec::new(),
            rows: Vec::new(),
            keyboard: None,
            bracketed: None,
        }
    }

    fn send(&mut self, bytes: Vec<u8>) {
        self.stream
            .write_all(&FrameCodec::encode(&Frame::input(bytes)).unwrap())
            .unwrap();
    }

    pub fn key(&mut self, ch: &str, ctrl: bool) {
        let event = KeyEvent::character(ch);
        let modifiers = Modifiers {
            ctrl,
            ..Default::default()
        };
        self.send(encode_action(&event, modifiers, self.keyboard, KeyAction::Press).unwrap());
        if let Ok(release) = encode_action(&event, modifiers, self.keyboard, KeyAction::Release)
            && !release.is_empty()
        {
            self.send(release);
        }
    }

    pub fn paste(&mut self, text: &str) {
        self.send(paste(text, self.bracketed.expect("received Modes")));
    }

    pub fn wait(&mut self, timeout: Duration, seen: impl Fn(&[String]) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut bytes = [0_u8; 16384];
        while self.bracketed.is_none() || !seen(&self.rows) {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            self.stream.set_read_timeout(Some(left)).unwrap();
            let n = match self.stream.read(&mut bytes) {
                Ok(0) => return false,
                Ok(n) => n,
                Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return false;
                }
                Err(err) => panic!("reading desktop attachment: {err}"),
            };
            for frame in self.codec.feed(&bytes[..n]).unwrap() {
                if let Some((_, bracketed, _)) = frame.terminal_modes_payload() {
                    self.bracketed = Some(bracketed);
                    self.keyboard = frame.keyboard_state_payload().unwrap();
                }
                if frame.frame_type == FrameType::Grid {
                    let update = frame.grid_payload().unwrap().unwrap();
                    if update.is_full_snapshot {
                        self.rows.clear();
                    }
                    self.rows.resize_with(usize::from(update.rows), String::new);
                    for row in update.changed_rows {
                        self.rows[usize::from(row.y)] = row
                            .cells
                            .iter()
                            .map(|c| {
                                char::from_u32(c.scalar)
                                    .filter(|&ch| ch != '\0')
                                    .unwrap_or(' ')
                            })
                            .collect();
                    }
                }
            }
        }
        true
    }
}
