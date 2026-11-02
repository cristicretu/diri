//! `dirijor script`: run a Lua file, a line of Lua, or a REPL against the
//! running Engine, with the same `diri` API as `~/.config/diri/init.lua`;
//! `--check` validates a file without running it. `dirijor keymap` prints
//! what the window's keys run and where each binding came from.

use super::*;
use diri_script::driver::{
    Config, ConsoleGrants, ConsoleHost, Driver, Exit, Input, Sink, Source, grants,
};
use diri_script::{Layer, LoadError, Options, Runtime};
use std::io::{BufRead, IsTerminal};
use std::sync::mpsc;

pub(super) const HELP: &str = "Lua scripts (the API of ~/.config/diri/init.lua; see docs/scripting.md):\n  \
dirijor script FILE [KEY=VALUE ...] [--args JSON] [--for SECONDS]\n                                          \
run a file; KEY=VALUE and --args fill diri.args; with\n                                          \
handlers, timers or waits it runs until diri.stop(),\n                                          \
--for or Ctrl-C\n  \
dirijor script -e CODE [KEY=VALUE ...]     run a line of Lua\n  \
dirijor script --check FILE               load it without calling Diri; exit 1 on an error\n  \
dirijor script                            an interactive prompt\n  \
dirijor keymap [--json]                   every key binding and the file:line that bound it\n  \
Options: --session ID sets diri.session_id (default $DIRIJOR_SESSION_ID);\n  \
--allow-exec PROG grants diri.exec PROG without asking (repeatable).";

struct QuietSink;

impl Sink for QuietSink {
    fn layer(&mut self, layer: Layer) {
        if !layer.is_empty() {
            eprintln!(
                "dirijor: keys and palette commands only take effect from init.lua inside Diri"
            );
        }
    }

    fn loaded(&mut self, _result: &Result<(), LoadError>, _first: bool) {}
}

/// `dirijor script`'s arguments.
#[derive(Debug, Default, PartialEq)]
struct Invocation {
    code: Option<String>,
    file: Option<PathBuf>,
    until: Option<f64>,
    check: bool,
    args: serde_json::Map<String, Value>,
    session: Option<String>,
    allow: Vec<String>,
}

fn parse(arguments: &[String]) -> Result<Invocation, CliError> {
    let mut invocation = Invocation::default();
    let mut words = arguments.iter();
    let value = |words: &mut std::slice::Iter<'_, String>, flag: &str| {
        words
            .next()
            .cloned()
            .ok_or_else(|| CliError::failure(format!("{flag} needs a value")))
    };
    while let Some(word) = words.next() {
        match word.as_str() {
            "-e" | "--eval" => invocation.code = Some(value(&mut words, word)?),
            "--for" => {
                let raw = value(&mut words, word)?;
                invocation.until = Some(
                    raw.parse::<f64>()
                        .ok()
                        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                        .ok_or_else(|| CliError::failure("--for takes a number of seconds"))?,
                );
            }
            "--check" => invocation.check = true,
            "--session" => invocation.session = Some(value(&mut words, word)?),
            "--allow-exec" => invocation.allow.push(value(&mut words, word)?),
            "--args" => {
                let raw = value(&mut words, word)?;
                let text = match raw.strip_prefix('@') {
                    Some(path) => std::fs::read_to_string(path)
                        .map_err(|error| CliError::failure(format!("--args {raw}: {error}")))?,
                    None => raw,
                };
                let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&text) else {
                    return Err(CliError::failure("--args must be a JSON object"));
                };
                invocation.args.extend(object);
            }
            flag if flag.starts_with('-') && flag != "-" => {
                return Err(CliError::failure(format!("unknown option {flag}")));
            }
            positional => {
                let is_file = invocation.file.is_none()
                    && invocation.code.is_none()
                    && (!positional.contains('=') || Path::new(positional).exists());
                if is_file {
                    invocation.file = Some(PathBuf::from(positional));
                    continue;
                }
                let Some((key, raw)) = positional.split_once('=').filter(|(key, _)| {
                    !key.is_empty()
                        && key
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                }) else {
                    return Err(CliError::failure(format!(
                        "expected KEY=VALUE after the script, got {positional:?}"
                    )));
                };
                // The same rule as `dirijor do`: JSON when it parses, else text.
                let parsed = serde_json::from_str(raw).unwrap_or_else(|_| json!(raw));
                invocation.args.insert(key.to_owned(), parsed);
            }
        }
    }
    Ok(invocation)
}

pub(super) fn run(arguments: &[String]) -> Result<(), CliError> {
    let invocation = parse(arguments)?;
    let session_id = invocation.session.clone().or_else(|| {
        std::env::var(diri_proto::paths::ENV_SESSION_ID)
            .ok()
            .filter(|id| !id.is_empty())
    });
    let options = Options {
        args: Value::Object(invocation.args.clone()),
        session_id,
        ..Options::default()
    };
    let socket = default_socket_path();
    let build = concat!("dirijor-script-", env!("CARGO_PKG_VERSION"));
    if invocation.check {
        return check(&invocation, options, socket, build);
    }
    let interactive = invocation.code.is_none() && invocation.file.is_none();
    let source = match (invocation.code, invocation.file) {
        (Some(code), _) => Source::Code(code),
        (None, Some(file)) => Source::File(file),
        (None, None) => Source::Code(String::new()),
    };
    let until = invocation
        .until
        .map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
    let console_grants = ConsoleGrants {
        allow: invocation.allow,
        store: grants::default_store(),
        ask: io::stderr().is_terminal(),
    };
    let (tx, rx) = mpsc::channel();
    let host_socket = socket.clone();
    let mut driver = Driver::new(
        Config {
            source,
            options,
            socket,
            build: build.into(),
            exit_when_idle: !interactive,
        },
        Box::new(move || {
            Box::new(
                ConsoleHost::new(host_socket.clone(), build).with_grants(console_grants.clone()),
            )
        }),
        Box::new(QuietSink),
        tx.clone(),
    );
    driver
        .load()
        .map_err(|error| CliError::failure(error.message))?;
    if interactive {
        repl(tx);
    }
    let exit = driver.run(&rx, until);
    let code = driver.runtime().map_or(0, Runtime::exit_code);
    match exit {
        Exit::Stopped if code != 0 => std::process::exit(code),
        Exit::Stopped | Exit::Idle | Exit::Deadline | Exit::Disconnected => Ok(()),
    }
}

/// `--check`: loads the file in a throwaway state that never calls the
/// Engine, the window or a process, and reports what it set up.
fn check(
    invocation: &Invocation,
    options: Options,
    socket: PathBuf,
    build: &str,
) -> Result<(), CliError> {
    let Some(file) = &invocation.file else {
        return Err(CliError::failure("--check needs a FILE"));
    };
    let options = Options {
        check: true,
        root: std::path::absolute(file)
            .ok()
            .and_then(|path| path.parent().map(PathBuf::from)),
        ..options
    };
    // Only `app.actions.list` reaches the Engine, to check action names.
    let host = ConsoleHost::new(socket, build);
    let runtime =
        Runtime::new(Box::new(host), options).map_err(|error| CliError::failure(error.message))?;
    runtime
        .load_file(file)
        .map_err(|error| CliError::failure(error.message))?;
    let summary = runtime.summary();
    let count =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    println!("{}: ok", file.display());
    println!(
        "  {}, {}, {}, {}, {}",
        count(summary.keys, "key", "keys"),
        count(summary.modes, "mode", "modes"),
        count(summary.commands, "command", "commands"),
        count(summary.handlers, "handler", "handlers"),
        count(summary.timers, "timer", "timers"),
    );
    if !summary.grants_requested.is_empty() {
        println!("  asks to run: {}", summary.grants_requested.join(", "));
    }
    if let Some(at) = &summary.stopped_at {
        println!(
            "  stopped at {at}, the first call outside the script: the rest was compiled, not run"
        );
    }
    Ok(())
}

/// Reads lines on their own thread and evaluates each on the script
/// thread, so handlers a line registers keep running between prompts.
fn repl(tx: mpsc::Sender<Input>) {
    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut lines = stdin.lock().lines();
        loop {
            print!("diri> ");
            let _ = io::stdout().flush();
            let Some(Ok(code)) = lines.next() else {
                println!();
                let _ = tx.send(Input::Stop);
                return;
            };
            if code.trim().is_empty() {
                continue;
            }
            let (reply, answer) = mpsc::channel();
            if tx.send(Input::Eval { code, reply }).is_err() {
                return;
            }
            match answer.recv() {
                Ok(Ok(text)) if !text.is_empty() => println!("{text}"),
                Ok(Ok(_)) => {}
                Ok(Err(error)) => eprintln!("error: {error}"),
                Err(_) => return,
            }
        }
    });
}

/// `dirijor keymap`: the window's merged keymap, from the app.
pub(super) fn keymap(arguments: &[String]) -> Result<(), CliError> {
    let result = request(
        Method::APP_PERFORM,
        json!({"action": "list-keys", "args": {}}),
        Duration::from_secs(15),
    )?;
    let keymap = result.get("result").cloned().unwrap_or(Value::Null);
    if has_flag(arguments, "--json") {
        print_json(&keymap);
        return Ok(());
    }
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    let rows: Vec<[String; 4]> = keymap["bindings"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|binding| {
            let keys = match binding["mode"].as_str() {
                Some(mode) => format!("{mode}/{}", text(&binding["keys"])),
                None => text(&binding["keys"]),
            };
            let args = match &binding["args"] {
                Value::Object(map) if !map.is_empty() => {
                    serde_json::to_string(&binding["args"]).unwrap_or_default()
                }
                _ => String::new(),
            };
            [
                keys,
                text(&binding["action"]),
                args,
                text(&binding["source"]),
            ]
        })
        .collect();
    print_table(&["KEY", "ACTION", "ARGS", "SOURCE"], &rows);
    let modes: Vec<[String; 4]> = keymap["modes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|mode| {
            let flags = ["exclusive", "once"]
                .into_iter()
                .filter(|flag| mode[*flag].as_bool() == Some(true))
                .collect::<Vec<_>>()
                .join(", ");
            [
                text(&mode["name"]),
                flags,
                text(&mode["description"]),
                text(&mode["source"]),
            ]
        })
        .collect();
    if !modes.is_empty() {
        println!();
        print_table(&["MODE", "FLAGS", "DESCRIPTION", "SOURCE"], &modes);
    }
    Ok(())
}

fn print_table<const N: usize>(header: &[&str; N], rows: &[[String; N]]) {
    let mut widths = header.map(str::len);
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut out = String::new();
        for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
            if index + 1 == N {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{cell:width$}  "));
            }
        }
        println!("{}", out.trim_end());
    };
    line(header.to_vec());
    for row in rows {
        line(row.iter().map(String::as_str).collect());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn key_values_and_args_json_fill_diri_args() {
        let invocation = parse(&words(&[
            "deploy.lua",
            "branch=main",
            "count=3",
            "--args",
            r#"{"dry": true}"#,
            "--for",
            "5",
            "--allow-exec",
            "git",
        ]))
        .unwrap();
        assert_eq!(invocation.file, Some(PathBuf::from("deploy.lua")));
        assert_eq!(
            Value::Object(invocation.args),
            json!({"branch": "main", "count": 3, "dry": true})
        );
        assert_eq!(invocation.until, Some(5.0));
        assert_eq!(invocation.allow, vec!["git".to_owned()]);

        let inline = parse(&words(&["-e", "print(diri.args.x)", "x=1"])).unwrap();
        assert_eq!(inline.file, None);
        assert_eq!(inline.args["x"], 1);
        assert!(parse(&words(&["a.lua", "not-a-pair"])).is_err());
        assert!(parse(&words(&["--args", "[1]"])).is_err());
        assert!(parse(&words(&["--bogus"])).is_err());
    }
}
