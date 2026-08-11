//! Short-lived SSH stdio ↔ Holder UDS bridge.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

use diri_proto::frames::MAX_FRAME_BYTES;
use diri_proto::remote_pty::{RemoteCodec, RemoteMessage};

use crate::paths::StatePaths;

pub fn run<R: Read, W: Write + Send>(mut input: R, mut output: W) -> io::Result<()> {
    let first = read_frame(&mut input)?;
    let mut codec = RemoteCodec::new();
    let messages = codec.feed(&first).map_err(io::Error::other)?;
    let [RemoteMessage::Hello(hello)] = messages.as_slice() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "attach stream must begin with exactly one Hello",
        ));
    };
    let paths = StatePaths::resolve()?.session(&hello.session_id)?;
    let mut upstream = UnixStream::connect(&paths.socket)?;
    upstream.write_all(&first)?;
    upstream.flush()?;
    let mut downstream = upstream.try_clone()?;

    std::thread::scope(|scope| -> io::Result<()> {
        let receive = scope.spawn(move || -> io::Result<()> {
            let mut bytes = [0_u8; 64 * 1024];
            loop {
                let count = downstream.read(&mut bytes)?;
                if count == 0 {
                    return output.flush();
                }
                output.write_all(&bytes[..count])?;
                // A pipe-backed `Stdout` can otherwise retain a final
                // sub-buffer frame indefinitely while the SSH channel stays
                // open. Commit every UDS batch to preserve protocol latency.
                output.flush()?;
            }
        });
        let sent = io::copy(&mut input, &mut upstream);
        let _ = upstream.shutdown(Shutdown::Write);
        let received = receive
            .join()
            .map_err(|_| io::Error::other("attach receive thread panicked"))?;
        sent.and(received).map(|_| ())
    })
}

fn read_frame(reader: &mut dyn Read) -> io::Result<Vec<u8>> {
    let mut header = [0_u8; 5];
    reader.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header[1..5].try_into().expect("header")) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "first attach frame exceeds the protocol limit",
        ));
    }
    let mut frame = Vec::with_capacity(5 + length);
    frame.extend_from_slice(&header);
    frame.resize(5 + length, 0);
    reader.read_exact(&mut frame[5..])?;
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_first_frame_is_rejected_before_allocation() {
        let mut bytes = vec![32];
        bytes.extend_from_slice(&(MAX_FRAME_BYTES as u32 + 1).to_be_bytes());
        let error = read_frame(&mut bytes.as_slice()).expect_err("oversized");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
