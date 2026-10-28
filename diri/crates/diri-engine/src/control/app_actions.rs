//! App actions and the Engine's self-description.
//!
//! The connected app publishes the actions its windows run from menus, keys
//! and the command palette (`app.actions.publish`). That connection becomes
//! the performer: `app.perform` checks a request against the published
//! catalog, writes one `app.perform_requested` frame to that connection only
//! and waits for the app's `app.reply`. Nothing goes through the replayable
//! event log, so a reconnecting app can never run an old request again.
//!
//! Cost: one catalog (a few tens of KB) while an app is connected, and one
//! map entry plus a parked request thread per in-flight perform. Nothing per
//! session and nothing on the PTY path.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use diri_platform::ipc::UnixStream;
use diri_proto::api::{
    ApiDescribeParams, ApiDescribeResult, ApiEntry, ApiListResult, ApiMethod, ApiTextResult,
};
use diri_proto::app_actions::{
    self as app, AppActionsListResult, AppActionsPublishParams, AppPerformParams,
    AppPerformRequest, AppPerformResult, AppReplyParams,
};
use diri_proto::{ControlError, ControlMessage, EventName, JsonValue};
use serde_json::json;

type Writer = Arc<Mutex<UnixStream>>;
type Reply = Result<JsonValue, ControlError>;

struct Performer {
    writer: Writer,
    build: String,
    actions: Vec<app::AppActionSpec>,
}

/// The performer, the requests waiting for its replies, and the cached method
/// catalog. One per Engine.
#[derive(Default)]
pub(crate) struct AppActionHub {
    performer: Mutex<Option<Performer>>,
    pending: Mutex<HashMap<u64, SyncSender<Reply>>>,
    next_request: AtomicU64,
    methods: OnceLock<Vec<ApiMethod>>,
}

/// Forgets the performer when its connection closes. Held by the connection
/// loop for its whole life.
pub(crate) struct PerformerGuard {
    hub: Arc<AppActionHub>,
    writer: Writer,
}

impl Drop for PerformerGuard {
    fn drop(&mut self) {
        self.hub.disconnected(&self.writer);
    }
}

impl AppActionHub {
    pub(crate) fn guard(self: &Arc<Self>, writer: &Writer) -> PerformerGuard {
        PerformerGuard {
            hub: Arc::clone(self),
            writer: Arc::clone(writer),
        }
    }

    /// `app.actions.publish`: replaces the catalog and makes `writer` the
    /// performer. A second app replaces the first; the newest one acts.
    pub(crate) fn publish(
        &self,
        params: Option<JsonValue>,
        writer: &Writer,
    ) -> Result<JsonValue, ControlError> {
        let p: AppActionsPublishParams = super::decode(params)?;
        if p.actions.len() > app::MAX_ACTIONS {
            return Err(ControlError::bad_request(format!(
                "an app may publish at most {} actions",
                app::MAX_ACTIONS
            )));
        }
        let count = p.actions.len();
        *self.performer.lock().map_err(super::poisoned)? = Some(Performer {
            writer: Arc::clone(writer),
            build: p.build,
            actions: p.actions,
        });
        Ok(json!({ "registered": count }))
    }

    fn disconnected(&self, writer: &Writer) {
        let Ok(mut performer) = self.performer.lock() else {
            return;
        };
        if performer
            .as_ref()
            .is_some_and(|performer| Arc::ptr_eq(&performer.writer, writer))
        {
            *performer = None;
            drop(performer);
            // Nobody will answer what is still waiting: say so now rather
            // than at each request's timeout.
            if let Ok(mut pending) = self.pending.lock() {
                for (_, reply) in pending.drain() {
                    let _ = reply.try_send(Err(not_connected()));
                }
            }
        }
    }

    pub(crate) fn list(&self) -> AppActionsListResult {
        let performer = self.performer.lock().unwrap_or_else(|p| p.into_inner());
        match performer.as_ref() {
            Some(performer) => AppActionsListResult {
                connected: true,
                build: Some(performer.build.clone()).filter(|build| !build.is_empty()),
                actions: performer.actions.clone(),
            },
            None => AppActionsListResult::default(),
        }
    }

    /// `app.perform`. Blocks its (background) request thread until the app
    /// answers, the connection closes, or the timeout passes.
    pub(crate) fn perform(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: AppPerformParams = super::decode(params)?;
        if !p.args.is_object() {
            return Err(ControlError::new(
                app::INVALID_ARGS,
                "args must be an object",
            ));
        }
        let timeout = Duration::from_millis(
            p.timeout_ms
                .unwrap_or(app::DEFAULT_TIMEOUT_MS)
                .clamp(1, app::MAX_TIMEOUT_MS),
        );
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed) + 1;
        let (reply, answer) = mpsc::sync_channel(1);
        let writer = {
            let performer = self.performer.lock().map_err(super::poisoned)?;
            let performer = performer.as_ref().ok_or_else(not_connected)?;
            let spec = performer
                .actions
                .iter()
                .find(|spec| spec.name == p.action)
                .ok_or_else(|| {
                    ControlError::new(
                        app::UNKNOWN_ACTION,
                        format!(
                            "the app has no action {:?}; app.actions.list shows them",
                            p.action
                        ),
                    )
                })?;
            spec.validate(&p.args)
                .map_err(|error| ControlError::new(app::INVALID_ARGS, error))?;
            Arc::clone(&performer.writer)
        };
        self.pending
            .lock()
            .map_err(super::poisoned)?
            .insert(request_id, reply);
        let request = AppPerformRequest {
            request_id,
            action: p.action.clone(),
            args: p.args,
            actor: p.actor,
        };
        let sent = serde_json::to_value(&request)
            .map_err(|error| ControlError::internal(error.to_string()))
            .and_then(|params| {
                super::write_message(
                    &writer,
                    &ControlMessage::Event {
                        name: EventName::APP_PERFORM_REQUESTED.into(),
                        // Not from the event log: sequence zero, which a
                        // client's resume cursor ignores.
                        seq: 0,
                        params,
                    },
                )
                .map_err(|_| not_connected())
            });
        let outcome = sent.and_then(|()| match answer.recv_timeout(timeout) {
            Ok(reply) => reply,
            Err(_) => Err(ControlError::new(
                app::APP_TIMEOUT,
                format!(
                    "the app did not answer {:?} within {} ms; it may still have run",
                    p.action,
                    timeout.as_millis()
                ),
            )),
        });
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&request_id);
        }
        let result = outcome?;
        super::encode(&AppPerformResult {
            action: p.action,
            result,
        })
    }

    /// `app.reply`. Only the performer's own connection may answer.
    pub(crate) fn reply(
        &self,
        params: Option<JsonValue>,
        writer: &Writer,
    ) -> Result<JsonValue, ControlError> {
        let p: AppReplyParams = super::decode(params)?;
        let is_performer = self
            .performer
            .lock()
            .map_err(super::poisoned)?
            .as_ref()
            .is_some_and(|performer| Arc::ptr_eq(&performer.writer, writer));
        if !is_performer {
            return Err(ControlError::new(
                "forbidden",
                "only the app that published the actions may answer them",
            ));
        }
        let waiting = self
            .pending
            .lock()
            .map_err(super::poisoned)?
            .remove(&p.request_id);
        // A late reply after a timeout has nobody to tell; that is fine.
        if let Some(waiting) = waiting {
            let reply = match p.error {
                Some(error) => Err(error),
                None => Ok(p.result.unwrap_or(JsonValue::Null)),
            };
            let _ = waiting.try_send(reply);
        }
        Ok(json!({}))
    }

    fn methods(&self) -> &[ApiMethod] {
        self.methods.get_or_init(diri_proto::api::methods)
    }

    pub(crate) fn api_list(&self) -> ApiListResult {
        let actions = self.list();
        ApiListResult {
            methods: self
                .methods()
                .iter()
                .map(|method| ApiEntry {
                    name: method.name.clone(),
                    summary: method.summary.clone(),
                })
                .collect(),
            actions: actions
                .actions
                .iter()
                .map(|action| ApiEntry {
                    name: action.name.clone(),
                    summary: if action.description.is_empty() {
                        action.title.clone()
                    } else {
                        action.description.clone()
                    },
                })
                .collect(),
            app_connected: actions.connected,
        }
    }

    pub(crate) fn api_describe(
        &self,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let p: ApiDescribeParams = super::decode(params)?;
        let method = self
            .methods()
            .iter()
            .find(|method| method.name == p.name)
            .cloned();
        let action = method
            .is_none()
            .then(|| {
                self.list()
                    .actions
                    .into_iter()
                    .find(|action| action.name == p.name)
            })
            .flatten();
        if method.is_none() && action.is_none() {
            return Err(ControlError::not_found(format!(
                "no method or app action named {:?}; api.list shows them",
                p.name
            )));
        }
        super::encode(&ApiDescribeResult { method, action })
    }

    pub(crate) fn api_llms(&self) -> ApiTextResult {
        let actions = self.list();
        ApiTextResult {
            text: diri_proto::api::llms_text(self.methods(), &actions.actions, actions.connected),
        }
    }
}

fn not_connected() -> ControlError {
    ControlError::new(
        app::APP_NOT_CONNECTED,
        "no Diri app is connected to this Engine, so app actions cannot run; open Diri and retry",
    )
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::time::Duration;

    use diri_platform::ipc::UnixStream;
    use diri_proto::{EventName, Method};
    use serde_json::{Value, json};

    /// One control connection to a test Engine, served on its own thread.
    struct Peer {
        writer: UnixStream,
        reader: BufReader<UnixStream>,
        next: u64,
    }

    impl Peer {
        fn connect(server: &std::sync::Arc<crate::control::ControlServer>) -> Self {
            let (client, engine_end) = UnixStream::pair().expect("pair");
            let server = std::sync::Arc::clone(server);
            std::thread::spawn(move || server.serve(engine_end));
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            Self {
                reader: BufReader::new(client.try_clone().expect("clone")),
                writer: client,
                next: 0,
            }
        }

        fn send(&mut self, method: &str, params: Value) -> u64 {
            self.next += 1;
            let mut line =
                serde_json::to_vec(&json!({"id": self.next, "method": method, "params": params}))
                    .unwrap();
            line.push(b'\n');
            self.writer.write_all(&line).unwrap();
            self.next
        }

        fn line(&mut self) -> Value {
            let mut line = String::new();
            self.reader.read_line(&mut line).expect("line");
            serde_json::from_str(&line).expect("json")
        }

        /// Sends a request and returns its response, skipping event frames.
        fn call(&mut self, method: &str, params: Value) -> Value {
            let id = self.send(method, params);
            loop {
                let message = self.line();
                if message["id"] == id {
                    return message;
                }
            }
        }
    }

    fn catalog() -> Value {
        json!({
            "build": "test-app",
            "actions": [
                {"name": "toggle-sidebar", "title": "Toggle Sidebar", "args": {"type":"object","properties":{},"additionalProperties":false}},
                {"name": "select-tab", "title": "Select Tab", "args": {"type":"object","properties":{"index":{"type":"integer","minimum":-1}},"required":["index"],"additionalProperties":false}}
            ]
        })
    }

    #[test]
    fn an_action_reaches_the_publishing_app_and_its_answer_reaches_the_caller() {
        let temp = tempfile::tempdir().unwrap();
        let server = crate::control::tests::server(temp.path());
        let notices = server.events().subscribe(
            None,
            crate::events::Filter::new(None, Some(vec![EventName::AGENT_ACTION.into()])),
        );
        let mut caller = Peer::connect(&server);

        // No app yet: a structured error at once, not a timeout.
        let early = caller.call(Method::APP_PERFORM, json!({"action": "toggle-sidebar"}));
        assert_eq!(early["err"]["code"], "app_not_connected");
        let listed = caller.call(Method::APP_ACTIONS_LIST, json!({}));
        assert_eq!(listed["ok"]["connected"], false);

        let mut app = Peer::connect(&server);
        let published = app.call(Method::APP_ACTIONS_PUBLISH, catalog());
        assert_eq!(published["ok"]["registered"], 2);
        let listed = caller.call(Method::APP_ACTIONS_LIST, json!({}));
        assert_eq!(listed["ok"]["connected"], true);
        assert_eq!(listed["ok"]["build"], "test-app");
        assert_eq!(listed["ok"]["actions"][1]["name"], "select-tab");

        // Checked against the published schema before the app hears of it.
        let unknown = caller.call(Method::APP_PERFORM, json!({"action": "rm-rf"}));
        assert_eq!(unknown["err"]["code"], "unknown_action");
        let invalid = caller.call(
            Method::APP_PERFORM,
            json!({"action": "select-tab", "args": {"index": "two"}}),
        );
        assert_eq!(invalid["err"]["code"], "invalid_args");

        let id = caller.send(
            Method::APP_PERFORM,
            json!({"action": "select-tab", "args": {"index": 2}, "actor": "agent-1"}),
        );
        let request = app.line();
        assert_eq!(request["event"], EventName::APP_PERFORM_REQUESTED);
        assert_eq!(request["seq"], 0, "never from the replayable log");
        assert_eq!(request["params"]["action"], "select-tab");
        assert_eq!(request["params"]["args"], json!({"index": 2}));
        assert_eq!(request["params"]["actor"], "agent-1");
        let request_id = request["params"]["requestId"].clone();

        // Only the performer may answer.
        let forged = caller.call(
            Method::APP_REPLY,
            json!({"requestId": request_id, "result": {"forged": true}}),
        );
        assert_eq!(forged["err"]["code"], "forbidden");
        let replied = app.call(
            Method::APP_REPLY,
            json!({"requestId": request_id, "result": {"selected": 2}}),
        );
        assert!(replied.get("err").is_none(), "{replied}");
        let answer = loop {
            let message = caller.line();
            if message["id"] == id {
                break message;
            }
        };
        assert_eq!(
            answer["ok"],
            json!({"action": "select-tab", "result": {"selected": 2}})
        );
        // The Agent's action is announced, once it succeeded, with its name.
        let notice = notices.recv(Duration::from_secs(1)).expect("announced");
        assert_eq!(
            notice.params(),
            json!({"sessionID": "agent-1", "action": "app_action", "command": "select-tab"})
        );
        assert!(notices.recv(Duration::ZERO).is_none());

        // The app's error passes through unchanged, and is not announced.
        let id = caller.send(
            Method::APP_PERFORM,
            json!({"action": "toggle-sidebar", "actor": "agent-1"}),
        );
        let request = app.line();
        app.call(
            Method::APP_REPLY,
            json!({"requestId": request["params"]["requestId"], "error": {"code": "action_unavailable", "message": "no window"}}),
        );
        let answer = loop {
            let message = caller.line();
            if message["id"] == id {
                break message;
            }
        };
        assert_eq!(answer["err"]["code"], "action_unavailable");
        assert!(notices.recv(Duration::ZERO).is_none());
    }

    #[test]
    fn a_silent_app_times_out_and_a_closed_one_fails_waiting_requests_at_once() {
        let temp = tempfile::tempdir().unwrap();
        let server = crate::control::tests::server(temp.path());
        let mut caller = Peer::connect(&server);
        let mut app = Peer::connect(&server);
        app.call(Method::APP_ACTIONS_PUBLISH, catalog());

        let started = std::time::Instant::now();
        let silent = caller.call(
            Method::APP_PERFORM,
            json!({"action": "toggle-sidebar", "timeoutMs": 50}),
        );
        assert_eq!(silent["err"]["code"], "app_timeout");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(app.line()["event"], EventName::APP_PERFORM_REQUESTED);

        let id = caller.send(
            Method::APP_PERFORM,
            json!({"action": "toggle-sidebar", "timeoutMs": 20_000}),
        );
        assert_eq!(app.line()["event"], EventName::APP_PERFORM_REQUESTED);
        let started = std::time::Instant::now();
        drop(app);
        let answer = loop {
            let message = caller.line();
            if message["id"] == id {
                break message;
            }
        };
        assert_eq!(answer["err"]["code"], "app_not_connected");
        assert!(started.elapsed() < Duration::from_secs(5));
        let listed = caller.call(Method::APP_ACTIONS_LIST, json!({}));
        assert_eq!(listed["ok"]["connected"], false);
    }

    #[test]
    fn the_api_describes_every_method_and_the_apps_actions() {
        let temp = tempfile::tempdir().unwrap();
        let server = crate::control::tests::server(temp.path());
        let mut caller = Peer::connect(&server);
        let listed = caller.call(Method::API_LIST, json!({}));
        let methods = listed["ok"]["methods"].as_array().unwrap();
        assert_eq!(methods.len(), Method::ALL.len());
        assert_eq!(listed["ok"]["appConnected"], false);

        let described = caller.call(Method::API_DESCRIBE, json!({"name": "session.send_text"}));
        let method = &described["ok"]["method"];
        assert_eq!(method["params"]["type"], "object");
        assert!(
            method["params"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("sessionID"))
        );
        assert_eq!(method["example"]["text"], "npm test");
        let missing = caller.call(Method::API_DESCRIBE, json!({"name": "nope"}));
        assert_eq!(missing["err"]["code"], "not_found");

        let mut app = Peer::connect(&server);
        app.call(Method::APP_ACTIONS_PUBLISH, catalog());
        let action = caller.call(Method::API_DESCRIBE, json!({"name": "select-tab"}));
        assert_eq!(action["ok"]["action"]["args"]["required"], json!(["index"]));
        let text = caller.call(Method::API_LLMS, json!({}));
        let text = text["ok"]["text"].as_str().unwrap();
        assert!(
            text.contains("- session.send_text(sessionID, submit, text) -> {}"),
            "{text}"
        );
        assert!(text.contains("- select-tab(index)"), "{text}");
        // Every method is in the text an Agent reads.
        for name in Method::ALL {
            assert!(text.contains(&format!("- {name}(")), "{name}");
        }
    }

    /// Caller → Engine → app → Engine → caller over Unix sockets, the app
    /// answering at once. `cargo test --release -p diri-engine --lib
    /// perform_round_trip -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn perform_round_trip_latency() {
        let temp = tempfile::tempdir().unwrap();
        let server = crate::control::tests::server(temp.path());
        let mut caller = Peer::connect(&server);
        let mut app = Peer::connect(&server);
        app.call(Method::APP_ACTIONS_PUBLISH, catalog());
        let answering = std::thread::spawn(move || {
            let mut answered = 0;
            while answered < 2_000 {
                let request = app.line();
                if request.get("event").is_none() {
                    continue; // the Engine's ack of an earlier reply
                }
                answered += 1;
                let id = request["params"]["requestId"].clone();
                app.send(Method::APP_REPLY, json!({"requestId": id, "result": null}));
            }
            app
        });
        let mut samples = Vec::with_capacity(2_000);
        for _ in 0..2_000 {
            let started = std::time::Instant::now();
            let answer = caller.call(Method::APP_PERFORM, json!({"action": "toggle-sidebar"}));
            samples.push(started.elapsed());
            assert!(answer.get("ok").is_some(), "{answer}");
        }
        let _app = answering.join().unwrap();
        let text = caller.call(Method::API_LLMS, json!({}));
        let describe_all: usize = Method::ALL
            .iter()
            .map(|name| {
                caller
                    .call(Method::API_DESCRIBE, json!({"name": name}))
                    .to_string()
                    .len()
            })
            .sum();
        println!(
            "api.llms text: {} bytes; api.describe of every method: {describe_all} bytes",
            text["ok"]["text"].as_str().unwrap().len()
        );
        samples.sort();
        let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
        println!(
            "app.perform round trip over {} calls: p50 {:?} p90 {:?} p99 {:?}",
            samples.len(),
            at(0.5),
            at(0.9),
            at(0.99)
        );
    }
}
