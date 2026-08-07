mod command;
mod inspect;
mod runner;

#[cfg(test)]
pub mod fake;

pub use command::CommandSpec;
pub use inspect::{start_signature, ProcessInspector, RealProcessInspector};
pub use runner::{ProcessRunner, RealProcessRunner};
