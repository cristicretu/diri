use super::*;
use crate::session::SessionSpec;
use crate::status::Authority;
use diri_proto::grid::GridRowCodec;

#[test]
fn find_capture_is_bounded_atomic_and_pinned_to_the_original_session_owner() {
    let temp = tempfile::tempdir().unwrap();
    let server = server(temp.path());
    let spawn = || {
        server.registry.lock().unwrap().spawn(SessionSpec {
            id: "find-fixture".into(),
            pty: crate::pty::PtySpec::new(vec!["/bin/sh".into(), "-c".into(),
                "i=0; while [ $i -lt 6000 ]; do printf '0123456789012345678901234567890123456789\\r\\n'; i=$((i+1)); done; printf 'CAPTURE READY'; read line".into()], temp.path()).size(40, 30),
            manifest_id: "shell".into(), authority: Authority::ProcessOnly,
            logs_dir: temp.path().join("logs"), holder: None, remote: None, defer_launch: false,
        }, test_record("find-fixture")).unwrap();
    };
    spawn();
    // Readiness is not what this test measures; the budget only has to
    // outlast 6000 lines through a debug-build parser on a loaded machine.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let ready = server
            .registry
            .lock()
            .unwrap()
            .get("find-fixture")
            .unwrap()
            .read_scrollback()
            .lines
            .iter()
            .any(|line| line.contains("CAPTURE READY"));
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fixture never printed CAPTURE READY into retained history"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let value = server
        .session_capture_find(Some(json!({"sessionID":"find-fixture"})))
        .unwrap();
    assert!(serde_json::to_vec(&value).unwrap().len() < MAX_CONTROL_LINE_BYTES - 1024);
    let first: diri_proto::CaptureFindResult = serde_json::from_value(value).unwrap();
    assert!(first.partial);
    assert!(
        first.cells.row_count as usize * first.cells.cols as usize
            <= diri_proto::FIND_CAPTURE_MAX_CELLS
    );
    assert_eq!(
        first.cells.first_row + first.cells.row_count,
        first.cells.total_rows
    );
    assert_eq!(first.cells.total_rows - first.cells.live_start_row, 30);
    let decoded =
        GridRowCodec::decode_rows(&first.cells.payload, first.cells.row_count as usize).unwrap();
    assert!(decoded.iter().any(|row| {
        row.iter()
            .filter_map(|cell| char::from_u32(cell.scalar))
            .collect::<String>()
            .contains("CAPTURE READY")
    }));
    let reader = server
        .registry
        .lock()
        .unwrap()
        .get("find-fixture")
        .unwrap()
        .scrollback_reader();
    server
        .registry
        .lock()
        .unwrap()
        .remove("find-fixture", &temp.path().join("logs"))
        .unwrap();
    spawn();
    // The old reader owns its original immutable state even though the same
    // public SessionId now resolves to another PTY. It cannot capture the new one.
    let old = reader.capture_find().unwrap();
    let new: diri_proto::CaptureFindResult = serde_json::from_value(
        server
            .session_capture_find(Some(json!({"sessionID":"find-fixture"})))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first.owner, old.owner);
    assert!(old.capture_revision > first.capture_revision);
    assert_ne!(old.owner, new.owner);
    server
        .registry
        .lock()
        .unwrap()
        .remove("find-fixture", &temp.path().join("logs"))
        .unwrap();
}

#[test]
fn find_history_reaches_rows_older_than_the_capture() {
    let temp = tempfile::tempdir().unwrap();
    let server = server(temp.path());
    server.registry.lock().unwrap().spawn(SessionSpec {
        id: "deep-find".into(),
        pty: crate::pty::PtySpec::new(vec!["/bin/sh".into(), "-c".into(),
            "printf 'OLDEST needle\\r\\n'; i=0; while [ $i -lt 12000 ]; do printf 'hay %d\\r\\n' $i; i=$((i+1)); done; printf 'DEEP READY'; read line".into()], temp.path()).size(80, 24),
        manifest_id: "shell".into(), authority: Authority::ProcessOnly,
        logs_dir: temp.path().join("logs"), holder: None, remote: None, defer_launch: false,
    }, test_record("deep-find")).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !server
        .registry
        .lock()
        .unwrap()
        .get("deep-find")
        .unwrap()
        .read_scrollback()
        .lines
        .iter()
        .any(|line| line.contains("DEEP READY"))
    {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture never finished"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let capture: diri_proto::CaptureFindResult = serde_json::from_value(
        server
            .session_capture_find(Some(json!({"sessionID":"deep-find"})))
            .unwrap(),
    )
    .unwrap();
    assert!(capture.partial, "the capture holds only the newest rows");
    let origin = capture
        .history_origin
        .expect("local captures carry an origin");
    // Continue below the capture, page by page, as the app does.
    let mut before = Some(origin + capture.cells.first_row);
    let mut found = Vec::new();
    while let Some(row) = before {
        let page: diri_proto::find::FindHistoryResult = serde_json::from_value(
            server
                .session_find_history(Some(json!({
                    "sessionID": "deep-find", "query": "needle", "beforeRow": row,
                    "cols": capture.cells.cols, "maxMatches": 64, "budgetMs": 20
                })))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(page.cols, 80);
        found.extend(page.matches);
        before = page.next_before_row;
    }
    // The command line echo may also match; the printed line is the oldest.
    let oldest = found.last().expect("the oldest line is found");
    assert!(oldest[0] < origin + capture.cells.first_row);
    assert_eq!(&oldest[1..], &[7, 13]);
    server
        .registry
        .lock()
        .unwrap()
        .remove("deep-find", &temp.path().join("logs"))
        .unwrap();
}
