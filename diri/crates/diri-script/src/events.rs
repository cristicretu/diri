//! The events a script can listen to with `diri.on`, and how they are made
//! from the Engine's event stream.
//!
//! The Engine publishes whole records (`session.updated`); scripts want
//! transitions ("this session started working", "that one exited"). The
//! [`Deriver`] keeps the last status kind and title per session it has seen
//! and turns each update into the transitions it contains. It runs on the
//! script thread, never in the Engine, so nothing here costs anything until
//! a script listens.

use std::collections::HashMap;

use diri_proto::EventName;
use serde_json::{Value, json};

/// A session appeared. `{session, kind}`. Every `session.*` event that
/// carries a record also carries `kind`, its Agent's manifest id (`shell`,
/// `claude-code`, `codex`, ...).
pub const SESSION_CREATED: &str = "session.created";
/// A session's status changed. `{session, from, to}`; the kinds are
/// `starting`, `idle`, `working`, `needs_input`, `exited`, `unknown`.
pub const SESSION_STATUS: &str = "session.status";
/// A session's process ended. `{session, code, signal, reason}`.
pub const SESSION_EXITED: &str = "session.exited";
/// A session's title changed. `{session, from, to}`.
pub const SESSION_TITLE: &str = "session.title";
/// A session was closed or released. `{id, reason}`.
pub const SESSION_REMOVED: &str = "session.removed";
/// A program in a session posted a notification (OSC 9/777, an Agent's
/// hook). `{session_id, title, body}`.
pub const SESSION_NOTIFICATION: &str = "session.notification";
/// Workspaces, tabs or splits changed. The new snapshot.
pub const WORKSPACE_UPDATED: &str = "workspace.updated";
/// A schedule was created, changed, ran or was deleted. `{id}`.
pub const SCHEDULE_UPDATED: &str = "schedule.updated";
/// The window's selected session changed (only inside the app).
/// `{session}`, the id.
pub const FOCUS: &str = "focus";

/// Every event `diri.on` accepts, with a one-line description.
pub const ALL: &[(&str, &str)] = &[
    (SESSION_CREATED, "A session appeared: {session, kind}."),
    (
        SESSION_STATUS,
        "A session's status changed: {session, kind, from, to}.",
    ),
    (
        SESSION_EXITED,
        "A session's process ended: {session, kind, code, signal, reason}.",
    ),
    (
        SESSION_TITLE,
        "A session's title changed: {session, kind, from, to}.",
    ),
    (SESSION_REMOVED, "A session was closed: {id, reason}."),
    (
        SESSION_NOTIFICATION,
        "A program posted a notification: {session_id, title, body}.",
    ),
    (
        WORKSPACE_UPDATED,
        "Workspaces, tabs or splits changed: the new snapshot.",
    ),
    (SCHEDULE_UPDATED, "A schedule changed or ran: {id}."),
    (
        FOCUS,
        "The window's selected session changed (in the app only): {session}.",
    ),
];

/// Whether `name` is an event `diri.on` accepts.
pub fn is_known(name: &str) -> bool {
    ALL.iter().any(|(known, _)| *known == name)
}

/// Whether listening to `name` needs the Engine's event stream.
pub fn needs_engine(name: &str) -> bool {
    name != FOCUS
}

/// The Engine event kinds the deriver reads.
pub const ENGINE_KINDS: &[&str] = &[
    EventName::SESSION_UPDATED,
    EventName::SESSION_REMOVED,
    EventName::SESSION_NOTIFICATION,
    EventName::WORKSPACE_UPDATED,
    EventName::SCHEDULE_UPDATED,
];

/// `starting`, `idle`, `working`, `needs_input`, `exited` or `unknown`.
pub fn status_kind(record: &Value) -> &'static str {
    let status = &record["status"];
    let case = status
        .as_str()
        .or_else(|| {
            status
                .as_object()
                .and_then(|object| object.keys().next().map(String::as_str))
        })
        .unwrap_or("unknown");
    match case {
        "starting" => "starting",
        "idle" => "idle",
        "working" => "working",
        "needsInput" => "needs_input",
        "exited" => "exited",
        _ => "unknown",
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Seen {
    status: &'static str,
    title: String,
}

/// Turns Engine events into script events.
#[derive(Default)]
pub struct Deriver {
    seen: HashMap<String, Seen>,
}

impl Deriver {
    /// Records the sessions that already exist, so the first update of each
    /// is compared against its real state instead of announcing it as new.
    pub fn seed(&mut self, records: &[Value]) {
        for record in records {
            if let Some(id) = record["id"].as_str() {
                self.seen.insert(id.to_owned(), seen(record));
            }
        }
    }

    /// The script events one Engine event stands for, in order.
    pub fn derive(&mut self, name: &str, params: Value) -> Vec<(&'static str, Value)> {
        match name {
            EventName::SESSION_UPDATED => self.updated(params),
            EventName::SESSION_REMOVED => {
                if let Some(id) = params["id"].as_str() {
                    self.seen.remove(id);
                }
                vec![(SESSION_REMOVED, params)]
            }
            EventName::SESSION_NOTIFICATION => vec![(
                SESSION_NOTIFICATION,
                json!({
                    "session_id": params["sessionId"].clone(),
                    "title": params["title"].clone(),
                    "body": params["body"].clone(),
                }),
            )],
            EventName::WORKSPACE_UPDATED => vec![(WORKSPACE_UPDATED, params)],
            EventName::SCHEDULE_UPDATED => vec![(SCHEDULE_UPDATED, params)],
            _ => Vec::new(),
        }
    }

    fn updated(&mut self, record: Value) -> Vec<(&'static str, Value)> {
        let Some(id) = record["id"].as_str().map(str::to_owned) else {
            return Vec::new();
        };
        let kind = kind_id(&record);
        let now = seen(&record);
        let before = self.seen.insert(id, now.clone());
        let mut out = Vec::new();
        let Some(before) = before else {
            out.push((SESSION_CREATED, json!({"session": record, "kind": kind})));
            return out;
        };
        if before.title != now.title {
            out.push((
                SESSION_TITLE,
                json!({"session": record, "kind": kind, "from": before.title, "to": now.title}),
            ));
        }
        if before.status != now.status {
            out.push((
                SESSION_STATUS,
                json!({"session": record, "kind": kind, "from": before.status, "to": now.status}),
            ));
            if now.status == "exited" {
                let info = &record["status"]["exited"]["_0"];
                out.push((
                    SESSION_EXITED,
                    json!({
                        "session": record,
                        "kind": kind,
                        "code": info["code"].clone(),
                        "signal": info["signal"].clone(),
                        "reason": info["reason"].clone(),
                    }),
                ));
            }
        }
        out
    }
}

/// The record's Agent as its manifest id: `shell`, `claude-code`, ...
pub fn kind_id(record: &Value) -> String {
    serde_json::from_value::<diri_proto::AgentKind>(record["kind"].clone())
        .map(|kind| kind.id().to_string())
        .unwrap_or_default()
}

fn seen(record: &Value) -> Seen {
    Seen {
        status: status_kind(record),
        title: record["title"].as_str().unwrap_or_default().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, status: Value, title: &str) -> Value {
        json!({"id": id, "status": status, "title": title})
    }

    #[test]
    fn an_update_becomes_the_transitions_it_contains() {
        let mut deriver = Deriver::default();
        deriver.seed(&[record("a", json!({"idle": {}}), "shell")]);
        assert!(
            deriver
                .derive(
                    EventName::SESSION_UPDATED,
                    record("a", json!({"idle": {}}), "shell")
                )
                .is_empty(),
            "an unchanged record is not an event"
        );

        let names = |events: Vec<(&'static str, Value)>| {
            events.into_iter().map(|(name, _)| name).collect::<Vec<_>>()
        };
        assert_eq!(
            names(deriver.derive(
                EventName::SESSION_UPDATED,
                record("a", json!({"working": {}}), "build")
            )),
            vec![SESSION_TITLE, SESSION_STATUS]
        );
        let exited = deriver.derive(
            EventName::SESSION_UPDATED,
            record(
                "a",
                json!({"exited": {"_0": {"reason": "exited", "code": 7}}}),
                "build",
            ),
        );
        assert_eq!(names(exited.clone()), vec![SESSION_STATUS, SESSION_EXITED]);
        assert_eq!(exited[0].1["from"], "working");
        assert_eq!(exited[0].1["to"], "exited");
        assert_eq!(exited[1].1["code"], 7);

        assert_eq!(
            names(deriver.derive(
                EventName::SESSION_UPDATED,
                record("b", json!({"starting": {}}), "")
            )),
            vec![SESSION_CREATED]
        );
        assert_eq!(
            names(deriver.derive(EventName::SESSION_REMOVED, json!({"id": "b"}))),
            vec![SESSION_REMOVED]
        );
        assert_eq!(
            names(deriver.derive(
                EventName::SESSION_UPDATED,
                record("b", json!({"idle": {}}), "")
            )),
            vec![SESSION_CREATED],
            "a removed id is forgotten"
        );
    }

    #[test]
    fn events_name_the_agent_by_its_manifest_id() {
        let mut deriver = Deriver::default();
        let created = deriver.derive(
            EventName::SESSION_UPDATED,
            json!({"id": "a", "kind": {"claudeCode": {}}, "status": {"idle": {}}}),
        );
        assert_eq!(created[0].1["kind"], "claude-code");
        assert_eq!(kind_id(&json!({"kind": {"shell": {}}})), "shell");
        assert_eq!(
            kind_id(&json!({"kind": {"agent": {"id": "opencode"}}})),
            "opencode"
        );
    }

    #[test]
    fn needs_input_reads_as_one_word() {
        assert_eq!(
            status_kind(&json!({"status": {"needsInput": {"_0": "permission"}}})),
            "needs_input"
        );
        assert_eq!(status_kind(&json!({})), "unknown");
    }
}
