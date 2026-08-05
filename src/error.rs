use thiserror::Error;

/// Exit-code contract:
///
/// | code | meaning                                                         |
/// |------|-----------------------------------------------------------------|
/// | 0    | success (including "already up" and "already down")             |
/// | 1    | the operation failed — a prerequisite, a subprocess, or refusal |
/// | 2    | the invocation was wrong (bad usage, or not inside a checkout)  |
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILURE: i32 = 1;
pub const EXIT_USAGE: i32 = 2;

#[derive(Error, Debug)]
pub enum Error {
    #[error("{0}")]
    ProjectLayout(String),

    #[error("prerequisite missing: {0}")]
    PrerequisiteMissing(String),

    #[error("state error: {0}")]
    State(String),

    #[error("process error: {0}")]
    Process(String),

    /// A failed request to the local SpacetimeDB node (`http.rs`).
    ///
    /// Its `Display` adds no prefix of its own, deliberately: every one of these is wrapped by the
    /// caller that knows what the request was FOR, and a nested "process error: process error:"
    /// would be the only thing a contributor noticed about the message.
    #[error("{0}")]
    Http(String),

    /// `down` found the recorded PID belongs to something else. Never kill it.
    #[error(
        "refusing to stop {component}: PID {pid} is no longer the process this CLI started \
         (recorded {expected:?}, found {found:?}). Clear it with `lyracore dev down --forget`."
    )]
    ForeignProcess {
        component: &'static str,
        pid: u32,
        expected: String,
        found: String,
    },

    #[error("{0}")]
    Usage(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("`{command}` failed with exit code {code}: {message}")]
    SubprocessFailed {
        command: String,
        code: i32,
        message: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::ProjectLayout(_) | Error::Usage(_) => EXIT_USAGE,
            _ => EXIT_FAILURE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_mistakes_and_operational_failures_get_distinct_codes() {
        assert_eq!(Error::Usage("nope".into()).exit_code(), EXIT_USAGE);
        assert_eq!(Error::ProjectLayout("nope".into()).exit_code(), EXIT_USAGE);
        assert_eq!(
            Error::PrerequisiteMissing("nope".into()).exit_code(),
            EXIT_FAILURE
        );
        assert_eq!(Error::Http("nope".into()).exit_code(), EXIT_FAILURE);
        // ...and an Http error adds no prefix of its own, because every one of them is wrapped by
        // a caller that says what the request was for.
        assert_eq!(
            Error::Http("node said no".into()).to_string(),
            "node said no"
        );
        assert_eq!(
            Error::SubprocessFailed {
                command: "spacetime publish".into(),
                code: 3,
                message: "boom".into(),
            }
            .exit_code(),
            EXIT_FAILURE
        );
    }
}
