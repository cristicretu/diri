//! Short, tmux-shaped verbs for shell scripts: `list`, `new`, `send`,
//! `capture` and `run --wait`. They are thin spellings over the `session`
//! commands and the same Engine requests; nothing here keeps state.

use super::*;
use diri_proto::terminal_input::{Key, KeyAction, Modifiers, NamedKey};

/// `run --wait` exits with the command's own status, so its own failures use
/// the codes `timeout(1)` and `env(1)` reserve instead of 1 to 5.
const RUN_EXIT_TIMEOUT: i32 = 124;
const RUN_EXIT_FAILURE: i32 = 125;

pub(super) const HELP: &str = "\
Scripting (TARGET is a session id, unique id prefix or unique part of a title;
inside a Diri pane your own id is $DIRIJOR_SESSION_ID):
  dirijor list [--all] [--json]
  dirijor new [--kind KIND] [--cwd PATH] [--name TEXT] [--prompt TEXT] [--host ID] [--json]
  dirijor send TARGET [-l] [--] TOKEN...      tokens: text, or keys Enter Tab Escape BSpace
                                               Up Down Left Right Home End PageUp PageDown
                                               Insert Delete Space F1..F12 C-x M-x S-x
  dirijor capture TARGET [--lines N] [--scrollback] [--json]
  dirijor run [--wait [--timeout SECONDS] [--rm]] [--cwd PATH] [--name TEXT] [--json] -- PROGRAM [ARG ...]

Exit codes: 0 ok, 1 failure, 2 timed out, 3 no such target, 4 Engine unreachable,
5 ambiguous target. `run --wait` exits with the command's status (128+N for signal N),
124 when --timeout passes and 125 when dirijor itself fails.";

pub(super) fn list(arguments: &[String]) -> Result<(), CliError> {
    session_list(arguments, false)
}

pub(super) fn new(arguments: &[String]) -> Result<(), CliError> {
    let kind = option_value(arguments, "--kind").unwrap_or_else(|| "shell".into());
    let cwd = match option_value(arguments, "--cwd") {
        Some(cwd) => cwd,
        None => std::env::current_dir()
            .map_err(|error| {
                CliError::failure(format!(
                    "could not determine the working directory: {error}"
                ))
            })?
            .to_string_lossy()
            .into_owned(),
    };
    let mut params = json!({
        "kind": kind,
        "cwd": cwd,
        "prompt": option_value(arguments, "--prompt"),
        "name": option_value(arguments, "--name").or_else(|| option_value(arguments, "--title")),
        "host": option_value(arguments, "--host"),
    });
    params
        .as_object_mut()
        .expect("spawn object")
        .retain(|_, value| !value.is_null());
    let result = bridge()
        .spawn_user_session(&params)
        .map_err(map_bridge_error)?;
    if has_flag(arguments, "--json") {
        print_json(&result);
        return Ok(());
    }
    let id = result
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::failure(format!("spawn returned no session id: {result}")))?;
    println!("{id}");
    Ok(())
}

/// One unit of `send` input, delivered in argument order.
#[derive(Debug, PartialEq)]
enum Input {
    Text(String),
    Key(Key, Modifiers),
}

/// tmux `send-keys` rules: a token that names a key is that key, anything
/// else is typed literally, and adjacent tokens are not joined by spaces.
/// `-l` (or `--literal`) makes every token text.
fn parse_send_tokens(tokens: &[String], literal: bool) -> Vec<Input> {
    let mut inputs: Vec<Input> = Vec::new();
    for token in tokens {
        match (!literal).then(|| parse_key(token)).flatten() {
            Some(key @ Input::Key(..)) => inputs.push(key),
            parsed => {
                let text = match parsed {
                    Some(Input::Text(text)) => text,
                    _ => token.clone(),
                };
                if let Some(Input::Text(previous)) = inputs.last_mut() {
                    previous.push_str(&text);
                } else {
                    inputs.push(Input::Text(text));
                }
            }
        }
    }
    inputs.retain(|input| !matches!(input, Input::Text(text) if text.is_empty()));
    inputs
}

fn parse_key(token: &str) -> Option<Input> {
    let mut modifiers = Modifiers::default();
    let mut rest = token;
    // A bare modifier prefix ("C-" alone) or a token like "M-" is text.
    while rest.len() > 2 {
        match rest.get(..2) {
            Some("C-") => modifiers.ctrl = true,
            Some("M-") => modifiers.alt = true,
            Some("S-") => modifiers.shift = true,
            _ => break,
        }
        rest = &rest[2..];
    }
    let modified = modifiers != Modifiers::default();
    let named = match rest.to_ascii_lowercase().as_str() {
        "enter" | "return" => Some(NamedKey::Enter),
        "tab" => Some(NamedKey::Tab),
        "escape" | "esc" => Some(NamedKey::Escape),
        "bspace" | "backspace" => Some(NamedKey::Backspace),
        "up" => Some(NamedKey::ArrowUp),
        "down" => Some(NamedKey::ArrowDown),
        "left" => Some(NamedKey::ArrowLeft),
        "right" => Some(NamedKey::ArrowRight),
        "home" => Some(NamedKey::Home),
        "end" => Some(NamedKey::End),
        "pageup" | "pgup" | "ppage" => Some(NamedKey::PageUp),
        "pagedown" | "pgdn" | "npage" => Some(NamedKey::PageDown),
        "insert" | "ic" => Some(NamedKey::Insert),
        "delete" | "dc" => Some(NamedKey::Delete),
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
        return Some(Input::Key(Key::Named(named), modifiers));
    }
    if rest.eq_ignore_ascii_case("space") {
        return Some(if modified {
            Input::Key(Key::Character(" ".into()), modifiers)
        } else {
            Input::Text(" ".into())
        });
    }
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) if modified => Some(Input::Key(Key::Character(ch.to_string()), modifiers)),
        _ => None,
    }
}

pub(super) fn send(arguments: &[String]) -> Result<(), CliError> {
    let usage = "send TARGET [-l] [--json] [--] TOKEN...";
    let (options, tokens) = match arguments.iter().position(|argument| argument == "--") {
        Some(separator) => (&arguments[..separator], &arguments[separator + 1..]),
        None => (arguments, &[][..]),
    };
    let mut literal = false;
    let mut json_output = false;
    let mut words = Vec::new();
    for option in options {
        match option.as_str() {
            "-l" | "--literal" => literal = true,
            "--json" => json_output = true,
            _ => words.push(option.clone()),
        }
    }
    let Some((target, leading)) = words.split_first() else {
        return Err(CliError::failure(usage));
    };
    let mut tokens: Vec<String> = leading.iter().chain(tokens).cloned().collect();
    if tokens.is_empty() {
        let text =
            String::from_utf8_lossy(&stdin_bytes(1 << 20, Duration::from_millis(500))).into_owned();
        if text.is_empty() {
            return Err(CliError::failure(usage));
        }
        tokens.push(text);
        literal = true;
    }
    let inputs = parse_send_tokens(&tokens, literal);
    let sessions = sessions()?;
    let record = resolve_session(target, &sessions)?;
    let mut bytes = 0u64;
    for input in &inputs {
        match input {
            Input::Text(text) => {
                request(
                    Method::SESSION_SEND_TEXT,
                    json!({"sessionID": record.id.0, "text": text, "submit": false}),
                    Duration::from_secs(10),
                )?;
                bytes += text.len() as u64;
            }
            Input::Key(key, modifiers) => {
                let params = diri_proto::SendKeyParams {
                    session_id: record.id.clone(),
                    key: key.clone(),
                    modifiers: *modifiers,
                    action: KeyAction::Press,
                };
                params.event().map_err(CliError::failure)?;
                let result = request(
                    Method::SESSION_SEND_KEY,
                    serde_json::to_value(&params)
                        .map_err(|error| CliError::failure(error.to_string()))?,
                    Duration::from_secs(10),
                )?;
                bytes += result["bytesAccepted"].as_u64().unwrap_or(0);
            }
        }
    }
    if json_output {
        print_json(&json!({"ok": true, "id": record.id.0, "inputs": inputs.len(), "bytes": bytes}));
    }
    Ok(())
}

pub(super) fn capture(arguments: &[String]) -> Result<(), CliError> {
    let target = positional(arguments, 0)
        .ok_or_else(|| CliError::failure("capture TARGET [--lines N] [--scrollback] [--json]"))?;
    let lines = match option_value(arguments, "--lines") {
        Some(raw) => Some(
            raw.parse::<usize>()
                .ok()
                .filter(|count| *count > 0)
                .ok_or_else(|| CliError::failure("--lines expects a positive number"))?,
        ),
        None => None,
    };
    let sessions = sessions()?;
    let record = resolve_session(&target, &sessions)?;
    let text = captured_lines(&record.id.0, has_flag(arguments, "--scrollback"), lines)?;
    if has_flag(arguments, "--json") {
        print_json(&json!({"id": record.id.0, "lines": text}));
    } else {
        for line in text {
            println!("{line}");
        }
    }
    Ok(())
}

/// Terminal rows as plain text: trailing blanks trimmed from each row and
/// trailing empty rows dropped, so `--lines N` counts rows that hold output.
fn captured_lines(
    session: &str,
    scrollback: bool,
    last: Option<usize>,
) -> Result<Vec<String>, CliError> {
    let mut lines: Vec<String> = if scrollback {
        let result = request(
            Method::SESSION_READ_SCROLLBACK,
            json!({"sessionID": session}),
            Duration::from_secs(10),
        )?;
        result["lines"]
            .as_array()
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        let result = request(
            Method::SESSION_READ_SCREEN,
            json!({"sessionID": session}),
            Duration::from_secs(10),
        )?;
        result["text"]
            .as_str()
            .unwrap_or_default()
            .split('\n')
            .map(str::to_owned)
            .collect()
    };
    for line in &mut lines {
        line.truncate(line.trim_end().len());
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    if let Some(count) = last
        && lines.len() > count
    {
        lines.drain(..lines.len() - count);
    }
    Ok(lines)
}

pub(super) fn run(arguments: &[String]) -> Result<(), CliError> {
    let separator = arguments.iter().position(|argument| argument == "--");
    let options = &arguments[..separator.unwrap_or(arguments.len())];
    let wait = has_flag(options, "--wait");
    let remove = has_flag(options, "--rm");
    let timeout = match option_value(options, "--timeout") {
        Some(raw) => Some(
            raw.parse::<f64>()
                .ok()
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                .ok_or_else(|| CliError::failure("--timeout expects seconds"))?,
        ),
        None => None,
    };
    if !wait && (remove || timeout.is_some()) {
        return Err(CliError::failure("--rm and --timeout need --wait"));
    }
    // The remaining options are `session run`'s own; reuse its parser so both
    // spellings take the same literal argv.
    let mut forwarded = Vec::new();
    let mut options_iter = options.iter();
    while let Some(option) = options_iter.next() {
        match option.as_str() {
            "--wait" | "--rm" => {}
            "--timeout" => {
                options_iter.next();
            }
            _ => forwarded.push(option.clone()),
        }
    }
    if let Some(separator) = separator {
        forwarded.extend_from_slice(&arguments[separator..]);
    }
    if !wait {
        let (params, json_output) = session_run_params(&forwarded)?;
        let result = request(Method::SESSION_SPAWN, params, Duration::from_secs(120))?;
        if json_output {
            print_json(&result);
        } else {
            println!("{}", result["id"].as_str().unwrap_or_default());
        }
        return Ok(());
    }
    // From here the command's status is the exit code, so dirijor's own
    // failures move out of its range.
    let reserve = |error: CliError| CliError {
        code: if error.code == EXIT_TIMEOUT {
            RUN_EXIT_TIMEOUT
        } else {
            RUN_EXIT_FAILURE
        },
        message: error.message,
    };
    let (params, json_output) = session_run_params(&forwarded).map_err(reserve)?;
    let spawned =
        request(Method::SESSION_SPAWN, params, Duration::from_secs(120)).map_err(reserve)?;
    let id = spawned["id"]
        .as_str()
        .ok_or_else(|| CliError {
            code: RUN_EXIT_FAILURE,
            message: format!("spawn returned no session id: {spawned}"),
        })?
        .to_owned();
    let exit = wait_for_exit(&id, timeout).map_err(reserve)?;
    let lines = captured_lines(&id, true, None).map_err(reserve)?;
    if remove {
        request(
            Method::SESSION_REMOVE,
            json!({"sessionID": id}),
            Duration::from_secs(10),
        )
        .map_err(reserve)?;
    }
    let code = exit_status(&exit);
    if json_output {
        print_json(&json!({
            "id": id,
            "exitCode": exit.code,
            "signal": exit.signal,
            "lines": lines,
        }));
    } else {
        let mut stdout = io::stdout().lock();
        for line in &lines {
            let _ = writeln!(stdout, "{line}");
        }
        let _ = stdout.flush();
    }
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Shell convention: the exit code, or 128 + N for a signal.
fn exit_status(exit: &diri_proto::ExitInfo) -> i32 {
    match (exit.code, exit.signal) {
        (Some(code), _) => code,
        (None, Some(signal)) => 128 + signal,
        (None, None) => RUN_EXIT_FAILURE,
    }
}

fn wait_for_exit(id: &str, timeout: Option<f64>) -> Result<diri_proto::ExitInfo, CliError> {
    // One `events.wait` per hour keeps a request's own deadline finite while
    // `--wait` without `--timeout` waits as long as the command runs.
    const SLICE: f64 = 3600.0;
    let deadline = timeout.map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
    loop {
        let slice = match deadline {
            Some(deadline) => deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64()
                .min(SLICE),
            None => SLICE,
        };
        let result = request(
            Method::EVENTS_WAIT,
            json!({"sessionID": id, "until": ["exited"], "timeoutMs": (slice * 1000.0) as i64}),
            Duration::from_secs_f64(slice + 5.0),
        )?;
        if result.get("timedOut").and_then(Value::as_bool) != Some(true) {
            let status: SessionStatus = serde_json::from_value(result["session"]["status"].clone())
                .map_err(|error| CliError::failure(format!("invalid session status: {error}")))?;
            return match status {
                SessionStatus::Exited(exit) => Ok(exit),
                other => Err(CliError::failure(format!(
                    "session {id} stopped waiting while {}",
                    status_label(&other)
                ))),
            };
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(CliError {
                code: EXIT_TIMEOUT,
                message: format!("timed out waiting for {id}; it keeps running"),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|token| (*token).to_owned()).collect()
    }

    fn key(named: NamedKey) -> Input {
        Input::Key(Key::Named(named), Modifiers::default())
    }

    #[test]
    fn tmux_tokens_split_into_text_and_keys() {
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        assert_eq!(
            parse_send_tokens(
                &tokens(&["echo hi", "Enter", "C-c", "a", "b", "Space", "c"]),
                false
            ),
            vec![
                Input::Text("echo hi".into()),
                key(NamedKey::Enter),
                Input::Key(Key::Character("c".into()), ctrl),
                Input::Text("ab c".into()),
            ]
        );
    }

    #[test]
    fn key_names_are_case_insensitive_and_modifiers_stack() {
        let meta_ctrl = Modifiers {
            ctrl: true,
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(
            parse_send_tokens(
                &tokens(&["escape", "BSpace", "C-M-x", "S-Tab", "PPage"]),
                false
            ),
            vec![
                key(NamedKey::Escape),
                key(NamedKey::Backspace),
                Input::Key(Key::Character("x".into()), meta_ctrl),
                Input::Key(
                    Key::Named(NamedKey::Tab),
                    Modifiers {
                        shift: true,
                        ..Modifiers::default()
                    }
                ),
                key(NamedKey::PageUp),
            ]
        );
    }

    #[test]
    fn unknown_names_bare_prefixes_and_literal_mode_stay_text() {
        assert_eq!(
            parse_send_tokens(&tokens(&["C-", "M-", "C-word", "Enterprise", "x"]), false),
            vec![Input::Text("C-M-C-wordEnterprisex".into())]
        );
        assert_eq!(
            parse_send_tokens(&tokens(&["Enter", "C-c"]), true),
            vec![Input::Text("EnterC-c".into())]
        );
        assert!(parse_send_tokens(&tokens(&[""]), false).is_empty());
    }

    #[test]
    fn exit_status_follows_the_shell_convention() {
        let exit = |code, signal| diri_proto::ExitInfo {
            reason: diri_proto::ExitReason::Exited,
            code,
            signal,
            system_restart: false,
            interrupted: false,
        };
        assert_eq!(exit_status(&exit(Some(7), None)), 7);
        assert_eq!(exit_status(&exit(None, Some(9))), 137);
        assert_eq!(exit_status(&exit(None, None)), RUN_EXIT_FAILURE);
    }
}
