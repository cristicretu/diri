//! Byte-offset tailing of an append-only JSONL file.
//!
//! A cursor is valid only at a record boundary: offset zero or one byte past
//! a newline. [`read_window`] consumes whole lines only, so the cursor it
//! returns is again a boundary, and reading a file in any number of windows
//! yields exactly the lines one read would (`DualLens.read_append_split`).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

/// One complete JSONL record and the offset of its first byte.
#[derive(Debug, PartialEq, Eq)]
pub struct Line {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Window {
    pub lines: Vec<Line>,
    /// One past the last newline consumed; a boundary.
    pub next: u64,
    /// Complete records remain after `next`.
    pub more: bool,
    /// A record longer than the window was skipped whole; its offset.
    pub skipped: Option<u64>,
}

/// Whether `offset` is a record boundary of `file` (whose length is `len`).
pub fn cursor_is_boundary(file: &mut File, offset: u64, len: u64) -> io::Result<bool> {
    if offset == 0 {
        return Ok(true);
    }
    if offset > len {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(offset - 1))?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    Ok(byte[0] == b'\n')
}

/// Reads complete lines from `offset` (a boundary) up to `len`, at most
/// `max_bytes` of them. A trailing partial line is left for the next read.
///
/// A single record longer than `max_bytes` (a pasted multi-megabyte image,
/// say) cannot fit any window; it is skipped whole and reported in
/// `skipped`, so the reader never stalls behind it.
pub fn read_window(file: &mut File, offset: u64, len: u64, max_bytes: u64) -> io::Result<Window> {
    let available = len.saturating_sub(offset);
    if available == 0 {
        return Ok(Window {
            lines: Vec::new(),
            next: offset,
            more: false,
            skipped: None,
        });
    }
    let want = available.min(max_bytes.max(1));
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = Vec::with_capacity(want as usize);
    file.by_ref().take(want).read_to_end(&mut buffer)?;
    let Some(last_newline) = buffer.iter().rposition(|&b| b == b'\n') else {
        if want < available {
            // The record starting here is longer than the window.
            return skip_oversized(file, offset, len);
        }
        return Ok(Window {
            lines: Vec::new(),
            next: offset,
            more: false,
            skipped: None,
        });
    };
    let consumed = last_newline + 1;
    buffer.truncate(consumed);
    let lines = split_lines(offset, &buffer);
    let next = offset + consumed as u64;
    let more = next < len && want < available;
    Ok(Window {
        lines,
        next,
        more,
        skipped: None,
    })
}

/// Splits `buffer` (which ends with a newline) into records keyed by their
/// absolute start offset. Empty lines carry no record and are dropped, but
/// still advance the offset.
pub fn split_lines(base: u64, buffer: &[u8]) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (index, &byte) in buffer.iter().enumerate() {
        if byte == b'\n' {
            let mut end = index;
            if end > start && buffer[end - 1] == b'\r' {
                end -= 1;
            }
            if end > start {
                lines.push(Line {
                    offset: base + start as u64,
                    bytes: buffer[start..end].to_vec(),
                });
            }
            start = index + 1;
        }
    }
    lines
}

fn skip_oversized(file: &mut File, offset: u64, len: u64) -> io::Result<Window> {
    const SCAN: usize = 64 * 1024;
    let mut position = offset;
    let mut chunk = vec![0u8; SCAN];
    file.seek(SeekFrom::Start(position))?;
    while position < len {
        let wanted = ((len - position) as usize).min(SCAN);
        let read = file.read(&mut chunk[..wanted])?;
        if read == 0 {
            break;
        }
        if let Some(newline) = chunk[..read].iter().position(|&b| b == b'\n') {
            let next = position + newline as u64 + 1;
            return Ok(Window {
                lines: Vec::new(),
                next,
                more: next < len,
                skipped: Some(offset),
            });
        }
        position += read as u64;
    }
    // Still being written: wait for its newline.
    Ok(Window {
        lines: Vec::new(),
        next: offset,
        more: false,
        skipped: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn file_with(bytes: &[u8]) -> (tempfile::NamedTempFile, File) {
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        temp.write_all(bytes).unwrap();
        temp.flush().unwrap();
        let file = File::open(temp.path()).unwrap();
        (temp, file)
    }

    fn drain(file: &mut File, len: u64, window: u64) -> (Vec<(u64, Vec<u8>)>, u64) {
        let mut offset = 0;
        let mut out = Vec::new();
        loop {
            let read = read_window(file, offset, len, window).unwrap();
            out.extend(read.lines.into_iter().map(|l| (l.offset, l.bytes)));
            let advanced = read.next != offset;
            offset = read.next;
            if !read.more && !advanced {
                break;
            }
            if !read.more {
                // One more pass to confirm nothing remains.
                continue;
            }
        }
        (out, offset)
    }

    /// Lean `read_append_split`: any windowing yields the single-read lines.
    #[test]
    fn tail_chunking_matches_one_read() {
        let data = b"{\"a\":1}\n\n{\"b\":22}\r\n{\"c\":333}\n{\"partial\"";
        let (_temp, mut file) = file_with(data);
        let len = data.len() as u64;
        let (whole, end) = drain(&mut file, len, 1 << 20);
        assert_eq!(
            whole.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
            vec![0, 9, 19]
        );
        assert_eq!(end, 29);
        for window in 10..40 {
            let (chunked, chunked_end) = drain(&mut file, len, window);
            assert_eq!(chunked, whole, "window {window}");
            assert_eq!(chunked_end, end);
        }
    }

    #[test]
    fn growth_between_reads_neither_duplicates_nor_drops() {
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        let mut seen = Vec::new();
        let mut offset = 0;
        let pieces: [&[u8]; 4] = [b"{\"x\":1}\n{\"y\"", b":2}\n", b"{\"z\":3}", b"\n"];
        for piece in pieces {
            temp.write_all(piece).unwrap();
            temp.flush().unwrap();
            let mut file = File::open(temp.path()).unwrap();
            let len = file.metadata().unwrap().len();
            assert!(cursor_is_boundary(&mut file, offset, len).unwrap());
            let read = read_window(&mut file, offset, len, 1 << 20).unwrap();
            seen.extend(read.lines.into_iter().map(|l| l.offset));
            offset = read.next;
        }
        assert_eq!(seen, vec![0, 8, 16]);
    }

    #[test]
    fn mid_record_cursor_is_not_a_boundary() {
        let (_temp, mut file) = file_with(b"{\"a\":1}\n{\"b\":2}\n");
        assert!(cursor_is_boundary(&mut file, 0, 16).unwrap());
        assert!(cursor_is_boundary(&mut file, 8, 16).unwrap());
        assert!(!cursor_is_boundary(&mut file, 3, 16).unwrap());
        assert!(!cursor_is_boundary(&mut file, 17, 16).unwrap());
    }

    #[test]
    fn an_oversized_record_is_skipped_whole() {
        let mut data = b"{\"a\":1}\n".to_vec();
        data.extend(std::iter::repeat_n(b'x', 300));
        data.extend(b"\n{\"b\":2}\n");
        let (_temp, mut file) = file_with(&data);
        let len = data.len() as u64;
        let first = read_window(&mut file, 0, len, 64).unwrap();
        assert_eq!(first.lines.len(), 1);
        let second = read_window(&mut file, first.next, len, 64).unwrap();
        assert_eq!(second.skipped, Some(8));
        assert!(second.lines.is_empty());
        let third = read_window(&mut file, second.next, len, 64).unwrap();
        assert_eq!(third.lines.len(), 1);
        assert_eq!(third.lines[0].bytes, b"{\"b\":2}");
        assert!(!third.more);
    }
}
