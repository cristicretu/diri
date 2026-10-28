//! One readiness thread for every idle lane a holder process hosts.
//!
//! A session's lanes (its PTY reader, control socket, input stream, output
//! subscribers and exit watch) each used to keep a thread blocked for the
//! session's whole life: seven per session, so a manager hosting a hundred
//! idle shells held seven hundred threads that did nothing but wait. A lane
//! now keeps its thread only while it is busy. After [`LANE_IDLE`] without an
//! event it hands its descriptor here and the thread ends; when the
//! descriptor turns readable, this thread runs the lane's wake, which starts
//! a fresh thread to carry on. A hundred idle sessions cost this one thread.
//!
//! Wakes run on the reactor thread, so they must be short and must never
//! block: in practice they spawn a thread or close a queue. Each registration
//! fires at most once (`EV_ONESHOT`, `EPOLLONESHOT`); a lane that wants more
//! registers again.
//!
//! Ownership discipline: whoever registered a descriptor keeps it open until
//! the wake has run or [`Reactor::cancel`] has returned. Cancelling removes
//! the kernel registration while the descriptor is still open, so a reused
//! descriptor number can never inherit a stale wake.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// How long a lane keeps its thread after its last event. Matches the point
/// at which a reference multiplexer parks an idle terminal's reader: long
/// enough that typing and streaming output never pay a thread start, short
/// enough that a fleet of idle sessions holds no threads.
pub(crate) const LANE_IDLE: Duration = Duration::from_secs(2);

/// [`LANE_IDLE`] for the control socket. Its connections are one-shot
/// requests, so a new thread costs a request nothing it would notice (tens of
/// microseconds against a stat's hundreds), and an Engine sampling a busy
/// shell every couple of seconds must not keep a thread per session alive by
/// arriving just before each park. A burst (a launch's stat and stream
/// negotiations, or the Engine's fast samples of an active Agent) still
/// shares one thread.
pub(super) const CONTROL_IDLE: Duration = Duration::from_millis(500);

/// Identifies one registration, for [`Reactor::cancel`]. Never reused.
pub(crate) type Token = u64;

type Wake = Box<dyn FnOnce() + Send>;

struct Waiter {
    fd: RawFd,
    /// Also armed with a timer (see [`Reactor::when_readable_within`]).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    timer: bool,
    wake: Wake,
}

pub(crate) struct Reactor {
    poller: OwnedFd,
    waiting: Mutex<HashMap<Token, Waiter>>,
    next: AtomicU64,
}

static SHARED: OnceLock<Option<&'static Reactor>> = OnceLock::new();

/// The process's reactor, started on first use. `None` if the kernel refused
/// a poller or a thread: every lane then keeps its thread, exactly as before
/// lanes could park, which is slower to idle but always correct.
pub(crate) fn shared() -> Option<&'static Reactor> {
    *SHARED.get_or_init(|| {
        lane_qos::capture();
        let reactor: &'static Reactor = Box::leak(Box::new(Reactor::new().ok()?));
        std::thread::Builder::new()
            .name("diri-reactor".into())
            .spawn(move || reactor.run())
            .ok()?;
        Some(reactor)
    })
}

/// Threads started from the reactor would inherit its interactive priority.
/// Lanes instead run at the priority of the thread that first started a
/// session in this process, which is what they inherited when each lane kept
/// a thread started there for the session's whole life.
pub(crate) mod lane_qos {
    #[cfg(target_vendor = "apple")]
    static CAPTURED: std::sync::OnceLock<(libc::qos_class_t, libc::c_int)> =
        std::sync::OnceLock::new();

    pub(super) fn capture() {
        #[cfg(target_vendor = "apple")]
        CAPTURED.get_or_init(|| {
            let mut class = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
            let mut relative = 0;
            // SAFETY: valid out-params for the calling thread's own QoS.
            unsafe {
                libc::pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut relative)
            };
            (class, relative)
        });
    }

    /// Puts the calling lane thread at the captured priority.
    pub(crate) fn adopt() {
        #[cfg(target_vendor = "apple")]
        if let Some(&(class, relative)) = CAPTURED.get()
            && !matches!(class, libc::qos_class_t::QOS_CLASS_UNSPECIFIED)
        {
            // SAFETY: changes only the calling thread's QoS class.
            unsafe { libc::pthread_set_qos_class_self_np(class, relative) };
        }
    }
}

/// How long a lane may sit idle before parking: [`LANE_IDLE`] when a reactor
/// will take it, otherwise forever (the lane keeps its thread).
pub(super) fn lane_idle() -> Option<Duration> {
    shared().map(|_| LANE_IDLE)
}

impl Reactor {
    fn new() -> std::io::Result<Self> {
        let poller = new_poller()?;
        Ok(Self {
            poller,
            waiting: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        })
    }

    /// Runs `wake` once, on the reactor thread, when `fd` is readable or hung
    /// up. The caller keeps `fd` open until then or until it cancels.
    pub(super) fn when_readable(
        &self,
        fd: RawFd,
        wake: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<Token> {
        let token = self.next.fetch_add(1, Ordering::Relaxed);
        // In the table before the kernel knows, so an event that fires at
        // once always finds its wake.
        self.waiting.lock().expect("reactor").insert(
            token,
            Waiter {
                fd,
                timer: false,
                wake: Box::new(wake),
            },
        );
        if let Err(error) = arm(self.poller.as_raw_fd(), fd, token) {
            self.waiting.lock().expect("reactor").remove(&token);
            return Err(error);
        }
        Ok(token)
    }

    /// [`Self::when_readable`], or after `timeout` if that comes first.
    #[cfg(target_os = "macos")]
    pub(crate) fn when_readable_within(
        &self,
        fd: RawFd,
        timeout: Duration,
        wake: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<Token> {
        let token = self.next.fetch_add(1, Ordering::Relaxed);
        self.waiting.lock().expect("reactor").insert(
            token,
            Waiter {
                fd,
                timer: true,
                wake: Box::new(wake),
            },
        );
        let armed = arm(self.poller.as_raw_fd(), fd, token)
            .and_then(|()| arm_timer(self.poller.as_raw_fd(), token, timeout));
        if let Err(error) = armed {
            disarm(self.poller.as_raw_fd(), fd);
            self.waiting.lock().expect("reactor").remove(&token);
            return Err(error);
        }
        Ok(token)
    }

    /// Withdraws a registration that has not fired. Returns false if its wake
    /// already ran or is running; either way it will never run after this.
    pub(super) fn cancel(&self, token: Token) -> bool {
        let waiter = self.waiting.lock().expect("reactor").remove(&token);
        match waiter {
            Some(waiter) => {
                disarm(self.poller.as_raw_fd(), waiter.fd);
                #[cfg(target_os = "macos")]
                if waiter.timer {
                    disarm_timer(self.poller.as_raw_fd(), token);
                }
                // Dropped only now: the wake may own the descriptor.
                drop(waiter);
                true
            }
            None => false,
        }
    }

    fn run(&self) {
        // The first keystroke after an idle pause comes through here; it
        // should not queue behind background work.
        super::server::prioritize_interactive_io();
        let mut ready = Vec::with_capacity(64);
        loop {
            ready.clear();
            if wait(self.poller.as_raw_fd(), &mut ready).is_err() {
                // Only a broken poller fails here; keep the thread rather
                // than spin, and leave lanes to their fallback timers.
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            for &(token, _timed_out) in &ready {
                let waiter = self.waiting.lock().expect("reactor").remove(&token);
                if let Some(waiter) = waiter {
                    // Whichever of a timed registration's two events fired
                    // first, the other must go before the wake can close
                    // the descriptor.
                    #[cfg(target_os = "macos")]
                    if waiter.timer {
                        if _timed_out {
                            disarm(self.poller.as_raw_fd(), waiter.fd);
                        } else {
                            disarm_timer(self.poller.as_raw_fd(), token);
                        }
                    }
                    // A panicking wake must not take every other session's
                    // wakes with it.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(waiter.wake));
                }
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn new_poller() -> std::io::Result<OwnedFd> {
    // SAFETY: kqueue takes no arguments and returns a fresh descriptor.
    let fd = unsafe { libc::kqueue() };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: fcntl on the descriptor just created.
    unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    Ok(fd)
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn change(poller: RawFd, fd: RawFd, flags: u16, token: Token) -> std::io::Result<()> {
    submit(
        poller,
        libc::kevent {
            ident: fd as libc::uintptr_t,
            filter: libc::EVFILT_READ,
            flags,
            fflags: 0,
            data: 0,
            udata: token as usize as *mut libc::c_void,
        },
    )
}

/// A one-shot timer named by the registration's own token, which is never
/// reused, so deleting it can never hit another registration.
#[cfg(target_os = "macos")]
fn arm_timer(poller: RawFd, token: Token, timeout: Duration) -> std::io::Result<()> {
    submit(
        poller,
        libc::kevent {
            ident: token as libc::uintptr_t,
            filter: libc::EVFILT_TIMER,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: 0,
            data: libc::intptr_t::try_from(timeout.as_millis()).unwrap_or(libc::intptr_t::MAX),
            udata: token as usize as *mut libc::c_void,
        },
    )
}

#[cfg(target_os = "macos")]
fn disarm_timer(poller: RawFd, token: Token) {
    let _ = submit(
        poller,
        libc::kevent {
            ident: token as libc::uintptr_t,
            filter: libc::EVFILT_TIMER,
            flags: libc::EV_DELETE,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        },
    );
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn submit(poller: RawFd, event: libc::kevent) -> std::io::Result<()> {
    // SAFETY: one initialized change, no output events requested.
    let result =
        unsafe { libc::kevent(poller, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn arm(poller: RawFd, fd: RawFd, token: Token) -> std::io::Result<()> {
    change(poller, fd, libc::EV_ADD | libc::EV_ONESHOT, token)
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn disarm(poller: RawFd, fd: RawFd) {
    // ENOENT when the one-shot already fired in the kernel: nothing to do.
    let _ = change(poller, fd, libc::EV_DELETE, 0);
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn wait(poller: RawFd, ready: &mut Vec<(Token, bool)>) -> std::io::Result<()> {
    // SAFETY: zeroed kevents are valid out-params.
    let mut events: [libc::kevent; 64] = unsafe { std::mem::zeroed() };
    // SAFETY: a writable array of the length passed; no timeout.
    let count = unsafe {
        libc::kevent(
            poller,
            std::ptr::null(),
            0,
            events.as_mut_ptr(),
            events.len() as libc::c_int,
            std::ptr::null(),
        )
    };
    if count < 0 {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::Interrupted {
            Ok(())
        } else {
            Err(error)
        };
    }
    ready.extend(events[..count as usize].iter().map(|event| {
        (
            event.udata as usize as Token,
            event.filter == libc::EVFILT_TIMER,
        )
    }));
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn new_poller() -> std::io::Result<OwnedFd> {
    // SAFETY: epoll_create1 takes a flag and returns a fresh descriptor.
    let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn arm(poller: RawFd, fd: RawFd, token: Token) -> std::io::Result<()> {
    let mut event = libc::epoll_event {
        events: (libc::EPOLLIN | libc::EPOLLRDHUP | libc::EPOLLONESHOT) as u32,
        u64: token,
    };
    // SAFETY: a valid event for the duration of the call.
    if unsafe { libc::epoll_ctl(poller, libc::EPOLL_CTL_ADD, fd, &mut event) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::EEXIST) {
        return Err(error);
    }
    // A one-shot that fired stays in the interest list, disabled: re-arm it.
    // SAFETY: as above.
    if unsafe { libc::epoll_ctl(poller, libc::EPOLL_CTL_MOD, fd, &mut event) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn disarm(poller: RawFd, fd: RawFd) {
    // SAFETY: the event argument is ignored for EPOLL_CTL_DEL.
    let _ = unsafe { libc::epoll_ctl(poller, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn wait(poller: RawFd, ready: &mut Vec<(Token, bool)>) -> std::io::Result<()> {
    // SAFETY: zeroed epoll_events are valid out-params.
    let mut events: [libc::epoll_event; 64] = unsafe { std::mem::zeroed() };
    // SAFETY: a writable array of the length passed; no timeout.
    let count =
        unsafe { libc::epoll_wait(poller, events.as_mut_ptr(), events.len() as libc::c_int, -1) };
    if count < 0 {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::Interrupted {
            Ok(())
        } else {
            Err(error)
        };
    }
    ready.extend(
        events[..count as usize]
            .iter()
            .map(|event| (event.u64, false)),
    );
    Ok(())
}

/// Waits up to `timeout` (forever when `None`) for `fd` or `wake` to be
/// readable. Returns which of the two is, or `Idle` when the time ran out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Readiness {
    Ready,
    Woken,
    Idle,
}

pub(super) fn wait_readable(
    fd: RawFd,
    wake: Option<RawFd>,
    timeout: Option<Duration>,
) -> std::io::Result<Readiness> {
    let mut descriptors = [
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let count = if wake.is_some() { 2 } else { 1 };
    let millis = timeout.map_or(-1, |timeout| {
        libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX)
    });
    loop {
        // SAFETY: `count` initialized pollfds that outlive the call.
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), count as libc::nfds_t, millis) };
        if ready > 0 {
            // Any event on the descriptor, errors included: the read that
            // follows turns a broken descriptor into EOF, where ignoring it
            // would spin.
            return Ok(if descriptors[0].revents != 0 {
                Readiness::Ready
            } else {
                Readiness::Woken
            });
        }
        if ready == 0 {
            return Ok(Readiness::Idle);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc;

    #[test]
    fn a_wake_runs_once_when_its_descriptor_turns_readable() {
        let reactor = shared().expect("reactor");
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        let (sender, received) = mpsc::channel();
        reactor
            .when_readable(reader.as_raw_fd(), move || sender.send(()).expect("send"))
            .expect("register");
        assert!(received.recv_timeout(Duration::from_millis(200)).is_err());
        writer.write_all(b"x").expect("write");
        received
            .recv_timeout(Duration::from_secs(5))
            .expect("woken once readable");
        // One-shot: still readable, but nothing more fires.
        assert!(received.recv_timeout(Duration::from_millis(200)).is_err());
        drop(reader);
    }

    #[test]
    fn a_cancelled_wake_never_runs_and_releases_what_it_held() {
        let reactor = shared().expect("reactor");
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        let held = std::sync::Arc::new(());
        let (sender, received) = mpsc::channel::<()>();
        let token = {
            let held = std::sync::Arc::clone(&held);
            reactor
                .when_readable(reader.as_raw_fd(), move || {
                    let _ = &held;
                    let _ = sender.send(());
                })
                .expect("register")
        };
        assert!(reactor.cancel(token));
        assert!(!reactor.cancel(token), "a token cancels once");
        assert_eq!(std::sync::Arc::strong_count(&held), 1);
        writer.write_all(b"x").expect("write");
        assert!(received.recv_timeout(Duration::from_millis(200)).is_err());
        drop(reader);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_timed_wake_runs_once_at_its_deadline_or_on_readiness() {
        let reactor = shared().expect("reactor");
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        let (sender, received) = mpsc::channel();
        let started = std::time::Instant::now();
        {
            let sender = sender.clone();
            reactor
                .when_readable_within(reader.as_raw_fd(), Duration::from_millis(100), move || {
                    sender.send("timer").expect("send");
                })
                .expect("register");
        }
        assert_eq!(received.recv_timeout(Duration::from_secs(5)), Ok("timer"));
        assert!(started.elapsed() >= Duration::from_millis(90));
        // The read half went with it: data now wakes nothing.
        writer.write_all(b"x").expect("write");
        assert!(received.recv_timeout(Duration::from_millis(200)).is_err());

        reactor
            .when_readable_within(reader.as_raw_fd(), Duration::from_millis(300), move || {
                sender.send("read").expect("send");
            })
            .expect("register");
        assert_eq!(received.recv_timeout(Duration::from_secs(5)), Ok("read"));
        // And the timer went with the read.
        assert!(received.recv_timeout(Duration::from_millis(500)).is_err());
        drop(reader);
    }

    #[test]
    fn a_hangup_wakes_like_data() {
        let reactor = shared().expect("reactor");
        let (reader, writer) = std::io::pipe().expect("pipe");
        let (sender, received) = mpsc::channel();
        reactor
            .when_readable(reader.as_raw_fd(), move || sender.send(()).expect("send"))
            .expect("register");
        drop(writer);
        received
            .recv_timeout(Duration::from_secs(5))
            .expect("woken by the hangup");
        drop(reader);
    }
}
