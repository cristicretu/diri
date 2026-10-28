//! Multi-agent coordination: fan-out, first-ready waits, result reading, and
//! folding children's work back. Every write re-checks policy against a fresh
//! Engine snapshot; every wait subscribes before its first read.

use super::*;
use diri_proto::tasks::{TaskRecord, TaskStatus};

const MAX_BATCH_SPAWNS: usize = 8;
const MAX_WAIT_TARGETS: usize = 64;
const MAX_OVERLAP_SIBLINGS: usize = 8;
const DIFF_TIMEOUT: Duration = Duration::from_secs(45);

impl Bridge {
    pub(super) fn spawn_agents(&self, arguments: &Value) -> Result<Value, String> {
        let entries = arguments["agents"].as_array().cloned().unwrap_or_default();
        if entries.is_empty() || entries.len() > MAX_BATCH_SPAWNS {
            return Err(format!("agents must contain 1–{MAX_BATCH_SPAWNS} entries"));
        }
        let snapshot = self.snapshot()?;
        McpPolicy::new(
            &snapshot.sessions,
            &snapshot.projects,
            self.caller.as_deref(),
        )?
        .authorize(WriteAction::Spawn {
            count: entries.len(),
        })?;
        let parent = self.caller.clone().map(SessionId);
        // Launches wait on trust walls and first prompts independently, so run
        // them side by side; each keeps its own idempotent operation identity.
        let results: Vec<Value> = std::thread::scope(|scope| {
            let handles: Vec<_> = entries
                .iter()
                .map(|entry| {
                    let parent = parent.clone();
                    scope.spawn(move || self.spawn_session(entry, parent))
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| match handle.join() {
                    Ok(Ok(value)) => value,
                    Ok(Err(error)) => json!({"ok": false, "error": error}),
                    Err(_) => json!({"ok": false, "error": "spawn failed unexpectedly; inspect list_children before retrying"}),
                })
                .collect()
        });
        let session_ids: Vec<&Value> = results
            .iter()
            .filter(|result| result["ok"] == true)
            .filter_map(|result| result.pointer("/spawn_receipt/session_id"))
            .collect();
        let task_ids: Vec<&Value> = results
            .iter()
            .filter_map(|result| result.pointer("/task/task/task_id"))
            .collect();
        let since_ms = results
            .iter()
            .filter_map(|result| result["since_ms"].as_f64())
            .fold(f64::INFINITY, f64::min);
        Ok(json!({
            "ok": results.iter().all(|result| result["ok"] == true),
            "session_ids": session_ids,
            "task_ids": task_ids,
            "since_ms": since_ms.is_finite().then_some(since_ms),
            "results": results,
        }))
    }

    pub(super) fn fork_agent(&self, arguments: &Value) -> Result<Value, String> {
        let source = required_string(arguments, "session_id")?;
        let as_task = optional_bool(arguments, "task").unwrap_or(false);
        let prompt = optional_string(arguments, "prompt");
        if as_task && prompt.is_none() {
            return Err("task:true requires a prompt".into());
        }
        let caller = self.require_caller()?.to_owned();
        let snapshot = self.snapshot()?;
        let policy = McpPolicy::new(&snapshot.sessions, &snapshot.projects, Some(&caller))?;
        policy.authorize(WriteAction::Spawn { count: 1 })?;
        policy.authorize(WriteAction::Manage { target: &source })?;
        let since_ms = now_ms();
        let mut forked = self.request(
            Method::SESSION_FORK,
            json!({"sessionID": source, "parent": caller}),
            SPAWN_TIMEOUT,
        )?;
        forked["since_ms"] = json!(since_ms);
        let Some(fork_id) = forked["id"].as_str().map(str::to_owned) else {
            return Ok(forked);
        };
        if let Some(prompt) = prompt {
            let delivery = if as_task {
                let mut task = json!({
                    "session_id": fork_id,
                    "text": prompt,
                    "request_id": format!("fork:{fork_id}"),
                });
                if let Some(schema) = arguments.get("result_schema") {
                    task["result_schema"] = schema.clone();
                }
                self.submit_task(&task)
            } else {
                self.deliver_message(
                    &json!({}),
                    &fork_id,
                    &prompt,
                    true,
                    &json!(["fork_agent", fork_id, prompt]),
                )
            };
            forked[if as_task { "task" } else { "receipt" }] =
                delivery.unwrap_or_else(|error| json!({"ok": false, "error": error}));
        }
        Ok(forked)
    }

    pub(super) fn manage_agent(&self, arguments: &Value) -> Result<Value, String> {
        let id = required_string(arguments, "session_id")?;
        let snapshot = self.snapshot()?;
        McpPolicy::new(
            &snapshot.sessions,
            &snapshot.projects,
            self.caller.as_deref(),
        )?
        .authorize(WriteAction::Manage { target: &id })?;
        let (method, timeout) = match arguments["action"].as_str() {
            Some("hibernate") => (Method::SESSION_HIBERNATE, Duration::from_secs(10)),
            Some("wake") => (Method::SESSION_WAKE, Duration::from_secs(10)),
            Some("resume") => (Method::SESSION_RESUME, SPAWN_TIMEOUT),
            _ => return Err("action must be hibernate, wake, or resume".into()),
        };
        self.request(method, json!({"sessionID": id}), timeout)?;
        let sessions = self.sessions()?;
        Ok(json!({"ok": true, "session": find_session(&sessions, &id).map(compact).ok()}))
    }

    /// Returns as soon as any target needs the caller, with all that do.
    /// Stateless by design: the caller drops handled ids and calls again, so a
    /// lost reply never hides an event.
    pub(super) fn wait_any(&self, arguments: &Value) -> Result<Value, String> {
        let task_ids = optional_strings(arguments, "task_ids");
        let session_ids = optional_strings(arguments, "session_ids");
        let total = task_ids.len() + session_ids.len();
        if total == 0 {
            return Err("provide task_ids and/or session_ids to wait on".into());
        }
        if total > MAX_WAIT_TARGETS {
            return Err(format!("wait on at most {MAX_WAIT_TARGETS} targets"));
        }
        let caller = self.require_caller()?.to_owned();
        let mode = optional_string(arguments, "until").unwrap_or_else(|| "settled".into());
        let since = optional_number(arguments, "since_ms");
        let timeout =
            Duration::from_secs_f64(optional_number(arguments, "timeout_s").unwrap_or(600.0));
        let deadline = Instant::now() + timeout;
        let read_deadline = if timeout.is_zero() {
            Instant::now() + DEFAULT_TIMEOUT
        } else {
            deadline
        };
        let worked = std::cell::RefCell::new(HashSet::<String>::new());
        let assess = || -> Result<Assessment, ControlFailure> {
            let mut assessment = Assessment::default();
            for id in &task_ids {
                let remaining = read_deadline
                    .checked_duration_since(Instant::now())
                    .filter(|left| !left.is_zero())
                    .ok_or(ControlFailure::Timeout)?;
                let mut client = self.connect(remaining.min(DEFAULT_TIMEOUT))?;
                let task = client
                    .request_until(
                        Method::TASK_GET.into(),
                        json!({"caller_id": caller, "task_id": id}),
                        read_deadline.min(Instant::now() + DEFAULT_TIMEOUT),
                    )
                    .and_then(|raw| {
                        serde_json::from_value::<TaskRecord>(raw)
                            .map_err(|_| ControlFailure::Protocol("invalid task receipt".into()))
                    });
                match task {
                    Ok(task) if task.status.is_terminal() || task.status == TaskStatus::Blocked => {
                        assessment.ready.push(json!({
                            "type": "task",
                            "id": id,
                            "reason": serde_json::to_value(&task.status).unwrap_or_default(),
                            "task": task,
                        }));
                    }
                    Ok(_) => assessment.pending.push(json!({"type": "task", "id": id})),
                    Err(ControlFailure::Daemon(error)) => assessment.ready.push(json!({
                        "type": "task", "id": id, "reason": "unavailable", "error": error.to_string(),
                    })),
                    Err(other) => return Err(other),
                }
            }
            if !session_ids.is_empty() {
                let sessions = self.sessions_before(read_deadline)?;
                let mut worked = worked.borrow_mut();
                for id in &session_ids {
                    let Some(record) = sessions.iter().find(|record| record.id.0 == *id) else {
                        assessment
                            .ready
                            .push(json!({"type": "session", "id": id, "reason": "removed"}));
                        continue;
                    };
                    if matches!(record.status, SessionStatus::Working) {
                        worked.insert(id.clone());
                    }
                    if reached_since(&mode, record, since, worked.contains(id)) {
                        let reason = match &record.status {
                            SessionStatus::Exited(_) => "exited",
                            SessionStatus::NeedsInput(_) => "needs_input",
                            _ => "done",
                        };
                        let mut ready = compact(record);
                        ready["type"] = json!("session");
                        ready["reason"] = json!(reason);
                        if let Some(detail) = &record.needs_input {
                            ready["needs_input"] = serde_json::to_value(detail).unwrap_or_default();
                        }
                        assessment.ready.push(ready);
                    } else {
                        assessment.pending.push(json!({
                            "type": "session", "id": id, "status": status_label(&record.status),
                        }));
                    }
                }
            }
            Ok(assessment)
        };
        let mut latest = assess().map_err(render_failure)?;
        if latest.ready.is_empty() && Instant::now() < deadline {
            let mut client = self
                .connect(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(DEFAULT_TIMEOUT),
                )
                .map_err(render_failure)?;
            // Task events carry no session id, so filter by kind only and
            // skip task events for other tasks without a round trip.
            let result = client.subscribe_observing(
                json!({"kinds": ["task.updated", "session.updated", "session.removed"]}),
                deadline,
                |event| {
                    let relevant = match event {
                        None => true,
                        Some(("task.updated", _, params)) => params["task_id"]
                            .as_str()
                            .is_some_and(|id| task_ids.iter().any(|wanted| wanted == id)),
                        Some(_) => !session_ids.is_empty(),
                    };
                    if relevant {
                        latest = assess()?;
                    }
                    Ok(latest.ready.is_empty())
                },
            );
            if !matches!(result, Ok(()) | Err(ControlFailure::Timeout)) {
                result.map_err(render_failure)?;
            }
        }
        let timed_out = latest.ready.is_empty();
        Ok(json!({
            "ready": latest.ready,
            "pending": latest.pending,
            "timed_out": timed_out,
            "note": if timed_out {
                "Nothing needed attention before the timeout; call again to keep waiting."
            } else {
                "Handle the ready items, then call wait_any again with only the pending ids."
            },
        }))
    }

    pub(super) fn read_output(&self, arguments: &Value) -> Result<Value, String> {
        let id = required_string(arguments, "session_id")?;
        let mode = optional_string(arguments, "mode").unwrap_or_else(|| "screen".into());
        match mode.as_str() {
            "last_message" | "transcript" => {
                let turns = if mode == "last_message" {
                    // Read a few turns so a trailing user echo is skipped.
                    4
                } else {
                    optional_number(arguments, "turns").unwrap_or(6.0) as u32
                };
                let transcript: ReadTranscriptResult = self.request_typed(
                    Method::SESSION_READ_TRANSCRIPT,
                    json!({"sessionID": id, "turns": turns}),
                    DEFAULT_TIMEOUT,
                )?;
                if !transcript.available {
                    let mut fallback = self.read_screen(&id, Some(50))?;
                    if fallback.get("mode").is_none() {
                        fallback["mode"] = json!("tail");
                    }
                    fallback["transcript_unavailable"] = json!(transcript.reason);
                    return Ok(fallback);
                }
                if mode == "last_message" {
                    let message = transcript
                        .turns
                        .iter()
                        .rev()
                        .find(|turn| turn.role == "agent")
                        .map(|turn| turn.text.clone());
                    return Ok(json!({"mode": mode, "message": message}));
                }
                Ok(json!({"mode": mode, "turns": transcript.turns}))
            }
            "since" => self.read_since(&id, arguments),
            "tail" => {
                let lines = optional_number(arguments, "lines")
                    .unwrap_or(50.0)
                    .clamp(1.0, 500.0) as usize;
                self.read_screen(&id, Some(lines))
            }
            _ => self.read_screen(&id, None),
        }
    }

    fn read_screen(&self, id: &str, tail: Option<usize>) -> Result<Value, String> {
        let raw = match self.request_failure(
            Method::SESSION_READ_SCREEN,
            json!({"sessionID": id}),
            DEFAULT_TIMEOUT,
        ) {
            Ok(raw) => raw,
            // A note has text, not a screen: hand that over instead of an
            // error the calling Agent would retry.
            Err(ControlFailure::Daemon(error))
                if error.code == diri_proto::control::SESSION_HAS_NO_TERMINAL =>
            {
                let mut note = self.read_note(&json!({ "note": id }))?;
                note["mode"] = json!("note");
                note["note"] = json!(
                    "This session is a note, which has no terminal; its Markdown is in `markdown`. Use read_note and edit_note for notes."
                );
                return Ok(note);
            }
            // Ended without a kept screen: say so once, with what is left.
            Err(ControlFailure::Daemon(error))
                if error.code == diri_proto::control::TERMINAL_NOT_RETAINED =>
            {
                let mut ended = json!({
                    "mode": "ended",
                    "text": "",
                    "screen_unavailable": error.message,
                });
                if let Ok(transcript) = self.request_typed::<ReadTranscriptResult>(
                    Method::SESSION_READ_TRANSCRIPT,
                    json!({"sessionID": id, "turns": 6}),
                    DEFAULT_TIMEOUT,
                ) && transcript.available
                {
                    ended["turns"] = json!(transcript.turns);
                }
                return Ok(ended);
            }
            Err(error) => return Err(render_failure(error)),
        };
        let mut result: ReadScreenResult = serde_json::from_value(raw).map_err(|error| {
            format!("invalid {} response: {error}", Method::SESSION_READ_SCREEN)
        })?;
        if let Some(lines) = tail {
            let kept: Vec<&str> = result.text.lines().collect();
            result.text = kept[kept.len().saturating_sub(lines)..].join("\n");
        }
        serde_json::to_value(result).map_err(|error| error.to_string())
    }

    /// Terminal lines appended since `cursor`. Rows above the live screen are
    /// immutable history, so the next cursor stops there and the live screen
    /// (which TUIs redraw in place) is re-read each time.
    fn read_since(&self, id: &str, arguments: &Value) -> Result<Value, String> {
        let scrollback: diri_proto::ReadScrollbackResult = self.request_typed(
            Method::SESSION_READ_SCROLLBACK,
            json!({"sessionID": id}),
            DEFAULT_TIMEOUT,
        )?;
        let limit = optional_number(arguments, "lines")
            .unwrap_or(200.0)
            .clamp(1.0, 2000.0) as usize;
        let cursor = optional_number(arguments, "cursor").map_or(0, |cursor| cursor as i64);
        let first = scrollback.first_row;
        let start = cursor.max(first);
        let skip = (start - first).max(0) as usize;
        let mut lines: Vec<&str> = scrollback
            .lines
            .iter()
            .skip(skip)
            .map(String::as_str)
            .collect();
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        let clipped = lines.len() > limit;
        let lines = &lines[lines.len().saturating_sub(limit)..];
        let next = scrollback.visible_start_row.max(start);
        Ok(json!({
            "mode": "since",
            "text": lines.join("\n"),
            "cursor": next,
            "gap": cursor > 0 && cursor < first,
            "clipped": clipped,
            "alt_screen": scrollback.is_alt_screen,
        }))
    }

    pub(super) fn get_diff(&self, arguments: &Value) -> Result<Value, String> {
        let id = required_string(arguments, "session_id")?;
        let base = optional_string(arguments, "base").unwrap_or_else(|| "default_branch".into());
        let diff = self.read_diff(&id, &base)?;
        let files = diff_stat(&diff.patch);
        let mut result = json!({
            "session_id": id,
            "repo_root": diff.repo_root,
            "base_ref": diff.base_ref,
            "truncated": diff.truncated,
            "files": files.iter().map(|file| json!({
                "path": file.path, "added": file.added, "removed": file.removed, "binary": file.binary,
            })).collect::<Vec<_>>(),
            "added": files.iter().map(|file| file.added).sum::<u64>(),
            "removed": files.iter().map(|file| file.removed).sum::<u64>(),
        });
        if base == "default_branch"
            && let Ok(uncommitted) = self.read_diff(&id, "head")
        {
            result["uncommitted_files"] = json!(
                diff_stat(&uncommitted.patch)
                    .into_iter()
                    .map(|file| file.path)
                    .collect::<Vec<_>>()
            );
        }
        if optional_bool(arguments, "overlaps").unwrap_or(true) {
            result["overlaps"] = self.overlaps(&id, &base, &files)?;
        }
        if optional_bool(arguments, "patch").unwrap_or(false) {
            let limit = optional_number(arguments, "max_patch_bytes").unwrap_or(32768.0) as usize;
            let patch = String::from_utf8_lossy(&diff.patch);
            let mut end = patch.len().min(limit);
            while !patch.is_char_boundary(end) {
                end -= 1;
            }
            result["patch"] = json!(&patch[..end]);
            result["patch_clipped"] = json!(end < patch.len());
        }
        Ok(result)
    }

    fn read_diff(&self, id: &str, base: &str) -> Result<diri_proto::SessionReadDiffResult, String> {
        let base = if base == "head" {
            "head"
        } else {
            "defaultBranch"
        };
        self.request_typed(
            Method::SESSION_READ_DIFF,
            json!({"sessionID": id, "base": base}),
            DIFF_TIMEOUT,
        )
    }

    /// Files this session changed that its live siblings (same parent, other
    /// checkout) also changed: the conflicts integration would hit.
    fn overlaps(&self, id: &str, base: &str, files: &[FileStat]) -> Result<Value, String> {
        let sessions = self.sessions()?;
        let record = find_session(&sessions, id)?;
        let Some(parent) = &record.parent else {
            return Ok(json!([]));
        };
        let mine: HashSet<&str> = files.iter().map(|file| file.path.as_str()).collect();
        let siblings: Vec<&SessionRecord> = sessions
            .iter()
            .filter(|other| {
                other.id != record.id
                    && other.parent.as_ref() == Some(parent)
                    && other.cwd != record.cwd
                    && !other.is_archived()
                    && !matches!(other.status, SessionStatus::Exited(_))
            })
            .take(MAX_OVERLAP_SIBLINGS)
            .collect();
        let overlaps: Vec<Value> = siblings
            .into_iter()
            .filter_map(|sibling| {
                let diff = self.read_diff(&sibling.id.0, base).ok()?;
                let shared: Vec<String> = diff_stat(&diff.patch)
                    .into_iter()
                    .map(|file| file.path)
                    .filter(|path| mine.contains(path.as_str()))
                    .collect();
                (!shared.is_empty()).then(
                    || json!({"session_id": sibling.id.0, "title": sibling.title, "paths": shared}),
                )
            })
            .collect();
        Ok(Value::Array(overlaps))
    }

    pub(super) fn integrate(&self, arguments: &Value) -> Result<Value, String> {
        let source = required_string(arguments, "session_id")?;
        let caller = self.require_caller()?.to_owned();
        let snapshot = self.snapshot()?;
        McpPolicy::new(&snapshot.sessions, &snapshot.projects, Some(&caller))?
            .authorize(WriteAction::Manage { target: &source })?;
        self.request(
            Method::WORKTREE_INTEGRATE,
            json!({
                "sourceSessionID": source,
                "targetSessionID": caller,
                "strategy": optional_string(arguments, "strategy").unwrap_or_else(|| "merge".into()),
                "message": optional_string(arguments, "message"),
            }),
            Duration::from_secs(120),
        )
    }
}

#[derive(Default)]
struct Assessment {
    ready: Vec<Value>,
    pending: Vec<Value>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct FileStat {
    pub(super) path: String,
    pub(super) added: u64,
    pub(super) removed: u64,
    pub(super) binary: bool,
}

/// Per-file +/- counts from a unified diff. Paths come from the `b/` side of
/// each `diff --git` header (the `a/` side for deletions).
pub(super) fn diff_stat(patch: &[u8]) -> Vec<FileStat> {
    let text = String::from_utf8_lossy(patch);
    let mut files: Vec<FileStat> = Vec::new();
    let mut in_hunk = false;
    for line in text.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            in_hunk = false;
            let path = header
                .rsplit_once(" b/")
                .map(|(_, path)| path)
                .or_else(|| header.strip_prefix("a/"))
                .unwrap_or(header);
            files.push(FileStat {
                path: path.to_owned(),
                added: 0,
                removed: 0,
                binary: false,
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if line.starts_with("@@") {
            in_hunk = true;
        } else if !in_hunk {
            if line.starts_with("Binary files ") {
                file.binary = true;
            } else if let Some(path) = line.strip_prefix("+++ b/") {
                file.path = path.to_owned();
            }
        } else if line.starts_with('+') {
            file.added += 1;
        } else if line.starts_with('-') {
            file.removed += 1;
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_stat_counts_hunk_lines_and_ignores_headers() {
        let patch = b"diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,3 @@\n keep\n-old\n+new\n+more\ndiff --git a/img.png b/img.png\nBinary files a/img.png and b/img.png differ\ndiff --git a/new file.txt b/new file.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new file.txt\n@@ -0,0 +1 @@\n+hello\n";
        let stats = diff_stat(patch);
        assert_eq!(
            stats,
            vec![
                FileStat {
                    path: "src/a.rs".into(),
                    added: 2,
                    removed: 1,
                    binary: false
                },
                FileStat {
                    path: "img.png".into(),
                    added: 0,
                    removed: 0,
                    binary: true
                },
                FileStat {
                    path: "new file.txt".into(),
                    added: 1,
                    removed: 0,
                    binary: false
                },
            ]
        );
    }
}
