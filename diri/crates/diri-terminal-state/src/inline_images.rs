//! Inline images: the Kitty graphics protocol (`APC G`) and iTerm2's
//! `OSC 1337 File=`.
//!
//! The emulator has no image support and silently drops both sequences, so
//! they are cut out of the byte stream here, before the parser sees them.
//! Each one is handled at its exact place in the stream: the bytes before it
//! are parsed first, so the cursor it is placed at, and the order of any
//! reply against the emulator's own replies, are what a real terminal has.
//!
//! A placement does not live beside the grid. The cells an image covers are
//! written as blanks carrying an [`ImageAnchor`] hyperlink, one per row, so
//! the image scrolls into history, is cleared by erases and overwritten by
//! later output, and leaves history with its rows, exactly as text does. The
//! pixels sit in a small per-terminal store with a hard byte budget that
//! evicts the oldest image; a client reads them by key.
//!
//! Off unless the owner opts in ([`HeadlessScreen::with_inline_images`]).
//! The local Engine does for its own terminals. A remote Holder does from
//! `inline-images-v1` on, so a Holder built before it shows no support. A
//! `inline-images-v1` Holder also called
//! [`HeadlessScreen::answering_image_queries_only`] and answered only the
//! probes that advertise images; from `query-replies-v1` on it answers every
//! query, as the local Engine does.
//!
//! Not supported yet, and answered with an error where the protocol has one:
//! Kitty file, temp-file and shared-memory transmission, Unicode placeholders,
//! animation frames, source rectangles and z-index layering; iTerm2 stretched
//! (`preserveAspectRatio=0`) images; Sixel.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Hyperlink};
use alacritty_terminal::vte::ansi::Handler;
use diri_proto::inline_image::{ImageAnchor, InlineImageFormat, MAX_IMAGE_BYTES};

use crate::HeadlessScreen;

/// Hard budget for one terminal's stored images. Placing a new image evicts
/// the oldest until it fits; evicted placements stay as blank cells.
pub const STORE_BUDGET_BYTES: usize = 32 * 1024 * 1024;
/// Largest width or height accepted, in pixels.
const MAX_DIMENSION: u32 = 10_000;
/// Decoded RGBA size a client must be able to afford for one image.
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;
/// Tallest placement, in rows; also bounds the work one sequence can cause.
const MAX_PLACEMENT_ROWS: u32 = 1_000;
/// Widest placement box, in columns.
const MAX_PLACEMENT_COLS: u32 = 1_000;
/// A sequence's encoded body may not exceed the base64 of the largest image.
const MAX_SEQUENCE_BYTES: usize = MAX_IMAGE_BYTES / 3 * 4 + 64 * 1024;

/// Device pixels of one cell until a client reports its own: a 13 pt
/// monospace face at 2x. Only the size images are given in cells depends on
/// it; clients fit each image into its cell box at whatever cell size they
/// draw.
pub const DEFAULT_CELL_PIXELS: (u16, u16) = diri_proto::terminal::NOMINAL_CELL_PIXELS;

const KITTY_PREFIX: &[u8] = b"\x1b_G";
const ITERM_PREFIX: &[u8] = b"\x1b]1337;";
const PREFIXES: [&[u8]; 2] = [KITTY_PREFIX, ITERM_PREFIX];
/// Both openers are two-byte pairs no other output uses, so a SIMD pair
/// search skips the escapes of ordinary colored and cursor-addressed output
/// instead of stopping at each one.
static FINDERS: std::sync::LazyLock<[memchr::memmem::Finder<'static>; 2]> =
    std::sync::LazyLock::new(|| PREFIXES.map(memchr::memmem::Finder::new));

/// One stored image. Cloning shares the bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredImage {
    pub format: InlineImageFormat,
    /// Raw formats only: the bytes are zlib-compressed (Kitty `o=z`).
    pub compressed: bool,
    pub width: u32,
    pub height: u32,
    pub data: Arc<[u8]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SequenceKind {
    Kitty,
    Iterm,
}

/// An image sequence whose body is still arriving.
struct Collecting {
    kind: SequenceKind,
    body: Vec<u8>,
    /// The body outgrew [`MAX_SEQUENCE_BYTES`]; the rest is skipped.
    overflow: bool,
    /// The previous chunk ended on the ESC of a possible ST.
    pending_esc: bool,
}

/// A chunked Kitty transmission (`m=1`) still receiving data.
struct KittyUpload {
    command: KittyCommand,
    data: Vec<u8>,
    error: Option<&'static str>,
}

/// An iTerm2 `MultipartFile` still receiving `FilePart`s.
struct ItermUpload {
    args: ItermArgs,
    data: Vec<u8>,
    overflow: bool,
}

pub(crate) struct InlineImages {
    store: ImageStore,
    collecting: Option<Collecting>,
    /// The end of the previous chunk, held back because it may begin an
    /// image sequence.
    held: Vec<u8>,
    kitty_upload: Option<KittyUpload>,
    iterm_upload: Option<ItermUpload>,
    pub(crate) cell_pixels: (u16, u16),
}

impl InlineImages {
    pub(crate) fn new() -> Self {
        Self {
            store: ImageStore::new(),
            collecting: None,
            held: Vec::new(),
            kitty_upload: None,
            iterm_upload: None,
            cell_pixels: DEFAULT_CELL_PIXELS,
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<StoredImage> {
        self.store.get(key)
    }

    pub(crate) fn stored_bytes(&self) -> usize {
        self.store.bytes
    }
}

impl HeadlessScreen {
    /// Opt in only where a client renders this terminal's grid: the local
    /// Engine, or a capable remote Holder. See the
    /// [module docs](crate::inline_images).
    #[must_use]
    pub fn with_inline_images(mut self) -> Self {
        self.images = Some(Box::new(InlineImages::new()));
        self
    }

    /// Keep only the replies that report inline-image support: Kitty graphics
    /// responses, `OSC 1337 ReportCellSize`, `CSI 14 t`/`CSI 16 t` and primary
    /// device attributes (which a support probe waits for). Every other query
    /// stays unanswered and nothing accumulates. This was the remote Holder's
    /// setup from `inline-images-v1` until `query-replies-v1`; see the
    /// [module docs](self).
    #[must_use]
    pub fn answering_image_queries_only(mut self) -> Self {
        self.image_replies_only = true;
        self
    }

    /// Whether this terminal intercepts and stores inline images.
    #[must_use]
    pub fn inline_images_enabled(&self) -> bool {
        self.images.is_some()
    }

    /// The stored image an anchor key names. `None` once it is evicted or
    /// deleted, and always without [`Self::with_inline_images`].
    #[must_use]
    pub fn inline_image(&self, key: &str) -> Option<StoredImage> {
        self.images.as_ref()?.get(key)
    }

    /// Bytes currently held by the image store.
    #[must_use]
    pub fn inline_image_bytes(&self) -> usize {
        self.images
            .as_ref()
            .map_or(0, |images| images.stored_bytes())
    }

    /// The device-pixel size of one cell as the client draws it. Images given
    /// no size in cells are sized from it, and pixel-size queries report it.
    pub fn set_cell_pixels(&mut self, width: u16, height: u16) {
        if let Some(images) = &mut self.images
            && width > 0
            && height > 0
        {
            images.cell_pixels = (width, height);
        }
    }

    pub(crate) fn cell_pixels(&self) -> Option<(u16, u16)> {
        self.images.as_ref().map(|images| images.cell_pixels)
    }

    /// Parses `bytes`, cutting image sequences out of the stream. Replies to
    /// sequences that end inside the first `historical_bytes` are dropped.
    /// `csi_only`: every escape in `bytes` is a CSI (see `only_csi_escapes`),
    /// so none can open an image sequence.
    pub(crate) fn advance_with_images(
        &mut self,
        bytes: &[u8],
        historical_bytes: usize,
        csi_only: bool,
    ) {
        let Some(images) = self.images.as_mut() else {
            self.parser.advance(&mut self.term, bytes);
            return;
        };
        // Fast path: nothing pending and nothing that could start a sequence.
        if images.held.is_empty() && images.collecting.is_none() && csi_only {
            self.parser.advance(&mut self.term, bytes);
            return;
        }
        let held = std::mem::take(&mut images.held);
        let joined;
        let (input, historical) = if held.is_empty() {
            (bytes, historical_bytes)
        } else {
            let historical = if historical_bytes > 0 {
                historical_bytes + held.len()
            } else {
                0
            };
            joined = [held.as_slice(), bytes].concat();
            (joined.as_slice(), historical)
        };
        let mut position = 0;
        while position < input.len() {
            let images = self.images.as_mut().expect("images");
            if let Some(collecting) = images.collecting.as_mut() {
                match collect(collecting, &input[position..]) {
                    Collected::Complete(consumed) => {
                        position += consumed;
                        let collecting = images.collecting.take().expect("collecting");
                        self.dispatch(collecting, position <= historical);
                    }
                    Collected::Aborted(consumed) => {
                        // An ESC that does not end the string starts a new
                        // sequence: hand it back to the parser.
                        images.collecting = None;
                        position += consumed;
                    }
                    Collected::Partial => position = input.len(),
                }
                continue;
            }
            match next_sequence(&input[position..]) {
                Next::None => {
                    self.parser.advance(&mut self.term, &input[position..]);
                    position = input.len();
                }
                Next::Partial(start) => {
                    self.parser
                        .advance(&mut self.term, &input[position..position + start]);
                    let images = self.images.as_mut().expect("images");
                    images.held.extend_from_slice(&input[position + start..]);
                    position = input.len();
                }
                Next::Found(start, prefix) => {
                    self.parser
                        .advance(&mut self.term, &input[position..position + start]);
                    position += start + prefix.len();
                    let kind = if prefix == KITTY_PREFIX {
                        SequenceKind::Kitty
                    } else {
                        SequenceKind::Iterm
                    };
                    self.images.as_mut().expect("images").collecting = Some(Collecting {
                        kind,
                        body: Vec::new(),
                        overflow: false,
                        pending_esc: false,
                    });
                }
            }
        }
    }

    /// Bytes held back or collected across reads; bounded by
    /// [`MAX_SEQUENCE_BYTES`] plus one prefix.
    #[cfg(test)]
    pub(crate) fn image_carry_bytes(&self) -> usize {
        self.images.as_ref().map_or(0, |images| {
            images.held.len()
                + images
                    .collecting
                    .as_ref()
                    .map_or(0, |collecting| collecting.body.capacity())
        })
    }

    fn reply(&mut self, historical: bool, reply: &[u8]) {
        if historical {
            return;
        }
        // The emulator's replies to everything before this sequence are
        // still queued as events; they go first.
        self.drain_events();
        self.replies.extend_from_slice(reply);
    }

    fn dispatch(&mut self, collecting: Collecting, historical: bool) {
        // Bytes a synchronized update holds back were written before this
        // image; they must land first.
        if self.parser.sync_timeout().sync_timeout().is_some() {
            self.parser.stop_sync(&mut self.term);
        }
        match collecting.kind {
            SequenceKind::Kitty => self.kitty(&collecting.body, collecting.overflow, historical),
            SequenceKind::Iterm => self.iterm(&collecting.body, collecting.overflow, historical),
        }
    }

    // MARK: Kitty

    fn kitty(&mut self, body: &[u8], overflow: bool, historical: bool) {
        let (control, payload) = match body.iter().position(|&byte| byte == b';') {
            Some(split) => (&body[..split], &body[split + 1..]),
            None => (body, &[][..]),
        };
        let parsed = KittyCommand::parse(control);
        let images = self.images.as_mut().expect("images");
        // A chunk of a transmission in progress carries only `m` (and `q`).
        if let Some(mut upload) = images.kitty_upload.take() {
            if overflow {
                upload.error.get_or_insert("EFBIG:image is too large");
            } else if upload.error.is_none()
                && !decode_base64_into(payload, &mut upload.data, MAX_IMAGE_BYTES)
            {
                upload.error = Some(if upload.data.len() >= MAX_IMAGE_BYTES {
                    "EFBIG:image is too large"
                } else {
                    "EINVAL:invalid base64 data"
                });
                upload.data = Vec::new();
            }
            if parsed.more {
                images.kitty_upload = Some(upload);
                return;
            }
            if parsed.quiet > upload.command.quiet {
                upload.command.quiet = parsed.quiet;
            }
            let KittyUpload {
                command,
                data,
                error,
            } = upload;
            self.kitty_finish(command, data, error, historical);
            return;
        }
        let command = parsed;
        if command.action == b'd' {
            self.kitty_delete(&command);
            return;
        }
        if command.action == b'p' {
            self.kitty_put(&command, historical);
            return;
        }
        if !matches!(command.action, b't' | b'T' | b'q') {
            return;
        }
        let mut data = Vec::new();
        let mut error = None;
        if overflow {
            error = Some("EFBIG:image is too large");
        } else if !decode_base64_into(payload, &mut data, MAX_IMAGE_BYTES) {
            error = Some(if data.len() >= MAX_IMAGE_BYTES {
                "EFBIG:image is too large"
            } else {
                "EINVAL:invalid base64 data"
            });
            data = Vec::new();
        }
        if command.more {
            self.images.as_mut().expect("images").kitty_upload = Some(KittyUpload {
                command,
                data,
                error,
            });
            return;
        }
        self.kitty_finish(command, data, error, historical);
    }

    fn kitty_finish(
        &mut self,
        command: KittyCommand,
        data: Vec<u8>,
        error: Option<&'static str>,
        historical: bool,
    ) {
        if command.id != 0 && command.number != 0 {
            self.kitty_reply(&command, 0, "EINVAL:i and I are exclusive", historical);
            return;
        }
        let image = match error.map_or_else(|| command.image(data), Err) {
            Ok(image) => image,
            Err(error) => {
                self.kitty_reply(&command, command.id, error, historical);
                return;
            }
        };
        if command.action == b'q' {
            self.kitty_reply(&command, command.id, "OK", historical);
            return;
        }
        let images = self.images.as_mut().expect("images");
        let kitty_id = if command.number != 0 {
            images.store.kitty_number(command.number)
        } else {
            command.id
        };
        let internal = images
            .store
            .insert(image, (kitty_id != 0).then_some(kitty_id));
        self.kitty_reply(&command, kitty_id, "OK", historical);
        if command.action == b'T' {
            self.kitty_place(internal, &command);
        }
    }

    fn kitty_put(&mut self, command: &KittyCommand, historical: bool) {
        let images = self.images.as_mut().expect("images");
        let kitty_id = if command.number != 0 {
            images.store.kitty_by_number(command.number)
        } else {
            Some(command.id)
        };
        match kitty_id.and_then(|id| images.store.kitty_internal(id)) {
            Some((kitty_id, internal)) => {
                self.kitty_reply(command, kitty_id, "OK", historical);
                self.kitty_place(internal, command);
            }
            None => self.kitty_reply(command, command.id, "ENOENT:no such image", historical),
        }
    }

    fn kitty_place(&mut self, internal: u32, command: &KittyCommand) {
        if command.unicode_placeholder {
            return;
        }
        let Some((width, height)) = self
            .images
            .as_ref()
            .and_then(|images| images.store.dimensions(internal))
        else {
            return;
        };
        let request = (
            (command.cols > 0).then_some(command.cols),
            (command.rows > 0).then_some(command.rows),
        );
        self.place(internal, (width, height), request, false, !command.no_move);
    }

    fn kitty_delete(&mut self, command: &KittyCommand) {
        let images = self.images.as_mut().expect("images");
        let free = command.delete.is_ascii_uppercase();
        let target: Option<u32> = match command.delete.to_ascii_lowercase() {
            b'a' => None,
            b'i' | b'p' => {
                let Some(found) = images.store.kitty_internal(command.id) else {
                    return;
                };
                Some(found.1)
            }
            b'n' => {
                let Some(found) = images
                    .store
                    .kitty_by_number(command.number)
                    .and_then(|id| images.store.kitty_internal(id))
                else {
                    return;
                };
                Some(found.1)
            }
            b'c' => {
                let point = self.term.grid().cursor.point;
                let Some(internal) = self.anchor_at(point.line, point.column) else {
                    return;
                };
                Some(internal)
            }
            // Deletes by row, column, cell range or z-index: no placement
            // geometry is kept to answer them.
            _ => return,
        };
        let store = &self.images.as_ref().expect("images").store;
        let positions = self.visible_anchor_cells(&|internal| match target {
            Some(target) => internal == target,
            None => store.is_kitty(internal),
        });
        self.blank_cells(&positions);
        if free {
            let store = &mut self.images.as_mut().expect("images").store;
            match target {
                Some(target) => store.remove(target),
                None => store.remove_kitty(),
            }
        }
    }

    fn kitty_reply(&mut self, command: &KittyCommand, id: u32, message: &str, historical: bool) {
        let ok = message == "OK";
        if (command.id == 0 && command.number == 0)
            || (ok && command.quiet >= 1)
            || command.quiet >= 2
        {
            return;
        }
        let mut reply = format!("\x1b_Gi={id}");
        if command.number != 0 {
            reply.push_str(&format!(",I={}", command.number));
        }
        if command.placement != 0 {
            reply.push_str(&format!(",p={}", command.placement));
        }
        reply.push(';');
        reply.push_str(message);
        reply.push_str("\x1b\\");
        self.reply(historical, reply.as_bytes());
    }

    // MARK: iTerm2

    fn iterm(&mut self, body: &[u8], overflow: bool, historical: bool) {
        let (command, rest) = match body.iter().position(|&byte| byte == b'=') {
            Some(split) => (&body[..split], &body[split + 1..]),
            None => (body, &[][..]),
        };
        let images = self.images.as_mut().expect("images");
        match command {
            b"File" => {
                images.iterm_upload = None;
                let (args, payload) = match rest.iter().position(|&byte| byte == b':') {
                    Some(split) => (&rest[..split], &rest[split + 1..]),
                    None => (rest, &[][..]),
                };
                let args = ItermArgs::parse(args);
                if overflow || !args.inline {
                    return;
                }
                let mut data = Vec::new();
                if decode_base64_into(payload, &mut data, MAX_IMAGE_BYTES) {
                    self.iterm_show(&args, data);
                }
            }
            b"MultipartFile" => {
                images.iterm_upload = Some(ItermUpload {
                    args: ItermArgs::parse(rest),
                    data: Vec::new(),
                    overflow: false,
                });
            }
            b"FilePart" => {
                if let Some(upload) = images.iterm_upload.as_mut()
                    && !upload.overflow
                    && (overflow || !decode_base64_into(rest, &mut upload.data, MAX_IMAGE_BYTES))
                {
                    upload.overflow = true;
                    upload.data = Vec::new();
                }
            }
            b"FileEnd" => {
                if let Some(upload) = images.iterm_upload.take()
                    && !upload.overflow
                    && upload.args.inline
                {
                    self.iterm_show(&upload.args, upload.data);
                }
            }
            b"ReportCellSize" => {
                let (width, height) = images.cell_pixels;
                self.reply(
                    historical,
                    format!("\x1b]1337;ReportCellSize={height}.0;{width}.0;1.0\x1b\\").as_bytes(),
                );
            }
            // Other iTerm2 commands are no-ops here, as they were before.
            _ => {}
        }
    }

    fn iterm_show(&mut self, args: &ItermArgs, data: Vec<u8>) {
        let Some((format, width, height)) = sniff(&data) else {
            return;
        };
        if !dimensions_allowed(width, height) {
            return;
        }
        let (cell_width, cell_height) = self.images.as_ref().expect("images").cell_pixels;
        let cols = self.geometry.cols as u32;
        let rows = self.geometry.rows as u32;
        let resolve = |size: ItermSize, cell: u16, screen: u32| match size {
            ItermSize::Auto => None,
            ItermSize::Cells(count) => Some(count),
            ItermSize::Pixels(pixels) => Some(pixels.div_ceil(u32::from(cell))),
            ItermSize::Percent(percent) => Some((screen * percent).div_ceil(100)),
        };
        let request = (
            resolve(args.width, cell_width, cols).filter(|&count| count > 0),
            resolve(args.height, cell_height, rows).filter(|&count| count > 0),
        );
        let image = StoredImage {
            format,
            compressed: false,
            width,
            height,
            data: data.into(),
        };
        let internal = self
            .images
            .as_mut()
            .expect("images")
            .store
            .insert(image, None);
        // iTerm2 shrinks an image given no size to fit the window.
        let fit_height = request == (None, None);
        self.place(
            internal,
            (width, height),
            request,
            fit_height,
            !args.do_not_move_cursor,
        );
    }

    // MARK: Placement

    /// Writes `internal`'s anchors over the cells its box covers, starting at
    /// the cursor. Moving the cursor scrolls like printed lines do and leaves
    /// it after the image's last column on its last row, as Kitty does;
    /// otherwise rows below the screen are dropped and the cursor stays.
    fn place(
        &mut self,
        internal: u32,
        pixels: (u32, u32),
        request: (Option<u32>, Option<u32>),
        fit_height: bool,
        move_cursor: bool,
    ) {
        let images = self.images.as_ref().expect("images");
        let key = images.store.key(internal);
        let cell = images.cell_pixels;
        let columns = self.geometry.cols;
        let screen_lines = self.geometry.rows;
        let cursor = self.term.grid().cursor.point;
        let col0 = cursor.column.0.min(columns - 1);
        let available = (columns - col0) as u32;
        let (cols, rows) = cell_box(
            pixels,
            request,
            cell,
            available,
            fit_height.then_some(screen_lines as u32),
        );
        let saved_template = self.term.grid().cursor.template.clone();
        let saved_wrap = self.term.grid().cursor.input_needs_wrap;
        let written = usize::from(cols).min(columns - col0);
        let mut line = cursor.line;
        for row in 0..rows {
            if row > 0 {
                if move_cursor {
                    Handler::linefeed(&mut self.term);
                    line = self.term.grid().cursor.point.line;
                } else {
                    line = Line(line.0 + 1);
                    if line.0 >= screen_lines as i32 {
                        break;
                    }
                }
            }
            let anchor = ImageAnchor {
                key: key.clone(),
                row,
                col: col0 as u16,
                cols,
                rows,
            };
            let mut template = Cell::default();
            template.set_hyperlink(Some(Hyperlink::new(None::<&str>, anchor.to_uri())));
            let grid = self.term.grid_mut();
            grid.cursor.template = template;
            grid.cursor.point.line = line;
            grid.cursor.point.column = Column(col0);
            grid.cursor.input_needs_wrap = false;
            for _ in 0..written {
                Handler::input(&mut self.term, ' ');
            }
        }
        let grid = self.term.grid_mut();
        grid.cursor.template = saved_template;
        if !move_cursor {
            grid.cursor.point = cursor;
            grid.cursor.input_needs_wrap = saved_wrap;
        }
    }

    /// The image whose anchor covers a visible cell.
    fn anchor_at(&self, line: Line, column: Column) -> Option<u32> {
        let grid = self.term.grid();
        if line.0 < 0 || line.0 >= grid.screen_lines() as i32 || column.0 >= grid.columns() {
            return None;
        }
        let link = grid[line][column].hyperlink()?;
        let anchor = ImageAnchor::parse(link.uri())?;
        self.images.as_ref()?.store.internal(&anchor.key)
    }

    fn visible_anchor_cells(&self, matches: &dyn Fn(u32) -> bool) -> Vec<(Line, Column)> {
        let Some(images) = self.images.as_ref() else {
            return Vec::new();
        };
        let grid = self.term.grid();
        let mut positions = Vec::new();
        for line in 0..grid.screen_lines() {
            let line = Line(line as i32);
            let row = &grid[line];
            for column in 0..grid.columns() {
                let column = Column(column);
                let Some(link) = row[column].hyperlink() else {
                    continue;
                };
                if let Some(anchor) = ImageAnchor::parse(link.uri())
                    && images.store.internal(&anchor.key).is_some_and(matches)
                {
                    positions.push((line, column));
                }
            }
        }
        positions
    }

    /// Rewrites cells as plain blanks through the emulator, so the change is
    /// damage-tracked like output. The cursor is left exactly as it was.
    fn blank_cells(&mut self, positions: &[(Line, Column)]) {
        if positions.is_empty() {
            return;
        }
        let grid = self.term.grid();
        let saved_point = grid.cursor.point;
        let saved_wrap = grid.cursor.input_needs_wrap;
        let saved_template = grid.cursor.template.clone();
        self.term.grid_mut().cursor.template = Cell::default();
        for &(line, column) in positions {
            let grid = self.term.grid_mut();
            grid.cursor.point.line = line;
            grid.cursor.point.column = column;
            grid.cursor.input_needs_wrap = false;
            Handler::input(&mut self.term, ' ');
        }
        let grid = self.term.grid_mut();
        grid.cursor.template = saved_template;
        grid.cursor.point = saved_point;
        grid.cursor.input_needs_wrap = saved_wrap;
    }
}

// MARK: Cell box

/// The cell box an image of `pixels` occupies. An explicit size wins; one
/// explicit side takes the other from the aspect ratio; with neither, the
/// image keeps its pixel size. An image wider than `available` columns, or
/// taller than `max_rows` when given, shrinks to fit (never grows).
fn cell_box(
    (width, height): (u32, u32),
    request: (Option<u32>, Option<u32>),
    (cell_width, cell_height): (u16, u16),
    available: u32,
    max_rows: Option<u32>,
) -> (u16, u16) {
    let cell_width = f64::from(cell_width.max(1));
    let cell_height = f64::from(cell_height.max(1));
    let width = f64::from(width.max(1));
    let height = f64::from(height.max(1));
    let (cols, rows) = match request {
        (Some(cols), Some(rows)) => (f64::from(cols), f64::from(rows)),
        (Some(cols), None) => {
            let cols = f64::from(cols);
            (
                cols,
                (cols * cell_width * height / width / cell_height).ceil(),
            )
        }
        (None, Some(rows)) => {
            let rows = f64::from(rows);
            (
                (rows * cell_height * width / height / cell_width).ceil(),
                rows,
            )
        }
        (None, None) => {
            let mut scale = 1.0_f64;
            scale = scale.min(f64::from(available.max(1)) * cell_width / width);
            if let Some(max_rows) = max_rows {
                scale = scale.min(f64::from(max_rows.max(1)) * cell_height / height);
            }
            (
                (width * scale / cell_width).ceil(),
                (height * scale / cell_height).ceil(),
            )
        }
    };
    let clamp = |value: f64, max: u32| value.clamp(1.0, f64::from(max)) as u16;
    (
        clamp(cols, MAX_PLACEMENT_COLS),
        clamp(rows, MAX_PLACEMENT_ROWS),
    )
}

fn dimensions_allowed(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= MAX_DIMENSION
        && height <= MAX_DIMENSION
        && u64::from(width) * u64::from(height) * 4 <= MAX_DECODED_BYTES
}

// MARK: Store

struct ImageStore {
    /// Distinguishes this terminal's keys from a previous one's (a restarted
    /// Engine replays the same session), so client caches cannot collide.
    nonce: u32,
    next_id: u32,
    images: HashMap<u32, (StoredImage, Option<u32>)>,
    /// Oldest first.
    order: VecDeque<u32>,
    bytes: usize,
    /// Kitty image id → internal id.
    kitty: HashMap<u32, u32>,
    /// Kitty image number (`I`) → the id assigned to it.
    kitty_numbers: HashMap<u32, u32>,
    next_kitty_id: u32,
}

impl ImageStore {
    fn new() -> Self {
        static SERIAL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nonce = (time.as_nanos() as u32)
            ^ (time.as_secs() as u32).rotate_left(16)
            ^ std::process::id().rotate_left(8)
            ^ serial.wrapping_mul(0x9e37_79b9);
        Self {
            nonce,
            next_id: 1,
            images: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            kitty: HashMap::new(),
            kitty_numbers: HashMap::new(),
            // Ids the terminal assigns for image numbers stay clear of the
            // small ids programs pick themselves.
            next_kitty_id: 0x7f00_0000,
        }
    }

    fn key(&self, internal: u32) -> String {
        format!("{:08x}.{internal}", self.nonce)
    }

    fn internal(&self, key: &str) -> Option<u32> {
        let (nonce, id) = key.split_once('.')?;
        if u32::from_str_radix(nonce, 16).ok()? != self.nonce {
            return None;
        }
        let id = id.parse().ok()?;
        self.images.contains_key(&id).then_some(id)
    }

    fn get(&self, key: &str) -> Option<StoredImage> {
        let internal = self.internal(key)?;
        self.images.get(&internal).map(|(image, _)| image.clone())
    }

    fn dimensions(&self, internal: u32) -> Option<(u32, u32)> {
        self.images
            .get(&internal)
            .map(|(image, _)| (image.width, image.height))
    }

    fn is_kitty(&self, internal: u32) -> bool {
        self.images
            .get(&internal)
            .is_some_and(|(_, kitty)| kitty.is_some())
    }

    /// Stores an image, evicting the oldest until it fits the budget. A Kitty
    /// id that already names an image replaces it.
    fn insert(&mut self, image: StoredImage, kitty_id: Option<u32>) -> u32 {
        if let Some(previous) = kitty_id.and_then(|id| self.kitty.get(&id).copied()) {
            self.remove(previous);
        }
        let size = image.data.len();
        while self.bytes + size > STORE_BUDGET_BYTES {
            let Some(oldest) = self.order.front().copied() else {
                break;
            };
            self.remove(oldest);
        }
        let internal = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.bytes += size;
        self.order.push_back(internal);
        self.images.insert(internal, (image, kitty_id));
        if let Some(id) = kitty_id {
            self.kitty.insert(id, internal);
        }
        internal
    }

    fn remove(&mut self, internal: u32) {
        let Some((image, kitty_id)) = self.images.remove(&internal) else {
            return;
        };
        self.bytes -= image.data.len();
        self.order.retain(|&id| id != internal);
        if let Some(id) = kitty_id
            && self.kitty.get(&id) == Some(&internal)
        {
            self.kitty.remove(&id);
            self.kitty_numbers.retain(|_, assigned| *assigned != id);
        }
    }

    fn remove_kitty(&mut self) {
        let kitty: Vec<u32> = self.kitty.values().copied().collect();
        for internal in kitty {
            self.remove(internal);
        }
    }

    /// The Kitty id for image number `number`; a new transmission under a
    /// number gets a fresh id, as in Kitty.
    fn kitty_number(&mut self, number: u32) -> u32 {
        let id = self.next_kitty_id;
        self.next_kitty_id = self.next_kitty_id.wrapping_add(1).max(0x7f00_0000);
        self.kitty_numbers.insert(number, id);
        id
    }

    fn kitty_by_number(&self, number: u32) -> Option<u32> {
        self.kitty_numbers.get(&number).copied()
    }

    fn kitty_internal(&self, id: u32) -> Option<(u32, u32)> {
        (id != 0)
            .then(|| self.kitty.get(&id).map(|&internal| (id, internal)))
            .flatten()
    }
}

// MARK: Kitty command

#[derive(Clone, Debug)]
struct KittyCommand {
    action: u8,
    format: u32,
    medium: u8,
    compressed: bool,
    width: u32,
    height: u32,
    id: u32,
    number: u32,
    placement: u32,
    more: bool,
    quiet: u8,
    cols: u32,
    rows: u32,
    no_move: bool,
    delete: u8,
    unicode_placeholder: bool,
    /// Keys whose meaning is not implemented: answered with an error rather
    /// than drawn wrong.
    unsupported: Option<&'static str>,
}

impl KittyCommand {
    fn parse(control: &[u8]) -> Self {
        let mut command = Self {
            action: b't',
            format: 32,
            medium: b'd',
            compressed: false,
            width: 0,
            height: 0,
            id: 0,
            number: 0,
            placement: 0,
            more: false,
            quiet: 0,
            cols: 0,
            rows: 0,
            no_move: false,
            delete: b'a',
            unicode_placeholder: false,
            unsupported: None,
        };
        for pair in control.split(|&byte| byte == b',') {
            let Some(split) = pair.iter().position(|&byte| byte == b'=') else {
                continue;
            };
            let (key, value) = (&pair[..split], &pair[split + 1..]);
            let [key] = key else { continue };
            let number = || {
                std::str::from_utf8(value)
                    .ok()
                    .and_then(|text| text.parse::<u32>().ok())
                    .unwrap_or(0)
            };
            let letter = value.first().copied().unwrap_or(0);
            match key {
                b'a' => command.action = letter,
                b'f' => command.format = number(),
                b't' => command.medium = letter,
                b'o' => command.compressed = letter == b'z',
                b's' => command.width = number(),
                b'v' => command.height = number(),
                b'i' => command.id = number(),
                b'I' => command.number = number(),
                b'p' => command.placement = number(),
                b'm' => command.more = number() == 1,
                b'q' => command.quiet = number().min(2) as u8,
                b'c' => command.cols = number(),
                b'r' => command.rows = number(),
                b'C' => command.no_move = number() == 1,
                b'd' => command.delete = letter,
                b'U' => command.unicode_placeholder = number() == 1,
                b'x' | b'y' | b'w' | b'h' if number() != 0 => {
                    command.unsupported = Some("ENOTSUP:source rectangles are not supported");
                }
                _ => {}
            }
        }
        if command.unicode_placeholder {
            command.unsupported = Some("ENOTSUP:unicode placeholders are not supported");
        }
        command
    }

    /// Validates a complete transmission into a stored image.
    fn image(&self, data: Vec<u8>) -> Result<StoredImage, &'static str> {
        if let Some(error) = self.unsupported {
            return Err(error);
        }
        if self.medium != b'd' {
            return Err("EBADF:only direct transmission is supported");
        }
        let (format, width, height) = match self.format {
            100 => {
                if self.compressed {
                    return Err("EINVAL:compressed PNG data is not supported");
                }
                match sniff(&data) {
                    Some((InlineImageFormat::Png, width, height)) => {
                        (InlineImageFormat::Png, width, height)
                    }
                    _ => return Err("EBADPNG:not a PNG image"),
                }
            }
            24 | 32 => {
                let format = if self.format == 24 {
                    InlineImageFormat::Rgb
                } else {
                    InlineImageFormat::Rgba
                };
                if self.width == 0 || self.height == 0 {
                    return Err("EINVAL:image dimensions are required");
                }
                let channels = if self.format == 24 { 3 } else { 4 };
                let expected = u64::from(self.width) * u64::from(self.height) * channels;
                if !self.compressed && (data.len() as u64) < expected {
                    return Err("ENODATA:insufficient image data");
                }
                (format, self.width, self.height)
            }
            _ => return Err("EINVAL:unsupported format"),
        };
        if data.is_empty() {
            return Err("ENODATA:no image data");
        }
        if !dimensions_allowed(width, height) {
            return Err("EFBIG:image dimensions are too large");
        }
        let mut data = data;
        if let Some(channels) = format.raw_channels()
            && !self.compressed
        {
            data.truncate(width as usize * height as usize * channels);
        }
        Ok(StoredImage {
            format,
            compressed: self.compressed && format.raw_channels().is_some(),
            width,
            height,
            data: data.into(),
        })
    }
}

// MARK: iTerm2 arguments

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ItermSize {
    #[default]
    Auto,
    Cells(u32),
    Pixels(u32),
    Percent(u32),
}

impl ItermSize {
    fn parse(value: &str) -> Self {
        let parse = |text: &str| text.trim().parse::<u32>().ok();
        if let Some(pixels) = value.strip_suffix("px") {
            parse(pixels).map_or(Self::Auto, Self::Pixels)
        } else if let Some(percent) = value.strip_suffix('%') {
            parse(percent).map_or(Self::Auto, |percent| Self::Percent(percent.min(100)))
        } else {
            parse(value).map_or(Self::Auto, Self::Cells)
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ItermArgs {
    inline: bool,
    width: ItermSize,
    height: ItermSize,
    do_not_move_cursor: bool,
}

impl ItermArgs {
    fn parse(args: &[u8]) -> Self {
        let mut parsed = Self::default();
        for pair in args.split(|&byte| byte == b';') {
            let Ok(pair) = std::str::from_utf8(pair) else {
                continue;
            };
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            match key {
                "inline" => parsed.inline = value == "1",
                "width" => parsed.width = ItermSize::parse(value),
                "height" => parsed.height = ItermSize::parse(value),
                "doNotMoveCursor" => parsed.do_not_move_cursor = value == "1",
                _ => {}
            }
        }
        parsed
    }
}

// MARK: Stream scanning

enum Next {
    None,
    /// An image sequence starts here, after this prefix.
    Found(usize, &'static [u8]),
    /// The input ends in what may be the start of a sequence.
    Partial(usize),
}

fn next_sequence(input: &[u8]) -> Next {
    let found = PREFIXES
        .iter()
        .zip(FINDERS.iter())
        .filter_map(|(prefix, finder)| finder.find(input).map(|start| (start, *prefix)))
        .min_by_key(|(start, _)| *start);
    if let Some((start, prefix)) = found {
        return Next::Found(start, prefix);
    }
    // The input may end inside an opener: hold the longest such tail.
    (1..ITERM_PREFIX.len())
        .rev()
        .find(|&length| {
            PREFIXES
                .iter()
                .any(|prefix| length < prefix.len() && input.ends_with(&prefix[..length]))
        })
        .map_or(Next::None, |length| Next::Partial(input.len() - length))
}

enum Collected {
    /// The terminator ended at this many bytes in.
    Complete(usize),
    /// A stray ESC begins at this many bytes in; it is not consumed.
    Aborted(usize),
    Partial,
}

/// Appends body bytes up to a terminator (BEL or ST).
fn collect(collecting: &mut Collecting, input: &[u8]) -> Collected {
    if collecting.pending_esc {
        collecting.pending_esc = false;
        if input.first() == Some(&b'\\') {
            return Collected::Complete(1);
        }
        // The ESC we held back starts something else; it is lost to the
        // parser, but so it is in a real terminal's string state.
        return Collected::Aborted(0);
    }
    let Some(at) = memchr::memchr2(0x07, 0x1b, input) else {
        push_body(collecting, input);
        return Collected::Partial;
    };
    push_body(collecting, &input[..at]);
    if input[at] == 0x07 {
        return Collected::Complete(at + 1);
    }
    match input.get(at + 1) {
        Some(b'\\') => Collected::Complete(at + 2),
        Some(_) => Collected::Aborted(at),
        None => {
            collecting.pending_esc = true;
            Collected::Partial
        }
    }
}

fn push_body(collecting: &mut Collecting, bytes: &[u8]) {
    if collecting.overflow {
        return;
    }
    if collecting.body.len() + bytes.len() > MAX_SEQUENCE_BYTES {
        collecting.overflow = true;
        collecting.body = Vec::new();
        return;
    }
    collecting.body.extend_from_slice(bytes);
}

// MARK: Decoding

/// Forgiving standard base64: padding optional, whitespace skipped, chunks
/// may be concatenated. False on any other byte or if `limit` is exceeded.
fn decode_base64_into(input: &[u8], output: &mut Vec<u8>, limit: usize) -> bool {
    fn value(byte: u8) -> Option<u32> {
        Some(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    }
    output.reserve((input.len() / 4 * 3).min(limit.saturating_sub(output.len())));
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for &byte in input {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let Some(value) = value(byte) else {
            return false;
        };
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            if output.len() >= limit {
                return false;
            }
            output.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    true
}

/// Format and pixel size from a file's header: PNG, GIF and JPEG.
pub(crate) fn sniff(data: &[u8]) -> Option<(InlineImageFormat, u32, u32)> {
    let be32 = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
    };
    if data.starts_with(b"\x89PNG\r\n\x1a\n") && data.get(12..16) == Some(b"IHDR") {
        return Some((InlineImageFormat::Png, be32(16)?, be32(20)?));
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        let le16 = |at: usize| -> Option<u32> {
            Some(u32::from(u16::from_le_bytes(
                data.get(at..at + 2)?.try_into().ok()?,
            )))
        };
        return Some((InlineImageFormat::Gif, le16(6)?, le16(8)?));
    }
    if data.starts_with(&[0xff, 0xd8]) {
        let be16 = |at: usize| -> Option<u32> {
            Some(u32::from(u16::from_be_bytes(
                data.get(at..at + 2)?.try_into().ok()?,
            )))
        };
        let mut at = 2;
        while at + 4 <= data.len() {
            if data[at] != 0xff {
                return None;
            }
            let marker = data[at + 1];
            if marker == 0xff {
                at += 1;
                continue;
            }
            if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
                at += 2;
                continue;
            }
            let length = be16(at + 2)? as usize;
            if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                return Some((InlineImageFormat::Jpeg, be16(at + 7)?, be16(at + 5)?));
            }
            at += 2 + length;
        }
    }
    None
}

#[cfg(test)]
mod tests;
