//! Cursor dashboard usage events. Token counts and billed cents come from
//! Cursor's personal usage API, not local transcripts (those have no usage).
//!
//! Auth is read from the signed-in Cursor IDE store or macOS keychain. Tokens
//! travel through curl's stdin config and are never logged.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{cache::CursorFetchWindow, model::UsageHourAgg, parser::fnv1a};

const DASHBOARD_URL: &str = "https://cursor.com/api/dashboard/get-filtered-usage-events";
const OAUTH_TOKEN_URL: &str = "https://api2.cursor.sh/oauth/token";
/// Cursor's public native OAuth client id (same value TokenLeader uses).
const OAUTH_CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";
const PAGE_SIZE: u32 = 100;
const MAX_PAGES: u32 = 10;
const OVERLAP_MS: i64 = 5 * 60 * 1000;
const FETCH_TIMEOUT_SECS: u64 = 25;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CursorUsageEvent {
    pub id: String,
    pub timestamp_ms: i64,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost: f64,
}

pub(crate) fn event_hour(event: &CursorUsageEvent) -> i64 {
    event.timestamp_ms.div_euclid(3_600_000)
}

pub(crate) fn event_aggregate(event: &CursorUsageEvent) -> UsageHourAgg {
    UsageHourAgg {
        i: event.input_tokens,
        o: event.output_tokens,
        cr: event.cache_read_tokens,
        cw: event.cache_write_tokens,
        c: event.cost,
    }
}

pub(crate) fn map_dashboard_event(value: &Value) -> Option<CursorUsageEvent> {
    let timestamp_ms = parse_timestamp_ms(value.get("timestamp"))?;
    let usage = value.get("tokenUsage");
    let input_tokens = integer(value.get("inputTokens")).or_else(|| {
        usage
            .and_then(|usage| usage.get("inputTokens"))
            .map(integer_value)
    });
    let output_tokens = integer(value.get("outputTokens")).or_else(|| {
        usage
            .and_then(|usage| usage.get("outputTokens"))
            .map(integer_value)
    });
    let cache_write = usage
        .and_then(|usage| usage.get("cacheWriteTokens"))
        .map(integer_value)
        .unwrap_or(0);
    let cache_read = usage
        .and_then(|usage| usage.get("cacheReadTokens"))
        .map(integer_value)
        .unwrap_or(0);
    let cents = float(value.get("totalCents")).or_else(|| {
        usage
            .and_then(|usage| usage.get("totalCents"))
            .map(float_value)
    });
    let model = value
        .get("modelName")
        .and_then(Value::as_str)
        .or_else(|| value.get("model").and_then(Value::as_str))
        .unwrap_or("cursor")
        .trim();
    let model = if model.is_empty() {
        "cursor".to_owned()
    } else {
        model.to_owned()
    };
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "{timestamp_ms}:{model}:{}:{}:{cache_write}:{cache_read}",
                input_tokens.unwrap_or(0),
                output_tokens.unwrap_or(0)
            )
        });
    Some(CursorUsageEvent {
        id,
        timestamp_ms,
        model,
        input_tokens: input_tokens.unwrap_or(0).max(0),
        output_tokens: output_tokens.unwrap_or(0).max(0),
        cache_read_tokens: cache_read.max(0),
        cache_write_tokens: cache_write.max(0),
        cost: cents.unwrap_or(0.0).max(0.0) / 100.0,
    })
}

pub(crate) fn events_from_dashboard_body(body: &Value) -> Vec<CursorUsageEvent> {
    body.get("usageEvents")
        .or_else(|| body.get("usageEventsDisplay"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(map_dashboard_event)
        .collect()
}

pub(crate) fn workos_session_token(access_token: &str) -> Option<String> {
    let access_token = access_token.trim();
    if access_token.is_empty() {
        return None;
    }
    if access_token.contains("::") {
        return Some(access_token.to_owned());
    }
    let user_id = user_id_from_jwt(access_token)?;
    Some(format!("{user_id}::{access_token}"))
}

fn user_id_from_jwt(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = decode_b64url(payload)?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    let sub = value.get("sub")?.as_str()?.trim();
    if sub.is_empty() {
        return None;
    }
    Some(
        sub.rsplit_once('|')
            .map(|(_, id)| id)
            .unwrap_or(sub)
            .to_owned(),
    )
}

fn decode_b64url(input: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};

    URL_SAFE_NO_PAD.decode(input).ok().or_else(|| {
        let mut padded = input.to_owned();
        while !padded.len().is_multiple_of(4) {
            padded.push('=');
        }
        URL_SAFE.decode(padded).ok()
    })
}

struct CursorAuth {
    session_token: String,
    refresh_token: Option<String>,
    machine_id: Option<String>,
}

/// A bounded page batch; only a complete walk may advance the committed watermark.
pub(crate) struct CursorBatch {
    pub events: Vec<CursorUsageEvent>,
    pub window: CursorFetchWindow,
    pub complete: bool,
}

/// One independent fetch at a time. Transcript updates never wait on this task.
#[derive(Default)]
pub(crate) struct CursorRefresh {
    task: Option<tokio::task::JoinHandle<Result<CursorBatch, ()>>>,
}

impl CursorRefresh {
    pub(crate) fn start(&mut self, home: &Path, window: CursorFetchWindow) {
        if self.task.is_some() {
            return;
        }
        let home = home.to_owned();
        self.task = Some(tokio::spawn(async move {
            // SQLite and Keychain access must not block a Tokio worker.
            let mut auth = tokio::task::spawn_blocking(move || load_cursor_auth(&home))
                .await
                .map_err(|_| ())?
                .ok_or(())?;
            fetch_cursor_events(&CurlHttp, &mut auth, window).await
        }));
    }

    pub(crate) async fn next(&mut self) -> Result<CursorBatch, ()> {
        let Some(task) = self.task.as_mut() else {
            return std::future::pending().await;
        };
        let result = task.await.map_err(|_| ()).and_then(|result| result);
        self.task = None;
        result
    }

    #[cfg(test)]
    pub(crate) fn with_task(task: tokio::task::JoinHandle<Result<CursorBatch, ()>>) -> Self {
        Self { task: Some(task) }
    }
}

impl Drop for CursorRefresh {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn load_cursor_auth(home: &Path) -> Option<CursorAuth> {
    let from_ide = read_ide_auth(home);
    let access = from_ide
        .as_ref()
        .and_then(|auth| auth.access_token.clone())
        .or_else(keychain_access_token)?;
    let session_token = workos_session_token(&access)?;
    if session_token.chars().any(char::is_control) {
        return None;
    }
    let refresh = from_ide
        .as_ref()
        .and_then(|auth| auth.refresh_token.clone())
        .or_else(keychain_refresh_token)
        .filter(|token| !token.is_empty() && !token.chars().any(char::is_control));
    let machine_id = from_ide
        .and_then(|auth| auth.machine_id)
        .filter(|id| !id.is_empty() && !id.chars().any(char::is_control));
    Some(CursorAuth {
        session_token,
        refresh_token: refresh,
        machine_id,
    })
}

struct IdeAuth {
    access_token: Option<String>,
    refresh_token: Option<String>,
    machine_id: Option<String>,
}

fn read_ide_auth(home: &Path) -> Option<IdeAuth> {
    let path = cursor_state_db(home);
    if !path.is_file() {
        return None;
    }
    let connection = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = connection.busy_timeout(Duration::from_millis(40));
    Some(IdeAuth {
        access_token: query_item(&connection, "cursorAuth/accessToken"),
        refresh_token: query_item(&connection, "cursorAuth/refreshToken"),
        machine_id: query_item(&connection, "storage.serviceMachineId")
            .or_else(|| query_item(&connection, "telemetry.machineId")),
    })
}

fn cursor_state_db(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb")
    }
    #[cfg(not(target_os = "macos"))]
    {
        home.join(".config/Cursor/User/globalStorage/state.vscdb")
    }
}

fn query_item(connection: &rusqlite::Connection, key: &str) -> Option<String> {
    let value: String = connection
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1 LIMIT 1",
            [key],
            |row| row.get(0),
        )
        .ok()?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn keychain_access_token() -> Option<String> {
    keychain_password("cursor-access-token")
}

fn keychain_refresh_token() -> Option<String> {
    keychain_password("cursor-refresh-token")
}

fn keychain_password(service: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-s",
                service,
                "-a",
                "cursor-user",
                "-w",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let value = String::from_utf8(output.stdout).ok()?;
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_owned())
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = service;
        None
    }
}

async fn fetch_cursor_events(
    http: &impl CursorHttp,
    auth: &mut CursorAuth,
    mut window: CursorFetchWindow,
) -> Result<CursorBatch, ()> {
    let mut events = Vec::new();
    let mut complete = false;
    for _ in 0..MAX_PAGES {
        let body = serde_json::json!({
            "page": window.next_page,
            "pageSize": PAGE_SIZE,
            "startDate": window.start_ms.max(0),
            "endDate": window.end_ms,
        });
        let (mut status, mut payload) = dashboard_post(http, auth, &body).await?;
        if status == 401 || status == 403 {
            refresh_session(http, auth).await?;
            (status, payload) = dashboard_post(http, auth, &body).await?;
        }
        if status != 200 {
            return Err(());
        }
        // Invalid responses must not masquerade as the end of a successful walk.
        let raw_events = payload
            .get("usageEvents")
            .or_else(|| payload.get("usageEventsDisplay"))
            .and_then(Value::as_array)
            .ok_or(())?;
        let count = raw_events.len();
        let page_events = events_from_dashboard_body(&payload);
        if page_events.len() != count {
            return Err(());
        }
        for event in &page_events {
            window.newest_event_ms = window.newest_event_ms.max(event.timestamp_ms);
        }
        events.extend(page_events);
        let has_next = payload
            .pointer("/pagination/hasNextPage")
            .and_then(Value::as_bool);
        window.next_page = window.next_page.checked_add(1).ok_or(())?;
        if has_next == Some(false) || (has_next != Some(true) && count < PAGE_SIZE as usize) {
            complete = true;
            break;
        }
        if count == 0 {
            return Err(());
        }
    }
    Ok(CursorBatch {
        events,
        window,
        complete,
    })
}

async fn dashboard_post(
    http: &impl CursorHttp,
    auth: &CursorAuth,
    body: &Value,
) -> Result<(u16, Value), ()> {
    let mut headers = vec![
        ("Content-Type".into(), "application/json".into()),
        (
            "Cookie".into(),
            format!("WorkosCursorSessionToken={}", auth.session_token),
        ),
        ("Origin".into(), "https://cursor.com".into()),
    ];
    if let Some(machine_id) = &auth.machine_id {
        headers.push(("x-cursor-client-id".into(), machine_id.clone()));
    }
    http.post(DASHBOARD_URL, &headers, body).await
}

async fn refresh_session(http: &impl CursorHttp, auth: &mut CursorAuth) -> Result<(), ()> {
    let refresh = auth.refresh_token.as_deref().ok_or(())?;
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": OAUTH_CLIENT_ID,
        "refresh_token": refresh,
    });
    let (status, payload) = http
        .post(
            OAUTH_TOKEN_URL,
            &[("Content-Type".into(), "application/json".into())],
            &body,
        )
        .await?;
    if status != 200 || payload.get("shouldLogout").and_then(Value::as_bool) == Some(true) {
        return Err(());
    }
    let access = payload
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or(())?;
    auth.session_token = workos_session_token(access).ok_or(())?;
    Ok(())
}

trait CursorHttp {
    async fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<(u16, Value), ()>;
}

struct CurlHttp;

impl CursorHttp for CurlHttp {
    async fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<(u16, Value), ()> {
        http_json("POST", url, headers, Some(body)).await
    }
}

async fn http_json(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&Value>,
) -> Result<(u16, Value), ()> {
    let mut config = String::from("silent\nproto = \"=https\"\n");
    config.push_str(&format!("max-time = \"{FETCH_TIMEOUT_SECS}\"\n"));
    config.push_str("max-filesize = \"2097152\"\n");
    config.push_str("write-out = \"\\n%{http_code}\"\n");
    config.push_str("request = \"");
    config.push_str(method);
    config.push_str("\"\nurl = \"");
    config.push_str(&curl_escape(url));
    config.push_str("\"\n");
    for (name, value) in headers {
        if name.chars().any(char::is_control) || value.chars().any(char::is_control) {
            return Err(());
        }
        config.push_str("header = \"");
        config.push_str(&curl_escape(&format!("{name}: {value}")));
        config.push_str("\"\n");
    }
    if let Some(body) = body {
        config.push_str("data = \"");
        config.push_str(&curl_escape(&body.to_string()));
        config.push_str("\"\n");
    }
    let mut child = Command::new("/usr/bin/curl")
        .args(["-q", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ())?;
    let mut stdin = child.stdin.take().ok_or(())?;
    stdin.write_all(config.as_bytes()).await.map_err(|_| ())?;
    drop(stdin);
    let output = tokio::time::timeout(
        Duration::from_secs(FETCH_TIMEOUT_SECS + 4),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }
    let raw = std::str::from_utf8(&output.stdout).map_err(|_| ())?;
    let (body, status) = raw.rsplit_once('\n').ok_or(())?;
    let status: u16 = status.trim().parse().map_err(|_| ())?;
    if body.is_empty() {
        return Ok((status, Value::Null));
    }
    let payload = serde_json::from_str(body).unwrap_or(Value::Null);
    Ok((status, payload))
}

fn curl_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn parse_timestamp_ms(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    let ms = if let Some(number) = value.as_i64() {
        number
    } else if let Some(number) = value.as_f64() {
        number as i64
    } else {
        value.as_str()?.parse::<i64>().ok()?
    };
    (ms > 0).then_some(ms)
}

fn integer(value: Option<&Value>) -> Option<i64> {
    value.map(integer_value)
}

fn integer_value(value: &Value) -> i64 {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
        .unwrap_or(0)
}

fn float(value: Option<&Value>) -> Option<f64> {
    value.map(float_value)
}

fn float_value(value: &Value) -> f64 {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|number| number as f64))
        .unwrap_or(0.0)
}

pub(crate) fn cursor_overlap_ms() -> i64 {
    OVERLAP_MS
}

pub(crate) fn event_dedup_hash(id: &str) -> u64 {
    fnv1a(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_model_name_and_billed_cents() {
        let event = map_dashboard_event(&json!({
            "id": "evt-123",
            "timestamp": "1704067200000",
            "modelName": "claude-4.5-sonnet",
            "inputTokens": 100,
            "outputTokens": 50,
            "totalCents": 1.23,
        }))
        .unwrap();
        assert_eq!(event.id, "evt-123");
        assert_eq!(event.model, "claude-4.5-sonnet");
        assert_eq!(event.input_tokens, 100);
        assert_eq!(event.output_tokens, 50);
        assert!((event.cost - 0.0123).abs() < 1e-9);
        assert_eq!(event_hour(&event), 1704067200000 / 3_600_000);
    }

    #[test]
    fn reads_nested_token_usage() {
        let event = map_dashboard_event(&json!({
            "id": "evt-456",
            "timestamp": 1_700_000_000_000_i64,
            "model": "gpt-4.1",
            "tokenUsage": {
                "inputTokens": 10,
                "outputTokens": 5,
                "cacheWriteTokens": 2,
                "cacheReadTokens": 3,
                "totalCents": 0.5,
            },
        }))
        .unwrap();
        assert_eq!(event.input_tokens, 10);
        assert_eq!(event.cache_write_tokens, 2);
        assert_eq!(event.cache_read_tokens, 3);
        assert!((event.cost - 0.005).abs() < 1e-9);
    }

    #[test]
    fn workos_cookie_uses_jwt_subject_tail() {
        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            br#"{"sub":"auth0|user-99"}"#,
        );
        let jwt = format!("header.{payload}.sig");
        assert_eq!(
            workos_session_token(&jwt).as_deref(),
            Some(format!("user-99::{jwt}").as_str())
        );
        assert_eq!(
            workos_session_token("abc::already").as_deref(),
            Some("abc::already")
        );
    }

    #[test]
    fn dashboard_body_prefers_usage_events() {
        let events = events_from_dashboard_body(&json!({
            "usageEvents": [{
                "id": "a",
                "timestamp": 1_700_000_000_000_i64,
                "model": "composer-1",
                "inputTokens": 1,
                "outputTokens": 2,
                "totalCents": 10
            }],
            "usageEventsDisplay": [],
        }));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model, "composer-1");
    }
    struct RefreshHttp;

    impl CursorHttp for RefreshHttp {
        async fn post(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: &Value,
        ) -> Result<(u16, Value), ()> {
            assert_eq!(url, OAUTH_TOKEN_URL);
            assert!(
                headers
                    .iter()
                    .any(|(name, value)| name == "Content-Type" && value == "application/json"),
                "JSON refresh requests must declare their content type"
            );
            assert_eq!(body["grant_type"], "refresh_token");
            Ok((200, json!({"access_token": "user::refreshed"})))
        }
    }

    #[tokio::test]
    async fn cursor_regression_refresh_declares_json() {
        let mut auth = CursorAuth {
            session_token: "user::expired".into(),
            refresh_token: Some("fixture-refresh".into()),
            machine_id: None,
        };
        refresh_session(&RefreshHttp, &mut auth).await.unwrap();
        assert_eq!(auth.session_token, "user::refreshed");
    }

    struct PagesHttp;

    impl CursorHttp for PagesHttp {
        async fn post(
            &self,
            url: &str,
            _: &[(String, String)],
            body: &Value,
        ) -> Result<(u16, Value), ()> {
            assert_eq!(url, DASHBOARD_URL);
            let page = body["page"].as_u64().unwrap();
            let events: Vec<_> = ((page - 1) * 100..(page * 100).min(1001))
                .map(|index| {
                    json!({
                        "id": format!("event-{index}"),
                        "timestamp": 1_784_717_999_000_i64 - index as i64 * 60_000,
                        "totalCents": 100,
                    })
                })
                .collect();
            Ok((
                200,
                json!({
                    "usageEvents": events,
                    "pagination": {"hasNextPage": page < 11},
                }),
            ))
        }
    }

    #[tokio::test]
    async fn cursor_regression_fetch_keeps_older_pages() {
        let mut auth = CursorAuth {
            session_token: "user::fixture".into(),
            refresh_token: None,
            machine_id: None,
        };
        let window = CursorFetchWindow {
            start_ms: 0,
            end_ms: 1_784_718_000_000,
            next_page: 1,
            newest_event_ms: 0,
        };
        let first = fetch_cursor_events(&PagesHttp, &mut auth, window)
            .await
            .unwrap();
        assert!(!first.complete);
        assert_eq!(first.window.next_page, 11);
        assert_eq!(first.window.start_ms, window.start_ms);
        assert_eq!(first.window.end_ms, window.end_ms);
        let second = fetch_cursor_events(&PagesHttp, &mut auth, first.window)
            .await
            .unwrap();
        assert!(second.complete);
        assert_eq!(
            first.events.len() + second.events.len(),
            1001,
            "older usage must remain reachable after the page cap"
        );
    }
    struct ScriptHttp {
        responses: std::cell::RefCell<std::collections::VecDeque<Result<(u16, Value), ()>>>,
        requests: std::cell::RefCell<Vec<(String, Value)>>,
    }

    impl CursorHttp for ScriptHttp {
        async fn post(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: &Value,
        ) -> Result<(u16, Value), ()> {
            assert!(
                headers
                    .iter()
                    .any(|(name, value)| name == "Content-Type" && value == "application/json")
            );
            self.requests.borrow_mut().push((url.into(), body.clone()));
            self.responses
                .borrow_mut()
                .pop_front()
                .expect("unexpected extra request")
        }
    }

    #[tokio::test]
    async fn cursor_regression_expired_auth_retries_the_same_page() {
        let http = ScriptHttp {
            responses: std::cell::RefCell::new(
                [
                    Ok((401, Value::Null)),
                    Ok((200, json!({"access_token": "user::refreshed"}))),
                    Ok((
                        200,
                        json!({"usageEvents": [], "pagination": {"hasNextPage": false}}),
                    )),
                ]
                .into(),
            ),
            requests: Default::default(),
        };
        let mut auth = CursorAuth {
            session_token: "user::expired".into(),
            refresh_token: Some("fixture".into()),
            machine_id: None,
        };
        let window = CursorFetchWindow {
            start_ms: 1000,
            end_ms: 2000,
            next_page: 11,
            newest_event_ms: 1500,
        };
        let batch = fetch_cursor_events(&http, &mut auth, window).await.unwrap();
        assert!(batch.complete);
        let requests = http.requests.borrow();
        assert_eq!(requests[0], requests[2]);
        assert_eq!(requests[1].0, OAUTH_TOKEN_URL);
        assert_eq!(auth.session_token, "user::refreshed");
    }

    #[tokio::test]
    async fn cursor_regression_failed_pages_do_not_complete_a_walk() {
        for response in [
            Err(()),
            Ok((500, Value::Null)),
            Ok((200, Value::Null)),
            Ok((200, json!({"usageEvents": [{"timestamp": "invalid"}]}))),
        ] {
            let http = ScriptHttp {
                responses: std::cell::RefCell::new([
                    Ok((200, json!({"usageEvents": [{"timestamp": 1500}], "pagination": {"hasNextPage": true}}))),
                    response,
                ].into()),
                requests: Default::default(),
            };
            let mut auth = CursorAuth {
                session_token: "user::fixture".into(),
                refresh_token: None,
                machine_id: None,
            };
            let window = CursorFetchWindow {
                start_ms: 1000,
                end_ms: 2000,
                next_page: 11,
                newest_event_ms: 1500,
            };
            assert!(fetch_cursor_events(&http, &mut auth, window).await.is_err());
            assert_eq!(http.requests.borrow().len(), 2);
        }
    }
}
