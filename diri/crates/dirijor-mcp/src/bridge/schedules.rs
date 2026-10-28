use super::*;

const MAX_CATCH_UP_HOURS: f64 = 168.0;

impl Bridge {
    /// Stores a schedule in the Engine. Each run is a new top-level session,
    /// not a child of the caller: it outlives the session that scheduled it.
    pub(super) fn schedule_agent(&self, args: &Value) -> Result<Value, String> {
        let when = match (
            optional_string(args, "cron"),
            optional_number(args, "at_ms"),
            optional_number(args, "in_minutes"),
        ) {
            (Some(expr), None, None) => json!({ "kind": "cron", "expr": expr }),
            (None, Some(at), None) => json!({ "kind": "once", "at": at }),
            (None, None, Some(minutes)) if minutes >= 0.0 => {
                json!({ "kind": "once", "at": now_ms() + minutes * 60_000.0 })
            }
            _ => {
                return Err("give exactly one of cron, at_ms, or in_minutes (>= 0)".to_owned());
            }
        };
        let catch_up_ms = match optional_number(args, "catch_up_hours") {
            Some(hours) if (0.0..=MAX_CATCH_UP_HOURS).contains(&hours) => {
                Some((hours * 3_600_000.0) as u64)
            }
            Some(_) => return Err("catch_up_hours must be between 0 and 168".to_owned()),
            None => None,
        };
        let requested = required_string(args, "kind")?;
        let readiness: AgentReadinessResult =
            self.request_typed(Method::AGENT_READINESS, json!({}), DEFAULT_TIMEOUT)?;
        let prompt = required_string(args, "prompt")?;
        let spawn = SessionSpawnParams {
            keep_open: None,
            appearance: None,
            kind: resolve_agent_kind(&readiness, &requested),
            cwd: required_string(args, "cwd")?,
            new_worktree: optional_bool(args, "worktree"),
            worktree_branch: optional_string(args, "branch"),
            worktree_base: optional_string(args, "base"),
            title: None,
            initial_prompt: Some(prompt.clone()),
            parent: None,
            initial_cols: None,
            initial_rows: None,
            host: None,
            account_profile_id: None,
            same_repo_as: None,
            start_directory: None,
            note_id: None,
            argv: None,
        };
        let title = optional_string(args, "name").unwrap_or_else(|| {
            let first_line = prompt.lines().next().unwrap_or("Scheduled task");
            first_line.chars().take(60).collect()
        });
        let mut params = json!({
            "title": title,
            "when": when,
            "spawn": spawn,
            "keepAwake": optional_bool(args, "keep_awake").unwrap_or(false),
            "wakeMac": optional_bool(args, "wake_mac").unwrap_or(false),
        });
        if let Some(window) = catch_up_ms {
            params["catchUpWindowMs"] = json!(window);
        }
        self.request(Method::SCHEDULE_CREATE, params, DEFAULT_TIMEOUT)
    }

    pub(super) fn list_schedules(&self) -> Result<Value, String> {
        self.request(Method::SCHEDULE_LIST, json!({}), DEFAULT_TIMEOUT)
    }

    pub(super) fn delete_schedule(&self, args: &Value) -> Result<Value, String> {
        self.request(
            Method::SCHEDULE_DELETE,
            json!({ "id": required_string(args, "schedule_id")? }),
            DEFAULT_TIMEOUT,
        )
    }
}
