use diri_proto::{
    AgentDescriptor, AgentKind, AgentReadinessResult, AttachRequest, ClientRole, ControlMessage,
    DateMillis, EventName, EventsSubscribeParams, ExitReason, HostInitializeParams, Method,
    ReadScrollbackCellsResult, SessionDiffBase, SessionId, SessionListResult,
    SessionReadDiffParams, SessionReadDiffResult, SessionStatus, StateSnapshotResult,
    WorktreeListResult,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

const FIXTURES: &[&str] = &[
    include_str!("fixtures/hello_response.json"),
    include_str!("fixtures/session_list_response.json"),
    include_str!("fixtures/state_snapshot_response.json"),
    include_str!("fixtures/agent_readiness_response.json"),
    include_str!("fixtures/worktree_list_response.json"),
];

#[test]
fn live_control_fixtures_round_trip_semantically() {
    for fixture in FIXTURES {
        let first: ControlMessage = serde_json::from_str(fixture).expect("fixture must decode");
        let encoded = serde_json::to_vec(&first).expect("message must encode");
        let second: ControlMessage =
            serde_json::from_slice(&encoded).expect("encoded message must decode");
        assert_eq!(first, second);
    }
}

#[test]
fn live_results_decode_as_typed_payloads() {
    let sessions: SessionListResult = fixture_ok(FIXTURES[1]);
    assert_eq!(sessions.sessions.len(), 2);
    assert_eq!(sessions.sessions[0].status, SessionStatus::Working);
    assert!(matches!(
        sessions.sessions[1].status,
        SessionStatus::NeedsInput(_)
    ));
    typed_round_trip(&sessions);

    let snapshot: StateSnapshotResult = fixture_ok(FIXTURES[2]);
    assert_eq!(snapshot.sessions.len(), 1);
    typed_round_trip(&snapshot);

    let readiness: AgentReadinessResult = fixture_ok(FIXTURES[3]);
    assert_eq!(readiness.agents.len(), 4);
    assert!(!readiness.agents[3].available());
    typed_round_trip(&readiness);

    let worktrees: WorktreeListResult = fixture_ok(FIXTURES[4]);
    assert_eq!(worktrees.len(), 4);
    typed_round_trip(&worktrees);
}

#[test]
fn swift_associated_value_shapes_match_real_data() {
    assert_eq!(
        serde_json::to_value(AgentKind::generic("my-agent")).unwrap(),
        json!({"generic": {"command": "my-agent"}})
    );
    // The five kinds that predate manifest-backed AgentKind keep their exact
    // legacy case keys: a session list from any build must decode in any other.
    for (kind, expected) in [
        (AgentKind::CLAUDE_CODE, "claudeCode"),
        (AgentKind::CODEX, "codex"),
        (AgentKind::CURSOR, "cursor"),
        (AgentKind::GEMINI, "gemini"),
        (AgentKind::SHELL, "shell"),
    ] {
        assert_eq!(
            serde_json::to_value(&kind).unwrap(),
            json!({ expected: {} }),
        );
        assert_eq!(
            serde_json::from_value::<AgentKind>(json!({ expected: {} })).unwrap(),
            kind
        );
    }
    // A manifest agent with no legacy case travels under "agent" and keeps its
    // id, so the descriptor catalog can still name and style it.
    let amp = AgentKind::new("amp");
    assert_eq!(
        serde_json::to_value(&amp).unwrap(),
        json!({"agent": {"id": "amp"}})
    );
    assert_eq!(
        serde_json::from_value::<AgentKind>(json!({"agent": {"id": "amp"}})).unwrap(),
        amp
    );
    // The Rust Engine's live manifest catalog uses the manifest id directly.
    assert_eq!(
        serde_json::from_value::<AgentKind>(json!("amp")).unwrap(),
        amp
    );

    let exited: SessionStatus = serde_json::from_value(json!({
        "exited": {"_0": {"reason": "daemonRestart"}}
    }))
    .unwrap();
    assert!(matches!(
        exited,
        SessionStatus::Exited(ref info) if info.reason == ExitReason::DaemonRestart
    ));
    assert_eq!(
        serde_json::to_value(exited).unwrap(),
        json!({"exited": {"_0": {"reason": "daemonRestart"}}})
    );
}

#[test]
fn additive_unknown_variants_fall_back_and_unknown_fields_are_ignored() {
    // An unrecognized case key is read as a manifest id rather than discarded:
    // a newer daemon's agent stays identifiable even to a client that has never
    // heard of it.
    let kind: AgentKind = serde_json::from_value(json!({"futureAgent": {"mode": 2}})).unwrap();
    assert_eq!(kind.id(), "futureAgent");
    // Only a payload we can't extract an id from collapses to Unknown.
    let empty: AgentKind = serde_json::from_value(json!({"agent": {}})).unwrap();
    assert_eq!(empty, AgentKind::UNKNOWN);

    let status: SessionStatus =
        serde_json::from_value(json!({"waitingOnNetwork": {"retry": 3}})).unwrap();
    assert_eq!(status, SessionStatus::Unknown);

    let role: ClientRole = serde_json::from_value(json!("tablet")).unwrap();
    assert_eq!(role, ClientRole::Unknown);

    let worktree: diri_proto::WorktreeInfo = serde_json::from_value(json!({
        "path": "/tmp/wt",
        "branch": "main",
        "isBare": false,
        "isDetached": false,
        "isPrunable": false,
        "futureField": true
    }))
    .unwrap();
    assert_eq!(worktree.path, "/tmp/wt");
}

#[test]
fn setup_metadata_is_additive_for_old_and_new_catalog_entries() {
    let old: AgentDescriptor = serde_json::from_value(json!({
        "id": "amp",
        "displayName": "Amp"
    }))
    .expect("old daemon descriptor");
    assert_eq!(old.setup, None);

    let guided: AgentDescriptor = serde_json::from_value(json!({
        "id": "amp",
        "displayName": "Amp",
        "setup": {
            "url": "https://ampcode.com/manual",
            "installHint": "Install Amp.",
            "signInHint": "Run amp and sign in."
        }
    }))
    .expect("new descriptor");
    let setup = guided.setup.as_ref().expect("setup metadata");
    assert_eq!(setup.url.as_deref(), Some("https://ampcode.com/manual"));
    typed_round_trip(&guided);
}

#[test]
fn host_initialization_defaults_to_non_destructive_version_ensure() {
    let compatible: HostInitializeParams =
        serde_json::from_value(json!({ "host": "forge" })).expect("old request shape");
    assert_eq!(compatible.host, "forge");
    assert!(!compatible.force_reinstall);

    let reinstall: HostInitializeParams = serde_json::from_value(json!({
        "host": "forge",
        "forceReinstall": true
    }))
    .expect("reinstall request");
    assert!(reinstall.force_reinstall);
}

#[test]
fn epoch_milliseconds_and_swift_data_base64_round_trip() {
    let date: DateMillis = serde_json::from_str("1784728215930.502").unwrap();
    assert_eq!(
        serde_json::to_value(date).unwrap(),
        json!(1784728215930.502)
    );

    let cells: ReadScrollbackCellsResult = serde_json::from_value(json!({
        "payload": "AAECA/8=",
        "firstRow": 1,
        "rowCount": 2,
        "totalRows": 3,
        "liveStartRow": 2,
        "cols": 80,
        "contentSeq": 4
    }))
    .unwrap();
    assert_eq!(cells.payload, vec![0, 1, 2, 3, 255]);
    assert_eq!(
        serde_json::to_value(cells).unwrap()["payload"],
        json!("AAECA/8=")
    );
}

#[test]
fn method_name_set_is_complete() {
    let methods = [
        Method::HELLO,
        Method::SESSION_SPAWN,
        Method::SESSION_LIST,
        Method::SESSION_KILL,
        Method::SESSION_REMOVE,
        Method::SESSION_RENAME,
        Method::SESSION_RESUME,
        Method::SESSION_SEND_TEXT,
        Method::SESSION_RESIZE,
        Method::SESSION_READ_SCREEN,
        Method::SESSION_READ_SCROLLBACK,
        Method::SESSION_READ_SCROLLBACK_CELLS,
        Method::SESSION_READ_DIFF,
        Method::SESSION_MARK_SEEN,
        Method::SESSION_HIBERNATE,
        Method::SESSION_WAKE,
        Method::SESSION_ARCHIVE,
        Method::SESSION_UNARCHIVE,
        Method::SESSION_REOPEN_LAST,
        Method::SESSION_HISTORY,
        Method::SESSION_RESUME_FROM_HISTORY,
        Method::WORKTREE_CREATE,
        Method::WORKTREE_LIST,
        Method::WORKTREE_REMOVE,
        Method::WORKTREE_OVERVIEW,
        Method::PROJECT_ADD,
        Method::CLIENT_SET_ACTIVE,
        Method::GOVERNOR_CONFIGURE,
        Method::AGENT_READINESS,
        Method::EVENTS_SUBSCRIBE,
        Method::EVENTS_WAIT,
        Method::HOOK_REPORT,
        Method::TEST_RUN,
        Method::STATE_SNAPSHOT,
        Method::DAEMON_PREPARE_SHUTDOWN,
        Method::DAEMON_SHUTDOWN_IF_IDLE,
        Method::DAEMON_SHUTDOWN,
    ];
    assert_eq!(methods.len(), 37);
    assert_eq!(methods[0], "hello");
    assert_eq!(methods.last().copied(), Some("daemon.shutdown"));
}

#[test]
fn session_read_diff_matches_swift_data_and_field_names() {
    let params = SessionReadDiffParams {
        session_id: SessionId::new("s_remote"),
        base: Some(SessionDiffBase::Head),
    };
    let encoded_params = serde_json::to_value(&params).unwrap();
    assert_eq!(encoded_params["sessionID"], json!("s_remote"));
    assert_eq!(encoded_params["base"], json!("head"));
    typed_round_trip(&params);

    let result = SessionReadDiffResult {
        patch: b"diff --git a/a b/a\n+hello\n".to_vec(),
        repo_root: "/srv/app".to_owned(),
        truncated: false,
        base_ref: Some("origin/main".to_owned()),
    };
    let encoded = serde_json::to_value(&result).unwrap();
    assert_eq!(
        encoded["patch"],
        json!("ZGlmZiAtLWdpdCBhL2EgYi9hCitoZWxsbwo=")
    );
    assert_eq!(encoded["repoRoot"], json!("/srv/app"));
    assert_eq!(encoded["baseRef"], json!("origin/main"));
    typed_round_trip(&result);
}

#[test]
fn envelope_discrimination_and_attach_legacy_default_match_swift() {
    let event: ControlMessage =
        serde_json::from_value(json!({"event": "session.updated", "seq": 9})).unwrap();
    assert_eq!(
        event,
        ControlMessage::Event {
            name: "session.updated".into(),
            seq: 9,
            params: Value::Null,
        }
    );
    assert_eq!(
        serde_json::to_value(event).unwrap(),
        json!({"event": "session.updated", "seq": 9, "params": null})
    );

    let attach: AttachRequest = serde_json::from_value(json!({"attach": "s_123"})).unwrap();
    assert_eq!(attach.role, ClientRole::Desktop);
    assert_eq!(
        serde_json::to_value(attach).unwrap(),
        json!({"attach": "s_123", "role": "desktop"})
    );
}

#[test]
fn spawn_params_host_field_is_wire_compatible() {
    // Absent on the wire ⇒ None (legacy peers), and None never serializes.
    let legacy: diri_proto::SessionSpawnParams =
        serde_json::from_value(json!({"kind": {"shell": {}}, "cwd": "/tmp"})).unwrap();
    assert_eq!(legacy.host, None);
    let encoded = serde_json::to_value(&legacy).unwrap();
    assert!(encoded.get("host").is_none());

    let remote = diri_proto::SessionSpawnParams {
        host: Some("forge".into()),
        ..legacy
    };
    let encoded = serde_json::to_value(&remote).unwrap();
    assert_eq!(encoded["host"], json!("forge"));
    typed_round_trip(&remote);
}

#[test]
fn migration_and_host_methods_use_the_swift_wire_names() {
    // session.migrate: sessionID spelling, targetHost skip-if-none.
    let to_local = diri_proto::SessionMigrateParams {
        session_id: diri_proto::SessionId::new("s_1"),
        target_host: None,
    };
    assert_eq!(
        serde_json::to_value(&to_local).unwrap(),
        json!({"sessionID": "s_1"})
    );
    let to_forge: diri_proto::SessionMigrateParams =
        serde_json::from_value(json!({"sessionID": "s_1", "targetHost": "forge"})).unwrap();
    assert_eq!(to_forge.target_host.as_deref(), Some("forge"));
    typed_round_trip(&to_forge);

    // host.sync_prefs result: per-tool reports, error skip-if-none.
    let report: diri_proto::HostSyncPrefsResult = serde_json::from_value(json!({
        "tools": [
            {"tool": "claude", "ok": true, "synced": ["CLAUDE.md", "commands"]},
            {"tool": "codex", "ok": false, "synced": [], "error": "rsync is not installed on Forge"},
        ]
    }))
    .unwrap();
    assert!(report.tools[0].ok && report.tools[0].error.is_none());
    assert_eq!(
        report.tools[1].error.as_deref(),
        Some("rsync is not installed on Forge")
    );
    typed_round_trip(&report);

    // host.locate_repo: originURL spelling, both fields optional.
    let locate = diri_proto::HostLocateRepoParams {
        host: Some("forge".into()),
        origin_url: None,
        session_id: Some(diri_proto::SessionId::new("s_1")),
    };
    assert_eq!(
        serde_json::to_value(&locate).unwrap(),
        json!({"host": "forge", "sessionID": "s_1"})
    );
    let found: diri_proto::HostLocateRepoResult = serde_json::from_value(
        json!({"path": "/home/cristi/code/anara", "originURL": "git@github.com:anara/anara.git"}),
    )
    .unwrap();
    assert_eq!(found.path.as_deref(), Some("/home/cristi/code/anara"));
    let miss: diri_proto::HostLocateRepoResult = serde_json::from_value(json!({})).unwrap();
    assert_eq!(miss, diri_proto::HostLocateRepoResult::default());

    // Repo-preserving spawn field: sameRepoAs spelling, absent ⇒ None.
    let legacy: diri_proto::SessionSpawnParams =
        serde_json::from_value(json!({"kind": {"shell": {}}, "cwd": "/tmp"})).unwrap();
    assert_eq!(legacy.same_repo_as, None);
    let preserving = diri_proto::SessionSpawnParams {
        same_repo_as: Some(diri_proto::SessionId::new("s_ref")),
        ..legacy
    };
    assert_eq!(
        serde_json::to_value(&preserving).unwrap()["sameRepoAs"],
        json!("s_ref")
    );
}

#[test]
fn session_record_host_field_is_wire_compatible() {
    // Existing fixtures predate the field: they decode to None and stay
    // byte-compatible on re-encode (skip_serializing_if).
    let sessions: SessionListResult = fixture_ok(FIXTURES[1]);
    assert!(
        sessions
            .sessions
            .iter()
            .all(|session| session.host.is_none())
    );

    let mut record = sessions.sessions[0].clone();
    record.host = Some("forge".into());
    let encoded = serde_json::to_value(&record).unwrap();
    assert_eq!(encoded["host"], json!("forge"));
    typed_round_trip(&record);
}

/// The subscribe filter is a wire contract with a Swift daemon, and the exact
/// key names are what makes it take effect. A daemon that predates the filter
/// ignores unknown keys and streams everything, so a mis-spelled key fails
/// OPEN — the client keeps working while quietly paying the full firehose
/// forever. Pin the shape so that regression is loud instead of invisible.
#[test]
fn events_subscribe_filter_serializes_the_keys_the_daemon_reads() {
    let params = EventsSubscribeParams {
        since_seq: Some(42),
        sessions: None,
        kinds: Some(vec![
            EventName::SESSION_UPDATED.to_string(),
            EventName::SESSION_REMOVED.to_string(),
            EventName::PROJECT_UPDATED.to_string(),
        ]),
    };
    assert_eq!(
        serde_json::to_value(&params).expect("params must encode"),
        json!({
            "sinceSeq": 42,
            "kinds": ["session.updated", "session.removed", "project.updated"],
        })
    );
    // Absent filters are omitted rather than sent as null, so a fresh
    // subscription is byte-identical to what pre-filter clients sent.
    assert_eq!(
        serde_json::to_value(EventsSubscribeParams::default()).expect("params must encode"),
        json!({})
    );
}

fn fixture_ok<T: DeserializeOwned>(fixture: &str) -> T {
    let message: ControlMessage = serde_json::from_str(fixture).unwrap();
    match message {
        ControlMessage::Response {
            result: Ok(value), ..
        } => serde_json::from_value(value).unwrap(),
        other => panic!("expected successful response, got {other:?}"),
    }
}

fn typed_round_trip<T>(value: &T)
where
    T: DeserializeOwned + PartialEq + Serialize + std::fmt::Debug,
{
    let encoded = serde_json::to_vec(value).unwrap();
    let decoded: T = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(value, &decoded);
}
