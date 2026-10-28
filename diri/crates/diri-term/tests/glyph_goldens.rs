//! Headless renders of the glyphs terminal programs lean on beyond plain
//! text: Nerd Font icons (Private Use Area), powerline caps, rounded and
//! nested box drawing, braille graphs, block meters, sextants and octants,
//! and programming ligatures, from real escape sequences through the shared
//! parser and the grid wire:
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/glyphs cargo test -p diri-term --test glyph_goldens -- --ignored
//! ```
//!
//! Writes `starship.png`, `lazygit.png`, `eza.png`, `nvim-tree.png`,
//! `btop.png`, `mosaics.png` and `ligatures-{on,off}.png`.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;

use diri_proto::grid::GridUpdate;
use diri_term::{buffer::GridBuffer, element::TerminalElement};
use diri_terminal_state::HeadlessScreen;
use gpui::{
    AppContext as _, Context, Font, IntoElement, ParentElement, Render, Styled, Window, div, px,
};

struct Fixture(TerminalElement);

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

fn screen(cols: usize, rows: usize, lines: &[&str]) -> GridUpdate {
    let mut screen = HeadlessScreen::new(cols, rows);
    screen.feed(b"\x1b[H");
    for (index, line) in lines.iter().enumerate() {
        screen.feed(line.as_bytes());
        if index + 1 < lines.len() {
            screen.feed(b"\x1b[0m\r\n");
        }
    }
    screen.feed(b"\x1b[0m\x1b[?25l");
    screen.full_snapshot()
}

/// The terminal font as the app builds it: the family plus the fallback chain.
fn terminal_font(family: &str, ligatures: bool) -> Font {
    diri_term::glyphs::terminal_font(family, ligatures)
}

fn capture(cx: &mut gpui::HeadlessAppContext, update: GridUpdate, font: Font, name: &str) {
    let (cols, rows) = (update.cols, update.rows);
    // A fractional cell (13.5 px Menlo is 8.13 px wide) so seams would show.
    let font_size = 13.5;
    let window_size = gpui::size(
        px(f32::from(cols) * font_size * 0.61),
        px(f32::from(rows) * font_size * 1.5),
    );
    let element = TerminalElement::with_buffer(GridBuffer::new(cols, rows))
        .focused(false)
        .font(font)
        .font_size(px(font_size));
    element.apply_damage(update);
    let window = cx
        .open_window(window_size, |_, cx| cx.new(|_| Fixture(element)))
        .expect("headless window");
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .unwrap();
    cx.run_until_parked();
    let image = cx.capture_screenshot(window.into()).expect("screenshot");
    image.save(output(name)).expect("write png");
}

fn output(name: &str) -> PathBuf {
    let directory = std::env::var_os("DIRI_VISUAL_OUTPUT")
        .map(PathBuf::from)
        .expect("DIRI_VISUAL_OUTPUT names the output directory");
    std::fs::create_dir_all(&directory).expect("output directory");
    directory.join(name)
}

const BLUE: &str = "\x1b[38;2;30;30;46;48;2;137;180;250m";
const BLUE_ON_MAUVE: &str = "\x1b[38;2;137;180;250;48;2;203;166;247m";
const MAUVE: &str = "\x1b[38;2;30;30;46;48;2;203;166;247m";
const MAUVE_END: &str = "\x1b[0;38;2;203;166;247m";
const GREEN: &str = "\x1b[0;38;2;166;227;161m";
const DIM: &str = "\x1b[0;38;2;108;112;134m";
const YELLOW: &str = "\x1b[0;38;2;249;226;175m";
const RED: &str = "\x1b[0;38;2;243;139;168m";

fn starship() -> GridUpdate {
    let line1 = format!(
        "{MAUVE_END}\u{e0b6}{MAUVE} \u{f179} giga {BLUE_ON_MAUVE}\u{e0b0}{BLUE} \u{f07c} ~/fun/diri \
         \x1b[38;2;137;180;250;48;2;166;227;161m\u{e0b0}\x1b[38;2;30;30;46;48;2;166;227;161m \u{e0a0} main \
         \u{f044} \x1b[0;38;2;166;227;161m\u{e0b4}{DIM} via {RED}\u{e7a8} v1.95.0"
    );
    let line2 = format!("{GREEN}\u{276f}\x1b[0m cargo test");
    let line3 = format!(
        "{DIM}\u{e0b2}\x1b[38;2;30;30;46;48;2;108;112;134m \u{f017} 22:41 \x1b[0m  {YELLOW}\u{e0b7}\u{e0b5} \u{e0b3}\u{e0b1} \u{e0b8}\u{e0ba}\u{e0bc}\u{e0be}"
    );
    screen(64, 4, &[&line1, &line2, "", &line3])
}

/// Columns `text` occupies once its escape sequences are stripped.
fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        width += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    width
}

/// A rounded panel `width` columns wide around `rows`, the way lazygit and
/// btop draw theirs.
fn panel(title: &str, footer: &str, width: usize, rows: &[String], color: &str) -> Vec<String> {
    let inner = width - 2;
    let rule = |label: &str, left: char, right: char| {
        let used = visible_width(label);
        format!(
            "{color}{left}─{label}{color}{}{right}\x1b[0m",
            "─".repeat(inner - 1 - used)
        )
    };
    let mut lines = vec![rule(title, '╭', '╮')];
    for row in rows {
        let pad = inner.saturating_sub(visible_width(row));
        lines.push(format!(
            "{color}│\x1b[0m{row}\x1b[0m{}{color}│\x1b[0m",
            " ".repeat(pad)
        ));
    }
    lines.push(rule(footer, '╰', '╯'));
    lines
}

fn side_by_side(left: Vec<String>, right: Vec<String>) -> Vec<String> {
    left.into_iter().zip(right).map(|(l, r)| l + &r).collect()
}

fn lazygit() -> GridUpdate {
    let accent = "\x1b[38;2;166;227;161m";
    let status = panel(
        "[1]─Status",
        "",
        24,
        &[" diri → main ✓".to_string()],
        accent,
    );
    let files = panel(
        "[2]─Files─\x1b[1mWorktrees\x1b[0m",
        "1 of 9",
        24,
        &[
            "\x1b[7m M crates/            ".to_string(),
            " ┃ M diri-term/".to_string(),
            " ┗━ A glyphs.rs".to_string(),
        ],
        "",
    );
    let unstaged = panel(
        "[0]─Unstaged changes",
        "",
        34,
        &[
            " M  crates/diri-term/src/glyph".to_string(),
            " A  crates/diri-term/fonts/...".to_string(),
            " ?? docs/notes.md".to_string(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ],
        "",
    );
    let mut left = status;
    left.extend(files);
    let mut lines = side_by_side(left, unstaged);
    lines.push("┌─┬─┐ ╔═╦═╗ ┏━┳━┓ ╒═╤═╕ ┄┄┄ ╌╌╌ ╱╲╳".to_string());
    lines.push("├─┼─┤ ╠═╬═╣ ┣━╋━┫ ╞═╪═╡ ┆┇┊ ╎╏".to_string());
    lines.push("└─┴─┘ ╚═╩═╝ ┗━┻━┛ ╘═╧═╛".to_string());
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    screen(58, lines.len(), &refs)
}

fn eza() -> GridUpdate {
    let dir = "\x1b[1;38;2;137;180;250m";
    let lines = [
        format!("{DIM}drwxr-xr-x\x1b[0m  - giga 27 Oct 22:41 {dir}\u{f024b} crates"),
        format!("{DIM}drwxr-xr-x\x1b[0m  - giga 27 Oct 22:41 {dir}\u{e5fb} .github"),
        format!("{DIM}drwxr-xr-x\x1b[0m  - giga 27 Oct 22:41 {dir}\u{f0c7c} vendor"),
        format!("{DIM}.rw-r--r--\x1b[0m 2.1k giga 27 Oct 22:41 {RED}\u{e7a8}\x1b[0m Cargo.toml"),
        format!("{DIM}.rw-r--r--\x1b[0m  98k giga 27 Oct 22:41 \u{f023}\x1b[0m Cargo.lock"),
        format!("{DIM}.rw-r--r--\x1b[0m  12k giga 27 Oct 22:41 {YELLOW}\u{f48a}\x1b[0m README.md"),
        format!("{DIM}.rw-r--r--\x1b[0m  412 giga 27 Oct 22:41 {RED}\u{f1d3}\x1b[0m .gitignore"),
        format!("{DIM}.rwxr-xr-x\x1b[0m  1.2k giga 27 Oct 22:41 {GREEN}\u{f489}\x1b[0m package.sh"),
        format!(
            "{DIM}.rw-r--r--\x1b[0m  3.4k giga 27 Oct 22:41 {YELLOW}\u{e60b}\x1b[0m tsconfig.json"
        ),
        format!(
            "{DIM}.rw-r--r--\x1b[0m  880 giga 27 Oct 22:41 \u{e606}\x1b[0m probe.py  \u{f0219} \u{ea60} \u{f418}"
        ),
    ];
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    screen(60, lines.len(), &refs)
}

fn nvim_tree() -> GridUpdate {
    let dir = "\x1b[38;2;137;180;250m";
    let lines = [
        format!("{dir}\u{f115}  diri\x1b[0m                     {DIM}│\x1b[0m 1 use gpui::Font;"),
        format!("{DIM}│ \x1b[0m{dir}\u{f07c}  crates\x1b[0m                 {DIM}│\x1b[0m 2"),
        format!(
            "{DIM}│ │ \x1b[0m{dir}\u{f114}  diri-app\x1b[0m             {DIM}│\x1b[0m 3 pub fn terminal_font("
        ),
        format!(
            "{DIM}│ │ \x1b[0m{dir}\u{f07c}  diri-term\x1b[0m            {DIM}│\x1b[0m 4     family: &str,"
        ),
        format!(
            "{DIM}│ │ │ \x1b[0m{RED}\u{e7a8}\x1b[0m  Cargo.toml         {DIM}│\x1b[0m 5 ) -> Font {{"
        ),
        format!(
            "{DIM}│ │ └ \x1b[0m{dir}\u{f07b}  src\x1b[0m                {DIM}│\x1b[0m 6     \u{f0eb} todo"
        ),
        format!("{DIM}│ └ \x1b[0m{YELLOW}\u{e60b}\x1b[0m  tsconfig.json        {DIM}│\x1b[0m 7 }}"),
        format!("{DIM}└ \x1b[0m\u{f48a}  README.md                {DIM}│\x1b[0m"),
        format!(
            "\x1b[38;2;30;30;46;48;2;137;180;250m NORMAL \x1b[38;2;137;180;250;48;2;49;50;68m\u{e0b0}\x1b[38;2;205;214;244m \u{e0a0} main \x1b[38;2;49;50;68;48;2;30;30;46m\u{e0b0}\x1b[0m {RED}\u{f00d} 2 {YELLOW}\u{f071} 1"
        ),
    ];
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    screen(60, lines.len(), &refs)
}

fn btop() -> GridUpdate {
    let graph = "\x1b[38;2;137;220;235m";
    let rows = [
        format!("{graph}⠀⠀⠀⠀⢀⣀⡀⠀⠀⠀⠀⠀⣠⣴⣶⣦⣄⠀⠀⠀⠀⢀⣤⣶⣿⣿⣷⣦⣀⠀⠀⢀⣀⣤⣴⣶⣾⣿⣿⣿⣿⣷⣶⣤⣀⠀⠀⠀⠀"),
        format!("{graph}⣀⣠⣴⣾⣿⣿⣿⣷⣦⣤⣴⣾⣿⣿⣿⣿⣿⣿⣦⣤⣴⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣷⣦⣀"),
        format!(
            " C0 {GREEN}■■■■■■■■\x1b[38;2;249;226;175m■■■■\x1b[38;2;69;71;90m■■■■■■■■\x1b[0m 48% ▁▂▃▄▅▆▇█▇▆▅▄▃▂▁"
        ),
        " mem ░░░▒▒▒▓▓▓███ ▏▎▍▌▋▊▉█ ▐▕▔▀▄ ▖▗▘▙▚▛▜▝▞▟".to_string(),
    ];
    let lines = panel(" cpu ", "", 52, &rows, GREEN);
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    screen(52, lines.len(), &refs)
}

fn mosaics() -> GridUpdate {
    let sextants: String = (0x1fb00..=0x1fb3b).filter_map(char::from_u32).collect();
    let octants: String = (0x1cd00..=0x1cd3b).filter_map(char::from_u32).collect();
    let tiled = format!(
        "\x1b[38;2;166;227;161m{}\x1b[0m {}",
        "\u{1fb0b}\u{1fb07}\u{1fb1b}\u{1fb38}\u{1fb3b}\u{1fb2d}\u{1fb02}",
        "\u{1cd03}\u{1cd5a}\u{1cde5}\u{1cd8f}"
    );
    screen(62, 4, &[&sextants, &octants, "", &tiled])
}

fn ligatures() -> GridUpdate {
    screen(
        52,
        3,
        &[
            "a => b != c -> d === e <= f >= g :: h |> i",
            "\x1b[38;2;137;180;250mfn\x1b[0m f() -> Result<(), E> { x != y && a == b }",
            "<!-- www --> /* */ // ... ;; ++ -- ** |||",
        ],
    )
}

// One test: two headless platforms cannot start concurrently.
#[test]
#[ignore = "writes PNG renders for review"]
fn render_glyph_goldens() {
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    let started = std::time::Instant::now();
    cx.update(|cx| diri_term::glyphs::register_bundled_fonts(cx))
        .expect("bundled fonts");
    eprintln!("bundled font registration: {:?}", started.elapsed());
    let menlo = terminal_font("Menlo", true);
    capture(&mut cx, starship(), menlo.clone(), "starship.png");
    capture(&mut cx, lazygit(), menlo.clone(), "lazygit.png");
    capture(&mut cx, eza(), menlo.clone(), "eza.png");
    capture(&mut cx, nvim_tree(), menlo.clone(), "nvim-tree.png");
    capture(&mut cx, btop(), menlo.clone(), "btop.png");
    capture(&mut cx, mosaics(), menlo, "mosaics.png");
    capture(
        &mut cx,
        ligatures(),
        terminal_font("JetBrains Mono", true),
        "ligatures-on.png",
    );
    capture(
        &mut cx,
        ligatures(),
        terminal_font("JetBrains Mono", false),
        "ligatures-off.png",
    );
}
