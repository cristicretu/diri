//! Opt-in native fixture for Agents' sidebar Browser actions
//! (`DIRI_BROWSER_PANE_SMOKE=1`): drives `crate::browser_pane` plans through
//! a real WebKit view against a loopback form, the way Root does, and fails
//! loudly on any wrong outcome. The app stays in the background (accessory,
//! never activated) and its window sits off screen. With
//! `DIRI_BROWSER_PANE_FRAMES=<dir>` it also writes PNG frames of the agent
//! cursor at work, for PR media.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEventMask, NSView,
    NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSRunLoop, NSSize};
use objc2_web_kit::WKWebView;
use raw_window_handle::{AppKitWindowHandle, HandleError, HasWindowHandle, RawWindowHandle, WindowHandle};
use serde_json::{Value, json};

use crate::browser_pane::{GLIDE_MS, Plan, World, plan, point_script, result_value};

const PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Acme · Create account</title>
<style>
  body{font:15px/1.4 -apple-system,system-ui,sans-serif;margin:0;background:#f6f5f2;color:#1d1b18}
  main{max-width:380px;margin:36px auto;padding:28px;background:#fff;border-radius:14px;box-shadow:0 1px 3px rgba(0,0,0,.08)}
  h1{font-size:22px;margin:0 0 4px} p.sub{margin:0 0 20px;color:#6b665e}
  label{display:block;font-weight:600;font-size:13px;margin:14px 0 6px}
  input[type=email],input[type=password],select{width:100%;box-sizing:border-box;padding:10px 12px;border:1px solid #d9d5ce;border-radius:9px;font:inherit}
  .row{display:flex;gap:8px;align-items:center;margin-top:16px;font-size:13px}
  button{margin-top:22px;width:100%;padding:11px;border:0;border-radius:9px;background:#1d1b18;color:#fff;font:600 15px system-ui}
  #status{margin-top:16px;font-weight:600;color:#1f7a3e}
  #error{margin-top:16px;color:#b42318}
</style></head>
<body><main>
  <h1>Create your account</h1><p class="sub">Start a 14-day trial. No card needed.</p>
  <form id="signup" novalidate>
    <label for="email">Work email</label><input id="email" type="email" placeholder="you@company.com" required>
    <label for="password">Password</label><input id="password" type="password" placeholder="At least 8 characters" required>
    <label for="plan">Plan</label><select id="plan"><option value="starter">Starter</option><option value="team">Team</option><option value="scale">Scale</option></select>
    <div class="row"><input id="terms" type="checkbox"><label for="terms" style="margin:0;font-weight:400">I agree to the terms</label></div>
    <button type="submit">Create account</button>
  </form>
  <div id="error" role="alert"></div><div id="status" role="status"></div>
</main>
<script>
  // Like React: the field's instance setter records what the page wrote, so
  // only a value that came from outside the page counts as typing.
  const tracked = document.getElementById('email');
  let pageValue = '';
  window.typed = 0;
  tracked.addEventListener('input', () => { if (tracked.value !== pageValue) window.typed++; pageValue = tracked.value; });
  document.getElementById('signup').addEventListener('submit', (event) => {
    event.preventDefault();
    const email = tracked.value, password = document.getElementById('password').value;
    document.getElementById('error').textContent = '';
    if (!document.getElementById('terms').checked) {
      console.error('signup blocked: terms not accepted');
      document.getElementById('error').textContent = 'Please accept the terms.';
      return;
    }
    if (password.length < 8) { console.warn('weak password'); }
    console.log('signup', JSON.stringify({ email, plan: document.getElementById('plan').value }));
    document.getElementById('status').textContent = `Welcome aboard, ${email}!`;
    setTimeout(() => { throw new Error('analytics beacon failed'); }, 0);
  });
</script></body></html>"#;

struct Host(Retained<NSView>);
impl HasWindowHandle for Host {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let handle = AppKitWindowHandle::new(std::ptr::NonNull::from(&*self.0).cast());
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::AppKit(handle)) })
    }
}

fn pump() {
    let app = NSApplication::sharedApplication(MainThreadMarker::new().unwrap());
    while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
        NSEventMask::Any,
        Some(&NSDate::distantPast()),
        unsafe { NSDefaultRunLoopMode },
        true,
    ) {
        app.sendEvent(&event);
    }
    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.008));
}

#[track_caller]
fn until<T>(mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = ready() {
            return value;
        }
        assert!(Instant::now() < deadline, "sidebar Browser fixture timed out");
        pump();
    }
}

fn idle(duration: Duration) {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        pump();
    }
}

fn evaluate(view: &WKWebView, body: String, world: World) -> Result<String, String> {
    let slot = Rc::new(RefCell::new(None));
    let sink = slot.clone();
    super::browser::call_async(view, &body, world, move |outcome| {
        *sink.borrow_mut() = Some(outcome);
    });
    until(|| slot.borrow_mut().take())
}

fn capture(view: &WKWebView, max_edge: f64) -> Result<(Vec<u8>, i64, i64), String> {
    let slot = Rc::new(RefCell::new(None));
    let sink = slot.clone();
    super::browser::snapshot_png(view, max_edge, move |outcome| {
        *sink.borrow_mut() = Some(outcome);
    });
    until(|| slot.borrow_mut().take())
}

struct Frames {
    dir: Option<PathBuf>,
    next: usize,
}

impl Frames {
    fn shoot(&mut self, view: &WKWebView) {
        let Some(dir) = &self.dir else { return };
        if let Ok((png, _, _)) = capture(view, 1200.0) {
            let _ = std::fs::write(dir.join(format!("frame-{:04}.png", self.next)), png);
            self.next += 1;
        }
    }

    /// A few frames across a pause, so the cursor's glide is visible.
    fn hold(&mut self, view: &WKWebView, duration: Duration) {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            self.shoot(view);
            idle(Duration::from_millis(40));
        }
    }
}

/// One Agent action, the way Root runs it: glide, pause, act.
fn act(view: &WKWebView, frames: &mut Frames, action: &str, args: Value) -> Result<Value, String> {
    match plan(action, &args)? {
        Plan::Script { point, body, world } => {
            if let Some(reference) = point {
                evaluate(view, point_script(&reference, "Codex · signup flow"), World::Client)?;
                frames.hold(view, Duration::from_millis(GLIDE_MS + 120));
            }
            let value = evaluate(view, body, world).map(|json| result_value(&json));
            frames.hold(view, Duration::from_millis(220));
            value
        }
        Plan::Screenshot => capture(view, 1568.0)
            .map(|(png, width, height)| json!({"bytes": png.len(), "width": width, "height": height})),
        other => Err(format!("fixture does not run {other:?}")),
    }
}

fn reference(outline: &str, needle: &str) -> String {
    let line = outline
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no {needle:?} in outline:\n{outline}"));
    let start = line.rfind("[e").expect("ref") + 1;
    let end = start + line[start..].find(']').expect("ref end");
    line[start..end].to_owned()
}

#[cfg(debug_assertions)]
pub fn smoke_test() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture listener");
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = stopping.clone();
    let server = std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                let mut request = [0; 4096];
                let _ = stream.read(&mut request);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PAGE}",
                    PAGE.len()
                );
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    });

    let mtm = MainThreadMarker::new().expect("fixture must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // Never take the person's focus: no Dock icon, never activated.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    let size = NSSize::new(460.0, 760.0);
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            NSRect::new(NSPoint::new(-6000.0, -6000.0), size),
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.orderBack(None);
    let host = Host(window.contentView().expect("fixture content view"));
    let (mut browser, _events) = super::browser::NativeBrowser::new();
    browser.select_tab(1);
    browser.set_visible(true);
    browser.load(format!("http://{address}/signup"));
    browser.sync(
        &host,
        true,
        super::browser::BrowserFrame {
            x: 0.0,
            y: 0.0,
            width: size.width as f32,
            height: size.height as f32,
        },
    );
    until(|| {
        let state = browser.state();
        (state.title.as_deref() == Some("Acme · Create account") && !state.is_loading).then_some(())
    });
    let view = browser.script_view(1, &host).expect("script view");
    let mut frames = Frames {
        dir: std::env::var_os("DIRI_BROWSER_PANE_FRAMES").map(PathBuf::from),
        next: 0,
    };
    if let Some(dir) = &frames.dir {
        std::fs::create_dir_all(dir).expect("frames dir");
    }
    frames.hold(&view, Duration::from_millis(300));

    let started = Instant::now();
    let snapshot = act(&view, &mut frames, "snapshot", json!({})).expect("snapshot");
    let outline = snapshot["outline"].as_str().expect("outline").to_owned();
    eprintln!("snapshot:\n{outline}");
    assert!(outline.contains("heading1 \"Create your account\""), "{outline}");
    let email = reference(&outline, "textbox \"Work email\"");
    let password = reference(&outline, "textbox \"Password\"");
    let plan_ref = reference(&outline, "combobox \"Plan\"");
    let terms = reference(&outline, "checkbox \"I agree to the terms\"");
    let submit = reference(&outline, "button \"Create account\"");

    // Submitting without the terms reports the page's own error.
    act(&view, &mut frames, "fill", json!({"ref": email, "value": "ada@acme.dev"})).expect("fill email");
    act(&view, &mut frames, "fill", json!({"ref": password, "value": "correct horse"})).expect("fill password");
    let missing = act(&view, &mut frames, "fill", json!({"ref": plan_ref, "value": "Enterprise"})).unwrap_err();
    assert!(missing.contains("\"Scale\""), "names the choices: {missing}");
    act(&view, &mut frames, "fill", json!({"ref": plan_ref, "value": "Team"})).expect("pick plan by label");
    act(&view, &mut frames, "click", json!({"ref": submit})).expect("submit");
    let after = act(&view, &mut frames, "snapshot", json!({})).expect("snapshot");
    assert!(
        after["outline"].as_str().unwrap().contains("Please accept the terms."),
        "{after}"
    );
    let console = act(&view, &mut frames, "console", json!({})).expect("console");
    assert_eq!(console["entries"][0]["level"], "error", "{console}");

    act(&view, &mut frames, "click", json!({"ref": terms})).expect("terms");
    act(&view, &mut frames, "click", json!({"ref": submit})).expect("submit again");
    let waited = act(&view, &mut frames, "wait", json!({"text": "Welcome aboard", "ms": 3000})).expect("wait");
    assert_eq!(waited["found"], true, "{waited}");
    let done = act(&view, &mut frames, "snapshot", json!({})).expect("snapshot");
    let done_outline = done["outline"].as_str().unwrap();
    assert!(done_outline.contains("Welcome aboard, ada@acme.dev!"), "{done_outline}");
    assert!(done_outline.contains("(checked)"), "{done_outline}");
    assert!(!done_outline.contains("correct horse"), "passwords are masked");
    idle(Duration::from_millis(50));
    let console = act(&view, &mut frames, "console", json!({})).expect("console");
    let levels: Vec<&str> = console["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["level"].as_str().unwrap())
        .collect();
    assert!(levels.contains(&"log"), "{console}");
    assert!(levels.contains(&"uncaught"), "uncaught errors are recorded: {console}");
    let typed = act(&view, &mut frames, "eval", json!({"script": "window.typed"})).expect("eval");
    assert_eq!(typed, json!(1), "the page saw exactly one typed email");
    let plan_value = act(&view, &mut frames, "eval", json!({"script": "const p = document.getElementById('plan');\nreturn p.value;"})).expect("eval body");
    assert_eq!(plan_value, json!("team"));
    let stale = act(&view, &mut frames, "click", json!({"ref": "e999"})).unwrap_err();
    assert!(stale.contains("take a new snapshot"), "{stale}");
    let thrown = act(&view, &mut frames, "eval", json!({"script": "missingThing.value"})).unwrap_err();
    assert!(thrown.contains("missingThing"), "{thrown}");
    let shot = act(&view, &mut frames, "screenshot", json!({})).expect("screenshot");
    assert!(shot["bytes"].as_u64().unwrap() > 1000, "{shot}");
    let elapsed = started.elapsed();
    frames.hold(&view, Duration::from_millis(600));

    stopping.store(true, Ordering::Relaxed);
    let _ = server.join();
    window.close();
    println!(
        "{}",
        json!({"ok": true, "elapsed_ms": elapsed.as_millis(), "frames": frames.next,
               "screenshot": shot, "outline_lines": outline.lines().count()})
    );
}
