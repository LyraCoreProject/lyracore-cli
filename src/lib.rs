//! The source-first LyraCore development CLI (#251).
//!
//! Scope is one contributor's machine: a loopback fixture, sharded across four databases (three
//! since #11, plus the instance pool in #108). Cross-shard transfer topology is Phase C's and is
//! deliberately untouched here.

pub mod cmd;
pub mod config;
pub mod error;
pub mod harness;
pub mod http;
pub mod proc;
pub mod project;
pub mod rls;
pub mod state;
pub mod token;

pub use error::{Error, Result};
