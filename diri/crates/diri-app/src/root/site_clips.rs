//! Records the feature clips on diri.sh from the real window, frame by frame,
//! the same way the What's New clips are recorded.
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/site-clips \
//!   cargo test -p diri-app --bin diri -- --ignored render_site_clips
//! scripts/site-clips.sh /tmp/site-clips   # frames -> website/assets/features/*.webp
//! ```
//!
//! `DIRI_CLIP=agents|schedules|computer|voice` records one clip. Terminal
//! contents are scripted grids: what each agent prints is written here, the
//! window, sidebar, panes and marks around it are the app's own.

use std::path::PathBuf;
use std::time::Duration;

use diri_proto::grid::{GridCell, TermColor, TermStyle};
use diri_proto::schedules::ScheduledRunInfo;
use diri_proto::workspace::*;
use diri_proto::{DateMillis, SessionStatus};
use diri_term::buffer::{CursorState, GridBuffer};
use gpui::{AnyWindowHandle, HeadlessAppContext, size};

use super::tests::test_services;
use super::whats_new_clips::Recorder;
use super::*;
use crate::SidebarPreviewFixture;
use crate::store::TabOrientation;

/// A normal window's proportions, shown beside or under its feature's text.
const WINDOW: ((f32, f32), f32) = ((1140.0, 700.0), 13.0);
/// The agents clip spans the site's whole content column (up to about 1160
/// screen pixels), so it is the same shape, larger: 2x is never stretched.
/// Its terminal font is larger by the same factor, so it reads at the size of
/// the page's other clips.
const WIDE_WINDOW: ((f32, f32), f32) = ((1520.0, 930.0), 17.0);
const COLS: u16 = 170;
/// `WINDOW` under horizontal tabs: the pane's columns and rows, so the prompt
/// box spans the pane and sits on its last rows.
const SWARM_COLS: usize = 148;
const SWARM_ROWS: usize = 42;

#[derive(Clone, Copy)]
enum Hue {
    Plain,
    Bold,
    Dim,
    Green,
    Red,
    Blue,
    Purple,
    Claude,
}

type Line = Vec<(String, Hue)>;

fn l(spans: &[(&str, Hue)]) -> Line {
    spans.iter().map(|(t, i)| ((*t).to_owned(), *i)).collect()
}

fn p(text: &str) -> Line {
    l(&[(text, Hue::Plain)])
}

/// What a pane shows: scrollback ending at the agent's input box, with the
/// cursor at `cursor` (row, column). A pane shorter than the grid shows its
/// bottom rows, so a long screen scrolls the way a terminal does.
struct Screen {
    lines: Vec<Line>,
    cursor: (usize, usize),
}

fn grid(screen: &Screen) -> GridBuffer {
    let lines = &screen.lines;
    let rows = lines.len() as u16;
    let mut grid = GridBuffer::new(COLS, rows.max(1));
    for (y, line) in lines.iter().enumerate() {
        let mut x = 0usize;
        for (text, hue) in line {
            for ch in text.chars() {
                if x >= COLS as usize {
                    break;
                }
                let cell = &mut grid.cells[y * COLS as usize + x];
                *cell = GridCell {
                    scalar: ch as u32,
                    ..GridCell::BLANK
                };
                match hue {
                    Hue::Plain => {}
                    Hue::Bold => cell.style = TermStyle::BOLD,
                    Hue::Dim => cell.fg = TermColor::Ansi(8),
                    Hue::Green => cell.fg = TermColor::Ansi(2),
                    Hue::Red => cell.fg = TermColor::Ansi(1),
                    Hue::Blue => cell.fg = TermColor::Ansi(4),
                    Hue::Purple => cell.fg = TermColor::Ansi(5),
                    Hue::Claude => cell.fg = TermColor::Rgb(215, 119, 87),
                }
                x += 1;
            }
        }
    }
    // Agent TUIs hide the terminal cursor and draw their own inverted cell.
    let (row, col) = screen.cursor;
    if row < lines.len() && col < COLS as usize {
        let cell = &mut grid.cells[row * COLS as usize + col];
        if cell.scalar == 0 {
            cell.scalar = 32;
        }
        cell.style |= TermStyle::INVERSE;
    }
    grid.cursor = CursorState {
        col: col as u16,
        row: row as u16,
        visible: false,
        ..CursorState::default()
    };
    grid
}

fn rule(width: usize) -> Line {
    l(&[(&"─".repeat(width), Hue::Dim)])
}

/// Claude Code: the transcript, then its prompt box with `input` typed.
fn claude(body: &[Line], input: &str, width: usize) -> Screen {
    let mut lines = body.to_vec();
    lines.push(p(""));
    lines.push(rule(width));
    let row = lines.len();
    lines.push(l(&[("> ", Hue::Plain), (input, Hue::Plain)]));
    lines.push(rule(width));
    lines.push(l(&[("  ? for shortcuts", Hue::Dim)]));
    Screen {
        lines,
        cursor: (row, 2 + input.chars().count()),
    }
}

/// Codex: the transcript, a working line while it works, then its composer.
fn codex(body: &[Line], working: Option<&str>) -> Screen {
    let mut lines = body.to_vec();
    lines.push(p(""));
    if let Some(time) = working {
        lines.push(l(&[("• ", Hue::Dim), ("Working", Hue::Bold), (&format!(" ({time} • esc to interrupt)"), Hue::Dim)]));
        lines.push(p(""));
    }
    let row = lines.len();
    lines.push(l(&[("› ", Hue::Bold), ("Ask Codex to do anything", Hue::Dim)]));
    lines.push(p(""));
    lines.push(l(&[("  ⏎ send   ⇧⏎ newline   ⌃T transcript   ⌃C quit", Hue::Dim)]));
    Screen {
        lines,
        cursor: (row, 2),
    }
}

fn codex_banner(dir: &str) -> Vec<Line> {
    let w = 46;
    let row = |text: &str| {
        let pad = w - 4 - text.chars().count();
        l(&[("│ ", Hue::Dim), (text, Hue::Plain), (&" ".repeat(pad), Hue::Plain), (" │", Hue::Dim)])
    };
    vec![
        l(&[(&format!("╭{}╮", "─".repeat(w - 2)), Hue::Dim)]),
        row(">_ OpenAI Codex (v0.160.0)"),
        row(""),
        row("model:     gpt-6 high"),
        row(&format!("directory: {dir}")),
        l(&[(&format!("╰{}╯", "─".repeat(w - 2)), Hue::Dim)]),
        p(""),
    ]
}

fn claude_banner(dir: &str) -> Vec<Line> {
    let w = 52;
    let row = |text: &str, hue: Hue| {
        let pad = w - 4 - text.chars().count();
        l(&[("│ ", Hue::Claude), (text, hue), (&" ".repeat(pad), Hue::Plain), (" │", Hue::Claude)])
    };
    vec![
        l(&[(&format!("╭{}╮", "─".repeat(w - 2)), Hue::Claude)]),
        row("✻ Welcome to Claude Code!", Hue::Bold),
        row("", Hue::Plain),
        row(&format!("  cwd: {dir}"), Hue::Dim),
        l(&[(&format!("╰{}╯", "─".repeat(w - 2)), Hue::Claude)]),
        p(""),
    ]
}

struct Clip {
    window: AnyWindowHandle,
}

fn session_like(
    template: &SessionRecord,
    id: &str,
    kind: AgentKind,
    title: &str,
    status: SessionStatus,
) -> SessionRecord {
    let mut s = template.clone();
    s.id = SessionId::new(id);
    s.kind = kind;
    s.title = title.into();
    s.title_source = diri_proto::TitleSource::DirijorAssigned;
    s.status = status;
    s.parent = None;
    s.pinned = false;
    s.archived_at = None;
    s.needs_input = None;
    s.attention_state = None;
    s.pull_requests = None;
    s.listening_ports = None;
    s.foreground_agent = None;
    s.artifacts = None;
    s.host = None;
    s.scheduled_run = None;
    s.last_turn_completed_at = None;
    s
}

/// The real window over the Typical sidebar with `sessions` on top and
/// `selected` open in a `window_size` window. `workspace` puts the selected
/// tab in a split layout.
fn open(
    cx: &mut HeadlessAppContext,
    sessions: impl FnOnce(&SessionRecord) -> Vec<SessionRecord>,
    selected: &str,
    workspace: Option<WorkspaceSnapshot>,
    (window_size, font_size): ((f32, f32), f32),
    material: crate::store::WindowMaterial,
    tabs: TabOrientation,
) -> Clip {
    let services = test_services();
    let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    // Questions and permission prompts would pull the eye off the clip.
    fixture.list.sessions.retain(|s| {
        !matches!(
            s.id.0.as_str(),
            "preview-question" | "preview-claude" | "preview-long" | "preview-ended"
        )
    });
    let template = fixture.list.sessions[0].clone();
    let own = sessions(&template);
    // Tabs show every session, so a horizontal-tabs clip shows only its own.
    if tabs == TabOrientation::Horizontal {
        fixture.list.sessions.clear();
    }
    for (i, s) in own.into_iter().enumerate() {
        fixture.list.sessions.insert(i, s);
    }
    {
        let mut store = services.store.store.write().unwrap();
        store.hydrate(fixture.list);
        store.mark_connected_for_test();
        store.set_agent_catalog(crate::agent_setup::bundled_catalog(&[
            "claude-code",
            "codex",
            "gemini",
            "pi",
        ]));
        store.select(SessionId::new(selected));
        store
            .update_preferences(|prefs| {
                prefs.sidebar_visible = tabs == TabOrientation::Vertical;
                prefs.tab_orientation = tabs;
                prefs.terminal_theme = "rose-pine".into();
                prefs.window_material = material;
                prefs.terminal_font_size = font_size;
            })
            .unwrap();
        if let Some(snapshot) = workspace.clone() {
            store.seed_workspace_snapshot_for_test(snapshot);
        }
    }
    let window = cx
        .open_window(size(px(window_size.0), px(window_size.1)), move |window, cx| {
            cx.new(|cx| {
                let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                if let Some(snapshot) = &workspace {
                    let id = snapshot.workspaces[0].id.clone();
                    root.sidebar
                        .update(cx, |sidebar, cx| sidebar.activate_workspace(Some(id), cx));
                }
                root
            })
        })
        .unwrap();
    cx.run_until_parked();
    // A headless window opens opaque and the root only pushes a material when
    // it changes, so glass is set here: the capture keeps the translucent
    // chrome's alpha for the page to show its own backdrop through.
    cx.update_window(window.into(), |_, window, _| {
        window.set_background_appearance(window_background(material));
    })
    .unwrap();
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(120));
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
    }
    Clip {
        window: window.into(),
    }
}

thread_local! {
    static LIVE: std::cell::RefCell<std::collections::HashMap<String, GridBuffer>> =
        Default::default();
}

/// `screen` applied to `pane`'s running grid as a full snapshot, the way the
/// Engine delivers one. A fresh grid each frame would restart its row
/// generations, and the renderer would keep painting the rows it prepared.
fn live(pane: &str, screen: &Screen) -> GridBuffer {
    let target = grid(screen);
    LIVE.with(|live| {
        let mut live = live.borrow_mut();
        let buffer = live
            .entry(pane.to_owned())
            .or_insert_with(|| GridBuffer::new(COLS, 1));
        let cols = usize::from(target.cols);
        buffer.apply(diri_proto::grid::GridUpdate {
            cols: target.cols,
            rows: target.rows,
            cursor_col: target.cursor.col,
            cursor_row: target.cursor.row,
            cursor_visible: target.cursor.visible,
            is_full_snapshot: true,
            changed_rows: (0..target.rows)
                .map(|y| {
                    let start = usize::from(y) * cols;
                    diri_proto::grid::ChangedRow::new(y, target.cells[start..start + cols].to_vec())
                })
                .collect(),
            cursor_style: Default::default(),
            history: None,
        });
        buffer.clone()
    })
}

/// Puts `lines` in the selected session's terminal.
fn show(rec: &mut Recorder, screen: &Screen) {
    let grid = live(":selected", screen);
    rec.cx
        .update_window(rec.window, |view, _, cx| {
            let root = view.downcast::<RootView>().unwrap();
            let terminal = root.read(cx).terminal.clone().expect("terminal pane");
            terminal.update(cx, |terminal, cx| {
                terminal.seed_preview_grid_for_test(grid, cx);
                cx.notify();
            });
        })
        .unwrap();
    rec.cx.run_until_parked();
}

/// Puts `lines` in `session`'s split pane.
fn show_pane(rec: &mut Recorder, session: &str, screen: &Screen) {
    let grid = live(session, screen);
    rec.cx
        .update_window(rec.window, |view, _, cx| {
            let root = view.downcast::<RootView>().unwrap();
            let workbench = root
                .read(cx)
                .workspace_workbench
                .clone()
                .expect("workspace workbench");
            workbench.update(cx, |workbench, cx| {
                workbench.seed_session_grid_for_test(session, &grid, cx)
            });
        })
        .unwrap();
    rec.cx.run_until_parked();
}

fn update_session(rec: &mut Recorder, id: &str, f: impl FnOnce(&mut SessionRecord)) {
    rec.cx
        .update_window(rec.window, |view, _, cx| {
            let root = view.downcast::<RootView>().unwrap();
            let runtime = root.read(cx).services.store.clone();
            let mut store = runtime.store.write().unwrap();
            let mut s = (**store.sessions().get(&SessionId::new(id)).unwrap()).clone();
            f(&mut s);
            store.upsert_session(s);
        })
        .unwrap();
    rec.cx.run_until_parked();
}

fn add_session(rec: &mut Recorder, s: SessionRecord) {
    rec.cx
        .update_window(rec.window, |view, _, cx| {
            let root = view.downcast::<RootView>().unwrap();
            let runtime = root.read(cx).services.store.clone();
            runtime.store.write().unwrap().upsert_session(s);
        })
        .unwrap();
    rec.cx.run_until_parked();
}

fn set_layout(rec: &mut Recorder, snapshot: WorkspaceSnapshot) {
    rec.cx
        .update_window(rec.window, |view, _, cx| {
            let root = view.downcast::<RootView>().unwrap();
            let runtime = root.read(cx).services.store.clone();
            runtime
                .store
                .write()
                .unwrap()
                .seed_workspace_snapshot_for_test(snapshot);
            root.update(cx, |_, cx| cx.notify());
        })
        .unwrap();
    rec.cx.run_until_parked();
    rec.settle();
}

fn workspace(revision: u64, layout: LayoutNode) -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        revision,
        workspaces: vec![WorkspaceRecord {
            project_id: None,
            id: WorkspaceId::new("clip-workspace"),
            name: "diri".into(),
            selected_tab: Some(TabId::new("clip-tab")),
            tabs: vec![WorkspaceTab {
                id: TabId::new("clip-tab"),
                title: Some("Fix the flaky reconnect test".into()),
                focused_pane: PaneId::new("lead"),
                zoomed_pane: None,
                layout,
            }],
        }],
        ..Default::default()
    }
}

fn lead_pane() -> LayoutNode {
    LayoutNode::Pane {
        id: PaneId::new("lead"),
        session_id: SessionId::new("clip-lead"),
    }
}

/// A full pane is about 110 columns wide in WINDOW; in WIDE_WINDOW a full
/// pane is 120 columns by 43 rows and a split pane about 59 columns.
const FULL: usize = 108;
const WIDE_FULL: usize = 118;
const WIDE_HALF: usize = 57;
const WIDE_ROWS: usize = 43;

/// `screen` with blank rows inserted after its first `body` lines, so an
/// agent's input stays on the pane's last rows the way a full-screen TUI
/// keeps it while the transcript above is still short.
fn pin(mut screen: Screen, body: usize, rows: usize) -> Screen {
    let pad = rows.saturating_sub(screen.lines.len());
    screen.lines.splice(body..body, (0..pad).map(|_| p("")));
    screen.cursor.0 += pad;
    screen
}

/// Claude Code starts Codex on `forge` beside itself over MCP, waits, and
/// reads its result.
fn record_agents(cx: &mut HeadlessAppContext, dir: PathBuf) {
    let clip = open(
        cx,
        |t| {
            vec![session_like(
                t,
                "clip-lead",
                AgentKind::CLAUDE_CODE,
                "Fix the flaky reconnect test",
                SessionStatus::Idle,
            )]
        },
        "clip-lead",
        Some(workspace(1, lead_pane())),
        WIDE_WINDOW,
        crate::store::WindowMaterial::Glass,
        TabOrientation::Vertical,
    );
    // The hero clip: its sidebar spinners turn, so motion stays on and the
    // clock steps with the frames.
    cx.update(|cx| cx.set_reduce_motion(false));
    let mut rec = Recorder::new(cx, clip.window, dir).animated(125);
    use Hue::*;
    let mut lead: Vec<Line> = vec![
        l(&[("> ", Dim), ("Bump tokio and check nothing else moved.", Plain)]),
        p(""),
        l(&[("● ", Green), ("Update", Bold), ("(Cargo.toml)", Plain)]),
        l(&[("  ⎿ ", Dim), ("Updated with ", Plain), ("1 addition", Green), (" and ", Plain), ("1 removal", Red)]),
        l(&[("● ", Green), ("Bash", Bold), ("(cargo test --workspace)", Plain)]),
        l(&[("  ⎿ ", Dim), ("1,642 passed, 0 failed", Green)]),
        l(&[("● ", Claude), ("tokio is on 1.48 and the workspace is green.", Plain)]),
        p(""),
        l(&[("> ", Dim), ("The reconnect test failed on CI again.", Plain)]),
        p(""),
        l(&[("● ", Green), ("Bash", Bold), ("(gh run view 4412 --log-failed)", Plain)]),
        l(&[("  ⎿ ", Dim), ("handshake_timeout ... ", Plain), ("FAILED", Red)]),
        p(""),
        l(&[("● ", Claude), ("It only fails under load, and I can't", Plain)]),
        l(&[("  ", Plain), ("reproduce it on this Mac. forge has the", Plain)]),
        l(&[("  ", Plain), ("same 32 cores as CI.", Plain)]),
        p(""),
    ];
    let ask = "Use diri to start Codex on forge and fix it there. Show it beside me.";
    let mut typed = String::new();
    show_pane(&mut rec, "clip-lead", &pin(claude(&lead, "", WIDE_FULL), lead.len(), WIDE_ROWS));
    rec.shot(700);
    for word in ask.split_inclusive(' ') {
        typed.push_str(word);
        // The prompt box wraps at the pane's width.
        let shown: String = typed.chars().rev().take(WIDE_FULL - 4).collect::<Vec<_>>().into_iter().rev().collect();
        show_pane(&mut rec, "clip-lead", &pin(claude(&lead, &shown, WIDE_FULL), lead.len(), WIDE_ROWS));
        rec.shot(70);
    }
    rec.shot(400);
    lead.extend([
        l(&[("> ", Dim), ("Use diri to start Codex on forge and fix", Plain)]),
        l(&[("  ", Dim), ("it there. Show it beside me.", Plain)]),
        p(""),
        l(&[("● ", Green), ("dirijor - spawn_agent", Bold), (" (MCP)", Dim)]),
        l(&[("  ⎿ ", Dim), ("codex · host forge · split right", Dim)]),
    ]);
    update_session(&mut rec, "clip-lead", |s| s.status = SessionStatus::Working);
    show_pane(&mut rec, "clip-lead", &pin(claude(&lead, "", WIDE_FULL), lead.len(), WIDE_ROWS));
    rec.shot(800);
    // The Engine opens the new session as a pane beside the caller.
    let template = rec
        .cx
        .update_window(rec.window, |view, _, cx| {
            let root = view.downcast::<RootView>().unwrap();
            let runtime = root.read(cx).services.store.clone();
            let store = runtime.store.read().unwrap();
            (**store.sessions().get(&SessionId::new("clip-lead")).unwrap()).clone()
        })
        .unwrap();
    let mut helper = session_like(
        &template,
        "clip-codex",
        AgentKind::CODEX,
        "Fix the reconnect test on forge",
        SessionStatus::Working,
    );
    helper.parent = Some(SessionId::new("clip-lead"));
    helper.host = Some("forge".into());
    add_session(&mut rec, helper);
    set_layout(
        &mut rec,
        workspace(
            2,
            LayoutNode::Split {
                id: SplitId::new("clip-split"),
                axis: LayoutAxis::Horizontal,
                fraction: 0.5,
                first: Box::new(lead_pane()),
                second: Box::new(LayoutNode::Pane {
                    id: PaneId::new("helper"),
                    session_id: SessionId::new("clip-codex"),
                }),
            },
        ),
    );
    lead.extend([
        l(&[("  ⎿ ", Dim), ("Started Codex on forge", Plain)]),
        p(""),
        l(&[("● ", Green), ("dirijor - wait_for_agent", Bold), (" (MCP)", Dim)]),
        l(&[("  ⎿ ", Dim), ("until done…", Dim)]),
    ]);
    show_pane(&mut rec, "clip-lead", &pin(claude(&lead, "", WIDE_HALF), lead.len(), WIDE_ROWS));
    let mut work = codex_banner("~/diri on forge");
    work.extend([
        l(&[("› ", Bold), ("Fix the flaky reconnect test. It fails under", Plain)]),
        l(&[("  ", Plain), ("load on CI; reproduce it here first.", Plain)]),
        p(""),
    ]);
    let steps: Vec<(Vec<Line>, &str)> = vec![
        (vec![], "0s"),
        (vec![
            l(&[("• ", Dim), ("Ran ", Bold), ("cargo test -p reconnect ×20", Plain)]),
            l(&[("  └ ", Dim), ("3 of 20 failed: handshake_timeout", Red)]),
        ], "48s"),
        (vec![
            l(&[("• ", Dim), ("Explored", Bold)]),
            l(&[("  └ ", Dim), ("Read retry.rs, reconnect.rs", Dim)]),
        ], "1m 30s"),
        (vec![
            l(&[("• ", Dim), ("The retry timer can fire before the", Plain)]),
            l(&[("  ", Dim), ("handshake resolves.", Plain)]),
        ], "2m 41s"),
        (vec![
            l(&[("• ", Dim), ("Edited ", Bold), ("src/retry.rs ", Plain), ("(+14 ", Green), ("-3)", Red)]),
        ], "3m 05s"),
        (vec![
            l(&[("• ", Dim), ("Ran ", Bold), ("cargo test -p reconnect ×20", Plain)]),
            l(&[("  └ ", Dim), ("20 of 20 passed", Green)]),
        ], "6m 02s"),
    ];
    for (i, (block, time)) in steps.into_iter().enumerate() {
        work.extend(block);
        show_pane(&mut rec, "clip-codex", &pin(codex(&work, Some(time)), work.len(), WIDE_ROWS));
        rec.shot(if i == 0 { 900 } else { 650 });
    }
    work.extend([p(""), l(&[("─ Worked for 6m 12s ─", Dim)])]);
    show_pane(&mut rec, "clip-codex", &pin(codex(&work, None), work.len(), WIDE_ROWS));
    update_session(&mut rec, "clip-codex", |s| {
        s.status = SessionStatus::Idle;
        s.last_turn_completed_at = Some(DateMillis(3.0));
    });
    rec.shot(700);
    lead.pop();
    lead.extend([
        l(&[("  ⎿ ", Dim), ("Done in 6m 12s", Green)]),
        p(""),
        l(&[("● ", Green), ("dirijor - read_output", Bold), (" (MCP)", Dim)]),
        l(&[("  ⎿ ", Dim), ("last message from Codex", Dim)]),
    ]);
    show_pane(&mut rec, "clip-lead", &pin(claude(&lead, "", WIDE_HALF), lead.len(), WIDE_ROWS));
    rec.shot(800);
    lead.extend([
        p(""),
        l(&[("● ", Claude), ("Codex fixed it on forge: the retry timer", Plain)]),
        l(&[("  ", Plain), ("raced the handshake. 20 of 20 runs pass.", Plain)]),
    ]);
    show_pane(&mut rec, "clip-lead", &pin(claude(&lead, "", WIDE_HALF), lead.len(), WIDE_ROWS));
    update_session(&mut rec, "clip-lead", |s| {
        s.status = SessionStatus::Idle;
        s.last_turn_completed_at = Some(DateMillis(4.0));
    });
    rec.shot(3200);
    rec.finish();
    cx.update(|cx| cx.set_reduce_motion(true));
}

/// A schedule woke the Mac at 6:58 and its Claude Code run triages issues.
fn record_schedules(cx: &mut HeadlessAppContext, dir: PathBuf) {
    let clip = open(
        cx,
        |t| {
            let run = |title: &str, woke: bool| ScheduledRunInfo {
                schedule_id: format!("sched-{title}"),
                title: title.into(),
                due_at: DateMillis(0.0),
                wake_mac: woke,
                woke_mac: woke,
            };
            let mut triage = session_like(
                t,
                "clip-triage",
                AgentKind::CLAUDE_CODE,
                "Triage new issues",
                SessionStatus::Working,
            );
            triage.scheduled_run = Some(run("Triage new issues", true));
            let mut errors = session_like(
                t,
                "clip-errors",
                AgentKind::CODEX,
                "Check production errors",
                SessionStatus::Idle,
            );
            errors.scheduled_run = Some(run("Check production errors", false));
            errors.last_turn_completed_at = Some(DateMillis(2.0));
            vec![triage, errors]
        },
        "clip-triage",
        None,
        WINDOW,
        crate::store::WindowMaterial::Opaque,
        TabOrientation::Vertical,
    );
    let mut rec = Recorder::new(cx, clip.window, dir);
    use Hue::*;
    let mut lines = claude_banner("~/acme/app");
    lines.extend([
        l(&[("> ", Dim), ("Triage new GitHub issues and label them. Fix anything small and open a PR.", Plain)]),
        l(&[("  ", Dim), ("Scheduled by diri · weekdays at 7:00 · diri woke this Mac at 6:58", Purple)]),
        p(""),
    ]);
    show(&mut rec, &claude(&lines, "", FULL));
    rec.shot(1400);
    let blocks: Vec<Vec<Line>> = vec![
        vec![
            l(&[("● ", Green), ("Bash", Bold), ("(gh issue list --state open --search \"no:label\" --limit 50)", Plain)]),
            l(&[("  ⎿ ", Dim), ("#431  Sidebar flickers when a session is renamed", Plain)]),
            l(&[("     ", Dim), ("#433  App crashes on resume after sleep", Plain)]),
            l(&[("     ", Dim), ("#436  Option to hide the usage footer", Plain)]),
            l(&[("     ", Dim), ("… +6 issues", Dim)]),
            p(""),
        ],
        vec![
            l(&[("● ", Green), ("Bash", Bold), ("(gh issue edit 431 433 437 438 --add-label bug)", Plain)]),
            l(&[("  ⎿ ", Dim), ("Labelled 9 issues: 4 bug, 3 enhancement, 2 docs", Plain)]),
            p(""),
        ],
        vec![
            l(&[("● ", Claude), ("#433 is real: resuming after sleep reads a socket that was closed. Reproduced it; the fix is small.", Plain)]),
            p(""),
        ],
        vec![
            l(&[("● ", Green), ("Update", Bold), ("(crates/engine/src/resume.rs)", Plain)]),
            l(&[("  ⎿ ", Dim), ("Updated with ", Plain), ("8 additions", Green), (" and ", Plain), ("2 removals", Red)]),
            p(""),
        ],
        vec![
            l(&[("● ", Green), ("Bash", Bold), ("(cargo test -p engine resume && gh pr create --fill)", Plain)]),
            l(&[("  ⎿ ", Dim), ("test result: ok. 41 passed", Green)]),
            l(&[("     ", Dim), ("https://github.com/acme/app/pull/812", Blue)]),
            p(""),
        ],
        vec![
            l(&[("● ", Claude), ("Done before you're up. PR #812 fixes the resume crash.", Plain)]),
            l(&[("  ", Plain), ("Two need you: #436 is a design call, and #438 needs steps to reproduce.", Plain)]),
        ],
    ];
    for block in blocks {
        lines.extend(block);
        show(&mut rec, &claude(&lines, "", FULL));
        rec.shot(850);
    }
    update_session(&mut rec, "clip-triage", |s| {
        s.status = SessionStatus::Idle;
        s.last_turn_completed_at = Some(DateMillis(5.0));
    });
    rec.shot(3200);
    rec.finish();
}

/// Selects `id`'s tab the way a click or shortcut does.
fn select(rec: &mut Recorder, id: &str) {
    for index in 0..8 {
        let selected = rec
            .cx
            .update_window(rec.window, |view, _, cx| {
                let root = view.downcast::<RootView>().unwrap();
                root.update(cx, |root, cx| root.select_session_shortcut(index, cx));
                let root = root.read(cx);
                let store = root.window_store.read().unwrap();
                store.selected_session_id().is_some_and(|s| s.0 == id)
            })
            .unwrap();
        rec.cx.run_until_parked();
        if selected {
            return;
        }
    }
    panic!("no tab for {id}");
}

/// Pi: the transcript, then its editor between rules and the footer.
fn pi(body: &[Line], working: bool, rows: usize) -> Screen {
    let mut lines = body.to_vec();
    lines.push(p(""));
    if working {
        lines.push(l(&[("⠹ ", Hue::Blue), ("Working…", Hue::Dim)]));
    }
    let pad = rows.saturating_sub(lines.len() + 4);
    lines.extend((0..pad).map(|_| p("")));
    lines.push(rule(SWARM_COLS));
    let row = lines.len();
    lines.push(p(""));
    lines.push(rule(SWARM_COLS));
    lines.push(l(&[("~/acme/ios (main)", Hue::Dim)]));
    Screen {
        lines,
        cursor: (row, 0),
    }
}

/// Three agents in horizontal tabs, each driving its own Simulator through
/// the computer tool. Claude Code starts Codex and Pi; the window follows each
/// to its tab as its Simulator opens, then comes back to Claude's. The
/// Simulator windows are drawn over the frames by the encoder
/// (`scripts/site-clip-simulator.mjs`), timed by the marks.
fn record_computer(cx: &mut HeadlessAppContext, dir: PathBuf) {
    let clip = open(
        cx,
        |t| {
            let tab = |id, kind, title| session_like(t, id, kind, title, SessionStatus::Working);
            vec![
                tab("clip-sim", AgentKind::CLAUDE_CODE, "Notifications"),
                tab("clip-sim-se", AgentKind::CODEX, "iPhone SE"),
                tab("clip-sim-tour", AgentKind::new("pi"), "Dark mode"),
            ]
        },
        "clip-sim",
        None,
        WINDOW,
        crate::store::WindowMaterial::Opaque,
        TabOrientation::Horizontal,
    );
    // The tab spinners turn, so motion stays on and the clock steps with
    // the frames.
    cx.update(|cx| cx.set_reduce_motion(false));
    let mut rec = Recorder::new(cx, clip.window, dir).animated(125);
    use Hue::*;
    let call = |tool: &str, args: &str| {
        l(&[
            ("● ", Green),
            ("dirijor - ", Bold),
            (tool, Bold),
            (" (MCP)", Dim),
            (args, Plain),
        ])
    };
    let result = |text: &str| l(&[("  ⎿  ", Dim), (text, Plain)]);
    let mut lead = claude_banner("~/acme/ios");
    lead.extend([
        l(&[("> ", Dim), ("Check the onboarding on three phones. Split it with Codex and Pi.", Plain)]),
        p(""),
    ]);
    let done = [
        p(""),
        l(&[("● ", Claude), ("All three pass. Your cursor and keyboard were never used.", Plain)]),
    ];
    let claude_rows = {
        let mut all = lead.clone();
        all.extend([
            call("spawn_agents", ""), result(""), result(""), p(""),
            call("computer", ""), call("computer", ""), result(""), p(""),
            call("wait_for_children", ""), result(""), result(""), p(""),
            l(&[]), l(&[]),
        ]);
        all.extend(done.clone());
        claude(&all, "", SWARM_COLS).lines.len().max(SWARM_ROWS)
    };
    let claude_at = |body: &[Line], working: Option<(&str, &str)>| {
        let mut body = body.to_vec();
        if let Some((verb, time)) = working {
            body.extend([p(""), l(&[("✻ ", Claude), (verb, Claude), (&format!(" ({time} · esc to interrupt)"), Dim)])]);
        }
        let mut screen = claude(&body, "", SWARM_COLS);
        let pad = claude_rows.saturating_sub(screen.lines.len());
        let at = body.len() - working.map_or(0, |_| 2);
        screen.lines.splice(at..at, (0..pad).map(|_| p("")));
        screen.cursor.0 += pad;
        screen
    };

    // Claude Code takes the task and hands two phones to Codex and Pi.
    show(&mut rec, &claude_at(&lead, Some(("Thinking…", "0m 04s"))));
    rec.shot(800);
    lead.extend([
        call("spawn_agents", "(codex: iPhone SE, pi: 17 Pro Max)"),
        result("Codex signs in on the SE"),
        result("Pi walks the tour in dark mode"),
        p(""),
    ]);
    show(&mut rec, &claude_at(&lead, Some(("Delegating…", "0m 09s"))));
    rec.mark("spawn");
    rec.shot(900);

    // Codex opens the iPhone SE and signs in.
    select(&mut rec, "clip-sim-se");
    let mut work = codex_banner("~/acme/ios");
    work.extend([
        l(&[("› ", Bold), ("Sign in on the iPhone SE and check the welcome screen.", Plain)]),
        p(""),
        l(&[("• ", Green), ("Called ", Bold), ("dirijor.computer", Plain), ("(launch_app: iPhone SE)", Dim)]),
    ]);
    show(&mut rec, &pin(codex(&work, Some("0m 03s")), work.len(), SWARM_ROWS));
    rec.mark("codex");
    rec.shot(1100);
    work.extend([
        l(&[("• ", Green), ("Called ", Bold), ("dirijor.computer", Plain), ("(type_text: sam@acme.dev)", Dim)]),
        l(&[("• ", Green), ("Called ", Bold), ("dirijor.computer", Plain), ("(click: Continue)", Dim)]),
    ]);
    show(&mut rec, &pin(codex(&work, Some("0m 07s")), work.len(), SWARM_ROWS));
    rec.shot(1200);

    // Pi opens the 17 Pro Max in dark mode and pages through the tour.
    select(&mut rec, "clip-sim-tour");
    let mut tour = vec![
        l(&[("pi", Bold), (" v0.31 · ~/acme/ios", Dim)]),
        p(""),
        l(&[("Walk the onboarding tour on the iPhone 17 Pro Max in dark mode.", Bold)]),
        p(""),
        l(&[("computer ", Blue), ("launch_app iPhone 17 Pro Max · appearance dark", Plain)]),
    ];
    show(&mut rec, &pi(&tour, true, SWARM_ROWS));
    rec.mark("pi");
    rec.shot(1100);
    tour.push(l(&[("computer ", Blue), ("click Next · click Next · click Get started", Plain)]));
    show(&mut rec, &pi(&tour, true, SWARM_ROWS));
    rec.shot(1300);

    // Back to Claude Code, which turns on notifications on its own phone.
    select(&mut rec, "clip-sim");
    lead.extend([
        call("computer", "(launch_app: iPhone 17 Pro)"),
        call("computer", "(click: Allow)"),
        result("Notifications on"),
        p(""),
    ]);
    show(&mut rec, &claude_at(&lead, Some(("Checking…", "0m 31s"))));
    rec.mark("claude");
    rec.shot(2600);
    lead.extend([
        call("wait_for_children", ""),
        result("Codex: the page dots overlap Skip on the SE"),
        result("Pi: the tour reads fine in dark mode"),
        p(""),
    ]);
    show(&mut rec, &claude_at(&lead, Some(("Fixing…", "1m 04s"))));
    rec.mark("wait");
    rec.shot(1700);
    lead.extend([
        l(&[("● ", Green), ("Update", Bold), ("(ios/Onboarding/PageDots.swift)", Plain)]),
        l(&[("  ⎿  ", Dim), ("Updated with ", Plain), ("6 additions", Green), (" and ", Plain), ("2 removals", Red)]),
    ]);
    show(&mut rec, &claude_at(&lead, Some(("Rechecking…", "1m 40s"))));
    rec.mark("fix");
    rec.shot(1500);
    lead.extend(done);
    show(&mut rec, &claude_at(&lead, None));
    for id in ["clip-sim", "clip-sim-se", "clip-sim-tour"] {
        update_session(&mut rec, id, |s| {
            s.status = SessionStatus::Idle;
            s.last_turn_completed_at = Some(DateMillis(5.0));
        });
    }
    rec.mark("done");
    rec.shot(3200);
    rec.finish();
    cx.update(|cx| cx.set_reduce_motion(true));
}

/// Claude Code finishes and hands diri a summary to read aloud; dictation
/// hears the answer, pastes it into the prompt, and it is sent.
fn record_voice(cx: &mut HeadlessAppContext, dir: PathBuf) {
    let clip = open(
        cx,
        |t| {
            vec![session_like(
                t,
                "clip-voice",
                AgentKind::CLAUDE_CODE,
                "Fix the flaky reconnect test",
                SessionStatus::Working,
            )]
        },
        "clip-voice",
        None,
        WINDOW,
        crate::store::WindowMaterial::Opaque,
        TabOrientation::Vertical,
    );
    let listen = dir.join("listen.txt");
    let mut rec = Recorder::new(cx, clip.window, dir);
    use Hue::*;
    let mut lines: Vec<Line> = vec![
        l(&[("> ", Dim), ("Find out why the reconnect test is flaky and fix it. I'll be away from the desk.", Plain)]),
        p(""),
        l(&[("● ", Green), ("Update", Bold), ("(src/retry.rs)", Plain)]),
        l(&[("  ⎿ ", Dim), ("Updated with ", Plain), ("14 additions", Green), (" and ", Plain), ("3 removals", Red)]),
        p(""),
        l(&[("● ", Green), ("Bash", Bold), ("(cargo test -p reconnect -- --test-threads 32 ×20 && gh pr create --fill)", Plain)]),
        l(&[("  ⎿ ", Dim), ("test result: ok. 20 of 20 runs passed", Green)]),
        l(&[("     ", Dim), ("https://github.com/acme/app/pull/812", Blue)]),
        p(""),
        l(&[("● ", Claude), ("Fixed: the retry timer could fire before the handshake resolved. PR #812 is open.", Plain)]),
        p(""),
    ];
    show(&mut rec, &claude(&lines, "", FULL));
    rec.shot(900);
    lines.extend([
        l(&[("● ", Green), ("dirijor - speak", Bold), (" (MCP)", Dim)]),
        l(&[("  ⎿ ", Dim), ("“The reconnect test passes twenty runs in a row and PR 812 is open. Want me to merge it?”", Plain)]),
        l(&[("     ", Dim), ("Read aloud by diri", Purple)]),
    ]);
    show(&mut rec, &claude(&lines, "", FULL));
    update_session(&mut rec, "clip-voice", |s| {
        s.status = SessionStatus::Idle;
        s.last_turn_completed_at = Some(DateMillis(3.0));
    });
    rec.shot(3000);
    // Dictation listens, then pastes the answer; "send to agent" submits it.
    // The site draws the dictation pill over the frames listen.txt names.
    let mut listening = String::new();
    for frame in 2..24 {
        listening.push_str(&format!("frame_{frame:04}.png\n"));
        show(&mut rec, &claude(&lines, "", FULL));
        rec.shot(100);
    }
    std::fs::write(listen, listening).unwrap();
    show(&mut rec, &claude(&lines, "Yes, merge it, then start on the release notes.", FULL));
    rec.shot(900);
    lines.extend([
        p(""),
        l(&[("> ", Dim), ("Yes, merge it, then start on the release notes.", Plain)]),
        p(""),
        l(&[("● ", Green), ("Bash", Bold), ("(gh pr merge 812 --squash --delete-branch)", Plain)]),
        l(&[("  ⎿ ", Dim), ("Merged #812 into main", Green)]),
    ]);
    update_session(&mut rec, "clip-voice", |s| s.status = SessionStatus::Working);
    show(&mut rec, &claude(&lines, "", FULL));
    rec.shot(2800);
    rec.finish();
}

#[test]
#[ignore = "writes diri.sh feature clip frames to the DIRI_VISUAL_OUTPUT directory"]
fn render_site_clips() {
    let output = PathBuf::from(std::env::var("DIRI_VISUAL_OUTPUT").unwrap());
    let only = std::env::var("DIRI_CLIP").ok();
    // SAFETY: set before the app context starts any thread.
    unsafe { std::env::set_var("USER", "alex") };
    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        // Transparent, so a glass clip keeps its alpha; opaque clips paint
        // every pixel anyway.
        || -> Option<Box<dyn gpui::PlatformHeadlessRenderer>> {
            Some(Box::new(gpui_macos::metal_renderer::MetalHeadlessRenderer::transparent()))
        },
    );
    cx.update(|cx| {
        crate::fonts::init(cx);
        cx.set_reduce_motion(true);
    });
    diri_ui::set_mark_rasterizer(crate::macos::brand_raster::raster_mark);
    type Record = fn(&mut HeadlessAppContext, PathBuf);
    let clips: [(&str, Record); 4] = [
        ("agents", record_agents),
        ("schedules", record_schedules),
        ("computer", record_computer),
        ("voice", record_voice),
    ];
    for (name, record) in clips {
        if only.as_deref().is_some_and(|only| only != name) {
            continue;
        }
        record(&mut cx, output.join(name));
    }
}
