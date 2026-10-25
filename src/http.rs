//! See docs/commands.md for authenticated loopback requests and Development reload.

use crate::{Error, Result};
use std::time::Duration;

pub trait HttpClient {
    // Keep the bearer separate from the URL and omit it from diagnostics.
    fn post_json(&self, url: &str, bearer: Option<&str>, body: &str) -> Result<String>;
    fn publish_module(&self, url: &str, bearer: &str, wasm: &[u8]) -> Result<()>;
}

pub struct LoopbackHttpClient;

impl LoopbackHttpClient {
    fn request(
        &self,
        method: &str,
        url: &str,
        bearer: Option<&str>,
        content_type: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<String> {
        let authority = url
            .strip_prefix("http://")
            .and_then(|rest| rest.split('/').next());
        if !authority
            .and_then(|value| value.parse::<std::net::SocketAddr>().ok())
            .is_some_and(|address| address.ip().is_loopback() && address.port() != 0)
        {
            return Err(Error::Http(
                "HTTP requests require an explicit loopback address and port".into(),
            ));
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(timeout))
            .build()
            .into();
        let mut request = ureq::http::Request::builder()
            .method(method)
            .uri(url)
            .header("Content-Type", content_type)
            .header("Connection", "close");
        if let Some(bearer) = bearer {
            request = request.header("Authorization", format!("Bearer {bearer}"));
        }
        let request = request
            .body(body)
            .map_err(|_| Error::Http("cannot construct loopback request".into()))?;
        let unknown = if method == "PUT" {
            "; the update outcome may be unknown"
        } else {
            ""
        };
        let mut response = agent.run(request).map_err(|error| {
            let reason = match error {
                ureq::Error::Io(cause) => format!("{:?}", cause.kind()),
                ureq::Error::Timeout(_) => "request timed out".into(),
                _ => "HTTP request failed".into(),
            };
            Error::Http(format!(
                "cannot complete request to {url}: {reason}{unknown}"
            ))
        })?;
        let status = response.status();
        let body = response
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .read_to_string()
            .map_err(|_| Error::Http(format!("cannot read response from {url}{unknown}")))?;
        if status.is_success() {
            return Ok(body);
        }
        let detail = if let Some(bearer) = bearer {
            body.replace(bearer, "[redacted]")
        } else {
            body
        };
        let outcome = if status.is_server_error() {
            unknown
        } else {
            ""
        };
        Err(Error::Http(format!(
            "{url} answered HTTP {}{outcome}: {}",
            status.as_u16(),
            detail.trim()
        )))
    }
}

impl HttpClient for LoopbackHttpClient {
    fn post_json(&self, url: &str, bearer: Option<&str>, body: &str) -> Result<String> {
        self.request(
            "POST",
            url,
            bearer,
            "application/json",
            body.as_bytes(),
            Duration::from_secs(15),
        )
    }

    fn publish_module(&self, url: &str, bearer: &str, wasm: &[u8]) -> Result<()> {
        let body = self.request(
            "PUT",
            url,
            Some(bearer),
            "application/wasm",
            wasm,
            Duration::from_secs(180),
        )?;
        let reply: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
            Error::Http(
                "publication returned an invalid response; the update outcome may be unknown"
                    .into(),
            )
        })?;
        if reply.get("EnvironmentError").is_some() || reply.get("PermissionDenied").is_some() {
            return Err(Error::Http(format!(
                "publication refused: {}",
                body.replace(bearer, "[redacted]")
            )));
        }
        if !matches!(reply["Success"]["op"].as_str(), Some("created" | "updated")) {
            return Err(Error::Http(
                "publication did not confirm success; the update outcome may be unknown".into(),
            ));
        }
        Ok(())
    }
}

pub fn json_field(body: &str, field: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get(field)?
        .as_str()
        .map(str::to_owned)
}

#[cfg(test)]
pub mod fake {
    //! A recording HTTP client, and a real one-shot responder to exercise the socket path.

    use super::*;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Request {
        pub method: &'static str,
        pub url: String,
        pub bearer: Option<String>,
        pub body: String,
    }

    /// The identity a fake node mints. Distinctive so a test can assert it is absent from
    /// everything rendered, logged or serialized.
    pub const MINTED_TOKEN: &str = "eyJtaW50ZWQ.SERVER-ISSUED-must-never-be-rendered.SIG";
    pub const MINTED_IDENTITY: &str = "c200deadbeef";

    #[derive(Clone, Default)]
    pub struct FakeHttp {
        requests: Arc<Mutex<Vec<Request>>>,
        failure: Option<String>,
        /// Refuse only the requests whose URL contains this — one reducer, not the whole node.
        refused: Option<String>,
    }

    impl FakeHttp {
        pub fn new() -> Self {
            Self::default()
        }

        /// A node that refuses every request with `message` — e.g. a `claim_operator` that a
        /// different identity already claimed.
        pub fn failing(message: &str) -> Self {
            Self {
                failure: Some(message.to_string()),
                ..Self::default()
            }
        }

        /// A node that answers everything except the requests whose URL contains `needle`, which it
        /// refuses with `message`. For one database that rejects a call the rest of the run makes
        /// happily — `set_gm_level` on a shard that has never heard of the character, say.
        pub fn refusing(needle: &str, message: &str) -> Self {
            Self {
                failure: Some(message.to_string()),
                refused: Some(needle.to_string()),
                ..Self::default()
            }
        }

        pub fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }

        /// Just the mint requests, which is what "did this run mint a NEW identity?" asks.
        pub fn mints(&self) -> Vec<Request> {
            self.requests()
                .into_iter()
                .filter(|r| r.url.ends_with("/v1/identity"))
                .collect()
        }
    }

    impl HttpClient for FakeHttp {
        fn publish_module(&self, url: &str, bearer: &str, wasm: &[u8]) -> Result<()> {
            let result = self.post_json(url, Some(bearer), &String::from_utf8_lossy(wasm));
            self.requests.lock().unwrap().last_mut().unwrap().method = "PUT";
            result.map(|_| ())
        }

        fn post_json(&self, url: &str, bearer: Option<&str>, body: &str) -> Result<String> {
            self.requests.lock().unwrap().push(Request {
                method: "POST",
                url: url.to_string(),
                bearer: bearer.map(str::to_string),
                body: body.to_string(),
            });
            if let Some(message) = &self.failure {
                let refused = self
                    .refused
                    .as_ref()
                    .is_none_or(|n| url.contains(n.as_str()));
                if refused {
                    return Err(Error::Http(format!("{url} answered HTTP 400: {message}")));
                }
            }
            Ok(if url.ends_with("/v1/identity") {
                format!(r#"{{"identity":"{MINTED_IDENTITY}","token":"{MINTED_TOKEN}"}}"#)
            } else {
                String::new()
            })
        }
    }

    pub struct FakeNode {
        pub url: String,
        pub received: Arc<Mutex<String>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeNode {
        /// Answer exactly one request with `status` and `body`.
        pub fn answering_once(status: &str, body: &str) -> Self {
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            Self::raw_response(response)
        }

        pub fn raw_response(response: String) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let received = Arc::new(Mutex::new(String::new()));
            let sink = received.clone();
            let handle = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                // Drain the declared body before replying so closing the socket cannot reset the request.
                let mut seen = Vec::new();
                let mut byte = [0_u8; 1];
                while socket.read(&mut byte).unwrap_or(0) == 1 {
                    seen.push(byte[0]);
                    if seen.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&seen).into_owned();
                let length: usize = head
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value)
                    })
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
                let mut body_bytes = vec![0_u8; length];
                let _ = socket.read_exact(&mut body_bytes);
                *sink.lock().unwrap() = head + &String::from_utf8_lossy(&body_bytes);
                let _ = socket.write_all(response.as_bytes());
            });
            Self {
                url,
                received,
                handle: Some(handle),
            }
        }

        pub fn request(&self) -> String {
            self.received.lock().unwrap().clone()
        }
    }

    impl Drop for FakeNode {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeNode;
    use super::*;

    #[test]
    fn authenticated_json_is_framed_and_the_token_stays_out_of_the_url() {
        let node = FakeNode::answering_once("200 OK", r#"{"token":"example"}"#);
        let url = format!("{}/v1/identity", node.url);
        let body = LoopbackHttpClient
            .post_json(&url, Some("secret"), "[]")
            .unwrap();
        assert_eq!(json_field(&body, "token").as_deref(), Some("example"));
        let request = node.request().to_lowercase();
        assert!(request.starts_with("post /v1/identity http/1.1\r\n"));
        assert!(request.contains("authorization: bearer secret\r\n"));
        assert!(request.ends_with("\r\n\r\n[]"));
        assert!(!url.contains("secret"));
    }

    #[test]
    fn publication_sends_the_whole_module_and_decodes_chunked_confirmation() {
        let reply = r#"{"Success":{"domain":"lyracore","database_identity":"c200000000000000000000000000000000000000000000000000000000000001","op":"updated"}}"#;
        let chunks = reply
            .as_bytes()
            .chunks(3)
            .map(|part| {
                format!(
                    "{:x}\r\n{}\r\n",
                    part.len(),
                    std::str::from_utf8(part).unwrap()
                )
            })
            .collect::<String>();
        let node = FakeNode::raw_response(format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{chunks}0\r\n\r\n"));
        let wasm = vec![b'X'; 34 * 1024 * 1024];
        LoopbackHttpClient
            .publish_module(
                &format!(
                    "{}/v1/database/lyracore?clear=false&policy=Compatible",
                    node.url
                ),
                "secret",
                &wasm,
            )
            .unwrap();
        let request = node.request();
        assert!(request
            .starts_with("PUT /v1/database/lyracore?clear=false&policy=Compatible HTTP/1.1\r\n"));
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(head
            .to_lowercase()
            .contains("content-type: application/wasm\r\n"));
        assert_eq!(body.as_bytes(), wasm);
    }

    #[test]
    fn creation_confirmation_matches_the_host_reply() {
        let node = FakeNode::answering_once(
            "200 OK",
            r#"{"Success":{"domain":"lyracore","database_identity":"c200000000000000000000000000000000000000000000000000000000000001","op":"created"}}"#,
        );
        LoopbackHttpClient
            .publish_module(
                &format!("{}/v1/database/lyracore", node.url),
                "secret",
                b"wasm",
            )
            .unwrap();
    }

    #[test]
    fn publication_does_not_follow_a_redirect_to_another_listener() {
        let destination = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let node = FakeNode::raw_response(format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{}/other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            destination.local_addr().unwrap(),
        ));
        let error = LoopbackHttpClient
            .publish_module(
                &format!("{}/v1/database/lyracore", node.url),
                "secret",
                b"wasm",
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("307"));
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn publication_requires_a_success_reply() {
        for (status, body) in [
            ("400 Bad Request", "incompatible definition"),
            ("200 OK", "{}"),
            ("302 Found", "moved"),
            ("500 Internal Server Error", "failed confirmation"),
        ] {
            let node = FakeNode::answering_once(status, body);
            let result = LoopbackHttpClient.publish_module(
                &format!("{}/v1/database/lyracore", node.url),
                "secret",
                b"wasm",
            );
            let error = result.unwrap_err().to_string();
            assert!(!error.contains("secret"));
            if status.starts_with("400") {
                assert!(error.contains("incompatible definition"));
                assert!(!error.contains("unknown"));
            } else if status.starts_with("500") || status.starts_with("200") {
                assert!(error.contains("unknown"));
            }
        }
    }

    #[test]
    fn lost_response_has_unknown_outcome() {
        let node = FakeNode::raw_response(String::new());
        let error = LoopbackHttpClient
            .publish_module(
                &format!("{}/v1/database/lyracore", node.url),
                "secret",
                b"wasm",
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown"));
        assert!(!error.contains("secret"));
        assert!(node.request().starts_with("PUT "));
    }

    #[test]
    fn a_failed_reducer_is_a_known_refusal() {
        let node = FakeNode::answering_once("530 Reducer Failed", "operator only");
        let error = LoopbackHttpClient
            .post_json(
                &format!("{}/v1/database/lyracore/call/claim_operator", node.url),
                Some("secret"),
                "[]",
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("530"));
        assert!(error.contains("operator only"));
        assert!(!error.contains("unknown"));
    }

    #[test]
    fn transport_errors_keep_the_io_kind_without_request_headers() {
        let node = FakeNode::raw_response(String::new());
        let error = LoopbackHttpClient
            .post_json(&node.url, Some("secret"), "[]")
            .unwrap_err()
            .to_string();
        assert!(error.contains("UnexpectedEof"));
        assert!(!error.contains("secret"));
        assert!(!error.contains("unknown"));
    }

    #[test]
    fn non_loopback_and_named_destinations_are_refused_before_sending() {
        for url in [
            "https://127.0.0.1:3000/",
            "http://example.com:3000/",
            "http://192.0.2.1:3000/",
            "http://127.0.0.1:0/",
        ] {
            assert!(LoopbackHttpClient
                .post_json(url, Some("secret"), "")
                .unwrap_err()
                .to_string()
                .contains("loopback"));
        }
    }

    #[test]
    fn error_responses_cannot_echo_the_credential() {
        let node = FakeNode::answering_once("401 Unauthorized", "bad token secret");
        let error = LoopbackHttpClient
            .post_json(&node.url, Some("secret"), "")
            .unwrap_err()
            .to_string();
        assert!(error.contains("401"));
        assert!(!error.contains("secret"));
    }
}
