//! Session-addressed verbs for scripts and agents: press keys, wait, kill,
//! arrange panes by session, and say who and where you are. Every one takes
//! the targets the rest of the CLI takes (an id, a unique id prefix or a
//! unique part of a title) and resolves tab and pane ids itself, with the
//! same layout helpers and release policy as the MCP tools.

use std::collections::BTreeMap;

use super::spec;
use super::*;
use diri_proto::terminal_input::{Key, KeyAction, KeypadKey, Modifiers, NamedKey};
use diri_proto::workspace::{
    DockEdge, LayoutNode, PaneId, SpawnPane, SplitId, TabId, WorkspaceMutation, WorkspaceSnapshot,
};
use dirijor_mcp::bridge::cli::{self as shared, CliCaller};

/// A destructive command an Agent is not allowed to run (the MCP rule).
pub(super) const EXIT_DENIED: i32 = 6;

pub(super) const HELP: &str = "\
Session verbs (TARGET as above; it defaults to your own session where noted):
  dirijor send-key TARGET KEY...               enter, tab, escape, ctrl+c, shift+tab, alt+left, f5, C-c
  dirijor wait TARGET [--for 30s] [--until STATUS] [--json]   default: until it exits (agents: done or needsInput)
  dirijor kill TARGET [--keep] [--json]        stop and close; agents follow the MCP release rule (exit 6)
  dirijor split [TARGET] [--edge right|left|up|down] [--ratio 30] [-c DIR] [--label T] [--focus] [-- PROGRAM ...]
  dirijor run --split EDGE [--beside TARGET] [--ratio N] [--focus] -- PROGRAM ...
  dirijor focus TARGET | zoom TARGET [--on|--off] | move TARGET EDGE ANCHOR [--ratio N]
  dirijor swap TARGET OTHER | resize TARGET --ratio N
  dirijor session inspect|rename|close TARGET ... | tab ls | pane ls [TARGET]
  dirijor whoami [--json] | version [--json] | completion fish|zsh|bash
  dirijor help COMMAND, or COMMAND --help, describes any command.";

/// Arguments parsed against the command's flags in `spec.rs`, so what a
/// command accepts is exactly what its help and completions say.
#[derive(Debug, Default)]
pub(super) struct Parsed {
    pub(super) positionals: Vec<String>,
    values: BTreeMap<&'static str, Vec<String>>,
    /// Everything after `--`, untouched.
    pub(super) command: Option<Vec<String>>,
}

impl Parsed {
    pub(super) fn flag(&self, long: &str) -> bool {
        self.values.contains_key(long)
    }

    pub(super) fn value(&self, long: &str) -> Option<&str> {
        self.values
            .get(long)
            .and_then(|values| values.last())
            .map(String::as_str)
    }

    fn values(&self, long: &str) -> Vec<String> {
        self.values.get(long).cloned().unwrap_or_default()
    }
}

/// `None` when `--help` was asked for (and printed).
pub(super) fn parse(path: &[&str], arguments: &[String]) -> Result<Option<Parsed>, CliError> {
    let command = spec::find(path).expect("verb is in the spec");
    let name = format!("dirijor {}", path.join(" "));
    let mut parsed = Parsed::default();
    let mut words = arguments.iter();
    while let Some(word) = words.next() {
        if word == "--" {
            parsed.command = Some(words.cloned().collect());
            break;
        }
        if word == "--help" || word == "-h" {
            println!("{}", spec::help(path, command));
            return Ok(None);
        }
        if !word.starts_with('-') || word == "-" {
            parsed.positionals.push(word.clone());
            continue;
        }
        let (spelling, inline) = match word.split_once('=') {
            Some((spelling, value)) if spelling.starts_with("--") => (spelling, Some(value)),
            _ => (word.as_str(), None),
        };
        let flag = command
            .flags
            .iter()
            .find(|flag| {
                flag.long == spelling
                    || flag
                        .short
                        .is_some_and(|short| spelling == format!("-{short}"))
                    || alias_of(flag.long, spelling)
            })
            .ok_or_else(|| {
                CliError::failure(format!(
                    "unknown option {spelling} for {name}; see {name} --help"
                ))
            })?;
        let value = match (flag.value, inline) {
            (None, None) => String::new(),
            (None, Some(_)) => {
                return Err(CliError::failure(format!("{} takes no value", flag.long)));
            }
            (Some(_), Some(value)) => value.to_owned(),
            // `--focus` alone means true; other value flags need one.
            (Some(_), None) if flag.long == "--focus" => "true".into(),
            (Some(placeholder), None) => words
                .next()
                .filter(|value| !value.is_empty())
                .cloned()
                .ok_or_else(|| {
                    CliError::failure(format!("{} needs a value ({placeholder})", flag.long))
                })?,
        };
        if !flag.choices.is_empty() && !flag.choices.contains(&value.as_str()) {
            return Err(CliError::failure(format!(
                "{} must be one of {}, not {value:?}",
                flag.long,
                flag.choices.join(", ")
            )));
        }
        parsed.values.entry(flag.long).or_default().push(value);
    }
    Ok(Some(parsed))
}

/// Spellings kept for scripts written against the older flags.
fn alias_of(long: &str, spelling: &str) -> bool {
    matches!(
        (long, spelling),
        ("--label", "--name" | "--title") | ("--for", "--timeout") | ("--edge", "--split")
    )
}

/// `30`, `1.5`, `500ms`, `30s`, `2m`, `1h`, `1m30s`.
pub(super) fn parse_duration(raw: &str) -> Result<Duration, CliError> {
    let invalid = || {
        CliError::failure(format!(
            "invalid duration {raw:?}; use seconds or 500ms, 30s, 2m, 1h, 1m30s"
        ))
    };
    if raw.is_empty() {
        return Err(invalid());
    }
    if let Ok(seconds) = raw.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0)
            .then(|| Duration::from_secs_f64(seconds))
            .ok_or_else(invalid);
    }
    let mut total = 0.0f64;
    let mut rest = raw;
    while !rest.is_empty() {
        let digits = rest
            .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
            .ok_or_else(invalid)?;
        let number: f64 = rest[..digits].parse().map_err(|_| invalid())?;
        rest = &rest[digits..];
        let unit = rest
            .find(|ch: char| ch.is_ascii_digit() || ch == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit] {
            "ms" => 0.001,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return Err(invalid()),
        };
        total += number * scale;
        rest = &rest[unit..];
    }
    Ok(Duration::from_secs_f64(total))
}

/// A pane's share of its split from `30` or `30%`; Diri keeps each side of
/// a split between 10% and 90%.
pub(super) fn parse_ratio(raw: &str) -> Result<f32, CliError> {
    let percent: f32 = raw
        .trim_end_matches('%')
        .parse()
        .map_err(|_| CliError::failure(format!("--ratio expects a percentage, not {raw:?}")))?;
    if !(10.0..=90.0).contains(&percent) {
        return Err(CliError::failure(
            "--ratio must be between 10 and 90: each side of a split keeps at least a tenth",
        ));
    }
    Ok(percent / 100.0)
}

pub(super) fn parse_edge(raw: &str) -> Result<DockEdge, CliError> {
    match raw {
        "right" => Ok(DockEdge::Right),
        "left" => Ok(DockEdge::Left),
        "up" | "above" | "top" => Ok(DockEdge::Top),
        "down" | "below" | "bottom" => Ok(DockEdge::Bottom),
        other => Err(CliError::failure(format!(
            "edge must be right, left, up or down, not {other:?}"
        ))),
    }
}

fn parse_bool(flag: &str, raw: &str) -> Result<bool, CliError> {
    match raw {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        other => Err(CliError::failure(format!(
            "{flag} must be true or false, not {other:?}"
        ))),
    }
}

/// A key chord: `enter`, `ctrl+c`, `shift+tab`, `ctrl+alt+delete`, `G`
/// (types `g`, as letter names are case-insensitive; `shift+g` types `G`),
/// `keypad:5`, or the tmux spellings `C-c`, `M-x`, `S-Tab`.
pub(super) fn parse_chord(token: &str) -> Result<(Key, Modifiers), CliError> {
    let unknown = || {
        CliError::failure(format!(
            "unknown key {token:?}; use a name such as enter, tab, escape, up, f5, ctrl+c or shift+tab \
             (dirijor send types text)"
        ))
    };
    let mut modifiers = Modifiers::default();
    let mut rest = token;
    loop {
        if let Some((head, tail)) = rest.split_once('+')
            && !tail.is_empty()
        {
            match head.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "c" => modifiers.ctrl = true,
                "alt" | "option" | "opt" | "meta" | "m" => modifiers.alt = true,
                "shift" | "s" => modifiers.shift = true,
                "cmd" | "command" | "super" => modifiers.cmd = true,
                _ => return Err(unknown()),
            }
            rest = tail;
            continue;
        }
        if rest.len() > 2 {
            match rest.get(..2) {
                Some("C-") => modifiers.ctrl = true,
                Some("M-") => modifiers.alt = true,
                Some("S-") => modifiers.shift = true,
                _ => break,
            }
            rest = &rest[2..];
            continue;
        }
        break;
    }
    if let Some(digit) = rest.strip_prefix("keypad:") {
        let name = match digit {
            "0" => "zero",
            "1" => "one",
            "2" => "two",
            "3" => "three",
            "4" => "four",
            "5" => "five",
            "6" => "six",
            "7" => "seven",
            "8" => "eight",
            "9" => "nine",
            "." => "decimal",
            other => other,
        };
        let key = serde_json::from_value::<KeypadKey>(json!(name)).map_err(|_| unknown())?;
        return Ok((Key::Keypad(key), modifiers));
    }
    let named = match rest.to_ascii_lowercase().as_str() {
        "enter" | "return" | "ret" | "cr" => Some(NamedKey::Enter),
        "tab" => Some(NamedKey::Tab),
        "escape" | "esc" => Some(NamedKey::Escape),
        "backspace" | "bspace" | "bs" => Some(NamedKey::Backspace),
        "delete" | "del" | "dc" => Some(NamedKey::Delete),
        "insert" | "ins" | "ic" => Some(NamedKey::Insert),
        "home" => Some(NamedKey::Home),
        "end" => Some(NamedKey::End),
        "pageup" | "pgup" | "ppage" => Some(NamedKey::PageUp),
        "pagedown" | "pgdn" | "npage" => Some(NamedKey::PageDown),
        "up" => Some(NamedKey::ArrowUp),
        "down" => Some(NamedKey::ArrowDown),
        "left" => Some(NamedKey::ArrowLeft),
        "right" => Some(NamedKey::ArrowRight),
        "f1" => Some(NamedKey::F1),
        "f2" => Some(NamedKey::F2),
        "f3" => Some(NamedKey::F3),
        "f4" => Some(NamedKey::F4),
        "f5" => Some(NamedKey::F5),
        "f6" => Some(NamedKey::F6),
        "f7" => Some(NamedKey::F7),
        "f8" => Some(NamedKey::F8),
        "f9" => Some(NamedKey::F9),
        "f10" => Some(NamedKey::F10),
        "f11" => Some(NamedKey::F11),
        "f12" => Some(NamedKey::F12),
        _ => None,
    };
    if let Some(named) = named {
        return Ok((Key::Named(named), modifiers));
    }
    if rest.eq_ignore_ascii_case("space") {
        return Ok((Key::Character(" ".into()), modifiers));
    }
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) if !ch.is_control() => Ok((
            Key::Character(ch.to_ascii_lowercase().to_string()),
            modifiers,
        )),
        _ => Err(unknown()),
    }
}

fn shared_error(message: String) -> CliError {
    map_bridge_error(message)
}

/// The session `TARGET` names, or the caller's own when it is left out.
fn target_or_self(
    parsed: &Parsed,
    index: usize,
    sessions: &[SessionRecord],
    verb: &str,
) -> Result<SessionRecord, CliError> {
    match parsed.positionals.get(index) {
        Some(target) => Ok(resolve_session(target, sessions)?.clone()),
        None => {
            let own = std::env::var("DIRIJOR_SESSION_ID").map_err(|_| {
                CliError::failure(format!(
                    "{verb} needs a TARGET outside a Diri terminal (inside one it defaults to $DIRIJOR_SESSION_ID)"
                ))
            })?;
            Ok(resolve_session(&own, sessions)?.clone())
        }
    }
}

fn required_target(
    parsed: &Parsed,
    index: usize,
    sessions: &[SessionRecord],
    usage: &str,
) -> Result<SessionRecord, CliError> {
    let target = parsed
        .positionals
        .get(index)
        .ok_or_else(|| CliError::failure(format!("usage: dirijor {usage}")))?;
    Ok(resolve_session(target, sessions)?.clone())
}

fn locate(
    snapshot: &WorkspaceSnapshot,
    record: &SessionRecord,
) -> Result<shared::Placement, CliError> {
    shared::locate(snapshot, &record.id.0).map_err(|_| CliError {
        code: EXIT_NOT_FOUND,
        message: format!(
            "{} ({}) is not shown in any tab; open it in Diri, or dock it with dirijor move {} right ANCHOR",
            record.id.0, record.title, record.id.0
        ),
    })
}

fn find_tab<'a>(
    snapshot: &'a WorkspaceSnapshot,
    tab: &TabId,
) -> Option<&'a diri_proto::workspace::WorkspaceTab> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .find(|candidate| &candidate.id == tab)
}

/// The split that holds `pane` directly, and whether the pane is its first
/// (left or top) child.
pub(super) fn parent_split(node: &LayoutNode, pane: &PaneId) -> Option<(SplitId, bool)> {
    match node {
        LayoutNode::Pane { .. } => None,
        LayoutNode::Split {
            id, first, second, ..
        } => {
            let is =
                |child: &LayoutNode| matches!(child, LayoutNode::Pane { id, .. } if id == pane);
            if is(first) {
                Some((id.clone(), true))
            } else if is(second) {
                Some((id.clone(), false))
            } else {
                parent_split(first, pane).or_else(|| parent_split(second, pane))
            }
        }
    }
}

/// Gives `session`'s pane `share` of its split.
fn resize_to(session: &str, share: f32) -> Result<WorkspaceSnapshot, CliError> {
    bridge()
        .mutate_workspace(|snapshot| {
            let placement = shared::locate(snapshot, session)?;
            let tab = find_tab(snapshot, &placement.tab_id).ok_or("tab vanished")?;
            let (split_id, first) =
                parent_split(&tab.layout, &placement.pane_id).ok_or_else(|| {
                    format!("{session} is alone in its tab; there is no split to resize")
                })?;
            Ok(WorkspaceMutation::ResizeSplit {
                tab_id: placement.tab_id,
                split_id,
                fraction: if first { share } else { 1.0 - share },
            })
        })
        .map_err(shared_error)
}

fn layout_result(snapshot: &WorkspaceSnapshot, tab: &TabId) -> Result<Value, CliError> {
    let sessions = sessions()?;
    Ok(json!({
        "ok": true,
        "revision": snapshot.revision,
        "tab": shared::tab_view(snapshot, tab, &sessions),
    }))
}

fn print_layout(json_output: bool, value: &Value, text: impl FnOnce() -> String) {
    if json_output {
        print_json(value);
    } else {
        println!("{}", text());
    }
}

pub(super) fn send_key(arguments: &[String]) -> Result<(), CliError> {
    let Some(parsed) = parse(&["send-key"], arguments)? else {
        return Ok(());
    };
    let usage = "send-key TARGET KEY... (for example: dirijor send-key \"$id\" ctrl+c)";
    let (target, keys) = parsed
        .positionals
        .split_first()
        .filter(|(_, keys)| !keys.is_empty())
        .ok_or_else(|| CliError::failure(format!("usage: dirijor {usage}")))?;
    let repeat = match parsed.value("--repeat") {
        Some(raw) => raw
            .parse::<u32>()
            .ok()
            .filter(|count| (1..=100).contains(count))
            .ok_or_else(|| CliError::failure("--repeat expects 1 to 100"))?,
        None => 1,
    };
    // Every key is checked before the first one is pressed.
    let chords = keys
        .iter()
        .map(|key| parse_chord(key))
        .collect::<Result<Vec<_>, _>>()?;
    let sessions = sessions()?;
    let record = resolve_session(target, &sessions)?;
    let mut bytes = 0u64;
    let mut pressed = 0u32;
    for (key, modifiers) in &chords {
        let params = diri_proto::SendKeyParams {
            session_id: record.id.clone(),
            key: key.clone(),
            modifiers: *modifiers,
            action: KeyAction::Press,
        };
        params.event().map_err(CliError::failure)?;
        let value =
            serde_json::to_value(&params).map_err(|error| CliError::failure(error.to_string()))?;
        for _ in 0..repeat {
            let result = request(
                Method::SESSION_SEND_KEY,
                value.clone(),
                Duration::from_secs(10),
            )?;
            bytes += result["bytesAccepted"].as_u64().unwrap_or(0);
            pressed += 1;
        }
    }
    if parsed.flag("--json") {
        print_json(&json!({"ok": true, "id": record.id.0, "keys": pressed, "bytes": bytes}));
    }
    Ok(())
}

/// What `wait` waits for when `--until` is left out: a terminal's program
/// exiting, or an Agent finishing its turn or stopping to ask.
pub(super) fn default_until(record: &SessionRecord) -> Vec<String> {
    if record.effective_kind().is_terminal() {
        vec!["exited".into()]
    } else {
        vec!["done".into(), "needsInput".into()]
    }
}

pub(super) fn wait(arguments: &[String]) -> Result<(), CliError> {
    let Some(parsed) = parse(&["wait"], arguments)? else {
        return Ok(());
    };
    let limit = parsed.value("--for").map(parse_duration).transpose()?;
    let sessions = sessions()?;
    let record = required_target(
        &parsed,
        0,
        &sessions,
        "wait TARGET [--for 30s] [--until STATUS]",
    )?;
    let mut until = parsed.values("--until");
    if until.is_empty() {
        until = default_until(&record);
    }
    // The Engine answers one wait for at most 10 minutes; longer waits
    // (or none at all) are a series of them.
    const SLICE: Duration = Duration::from_secs(600);
    let deadline = limit.map(|limit| Instant::now() + limit);
    let result = loop {
        let slice = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(SLICE)
            .min(SLICE);
        let result = request(
            Method::EVENTS_WAIT,
            json!({"sessionID": record.id.0, "until": until, "timeoutMs": slice.as_millis() as i64}),
            slice + Duration::from_secs(5),
        )?;
        let timed_out = result["timedOut"].as_bool() == Some(true);
        if !timed_out || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break result;
        }
    };
    let timed_out = result["timedOut"].as_bool() == Some(true);
    let status: Option<SessionStatus> =
        serde_json::from_value(result["session"]["status"].clone()).ok();
    let exit = match &status {
        Some(SessionStatus::Exited(exit)) => Some(exit),
        _ => None,
    };
    let label = status.as_ref().map(status_label).unwrap_or("unknown");
    if parsed.flag("--json") {
        print_json(&json!({
            "id": record.id.0,
            "until": until,
            "timedOut": timed_out,
            "status": label,
            "exitCode": exit.and_then(|exit| exit.code),
            "signal": exit.and_then(|exit| exit.signal),
            "session": result["session"],
        }));
    } else if !timed_out {
        match exit {
            Some(exit) => println!(
                "{} exited {}",
                record.id.0,
                match (exit.code, exit.signal) {
                    (Some(code), _) => code.to_string(),
                    (None, Some(signal)) => format!("by signal {signal}"),
                    (None, None) => "with no status".into(),
                }
            ),
            None => println!("{} {label}", record.id.0),
        }
    }
    if timed_out {
        return Err(CliError {
            code: EXIT_TIMEOUT,
            message: format!(
                "{} is still {label} after {}; a timeout says nothing about how it will end",
                record.id.0,
                parsed.value("--for").unwrap_or("the wait")
            ),
        });
    }
    Ok(())
}

/// The gate every CLI stop/remove passes, `session release` included.
pub(super) fn authorize_release(record: &SessionRecord) -> Result<CliCaller, CliError> {
    bridge()
        .authorize_cli_release(&record.id.0)
        .map_err(|message| {
            if message.contains("connect") || message.contains("socket") {
                map_bridge_error(message)
            } else {
                CliError {
                    code: EXIT_DENIED,
                    message,
                }
            }
        })
}

/// Stops the session, then removes its record unless `keep`.
pub(super) fn stop(record: &SessionRecord, keep: bool) -> Result<(), CliError> {
    authorize_release(record)?;
    let killed = request(
        Method::SESSION_KILL,
        json!({"sessionID": record.id.0}),
        Duration::from_secs(10),
    );
    match killed {
        Ok(_) => {}
        // Already finished: there is nothing left to stop.
        Err(_) if matches!(record.status, SessionStatus::Exited(_)) => {}
        Err(error) => return Err(error),
    }
    if !keep {
        request(
            Method::SESSION_REMOVE,
            json!({"sessionID": record.id.0}),
            Duration::from_secs(10),
        )?;
    }
    Ok(())
}

pub(super) fn kill(arguments: &[String]) -> Result<(), CliError> {
    kill_as(&["kill"], arguments)
}

pub(super) fn session_close(arguments: &[String]) -> Result<(), CliError> {
    kill_as(&["session", "close"], arguments)
}

fn kill_as(path: &[&str], arguments: &[String]) -> Result<(), CliError> {
    let Some(parsed) = parse(path, arguments)? else {
        return Ok(());
    };
    let sessions = sessions()?;
    let record = required_target(&parsed, 0, &sessions, &format!("{} TARGET", path.join(" ")))?;
    let keep = parsed.flag("--keep");
    stop(&record, keep)?;
    if parsed.flag("--json") {
        print_json(&json!({"ok": true, "id": record.id.0, "removed": !keep}));
    } else {
        println!("killed {}", record.id.0);
    }
    Ok(())
}

pub(super) fn focus(arguments: &[String]) -> Result<(), CliError> {
    let Some(parsed) = parse(&["focus"], arguments)? else {
        return Ok(());
    };
    let sessions = sessions()?;
    let record = required_target(&parsed, 0, &sessions, "focus TARGET")?;
    let tab = focus_session(&record.id.0)?;
    let value = match &tab {
        Some(tab) => layout_result(&bridge().workspace().map_err(shared_error)?, tab)?,
        None => json!({"ok": true, "tab": null}),
    };
    print_layout(parsed.flag("--json"), &value, || {
        format!("focused {}", record.id.0)
    });
    Ok(())
}

/// Focuses the session's pane when it is shown, and asks the app to show
/// the session. Returns the tab that holds it.
fn focus_session(session: &str) -> Result<Option<TabId>, CliError> {
    let mut tab = None;
    if shared::locate(&bridge().workspace().map_err(shared_error)?, session).is_ok() {
        bridge()
            .mutate_workspace(|snapshot| {
                let placement = shared::locate(snapshot, session)?;
                tab = Some(placement.tab_id.clone());
                Ok(WorkspaceMutation::FocusPane {
                    tab_id: placement.tab_id,
                    pane_id: placement.pane_id,
                })
            })
            .map_err(shared_error)?;
    }
    request(
        Method::SESSION_REVEAL,
        json!({"sessionID": session}),
        Duration::from_secs(5),
    )?;
    Ok(tab)
}

pub(super) fn zoom(arguments: &[String]) -> Result<(), CliError> {
    let Some(parsed) = parse(&["zoom"], arguments)? else {
        return Ok(());
    };
    if parsed.flag("--on") && parsed.flag("--off") {
        return Err(CliError::failure("choose one of --on and --off"));
    }
    let sessions = sessions()?;
    let record = required_target(&parsed, 0, &sessions, "zoom TARGET [--on|--off]")?;
    let session = record.id.0.clone();
    let mut tab = None;
    let mut zoomed = false;
    let snapshot = bridge()
        .mutate_workspace(|snapshot| {
            let placement = shared::locate(snapshot, &session)
                .map_err(|_| format!("{session} is not shown in any tab"))?;
            let current = find_tab(snapshot, &placement.tab_id).ok_or("tab vanished")?;
            if matches!(current.layout, LayoutNode::Pane { .. }) {
                return Err(format!(
                    "{session} is alone in its tab; there is nothing to zoom"
                ));
            }
            let is_zoomed = current.zoomed_pane.as_ref() == Some(&placement.pane_id);
            zoomed = if parsed.flag("--on") {
                true
            } else if parsed.flag("--off") {
                false
            } else {
                !is_zoomed
            };
            tab = Some(placement.tab_id.clone());
            Ok(WorkspaceMutation::ZoomPane {
                tab_id: placement.tab_id,
                pane_id: zoomed.then_some(placement.pane_id),
            })
        })
        .map_err(shared_error)?;
    let mut value = layout_result(&snapshot, tab.as_ref().expect("zoom resolved a tab"))?;
    value["zoomed"] = json!(zoomed);
    print_layout(parsed.flag("--json"), &value, || {
        format!("{} {}", if zoomed { "zoomed" } else { "unzoomed" }, session)
    });
    Ok(())
}

pub(super) fn move_pane(arguments: &[String]) -> Result<(), CliError> {
    let Some(parsed) = parse(&["move"], arguments)? else {
        return Ok(());
    };
    let usage = "move TARGET EDGE ANCHOR [--ratio N] (for example: dirijor move logs down editor)";
    if parsed.positionals.len() != 3 {
        return Err(CliError::failure(format!("usage: dirijor {usage}")));
    }
    let edge = parse_edge(&parsed.positionals[1])?;
    let ratio = parsed.value("--ratio").map(parse_ratio).transpose()?;
    let sessions = sessions()?;
    let record = required_target(&parsed, 0, &sessions, usage)?;
    let anchor = required_target(&parsed, 2, &sessions, usage)?;
    if record.id == anchor.id {
        return Err(CliError::failure("move a session beside a different one"));
    }
    let mut snapshot = bridge()
        .mutate_workspace(|snapshot| {
            shared::split_mutation(snapshot, &record.id.0, &anchor.id.0, edge)
        })
        .map_err(|message| {
            if message.contains("not shown") {
                CliError {
                    code: EXIT_NOT_FOUND,
                    message: format!("{} is not shown in any tab", anchor.id.0),
                }
            } else {
                shared_error(message)
            }
        })?;
    if let Some(share) = ratio {
        snapshot = resize_to(&record.id.0, share)?;
    }
    let tab = locate(&snapshot, &anchor)?.tab_id;
    print_layout(
        parsed.flag("--json"),
        &layout_result(&snapshot, &tab)?,
        || format!("moved {} beside {}", record.id.0, anchor.id.0),
    );
    Ok(())
}

pub(super) fn swap(arguments: &[String]) -> Result<(), CliError> {
    let Some(parsed) = parse(&["swap"], arguments)? else {
        return Ok(());
    };
    let usage = "swap TARGET OTHER";
    if parsed.positionals.len() != 2 {
        return Err(CliError::failure(format!("usage: dirijor {usage}")));
    }
    let sessions = sessions()?;
    let first = required_target(&parsed, 0, &sessions, usage)?;
    let second = required_target(&parsed, 1, &sessions, usage)?;
    if first.id == second.id {
        return Err(CliError::failure("swap two different sessions"));
    }
    let workspace = bridge().workspace().map_err(shared_error)?;
    locate(&workspace, &first)?;
    locate(&workspace, &second)?;
    let snapshot = bridge()
        .mutate_workspace(|snapshot| {
            let a = shared::locate(snapshot, &first.id.0)?;
            let b = shared::locate(snapshot, &second.id.0)?;
            Ok(WorkspaceMutation::SwapPanes {
                first_tab: a.tab_id,
                first: a.pane_id,
                second_tab: b.tab_id,
                second: b.pane_id,
            })
        })
        .map_err(shared_error)?;
    let all = super::sessions()?;
    let mut tabs: Vec<TabId> = Vec::new();
    for record in [&first, &second] {
        let tab = locate(&snapshot, record)?.tab_id;
        if !tabs.contains(&tab) {
            tabs.push(tab);
        }
    }
    let value = json!({
        "ok": true,
        "revision": snapshot.revision,
        "tabs": tabs.iter().map(|tab| shared::tab_view(&snapshot, tab, &all)).collect::<Vec<_>>(),
    });
    print_layout(parsed.flag("--json"), &value, || {
        format!("swapped {} and {}", first.id.0, second.id.0)
    });
    Ok(())
}

pub(super) fn resize(arguments: &[String]) -> Result<(), CliError> {
    let Some(parsed) = parse(&["resize"], arguments)? else {
        return Ok(());
    };
    let share = parse_ratio(
        parsed
            .value("--ratio")
            .ok_or_else(|| CliError::failure("usage: dirijor resize TARGET --ratio PERCENT"))?,
    )?;
    let sessions = sessions()?;
    let record = required_target(&parsed, 0, &sessions, "resize TARGET --ratio PERCENT")?;
    locate(&bridge().workspace().map_err(shared_error)?, &record)?;
    let snapshot = resize_to(&record.id.0, share)?;
    let tab = locate(&snapshot, &record)?.tab_id;
    print_layout(
        parsed.flag("--json"),
        &layout_result(&snapshot, &tab)?,
        || format!("{} now has {:.0}% of its split", record.id.0, share * 100.0),
    );
    Ok(())
}

/// Where `run --split` or `split` puts the new session, and what happens
/// once it has started.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Placement {
    pub(super) edge: Option<DockEdge>,
    pub(super) beside: Option<String>,
    pub(super) ratio: Option<f32>,
    pub(super) focus: bool,
}

impl Placement {
    /// Takes `--split EDGE`, `--beside TARGET`, `--ratio N` and
    /// `--focus[=BOOL]` out of `run`'s options, leaving the rest.
    pub(super) fn take(options: &mut Vec<String>) -> Result<Self, CliError> {
        let mut placement = Self::default();
        let mut kept = Vec::new();
        let mut words = std::mem::take(options).into_iter();
        while let Some(word) = words.next() {
            let (spelling, inline) = match word.split_once('=') {
                Some((spelling, value)) if spelling.starts_with("--") => {
                    (spelling.to_owned(), Some(value.to_owned()))
                }
                _ => (word.clone(), None),
            };
            let mut value = |name: &str| {
                inline
                    .clone()
                    .or_else(|| words.next())
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| CliError::failure(format!("{name} needs a value")))
            };
            match spelling.as_str() {
                "--split" => placement.edge = Some(parse_edge(&value("--split")?)?),
                "--beside" => placement.beside = Some(value("--beside")?),
                "--ratio" => placement.ratio = Some(parse_ratio(&value("--ratio")?)?),
                "--focus" => {
                    placement.focus = match &inline {
                        Some(raw) => parse_bool("--focus", raw)?,
                        None => true,
                    }
                }
                _ => kept.push(word),
            }
        }
        *options = kept;
        if placement.edge.is_none() && (placement.beside.is_some() || placement.ratio.is_some()) {
            return Err(CliError::failure("--beside and --ratio need --split EDGE"));
        }
        Ok(placement)
    }

    /// Adds the Engine's `pane` placement to the spawn params. Resolved
    /// before launching, so a bad anchor starts nothing.
    pub(super) fn apply(&self, params: &mut Value) -> Result<Option<SessionRecord>, CliError> {
        let Some(edge) = self.edge else {
            return Ok(None);
        };
        let sessions = sessions()?;
        let anchor = match &self.beside {
            Some(target) => resolve_session(target, &sessions)?.clone(),
            None => {
                let own = std::env::var("DIRIJOR_SESSION_ID").map_err(|_| {
                    CliError::failure(
                        "--split needs --beside TARGET outside a Diri terminal (inside one it splits your own pane)",
                    )
                })?;
                resolve_session(&own, &sessions)?.clone()
            }
        };
        let target = locate(&bridge().workspace().map_err(shared_error)?, &anchor)?;
        params[diri_proto::workspace::SPAWN_PANE_FIELD] = serde_json::to_value(SpawnPane::Beside {
            tab_id: target.tab_id,
            target: target.pane_id,
            edge,
        })
        .map_err(|error| CliError::failure(error.to_string()))?;
        Ok(Some(anchor))
    }

    /// After the spawn: size the new pane, focus it, and say where it went.
    pub(super) fn finish(
        &self,
        id: &str,
        anchor: Option<&SessionRecord>,
    ) -> Result<Value, CliError> {
        let mut layout = Value::Null;
        if let Some(anchor) = anchor {
            let snapshot = bridge().workspace().map_err(shared_error)?;
            match shared::placements(&snapshot, id).into_iter().next() {
                Some(placement) => {
                    if let Some(share) = self.ratio {
                        resize_to(id, share)?;
                    }
                    layout = json!({
                        "placed": true,
                        "tabId": placement.tab_id.0,
                        "paneId": placement.pane_id.0,
                        "besideSessionId": anchor.id.0,
                    });
                }
                None => {
                    eprintln!(
                        "dirijor: {id} started but was not docked beside {} (the tab may already hold 8 panes); dock it with dirijor move",
                        anchor.id.0
                    );
                    layout = json!({"placed": false, "besideSessionId": anchor.id.0});
                }
            }
        }
        if self.focus {
            focus_session(id)?;
        }
        Ok(layout)
    }
}

pub(super) fn split(arguments: &[String]) -> Result<(), CliError> {
    let Some(parsed) = parse(&["split"], arguments)? else {
        return Ok(());
    };
    let sessions = sessions()?;
    let anchor = target_or_self(&parsed, 0, &sessions, "split")?;
    if parsed.positionals.len() > 1 {
        return Err(CliError::failure(
            "usage: dirijor split [TARGET] [OPTIONS] [-- PROGRAM ARG...]; put the program after --",
        ));
    }
    let placement = Placement {
        edge: Some(parse_edge(parsed.value("--edge").unwrap_or("right"))?),
        beside: Some(anchor.id.0.clone()),
        ratio: parsed.value("--ratio").map(parse_ratio).transpose()?,
        focus: parsed
            .value("--focus")
            .map(|raw| parse_bool("--focus", raw))
            .transpose()?
            .unwrap_or(false),
    };
    // A split opens where the pane beside it is: the same host, and that
    // terminal's current folder unless -c says otherwise.
    let cwd = match parsed.value("--cwd") {
        Some(cwd) => cwd.to_owned(),
        None if anchor.host.is_some() => anchor.cwd.clone(),
        None => anchor
            .terminal_cwd
            .clone()
            .filter(|cwd| Path::new(cwd).is_dir())
            .unwrap_or_else(|| anchor.cwd.clone()),
    };
    let mut params = match &parsed.command {
        Some(argv) if !argv.is_empty() => {
            if argv[0].is_empty() {
                return Err(CliError::failure("the program after -- must not be empty"));
            }
            json!({"kind": diri_proto::AgentKind::new("generic"), "argv": argv, "cwd": cwd})
        }
        _ => {
            if parsed.flag("--keep-open") {
                return Err(CliError::failure(
                    "--keep-open applies to a command after --; a shell pane already stays",
                ));
            }
            json!({"kind": diri_proto::AgentKind::SHELL, "cwd": cwd})
        }
    };
    if let Some(host) = &anchor.host {
        params["host"] = json!(host);
    }
    if let Some(label) = parsed.value("--label") {
        params["title"] = json!(label);
    }
    if parsed.flag("--keep-open") {
        params["keepOpen"] = json!(true);
    }
    let anchor = placement.apply(&mut params)?;
    let spawned = request(Method::SESSION_SPAWN, params, Duration::from_secs(120))?;
    let id = spawned["id"]
        .as_str()
        .ok_or_else(|| CliError::failure(format!("spawn returned no session id: {spawned}")))?
        .to_owned();
    let layout = placement.finish(&id, anchor.as_ref())?;
    if parsed.flag("--json") {
        print_json(&json!({"id": id, "layout": layout, "session": spawned}));
    } else {
        println!("{id}");
    }
    Ok(())
}

fn engine_hello() -> Result<Value, CliError> {
    request(
        Method::HELLO,
        json!({"proto": diri_proto::WIRE_VERSION, "build": concat!("dirijor-cli/", env!("CARGO_PKG_VERSION"))}),
        Duration::from_secs(3),
    )
}

pub(super) fn version(arguments: &[String]) -> Result<(), CliError> {
    let Some(parsed) = parse(&["version"], arguments)? else {
        return Ok(());
    };
    let engine = engine_hello().ok();
    if parsed.flag("--json") {
        print_json(&json!({
            "cli": env!("CARGO_PKG_VERSION"),
            "proto": diri_proto::WIRE_VERSION,
            "engine": engine.as_ref().map(|hello| json!({
                "build": hello["build"],
                "pid": hello["pid"],
                "proto": hello["proto"],
                "kind": hello["engineKind"],
            })),
        }));
    } else {
        println!("dirijor {}", env!("CARGO_PKG_VERSION"));
        match engine {
            Some(hello) => println!(
                "Engine {} (pid {}, protocol {})",
                hello["build"].as_str().unwrap_or("unknown"),
                hello["pid"],
                hello["proto"]
            ),
            None => println!("Engine unreachable at {}", default_socket_path().display()),
        }
    }
    Ok(())
}

pub(super) fn whoami(arguments: &[String]) -> Result<(), CliError> {
    let Some(parsed) = parse(&["whoami"], arguments)? else {
        return Ok(());
    };
    let hello = engine_hello()?;
    let own = std::env::var("DIRIJOR_SESSION_ID")
        .ok()
        .filter(|id| !id.is_empty());
    let sessions = sessions()?;
    let record = own
        .as_deref()
        .and_then(|own| sessions.iter().find(|record| record.id.0 == own));
    let workspace = bridge().workspace().ok();
    let panes: Vec<Value> = match (record, &workspace) {
        (Some(record), Some(snapshot)) => shared::placements(snapshot, &record.id.0)
            .into_iter()
            .map(|placement| {
                let workspace = snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == placement.workspace_id);
                let tab = find_tab(snapshot, &placement.tab_id);
                json!({
                    "workspaceId": placement.workspace_id.0,
                    "workspace": workspace.map(|workspace| workspace.name.clone()),
                    "tabId": placement.tab_id.0,
                    "paneId": placement.pane_id.0,
                    "selectedTab": workspace.is_some_and(|workspace| workspace.selected_tab.as_ref() == Some(&placement.tab_id)),
                    "focusedPane": tab.is_some_and(|tab| tab.focused_pane == placement.pane_id),
                })
            })
            .collect(),
        _ => Vec::new(),
    };
    let value = json!({
        "sessionId": record.map(|record| record.id.0.clone()),
        "staleSessionId": own.as_ref().filter(|_| record.is_none()),
        "session": record.map(|record| json!({
            "id": record.id.0,
            "title": record.title,
            "kind": record.effective_kind().id(),
            "status": status_label(&record.status),
            "cwd": record.cwd,
            "host": record.host,
            "parent": record.parent.as_ref().map(|parent| parent.0.clone()),
        })),
        "panes": panes,
        "engine": {
            "socket": default_socket_path(),
            "build": hello["build"],
            "pid": hello["pid"],
        },
    });
    if parsed.flag("--json") {
        print_json(&value);
        return Ok(());
    }
    match (record, &own) {
        (Some(record), _) => {
            println!(
                "session  {}  {} ({}, {})",
                record.id.0,
                record.title,
                record.effective_kind().id(),
                status_label(&record.status)
            );
            for pane in &panes {
                println!(
                    "pane     {} in tab {} of {}{}",
                    pane["paneId"].as_str().unwrap_or(""),
                    pane["tabId"].as_str().unwrap_or(""),
                    pane["workspace"].as_str().unwrap_or("?"),
                    if pane["focusedPane"] == true {
                        " (focused)"
                    } else {
                        ""
                    }
                );
            }
            if panes.is_empty() {
                println!("pane     not shown in any tab");
            }
        }
        (None, Some(own)) => {
            println!("session  {own} from DIRIJOR_SESSION_ID is not a live session")
        }
        (None, None) => println!("session  none (not running inside a Diri terminal)"),
    }
    println!(
        "engine   {} (pid {}) at {}",
        hello["build"].as_str().unwrap_or("unknown"),
        hello["pid"],
        default_socket_path().display()
    );
    Ok(())
}

pub(super) fn session_inspect(arguments: &[String]) -> Result<(), CliError> {
    let Some(parsed) = parse(&["session", "inspect"], arguments)? else {
        return Ok(());
    };
    let sessions = sessions()?;
    let record = required_target(&parsed, 0, &sessions, "session inspect TARGET")?;
    let workspace = bridge().workspace().map_err(shared_error)?;
    let mut tabs: Vec<TabId> = Vec::new();
    let mut panes = Vec::new();
    for placement in shared::placements(&workspace, &record.id.0) {
        panes.push(json!({"tabId": placement.tab_id.0, "paneId": placement.pane_id.0, "workspaceId": placement.workspace_id.0}));
        if !tabs.contains(&placement.tab_id) {
            tabs.push(placement.tab_id);
        }
    }
    let process = request(
        Method::SESSION_PROCESS_INFO,
        json!({"sessionID": record.id.0}),
        Duration::from_secs(3),
    )
    .ok();
    print_json(&json!({
        "id": record.id.0,
        "session": record,
        "panes": panes,
        "tabs": tabs.iter().map(|tab| shared::tab_view(&workspace, tab, &sessions)).collect::<Vec<_>>(),
        "process": process,
    }));
    Ok(())
}

pub(super) fn session_rename(arguments: &[String]) -> Result<(), CliError> {
    let Some(parsed) = parse(&["session", "rename"], arguments)? else {
        return Ok(());
    };
    let usage = "usage: dirijor session rename TARGET TITLE";
    let (target, words) = parsed
        .positionals
        .split_first()
        .ok_or_else(|| CliError::failure(usage))?;
    let title = words.join(" ");
    if title.trim().is_empty() {
        return Err(CliError::failure(usage));
    }
    let sessions = sessions()?;
    let record = resolve_session(target, &sessions)?;
    // An Agent's name is a suggestion the user's own name outranks, the
    // same rule the MCP rename follows.
    let by = match bridge().cli_caller().map_err(shared_error)? {
        CliCaller::Agent if parsed.flag("--for-user") => {
            Some(diri_proto::RenameAuthor::AgentForUser)
        }
        CliCaller::Agent => Some(diri_proto::RenameAuthor::Agent),
        _ => None,
    };
    let params = diri_proto::SessionRenameParams {
        session_id: record.id.clone(),
        title: title.clone(),
        by,
    };
    let result = request(
        Method::SESSION_RENAME,
        serde_json::to_value(&params).map_err(|error| CliError::failure(error.to_string()))?,
        Duration::from_secs(5),
    )?;
    if result.get("applied").and_then(Value::as_bool) == Some(false) {
        return Err(CliError::failure(format!(
            "{} keeps its title: {}",
            record.id.0,
            result["reason"]
                .as_str()
                .unwrap_or("the user or Diri named it")
        )));
    }
    if parsed.flag("--json") {
        print_json(&json!({"ok": true, "id": record.id.0, "title": title}));
    } else {
        println!("renamed {} to {title}", record.id.0);
    }
    Ok(())
}

/// `tab ls`: one row per tab, in session ids.
pub(super) fn tab_list(json_output: bool) -> Result<(), CliError> {
    let snapshot = bridge().workspace().map_err(shared_error)?;
    let sessions = sessions()?;
    let tabs: Vec<Value> = snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.tabs.iter().map(|tab| {
                let mut view = shared::tab_view(&snapshot, &tab.id, &sessions);
                view["workspace_id"] = json!(workspace.id.0);
                view.as_object_mut().map(|view| view.remove("layout"));
                view
            })
        })
        .collect();
    if json_output {
        print_json(&json!({"revision": snapshot.revision, "tabs": tabs}));
        return Ok(());
    }
    if tabs.is_empty() {
        println!("No tabs.");
        return Ok(());
    }
    println!("TAB                 WORKSPACE         PANES  FOCUSED SESSION");
    for tab in &tabs {
        println!(
            "{:<19} {:<17} {:<6} {}{}",
            tab["tab_id"].as_str().unwrap_or(""),
            tab["workspace"].as_str().unwrap_or(""),
            tab["pane_count"],
            tab["focused_session"].as_str().unwrap_or("-"),
            if tab["selected"] == true {
                "  (selected)"
            } else {
                ""
            }
        );
    }
    Ok(())
}

/// `pane ls [TARGET]`: one row per pane, optionally only the tabs that show
/// TARGET.
pub(super) fn pane_list(target: Option<&str>, json_output: bool) -> Result<(), CliError> {
    let snapshot = bridge().workspace().map_err(shared_error)?;
    let sessions = sessions()?;
    let only = target
        .map(|target| resolve_session(target, &sessions).map(|record| record.id.0.clone()))
        .transpose()?;
    fn collect(node: &LayoutNode, out: &mut Vec<(PaneId, String)>) {
        match node {
            LayoutNode::Pane { id, session_id } => out.push((id.clone(), session_id.0.clone())),
            LayoutNode::Split { first, second, .. } => {
                collect(first, out);
                collect(second, out);
            }
        }
    }
    let mut rows = Vec::new();
    for workspace in &snapshot.workspaces {
        for tab in &workspace.tabs {
            let mut panes = Vec::new();
            collect(&tab.layout, &mut panes);
            if only
                .as_ref()
                .is_some_and(|only| !panes.iter().any(|(_, session)| session == only))
            {
                continue;
            }
            for (pane, session) in panes {
                let record = sessions.iter().find(|record| record.id.0 == session);
                rows.push(json!({
                    "pane_id": pane.0,
                    "tab_id": tab.id.0,
                    "workspace_id": workspace.id.0,
                    "session_id": session,
                    "title": record.map(|record| record.title.clone()),
                    "status": record.map(|record| status_label(&record.status)),
                    "focused": tab.focused_pane == pane,
                    "zoomed": tab.zoomed_pane.as_ref() == Some(&pane),
                }));
            }
        }
    }
    if json_output {
        print_json(&json!({"revision": snapshot.revision, "panes": rows}));
        return Ok(());
    }
    if rows.is_empty() {
        println!("No panes.");
        return Ok(());
    }
    println!("PANE                TAB                 SESSION      TITLE");
    for row in &rows {
        println!(
            "{:<19} {:<19} {:<12} {}{}",
            row["pane_id"].as_str().unwrap_or(""),
            row["tab_id"].as_str().unwrap_or(""),
            row["session_id"].as_str().unwrap_or(""),
            row["title"].as_str().unwrap_or(""),
            if row["focused"] == true {
                "  (focused)"
            } else {
                ""
            }
        );
    }
    Ok(())
}

/// `dirijor COMMAND --help`, `dirijor COMMAND SUB --help` and `dirijor help
/// COMMAND...`, answered from the spec. Commands with their own detailed
/// help keep it.
pub(super) fn help_for(arguments: &[String]) -> Option<String> {
    let words: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let path: Vec<&str> = match words.as_slice() {
        ["help", "--markdown"] => return Some(spec::cheat_sheet()),
        ["help", rest @ ..] if !rest.is_empty() => rest.to_vec(),
        [command, "--help" | "-h"] => vec![*command],
        [command, sub, "--help" | "-h"] => vec![*command, *sub],
        _ => return None,
    };
    let own_help = matches!(
        path.as_slice(),
        ["note" | "notes", ..]
            | [
                "session",
                "spawn" | "run" | "reset-terminal" | "terminal-title"
            ]
    );
    if own_help && words.first() != Some(&"help") {
        return None;
    }
    let command = spec::find(&path)?;
    Some(spec::help(&path, command))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn durations_take_units_and_bare_seconds() {
        let seconds = |raw| parse_duration(raw).unwrap().as_secs_f64();
        assert_eq!(seconds("30"), 30.0);
        assert_eq!(seconds("1.5"), 1.5);
        assert_eq!(seconds("500ms"), 0.5);
        assert_eq!(seconds("30s"), 30.0);
        assert_eq!(seconds("2m"), 120.0);
        assert_eq!(seconds("1h"), 3600.0);
        assert_eq!(seconds("1m30s"), 90.0);
        for bad in ["", "s", "-1", "3d", "1m30", "abc", "inf"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn chords_follow_plus_and_tmux_spellings() {
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            parse_chord("enter").unwrap(),
            (Key::Named(NamedKey::Enter), Modifiers::default())
        );
        assert_eq!(
            parse_chord("ctrl+c").unwrap(),
            (Key::Character("c".into()), ctrl)
        );
        assert_eq!(
            parse_chord("C-c").unwrap(),
            (Key::Character("c".into()), ctrl)
        );
        assert_eq!(
            parse_chord("Ctrl+C").unwrap(),
            (Key::Character("c".into()), ctrl)
        );
        // Letter names are case-insensitive; shift makes the capital.
        assert_eq!(
            parse_chord("G").unwrap(),
            (Key::Character("g".into()), Modifiers::default())
        );
        assert_eq!(
            parse_chord("shift+g").unwrap(),
            (Key::Character("g".into()), shift)
        );
        assert_eq!(
            parse_chord("shift+tab").unwrap(),
            (Key::Named(NamedKey::Tab), shift)
        );
        assert_eq!(
            parse_chord("ctrl+alt+delete").unwrap().1,
            Modifiers {
                ctrl: true,
                alt: true,
                ..Modifiers::default()
            }
        );
        assert_eq!(
            parse_chord("pgdn").unwrap().0,
            Key::Named(NamedKey::PageDown)
        );
        assert_eq!(parse_chord("F12").unwrap().0, Key::Named(NamedKey::F12));
        assert_eq!(parse_chord("space").unwrap().0, Key::Character(" ".into()));
        assert_eq!(parse_chord("+").unwrap().0, Key::Character("+".into()));
        assert_eq!(
            parse_chord("keypad:5").unwrap().0,
            Key::Keypad(KeypadKey::Five)
        );
        for bad in ["enterprise", "hyper+x", "ctrl+", "", "\u{7}"] {
            assert!(parse_chord(bad).is_err(), "{bad:?}");
        }
        // Every key the completions offer parses.
        for name in spec::KEY_NAMES {
            parse_chord(name).unwrap();
        }
    }

    #[test]
    fn ratios_and_edges_are_bounded_and_named() {
        assert_eq!(parse_ratio("30").unwrap(), 0.3);
        assert_eq!(parse_ratio("25%").unwrap(), 0.25);
        assert!(parse_ratio("5").is_err());
        assert!(parse_ratio("95").is_err());
        assert!(parse_ratio("half").is_err());
        assert_eq!(parse_edge("below").unwrap(), DockEdge::Bottom);
        assert_eq!(parse_edge("up").unwrap(), DockEdge::Top);
        assert!(parse_edge("sideways").is_err());
    }

    #[test]
    fn verbs_parse_against_their_spec() {
        let parsed = parse(
            &["wait"],
            &args(&["s_a", "--for=30s", "--until", "exited", "--json"]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.positionals, ["s_a"]);
        assert_eq!(parsed.value("--for"), Some("30s"));
        assert_eq!(parsed.values("--until"), ["exited"]);
        assert!(parsed.flag("--json"));
        // Older spellings stay accepted.
        let parsed = parse(&["wait"], &args(&["s_a", "--timeout", "5"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.value("--for"), Some("5"));
        assert!(parse(&["wait"], &args(&["s_a", "--until", "never"])).is_err());
        assert!(parse(&["wait"], &args(&["s_a", "--bogus"])).is_err());
        assert!(parse(&["wait"], &args(&["s_a", "--for"])).is_err());
        assert!(parse(&["wait"], &args(&["--help"])).unwrap().is_none());

        let parsed = parse(
            &["split"],
            &args(&[
                "-c", "/tmp", "--focus", "--label", "Logs", "--", "tail", "-f", "--json",
            ]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.value("--cwd"), Some("/tmp"));
        assert_eq!(parsed.value("--focus"), Some("true"));
        assert_eq!(parsed.value("--label"), Some("Logs"));
        assert!(!parsed.flag("--json"), "words after -- are the program's");
        assert_eq!(parsed.command.unwrap(), ["tail", "-f", "--json"]);
        assert!(parse(&["split"], &args(&["--focus=maybe"])).is_err());
    }

    #[test]
    fn run_placement_options_come_out_of_the_forwarded_list() {
        let mut options = args(&[
            "--cwd",
            "/tmp",
            "--split",
            "below",
            "--ratio",
            "30",
            "--focus=false",
            "--json",
        ]);
        let placement = Placement::take(&mut options).unwrap();
        assert_eq!(options, ["--cwd", "/tmp", "--json"]);
        assert_eq!(
            placement,
            Placement {
                edge: Some(DockEdge::Bottom),
                beside: None,
                ratio: Some(0.3),
                focus: false,
            }
        );
        let mut options = args(&["--focus"]);
        assert!(Placement::take(&mut options).unwrap().focus);
        assert!(Placement::take(&mut args(&["--ratio", "30"])).is_err());
        assert!(Placement::take(&mut args(&["--split", "diagonal"])).is_err());
    }

    #[test]
    fn parent_split_names_the_side() {
        let pane = |id: &str| LayoutNode::Pane {
            id: PaneId::new(id),
            session_id: diri_proto::SessionId::new(id),
        };
        let layout = LayoutNode::Split {
            id: SplitId::new("outer"),
            axis: diri_proto::workspace::LayoutAxis::Horizontal,
            fraction: 0.5,
            first: Box::new(pane("a")),
            second: Box::new(LayoutNode::Split {
                id: SplitId::new("inner"),
                axis: diri_proto::workspace::LayoutAxis::Vertical,
                fraction: 0.5,
                first: Box::new(pane("b")),
                second: Box::new(pane("c")),
            }),
        };
        assert_eq!(
            parent_split(&layout, &PaneId::new("a")),
            Some((SplitId::new("outer"), true))
        );
        assert_eq!(
            parent_split(&layout, &PaneId::new("c")),
            Some((SplitId::new("inner"), false))
        );
        assert_eq!(parent_split(&pane("a"), &PaneId::new("a")), None);
    }

    #[test]
    fn help_comes_from_the_spec_except_where_commands_own_it() {
        assert!(
            help_for(&args(&["wait", "--help"]))
                .unwrap()
                .contains("--for DURATION")
        );
        assert!(
            help_for(&args(&["help", "session", "rename"]))
                .unwrap()
                .contains("--for-user")
        );
        assert!(help_for(&args(&["session", "spawn", "--help"])).is_none());
        assert!(help_for(&args(&["note", "--help"])).is_none());
        assert!(help_for(&args(&["help", "nothing"])).is_none());
        assert!(help_for(&args(&["help"])).is_none());
    }
}
