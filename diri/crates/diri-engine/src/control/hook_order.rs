//! A callback reserves its place before the CLI returns to the provider.
//! The connection owns that reservation and consumes at most one commit.
//! A later connection can finish reading first, but its handler waits for
//! its predecessor before handing anything to the existing hook applier.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex, MutexGuard, mpsc};

const MAX_RESERVATIONS: usize = 1024;

#[derive(Default)]
pub(super) struct HookOrder {
    state: Mutex<State>,
    capacity: Condvar,
}

#[derive(Default)]
struct State {
    last_sequence: u64,
    active: usize,
    tails: HashMap<String, (u64, mpsc::Receiver<()>)>,
}

pub(super) struct Reservation<'a> {
    order: &'a HookOrder,
    session: String,
    pub(super) sequence: u64,
    predecessor: Option<mpsc::Receiver<()>>,
    successor: mpsc::Sender<()>,
}

impl HookOrder {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub(super) fn reserve(&self, session: &str) -> Reservation<'_> {
        let mut state = self
            .capacity
            .wait_while(self.lock(), |s| s.active == MAX_RESERVATIONS)
            .unwrap_or_else(|error| error.into_inner());
        state.last_sequence = state
            .last_sequence
            .checked_add(1)
            .expect("hook sequence space exhausted");
        let sequence = state.last_sequence;
        let (successor, tail) = mpsc::channel();
        let predecessor = state
            .tails
            .insert(session.to_owned(), (sequence, tail))
            .map(|(_, rx)| rx);
        state.active += 1;
        Reservation {
            order: self,
            session: session.to_owned(),
            sequence,
            predecessor,
            successor,
        }
    }
}

impl Reservation<'_> {
    fn wait_for_predecessor(&mut self) {
        if let Some(predecessor) = self.predecessor.take() {
            let _ = predecessor.recv();
        }
    }

    pub(super) fn finish<T>(mut self, apply: impl FnOnce() -> T) -> T {
        self.wait_for_predecessor();
        apply()
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        // An EOF, invalid commit or failed Hello reply cancels this slot.
        // It still sits behind its predecessor, so cancelling the middle of
        // a chain cannot let the last connection overtake the first one.
        self.wait_for_predecessor();
        let mut state = self.order.lock();
        if state
            .tails
            .get(&self.session)
            .is_some_and(|(seq, _)| *seq == self.sequence)
        {
            state.tails.remove(&self.session);
        }
        state.active -= 1;
        let _ = self.successor.send(());
        self.order.capacity.notify_one();
    }
}

impl super::ControlServer {
    pub(super) fn serve_reserved_hook(
        &self,
        frame: &serde_json::Value,
        reader: &mut std::io::BufReader<diri_platform::ipc::UnixStream>,
        writer: &std::sync::Arc<Mutex<diri_platform::ipc::UnixStream>>,
    ) -> std::io::Result<()> {
        use diri_proto::{ControlError, ControlMessage, HookReportParams};
        use serde_json::json;
        use std::io;
        let id = frame["id"]
            .as_u64()
            .ok_or_else(|| io::Error::other("invalid hook Hello"))?;
        let intent = &frame["params"]["reserveHook"];
        let session = intent["sessionID"].as_str().unwrap_or("");
        let kind = intent["kind"].as_str().unwrap_or("");
        let event = intent["event"].as_str();
        let hello = self.hello(Some(frame["params"].clone())).and_then(|hello| {
            if session.is_empty()
                || session.len() > 256
                || !matches!(kind, "claude-hook" | "codex-notify")
            {
                return Err(ControlError::bad_request("invalid hook intent"));
            }
            Ok(hello)
        });
        let mut hello = match hello {
            Ok(hello) => hello,
            Err(error) => {
                return super::write_message(
                    writer,
                    &ControlMessage::Response {
                        id,
                        result: Err(error),
                    },
                );
            }
        };
        let slot = self.hook_order.reserve(session);
        hello["hookReservation"] = json!({
            "sequence": slot.sequence,
            "response": self.hook_response(kind, event, session),
        });
        // Bound abandoned connections, including a Hello whose caller never
        // reads the reply. Each connection is dedicated to this one report.
        let timeout = Some(std::time::Duration::from_secs(3));
        reader.get_ref().set_read_timeout(timeout)?;
        reader.get_ref().set_write_timeout(timeout)?;
        super::write_message(
            writer,
            &ControlMessage::Response {
                id,
                result: Ok(hello),
            },
        )?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let Some(line) = super::read_bounded_control_line_with(reader, |reader| {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            reader.get_ref().set_read_timeout(Some(remaining))
        })?
        else {
            return Ok(());
        };
        let Ok(ControlMessage::Request { id, method, params }) = serde_json::from_slice(&line)
        else {
            return Ok(());
        };
        if id != slot.sequence || method != diri_proto::control::HOOK_COMMIT {
            return Ok(());
        }
        let report: HookReportParams =
            super::decode(params).map_err(|_| io::Error::other("invalid hook report"))?;
        if report.dirijor_session_id.as_ref().map(|id| id.0.as_str()) == Some(session)
            && report.kind == kind
            && report.event.as_deref() == event
        {
            slot.finish(|| self.apply_hook_report(report))
                .map_err(|_| io::Error::other("hook delivery failed"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::parse_claude_hook;
    use crate::status::{Authority, StatusReducer};
    use serde_json::json;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn shuffled_commits_match_in_order_status() {
        let order = HookOrder::default();
        let events = [
            Some("SessionStart"),
            Some("UserPromptSubmit"),
            None, // disconnected in the middle of two live reservations
            Some("PreToolUse"),
            Some("PermissionRequest"),
            Some("SubagentStart"),
            Some("Stop"),
        ];
        let initial = || StatusReducer::new(Authority::HooksPrimary, UNIX_EPOCH);
        let apply = |reducer: &mut StatusReducer, index: usize| {
            let event: &str = events[index].unwrap();
            let now = UNIX_EPOCH + Duration::from_secs(index as u64 + 1);
            let (signal, _) = parse_claude_hook(event, &json!({"tool_name":"Bash"}), now).unwrap();
            reducer.reduce(signal, now);
            reducer.status().clone()
        };
        let mut canonical = initial();
        let expected: Vec<_> = (0..events.len())
            .filter(|&i| events[i].is_some())
            .map(|i| (i, apply(&mut canonical, i)))
            .collect();
        for shift in 0..events.len() {
            for reverse in [true, false] {
                let state = Mutex::new((initial(), Vec::new()));
                let mut slots: Vec<_> = (0..events.len())
                    .map(|i| (i, order.reserve("session")))
                    .collect();
                slots.rotate_left(shift);
                if reverse {
                    slots.reverse();
                }
                std::thread::scope(|scope| {
                    for (i, slot) in slots {
                        let state = &state;
                        let (arrived, ready) = mpsc::sync_channel(0);
                        scope.spawn(move || {
                            arrived.send(()).unwrap();
                            if events[i].is_some() {
                                slot.finish(|| {
                                    let mut state = state.lock().unwrap();
                                    let next = apply(&mut state.0, i);
                                    state.1.push((i, next));
                                });
                            }
                        });
                        ready.recv().unwrap();
                    }
                });
                assert_eq!(state.into_inner().unwrap().1, expected);
                assert!(order.lock().tails.is_empty());
            }
        }
    }
}
