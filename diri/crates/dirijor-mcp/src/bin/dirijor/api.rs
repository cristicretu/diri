//! `dirijor actions`, `dirijor do` and `dirijor api`: the app's window
//! actions and the Engine's self-description, from a shell.

use super::*;
use diri_proto::api::{ApiDescribeResult, ApiListResult, ApiTextResult};
use diri_proto::app_actions::{
    ActionRisk, AppActionSpec, AppActionsListResult, DEFAULT_TIMEOUT_MS,
};

const TIMEOUT: Duration = Duration::from_secs(10);
const PERFORM_TIMEOUT: Duration = Duration::from_millis(DEFAULT_TIMEOUT_MS + 5_000);

pub(super) const HELP: &str = "App actions and API:\n  \
dirijor actions [QUERY] [--json]          the Diri window's actions and their arguments\n  \
dirijor do ACTION [KEY=VALUE ...] [--args JSON] [--json]\n  \
                                          run one (VALUE is JSON when it parses, else text)\n  \
dirijor api list [--json]                 every Engine method and app action\n  \
dirijor api describe NAME [--json]        one method's or action's JSON Schema and example\n  \
dirijor api llms                          the whole API as compact text for an Agent";

pub(super) fn actions(arguments: &[String]) -> Result<(), CliError> {
    let catalog: AppActionsListResult = typed(Method::APP_ACTIONS_LIST, json!({}))?;
    if has_flag(arguments, "--json") {
        print_json(&serde_json::to_value(&catalog).unwrap_or_default());
        return Ok(());
    }
    if !catalog.connected {
        return Err(CliError {
            code: EXIT_UNREACHABLE,
            message: "no Diri app is connected, so there are no actions to run".into(),
        });
    }
    let query = arguments
        .iter()
        .find(|argument| !argument.starts_with("--"))
        .map(|query| query.to_lowercase());
    let rows: Vec<&AppActionSpec> = catalog
        .actions
        .iter()
        .filter(|action| {
            query.as_deref().is_none_or(|query| {
                action.name.contains(query) || action.title.to_lowercase().contains(query)
            })
        })
        .collect();
    let width = rows
        .iter()
        .map(|action| action.name.len())
        .max()
        .unwrap_or(0);
    for action in rows {
        let args = signature(action);
        let risk = if action.risk == ActionRisk::Destructive {
            "  [destructive]"
        } else {
            ""
        };
        println!(
            "{:width$}  {}{}{}",
            action.name,
            action.title,
            if args.is_empty() {
                String::new()
            } else {
                format!("  ({args})")
            },
            risk
        );
    }
    Ok(())
}

pub(super) fn perform(arguments: &[String]) -> Result<(), CliError> {
    let Some(action) = arguments.first().filter(|action| !action.starts_with("--")) else {
        return Err(CliError::failure(
            "usage: dirijor do ACTION [KEY=VALUE ...] [--args JSON]",
        ));
    };
    let mut args = match option_value(arguments, "--args") {
        Some(raw) => serde_json::from_str::<Value>(&raw)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| CliError::failure("--args must be a JSON object"))?,
        None => json!({}),
    };
    let mut skip_next = false;
    for argument in &arguments[1..] {
        if std::mem::take(&mut skip_next) {
            continue;
        }
        if argument == "--args" {
            skip_next = true;
            continue;
        }
        if argument.starts_with("--") {
            continue;
        }
        let (key, value) = argument
            .split_once('=')
            .ok_or_else(|| CliError::failure(format!("expected KEY=VALUE, got {argument:?}")))?;
        args[key] = serde_json::from_str(value).unwrap_or_else(|_| json!(value));
    }
    let result = request(
        Method::APP_PERFORM,
        json!({"action": action, "args": args}),
        PERFORM_TIMEOUT,
    )?;
    let result = result.get("result").cloned().unwrap_or(Value::Null);
    if has_flag(arguments, "--json") {
        print_json(&result);
    } else if !result.is_null() {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).unwrap_or_default()
        );
    }
    Ok(())
}

pub(super) fn api(arguments: &[String]) -> Result<(), CliError> {
    let json_output = has_flag(arguments, "--json");
    match arguments.first().map(String::as_str) {
        None | Some("list") | Some("--json") => {
            let listed: ApiListResult = typed(Method::API_LIST, json!({}))?;
            if json_output {
                print_json(&serde_json::to_value(&listed).unwrap_or_default());
                return Ok(());
            }
            let width = listed
                .methods
                .iter()
                .chain(&listed.actions)
                .map(|entry| entry.name.len())
                .max()
                .unwrap_or(0);
            for method in &listed.methods {
                println!("{:width$}  {}", method.name, method.summary);
            }
            if listed.app_connected {
                println!("\nApp actions (dirijor do NAME):");
                for action in &listed.actions {
                    println!("{:width$}  {}", action.name, action.summary);
                }
            } else {
                println!("\nNo Diri app is connected, so no app actions are listed.");
            }
            Ok(())
        }
        Some("describe") => {
            let name = arguments
                .get(1)
                .filter(|name| !name.starts_with("--"))
                .ok_or_else(|| CliError::failure("usage: dirijor api describe NAME [--json]"))?;
            let described: ApiDescribeResult = typed(Method::API_DESCRIBE, json!({"name": name}))?;
            if json_output {
                print_json(&serde_json::to_value(&described).unwrap_or_default());
            } else if let Some(method) = described.method {
                println!("{} -> {}\n{}\n", method.name, method.result, method.summary);
                println!(
                    "params:\n{}\n",
                    serde_json::to_string_pretty(&method.params).unwrap_or_default()
                );
                println!(
                    "example:\n{}",
                    serde_json::to_string_pretty(&method.example).unwrap_or_default()
                );
            } else if let Some(action) = described.action {
                println!("{}: {}", action.name, action.title);
                if !action.description.is_empty() {
                    println!("{}", action.description);
                }
                println!(
                    "\nargs:\n{}",
                    serde_json::to_string_pretty(&action.args).unwrap_or_default()
                );
            }
            Ok(())
        }
        Some("llms") => {
            let text: ApiTextResult = typed(Method::API_LLMS, json!({}))?;
            print!("{}", text.text);
            Ok(())
        }
        Some(other) => Err(CliError::failure(format!(
            "unknown api command {other:?}; use list, describe or llms"
        ))),
    }
}

fn typed<T: serde::de::DeserializeOwned>(method: &str, params: Value) -> Result<T, CliError> {
    serde_json::from_value(request(method, params, TIMEOUT)?)
        .map_err(|error| CliError::failure(format!("invalid {method} response: {error}")))
}

fn signature(action: &AppActionSpec) -> String {
    let Some(properties) = action.args.get("properties").and_then(Value::as_object) else {
        return String::new();
    };
    let required: Vec<&str> = action
        .args
        .get("required")
        .and_then(Value::as_array)
        .map(|keys| keys.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    properties
        .keys()
        .map(|key| {
            if required.contains(&key.as_str()) {
                key.clone()
            } else {
                format!("{key}?")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}
