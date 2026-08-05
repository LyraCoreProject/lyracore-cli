//! The source-first LyraCore development CLI (#251).
//!
//! Scope is one contributor's machine: a loopback, single-database fixture. Multi-shard, region,
//! seam, and transfer topology is Phase C's and is deliberately untouched here.

pub mod cmd;
pub mod error;
pub mod http;
pub mod proc;
pub mod project;
pub mod state;
pub mod token;

pub use error::{Error, Result};
