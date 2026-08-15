//! Native-workbench adapter for Diri's shared workspace intelligence.
//!
//! The implementation lives in `diri-code-intelligence` so the UI, the
//! Engine's embedded MCP host, and the standalone MCP bridge cross the same
//! containment, indexing, and source-reading boundary.

pub use diri_code_intelligence::*;
