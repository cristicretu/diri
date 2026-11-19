//! App Skills: computer use learns a task in a Mac app once, saves it per
//! app as a parameterized `run_steps` program, and replays it later with no
//! model in the loop. When a replay breaks, the calling Agent repairs it.
//!
//! The Engine makes no model calls. The Agent calling `app_skills` learns,
//! compiles and repairs; the Engine records (only in learn mode), validates,
//! verifies by replaying, stores, and replays through `computer.act`.
//! Nothing runs while no Agent is calling: no process, thread or poll.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use diri_proto::{AppSkillStatus, AppSkillSummary, AppSkillsApp, AppSkillsList};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::ComputerError;

mod compile;
mod recorder;
mod runner;
mod store;
#[cfg(test)]
mod tests;

pub(super) use runner::Actor;
#[cfg(test)]
pub(super) use runner::BundleInfo;

/// The draft or arguments are not a valid skill or run.
pub const SKILL_INVALID: &str = "computer_skill_invalid";
/// No skill (or app) by that name.
pub const SKILL_NOT_FOUND: &str = "computer_skill_not_found";
/// A destructive skill over several items needs `confirm: true`, which the
/// Agent may only pass after the person agreed.
pub const SKILL_CONFIRM: &str = "computer_skill_confirm";
/// The action needs the calling session (learn, trace, save, run).
pub const SKILL_NEEDS_SESSION: &str = "computer_skill_needs_session";

const SCHEMA_VERSION: u32 = 1;
/// Consecutive failed runs that mark a skill broken.
const BROKEN_AFTER: u32 = 3;
/// Most items one `run` takes.
const MAX_ITEMS: usize = 50;
/// A run stops starting items after this long and reports the rest.
const RUN_BUDGET: Duration = Duration::from_secs(600);
/// Runs `history` shows.
const HISTORY_RUNS: usize = 20;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct AppRef {
    pub bundle_id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ParamKind {
    #[default]
    String,
    Number,
    Boolean,
}

impl ParamKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct Param {
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: ParamKind,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct Stats {
    #[serde(default)]
    pub runs: u64,
    #[serde(default)]
    pub failures: u64,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<u64>,
}

/// One saved skill, `schema_version` 1.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(super) struct Skill {
    pub schema_version: u32,
    pub name: String,
    pub app: AppRef,
    pub goal: String,
    #[serde(default)]
    pub params: Vec<Param>,
    pub steps: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub postconditions: Vec<Value>,
    #[serde(default)]
    pub destructive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    #[serde(default)]
    pub status: AppSkillStatus,
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub saved_at: u64,
    #[serde(default)]
    pub stats: Stats,
}

impl Skill {
    fn summary(&self) -> AppSkillSummary {
        AppSkillSummary {
            name: self.name.clone(),
            goal: self.goal.clone(),
            params: self.params.iter().map(|param| param.name.clone()).collect(),
            status: self.status,
            version: self.version,
            destructive: self.destructive,
            app_version: self.app_version.clone(),
            runs: self.stats.runs,
            last_ok_at: self.stats.last_ok_at,
            last_run_at: self.stats.last_run_at,
        }
    }

    /// What an Agent needs to call it.
    fn listing(&self) -> Value {
        json!({
            "name": self.name,
            "goal": self.goal,
            "params": self.params,
            "status": self.status,
            "version": self.version,
            "destructive": self.destructive,
            "steps": self.steps.len(),
            "app_version": self.app_version,
        })
    }
}

/// What an Agent sends to `save`: a skill without the Engine's bookkeeping.
/// A whole saved skill (from a failure report) is accepted too; its status,
/// version and stats are ignored.
#[derive(Clone, Debug, Deserialize)]
struct Draft {
    name: String,
    #[serde(default)]
    app: Option<AppRef>,
    goal: String,
    #[serde(default)]
    params: Vec<Param>,
    steps: Vec<Value>,
    #[serde(default)]
    preconditions: Vec<Value>,
    #[serde(default)]
    postconditions: Vec<Value>,
    #[serde(default)]
    destructive: bool,
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn invalid(message: impl Into<String>) -> ComputerError {
    ComputerError::new(SKILL_INVALID, message)
}

fn io_error(error: std::io::Error) -> ComputerError {
    if error.kind() == std::io::ErrorKind::InvalidInput {
        invalid(error.to_string())
    } else {
        ComputerError::new(super::DRIVER_ERROR, format!("App Skills storage: {error}"))
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str, ComputerError> {
    arg_str(args, key).ok_or_else(|| invalid(format!("missing {key}")))
}

pub(super) struct AppSkills {
    store: store::Store,
    learners: Mutex<HashMap<String, recorder::Learning>>,
    /// How many sessions are learning: `computer.act` checks this before
    /// touching the map, so recording costs nothing when nobody learns.
    learning: AtomicUsize,
}

impl AppSkills {
    pub fn new(root: PathBuf) -> Self {
        Self {
            store: store::Store::new(root),
            learners: Mutex::new(HashMap::new()),
            learning: AtomicUsize::new(0),
        }
    }

    fn learners(&self) -> std::sync::MutexGuard<'_, HashMap<String, recorder::Learning>> {
        self.learners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set_count(&self, learners: &HashMap<String, recorder::Learning>) {
        self.learning.store(learners.len(), Ordering::Release);
    }

    pub fn is_learning(&self, session_id: &str) -> bool {
        self.learning.load(Ordering::Acquire) > 0 && self.learners().contains_key(session_id)
    }

    /// One `computer` call made by a learning session.
    pub fn observe(
        &self,
        session_id: &str,
        tool: &str,
        args: &Map<String, Value>,
        result: Option<&Value>,
    ) {
        if let Some(learning) = self.learners().get_mut(session_id) {
            learning.record(tool, args, result);
        }
    }

    /// The session ended: its recording goes.
    pub fn end_session(&self, session_id: &str) {
        if self.learning.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut learners = self.learners();
        learners.remove(session_id);
        self.set_count(&learners);
    }

    /// One `app_skills` action. `actor` and `session` are the calling
    /// session's, when there is one.
    pub fn handle(
        &self,
        actor: Option<&dyn Actor>,
        session: Option<&str>,
        action: &str,
        args: &Value,
        is_live: &dyn Fn(&str) -> bool,
    ) -> Result<Value, ComputerError> {
        if self.learning.load(Ordering::Acquire) > 0 {
            let mut learners = self.learners();
            learners.retain(|id, _| is_live(id));
            self.set_count(&learners);
        }
        let need = || match (actor, session) {
            (Some(actor), Some(session)) => Ok((actor, session)),
            _ => Err(ComputerError::new(
                SKILL_NEEDS_SESSION,
                format!("{action} must be called from a Diri session"),
            )),
        };
        match action {
            "list" => self.list_for_agent(arg_str(args, "app")),
            "summary" => {
                serde_json::to_value(self.list()).map_err(|error| invalid(error.to_string()))
            }
            "learn" => {
                let (actor, session) = need()?;
                self.learn(actor, session, args)
            }
            "trace" => {
                let (_, session) = need()?;
                self.trace(session)
            }
            "abandon" => {
                let (_, session) = need()?;
                let mut learners = self.learners();
                let was = learners.remove(session).is_some();
                self.set_count(&learners);
                Ok(json!({"abandoned": was}))
            }
            "save" => {
                let (actor, session) = need()?;
                self.save(actor, session, args)
            }
            "run" => {
                let (actor, _) = need()?;
                self.run(actor, args)
            }
            "forget" => self.forget(args),
            "history" => self.history(args),
            "rollback" => self.rollback(args),
            other => Err(invalid(format!(
                "unknown action {other:?}; use list, learn, trace, save, run, forget, history, rollback or abandon"
            ))),
        }
    }

    /// Every app with skills, for Settings and `summary`.
    pub fn list(&self) -> AppSkillsList {
        let apps = self
            .store
            .apps()
            .into_iter()
            .filter_map(|bundle_id| {
                let skills: Vec<AppSkillSummary> = self
                    .store
                    .skill_names(&bundle_id)
                    .ok()?
                    .iter()
                    .filter_map(|name| self.store.load(&bundle_id, name).ok().flatten())
                    .map(|skill| skill.summary())
                    .collect();
                if skills.is_empty() {
                    return None;
                }
                let name = self
                    .store
                    .app(&bundle_id)
                    .ok()
                    .flatten()
                    .map(|meta| meta.name)
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| bundle_id.clone());
                Some(AppSkillsApp {
                    bundle_id,
                    name,
                    skills,
                })
            })
            .collect();
        AppSkillsList { apps }
    }

    fn list_for_agent(&self, app: Option<&str>) -> Result<Value, ComputerError> {
        let filter = match app {
            Some(app) => Some(self.stored_app(app)?),
            None => None,
        };
        let apps: Vec<Value> = self
            .store
            .apps()
            .into_iter()
            .filter(|bundle_id| filter.as_ref().is_none_or(|only| only == bundle_id))
            .filter_map(|bundle_id| {
                let skills: Vec<Value> = self
                    .store
                    .skill_names(&bundle_id)
                    .ok()?
                    .iter()
                    .filter_map(|name| self.store.load(&bundle_id, name).ok().flatten())
                    .map(|skill| skill.listing())
                    .collect();
                let name = self
                    .store
                    .app(&bundle_id)
                    .ok()
                    .flatten()
                    .map(|meta| meta.name);
                (!skills.is_empty())
                    .then(|| json!({"bundle_id": bundle_id, "name": name, "skills": skills}))
            })
            .collect();
        let hint = if apps.is_empty() {
            "No skills yet. For a task you'll repeat, call learn {app, goal}, do it once with the computer tool, then trace and save."
        } else {
            "run {app, skill, args} replays one (args_list for many items). A broken or unverified skill may need repair: run it, read the failure, fix the steps, save."
        };
        Ok(json!({"apps": apps, "hint": hint}))
    }

    /// An app named by bundle id or by the name it was saved under.
    fn stored_app(&self, app: &str) -> Result<String, ComputerError> {
        let apps = self.store.apps();
        if apps.iter().any(|bundle_id| bundle_id == app) {
            return Ok(app.to_owned());
        }
        apps.into_iter()
            .find(|bundle_id| {
                bundle_id.eq_ignore_ascii_case(app)
                    || self
                        .store
                        .app(bundle_id)
                        .ok()
                        .flatten()
                        .is_some_and(|meta| meta.name.eq_ignore_ascii_case(app))
            })
            .ok_or_else(|| {
                ComputerError::new(SKILL_NOT_FOUND, format!("no skills for app {app:?}"))
            })
    }

    fn stored_skill(&self, args: &Value) -> Result<Skill, ComputerError> {
        let bundle_id = self.stored_app(required(args, "app")?)?;
        let name = required(args, "skill")?;
        self.store
            .load(&bundle_id, name)
            .map_err(io_error)?
            .ok_or_else(|| {
                ComputerError::new(
                    SKILL_NOT_FOUND,
                    format!("{bundle_id} has no skill {name:?}"),
                )
            })
    }

    fn learn(
        &self,
        actor: &dyn Actor,
        session: &str,
        args: &Value,
    ) -> Result<Value, ComputerError> {
        let app = resolve_app(actor, required(args, "app")?)?;
        let goal = required(args, "goal")?.to_owned();
        let mut learners = self.learners();
        learners.insert(
            session.to_owned(),
            recorder::Learning::new(app.clone(), goal.clone()),
        );
        self.set_count(&learners);
        let existing = self.store.skill_names(&app.bundle_id).unwrap_or_default();
        Ok(json!({
            "learning": true,
            "app": app,
            "goal": goal,
            "existing_skills": existing,
            "next": "Do the task once now with the computer tool, as you normally would (get_window_state, then act with element tokens or run_steps). Only successful actions are recorded; reads are not. Use real example values for anything that varies. Then call app_skills trace, edit the draft into a skill, and call app_skills save. Call abandon to stop without saving.",
        }))
    }

    fn trace(&self, session: &str) -> Result<Value, ComputerError> {
        let learners = self.learners();
        let learning = learners
            .get(session)
            .ok_or_else(|| invalid("this session is not learning; call learn {app, goal} first"))?;
        let mut trace = learning.trace();
        trace["elapsed_ms"] = json!(learning.started.elapsed().as_millis() as u64);
        if learning.len() == 0 {
            trace["next"] =
                json!("Nothing recorded yet: do the task with the computer tool first.");
        }
        Ok(trace)
    }

    fn save(&self, actor: &dyn Actor, session: &str, args: &Value) -> Result<Value, ComputerError> {
        let draft: Draft = serde_json::from_value(
            args.get("skill")
                .cloned()
                .ok_or_else(|| invalid("missing skill (the skill object to save)"))?,
        )
        .map_err(|error| invalid(format!("skill: {error}")))?;
        if !store::valid_skill_name(&draft.name) {
            return Err(invalid(format!(
                "skill name {:?} must be snake_case (a-z, 0-9, _), at most 64 characters",
                draft.name
            )));
        }
        let learning_app = self
            .learners()
            .get(session)
            .map(|learning| learning.app.clone());
        let app = match (&draft.app, arg_str(args, "app")) {
            (Some(app), _) if !app.bundle_id.is_empty() => app.clone(),
            (_, Some(app)) => match self.stored_app(app) {
                Ok(bundle_id) => AppRef {
                    name: self
                        .store
                        .app(&bundle_id)
                        .ok()
                        .flatten()
                        .map(|meta| meta.name)
                        .unwrap_or_default(),
                    bundle_id,
                },
                Err(_) => resolve_app(actor, app)?,
            },
            _ => learning_app.ok_or_else(|| {
                invalid("which app? pass app (or skill.app.bundle_id), or call learn first")
            })?,
        };
        if !store::valid_bundle_id(&app.bundle_id) {
            return Err(invalid(format!("{:?} is not a bundle id", app.bundle_id)));
        }
        if draft.goal.trim().is_empty() {
            return Err(invalid("goal must say in one sentence what the skill does"));
        }
        let compiled = compile::compile(
            &draft.params,
            &draft.steps,
            &draft.preconditions,
            &draft.postconditions,
        )
        .map_err(|problems| invalid(format!("the skill was not saved: {}", problems.join("; "))))?;
        let example = compile::check_args(
            &draft.params,
            args.get("example_args").unwrap_or(&Value::Null),
        )
        .map_err(|problem| invalid(format!("example_args: {problem}")))?;
        let reach = match arg_str(args, "verify") {
            None if draft.destructive => Some(runner::Reach::UntilLast),
            None | Some("full") => Some(runner::Reach::Full),
            Some("until_last") => Some(runner::Reach::UntilLast),
            Some("none") => None,
            Some(other) => {
                return Err(invalid(format!(
                    "verify {other:?}: use full, until_last or none"
                )));
            }
        };
        let mut skill = Skill {
            schema_version: SCHEMA_VERSION,
            name: draft.name.clone(),
            app: app.clone(),
            goal: draft.goal.trim().to_owned(),
            params: draft.params,
            steps: compiled.steps,
            preconditions: compiled.preconditions,
            postconditions: compiled.postconditions,
            destructive: draft.destructive,
            app_version: None,
            status: AppSkillStatus::Unverified,
            version: 0,
            saved_at: now_ms(),
            stats: Stats::default(),
        };
        let target = runner::ensure_running(actor, &app.bundle_id)?;
        skill.app_version = target.version.clone();
        let mut verified = false;
        let mut duration = Duration::ZERO;
        if let Some(reach) = reach {
            let (outcome, took) = runner::timed(|| {
                runner::run_once(actor, &skill, &example, reach, target.app(&app.bundle_id))
            });
            duration = took;
            if let Err(failure) = outcome {
                let _ = self.store.append_run(
                    &app.bundle_id,
                    &store::RunRecord {
                        at: now_ms(),
                        skill: skill.name.clone(),
                        version: 0,
                        outcome: "verify_failed".into(),
                        failed_step: (failure.part == "step").then_some(failure.index),
                        duration_ms: took.as_millis() as u64,
                    },
                );
                let mut report = self.failure_report(actor, &skill, &failure);
                report["saved"] = json!(false);
                report["repair"] = json!(
                    "Verification replayed the skill with example_args and it failed here, so nothing was saved. Fix the step (selector, a wait_for/expect, or the order) and call save again."
                );
                return Ok(report);
            }
            verified = true;
        }
        if verified && !compile::has_pixel_steps(&skill.steps) {
            skill.status = AppSkillStatus::Verified;
        }
        let version = self.store.save(&mut skill).map_err(io_error)?;
        let _ = self.store.note_app(&app, target.version.as_deref());
        if reach.is_some() {
            let _ = self.store.append_run(
                &app.bundle_id,
                &store::RunRecord {
                    at: now_ms(),
                    skill: skill.name.clone(),
                    version,
                    outcome: "verify_ok".into(),
                    failed_step: None,
                    duration_ms: duration.as_millis() as u64,
                },
            );
        }
        {
            let mut learners = self.learners();
            if learners
                .get(session)
                .is_some_and(|learning| learning.app.bundle_id == app.bundle_id)
            {
                learners.remove(session);
                self.set_count(&learners);
            }
        }
        let mut reply = json!({
            "saved": true,
            "app": app,
            "skill": skill.name,
            "version": version,
            "status": skill.status,
            "app_version": skill.app_version,
            "verify_ms": duration.as_millis() as u64,
        });
        if reach == Some(runner::Reach::UntilLast) {
            let last = skill
                .steps
                .last()
                .and_then(|step| step["intent"].as_str())
                .unwrap_or("the final step");
            reply["note"] = json!(format!(
                "Verified up to, not including, the final step ({last}): the app is left mid-flow there. Cancel or finish it with the computer tool, or tell the person."
            ));
        }
        if compile::has_pixel_steps(&skill.steps) {
            reply["warning"] = json!(
                "Saved unverified: it has pixel steps, which break when a window moves or resizes."
            );
        }
        Ok(reply)
    }

    fn run(&self, actor: &dyn Actor, args: &Value) -> Result<Value, ComputerError> {
        let skill = self.stored_skill(args)?;
        let items: Vec<Value> = match (args.get("args_list"), args.get("args")) {
            (Some(Value::Array(list)), _) => list.clone(),
            (Some(_), _) => return Err(invalid("args_list must be an array of args objects")),
            (None, Some(one)) => vec![one.clone()],
            (None, None) => vec![Value::Null],
        };
        if items.is_empty() {
            return Err(invalid("args_list is empty"));
        }
        if items.len() > MAX_ITEMS {
            return Err(invalid(format!(
                "{} items; one run takes at most {MAX_ITEMS}, so split it",
                items.len()
            )));
        }
        if skill.destructive
            && items.len() > 1
            && args.get("confirm").and_then(Value::as_bool) != Some(true)
        {
            return Err(ComputerError::new(
                SKILL_CONFIRM,
                format!(
                    "{} is destructive ({}) and would run {} times. Ask the person first; only after they agree, call run again with confirm: true.",
                    skill.name,
                    skill.goal,
                    items.len()
                ),
            ));
        }
        let checked: Vec<Map<String, Value>> = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                compile::check_args(&skill.params, item)
                    .map_err(|problem| invalid(format!("item {i}: {problem}")))
            })
            .collect::<Result<_, _>>()?;
        let continue_on_error =
            args.get("continue_on_error").and_then(Value::as_bool) == Some(true);
        let bundle_id = skill.app.bundle_id.clone();

        let target = runner::ensure_running(actor, &bundle_id)?;
        let drifted = matches!(
            (&skill.app_version, &target.version),
            (Some(learned), Some(now)) if learned != now
        );
        let mut skill = skill;
        if drifted {
            if let Ok(Some(updated)) =
                self.store
                    .update(&bundle_id, &skill.name, skill.version, |skill| {
                        if skill.status == AppSkillStatus::Verified {
                            skill.status = AppSkillStatus::Unverified;
                        }
                    })
            {
                skill = updated;
            }
            let _ = self.store.note_app(&skill.app, target.version.as_deref());
        }

        let started = std::time::Instant::now();
        let mut results = Vec::new();
        let mut first_failure: Option<Value> = None;
        let mut not_run = Vec::new();
        for (i, item) in checked.iter().enumerate() {
            if (first_failure.is_some() && !continue_on_error) || started.elapsed() >= RUN_BUDGET {
                not_run.push(i);
                continue;
            }
            let (outcome, took) = runner::timed(|| {
                runner::run_once(
                    actor,
                    &skill,
                    item,
                    runner::Reach::Full,
                    target.app(&bundle_id),
                )
            });
            let ms = took.as_millis() as u64;
            let pixel = compile::has_pixel_steps(&skill.steps);
            let version = target.version.clone();
            let updated = self
                .store
                .update(&bundle_id, &skill.name, skill.version, |skill| {
                    let now = now_ms();
                    skill.stats.runs += 1;
                    skill.stats.last_run_at = Some(now);
                    match &outcome {
                        Ok(()) => {
                            skill.stats.consecutive_failures = 0;
                            skill.stats.last_ok_at = Some(now);
                            if !pixel {
                                skill.status = AppSkillStatus::Verified;
                            }
                            if version.is_some() {
                                skill.app_version = version;
                            }
                        }
                        Err(_) => {
                            skill.stats.failures += 1;
                            skill.stats.consecutive_failures += 1;
                            if skill.stats.consecutive_failures >= BROKEN_AFTER {
                                skill.status = AppSkillStatus::Broken;
                            }
                        }
                    }
                })
                .ok()
                .flatten();
            let _ = self.store.append_run(
                &bundle_id,
                &store::RunRecord {
                    at: now_ms(),
                    skill: skill.name.clone(),
                    version: skill.version,
                    outcome: if outcome.is_ok() { "ok" } else { "failed" }.into(),
                    failed_step: outcome
                        .as_ref()
                        .err()
                        .filter(|failure| failure.part == "step")
                        .map(|failure| failure.index),
                    duration_ms: ms,
                },
            );
            if let Some(updated) = updated {
                skill.status = updated.status;
                skill.stats = updated.stats;
            }
            match outcome {
                Ok(()) => results.push(json!({"item": i, "ok": true, "duration_ms": ms})),
                Err(failure) => {
                    results.push(json!({
                        "item": i,
                        "ok": false,
                        "duration_ms": ms,
                        "failed": failure.part,
                        "failed_step": failure.index,
                        "driver_error": failure.message,
                    }));
                    if first_failure.is_none() {
                        let mut report = self.failure_report(actor, &skill, &failure);
                        report["item"] = json!(i);
                        first_failure = Some(report);
                    }
                }
            }
        }
        let ok = first_failure.is_none() && not_run.is_empty();
        if checked.len() == 1 && args.get("args_list").is_none() {
            if let Some(mut failure) = first_failure {
                failure["ok"] = json!(false);
                failure["duration_ms"] = results[0]["duration_ms"].clone();
                if drifted {
                    failure["drift"] = json!(drift_note(&skill, &target));
                }
                return Ok(failure);
            }
            let mut reply = json!({
                "ok": true,
                "skill": skill.name,
                "version": skill.version,
                "status": skill.status,
                "duration_ms": results[0]["duration_ms"],
            });
            if drifted {
                reply["drift"] = json!(drift_note(&skill, &target));
            }
            return Ok(reply);
        }
        let mut reply = json!({
            "ok": ok,
            "skill": skill.name,
            "version": skill.version,
            "status": skill.status,
            "items": results,
            "duration_ms": started.elapsed().as_millis() as u64,
        });
        if !not_run.is_empty() {
            reply["not_run"] = json!(not_run);
        }
        if let Some(failure) = first_failure {
            reply["first_failure"] = failure;
        }
        if drifted {
            reply["drift"] = json!(drift_note(&skill, &target));
        }
        Ok(reply)
    }

    /// Everything a repairing Agent needs, and no argument values.
    fn failure_report(&self, actor: &dyn Actor, skill: &Skill, failure: &runner::Failure) -> Value {
        let step = match failure.part {
            "step" => skill.steps.get(failure.index),
            "precondition" => skill.preconditions.get(failure.index),
            _ => skill.postconditions.get(failure.index),
        };
        let intent = step
            .and_then(|step| step["intent"].as_str())
            .map(str::to_owned);
        json!({
            "ok": false,
            "skill": skill.name,
            "version": skill.version,
            "goal": skill.goal,
            "failed": failure.part,
            "failed_step": failure.index,
            "phase": failure.phase,
            "intent": intent,
            "step": step,
            "driver_code": failure.code,
            "driver_error": failure.message,
            "status": skill.status,
            "consecutive_failures": skill.stats.consecutive_failures,
            "window": runner::window_skeleton(actor, &skill.app.bundle_id, failure.observation.as_ref()),
            "saved_skill": {
                "name": skill.name,
                "app": skill.app,
                "goal": skill.goal,
                "params": skill.params,
                "steps": skill.steps,
                "preconditions": skill.preconditions,
                "postconditions": skill.postconditions,
                "destructive": skill.destructive,
            },
            "repair": "Fix the step(s) (look at the window with the computer tool if needed) and call app_skills save with the corrected saved_skill and example_args; that verifies and bumps the version. If it failed because this account lacks permission or the data differs, not because the app changed, don't overwrite the skill.",
        })
    }

    fn forget(&self, args: &Value) -> Result<Value, ComputerError> {
        let bundle_id = self.stored_app(required(args, "app")?)?;
        let name = required(args, "skill")?;
        let existed = self.store.forget(&bundle_id, name).map_err(io_error)?;
        if !existed {
            return Err(ComputerError::new(
                SKILL_NOT_FOUND,
                format!("{bundle_id} has no skill {name:?}"),
            ));
        }
        Ok(json!({"forgotten": name, "app": bundle_id}))
    }

    fn history(&self, args: &Value) -> Result<Value, ComputerError> {
        let skill = self.stored_skill(args)?;
        let bundle_id = &skill.app.bundle_id;
        let mut versions: Vec<Value> = self
            .store
            .archived_versions(bundle_id, &skill.name)
            .map_err(io_error)?
            .into_iter()
            .filter_map(|version| self.store.load_version(bundle_id, &skill.name, version).ok().flatten())
            .map(|old| json!({"version": old.version, "saved_at": old.saved_at, "status": old.status, "steps": old.steps.len(), "app_version": old.app_version}))
            .collect();
        versions.sort_by_key(|entry| entry["version"].as_u64());
        versions.push(json!({"version": skill.version, "saved_at": skill.saved_at, "status": skill.status, "steps": skill.steps.len(), "app_version": skill.app_version, "current": true}));
        let runs = self
            .store
            .runs(bundle_id, &skill.name, HISTORY_RUNS)
            .map_err(io_error)?;
        Ok(json!({
            "skill": skill.name,
            "app": skill.app,
            "stats": skill.stats,
            "versions": versions,
            "runs": runs,
        }))
    }

    /// Makes an earlier version current again, as a new version, unverified
    /// until it runs cleanly.
    fn rollback(&self, args: &Value) -> Result<Value, ComputerError> {
        let current = self.stored_skill(args)?;
        let version = args
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("missing version (see history)"))? as u32;
        let mut old = self
            .store
            .load_version(&current.app.bundle_id, &current.name, version)
            .map_err(io_error)?
            .ok_or_else(|| ComputerError::new(SKILL_NOT_FOUND, format!("no version {version}")))?;
        old.status = AppSkillStatus::Unverified;
        old.stats = Stats::default();
        old.saved_at = now_ms();
        let new = self.store.save(&mut old).map_err(io_error)?;
        Ok(json!({"skill": old.name, "restored": version, "version": new, "status": old.status}))
    }
}

fn drift_note(skill: &Skill, target: &runner::Target) -> String {
    format!(
        "{} changed version ({} → {}); the skill was marked unverified until a clean run",
        if skill.app.name.is_empty() {
            &skill.app.bundle_id
        } else {
            &skill.app.name
        },
        skill.app_version.as_deref().unwrap_or("?"),
        target.version.as_deref().unwrap_or("?"),
    )
}

/// An app by bundle id or name: a running app's own bundle, else an
/// installed one, else a bundle id taken at its word.
fn resolve_app(actor: &dyn Actor, query: &str) -> Result<AppRef, ComputerError> {
    if let Ok(result) = actor.call("list_windows", json!({"app": query}))
        && let Some(pid) = result["mcpToolResult"]["structuredContent"]["windows"]
            .as_array()
            .and_then(|windows| windows.iter().find_map(|window| window["pid"].as_i64()))
        && let Some(info) = actor.bundle_info(pid)
        && let Some(bundle_id) = info.bundle_id
    {
        return Ok(AppRef {
            name: info.name.unwrap_or_else(|| query.to_owned()),
            bundle_id,
        });
    }
    if let Ok(result) = actor.call("list_apps", json!({}))
        && let Some(app) = result["mcpToolResult"]["structuredContent"]["apps"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|app| {
                app["bundle_id"].as_str() == Some(query)
                    || app["name"]
                        .as_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case(query))
            })
        && let Some(bundle_id) = app["bundle_id"].as_str()
    {
        return Ok(AppRef {
            bundle_id: bundle_id.to_owned(),
            name: app["name"].as_str().unwrap_or(query).to_owned(),
        });
    }
    if query.contains('.') && store::valid_bundle_id(query) {
        return Ok(AppRef {
            bundle_id: query.to_owned(),
            name: query.to_owned(),
        });
    }
    Err(ComputerError::new(
        SKILL_NOT_FOUND,
        format!("no app {query:?} is running or installed; pass its bundle id"),
    ))
}

/// One session's `computer.act`, unrecorded, for replays and lookups.
struct DriverActor<'a> {
    computer: &'a super::ComputerUse,
    caller: &'a super::Caller<'a>,
    is_live: &'a dyn Fn(&str) -> bool,
}

impl Actor for DriverActor<'_> {
    fn call(&self, tool: &str, args: Value) -> Result<Value, ComputerError> {
        self.computer
            .act_unrecorded(self.caller, tool, args, self.is_live)
    }
}

impl super::ComputerUse {
    /// One `app_skills` action. `caller` is the calling session; Settings
    /// calls without one (list, summary, history, forget, rollback).
    pub fn skills(
        &self,
        caller: Option<&super::Caller<'_>>,
        action: &str,
        args: &Value,
        is_live: &dyn Fn(&str) -> bool,
    ) -> Result<Value, ComputerError> {
        let actor = caller.map(|caller| DriverActor {
            computer: self,
            caller,
            is_live,
        });
        self.shared.skills.handle(
            actor.as_ref().map(|actor| actor as &dyn Actor),
            caller.map(|caller| caller.session_id),
            action,
            args,
            is_live,
        )
    }

    /// The session ended: drop its learn-mode recording, if any.
    pub fn end_skill_learning(&self, session_id: &str) {
        self.shared.skills.end_session(session_id);
    }
}
