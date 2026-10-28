//! Frame pacing for macOS windows, built on `CVDisplayLink`.
//!
//! CVDisplayLink has no safe teardown: `CVDisplayLinkStop` merely flags the
//! link's io thread to exit "soon" and returns immediately, and there is no
//! way to learn when (or whether) a final output callback has finished. Two
//! crash classes came from ignoring this:
//!
//! * releasing the link after `stop` let the still-running io thread read the
//!   freed link's internals (segfault in `CVHWTime::reset`, fixed by leaking
//!   the link in #32116);
//! * releasing the dispatch source that the output callback dereferenced via
//!   its context pointer let a straggler callback read freed memory (segfault
//!   in `dispatch_source_merge_data`, Sentry issue ZED-7XR).
//!
//! Instead of leaking one link and one source per teardown (and teardown
//! happens on every resize tick, activation, and occlusion change), we keep a
//! single immortal `CVDisplayLink` per display in a static registry and have
//! windows subscribe to it:
//!
//! * Links are created lazily and never released, so the release race cannot
//!   occur. The output callback's context is the display id (an integer, not
//!   a pointer), and the only memory the callback dereferences is the static
//!   registry, so a straggler callback after `stop` is a harmless no-op.
//! * Each window owns one dispatch source for the life of the window. On
//!   window close it is removed from the registry under the lock (making it
//!   unreachable from the callback), cancelled, and genuinely released.
//! * A display's link runs iff it has subscribers; windows never start or
//!   stop links directly, so interleaved starts/stops of windows sharing a
//!   display cannot conflict.
//!
//! One tradeoff of immortal entries: a link created for a given
//! `CGDirectDisplayID` is reused forever, including after the display is
//! unplugged and one reappears with the same id, or after mode/refresh-rate
//! changes. `CVDisplayLink` looks up display timing dynamically, so a cached
//! link keeps pacing correctly; if that ever proves untrue the fix is to
//! also refresh entries from a display-reconfiguration callback, not to
//! release links (which would reintroduce the teardown race).
//!
//! Lock ordering: the output callback runs on the link's io thread and takes
//! the registry lock, possibly while holding CVDisplayLink-internal locks. To
//! avoid a lock cycle through those (undocumented) internals, we never call a
//! CoreVideo function while holding the registry lock. Registry mutations and
//! link start/stop happen on the main thread only, which keeps the `running`
//! flag and the link's actual state consistent without holding the lock
//! across the calls.
//!
//! `std::sync::Mutex` rather than `parking_lot` is deliberate: on macOS it is
//! currently backed by `os_unfair_lock`, whose priority donation resolves
//! inversions between the high-priority io thread and the main thread. (That
//! is a std implementation detail, not a guarantee; if it changes, the cost
//! is added latency under contention, not incorrectness.)

use anyhow::Result;
use core_graphics::display::CGDirectDisplayID;
use dispatch2::{
    _dispatch_source_type_data_add, DispatchObject, DispatchQueue, DispatchRetained, DispatchSource,
};
use gpui_util::ResultExt;
use std::{
    collections::{BTreeMap, btree_map},
    ffi::c_void,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

static REGISTRY: Mutex<Registry> = Mutex::new(Registry::new());

struct Registry {
    displays: BTreeMap<CGDirectDisplayID, DisplayEntry>,
    next_subscriber_id: u64,
}

impl Registry {
    const fn new() -> Self {
        Registry {
            displays: BTreeMap::new(),
            next_subscriber_id: 0,
        }
    }
}

struct DisplayEntry {
    link: sys::DisplayLink,
    running: bool,
    subscribers: Vec<(SubscriberId, DispatchRetained<DispatchSource>)>,
    timing: Arc<DisplayTiming>,
}

/// DIRI PATCH (frame target time): the display's refresh grid, as its link
/// last reported it, so a frame can learn which refresh it is drawn for.
/// Written by the link's io thread, read on the main thread; the three
/// values are independent atomics, and a read that mixes two callbacks is
/// off by at most one period's drift.
#[derive(Default)]
pub(crate) struct DisplayTiming {
    /// Host nanoseconds of the latest refresh the link called back for.
    output_ns: AtomicU64,
    /// Nanoseconds between refreshes.
    period_ns: AtomicU64,
    /// How far ahead of its callback the link aims: output minus current.
    lead_ns: AtomicU64,
}

impl DisplayTiming {
    fn record(&self, current: &sys::CVTimeStamp, output: &sys::CVTimeStamp) {
        let valid = sys::kCVTimeStampHostTimeValid;
        if current.flags & valid == 0 || output.flags & valid == 0 {
            return;
        }
        let output_ns = host_ns(output.host_time);
        let current_ns = host_ns(current.host_time);
        let period_ns = if output.flags & sys::kCVTimeStampVideoRefreshPeriodValid != 0
            && output.video_time_scale > 0
            && output.video_refresh_period > 0
        {
            (output.video_refresh_period as u128 * 1_000_000_000
                / output.video_time_scale as u128) as u64
        } else {
            0
        };
        if period_ns == 0 {
            return;
        }
        self.period_ns.store(period_ns, Ordering::Relaxed);
        self.lead_ns
            .store(output_ns.saturating_sub(current_ns), Ordering::Relaxed);
        self.output_ns.store(output_ns, Ordering::Relaxed);
    }

    /// The refresh a frame starting at host time `now_ns` will be shown on:
    /// the one on the link's grid nearest to `now_ns` plus the link's lead.
    /// A frame run by a display-link tick gets that tick's output time; one
    /// run early (an immediate frame) or late gets the refresh it really
    /// lands on, as the link would have aimed it.
    fn target_ns(&self, now_ns: u64) -> Option<u64> {
        let output_ns = self.output_ns.load(Ordering::Relaxed);
        let period_ns = self.period_ns.load(Ordering::Relaxed);
        if output_ns == 0 || period_ns == 0 {
            return None;
        }
        let aim = now_ns.saturating_add(self.lead_ns.load(Ordering::Relaxed));
        let offset = aim as i128 - output_ns as i128;
        let period = period_ns as i128;
        let refreshes = (offset + offset.signum() * period / 2) / period;
        u64::try_from(output_ns as i128 + refreshes * period).ok()
    }
}

/// Host time (`mach_absolute_time`, the clock of `CVTimeStamp.hostTime` and
/// of `Instant`) in nanoseconds.
fn host_ns(host_time: u64) -> u64 {
    static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();
    let (numer, denom) = *TIMEBASE.get_or_init(|| {
        let mut info = sys::MachTimebaseInfo { numer: 0, denom: 0 };
        if unsafe { sys::mach_timebase_info(&mut info) } != 0 || info.denom == 0 {
            return (1, 1);
        }
        (info.numer, info.denom)
    });
    (host_time as u128 * numer as u128 / denom as u128) as u64
}

/// Now on the host clock, in nanoseconds.
pub(crate) fn host_now_ns() -> u64 {
    host_ns(unsafe { sys::mach_absolute_time() })
}

/// The `Instant` of host time `at_ns`, given `now` read together with
/// `now_ns`.
pub(crate) fn instant_at(now: Instant, now_ns: u64, at_ns: u64) -> Option<Instant> {
    if at_ns >= now_ns {
        now.checked_add(Duration::from_nanos(at_ns - now_ns))
    } else {
        now.checked_sub(Duration::from_nanos(now_ns - at_ns))
    }
}

// SAFETY: Both fields wrapping raw pointers are refcounted handles to
// thread-safe objects that are valid on any thread: `sys::DisplayLink` to a
// CoreVideo object, and each subscriber's `DispatchRetained<DispatchSource>`
// to a GCD object (which the display's io thread really does use, calling
// `merge_data` from the output callback). All mutation of the entry itself
// is serialized by the registry lock.
unsafe impl Send for DisplayEntry {}

#[derive(Copy, Clone, PartialEq, Eq)]
struct SubscriberId(u64);

fn lock_registry() -> MutexGuard<'static, Registry> {
    // Proceeding past poison is safe here (the map's invariants hold after
    // any partial mutation), and panicking instead would abort the process
    // when this is reached from the extern "C" output callback.
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

fn debug_assert_main_thread() {
    #[cfg(debug_assertions)]
    {
        use objc::{class, msg_send, sel, sel_impl};
        let is_main_thread: objc::runtime::BOOL =
            unsafe { msg_send![class!(NSThread), isMainThread] };
        debug_assert!(
            is_main_thread == objc::runtime::YES,
            "display link registry mutations must happen on the main thread; \
             the registry's lock ordering and state consistency depend on it"
        );
    }
}

unsafe extern "C" fn display_link_output_callback(
    _display_link_out: *mut sys::CVDisplayLink,
    current_time: *const sys::CVTimeStamp,
    output_time: *const sys::CVTimeStamp,
    _flags_in: i64,
    _flags_out: *mut i64,
    display_id: *mut c_void,
) -> i32 {
    let display_id = display_id as usize as CGDirectDisplayID;
    let registry = lock_registry();
    if let Some(entry) = registry.displays.get(&display_id) {
        // SAFETY: CoreVideo passes valid timestamps for the duration of the
        // callback; null is checked anyway.
        if let (Some(current), Some(output)) =
            unsafe { (current_time.as_ref(), output_time.as_ref()) }
        {
            entry.timing.record(current, output);
        }
        for (_, frame_requests) in &entry.subscribers {
            frame_requests.merge_data(1);
        }
    }
    0
}

fn subscribe(
    display_id: CGDirectDisplayID,
    frame_requests: DispatchRetained<DispatchSource>,
) -> Result<(SubscriberId, Arc<DisplayTiming>)> {
    debug_assert_main_thread();

    let needs_link = !lock_registry().displays.contains_key(&display_id);
    let new_link = if needs_link {
        // Created outside the registry lock; see the lock ordering note above.
        Some(unsafe {
            sys::DisplayLink::new(
                display_id,
                display_link_output_callback,
                display_id as usize as *mut c_void,
            )?
        })
    } else {
        None
    };

    let (subscriber_id, timing, link_to_start) = {
        let mut registry = lock_registry();
        let registry = &mut *registry;
        let subscriber_id = SubscriberId(registry.next_subscriber_id);
        registry.next_subscriber_id += 1;
        let entry = match (registry.displays.entry(display_id), new_link) {
            // If an entry appeared since the check above, dropping `new_link`
            // is safe: a never-started link has no io thread, so releasing it
            // doesn't race.
            (btree_map::Entry::Occupied(entry), _) => entry.into_mut(),
            (btree_map::Entry::Vacant(vacant), Some(link)) => vacant.insert(DisplayEntry {
                link,
                running: false,
                subscribers: Vec::new(),
                timing: Arc::default(),
            }),
            (btree_map::Entry::Vacant(_), None) => {
                // Entries are never removed, so an entry observed above cannot
                // have disappeared while subscriptions stay on the main thread.
                anyhow::bail!("display link registry entry vanished for display {display_id}");
            }
        };
        entry.subscribers.push((subscriber_id, frame_requests));
        let timing = Arc::clone(&entry.timing);
        let link_to_start = if entry.running {
            None
        } else {
            entry.running = true;
            // Clone the refcounted handle so the CVDisplayLinkStart call can
            // happen after the lock is released.
            Some(entry.link.clone())
        };
        (subscriber_id, timing, link_to_start)
    };

    if let Some(mut link) = link_to_start {
        if let Err(error) = unsafe { link.start() } {
            let mut registry = lock_registry();
            if let Some(entry) = registry.displays.get_mut(&display_id) {
                entry.running = false;
                entry.subscribers.retain(|(id, _)| *id != subscriber_id);
            }
            return Err(error);
        }
    }

    Ok((subscriber_id, timing))
}

fn unsubscribe(display_id: CGDirectDisplayID, subscriber_id: SubscriberId) {
    debug_assert_main_thread();

    let link_to_stop = {
        let mut registry = lock_registry();
        let Some(entry) = registry.displays.get_mut(&display_id) else {
            return;
        };
        entry.subscribers.retain(|(id, _)| *id != subscriber_id);
        if entry.subscribers.is_empty() && entry.running {
            entry.running = false;
            Some(entry.link.clone())
        } else {
            None
        }
    };

    if let Some(mut link) = link_to_stop {
        // A final output callback can still fire after this returns; it finds
        // no subscribers for this display and does nothing.
        unsafe { link.stop().log_err() };
    }
}

/// A per-window source of frame requests, paced by the display the window is
/// on. The wrapped dispatch source coalesces vsync ticks from the display's
/// io thread and invokes `callback(data)` on the main queue.
pub struct WindowFrameSource {
    frame_requests: DispatchRetained<DispatchSource>,
    registration: Option<(CGDirectDisplayID, SubscriberId)>,
    timing: Option<Arc<DisplayTiming>>,
}

impl WindowFrameSource {
    pub fn new(data: *mut c_void, callback: extern "C" fn(*mut c_void)) -> Self {
        let frame_requests = unsafe {
            let frame_requests = DispatchSource::new(
                &raw const _dispatch_source_type_data_add as *mut _,
                0,
                0,
                Some(DispatchQueue::main()),
            );
            frame_requests.set_context(data);
            frame_requests.set_event_handler_f(callback);
            // Resume before this source can ever be dropped: destroying a
            // suspended dispatch source is undefined behavior (#50875).
            frame_requests.resume();
            frame_requests
        };
        Self {
            frame_requests,
            registration: None,
            timing: None,
        }
    }

    pub fn start(&mut self, display_id: CGDirectDisplayID) -> Result<()> {
        self.stop();
        let (subscriber_id, timing) = subscribe(display_id, self.frame_requests.clone())?;
        self.registration = Some((display_id, subscriber_id));
        self.timing = Some(timing);
        Ok(())
    }

    /// When a frame starting now will be on the display, from the timing the
    /// display's link last reported; `None` before its first callback.
    pub fn frame_target_time(&self) -> Option<Instant> {
        let timing = self.timing.as_ref()?;
        let now = Instant::now();
        let now_ns = host_now_ns();
        instant_at(now, now_ns, timing.target_ns(now_ns)?)
    }

    /// Queues one frame request on the main queue now, as a display-link tick
    /// would, while the source is subscribed (the window is visible). Pending
    /// requests coalesce, so this and a tick already queued run one frame.
    pub fn request_now(&self) -> bool {
        if self.registration.is_none() {
            return false;
        }
        self.frame_requests.merge_data(1);
        true
    }

    pub fn stop(&mut self) {
        if let Some((display_id, subscriber_id)) = self.registration.take() {
            unsubscribe(display_id, subscriber_id);
        }
        self.timing = None;
    }
}

impl Drop for WindowFrameSource {
    fn drop(&mut self) {
        self.stop();
        // Unsubscribing makes this source unreachable from the output
        // callback, so unlike before (ZED-7XR) it is safe to actually release
        // it. Cancelling first guarantees the event handler never runs again;
        // its context points at the window's native view, which may be
        // deallocated after this.
        self.frame_requests.cancel();
    }
}

mod sys {
    //! Derived from display-link crate under the following license:
    //! <https://github.com/BrainiumLLC/display-link/blob/master/LICENSE-MIT>
    //! Apple docs: [CVDisplayLink](https://developer.apple.com/documentation/corevideo/cvdisplaylinkoutputcallback?language=objc)
    #![allow(dead_code, non_upper_case_globals)]

    use anyhow::Result;
    use core_graphics::display::CGDirectDisplayID;
    use foreign_types::{ForeignType, foreign_type};
    use std::{
        ffi::c_void,
        fmt::{self, Debug, Formatter},
    };

    #[derive(Debug)]
    pub enum CVDisplayLink {}

    foreign_type! {
        pub unsafe type DisplayLink {
            type CType = CVDisplayLink;
            fn drop = CVDisplayLinkRelease;
            fn clone = CVDisplayLinkRetain;
        }
    }

    impl Debug for DisplayLink {
        fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
            formatter
                .debug_tuple("DisplayLink")
                .field(&self.as_ptr())
                .finish()
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(crate) struct CVTimeStamp {
        pub version: u32,
        pub video_time_scale: i32,
        pub video_time: i64,
        pub host_time: u64,
        pub rate_scalar: f64,
        pub video_refresh_period: i64,
        pub smpte_time: CVSMPTETime,
        pub flags: u64,
        pub reserved: u64,
    }

    pub type CVTimeStampFlags = u64;

    pub const kCVTimeStampVideoTimeValid: CVTimeStampFlags = 1 << 0;
    pub const kCVTimeStampHostTimeValid: CVTimeStampFlags = 1 << 1;
    pub const kCVTimeStampSMPTETimeValid: CVTimeStampFlags = 1 << 2;
    pub const kCVTimeStampVideoRefreshPeriodValid: CVTimeStampFlags = 1 << 3;
    pub const kCVTimeStampRateScalarValid: CVTimeStampFlags = 1 << 4;
    pub const kCVTimeStampTopField: CVTimeStampFlags = 1 << 16;
    pub const kCVTimeStampBottomField: CVTimeStampFlags = 1 << 17;
    pub const kCVTimeStampVideoHostTimeValid: CVTimeStampFlags =
        kCVTimeStampVideoTimeValid | kCVTimeStampHostTimeValid;
    pub const kCVTimeStampIsInterlaced: CVTimeStampFlags =
        kCVTimeStampTopField | kCVTimeStampBottomField;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub(crate) struct CVSMPTETime {
        pub subframes: i16,
        pub subframe_divisor: i16,
        pub counter: u32,
        pub time_type: u32,
        pub flags: u32,
        pub hours: i16,
        pub minutes: i16,
        pub seconds: i16,
        pub frames: i16,
    }

    pub type CVSMPTETimeType = u32;

    pub const kCVSMPTETimeType24: CVSMPTETimeType = 0;
    pub const kCVSMPTETimeType25: CVSMPTETimeType = 1;
    pub const kCVSMPTETimeType30Drop: CVSMPTETimeType = 2;
    pub const kCVSMPTETimeType30: CVSMPTETimeType = 3;
    pub const kCVSMPTETimeType2997: CVSMPTETimeType = 4;
    pub const kCVSMPTETimeType2997Drop: CVSMPTETimeType = 5;
    pub const kCVSMPTETimeType60: CVSMPTETimeType = 6;
    pub const kCVSMPTETimeType5994: CVSMPTETimeType = 7;

    pub type CVSMPTETimeFlags = u32;

    pub const kCVSMPTETimeValid: CVSMPTETimeFlags = 1 << 0;
    pub const kCVSMPTETimeRunning: CVSMPTETimeFlags = 1 << 1;

    pub type CVDisplayLinkOutputCallback = unsafe extern "C" fn(
        display_link_out: *mut CVDisplayLink,
        // A pointer to the current timestamp. This represents the timestamp when the callback is called.
        current_time: *const CVTimeStamp,
        // A pointer to the output timestamp. This represents the timestamp for when the frame will be displayed.
        output_time: *const CVTimeStamp,
        // Unused
        flags_in: i64,
        // Unused
        flags_out: *mut i64,
        // A pointer to app-defined data.
        display_link_context: *mut c_void,
    ) -> i32;

    #[link(name = "CoreFoundation", kind = "framework")]
    #[link(name = "CoreVideo", kind = "framework")]
    #[allow(improper_ctypes, unknown_lints, clippy::duplicated_attributes)]
    unsafe extern "C" {
        pub fn CVDisplayLinkCreateWithActiveCGDisplays(
            display_link_out: *mut *mut CVDisplayLink,
        ) -> i32;
        pub fn CVDisplayLinkSetCurrentCGDisplay(
            display_link: &mut DisplayLinkRef,
            display_id: u32,
        ) -> i32;
        pub fn CVDisplayLinkSetOutputCallback(
            display_link: &mut DisplayLinkRef,
            callback: CVDisplayLinkOutputCallback,
            user_info: *mut c_void,
        ) -> i32;
        pub fn CVDisplayLinkStart(display_link: &mut DisplayLinkRef) -> i32;
        pub fn CVDisplayLinkStop(display_link: &mut DisplayLinkRef) -> i32;
        pub fn CVDisplayLinkRelease(display_link: *mut CVDisplayLink);
        pub fn CVDisplayLinkRetain(display_link: *mut CVDisplayLink) -> *mut CVDisplayLink;
    }

    #[repr(C)]
    pub(crate) struct MachTimebaseInfo {
        pub numer: u32,
        pub denom: u32,
    }

    unsafe extern "C" {
        pub fn mach_absolute_time() -> u64;
        pub fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }

    impl DisplayLink {
        /// Apple docs: [CVDisplayLinkCreateWithCGDisplay](https://developer.apple.com/documentation/corevideo/1456981-cvdisplaylinkcreatewithcgdisplay?language=objc)
        pub unsafe fn new(
            display_id: CGDirectDisplayID,
            callback: CVDisplayLinkOutputCallback,
            user_info: *mut c_void,
        ) -> Result<Self> {
            unsafe {
                let mut display_link: *mut CVDisplayLink = 0 as _;

                let code = CVDisplayLinkCreateWithActiveCGDisplays(&mut display_link);
                anyhow::ensure!(code == 0, "could not create display link, code: {}", code);

                let mut display_link = DisplayLink::from_ptr(display_link);

                let code = CVDisplayLinkSetOutputCallback(&mut display_link, callback, user_info);
                anyhow::ensure!(code == 0, "could not set output callback, code: {}", code);

                let code = CVDisplayLinkSetCurrentCGDisplay(&mut display_link, display_id);
                anyhow::ensure!(
                    code == 0,
                    "could not assign display to display link, code: {}",
                    code
                );

                Ok(display_link)
            }
        }
    }

    impl DisplayLinkRef {
        /// Apple docs: [CVDisplayLinkStart](https://developer.apple.com/documentation/corevideo/1457193-cvdisplaylinkstart?language=objc)
        pub unsafe fn start(&mut self) -> Result<()> {
            unsafe {
                let code = CVDisplayLinkStart(self);
                anyhow::ensure!(code == 0, "could not start display link, code: {}", code);
                Ok(())
            }
        }

        /// Apple docs: [CVDisplayLinkStop](https://developer.apple.com/documentation/corevideo/1457281-cvdisplaylinkstop?language=objc)
        pub unsafe fn stop(&mut self) -> Result<()> {
            unsafe {
                let code = CVDisplayLinkStop(self);
                anyhow::ensure!(code == 0, "could not stop display link, code: {}", code);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
#[path = "display_link_pacing_tests.rs"]
mod pacing_tests;

#[cfg(test)]
mod frame_target_tests {
    use super::*;

    fn timing(output_ns: u64, period_ns: u64, lead_ns: u64) -> DisplayTiming {
        let timing = DisplayTiming::default();
        timing.output_ns.store(output_ns, Ordering::Relaxed);
        timing.period_ns.store(period_ns, Ordering::Relaxed);
        timing.lead_ns.store(lead_ns, Ordering::Relaxed);
        timing
    }

    #[test]
    fn a_frame_targets_the_refresh_its_link_aims_for() {
        // 120 Hz, the link calling back 16.7 ms (two refreshes) ahead.
        let period = 8_333_333;
        let timing = timing(1_000_000_000, period, 2 * period);
        // A tick frame running just after its callback gets that output.
        let tick = 1_000_000_000 - 2 * period + 300_000;
        assert_eq!(timing.target_ns(tick), Some(1_000_000_000));
        // Run late by most of a refresh, it lands one refresh later.
        assert_eq!(
            timing.target_ns(tick + period - 1_000_000),
            Some(1_000_000_000 + period)
        );
        // Frames a few refreshes later, or earlier, stay on the same grid.
        assert_eq!(
            timing.target_ns(tick + 5 * period),
            Some(1_000_000_000 + 5 * period)
        );
        assert_eq!(
            timing.target_ns(tick - 3 * period),
            Some(1_000_000_000 - 3 * period)
        );
        assert_eq!(DisplayTiming::default().target_ns(tick), None);
    }

    #[test]
    fn host_time_maps_onto_instants() {
        let now = Instant::now();
        let now_ns = host_now_ns();
        let later = instant_at(now, now_ns, now_ns + 5_000_000).unwrap();
        assert_eq!(later.duration_since(now), Duration::from_millis(5));
        let earlier = instant_at(now, now_ns, now_ns - 3_000_000).unwrap();
        assert_eq!(now.duration_since(earlier), Duration::from_millis(3));
        // Instant and the host clock are the same clock on macOS.
        let skew = Instant::now().duration_since(now).as_nanos() as i128
            - (host_now_ns() - now_ns) as i128;
        assert!(skew.abs() < 2_000_000, "skew {skew} ns");
    }
}
