//! Compare an always-paced window to a demand-paced window on the main
//! display. No NSWindow, rendering, screen recording, or input injection.
#[cfg(all(target_os = "macos", not(test)))]
#[path = "../src/display_link.rs"]
mod display_link;

#[cfg(all(target_os = "macos", not(test)))]
fn main() {
    use core_foundation::runloop::{CFRunLoop, kCFRunLoopDefaultMode};
    use core_graphics::display::CGDisplay;
    use display_link::WindowFrameSource;
    use std::{
        cell::{Cell, RefCell},
        ffi::c_void,
        time::Duration,
    };

    thread_local! {
        static SOURCE: RefCell<Option<WindowFrameSource>> = const { RefCell::new(None) };
        static IDLE: Cell<bool> = const { Cell::new(false) };
        static FRAMES: Cell<usize> = const { Cell::new(0) };
    }

    extern "C" fn frame(_: *mut c_void) {
        SOURCE.with_borrow_mut(|source| {
            let source = source.as_mut().unwrap();
            if source.accept_frame() {
                FRAMES.set(FRAMES.get() + 1);
                if IDLE.get() {
                    source.frame_demand(false);
                }
            }
        });
    }

    let mut source = WindowFrameSource::new(std::ptr::null_mut(), frame);
    source.start(CGDisplay::main().id).expect("display link");
    let wake = source.idle_waker();
    SOURCE.with_borrow_mut(|slot| *slot = Some(source));
    let sample = |seconds| {
        FRAMES.set(0);
        unsafe {
            CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, Duration::from_secs(seconds), false)
        };
        FRAMES.get()
    };
    let continuous = sample(3);
    assert!(continuous > 3, "no running display clock");
    IDLE.set(true);
    let settling = sample(1);
    let parked = sample(3);
    wake();
    let resumed = sample(1);
    println!(
        "3 s continuous: {continuous}, settling: {settling}, 3 s parked: {parked}, after wake: {resumed}"
    );
    assert_eq!((settling, parked, resumed), (3, 0, 3));
    SOURCE.with_borrow_mut(|slot| {
        let source = slot.as_mut().unwrap();
        source.stop();
        assert!(!source.request_now());
    });
    wake();
    assert_eq!(sample(1), 0, "an occluded source stayed awake");
    SOURCE.with_borrow_mut(|source| source.take());
}

#[cfg(any(not(target_os = "macos"), test))]
fn main() {}
