//! Agents' actions on their Session's sidebar Browser page (the
//! `sidebar_browser` MCP tool). The Engine relays each action as a
//! `session.browser_pane` event; Root runs it against the page's WebKit view
//! and answers with `browser.pane_reply`. This module turns an action into
//! the scripts to run; `crate::macos::browser` runs them.
// Root runs these through WebKit, which only the macOS build has.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use serde_json::{Value, json};

/// The page helper (refs, outline, agent cursor), for WebKit's isolated
/// client world.
pub(crate) const LIB: &str = include_str!("browser_pane.js");
/// The console recorder, injected at document start in the page world.
pub(crate) const CONSOLE_HOOK: &str = include_str!("browser_pane_console.js");

/// How long the cursor glides before the action it announces.
pub(crate) const GLIDE_MS: u64 = 320;
const MAX_WAIT_MS: u64 = 10_000;
/// A result an Agent can read in one go; longer ones are cut, with a note.
const MAX_RESULT_CHARS: usize = 200_000;

/// Which JavaScript world a script runs in: the isolated client world for
/// Diri's helper, the page's own for `eval` and the console it recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum World {
    Client,
    Page,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Plan {
    /// URL, title, loading: answered from the view without a script.
    State,
    Open { url: String },
    /// Glide the agent cursor to `point` first when set, then run `body`.
    Script {
        point: Option<String>,
        body: String,
        world: World,
    },
    Screenshot,
}

pub(crate) fn plan(action: &str, args: &Value) -> Result<Plan, String> {
    let text = |key: &str| args.get(key).and_then(Value::as_str);
    let reference = || {
        text("ref")
            .filter(|reference| !reference.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("{action} needs a ref from the latest snapshot"))
    };
    Ok(match action {
        "state" => Plan::State,
        "open" => {
            let typed = text("url").ok_or("open needs a url")?;
            Plan::Open {
                url: typed_web_url(typed).ok_or_else(|| format!("not a web address: {typed}"))?,
            }
        }
        "screenshot" => Plan::Screenshot,
        "snapshot" | "press" | "scroll" => Plan::Script {
            point: None,
            body: run(action, args),
            world: World::Client,
        },
        "click" | "fill" => {
            let reference = reference()?;
            if action == "fill" && args.get("value").is_none() {
                return Err("fill needs a value".into());
            }
            Plan::Script {
                point: Some(reference),
                body: run(action, args),
                world: World::Client,
            }
        }
        "wait" => {
            let ms = args
                .get("ms")
                .and_then(Value::as_f64)
                .map_or(5_000, |ms| ms.max(0.0) as u64)
                .min(MAX_WAIT_MS);
            let condition = match text("text") {
                Some(wanted) => format!(
                    "(document.body?.innerText || \"\").includes({})",
                    js_string(wanted)
                ),
                None => "false".to_owned(),
            };
            Plan::Script {
                point: None,
                body: format!(
                    "const started = Date.now();\n\
                     while (true) {{\n\
                       if ({condition}) return JSON.stringify({{found: true, waited_ms: Date.now() - started}});\n\
                       if (Date.now() - started >= {ms}) return JSON.stringify({{found: {found_on_timeout}, waited_ms: Date.now() - started}});\n\
                       await new Promise((resolve) => setTimeout(resolve, 100));\n\
                     }}",
                    found_on_timeout = if text("text").is_some() { "false" } else { "true" },
                ),
                world: World::Client,
            }
        }
        "console" => Plan::Script {
            point: None,
            body: "const recorded = window.__diriConsole;\n\
                   if (!recorded) return JSON.stringify({entries: [], note: \"Console recording starts with the next page load.\"});\n\
                   const fresh = recorded.entries.filter((entry) => entry.seq > recorded.read);\n\
                   if (recorded.entries.length) recorded.read = recorded.entries[recorded.entries.length - 1].seq;\n\
                   return JSON.stringify({entries: fresh, note: \"Messages since the last console call (newest 200 kept).\"});"
                .to_owned(),
            world: World::Page,
        },
        "eval" => {
            let script = text("script")
                .filter(|script| !script.trim().is_empty())
                .ok_or("eval needs a script")?;
            // A bare expression is returned; a body says what it returns.
            let body = if mentions_return(script) {
                script.to_owned()
            } else {
                format!("return (\n{script}\n);")
            };
            Plan::Script {
                point: None,
                body: format!(
                    "const __diriValue = await (async () => {{\n{body}\n}})();\n\
                     try {{ return JSON.stringify(__diriValue === undefined ? null : __diriValue); }}\n\
                     catch (error) {{ return JSON.stringify(String(__diriValue)); }}"
                ),
                world: World::Page,
            }
        }
        other => return Err(format!("unknown sidebar_browser action {other}")),
    })
}

/// A client-world call into the helper, installing it first if this page
/// has not run it yet.
fn run(action: &str, args: &Value) -> String {
    format!(
        "{LIB}\nreturn JSON.stringify(await window.__diri.run({}, {}));",
        js_string(action),
        serde_json::to_string(args).unwrap_or_else(|_| "{}".into())
    )
}

/// Glides the agent cursor, labelled `badge`, to the element `reference`.
pub(crate) fn point_script(reference: &str, badge: &str) -> String {
    format!(
        "{LIB}\nreturn JSON.stringify(window.__diri.point({}, {}));",
        js_string(reference),
        js_string(badge)
    )
}

/// A script's JSON answer as a value; overlong answers are cut with a note
/// rather than refused.
pub(crate) fn result_value(json: &str) -> Value {
    if json.chars().count() > MAX_RESULT_CHARS {
        let cut: String = json.chars().take(MAX_RESULT_CHARS).collect();
        return json!({"truncated": true, "text": cut,
                      "note": "The result was cut; ask for less (eval a narrower expression)."});
    }
    serde_json::from_str(json).unwrap_or_else(|_| Value::String(json.to_owned()))
}

/// What the address bar does with typed text: a web URL as is, otherwise
/// `http://` for a loopback host and `https://` for the rest.
pub(crate) fn typed_web_url(typed: &str) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return None;
    }
    crate::agent_catalog::normal_web_url(typed).or_else(|| {
        let candidate = url::Url::parse(&format!("https://{typed}")).ok()?;
        let local = candidate.host_str().is_some_and(|host| {
            host == "localhost"
                || host == "[::1]"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        crate::agent_catalog::normal_web_url(&format!(
            "{}://{typed}",
            if local { "http" } else { "https" }
        ))
    })
}

fn js_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

fn mentions_return(script: &str) -> bool {
    script
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .any(|word| word == "return")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_become_scripts_in_the_right_world() {
        assert_eq!(plan("state", &json!({})), Ok(Plan::State));
        assert_eq!(plan("screenshot", &json!({})), Ok(Plan::Screenshot));
        assert_eq!(
            plan("open", &json!({"url": "localhost:5173/login"})),
            Ok(Plan::Open {
                url: "http://localhost:5173/login".into()
            })
        );
        assert_eq!(
            plan("open", &json!({"url": "example.com"})),
            Ok(Plan::Open {
                url: "https://example.com".into()
            })
        );
        assert!(plan("open", &json!({"url": "javascript:alert(1)"})).is_err());

        let Ok(Plan::Script { point, body, world }) =
            plan("click", &json!({"ref": "e7"}))
        else {
            panic!("click is a script");
        };
        assert_eq!(point.as_deref(), Some("e7"));
        assert_eq!(world, World::Client);
        assert!(body.contains("window.__diri.run(\"click\", {\"ref\":\"e7\"})"));
        assert!(plan("click", &json!({})).is_err(), "click needs a ref");
        assert!(plan("fill", &json!({"ref": "e1"})).is_err(), "fill needs a value");

        let Ok(Plan::Script { world, .. }) = plan("console", &json!({})) else {
            panic!("console is a script");
        };
        assert_eq!(world, World::Page, "the page world holds the recorder");
        assert!(plan("drag", &json!({})).is_err());
    }

    #[test]
    fn arguments_cannot_break_out_of_their_string() {
        let hostile = "\"); window.evil = 1; (\"";
        let Ok(Plan::Script { body, .. }) =
            plan("fill", &json!({"ref": "e1", "value": hostile}))
        else {
            panic!("fill is a script");
        };
        assert!(body.contains(r#""value":"\"); window.evil = 1; (\"""#), "{body}");
        let script = point_script("e1\"", "Codex · \"x\"");
        assert!(script.contains(r#"window.__diri.point("e1\"", "Codex · \"x\"")"#));
    }

    #[test]
    fn eval_returns_bare_expressions_and_keeps_bodies() {
        let Ok(Plan::Script { body, .. }) = plan("eval", &json!({"script": "document.title"}))
        else {
            panic!("eval is a script");
        };
        assert!(body.contains("return (\ndocument.title\n);"));
        let Ok(Plan::Script { body, .. }) = plan(
            "eval",
            &json!({"script": "const n = 2;\nreturn n * 21;"}),
        ) else {
            panic!("eval is a script");
        };
        assert!(!body.contains("return (\nconst"));
        assert!(!mentions_return("returnValue + 1"));
    }

    #[test]
    fn waits_are_bounded_and_text_waits_report_misses() {
        let Ok(Plan::Script { body, .. }) =
            plan("wait", &json!({"text": "Saved", "ms": 600000}))
        else {
            panic!("wait is a script");
        };
        assert!(body.contains(">= 10000"));
        assert!(body.contains("includes(\"Saved\")"));
        assert!(body.contains("found: false, waited_ms"));
    }

    #[test]
    fn long_results_are_cut_not_refused() {
        assert_eq!(result_value("{\"a\":1}"), json!({"a": 1}));
        assert_eq!(result_value("not json"), json!("not json"));
        let long = format!("\"{}\"", "x".repeat(MAX_RESULT_CHARS + 10));
        assert_eq!(result_value(&long)["truncated"], true);
    }
}
