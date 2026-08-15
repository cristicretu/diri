//! Dirijor's per-user execution node.
//!
//! A node keeps provider credentials on the machine where they are used and
//! exposes a small, versioned management interface over an encrypted private
//! network. Terminal transport remains independent from this crate.

pub mod accounts;
pub mod checkpoint;
pub mod config;
pub mod error;
pub mod portable;
pub mod provider;
pub mod server;
pub mod service;
pub mod usage;

pub use config::{NodeConfig, NodePaths};
pub use error::{NodeError, NodeResult};
pub use server::NodeServer;
pub use service::NodeService;

pub const NODE_BUILD: &str = concat!("diri-node-", env!("CARGO_PKG_VERSION"));
