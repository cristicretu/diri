//! Main-thread AppKit responder integration. Opt in with
//! `DIRI_TEST_APPKIT_GESTURE=1 cargo test -p diri-app --test tab_gesture_appkit`.
#[cfg(target_os = "macos")]
#[allow(dead_code, unused_imports)]
#[path = "../src/peek_settle.rs"]
mod peek_settle;
#[cfg(target_os = "macos")]
#[allow(dead_code, unused_imports)]
#[path = "../src/macos/tab_gesture.rs"]
mod tab_gesture;
#[cfg(target_os = "macos")]
#[allow(dead_code, unused_imports)]
#[path = "../src/tab_peek.rs"]
mod tab_peek;
fn main() {
    #[cfg(target_os = "macos")]
    if std::env::var_os("DIRI_TEST_APPKIT_GESTURE").is_some() {
        tab_gesture::verify_appkit_bridge();
        println!(
            "AppKit gesture responder ownership and contact-delivery regression passed; contacts are synthetic."
        );
        return;
    }
    println!("AppKit gesture regression skipped; opt in on macOS with DIRI_TEST_APPKIT_GESTURE=1.");
}
