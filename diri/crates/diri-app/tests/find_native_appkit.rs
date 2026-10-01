//! Opt-in actual AppKit installed-input-handler acceptance on the main thread.
#![allow(dead_code, unused_imports)]
#[cfg(target_os = "macos")]
include!("../src/main.rs");
#[cfg(not(target_os = "macos"))]
fn main() {}
