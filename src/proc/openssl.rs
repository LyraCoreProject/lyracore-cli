use super::{CommandSpec, ProcessRunner};
use crate::{Error, Result};

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_supported_version(version: &str) -> bool {
    let (core, suffix) = match version.find(['-', '+']) {
        Some(at) => (&version[..at], Some(&version[at + 1..])),
        None => (version, None),
    };
    let suffix_is_sane = suffix.is_none_or(|text| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-+_~".contains(&byte))
    });
    let parts: Vec<&str> = core.split('.').collect();
    suffix_is_sane
        && parts.len() == 3
        && parts.iter().all(|part| is_number(part))
        && parts[0].parse::<u32>().is_ok_and(|major| major >= 3)
}

pub(crate) fn require_openssl(runner: &dyn ProcessRunner) -> Result<String> {
    let guidance = "Install OpenSSL 3 or newer and put its bin directory on PATH; see docs/quickstart.md#1-prerequisites in this Core checkout";
    let banner = runner
        .run_and_wait(&CommandSpec::new("openssl").arg("version"))
        .map_err(|error| {
            Error::PrerequisiteMissing(format!("could not check OpenSSL: {error}. {guidance}"))
        })?;
    let mut words = banner.split_whitespace();
    if words.next() != Some("OpenSSL") || !words.next().is_some_and(is_supported_version) {
        return Err(Error::PrerequisiteMissing(format!(
            "Native Host JWT keys require OpenSSL 3 or newer; found {}. {guidance}",
            banner.trim()
        )));
    }
    Ok(banner.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::FakeStack;

    const ACCEPTED: &[&str] = &[
        "OpenSSL 3.0.0 7 Sep 2021",
        "OpenSSL 3.6.5",
        "OpenSSL 4.0.0",
        "OpenSSL 3.6.0-dev",
        "OpenSSL 3.1.5+quic",
        "OpenSSL 3.2.0-beta1 (Library: OpenSSL 3.2.0-beta1)",
    ];

    const REFUSED: &[&str] = &[
        "LibreSSL 3.3.6",
        "OpenSSL 1.1.1w",
        "OpenSSL 2.9.9",
        "OpenSSL 3.bad",
        "OpenSSL 3",
        "OpenSSL 3.0",
        "OpenSSL 3.0.0.1",
        "OpenSSL 3..0",
        "OpenSSL 3.0.",
        "OpenSSL 3.0.x",
        "OpenSSL +3.0.0",
        "OpenSSL 3.0.0-",
        "OpenSSL 3.0.0+",
        "OpenSSL 3.0.0-dev!",
        "OpenSSL 3.0.0-dév",
        "OpenSSL 3.0-dev",
        "OpenSSL",
        "unknown",
        "",
    ];

    #[test]
    fn accepts_openssl_three_or_newer_with_a_sane_suffix() {
        for banner in ACCEPTED {
            let stack = FakeStack::new().with_stdout("openssl version", banner);
            assert_eq!(
                require_openssl(&stack.runner()).unwrap(),
                banner.trim(),
                "{banner}"
            );
        }
    }

    #[test]
    fn refuses_other_tools_old_versions_and_malformed_versions() {
        for banner in REFUSED {
            let stack = FakeStack::new().with_stdout("openssl version", banner);
            let error = require_openssl(&stack.runner()).unwrap_err().to_string();
            assert!(error.contains("OpenSSL 3 or newer"), "{banner}: {error}");
        }
    }
}
