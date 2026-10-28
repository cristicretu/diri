//! A real editor in a real PTY, through the shared parser: insert mode
//! asks for a bar cursor and a diagnostic paints a colored undercurl, and both
//! must reach the grid wire.
//!
//! Opt-in, because it needs an installed editor:
//!
//! ```sh
//! DIRI_REAL_EDITOR=$(command -v nvim) cargo test -p diri-terminal-state \
//!     --test real_editor -- --ignored --nocapture
//! ```
//!
//! `DIRI_REAL_EDITOR` may name `nvim` or `vim`. Neither reads the user's
//! configuration (`--clean` / `-u NONE`) or writes a file; the editor quits
//! with `:qa!` and its PTY is closed. With `DIRI_VISUAL_OUTPUT` set, the
//! insert-mode screen is written there as `editor-insert.grid` (an encoded
//! full grid) for `diri-term`'s `style_goldens` render.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use diri_proto::grid::{CursorShape, TermColor, UnderlineKind};
use diri_terminal_state::HeadlessScreen;

const COLS: usize = 80;
const ROWS: usize = 24;

struct Editor {
    pty: diri_pty::Pty,
    output: diri_pty::PtyStream,
    input: diri_pty::PtyStream,
    screen: HeadlessScreen,
}

impl Editor {
    fn settle(&mut self, quiet: Duration) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = Instant::now();
        let mut buffer = [0; 8192];
        while Instant::now() < deadline && last.elapsed() < quiet {
            match self.output.read(&mut buffer) {
                Ok(0) => return,
                Ok(n) => {
                    last = Instant::now();
                    self.screen.feed(&buffer[..n]);
                    let replies = self.screen.take_replies();
                    if !replies.is_empty() {
                        self.input.write_all(&replies).unwrap();
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let _ = self.output.wait_readable(Duration::from_millis(10));
                }
                Err(error) if error.raw_os_error() == Some(5) => return,
                Err(error) => panic!("{error}"),
            }
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.input.write_all(bytes).unwrap();
        self.settle(Duration::from_millis(300));
    }
}

#[test]
#[ignore = "needs DIRI_REAL_EDITOR (nvim or vim)"]
fn editor_insert_mode_bar_cursor_and_undercurl_reach_the_wire() {
    let editor = std::env::var("DIRI_REAL_EDITOR").expect("DIRI_REAL_EDITOR names nvim or vim");
    let neovim = editor.contains("nvim");
    // The highlight a language server's error diagnostic uses, on a word.
    let argv: Vec<String> = if neovim {
        [
            editor.as_str(),
            "--clean",
            "-n",
            "-c",
            "set termguicolors",
            "-c",
            "hi DiagnosticUnderlineError gui=undercurl guisp=#e06c75",
            "-c",
            "call matchadd('DiagnosticUnderlineError', 'requset')",
        ]
        .map(String::from)
        .to_vec()
    } else {
        // Vim needs the de-facto standard vimrc lines; it does not probe.
        [
            editor.as_str(),
            "-u",
            "NONE",
            "-N",
            "-i",
            "NONE",
            "-n",
            "-c",
            "let &t_SI = \"\\e[6 q\" | let &t_EI = \"\\e[2 q\"",
            "-c",
            "let &t_Cs = \"\\e[4:3m\" | let &t_Ce = \"\\e[4:0m\"",
            "-c",
            "hi SpellBad cterm=undercurl ctermbg=NONE ctermul=1",
            "-c",
            "call matchadd('SpellBad', 'requset')",
        ]
        .map(String::from)
        .to_vec()
    };
    let mut spec = diri_pty::PtySpec::new(argv, std::env::temp_dir());
    spec.env = diri_platform::launch::local_environment();
    spec.env.retain(|(key, _)| {
        !matches!(
            key.as_str(),
            "TERM" | "COLORTERM" | "NO_COLOR" | "VIMINIT" | "MYVIMRC"
        )
    });
    spec.env.extend([
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
    ]);
    spec.cols = COLS as u16;
    spec.rows = ROWS as u16;
    let pty = diri_pty::Pty::spawn(&spec).expect("spawn editor");
    let output = pty.reader().unwrap();
    output.set_nonblocking(true).unwrap();
    let input = pty.writer().unwrap();
    let mut editor = Editor {
        pty,
        output,
        input,
        screen: HeadlessScreen::new(COLS, ROWS),
    };
    editor.settle(Duration::from_millis(800));

    editor.send(b"ilet x = requset;");
    let insert = editor.screen.full_snapshot();
    println!("insert-mode cursor: {:?}", insert.cursor_style);
    assert_eq!(
        insert.cursor_style.shape,
        CursorShape::Bar,
        "insert mode is a bar"
    );
    let (row, span) = insert
        .changed_rows
        .iter()
        .find_map(|row| {
            row.metadata
                .styles
                .iter()
                .find(|span| span.underline == UnderlineKind::Curly)
                .map(|span| (row, span))
        })
        .expect("an undercurl span");
    let word: String = row.cells[usize::from(span.start)..usize::from(span.end)]
        .iter()
        .filter_map(|cell| char::from_u32(cell.scalar))
        .collect();
    println!(
        "undercurl on {word:?} cols {}..{} color {:?}",
        span.start,
        span.end,
        span.underline_color.map(TermColor::unpack)
    );
    assert_eq!(word, "requset");
    assert!(span.underline_color.is_some(), "the diagnostic color");
    if let Some(directory) = std::env::var_os("DIRI_VISUAL_OUTPUT") {
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            std::path::Path::new(&directory).join("editor-insert.grid"),
            insert.encode().unwrap(),
        )
        .unwrap();
    }

    // Vim holds a lone Escape for `timeoutlen` before leaving insert mode.
    editor.input.write_all(b"\x1b").unwrap();
    editor.settle(Duration::from_millis(1_500));
    let normal = editor.screen.cursor_style();
    println!("normal-mode cursor: {normal:?}");
    assert_eq!(normal.shape, CursorShape::Block);
    editor.send(b":qa!\r");
    let _ = editor.pty.kill_group(9);
}
