//! The `sandbox` tool and `dirijor sandbox`: one request shape for both.

use std::time::Duration;

use diri_proto::Method;
use serde_json::{Map, Value, json};

/// `(method, params, timeout)` for one `sandbox` call. `caller` is the
/// acting session; `None` acts as the person (a plain shell).
pub fn sandbox_request(
    caller: Option<&str>,
    arguments: &Value,
) -> Result<(&'static str, Value, Duration), String> {
    let action = arguments
        .get("action")
        .and_then(Value::as_str)
        .ok_or("missing required argument: action")?;
    let mut params = Map::new();
    if let Some(caller) = caller {
        params.insert("sessionID".into(), json!(caller));
    }
    let sandbox_id = || {
        arguments
            .get("sandbox_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(|id| json!(id))
            .ok_or_else(|| format!("{action} needs sandbox_id"))
    };
    let copy = |params: &mut Map<String, Value>, from: &str, to: &str| {
        if let Some(value) = arguments.get(from).filter(|value| !value.is_null()) {
            params.insert(to.into(), value.clone());
        }
    };
    let request = match action {
        "start" => {
            for key in ["url", "width", "height", "label", "backend"] {
                copy(&mut params, key, key);
            }
            (Method::SANDBOX_START, Duration::from_secs(45))
        }
        "act" => {
            params.insert("sandboxID".into(), sandbox_id()?);
            let step = arguments
                .get("step")
                .and_then(Value::as_object)
                .ok_or("act needs step, e.g. {\"action\":\"click\",\"selector\":\"#go\"}")?;
            if !step.contains_key("action") {
                return Err("step needs an action".into());
            }
            for (key, value) in step {
                // The wire spells it timeoutMs.
                let key = if key == "timeout_ms" {
                    "timeoutMs"
                } else {
                    key
                };
                if key != "sessionID" && key != "sandboxID" {
                    params.insert(key.to_owned(), value.clone());
                }
            }
            (Method::SANDBOX_ACT, Duration::from_secs(75))
        }
        "snapshot" => {
            params.insert("sandboxID".into(), sandbox_id()?);
            copy(&mut params, "label", "label");
            params.insert(
                "inline".into(),
                arguments.get("inline").cloned().unwrap_or(json!(true)),
            );
            (Method::SANDBOX_SNAPSHOT, Duration::from_secs(45))
        }
        "stop" => {
            params.insert("sandboxID".into(), sandbox_id()?);
            (Method::SANDBOX_STOP, Duration::from_secs(20))
        }
        "list" => (Method::SANDBOX_LIST, Duration::from_secs(15)),
        other => return Err(format!("unknown sandbox action {other}")),
    };
    Ok((request.0, Value::Object(params), request.1))
}

/// A snapshot as an MCP result: the facts as text and structured content,
/// and the JPEG as an image the model sees.
pub fn sandbox_snapshot_result(mut result: Value) -> Value {
    let jpeg = result
        .as_object_mut()
        .and_then(|object| object.remove("jpeg"))
        .and_then(|jpeg| jpeg.as_str().map(str::to_owned));
    let mut content = vec![json!({"type": "text", "text": result.to_string()})];
    if let Some(jpeg) = jpeg {
        content.push(json!({"type": "image", "data": jpeg, "mimeType": "image/jpeg"}));
    }
    json!({"mcpToolResult": {"content": content, "structuredContent": result}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn act_flattens_the_step_under_the_caller() {
        let (method, params, _) = sandbox_request(
            Some("s1"),
            &json!({"action": "act", "sandbox_id": "g2",
                    "step": {"action": "wait_for", "selector": "#ok", "timeout_ms": 900,
                             "sessionID": "forged"}}),
        )
        .unwrap();
        assert_eq!(method, Method::SANDBOX_ACT);
        assert_eq!(
            params,
            json!({"sessionID": "s1", "sandboxID": "g2", "action": "wait_for",
                   "selector": "#ok", "timeoutMs": 900})
        );
        let decoded: diri_proto::sandbox::SandboxActParams =
            serde_json::from_value(params).unwrap();
        assert_eq!(decoded.session_id.unwrap().0, "s1");
    }

    #[test]
    fn the_person_calls_without_a_session() {
        let (method, params, _) =
            sandbox_request(None, &json!({"action": "start", "url": "localhost:3000"})).unwrap();
        assert_eq!(method, Method::SANDBOX_START);
        assert_eq!(params, json!({"url": "localhost:3000"}));
    }

    #[test]
    fn missing_pieces_are_named() {
        assert!(
            sandbox_request(Some("s"), &json!({"action": "stop"}))
                .unwrap_err()
                .contains("sandbox_id")
        );
        assert!(
            sandbox_request(Some("s"), &json!({"action": "act", "sandbox_id": "g1"}))
                .unwrap_err()
                .contains("step")
        );
        assert!(sandbox_request(Some("s"), &json!({"action": "fly"})).is_err());
    }

    #[test]
    fn snapshots_carry_their_image() {
        let result =
            sandbox_snapshot_result(json!({"path": "/x.png", "sha256": "ab", "jpeg": "AAAA"}));
        let content = &result["mcpToolResult"]["content"];
        assert_eq!(content[1]["type"], "image");
        assert!(
            result["mcpToolResult"]["structuredContent"]
                .get("jpeg")
                .is_none()
        );
    }
}
