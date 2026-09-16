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
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
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
        assert!(std::time::Instant::now() < deadline);
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
