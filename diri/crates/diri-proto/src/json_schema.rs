//! A small JSON Schema checker for the subset Diri's own schemas use: MCP
//! tool arguments, app action arguments, and the Engine's method catalog.
//!
//! Supported: `type` (one name or a list), `enum`, `const`, `required`,
//! `properties`, `additionalProperties: false`, `items`, `minLength` /
//! `maxLength`, `minimum` / `maximum`, `anyOf` / `oneOf` (any branch), and
//! local `$ref`s into `$defs` / `definitions`. Unknown keywords are ignored,
//! so a schema is never rejected for describing more than this checks.

use serde_json::Value;

/// Checks `value` against `schema`. `path` names the value in errors
/// (`arguments`, `args`, ...).
pub fn validate(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    validate_in(value, schema, schema, path, 0)
}

const MAX_DEPTH: usize = 64;

fn validate_in(
    value: &Value,
    schema: &Value,
    root: &Value,
    path: &str,
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("{path} is nested too deeply"));
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let target = resolve(root, reference)
            .ok_or_else(|| format!("{path}: unknown schema reference {reference}"))?;
        return validate_in(value, target, root, path, depth + 1);
    }
    if schema == &Value::Bool(false) {
        return Err(format!("{path} is not allowed"));
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(keyword).and_then(Value::as_array) {
            let mut first_error = None;
            let matched = branches.iter().any(|branch| {
                match validate_in(value, branch, root, path, depth + 1) {
                    Ok(()) => true,
                    Err(error) => {
                        first_error.get_or_insert(error);
                        false
                    }
                }
            });
            if !matched {
                return Err(first_error.unwrap_or_else(|| format!("{path} matches no schema")));
            }
        }
    }
    if let Some(types) = schema.get("type") {
        let names: Vec<&str> = match types {
            Value::String(name) => vec![name.as_str()],
            Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !names.is_empty() && !names.iter().any(|name| is_type(value, name)) {
            return Err(format!("{path} must be {}", names.join(" or ")));
        }
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!("{path} is not a supported value"));
    }
    if let Some(expected) = schema.get("const")
        && expected != value
    {
        return Err(format!("{path} is not a supported value"));
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    return Err(format!("missing required argument: {key}"));
                }
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        for (key, field) in object {
            if let Some(field_schema) = properties.and_then(|properties| properties.get(key)) {
                validate_in(
                    field,
                    field_schema,
                    root,
                    &format!("{path}.{key}"),
                    depth + 1,
                )?;
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(format!("unsupported argument: {key}"));
            } else if let Some(extra) = schema
                .get("additionalProperties")
                .filter(|extra| extra.is_object())
            {
                validate_in(field, extra, root, &format!("{path}.{key}"), depth + 1)?;
            }
        }
    }
    if let (Some(entries), Some(items)) = (value.as_array(), schema.get("items"))
        && items.is_object()
    {
        for (index, entry) in entries.iter().enumerate() {
            validate_in(entry, items, root, &format!("{path}[{index}]"), depth + 1)?;
        }
    }
    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if schema
            .get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|min| length < min)
            || schema
                .get("maxLength")
                .and_then(Value::as_u64)
                .is_some_and(|max| length > max)
        {
            return Err(format!("{path} has an invalid length"));
        }
    }
    if let Some(number) = value.as_f64()
        && (schema
            .get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|min| number < min)
            || schema
                .get("maximum")
                .and_then(Value::as_f64)
                .is_some_and(|max| number > max))
    {
        return Err(format!("{path} is outside the supported range"));
    }
    Ok(())
}

fn is_type(value: &Value, name: &str) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.as_f64().is_some_and(f64::is_finite),
        "integer" => value.is_i64() || value.is_u64(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn resolve<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    root.pointer(pointer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn checks_the_subset_diri_schemas_use() {
        let schema = json!({
            "type": "object",
            "properties": {
                "index": {"type": "integer", "minimum": -1, "maximum": 9},
                "name": {"type": ["string", "null"], "maxLength": 3},
                "edge": {"$ref": "#/$defs/Edge"},
                "either": {"anyOf": [{"type": "boolean"}, {"const": "auto"}]}
            },
            "required": ["index"],
            "additionalProperties": false,
            "$defs": {"Edge": {"type": "string", "enum": ["left", "right"]}}
        });
        assert!(validate(&json!({"index": 2}), &schema, "args").is_ok());
        assert!(validate(&json!({"index": 2, "name": null}), &schema, "args").is_ok());
        assert!(validate(&json!({"index": 2, "edge": "left"}), &schema, "args").is_ok());
        assert!(validate(&json!({"index": 2, "either": "auto"}), &schema, "args").is_ok());
        for (bad, message) in [
            (json!({}), "missing required argument: index"),
            (
                json!({"index": 10}),
                "args.index is outside the supported range",
            ),
            (json!({"index": 1.5}), "args.index must be integer"),
            (
                json!({"index": 1, "name": "long"}),
                "args.name has an invalid length",
            ),
            (
                json!({"index": 1, "edge": "up"}),
                "args.edge is not a supported value",
            ),
            (
                json!({"index": 1, "either": "x"}),
                "args.either must be boolean",
            ),
            (
                json!({"index": 1, "bogus": 1}),
                "unsupported argument: bogus",
            ),
        ] {
            assert_eq!(
                validate(&bad, &schema, "args").unwrap_err(),
                message,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_dangling_or_recursive_reference_fails_closed() {
        let dangling = json!({"$ref": "#/$defs/Missing"});
        assert!(validate(&json!(1), &dangling, "v").is_err());
        let recursive = json!({"$ref": "#"});
        assert!(validate(&json!(1), &recursive, "v").is_err());
    }
}
