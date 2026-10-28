//! Core v2 prototype (feature `core-v2`, not used by the Engine): an 8-byte
//! cell, per-page style and grapheme tables, history living in the same
//! pages as the screen, row reuse without per-cell resets, and a width
//! lookup fused with UTF-8 decoding. See `docs/core-v2.md`.

mod bridge;
pub mod cell;
pub mod check;
pub mod page;
pub mod term;
pub mod text;

pub use term::Core;

#[cfg(test)]
mod tests;
