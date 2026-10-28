//! Native crash reports from macOS's DiagnosticReports.
//!
//! The panic hook only sees Rust panics. A SIGSEGV in GPUI or an objc
//! exception that aborts the App leaves nothing in the spool, but macOS
//! writes an `.ips` report for it. The Engine outlives App crashes, so it
//! scans for reports from Diri's own processes at start and every few
//! minutes, and records each new one once as a `crash.report` incident.
//!
//! An `.ips` file is two JSON documents: a one-line header (`app_name`,
//! `app_version`, `timestamp`, `incident_id`...) and the report body
//! (`procName`, `exception`, `termination`, `faultingThread`, `threads`,
//! `usedImages`, and for an uncaught Objective-C exception `asi` and
//! `lastExceptionBacktrace`). Only bounded, scrubbed facts are taken from it:
//! no paths, registers, environment or application-specific messages. Of an
//! Objective-C exception that means its name and where it was thrown, never
//! its reason.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use diri_telemetry::{Value, id, text};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// Executables whose crashes are Diri's.
pub const PROCESSES: [&str; 6] = [
    "diri",
    "dirijord-rs",
    "diri-holder",
    "dirijor",
    "dirijor-mcp",
    "diri-ssh-askpass",
];

const MAX_FRAMES: usize = 24;
/// Diri's own frames kept from anywhere on the crashed thread.
const MAX_APP_FRAMES: usize = 12;
/// Frames that only raise an Objective-C exception, never the cause of one.
const THROW_FRAMES: [&str; 5] = [
    "__exceptionPreprocess",
    "objc_exception_throw",
    "-[NSException raise]",
    "+[NSException raise:format:]",
    "+[NSException raise:format:arguments:]",
];
/// A report larger than this is not one macOS wrote for a crash.
const MAX_REPORT_BYTES: u64 = 16 << 20;
/// On the very first scan, look this far back instead of at everything.
const FIRST_SCAN_LOOKBACK: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub const SCAN_INTERVAL: Duration = Duration::from_secs(10 * 60);
const WATERMARK_FILE: &str = "crash_watermark.json";

#[derive(Clone, Debug, PartialEq)]
pub struct CrashReport {
    pub process: String,
    pub app_version: Option<String>,
    pub incident_id: Option<String>,
    pub timestamp: Option<String>,
    pub exception_type: Option<String>,
    pub signal: Option<String>,
    pub subtype: Option<String>,
    pub termination: Option<String>,
    pub termination_namespace: Option<String>,
    pub termination_code: Option<i64>,
    pub crashed_thread: Option<u64>,
    pub thread_name: Option<String>,
    /// `image!symbol+offset`, crashed thread, innermost first.
    pub frames: Vec<String>,
    /// Diri's own frames on the crashed thread, innermost first, including
    /// ones past [`MAX_FRAMES`] (a panic's abort path fills the first dozen).
    pub app_frames: Vec<String>,
    /// The uncaught Objective-C exception's name, such as
    /// `NSInternalInconsistencyException`, when AppKit says.
    pub objc_exception: Option<String>,
    /// Where that exception was thrown (`lastExceptionBacktrace`). AppKit
    /// aborts from its own run loop (`_crashOnException:`), so the crashed
    /// thread alone never names the code that raised it.
    pub exception_frames: Vec<String>,
}

impl CrashReport {
    /// The innermost frame in Diri's own code, else the innermost frame:
    /// what groups the same crash across installs. For an uncaught
    /// Objective-C exception, taken from where it was thrown.
    #[must_use]
    pub fn signature(&self) -> Option<&str> {
        let thrown: Vec<&String> = self
            .exception_frames
            .iter()
            .filter(|frame| !is_throw_frame(frame))
            .collect();
        if !thrown.is_empty() {
            return thrown
                .iter()
                .find(|frame| is_app_frame(frame))
                .or_else(|| thrown.first())
                .map(|frame| frame.as_str());
        }
        self.frames
            .iter()
            .find(|frame| is_app_frame(frame))
            .or_else(|| self.frames.first())
            .map(String::as_str)
    }

    fn fields(&self, crashed_at_ms: u64) -> Vec<(&'static str, Value)> {
        vec![
            ("process", Value::from(id(&self.process))),
            (
                "app_version",
                Value::from(self.app_version.as_deref().map(id)),
            ),
            (
                "incident_id",
                Value::from(self.incident_id.as_deref().map(id)),
            ),
            ("crashed_at", Value::from(crashed_at_ms)),
            (
                "timestamp",
                Value::from(self.timestamp.as_deref().map(text)),
            ),
            (
                "exception",
                Value::from(self.exception_type.as_deref().map(id)),
            ),
            ("signal", Value::from(self.signal.as_deref().map(id))),
            ("subtype", Value::from(self.subtype.as_deref().map(text))),
            (
                "termination",
                Value::from(self.termination.as_deref().map(text)),
            ),
            (
                "namespace",
                Value::from(self.termination_namespace.as_deref().map(id)),
            ),
            ("code", Value::from(self.termination_code)),
            ("thread", Value::from(self.crashed_thread)),
            (
                "thread_name",
                Value::from(self.thread_name.as_deref().map(text)),
            ),
            (
                "objc_exception",
                Value::from(self.objc_exception.as_deref().map(id)),
            ),
            ("signature", Value::from(self.signature().map(text))),
            // Most telling first: the incident store keeps 2 KiB of fields
            // in order and drops whatever comes after.
            ("exception_frames", frame_list(&self.exception_frames)),
            ("app_frames", frame_list(&self.app_frames)),
            ("frames", frame_list(&self.frames)),
        ]
    }
}

fn frame_list(frames: &[String]) -> Value {
    Value::List(frames.iter().map(|f| Value::from(text(f))).collect())
}

fn is_app_frame(frame: &str) -> bool {
    frame
        .split_once('!')
        .is_some_and(|(image, _)| PROCESSES.contains(&image))
}

fn is_throw_frame(frame: &str) -> bool {
    frame.split_once('!').is_some_and(|(_, symbol)| {
        THROW_FRAMES.iter().any(|throw| {
            symbol
                .strip_prefix(throw)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('+'))
        })
    })
}

/// The exception name from AppKit's application-specific message, e.g.
/// `*** Terminating app due to uncaught exception 'NSRangeException', reason:
/// …`. Only the quoted name is kept, and only if it is a plain identifier.
fn objc_exception_name(body: &Json) -> Option<String> {
    const MARKER: &str = "uncaught exception '";
    let messages = body.get("asi")?.as_object()?;
    messages
        .values()
        .filter_map(Json::as_array)
        .flatten()
        .filter_map(Json::as_str)
        .find_map(|message| {
            let rest = &message[message.find(MARKER)? + MARKER.len()..];
            let name = &rest[..rest.find('\'')?];
            (!name.is_empty()
                && name.len() <= 96
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
            .then(|| name.to_owned())
        })
}

/// Parses one `.ips` report, or `None` when it is not a crash of one of
/// [`PROCESSES`].
#[must_use]
pub fn parse(contents: &str) -> Option<CrashReport> {
    let (header, body) = contents.split_once('\n')?;
    let header: Json = serde_json::from_str(header.trim()).ok()?;
    let body: Json = serde_json::from_str(body).ok()?;
    let process = body
        .get("procName")
        .and_then(Json::as_str)
        .or_else(|| header.get("name").and_then(Json::as_str))
        .or_else(|| header.get("app_name").and_then(Json::as_str))?;
    if !PROCESSES.contains(&process) {
        return None;
    }
    let string = |value: &Json, key: &str| {
        value
            .get(key)
            .and_then(Json::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let exception = body.get("exception").cloned().unwrap_or(Json::Null);
    let termination = body.get("termination").cloned().unwrap_or(Json::Null);
    let crashed_thread = body.get("faultingThread").and_then(Json::as_u64);
    let threads = body.get("threads").and_then(Json::as_array);
    let thread = threads.and_then(|threads| {
        crashed_thread
            .and_then(|index| threads.get(usize::try_from(index).ok()?))
            .or_else(|| {
                threads.iter().find(|thread| {
                    thread
                        .get("triggered")
                        .and_then(Json::as_bool)
                        .unwrap_or(false)
                })
            })
    });
    let images = body.get("usedImages").and_then(Json::as_array);
    let thread_frames = thread
        .and_then(|thread| thread.get("frames"))
        .and_then(Json::as_array);
    let frames = thread_frames
        .map(|frames| {
            frames
                .iter()
                .take(MAX_FRAMES)
                .map(|frame| describe_frame(frame, images))
                .collect()
        })
        .unwrap_or_default();
    let app_frames = thread_frames
        .map(|frames| {
            frames
                .iter()
                .map(|frame| describe_frame(frame, images))
                .filter(|frame| is_app_frame(frame))
                .take(MAX_APP_FRAMES)
                .collect()
        })
        .unwrap_or_default();
    let exception_frames = body
        .get("lastExceptionBacktrace")
        .and_then(Json::as_array)
        .map(|frames| {
            frames
                .iter()
                .take(MAX_FRAMES)
                .map(|frame| describe_frame(frame, images))
                .collect()
        })
        .unwrap_or_default();
    Some(CrashReport {
        process: process.to_owned(),
        app_version: string(&header, "app_version").or_else(|| {
            body.get("bundleInfo")
                .and_then(|info| string(info, "CFBundleShortVersionString"))
        }),
        incident_id: string(&header, "incident_id"),
        timestamp: string(&header, "timestamp"),
        exception_type: string(&exception, "type"),
        signal: string(&exception, "signal"),
        subtype: string(&exception, "subtype"),
        termination: string(&termination, "indicator"),
        termination_namespace: string(&termination, "namespace"),
        termination_code: termination.get("code").and_then(Json::as_i64),
        crashed_thread,
        thread_name: thread
            .and_then(|thread| string(thread, "name").or_else(|| string(thread, "queue"))),
        frames,
        app_frames,
        objc_exception: objc_exception_name(&body),
        exception_frames,
    })
}

fn describe_frame(frame: &Json, images: Option<&Vec<Json>>) -> String {
    let image = frame
        .get("imageIndex")
        .and_then(Json::as_u64)
        .and_then(|index| images?.get(usize::try_from(index).ok()?))
        .and_then(|image| image.get("name").and_then(Json::as_str))
        .unwrap_or("?");
    let offset = frame.get("imageOffset").and_then(Json::as_u64).unwrap_or(0);
    match frame.get("symbol").and_then(Json::as_str) {
        Some(symbol) => {
            let location = frame
                .get("symbolLocation")
                .and_then(Json::as_u64)
                .unwrap_or(0);
            format!("{image}!{symbol}+{location}")
        }
        None => format!("{image}!0x{offset:x}"),
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Watermark {
    /// Newest report mtime (ms since the epoch) already recorded.
    mtime_ms: u64,
}

/// `~/Library/Logs/DiagnosticReports`.
#[must_use]
pub fn reports_dir(home: &Path) -> PathBuf {
    home.join("Library/Logs/DiagnosticReports")
}

/// Records every Diri crash report in `reports` newer than the watermark kept
/// under `state_dir`, then advances it. Returns how many were recorded.
pub fn scan(reports: &Path, state_dir: &Path) -> usize {
    let watermark_path = diri_telemetry::telemetry_dir(state_dir).join(WATERMARK_FILE);
    let watermark = std::fs::read(&watermark_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Watermark>(&bytes).ok())
        .map_or_else(
            || millis(SystemTime::now()).saturating_sub(millis_of(FIRST_SCAN_LOOKBACK)),
            |mark| mark.mtime_ms,
        );
    let Ok(entries) = std::fs::read_dir(reports) else {
        return 0;
    };
    let mut found: Vec<(u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".ips")
                && PROCESSES.iter().any(|process| {
                    name.strip_prefix(process)
                        .is_some_and(|rest| rest.starts_with('-'))
                })
        })
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            let mtime = millis(metadata.modified().ok()?);
            (metadata.is_file() && metadata.len() <= MAX_REPORT_BYTES && mtime > watermark)
                .then(|| (mtime, entry.path()))
        })
        .collect();
    found.sort();
    let mut recorded = 0;
    let mut newest = watermark;
    for (mtime, path) in found {
        newest = newest.max(mtime);
        let Some(report) = std::fs::read_to_string(&path)
            .ok()
            .as_deref()
            .and_then(parse)
        else {
            continue;
        };
        diri_telemetry::record(
            "crash.report",
            diri_telemetry::Severity::Incident,
            report.fields(mtime),
        );
        recorded += 1;
    }
    if newest != watermark {
        let _ = diri_platform::private::create_dir_all(&diri_telemetry::telemetry_dir(state_dir));
        if let Ok(bytes) = serde_json::to_vec(&Watermark { mtime_ms: newest }) {
            let _ = diri_platform::private::write(&watermark_path, bytes);
        }
    }
    recorded
}

/// Scans at start and then every [`SCAN_INTERVAL`] on a background thread.
/// A directory read per interval; nothing runs when recording is off.
pub fn spawn_watcher(home: PathBuf, state_dir: PathBuf) {
    if !diri_telemetry::is_enabled() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("diri-crash-reports".into())
        .spawn(move || {
            let reports = reports_dir(&home);
            loop {
                scan(&reports, &state_dir);
                std::thread::sleep(SCAN_INTERVAL);
            }
        });
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, millis_of)
}

fn millis_of(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"app_name":"diri","timestamp":"2026-09-27 23:40:01.00 +0300","app_version":"0.8.8","slice_uuid":"b864fa4d-dd06-3137-a1af-4ff6634aa247","build_version":"1","bug_type":"309","os_version":"macOS 27.0 (26A428)","incident_id":"E9127141-B5BE-4A5C-883F-097585299623","name":"diri"}
{
  "procName" : "diri",
  "procPath" : "/Users/alex/Applications/diri.app/Contents/MacOS/diri",
  "pid" : 812,
  "exception" : {"codes":"0x1, 0x10","type":"EXC_BAD_ACCESS","signal":"SIGSEGV","subtype":"KERN_INVALID_ADDRESS at 0x0000000000000010"},
  "termination" : {"flags":0,"code":11,"namespace":"SIGNAL","indicator":"Segmentation fault: 11","byProc":"exc handler","byPid":812},
  "faultingThread" : 1,
  "threads" : [
    {"id": 1, "queue": "com.apple.main-thread", "frames": [{"imageOffset": 10, "imageIndex": 1}]},
    {"id": 2, "triggered": true, "name": "gpui-render", "frames": [
      {"imageOffset": 4096, "symbol": "objc_msgSend", "symbolLocation": 56, "imageIndex": 1},
      {"imageOffset": 8192, "symbol": "gpui::platform::mac::window::draw", "symbolLocation": 120, "imageIndex": 0},
      {"imageOffset": 9000, "imageIndex": 0}
    ]}
  ],
  "usedImages" : [
    {"name": "diri", "path": "/Users/alex/Applications/diri.app/Contents/MacOS/diri", "base": 4294967296},
    {"name": "libobjc.A.dylib", "path": "/usr/lib/libobjc.A.dylib", "base": 1}
  ]
}"#;

    #[test]
    fn parses_the_crashed_thread_and_its_frames() {
        let report = parse(FIXTURE).expect("a diri report");
        assert_eq!(report.process, "diri");
        assert_eq!(report.app_version.as_deref(), Some("0.8.8"));
        assert_eq!(report.exception_type.as_deref(), Some("EXC_BAD_ACCESS"));
        assert_eq!(report.signal.as_deref(), Some("SIGSEGV"));
        assert_eq!(
            report.termination.as_deref(),
            Some("Segmentation fault: 11")
        );
        assert_eq!(report.termination_code, Some(11));
        assert_eq!(report.crashed_thread, Some(1));
        assert_eq!(report.thread_name.as_deref(), Some("gpui-render"));
        assert_eq!(
            report.frames,
            [
                "libobjc.A.dylib!objc_msgSend+56",
                "diri!gpui::platform::mac::window::draw+120",
                "diri!0x2328",
            ]
        );
        assert_eq!(
            report.signature(),
            Some("diri!gpui::platform::mac::window::draw+120")
        );
        let fields = serde_json::to_string(&Value::Obj(report.fields(1))).unwrap();
        assert!(
            !fields.contains("alex"),
            "no paths leave the report: {fields}"
        );
    }

    /// AppKit's own abort for an exception raised during a display cycle:
    /// the crashed thread is only the run loop, the throw site is elsewhere.
    const OBJC_EXCEPTION_FIXTURE: &str = r#"{"app_name":"diri","timestamp":"2026-10-01 12:00:00.00 +0000","app_version":"0.9.1","bug_type":"309","incident_id":"00000000-0000-4000-8000-000000000001","name":"diri"}
{
  "procName" : "diri",
  "exception" : {"codes":"0x1, 0x18b7c2a44","type":"EXC_BREAKPOINT","signal":"SIGTRAP"},
  "termination" : {"code":5,"namespace":"SIGNAL","indicator":"Trace/BPT trap: 5"},
  "asi" : {"AppKit":["*** Terminating app due to uncaught exception 'NSInternalInconsistencyException', reason: 'secret at /Users/alex/notes.md'"]},
  "lastExceptionBacktrace" : [
    {"imageOffset": 1, "symbol": "__exceptionPreprocess", "symbolLocation": 164, "imageIndex": 1},
    {"imageOffset": 2, "symbol": "objc_exception_throw", "symbolLocation": 60, "imageIndex": 2},
    {"imageOffset": 3, "symbol": "-[NSView(Example) _throwingMethod]", "symbolLocation": 40, "imageIndex": 3},
    {"imageOffset": 4, "symbol": "-[NSView _layoutSubtreeIfNeeded]", "symbolLocation": 12, "imageIndex": 3}
  ],
  "faultingThread" : 0,
  "threads" : [
    {"id": 1, "triggered": true, "queue": "com.apple.main-thread", "frames": [
      {"imageOffset": 5, "symbol": "-[NSApplication _crashOnException:]", "symbolLocation": 256, "imageIndex": 3},
      {"imageOffset": 6, "symbol": "_RNvXs_NtCs_10gpui_macos8platformNtB4_11MacPlatform3run", "symbolLocation": 412, "imageIndex": 0}
    ]}
  ],
  "usedImages" : [
    {"name": "diri", "path": "/Users/alex/Applications/diri.app/Contents/MacOS/diri"},
    {"name": "CoreFoundation"},
    {"name": "libobjc.A.dylib"},
    {"name": "AppKit"}
  ]
}"#;

    #[test]
    fn an_objc_exception_keeps_its_name_and_throw_site_but_not_its_reason() {
        let report = parse(OBJC_EXCEPTION_FIXTURE).expect("a diri report");
        assert_eq!(
            report.objc_exception.as_deref(),
            Some("NSInternalInconsistencyException")
        );
        assert_eq!(
            report.exception_frames,
            [
                "CoreFoundation!__exceptionPreprocess+164",
                "libobjc.A.dylib!objc_exception_throw+60",
                "AppKit!-[NSView(Example) _throwingMethod]+40",
                "AppKit!-[NSView _layoutSubtreeIfNeeded]+12",
            ]
        );
        assert_eq!(
            report.signature(),
            Some("AppKit!-[NSView(Example) _throwingMethod]+40"),
            "grouped by where it was thrown, not by the run loop that aborted"
        );
        let fields = serde_json::to_string(&Value::Obj(report.fields(1))).unwrap();
        for private in ["secret", "notes.md", "alex", "reason"] {
            assert!(!fields.contains(private), "{private} leaked: {fields}");
        }
    }

    #[test]
    fn app_frames_reach_past_a_long_abort_path() {
        let mut thread_frames: Vec<String> = (0..30)
            .map(|index| {
                format!(
                    r#"{{"imageOffset": {index}, "symbol": "abort_path_{index}", "imageIndex": 1}}"#
                )
            })
            .collect();
        thread_frames.push(
            r#"{"imageOffset": 99, "symbol": "_RNvNtCs_10gpui_macos8platform20should_handle_reopen", "symbolLocation": 352, "imageIndex": 0}"#
                .to_owned(),
        );
        let fixture = FIXTURE.replace(
            r#"{"imageOffset": 4096, "symbol": "objc_msgSend", "symbolLocation": 56, "imageIndex": 1},"#,
            &format!("{},", thread_frames.join(",")),
        );
        let report = parse(&fixture).expect("a diri report");
        assert_eq!(report.frames.len(), MAX_FRAMES);
        assert_eq!(
            report.app_frames.first().map(String::as_str),
            Some("diri!_RNvNtCs_10gpui_macos8platform20should_handle_reopen+352")
        );
        assert_eq!(report.objc_exception, None);
        assert!(report.exception_frames.is_empty());
    }

    #[test]
    fn other_processes_are_not_diris() {
        let other = FIXTURE
            .replace("\"diri\"", "\"Anara\"")
            .replace("\"procName\" : \"diri\"", "\"procName\" : \"Anara\"");
        assert_eq!(parse(&other), None);
        assert_eq!(parse("not json"), None);
    }

    #[test]
    fn a_scan_advances_its_watermark_and_never_repeats_a_report() {
        let temp = tempfile::tempdir().unwrap();
        let reports = temp.path().join("reports");
        std::fs::create_dir_all(&reports).unwrap();
        std::fs::write(reports.join("diri-2026-09-27-234001.ips"), FIXTURE).unwrap();
        std::fs::write(reports.join("Anara-2026-09-27-234001.ips"), FIXTURE).unwrap();
        let state = temp.path().join("state");

        assert_eq!(scan(&reports, &state), 1);
        let watermark = diri_telemetry::telemetry_dir(&state).join(WATERMARK_FILE);
        assert!(watermark.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&watermark), 0o600);
            assert_eq!(mode(watermark.parent().unwrap()), 0o700);
        }
        assert_eq!(scan(&reports, &state), 0);
    }
}
