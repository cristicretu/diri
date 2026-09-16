use super::*;
use gpui::{
    App, AppContext, AsyncApp, Bounds, WindowBounds, WindowHandle, WindowOptions, point, size,
};
use objc2::{Encode, Encoding, msg_send, rc::Retained};
use objc2_app_kit::NSView;
use objc2_foundation::{NSNotFound, NSString};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

// GPUI's current Cocoa responder registers this ABI under "NSRange", while
// objc2-foundation uses "_NSRange". Match the installed method's exact encoding.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct NativeRange {
    location: usize,
    length: usize,
}
unsafe impl Encode for NativeRange {
    const ENCODING: Encoding = Encoding::Struct("NSRange", &[usize::ENCODING, usize::ENCODING]);
}
impl NativeRange {
    fn new(location: usize, length: usize) -> Self {
        Self { location, length }
    }
}

/// Runs only through the harness-free test executable on the AppKit main thread.
pub(crate) fn run_native() {
    let fixture = crate::workspace_fixture::LiveWorkspace::start_with_script(
        "stty raw -echo; printf 'Native find input fixture\\r\\n'; printf ready > ready; cat > received",
    );
    let services = fixture.services.clone();
    gpui_platform::application().with_assets(diri_ui::IconAssets).run(move |cx: &mut App| {
        crate::fonts::init(cx);
        crate::commands::bind_keys(cx, &Default::default());
        let window = cx.open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(120.0), px(120.0)), size(px(760.0), px(520.0))))),
            ..Default::default()
        }, |window, cx| {
            cx.new(|cx| {
                let mut pane = TerminalPane::new_fixed(services.store.clone(), services.tokio.clone(), SessionId::new("build"), window, cx);
                pane.set_viewport(TerminalViewport { x:0.0, y:0.0, width:760.0, height:520.0 }, cx);
                pane.focus(window, cx);
                pane
            })
        }).unwrap();
        cx.activate(true);
        cx.spawn(async move |cx| {
            let result = exercise_native(window, &fixture.directory.path().join("build/received"), cx).await;
            let _ = cx.update_window(window.into(), |_, window, _| window.remove_window());
            let received = std::fs::read(fixture.directory.path().join("build/received")).unwrap_or_default();
            fixture.verify_process_identity();
            drop(fixture);
            match result {
                Ok(()) if received == b"controlafter" => {
                    println!("Native Find IME query routing passed; no composed bytes reached the PTY.");
                    std::process::exit(0);
                }
                other => {
                    eprintln!("Native Find IME query routing failed: {other:?}; PTY received {received:?}");
                    std::process::exit(1);
                }
            }
        }).detach();
    });
}

async fn exercise_native(
    window: WindowHandle<TerminalPane>,
    received_path: &std::path::Path,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let id = SessionId::new("build");
    let mut ready = false;
    for _ in 0..100 {
        cx.background_executor()
            .timer(Duration::from_millis(20))
            .await;
        ready = window
            .update(cx, |pane, native, cx| {
                pane.focus(native, cx);
                // This executable has no signed app bundle and AppKit may keep it
                // inactive. Claim only this disposable controller to isolate the
                // installed native input handler; this is not an OS focus test.
                pane.claim_selected_control();
                pane.residents.get(&id).is_some_and(|resident| {
                    resident.attachment_state == AttachmentState::Live
                        && resident.attachment.is_controller()
                })
            })
            .map_err(|e| e.to_string())?;
        if ready {
            break;
        }
    }
    if !ready {
        let status = window
            .update(cx, |pane, native, _| {
                format!(
                    "active={}, focused={}, resident={:?}",
                    native.is_window_active(),
                    pane.focus.is_focused(native),
                    pane.residents
                        .get(&id)
                        .map(|r| (r.attachment_state, r.attachment.is_controller()))
                )
            })
            .map_err(|e| e.to_string())?;
        return Err(format!(
            "terminal attachment did not become live with active control: {status}"
        ));
    }
    let view = cx
        .update_window(window.into(), |_, window, _cx| {
            let RawWindowHandle::AppKit(handle) = window.window_handle().unwrap().as_raw() else {
                panic!("expected actual AppKit window")
            };
            unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()).unwrap() }
        })
        .map_err(|e| e.to_string())?;
    cx.background_executor()
        .timer(Duration::from_millis(80))
        .await;
    unsafe {
        let _: () = msg_send![&*view, insertText: &*NSString::from_str("control"), replacementRange: NativeRange::new(NSNotFound as usize,0)];
    }
    cx.background_executor()
        .timer(Duration::from_millis(120))
        .await;
    let control = std::fs::read(received_path).unwrap_or_default();
    if control != b"control" {
        return Err(format!(
            "closed-Find native commit positive control failed: {control:?}"
        ));
    }
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_keystroke(gpui::Keystroke::parse("cmd-f").unwrap(), cx)
    })
    .map_err(|e| e.to_string())?;
    cx.background_executor()
        .timer(Duration::from_millis(80))
        .await;
    let find_open = window
        .update(cx, |pane, _, _| pane.residents[&id].find.is_some())
        .map_err(|e| e.to_string())?;
    if !find_open {
        return Err("Cmd+F did not open Find".into());
    }
    unsafe {
        let _: () = msg_send![&*view, setMarkedText: &*NSString::from_str("ni"), selectedRange: NativeRange::new(2,0), replacementRange: NativeRange::new(NSNotFound as usize,0)];
        let marked: NativeRange = msg_send![&*view, markedRange];
        if marked.length != 2 {
            return Err(format!("native marked range was {marked:?}"));
        }
        let _: () = msg_send![&*view, insertText: &*NSString::from_str("你"), replacementRange: NativeRange::new(NSNotFound as usize,0)];
    }
    cx.background_executor()
        .timer(Duration::from_millis(120))
        .await;
    let query = window
        .update(cx, |pane, _, _| {
            pane.residents[&id].find_query.text().to_owned()
        })
        .map_err(|e| e.to_string())?;
    if query != "你" {
        return Err(format!(
            "Find query should contain the composed text, got {query:?}"
        ));
    }
    let geometry_before = window
        .update(cx, |pane, _, _| pane.residents[&id].last_size)
        .map_err(|e| e.to_string())?;
    unsafe {
        let _: () = msg_send![&*view, setMarkedText: &*NSString::from_str("😀e\u{301}"), selectedRange: NativeRange::new(2,2), replacementRange: NativeRange::new(NSNotFound as usize,0)];
        let marked: NativeRange = msg_send![&*view, markedRange];
        let selected: NativeRange = msg_send![&*view, selectedRange];
        if (marked.location, marked.length) != (1, 4)
            || (selected.location, selected.length) != (3, 2)
        {
            return Err(format!(
                "UTF16 composition ranges incorrect: marked={marked:?}, selected={selected:?}"
            ));
        }
    }
    cx.background_executor()
        .timer(Duration::from_millis(80))
        .await;
    let candidate = || unsafe {
        let null: *mut objc2::runtime::AnyObject = std::ptr::null_mut();
        let rect: objc2_foundation::NSRect =
            msg_send![&*view, firstRectForCharacterRange: NativeRange::new(5,0), actualRange: null];
        rect
    };
    let before = candidate();
    window
        .update(cx, |pane, _, cx| {
            pane.residents[&id]
                .element
                .set_find_highlights(vec![diri_term::find::FindSpan {
                    row: 0,
                    start_col: 0,
                    end_col_exclusive: 200,
                    is_current: true,
                }]);
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
    cx.background_executor()
        .timer(Duration::from_millis(80))
        .await;
    let after = candidate();
    if before.size.width != 1.0 || before.size.height != 18.0 || after.origin.y >= before.origin.y {
        return Err(format!(
            "candidate caret did not follow relocated query: {before:?} -> {after:?}"
        ));
    }
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_keystroke(gpui::Keystroke::parse("escape").unwrap(), cx)
    })
    .map_err(|e| e.to_string())?;
    cx.background_executor()
        .timer(Duration::from_millis(80))
        .await;
    window
        .update(cx, |pane, _, _| {
            assert!(pane.residents[&id].find.is_none());
            assert_eq!(
                pane.residents[&id].find_query.text(),
                "你",
                "close cancels preedit"
            );
            assert_eq!(pane.residents[&id].last_size, geometry_before);
        })
        .map_err(|e| e.to_string())?;
    if std::fs::read(received_path).unwrap_or_default() != b"control" {
        return Err("composition/cancel leaked terminal bytes".into());
    }
    unsafe {
        let marked: NativeRange = msg_send![&*view, markedRange];
        if marked.location != NSNotFound as usize {
            return Err(format!("composition remained after close: {marked:?}"));
        }
        let _: () = msg_send![&*view, insertText: &*NSString::from_str("after"), replacementRange: NativeRange::new(NSNotFound as usize,0)];
    }
    cx.background_executor()
        .timer(Duration::from_millis(120))
        .await;
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_keystroke(gpui::Keystroke::parse("cmd-f").unwrap(), cx)
    })
    .map_err(|e| e.to_string())?;
    cx.background_executor()
        .timer(Duration::from_millis(80))
        .await;
    unsafe {
        let _: () = msg_send![&*view, setMarkedText: &*NSString::from_str("draft"), selectedRange: NativeRange::new(5,0), replacementRange: NativeRange::new(NSNotFound as usize,0)];
    }
    let other_focus = cx
        .update_window(window.into(), |_, window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus, cx);
            focus
        })
        .map_err(|e| e.to_string())?;
    // Callback from the previously installed handler before the next paint:
    // losing its originating focus must reject it synchronously.
    unsafe {
        let _: () = msg_send![&*view, insertText: &*NSString::from_str("stale"), replacementRange: NativeRange::new(NSNotFound as usize,0)];
    }
    cx.background_executor()
        .timer(Duration::from_millis(80))
        .await;
    let restored = window
        .update(cx, |pane, _, _| {
            pane.residents[&id].find_query.text().to_owned()
        })
        .map_err(|e| e.to_string())?;
    if restored != "你" {
        return Err(format!(
            "focus loss failed to cancel query composition: {restored:?}"
        ));
    }
    drop(other_focus);
    window
        .update(cx, |pane, window, cx| pane.focus(window, cx))
        .map_err(|e| e.to_string())?;
    cx.background_executor()
        .timer(Duration::from_millis(80))
        .await;
    unsafe {
        let _: () = msg_send![&*view, insertText: &*NSString::from_str("好"), replacementRange: NativeRange::new(NSNotFound as usize,0)];
    }
    let reopened = window
        .update(cx, |pane, _, _| {
            pane.residents[&id].find_query.text().to_owned()
        })
        .map_err(|e| e.to_string())?;
    if reopened != "好" {
        return Err(format!(
            "refocused query did not replace its saved selection: {reopened:?}"
        ));
    }
    cx.background_executor()
        .timer(Duration::from_millis(80))
        .await;
    Ok(())
}

#[test]
#[ignore = "native Metal Find geometry with disposable PTYs paused before query; optional DIRI_FIND_CAPTURES"]
fn real_pty_find_preserves_geometry_across_fonts_and_widths() {
    let output_dir = std::env::var_os("DIRI_FIND_CAPTURES").map(std::path::PathBuf::from);
    if let Some(directory) = &output_dir {
        std::fs::create_dir_all(directory).unwrap();
    }
    for (name, width, font_size, light) in [
        ("wide-dark", 900.0, 17.0, false),
        ("narrow-light", 440.0, 21.0, true),
    ] {
        let fixture = crate::workspace_fixture::LiveWorkspace::start_with_script(
            r#"stty -echo; printf ready > ready; while IFS= read -r line; do stty size > geometry; set -- $(stty size); printf '\033[2J\033[H'; printf '%*s界 é 😀\r\n' "$(($2 - 10))" ''; printf 'Live shell output: %s\r\n\r\n' "$line"; printf 'Unicode search: 界 and café and 😀. This ordinary shell line wraps to the actual PTY width.\r\n'; done"#,
        );
        fixture
            .services
            .store
            .store
            .write()
            .unwrap()
            .update_preferences(|prefs| {
                prefs.terminal_font_size = font_size;
                prefs.terminal_theme = if light { "dirijor-light" } else { "dirijor" }.into();
            })
            .unwrap();
        let output = fixture.continuous_output();
        let platform = gpui_platform::current_platform(true);
        let mut cx = gpui::HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            crate::commands::bind_keys(cx, &Default::default());
        });
        let services = fixture.services.clone();
        let id = SessionId::new("build");
        let window = cx
            .open_window(size(px(width), px(520.0)), |window, cx| {
                cx.new(|cx| {
                    let mut pane = TerminalPane::new_fixed(
                        services.store.clone(),
                        services.tokio.clone(),
                        SessionId::new("build"),
                        window,
                        cx,
                    );
                    pane.set_viewport(
                        TerminalViewport {
                            x: 0.0,
                            y: 0.0,
                            width,
                            height: 520.0,
                        },
                        cx,
                    );
                    pane.focus(window, cx);
                    pane
                })
            })
            .unwrap();
        macro_rules! settle {
            () => {
                // Pump asynchronous read/scan completions independently of the
                // synthetic frame cadence, as the native event loop does.
                for tick in 0..650 {
                    cx.advance_clock(Duration::from_millis(1));
                    if tick % 16 == 0 {
                        cx.update_window(window.into(), |_, window, cx| {
                            window.simulate_next_frame(cx)
                        })
                        .unwrap();
                    }
                    cx.run_until_parked();
                    std::thread::sleep(Duration::from_millis(1));
                }
            };
        }
        macro_rules! key {
            ($key:expr) => {
                cx.update_window(window.into(), |_, window, cx| {
                    window.dispatch_keystroke(gpui::Keystroke::parse($key).unwrap(), cx)
                })
                .unwrap();
                cx.run_until_parked();
            };
        }
        settle!();
        window
            .update(&mut cx, |pane, window, cx| {
                window.activate_window();
                pane.focus(window, cx);
            })
            .unwrap();
        settle!();
        let original = window
            .update(&mut cx, |pane, _, _| {
                let resident = &pane.residents[&id];
                assert_eq!(resident.attachment_state, AttachmentState::Live);
                assert!(resident.attachment.is_controller());
                resident.last_size
            })
            .unwrap();
        fixture.verify_geometry(&[(id.clone(), original.0, original.1)]);
        let output_ticks = output.ticks();
        // Isolate geometry/composition acceptance from the separately recorded
        // continuous-output search invalidation regression. The captured cells
        // still come from a real PTY and its actual resized shell output.
        drop(output);
        settle!();
        key!("cmd-f");
        key!("界");
        settle!();
        window
            .update(&mut cx, |pane, _, _| {
                let resident = &pane.residents[&id];
                assert_eq!(resident.find_query.text(), "界");
                assert!(
                    !resident.find.as_ref().unwrap().matches().is_empty(),
                    "real Engine snapshot search after fixture output settles: {:?}; scheduler={:?}",
                    resident.find,
                    resident.find_scheduler
                );
                assert_eq!(resident.last_size, original);
            })
            .unwrap();
        if let Some(directory) = &output_dir {
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(directory.join(format!("{name}-wide-glyph.png")))
                .unwrap();
        }
        key!("cmd-a");
        key!("e");
        // Combining mark is an input callback unit; this dispatch uses the
        // normal query key path, while Cocoa composition is covered above.
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_keystroke(
                gpui::Keystroke {
                    key: "́".into(),
                    key_char: Some("́".into()),
                    modifiers: Default::default(),
                },
                cx,
            )
        })
        .unwrap();
        settle!();
        window
            .update(&mut cx, |pane, _, _| {
                let resident = &pane.residents[&id];
                assert_eq!(resident.find_query.text(), "e\u{301}");
                assert!(!resident.find.as_ref().unwrap().matches().is_empty());
                assert_eq!(resident.last_size, original);
            })
            .unwrap();
        if let Some(directory) = &output_dir {
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(directory.join(format!("{name}-combining.png")))
                .unwrap();
        }
        key!("escape");
        settle!();
        fixture.verify_geometry(&[(id.clone(), original.0, original.1)]);
        assert!(
            output_ticks >= 20,
            "fixture exercised live PTY output before pausing"
        );
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }
}
