//! Replaying a skill through the same `computer.act` path an Agent uses:
//! leases, the background-only rule and the session's cursor badge all
//! apply, and no model is in the loop.

use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::super::{ComputerError, MAX_BATCH_STEPS};
use super::Skill;
use super::compile::{check_step, driver_step};

/// The longest compact window tree a failure report carries.
const FAILURE_TREE_CHARS: usize = 6_000;

/// Where skills reach the driver. The Engine's actor is `computer.act` for
/// one session; tests script one.
pub(in crate::computer) trait Actor {
    /// One driver tool call; the result is relayed as `{"mcpToolResult": ...}`.
    fn call(&self, tool: &str, args: Value) -> Result<Value, ComputerError>;

    /// The bundle a running process belongs to.
    fn bundle_info(&self, pid: i64) -> Option<BundleInfo> {
        bundle_info_for_pid(pid)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::computer) struct BundleInfo {
    pub bundle_id: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
}

/// What a run found out about the app before it acted.
#[derive(Clone, Debug, Default)]
pub(super) struct Target {
    pub pid: Option<i64>,
    pub window_id: Option<i64>,
    pub version: Option<String>,
    /// The name its windows carry: what `run_steps` finds a window by
    /// first (a bundle id goes through `list_apps`, which only knows
    /// installed apps and regular running ones).
    pub app_name: Option<String>,
}

impl Target {
    /// What each batch names as its `app`.
    pub fn app<'a>(&'a self, bundle_id: &'a str) -> &'a str {
        self.app_name.as_deref().unwrap_or(bundle_id)
    }
}

/// How far a replay goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reach {
    /// Every step and both kinds of checks.
    Full,
    /// Everything but the final step (and the postconditions): verifying a
    /// destructive skill without doing the destructive part.
    UntilLast,
}

/// Where a replay stopped, in skill terms.
#[derive(Clone, Debug)]
pub(super) struct Failure {
    /// What failed: `step`, `precondition` or `postcondition`.
    pub part: &'static str,
    /// 0-based index within that part.
    pub index: usize,
    pub phase: Option<String>,
    pub code: Option<String>,
    pub message: String,
    /// The driver's own end-of-batch read, when it made one.
    pub observation: Option<Value>,
}

fn structured(result: &Value) -> &Value {
    &result["mcpToolResult"]["structuredContent"]
}

fn first_text(result: &Value) -> String {
    result["mcpToolResult"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// The app's front window and version, if it is running.
pub(super) fn locate(actor: &dyn Actor, bundle_id: &str) -> Target {
    let Ok(result) = actor.call("list_windows", json!({"app": bundle_id})) else {
        return Target::default();
    };
    let windows = structured(&result)["windows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let front = windows
        .iter()
        .find(|window| window["is_on_screen"] != false)
        .or_else(|| windows.first());
    let pid = front.and_then(|window| window["pid"].as_i64());
    Target {
        pid,
        window_id: front.and_then(|window| window["window_id"].as_i64()),
        app_name: front
            .and_then(|window| window["app_name"].as_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        version: pid
            .and_then(|pid| actor.bundle_info(pid))
            .and_then(|info| info.version),
    }
}

/// The app's front window, launching it (in the background) when it has none.
pub(super) fn ensure_running(actor: &dyn Actor, bundle_id: &str) -> Result<Target, ComputerError> {
    let target = locate(actor, bundle_id);
    if target.pid.is_some() {
        return Ok(target);
    }
    actor.call("launch_app", json!({"bundle_id": bundle_id}))?;
    Ok(locate(actor, bundle_id))
}

/// Runs a skill once with checked `args`, in `run_steps` calls of at most
/// [`MAX_BATCH_STEPS`] steps, so other Agents' actions interleave.
pub(super) fn run_once(
    actor: &dyn Actor,
    skill: &Skill,
    args: &Map<String, Value>,
    reach: Reach,
    app: &str,
) -> Result<(), Box<Failure>> {
    let mut sent: Vec<(&'static str, usize, Value)> = Vec::new();
    for (i, check) in skill.preconditions.iter().enumerate() {
        sent.push((
            "precondition",
            i,
            check_step(&super::compile::substitute(check, args), "wait_for"),
        ));
    }
    let steps = match reach {
        Reach::Full => skill.steps.len(),
        Reach::UntilLast => skill.steps.len().saturating_sub(1),
    };
    for (i, step) in skill.steps.iter().take(steps).enumerate() {
        sent.push(("step", i, driver_step(step, args)));
    }
    if reach == Reach::Full {
        for (i, check) in skill.postconditions.iter().enumerate() {
            sent.push((
                "postcondition",
                i,
                check_step(&super::compile::substitute(check, args), "expect"),
            ));
        }
    }
    for chunk in sent.chunks(MAX_BATCH_STEPS) {
        let steps: Vec<Value> = chunk.iter().map(|(_, _, step)| step.clone()).collect();
        let at = |index: usize| {
            let (part, index, _) = chunk[index.min(chunk.len() - 1)];
            (part, index)
        };
        let result = match actor.call("run_steps", json!({"steps": steps, "app": app})) {
            Ok(result) => result,
            Err(error) => {
                let (part, index) = at(0);
                return Err(Box::new(Failure {
                    part,
                    index,
                    phase: None,
                    code: Some(error.code.to_owned()),
                    message: error.message,
                    observation: None,
                }));
            }
        };
        let report = structured(&result);
        let failed = result["mcpToolResult"]["isError"] == true || report["ok"] == false;
        if failed {
            let failed_step = report["failed_step"].as_u64().unwrap_or(0) as usize;
            let step_report = &report["steps"][failed_step];
            let (part, index) = at(failed_step);
            let message = step_report["message"]
                .as_str()
                .or_else(|| report["detail"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| first_text(&result));
            return Err(Box::new(Failure {
                part,
                index,
                phase: step_report["phase"].as_str().map(str::to_owned),
                code: step_report["code"]
                    .as_str()
                    .or_else(|| report["code"].as_str())
                    .map(str::to_owned),
                message,
                observation: report["observation"]["state"]
                    .is_object()
                    .then(|| report["observation"]["state"].clone()),
            }));
        }
    }
    Ok(())
}

/// A compact read of where the app was left: the driver's own read when it
/// made one, otherwise one tree-only read of the front window.
pub(super) fn window_skeleton(
    actor: &dyn Actor,
    bundle_id: &str,
    observation: Option<&Value>,
) -> Value {
    let state = match observation {
        Some(state) => state.clone(),
        None => {
            let target = locate(actor, bundle_id);
            let (Some(pid), Some(window_id)) = (target.pid, target.window_id) else {
                return json!({"note": "the app has no window open"});
            };
            match actor.call(
                "get_window_state",
                json!({"pid": pid, "window_id": window_id, "include_screenshot": false, "max_elements": 120}),
            ) {
                Ok(result) => structured(&result).clone(),
                Err(error) => return json!({"note": format!("could not read the window: {}", error.message)}),
            }
        }
    };
    let tree = state["tree_markdown"]
        .as_str()
        .or_else(|| state["tree_diff"].as_str())
        .unwrap_or_default();
    let mut tree: String = tree.chars().take(FAILURE_TREE_CHARS).collect();
    if tree.len() < state["tree_markdown"].as_str().map_or(0, str::len) {
        tree.push_str("\n… (cut; read the window with the computer tool for the rest)");
    }
    json!({
        "app_name": state["app_name"],
        "window_title": state["window_title"],
        "snapshot_id": state["snapshot_id"],
        "tree": tree,
    })
}

/// Measures `f`.
pub(super) fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let out = f();
    (out, started.elapsed())
}

/// The `.app` a process runs from, read from its executable path, and that
/// bundle's identifier, name and short version.
#[cfg(target_os = "macos")]
pub(super) fn bundle_info_for_pid(pid: i64) -> Option<BundleInfo> {
    let pid = i32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is as long as the size passed.
    let len = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if len <= 0 {
        return None;
    }
    buffer.truncate(len as usize);
    let exe = std::path::PathBuf::from(String::from_utf8(buffer).ok()?);
    let bundle = exe
        .ancestors()
        .find(|dir| dir.extension().is_some_and(|ext| ext == "app"))?;
    bundle_info_at(bundle)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn bundle_info_for_pid(_pid: i64) -> Option<BundleInfo> {
    None
}

#[cfg(target_os = "macos")]
fn bundle_info_at(bundle: &std::path::Path) -> Option<BundleInfo> {
    let plist = plist::Value::from_file(bundle.join("Contents/Info.plist")).ok()?;
    let dict = plist.as_dictionary()?;
    let string = |key: &str| {
        dict.get(key)
            .and_then(plist::Value::as_string)
            .map(str::to_owned)
    };
    Some(BundleInfo {
        bundle_id: string("CFBundleIdentifier"),
        name: string("CFBundleName").or_else(|| string("CFBundleDisplayName")),
        version: string("CFBundleShortVersionString").or_else(|| string("CFBundleVersion")),
    })
}
