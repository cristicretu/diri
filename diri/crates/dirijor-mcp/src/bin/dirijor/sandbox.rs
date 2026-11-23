//! `dirijor sandbox`: Ghost Desktops from a shell. Inside a Diri session
//! the sandbox is leased to that session ($DIRIJOR_SESSION_ID); from a
//! plain terminal it is the person's.

use super::*;
use dirijor_mcp::bridge::sandbox::sandbox_request;

pub(super) fn run(arguments: &[String]) -> Result<(), CliError> {
    let Some(verb) = arguments.first().map(String::as_str) else {
        return Err(CliError::failure(
            "usage: dirijor sandbox <start|act|snapshot|stop|list> ...",
        ));
    };
    let rest = arguments.get(1..).unwrap_or_default();
    let words: Vec<&String> = rest.iter().filter(|word| !word.starts_with("--")).collect();
    let json_out = has_flag(rest, "--json");
    let mut tool = serde_json::Map::new();
    tool.insert("action".into(), json!(verb));
    match verb {
        "start" => {
            let flag_values: Vec<String> = ["--width", "--height", "--label"]
                .iter()
                .filter_map(|flag| option_value(rest, flag))
                .collect();
            if let Some(url) = words.iter().find(|word| !flag_values.contains(word)) {
                tool.insert("url".into(), json!(url));
            }
            for (flag, key) in [("--width", "width"), ("--height", "height")] {
                if let Some(value) = option_value(rest, flag) {
                    let value: u32 = value
                        .parse()
                        .map_err(|_| CliError::failure(format!("{flag} takes a number")))?;
                    tool.insert(key.into(), json!(value));
                }
            }
            if let Some(label) = option_value(rest, "--label") {
                tool.insert("label".into(), json!(label));
            }
        }
        "act" => {
            let (Some(id), Some(action)) = (words.first(), words.get(1)) else {
                return Err(CliError::failure(
                    "usage: dirijor sandbox act ID ACTION [KEY=VALUE ...]",
                ));
            };
            tool.insert("sandbox_id".into(), json!(id));
            let mut step = serde_json::Map::new();
            step.insert("action".into(), json!(action));
            for pair in words.iter().skip(2) {
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| CliError::failure(format!("{pair}: expected KEY=VALUE")))?;
                // Numbers stay numbers; anything else is text.
                let value = match key {
                    "x" | "y" | "dx" | "dy" | "ms" | "width" | "height" | "timeout_ms" => {
                        serde_json::from_str(value).unwrap_or_else(|_| json!(value))
                    }
                    _ => json!(value),
                };
                step.insert(key.into(), value);
            }
            tool.insert("step".into(), Value::Object(step));
        }
        "snapshot" => {
            let Some(id) = words.first() else {
                return Err(CliError::failure(
                    "usage: dirijor sandbox snapshot ID [LABEL]",
                ));
            };
            tool.insert("sandbox_id".into(), json!(id));
            if let Some(label) = words.get(1) {
                tool.insert("label".into(), json!(label));
            }
            tool.insert("inline".into(), json!(false));
        }
        "stop" => {
            let Some(id) = words.first() else {
                return Err(CliError::failure("usage: dirijor sandbox stop ID"));
            };
            tool.insert("sandbox_id".into(), json!(id));
        }
        "list" => {}
        other => return Err(CliError::failure(format!("unknown sandbox verb {other}"))),
    }
    let caller = std::env::var("DIRIJOR_SESSION_ID")
        .ok()
        .filter(|id| !id.is_empty());
    let (method, params, timeout) =
        sandbox_request(caller.as_deref(), &Value::Object(tool)).map_err(CliError::failure)?;
    let result = request(method, params, timeout)?;
    if json_out {
        print_json(&result);
        return Ok(());
    }
    match verb {
        "start" => println!(
            "{}  {}  ready in {} ms",
            result["id"].as_str().unwrap_or_default(),
            result["url"].as_str().unwrap_or_default(),
            result["spinUpMs"]
        ),
        "snapshot" => println!(
            "{}  {}x{}  sha256 {}",
            result["path"].as_str().unwrap_or_default(),
            result["width"],
            result["height"],
            result["sha256"].as_str().unwrap_or_default()
        ),
        "stop" => {}
        "list" => print_list(&result),
        _ => print_json(&result),
    }
    Ok(())
}

fn print_list(result: &Value) {
    let sandboxes = result["sandboxes"].as_array().cloned().unwrap_or_default();
    println!(
        "{} of {} slots in use",
        sandboxes.len(),
        result["capacity"].as_u64().unwrap_or(0)
    );
    for sandbox in &sandboxes {
        let usage = &sandbox["usage"];
        println!(
            "{:<4} {:>6.1} MB RAM {:>7.1} MB disk {:>2} procs  {}  {}",
            sandbox["id"].as_str().unwrap_or_default(),
            usage["memoryBytes"].as_f64().unwrap_or(0.0) / 1_048_576.0,
            usage["diskBytes"].as_f64().unwrap_or(0.0) / 1_048_576.0,
            usage["processes"].as_u64().unwrap_or(0),
            sandbox["ownerID"].as_str().unwrap_or("person"),
            sandbox["url"].as_str().unwrap_or_default(),
        );
    }
    for backend in result["backends"].as_array().into_iter().flatten() {
        println!(
            "backend {}: {}{}",
            backend["kind"].as_str().unwrap_or_default(),
            if backend["available"] == true {
                ""
            } else {
                "unavailable — "
            },
            backend["detail"].as_str().unwrap_or_default()
        );
    }
}
