#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

// Keep binary-only crate attributes outside the app implementation, which is
// also included by the native AppKit harnesses.
include!("main.rs");
