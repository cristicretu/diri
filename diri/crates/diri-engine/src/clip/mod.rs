//! Context Ring: recent screen/browser captures and per-tab Session memory.
//! See `diri_proto::clip` for the wire types and `control/clips.rs` for the
//! methods that serve them.

pub mod affinity;
pub mod prompt;
pub mod ring;
pub mod store;
pub mod tab_key;

pub use store::ClipStore;

/// The ring's directory under the Engine's durable state directory.
pub const CLIPS_DIR: &str = "clips";
