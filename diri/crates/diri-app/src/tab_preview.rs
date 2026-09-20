//! Bounded lifetimes for read-only preview resources. A slot is never a session owner.
use std::collections::{HashMap, HashSet};

use diri_proto::SessionId;

pub(crate) const MAX_VISIBLE_PREVIEWS: usize = 8;

pub(crate) struct PreviewSet<T> {
    slots: HashMap<SessionId, T>,
}

impl<T> Default for PreviewSet<T> {
    fn default() -> Self {
        Self {
            slots: HashMap::new(),
        }
    }
}

impl<T> PreviewSet<T> {
    /// Release offscreen slots before opening replacements. Failed slots remain
    /// present until they leave the viewport, avoiding a render-driven retry loop.
    pub(crate) fn sync(
        &mut self,
        visible: impl IntoIterator<Item = SessionId>,
        mut open: impl FnMut(SessionId) -> T,
    ) {
        let mut seen = HashSet::new();
        let wanted: Vec<_> = visible
            .into_iter()
            .filter(|id| seen.insert(id.clone()))
            .take(MAX_VISIBLE_PREVIEWS)
            .collect();
        self.slots.retain(|id, _| wanted.contains(id));
        for id in wanted {
            self.slots.entry(id.clone()).or_insert_with(|| open(id));
        }
    }

    pub(crate) fn get(&self, id: &SessionId) -> Option<&T> {
        self.slots.get(id)
    }

    pub(crate) fn clear(&mut self) {
        self.slots.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    #[derive(Default)]
    struct FakeSource {
        live: HashSet<SessionId>,
        opened: Vec<SessionId>,
        peak: usize,
    }
    struct Lease(SessionId, Rc<RefCell<FakeSource>>);
    impl Drop for Lease {
        fn drop(&mut self) {
            assert!(self.1.borrow_mut().live.remove(&self.0));
        }
    }
    fn open(source: &Rc<RefCell<FakeSource>>, id: SessionId) -> Lease {
        let mut state = source.borrow_mut();
        assert!(state.live.insert(id.clone()));
        state.opened.push(id.clone());
        state.peak = state.peak.max(state.live.len());
        Lease(id, source.clone())
    }
    fn ids(start: usize, end: usize) -> impl Iterator<Item = SessionId> {
        (start..end).map(|index| SessionId::new(format!("preview-{index}")))
    }

    #[test]
    fn visible_fleet_is_bounded_and_releases_before_opening_replacements() {
        let source = Rc::new(RefCell::new(FakeSource::default()));
        let mut previews = PreviewSet::default();
        previews.sync(ids(0, 100), |id| open(&source, id));
        assert_eq!(source.borrow().live.len(), 8);
        previews.sync(ids(5, 20), |id| open(&source, id));
        assert_eq!(source.borrow().opened.len(), 13);
        assert_eq!(source.borrow().peak, 8);
        assert!(previews.get(&SessionId::new("preview-0")).is_none());
        previews.clear();
        assert!(source.borrow().live.is_empty());
    }

    #[test]
    fn same_visible_identity_keeps_one_source_until_dismiss_or_drop() {
        let source = Rc::new(RefCell::new(FakeSource::default()));
        let mut previews = PreviewSet::default();
        previews.sync(ids(0, 2).chain(ids(0, 2)), |id| open(&source, id));
        for _ in 0..100 {
            previews.sync(ids(0, 2), |id| open(&source, id));
        }
        assert_eq!(source.borrow().opened.len(), 2);
        previews.sync(std::iter::empty(), |id| open(&source, id));
        assert!(source.borrow().live.is_empty());
        previews.sync(ids(0, 2), |id| open(&source, id));
        drop(previews);
        assert!(source.borrow().live.is_empty());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreviewState {
    Loading,
    Live,
    Disconnected,
    Unavailable,
}

pub(crate) struct LivePreview {
    pub(crate) element: diri_term::element::TerminalElement,
    pub(crate) state: tokio::sync::watch::Receiver<PreviewState>,
    _notifications: gpui::Task<()>,
    abort: tokio::task::AbortHandle,
}

impl Drop for LivePreview {
    fn drop(&mut self) {
        // Aborting the drain also drops SessionPreview, closing its socket.
        self.abort.abort();
    }
}

impl LivePreview {
    pub(crate) fn open(
        runtime: &tokio::runtime::Handle,
        socket: std::path::PathBuf,
        id: SessionId,
        cx: &mut gpui::Context<crate::session_surfaces::SessionSurfaces>,
    ) -> Self {
        use diri_term::{buffer::GridBuffer, element::TerminalElement};
        let element = TerminalElement::with_buffer(GridBuffer::new(0, 0))
            .focused(false)
            .without_cursor();
        let buffer = element.buffer();
        let (updates, state) = tokio::sync::watch::channel(PreviewState::Loading);
        let mut changes = state.clone();
        let task = runtime.spawn(receive_preview(socket, id, buffer, updates));
        let abort = task.abort_handle();
        let notifications = cx.spawn(async move |this, cx| {
            while changes.changed().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        });
        Self {
            element,
            state,
            _notifications: notifications,
            abort,
        }
    }
}

async fn receive_preview(
    socket: std::path::PathBuf,
    id: SessionId,
    buffer: diri_term::element::SharedGridBuffer,
    updates: tokio::sync::watch::Sender<PreviewState>,
) {
    use diri_client::attachment::{SessionPreview, TerminalChunk};
    let mut preview = match SessionPreview::connect(socket, id).await {
        Ok(preview) => preview,
        Err(_) => {
            updates.send_replace(PreviewState::Unavailable);
            return;
        }
    };
    let mut seeded = false;
    let first_grid_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let chunk = if seeded {
            preview.chunks.recv().await
        } else {
            tokio::time::timeout_at(first_grid_deadline, preview.chunks.recv())
                .await
                .unwrap_or(None)
        };
        let Some(chunk) = chunk else { break };
        if let TerminalChunk::Grid(grid) = chunk {
            if !seeded && !grid.is_full_snapshot {
                continue;
            }
            seeded = true;
            buffer
                .write()
                .expect("preview grid lock poisoned")
                .apply(grid);
            // Only an absolute status/dirty notification is coalesced;
            // every ordered grid patch was applied before notifying.
            updates.send_replace(PreviewState::Live);
        }
    }
    updates.send_replace(if seeded {
        PreviewState::Disconnected
    } else {
        PreviewState::Unavailable
    });
}

#[cfg(test)]
mod source_tests {
    use super::*;
    use diri_proto::{
        frames::{Frame, FrameCodec},
        grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle},
    };
    use diri_term::buffer::GridBuffer;
    use std::{
        sync::{Arc, RwLock},
        time::Duration,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    fn grid(full: bool, row: u16, ch: char) -> GridUpdate {
        GridUpdate {
            cols: 8,
            rows: 2,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: false,
            is_full_snapshot: full,
            changed_rows: vec![ChangedRow::new(
                row,
                vec![
                    GridCell::new(
                        ch as u32,
                        TermColor::Default,
                        TermColor::DefaultInverted,
                        TermStyle::empty()
                    );
                    8
                ],
            )],
        }
    }

    #[tokio::test]
    async fn inactive_preview_applies_all_diffs_without_sending_terminal_effects() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("preview.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let buffer = Arc::new(RwLock::new(GridBuffer::new(0, 0)));
        let (updates, state) = tokio::sync::watch::channel(PreviewState::Loading);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            stream.read_line(&mut request).await.unwrap();
            let request: serde_json::Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["preview"], "inactive");
            assert!(request.get("attach").is_none());
            stream
                .get_mut()
                .write_all(b"{\"preview\":\"inactive\",\"version\":1}\n")
                .await
                .unwrap();
            for update in [grid(true, 0, 'a'), grid(false, 1, 'b'), grid(false, 0, 'c')] {
                stream
                    .get_mut()
                    .write_all(&FrameCodec::encode(&Frame::grid(&update).unwrap()).unwrap())
                    .await
                    .unwrap();
            }
            // The source is receive-only: no resize, input, scroll or keepalive.
            let mut byte = [0];
            assert!(
                tokio::time::timeout(Duration::from_millis(30), stream.read(&mut byte))
                    .await
                    .is_err()
            );
        });
        tokio::time::timeout(
            Duration::from_secs(2),
            receive_preview(socket, SessionId::new("inactive"), buffer.clone(), updates),
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(*state.borrow(), PreviewState::Disconnected);
        let grid = buffer.read().unwrap();
        assert_eq!((grid.cols, grid.rows), (8, 2));
        assert_eq!(grid.row_text_with_columns(0).unwrap().0, "cccccccc");
        assert_eq!(grid.row_text_with_columns(1).unwrap().0, "bbbbbbbb");
    }

    #[tokio::test]
    async fn cancel_during_loading_closes_source_and_failure_does_not_retry() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("preview.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let buffer = Arc::new(RwLock::new(GridBuffer::new(0, 0)));
        let (updates, _) = tokio::sync::watch::channel(PreviewState::Loading);
        let worker = tokio::spawn(receive_preview(
            socket.clone(),
            SessionId::new("inactive"),
            buffer.clone(),
            updates,
        ));
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut request = String::new();
        stream.read_line(&mut request).await.unwrap();
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(bytes.is_empty());
        drop(listener);
        let (updates, state) = tokio::sync::watch::channel(PreviewState::Loading);
        receive_preview(socket, SessionId::new("inactive"), buffer, updates).await;
        assert_eq!(*state.borrow(), PreviewState::Unavailable);
    }
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) mod screenshot_fixture {
    use super::PreviewState;
    use diri_proto::{
        frames::{Frame, FrameCodec},
        grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle},
    };
    use std::path::PathBuf;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    /// Synthetic Engine-local source used only by the full-workbench renderer.
    pub(crate) struct Source {
        pub(crate) runtime: tokio::runtime::Runtime,
        pub(crate) socket: PathBuf,
        _directory: tempfile::TempDir,
    }
    impl Source {
        pub(crate) fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("preview.sock");
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let listener = {
                let _entered = runtime.enter();
                tokio::net::UnixListener::bind(&socket).unwrap()
            };
            runtime.spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    tokio::spawn(async move {
                        let mut stream = BufReader::new(stream);
                        let mut line = String::new();
                        stream.read_line(&mut line).await.unwrap();
                        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                        assert!(request.get("attach").is_none());
                        let mut ack = serde_json::to_vec(&request).unwrap();
                        ack.push(b'\n');
                        stream.get_mut().write_all(&ack).await.unwrap();
                        let lines = [
                            "$ cargo test -p diri-engine",
                            "",
                            "running 4 tests",
                            "test preserves_session_identity ... ok",
                            "test preview_never_resizes ... ok",
                            "test shared_terminal_grid ... ok",
                            "test cancel_closes_preview ... ok",
                            "",
                            "test result: ok. 4 passed; 0 failed",
                            "",
                            "$ git status --short",
                            " M crates/diri-app/src/tab_preview.rs",
                            "",
                            "$ ",
                        ];
                        let update = GridUpdate {
                            cols: 80,
                            rows: 24,
                            cursor_col: 2,
                            cursor_row: 13,
                            cursor_visible: true,
                            is_full_snapshot: true,
                            changed_rows: lines
                                .iter()
                                .enumerate()
                                .map(|(index, line)| {
                                    ChangedRow::new(
                                        index as u16,
                                        line.chars()
                                            .map(|ch| {
                                                GridCell::new(
                                                    ch as u32,
                                                    if line.contains(" ... ok") {
                                                        TermColor::Ansi(2)
                                                    } else {
                                                        TermColor::Default
                                                    },
                                                    TermColor::DefaultInverted,
                                                    TermStyle::empty(),
                                                )
                                            })
                                            .collect(),
                                    )
                                })
                                .collect(),
                        };
                        stream
                            .get_mut()
                            .write_all(&FrameCodec::encode(&Frame::grid(&update).unwrap()).unwrap())
                            .await
                            .unwrap();
                        let mut unexpected = Vec::new();
                        stream.read_to_end(&mut unexpected).await.unwrap();
                        assert!(unexpected.is_empty());
                    });
                }
            });
            Self {
                runtime,
                socket,
                _directory: directory,
            }
        }
        pub(crate) fn settle(&self, states: Vec<tokio::sync::watch::Receiver<PreviewState>>) {
            self.runtime.block_on(async {
                for mut state in states {
                    tokio::time::timeout(std::time::Duration::from_secs(2), async {
                        while *state.borrow() != PreviewState::Live {
                            state.changed().await.unwrap();
                        }
                    })
                    .await
                    .unwrap();
                }
            });
        }
    }
}
