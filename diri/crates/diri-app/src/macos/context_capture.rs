//! Native half of Capture Context: what was in front, the region the person
//! drags, and the text inside it.
//!
//! * The front app is read on the main thread before anything appears.
//! * Browser tab URL and title come from the browser's own AppleScript
//!   dictionary through fixed, internally generated scripts (no extension,
//!   no JavaScript). Firefox has none, so it gives its window title only.
//! * The selection overlay is macOS's own (`screencapture -i`): drag a
//!   region, or press Space for a window; Escape cancels. It is pixel exact
//!   on every display and needs no window of ours over other apps.
//! * Text is the selection when there is one (Accessibility), otherwise
//!   on-device Vision OCR over the cropped PNG.
//!
//! Everything after the front-app read runs on a background thread, only
//! while a capture is in progress. Nothing captured is ever logged.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use diri_proto::clip::{ClipSource, ClipTextOrigin};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{MainThreadMarker, msg_send};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSArray, NSDictionary, NSRect, NSString, NSURL};

/// The app in front when the hotkey was pressed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Frontmost {
    pub name: Option<String>,
    pub bundle_id: Option<String>,
    pub pid: i32,
}

/// Region (drag; Space switches to a window) or window (click one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Region,
    Window,
}

/// A finished capture, ready for `clip.save`.
#[derive(Debug)]
pub(crate) struct Captured {
    pub png: PathBuf,
    pub source: ClipSource,
    pub text: Option<String>,
    pub text_origin: ClipTextOrigin,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CaptureError {
    /// Screen Recording is not granted to Diri.
    ScreenRecordingDenied,
    /// `screencapture` could not run or failed.
    Failed,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const c_void);
    fn CFGetTypeID(value: *const c_void) -> usize;
    fn CFStringGetTypeID() -> usize;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

// Vision is loaded by linking; its classes are looked up at run time so a
// missing class degrades to "no OCR" instead of aborting.
#[link(name = "Vision", kind = "framework")]
unsafe extern "C" {}

/// Reads the front app. Main thread only; cheap.
pub(crate) fn frontmost() -> Option<Frontmost> {
    MainThreadMarker::new()?;
    let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    Some(Frontmost {
        name: app.localizedName().map(|n| n.to_string()),
        bundle_id: app.bundleIdentifier().map(|b| b.to_string()),
        // SAFETY: NSRunningApplication's pid_t property.
        pid: unsafe { msg_send![&*app, processIdentifier] },
    })
}

/// Whether Diri may record the screen. Asking shows macOS's own prompt
/// (and adds Diri to the Screen Recording list) the first time only.
pub(crate) fn screen_recording_allowed(ask: bool) -> bool {
    // SAFETY: plain CoreGraphics queries with no arguments.
    unsafe { CGPreflightScreenCaptureAccess() || (ask && CGRequestScreenCaptureAccess()) }
}

/// Denials seen this launch: the first is answered by macOS's own prompt,
/// later ones by Diri's drag-to-grant guide.
static DENIALS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Whether a denial should open the guide rather than leave macOS's prompt
/// on its own.
pub(crate) fn guide_after_denial() -> bool {
    DENIALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 0
}

/// How a browser's front tab is read, by bundle id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrowserFamily {
    /// Chromium's dictionary: `active tab of front window`, `URL`, `title`.
    Chromium,
    /// Safari's: `current tab of front window`, `URL`, `name`.
    Safari,
    /// No scripting dictionary; window title only.
    Untitled,
}

/// The browser a bundle id belongs to, with its display name.
pub(crate) fn browser(bundle_id: &str) -> Option<(BrowserFamily, &'static str)> {
    use BrowserFamily::*;
    Some(match bundle_id {
        "com.google.Chrome" => (Chromium, "Chrome"),
        "com.google.Chrome.beta" | "com.google.Chrome.dev" => (Chromium, "Chrome"),
        "com.google.Chrome.canary" => (Chromium, "Chrome Canary"),
        "company.thebrowser.Browser" => (Chromium, "Arc"),
        "company.thebrowser.dia" => (Chromium, "Dia"),
        "com.brave.Browser" | "com.brave.Browser.beta" | "com.brave.Browser.nightly" => {
            (Chromium, "Brave")
        }
        "com.microsoft.edgemac" | "com.microsoft.edgemac.Beta" | "com.microsoft.edgemac.Dev" => {
            (Chromium, "Edge")
        }
        "com.vivaldi.Vivaldi" => (Chromium, "Vivaldi"),
        "com.operasoftware.Opera" => (Chromium, "Opera"),
        "org.chromium.Chromium" => (Chromium, "Chromium"),
        "com.apple.Safari" => (Safari, "Safari"),
        "com.apple.SafariTechnologyPreview" => (Safari, "Safari"),
        "org.mozilla.firefox" | "org.mozilla.firefoxdeveloperedition" | "org.mozilla.nightly" => {
            (Untitled, "Firefox")
        }
        "app.zen-browser.zen" => (Untitled, "Zen"),
        _ => return None,
    })
}

/// The fixed script that prints the front tab's URL and title on two
/// lines. Only bundle ids from [`browser`]'s table reach it, and they are
/// validated as reverse-DNS names before interpolation.
pub(crate) fn tab_script(family: BrowserFamily, bundle_id: &str) -> Option<String> {
    let valid = !bundle_id.is_empty()
        && bundle_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if !valid {
        return None;
    }
    let (tab, title) = match family {
        BrowserFamily::Chromium => ("active tab", "title"),
        BrowserFamily::Safari => ("current tab", "name"),
        BrowserFamily::Untitled => return None,
    };
    Some(format!(
        "tell application id \"{bundle_id}\"\n\
         if (count of windows) is 0 then return \"\"\n\
         set t to {tab} of front window\n\
         return (URL of t) & linefeed & ({title} of t)\n\
         end tell"
    ))
}

/// Runs `program` with a deadline, answering stdout on success. Kills it
/// on timeout.
fn run(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    use std::io::Read as _;
                    let _ = stdout.read_to_string(&mut out);
                }
                return status.success().then_some(out);
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(15));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn browser_tab(family: BrowserFamily, bundle_id: &str) -> Option<(String, String)> {
    let script = tab_script(family, bundle_id)?;
    let out = run(
        "/usr/bin/osascript",
        &["-e", &script],
        Duration::from_millis(1500),
    )?;
    parse_tab(&out)
}

/// `url\ntitle` → both, trimmed; the URL must look like one.
pub(crate) fn parse_tab(out: &str) -> Option<(String, String)> {
    let mut lines = out.trim_end_matches(['\n', '\r']).splitn(2, '\n');
    let url = lines.next()?.trim().to_owned();
    let title = lines.next().unwrap_or_default().trim().to_owned();
    (url.contains("://") && !url.contains(char::is_whitespace)).then_some((url, title))
}

/// One string attribute of an AX element, or of `child` under it.
fn ax_string(element: *const c_void, attribute: &str) -> Option<String> {
    let name = NSString::from_str(attribute);
    let mut value: *const c_void = std::ptr::null();
    // SAFETY: `element` is a live AXUIElementRef; NSString is toll-free
    // bridged to CFString; a returned value is owned and released here.
    unsafe {
        let status = AXUIElementCopyAttributeValue(
            element,
            Retained::as_ptr(&name).cast(),
            &mut value,
        );
        if status != 0 || value.is_null() {
            return None;
        }
        let text = (CFGetTypeID(value) == CFStringGetTypeID())
            .then(|| (*value.cast::<NSString>()).to_string());
        CFRelease(value);
        text
    }
}

/// An element attribute of an AX element, owned by the caller.
fn ax_element(element: *const c_void, attribute: &str) -> Option<AxRef> {
    let name = NSString::from_str(attribute);
    let mut value: *const c_void = std::ptr::null();
    // SAFETY: as in `ax_string`; ownership of `value` moves into `AxRef`.
    let status = unsafe {
        AXUIElementCopyAttributeValue(element, Retained::as_ptr(&name).cast(), &mut value)
    };
    (status == 0 && !value.is_null()).then_some(AxRef(value))
}

struct AxRef(*const c_void);

impl Drop for AxRef {
    fn drop(&mut self) {
        // SAFETY: an owned CF reference, released once.
        unsafe { CFRelease(self.0) };
    }
}

/// The front window's title and the selected text, when Accessibility is
/// granted. Both are best effort.
fn accessibility(pid: i32) -> (Option<String>, Option<String>) {
    // SAFETY: a plain query.
    if pid <= 0 || !unsafe { AXIsProcessTrusted() } {
        return (None, None);
    }
    // SAFETY: creates an owned AXUIElementRef for the process.
    let app = AxRef(unsafe { AXUIElementCreateApplication(pid) });
    if app.0.is_null() {
        return (None, None);
    }
    let title = ax_element(app.0, "AXFocusedWindow")
        .and_then(|window| ax_string(window.0, "AXTitle"))
        .filter(|t| !t.trim().is_empty());
    let selection = ax_element(app.0, "AXFocusedUIElement")
        .and_then(|focused| ax_string(focused.0, "AXSelectedText"))
        .filter(|t| !t.trim().is_empty());
    (title, selection)
}

/// One Vision text observation: its text and normalized bounding box
/// (origin bottom-left, as Vision reports it).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Observation {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Reading order: top to bottom, observations on one visual row joined
/// left to right with a space, rows joined with newlines. Chat threads and
/// cards keep their line structure.
pub(crate) fn layout(mut observations: Vec<Observation>) -> String {
    observations.retain(|o| !o.text.trim().is_empty());
    observations.sort_by(|a, b| {
        (b.y + b.height)
            .total_cmp(&(a.y + a.height))
            .then(a.x.total_cmp(&b.x))
    });
    let mut rows: Vec<Vec<Observation>> = Vec::new();
    for observation in observations {
        let center = observation.y + observation.height / 2.0;
        match rows.last_mut() {
            Some(row)
                if {
                    let first = &row[0];
                    (first.y + first.height / 2.0 - center).abs()
                        < first.height.min(observation.height) / 2.0
                } =>
            {
                row.push(observation)
            }
            _ => rows.push(vec![observation]),
        }
    }
    rows.into_iter()
        .map(|mut row| {
            row.sort_by(|a, b| a.x.total_cmp(&b.x));
            row.iter()
                .map(|o| o.text.trim())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// On-device text recognition over a PNG. `None` when Vision is missing
/// or finds nothing.
pub(crate) fn recognize_text(png: &Path) -> Option<String> {
    let handler_class = AnyClass::get(c"VNImageRequestHandler")?;
    let request_class = AnyClass::get(c"VNRecognizeTextRequest")?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(png.to_str()?));
    let options = NSDictionary::<AnyObject, AnyObject>::new();
    // SAFETY: documented Vision API: VNImageRequestHandler
    // initWithURL:options:, VNRecognizeTextRequest new, performRequests:
    // error:, results, topCandidates:, string and boundingBox. Every
    // object here is retained for the duration of its use.
    unsafe {
        let handler: Option<Retained<AnyObject>> = msg_send![
            msg_send![handler_class, alloc],
            initWithURL: &*url,
            options: &*options
        ];
        let handler = handler?;
        let request: Option<Retained<AnyObject>> = msg_send![request_class, new];
        let request = request?;
        // VNRequestTextRecognitionLevelAccurate
        let _: () = msg_send![&*request, setRecognitionLevel: 0isize];
        let _: () = msg_send![&*request, setUsesLanguageCorrection: true];
        let requests = NSArray::from_retained_slice(std::slice::from_ref(&request));
        let mut error: *mut AnyObject = std::ptr::null_mut();
        let ok: bool = msg_send![&*handler, performRequests: &*requests, error: &mut error];
        if !ok {
            return None;
        }
        let results: Option<Retained<NSArray<AnyObject>>> = msg_send![&*request, results];
        let mut observations = Vec::new();
        for observation in results?.iter() {
            let candidates: Option<Retained<NSArray<AnyObject>>> =
                msg_send![&*observation, topCandidates: 1usize];
            let Some(best) = candidates.and_then(|c| c.firstObject()) else {
                continue;
            };
            let text: Option<Retained<NSString>> = msg_send![&*best, string];
            let Some(text) = text else { continue };
            let bounds: NSRect = msg_send![&*observation, boundingBox];
            observations.push(Observation {
                text: text.to_string(),
                x: bounds.origin.x,
                y: bounds.origin.y,
                width: bounds.size.width,
                height: bounds.size.height,
            });
        }
        let text = layout(observations);
        (!text.trim().is_empty()).then_some(text)
    }
}

/// The scratch file a capture is written to before the Engine copies it
/// into the ring: owner-only, in a private per-user directory.
fn scratch_png(dir: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    Ok(dir.join(format!("capture-{}-{nanos}.png", std::process::id())))
}

/// The whole capture after the front app is known. Blocks until the person
/// finishes or cancels the overlay; `Ok(None)` when they cancel.
pub(crate) fn capture(
    front: &Frontmost,
    mode: Mode,
    scratch_dir: &Path,
) -> Result<Option<Captured>, CaptureError> {
    if !screen_recording_allowed(true) {
        return Err(CaptureError::ScreenRecordingDenied);
    }
    // Metadata first, while the captured app is still in front and before
    // the overlay takes the pointer.
    let mut source = ClipSource {
        app_name: front.name.clone(),
        bundle_id: front.bundle_id.clone(),
        ..Default::default()
    };
    let (window_title, selection) = accessibility(front.pid);
    source.window_title = window_title;
    if let Some(bundle) = front.bundle_id.as_deref()
        && let Some((family, name)) = browser(bundle)
    {
        source.browser = Some(name.to_owned());
        if let Some((url, title)) = browser_tab(family, bundle) {
            source.url = Some(url);
            source.page_title = (!title.is_empty()).then_some(title);
        }
    }

    let png = scratch_png(scratch_dir).map_err(|_| CaptureError::Failed)?;
    let path = png.to_str().ok_or(CaptureError::Failed)?;
    // -i interactive, -x no sound, -o no window shadow, -W start in window mode.
    let mut args = vec!["-i", "-x", "-o"];
    if mode == Mode::Window {
        args.push("-W");
    }
    args.push(path);
    run("/usr/sbin/screencapture", &args, Duration::from_secs(300))
        .ok_or(CaptureError::Failed)?;
    if !png.is_file() {
        return Ok(None); // Escape
    }
    let (text, text_origin) = match selection {
        Some(selection) => (Some(selection), ClipTextOrigin::Selection),
        None => match recognize_text(&png) {
            Some(text) => (Some(text), ClipTextOrigin::Ocr),
            None => (None, ClipTextOrigin::None),
        },
    };
    Ok(Some(Captured {
        png,
        source,
        text,
        text_origin,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(text: &str, x: f64, y: f64) -> Observation {
        Observation {
            text: text.into(),
            x,
            y,
            width: 0.2,
            height: 0.04,
        }
    }

    #[test]
    fn ocr_text_reads_top_to_bottom_and_left_to_right() {
        let text = layout(vec![
            observation("reply", 0.1, 0.50),
            observation("12:04", 0.7, 0.805),
            observation("Ana", 0.1, 0.80),
            observation("  ", 0.1, 0.3),
            observation("the button is off by 4px", 0.1, 0.70),
        ]);
        assert_eq!(text, "Ana 12:04\nthe button is off by 4px\nreply");
        assert_eq!(layout(Vec::new()), "");
    }

    #[test]
    fn browsers_map_to_their_dictionary() {
        assert_eq!(
            browser("company.thebrowser.Browser"),
            Some((BrowserFamily::Chromium, "Arc"))
        );
        assert_eq!(
            browser("com.apple.Safari"),
            Some((BrowserFamily::Safari, "Safari"))
        );
        assert_eq!(
            browser("org.mozilla.firefox"),
            Some((BrowserFamily::Untitled, "Firefox"))
        );
        assert_eq!(browser("com.tinyspeck.slackmacgap"), None);
    }

    #[test]
    fn tab_scripts_are_fixed_and_validated() {
        let script = tab_script(BrowserFamily::Chromium, "com.google.Chrome").unwrap();
        assert!(script.contains("tell application id \"com.google.Chrome\""));
        assert!(script.contains("active tab of front window"));
        assert!(script.contains("(title of t)"));
        let safari = tab_script(BrowserFamily::Safari, "com.apple.Safari").unwrap();
        assert!(safari.contains("current tab") && safari.contains("(name of t)"));
        assert_eq!(tab_script(BrowserFamily::Untitled, "org.mozilla.firefox"), None);
        assert_eq!(
            tab_script(BrowserFamily::Chromium, "x\" to do shell script \"rm"),
            None
        );
    }

    #[test]
    fn tab_output_needs_a_url() {
        assert_eq!(
            parse_tab("https://app.slack.com/client/T/C\nSlack | design\n"),
            Some((
                "https://app.slack.com/client/T/C".into(),
                "Slack | design".into()
            ))
        );
        assert_eq!(parse_tab("\n"), None);
        assert_eq!(parse_tab("missing value\nx"), None);
        assert_eq!(
            parse_tab("http://localhost:3000"),
            Some(("http://localhost:3000".into(), String::new()))
        );
    }

    /// Renders text into a PNG with AppKit and reads it back with Vision.
    /// Ignored by default: it needs a window server.
    #[test]
    #[ignore = "needs a macOS window server; run with --ignored"]
    fn vision_reads_rendered_text() {
        let dir = std::env::temp_dir().join(format!("diri-ocr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("text.png");
        let swift = format!(
            "import AppKit\nlet s = NSAttributedString(string: \"Save button overlaps footer\", attributes: [.font: NSFont.systemFont(ofSize: 48), .foregroundColor: NSColor.black])\nlet img = NSImage(size: NSSize(width: 900, height: 120)); img.lockFocus(); NSColor.white.setFill(); NSRect(x:0,y:0,width:900,height:120).fill(); s.draw(at: NSPoint(x: 20, y: 30)); img.unlockFocus()\nlet rep = NSBitmapImageRep(data: img.tiffRepresentation!)!\ntry! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: \"{}\"))\n",
            png.display()
        );
        let script = dir.join("render.swift");
        std::fs::write(&script, swift).unwrap();
        let status = Command::new("/usr/bin/swift").arg(&script).status().unwrap();
        assert!(status.success());
        let text = recognize_text(&png).expect("text");
        assert!(text.to_lowercase().contains("overlaps"), "{text}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
