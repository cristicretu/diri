//! `dirijor surface` — build and update Live Surfaces from any shell, agent
//! or scheduled task. Every verb is the MCP `surface` action with the same
//! arguments, so the CLI and agents can never disagree.

use std::io::{IsTerminal, Read};

use serde_json::{Value, json};

use super::CliError;

pub(crate) const HELP: &str = "dirijor surface list [--saved | --session] [--json]\n  \
dirijor surface create TITLE [--template today|blank] [--global | --project [DIR] | --session ID] [--pin]\n                                        [--root FILE|-]   (component JSON array; reads stdin when piped)\n  \
dirijor surface update SURFACE [--ops FILE|-] [--root FILE|-] [--title T] [--expect REVISION] [--pin | --unpin]\n                                        --ops takes a JSON array of patch ops: set, replace, remove, insert, title, root\n  \
dirijor surface get SURFACE [--outline] \n  \
dirijor surface events SURFACE [--after SEQ]   what the person ticked or clicked\n  \
dirijor surface history SURFACE [REVISION]\n  \
dirijor surface restore SURFACE REVISION\n  \
dirijor surface promote SURFACE [--project DIR]   save a session surface to the sidebar\n  \
dirijor surface delete SURFACE\n  \
dirijor surface schema                 print the component and patch reference\n  \
dirijor surface path                   print the folder where surfaces are kept";

const VALUED: &[&str] = &[
    "--template",
    "--project",
    "--session",
    "--root",
    "--ops",
    "--title",
    "--subtitle",
    "--expect",
    "--after",
    "--limit",
];

struct Args {
    positional: Vec<String>,
    flags: Vec<String>,
    values: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(arguments: &[String]) -> Self {
        let mut out = Self {
            positional: Vec::new(),
            flags: Vec::new(),
            values: Vec::new(),
        };
        let mut iter = arguments.iter().peekable();
        while let Some(argument) = iter.next() {
            if VALUED.contains(&argument.as_str()) {
                // `--project` alone means this folder; a following flag is
                // not its value.
                let value = iter
                    .next_if(|next| !next.starts_with("--") || next.as_str() == "-")
                    .cloned();
                out.values.push((argument.clone(), value));
            } else if argument.starts_with("--") {
                out.flags.push(argument.clone());
            } else {
                out.positional.push(argument.clone());
            }
        }
        out
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|flag| flag == name)
    }

    fn has(&self, name: &str) -> bool {
        self.values.iter().any(|(key, _)| key == name)
    }

    fn value(&self, name: &str) -> Option<String> {
        self.values
            .iter()
            .find(|(key, _)| key == name)
            .and_then(|(_, value)| value.clone())
    }

    fn surface(&self) -> Result<String, CliError> {
        self.positional
            .first()
            .cloned()
            .ok_or_else(|| CliError::failure("name a surface (its id or part of its title)"))
    }
}

pub(crate) fn run(arguments: &[String]) -> Result<(), CliError> {
    let Some((verb, rest)) = arguments.split_first() else {
        println!("{HELP}");
        return Ok(());
    };
    let args = Args::parse(rest);
    let request = match verb.as_str() {
        "help" | "--help" | "-h" => {
            println!("{HELP}");
            return Ok(());
        }
        "path" => {
            let dir = diri_surfaces::SurfaceStore::resolve_dir()
                .ok_or_else(|| CliError::failure("cannot find a home directory"))?;
            println!("{}", dir.display());
            return Ok(());
        }
        "schema" => {
            let result = call(json!({"op": "schema"}))?;
            println!("{}", result["guide"].as_str().unwrap_or_default());
            return Ok(());
        }
        "list" | "ls" => {
            let scope = if args.flag("--saved") {
                "saved"
            } else if args.flag("--session") || args.has("--session") {
                "session"
            } else {
                "all"
            };
            let result = call(json!({"op": "list", "scope": scope}))?;
            if args.flag("--json") {
                return print(&result);
            }
            for surface in result["surfaces"].as_array().into_iter().flatten() {
                let scope = match surface["scope"]["kind"].as_str() {
                    Some("session") => format!(
                        "session {}",
                        surface["scope"]["session"].as_str().unwrap_or("")
                    ),
                    Some("project") => format!(
                        "project {}",
                        surface["scope"]["project"].as_str().unwrap_or("")
                    ),
                    _ => "global".into(),
                };
                let done = &surface["checklist"];
                println!(
                    "{}\t{}\t{}\tr{}\t{}/{}",
                    surface["id"].as_str().unwrap_or(""),
                    surface["title"].as_str().unwrap_or(""),
                    scope,
                    surface["revision"],
                    done[0],
                    done[1],
                );
            }
            return Ok(());
        }
        "create" | "new" => {
            let mut request = json!({"op": "create"});
            if let Some(title) = args.positional.first() {
                request["title"] = json!(title);
            }
            if let Some(template) = args.value("--template") {
                request["template"] = json!(template);
            }
            if args.flag("--global") {
                request["scope"] = json!("global");
            } else if args.has("--project") {
                request["scope"] = json!("project");
                let dir = args.value("--project").unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default()
                });
                request["project"] = json!(dir);
            } else if let Some(session) = args.value("--session") {
                request["scope"] = json!("session");
                request["session"] = json!(session);
            }
            if args.flag("--pin") {
                request["pinned"] = json!(true);
            }
            if let Some(root) = read_json(&args, "--root")? {
                request["root"] = root;
            }
            request
        }
        "update" | "patch" | "set" => {
            let mut request = json!({"op": "update", "surface": args.surface()?});
            if let Some(ops) = read_json(&args, "--ops")? {
                // Accept a bare array or {"ops": [...]}.
                request["ops"] = ops.get("ops").cloned().unwrap_or(ops);
            }
            if let Some(root) = read_json(&args, "--root")? {
                request["root"] = root;
            }
            if let Some(title) = args.value("--title") {
                request["title"] = json!(title);
            }
            if let Some(subtitle) = args.value("--subtitle") {
                request["subtitle"] = json!(subtitle);
            }
            if let Some(expect) = args.value("--expect") {
                let revision: u64 = expect
                    .parse()
                    .map_err(|_| CliError::failure("--expect takes a revision number"))?;
                request["expected_revision"] = json!(revision);
            }
            if args.flag("--pin") {
                request["pinned"] = json!(true);
            } else if args.flag("--unpin") {
                request["pinned"] = json!(false);
            }
            request
        }
        "get" | "show" => {
            let mut request = json!({"op": "get", "surface": args.surface()?});
            if args.flag("--outline") {
                request["format"] = json!("outline");
                let result = call(request)?;
                print!("{}", result["outline"].as_str().unwrap_or_default());
                return Ok(());
            }
            request
        }
        "events" => {
            let mut request = json!({"op": "events", "surface": args.surface()?});
            if let Some(after) = args.value("--after") {
                request["after"] = json!(
                    after
                        .parse::<u64>()
                        .map_err(|_| CliError::failure("--after takes a number"))?
                );
            }
            if let Some(limit) = args.value("--limit") {
                request["limit"] = json!(
                    limit
                        .parse::<u64>()
                        .map_err(|_| CliError::failure("--limit takes a number"))?
                );
            }
            request
        }
        "history" | "versions" => {
            let mut request = json!({"op": "history", "surface": args.surface()?});
            if let Some(revision) = args.positional.get(1) {
                request["revision"] = json!(parse_revision(revision)?);
            }
            request
        }
        "restore" => {
            let revision = args
                .positional
                .get(1)
                .ok_or_else(|| CliError::failure("restore SURFACE REVISION"))?;
            json!({"op": "restore", "surface": args.surface()?, "revision": parse_revision(revision)?})
        }
        "promote" | "save" => {
            let mut request = json!({"op": "promote", "surface": args.surface()?});
            if args.has("--project") {
                request["scope"] = json!("project");
                request["project"] = json!(args.value("--project").unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default()
                }));
            }
            request
        }
        "delete" | "rm" => json!({"op": "delete", "surface": args.surface()?}),
        other => {
            return Err(CliError::failure(format!(
                "unknown surface verb {other:?}\n  {HELP}"
            )));
        }
    };
    print(&call(request)?)
}

fn call(request: Value) -> Result<Value, CliError> {
    super::bridge()
        .call("surface", &request)
        .map_err(CliError::failure)
}

fn print(value: &Value) -> Result<(), CliError> {
    super::print_json(value);
    Ok(())
}

fn parse_revision(text: &str) -> Result<u64, CliError> {
    text.trim_start_matches('r')
        .parse()
        .map_err(|_| CliError::failure(format!("{text:?} is not a revision number")))
}

/// JSON from `--flag FILE`, `--flag -` (stdin), or piped stdin when the
/// flag is given without a value.
fn read_json(args: &Args, flag: &str) -> Result<Option<Value>, CliError> {
    if !args.has(flag) {
        return Ok(None);
    }
    let text = match args.value(flag).as_deref() {
        Some("-") | None => {
            if std::io::stdin().is_terminal() {
                return Err(CliError::failure(format!(
                    "{flag} needs a file, or JSON piped in"
                )));
            }
            let mut text = String::new();
            std::io::stdin()
                .take(diri_proto::surface::limits::MAX_FILE_BYTES as u64 + 1)
                .read_to_string(&mut text)
                .map_err(|e| CliError::failure(format!("cannot read stdin: {e}")))?;
            text
        }
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| CliError::failure(format!("cannot read {path}: {e}")))?,
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| CliError::failure(format!("{flag}: invalid JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn flags_and_values_parse() {
        let args = Args::parse(&strings(&[
            "Today",
            "--project",
            "--pin",
            "--template",
            "today",
        ]));
        assert_eq!(args.positional, vec!["Today"]);
        assert!(args.has("--project"));
        assert_eq!(
            args.value("--project"),
            None,
            "a following flag is not a value"
        );
        assert!(args.flag("--pin"));
        assert_eq!(args.value("--template").as_deref(), Some("today"));
        let args = Args::parse(&strings(&["x", "--ops", "-"]));
        assert_eq!(args.value("--ops").as_deref(), Some("-"));
    }
}
