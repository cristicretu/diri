//! `dirijor script`: run a Lua file, a line of Lua, or a REPL against the
//! running Engine, with the same `diri` API as `~/.config/diri/init.lua`.

use super::*;
use diri_script::driver::{Config, ConsoleHost, Driver, Exit, Input, Sink, Source};
use diri_script::{Layer, LoadError, Options};
use std::io::BufRead;
use std::sync::mpsc;

pub(super) const HELP: &str = "Lua scripts (the API of ~/.config/diri/init.lua; see docs/scripting.md):\n  \
dirijor script FILE [--for SECONDS]       run a file; with handlers or timers it runs until\n  \
                                          diri.stop(), --for or Ctrl-C\n  \
dirijor script -e CODE [--for SECONDS]    run a line of Lua\n  \
dirijor script                            an interactive prompt";

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

pub(super) fn run(arguments: &[String]) -> Result<(), CliError> {
    let until = option_value(arguments, "--for")
        .map(|raw| {
            raw.parse::<f64>()
                .ok()
                .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                .map(|seconds| Instant::now() + Duration::from_secs_f64(seconds))
                .ok_or_else(|| CliError::failure("--for takes a number of seconds"))
        })
        .transpose()?;
    let code = option_value(arguments, "-e").or_else(|| option_value(arguments, "--eval"));
    let file = arguments
        .iter()
        .enumerate()
        .find(|(index, argument)| {
            !argument.starts_with('-')
                && (*index == 0
                    || !matches!(arguments[index - 1].as_str(), "-e" | "--eval" | "--for"))
        })
        .map(|(_, argument)| PathBuf::from(argument));
    let interactive = code.is_none() && file.is_none();
    let source = match (code, file) {
        (Some(code), _) => Source::Code(code),
        (None, Some(file)) => Source::File(file),
        (None, None) => Source::Code(String::new()),
    };
    let socket = default_socket_path();
    let build = concat!("dirijor-script-", env!("CARGO_PKG_VERSION"));
    let (tx, rx) = mpsc::channel();
    let host_socket = socket.clone();
    let mut driver = Driver::new(
        Config {
            source,
            options: Options::default(),
            socket,
            build: build.into(),
            exit_when_idle: !interactive,
        },
        Box::new(move || Box::new(ConsoleHost::new(host_socket.clone(), build))),
        Box::new(QuietSink),
        tx.clone(),
    );
    driver
        .load()
        .map_err(|error| CliError::failure(error.message))?;
    if interactive {
        repl(tx);
    }
    match driver.run(&rx, until) {
        Exit::Stopped | Exit::Idle | Exit::Deadline | Exit::Disconnected => Ok(()),
    }
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
