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
const HASH_BITS: u32 = 12;
const WINDOW: usize = u16::MAX as usize;
/// Matches never start in the final bytes, so the literal tail always exists
/// and a match search never reads past the input.
const TAIL: usize = 8;

#[inline]
fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
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
    let mut len = 0;
    while later + len + 8 <= end {
        let a = u64::from_le_bytes(input[earlier + len..earlier + len + 8].try_into().unwrap());
        let b = u64::from_le_bytes(input[later + len..later + len + 8].try_into().unwrap());
        let difference = a ^ b;
        if difference != 0 {
            return len + (difference.trailing_zeros() / 8) as usize;
        }
        len += 8;
    }
    while later + len < end && input[earlier + len] == input[later + len] {
        len += 1;
    }
    len
}

#[cfg(test)]
pub(super) fn compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 2 + 16);
    compress_into(input, &mut out);
    out
}

/// Appends the compressed form of `input` to `out`.
pub(super) fn compress_into(input: &[u8], out: &mut Vec<u8>) {
    put_varint(out, input.len());
    let mut table = [u32::MAX; 1 << HASH_BITS];
    let mut anchor = 0;
    let mut position = 0;
    let limit = input.len().saturating_sub(TAIL);
    let mut misses = 0usize;
    while position < limit {
        let value = read_u32(input, position);
        let slot = &mut table[hash(value)];
        let candidate = *slot as usize;
        *slot = position as u32;
        if candidate >= position
            || position - candidate > WINDOW
            || read_u32(input, candidate) != value
        {
            misses += 1;
            position += 1 + (misses >> 4);
            continue;
        }
        misses = 0;
        let len = MIN_MATCH + match_length(input, candidate + MIN_MATCH, position + MIN_MATCH);
        emit(
            out,
            &input[anchor..position],
            Some((position - candidate, len)),
        );
        // Index one position inside the match so repeated structure keeps
        // finding recent candidates without hashing every byte.
        if position + len - 2 < limit {
            let inner = position + len - 2;
            table[hash(read_u32(input, inner))] = inner as u32;
        }
        position += len;
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
