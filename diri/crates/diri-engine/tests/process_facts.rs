//! Real one-shot account worker, independent of Holder manager/session launch.
#![cfg(unix)]
use diri_proto::process_facts::ProcessValue;

#[test]
fn native_process_facts_use_the_bounded_existing_holder_binary() {
    let identity = diri_pty::process_identity::observe(std::process::id()).unwrap();
    let helper = std::path::Path::new(env!("CARGO_BIN_EXE_diri-holder"));
    let facts = diri_pty::process_facts::inspect(&identity, |uid| {
        // Functional cold-binary test uses the bounded one-second ceiling.
        // Deterministic worker tests separately prove the interactive timeout.
        diri_pty::process_facts::account::lookup_until(
            helper,
            uid,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        )
    })
    .unwrap();
    assert_eq!(facts.identity, identity);
    let ProcessValue::Available { value: ids } = facts.user_ids else {
        panic!("native UID unavailable")
    };
    let ProcessValue::Available { value: account } = facts.account else {
        panic!("bounded native account unavailable")
    };
    assert_eq!(account.uid, ids.effective);
    assert!(!account.name.is_empty());
    assert!(account.home_directory.starts_with('/'));
    // Never print the actual account/path fields in test or CI logs.
}
