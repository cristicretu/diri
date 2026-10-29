//! The phone's real terminal: a styled grid, scrollback pages, and a live
//! snapshot/diff stream.
//!
//! Every stream is a read-only Engine **preview** (`SessionPreview`), the same
//! receive-only channel the desktop's tab previews use. It observes the
//! Engine-owned grid, so it never attaches to a Remote Holder, never takes the
//! controller lease, and never disturbs the Mac's own attach. Typing goes
//! through `session.send_text` unframed, the path `/key` already uses.
//!
//! One task per stream owns the mirror, the 16 ms coalescing and the diff
//! construction; it hands finished SSE frames to the socket writer through a
//! small bounded queue. When the phone's socket falls behind and that queue
//! is full, diffs are discarded and the next free slot carries a fresh
//! `snapshot` instead, so a slow phone costs memory proportional to one grid.
//!
//! Terminal contents and input bytes are never logged.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use diri_client::{ClientError, DaemonClient, EventEnvelope, SessionPreview, TerminalChunk};
use diri_proto::grid::{GridCell, GridRowCodec, GridUpdate, RowMetadata, TermColor, TermStyle};
use diri_proto::terminal::MouseTrackingMode;
use diri_proto::{ReadScrollbackCellsResult, SessionId, SessionStatus};
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

/// Upper bound on how long output is held to merge it with what follows.
pub const COALESCE: Duration = Duration::from_millis(16);
/// Finished frames between the stream task and the phone's socket. Small on
/// purpose: anything beyond a few frames is stale, and a reseed replaces it.
pub const OUT_QUEUE: usize = 4;
pub const DEFAULT_HISTORY_PAGE: i64 = 200;
pub const MAX_HISTORY_PAGE: i64 = 1000;
/// Opening a preview and receiving its seed. Local, so generous is plenty.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
/// A remote history read crosses SSH to the Holder.
const HISTORY_TIMEOUT: Duration = Duration::from_secs(15);

/// One counter for the process: a `seq` is never reused, so a phone can tell
/// a frame from a previous stream apart from the current one.
static SEQ: AtomicU64 = AtomicU64::new(1);

fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// The 16 ANSI colours plus default foreground and background the Mac paints
/// with, so the phone renders the same theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalPalette {
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub ansi: [[u8; 3]; 16],
}

impl Default for TerminalPalette {
    /// Dirijor Dark, the app's default theme. A gateway without an app (the
    /// standalone binary on a VPS) serves this.
    fn default() -> Self {
        const fn rgb(value: u32) -> [u8; 3] {
            [(value >> 16) as u8, (value >> 8) as u8, value as u8]
        }
        Self {
            foreground: [230, 230, 230],
            background: [18, 19, 24],
            ansi: [
                rgb(0x000000),
                rgb(0xcd3131),
                rgb(0x0dbc79),
                rgb(0xe5e510),
                rgb(0x2472c8),
                rgb(0xbc3fbc),
                rgb(0x11a8cd),
                rgb(0xe5e5e5),
                rgb(0x666666),
                rgb(0xf14c4c),
                rgb(0x23d18b),
                rgb(0xf5f543),
                rgb(0x3b8eea),
                rgb(0xd670d6),
                rgb(0x29b8db),
                rgb(0xffffff),
            ],
        }
    }
}

static PALETTE: RwLock<Option<TerminalPalette>> = RwLock::new(None);

/// Publishes the theme the Mac shows. Cheap when unchanged: the app calls it
/// as it renders.
pub fn set_terminal_palette(palette: TerminalPalette) {
    if current_palette() == palette {
        return;
    }
    if let Ok(mut slot) = PALETTE.write() {
        *slot = Some(palette);
    }
}

fn current_palette() -> TerminalPalette {
    PALETTE
        .read()
        .ok()
        .and_then(|slot| *slot)
        .unwrap_or_default()
}

fn hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn palette_json(palette: &TerminalPalette) -> Value {
    json!({
        "fg": hex(palette.foreground),
        "bg": hex(palette.background),
        "ansi": palette.ansi.iter().map(|color| hex(*color)).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// Lines
// ---------------------------------------------------------------------------

/// `Run.a` bits of the phone contract.
const A_BOLD: u8 = 1;
const A_DIM: u8 = 2;
const A_ITALIC: u8 = 4;
const A_UNDERLINE: u8 = 8;
const A_INVERSE: u8 = 16;
const A_STRIKE: u8 = 32;
const A_INVISIBLE: u8 = 64;

fn attributes(style: TermStyle) -> u8 {
    [
        (TermStyle::BOLD, A_BOLD),
        (TermStyle::DIM, A_DIM),
        (TermStyle::ITALIC, A_ITALIC),
        (TermStyle::UNDERLINE, A_UNDERLINE),
        (TermStyle::INVERSE, A_INVERSE),
        (TermStyle::CROSSED_OUT, A_STRIKE),
        (TermStyle::INVISIBLE, A_INVISIBLE),
    ]
    .into_iter()
    .filter(|(flag, _)| style.contains(*flag))
    .fold(0, |bits, (_, bit)| bits | bit)
}

/// A colour as the contract spells it. `null` is "the default for this
/// slot"; the swapped defaults (text drawn in the background colour and the
/// reverse) resolve against the palette, since `null` cannot say them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Paint {
    Default,
    Index(u8),
    Rgb([u8; 3]),
}

impl Paint {
    fn foreground(color: TermColor, palette: &TerminalPalette) -> Self {
        match color {
            TermColor::Default => Self::Default,
            TermColor::DefaultInverted => Self::Rgb(palette.background),
            TermColor::Ansi(index) => Self::Index(index),
            TermColor::Rgb(r, g, b) => Self::Rgb([r, g, b]),
        }
    }

    fn background(color: TermColor, palette: &TerminalPalette) -> Self {
        match color {
            TermColor::DefaultInverted => Self::Default,
            TermColor::Default => Self::Rgb(palette.foreground),
            TermColor::Ansi(index) => Self::Index(index),
            TermColor::Rgb(r, g, b) => Self::Rgb([r, g, b]),
        }
    }

    fn json(self) -> Value {
        match self {
            Self::Default => Value::Null,
            Self::Index(index) => Value::from(index),
            Self::Rgb(rgb) => Value::String(hex(rgb)),
        }
    }
}

/// One terminal row as a `Line`: runs of identically styled text, wide
/// characters kept whole (their spacer cells are dropped), combined
/// graphemes restored, and trailing default blanks trimmed. An empty row is
/// `{"runs":[],"wrapped":false}`.
pub fn line_json(cells: &[GridCell], metadata: &RowMetadata, palette: &TerminalPalette) -> Value {
    let mut runs: Vec<(Paint, Paint, u8, String)> = Vec::new();
    let mut wrapped = false;
    let mut graphemes = metadata.graphemes.iter().peekable();
    for (col, cell) in cells.iter().enumerate() {
        wrapped |= cell.style.contains(TermStyle::SOFT_WRAP);
        if cell.style.contains(TermStyle::WIDE_SPACER) {
            continue;
        }
        while graphemes
            .peek()
            .is_some_and(|(at, _)| usize::from(*at) < col)
        {
            graphemes.next();
        }
        let style = (
            Paint::foreground(cell.fg, palette),
            Paint::background(cell.bg, palette),
            attributes(cell.style),
        );
        let run = match runs.last_mut() {
            Some(last) if (last.0, last.1, last.2) == style => last,
            _ => {
                runs.push((style.0, style.1, style.2, String::new()));
                runs.last_mut().expect("just pushed")
            }
        };
        match graphemes.peek() {
            Some((at, text)) if usize::from(*at) == col => run.3.push_str(text),
            _ => run.3.push(match cell.scalar {
                0 => ' ',
                scalar => char::from_u32(scalar).unwrap_or('\u{fffd}'),
            }),
        }
    }
    // Trailing blanks the client would paint identically by leaving them out.
    const VISIBLE_ON_BLANK: u8 = A_INVERSE | A_UNDERLINE | A_STRIKE;
    while let Some(last) = runs.last_mut() {
        if last.1 != Paint::Default || last.2 & VISIBLE_ON_BLANK != 0 {
            break;
        }
        let kept = last.3.trim_end_matches(' ').len();
        last.3.truncate(kept);
        if !last.3.is_empty() {
            break;
        }
        runs.pop();
    }
    json!({
        "runs": runs.into_iter().map(|(fg, bg, a, t)| json!({
            "t": t,
            "fg": fg.json(),
            "bg": bg.json(),
            "a": a,
        })).collect::<Vec<_>>(),
        "wrapped": wrapped,
    })
}

// ---------------------------------------------------------------------------
// Mirror
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modes {
    pub alt_screen: bool,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    pub mouse: bool,
}

/// The stream task's copy of the visible grid, plus what changed since the
/// last frame the phone was sent.
#[derive(Debug, Default)]
pub struct Mirror {
    cols: u16,
    rows: Vec<(Vec<GridCell>, RowMetadata)>,
    cursor: (u16, u16, bool),
    modes: Modes,
    history_size: i64,
    has_grid: bool,
    has_modes: bool,
    /// Rows changed since the last frame.
    dirty: BTreeSet<u16>,
    /// Cursor, modes or history size changed since the last frame.
    meta_dirty: bool,
    /// The Engine reseeded or the geometry changed: the next frame must be a
    /// snapshot, because the phone's rows no longer line up.
    snapshot_due: bool,
}

impl Mirror {
    pub fn apply(&mut self, update: GridUpdate) {
        let rows = usize::from(update.rows);
        if update.is_full_snapshot || update.cols != self.cols || rows != self.rows.len() {
            self.cols = update.cols;
            self.rows = vec![(Vec::new(), RowMetadata::default()); rows];
            self.dirty.clear();
            self.snapshot_due = true;
        }
        self.has_grid = true;
        let cursor = (update.cursor_col, update.cursor_row, update.cursor_visible);
        if cursor != self.cursor {
            self.cursor = cursor;
            self.meta_dirty = true;
        }
        let cols = usize::from(update.cols);
        for row in update.changed_rows {
            let Some(slot) = self.rows.get_mut(usize::from(row.y)) else {
                continue;
            };
            let mut cells = row.cells;
            cells.truncate(cols);
            *slot = (cells, row.metadata);
            self.dirty.insert(row.y);
        }
    }

    pub fn set_modes(&mut self, modes: Modes) {
        self.has_modes = true;
        if modes != self.modes {
            self.modes = modes;
            self.meta_dirty = true;
        }
    }

    pub fn set_history_size(&mut self, size: i64) {
        if size != self.history_size {
            self.history_size = size;
            self.meta_dirty = true;
        }
    }

    pub fn is_seeded(&self) -> bool {
        self.has_grid && self.has_modes
    }

    fn has_changes(&self) -> bool {
        self.snapshot_due || self.meta_dirty || !self.dirty.is_empty()
    }

    fn cursor_json(&self) -> Value {
        json!({
            "x": self.cursor.0,
            "y": self.cursor.1,
            "visible": self.cursor.2,
            // The Engine's terminal state does not track DECSCUSR.
            "shape": "block",
        })
    }

    fn modes_json(&self) -> Value {
        json!({
            "altScreen": self.modes.alt_screen,
            "appCursor": self.modes.app_cursor,
            "bracketedPaste": self.modes.bracketed_paste,
            "mouse": self.modes.mouse,
        })
    }

    /// The `/grid` body and `snapshot` event. Clears every pending change.
    pub fn snapshot(&mut self) -> Value {
        let palette = current_palette();
        self.dirty.clear();
        self.meta_dirty = false;
        self.snapshot_due = false;
        json!({
            "cols": self.cols,
            "rows": self.rows.len(),
            "seq": next_seq(),
            "historySize": self.history_size,
            "cursor": self.cursor_json(),
            "modes": self.modes_json(),
            "palette": palette_json(&palette),
            "lines": self.rows.iter()
                .map(|(cells, metadata)| line_json(cells, metadata, &palette))
                .collect::<Vec<_>>(),
        })
    }

    /// The `resume` event for a client that reconnected still showing a grid
    /// of the same size: only the rows whose digest differs from the one it
    /// sent, plus cursor, modes and palette. `None` means it needs a
    /// snapshot. Clears every pending change, like [`Self::snapshot`].
    pub fn resume(&mut self, from: &Resume) -> Option<Value> {
        if from.cols != self.cols || from.digests.len() != self.rows.len() {
            return None;
        }
        let palette = current_palette();
        let mut lines = Map::new();
        for (y, ((cells, metadata), digest)) in self.rows.iter().zip(&from.digests).enumerate() {
            let line = line_json(cells, metadata, &palette);
            if line_digest(&line) != *digest {
                lines.insert(y.to_string(), line);
            }
        }
        self.dirty.clear();
        self.meta_dirty = false;
        self.snapshot_due = false;
        Some(json!({
            "cols": self.cols,
            "rows": self.rows.len(),
            "seq": next_seq(),
            "historySize": self.history_size,
            "cursor": self.cursor_json(),
            "modes": self.modes_json(),
            "palette": palette_json(&palette),
            "lines": lines,
        }))
    }

    /// The `diff` event for everything changed since the last frame.
    fn diff(&mut self) -> Value {
        let palette = current_palette();
        let mut lines = Map::new();
        for y in std::mem::take(&mut self.dirty) {
            if let Some((cells, metadata)) = self.rows.get(usize::from(y)) {
                lines.insert(y.to_string(), line_json(cells, metadata, &palette));
            }
        }
        self.meta_dirty = false;
        json!({
            "seq": next_seq(),
            "lines": lines,
            "cursor": self.cursor_json(),
            "modes": self.modes_json(),
            "historySize": self.history_size,
        })
    }

    pub fn absorb(&mut self, chunk: TerminalChunk) -> bool {
        match chunk {
            TerminalChunk::Grid(update) => self.apply(update),
            TerminalChunk::Modes {
                keyboard,
                alt_screen,
                bracketed_paste,
                mouse,
                ..
            } => self.set_modes(Modes {
                alt_screen,
                app_cursor: keyboard.is_some_and(|state| state.application_cursor_keys),
                bracketed_paste,
                mouse: mouse.tracking != MouseTrackingMode::Off,
            }),
            TerminalChunk::Pong => {}
            TerminalChunk::Rejected(_) => return false,
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Resume
// ---------------------------------------------------------------------------

/// Rows a client is limited to describing on reconnect; matches `/resize`.
const MAX_RESUME_ROWS: usize = 300;

/// What a reconnecting client still shows: its width and one digest per
/// visible row, sent as `?resume=COLSxROWS.HEX.HEX…`. The gateway keeps no
/// state between connections; a mismatch only costs the rows it covers.
#[derive(Debug, PartialEq, Eq)]
pub struct Resume {
    cols: u16,
    digests: Vec<u32>,
}

impl Resume {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.split('.');
        let (cols, rows) = parts.next()?.split_once('x')?;
        let cols: u16 = cols.parse().ok()?;
        let rows: usize = rows.parse().ok()?;
        if rows > MAX_RESUME_ROWS {
            return None;
        }
        let digests = parts
            .map(|hex| u32::from_str_radix(hex, 16).ok())
            .collect::<Option<Vec<_>>>()?;
        (digests.len() == rows).then_some(Self { cols, digests })
    }
}

/// FNV-1a over a line's JSON exactly as the stream serialises it. The page
/// computes the same over `JSON.stringify` of the parsed line, which
/// reproduces these bytes for every value a line can hold (strings, small
/// integers, booleans, null, keys in the order received).
#[must_use]
pub fn line_digest(line: &Value) -> u32 {
    serde_json::to_string(line)
        .unwrap_or_default()
        .bytes()
        .fold(0x811c_9dc5_u32, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
        })
}

// ---------------------------------------------------------------------------
// Opening a preview
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum OpenError {
    /// The Engine refused or does not know the session.
    NotFound,
    /// The Engine socket is not answering.
    Unavailable(String),
}

/// Something that yields the Engine's terminal chunks. The live source is a
/// preview; tests feed a channel.
pub trait ChunkSource: Send {
    fn next_chunk(&mut self) -> impl Future<Output = Option<TerminalChunk>> + Send;
}

impl ChunkSource for SessionPreview {
    async fn next_chunk(&mut self) -> Option<TerminalChunk> {
        self.chunks.recv().await
    }
}

impl ChunkSource for mpsc::Receiver<TerminalChunk> {
    async fn next_chunk(&mut self) -> Option<TerminalChunk> {
        self.recv().await
    }
}

/// Opens a read-only preview and reads its seed (grid, then modes), with the
/// history size read alongside so neither waits on the other.
pub async fn open(
    client: &DaemonClient,
    session: &SessionId,
) -> Result<(SessionPreview, Mirror), OpenError> {
    let seeded = async {
        let mut preview = SessionPreview::connect(client.socket_path(), session.clone())
            .await
            .map_err(|error| match error {
                diri_client::AttachmentError::Io(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound
                            | std::io::ErrorKind::ConnectionRefused
                            | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    OpenError::Unavailable(error.to_string())
                }
                _ => OpenError::NotFound,
            })?;
        let mut mirror = Mirror::default();
        while !mirror.is_seeded() {
            let chunk = preview.next_chunk().await.ok_or(OpenError::NotFound)?;
            if !mirror.absorb(chunk) {
                return Err(OpenError::NotFound);
            }
        }
        Ok((preview, mirror))
    };
    let (seeded, history) = tokio::time::timeout(OPEN_TIMEOUT, async {
        tokio::join!(seeded, history_size(client, session))
    })
    .await
    .map_err(|_| OpenError::Unavailable("the Engine did not send the terminal in time".into()))?;
    let (preview, mut mirror) = seeded?;
    if let Some(size) = history {
        mirror.set_history_size(size);
    }
    Ok((preview, mirror))
}

async fn read_cells(
    client: &DaemonClient,
    session: &SessionId,
    first_row: i64,
    max_rows: i64,
) -> Result<ReadScrollbackCellsResult, ClientError> {
    let params = diri_proto::ReadScrollbackCellsParams {
        session_id: session.clone(),
        first_row,
        max_rows,
        styles: false,
    };
    let value = client
        .request(
            diri_proto::Method::SESSION_READ_SCROLLBACK_CELLS,
            Some(&params),
            Some(HISTORY_TIMEOUT),
        )
        .await?;
    serde_json::from_value(value).map_err(|error| ClientError::Json(error.to_string()))
}

/// Rows in the session's scrollback, from a zero-row read: no cells cross
/// the wire, local or remote.
async fn history_size(client: &DaemonClient, session: &SessionId) -> Option<i64> {
    read_cells(client, session, 0, 0)
        .await
        .ok()
        .map(|result| result.live_start_row)
}

/// `/history`: rows `[before - limit, before)` of the scrollback, oldest
/// first. `before` defaults to the end of history and is clamped to it, so a
/// page never includes visible rows.
pub async fn history(
    client: &DaemonClient,
    session: &SessionId,
    before: Option<i64>,
    limit: i64,
) -> Result<Value, ClientError> {
    let before = match before {
        Some(before) => before.max(0),
        None => read_cells(client, session, 0, 0).await?.live_start_row,
    };
    let start = before.saturating_sub(limit).max(0);
    let result = read_cells(client, session, start, before - start).await?;
    let history_size = result.live_start_row;
    let usable = usize::try_from((history_size - result.first_row).max(0))
        .unwrap_or_default()
        .min(usize::try_from(result.row_count.max(0)).unwrap_or_default());
    let rows = GridRowCodec::decode_rows(&result.payload, usable)
        .map_err(|error| ClientError::Json(format!("{error:?}")))?;
    let palette = current_palette();
    let empty = RowMetadata::default();
    let lines: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(index, cells)| {
            line_json(
                cells,
                result.metadata.get(index).unwrap_or(&empty),
                &palette,
            )
        })
        .collect();
    Ok(json!({
        "lines": lines,
        "start": result.first_row,
        "historySize": history_size,
    }))
}

// ---------------------------------------------------------------------------
// The stream
// ---------------------------------------------------------------------------

/// A complete SSE frame.
pub fn frame(event: &str, data: &Value) -> String {
    crate::http::event_frame(event, data)
}

/// The exit code a `session.updated`/`session.removed` event reports for
/// `session`, if it ends the stream.
fn exit_of(event: &EventEnvelope, session: &SessionId) -> Option<Option<i32>> {
    if event.params.get("id").and_then(Value::as_str) != Some(session.0.as_str()) {
        return None;
    }
    match event.name.as_str() {
        diri_proto::EventName::SESSION_REMOVED => Some(None),
        diri_proto::EventName::SESSION_UPDATED => {
            match serde_json::from_value::<SessionStatus>(event.params.get("status")?.clone())
                .ok()?
            {
                SessionStatus::Exited(info) => Some(info.code),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Runs one stream until the preview ends, the session exits, or the writer
/// goes away. Sends the opening snapshot first.
pub async fn pump<S: ChunkSource>(
    mut source: S,
    mut mirror: Mirror,
    client: Arc<DaemonClient>,
    session: SessionId,
    mut events: broadcast::Receiver<EventEnvelope>,
    out: mpsc::Sender<String>,
    resume: Option<Resume>,
) {
    let opening = match resume.as_ref().and_then(|from| mirror.resume(from)) {
        Some(delta) => frame("resume", &delta),
        None => frame("snapshot", &mirror.snapshot()),
    };
    if out.send(opening).await.is_err() {
        return;
    }
    let mut deadline: Option<Instant> = None;
    let mut last_flush = Instant::now();
    let mut needs_snapshot = false;
    // At most one history-size read in flight; another follows if output
    // arrived meanwhile. Remote reads cost a round trip, so never one per
    // frame.
    let (probe_tx, mut probe_rx) = mpsc::channel::<Option<i64>>(1);
    let mut probing = false;
    let mut probe_again = false;

    loop {
        tokio::select! {
            chunk = source.next_chunk() => {
                let Some(chunk) = chunk else { return };
                if !mirror.absorb(chunk) {
                    return;
                }
                if mirror.has_changes() && deadline.is_none() {
                    // Leading edge: a keystroke echo after a pause goes now;
                    // a burst is held to one frame per window.
                    deadline = Some((last_flush + COALESCE).max(Instant::now()));
                }
            }
            () = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                deadline = None;
                last_flush = Instant::now();
                if needs_snapshot {
                    // The snapshot that is waiting for room covers this.
                    continue;
                }
                let lines_changed = mirror.snapshot_due || !mirror.dirty.is_empty();
                let body = if mirror.snapshot_due {
                    frame("snapshot", &mirror.snapshot())
                } else {
                    frame("diff", &mirror.diff())
                };
                match out.try_send(body) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => needs_snapshot = true,
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                }
                if lines_changed && !mirror.modes.alt_screen {
                    if probing {
                        probe_again = true;
                    } else {
                        probing = true;
                        spawn_probe(&client, &session, &probe_tx);
                    }
                }
            }
            permit = out.reserve(), if needs_snapshot => {
                let Ok(permit) = permit else { return };
                needs_snapshot = false;
                permit.send(frame("snapshot", &mirror.snapshot()));
            }
            Some(size) = probe_rx.recv() => {
                probing = false;
                if let Some(size) = size {
                    mirror.set_history_size(size);
                    if mirror.has_changes() && deadline.is_none() {
                        deadline = Some((last_flush + COALESCE).max(Instant::now()));
                    }
                }
                if std::mem::take(&mut probe_again) {
                    probing = true;
                    spawn_probe(&client, &session, &probe_tx);
                }
            }
            event = events.recv() => match event {
                Ok(event) => {
                    if let Some(code) = exit_of(&event, &session) {
                        // The final grid first, so the phone ends on it.
                        if mirror.has_changes() && !needs_snapshot {
                            let _ = out.try_send(frame("diff", &mirror.diff()));
                        }
                        let _ = tokio::time::timeout(
                            Duration::from_secs(5),
                            out.send(frame("exit", &json!({ "code": code }))),
                        )
                        .await;
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

fn spawn_probe(client: &Arc<DaemonClient>, session: &SessionId, done: &mpsc::Sender<Option<i64>>) {
    let client = Arc::clone(client);
    let session = session.clone();
    let done = done.clone();
    tokio::spawn(async move {
        let _ = done.send(history_size(&client, &session).await).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::grid::ChangedRow;

    fn cell(ch: char) -> GridCell {
        GridCell::new(
            ch as u32,
            TermColor::Default,
            TermColor::DefaultInverted,
            TermStyle::empty(),
        )
    }

    fn row(text: &str, cols: usize) -> Vec<GridCell> {
        let mut cells: Vec<GridCell> = text.chars().map(cell).collect();
        cells.resize(cols, GridCell::BLANK);
        cells
    }

    #[test]
    fn a_line_is_styled_runs_without_trailing_blanks() {
        let palette = TerminalPalette::default();
        let mut cells = row("ab  cd", 10);
        cells[0].style = TermStyle::BOLD | TermStyle::ITALIC;
        cells[1].style = TermStyle::BOLD | TermStyle::ITALIC;
        cells[4].fg = TermColor::Ansi(2);
        cells[5].fg = TermColor::Rgb(1, 2, 255);
        cells[9].style = TermStyle::SOFT_WRAP;
        assert_eq!(
            line_json(&cells, &RowMetadata::default(), &palette),
            json!({
                "runs": [
                    {"t": "ab", "fg": null, "bg": null, "a": 5},
                    {"t": "  ", "fg": null, "bg": null, "a": 0},
                    {"t": "c", "fg": 2, "bg": null, "a": 0},
                    {"t": "d", "fg": "#0102ff", "bg": null, "a": 0},
                ],
                "wrapped": true,
            })
        );
        assert_eq!(
            line_json(&row("", 8), &RowMetadata::default(), &palette),
            json!({"runs": [], "wrapped": false})
        );
    }

    #[test]
    fn wide_characters_and_graphemes_stay_whole() {
        let palette = TerminalPalette::default();
        let mut cells = row("界 ex", 6);
        cells[1] = GridCell::new(
            0,
            TermColor::Default,
            TermColor::DefaultInverted,
            TermStyle::WIDE_SPACER,
        );
        let metadata = RowMetadata {
            graphemes: vec![(2, "e\u{301}".into())],
            ..RowMetadata::default()
        };
        let line = line_json(&cells, &metadata, &palette);
        assert_eq!(line["runs"][0]["t"], "界e\u{301}x");
    }

    #[test]
    fn blank_cells_with_a_background_are_kept_and_inverted_defaults_resolve() {
        let palette = TerminalPalette::default();
        let mut cells = row("", 3);
        cells[0].bg = TermColor::Ansi(4);
        cells[1].fg = TermColor::DefaultInverted;
        cells[1].bg = TermColor::Default;
        cells[1].scalar = 'x' as u32;
        let line = line_json(&cells, &RowMetadata::default(), &palette);
        assert_eq!(
            line["runs"],
            json!([
                {"t": " ", "fg": null, "bg": 4, "a": 0},
                {"t": "x", "fg": "#121318", "bg": "#e6e6e6", "a": 0},
            ])
        );
    }

    #[test]
    fn attributes_follow_the_contract_bits() {
        assert_eq!(attributes(TermStyle::DIM), 2);
        assert_eq!(attributes(TermStyle::UNDERLINE), 8);
        assert_eq!(attributes(TermStyle::INVERSE), 16);
        assert_eq!(attributes(TermStyle::CROSSED_OUT), 32);
        assert_eq!(attributes(TermStyle::INVISIBLE), 64);
        assert_eq!(attributes(TermStyle::SOFT_WRAP | TermStyle::BLINK), 0);
    }

    pub(crate) fn update(full: bool, rows: &[(u16, &str)]) -> GridUpdate {
        GridUpdate {
            cols: 10,
            rows: 4,
            cursor_col: 1,
            cursor_row: 2,
            cursor_visible: true,
            is_full_snapshot: full,
            changed_rows: rows
                .iter()
                .map(|(y, text)| ChangedRow::new(*y, row(text, 10)))
                .collect(),
            cursor_style: Default::default(),
            history: None,
        }
    }

    #[test]
    fn the_mirror_reports_only_changed_rows() {
        let mut mirror = Mirror::default();
        mirror.apply(update(true, &[(0, "one"), (1, "two")]));
        let snapshot = mirror.snapshot();
        assert_eq!(snapshot["rows"], 4);
        assert_eq!(snapshot["lines"].as_array().unwrap().len(), 4);
        assert_eq!(
            snapshot["cursor"],
            json!({"x":1,"y":2,"visible":true,"shape":"block"})
        );
        mirror.apply(update(false, &[(3, "three")]));
        mirror.apply(update(false, &[(3, "four")]));
        let diff = mirror.diff();
        assert_eq!(diff["lines"].as_object().unwrap().len(), 1);
        assert_eq!(diff["lines"]["3"]["runs"][0]["t"], "four");
        assert!(diff["seq"].as_u64() > snapshot["seq"].as_u64());
        assert!(!mirror.has_changes());
    }

    fn digests_of(mirror: &mut Mirror) -> Vec<u32> {
        mirror.snapshot()["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(line_digest)
            .collect()
    }

    #[test]
    fn a_resume_names_only_the_rows_that_changed_and_never_trusts_a_bad_token() {
        let mut mirror = Mirror::default();
        mirror.apply(update(true, &[(0, "one"), (1, "two"), (2, "three")]));
        mirror.set_modes(Modes::default());
        let digests = digests_of(&mut mirror);
        let token = format!(
            "10x4{}",
            digests.iter().map(|d| format!(".{d:x}")).collect::<String>()
        );
        let resume = Resume::parse(&token).unwrap();

        // Unchanged since the client last saw it: no rows at all.
        let quiet = mirror.resume(&resume).unwrap();
        assert_eq!(quiet["lines"], json!({}));
        assert_eq!(quiet["cols"], 10);
        assert!(quiet["palette"].is_object());

        mirror.apply(update(false, &[(1, "TWO")]));
        let delta = mirror.resume(&resume).unwrap();
        assert_eq!(delta["lines"].as_object().unwrap().len(), 1);
        assert_eq!(delta["lines"]["1"]["runs"][0]["t"], "TWO");
        assert!(!mirror.has_changes(), "the resume frame covers pending rows");

        // Another width or height means a snapshot.
        let narrower = Resume::parse(&token.replacen("10x", "9x", 1)).unwrap();
        assert!(mirror.resume(&narrower).is_none());
        let shorter = Resume::parse("10x3.0.0.0").unwrap();
        assert!(mirror.resume(&shorter).is_none());

        for bad in ["", "10", "10x2.zz.0", "10x3.0.0", "x1.0", "10x301"] {
            assert_eq!(Resume::parse(bad), None, "{bad:?}");
        }
    }

    /// The page computes FNV-1a over `JSON.stringify` of the parsed line;
    /// these are the values its implementation gives for the same lines.
    #[test]
    fn line_digests_match_the_pages_algorithm() {
        assert_eq!(line_digest(&json!({"runs": [], "wrapped": false})), {
            "{\"runs\":[],\"wrapped\":false}"
                .bytes()
                .fold(0x811c_9dc5_u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
        });
        // Serialised key order and escapes are what JSON.stringify writes.
        assert_eq!(
            serde_json::to_string(&json!({"runs": [{"t": "a\u{1}\"é", "fg": null, "bg": 3, "a": 1}], "wrapped": true})).unwrap(),
            r#"{"runs":[{"a":1,"bg":3,"fg":null,"t":"a\u0001\"é"}],"wrapped":true}"#
        );
    }

    fn event_of(frame: &str) -> (&str, Value) {
        let (head, data) = frame.split_once('\n').unwrap();
        let data = data.strip_prefix("data: ").unwrap().trim_end();
        (
            head.strip_prefix("event: ").unwrap(),
            serde_json::from_str(data).unwrap(),
        )
    }

    /// A phone that stops reading must not queue every diff: once the queue
    /// is full they are discarded, and the first free slot carries a fresh
    /// snapshot of the latest grid instead.
    #[tokio::test]
    async fn a_slow_phone_gets_a_fresh_snapshot_instead_of_stale_diffs() {
        use diri_proto::Method;
        let engine = crate::testing::fake_engine(|method, _| match method {
            Method::SESSION_READ_SCROLLBACK_CELLS => json!({
                "payload": "", "firstRow": 0, "rowCount": 0, "totalRows": 4,
                "liveStartRow": 0, "cols": 10, "contentSeq": 1,
            }),
            _ => json!({}),
        })
        .await;
        let (chunks, source) = mpsc::channel(1);
        let (out, mut frames) = mpsc::channel(OUT_QUEUE);
        let (_events_tx, events) = broadcast::channel(4);
        let mut mirror = Mirror::default();
        mirror.apply(update(true, &[(0, "seed")]));
        mirror.set_modes(Modes::default());
        let task = tokio::spawn(pump(
            source,
            mirror,
            Arc::clone(&engine.client),
            SessionId("s_a".into()),
            events,
            out,
            None,
        ));
        // Far more frames than the queue holds, each in its own window, and
        // nobody reading.
        for index in 0..(OUT_QUEUE * 4) {
            let text = format!("frame {index}");
            chunks
                .send(TerminalChunk::Grid(update(false, &[(1, &text)])))
                .await
                .unwrap();
            tokio::time::sleep(COALESCE * 2).await;
        }
        let mut received = Vec::new();
        while let Ok(frame) = tokio::time::timeout(Duration::from_millis(200), frames.recv()).await
        {
            received.push(frame.unwrap());
        }
        let events: Vec<(&str, Value)> = received.iter().map(|f| event_of(f)).collect();
        assert!(
            events.len() <= OUT_QUEUE + 2,
            "a stalled phone was queued {} frames",
            events.len()
        );
        assert_eq!(events[0].0, "snapshot");
        let (last, body) = events.last().unwrap();
        assert_eq!(*last, "snapshot", "the reseed follows the dropped diffs");
        let newest = format!("frame {}", OUT_QUEUE * 4 - 1);
        assert_eq!(body["lines"][1]["runs"][0]["t"], newest.as_str());
        assert_eq!(body["lines"][0]["runs"][0]["t"], "seed");
        task.abort();
    }

    /// A burst inside one window reaches the phone as one diff.
    #[tokio::test]
    async fn a_burst_is_coalesced_into_one_diff() {
        let engine = crate::testing::fake_engine(|_, _| json!({})).await;
        let (chunks, source) = mpsc::channel(8);
        let (out, mut frames) = mpsc::channel(OUT_QUEUE);
        let (_events_tx, events) = broadcast::channel(4);
        let mut mirror = Mirror::default();
        mirror.apply(update(true, &[]));
        // Alternate screen: no history-size reads to wait for.
        mirror.set_modes(Modes {
            alt_screen: true,
            ..Modes::default()
        });
        let task = tokio::spawn(pump(
            source,
            mirror,
            Arc::clone(&engine.client),
            SessionId("s_a".into()),
            events,
            out,
            None,
        ));
        assert_eq!(event_of(&frames.recv().await.unwrap()).0, "snapshot");
        // Spend the leading edge, then send a burst inside the next window.
        chunks
            .send(TerminalChunk::Grid(update(false, &[(0, "a")])))
            .await
            .unwrap();
        assert_eq!(event_of(&frames.recv().await.unwrap()).0, "diff");
        for row in 1..4 {
            chunks
                .send(TerminalChunk::Grid(update(false, &[(row, "b")])))
                .await
                .unwrap();
        }
        let frame = frames.recv().await.unwrap();
        let (name, body) = event_of(&frame);
        assert_eq!(name, "diff");
        assert_eq!(
            body["lines"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["1", "2", "3"]
        );
        task.abort();
    }
}
