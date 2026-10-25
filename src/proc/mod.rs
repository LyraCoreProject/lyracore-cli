mod command;
mod inspect;
mod openssl;
mod runner;

#[cfg(test)]
pub mod fake;

pub use command::CommandSpec;
pub use inspect::{start_signature, ProcessInspector, RealProcessInspector};
pub(crate) use openssl::require_openssl;
pub use runner::{ProcessRunner, RealProcessRunner};
