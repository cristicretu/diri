//! Cost of capturing a reading view and continuing to consume live damage.
use criterion::{Criterion, criterion_group, criterion_main};
use diri_proto::grid::{ChangedRow, GridCell, GridUpdate};
use diri_term::{buffer::GridBuffer, element::TerminalElement};
use std::hint::black_box;

fn reading_view(c: &mut Criterion) {
    const COLS: u16 = 160;
    const ROWS: u16 = 50;
    let element = TerminalElement::with_buffer(GridBuffer::new(COLS, ROWS));
    let mut group = c.benchmark_group("reading_view_160x50");
    group.bench_function("capture_and_release", |b| {
        b.iter(|| {
            black_box(element.set_view_offset(1, usize::from(ROWS)));
            black_box(element.scroll_to_live(usize::from(ROWS)));
        });
    });

    let frames = ['a', 'b'].map(|ch| GridUpdate {
        cols: COLS,
        rows: ROWS,
        cursor_col: 0,
        cursor_row: ROWS - 1,
        cursor_visible: true,
        is_full_snapshot: false,
        changed_rows: (0..ROWS)
            .map(|y| {
                let mut cell = GridCell::BLANK;
                cell.scalar = u32::from(ch);
                ChangedRow::new(y, vec![cell; usize::from(COLS)])
            })
            .collect(),
    });
    for held in [false, true] {
        element.set_view_offset(i64::from(held), usize::from(ROWS));
        let mut next = 0;
        group.bench_function(if held { "held_damage" } else { "live_damage" }, |b| {
            b.iter(|| {
                black_box(element.apply_damage(frames[next].clone()));
                next ^= 1;
            });
        });
    }
    group.finish();
}

criterion_group!(benches, reading_view);
criterion_main!(benches);
