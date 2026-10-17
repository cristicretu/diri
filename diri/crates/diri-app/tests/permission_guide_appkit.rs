//! Shows the permission guide for real, over System Settings, for a look or a
//! screenshot. Opt in with
//! `DIRI_TEST_PERMISSION_GUIDE=accessibility cargo test -p diri-app --test permission_guide_appkit`
//! (or `=screen-recording`); `DIRI_TEST_PERMISSION_GUIDE_SECONDS` keeps it up
//! for that long (default 20), and `DIRI_TEST_PERMISSION_GUIDE_APP` picks the
//! app to offer (default /Applications/Diri.app). Opens System Settings.
//! `DIRI_TEST_PERMISSION_GUIDE=render:<dir>` instead draws the panel, light
//! and dark, to `<dir>/guide-light.png` and `<dir>/guide-dark.png`.
#[cfg(target_os = "macos")]
#[allow(dead_code, unused_imports)]
#[path = "../src/macos/permission_guide.rs"]
mod permission_guide;

fn main() {
    #[cfg(target_os = "macos")]
    if let Some(pane) = std::env::var_os("DIRI_TEST_PERMISSION_GUIDE") {
        show(&pane.to_string_lossy());
        return;
    }
    println!(
        "Permission guide demo skipped; opt in on macOS with DIRI_TEST_PERMISSION_GUIDE=accessibility."
    );
}

#[cfg(target_os = "macos")]
fn show(pane: &str) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::{NSDate, NSRunLoop};
    use permission_guide::Pane;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    let mtm = MainThreadMarker::new().expect("harness = false runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon, and never the active app: Settings keeps focus.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    if let Some(dir) = pane.strip_prefix("render:") {
        let bundle = std::env::var_os("DIRI_TEST_PERMISSION_GUIDE_APP")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Applications/Diri.app"));
        for (dark, file) in [(false, "guide-light.png"), (true, "guide-dark.png")] {
            let path = PathBuf::from(dir).join(file);
            assert!(
                permission_guide::render_png(
                    mtm,
                    &bundle,
                    "Diri",
                    "Drag Diri to the list above to allow Accessibility",
                    dark,
                    &path,
                ),
                "rendered {}",
                path.display()
            );
            println!("wrote {}", path.display());
        }
        return;
    }
    let (pane, what) = match pane {
        "screen-recording" => (Pane::ScreenRecording, "Screen Recording"),
        _ => (Pane::Accessibility, "Accessibility"),
    };
    let bundle = std::env::var_os("DIRI_TEST_PERMISSION_GUIDE_APP")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Applications/Diri.app"));
    let seconds: u64 = std::env::var("DIRI_TEST_PERMISSION_GUIDE_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    let name = bundle
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let closed = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = closed.clone();
    permission_guide::present(
        mtm,
        pane,
        &bundle,
        &name,
        &format!("Drag {name} to the list above to allow {what}"),
        Box::new(move || flag.set(true)),
    );
    println!("guide pid {}", std::process::id());
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline && !closed.get() {
        app.updateWindows();
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05));
        // Events too, so the close button and drags work.
        while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            objc2_app_kit::NSEventMask::Any,
            Some(&NSDate::distantPast()),
            unsafe { objc2_foundation::NSDefaultRunLoopMode },
            true,
        ) {
            app.sendEvent(&event);
        }
    }
    println!("presented at exit: {}", permission_guide::is_presented());
    permission_guide::dismiss();
    assert!(!permission_guide::is_presented());
}
