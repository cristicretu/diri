//! One receive-only connection for the visible leaves of saved tab cards.
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use diri_client::{PreviewSet, PreviewSetUpdate, TerminalChunk};
use diri_proto::{SessionId, preview_set::PreviewSetMembership};
use diri_term::{
    buffer::GridBuffer,
    element::{SharedGridBuffer, TerminalElement},
};
use tokio::sync::watch;

use crate::tab_preview::PreviewState;

pub(crate) const MAX_SOURCES: usize = 16;

#[derive(Clone)]
struct Target {
    id: SessionId,
    buffer: SharedGridBuffer,
    state: watch::Sender<PreviewState>,
}

pub(crate) struct Preview {
    pub(crate) element: TerminalElement,
    pub(crate) state: watch::Receiver<PreviewState>,
    target: Target,
}
struct Worker {
    desired: watch::Sender<Vec<Target>>,
    abort: tokio::task::AbortHandle,
    _notifications: gpui::Task<()>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.abort.abort();
    }
}
#[derive(Default)]
pub(crate) struct WorkspacePreviews {
    sources: HashMap<SessionId, Preview>,
    worker: Option<Worker>,
}
impl WorkspacePreviews {
    pub(crate) fn get(&self, id: &SessionId) -> Option<&Preview> {
        self.sources.get(id)
    }
    pub(crate) fn elements(&self) -> HashMap<SessionId, TerminalElement> {
        self.sources
            .iter()
            .map(|(id, preview)| (id.clone(), preview.element.clone()))
            .collect()
    }
    pub(crate) fn clear(&mut self) {
        self.worker = None;
        self.sources.clear();
    }
    pub(crate) fn sync(
        &mut self,
        wanted: Vec<SessionId>,
        runtime: &tokio::runtime::Handle,
        socket: PathBuf,
        cx: &mut gpui::Context<crate::session_surfaces::SessionSurfaces>,
    ) {
        let mut unique = std::collections::HashSet::new();
        let wanted = wanted
            .into_iter()
            .filter(|id| unique.insert(id.clone()))
            .take(MAX_SOURCES)
            .collect::<Vec<_>>();
        if wanted.is_empty() {
            self.clear();
            return;
        }
        let changed = self.sources.len() != wanted.len()
            || wanted.iter().any(|id| !self.sources.contains_key(id));
        if !changed {
            return;
        }
        self.sources.retain(|id, _| wanted.contains(id));
        for id in &wanted {
            self.sources.entry(id.clone()).or_insert_with(|| {
                let element = TerminalElement::with_buffer(GridBuffer::new(0, 0)).focused(false);
                let (state_tx, state) = watch::channel(PreviewState::Loading);
                Preview {
                    target: Target {
                        id: id.clone(),
                        buffer: element.buffer(),
                        state: state_tx,
                    },
                    element,
                    state,
                }
            });
        }
        let desired = wanted
            .iter()
            .map(|id| self.sources[id].target.clone())
            .collect::<Vec<_>>();
        if let Some(worker) = &self.worker {
            // Failure is retained until dismissal, never retried by a repaint.
            if worker.desired.send(desired).is_err() {
                for preview in self.sources.values() {
                    preview.target.state.send_replace(PreviewState::Unavailable);
                }
            }
        } else {
            let (desired_tx, desired_rx) = watch::channel(desired);
            let (dirty, mut notifications) = watch::channel(0u64);
            let task = runtime.spawn(receive(socket, desired_rx, dirty));
            let notifications = cx.spawn(async move |this, cx| {
                while notifications.changed().await.is_ok() {
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        return;
                    }
                }
            });
            self.worker = Some(Worker {
                desired: desired_tx,
                abort: task.abort_handle(),
                _notifications: notifications,
            });
        }
    }
}

struct Member {
    target: Target,
    generation: u64,
    seeded: bool,
    deadline: Option<tokio::time::Instant>,
}

fn membership_targets(current: &HashMap<SessionId, Member>, desired: &[Target]) -> Vec<SessionId> {
    desired
        .iter()
        .filter(|target| {
            current
                .get(&target.id)
                .is_some_and(|member| Arc::ptr_eq(&member.target.buffer, &target.buffer))
        })
        .map(|target| target.id.clone())
        .collect()
}

fn install_members(
    current: &mut HashMap<SessionId, Member>,
    desired: Vec<Target>,
    membership: PreviewSetMembership,
) {
    current.retain(|id, _| desired.iter().any(|target| &target.id == id));
    for target in desired {
        let Some(identity) = membership
            .members
            .iter()
            .find(|identity| identity.session_id == target.id)
        else {
            continue;
        };
        let keep = current.get(&target.id).is_some_and(|member| {
            member.generation == identity.generation
                && Arc::ptr_eq(&member.target.buffer, &target.buffer)
        });
        if !keep {
            current.insert(
                target.id.clone(),
                Member {
                    target,
                    generation: identity.generation,
                    seeded: false,
                    deadline: Some(tokio::time::Instant::now() + Duration::from_secs(3)),
                },
            );
        }
    }
}

async fn receive(
    socket: PathBuf,
    mut desired: watch::Receiver<Vec<Target>>,
    dirty: watch::Sender<u64>,
) {
    let mut connection = match PreviewSet::connect(socket).await {
        Ok(connection) => connection,
        Err(_) => {
            for target in desired.borrow().iter() {
                target.state.send_replace(PreviewState::Unavailable);
            }
            dirty.send_modify(|revision| *revision = revision.wrapping_add(1));
            return;
        }
    };
    let mut current = HashMap::new();
    let mut next = Some(desired.borrow_and_update().clone());
    loop {
        if let Some(targets) = next.take() {
            // A remove/re-add can be coalesced in our watch. Drop retained IDs
            // whose buffer identity changed before adding them again, forcing
            // a new client generation and full seed for the new UI view.
            let retained = membership_targets(&current, &targets);
            if connection.set_sessions(retained).is_err() {
                break;
            }
            let membership = match connection
                .set_sessions(targets.iter().map(|target| target.id.clone()).collect())
            {
                Ok(membership) => membership,
                Err(_) => break,
            };
            install_members(&mut current, targets, membership);
        }
        let deadline = current.values().filter_map(|member| member.deadline).min();
        tokio::select! {
            result=desired.changed()=>{
                if result.is_err() { break; }
                next=Some(desired.borrow_and_update().clone());
            }
            event=connection.recv()=>{
                let Some(event)=event else { break; };
                let Some(member)=current.get_mut(&event.member.session_id) else { continue; };
                if member.generation!=event.member.generation { continue; }
                match event.update {
                    PreviewSetUpdate::Chunk(TerminalChunk::Grid(grid))=>{
                        if !member.seeded && !grid.is_full_snapshot { continue; }
                        member.seeded=true; member.deadline=None;
                        member.target.buffer.write().expect("preview grid").apply(grid);
                        member.target.state.send_replace(PreviewState::Live);
                        dirty.send_modify(|revision| *revision=revision.wrapping_add(1));
                    }
                    PreviewSetUpdate::Unavailable(_)=>{
                        member.deadline=None;
                        member.target.state.send_replace(PreviewState::Unavailable);
                        dirty.send_modify(|revision| *revision=revision.wrapping_add(1));
                    }
                    _=>{}
                }
            }
            _=async { if let Some(deadline)=deadline { tokio::time::sleep_until(deadline).await; } else { std::future::pending::<()>().await; } }=>{
                // No idle timer after full seeds. A missing initial seed is a
                // visible failed source and cannot consume admission forever.
                let now=tokio::time::Instant::now();
                for member in current.values_mut() {
                    if member.deadline.is_some_and(|deadline|deadline<=now) {
                        member.deadline=None; member.target.state.send_replace(PreviewState::Unavailable);
                    }
                }
                let active=current.values().filter(|member|member.seeded || member.deadline.is_some()).map(|member|member.target.id.clone()).collect();
                if connection.set_sessions(active).is_err() { break; }
                dirty.send_modify(|revision| *revision=revision.wrapping_add(1));
            }
        }
    }
    for target in desired.borrow().iter() {
        let seeded = current.get(&target.id).is_some_and(|member| {
            member.seeded && Arc::ptr_eq(&member.target.buffer, &target.buffer)
        });
        target.state.send_replace(if seeded {
            PreviewState::Disconnected
        } else {
            PreviewState::Unavailable
        });
    }
    dirty.send_modify(|revision| *revision = revision.wrapping_add(1));
    connection.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::{
        frames::{Frame, FrameCodec},
        grid::{ChangedRow, GridCell, GridUpdate},
        preview_set::{PreviewMember, PreviewSetHeader},
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };
    fn target(id: &str) -> Target {
        let (state, _) = watch::channel(PreviewState::Loading);
        Target {
            id: SessionId::new(id),
            buffer: Arc::new(std::sync::RwLock::new(GridBuffer::new(0, 0))),
            state,
        }
    }
    fn packet(member: &PreviewMember, full: bool, row: u16, ch: char) -> Vec<u8> {
        let mut cell = GridCell::BLANK;
        cell.scalar = ch as u32;
        let frame = FrameCodec::encode(
            &Frame::grid(&GridUpdate {
                cols: 4,
                rows: 2,
                cursor_col: 0,
                cursor_row: 0,
                cursor_visible: false,
                is_full_snapshot: full,
                changed_rows: vec![ChangedRow::new(row, vec![cell; 4])],
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
    async fn membership(stream: &mut BufReader<tokio::net::UnixStream>) -> PreviewSetMembership {
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }
    async fn wait_live(target: &Target, ch: char) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if target
                    .buffer
                    .read()
                    .unwrap()
                    .cells
                    .first()
                    .is_some_and(|cell| cell.scalar == ch as u32)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn multiplex_keeps_order_reseeds_new_views_and_closes_without_terminal_effects() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("preview.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let first = target("one");
        let second = target("two");
        let (desired, rx) = watch::channel(vec![first.clone(), second.clone()]);
        let (dirty, _) = watch::channel(0);
        let task = tokio::spawn(receive(socket, rx, dirty));
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut header = String::new();
        stream.read_line(&mut header).await.unwrap();
        let header: serde_json::Value = serde_json::from_str(&header).unwrap();
        assert_eq!(header["preview_set"], true);
        assert!(header.get("attach").is_none());
        stream
            .get_mut()
            .write_all(b"{\"version\":1}\n")
            .await
            .unwrap();
        let members = membership(&mut stream).await;
        for member in &members.members {
            for frame in [
                packet(member, true, 0, 'a'),
                packet(member, false, 1, 'b'),
                packet(member, false, 0, 'c'),
            ] {
                stream.get_mut().write_all(&frame).await.unwrap();
            }
        }
        wait_live(&first, 'c').await;
        wait_live(&second, 'c').await;
        assert_eq!(
            first
                .buffer
                .read()
                .unwrap()
                .row_text_with_columns(1)
                .unwrap()
                .0,
            "bbbb"
        );
        // No input, resize, scroll or idle heartbeat follows a healthy seed.
        assert!(
            tokio::time::timeout(Duration::from_millis(30), stream.read_u8())
                .await
                .is_err()
        );
        let replacement = target("one");
        desired.send_replace(vec![replacement.clone(), second.clone()]);
        let fresh = loop {
            let update = membership(&mut stream).await;
            if let Some(member) = update
                .members
                .iter()
                .find(|member| member.session_id == first.id)
            {
                break member.clone();
            }
        };
        let old = members
            .members
            .iter()
            .find(|member| member.session_id == first.id)
            .unwrap();
        assert_ne!(fresh.generation, old.generation);
        stream
            .get_mut()
            .write_all(&packet(old, false, 0, 'x'))
            .await
            .unwrap();
        stream
            .get_mut()
            .write_all(&packet(&fresh, true, 0, 'n'))
            .await
            .unwrap();
        wait_live(&replacement, 'n').await;
        assert_eq!(
            first
                .buffer
                .read()
                .unwrap()
                .row_text_with_columns(0)
                .unwrap()
                .0,
            "cccc"
        );
        assert_eq!(
            second
                .buffer
                .read()
                .unwrap()
                .row_text_with_columns(0)
                .unwrap()
                .0,
            "cccc"
        );
        drop(desired);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(stream.read_u8().await.is_err());
    }
    #[tokio::test]
    async fn connection_failure_finishes_loading_and_does_not_retry() {
        let temp = tempfile::tempdir().unwrap();
        let item = target("one");
        let (_desired, rx) = watch::channel(vec![item.clone()]);
        let (dirty, _) = watch::channel(0);
        receive(temp.path().join("missing.sock"), rx, dirty).await;
        assert_eq!(*item.state.borrow(), PreviewState::Unavailable);
    }
}
