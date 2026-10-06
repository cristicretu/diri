//! One sample of a stuck main thread's call stack, for `ui.stall`.
//!
//! The watchdog thread suspends the main thread for a few microseconds,
//! reads its registers, walks the frame-pointer chain into a fixed array
//! (nothing allocates while the main thread is stopped: it may hold the
//! allocator's lock) and resumes it. Symbolizing comes after, with
//! `dladdr`, which knows every symbol the shipped binary keeps and every
//! exported system symbol (`mach_msg2_trap`, `-[SMAppService status]`, CoreText).
//!
//! Frames are code locations only: `image symbol`, with the image's file
//! name (never its path) and the demangled function name, or the offset
//! into the image when it has none. Nothing here reads the stack's data.

/// The deepest frames kept; a hang is attributed by its innermost frames
/// and by the diri frame that called into them.
const MAX_FRAMES: usize = 32;

/// A thread to sample: its Mach port and the bounds of its stack.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SampledThread {
    port: u32,
    stack_top: usize,
    stack_bottom: usize,
}

impl SampledThread {
    /// The calling thread, to be sampled later from another thread.
    pub(crate) fn current() -> Option<Self> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            // SAFETY: both read the calling thread's own pthread; the port is
            // returned without a reference to release, and the stack bounds
            // stay valid for the thread's lifetime.
            unsafe {
                let thread = libc::pthread_self();
                let port = libc::pthread_mach_thread_np(thread);
                let top = libc::pthread_get_stackaddr_np(thread) as usize;
                let size = libc::pthread_get_stacksize_np(thread);
                Some(Self {
                    port,
                    stack_top: top,
                    stack_bottom: top.saturating_sub(size),
                })
            }
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        None
    }

    /// The thread's current call stack, innermost first. Empty when it
    /// could not be stopped or read. Never call it on the thread itself.
    pub(crate) fn sample(self) -> Vec<String> {
        let mut addresses = [0usize; MAX_FRAMES];
        let count = self.return_addresses(&mut addresses);
        let mut frames: Vec<String> = addresses[..count]
            .iter()
            .map(|&address| symbolize(address))
            .collect();
        frames.dedup();
        frames
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn return_addresses(self, out: &mut [usize; MAX_FRAMES]) -> usize {
        // SAFETY: the target is suspended while its registers and frame
        // records are read, and resumed before returning. Every frame
        // address is checked to lie inside the target's own stack, word
        // aligned and strictly increasing, so the walk reads only mapped
        // stack memory and terminates.
        unsafe {
            if mach::thread_suspend(self.port) != 0 {
                return 0;
            }
            let mut state = mach::ArmThreadState64::default();
            let mut words = mach::ARM_THREAD_STATE64_COUNT;
            let read = mach::thread_get_state(
                self.port,
                mach::ARM_THREAD_STATE64,
                (&raw mut state).cast(),
                &mut words,
            );
            let mut count = 0;
            if read == 0 {
                out[0] = strip(state.pc as usize);
                // A thread blocked in the kernel sits in a frameless trap
                // stub: its caller is only in the link register. When the
                // innermost function has a frame, the link register repeats
                // the first record (skipped below) or points back into it
                // (merged when symbolized).
                let lr = strip(state.lr as usize);
                out[1] = lr;
                count = 2;
                let mut fp = state.fp as usize;
                while count < MAX_FRAMES && self.holds_frame(fp) {
                    let record = fp as *const usize;
                    let caller_fp = *record;
                    let return_address = strip(*record.add(1));
                    if return_address == 0 {
                        break;
                    }
                    if !(count == 2 && return_address == lr) {
                        out[count] = return_address;
                        count += 1;
                    }
                    if caller_fp <= fp {
                        break;
                    }
                    fp = caller_fp;
                }
            }
            mach::thread_resume(self.port);
            count
        }
    }

    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    fn return_addresses(self, _out: &mut [usize; MAX_FRAMES]) -> usize {
        let _ = (self.port, self.stack_top, self.stack_bottom);
        0
    }

    /// A frame record (caller fp, return address) at `fp` fits in the stack.
    #[cfg_attr(
        not(all(target_os = "macos", target_arch = "aarch64")),
        allow(dead_code)
    )]
    fn holds_frame(self, fp: usize) -> bool {
        fp.is_multiple_of(std::mem::align_of::<usize>())
            && fp >= self.stack_bottom
            && fp.saturating_add(2 * std::mem::size_of::<usize>()) <= self.stack_top
    }
}

/// Drops pointer-authentication bits: system frameworks are arm64e and sign
/// the return addresses they save. User addresses fit in 47 bits.
#[cfg_attr(
    not(all(target_os = "macos", target_arch = "aarch64")),
    allow(dead_code)
)]
fn strip(address: usize) -> usize {
    address & ((1 << 47) - 1)
}

/// `image symbol` for a code address, `image +0x…` without a symbol.
fn symbolize(address: usize) -> String {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: dladdr fills a caller-owned Dl_info; the strings it points
        // at belong to loaded images and are copied out immediately.
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        // A return address points after the call; one byte back stays inside
        // the calling function even when the call was its last instruction.
        let lookup = address.saturating_sub(1);
        if unsafe { libc::dladdr(lookup as *const libc::c_void, &mut info) } != 0 {
            let image = c_str(info.dli_fname)
                .map(|path| path.rsplit('/').next().unwrap_or_default().to_owned())
                .unwrap_or_else(|| "?".to_owned());
            return match c_str(info.dli_sname) {
                Some(name) => format!("{image} {}", demangle(&name)),
                None => format!(
                    "{image} +0x{:x}",
                    address.saturating_sub(info.dli_fbase as usize)
                ),
            };
        }
    }
    format!("? 0x{address:x}")
}

#[cfg(target_os = "macos")]
fn c_str(pointer: *const libc::c_char) -> Option<String> {
    // SAFETY: dladdr's strings are NUL-terminated and live with the image.
    (!pointer.is_null()).then(|| {
        unsafe { std::ffi::CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    })
}

/// Rust's legacy mangling (`_ZN4diri3app5stall17h0123456789abcdefE`) as a
/// path without its hash; anything else (C, Objective-C, Swift) unchanged.
#[cfg_attr(
    not(all(target_os = "macos", target_arch = "aarch64")),
    allow(dead_code)
)]
fn demangle(symbol: &str) -> String {
    let Some(mut rest) = symbol
        .strip_prefix("__ZN")
        .or_else(|| symbol.strip_prefix("_ZN"))
    else {
        return symbol.to_owned();
    };
    let mut parts = Vec::new();
    while let Some(digits) = rest
        .find(|c: char| !c.is_ascii_digit())
        .filter(|&end| end > 0)
    {
        let Ok(len) = rest[..digits].parse::<usize>() else {
            break;
        };
        let Some(part) = rest.get(digits..digits + len) else {
            break;
        };
        parts.push(part);
        rest = &rest[digits + len..];
    }
    if !rest.starts_with('E') || parts.is_empty() {
        return symbol.to_owned();
    }
    if parts.last().is_some_and(|last| {
        last.len() == 17
            && last.starts_with('h')
            && last[1..].bytes().all(|b| b.is_ascii_hexdigit())
    }) {
        parts.pop();
    }
    parts
        .iter()
        .map(|part| unescape(part))
        .collect::<Vec<_>>()
        .join("::")
}

/// Legacy escapes: `$LT$` → `<`, `$u20$` → ` `, `..` → `::`; a part that
/// begins with an escape carries a leading `_`.
#[cfg_attr(
    not(all(target_os = "macos", target_arch = "aarch64")),
    allow(dead_code)
)]
fn unescape(part: &str) -> String {
    let part = part
        .strip_prefix('_')
        .filter(|p| p.starts_with('$'))
        .unwrap_or(part);
    let mut out = String::with_capacity(part.len());
    let mut rest = part;
    while let Some(start) = rest.find(['$', '.']) {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        if let Some(after) = rest.strip_prefix("..") {
            out.push_str("::");
            rest = after;
            continue;
        }
        if let Some(after) = rest.strip_prefix('.') {
            out.push('.');
            rest = after;
            continue;
        }
        let Some(end) = rest[1..].find('$') else {
            break;
        };
        let code = &rest[1..=end];
        let replacement = match code {
            "SP" => "@".to_owned(),
            "BP" => "*".to_owned(),
            "RF" => "&".to_owned(),
            "LT" => "<".to_owned(),
            "GT" => ">".to_owned(),
            "LP" => "(".to_owned(),
            "RP" => ")".to_owned(),
            "C" => ",".to_owned(),
            hex if hex.starts_with('u') => u32::from_str_radix(&hex[1..], 16)
                .ok()
                .and_then(char::from_u32)
                .map(String::from)
                .unwrap_or_default(),
            _ => String::new(),
        };
        out.push_str(&replacement);
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod mach {
    pub(super) const ARM_THREAD_STATE64: i32 = 6;
    pub(super) const ARM_THREAD_STATE64_COUNT: u32 =
        (std::mem::size_of::<ArmThreadState64>() / std::mem::size_of::<u32>()) as u32;

    /// `arm_thread_state64_t` for a non-arm64e process: plain registers.
    #[repr(C)]
    #[derive(Default)]
    pub(super) struct ArmThreadState64 {
        pub(super) x: [u64; 29],
        pub(super) fp: u64,
        pub(super) lr: u64,
        pub(super) sp: u64,
        pub(super) pc: u64,
        pub(super) cpsr: u32,
        pub(super) pad: u32,
    }

    unsafe extern "C" {
        pub(super) fn thread_suspend(thread: u32) -> i32;
        pub(super) fn thread_resume(thread: u32) -> i32;
        pub(super) fn thread_get_state(
            thread: u32,
            flavor: i32,
            state: *mut u32,
            count: *mut u32,
        ) -> i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demangles_legacy_rust_symbols_without_the_hash() {
        assert_eq!(
            demangle(
                "__ZN8diri_app13surface_shell15UtilitySurfaces13open_settings17h0123456789abcdefE"
            ),
            "diri_app::surface_shell::UtilitySurfaces::open_settings"
        );
        assert_eq!(
            demangle(
                "__ZN50_$LT$gpui..app..App$u20$as$u20$core..ops..Drop$GT$4drop17h0123456789abcdefE"
            ),
            "<gpui::app::App as core::ops::Drop>::drop"
        );
        assert_eq!(demangle("mach_msg2_trap"), "mach_msg2_trap");
        assert_eq!(demangle("-[SMAppService status]"), "-[SMAppService status]");
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn samples_another_threads_stack_through_its_callers() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, mpsc};

        // Blocked in the kernel, like a main thread waiting on a reply.
        #[inline(never)]
        fn stall_marker_inner(stop: &AtomicBool) {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        #[inline(never)]
        fn stall_marker_outer(stop: &AtomicBool) {
            stall_marker_inner(stop);
            std::hint::black_box(());
        }

        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let worker = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                tx.send(SampledThread::current().unwrap()).unwrap();
                stall_marker_outer(&stop);
            })
        };
        let thread = rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let frames = thread.sample();
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();

        let inner = frames
            .iter()
            .position(|frame| frame.contains("stall_marker_inner"));
        let outer = frames
            .iter()
            .position(|frame| frame.contains("stall_marker_outer"));
        assert!(
            matches!((inner, outer), (Some(inner), Some(outer)) if inner < outer),
            "{frames:#?}"
        );
        assert!(
            frames.iter().all(|frame| !frame.contains('/')),
            "{frames:#?}"
        );
    }
}
