//! The two loopback HTTP requests this CLI makes to the SpacetimeDB node itself.
//!
//! Everything else the CLI does goes through the `spacetime` binary — deliberately, because the
//! CLI's config layout is its own business. Two things cannot: minting a **server-issued**
//! identity (`POST /v1/identity`) and calling `claim_operator` **as that identity**. Neither has a
//! `spacetime` sub-command that does not first write a token into the contributor's global
//! `cli.toml`, and rewriting a global credential is not something `lyracore dev up` may do.
//!
//! Scope is exactly that: loopback, plaintext, two small JSON requests. There is no HTTPS, no
//! redirect following, no keep-alive and no dependency — `http://127.0.0.1:3000` is the only
//! address this ever talks to (`--lan` never moves the database), so a TLS stack would be
//! unreachable code and a new dependency in a crate that is `cargo install`ed from git on a
//! contributor's first run.
//!
//! ponytail: a hand-written HTTP/1.1 request rather than reqwest/ureq. Ceiling — it speaks only
//! what these two endpoints answer with (`Connection: close`, and a body it does not have to
//! frame-decode; see [`json_field`]). Upgrade path: if this ever needs a third endpoint with a
//! streaming or chunk-critical response, take the dependency.

use crate::{Error, Result};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// A node that is up answers these instantly; one that is wedged must not hang `dev up` forever.
const TIMEOUT: Duration = Duration::from_secs(15);

pub trait HttpClient {
    /// `POST url` with a JSON body, optionally authenticated as `bearer`, returning the response
    /// body.
    ///
    /// `bearer` is a separate argument for the same reason a password is in
    /// [`ProcessRunner::run_with_secret_stdin`](crate::proc::ProcessRunner::run_with_secret_stdin):
    /// it can then be kept out of every error this returns, because no error is ever built from
    /// the whole request.
    fn post_json(&self, url: &str, bearer: Option<&str>, body: &str) -> Result<String>;
}

pub struct LoopbackHttpClient;

impl HttpClient for LoopbackHttpClient {
    fn post_json(&self, url: &str, bearer: Option<&str>, body: &str) -> Result<String> {
        let (authority, path) = split_url(url)?;
        let address = authority
            .to_socket_addrs()
            .map_err(|e| Error::Process(format!("cannot resolve {authority}: {e}")))?
            .next()
            .ok_or_else(|| Error::Process(format!("cannot resolve {authority}")))?;

        let mut stream = TcpStream::connect_timeout(&address, TIMEOUT)
            .map_err(|e| Error::Process(format!("cannot reach {url}: {e}")))?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;

        // Built as bytes and written once: the Authorization line exists only inside this buffer,
        // which is never rendered, logged, or put in an error.
        let mut request = format!(
            "POST {path} HTTP/1.1\r\nHost: {authority}\r\nAccept: application/json\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if let Some(bearer) = bearer {
            request.push_str(&format!("Authorization: Bearer {bearer}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);

        stream
            .write_all(request.as_bytes())
            .map_err(|e| Error::Process(format!("cannot send to {url}: {e}")))?;
        stream.flush()?;

        // `Connection: close` means the response ends at EOF, so this needs no framing.
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .map_err(|e| Error::Process(format!("no answer from {url}: {e}")))?;
        interpret(url, &String::from_utf8_lossy(&response))
    }
}

/// Split `http://host:port/path` into the authority and the path.
fn split_url(url: &str) -> Result<(&str, &str)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| Error::Process(format!("{url} is not a plaintext http:// URL")))?;
    match rest.find('/') {
        Some(slash) => Ok((&rest[..slash], &rest[slash..])),
        None => Ok((rest, "/")),
    }
}

/// Turn a raw HTTP response into a body, or into an error that quotes the node's own complaint.
fn interpret(url: &str, response: &str) -> Result<String> {
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| Error::Process(format!("{url} answered with a truncated HTTP response")))?;
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| Error::Process(format!("{url} answered without an HTTP status line")))?;

    if (200..300).contains(&status) {
        return Ok(body.to_string());
    }
    // The node's message is the actionable half ("operator already claimed"), so it is quoted —
    // and it is the node's, not ours: nothing here can contain the credential we sent.
    Err(Error::Process(format!(
        "{url} answered HTTP {status}: {}",
        body.trim().lines().next().unwrap_or("(no detail)")
    )))
}

/// Read one string field out of a small JSON object.
///
/// Deliberately a scan rather than a parse: it is indifferent to whatever framing the body arrives
/// in (a chunked response interleaves hex lengths with the JSON), and these two endpoints return a
/// flat object of strings. A missing field reads as `None`, which callers turn into an actionable
/// error rather than a bogus credential.
pub fn json_field(body: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\"");
    let after_key = &body[body.find(&key)? + key.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?.trim_start();
    let value = after_colon.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(value[..end].to_string())
}

#[cfg(test)]
pub mod fake {
    //! A recording HTTP client, and a real one-shot responder to exercise the socket path.

    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Request {
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
        fn post_json(&self, url: &str, bearer: Option<&str>, body: &str) -> Result<String> {
            self.requests.lock().unwrap().push(Request {
                url: url.to_string(),
                bearer: bearer.map(str::to_string),
                body: body.to_string(),
            });
            if let Some(message) = &self.failure {
                return Err(Error::Process(format!(
                    "{url} answered HTTP 400: {message}"
                )));
            }
            Ok(if url.ends_with("/v1/identity") {
                format!(r#"{{"identity":"{MINTED_IDENTITY}","token":"{MINTED_TOKEN}"}}"#)
            } else {
                String::new()
            })
        }
    }

    /// A real HTTP responder on a loopback port of its own, so [`LoopbackHttpClient`] can be
    /// tested end to end over a socket — without a SpacetimeDB node, and without ever touching
    /// :3000, which on a developer machine is somebody's live realm.
    pub struct FakeNode {
        pub url: String,
        pub received: Arc<Mutex<String>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeNode {
        /// Answer exactly one request with `status` and `body`.
        pub fn answering_once(status: &str, body: &str) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let received = Arc::new(Mutex::new(String::new()));
            let sink = received.clone();
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let handle = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                // Read the head, then exactly the declared body. Reading to EOF would deadlock
                // (the client keeps its half open for the answer), and answering BEFORE draining
                // the body would reset the connection under the client's feet.
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
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
                let mut body_bytes = vec![0_u8; length];
                let _ = socket.read_exact(&mut body_bytes);
                *sink.lock().unwrap() = head;
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
    fn a_url_splits_into_an_authority_and_a_path() {
        assert_eq!(
            split_url("http://127.0.0.1:3000/v1/identity").unwrap(),
            ("127.0.0.1:3000", "/v1/identity")
        );
        assert_eq!(
            split_url("http://127.0.0.1:3000").unwrap(),
            ("127.0.0.1:3000", "/")
        );
        // The database is loopback plaintext in every mode; an https:// URL here would mean
        // something upstream got the topology wrong, so it is refused rather than downgraded.
        assert!(split_url("https://example.com/v1/identity").is_err());
    }

    #[test]
    fn a_string_field_is_read_out_of_a_small_json_object() {
        let body = r#"{"identity":"c200","token":"eyJhbGciOiJFUzI1NiJ9.P.S"}"#;
        assert_eq!(
            json_field(body, "token").as_deref(),
            Some("eyJhbGciOiJFUzI1NiJ9.P.S")
        );
        assert_eq!(json_field(body, "identity").as_deref(), Some("c200"));
        assert_eq!(json_field(body, "absent"), None);
    }

    #[test]
    fn a_chunk_framed_body_still_yields_its_fields() {
        // The reason this is a scan and not a parse: a chunked response interleaves hex lengths
        // with the JSON, and `serde_json` would reject the whole thing.
        let chunked = "31\r\n{\"identity\":\"c200\",\"token\":\"eyJ.P.S\"}\r\n0\r\n\r\n";
        assert_eq!(json_field(chunked, "token").as_deref(), Some("eyJ.P.S"));
    }

    #[test]
    fn a_response_without_a_body_or_a_status_is_an_error_not_a_credential() {
        assert!(interpret("http://node/v1/identity", "").is_err());
        assert!(interpret("http://node/v1/identity", "garbage\r\n\r\n{}").is_err());
        assert!(interpret("http://node/v1/identity", "HTTP/1.1 200 OK\r\n\r\n{}").is_ok());
    }

    #[test]
    fn a_refusal_quotes_the_nodes_own_message() {
        let error = interpret(
            "http://127.0.0.1:3000/v1/database/lyracore/call/claim_operator",
            "HTTP/1.1 400 Bad Request\r\n\r\noperator already claimed",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("operator already claimed"),
            "{error}"
        );
        assert!(error.to_string().contains("400"), "{error}");
    }

    // ---- the socket path, against a responder of our own ----

    #[test]
    fn the_real_client_posts_json_and_reads_the_body_back() {
        let node = FakeNode::answering_once("200 OK", r#"{"identity":"c2","token":"eyJ.P.S"}"#);
        let body = LoopbackHttpClient
            .post_json(&format!("{}/v1/identity", node.url), None, "")
            .unwrap();
        assert_eq!(json_field(&body, "token").as_deref(), Some("eyJ.P.S"));

        let request = node.request();
        assert!(
            request.starts_with("POST /v1/identity HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(request.contains("Connection: close"), "{request}");
        // Unauthenticated: minting an identity is precisely the request you have no credential for.
        assert!(!request.contains("Authorization"), "{request}");
    }

    #[test]
    fn a_bearer_is_sent_as_a_header_and_never_as_part_of_the_url() {
        const TOKEN: &str = "eyJ.SECRET.SIG";
        let node = FakeNode::answering_once("200 OK", "{}");
        let url = format!("{}/v1/database/lyracore/call/claim_operator", node.url);
        LoopbackHttpClient
            .post_json(&url, Some(TOKEN), "[]")
            .unwrap();

        let request = node.request();
        assert!(
            request.contains(&format!("Authorization: Bearer {TOKEN}\r\n")),
            "{request}"
        );
        // The request line — the part that lands in a proxy log — carries no credential.
        let request_line = request.lines().next().unwrap();
        assert!(!request_line.contains(TOKEN), "{request_line}");
        assert!(!url.contains(TOKEN), "{url}");
    }

    #[test]
    fn an_http_failure_names_the_url_and_carries_no_credential() {
        const TOKEN: &str = "eyJ.SECRET.SIG";
        let node = FakeNode::answering_once("401 Unauthorized", "invalid token");
        let url = format!("{}/v1/database/lyracore/call/claim_operator", node.url);
        let error = LoopbackHttpClient
            .post_json(&url, Some(TOKEN), "[]")
            .unwrap_err();

        assert!(error.to_string().contains("401"), "{error}");
        assert!(
            !error.to_string().contains(TOKEN),
            "the credential leaked into an error: {error}"
        );
    }

    #[test]
    fn a_node_that_is_not_listening_is_a_clean_error() {
        // Bind and drop, so the port is one nothing answers on — and it is never :3000.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/identity", dead.local_addr().unwrap());
        drop(dead);
        let error = LoopbackHttpClient.post_json(&url, None, "").unwrap_err();
        assert!(error.to_string().contains("cannot reach"), "{error}");
    }
}
