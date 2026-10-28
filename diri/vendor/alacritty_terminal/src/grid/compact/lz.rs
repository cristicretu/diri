//! Byte-oriented LZ77 for process-local history blocks.
//!
//! This is an LZ4-style block codec: no entropy coding, 64 KiB window, 4-byte
//! minimum matches. It trades some ratio against DEFLATE for an order of
//! magnitude less CPU, so sealing history keeps pace with the terminal parser.
//! Payloads never leave the process and are only decoded from this encoder.
//!
//! Layout: raw length, then sequences of
//! `token (literal len:4 | match len - 4:4), [len ext], literals, [offset:u16 le, [len ext]]`.
//! The final sequence has literals only.

const MIN_MATCH: usize = 4;
const HASH_BITS: u32 = 11;
/// How fast probing speeds up through bytes without matches (see
/// [`compress_into`]).
const SKIP_SHIFT: u32 = 3;
const WINDOW: usize = u16::MAX as usize;
/// Matches never start in the final bytes, so the literal tail always exists
/// and a match search never reads past the input.
const TAIL: usize = 8;

#[inline]
fn read_u32(bytes: &[u8], at: usize) -> u32 {
    // One bounds check and one load, where four indexed bytes take four.
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

#[inline]
fn hash(value: u32) -> usize {
    (value.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

fn put_length(out: &mut Vec<u8>, mut value: usize) {
    while value >= 255 {
        out.push(255);
        value -= 255;
    }
    out.push(value as u8);
}

fn put_varint(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn emit(out: &mut Vec<u8>, literals: &[u8], matched: Option<(usize, usize)>) {
    let literal_nibble = literals.len().min(15);
    let match_nibble = matched.map_or(0, |(_, len)| (len - MIN_MATCH).min(15));
    out.push((literal_nibble << 4 | match_nibble) as u8);
    if literals.len() >= 15 {
        put_length(out, literals.len() - 15);
    }
    out.extend_from_slice(literals);
    if let Some((offset, len)) = matched {
        out.extend_from_slice(&(offset as u16).to_le_bytes());
        if len - MIN_MATCH >= 15 {
            put_length(out, len - MIN_MATCH - 15);
        }
    }
}

/// Length of the common run at `earlier` and `later`, never reaching the
/// literal tail. Compares eight bytes at a time.
#[inline]
fn match_length(input: &[u8], earlier: usize, later: usize) -> usize {
    let end = input.len() - TAIL;
    // `earlier < later`, so both runs fit in `later..end`'s length (empty
    // when `later` is already in the tail); slicing once lets the loops
    // below index without further checks.
    let available = end.saturating_sub(later);
    let (a, b) = (
        &input[earlier..earlier + available],
        &input[later.min(end)..end],
    );
    let mut len = 0;
    for (a, b) in a.chunks_exact(8).zip(b.chunks_exact(8)) {
        let a = u64::from_le_bytes(a.try_into().expect("eight bytes"));
        let b = u64::from_le_bytes(b.try_into().expect("eight bytes"));
        let difference = a ^ b;
        if difference != 0 {
            return len + (difference.trailing_zeros() / 8) as usize;
        }
        len += 8;
    }
    len + a[len..]
        .iter()
        .zip(&b[len..])
        .take_while(|(a, b)| a == b)
        .count()
}

#[cfg(test)]
pub(super) fn compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    compress_into(input, &mut out);
    out
}

/// [`compress`] into `out`, replacing what it held and keeping its capacity.
///
/// Each miss lengthens the step to the next probe (one more byte every
/// `1 << SKIP_SHIFT` misses), so text without repeats costs a fraction of a
/// probe per byte. A match found after a skip is first extended backwards,
/// which recovers the bytes the skip stepped over.
pub(super) fn compress_into(input: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(input.len() / 2 + 16);
    put_varint(out, input.len());
    let limit = input.len().saturating_sub(TAIL);
    if limit == 0 {
        emit(out, input, None);
        return;
    }
    // Zero is a valid candidate: every candidate's bytes are compared before
    // a match is taken, so an empty slot costs one comparison.
    let mut table = [0u32; 1 << HASH_BITS];
    table[hash(read_u32(input, 0))] = 0;
    let mut anchor = 0;
    let mut position = 1;
    let mut misses = 0usize;
    while position < limit {
        let value = read_u32(input, position);
        let slot = &mut table[hash(value)];
        let mut candidate = *slot as usize;
        *slot = position as u32;
        if position - candidate > WINDOW || read_u32(input, candidate) != value {
            misses += 1;
            position += 1 + (misses >> SKIP_SHIFT);
            continue;
        }
        misses = 0;
        let forward = MIN_MATCH + match_length(input, candidate + MIN_MATCH, position + MIN_MATCH);
        let mut start = position;
        while start > anchor && candidate > 0 && input[start - 1] == input[candidate - 1] {
            start -= 1;
            candidate -= 1;
        }
        let len = forward + (position - start);
        emit(out, &input[anchor..start], Some((start - candidate, len)));
        position = start + len;
        // Index one position inside the match so repeated structure keeps
        // finding recent candidates without hashing every byte.
        if position - 2 < limit {
            table[hash(read_u32(input, position - 2))] = (position - 2) as u32;
        }
        anchor = position;
    }
    emit(out, &input[anchor..], None);
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    #[inline]
    fn byte(&mut self) -> u8 {
        let byte = self.bytes[self.at];
        self.at += 1;
        byte
    }

    #[inline]
    fn length(&mut self, nibble: usize) -> usize {
        let mut len = nibble;
        if nibble == 15 {
            loop {
                let byte = self.byte();
                len += usize::from(byte);
                if byte != 255 {
                    break;
                }
            }
        }
        len
    }
}

pub(super) fn decompress(bytes: &[u8]) -> Vec<u8> {
    let mut cursor = Cursor { bytes, at: 0 };
    let mut raw_len = 0usize;
    let mut shift = 0;
    loop {
        let byte = cursor.byte();
        raw_len |= usize::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            break;
        }
        shift += 7;
    }
    let mut out = Vec::with_capacity(raw_len);
    loop {
        let token = usize::from(cursor.byte());
        let literals = cursor.length(token >> 4);
        out.extend_from_slice(&bytes[cursor.at..cursor.at + literals]);
        cursor.at += literals;
        if cursor.at == bytes.len() {
            break;
        }
        let offset = usize::from(u16::from_le_bytes([cursor.byte(), cursor.byte()]));
        let len = cursor.length(token & 15) + MIN_MATCH;
        assert!(
            offset != 0 && offset <= out.len(),
            "internally encoded match"
        );
        let start = out.len() - offset;
        if offset >= len {
            out.extend_from_within(start..start + len);
        } else {
            for index in start..start + len {
                let byte = out[index];
                out.push(byte);
            }
        }
    }
    assert_eq!(out.len(), raw_len, "internally encoded length");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The common run, a byte at a time, never reaching the literal tail.
    fn bytewise_match_length(input: &[u8], earlier: usize, later: usize) -> usize {
        let end = input.len() - TAIL;
        let mut len = 0;
        while later + len < end && input[earlier + len] == input[later + len] {
            len += 1;
        }
        len
    }

    #[test]
    fn match_length_agrees_with_a_bytewise_scan() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let length = TAIL + 1 + next() as usize % 300;
            // Two symbols make long common runs likely.
            let input: Vec<u8> = (0..length).map(|_| b"ab"[next() as usize % 2]).collect();
            let later = 1 + next() as usize % (length - 1);
            let earlier = next() as usize % later;
            assert_eq!(
                match_length(&input, earlier, later),
                bytewise_match_length(&input, earlier, later),
                "earlier {earlier} later {later} of {length}"
            );
        }
    }
}
