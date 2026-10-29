//! Push notifications to paired iPhones: an agent needs you, failed, or
//! finished.
//!
//! Trust model (see `docs/remote-access.md`, "Push"):
//!
//! - **The payload carries no session content.** Apple's push service sees
//!   only an opaque session id, the kind of event (`needs`, `failed`,
//!   `done`), when the prompt was raised, and the gateway address the phone
//!   registered from. The question, its options and the session's title are
//!   fetched by the phone's notification service extension straight from
//!   this gateway, inside the tailnet, where tailnet admission identifies the
//!   phone by its WireGuard key. If the phone cannot reach the gateway the
//!   banner stays generic.
//! - **The sender is this gateway.** It signs an APNs provider token (ES256,
//!   the owner's `.p8` auth key from their Apple developer account) and posts
//!   to Apple over HTTP/2. No relay, no Diri server, no third party: the key
//!   never leaves the Mac, and turning phone access off stops all pushes.
//!   The signing and HTTP/2 run through the system's `openssl` and `curl`
//!   rather than new crates (a TLS + HTTP/2 + ECDSA stack would be the
//!   gateway's largest dependency by far, for one request per alert).
//! - **Answers never travel in a push.** Approve/Deny on a notification is
//!   the phone calling `/answer`, which re-checks the prompt (`occurredAt`)
//!   and writes once; a destructive Approve requires the phone unlocked.
//!
//! What is pushed follows the Mac's per-state alert switches (Settings ›
//! General › Alerts › Notify), which the app hands over with
//! [`set_push_routes`].

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use diri_proto::{EventName, SessionRecord, SessionStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;

// ---------------------------------------------------------------------------
// Routes: the Mac's per-state "Notify" switches
// ---------------------------------------------------------------------------

const ROUTE_NEEDS: u8 = 1;
const ROUTE_FAILED: u8 = 2;
const ROUTE_DONE: u8 = 4;

static ROUTES: AtomicU8 = AtomicU8::new(ROUTE_NEEDS | ROUTE_FAILED | ROUTE_DONE);

/// Which events reach the phone: the Mac app passes its Alerts settings'
/// Notify column (Needs you / Failed / Finished). All on by default, as on
/// the Mac.
pub fn set_push_routes(needs_you: bool, failed: bool, finished: bool) {
    let bits = [
        (needs_you, ROUTE_NEEDS),
        (failed, ROUTE_FAILED),
        (finished, ROUTE_DONE),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .fold(0, |bits, (_, bit)| bits | bit);
    ROUTES.store(bits, Ordering::Relaxed);
}

fn routed(kind: Kind) -> bool {
    let bit = match kind {
        Kind::Needs => ROUTE_NEEDS,
        Kind::Failed => ROUTE_FAILED,
        Kind::Done => ROUTE_DONE,
    };
    ROUTES.load(Ordering::Relaxed) & bit != 0
}

// ---------------------------------------------------------------------------
// Configuration and registrations
// ---------------------------------------------------------------------------

/// The owner's APNs auth key: `push.json` beside the paired-device list.
/// Absent means push is off; the phone is told so when it registers.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PushConfig {
    /// The `.p8` file Apple issued (an ES256 private key, PKCS#8 PEM).
    pub key_path: PathBuf,
    pub key_id: String,
    pub team_id: String,
    /// The app's bundle id.
    #[serde(default = "default_topic")]
    pub topic: String,
}

fn default_topic() -> String {
    "dev.dirijor.ios".into()
}

impl PushConfig {
    /// `Ok(None)` when no config file exists.
    pub fn load(path: &Path) -> io::Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// One phone that asked for pushes.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    /// The APNs device token, hex.
    pub token: String,
    /// `sandbox` (development builds) or `production`.
    pub environment: String,
    /// The gateway address this phone uses, echoed in every payload so its
    /// extension knows where to fetch the details from.
    pub base: String,
    /// The tailnet node id, when admission knows it: revoking the device
    /// also stops its pushes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// Registered phones, owner-only on disk, replaced atomically.
pub struct Registrations {
    path: PathBuf,
    list: Mutex<Vec<Registration>>,
}

impl Registrations {
    #[must_use]
    pub fn open(path: PathBuf) -> Self {
        let list = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            list: Mutex::new(list),
        }
    }

    pub fn list(&self) -> Vec<Registration> {
        self.list.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Adds or refreshes `registration` (keyed by token; a phone that
    /// re-registers from another address replaces its entry).
    pub fn register(&self, registration: Registration) -> io::Result<()> {
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        list.retain(|known| known.token != registration.token);
        list.push(registration);
        save(&self.path, &list)
    }

    pub fn unregister(&self, token: &str) -> io::Result<()> {
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        list.retain(|known| known.token != token);
        save(&self.path, &list)
    }
}

fn save(path: &Path, list: &[Registration]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temporary)?;
        std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(list)?)?;
    }
    std::fs::rename(&temporary, path)
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Needs,
    Failed,
    Done,
}

impl Kind {
    fn wire(self) -> &'static str {
        match self {
            Self::Needs => "needs",
            Self::Failed => "failed",
            Self::Done => "done",
        }
    }
}

/// One thing worth a push.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub session: String,
    pub kind: Kind,
    /// When the prompt was raised (`needs`), so an answer can be checked
    /// against the prompt that was shown.
    pub occurred_at: Option<f64>,
}

/// Turns the Engine's `session.updated` stream into notices: a new prompt
/// (by `occurredAt`), a failed exit, a finished turn. A session first seen
/// in an update only sets its baseline, so a reconnect replays nothing.
#[derive(Default)]
pub struct Watcher {
    last: HashMap<String, Seen>,
    turns: crate::turns::TurnWatcher,
}

#[derive(Clone, Copy, Default, PartialEq)]
struct Seen {
    prompt: Option<f64>,
    exited: bool,
}

fn seen(record: &SessionRecord) -> Seen {
    let waiting = matches!(record.status, SessionStatus::NeedsInput(_));
    Seen {
        prompt: record
            .needs_input
            .as_ref()
            .filter(|_| waiting)
            .map(|detail| detail.occurred_at.0),
        exited: matches!(record.status, SessionStatus::Exited(_)),
    }
}

fn failed(record: &SessionRecord) -> bool {
    match &record.status {
        SessionStatus::Exited(info) => {
            let value = serde_json::to_value(info).unwrap_or(Value::Null);
            let code = value.get("code").and_then(Value::as_i64).unwrap_or(0);
            let reason = value.get("reason").and_then(Value::as_str).unwrap_or("");
            code != 0 || reason == "signaled"
        }
        _ => false,
    }
}

impl Watcher {
    pub fn seed(&mut self, records: Vec<SessionRecord>) {
        self.last = records
            .iter()
            .map(|record| (record.id.0.clone(), seen(record)))
            .collect();
        self.turns.seed_records(records);
    }

    pub fn observe(&mut self, name: &str, params: &Value) -> Vec<Notice> {
        let mut notices = Vec::new();
        if let Some((session, _)) = self.turns.observe(name, params) {
            notices.push(Notice {
                session,
                kind: Kind::Done,
                occurred_at: None,
            });
        }
        match name {
            EventName::SESSION_UPDATED => {
                let Ok(record) = SessionRecord::deserialize(params) else {
                    return notices;
                };
                let now = seen(&record);
                let before = self.last.insert(record.id.0.clone(), now);
                let Some(before) = before else {
                    return notices;
                };
                if let Some(prompt) = now.prompt
                    && before.prompt != Some(prompt)
                {
                    notices.push(Notice {
                        session: record.id.0.clone(),
                        kind: Kind::Needs,
                        occurred_at: Some(prompt),
                    });
                }
                if now.exited && !before.exited && failed(&record) {
                    // A failure is the news; a "finished" for the same exit
                    // would be noise.
                    notices.retain(|notice| notice.kind != Kind::Done);
                    notices.push(Notice {
                        session: record.id.0,
                        kind: Kind::Failed,
                        occurred_at: None,
                    });
                }
            }
            EventName::SESSION_REMOVED => {
                if let Some(id) = params.get("id").and_then(Value::as_str) {
                    self.last.remove(id);
                }
            }
            _ => {}
        }
        notices
    }
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

/// The APNs payload: a generic banner Apple can read, the ids the phone
/// needs to fetch the rest. Nothing from the session (title, question,
/// options, path) is in it.
#[must_use]
pub fn payload(notice: &Notice, registration: &Registration) -> Value {
    let body = match notice.kind {
        Kind::Needs => "An agent is waiting for you",
        Kind::Failed => "An agent stopped with an error",
        Kind::Done => "An agent finished",
    };
    let mut diri = json!({
        "s": notice.session,
        "k": notice.kind.wire(),
        "b": registration.base,
    });
    if let Some(occurred) = notice.occurred_at {
        diri["o"] = json!(occurred);
    }
    json!({
        "aps": {
            "alert": {"title": "Diri", "body": body},
            "sound": "default",
            // The extension replaces the generic banner with the details.
            "mutable-content": 1,
            "category": match notice.kind {
                Kind::Needs => "diri.needs",
                Kind::Failed | Kind::Done => "diri.news",
            },
            // One thread per session; a newer prompt replaces the older.
            "thread-id": notice.session,
        },
        "diri": diri,
    })
}

// ---------------------------------------------------------------------------
// Sending
// ---------------------------------------------------------------------------

/// Where notices go: Apple, or (fixtures) a directory of payload files a
/// test hands to `xcrun simctl push`.
pub enum Sender {
    Apns(Apns),
    Directory(PathBuf),
}

impl Sender {
    /// Sends to every registered phone `allowed` still admits (a device
    /// removed on the Mac is dropped from the list instead).
    pub async fn send(
        &self,
        notice: &Notice,
        registrations: &Registrations,
        allowed: &(dyn Fn(&Registration) -> bool + Sync),
    ) {
        for registration in registrations.list() {
            if !allowed(&registration) {
                let _ = registrations.unregister(&registration.token);
                continue;
            }
            let body = payload(notice, &registration);
            let outcome = match self {
                Self::Apns(apns) => apns.post(&registration, &body).await,
                Self::Directory(dir) => write_payload(dir, &body),
            };
            match outcome {
                Ok(Delivery::Sent) => {}
                Ok(Delivery::Gone) => {
                    // Apple says the token is dead (app deleted, push off).
                    let _ = registrations.unregister(&registration.token);
                }
                Err(error) => {
                    // Never the payload or the token: just that it failed.
                    eprintln!("diri-web: push not delivered: {error}");
                }
            }
        }
    }
}

pub enum Delivery {
    Sent,
    Gone,
}

fn write_payload(dir: &Path, body: &Value) -> io::Result<Delivery> {
    std::fs::create_dir_all(dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file = dir.join(format!("{stamp}.apns"));
    let mut body = body.clone();
    // `simctl push` needs the bundle id in the file.
    body["Simulator Target Bundle"] = json!(default_topic());
    std::fs::write(file, serde_json::to_vec_pretty(&body)?)?;
    Ok(Delivery::Sent)
}

/// APNs over HTTP/2 with a provider token.
pub struct Apns {
    config: PushConfig,
    /// Apple accepts a provider token for an hour and refuses one refreshed
    /// more often than every 20 minutes; reuse it for 50.
    token: Mutex<Option<(String, SystemTime)>>,
}

impl Apns {
    #[must_use]
    pub fn new(config: PushConfig) -> Self {
        Self {
            config,
            token: Mutex::new(None),
        }
    }

    async fn provider_token(&self) -> io::Result<String> {
        if let Some((token, minted)) = self.token.lock().unwrap_or_else(|e| e.into_inner()).clone()
            && minted.elapsed().unwrap_or(Duration::MAX) < Duration::from_secs(50 * 60)
        {
            return Ok(token);
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let token = provider_token(&self.config, now).await?;
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((token.clone(), SystemTime::now()));
        Ok(token)
    }

    async fn post(&self, registration: &Registration, body: &Value) -> io::Result<Delivery> {
        if !registration.token.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(Delivery::Gone);
        }
        let host = if registration.environment == "production" {
            "api.push.apple.com"
        } else {
            "api.sandbox.push.apple.com"
        };
        let jwt = self.provider_token().await?;
        let file = body_file(body)?;
        let outcome = self
            .curl(&url_for(host, &registration.token), &jwt, &file)
            .await;
        let _ = std::fs::remove_file(&file);
        outcome
    }

    async fn curl(&self, url: &str, jwt: &str, file: &Path) -> io::Result<Delivery> {
        // Headers and body go through stdin (`-H @-` is not portable, so a
        // config file on stdin): nothing secret appears in `ps`.
        let config = format!(
            "url = \"{url}\"\nhttp2\nsilent\nrequest = \"POST\"\nheader = \"authorization: bearer {jwt}\"\nheader = \"apns-topic: {}\"\nheader = \"apns-push-type: alert\"\nheader = \"apns-priority: 10\"\nheader = \"content-type: application/json\"\ndata = \"@{}\"\nwrite-out = \"%{{http_code}}\"\noutput = \"/dev/null\"\n",
            self.config.topic,
            file.display(),
        );
        let mut child = tokio::process::Command::new("curl")
            .args(["--config", "-", "--max-time", "15"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(config.as_bytes()).await?;
        }
        let output = child.wait_with_output().await?;
        let status = String::from_utf8_lossy(&output.stdout);
        match status.trim() {
            "200" => Ok(Delivery::Sent),
            // BadDeviceToken / Unregistered.
            "400" | "410" => Ok(Delivery::Gone),
            other => Err(io::Error::other(format!("APNs answered {other}"))),
        }
    }
}

/// The body in an owner-only temporary file, for curl's `data = "@file"`.
fn body_file(body: &Value) -> io::Result<PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "diri-push-{}-{}.json",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec(body)?)?;
    Ok(path)
}

fn url_for(host: &str, token: &str) -> String {
    format!("https://{host}/3/device/{token}")
}

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// An APNs provider token: a JWT `{alg: ES256, kid}.{iss: team, iat}`
/// signed with the auth key by `openssl` (which writes a DER signature; JWT
/// wants the raw 64-byte `r || s`).
pub async fn provider_token(config: &PushConfig, issued_at: u64) -> io::Result<String> {
    let header = base64url(
        json!({"alg": "ES256", "kid": config.key_id})
            .to_string()
            .as_bytes(),
    );
    let claims = base64url(
        json!({"iss": config.team_id, "iat": issued_at})
            .to_string()
            .as_bytes(),
    );
    let signing_input = format!("{header}.{claims}");
    let mut child = tokio::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-sign"])
        .arg(&config.key_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(signing_input.as_bytes()).await?;
    }
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        return Err(io::Error::other("openssl could not sign with the APNs key"));
    }
    let raw = der_to_raw(&output.stdout)
        .ok_or_else(|| io::Error::other("unexpected ECDSA signature encoding"))?;
    Ok(format!("{signing_input}.{}", base64url(&raw)))
}

/// `SEQUENCE { INTEGER r, INTEGER s }` → 32-byte big-endian `r || s`.
#[must_use]
pub fn der_to_raw(der: &[u8]) -> Option<[u8; 64]> {
    fn integer(input: &[u8]) -> Option<(&[u8], &[u8])> {
        let (&tag, rest) = input.split_first()?;
        let (&length, rest) = rest.split_first()?;
        if tag != 0x02 || usize::from(length) > rest.len() {
            return None;
        }
        Some(rest.split_at(usize::from(length)))
    }
    let (&tag, rest) = der.split_first()?;
    let (&length, body) = rest.split_first()?;
    if tag != 0x30 || usize::from(length) != body.len() {
        return None;
    }
    let (r, rest) = integer(body)?;
    let (s, rest) = integer(rest)?;
    if !rest.is_empty() {
        return None;
    }
    let mut raw = [0u8; 64];
    let (left, right) = raw.split_at_mut(32);
    for (value, slot) in [(r, left), (s, right)] {
        let value = value.strip_prefix(&[0]).unwrap_or(value);
        if value.len() > 32 {
            return None;
        }
        slot[32 - value.len()..].copy_from_slice(value);
    }
    Some(raw)
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

/// Watches the Engine and pushes notices until the gateway stops.
pub async fn run(api: Arc<crate::api::Api>, sender: Sender, registrations: Arc<Registrations>) {
    let gate = api.tailnet.clone();
    let allowed = move |registration: &Registration| match (&gate, &registration.node) {
        (Some(gate), Some(node)) => gate.devices.is_paired(node),
        // Under tailnet admission every registration carries its node.
        (Some(_), None) => false,
        (None, _) => true,
    };
    let mut watcher = Watcher::default();
    loop {
        // Subscribed first, then seeded, so nothing falls in between.
        let Ok(mut events) = api.client.subscribe_events().await else {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        };
        if let Ok(result) = api.client.sessions().await {
            watcher.seed(result.sessions);
        }
        loop {
            match events.recv().await {
                Ok(event) => {
                    for notice in watcher.observe(&event.name, &event.params) {
                        if routed(notice.kind) {
                            sender.send(&notice, &registrations, &allowed).await;
                        }
                    }
                }
                // Missed events: start again from what the sessions are now.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(status: Value, needs: Option<(&str, f64)>) -> Value {
        let mut value = json!({
            "id": "s_a", "kind": {"claudeCode": {}}, "cwd": "/secret/repo", "projectID": "p",
            "title": "Rotate the production keys", "titleSource": 0, "status": status,
            "resumability": "live", "createdAt": 1.0, "updatedAt": 1.0, "pinned": false,
        });
        if let Some((kind, at)) = needs {
            value["needsInput"] = json!({"kind": kind, "source": "screen",
                "summary": "Run rm -rf build?", "promptExcerpt": "Bash(rm -rf build)",
                "riskHint": "destructive", "occurredAt": at});
        }
        value
    }

    #[test]
    fn a_new_prompt_and_a_failure_are_noticed_once() {
        let mut watcher = Watcher::default();
        let working = record(json!({"working": {}}), None);
        watcher.seed(vec![serde_json::from_value(working.clone()).unwrap()]);
        let waiting = record(
            json!({"needsInput": {"_0": "permission"}}),
            Some(("permission", 10.0)),
        );
        let notices = watcher.observe(EventName::SESSION_UPDATED, &waiting);
        assert_eq!(
            notices,
            vec![Notice {
                session: "s_a".into(),
                kind: Kind::Needs,
                occurred_at: Some(10.0)
            }]
        );
        // The same prompt again (any other field changed): nothing new.
        assert!(
            watcher
                .observe(EventName::SESSION_UPDATED, &waiting)
                .is_empty()
        );
        // A different prompt is news.
        let next = record(
            json!({"needsInput": {"_0": "permission"}}),
            Some(("permission", 20.0)),
        );
        assert_eq!(watcher.observe(EventName::SESSION_UPDATED, &next).len(), 1);
        // A failed exit.
        let failed = record(
            json!({"exited": {"_0": {"reason": "exited", "code": 1}}}),
            None,
        );
        assert_eq!(
            watcher.observe(EventName::SESSION_UPDATED, &failed),
            vec![Notice {
                session: "s_a".into(),
                kind: Kind::Failed,
                occurred_at: None
            }]
        );
        // A session first seen in an update only sets its baseline.
        let mut other = waiting.clone();
        other["id"] = json!("s_new");
        assert!(
            watcher
                .observe(EventName::SESSION_UPDATED, &other)
                .is_empty()
        );
    }

    #[test]
    fn the_payload_carries_nothing_from_the_session() {
        let notice = Notice {
            session: "s_a".into(),
            kind: Kind::Needs,
            occurred_at: Some(10.0),
        };
        let registration = Registration {
            token: "ab".into(),
            environment: "sandbox".into(),
            base: "http://100.64.0.1:7380".into(),
            node: None,
        };
        let body = payload(&notice, &registration).to_string();
        for secret in ["Rotate", "rm -rf", "/secret/repo", "build"] {
            assert!(!body.contains(secret), "{secret} leaked into {body}");
        }
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["aps"]["mutable-content"], 1);
        assert_eq!(value["aps"]["category"], "diri.needs");
        assert_eq!(
            value["diri"],
            json!({"s": "s_a", "k": "needs", "b": "http://100.64.0.1:7380", "o": 10.0})
        );
    }

    #[test]
    fn routes_follow_the_macs_notify_switches() {
        set_push_routes(true, false, false);
        assert!(routed(Kind::Needs));
        assert!(!routed(Kind::Failed));
        assert!(!routed(Kind::Done));
        set_push_routes(true, true, true);
        assert!(routed(Kind::Done));
    }

    #[test]
    fn der_signatures_become_raw_r_and_s() {
        // r with a leading zero (high bit set), s short.
        let mut der = vec![0x30, 0x00, 0x02, 33, 0x00];
        der.extend([0x80; 32]);
        der.extend([0x02, 2, 0x01, 0x02]);
        der[1] = (der.len() - 2) as u8;
        let raw = der_to_raw(&der).unwrap();
        assert_eq!(&raw[..32], &[0x80; 32]);
        assert_eq!(&raw[32..62], &[0u8; 30]);
        assert_eq!(&raw[62..], &[0x01, 0x02]);
        assert!(der_to_raw(&[0x30, 0x01, 0x00]).is_none());
    }

    /// A real ES256 key through the real `openssl`: the token verifies.
    #[tokio::test]
    async fn the_provider_token_verifies_with_the_public_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("AuthKey.p8");
        let made = std::process::Command::new("openssl")
            .args([
                "ecparam",
                "-name",
                "prime256v1",
                "-genkey",
                "-noout",
                "-out",
            ])
            .arg(&key)
            .status();
        if !made.is_ok_and(|status| status.success()) {
            eprintln!("skipped: no openssl");
            return;
        }
        let config = PushConfig {
            key_path: key.clone(),
            key_id: "KEY123".into(),
            team_id: "TEAM45".into(),
            topic: default_topic(),
        };
        let token = provider_token(&config, 1_800_000_000).await.unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(parts[0])
                .unwrap(),
        )
        .unwrap();
        assert_eq!(header, json!({"alg": "ES256", "kid": "KEY123"}));
        let claims: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(parts[1])
                .unwrap(),
        )
        .unwrap();
        assert_eq!(claims, json!({"iss": "TEAM45", "iat": 1_800_000_000u64}));
        // Re-encode r||s as DER and verify with the public key.
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[2])
            .unwrap();
        assert_eq!(raw.len(), 64);
        let integer = |bytes: &[u8]| {
            let trimmed: Vec<u8> = bytes.iter().copied().skip_while(|b| *b == 0).collect();
            let mut value = if trimmed.first().is_some_and(|b| b & 0x80 != 0) {
                vec![0]
            } else {
                vec![]
            };
            value.extend(trimmed);
            let mut out = vec![0x02, value.len() as u8];
            out.extend(value);
            out
        };
        let mut body = integer(&raw[..32]);
        body.extend(integer(&raw[32..]));
        let mut der = vec![0x30, body.len() as u8];
        der.extend(body);
        let signature = dir.path().join("sig.der");
        std::fs::write(&signature, der).unwrap();
        let public = dir.path().join("pub.pem");
        assert!(
            std::process::Command::new("openssl")
                .args(["ec", "-pubout", "-in"])
                .arg(&key)
                .arg("-out")
                .arg(&public)
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        let input = dir.path().join("input");
        std::fs::write(&input, format!("{}.{}", parts[0], parts[1])).unwrap();
        let verified = std::process::Command::new("openssl")
            .args(["dgst", "-sha256", "-verify"])
            .arg(&public)
            .arg("-signature")
            .arg(&signature)
            .arg(&input)
            .output()
            .unwrap();
        assert!(
            verified.status.success(),
            "{}",
            String::from_utf8_lossy(&verified.stdout)
        );
    }

    #[test]
    fn registrations_are_owner_only_and_keyed_by_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("push-devices.json");
        let registrations = Registrations::open(path.clone());
        let phone = Registration {
            token: "aa".into(),
            environment: "sandbox".into(),
            base: "http://a".into(),
            node: None,
        };
        registrations.register(phone.clone()).unwrap();
        registrations
            .register(Registration {
                base: "http://b".into(),
                ..phone.clone()
            })
            .unwrap();
        assert_eq!(Registrations::open(path.clone()).list().len(), 1);
        assert_eq!(Registrations::open(path.clone()).list()[0].base, "http://b");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        registrations.unregister("aa").unwrap();
        assert!(Registrations::open(path).list().is_empty());
    }
}
