use super::{CommandSpec, ProcessRunner};
use crate::{Error, Result};

pub(crate) fn require_openssl(runner: &dyn ProcessRunner) -> Result<String> {
    let guidance = "Install OpenSSL 3 or newer and put its bin directory on PATH; see docs/quickstart.md#1-prerequisites in this Core checkout";
    let banner = runner
        .run_and_wait(&CommandSpec::new("openssl").arg("version"))
        .map_err(|error| {
            Error::PrerequisiteMissing(format!("could not check OpenSSL: {error}. {guidance}"))
        })?;
    let mut words = banner.split_whitespace();
    let name = words.next();
    let mut version = words.next().unwrap_or_default().split('.');
    let parts = (
        version.next().and_then(|value| value.parse::<u32>().ok()),
        version.next().and_then(|value| value.parse::<u32>().ok()),
        version.next().and_then(|value| value.parse::<u32>().ok()),
        version.next(),
    );
    if name != Some("OpenSSL") || !matches!(parts, (Some(3..), Some(_), Some(_), None)) {
        return Err(Error::PrerequisiteMissing(format!(
            "Native Host JWT keys require OpenSSL 3 or newer; found {}. {guidance}",
            banner.trim()
        )));
    }
    Ok(banner.trim().to_owned())
}
