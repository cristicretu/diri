//! Visual regression fixture for cell-sized Unicode block elements.
//! Run with `cargo run -p diri-term --example block_elements`.
//! Optional fixture controls: `DIRI_BLOCK_FONT_SIZE=13`, `DIRI_BLOCK_LIGHT=1`,
//! or `DIRI_BLOCK_GRID=/path/to/encoded-grid-update` to replay captured cells.
use diri_proto::grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle};
use diri_term::{buffer::GridBuffer, element::TerminalElement, theme::TermTheme};
use gpui::{
    App, Bounds, Context, Render, Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use gpui_platform::application;

struct Blocks {
    terminal: TerminalElement,
}
impl Render for Blocks {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.terminal.clone())
    }
}
fn main() {
    application().run(|cx: &mut App| {
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.0), px(700.0)),
                    cx,
                ))),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|_| {
                    let terminal = TerminalElement::with_buffer(GridBuffer::new(70, 23))
                        .font(gpui::font(if cfg!(target_os = "macos") {
                            "Menlo"
                        } else {
                            "monospace"
                        }))
                        .font_size(px(std::env::var("DIRI_BLOCK_FONT_SIZE")
                            .ok()
                            .and_then(|s| s.parse::<f32>().ok())
                            .filter(|s| s.is_finite() && *s > 0.0)
                            .unwrap_or(24.0)))
                        .theme(if std::env::var_os("DIRI_BLOCK_LIGHT").is_some() {
                            TermTheme::DIRIJOR_LIGHT
                        } else {
                            TermTheme::DIRIJOR_DARK
                        })
                        .focused(true);
                    let lines = [
                        "",
                        "  Anara block rendering",
                        "",
                        "       ██            ██",
                        "  ▄█▄  ██  ▄█▄  ▄█▄  ██  ▄█▄",
                        "   ▀▀██████▀▀    ▀▀██████▀▀",
                        "   ▄▄██████▄▄    ▄▄██████▄▄",
                        "  ▀█▀  ██  ▀█▀  ▀█▀  ██  ▀█▀",
                        "       ██            ██",
                        "",
                        "  Full / half blocks must touch across rows and columns.",
                        "",
                        "  ▀▁▂▃▄▅▆▇█▉▊▋▌▍▎▏▐ ▔▕ ▖▗▘▙▚▛▜▝▞▟",
                        "",
                        "  ██▀▀▄▄██   regular text stays on the same grid",
                        "  ██▄▄▀▀██",
                        "",
                        "  Selectable Unicode cells; foreground follows the theme.",
                    ];
                    let changed_rows = (0..23)
                        .map(|row| {
                            let mut cells = vec![GridCell::BLANK; 70];
                            for (col, ch) in lines.get(row).unwrap_or(&"").chars().enumerate() {
                                cells[col] = GridCell::new(
                                    ch as u32,
                                    TermColor::Default,
                                    TermColor::DefaultInverted,
                                    TermStyle::empty(),
                                );
                            }
                            ChangedRow::new(row as u16, cells)
                        })
                        .collect();
                    let grid = GridUpdate {
                        cols: 70,
                        rows: 23,
                        cursor_col: 4,
                        cursor_row: 14,
                        cursor_visible: true,
                        is_full_snapshot: true,
                        changed_rows,
                    };
                    let grid = std::env::var_os("DIRI_BLOCK_GRID").map_or(grid, |path| {
                        GridUpdate::decode(&std::fs::read(path).expect("read captured grid"))
                            .expect("decode captured grid")
                    });
                    terminal.apply(grid, window);
                    Blocks { terminal }
                })
            },
        )
        .expect("open block fixture");
        cx.activate(true);
    });
}
