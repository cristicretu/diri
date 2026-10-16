//! `browser.pane`: an Agent acts on the page in its Session's sidebar
//! Browser. That page lives in the app's WebKit view, so the Engine relays
//! each action as a `session.browser_pane` event and waits for the window
//! holding the page to answer with `browser.pane_reply`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

/// How long an ordinary action may take: the page runs it on the app's main
/// thread, so this only runs out when no window is listening.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(20);
/// `wait` may hold the page for up to this long, on top of the reply budget.
pub const MAX_WAIT: Duration = Duration::from_secs(10);

pub type Reply = Result<serde_json::Value, String>;

/// Requests waiting for the app's answer, by request id.
#[derive(Default)]
pub struct Waiters {
    next: AtomicU64,
    pending: Mutex<HashMap<String, mpsc::SyncSender<Reply>>>,
}

impl Waiters {
    /// A fresh request id and the receiver its answer will arrive on.
    pub fn register(&self) -> (String, mpsc::Receiver<Reply>) {
        let id = format!("bp{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
        let (sender, receiver) = mpsc::sync_channel(1);
        self.lock().insert(id.clone(), sender);
        (id, receiver)
    }

    /// Hands an answer to its waiter. Only the first answer for an id counts
    /// (several windows may hear the same event); unknown ids are ignored.
    pub fn resolve(&self, request_id: &str, reply: Reply) -> bool {
        let Some(sender) = self.lock().remove(request_id) else {
            return false;
        };
        sender.try_send(reply).is_ok()
    }

    /// Waits for `request_id`'s answer; `None` when no window answered.
    pub fn wait(
        &self,
        request_id: &str,
        receiver: &mpsc::Receiver<Reply>,
        timeout: Duration,
    ) -> Option<Reply> {
        let reply = receiver.recv_timeout(timeout).ok();
        if reply.is_none() {
            self.lock().remove(request_id);
        }
        reply
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, mpsc::SyncSender<Reply>>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }
}

/// How long to wait for the answer to `action`: `wait` holds the page.
pub fn timeout_for(action: &str, args: &serde_json::Value) -> Duration {
    if action != "wait" {
        return REPLY_TIMEOUT;
    }
    let ms = args
        .get("ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(5_000);
    REPLY_TIMEOUT + Duration::from_millis(ms).min(MAX_WAIT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_first_answer_reaches_its_waiter_and_later_ones_are_dropped() {
        let waiters = Waiters::default();
        let (id, receiver) = waiters.register();
        let (other, _other_receiver) = waiters.register();
        assert_ne!(id, other);
        assert!(!waiters.resolve("bp999", Ok(json!(1))), "unknown id");
        assert!(waiters.resolve(&id, Ok(json!({"url": "http://x"}))));
        assert!(!waiters.resolve(&id, Ok(json!("second window"))));
        assert_eq!(
            waiters.wait(&id, &receiver, Duration::from_millis(10)),
            Some(Ok(json!({"url": "http://x"})))
        );
        assert_eq!(waiters.len(), 1, "only the unanswered request remains");
    }

    #[test]
    fn an_unanswered_request_times_out_and_is_forgotten() {
        let waiters = Waiters::default();
        let (id, receiver) = waiters.register();
        assert_eq!(waiters.wait(&id, &receiver, Duration::from_millis(5)), None);
        assert_eq!(waiters.len(), 0);
        assert!(
            !waiters.resolve(&id, Ok(json!(1))),
            "a late answer is dropped"
        );
    }

    #[test]
    fn wait_actions_get_their_own_budget_capped() {
        assert_eq!(timeout_for("click", &json!({})), REPLY_TIMEOUT);
        assert_eq!(
            timeout_for("wait", &json!({"ms": 2000})),
            REPLY_TIMEOUT + Duration::from_secs(2)
        );
        assert_eq!(
            timeout_for("wait", &json!({"ms": 600_000})),
            REPLY_TIMEOUT + MAX_WAIT
        );
    }
}
