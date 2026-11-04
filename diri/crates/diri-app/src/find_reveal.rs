//! A Find that a global-search jump asks for, handed to whichever terminal
//! pane shows the session next.
//!
//! The search page knows the session and the words, not which pane entity
//! will paint it: the window's main pane, or a split's own pane mounted
//! later. So the request waits here, and the pane that renders that session
//! takes it, opens Find with the words and moves to the newest match. An
//! unclaimed request expires; checking costs one relaxed load while none is
//! waiting.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use diri_proto::SessionId;

/// Long enough for a tab switch to mount and paint its pane.
const EXPIRES: Duration = Duration::from_secs(5);

static ARMED: AtomicBool = AtomicBool::new(false);
static PENDING: Mutex<Option<(SessionId, String, Instant)>> = Mutex::new(None);

/// Asks the pane that shows `session` to Find `query` there.
pub(crate) fn request(session: SessionId, query: String) {
    *PENDING.lock().expect("find reveal") = Some((session, query, Instant::now()));
    ARMED.store(true, Ordering::Release);
}

/// The query waiting for `session`, taken so only one pane acts on it.
pub(crate) fn take_for(session: &SessionId) -> Option<String> {
    if !ARMED.load(Ordering::Acquire) {
        return None;
    }
    let mut pending = PENDING.lock().expect("find reveal");
    if pending
        .as_ref()
        .is_some_and(|(_, _, at)| at.elapsed() > EXPIRES)
    {
        *pending = None;
        ARMED.store(false, Ordering::Release);
        return None;
    }
    if pending.as_ref().is_none_or(|(id, _, _)| id != session) {
        return None;
    }
    ARMED.store(false, Ordering::Release);
    pending.take().map(|(_, query, _)| query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_named_session_takes_the_request_once() {
        let wanted = SessionId("find-reveal-a".into());
        request(wanted.clone(), "Traceback".into());
        assert_eq!(take_for(&SessionId("find-reveal-b".into())), None);
        assert_eq!(take_for(&wanted).as_deref(), Some("Traceback"));
        assert_eq!(take_for(&wanted), None);
    }
}
