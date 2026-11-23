//! Moving a Claude Code or Codex transcript to another Mac: only the `cwd`
//! members change, so the CLI's own "is this conversation from here" checks
//! pass where the project now lives. Message content is left byte-for-byte.

use serde_json::Value;

/// Most bytes one transcript may carry in a bundle.
pub const MAX_TRANSCRIPT_BYTES: u64 = 64 * 1024 * 1024;

/// Rewrites every `"cwd"` string member (at any depth) of every JSONL line
/// through `remap`. Lines without one, or that are not JSON, are copied
/// unchanged. Rewriting is a byte splice of the encoded value where the line
/// is compact JSON, so member order and the rest of the line are untouched.
pub fn rewrite_cwd(input: &[u8], remap: impl Fn(&str) -> String) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    for line in input.split_inclusive(|byte| *byte == b'\n') {
        let (body, newline) = match line.strip_suffix(b"\n") {
            Some(body) => (body, true),
            None => (line, false),
        };
        out.extend_from_slice(&rewrite_line(body, &remap));
        if newline {
            out.push(b'\n');
        }
    }
    out
}

fn rewrite_line(line: &[u8], remap: &impl Fn(&str) -> String) -> Vec<u8> {
    if memfind(line, b"\"cwd\"").is_none() {
        return line.to_vec();
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(line) else {
        return line.to_vec();
    };
    let mut pairs = Vec::new();
    collect_cwds(&value, remap, &mut pairs);
    if pairs.is_empty() {
        return line.to_vec();
    }
    let mut spliced = line.to_vec();
    let mut ok = true;
    for (old, new) in &pairs {
        let old = format!("\"cwd\":{}", Value::String(old.clone()));
        let new = format!("\"cwd\":{}", Value::String(new.clone()));
        let replaced = replace_all(&spliced, old.as_bytes(), new.as_bytes());
        if replaced.1 == 0 {
            ok = false;
            break;
        }
        spliced = replaced.0;
    }
    if ok {
        return spliced;
    }
    rewrite_values(&mut value, remap);
    serde_json::to_vec(&value).unwrap_or_else(|_| line.to_vec())
}

fn collect_cwds(value: &Value, remap: &impl Fn(&str) -> String, pairs: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "cwd"
                    && let Value::String(old) = child
                {
                    let new = remap(old);
                    if &new != old && !pairs.iter().any(|(o, _)| o == old) {
                        pairs.push((old.clone(), new));
                    }
                } else {
                    collect_cwds(child, remap, pairs);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_cwds(item, remap, pairs);
            }
        }
        _ => {}
    }
}

fn rewrite_values(value: &mut Value, remap: &impl Fn(&str) -> String) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if key == "cwd"
                    && let Value::String(old) = child
                {
                    *old = remap(old);
                } else {
                    rewrite_values(child, remap);
                }
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| rewrite_values(item, remap)),
        _ => {}
    }
}

fn memfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn replace_all(haystack: &[u8], needle: &[u8], with: &[u8]) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(haystack.len());
    let mut count = 0;
    let mut rest = haystack;
    while let Some(at) = memfind(rest, needle) {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(with);
        rest = &rest[at + needle.len()..];
        count += 1;
    }
    out.extend_from_slice(rest);
    (out, count)
}

/// The conversation id a Codex rollout file name ends with
/// (`rollout-2026-11-01T10-00-00-<uuid>.jsonl`).
pub fn codex_rollout_id(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".jsonl")?;
    if stem.len() < 36 {
        return None;
    }
    let id = &stem[stem.len() - 36..];
    (id.bytes().filter(|b| *b == b'-').count() == 4
        && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
    .then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_cwd_members_change() {
        let input = concat!(
            r#"{"type":"user","cwd":"/Users/alice/work/repo","message":{"content":"look at /Users/alice/work/repo/a.rs"},"sessionId":"x"}"#,
            "\n",
            r#"{"type":"session_meta","payload":{"id":"y","cwd":"/Users/alice/work/repo","git":{}}}"#,
            "\n",
            "not json with \"cwd\"\n",
            r#"{"type":"summary"}"#,
        );
        let out = rewrite_cwd(input.as_bytes(), |cwd| {
            cwd.replace("/Users/alice/work/repo", "/Users/bob/code/repo")
        });
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(r#""cwd":"/Users/bob/code/repo","message":{"content":"look at /Users/alice/work/repo/a.rs"}"#));
        assert!(out.contains(r#"{"id":"y","cwd":"/Users/bob/code/repo","git":{}}"#));
        assert!(out.contains("not json with \"cwd\"\n"));
        assert!(out.ends_with(r#"{"type":"summary"}"#));
        assert_eq!(out.lines().count(), 4);
    }

    #[test]
    fn spaced_json_falls_back_to_reencoding() {
        let input = br#"{"cwd": "/a/b", "n": 1}"#;
        let out = rewrite_cwd(input, |_| "/c".into());
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["cwd"], "/c");
        assert_eq!(value["n"], 1);
    }

    #[test]
    fn rollout_ids() {
        assert_eq!(
            codex_rollout_id(
                "rollout-2026-11-01T10-00-00-0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.jsonl"
            ),
            Some("0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b")
        );
        assert_eq!(codex_rollout_id("x.jsonl"), None);
    }
}
