use super::*;
use diri_proto::tasks::TaskRecord;

impl Bridge {
    pub(super) fn submit_task(&self, args: &Value) -> Result<Value, String> {
        let target = required_string(args, "session_id")?;
        let snapshot = self.snapshot()?;
        McpPolicy::new(
            &snapshot.sessions,
            &snapshot.projects,
            self.caller.as_deref(),
        )?
        .authorize(WriteAction::SendPrompt { target: &target })?;
        let request_id = optional_string(args, "request_id").unwrap_or_else(|| {
            format!(
                "auto:{}",
                Sha256::digest(json!([target, args["text"]]).to_string().as_bytes())
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            )
        });
        self.request(Method::TASK_SUBMIT,json!({
            "caller_id":self.require_caller()?,"request_id":request_id,
            "session_id":target,"text":required_string(args,"text")?
        }),Duration::from_secs(30)).map_err(|error| format!("{error}. Task request identity: {request_id}. Retry only this identity and text; do not submit a new task to recover a lost reply."))
    }
    pub(super) fn get_task(&self, args: &Value) -> Result<Value, String> {
        self.request(
            Method::TASK_GET,
            json!({"caller_id":self.require_caller()?,"task_id":args.get("task_id"),"request_id":args.get("request_id")}),
            DEFAULT_TIMEOUT,
        )
    }
    pub(super) fn report_task(&self, args: &Value) -> Result<Value, String> {
        self.request(
            Method::TASK_REPORT,
            json!({
                "caller_id":self.require_caller()?,"task_id":required_string(args,"task_id")?,
                "status":required_string(args,"status")?,"result":optional_string(args,"result")
            }),
            DEFAULT_TIMEOUT,
        )
    }
    pub(super) fn wait_for_task(&self, args: &Value) -> Result<Value, String> {
        let id = required_string(args, "task_id")?;
        let caller = self.require_caller()?;
        let timeout = Duration::from_secs_f64(optional_number(args, "timeout_s").unwrap_or(600.0));
        let deadline = Instant::now()
            + if timeout.is_zero() {
                DEFAULT_TIMEOUT
            } else {
                timeout
            };
        let refresh = || -> Result<TaskRecord, ControlFailure> {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or(ControlFailure::Timeout)?;
            let mut client = self.connect(remaining.min(DEFAULT_TIMEOUT))?;
            let raw = client.request_until(
                Method::TASK_GET.into(),
                json!({"caller_id":caller,"task_id":id}),
                deadline,
            )?;
            serde_json::from_value(raw)
                .map_err(|_| ControlFailure::Protocol("invalid task receipt".into()))
        };
        let mut task = refresh().map_err(render_failure)?;
        if !task.status.is_terminal() && !timeout.is_zero() {
            let mut client = self
                .connect(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(DEFAULT_TIMEOUT),
                )
                .map_err(render_failure)?;
            let result =
                client.subscribe_observing(json!({"kinds":["task.updated"]}), deadline, |event| {
                    if event.is_none_or(|(name, _, value)| {
                        name != "task.updated" || value["task_id"] == id
                    }) {
                        task = refresh()?;
                    }
                    Ok(!task.status.is_terminal())
                });
            if !matches!(result, Ok(()) | Err(ControlFailure::Timeout)) {
                result.map_err(render_failure)?;
            }
        }
        Ok(
            json!({"completed":task.status==diri_proto::tasks::TaskStatus::Completed,"timed_out":!task.status.is_terminal(),"task":task}),
        )
    }
}
