use std::collections::HashSet;
use std::time::Duration;

use diri_ui::SemanticColors;
use gpui::{AppContext as _, TestAppContext};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::http::{self, HttpError, Outgoing};
use super::json::{PrettyJson, Token};
use super::model::*;
use super::storage;
use super::*;

// MARK: URL ⇄ params

#[test]
fn typing_a_url_fills_the_params_table() {
    let request = ApiRequest::new(
        "r",
        Method::Get,
        "https://api.example.com/items?q=red%20shoes&page=2&flag#top",
    );
    assert_eq!(
        request.params,
        vec![
            Pair::new("q", "red shoes"),
            Pair::new("page", "2"),
            Pair::new("flag", ""),
        ]
    );
    assert_eq!(
        query_pairs("http://x/?a=1+2&b=%E2%9C%93"),
        vec![Pair::new("a", "1 2"), Pair::new("b", "✓")]
    );
    assert!(query_pairs("http://x/path").is_empty());
}

#[test]
fn editing_params_rewrites_only_the_query() {
    let mut request = ApiRequest::new("r", Method::Get, "https://api.example.com/items?q=old#frag");
    let mut params = request.params.clone();
    params[0].value = "café & co".into();
    params.push(Pair::new("page", "2"));
    request.set_params(params);
    assert_eq!(
        request.url,
        "https://api.example.com/items?q=caf%C3%A9%20%26%20co&page=2#frag"
    );

    // A row switched off leaves the URL but stays in the table.
    let mut params = request.params.clone();
    params[0].enabled = false;
    request.set_params(params);
    assert_eq!(request.url, "https://api.example.com/items?page=2#frag");
    assert_eq!(request.params.len(), 2);

    // A new, unnamed row does not go out, and survives retyping the URL.
    let mut params = request.params.clone();
    params.push(Pair::new("", ""));
    request.set_params(params);
    assert_eq!(request.url, "https://api.example.com/items?page=2#frag");
    request.set_url("https://api.example.com/items?page=3&sort=new");
    assert_eq!(
        request.params,
        vec![
            Pair::new("page", "3"),
            Pair::new("sort", "new"),
            Pair {
                enabled: false,
                name: "q".into(),
                value: "café & co".into()
            },
            Pair::new("", ""),
        ]
    );

    // Removing the last live row drops the `?`.
    request.set_params(vec![]);
    assert_eq!(request.url, "https://api.example.com/items");
}

#[test]
fn placeholders_survive_the_params_round_trip() {
    let mut request = ApiRequest::new("r", Method::Get, "{{base}}/search?q={{term}}");
    assert_eq!(request.params, vec![Pair::new("q", "{{term}}")]);
    let params = vec![Pair::new("q", "{{term}}"), Pair::new("key", "{{api key}}")];
    request.set_params(params);
    assert_eq!(request.url, "{{base}}/search?q={{term}}&key={{api key}}");
    assert_eq!(percent("a b{{x}}&"), "a%20b{{x}}%26");
    assert_eq!(percent("{{open"), "%7B%7Bopen");
    assert_eq!(unpercent("%zz%4"), "%zz%4", "malformed escapes stay");
}

#[test]
fn a_stored_request_whose_table_drifted_is_repaired() {
    let mut request = ApiRequest::new("r", Method::Get, "http://x/?a=1");
    request.params = vec![Pair::new("b", "2")];
    request.normalize();
    assert_eq!(request.params, vec![Pair::new("a", "1")]);
}

// MARK: Variables

fn environment(variables: &[(&str, &str, bool)]) -> Environment {
    Environment {
        id: "env".into(),
        name: "Local".into(),
        variables: variables
            .iter()
            .map(|(name, value, secret)| Variable::new(*name, *value, *secret))
            .collect(),
    }
}

#[test]
fn variables_fill_in_and_the_missing_are_named() {
    let env = environment(&[("host", "api.example.com", false)]);
    let resolved = resolve("https://{{host}}/v1?key={{ token }}", Some(&env));
    assert_eq!(resolved.text, "https://api.example.com/v1?key={{ token }}");
    assert_eq!(resolved.missing, vec!["token".to_owned()]);
    let resolved = resolve("{{host}}/{{open", Some(&env));
    assert_eq!(resolved.text, "api.example.com/{{open");
    assert!(
        resolved.missing.is_empty(),
        "braces that never close stay as written"
    );
    let none = resolve("{{host}}", None);
    assert_eq!(none.missing, vec!["host".to_owned()]);
}

#[test]
fn variables_nest_and_cycles_are_caught() {
    let env = environment(&[
        ("scheme", "https", false),
        ("host", "api.example.com", false),
        ("base", "{{scheme}}://{{host}}/v1", false),
        ("token", "sk_live_1", true),
        ("auth", "Bearer {{token}}", false),
        ("a", "{{b}}", false),
        ("b", "{{a}}", false),
    ]);
    let resolved = resolve("{{base}}/me", Some(&env));
    assert_eq!(resolved.text, "https://api.example.com/v1/me");
    assert!(!resolved.uses_secret);
    let auth = resolve("{{auth}}", Some(&env));
    assert_eq!(auth.text, "Bearer sk_live_1");
    assert!(
        auth.uses_secret,
        "a secret reached through another variable counts"
    );
    let cycle = resolve("{{a}}", Some(&env));
    assert_eq!(cycle.cyclic, vec!["a".to_owned()]);
    // A disabled variable is as good as missing.
    let mut disabled = environment(&[("host", "x", false)]);
    disabled.variables[0].enabled = false;
    assert_eq!(
        resolve("{{host}}", Some(&disabled)).missing,
        vec!["host".to_owned()]
    );
}

#[test]
fn prepare_resolves_everything_and_adds_auth_and_content_type() {
    let env = environment(&[("base", "localhost:3000", false), ("token", "t0k", true)]);
    let mut request = ApiRequest::new("r", Method::Post, "{{base}}/items?x=1");
    request.headers = vec![
        Pair::new("X-Trace", "{{token}}"),
        Pair {
            enabled: false,
            name: "X-Off".into(),
            value: "1".into(),
        },
    ];
    request.auth = Auth::Bearer {
        token: "{{token}}".into(),
    };
    request.body_kind = BodyKind::Json;
    request.body = "{\"a\":\"{{token}}\"}".into();
    let outgoing = prepare(&request, Some(&env)).unwrap();
    assert_eq!(outgoing.url, "http://localhost:3000/items?x=1");
    assert_eq!(
        outgoing.headers,
        vec![
            ("X-Trace".to_owned(), "t0k".to_owned()),
            ("Authorization".to_owned(), "Bearer t0k".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
        ]
    );
    assert_eq!(outgoing.body.as_deref(), Some(&b"{\"a\":\"t0k\"}"[..]));

    request.headers.push(Pair::new("{{missing}}", "1"));
    assert_eq!(
        prepare(&request, Some(&env)),
        Err(PrepareError::Missing(vec!["missing".into()]))
    );

    let mut form = ApiRequest::new("f", Method::Post, "https://example.com/login");
    form.body_kind = BodyKind::Form;
    form.form = vec![
        Pair::new("user", "ada lovelace"),
        Pair::new("pass", "a&b=c"),
    ];
    form.auth = Auth::Basic {
        user: "ada".into(),
        password: "pw".into(),
    };
    let outgoing = prepare(&form, None).unwrap();
    assert_eq!(
        outgoing.body.as_deref(),
        Some(&b"user=ada+lovelace&pass=a%26b%3Dc"[..])
    );
    assert!(
        outgoing
            .headers
            .contains(&("Authorization".to_owned(), "Basic YWRhOnB3".to_owned()))
    );
    assert!(outgoing.headers.contains(&(
        "Content-Type".to_owned(),
        "application/x-www-form-urlencoded".to_owned()
    )));
    assert_eq!(
        prepare(&ApiRequest::new("e", Method::Get, "  "), None),
        Err(PrepareError::EmptyUrl)
    );
}

#[test]
fn addresses_without_a_scheme_pick_one() {
    assert_eq!(absolute_url("localhost:3000/a"), "http://localhost:3000/a");
    assert_eq!(absolute_url("127.0.0.1"), "http://127.0.0.1");
    assert_eq!(absolute_url("app.test/x"), "http://app.test/x");
    assert_eq!(absolute_url("api.example.com"), "https://api.example.com");
    assert_eq!(absolute_url("http://x"), "http://x");
}

#[test]
fn an_agents_draft_becomes_a_request() {
    let draft = diri_proto::ApiRequestDraft {
        method: "POST".into(),
        url: "{{base}}/items?debug=1".into(),
        headers: vec![diri_proto::ApiHeaderDraft {
            name: "Content-Type".into(),
            value: "application/json".into(),
        }],
        body: Some("{\"a\":1}".into()),
        name: Some("Create".into()),
        environment: None,
        auto_send: true,
    };
    let request = from_draft(&draft);
    assert_eq!(request.method, Method::Post);
    assert_eq!(request.name, "Create");
    assert_eq!(request.body_kind, BodyKind::Json);
    assert_eq!(request.params, vec![Pair::new("debug", "1")]);
    let mut form = draft.clone();
    form.headers[0].value = "application/x-www-form-urlencoded".into();
    form.body = Some("a=1&b=two+words".into());
    let request = from_draft(&form);
    assert_eq!(request.body_kind, BodyKind::Form);
    assert_eq!(
        request.form,
        vec![Pair::new("a", "1"), Pair::new("b", "two words")]
    );
}

// MARK: JSON

#[test]
fn json_is_indented_in_the_order_it_came() {
    let pretty = PrettyJson::parse(r#"{"z":1,"a":[true,null,"s"],"e":{},"n":-1.50e3}"#).unwrap();
    assert_eq!(
        pretty.text(),
        "{\n  \"z\": 1,\n  \"a\": [\n    true,\n    null,\n    \"s\"\n  ],\n  \"e\": {},\n  \"n\": -1.50e3\n}"
    );
    let lines = &pretty.lines;
    assert_eq!(lines[0].fold_end, Some(9));
    assert_eq!(
        lines[2].fold_end,
        Some(6),
        "the array folds to its closing line"
    );
    assert_eq!(lines[7].fold_end, None, "an empty object does not fold");
    assert_eq!(lines[1].spans[0].1, Token::Key);
    assert_eq!(lines[1].spans.last().unwrap().1, Token::Punctuation);
    assert!(
        lines[3]
            .spans
            .iter()
            .any(|(_, token)| *token == Token::Literal)
    );
    assert!(
        lines[5]
            .spans
            .iter()
            .any(|(_, token)| *token == Token::String)
    );
    assert!(
        lines[8]
            .spans
            .iter()
            .any(|(_, token)| *token == Token::Number)
    );
    assert!(PrettyJson::parse("not json").is_none());
    assert!(PrettyJson::parse("").is_none());
    assert_eq!(
        PrettyJson::parse("\"x\\\"y\"").unwrap().text(),
        "\"x\\\"y\""
    );
}

#[test]
fn folding_hides_a_block_and_keeps_its_closing_tail() {
    let pretty = PrettyJson::parse(r#"{"a":{"b":[1,2]},"c":3}"#).unwrap();
    let all: Vec<usize> = (0..pretty.lines.len()).collect();
    assert_eq!(pretty.visible(&HashSet::new()), all);
    let a = 1;
    assert!(pretty.lines[a].text.contains("\"a\""));
    let folded = HashSet::from([a]);
    let visible = pretty.visible(&folded);
    assert_eq!(visible, vec![0, 1, 7, 8]);
    assert_eq!(pretty.folded_tail(a).as_deref(), Some("},"));
    assert_eq!(pretty.folds_at_depth(1), HashSet::from([1, 2]));
    // Folding the root shows one line.
    assert_eq!(pretty.visible(&HashSet::from([0])), vec![0]);
    assert_eq!(
        json::format_body("{\"b\":1,\"a\":2}").unwrap(),
        "{\n  \"b\": 1,\n  \"a\": 2\n}"
    );
}

// MARK: Collections and storage

#[test]
fn collection_tree_ops_and_rows() {
    let mut items = vec![
        Saved::Folder {
            id: "orders".into(),
            name: "Orders".into(),
            items: vec![Saved::Request {
                request: ApiRequest::new("list", Method::Get, "https://x/orders"),
            }],
        },
        Saved::Request {
            request: ApiRequest::new("me", Method::Get, "https://x/me"),
        },
    ];
    let rows = tree_rows(&items, &HashSet::new());
    assert_eq!(rows.len(), 2, "a closed folder hides its requests");
    assert_eq!(rows[0].count, 1);
    let rows = tree_rows(&items, &HashSet::from(["orders".to_owned()]));
    assert_eq!(
        rows.iter().map(|row| row.depth).collect::<Vec<_>>(),
        vec![0, 1, 0]
    );
    assert_eq!(rows[1].method, Some(Method::Get));

    insert_into(
        &mut items,
        Some("orders"),
        Saved::Request {
            request: ApiRequest::new("create", Method::Post, "https://x/orders"),
        },
    );
    insert_into(
        &mut items,
        Some("gone"),
        Saved::Request {
            request: ApiRequest::new("loose", Method::Get, "https://x/loose"),
        },
    );
    assert_eq!(find_request(&items, "create").unwrap().method, Method::Post);
    assert_eq!(items.len(), 3, "a missing folder puts it at the root");
    let mut renamed = ApiRequest::new("create", Method::Put, "https://x/orders/1");
    renamed.name = "Update".into();
    assert!(replace_request(&mut items, &renamed));
    assert_eq!(find_request(&items, "create").unwrap().name, "Update");
    assert!(rename_folder(&mut items, "orders", "All orders"));
    assert!(remove_item(&mut items, "list").is_some());
    assert!(find_request(&items, "list").is_none());
}

#[test]
fn history_moves_repeats_up_and_is_bounded() {
    let mut history = Vec::new();
    for index in 0..60 {
        record_history(
            &mut history,
            HistoryEntry {
                request: ApiRequest::new(
                    format!("r{index}"),
                    Method::Get,
                    format!("http://x/{index}"),
                ),
                status: Some(200),
                took_ms: 1,
                at_ms: index,
            },
        );
    }
    assert_eq!(history.len(), HISTORY_LIMIT);
    record_history(
        &mut history,
        HistoryEntry {
            request: ApiRequest::new("again", Method::Get, "http://x/30"),
            status: Some(404),
            took_ms: 1,
            at_ms: 99,
        },
    );
    assert_eq!(history.len(), HISTORY_LIMIT);
    assert_eq!(history[0].status, Some(404));
    assert_eq!(
        history
            .iter()
            .filter(|entry| entry.request.url == "http://x/30")
            .count(),
        1
    );
}

#[cfg(unix)]
#[test]
fn the_store_round_trips_with_owner_only_permissions() {
    use super::storage::ApiProject;
    use std::os::unix::fs::PermissionsExt as _;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("api");
    let path = storage::project_file(&root, "project/with/../slashes");
    assert_eq!(
        path.parent(),
        Some(root.as_path()),
        "no path component from the id"
    );
    let mut project = ApiProject::default();
    project.collections.push(Saved::Folder {
        id: "f".into(),
        name: "Folder".into(),
        items: vec![Saved::Request {
            request: ApiRequest::new("r", Method::Delete, "http://x/?a=1"),
        }],
    });
    project
        .environments
        .push(environment(&[("token", "secret", true)]));
    project.active_environment = Some("env".into());
    storage::save(&path, &project).unwrap();
    let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    let dir_mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
    assert_eq!(file_mode, 0o600);
    assert_eq!(dir_mode, 0o700);
    let mut loaded = storage::load(&path).unwrap();
    assert_eq!(loaded.version, 1);
    loaded.version = 0;
    assert_eq!(loaded, project);
    // Saving again replaces the file without loosening it.
    storage::save(&path, &loaded).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // A loose folder is tightened.
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    storage::save(&path, &loaded).unwrap();
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        storage::load(&root.join("missing.json")).unwrap(),
        ApiProject::default()
    );
    // A symlinked store is refused rather than followed.
    let target = temp.path().join("elsewhere.json");
    std::fs::write(&target, "{}").unwrap();
    let link = root.join("link.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(storage::load(&link).is_err());
}

// MARK: HTTP

/// An HTTP/1.1 server on loopback: answers each connection with
/// `respond(request_text)` and reports what it read.
async fn serve(
    respond: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static,
) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let (seen, requests) = tokio::sync::mpsc::unbounded_channel();
    let respond = std::sync::Arc::new(respond);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let respond = respond.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read the head, then as much body as Content-Length says.
                loop {
                    let read = socket.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buffer).to_string();
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let length = text[..head_end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buffer.len() >= head_end + 4 + length {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&buffer).to_string();
                let _ = seen.send(text.clone());
                let _ = socket.write_all(&respond(&text)).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (address, requests)
}

fn reply(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn never_cancel() -> tokio::sync::oneshot::Receiver<()> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::mem::forget(sender);
    receiver
}

#[tokio::test]
async fn sends_headers_and_body_and_reads_the_answer() {
    let (address, mut requests) = serve(|_| {
        reply(
            "201 Created",
            &[
                ("Content-Type", "application/json"),
                ("X-Request-Id", "abc"),
            ],
            br#"{"ok":true}"#,
        )
    })
    .await;
    let mut outgoing = Outgoing::new(Method::Post, format!("{address}/items?q=a%20b"));
    outgoing.headers = vec![
        ("Authorization".into(), "Bearer \"quoted\" \\ token".into()),
        ("X-Empty".into(), String::new()),
    ];
    outgoing.body = Some(b"{\"name\":\"tea\"}".to_vec());
    let response = http::send(outgoing, never_cancel()).await.unwrap();
    assert_eq!(response.status, 201);
    assert_eq!(response.reason, "Created");
    assert_eq!(response.header("x-request-id"), Some("abc"));
    assert_eq!(response.body, br#"{"ok":true}"#);
    assert_eq!(response.size, 11);
    assert!(!response.truncated);
    let seen = requests.recv().await.unwrap();
    assert!(
        seen.starts_with("POST /items?q=a%20b HTTP/1.1\r\n"),
        "{seen}"
    );
    assert!(
        seen.contains("Authorization: Bearer \"quoted\" \\ token\r\n"),
        "{seen}"
    );
    assert!(
        seen.contains("X-Empty:"),
        "an empty header still goes out: {seen}"
    );
    assert!(seen.ends_with("{\"name\":\"tea\"}"), "{seen}");
}

#[tokio::test]
async fn follows_redirects_and_reports_the_last_hop() {
    let (address, _requests) = serve(|request| {
        if request.starts_with("GET /old") {
            reply("302 Found", &[("Location", "/new")], b"")
        } else {
            reply("200 OK", &[("X-Hop", "final")], b"landed")
        }
    })
    .await;
    let response = http::send(
        Outgoing::new(Method::Get, format!("{address}/old")),
        never_cancel(),
    )
    .await
    .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.redirects, 1);
    assert_eq!(response.header("x-hop"), Some("final"));
    assert!(
        response.header("location").is_none(),
        "only the final hop's headers"
    );
    assert!(response.final_url.ends_with("/new"));
    assert_eq!(response.body, b"landed");
}

#[tokio::test]
async fn a_large_body_is_cut_at_the_cap() {
    let (address, _requests) = serve(|_| reply("200 OK", &[], &vec![b'x'; 300_000])).await;
    let mut outgoing = Outgoing::new(Method::Get, address);
    outgoing.max_body = 1000;
    let response = http::send(outgoing, never_cancel()).await.unwrap();
    assert!(response.truncated);
    assert_eq!(response.body.len(), 1000);
}

#[tokio::test]
async fn head_requests_do_not_wait_for_a_body() {
    let (address, mut requests) =
        serve(|_| b"HTTP/1.1 200 OK\r\nContent-Length: 1234\r\nConnection: close\r\n\r\n".to_vec())
            .await;
    let mut outgoing = Outgoing::new(Method::Head, address);
    outgoing.timeout = Duration::from_secs(5);
    let response = http::send(outgoing, never_cancel()).await.unwrap();
    assert_eq!(response.status, 200);
    assert!(response.body.is_empty());
    assert!(
        requests
            .recv()
            .await
            .unwrap()
            .starts_with("HEAD / HTTP/1.1")
    );
}

#[tokio::test]
async fn failures_are_named_without_echoing_the_request() {
    // Bind and drop: nothing listens on the port any more.
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let refused = http::send(
        Outgoing::new(
            Method::Get,
            format!("http://127.0.0.1:{port}/?token=secret"),
        ),
        never_cancel(),
    )
    .await
    .unwrap_err();
    assert_eq!(refused, HttpError::Connect);
    assert!(!refused.message().contains("secret"));
    for (url, header) in [
        ("file:///etc/passwd", None),
        ("http://x/a b", None),
        ("http://x/\nInjected", None),
        ("http://x/", Some(("Bad Name", "1"))),
        ("http://x/", Some(("X", "a\r\nInjected: 1"))),
    ] {
        let mut outgoing = Outgoing::new(Method::Get, url);
        if let Some((name, value)) = header {
            outgoing.headers.push((name.into(), value.into()));
        }
        assert!(
            matches!(
                http::send(outgoing, never_cancel()).await,
                Err(HttpError::Invalid(_))
            ),
            "{url} {header:?}"
        );
    }
}

#[tokio::test]
async fn cancelling_or_timing_out_stops_a_hung_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    // Accept and never answer.
    let held = tokio::spawn(async move {
        let mut sockets = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            sockets.push(socket);
        }
    });
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(http::send(
        Outgoing::new(Method::Get, address.clone()),
        cancelled,
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    cancel.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("cancel returns promptly")
        .unwrap();
    assert_eq!(result, Err(HttpError::Cancelled));

    let mut outgoing = Outgoing::new(Method::Get, address);
    outgoing.timeout = Duration::from_secs(1);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        http::send(outgoing, never_cancel()),
    )
    .await
    .expect("curl's max-time bounds the wait");
    assert_eq!(result, Err(HttpError::Timeout));
    held.abort();
}

#[test]
fn header_dumps_keep_only_the_last_hop() {
    let dump = "HTTP/1.1 301 Moved\r\nLocation: /b\r\n\r\nHTTP/2 200\r\ncontent-type: text/plain\r\nx-a: 1: 2\r\n\r\n";
    let (version, status, reason, headers) = http::parse_headers(dump);
    assert_eq!(
        (version.as_str(), status, reason.as_str()),
        ("HTTP/2", 200, "")
    );
    assert_eq!(
        headers,
        vec![
            ("content-type".to_owned(), "text/plain".to_owned()),
            ("x-a".to_owned(), "1: 2".to_owned()),
        ]
    );
    assert_eq!(http::reason(404), "Not Found");
    assert_eq!(http::format_size(1536), "1.5 KB");
    assert_eq!(http::format_duration(1240), "1.24 s");
}

// MARK: The surface

/// A runtime that only makes progress inside `block_on`, so nothing wakes
/// the GPUI test scheduler from another thread.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn type_text(cx: &mut gpui::VisualTestContext, text: &str) {
    for character in text.chars() {
        let keystroke = gpui::Keystroke {
            modifiers: gpui::Modifiers::none(),
            key: character.to_string(),
            key_char: Some(character.to_string()),
        };
        cx.update(|window, cx| {
            window.dispatch_keystroke(keystroke, cx);
        });
    }
    cx.run_until_parked();
}

#[gpui::test]
fn the_header_table_edits_through_its_fields(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let tokio = runtime();
    let handle = tokio.handle().clone();
    let root = temp.path().to_path_buf();
    let (api, cx) = cx.add_window_view(|window, cx| {
        let library = library_for(&root, "project", cx);
        let api = ApiClient::new(handle, library, SemanticColors::dark(), cx);
        window.focus(&api.focus, cx);
        api
    });
    cx.simulate_resize(gpui::size(gpui::px(420.0), gpui::px(700.0)));
    cx.run_until_parked();

    api.update_in(cx, |api, window, cx| {
        api.request_tab = RequestTab::Headers;
        api.add_row(RequestTab::Headers, window, cx);
        assert_eq!(api.focused_field(), Some(Field::Header(0, Column::Name)));
    });
    type_text(cx, "X-Trace");
    cx.simulate_keystrokes("tab");
    type_text(cx, "abc");
    api.read_with(cx, |api, _| {
        assert_eq!(api.draft.headers, vec![Pair::new("X-Trace", "abc")]);
        assert_eq!(api.focused_field(), Some(Field::Header(0, Column::Value)));
        assert!(!api.is_pristine(), "an edit makes the draft the person's");
    });
    cx.simulate_keystrokes("backspace");
    api.update_in(cx, |api, window, cx| {
        assert_eq!(api.draft.headers[0].value, "ab");
        api.toggle_row(RequestTab::Headers, 0, cx);
        assert!(!api.draft.headers[0].enabled);
        api.add_row(RequestTab::Headers, window, cx);
        api.remove_row(RequestTab::Headers, 0, cx);
        assert_eq!(api.draft.headers, vec![Pair::new("", "")]);
    });

    // The Params table and the URL stay in step as either is typed.
    api.update_in(cx, |api, window, cx| {
        api.request_tab = RequestTab::Params;
        api.focus_field(Field::Url, window, cx);
    });
    type_text(cx, "localhost:3000/items?page=2");
    api.update_in(cx, |api, window, cx| {
        assert_eq!(api.draft.params, vec![Pair::new("page", "2")]);
        api.focus_field(Field::Param(0, Column::Value), window, cx);
    });
    cx.simulate_keystrokes("backspace");
    type_text(cx, "3 4");
    api.read_with(cx, |api, _| {
        assert_eq!(api.draft.url, "localhost:3000/items?page=3%204");
    });
    // Escape leaves the field; Enter in the URL would send.
    cx.simulate_keystrokes("escape");
    api.read_with(cx, |api, _| assert_eq!(api.focused_field(), None));
}

#[gpui::test]
fn saving_puts_the_request_in_the_project_store(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let tokio = runtime();
    let handle = tokio.handle().clone();
    let root = temp.path().to_path_buf();
    let api = cx.update(|cx| {
        let library = library_for(&root, "project", cx);
        cx.new(|cx| ApiClient::new(handle, library, SemanticColors::dark(), cx))
    });
    api.update(cx, |api, cx| {
        api.load_request(
            ApiRequest::new("saved", Method::Patch, "http://localhost:3000/me"),
            cx,
        );
        api.save_to_collection(cx);
        assert!(api.is_saved(cx));
        api.library
            .update(cx, |library, _| library.save_now().unwrap());
    });
    let stored = storage::load(&storage::project_file(&root, "project")).unwrap();
    let request = find_request(&stored.collections, "saved").unwrap();
    assert_eq!(request.method, Method::Patch);
    assert_eq!(
        request.name, "localhost:3000/me",
        "an unnamed request takes its path"
    );
    // A second tab on the same project shares the library.
    let shared = cx.update(|cx| library_for(&root, "project", cx));
    api.read_with(cx, |api, _| assert_eq!(api.library, shared));
}

#[gpui::test]
fn agent_drafts_send_only_a_get_by_themselves(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let tokio = runtime();
    let handle = tokio.handle().clone();
    let root = temp.path().to_path_buf();
    let api = cx.update(|cx| {
        let library = library_for(&root, "project", cx);
        cx.new(|cx| ApiClient::new(handle, library, SemanticColors::dark(), cx))
    });
    let mut draft = diri_proto::ApiRequestDraft {
        method: "POST".into(),
        url: "http://127.0.0.1:9/items".into(),
        auto_send: true,
        environment: Some(diri_proto::ApiEnvironmentDraft {
            name: None,
            variables: vec![diri_proto::ApiVariableDraft {
                name: "token".into(),
                value: "sk".into(),
                secret: true,
            }],
        }),
        ..Default::default()
    };
    api.update(cx, |api, cx| {
        api.open_draft(&draft, cx);
        assert!(!api.is_sending(), "a POST waits for Send");
        assert_eq!(api.draft.method, Method::Post);
        assert!(api.is_pristine());
        let library = api.library.read(cx);
        let environment = library
            .active_environment()
            .expect("variables made an environment");
        assert_eq!(environment.name, "Local");
        assert!(environment.lookup("token").unwrap().secret);
    });
    draft.method = "GET".into();
    api.update(cx, |api, cx| {
        api.open_draft(&draft, cx);
        assert!(api.is_sending(), "a GET marked autoSend goes out");
        assert!(!api.is_pristine());
        api.cancel(cx);
        assert!(!api.is_sending());
    });
}

#[gpui::test]
fn a_sent_request_shows_its_response_and_lands_in_history(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let tokio = runtime();
    let (address, _requests) = tokio.block_on(serve(|_| {
        reply(
            "200 OK",
            &[("Content-Type", "application/json")],
            br#"{"items":[1,2]}"#,
        )
    }));
    let handle = tokio.handle().clone();
    let root = temp.path().to_path_buf();
    let api = cx.update(|cx| {
        let library = library_for(&root, "project", cx);
        cx.new(|cx| ApiClient::new(handle, library, SemanticColors::dark(), cx))
    });
    // The GPUI test scheduler refuses wakeups from tokio's threads, so the
    // transfer runs here and its result is handed to the surface the way
    // `send`'s task does.
    let request = ApiRequest::new("r", Method::Get, format!("{address}/items"));
    let outgoing = prepare(&request, None).unwrap();
    let response = tokio.block_on(http::send(outgoing, never_cancel()));
    api.update(cx, |api, cx| {
        api.load_request(request.clone(), cx);
        let generation = api.send_generation;
        api.finish(generation, request, response, cx);
    });
    api.read_with(cx, |api, cx| {
        let Some(Outcome::Response(view)) = &api.outcome else {
            panic!("a response");
        };
        assert_eq!(view.response.status, 200);
        let pretty = view.pretty.as_ref().expect("JSON is laid out");
        assert_eq!(pretty.lines[0].fold_end, Some(5));
        let history = &api.library.read(cx).project.history;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, Some(200));
    });
}

// MARK: Screenshot

/// Paints the surface on a panel background, as the inspector does. Only the
/// macOS screenshot below hosts it.
#[cfg(target_os = "macos")]
struct Frame {
    api: gpui::Entity<ApiClient>,
    colors: SemanticColors,
}

#[cfg(target_os = "macos")]
impl gpui::Render for Frame {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _};
        gpui::div()
            .size_full()
            .bg(self.colors.sidebar_surface_settled())
            .font_family(crate::fonts::ui_family())
            .child(self.api.clone())
    }
}

/// Renders the API surface with a fixture request and response into
/// `DIRI_VISUAL_OUTPUT`, headless. `DIRI_VISUAL_LIGHT` paints it light;
/// `DIRI_VISUAL_API_MODE` = `saved`, `history` or `env` shows that view.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "writes the API surface screenshot"]
fn render_api_surface_screenshot() {
    let output = std::env::var_os("DIRI_VISUAL_OUTPUT")
        .map(std::path::PathBuf::from)
        .expect("output path");
    let colors = if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() {
        SemanticColors::light()
    } else {
        SemanticColors::dark()
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let tokio = runtime();
    let handle = tokio.handle().clone();
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        std::sync::Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        crate::fonts::init(cx);
        cx.set_reduce_motion(true);
    });
    let window = cx
        .open_window(gpui::size(gpui::px(440.0), gpui::px(720.0)), |_, cx| {
            let library = library_for(&root, "fixture", cx);
            library.update(cx, |library, _| {
                library.project.environments.push(Environment {
                    id: "local".into(),
                    name: "Local".into(),
                    variables: vec![
                        Variable::new("baseUrl", "http://localhost:3000", false),
                        Variable::new("token", "sk_test_4f9a2c", true),
                        Variable::new("orgId", "acme", false),
                    ],
                });
                library.project.active_environment = Some("local".into());
                let mut list = ApiRequest::new("list", Method::Get, "{{baseUrl}}/v1/orders?status=open");
                list.name = "List orders".into();
                let mut create = ApiRequest::new("create", Method::Post, "{{baseUrl}}/v1/orders");
                create.name = "Create an order".into();
                let mut cancel = ApiRequest::new("cancel", Method::Delete, "{{baseUrl}}/v1/orders/1009");
                cancel.name = "Cancel an order".into();
                let mut me = ApiRequest::new("me", Method::Get, "{{baseUrl}}/v1/me");
                me.name = "Who am I".into();
                library.project.collections = vec![
                    Saved::Folder {
                        id: "orders".into(),
                        name: "Orders".into(),
                        items: vec![
                            Saved::Request { request: list.clone() },
                            Saved::Request { request: create },
                            Saved::Request { request: cancel },
                        ],
                    },
                    Saved::Folder {
                        id: "users".into(),
                        name: "Users".into(),
                        items: vec![Saved::Request { request: me }],
                    },
                ];
                for (index, (method, url, status)) in [
                    (Method::Get, "{{baseUrl}}/v1/orders?status=open&limit=20", Some(200)),
                    (Method::Post, "{{baseUrl}}/v1/orders", Some(201)),
                    (Method::Delete, "{{baseUrl}}/v1/orders/1009", Some(404)),
                    (Method::Get, "{{baseUrl}}/v1/me", None),
                ]
                .into_iter()
                .enumerate()
                {
                    library.project.history.push(HistoryEntry {
                        request: ApiRequest::new(format!("h{index}"), method, url),
                        status,
                        took_ms: 120,
                        at_ms: now_ms() - (index as u64 + 1) * 90_000,
                    });
                }
            });
            let api = cx.new(|cx| {
                let mut api = ApiClient::new(handle, library, colors, cx);
                let mut request = ApiRequest::new(
                    "list",
                    Method::Get,
                    "{{baseUrl}}/v1/orders?status=open&limit=20",
                );
                request.name = "List orders".into();
                request.headers = vec![
                    Pair::new("Accept", "application/json"),
                    Pair::new("Authorization", "Bearer {{token}}"),
                    Pair::new("X-Org", "{{orgId}}"),
                ];
                api.load_request(request, cx);
                api.request_tab = RequestTab::Headers;
                api.tree_open.insert("orders".into());
                api.mode = match std::env::var("DIRI_VISUAL_API_MODE").as_deref() {
                    Ok("saved") => Mode::Collections,
                    Ok("history") => Mode::History,
                    Ok("env") => Mode::Environments,
                    _ => Mode::Request,
                };
                api.outcome = Some(Outcome::Response(ResponseView::new(http::ApiResponse {
                    status: 200,
                    version: "HTTP/1.1".into(),
                    reason: "OK".into(),
                    headers: vec![
                        ("content-type".into(), "application/json".into()),
                        ("cache-control".into(), "no-store".into()),
                        ("x-request-id".into(), "a1f3-77c2".into()),
                    ],
                    body: br#"{"orders":[{"id":1009,"total":89.0,"status":"open","items":[{"sku":"tea","quantity":2}]},{"id":1010,"total":26.5,"status":"open","items":[]}],"next":null,"count":2,"live":true}"#.to_vec(),
                    truncated: false,
                    took_ms: 142,
                    size: 1380,
                    redirects: 0,
                    final_url: "http://localhost:3000/v1/orders?status=open&limit=20".into(),
                })));
                api.folded.insert(13);
                api
            });
            cx.new(|_| Frame { api, colors })
        })
        .expect("headless window");
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .unwrap();
    cx.run_until_parked();
    cx.capture_screenshot(window.into())
        .expect("screenshot")
        .save(output)
        .expect("save");
}
