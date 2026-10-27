//! Diri Notes for agents: discover, read, and add to the user's notes.
//!
//! Notes live outside project repos, in Diri's app-support directory, so
//! agents find them here rather than on disk: the notes for their project,
//! the notes that mention them, and the note they were started from. A note
//! in the sidebar is a Session of kind `note` whose `note_id` names its file;
//! "the note I was started from" is the nearest such Session among the
//! caller's ancestors. Reads need no Engine; anything about sessions (note
//! sessions, who is mentioned, their status, write authorization) comes from
//! `session.list`.

use super::*;
use diri_notes::backlinks::{self, LinkIndex, Reference, WikiRewrite};
use diri_notes::handoff::{self, TodoSelector};
use diri_notes::history::Author;
use diri_notes::mention::{self, MentionTarget};
use diri_notes::store::{self as note_store, Note, NoteMeta, NoteStore, Resolve};
use diri_proto::ProjectId;

const DEFAULT_NOTE_LIMIT: u64 = 50;
const MAX_NOTE_LIMIT: u64 = 200;
const MAX_APPEND_BYTES: usize = 64 * 1024;
const ORIGIN: &str = "origin";
const MAX_ENTRY_CHARS: usize = 500;
const MAX_HISTORY_ROWS: usize = 50;
const NOTE_SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
/// Backlinks read_note lists before it says how many more there are.
const READ_BACKLINKS: usize = 20;
/// Links and mentions note_links lists of each kind.
const MAX_LINK_ROWS: usize = 100;
const DEFAULT_UNLINKED: usize = 20;

/// What happened when the CLI asked the Engine to create a note Session.
#[derive(Debug)]
pub enum NoteSpawn {
    Created(Box<SessionRecord>),
    /// No Engine to ask, or one that predates note Sessions: the caller
    /// writes the file directly and the note has no Session yet.
    Unavailable(String),
}

impl Bridge {
    /// Creates a note Session (and its file) through `session.spawn`, so the
    /// note appears in the sidebar under `parent`. `cwd` is the project root.
    /// Only an unreachable or too-old Engine yields `Unavailable`; an Engine
    /// that rejects the request is an error, never a silent fallback.
    pub fn spawn_note(
        &self,
        cwd: &str,
        title: &str,
        body: &str,
        parent: Option<&str>,
    ) -> Result<NoteSpawn, String> {
        let params = SessionSpawnParams {
            appearance: None,
            account_profile_id: None,
            kind: AgentKind::NOTE,
            cwd: cwd.to_owned(),
            new_worktree: None,
            worktree_branch: None,
            worktree_base: None,
            title: Some(title.to_owned()).filter(|t| !t.trim().is_empty()),
            initial_prompt: Some(body.to_owned()).filter(|b| !b.trim().is_empty()),
            parent: parent.map(SessionId::new),
            initial_cols: None,
            initial_rows: None,
            host: None,
            same_repo_as: None,
            start_directory: None,
            note_id: None,
            argv: None,
        };
        let params = serde_json::to_value(params).map_err(|e| e.to_string())?;
        let mut client = match self.connect(NOTE_SPAWN_TIMEOUT) {
            Ok(client) => client,
            // The technical cause helps no one reading a note tool's reply.
            Err(_) => return Ok(NoteSpawn::Unavailable("Diri isn't running".into())),
        };
        let deadline = Instant::now() + NOTE_SPAWN_TIMEOUT;
        match client.request_until(
            Method::SESSION_SPAWN.into(),
            self.stamp_actor(Method::SESSION_SPAWN, params),
            deadline,
        ) {
            Ok(value) => {
                let record: SessionRecord = serde_json::from_value(value)
                    .map_err(|e| format!("invalid session.spawn response: {e}"))?;
                if !record.is_note() || record.note_id.is_none() {
                    return Err(format!(
                        "the Engine created session {} but not a note",
                        record.id.0
                    ));
                }
                Ok(NoteSpawn::Created(Box::new(record)))
            }
            Err(ControlFailure::Daemon(error))
                if error.message.contains("no manifest for agent") =>
            {
                Ok(NoteSpawn::Unavailable(
                    "the running Diri Engine predates note sessions".into(),
                ))
            }
            Err(failure) => Err(render_failure(failure)),
        }
    }

    pub(super) fn list_notes(&self, args: &Value) -> Result<Value, String> {
        let store = self.note_store()?;
        let notes = store
            .list()
            .map_err(|e| format!("cannot list notes: {e}"))?;
        let include_archived = optional_bool(args, "include_archived").unwrap_or(false);
        let query = optional_string(args, "query").map(|q| q.to_lowercase());
        let limit = args["limit"]
            .as_u64()
            .unwrap_or(DEFAULT_NOTE_LIMIT)
            .clamp(1, MAX_NOTE_LIMIT) as usize;
        let project = optional_string(args, "project")
            .unwrap_or_else(|| if self.caller.is_some() { "mine" } else { "all" }.to_owned());
        let mentions = optional_string(args, "mentions");
        // Notes linking to one note: "search by link".
        let linking: Option<(String, Vec<String>)> = match optional_string(args, "links_to") {
            None => None,
            Some(query) => {
                let target = self.resolve_note(&store, &query, self.sessions().as_deref().ok())?;
                let index =
                    LinkIndex::read(&store).map_err(|e| format!("cannot read notes: {e}"))?;
                let sources = index
                    .linking_notes(&target.id)
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                Some((target.id, sources))
            }
        };

        // Session facts are optional unless the filter itself needs them.
        let snapshot = self.snapshot();
        let needed = || {
            snapshot
                .as_ref()
                .map_err(|error| format!("this filter needs the Diri Engine: {error}"))
        };
        let (project_root, project_id) = match project.as_str() {
            "all" => (None, None),
            "mine" => {
                let (root, id) = self.caller_project(needed()?)?;
                (Some(root), Some(id))
            }
            path => (Some(path.to_owned()), None),
        };
        let sessions: &[SessionRecord] = snapshot.as_ref().map_or(&[], |s| &s.sessions);
        let by_note = note_sessions(sessions);
        // Session ids that count as "mentioned", each with how it relates to
        // the subject: the subject itself, or one of its ancestors.
        let mentioned: Option<HashMap<String, &'static str>> = match mentions.as_deref() {
            None => None,
            Some(subject) => {
                let subject = if subject == "me" {
                    self.require_caller()?.to_owned()
                } else {
                    subject.to_owned()
                };
                let lineage = Lineage::new(&needed()?.sessions, Some(&subject));
                let mut ids = HashMap::from([(subject.clone(), "self")]);
                for ancestor in lineage.ancestors(&subject) {
                    ids.entry(ancestor.id.0.clone()).or_insert("ancestor");
                }
                Some(ids)
            }
        };

        let mut rows = Vec::new();
        let mut total = 0;
        for note in &notes {
            let session = by_note.get(note.id.as_str()).copied();
            if (note.archived || session.is_some_and(SessionRecord::is_archived))
                && !include_archived
            {
                continue;
            }
            if let Some(root) = &project_root {
                let by_file = note
                    .project
                    .as_deref()
                    .is_some_and(|p| policy::same_path(p, root));
                let by_session = session
                    .zip(project_id.as_ref())
                    .is_some_and(|(record, id)| &record.project_id == id);
                if !by_file && !by_session {
                    continue;
                }
            }
            if let Some(query) = &query
                && !note.haystack.contains(query)
            {
                continue;
            }
            if let Some((_, sources)) = &linking
                && !sources.contains(&note.id)
            {
                continue;
            }
            let via = match &mentioned {
                None => None,
                Some(ids) => {
                    let via = note.mentions.iter().find_map(|target| match target {
                        MentionTarget::Session(id) => ids.get(id).copied(),
                        MentionTarget::Note(_) => None,
                    });
                    if via.is_none() {
                        continue;
                    }
                    via
                }
            };
            total += 1;
            if rows.len() < limit {
                let mut row = note_row(note);
                if let Some(record) = session {
                    row["session_id"] = json!(record.id.0);
                }
                if let Some(via) = via {
                    row["mentioned_via"] = json!(via);
                }
                rows.push(row);
            }
        }
        let mut result = json!({
            "notes": rows,
            "total": total,
            "truncated": total > rows.len(),
            "project": project_root,
            "notes_dir": store.dir(),
        });
        if let Some((target, _)) = linking {
            result["links_to"] = json!(target);
        }
        Ok(result)
    }

    pub(super) fn read_note(&self, args: &Value) -> Result<Value, String> {
        let store = self.note_store()?;
        // Notes are readable without Diri running; session facts are extra.
        let sessions = self.sessions();
        let meta = self.resolve_note(
            &store,
            &required_string(args, "note")?,
            sessions.as_deref().ok(),
        )?;
        let note = store
            .load(&meta.id)
            .map_err(|e| format!("cannot read note {}: {e}", meta.id))?;
        let find = |id: &str| {
            sessions
                .as_ref()
                .ok()
                .and_then(|all| all.iter().find(|record| record.id.0 == id))
        };
        let session_value = |id: &str| match (find(id), sessions.is_ok()) {
            (Some(record), _) => session_fact(record),
            (None, true) => json!({"id": id, "missing": true}),
            // Without the Engine a session is unknown, not missing.
            (None, false) => json!({"id": id}),
        };

        let todos: Vec<Value> = handoff::todos(&note)
            .into_iter()
            .map(|todo| {
                json!({
                    "index": todo.index,
                    "text": todo.text,
                    "checked": todo.checked,
                    "sessions": todo.sessions.iter().map(|id| session_value(id)).collect::<Vec<_>>(),
                })
            })
            .collect();
        let notes = store.list().unwrap_or_default();
        let mentions: Vec<Value> = note
            .doc
            .blocks
            .iter()
            .enumerate()
            .flat_map(|(index, block)| {
                mention::in_block(block)
                    .into_iter()
                    .map(move |chip| (index, block.text[chip.range.clone()].to_owned(), chip))
            })
            .map(|(block, label, mention)| {
                let mut value = json!({"label": label, "block": block});
                match &mention.target {
                    MentionTarget::Session(id) => {
                        value["session"] = session_value(id);
                    }
                    MentionTarget::Note(id) => {
                        value["note"] = match notes.iter().find(|n| &n.id == id) {
                            Some(other) => json!({"id": id, "title": other.display_title()}),
                            None => json!({"id": id, "missing": true}),
                        };
                    }
                }
                value
            })
            .collect();

        let mut result = note_row(&meta);
        let object = result.as_object_mut().expect("note_row is an object");
        if let Some(record) = sessions
            .as_deref()
            .ok()
            .and_then(|all| note_sessions(all).get(meta.id.as_str()).copied())
        {
            object.insert("session".into(), session_fact(record));
        }
        // Exactly the text edit_note matches: the canonical Markdown body,
        // title first, front matter left out.
        object.insert("markdown".into(), json!(diri_notes::text_edit::body(&note)));
        object.insert("path".into(), json!(meta.path));
        object.insert("todos".into(), Value::Array(todos));
        object.insert("mentions".into(), Value::Array(mentions));
        // Notes that link here: often the context this note was written in.
        let mut index = LinkIndex::default();
        for other in &notes {
            if other.id == meta.id {
                index.upsert_doc(&meta.id, &note.doc);
            } else if let Ok(loaded) = store.load(&other.id) {
                index.upsert_doc(&other.id, &loaded.doc);
            }
        }
        let backlinks = index.backlinks(&meta.id);
        object.insert("backlinks_total".into(), json!(backlinks.len()));
        object.insert(
            "backlinks".into(),
            Value::Array(
                backlinks
                    .iter()
                    .take(READ_BACKLINKS)
                    .map(reference_row)
                    .collect(),
            ),
        );
        if let Err(error) = sessions {
            object.insert("sessions_unavailable".into(), json!(error));
        }
        Ok(result)
    }

    /// A note's place among the others: the notes it links to, the notes
    /// linking to it (with the words around each link), places that write
    /// its title without linking, and optionally its neighbourhood graph.
    pub(super) fn note_links(&self, args: &Value) -> Result<Value, String> {
        let store = self.note_store()?;
        let sessions = self.sessions();
        let meta = self.resolve_note(
            &store,
            &required_string(args, "note")?,
            sessions.as_deref().ok(),
        )?;
        let index = LinkIndex::read(&store).map_err(|e| format!("cannot read notes: {e}"))?;
        let outgoing: Vec<Value> = index
            .outgoing(&meta.id)
            .iter()
            .take(MAX_LINK_ROWS)
            .map(|link| {
                let mut row = json!({
                    "note": link.target,
                    "label": link.label,
                    "block": link.block,
                    "context": link.context,
                });
                match index.title(&link.target) {
                    Some(title) => row["title"] = json!(title),
                    None => row["missing"] = json!(true),
                }
                row
            })
            .collect();
        let backlinks = index.backlinks(&meta.id);
        let mut result = json!({
            "note": meta.id,
            "title": meta.display_title(),
            "links": outgoing,
            "backlinks": backlinks.iter().take(MAX_LINK_ROWS).map(reference_row).collect::<Vec<_>>(),
            "backlinks_total": backlinks.len(),
        });
        if optional_bool(args, "unlinked").unwrap_or(true) {
            let unlinked = index.unlinked_mentions(&meta.id, DEFAULT_UNLINKED);
            result["unlinked_mentions"] =
                Value::Array(unlinked.iter().map(reference_row).collect());
        }
        if let Some(depth) = args["depth"].as_u64() {
            let graph = index.neighborhood(&meta.id, depth.clamp(1, 3) as usize);
            result["graph"] = json!({
                "nodes": graph.nodes.iter().map(|n| json!({"note": n.id, "title": n.title, "links": n.degree})).collect::<Vec<_>>(),
                "edges": graph.edges.iter().map(|(a, b)| json!([graph.nodes[*a].id, graph.nodes[*b].id])).collect::<Vec<_>>(),
            });
        }
        Ok(result)
    }

    /// `[[Title]]` in Markdown an agent is about to store, as note links.
    fn link_wiki_markdown(&self, store: &NoteStore, markdown: &str) -> WikiRewrite {
        if !markdown.contains("[[") {
            return WikiRewrite {
                text: markdown.to_owned(),
                ..WikiRewrite::default()
            };
        }
        let notes = store.list().unwrap_or_default();
        backlinks::rewrite_wiki_links(markdown, |name| backlinks::wiki_target(&notes, name))
    }

    pub(super) fn write_note(&self, args: &Value) -> Result<Value, String> {
        let append = optional_string(args, "append");
        let entry = optional_string(args, "entry");
        let checked = optional_bool(args, "checked");
        let link = optional_string(args, "link_session");
        let todo = match (args["todo_index"].as_u64(), optional_string(args, "todo")) {
            (Some(index), None) => Some(TodoSelector::Index(index as usize)),
            (None, Some(text)) => Some(TodoSelector::Text(text)),
            (None, None) => None,
            (Some(_), Some(_)) => return Err("pass todo or todo_index, not both".into()),
        };
        if append.is_none() && entry.is_none() && checked.is_none() && link.is_none() {
            return Err("write_note needs entry, checked, link_session, or append".into());
        }
        if todo.is_none() && (checked.is_some() || link.is_some()) {
            return Err("checked and link_session need todo or todo_index".into());
        }
        if todo.is_some() && checked.is_none() && link.is_none() && entry.is_none() {
            return Err(
                "todo selects a to-do for entry, checked, or link_session; pass one".into(),
            );
        }
        if append
            .as_ref()
            .is_some_and(|text| text.len() > MAX_APPEND_BYTES)
        {
            return Err(format!("append is larger than {MAX_APPEND_BYTES} bytes"));
        }
        if entry
            .as_ref()
            .is_some_and(|text| text.chars().count() > MAX_ENTRY_CHARS)
        {
            return Err(format!(
                "an entry is at most {MAX_ENTRY_CHARS} characters: one or two short sentences. Put longer write-ups in a note of their own with create_note."
            ));
        }

        let store = self.note_store()?;
        let snapshot = self.snapshot()?;
        let meta = self.resolve_note(
            &store,
            &required_string(args, "note")?,
            Some(&snapshot.sessions),
        )?;
        let note_session = note_sessions(&snapshot.sessions)
            .get(meta.id.as_str())
            .map(|record| record.id.0.clone());
        let mentioned: Vec<String> = meta
            .mentions
            .iter()
            .filter_map(|target| match target {
                MentionTarget::Session(id) => Some(id.clone()),
                MentionTarget::Note(_) => None,
            })
            .collect();
        McpPolicy::new(
            &snapshot.sessions,
            &snapshot.projects,
            self.caller.as_deref(),
        )?
        .authorize(WriteAction::WriteNote {
            mentions: &mentioned,
            note_session: note_session.as_deref(),
        })?;
        let link = match link {
            Some(id) => {
                let record = find_session(&snapshot.sessions, &id)?;
                let label = mention::session_label(
                    short_label(record.effective_kind().id()),
                    &record.title,
                );
                Some((id, label))
            }
            None => None,
        };

        let caller_record = self
            .caller
            .as_deref()
            .and_then(|id| snapshot.sessions.iter().find(|r| r.id.0 == id))
            .cloned();
        let catalog = store.list().unwrap_or_default();
        let mut wiki = WikiRewrite::default();
        let (note, (index, changes)) = store
            .update(
                &meta.id,
                &Author::Session(self.require_caller()?.to_owned()),
                |note: &mut Note| {
                    let before = block_texts(note);
                    let index = match &todo {
                        Some(selector) => {
                            Some(handoff::find_todo(note, selector).map_err(|e| {
                                std::io::Error::new(std::io::ErrorKind::InvalidInput, e)
                            })?)
                        }
                        None => None,
                    };
                    let mut changes = Vec::new();
                    if let (Some(index), Some(checked)) = (index, checked)
                        && handoff::set_checked(note, index, checked)
                    {
                        changes.push(if checked { "checked" } else { "unchecked" });
                    }
                    if let (Some(index), Some((id, label))) = (index, &link)
                        && handoff::link_session(note, index, label, id)
                    {
                        changes.push("linked");
                    }
                    if let Some(text) = &entry {
                        // Under the chosen to-do, else under the caller's own to-do,
                        // else in the note's Updates section.
                        let (label, id) = caller_record.as_ref().map_or_else(
                            || ("@agent".to_owned(), String::new()),
                            |r| {
                                (
                                    mention::session_label(
                                        &mention::agent_display_name(r.effective_kind().id()),
                                        &r.title,
                                    ),
                                    r.id.0.clone(),
                                )
                            },
                        );
                        let date = handoff::entry_stamp();
                        // A named to-do takes it; otherwise append_update files
                        // it under the caller's own to-do, else in Updates.
                        let placed = index.and_then(|todo| {
                            diri_notes::work::append_todo_update(
                                note, todo, &date, &label, &id, text,
                            )
                        });
                        if placed.is_none() {
                            handoff::append_update(note, &date, &label, &id, text);
                        }
                        changes.push("entry");
                    }
                    if let Some(text) = &append {
                        note_store::append_markdown(note, text);
                        changes.push("appended");
                    }
                    wiki = link_new_blocks(note, &before, &catalog);
                    Ok((index, changes))
                },
            )
            .map_err(|e| format!("cannot write note {}: {e}", meta.id))?;
        let mut result = json!({"note": meta.id, "changes": changes});
        add_wiki_report(&mut result, &wiki);
        if let Some(index) = index
            && let Some(todo) = handoff::todos(&note).into_iter().find(|t| t.index == index)
        {
            result["todo"] = json!({
                "index": todo.index,
                "text": todo.text,
                "checked": todo.checked,
                "sessions": todo.sessions,
            });
        }
        Ok(result)
    }

    /// A new note written by the calling agent, shown under it in the
    /// sidebar, in its project (or `project`), optionally opened for the
    /// person.
    pub(super) fn create_note(&self, args: &Value) -> Result<Value, String> {
        let title = required_string(args, "title")?;
        let markdown = optional_string(args, "markdown").unwrap_or_default();
        if markdown.len() > MAX_APPEND_BYTES {
            return Err(format!("markdown is larger than {MAX_APPEND_BYTES} bytes"));
        }
        let caller = self.require_caller()?.to_owned();
        let snapshot = self.snapshot()?;
        McpPolicy::new(&snapshot.sessions, &snapshot.projects, Some(&caller))?
            .authorize(WriteAction::CreateNote)?;
        let folder = match optional_string(args, "project") {
            Some(path) => {
                if !Path::new(&path).is_dir() {
                    return Err(format!("{path} is not a folder"));
                }
                path
            }
            None => self.caller_project(&snapshot)?.0,
        };
        let wiki = match self.note_store() {
            Ok(store) => self.link_wiki_markdown(&store, &markdown),
            Err(_) => WikiRewrite {
                text: markdown.clone(),
                ..WikiRewrite::default()
            },
        };
        let record = match self.spawn_note(&folder, &title, &wiki.text, Some(&caller))? {
            NoteSpawn::Created(record) => record,
            NoteSpawn::Unavailable(reason) => {
                return Err(format!("cannot create the note: {reason}"));
            }
        };
        let opened = optional_bool(args, "open").unwrap_or(false);
        if opened {
            self.request(
                Method::SESSION_REVEAL,
                json!({ "sessionID": record.id.0 }),
                DEFAULT_TIMEOUT,
            )?;
        }
        let mut result = json!({
            "note": record.note_id,
            "session_id": record.id.0,
            "title": record.title,
            "opened": opened,
        });
        add_wiki_report(&mut result, &wiki);
        Ok(result)
    }

    /// Starts an agent on a note (or one of its to-dos) with the note as its
    /// parent: it appears under the note, receives the note as its brief,
    /// and its chip is added to the to-do.
    pub(super) fn start_from_note(&self, args: &Value) -> Result<Value, String> {
        let caller = self.require_caller()?.to_owned();
        let store = self.note_store()?;
        let snapshot = self.snapshot()?;
        let meta = self.resolve_note(
            &store,
            &required_string(args, "note")?,
            Some(&snapshot.sessions),
        )?;
        let existing = note_sessions(&snapshot.sessions)
            .get(meta.id.as_str())
            .map(|record| (*record).clone());
        let (note_session, sessions) = match existing {
            Some(record) => (record, snapshot.sessions.clone()),
            // A note from before note sessions: give it one first.
            None => (self.adopt_note_session(&meta.id)?, self.sessions()?),
        };
        McpPolicy::new(&sessions, &snapshot.projects, Some(&caller))?.authorize(
            WriteAction::StartFromNote {
                note_session: &note_session.id.0,
            },
        )?;

        let note = store
            .load(&meta.id)
            .map_err(|e| format!("cannot read note {}: {e}", meta.id))?;
        let todo = match (args["todo_index"].as_u64(), optional_string(args, "todo")) {
            (Some(index), None) => Some(handoff::find_todo(
                &note,
                &TodoSelector::Index(index as usize),
            )?),
            (None, Some(text)) => Some(handoff::find_todo(&note, &TodoSelector::Text(text))?),
            (None, None) => None,
            (Some(_), Some(_)) => return Err("pass todo or todo_index, not both".into()),
        };
        let extra = optional_string(args, "prompt");
        let brief = match todo {
            // A to-do gets exactly the brief the app's Start sends.
            Some(index) => {
                let mut notes = Vec::new();
                let mut related = Vec::new();
                for target in diri_notes::work::mentions(&note.doc.blocks, index) {
                    match target {
                        MentionTarget::Note(id) => {
                            if let Ok(loaded) = store.load(&id) {
                                notes.push(diri_notes::work::ResolvedNote {
                                    body: diri_notes::markdown::write(
                                        &Default::default(),
                                        &loaded.doc,
                                    ),
                                    title: loaded.doc.title.clone(),
                                    id,
                                });
                            }
                        }
                        MentionTarget::Session(id) => {
                            if let Some(record) = sessions.iter().find(|r| r.id.0 == id) {
                                related.push(diri_notes::work::ResolvedSession {
                                    kind: short_label(record.effective_kind().id()).to_owned(),
                                    title: record.title.clone(),
                                    status: status_label(&record.status).to_owned(),
                                    id,
                                });
                            }
                        }
                    }
                }
                let brief =
                    diri_notes::work::brief(&meta.id, &note, index, &notes, &related).prompt;
                match extra.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
                    Some(extra) => format!("{extra}\n\n{brief}"),
                    None => brief,
                }
            }
            None => {
                let related: Vec<handoff::Related> = meta
                    .mentions
                    .iter()
                    .filter_map(|target| match target {
                        MentionTarget::Session(id) => sessions.iter().find(|r| &r.id.0 == id),
                        MentionTarget::Note(_) => None,
                    })
                    .filter(|record| !record.is_note())
                    .map(|record| handoff::Related {
                        session_id: record.id.0.clone(),
                        kind: short_label(record.effective_kind().id()).to_owned(),
                        title: record.title.clone(),
                        status: status_label(&record.status).to_owned(),
                    })
                    .collect();
                handoff::prompt(&meta.id, &note, None, &related, extra.as_deref())
            }
        };
        let kind = match optional_string(args, "kind") {
            Some(kind) => kind,
            None => sessions
                .iter()
                .find(|r| r.id.0 == caller)
                .map(|r| short_label(r.effective_kind().id()).to_owned())
                .ok_or("pass kind: which agent should do the work")?,
        };
        let name = todo
            .and_then(|index| note.doc.blocks.get(index))
            .map(|block| block.text.clone())
            .unwrap_or_else(|| meta.display_title().to_owned());
        let mut spawn = json!({
            "kind": kind,
            "cwd": note_session.cwd,
            "name": name,
            "prompt": brief,
        });
        if let Some(separate) = optional_bool(args, "separate_copy") {
            spawn["worktree"] = json!(separate);
        }
        for key in ["task", "result_schema", "operation_id"] {
            if let Some(value) = args.get(key) {
                spawn[key] = value.clone();
            }
        }
        let mut spawned = self.spawn_session(&spawn, Some(note_session.id.clone()))?;
        let child = spawned["spawn_receipt"]["session_id"]
            .as_str()
            .or_else(|| spawned["id"].as_str())
            .map(str::to_owned);
        if let (Some(index), Some(child)) = (todo, child.as_deref())
            && spawned["ok"] != false
        {
            // The same short chip the app writes: "@Claude Code". The to-do's
            // text is already right beside it.
            let label = mention::session_label(&mention::agent_display_name(&kind), "");
            store
                .update(&meta.id, &Author::Session(caller.clone()), |note| {
                    handoff::link_session(note, index, &label, child);
                    Ok(())
                })
                .map_err(|e| format!("started {child}, but could not link it in the note: {e}"))?;
        }
        spawned["note"] = json!(meta.id);
        spawned["note_session"] = json!(note_session.id.0);
        if let Some(index) = todo {
            spawned["todo"] = json!(index);
        }
        Ok(spawned)
    }

    /// Earlier versions of a note (newest first), or one version's text.
    /// Read-only: restoring a version is for the person, in the app or CLI.
    pub(super) fn note_history(&self, args: &Value) -> Result<Value, String> {
        let store = self.note_store()?;
        let sessions = self.sessions();
        let meta = self.resolve_note(
            &store,
            &required_string(args, "note")?,
            sessions.as_deref().ok(),
        )?;
        let history = store.history();
        if let Some(version) = args["version"].as_u64() {
            let markdown = history
                .read(&meta.id, version)
                .map_err(|e| format!("cannot read that version: {e}"))?;
            return Ok(json!({
                "note": meta.id,
                "version": version,
                "when": diri_notes::history::describe_time(version),
                "markdown": markdown,
            }));
        }
        let versions = history
            .list(&meta.id)
            .map_err(|e| format!("cannot read the history: {e}"))?;
        let total = versions.len();
        let rows: Vec<Value> = versions
            .into_iter()
            .take(MAX_HISTORY_ROWS)
            .map(|v| {
                json!({
                    "version": v.id,
                    "when": diri_notes::history::describe_time(v.id),
                    "by": v.author.describe(),
                    "size": v.bytes,
                    "summary": v.summary,
                })
            })
            .collect();
        Ok(json!({
            "note": meta.id,
            "title": meta.display_title(),
            "versions": rows,
            "total": total,
            "times_are": "UTC",
            "restore": "Only the person can restore a version, from the app (Version history…) or `dirijor note restore`.",
        }))
    }

    /// Gives an orphan note file its Session (idempotent in the Engine).
    fn adopt_note_session(&self, note_id: &str) -> Result<SessionRecord, String> {
        let params = json!({"kind": AgentKind::NOTE_ID, "cwd": "", "noteId": note_id});
        self.request_typed(Method::SESSION_SPAWN, params, NOTE_SPAWN_TIMEOUT)
    }

    /// Changes a note's text in place, like editing a Markdown file: the
    /// exact `old_string` (unique unless `replace_all`) becomes `new_string`,
    /// and an empty `new_string` deletes. Matches the Markdown `read_note`
    /// returns.
    pub(super) fn edit_note(&self, args: &Value) -> Result<Value, String> {
        let old = args["old_string"]
            .as_str()
            .ok_or("old_string is required: the exact text to change, copied from read_note")?
            .to_owned();
        let new = args["new_string"].as_str().unwrap_or_default().to_owned();
        let replace_all = optional_bool(args, "replace_all").unwrap_or(false);
        self.change_note(args, |note| {
            diri_notes::text_edit::edit(note, &old, &new, replace_all)
        })
    }

    /// Replaces everything under one heading of a note.
    pub(super) fn replace_section(&self, args: &Value) -> Result<Value, String> {
        let heading = required_string(args, "heading")?;
        let markdown = args["markdown"].as_str().unwrap_or_default().to_owned();
        if markdown.len() > MAX_APPEND_BYTES {
            return Err(format!("markdown is larger than {MAX_APPEND_BYTES} bytes"));
        }
        self.change_note(args, |note| {
            diri_notes::text_edit::replace_section(note, &heading, &markdown)
        })
    }

    /// Applies an in-place change under the store lock, attributed to the
    /// caller, with history before and after; an open editor merges it in.
    fn change_note(
        &self,
        args: &Value,
        change: impl FnOnce(&mut Note) -> Result<diri_notes::text_edit::Edited, String>,
    ) -> Result<Value, String> {
        let store = self.note_store()?;
        let snapshot = self.snapshot()?;
        let meta = self.resolve_note(
            &store,
            &required_string(args, "note")?,
            Some(&snapshot.sessions),
        )?;
        self.authorize_note_write(&snapshot, &meta)?;
        let caller = self.require_caller()?.to_owned();
        let catalog = store.list().unwrap_or_default();
        let mut wiki = WikiRewrite::default();
        let (_, edited) = store
            .update(&meta.id, &Author::Session(caller), |note| {
                let before = block_texts(note);
                let edited = change(note)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                wiki = link_new_blocks(note, &before, &catalog);
                Ok(edited)
            })
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::InvalidInput => e.to_string(),
                _ => format!("cannot change note {}: {e}", meta.id),
            })?;
        let version = store
            .history()
            .list(&meta.id)
            .ok()
            .and_then(|versions| versions.first().map(|v| v.id));
        let mut result = json!({
            "note": meta.id,
            "replacements": edited.replacements,
            "changed": edited.excerpt,
            "version": version,
            "undo": "Every earlier version is kept: note_history lists them, and the person can restore one.",
        });
        add_wiki_report(&mut result, &wiki);
        if edited.tolerant {
            result["matched"] = json!(
                "ignoring table spacing: Diri re-aligns tables after each change, so your text matched one row once spaces around | were ignored"
            );
        }
        Ok(result)
    }

    fn authorize_note_write(
        &self,
        snapshot: &SessionListResult,
        meta: &NoteMeta,
    ) -> Result<(), String> {
        let note_session = note_sessions(&snapshot.sessions)
            .get(meta.id.as_str())
            .map(|record| record.id.0.clone());
        let mentioned: Vec<String> = meta
            .mentions
            .iter()
            .filter_map(|target| match target {
                MentionTarget::Session(id) => Some(id.clone()),
                MentionTarget::Note(_) => None,
            })
            .collect();
        McpPolicy::new(
            &snapshot.sessions,
            &snapshot.projects,
            self.caller.as_deref(),
        )?
        .authorize(WriteAction::WriteNote {
            mentions: &mentioned,
            note_session: note_session.as_deref(),
        })?;
        Ok(())
    }

    fn note_store(&self) -> Result<NoteStore, String> {
        let dir = self
            .notes_dir
            .clone()
            .or_else(NoteStore::resolve_dir)
            .ok_or("cannot find a home directory for notes")?;
        NoteStore::open(dir).map_err(|e| format!("cannot open notes: {e}"))
    }

    /// A note by id or title, by its note Session's id, or `origin`: the
    /// nearest note Session among the caller's ancestors.
    fn resolve_note(
        &self,
        store: &NoteStore,
        query: &str,
        sessions: Option<&[SessionRecord]>,
    ) -> Result<NoteMeta, String> {
        let by_session = |id: &str| {
            sessions
                .and_then(|all| all.iter().find(|record| record.id.0 == id))
                .filter(|record| record.is_note())
                .and_then(|record| record.note_id.clone())
        };
        let query = if query == ORIGIN {
            let caller = self.require_caller()?;
            let sessions =
                sessions.ok_or("\"origin\" needs the Diri Engine, which is not reachable")?;
            Lineage::new(sessions, Some(caller))
                .ancestors(caller)
                .into_iter()
                .find(|record| record.is_note())
                .and_then(|record| record.note_id.clone())
                .ok_or("this session was not started from a note (no note session among its ancestors)")?
        } else {
            by_session(query).unwrap_or_else(|| query.to_owned())
        };
        let query = query.as_str();
        let notes = store
            .list()
            .map_err(|e| format!("cannot list notes: {e}"))?;
        match note_store::resolve(&notes, query) {
            Resolve::Found(note) => Ok(*note),
            Resolve::NotFound => Err(format!("no note matches \"{query}\"")),
            Resolve::Ambiguous(many) => Err(format!(
                "\"{query}\" matches {} notes; pass an id: {}",
                many.len(),
                many.iter()
                    .map(|n| format!("{} ({})", n.display_title(), n.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// The caller's project root and id.
    fn caller_project(&self, snapshot: &SessionListResult) -> Result<(String, ProjectId), String> {
        let caller = self.require_caller()?;
        let record = find_session(&snapshot.sessions, caller)?;
        snapshot
            .projects
            .iter()
            .find(|project| project.id == record.project_id)
            .map(|project| (project.root.clone(), project.id.clone()))
            .ok_or_else(|| {
                format!(
                    "calling session {caller} has no live project; pass project:\"all\" or a path"
                )
            })
    }

    /// Appends a child's report to its parent note's Updates section.
    pub(super) fn report_into_note(
        &self,
        note: &SessionRecord,
        caller: &SessionRecord,
        status: &str,
        summary: &str,
        artifacts: &[String],
    ) -> Result<Value, String> {
        let note_id = note
            .note_id
            .as_deref()
            .ok_or_else(|| format!("note session {} has no note file", note.id.0))?;
        let store = self.note_store()?;
        let label = mention::session_label(
            &mention::agent_display_name(caller.effective_kind().id()),
            &caller.title,
        );
        let mut text = if status == "update" {
            summary.to_owned()
        } else {
            let mut word = status.to_owned();
            if let Some(first) = word.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            format!("{word}: {summary}")
        };
        if !artifacts.is_empty() {
            text.push_str(&format!(" ({})", artifacts.join(", ")));
        }
        let date = handoff::entry_stamp();
        let mut place = handoff::UpdatePlace::Updates;
        store
            .update(note_id, &Author::Session(caller.id.0.clone()), |doc| {
                place = handoff::append_update(doc, &date, &label, &caller.id.0, &text);
                Ok(())
            })
            .map_err(|e| format!("cannot write note {note_id}: {e}"))?;
        let (todo, where_) = match place {
            handoff::UpdatePlace::Todo(index) => (
                json!(index),
                "Your parent is a note: the report was added under the to-do you were started from.",
            ),
            handoff::UpdatePlace::Updates => (
                Value::Null,
                "Your parent is a note: the report was added to its Updates section.",
            ),
        };
        Ok(json!({
            "ok": true,
            "parent": note.id.0,
            "status": status,
            "recorded_in_note": note_id,
            "todo": todo,
            "note": where_,
        }))
    }
}

/// Note Sessions by the file they show. A file has at most one live
/// Session; when an archived one lingers beside it the live one wins.
fn note_sessions(sessions: &[SessionRecord]) -> HashMap<&str, &SessionRecord> {
    let mut map: HashMap<&str, &SessionRecord> = HashMap::new();
    for record in sessions.iter().filter(|record| record.is_note()) {
        let Some(note_id) = record.note_id.as_deref() else {
            continue;
        };
        match map.get(note_id) {
            Some(existing) if !existing.is_archived() => {}
            _ => {
                map.insert(note_id, record);
            }
        }
    }
    map
}

fn note_row(note: &NoteMeta) -> Value {
    json!({
        "id": note.id,
        "title": note.display_title(),
        "snippet": note.snippet,
        "project": note.project,
        "pinned": note.pinned,
        "archived": note.archived,
        "modified_ms": note.modified_ms,
        "todos": {"done": note.todos_done, "total": note.todos_total, "open": note.open_todos.len()},
        "mentions": note.mentions.iter().map(|target| match target {
            MentionTarget::Session(id) => json!({"session": id}),
            MentionTarget::Note(id) => json!({"note": id}),
        }).collect::<Vec<_>>(),
    })
}

/// A backlink or an unlinked mention, as the tools return it.
fn reference_row(reference: &Reference) -> Value {
    json!({
        "note": reference.source,
        "title": reference.source_title,
        "block": reference.block,
        "context": reference.context,
    })
}

/// Every block's text, to tell the blocks a write added or changed.
fn block_texts(note: &Note) -> std::collections::HashSet<String> {
    note.doc.blocks.iter().map(|b| b.text.clone()).collect()
}

/// Links `[[Title]]` in the blocks a write added or changed; the person's
/// untouched text is left exactly as it was.
fn link_new_blocks(
    note: &mut Note,
    before: &std::collections::HashSet<String>,
    catalog: &[NoteMeta],
) -> WikiRewrite {
    backlinks::link_wiki_blocks(
        &mut note.doc,
        |block| block.text.contains("[[") && !before.contains(&block.text),
        |name| backlinks::wiki_target(catalog, name),
    )
}

/// Tells the agent which `[[…]]` became links and which matched no note.
fn add_wiki_report(result: &mut Value, wiki: &WikiRewrite) {
    if !wiki.linked.is_empty() {
        result["linked_notes"] = json!(wiki.linked);
    }
    if !wiki.unresolved.is_empty() {
        result["unlinked_titles"] = json!(wiki.unresolved);
        result["unlinked_hint"] = json!(
            "No single note has these titles, so they stay as text. Check the title with list_notes, or pass the note's id: [[<id>]]."
        );
    }
}

fn session_fact(record: &SessionRecord) -> Value {
    let mut value = compact(record);
    let live = !record.is_archived() && !matches!(record.status, SessionStatus::Exited(_));
    value["live"] = json!(live);
    value
}

#[cfg(test)]
mod tests {
    use super::super::audit_tests::Peer;
    use super::*;
    use diri_notes::doc::Document;

    struct Fixture {
        peer: Peer,
        notes: tempfile::TempDir,
    }

    impl Fixture {
        fn new(sessions: Vec<SessionRecord>) -> Self {
            Self::scripted(sessions, |_| None).0
        }

        /// Answers `session.list` from `sessions` and anything else from
        /// `reply`; every method called is logged in order.
        fn scripted(
            sessions: Vec<SessionRecord>,
            reply: impl Fn(&str) -> Option<Value> + Send + 'static,
        ) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
            let listing = json!({
                "sessions": sessions,
                "projects": [{"id": "p", "root": "/work/diri", "name": "diri"}],
            });
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let log = calls.clone();
            let peer = Peer::new(move |method| {
                log.lock().unwrap().push(method.to_owned());
                reply(method).unwrap_or_else(|| listing.clone())
            });
            (
                Self {
                    peer,
                    notes: tempfile::tempdir().unwrap(),
                },
                calls,
            )
        }

        fn store(&self) -> NoteStore {
            NoteStore::open(self.notes.path()).unwrap()
        }

        fn bridge(&self, caller: &str) -> Bridge {
            Bridge::new(self.peer.path.clone(), Some(caller.into()))
                .with_notes_dir(self.notes.path().to_owned())
        }

        fn note(&self, title: &str, project: Option<&str>, body: &str) -> String {
            let store = self.store();
            let (id, _) = store
                .create(Document::new(title, Vec::new()), project)
                .unwrap();
            store
                .append(&id, body, &diri_notes::history::Author::Cli)
                .unwrap();
            id
        }
    }

    fn sessions() -> Vec<SessionRecord> {
        let root = super::super::tests::record("root", None);
        let child = super::super::tests::record("child", Some("root"));
        let other = super::super::tests::record("other", None);
        vec![root, child, other]
    }

    fn ids(value: &Value) -> Vec<String> {
        value["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["title"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn lists_notes_for_the_callers_project_by_default() {
        let fixture = Fixture::new(sessions());
        fixture.note("Diri PRD", Some("/work/diri"), "- [ ] ship");
        fixture.note("Groceries", None, "milk");
        fixture.note("Other repo", Some("/work/else"), "x");

        let mine = fixture
            .bridge("root")
            .call("list_notes", &json!({}))
            .unwrap();
        assert_eq!(ids(&mine), vec!["Diri PRD"]);
        assert_eq!(mine["notes"][0]["todos"]["open"], 1);

        let all = fixture
            .bridge("root")
            .call("list_notes", &json!({"project": "all", "query": "milk"}))
            .unwrap();
        assert_eq!(ids(&all), vec!["Groceries"]);
    }

    #[test]
    fn finds_notes_that_mention_the_caller_or_its_ancestors() {
        let fixture = Fixture::new(sessions());
        fixture.note("Direct", None, "ask [@Child](diri://session/child)");
        fixture.note("Via parent", None, "ask [@Root](diri://session/root)");
        fixture.note("Unrelated", None, "ask [@Other](diri://session/other)");

        let found = fixture
            .bridge("child")
            .call("list_notes", &json!({"project": "all", "mentions": "me"}))
            .unwrap();
        let mut titles = ids(&found);
        titles.sort();
        assert_eq!(titles, vec!["Direct", "Via parent"]);
        for note in found["notes"].as_array().unwrap() {
            let expected = if note["title"] == "Direct" {
                "self"
            } else {
                "ancestor"
            };
            assert_eq!(note["mentioned_via"], expected);
        }
    }

    /// An Agent reading a note session's "screen" gets the note itself; the
    /// Engine's `session_has_no_terminal` would only be retried.
    #[test]
    fn read_output_of_a_note_session_returns_the_note() {
        let notes = tempfile::tempdir().unwrap();
        let store = NoteStore::open(notes.path()).unwrap();
        let (note_id, _) = store
            .create(Document::new("Plan", Vec::new()), None)
            .unwrap();
        store
            .append(&note_id, "ship the fix", &diri_notes::history::Author::Cli)
            .unwrap();
        let mut note = super::super::tests::record("s_note", None);
        note.kind = AgentKind::NOTE;
        note.note_id = Some(note_id);
        let mut sessions = sessions();
        sessions.push(note);
        let (fixture, _) = Fixture::scripted(sessions, |method| {
            (method == Method::SESSION_READ_SCREEN).then(|| {
                json!({"__error": {
                    "code": diri_proto::control::SESSION_HAS_NO_TERMINAL,
                    "message": "s_note is a note, which has no terminal",
                }})
            })
        });
        let fixture = Fixture { notes, ..fixture };

        let output = fixture
            .bridge("root")
            .call("read_output", &json!({"session_id": "s_note"}))
            .unwrap();

        assert_eq!(output["mode"], "note");
        assert_eq!(output["title"], "Plan");
        assert!(
            output["markdown"]
                .as_str()
                .unwrap()
                .contains("ship the fix"),
            "{output}"
        );
    }

    /// An ended session whose screen was never kept answers once, with its
    /// conversation, instead of `terminal_not_retained` on every call.
    #[test]
    fn read_output_of_an_ended_session_without_a_screen_says_so() {
        let (fixture, calls) = Fixture::scripted(sessions(), |method| match method {
            Method::SESSION_READ_SCREEN => Some(json!({"__error": {
                "code": diri_proto::control::TERMINAL_NOT_RETAINED,
                "message": "other has ended and Diri did not keep its final screen",
            }})),
            Method::SESSION_READ_TRANSCRIPT => Some(json!({
                "available": true,
                "turns": [{"role": "agent", "text": "all done"}],
            })),
            _ => None,
        });

        for mode in ["screen", "tail", "last_message"] {
            let output = fixture
                .bridge("root")
                .call("read_output", &json!({"session_id": "other", "mode": mode}))
                .unwrap();
            if mode == "last_message" {
                assert_eq!(output["message"], "all done", "{output}");
                continue;
            }
            assert_eq!(output["mode"], "ended", "{mode}: {output}");
            assert!(
                output["screen_unavailable"]
                    .as_str()
                    .unwrap()
                    .contains("did not keep"),
                "{output}"
            );
            assert_eq!(output["turns"][0]["text"], "all done", "{output}");
        }
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == Method::SESSION_READ_TRANSCRIPT)
        );
    }

    #[test]
    fn read_note_resolves_mentions_and_todo_sessions() {
        let fixture = Fixture::new(sessions());
        let other = fixture.note("Design", None, "text");
        fixture.note(
            "Resize PRD",
            Some("/work/diri"),
            &format!(
                "See [@Design](diri://note/{other}) and [@Gone](diri://session/s_gone).\n\n\
                 - [ ] Fix flicker [@Codex](diri://session/other)"
            ),
        );
        let note = fixture
            .bridge("root")
            .call("read_note", &json!({"note": "resize"}))
            .unwrap();
        assert_eq!(note["title"], "Resize PRD");
        assert!(
            note["markdown"]
                .as_str()
                .unwrap()
                .contains("- [ ] Fix flicker")
        );
        let mentions = note["mentions"].as_array().unwrap();
        assert_eq!(mentions[0]["note"]["title"], "Design");
        assert_eq!(mentions[1]["session"]["missing"], true);
        assert_eq!(mentions[2]["session"]["id"], "other");
        assert_eq!(mentions[2]["session"]["live"], true);
        let todo = &note["todos"][0];
        assert_eq!(todo["text"], "Fix flicker @Codex");
        assert_eq!(todo["sessions"][0]["id"], "other");

        let origin = fixture
            .bridge("child")
            .call("read_note", &json!({"note": "origin"}))
            .unwrap_err();
        assert!(origin.contains("not started from a note"), "{origin}");
    }

    #[test]
    fn tables_round_trip_through_write_and_read_note() {
        let fixture = Fixture::new(sessions());
        // An agent's table, written loosely the way agents write them.
        let id = fixture.note(
            "Gaps",
            None,
            "| What's missing | Type | Requirement for 5/5 |\n|---|---|---|\n| Onboarding email sequence | Content | 3 emails, tested |\n",
        );
        fixture
            .bridge("root")
            .call(
                "write_note",
                &json!({"note": id, "append": "| Channel | Spend |\n|:--|--:|\n| Search | $1,200 |"}),
            )
            .unwrap();
        let note = fixture
            .bridge("root")
            .call("read_note", &json!({"note": id}))
            .unwrap();
        let markdown = note["markdown"].as_str().unwrap();
        assert!(
            markdown.contains(
                "| What's missing            | Type    | Requirement for 5/5 |\n| ------------------------- | ------- | ------------------- |"
            ),
            "{markdown}"
        );
        assert!(markdown.contains("| :------ | -----: |"), "{markdown}");
        let (_, doc) = diri_notes::markdown::parse(markdown);
        let tables = doc.blocks.iter().filter(|b| b.kind.starts_table()).count();
        assert_eq!(tables, 2, "{markdown}");
    }

    #[test]
    fn write_note_checks_links_and_appends() {
        let fixture = Fixture::new(sessions());
        let id = fixture.note("Plan", None, "- [ ] Fix flicker\n- [ ] Fix tests");
        let result = fixture
            .bridge("root")
            .call(
                "write_note",
                &json!({"note": id, "todo": "flicker", "checked": true, "link_session": "child", "append": "Done in PR #600."}),
            )
            .unwrap();
        assert_eq!(result["changes"], json!(["checked", "linked", "appended"]));
        assert_eq!(result["todo"]["sessions"], json!(["child"]));
        let source = fixture.store().load(&id).unwrap().to_markdown();
        assert!(
            source.contains("- [x] Fix flicker [@codex: child](diri://session/child)"),
            "{source}"
        );
        assert!(source.trim_end().ends_with("Done in PR #600."), "{source}");
    }

    #[test]
    fn note_links_reports_links_backlinks_and_unlinked_mentions() {
        let fixture = Fixture::new(sessions());
        let plan = fixture.note("Release plan", None, "Ship on Friday.");
        let brief = fixture.note(
            "Launch brief",
            None,
            &format!("Builds on [@Release plan](diri://note/{plan}) and the pricing page."),
        );
        let retro = fixture.note("Retro", None, "The release plan slipped a day.");
        let bridge = fixture.bridge("root");
        let links = bridge
            .call("note_links", &json!({"note": "Release plan", "depth": 1}))
            .unwrap();
        assert_eq!(links["note"], plan.as_str());
        assert_eq!(links["backlinks_total"], 1);
        assert_eq!(links["backlinks"][0]["note"], brief.as_str());
        assert_eq!(links["backlinks"][0]["title"], "Launch brief");
        assert!(
            links["backlinks"][0]["context"]
                .as_str()
                .unwrap()
                .contains("Builds on @Release plan"),
            "{links}"
        );
        assert_eq!(links["unlinked_mentions"][0]["note"], retro.as_str());
        assert_eq!(links["graph"]["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(links["graph"]["edges"][0], json!([plan, brief]));
        let out = bridge
            .call("note_links", &json!({"note": brief, "unlinked": false}))
            .unwrap();
        assert_eq!(out["links"][0]["note"], plan.as_str());
        assert_eq!(out["links"][0]["title"], "Release plan");
        assert!(out.get("unlinked_mentions").is_none());

        // read_note carries the backlinks; list_notes searches by link.
        let read = bridge.call("read_note", &json!({"note": plan})).unwrap();
        assert_eq!(read["backlinks_total"], 1);
        assert_eq!(read["backlinks"][0]["note"], brief.as_str());
        let listed = bridge
            .call(
                "list_notes",
                &json!({"project": "all", "links_to": "Release plan"}),
            )
            .unwrap();
        assert_eq!(ids(&listed), ["Launch brief"]);
        assert_eq!(listed["links_to"], plan.as_str());
    }

    #[test]
    fn wiki_links_written_by_agents_become_note_links() {
        let fixture = Fixture::new(sessions());
        let plan = fixture.note("Release plan", None, "Ship on Friday.");
        let id = fixture.note("Status", None, "Untouched [[Release plan]] stays as typed.");
        let bridge = fixture.bridge("root");
        let result = bridge
            .call(
                "write_note",
                &json!({"note": id, "append": "Follows [[release plan]] and [[Nowhere]]."}),
            )
            .unwrap();
        assert_eq!(result["linked_notes"], json!([plan]));
        assert_eq!(result["unlinked_titles"], json!(["Nowhere"]));
        let source = fixture.store().load(&id).unwrap().to_markdown();
        assert!(
            source.contains(&format!("Follows [@Release plan](diri://note/{plan}) and")),
            "{source}"
        );
        assert!(
            source.contains("Untouched \\[\\[Release plan\\]\\] stays"),
            "the person's own text is not rewritten: {source}"
        );
        let edited = bridge
            .call(
                "edit_note",
                &json!({"note": "Release plan", "old_string": "Ship on Friday.", "new_string": "Ship on Friday, see [[Status|status]]."}),
            )
            .unwrap();
        assert_eq!(edited["linked_notes"], json!([id]));
        let links = bridge.call("note_links", &json!({"note": id})).unwrap();
        assert_eq!(links["backlinks"][0]["note"], plan.as_str());
        assert!(
            links["backlinks"][0]["context"]
                .as_str()
                .unwrap()
                .contains("@status"),
            "{links}"
        );
    }

    #[test]
    fn delegated_agents_write_only_to_notes_that_mention_them() {
        let fixture = Fixture::new(sessions());
        let unrelated = fixture.note("Private", None, "mine");
        let about = fixture.note("About root", None, "ask [@Root](diri://session/root)");

        let denied = fixture
            .bridge("child")
            .call("write_note", &json!({"note": unrelated, "append": "hi"}))
            .unwrap_err();
        assert!(denied.contains("write_note denied"), "{denied}");
        fixture
            .bridge("child")
            .call("write_note", &json!({"note": about, "append": "hi"}))
            .unwrap();
        fixture
            .bridge("other")
            .call(
                "write_note",
                &json!({"note": unrelated, "append": "root may"}),
            )
            .unwrap();
    }

    #[test]
    fn write_note_rejects_incomplete_requests_before_touching_files() {
        let fixture = Fixture::new(sessions());
        let id = fixture.note("Plan", None, "- [ ] a");
        let bridge = fixture.bridge("root");
        for args in [
            json!({"note": id}),
            json!({"note": id, "checked": true}),
            json!({"note": id, "todo": "a"}),
            json!({"note": id, "todo": "a", "todo_index": 0, "checked": true}),
        ] {
            assert!(bridge.call("write_note", &args).is_err(), "{args}");
        }
    }

    fn note_session(id: &str, note_id: &str, parent: Option<&str>) -> SessionRecord {
        let mut record = super::super::tests::record(id, parent);
        record.kind = AgentKind::NOTE;
        record.note_id = Some(note_id.into());
        record.title = "Resize PRD".into();
        record
    }

    /// A note session `s_note` with a child agent `child` started from it.
    fn from_note() -> (Fixture, String) {
        let notes = tempfile::tempdir().unwrap();
        let (note_id, _) = NoteStore::open(notes.path())
            .unwrap()
            .create(Document::new("Resize PRD", Vec::new()), Some("/elsewhere"))
            .unwrap();
        let mut fixture = Fixture::new(vec![
            note_session("s_note", &note_id, None),
            super::super::tests::record("child", Some("s_note")),
            super::super::tests::record("grandchild", Some("child")),
            super::super::tests::record("stranger", None),
        ]);
        fixture.notes = notes;
        (fixture, note_id)
    }

    #[test]
    fn origin_is_the_nearest_note_session_among_ancestors() {
        let (fixture, note_id) = from_note();
        for caller in ["child", "grandchild"] {
            let note = fixture
                .bridge(caller)
                .call("read_note", &json!({"note": "origin"}))
                .unwrap();
            assert_eq!(note["id"], note_id.as_str());
            assert_eq!(note["session"]["id"], "s_note");
        }
        let by_session = fixture
            .bridge("stranger")
            .call("read_note", &json!({"note": "s_note"}))
            .unwrap();
        assert_eq!(by_session["id"], note_id.as_str());
        let none = fixture
            .bridge("stranger")
            .call("read_note", &json!({"note": "origin"}))
            .unwrap_err();
        assert!(none.contains("not started from a note"), "{none}");

        let whoami = fixture
            .bridge("grandchild")
            .call("whoami", &json!({}))
            .unwrap();
        assert_eq!(whoami["origin_note"]["note_id"], note_id.as_str());
        assert_eq!(whoami["origin_note"]["session_id"], "s_note");
    }

    #[test]
    fn project_notes_include_note_sessions_in_the_project() {
        // The file says /elsewhere, but its Session lives in project "p".
        let (fixture, note_id) = from_note();
        let listed = fixture
            .bridge("child")
            .call("list_notes", &json!({}))
            .unwrap();
        assert_eq!(listed["notes"][0]["id"], note_id.as_str());
        assert_eq!(listed["notes"][0]["session_id"], "s_note");
    }

    #[test]
    fn reports_to_a_parent_note_land_in_its_updates() {
        let (fixture, note_id) = from_note();
        let report = fixture
            .bridge("child")
            .call(
                "report_to_parent",
                &json!({"summary": "Fixed the flicker", "status": "done", "artifacts": ["PR #600"]}),
            )
            .unwrap();
        assert_eq!(report["recorded_in_note"], note_id.as_str());
        let source = fixture.store().load(&note_id).unwrap().to_markdown();
        assert!(source.contains("## Updates"), "{source}");
        assert!(
            source.contains("](diri://session/child)")
                && source.contains("Done: Fixed the flicker (PR #600)"),
            "{source}"
        );
    }

    #[test]
    fn reports_from_a_todos_agent_land_under_that_todo() {
        let (fixture, note_id) = from_note();
        fixture
            .store()
            .update(&note_id, &Author::Cli, |note| {
                diri_notes::store::append_markdown(
                    note,
                    "- [ ] Draft the launch posts\n  - Tone: plain",
                );
                let index = handoff::find_todo(
                    note,
                    &handoff::TodoSelector::Text("Draft the launch posts".into()),
                )
                .expect("to-do");
                handoff::link_session(note, index, "@Codex", "child");
                Ok(())
            })
            .unwrap();
        let report = fixture
            .bridge("child")
            .call(
                "report_to_parent",
                &json!({"summary": "Drafted post 1 of 3", "status": "update"}),
            )
            .unwrap();
        assert!(report["todo"].is_u64(), "{report}");
        let source = fixture.store().load(&note_id).unwrap().to_markdown();
        assert!(
            source.contains(
                "- [ ] Draft the launch posts [@Codex](diri://session/child)\n  - Tone: plain\n  - [@"
            ) && source.contains("Drafted post 1 of 3"),
            "{source}"
        );
        assert!(!source.contains("## Updates"), "{source}");
    }

    #[test]
    fn a_note_is_not_an_agent_target_and_its_children_act_as_roots() {
        let (fixture, _) = from_note();
        let denied = fixture
            .bridge("child")
            .call(
                "send_prompt",
                &json!({"session_id": "s_note", "text": "hi"}),
            )
            .unwrap_err();
        assert!(denied.contains("is a note, not an agent"), "{denied}");
        // Started from a note, `child` is a root: it may add to any note.
        let other = fixture.note("Unrelated", None, "x");
        fixture
            .bridge("child")
            .call("write_note", &json!({"note": other, "append": "ok"}))
            .unwrap();
        // `grandchild` is delegated: its origin note yes, others no.
        fixture
            .bridge("grandchild")
            .call("write_note", &json!({"note": "origin", "append": "ok"}))
            .unwrap();
        let denied = fixture
            .bridge("grandchild")
            .call("write_note", &json!({"note": other, "append": "no"}))
            .unwrap_err();
        assert!(denied.contains("write_note denied"), "{denied}");
    }

    #[test]
    fn spawn_note_separates_unavailable_from_rejected() {
        let missing = Bridge::new(PathBuf::from("/nonexistent/diri.sock"), None);
        assert!(matches!(
            missing.spawn_note("/tmp", "t", "", None),
            Ok(NoteSpawn::Unavailable(_))
        ));

        let old = Peer::new(
            |_| json!({"__error": {"code": "not_found", "message": "no manifest for agent \"note\""}}),
        );
        let bridge = Bridge::new(old.path.clone(), None);
        assert!(matches!(
            bridge.spawn_note("/tmp", "t", "", None),
            Ok(NoteSpawn::Unavailable(reason)) if reason.contains("predates")
        ));

        let rejecting = Peer::new(
            |_| json!({"__error": {"code": "bad_request", "message": "cwd is not a directory"}}),
        );
        let bridge = Bridge::new(rejecting.path.clone(), None);
        assert!(bridge.spawn_note("/nope", "t", "", None).is_err());

        let created = note_session("s_new", "n-new", Some("parent"));
        let current = Peer::new(move |_| serde_json::to_value(&created).unwrap());
        let bridge = Bridge::new(current.path.clone(), None);
        let Ok(NoteSpawn::Created(record)) = bridge.spawn_note("/tmp", "t", "body", Some("parent"))
        else {
            panic!("expected a note session");
        };
        assert_eq!(record.note_id.as_deref(), Some("n-new"));

        let shell = super::super::tests::record("s_shell", None);
        let wrong = Peer::new(move |_| serde_json::to_value(&shell).unwrap());
        let bridge = Bridge::new(wrong.path.clone(), None);
        assert!(bridge.spawn_note("/tmp", "t", "", None).is_err());
    }

    #[test]
    fn create_note_makes_a_note_under_the_caller_and_can_open_it() {
        let made = note_session("s_new", "n-new", Some("root"));
        let (fixture, calls) = Fixture::scripted(sessions(), move |method| match method {
            "session.spawn" => Some(serde_json::to_value(&made).unwrap()),
            "session.reveal" => Some(json!({})),
            _ => None,
        });
        let result = fixture
            .bridge("root")
            .call(
                "create_note",
                &json!({"title": "How sign-in works", "markdown": "## In short\n\nIt uses a magic link.", "open": true}),
            )
            .unwrap();
        assert_eq!(result["session_id"], "s_new");
        assert_eq!(result["opened"], true);
        let calls = calls.lock().unwrap().clone();
        let spawn = calls
            .iter()
            .position(|m| m == "session.spawn")
            .expect("spawned");
        let reveal = calls
            .iter()
            .position(|m| m == "session.reveal")
            .expect("revealed");
        assert!(spawn < reveal, "{calls:?}");
    }

    #[test]
    fn entries_go_under_the_agents_todo_or_into_updates() {
        let (fixture, note_id) = from_note();
        let store = fixture.store();
        store
            .append(
                &note_id,
                "- [ ] Find the venue\n- [ ] Book flights",
                &Author::Cli,
            )
            .unwrap();
        store
            .update(&note_id, &Author::Cli, |note| {
                let index = handoff::find_todo(note, &TodoSelector::Text("venue".into())).unwrap();
                handoff::link_session(note, index, "@codex: child", "child");
                Ok(())
            })
            .unwrap();
        let bridge = fixture.bridge("child");
        bridge
            .call(
                "write_note",
                &json!({"note": "origin", "entry": "Decision: the Hall, 300 seats."}),
            )
            .unwrap();
        bridge
            .call("write_note", &json!({"note": "origin", "todo": "flights", "checked": true, "entry": "Booked for the 12th."}))
            .unwrap();
        let text = store.load(&note_id).unwrap().to_markdown();
        assert!(text.contains("- [ ] Find the venue [@codex: child](diri://session/child)\n  - [@Codex](diri://session/child) "), "{text}");
        assert!(text.contains("Decision: the Hall, 300 seats."), "{text}");
        assert!(
            text.contains("- [x] Book flights\n  - [@Codex](diri://session/child) "),
            "{text}"
        );
        let long = "word ".repeat(200);
        let refused = bridge
            .call("write_note", &json!({"note": "origin", "entry": long}))
            .unwrap_err();
        assert!(refused.contains("create_note"), "{refused}");
    }

    #[test]
    fn start_from_note_spawns_under_the_note_and_links_the_todo() {
        let notes = tempfile::tempdir().unwrap();
        let (note_id, _) = NoteStore::open(notes.path())
            .unwrap()
            .create(Document::new("Offsite", Vec::new()), Some("/work/diri"))
            .unwrap();
        NoteStore::open(notes.path())
            .unwrap()
            .append(&note_id, "- [ ] Find a venue", &Author::User)
            .unwrap();
        let mut note = note_session("s_note", &note_id, None);
        note.cwd = "/work/diri".into();
        let spawned = json!({
            "id": "s_worker", "ok": true,
            "spawn_receipt": {"session_id": "s_worker", "outcome": "completed"},
        });
        let (mut fixture, calls) = Fixture::scripted(
            vec![note, super::super::tests::record("root", None)],
            move |method| match method {
                "agent.readiness" => Some(json!({"agents": []})),
                "session.spawn_tracked" => Some(spawned.clone()),
                _ => None,
            },
        );
        fixture.notes = notes;
        let result = fixture
            .bridge("root")
            .call(
                "start_from_note",
                &json!({"note": "Offsite", "todo": "venue", "kind": "codex"}),
            )
            .unwrap();
        assert_eq!(result["note_session"], "s_note");
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|m| m == "session.spawn_tracked")
        );
        let text = fixture.store().load(&note_id).unwrap().to_markdown();
        assert!(
            text.contains("- [ ] Find a venue [@Codex](diri://session/s_worker)"),
            "{text}"
        );

        // A delegated agent may not start work from someone else's note.
        let other_notes = tempfile::tempdir().unwrap();
        let (other, _) = NoteStore::open(other_notes.path())
            .unwrap()
            .create(Document::new("Someone else's", Vec::new()), None)
            .unwrap();
        let mut denied = Fixture::new(vec![
            note_session("s_note", &other, None),
            super::super::tests::record("root", None),
            super::super::tests::record("deep", Some("root")),
        ]);
        denied.notes = other_notes;
        let error = denied
            .bridge("deep")
            .call(
                "start_from_note",
                &json!({"note": "s_note", "kind": "codex"}),
            )
            .unwrap_err();
        assert!(error.contains("start_from_note denied"), "{error}");
    }

    #[test]
    fn note_history_is_readable_but_not_restorable_by_agents() {
        let fixture = Fixture::new(sessions());
        let id = fixture.note("Plan", None, "first");
        fixture
            .bridge("root")
            .call("write_note", &json!({"note": id, "append": "second"}))
            .unwrap();
        let history = fixture
            .bridge("root")
            .call("note_history", &json!({"note": id}))
            .unwrap();
        let versions = history["versions"].as_array().unwrap();
        assert!(versions.len() >= 2, "{history}");
        assert_eq!(versions[0]["by"], "root");
        let version = versions.last().unwrap()["version"].as_u64().unwrap();
        let old = fixture
            .bridge("root")
            .call("note_history", &json!({"note": id, "version": version}))
            .unwrap();
        assert!(!old["markdown"].as_str().unwrap().contains("second"));
        assert!(
            crate::tools::tool_definitions_for(&[])
                .iter()
                .all(|tool| !tool.name.contains("restore")),
            "restoring stays with the person"
        );
    }

    const TRACKER: &str = "PRs in flight:\n\n| PR | State |\n| --- | --- |\n| #561 | Done |\n| #562 | Fix |\n\n## Status\n\nWaiting on review.\n\n## Notes\n\nKeep this.";

    #[test]
    fn edit_note_changes_a_table_row_copied_from_read_note() {
        let fixture = Fixture::new(sessions());
        let id = fixture.note("Release tracker", None, TRACKER);
        let bridge = fixture.bridge("root");
        let read = bridge.call("read_note", &json!({"note": id})).unwrap();
        let markdown = read["markdown"].as_str().unwrap();
        assert!(markdown.starts_with("# Release tracker\n"), "{markdown}");
        assert!(
            !markdown.contains("id:"),
            "front matter is not part of the text"
        );
        assert!(
            read["path"]
                .as_str()
                .unwrap()
                .ends_with(&format!("{id}.md"))
        );
        // Copy the row exactly as read_note shows it.
        let row = markdown.lines().find(|l| l.contains("#562")).unwrap();
        let result = bridge
            .call(
                "edit_note",
                &json!({"note": id, "old_string": row, "new_string": row.replace("Fix", "Done")}),
            )
            .unwrap();
        assert_eq!(result["replacements"], 1);
        assert!(
            result["changed"].as_str().unwrap().contains("#562 | Done"),
            "{result}"
        );
        let after = bridge.call("read_note", &json!({"note": id})).unwrap();
        // Tables are re-tidied after an edit; the row reads Done.
        let squash = |s: &str| s.split_whitespace().collect::<String>();
        let new_row = after["markdown"]
            .as_str()
            .unwrap()
            .lines()
            .find(|l| l.contains("#562"))
            .unwrap()
            .to_owned();
        assert_eq!(squash(&new_row), squash(&row.replace("Fix", "Done")));

        // History: the version before, then the agent's change.
        let history = bridge.call("note_history", &json!({"note": id})).unwrap();
        assert_eq!(history["versions"][0]["version"], result["version"]);
        assert_eq!(history["versions"][0]["by"], "root");
        let before = history["versions"][1]["version"].as_u64().unwrap();
        let old = bridge
            .call("note_history", &json!({"note": id, "version": before}))
            .unwrap();
        assert!(old["markdown"].as_str().unwrap().contains("#562 | Fix"));
    }

    #[test]
    fn edit_note_explains_misses_and_repeats() {
        let fixture = Fixture::new(sessions());
        let id = fixture.note("Release tracker", None, TRACKER);
        let bridge = fixture.bridge("root");
        let missing = bridge
            .call(
                "edit_note",
                &json!({"note": id, "old_string": "waiting on REVIEW", "new_string": "x"}),
            )
            .unwrap_err();
        assert!(
            missing.contains("not found") && missing.contains("Waiting on review."),
            "{missing}"
        );
        let twice = bridge
            .call(
                "edit_note",
                &json!({"note": id, "old_string": "| #56", "new_string": "| PR #56"}),
            )
            .unwrap_err();
        assert!(twice.contains("appears 2 times"), "{twice}");
        bridge
            .call("edit_note", &json!({"note": id, "old_string": "| #56", "new_string": "| PR #56", "replace_all": true}))
            .unwrap();
        // Deleting is an empty new_string.
        bridge
            .call(
                "edit_note",
                &json!({"note": id, "old_string": "\n\nKeep this.", "new_string": ""}),
            )
            .unwrap();
        let text = fixture.store().load(&id).unwrap();
        assert!(!diri_notes::text_edit::body(&text).contains("Keep this"));
    }

    #[test]
    fn replace_section_swaps_what_is_under_a_heading() {
        let fixture = Fixture::new(sessions());
        let id = fixture.note("Release tracker", None, TRACKER);
        let bridge = fixture.bridge("root");
        bridge
            .call("replace_section", &json!({"note": id, "heading": "Status", "markdown": "Shipped in 0.9.\n\n- [x] tag the release"}))
            .unwrap();
        let body = diri_notes::text_edit::body(&fixture.store().load(&id).unwrap());
        assert!(
            body.contains(
                "## Status\n\nShipped in 0.9.\n\n- [x] tag the release\n\n## Notes\n\nKeep this."
            ),
            "{body}"
        );
        let missing = bridge
            .call(
                "replace_section",
                &json!({"note": id, "heading": "Risks", "markdown": "x"}),
            )
            .unwrap_err();
        assert!(missing.contains("## Status"), "{missing}");
    }

    #[test]
    fn delegated_agents_edit_only_their_own_notes() {
        let (fixture, origin) = from_note();
        fixture
            .store()
            .append(&origin, "Draft: Fix", &Author::Cli)
            .unwrap();
        let other = fixture.note("Someone else's", None, "Draft: Fix");
        // `grandchild` is delegated (its parent is an agent) but descends from the note.
        fixture
            .bridge("grandchild")
            .call(
                "edit_note",
                &json!({"note": "origin", "old_string": "Draft: Fix", "new_string": "Draft: Done"}),
            )
            .unwrap();
        let denied = fixture
            .bridge("grandchild")
            .call(
                "edit_note",
                &json!({"note": other, "old_string": "Draft: Fix", "new_string": "x"}),
            )
            .unwrap_err();
        assert!(denied.contains("denied"), "{denied}");
        let denied = fixture
            .bridge("grandchild")
            .call(
                "replace_section",
                &json!({"note": other, "heading": "Someone else's", "markdown": "x"}),
            )
            .unwrap_err();
        assert!(denied.contains("denied"), "{denied}");
        // A root agent (the person's own chat) may edit any note.
        fixture
            .bridge("stranger")
            .call(
                "edit_note",
                &json!({"note": other, "old_string": "Draft: Fix", "new_string": "Draft: Done"}),
            )
            .unwrap();
    }

    #[test]
    fn a_second_edit_reuses_the_first_new_string_on_a_re_aligned_row() {
        let fixture = Fixture::new(sessions());
        let id = fixture.note("Release tracker", None, TRACKER);
        let bridge = fixture.bridge("root");
        let markdown = bridge.call("read_note", &json!({"note": id})).unwrap()["markdown"]
            .as_str()
            .unwrap()
            .to_owned();
        let row = markdown.lines().find(|l| l.contains("#562")).unwrap();
        let first = bridge
            .call(
                "edit_note",
                &json!({"note": id, "old_string": row, "new_string": "| #562 | Done |"}),
            )
            .unwrap();
        assert!(first.get("matched").is_none(), "{first}");
        // The agent reuses its own new_string; Diri has re-aligned the row.
        let second = bridge
            .call("edit_note", &json!({"note": id, "old_string": "| #562 | Done |", "new_string": "| #562 | Shipped |"}))
            .unwrap();
        assert!(
            second["matched"]
                .as_str()
                .unwrap()
                .contains("ignoring table spacing"),
            "{second}"
        );
        let after = bridge.call("read_note", &json!({"note": id})).unwrap();
        let row = after["markdown"]
            .as_str()
            .unwrap()
            .lines()
            .find(|l| l.contains("#562"))
            .unwrap()
            .to_owned();
        assert_eq!(row.split_whitespace().collect::<String>(), "|#562|Shipped|");
    }
}
