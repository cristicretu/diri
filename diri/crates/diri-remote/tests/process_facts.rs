//! Account worker runs in the actual Helper binary without launching a Holder.
#![cfg(unix)]
use diri_proto::process_facts::ProcessValue;

#[test]
fn remote_helper_account_worker_is_bounded_and_uid_bound() {
    // SAFETY: geteuid reads this fixture process's effective UID.
    let uid = unsafe { libc::geteuid() };
    let helper = std::path::Path::new(env!("CARGO_BIN_EXE_diri-remote"));
    // Functional cold-binary test uses the bounded one-second ceiling.
    let result = diri_pty::process_facts::account::lookup_until(
        helper,
        uid,
        std::time::Instant::now() + std::time::Duration::from_secs(1),
    );
    let ProcessValue::Available { value } = result else {
        panic!("native account unavailable")
    };
    assert_eq!(value.uid, uid);
    assert!(!value.name.is_empty());
    assert!(value.home_directory.starts_with('/'));
}
