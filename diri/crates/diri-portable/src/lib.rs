//! Portable Diri profile sync and Mac-to-Mac migration.
//!
//! Pure library: no Engine, no GPUI. The Engine supplies the state it owns
//! (session records, schedules, projects, preferences) and applies what
//! comes back. Nothing here ever reads credentials, sockets, Holder state or
//! terminal swap files: export is an allowlist ([`classify`]).

pub mod bundle;
pub mod classify;
pub mod docs;
pub mod files;
pub mod merge;
pub mod migrate;
pub mod origin;
pub mod pathmap;
pub mod sessions;
pub mod sync;
pub mod transcript;
