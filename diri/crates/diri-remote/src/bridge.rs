//! Short-lived SSH stdio ↔ Holder UDS bridge.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

use diri_proto::frames::MAX_FRAME_BYTES;
use diri_proto::remote_pty::{RemoteCodec, RemoteMessage};

use crate::paths::StatePaths;

pub fn run<R: Read + Send + 'static, W: Write>(mut input: R, mut output: W) -> io::Result<()> {
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

    // The two directions do not end together, and only one of them decides
    // whether this process still has a job. When the holder closes the socket
    // — which it does to shed a client that has fallen too far behind — the
    // socket-to-stdout side ends, and the bridge must end with it so the
    // engine sees its stream close and reconnects.
    //
    // Waiting for both would mean waiting for stdin, and the engine keeps that
    // open for as long as it believes the session is live: the bridge would
    // linger holding stdout, the engine would block reading it forever, and a
    // session that had already exited would never be reported. So the
    // stdin-to-socket direction runs detached and ends with the process.
    std::thread::Builder::new()
        .name("diri-remote-attach-input".into())
        .spawn(move || {
            let _ = io::copy(&mut input, &mut upstream);
            let _ = upstream.shutdown(Shutdown::Write);
        })?;

    let mut bytes = [0_u8; 64 * 1024];
    loop {
        let count = downstream.read(&mut bytes)?;
        if count == 0 {
            return output.flush();
        }
        output.write_all(&bytes[..count])?;
        // A pipe-backed `Stdout` can otherwise retain a final sub-buffer frame
        // indefinitely while the SSH channel stays open. Commit every UDS
        // batch to preserve protocol latency.
        output.flush()?;
    }
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

#[cfg(test)]
mod end_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// A reader that never returns, standing in for the engine's end of the
    /// SSH channel: it holds its input open for as long as it believes the
    /// session is live.
    struct NeverEnds;

    impl Read for NeverEnds {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_secs(3600));
            Ok(0)
        }
    }

    /// The bridge ends when its output stream does, not when its input does.
    ///
    /// The holder closes the socket to shed a client that has fallen behind.
    /// If the bridge waited for stdin as well it would linger holding stdout,
    /// the engine would block reading it, and a session that had already
    /// exited would never be reported as such.
    #[test]
    fn a_closed_socket_ends_the_bridge_even_while_input_stays_open() {
        let (mut holder, client) = UnixStream::pair().expect("socket pair");
        let (finished, wait) = mpsc::channel();
        std::thread::spawn(move || {
            let mut downstream = client.try_clone().expect("clone");
            let mut upstream = client;
            std::thread::spawn(move || {
                let mut input = NeverEnds;
                let _ = io::copy(&mut input, &mut upstream);
            });
            let mut output = Vec::new();
            let mut bytes = [0_u8; 1024];
            loop {
                match downstream.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => output.extend_from_slice(&bytes[..count]),
                }
            }
            let _ = finished.send(output);
        });

        holder.write_all(b"payload").expect("write");
        holder.shutdown(Shutdown::Both).expect("shutdown");

        let output = wait
            .recv_timeout(Duration::from_secs(5))
            .expect("bridge should end with its socket, not wait on input");
        assert_eq!(output, b"payload");
    }
}
