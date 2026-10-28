//! Opt-in output-to-photon probe through the real app window, on the AppKit
//! main thread. See `src/app_photon_tests.rs`; enabled by `DIRI_TEST_APP_PHOTON=1`.
#![allow(dead_code, unused_imports)]
#[cfg(target_os = "macos")]
include!("../src/main.rs");
#[cfg(not(target_os = "macos"))]
fn main() {}
