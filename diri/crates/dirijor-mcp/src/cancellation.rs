//! Request-scoped cancellation closes only this read operation's Engine sockets.
//! It never cancels an already-dispatched mutation or kills an Agent session.

use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<State>);

#[derive(Debug, Default)]
struct State {
    cancelled: AtomicBool,
    next_id: AtomicU64,
    sockets: Mutex<Vec<(u64, UnixStream)>>,
}

impl Cancellation {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        for (_, stream) in self.0.sockets.lock().unwrap().drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) fn register(
        &self,
        stream: &UnixStream,
    ) -> Result<Registration, crate::ControlFailure> {
        let mut sockets = self.0.sockets.lock().unwrap();
        if self.is_cancelled() {
            return Err(crate::ControlFailure::Cancelled);
        }
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        sockets.push((id, stream.try_clone()?));
        Ok(Registration {
            owner: self.clone(),
            id,
        })
    }
}

pub(crate) struct Registration {
    owner: Cancellation,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.owner
            .0
            .sockets
            .lock()
            .unwrap()
            .retain(|(id, _)| *id != self.id);
    }
}
