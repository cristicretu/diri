//! Attach to a live session the way the desktop pane does and log every grid
//! frame, so the seed/resize sequence can be inspected outside the GUI.
//! Usage: attach_probe <session-id> <cols> <rows>
use std::time::{Duration, Instant};

use diri_client::{SessionAttachment, TerminalChunk};
use diri_proto::SessionId;
use diri_proto::grid::GridCell;
use diri_proto::paths::DirijorPaths;

fn non_blank_rows(cells: &[GridCell], cols: usize) -> usize {
    if cols == 0 {
        return 0;
    }
    cells
        .chunks(cols)
        .filter(|row| {
            row.iter()
                .any(|cell| cell.scalar != 0 && cell.scalar != u32::from(b' '))
        })
        .count()
}

fn style_summary(cells: &[GridCell], cols: usize, rows_to_show: &[usize]) -> String {
    use diri_proto::grid::TermStyle;
    let mut invisible = 0usize;
    let mut dim = 0usize;
    let mut inverse = 0usize;
    let mut printable = 0usize;
    let mut fgs = std::collections::HashSet::new();
    for cell in cells {
        if cell.scalar != 0 && cell.scalar != 32 {
            printable += 1;
            fgs.insert(format!("{:?}", cell.fg));
            if cell.style.contains(TermStyle::INVISIBLE) {
                invisible += 1;
            }
            if cell.style.contains(TermStyle::DIM) {
                dim += 1;
            }
            if cell.style.contains(TermStyle::INVERSE) {
                inverse += 1;
            }
        }
    }
    let mut out = format!(
        "printable={printable} invisible={invisible} dim={dim} inverse={inverse} fgs={fgs:?}"
    );
    for row in rows_to_show {
        if let Some(cells) = cells.get(row * cols..(row + 1) * cols) {
            let text: String = cells
                .iter()
                .map(|c| char::from_u32(c.scalar).unwrap_or(' '))
                .collect();
            out.push_str(&format!("\n    row {row}: {:?}", text.trim_end()));
        }
    }
    out
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let id = SessionId(args.next().expect("session id"));
    let cols: u16 = args.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let rows: u16 = args.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let socket = DirijorPaths::socket(std::env::var("HOME").unwrap());
    let start = Instant::now();
    let mut attachment = SessionAttachment::connect(&socket, id)
        .await
        .expect("connect");
    eprintln!("[{:>7.1?}] connected", start.elapsed());
    let mut cells = Vec::new();
    let mut resized = false;
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let chunk = tokio::select! {
            chunk = attachment.chunks.recv() => chunk,
            _ = tokio::time::sleep(remaining.min(Duration::from_millis(300))) => {
                if !resized && cols > 0 && rows > 0 {
                    resized = true;
                    attachment.resize(cols, rows).expect("resize");
                    eprintln!("[{:>7.1?}] sent resize {cols}x{rows}", start.elapsed());
                }
                continue;
            }
        };
        let Some(chunk) = chunk else {
            eprintln!("closed");
            break;
        };
        match chunk {
            TerminalChunk::Grid(update) => {
                update.apply(&mut cells);
                eprintln!(
                    "[{:>7.1?}] grid full={} {}x{} changed_rows={} cursor=({},{}) vis={} -> composed non-blank rows={}",
                    start.elapsed(),
                    update.is_full_snapshot,
                    update.cols,
                    update.rows,
                    update.changed_rows.len(),
                    update.cursor_col,
                    update.cursor_row,
                    update.cursor_visible,
                    non_blank_rows(&cells, usize::from(update.cols)),
                );
                if update.is_full_snapshot {
                    let cursor_row = usize::from(update.cursor_row);
                    let rows = [
                        cursor_row.saturating_sub(2),
                        cursor_row.saturating_sub(1),
                        cursor_row,
                    ];
                    eprintln!(
                        "    {}",
                        style_summary(&cells, usize::from(update.cols), &rows)
                    );
                }
            }
            TerminalChunk::Modes {
                alt_screen,
                bracketed_paste,
                mouse,
            } => {
                eprintln!(
                    "[{:>7.1?}] modes alt={alt_screen} bp={bracketed_paste} mouse={mouse:?}",
                    start.elapsed()
                );
            }
            TerminalChunk::Pong => {}
        }
    }
    attachment.close().await;
}
