//! One receive-only local connection for a bounded changing set of previews.
use crate::{AttachmentError, TerminalChunk};
use diri_proto::SessionId;
use diri_proto::frames::FrameType;
use diri_proto::preview_set::*;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

#[derive(Clone, Copy, Debug, Default)]
pub struct PreviewSetOptions {
    pub terminal_graphics: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewSetUpdate {
    Chunk(TerminalChunk),
    Unavailable(PreviewUnavailable),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewSetEvent {
    pub member: PreviewMember,
    pub update: PreviewSetUpdate,
}

/// Desired membership is a latest-value watch, not an unbounded command queue.
/// Events remain ordered and bounded; no terminal patch is silently discarded.
/// A generation change always requires a new full grid before any patch.
pub struct PreviewSet {
    desired: watch::Sender<PreviewSetMembership>,
    current: PreviewSetMembership,
    next_generation: u64,
    events: mpsc::Receiver<PreviewSetEvent>,
    task: Option<JoinHandle<()>>,
}

impl PreviewSet {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, AttachmentError> {
        Self::connect_with_options(path, PreviewSetOptions::default()).await
    }

    pub async fn connect_with_options(
        path: impl AsRef<Path>,
        options: PreviewSetOptions,
    ) -> Result<Self, AttachmentError> {
        Self::adopt(UnixStream::connect(path).await?, options).await
    }

    async fn adopt(
        mut stream: UnixStream,
        options: PreviewSetOptions,
    ) -> Result<Self, AttachmentError> {
        let mut request = serde_json::to_vec(&PreviewSetRequest {
            preview_set: true,
            version: PREVIEW_SET_VERSION,
            terminal_graphics: options.terminal_graphics,
        })?;
        request.push(b'\n');
        tokio::time::timeout(Duration::from_secs(2), async {
            stream.write_all(&request).await?;
            let mut line = Vec::new();
            loop {
                let byte = stream.read_u8().await?;
                if byte == b'\n' {
                    break;
                }
                if line.len() >= MAX_PREVIEW_SET_HEADER_BYTES {
                    return Err(invalid("oversized preview set acknowledgement"));
                }
                line.push(byte);
            }
            let ready: PreviewSetReady = serde_json::from_slice(&line).map_err(invalid)?;
            if ready.version != PREVIEW_SET_VERSION {
                return Err(invalid("preview set version mismatch"));
            }
            Ok(())
        })
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "preview set handshake timed out")
        })??;
        let current = PreviewSetMembership::default();
        let (desired, desired_rx) = watch::channel(current.clone());
        let (event_tx, events) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            let (read, write) = stream.into_split();
            // Membership changes must remain writable while a slow UI applies
            // a frame; writes must not suspend socket reads during admission.
            tokio::select! {
                _ = write_memberships(write, desired_rx.clone()) => {}
                _ = read_previews(read, desired_rx, event_tx) => {}
            }
        });
        Ok(Self {
            desired,
            current,
            next_generation: 1,
            events,
            task: Some(task),
        })
    }

    /// Returns the accepted per-session generations. Callers must compare an
    /// event's member against these identities before applying queued UI work.
    /// Retained sessions keep their generation; remove/re-add receives a new
    /// one even when the intermediate membership is coalesced before writing.
    pub fn set_sessions(
        &mut self,
        sessions: Vec<SessionId>,
    ) -> Result<PreviewSetMembership, AttachmentError> {
        if self.task.as_ref().is_none_or(JoinHandle::is_finished) {
            return Err(
                io::Error::new(io::ErrorKind::BrokenPipe, "preview set disconnected").into(),
            );
        }
        let (membership, next) = membership_for(&self.current, self.next_generation, sessions)?;
        if membership != self.current {
            self.desired.send(membership.clone()).map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "preview set disconnected")
            })?;
            self.current = membership;
            self.next_generation = next;
        }
        Ok(self.current.clone())
    }

    /// Filters stale generations still queued inside the client. UI dispatch
    /// may queue an already delivered event, so its member remains explicit.
    pub async fn recv(&mut self) -> Option<PreviewSetEvent> {
        while let Some(event) = self.events.recv().await {
            if self.current.members.contains(&event.member) {
                return Some(event);
            }
        }
        None
    }

    pub async fn close(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        self.events.close();
    }
}

impl Drop for PreviewSet {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn membership_for(
    current: &PreviewSetMembership,
    mut next: u64,
    mut sessions: Vec<SessionId>,
) -> io::Result<(PreviewSetMembership, u64)> {
    if sessions.len() > MAX_PREVIEW_SET_MEMBERS {
        return Err(invalid("too many preview subscriptions"));
    }
    sessions.sort_by(|a, b| a.0.cmp(&b.0));
    let previous: HashMap<_, _> = current
        .members
        .iter()
        .map(|member| (&member.session_id, member.generation))
        .collect();
    let mut members = Vec::with_capacity(sessions.len());
    for session_id in sessions {
        let generation = if let Some(generation) = previous.get(&session_id) {
            *generation
        } else {
            let value = next;
            next = next
                .checked_add(1)
                .ok_or_else(|| invalid("preview generation exhausted"))?;
            value
        };
        members.push(PreviewMember {
            session_id,
            generation,
        });
    }
    let membership = PreviewSetMembership { members };
    membership.validate()?;
    Ok((membership, next))
}

async fn write_memberships(
    mut write: tokio::net::unix::OwnedWriteHalf,
    mut desired: watch::Receiver<PreviewSetMembership>,
) -> io::Result<()> {
    while desired.changed().await.is_ok() {
        let membership = desired.borrow_and_update().clone();
        let mut bytes = serde_json::to_vec(&membership).map_err(invalid)?;
        if bytes.len() > MAX_PREVIEW_SET_REQUEST_BYTES {
            return Err(invalid("oversized preview membership"));
        }
        bytes.push(b'\n');
        write.write_all(&bytes).await?;
    }
    Ok(())
}

async fn read_previews(
    mut read: tokio::net::unix::OwnedReadHalf,
    desired: watch::Receiver<PreviewSetMembership>,
    events: mpsc::Sender<PreviewSetEvent>,
) -> io::Result<()> {
    let mut decoder = PreviewSetDecoder::default();
    let mut bytes = [0; 64 * 1024];
    let mut seeded = HashSet::new();
    loop {
        let count = read.read(&mut bytes).await?;
        if count == 0 {
            return if decoder.has_partial_packet() {
                Err(invalid("truncated preview packet"))
            } else {
                Ok(())
            };
        }
        for packet in decoder.feed(&bytes[..count])? {
            let membership = desired.borrow().clone();
            seeded.retain(|member| membership.members.contains(member));
            if !membership.members.contains(packet.member()) {
                continue;
            }
            let event = match packet {
                PreviewSetPacket::Unavailable { member, reason } => {
                    seeded.remove(&member);
                    PreviewSetEvent {
                        member,
                        update: PreviewSetUpdate::Unavailable(reason),
                    }
                }
                PreviewSetPacket::Chunk { member, frame } => {
                    let chunk = match frame.frame_type {
                        FrameType::Grid => {
                            let grid = frame
                                .grid_payload()
                                .map_err(invalid)?
                                .ok_or_else(|| invalid("missing preview grid"))?;
                            if grid.is_full_snapshot {
                                seeded.insert(member.clone());
                            }
                            if !seeded.contains(&member) {
                                return Err(invalid("preview patch before full seed"));
                            }
                            TerminalChunk::Grid(grid)
                        }
                        FrameType::Modes if seeded.contains(&member) => {
                            let (alt_screen, bracketed_paste, mouse) = frame
                                .terminal_modes_payload()
                                .ok_or_else(|| invalid("invalid preview modes"))?;
                            TerminalChunk::Modes {
                                keyboard: frame.keyboard_state_payload().map_err(invalid)?,
                                alt_screen,
                                bracketed_paste,
                                mouse,
                                // A preview takes no keyboard input, so it
                                // has nothing to protect.
                                secret_input: false,
                            }
                        }
                        _ => return Err(invalid("unexpected preview frame before seed")),
                    };
                    PreviewSetEvent {
                        member,
                        update: PreviewSetUpdate::Chunk(chunk),
                    }
                }
            };
            if events.send(event).await.is_err() {
                return Ok(());
            }
        }
    }
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_members_keep_identity_and_remove_readd_changes_generation() {
        let id = SessionId::new("fixture");
        let (first, next) =
            membership_for(&PreviewSetMembership::default(), 1, vec![id.clone()]).unwrap();
        let (same, unchanged) = membership_for(&first, next, vec![id.clone()]).unwrap();
        assert_eq!(first, same);
        assert_eq!(next, unchanged);
        let (empty, next) = membership_for(&first, next, Vec::new()).unwrap();
        let (readded, _) = membership_for(&empty, next, vec![id.clone()]).unwrap();
        assert_ne!(first.members[0].generation, readded.members[0].generation);
        assert!(membership_for(&first, next, vec![id.clone(), id]).is_err());
    }

    async fn fixture() -> (PreviewSet, UnixStream) {
        let (client, mut server) = UnixStream::pair().unwrap();
        let accepting = tokio::spawn(PreviewSet::adopt(client, PreviewSetOptions::default()));
        let mut request = Vec::new();
        loop {
            let byte = server.read_u8().await.unwrap();
            if byte == b'\n' {
                break;
            }
            request.push(byte);
        }
        let request: PreviewSetRequest = serde_json::from_slice(&request).unwrap();
        assert!(request.preview_set);
        assert!(!request.terminal_graphics);
        server.write_all(b"{\"version\":1}\n").await.unwrap();
        (accepting.await.unwrap().unwrap(), server)
    }

    async fn membership(server: &mut UnixStream) -> PreviewSetMembership {
        let mut bytes = Vec::new();
        loop {
            let byte = server.read_u8().await.unwrap();
            if byte == b'\n' {
                break;
            }
            bytes.push(byte);
        }
        serde_json::from_slice(&bytes).unwrap()
    }

    fn grid_packet(member: &PreviewMember, full: bool) -> Vec<u8> {
        use diri_proto::{
            frames::{Frame, FrameCodec},
            grid::GridUpdate,
        };
        let frame = FrameCodec::encode(
            &Frame::grid(&GridUpdate {
                cols: 1,
                rows: 1,
                cursor_col: 0,
                cursor_row: 0,
                cursor_visible: true,
                is_full_snapshot: full,
                changed_rows: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        let mut bytes = PreviewSetHeader::Chunk {
            member: member.clone(),
            frame_bytes: frame.len(),
        }
        .encode()
        .unwrap();
        bytes.extend(frame);
        bytes
    }

    #[tokio::test]
    async fn remove_readd_filters_queued_old_frames_and_requires_a_new_seed() {
        let (mut client, mut server) = fixture().await;
        let id = SessionId::new("fixture");
        let first = client.set_sessions(vec![id.clone()]).unwrap();
        assert_eq!(membership(&mut server).await, first);
        server
            .write_all(&grid_packet(&first.members[0], true))
            .await
            .unwrap();
        // The first event occupies the bounded channel. Leave it queued while
        // the same session is removed and re-added before another socket write.
        while client.events.is_empty() {
            tokio::task::yield_now().await;
        }
        client.set_sessions(Vec::new()).unwrap();
        let second = client.set_sessions(vec![id]).unwrap();
        assert_ne!(first, second);
        assert_eq!(membership(&mut server).await, second);
        server
            .write_all(&grid_packet(&first.members[0], false))
            .await
            .unwrap();
        server
            .write_all(&grid_packet(&second.members[0], true))
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(1), client.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.member, second.members[0]);
        assert!(
            matches!(event.update, PreviewSetUpdate::Chunk(TerminalChunk::Grid(grid)) if grid.is_full_snapshot)
        );
        client.close().await;
    }

    #[tokio::test]
    async fn a_new_generation_cannot_inherit_the_old_generations_seed() {
        let (mut client, mut server) = fixture().await;
        let id = SessionId::new("fixture");
        let first = client.set_sessions(vec![id.clone()]).unwrap();
        membership(&mut server).await;
        server
            .write_all(&grid_packet(&first.members[0], true))
            .await
            .unwrap();
        assert!(client.recv().await.is_some());
        client.set_sessions(Vec::new()).unwrap();
        let second = client.set_sessions(vec![id]).unwrap();
        membership(&mut server).await;
        server
            .write_all(&grid_packet(&second.members[0], false))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), client.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn idle_has_no_ping_and_close_cancels_a_backpressured_receiver() {
        let (mut client, mut server) = fixture().await;
        let desired = client
            .set_sessions(vec![SessionId::new("fixture")])
            .unwrap();
        membership(&mut server).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            server.try_read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        for full in [true, false, false] {
            server
                .write_all(&grid_packet(&desired.members[0], full))
                .await
                .unwrap();
        }
        while client.events.is_empty() {
            tokio::task::yield_now().await;
        }
        client.close().await;
        assert_eq!(server.read(&mut [0; 1]).await.unwrap(), 0);
        assert!(
            client
                .set_sessions(vec![SessionId::new("fixture")])
                .is_err()
        );
    }
}
