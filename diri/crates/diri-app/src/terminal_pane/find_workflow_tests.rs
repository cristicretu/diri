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
                Ok(()) if received == b"control" => {
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
    Ok(())
}
