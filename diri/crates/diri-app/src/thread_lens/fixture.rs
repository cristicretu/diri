//! A deterministic thread for headless screenshots and layout tests.
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/lens-dark.png cargo test -p diri-app render_thread_lens_screenshot -- --ignored
//! DIRI_VISUAL_LIGHT=1 DIRI_VISUAL_OUTPUT=/tmp/lens-light.png cargo test -p diri-app render_thread_lens_screenshot -- --ignored
//! DIRI_VISUAL_EXPAND=1 …   # every Work strip and call opened
//! DIRI_VISUAL_PERMISSION=1 … # a pending permission card
//! DIRI_VISUAL_LIVE=1 …       # the Agent working, a message just sent
//! DIRI_VISUAL_SLASH=/co …    # the composer's `/` menu
//! DIRI_VISUAL_EMPTY=1 …      # a session with no turns yet
//! ```

use diri_proto::thread::{
    EditHunk, ItemKey, SegmentCause, THREAD_WIRE_VERSION, ThreadBody, ThreadCursor, ThreadImage,
    ThreadItem, ThreadReadResult, ThreadSegment, ToolDetail, TurnUsage,
};
use diri_proto::{AgentKind, SessionId};

pub const SESSION: &str = "lens-fixture";

fn item(offset: u64, at_ms: i64, body: ThreadBody) -> ThreadItem {
    ThreadItem {
        key: ItemKey { offset, part: 0 },
        at_ms: Some(at_ms),
        body,
    }
}

/// A small gradient PNG, `hue` shifting the palette, as base64.
pub fn screenshot(hue: u8, width: u32, height: u32) -> ThreadImage {
    use base64::Engine as _;
    let mut image = image::RgbaImage::new(width, height);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let fx = x as f32 / width as f32;
        let fy = y as f32 / height as f32;
        let band = ((fy * 6.0) as u32 % 2) as f32 * 18.0;
        *pixel = image::Rgba([
            (30.0 + fx * 120.0 + band) as u8 ^ hue,
            (40.0 + fy * 90.0) as u8,
            (120.0 + fx * 80.0) as u8 ^ (hue / 2),
            255,
        ]);
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .expect("encode fixture png");
    let base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    ThreadImage {
        media_type: "image/png".into(),
        bytes: bytes.len() as u64,
        base64: Some(base64),
    }
}

fn usage(model: &str, at_ms: i64, read: u64, write_1h: u64) -> ThreadBody {
    ThreadBody::Usage(TurnUsage {
        model: Some(model.into()),
        at_ms,
        input: 2,
        output: 400,
        cache_read: read,
        cache_write: write_1h,
        cache_write_1h: write_1h,
    })
}

pub fn segments() -> Vec<ThreadSegment> {
    let start = ThreadSegment {
        index: 0,
        kind: AgentKind::CLAUDE_CODE,
        session_id: SessionId(SESSION.into()),
        model: None,
        account: Some("Personal".into()),
        start_offset: 0,
        started_at_ms: 0,
        cause: SegmentCause::Start,
    };
    let account = ThreadSegment {
        account: Some("Work".into()),
        start_offset: 900,
        cause: SegmentCause::Account,
        ..start.clone()
    };
    vec![start, account]
}

/// `now_ms` anchors timestamps so the warmth pill reads `warm 4m12s`-ish.
pub fn conversation(now_ms: i64) -> Vec<ThreadItem> {
    let t0 = now_ms - 20 * 60 * 1000;
    let mut items = vec![
        item(0, t0, ThreadBody::User {
            text: "The streaming parser drops the last chunk when the socket closes mid-record. Fix it and add a regression test — here's what the inspector shows.".into(),
            images: vec![screenshot(0x00, 360, 200)],
        }),
        item(100, t0 + 1_000, ThreadBody::Thinking { text: "Look at the tail handling first.".into() }),
        item(110, t0 + 1_200, ThreadBody::ToolCall { call_id: "g1".into(), name: "Grep".into(), detail: ToolDetail::Search { pattern: "fn read_window".into(), path: Some("crates/diri-engine/src".into()) } }),
        item(111, t0 + 1_500, ThreadBody::ToolResult { call_id: "g1".into(), is_error: false, preview: "crates/diri-engine/src/thread/tail.rs:52".into(), images: vec![] }),
        item(120, t0 + 1_700, ThreadBody::ToolCall { call_id: "r1".into(), name: "Read".into(), detail: ToolDetail::Read { path: "crates/diri-engine/src/thread/tail.rs".into() } }),
        item(121, t0 + 1_900, ThreadBody::ToolResult { call_id: "r1".into(), is_error: false, preview: "//! Byte-offset tailing of an append-only JSONL file.".into(), images: vec![] }),
        item(130, t0 + 2_100, ThreadBody::ToolCall { call_id: "r2".into(), name: "Read".into(), detail: ToolDetail::Read { path: "crates/diri-engine/src/thread/mod.rs".into() } }),
        item(131, t0 + 2_300, ThreadBody::ToolResult { call_id: "r2".into(), is_error: false, preview: "pub fn read(".into(), images: vec![] }),
        item(140, t0 + 2_600, ThreadBody::ToolCall { call_id: "t1".into(), name: "Bash".into(), detail: ToolDetail::Command { command: "cargo test -p diri-engine thread::tail".into() } }),
        item(141, t0 + 6_800, ThreadBody::ToolResult { call_id: "t1".into(), is_error: true, preview: "test thread::tail::tests::close_mid_record ... FAILED\nassertion `left == right` failed\n  left: 2\n right: 3".into(), images: vec![] }),
        item(150, t0 + 7_000, usage("claude-opus-5-5", t0 + 7_000, 26_124, 13_075)),
        item(200, t0 + 8_000, ThreadBody::Assistant { text: "The window stops at the last newline, which is right — but `crates/diri-engine/src/thread/tail.rs:71` returns early when the **final** record has no newline yet *and* the window was full, so a record split across two reads is skipped instead of retried.\n\nThe fix:\n\n1. Treat a newline-less full window as *oversized* only when the record really exceeds `max_bytes`.\n2. Otherwise leave the cursor where it is and wait for the next append.\n3. Add `close_mid_record` covering the split.".into() }),
        item(300, t0 + 9_000, ThreadBody::ToolCall { call_id: "e1".into(), name: "Edit".into(), detail: ToolDetail::Edit {
            path: "crates/diri-engine/src/thread/tail.rs".into(),
            hunks: vec![EditHunk {
                old: "    let Some(last_newline) = buffer.iter().rposition(|&b| b == b'\\n') else {\n        return skip_oversized(file, offset, len);\n    };".into(),
                new: "    let Some(last_newline) = buffer.iter().rposition(|&b| b == b'\\n') else {\n        if want < available {\n            return skip_oversized(file, offset, len);\n        }\n        return Ok(Window::waiting(offset));\n    };".into(),
            }],
        } }),
        item(301, t0 + 9_300, ThreadBody::ToolResult { call_id: "e1".into(), is_error: false, preview: "Updated".into(), images: vec![] }),
        item(310, t0 + 9_600, ThreadBody::ToolCall { call_id: "t2".into(), name: "Bash".into(), detail: ToolDetail::Command { command: "cargo test -p diri-engine thread::".into() } }),
        item(311, t0 + 14_200, ThreadBody::ToolResult { call_id: "t2".into(), is_error: false, preview: "test result: ok. 18 passed; 0 failed".into(), images: vec![] }),
        item(320, t0 + 14_400, ThreadBody::ToolCall { call_id: "s1".into(), name: "mcp__browser__screenshot".into(), detail: ToolDetail::Other { input: "{\"page\":\"inspector\"}".into() } }),
        item(321, t0 + 15_000, ThreadBody::ToolResult { call_id: "s1".into(), is_error: false, preview: String::new(), images: vec![screenshot(0x00, 480, 300)] }),
        item(330, t0 + 15_200, ThreadBody::ToolCall { call_id: "s2".into(), name: "mcp__browser__screenshot".into(), detail: ToolDetail::Other { input: "{\"page\":\"inspector\"}".into() } }),
        item(331, t0 + 15_800, ThreadBody::ToolResult { call_id: "s2".into(), is_error: false, preview: String::new(), images: vec![screenshot(0x5a, 480, 300)] }),
        item(340, t0 + 16_000, usage("claude-opus-5-5", t0 + 16_000, 39_300, 1_200)),
        item(400, t0 + 17_000, ThreadBody::Assistant { text: "Fixed. A record split across two reads now waits for its newline instead of being skipped, and `close_mid_record` pins it.\n\n```rust\nif want < available {\n    return skip_oversized(file, offset, len);\n}\nreturn Ok(Window::waiting(offset));\n```\n\n```mermaid\ngraph LR\n  A[read_window] -->|newline| B[emit lines]\n  A -->|full, no newline| C[skip oversized]\n  A -->|partial| D[wait]\n```".into() }),
        item(500, t0 + 18_000, ThreadBody::Command { name: "/model".into(), args: "sonnet".into() }),
        item(501, t0 + 18_000, ThreadBody::CommandOutput { text: "Set model to `Sonnet 5.5` and saved as your default for new sessions".into(), is_error: false }),
        item(520, t0 + 19_000, ThreadBody::Command { name: "/cost".into(), args: String::new() }),
        item(521, t0 + 19_000, ThreadBody::CommandOutput { text: "Total cost:            $0.84\nTotal duration (API):  1m 12s\nTotal code changes:    6 lines added, 2 removed".into(), is_error: false }),
        item(510, now_ms - 252_000 - 30_000, usage("claude-sonnet-5-5", now_ms - 48_000, 0, 9_400)),
        item(600, now_ms - 47_000, ThreadBody::User { text: "Now tighten the docs on `read_window`.".into(), images: vec![] }),
        item(610, now_ms - 46_000, ThreadBody::ToolCall { call_id: "e2".into(), name: "Edit".into(), detail: ToolDetail::Edit {
            path: "crates/diri-engine/src/thread/tail.rs".into(),
            hunks: vec![EditHunk { old: "/// Reads complete lines from `offset`.".into(), new: "/// Reads complete lines from `offset` (a boundary) up to `len`.".into() }],
        } }),
        item(611, now_ms - 45_500, ThreadBody::ToolResult { call_id: "e2".into(), is_error: false, preview: "Updated".into(), images: vec![] }),
        item(620, now_ms - 45_000, usage("claude-sonnet-5-5", now_ms - 45_000, 41_000, 300)),
        item(700, now_ms - 44_000, ThreadBody::Notice { text: "Context compacted".into() }),
        item(950, now_ms - 40_000, ThreadBody::Assistant { text: "Picked up on the **Work** account with the same conversation. Docs on `read_window` now say the cursor must be a record boundary.".into() }),
        item(960, now_ms - 39_000, usage("claude-sonnet-5-5", now_ms - 39_000, 41_300, 0)),
    ];
    items.sort_by_key(|item| item.key);
    items
}

pub fn page(now_ms: i64) -> ThreadReadResult {
    ThreadReadResult {
        version: THREAD_WIRE_VERSION,
        structured: true,
        reason: None,
        reset: false,
        more: false,
        next: ThreadCursor {
            generation: "fixture".into(),
            offset: 1_000,
        },
        items: conversation(now_ms),
        segments: segments(),
    }
}

#[cfg(target_os = "macos")]
mod screenshots {
    use super::*;
    use crate::store::StoreRuntime;
    use crate::thread_lens::ThreadLens;
    use gpui::{AppContext as _, HeadlessAppContext, px, size};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn record(permission: bool, working: bool) -> serde_json::Value {
        let mut record = serde_json::json!({
            "id": SESSION, "kind": "claude-code", "cwd": env!("CARGO_MANIFEST_DIR"), "projectID": "p",
            "title": "Fix streaming parser", "titleSource": serde_json::to_value(diri_proto::TitleSource::UserRename).unwrap(),
            "status": serde_json::to_value(if working { diri_proto::SessionStatus::Working } else { diri_proto::SessionStatus::Idle }).unwrap(),
            "resumability": serde_json::to_value(diri_proto::Resumability::Live).unwrap(),
            "createdAt": 0.0, "updatedAt": 0.0, "pinned": false,
        });
        if permission {
            record["needsInput"] = serde_json::json!({
                "kind": serde_json::to_value(diri_proto::NeedsInputKind::Permission).unwrap(),
                "source": serde_json::to_value(diri_proto::NeedsInputSource::ClaudePermissionHook).unwrap(),
                "toolName": "Bash",
                "summary": "Run git push",
                "promptExcerpt": "git push origin feat/dual-lens-gui",
                "riskHint": serde_json::to_value(diri_proto::RiskHint::Network).unwrap(),
                "occurredAt": 1.0,
            });
        }
        record
    }

    /// Opens the lens over a seeded thread and writes a PNG.
    #[test]
    #[ignore = "writes the GUI Lens screenshot artifact"]
    fn render_thread_lens_screenshot() {
        let output = std::env::var_os("DIRI_VISUAL_OUTPUT")
            .map(PathBuf::from)
            .expect("set DIRI_VISUAL_OUTPUT to the target PNG path");
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            // A solid caret: the blink must not decide what the capture shows.
            cx.set_reduce_motion(true);
        });
        let runtime = Arc::new(StoreRuntime::inert());
        let live = std::env::var_os("DIRI_VISUAL_LIVE").is_some();
        let empty = std::env::var_os("DIRI_VISUAL_EMPTY").is_some();
        let slash = std::env::var("DIRI_VISUAL_SLASH").ok();
        if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() {
            runtime
                .store
                .write()
                .expect("store")
                .update_preferences(|prefs| prefs.terminal_theme = "dirijor-light".into())
                .expect("theme");
        }
        let permission = std::env::var_os("DIRI_VISUAL_PERMISSION").is_some();
        runtime.store.write().expect("store").handle_event(diri_client::EventEnvelope {
            name: diri_proto::EventName::SESSION_UPDATED.into(),
            seq: 1,
            params: record(permission, live),
        });
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio");
        let handle = tokio.handle().clone();
        let expand = std::env::var_os("DIRI_VISUAL_EXPAND").is_some();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let height = std::env::var("DIRI_VISUAL_HEIGHT")
            .ok()
            .and_then(|h| h.parse().ok())
            .unwrap_or(900.0);
        let window = cx
            .open_window(size(px(1000.0), px(height)), move |_, cx| {
                cx.new(|cx| {
                    let mut lens = ThreadLens::new(SessionId(SESSION.into()), runtime, handle, cx);
                    if !empty {
                        let mut first = page(now_ms);
                        if live {
                            first.items.push(item(990, now_ms - 2_000, ThreadBody::ToolCall {
                                call_id: "live".into(),
                                name: "Bash".into(),
                                detail: ToolDetail::Command { command: "cargo test -p diri-app thread_lens".into() },
                            }));
                        }
                        lens.ingest_for_test(first, cx);
                    }
                    if expand {
                        lens.expand_all_for_test(cx);
                    }
                    if live {
                        lens.send_for_test("Also run clippy before you push", cx);
                        // Typing while it works: the action reads Steer.
                        lens.type_for_test("and keep the old fallback", cx);
                    }
                    match &slash {
                        Some(query) => lens.type_for_test(query, cx),
                        None if !empty && !live => lens.type_for_test("Ship it — then open a PR with before/after screenshots", cx),
                        None => {}
                    }
                    lens
                })
            })
            .expect("open lens window");
        window
            .update(&mut cx, |lens, window, cx| {
                let handle = gpui::Focusable::focus_handle(lens, cx);
                window.focus(&handle, cx);
            })
            .expect("focus the composer");
        for _ in 0..3 {
            cx.run_until_parked();
            cx.update_window(window.into(), |_, window, _| window.refresh())
                .expect("refresh");
            // Mermaid draws on the background executor.
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        cx.run_until_parked();
        let screenshot = cx
            .capture_screenshot(window.into())
            .expect("capture lens screenshot");
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).expect("screenshot dir");
        }
        screenshot.save(output).expect("save lens screenshot");
        drop(tokio);
    }

    fn percentile(samples: &mut [f64], p: f64) -> f64 {
        samples.sort_by(f64::total_cmp);
        samples[((samples.len() as f64 - 1.0) * p).round() as usize]
    }

    /// Model ingest and headless frame cost over a long thread.
    /// `cargo test -p diri-app --release --bin diri bench_thread_lens -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark; prints ingest and frame timings"]
    fn bench_thread_lens() {
        use crate::thread_lens::model::ThreadModel;
        use std::time::Instant;
        let now_ms = 1_800_000_000_000;
        let base = conversation(now_ms);
        let mut items = Vec::new();
        for round in 0..200u64 {
            for item in &base {
                let mut item = item.clone();
                item.key.offset += round * 10_000;
                items.push(item);
            }
        }
        let total = items.len();
        let started = Instant::now();
        let mut model = ThreadModel::default();
        let mut whole = page(now_ms);
        whole.items = items.clone();
        model.ingest(whole);
        let cold = started.elapsed();
        let rows = model.rows.len();
        let mut appends = Vec::new();
        for n in 0..200u64 {
            let mut next = page(now_ms);
            let mut item = base[base.len() - 2].clone();
            item.key.offset = 10_000_000 + n;
            next.items = vec![item];
            let started = Instant::now();
            model.ingest(next);
            appends.push(started.elapsed().as_secs_f64() * 1e6);
        }
        println!(
            "lens model: {total} items → {rows} rows in {:.1} ms; one-item append p50 {:.1} µs p95 {:.1} µs; markdown parses {}",
            cold.as_secs_f64() * 1e3,
            percentile(&mut appends, 0.5),
            percentile(&mut appends, 0.95),
            model.parsed_documents(),
        );

        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let handle = tokio.handle().clone();
        let mut big = page(now_ms);
        big.items = items;
        let lens_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = lens_slot.clone();
        let window = cx
            .open_window(size(px(1000.0), px(900.0)), move |_, cx| {
                let lens = cx.new(|cx| {
                    let mut lens = ThreadLens::new(SessionId(SESSION.into()), runtime, handle, cx);
                    lens.ingest_for_test(big, cx);
                    lens
                });
                *slot.borrow_mut() = Some(lens.clone());
                lens
            })
            .expect("open lens window");
        cx.run_until_parked();
        let lens = lens_slot.borrow().clone().unwrap();
        let mut frames = Vec::new();
        for step in 0..240 {
            cx.update(|cx| lens.update(cx, |lens, _| lens.scroll_for_test(if step % 60 < 30 { -120.0 } else { 120.0 })));
            let started = Instant::now();
            cx.update_window(window.into(), |_, window, _| window.refresh()).unwrap();
            cx.run_until_parked();
            let _ = cx.capture_screenshot(window.into());
            frames.push(started.elapsed().as_secs_f64() * 1e3);
        }
        let mut idle = Vec::new();
        for _ in 0..60 {
            let started = Instant::now();
            cx.update_window(window.into(), |_, window, _| window.refresh()).unwrap();
            cx.run_until_parked();
            let _ = cx.capture_screenshot(window.into());
            idle.push(started.elapsed().as_secs_f64() * 1e3);
        }
        // The same capture path with an empty view of the same size: what
        // the headless renderer and texture readback cost on their own.
        struct Blank;
        impl gpui::Render for Blank {
            fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
                use gpui::Styled as _;
                gpui::div().size_full()
            }
        }
        let blank = cx
            .open_window(size(px(1000.0), px(900.0)), |_, cx| cx.new(|_| Blank))
            .expect("open blank window");
        let mut baseline = Vec::new();
        for _ in 0..120 {
            let started = Instant::now();
            cx.update_window(blank.into(), |_, window, _| window.refresh()).unwrap();
            cx.run_until_parked();
            let _ = cx.capture_screenshot(blank.into());
            baseline.push(started.elapsed().as_secs_f64() * 1e3);
        }
        let base = percentile(&mut baseline, 0.5);
        println!(
            "lens frames ({rows} rows, scrolling, headless draw+readback): p50 {:.2} ms p90 {:.2} ms p99 {:.2} ms; static redraw p50 {:.2} ms; empty-window draw+readback p50 {:.2} ms → lens cost p50 {:.2} ms p90 {:.2} ms",
            percentile(&mut frames, 0.5),
            percentile(&mut frames, 0.9),
            percentile(&mut frames, 0.99),
            percentile(&mut idle, 0.5),
            base,
            percentile(&mut frames, 0.5) - base,
            percentile(&mut frames, 0.9) - base,
        );
        drop(tokio);
    }
}
