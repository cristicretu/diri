//! A killed account worker keeps its admission slot until actual reap. Cleanup
//! runs only on demand; a stalled OS reap never extends the caller's deadline.
use std::io;
use std::process::Child;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const MAX_ACTIVE: usize = 4;
#[derive(Default)]
pub(super) struct Budget {
    active: AtomicUsize,
    pending: Mutex<Vec<Job>>,
}
pub(super) struct Permit(Arc<Budget>);
struct Job {
    child: Child,
    permit: Permit,
}
impl Budget {
    pub(super) fn acquire(self: &Arc<Self>) -> Option<Permit> {
        // Retry only on a new request, never with a background idle poller.
        let pending = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        for mut job in pending {
            if !matches!(job.child.try_wait(), Ok(Some(_))) {
                schedule(job, spawn);
            }
        }
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_ACTIVE).then_some(active + 1)
            })
            .ok()
            .map(|_| Permit(Arc::clone(self)))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}
impl Permit {
    pub(super) fn reap(self, child: Child) {
        schedule(
            Job {
                child,
                permit: self,
            },
            spawn,
        );
    }
}
fn spawn(work: Box<dyn FnOnce() + Send>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("account-worker-reap".into())
        .spawn(work)
        .map(drop)
}
fn schedule(job: Job, spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>) {
    // The caller retains an ownership slot if thread creation fails. Dropping
    // a rejected spawn closure must not lose the child or release its permit.
    let slot = Arc::new(Mutex::new(Some(job)));
    let worker_slot = Arc::clone(&slot);
    let result = spawn(Box::new(move || {
        let Some(mut job) = worker_slot.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        if job.child.wait().is_err() {
            retain(job);
        }
    }));
    if result.is_err()
        && let Some(job) = slot.lock().unwrap_or_else(|e| e.into_inner()).take()
    {
        retain(job);
    }
}
fn retain(job: Job) {
    let budget = Arc::clone(&job.permit.0);
    budget
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(job);
    // Every queued child still holds one of these same four permits. Even if
    // no more requests arrive, ownership remains recorded until process exit.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    fn child() -> Child {
        std::process::Command::new("/bin/sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }
    #[test]
    fn delayed_reap_retains_all_four_slots_without_blocking_caller() {
        let budget = Arc::new(Budget::default());
        let mut releases = Vec::new();
        let mut completions = Vec::new();
        for _ in 0..MAX_ACTIVE {
            let permit = budget.acquire().unwrap();
            let mut child = child();
            child.kill().unwrap();
            let (release, gate) = mpsc::channel();
            let (done, completion) = mpsc::channel();
            let start = Instant::now();
            schedule(Job { child, permit }, |work| {
                std::thread::spawn(move || {
                    gate.recv().unwrap();
                    work();
                    done.send(()).unwrap();
                });
                Ok(())
            });
            assert!(start.elapsed() < Duration::from_millis(250));
            releases.push(release);
            completions.push(completion);
        }
        assert!(budget.acquire().is_none());
        assert_eq!(budget.active.load(Ordering::Acquire), MAX_ACTIVE);
        for release in releases {
            release.send(()).unwrap();
        }
        for completion in completions {
            completion.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        assert_eq!(budget.active.load(Ordering::Acquire), 0);
        assert!(budget.acquire().is_some());
    }
    #[test]
    fn failed_reaper_spawn_retains_tracked_child_and_permit_until_retry() {
        let budget = Arc::new(Budget::default());
        let permit = budget.acquire().unwrap();
        let mut child = child();
        child.kill().unwrap();
        let pid = child.id();
        schedule(Job { child, permit }, |_| {
            Err(io::Error::other("fixture spawn failure"))
        });
        assert_eq!(budget.active.load(Ordering::Acquire), 1);
        assert_eq!(budget.pending.lock().unwrap()[0].child.id(), pid);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            drop(budget.acquire());
            if budget.active.load(Ordering::Acquire) == 0 {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(budget.pending.lock().unwrap().is_empty());
    }
}
