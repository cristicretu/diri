use super::*;
use diri_proto::terminal_input::{Key, KeyAction, KeypadKey, Modifiers, NamedKey};
use std::time::Instant;

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "key fixture progress");
        std::thread::sleep(Duration::from_millis(2));
    }
}

struct KeyFixture {
    server: Arc<ControlServer>,
    temp: tempfile::TempDir,
}
impl Drop for KeyFixture {
    fn drop(&mut self) {
        let _ = self
            .server
            .registry
            .lock()
            .unwrap()
            .terminate("keys", Duration::ZERO);
    }
}
impl KeyFixture {
    fn new(count: usize) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let server = server(temp.path());
        let script = format!(
            "stty raw -echo; printf '\\033[?1h\\033=\\033[?2004hready'; dd bs=1 count={count} of=received 2>/dev/null; printf done"
        );
        server
            .registry
            .lock()
            .unwrap()
            .spawn(
                crate::session::SessionSpec {
                    id: "keys".into(),
                    pty: crate::pty::PtySpec::new(
                        vec!["/bin/sh".into(), "-c".into(), script],
                        temp.path(),
                    )
                    .size(80, 24),
                    manifest_id: "shell".into(),
                    authority: crate::status::Authority::ProcessOnly,
                    logs_dir: temp.path().join("logs"),
                    holder: None,
                    remote: None,
                    defer_launch: false,
                },
                test_record("keys"),
            )
            .unwrap();
        wait_for(|| {
            server
                .registry
                .lock()
                .unwrap()
                .get("keys")
                .unwrap()
                .keyboard_state()
                .is_some_and(|state| state.application_cursor_keys && state.application_keypad)
        });
        Self { server, temp }
    }
    fn key(&self, key: Key, modifiers: Modifiers, action: KeyAction) -> ControlMessage {
        call(
            &self.server,
            Method::SESSION_SEND_KEY,
            Some(
                serde_json::to_value(diri_proto::SendKeyParams {
                    session_id: diri_proto::SessionId("keys".into()),
                    key,
                    modifiers,
                    action,
                })
                .unwrap(),
            ),
        )
    }
}

#[test]
fn key_rpc_uses_current_modes_and_keeps_enter_separate_from_bracketed_paste() {
    let expected = b"\x1bOA\x1bOH\x1bOq\r\x03\x1b[15;4~A";
    let fixture = KeyFixture::new(expected.len());
    assert_eq!(
        err_of(fixture.key(
            Key::Named(NamedKey::Enter),
            Modifiers::default(),
            KeyAction::Release
        ))
        .code,
        "unsupported_key_action"
    );
    assert_eq!(
        err_of(fixture.key(
            Key::Character("two keys".into()),
            Modifiers::default(),
            KeyAction::Press
        ))
        .code,
        "bad_request"
    );
    let cases = [
        (Key::Named(NamedKey::ArrowUp), Modifiers::default(), 3),
        (Key::Named(NamedKey::Home), Modifiers::default(), 3),
        (Key::Keypad(KeypadKey::One), Modifiers::default(), 3),
        (Key::Named(NamedKey::Enter), Modifiers::default(), 1),
        (
            Key::Character("c".into()),
            Modifiers {
                ctrl: true,
                ..Default::default()
            },
            1,
        ),
        (
            Key::Named(NamedKey::F5),
            Modifiers {
                shift: true,
                alt: true,
                ..Default::default()
            },
            7,
        ),
        (
            Key::Character("a".into()),
            Modifiers {
                shift: true,
                ..Default::default()
            },
            1,
        ),
    ];
    for (key, modifiers, count) in cases {
        assert_eq!(
            ok_of(fixture.key(key, modifiers, KeyAction::Press))["bytesAccepted"],
            count
        );
    }
    let path = fixture.temp.path().join("received");
    wait_for(|| std::fs::read(&path).is_ok_and(|data| data.len() == expected.len()));
    assert_eq!(std::fs::read(path).unwrap(), expected);
    wait_for(|| {
        matches!(
            fixture
                .server
                .registry
                .lock()
                .unwrap()
                .get("keys")
                .unwrap()
                .status(),
            diri_proto::SessionStatus::Exited(_)
        )
    });
    let error = err_of(fixture.key(
        Key::Named(NamedKey::Enter),
        Modifiers::default(),
        KeyAction::Press,
    ));
    assert!(error.message.contains("exit"), "{error:?}");
}

#[test]
fn unknown_session_and_invalid_actions_have_no_input_side_effects() {
    let temp = tempfile::tempdir().unwrap();
    let server = server(temp.path());
    let error = err_of(call(
        &server,
        Method::SESSION_SEND_KEY,
        Some(json!({
            "sessionID": "missing", "key": {"named":"enter"}
        })),
    ));
    assert_eq!(error.code, "not_found");
    let error = err_of(call(
        &server,
        Method::SESSION_SEND_KEY,
        Some(json!({
            "sessionID": "missing", "key": {"character":"\n"}
        })),
    ));
    assert_eq!(error.code, "bad_request");
}
