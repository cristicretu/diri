//! A note Session's Markdown, read and written for the phone.
//!
//! A note has no process, so none of the terminal routes apply to it. Its
//! content is a file in the notes store, the same store the app's editor,
//! the CLI and agents' MCP tools share. The phone edits the title and body;
//! the front matter (project, pin, Session stamp) is never on the wire and is
//! kept exactly as the file has it.
//!
//! Writes are compare-and-swap: the phone sends back the `version` it loaded,
//! and a file changed since (on the Mac, by an agent) answers `409` with the
//! current text instead of being overwritten.

use diri_notes::backlinks;
use diri_notes::doc::Document;
use diri_notes::markdown::{self, FrontMatter};
use diri_notes::store::{self, MAX_NOTE_BYTES, Note, NoteStore, SaveOutcome};
use diri_proto::SessionRecord;
use serde_json::{Value, json};

use crate::api::{Api, daemon_error, session_id};
use crate::http::{Request, Response};

impl Api {
    fn note_store(&self) -> Result<NoteStore, Response> {
        let dir = self
            .notes_dir
            .clone()
            .or_else(NoteStore::resolve_dir)
            .ok_or_else(|| Response::error(500, "no notes directory on this Mac"))?;
        NoteStore::open(dir).map_err(|error| Response::error(500, &error.to_string()))
    }

    /// The note Session `id` and its file id.
    async fn note_session(&self, id: &str) -> Result<(SessionRecord, String), Response> {
        let sessions = self
            .client
            .sessions()
            .await
            .map_err(|error| daemon_error(&error))?;
        let wanted = session_id(id);
        let record = sessions
            .sessions
            .into_iter()
            .find(|session| session.id == wanted)
            .ok_or_else(|| Response::error(404, "no such session"))?;
        if !record.is_note() {
            return Err(Response::error(400, "not a note"));
        }
        let note_id = record
            .note_id
            .clone()
            .filter(|note| store::is_valid_id(note))
            .ok_or_else(|| Response::error(404, "this note has no file"))?;
        Ok((record, note_id))
    }

    pub(crate) async fn read_note(&self, id: &str) -> Response {
        let (_, note_id) = match self.note_session(id).await {
            Ok(found) => found,
            Err(response) => return response,
        };
        let store = match self.note_store() {
            Ok(store) => store,
            Err(response) => return response,
        };
        match read_source(&store, &note_id) {
            Ok(source) => Response::json(&note_json(&note_id, &source)),
            Err(response) => response,
        }
    }

    pub(crate) async fn write_note(&self, id: &str, request: &Request) -> Response {
        let body = request.json();
        let Some(text) = body.get("markdown").and_then(Value::as_str) else {
            return Response::error(400, "missing markdown");
        };
        let Some(version) = body.get("version").and_then(Value::as_str) else {
            return Response::error(400, "missing version");
        };
        let (record, note_id) = match self.note_session(id).await {
            Ok(found) => found,
            Err(response) => return response,
        };
        let store = match self.note_store() {
            Ok(store) => store,
            Err(response) => return response,
        };
        let current = match read_source(&store, &note_id) {
            Ok(source) => source,
            Err(response) => return response,
        };
        if version_of(&current) != version {
            return conflict(&note_id, &current);
        }
        // `[[Title]]` typed on the phone links that note, as it does when an
        // agent writes it; a title no single note matches stays as written.
        //
        // The phone autosaves every pause in typing, so only a `[[…]]` this
        // save adds is looked up, and the store is listed only for one: an
        // unresolved link already in the note was tried when it was typed.
        let linked = if text.contains("[[") {
            let saved = editable_markdown(&current);
            let before = wiki_names(&saved);
            let notes = std::cell::OnceCell::new();
            backlinks::rewrite_wiki_links(text, |name| {
                if before.contains(name) {
                    return None;
                }
                let notes = notes.get_or_init(|| store.list().unwrap_or_default());
                backlinks::wiki_target(notes, name)
            })
            .text
        } else {
            text.to_owned()
        };
        let front = store::parse_note(&current).front;
        let note = Note {
            front,
            doc: parse_editable(&linked),
        };
        let title = note.doc.title.trim().to_owned();
        match store.save_if_unchanged(&note_id, &note, &current) {
            Ok(SaveOutcome::Saved { source }) => {
                // The sidebar row is the Session's title; keep it the note's,
                // as the Mac editor does.
                let shown = if title.is_empty() { "Untitled" } else { &title };
                if record.title != shown {
                    let _ = self.client.rename(&record.id, shown.to_owned()).await;
                }
                Response::json(&json!({ "version": version_of(&source), "title": title }))
            }
            Ok(SaveOutcome::Conflict { current }) => match current {
                Some(current) => conflict(&note_id, &current),
                None => Response::error(404, "the note was deleted"),
            },
            Err(error) => Response::error(500, &error.to_string()),
        }
    }
}

/// The names inside every `[[…]]` in `markdown`.
fn wiki_names(markdown: &str) -> std::collections::HashSet<&str> {
    let mut names = std::collections::HashSet::new();
    let mut rest = markdown;
    while let Some(open) = rest.find("[[") {
        rest = &rest[open + 2..];
        let Some(close) = rest.find("]]") else { break };
        names.insert(&rest[..close]);
        rest = &rest[close + 2..];
    }
    names
}

fn read_source(store: &NoteStore, note_id: &str) -> Result<String, Response> {
    let path = store
        .path_for(note_id)
        .map_err(|error| Response::error(400, &error.to_string()))?;
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.len() > MAX_NOTE_BYTES => {
            return Err(Response::error(413, "note is too large to open"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Response::error(404, "the note was deleted"));
        }
        Err(error) => return Err(Response::error(500, &error.to_string())),
    }
    std::fs::read_to_string(&path).map_err(|error| Response::error(500, &error.to_string()))
}

/// What the phone edits: the title as a leading `# ` line, then the body.
/// Front matter is the store's business and never reaches the phone.
pub(crate) fn editable_markdown(source: &str) -> String {
    let note = store::parse_note(source);
    markdown::write(&FrontMatter::default(), &note.doc)
}

/// The phone's text as a document. A leading newline keeps text that starts
/// with `---` a thematic break rather than front matter it never had.
pub(crate) fn parse_editable(text: &str) -> Document {
    markdown::parse(&format!("\n{text}")).1
}

/// A stable fingerprint of a note file (FNV-1a). Not a security boundary:
/// it only tells a phone whether the file moved since it loaded it.
pub(crate) fn version_of(source: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in source.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn note_json(note_id: &str, source: &str) -> Value {
    let note = store::parse_note(source);
    json!({
        "noteId": note_id,
        "title": note.doc.title,
        "markdown": editable_markdown(source),
        "version": version_of(source),
    })
}

fn conflict(note_id: &str, current: &str) -> Response {
    let mut body = note_json(note_id, current);
    body["error"] = json!("the note changed on your Mac");
    let mut response = Response::json(&body);
    response.status = 409;
    response
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use diri_proto::{AgentKind, Method, Resumability, SessionStatus, TitleSource};
    use serde_json::{Value, json};

    use super::*;
    use crate::auth::Auth;

    #[test]
    fn only_links_a_save_adds_are_looked_up() {
        let names = wiki_names("see [[A]] and [[B c]], not [[open");
        assert_eq!(names, ["A", "B c"].into_iter().collect());
        assert!(wiki_names("no links").is_empty());
    }

    fn record(id: &str, kind: &AgentKind, note_id: Option<&str>, title: &str) -> Value {
        let mut record = json!({
            "id": id,
            "kind": kind,
            "cwd": "/repo",
            "projectID": "p_repo",
            "title": title,
            "titleSource": TitleSource::DirijorAssigned,
            "status": SessionStatus::Idle,
            "resumability": Resumability::NotResumable,
            "createdAt": 1.0,
            "updatedAt": 1.0,
            "pinned": false,
        });
        if let Some(note_id) = note_id {
            record["noteId"] = json!(note_id);
        }
        record
    }

    async fn call(api: &Api, method: &str, path: &str, body: &str) -> (u16, Value) {
        use tokio::io::BufReader;
        let raw = format!(
            "{method} {path} HTTP/1.1\r\nHost: test\r\nX-Diri-Token: {}\r\nContent-Length: {}\r\n\r\n{body}",
            api.auth.token(),
            body.len()
        );
        let request = crate::http::read_request(&mut BufReader::new(raw.as_bytes()))
            .await
            .unwrap()
            .unwrap();
        let response = api.route(&request).await;
        (
            response.status,
            serde_json::from_slice(&response.body).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn a_note_round_trips_and_refuses_a_stale_write() {
        let notes = tempfile::tempdir_in("/tmp").unwrap();
        let store = NoteStore::open(notes.path()).unwrap();
        let doc = parse_editable("# Plan\n\n- [ ] ship it");
        let (note_id, _) = store
            .create_for_session(
                doc,
                Some("/repo"),
                Some("s_note"),
                &diri_notes::history::Author::User,
            )
            .unwrap();
        let listed = json!({
            "sessions": [
                record("s_note", &AgentKind::NOTE, Some(&note_id), "Plan"),
                record("s_shell", &AgentKind::SHELL, None, "zsh"),
            ],
            "projects": [],
        });
        let mut engine = crate::testing::fake_engine(move |method, _| match method {
            Method::SESSION_LIST => listed.clone(),
            _ => json!({}),
        })
        .await;
        let api = Api {
            client: Arc::clone(&engine.client),
            auth: Auth::temporary().unwrap(),
            host_label: "Mac".into(),
            inputs: Default::default(),
            desktop: None,
            notes_dir: Some(notes.path().to_owned()),
            tailnet: None,
        };

        let (status, loaded) = call(&api, "GET", "/api/session/s_note/note", "").await;
        assert_eq!(status, 200, "{loaded}");
        assert_eq!(loaded["title"], "Plan");
        assert_eq!(loaded["markdown"], "# Plan\n\n- [ ] ship it\n");
        let version = loaded["version"].as_str().unwrap().to_owned();

        // A terminal is not a note, and an unknown session is not found.
        assert_eq!(
            call(&api, "GET", "/api/session/s_shell/note", "").await.0,
            400
        );
        assert_eq!(
            call(&api, "GET", "/api/session/s_gone/note", "").await.0,
            404
        );

        let edit = json!({"markdown": "# Launch\n\n- [x] ship it", "version": version});
        let (status, saved) =
            call(&api, "PUT", "/api/session/s_note/note", &edit.to_string()).await;
        assert_eq!(status, 200, "{saved}");
        assert_eq!(saved["title"], "Launch");
        assert_ne!(saved["version"], json!(version));

        // Front matter the phone never saw is kept; the body is the phone's.
        let note = store.load(&note_id).unwrap();
        assert_eq!(note.doc.title, "Launch");
        assert_eq!(note.project(), Some("/repo"));
        assert_eq!(note.front.get(store::KEY_SESSION), Some("s_note"));

        // The new title reaches the sidebar row.
        let mut renamed = false;
        while let Ok(Some((method, params))) =
            tokio::time::timeout(std::time::Duration::from_secs(1), engine.calls.recv()).await
        {
            if method == Method::SESSION_RENAME {
                assert_eq!(params.unwrap()["title"], "Launch");
                renamed = true;
                break;
            }
        }
        assert!(renamed, "the Session was renamed after its note");

        // The first version is stale now: the write is refused with the
        // file as it stands, and the file is untouched.
        let stale = json!({"markdown": "# Mine", "version": version});
        let (status, conflict) =
            call(&api, "PUT", "/api/session/s_note/note", &stale.to_string()).await;
        assert_eq!(status, 409);
        assert_eq!(conflict["title"], "Launch");
        assert_eq!(conflict["version"], saved["version"]);
        assert_eq!(store.load(&note_id).unwrap().doc.title, "Launch");

        // `[[Title]]` typed on the phone becomes a link to that note.
        let (other, _) = store
            .create(Document::new("Release notes", Vec::new()), None)
            .unwrap();
        let linking =
            json!({"markdown": "# Launch\n\nsee [[Release notes]]", "version": saved["version"]});
        let (status, _) = call(
            &api,
            "PUT",
            "/api/session/s_note/note",
            &linking.to_string(),
        )
        .await;
        assert_eq!(status, 200);
        let (_, reread) = call(&api, "GET", "/api/session/s_note/note", "").await;
        assert!(
            reread["markdown"]
                .as_str()
                .unwrap()
                .contains(&format!("diri://note/{other}")),
            "{reread}"
        );

        assert_eq!(
            call(
                &api,
                "PUT",
                "/api/session/s_note/note",
                r#"{"markdown":"x"}"#
            )
            .await
            .0,
            400
        );
    }

    #[test]
    fn leading_rule_is_not_front_matter() {
        let doc = parse_editable("---\nkey: value\n---\nbody");
        assert!(doc.title.is_empty());
        assert!(doc.plain_text().contains("key: value"));
    }
}
