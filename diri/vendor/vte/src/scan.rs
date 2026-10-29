//! Diri patch: vector scans over string bodies.
//!
//! An OSC, DCS or APC body is mostly bytes the parser treats alike. These
//! scans find the first byte that is not, 64 bytes per step with NEON on
//! aarch64 and SSE2 on x86_64 (both baseline features of those targets), and
//! byte by byte elsewhere. Diri's own string scanners use the same routine.

/// The bytes that end a run.
#[derive(Clone, Copy, Debug)]
pub struct Stops {
    /// Every byte below this one stops (0: none).
    pub below: u8,
    /// Every byte above this one stops (0xFF: none).
    pub above: u8,
    /// These bytes stop too; repeat one to list fewer than four.
    pub bytes: [u8; 4],
}

impl Stops {
    #[inline(always)]
    fn stops(&self, byte: u8) -> bool {
        byte < self.below || byte > self.above || self.bytes.contains(&byte)
    }
}

/// The length of the leading run of `bytes` with no byte in `stops`.
#[inline]
pub fn run(bytes: &[u8], stops: Stops) -> usize {
    let index = vectors(bytes, stops);
    if index < bytes.len() && bytes.len() - index >= 16 {
        // `vectors` stopped at the 16 bytes holding the first stop.
        return index;
    }
    index + bytes[index..].iter().position(|&byte| stops.stops(byte)).unwrap_or(bytes.len() - index)
}

/// The offset of the first stop when it lies in a whole 16-byte vector, or
/// else of the tail shorter than a vector that is left to scan.
#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
fn vectors(bytes: &[u8], stops: Stops) -> usize {
    use core::arch::aarch64::*;

    // SAFETY: NEON is enabled for this target (cfg above), and every load
    // reads 16 bytes that lie inside `bytes`.
    unsafe {
        let below = vdupq_n_u8(stops.below);
        let above = vdupq_n_u8(stops.above);
        let [b0, b1, b2, b3] = stops.bytes.map(|byte| vdupq_n_u8(byte));
        let hit = |v: uint8x16_t| {
            vorrq_u8(
                vorrq_u8(vcltq_u8(v, below), vcgtq_u8(v, above)),
                vorrq_u8(
                    vorrq_u8(vceqq_u8(v, b0), vceqq_u8(v, b1)),
                    vorrq_u8(vceqq_u8(v, b2), vceqq_u8(v, b3)),
                ),
            )
        };
        // Four bits per byte of a compare mask; the first set nibble is the
        // first stop.
        let first = |mask: uint8x16_t| -> Option<usize> {
            let nibbles =
                vget_lane_u64(vreinterpret_u64_u8(vshrn_n_u16(vreinterpretq_u16_u8(mask), 4)), 0);
            (nibbles != 0).then(|| nibbles.trailing_zeros() as usize / 4)
        };
        let base = bytes.as_ptr();
        let mut index = 0;
        while index + 64 <= bytes.len() {
            let at = base.add(index);
            let any = vorrq_u8(
                vorrq_u8(hit(vld1q_u8(at)), hit(vld1q_u8(at.add(16)))),
                vorrq_u8(hit(vld1q_u8(at.add(32))), hit(vld1q_u8(at.add(48)))),
            );
            if vmaxvq_u8(any) != 0 {
                break;
            }
            index += 64;
        }
        while index + 16 <= bytes.len() {
            if let Some(at) = first(hit(vld1q_u8(base.add(index)))) {
                return index + at;
            }
            index += 16;
        }
        index
    }
}

/// The offset of the first stop when it lies in a whole 16-byte vector, or
/// else of the tail shorter than a vector that is left to scan.
#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
#[inline]
fn vectors(bytes: &[u8], stops: Stops) -> usize {
    use core::arch::x86_64::*;

    // SAFETY: SSE2 is enabled for this target (cfg above), and every load
    // reads 16 unaligned bytes that lie inside `bytes`.
    unsafe {
        // Unsigned `v < below` is `min(v, below - 1) == v`, and `v > above`
        // is `max(v, above + 1) == v`; an unused bound is masked off.
        let low = _mm_set1_epi8(stops.below.wrapping_sub(1) as i8);
        let low_on = _mm_set1_epi8(if stops.below == 0 { 0 } else { -1 });
        let high = _mm_set1_epi8(stops.above.wrapping_add(1) as i8);
        let high_on = _mm_set1_epi8(if stops.above == 0xFF { 0 } else { -1 });
        let [b0, b1, b2, b3] = stops.bytes.map(|byte| _mm_set1_epi8(byte as i8));
        let hit = |v: __m128i| {
            let low = _mm_and_si128(_mm_cmpeq_epi8(_mm_min_epu8(v, low), v), low_on);
            let high = _mm_and_si128(_mm_cmpeq_epi8(_mm_max_epu8(v, high), v), high_on);
            _mm_or_si128(
                _mm_or_si128(low, high),
                _mm_or_si128(
                    _mm_or_si128(_mm_cmpeq_epi8(v, b0), _mm_cmpeq_epi8(v, b1)),
                    _mm_or_si128(_mm_cmpeq_epi8(v, b2), _mm_cmpeq_epi8(v, b3)),
                ),
            )
        };
        let base = bytes.as_ptr();
        let mut index = 0;
        while index + 64 <= bytes.len() {
            let at = base.add(index) as *const __m128i;
            let any = _mm_or_si128(
                _mm_or_si128(hit(_mm_loadu_si128(at)), hit(_mm_loadu_si128(at.add(1)))),
                _mm_or_si128(hit(_mm_loadu_si128(at.add(2))), hit(_mm_loadu_si128(at.add(3)))),
            );
            if _mm_movemask_epi8(any) != 0 {
                break;
            }
            index += 64;
        }
        while index + 16 <= bytes.len() {
            let mask = _mm_movemask_epi8(hit(_mm_loadu_si128(base.add(index) as *const __m128i)));
            if mask != 0 {
                return index + mask.trailing_zeros() as usize;
            }
            index += 16;
        }
        index
    }
}

#[cfg(not(any(
    all(target_arch = "aarch64", target_feature = "neon"),
    all(target_arch = "x86_64", target_feature = "sse2")
)))]
#[inline]
fn vectors(bytes: &[u8], stops: Stops) -> usize {
    bytes.iter().position(|&byte| stops.stops(byte)).unwrap_or(bytes.len())
}

/// The first index where `bytes[index]` is `first` and the byte after it is
/// one of `second`: the start of a two-byte opener such as `ESC ]`.
#[inline]
pub fn find_pair<const N: usize>(bytes: &[u8], first: u8, second: [u8; N]) -> Option<usize> {
    let index = pair_vectors(bytes, first, second);
    let tail = bytes.get(index..)?;
    tail.windows(2)
        .position(|pair| pair[0] == first && second.contains(&pair[1]))
        .map(|at| index + at)
}

/// The offset of the first pair when it starts in a whole 16-byte vector
/// (with its successor byte loaded too), or else where the scalar tail
/// begins.
#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
fn pair_vectors<const N: usize>(bytes: &[u8], first: u8, second: [u8; N]) -> usize {
    use core::arch::aarch64::*;

    // SAFETY: NEON is enabled for this target (cfg above), and both loads
    // read 16 bytes inside `bytes` (`index + 17 <= len`).
    unsafe {
        let lead = vdupq_n_u8(first);
        let seconds = second.map(|byte| vdupq_n_u8(byte));
        let base = bytes.as_ptr();
        let mut index = 0;
        while index + 17 <= bytes.len() {
            let here = vld1q_u8(base.add(index));
            let next = vld1q_u8(base.add(index + 1));
            let follows = seconds
                .iter()
                .fold(vdupq_n_u8(0), |any, &byte| vorrq_u8(any, vceqq_u8(next, byte)));
            let mask = vandq_u8(vceqq_u8(here, lead), follows);
            let nibbles =
                vget_lane_u64(vreinterpret_u64_u8(vshrn_n_u16(vreinterpretq_u16_u8(mask), 4)), 0);
            if nibbles != 0 {
                return index + nibbles.trailing_zeros() as usize / 4;
            }
            index += 16;
        }
        index
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
#[inline]
fn pair_vectors<const N: usize>(bytes: &[u8], first: u8, second: [u8; N]) -> usize {
    use core::arch::x86_64::*;

    // SAFETY: SSE2 is enabled for this target (cfg above), and both loads
    // read 16 unaligned bytes inside `bytes` (`index + 17 <= len`).
    unsafe {
        let lead = _mm_set1_epi8(first as i8);
        let seconds = second.map(|byte| _mm_set1_epi8(byte as i8));
        let base = bytes.as_ptr();
        let mut index = 0;
        while index + 17 <= bytes.len() {
            let here = _mm_loadu_si128(base.add(index) as *const __m128i);
            let next = _mm_loadu_si128(base.add(index + 1) as *const __m128i);
            let follows = seconds.iter().fold(_mm_setzero_si128(), |any, &byte| {
                _mm_or_si128(any, _mm_cmpeq_epi8(next, byte))
            });
            let mask = _mm_movemask_epi8(_mm_and_si128(_mm_cmpeq_epi8(here, lead), follows));
            if mask != 0 {
                return index + mask.trailing_zeros() as usize;
            }
            index += 16;
        }
        index
    }
}

#[cfg(not(any(
    all(target_arch = "aarch64", target_feature = "neon"),
    all(target_arch = "x86_64", target_feature = "sse2")
)))]
#[inline]
fn pair_vectors<const N: usize>(_bytes: &[u8], _first: u8, _second: [u8; N]) -> usize {
    0
}

/// Where every C0 byte (below 0x20) of one read lies: the only bytes that
/// can end or abort a string body (BEL, CAN, SUB, ESC) or that an OSC drops.
///
/// Built once per read by [`c0_positions`] and shared by every layer that
/// looks for string terminators, so the bytes between them are scanned once.
/// Positions are relative to the start of the read; `base` places a slice of
/// that read within it.
#[derive(Clone, Copy, Debug)]
pub struct C0Index<'a> {
    positions: &'a [u32],
    base: usize,
}

impl<'a> C0Index<'a> {
    /// An index of a read, from [`c0_positions`].
    pub fn new(positions: &'a [u32]) -> Self {
        Self { positions, base: 0 }
    }

    /// The same index for the slice of the read that starts `offset` bytes
    /// into the slice this one describes.
    #[inline]
    #[must_use]
    pub fn offset(self, offset: usize) -> Self {
        Self { positions: self.positions, base: self.base + offset }
    }

    /// The first C0 byte at or after `from` in `bytes` (the slice this index
    /// describes) whose value is in `wanted`, or `bytes.len()`.
    #[inline]
    pub fn next(&self, bytes: &[u8], from: usize, wanted: &[u8]) -> usize {
        let start = (self.base + from) as u64;
        let end = (self.base + bytes.len()) as u64;
        let first = self.positions.partition_point(|&at| u64::from(at) < start);
        for &at in &self.positions[first..] {
            if u64::from(at) >= end {
                break;
            }
            let at = at as usize - self.base;
            if wanted.is_empty() || wanted.contains(&bytes[at]) {
                return at;
            }
        }
        bytes.len()
    }
}

/// Every C0 terminator of a string body.
pub const STRING_ENDS: [u8; 4] = [0x07, 0x18, 0x1A, 0x1B];

/// Fills `out` with the offset of every byte below 0x20 in `bytes`, in
/// order. False, with `out` unusable, when there are more than `cap` of them
/// (output dense in controls gains nothing from an index).
#[cfg(feature = "std")]
pub fn c0_positions(bytes: &[u8], out: &mut std::vec::Vec<u32>, cap: usize) -> bool {
    out.clear();
    if bytes.len() > u32::MAX as usize {
        return false;
    }
    let stops = Stops { below: 0x20, above: 0xFF, bytes: [0; 4] };
    let mut at = 0;
    loop {
        at += run(&bytes[at..], stops);
        if at == bytes.len() {
            return true;
        }
        if out.len() == cap {
            return false;
        }
        out.push(at as u32);
        at += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vector_scan_stops_where_a_byte_scan_does() {
        let sets = [
            Stops { below: 0x20, above: 0xFF, bytes: [b';'; 4] },
            Stops { below: 0, above: 0x7E, bytes: [0x18, 0x1A, 0x1B, 0x1B] },
            Stops { below: 0, above: 0xFF, bytes: [0x07, 0x18, 0x1A, 0x1B] },
            Stops { below: 0x01, above: 0xFE, bytes: [0x80; 4] },
        ];
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for stops in sets {
            for stop in 0..=255u8 {
                for len in [0usize, 1, 15, 16, 63, 64, 65, 127, 128, 200] {
                    for at in [0usize, 1, 17, 63, 64, 100, 199] {
                        let mut bytes: std::vec::Vec<u8> = (0..len)
                            .map(|_| {
                                state ^= state << 13;
                                state ^= state >> 7;
                                state ^= state << 17;
                                loop {
                                    let byte = (state >> 24) as u8;
                                    if !stops.stops(byte) {
                                        break byte;
                                    }
                                    state = state.rotate_left(9) ^ 0x9e37;
                                }
                            })
                            .collect();
                        if at < len {
                            bytes[at] = stop;
                        }
                        let expected =
                            bytes.iter().position(|&byte| stops.stops(byte)).unwrap_or(len);
                        assert_eq!(run(&bytes, stops), expected, "{stops:?} {stop} {len} {at}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_vector_pair_search_finds_what_a_byte_search_does() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for case in 0..20_000 {
            let len = (case % 90) as usize;
            let bytes: std::vec::Vec<u8> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    // Mostly openers and their pieces, so pairs straddle every
                    // vector boundary.
                    [0x1b, b']', b'_', b'a', 0x1b, b'x'][(state >> 20) as usize % 6]
                })
                .collect();
            let expected = bytes
                .windows(2)
                .position(|pair| pair[0] == 0x1b && (pair[1] == b']' || pair[1] == b'_'));
            assert_eq!(find_pair(&bytes, 0x1b, [b']', b'_']), expected, "{bytes:?}");
        }
    }
}
