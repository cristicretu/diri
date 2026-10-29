//! DIRI PATCH: test-only observation of OSC dispatches.
//!
//! Compiled only with the `observe` feature, which no production build
//! enables. A benchmark that claims every byte was parsed cannot prove it
//! from terminal state alone when the sequence it feeds (an OSC nothing
//! reads, such as OSC 6) leaves no state behind. This records, per thread,
//! how many OSCs reached the performer and how many parameter bytes they
//! carried, so a parser that skipped or truncated bodies reports different
//! totals than a reference parse of the same bytes.
//!
//! The order-sensitive checksum of every parameter byte costs a pass over
//! each body, so it is off until [`reset`] turns it on; the counts are
//! always kept (a few additions per dispatch).

use std::cell::Cell;

/// What the performer was handed since the last [`reset`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OscTotals {
    /// `osc_dispatch` calls.
    pub dispatches: u64,
    /// Parameters across all dispatches.
    pub params: u64,
    /// Parameter bytes across all dispatches.
    pub param_bytes: u64,
    /// Dispatches ended by BEL rather than ST.
    pub bell_terminated: u64,
    /// FNV-1a over every dispatch, parameter and byte in order; zero while
    /// checksums are off.
    pub checksum: u64,
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

std::thread_local! {
    static TOTALS: Cell<OscTotals> = const { Cell::new(OscTotals {
        dispatches: 0,
        params: 0,
        param_bytes: 0,
        bell_terminated: 0,
        checksum: 0,
    }) };
    static CHECKSUM: Cell<bool> = const { Cell::new(false) };
}

/// Zeroes this thread's totals; `checksum` turns the byte checksum on or
/// off.
pub fn reset(checksum: bool) {
    CHECKSUM.with(|on| on.set(checksum));
    TOTALS.with(|totals| {
        totals.set(OscTotals {
            checksum: if checksum { FNV_OFFSET } else { 0 },
            ..OscTotals::default()
        });
    });
}

/// This thread's totals since the last [`reset`].
#[must_use]
pub fn totals() -> OscTotals {
    TOTALS.with(Cell::get)
}

pub(crate) fn record(params: &[&[u8]], bell_terminated: bool) {
    let checksum = CHECKSUM.with(Cell::get);
    TOTALS.with(|cell| {
        let mut totals = cell.get();
        totals.dispatches += 1;
        totals.params += params.len() as u64;
        totals.bell_terminated += u64::from(bell_terminated);
        let mut hash = totals.checksum;
        let mut mix = |byte: u8| hash = (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
        if checksum {
            // Dispatch and parameter boundaries are part of the sum, so
            // moving a byte across one changes it.
            mix(0xFF);
            mix(u8::from(bell_terminated));
        }
        for param in params {
            totals.param_bytes += param.len() as u64;
            if checksum {
                for byte in (param.len() as u64).to_le_bytes() {
                    mix(byte);
                }
                for &byte in *param {
                    mix(byte);
                }
            }
        }
        totals.checksum = hash;
        cell.set(totals);
    });
}
