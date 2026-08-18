//! Networked provider adapters, behind the `online` feature.
//!
//! **This module is outside the deterministic envelope, by construction.** It
//! opens sockets, it waits on wall-clock timeouts, and what comes back
//! depends on a service the ship does not control. That is exactly why it is
//! feature-gated and why it sits where it does: an adapter implements
//! [`LlmProvider`](crate::provider::LlmProvider) and nothing else, so it
//! reaches the ship only as text handed to an actor — which the strict
//! [`protocol`](crate::protocol) parses and the safety kernel then rules on,
//! command by command. Nothing in here can reach subsystem state, and the
//! default build does not contain it at all.
//!
//! It adds no dependencies. HTTP/1.1 framing is in [`http`], JSON in
//! [`json`], and the connection itself is a [`HttpTransport`] the embedder
//! supplies — so `Cargo.lock` still holds nothing but the two workspace
//! members with this feature on. [`TcpTransport`] covers plaintext (a local
//! Ollama, a proxy on the same host); **TLS is yours to plug in**, because
//! bringing a TLS stack into this repository would cost the property the
//! whole project is built around.
//!
//! Nothing here retries. A rate limit, a timeout, or a 500 becomes a
//! [`ProviderError`], which becomes a
//! watchdog [`TurnFailure`](crate::watchdog::TurnFailure), which means the
//! autopilot flies that tick. A control loop that blocks waiting for a model
//! to become available again is the failure mode this whole design exists to
//! prevent.

pub mod anthropic;
pub mod http;
pub mod json;
pub mod ollama;
pub mod openai;

pub use anthropic::AnthropicProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAiProvider;

use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use self::http::{HttpError, HttpRequest, HttpResponse, Scheme};
use self::json::Value;
use super::ProviderError;

/// How long a turn may spend connecting and waiting. Generous enough for a
/// reasoning model, short enough that the ship is not left flying blind: past
/// this the watchdog hands the tick to the autopilot.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(120);

/// The connection half of an online adapter — the seam TLS plugs into.
///
/// Framing is not this trait's job: an adapter hands over a fully rendered
/// [`HttpRequest`] and expects an [`HttpResponse`] back. An implementation
/// may do that with a socket, a TLS stack, an HTTP client crate in the
/// embedder's own dependency tree, or a canned answer in a test.
pub trait HttpTransport {
    /// A stable identifier for logs and flight records.
    fn name(&self) -> &str;

    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum TransportError {
    /// This transport cannot reach that endpoint at all — plaintext asked to
    /// speak TLS, say.
    Unsupported(String),
    Connect(String),
    Io(String),
    TimedOut,
    /// The peer answered with something that is not an HTTP response.
    Protocol(HttpError),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Unsupported(detail) => write!(f, "unsupported: {detail}"),
            TransportError::Connect(detail) => write!(f, "connect failed: {detail}"),
            TransportError::Io(detail) => write!(f, "io error: {detail}"),
            TransportError::TimedOut => write!(f, "timed out"),
            TransportError::Protocol(error) => write!(f, "bad http response: {error}"),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<TransportError> for ProviderError {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::TimedOut => ProviderError::TimedOut,
            other => ProviderError::Unavailable(other.to_string()),
        }
    }
}

/// A plaintext HTTP/1.1 transport over `std::net::TcpStream`.
///
/// Speaks `http://` only. Asked for an `https://` endpoint it refuses with
/// [`TransportError::Unsupported`] and says where to plug a TLS transport in
/// — a clear refusal being much better than a ship that thinks it has an
/// encrypted link to its model and does not.
pub struct TcpTransport {
    connect_timeout: Duration,
    io_timeout: Duration,
}

impl TcpTransport {
    pub fn new() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            io_timeout: DEFAULT_IO_TIMEOUT,
        }
    }

    pub fn with_timeouts(connect_timeout: Duration, io_timeout: Duration) -> Self {
        Self {
            connect_timeout,
            io_timeout,
        }
    }
}

impl Default for TcpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport for TcpTransport {
    fn name(&self) -> &str {
        "tcp"
    }

    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        if request.url.scheme == Scheme::Https {
            return Err(TransportError::Unsupported(format!(
                "{} needs TLS; TcpTransport speaks plaintext only — supply an HttpTransport \
                 that does TLS, or point the endpoint at a local proxy",
                request.url.authority()
            )));
        }

        let address = (request.url.host.as_str(), request.url.port)
            .to_socket_addrs()
            .map_err(|error| TransportError::Connect(error.to_string()))?
            .next()
            .ok_or_else(|| {
                TransportError::Connect(format!("no address for {}", request.url.authority()))
            })?;

        let mut stream = TcpStream::connect_timeout(&address, self.connect_timeout)
            .map_err(|error| classify(error, TransportError::Connect))?;
        stream
            .set_read_timeout(Some(self.io_timeout))
            .and_then(|()| stream.set_write_timeout(Some(self.io_timeout)))
            .map_err(|error| classify(error, TransportError::Io))?;

        stream
            .write_all(&request.to_wire())
            .map_err(|error| classify(error, TransportError::Io))?;
        stream
            .flush()
            .map_err(|error| classify(error, TransportError::Io))?;

        // `connection: close` means the read ends when the peer is done.
        let mut raw = Vec::new();
        stream
            .take((http::MAX_BODY_BYTES + MAX_HEAD_BYTES) as u64)
            .read_to_end(&mut raw)
            .map_err(|error| classify(error, TransportError::Io))?;

        http::parse_response(&raw).map_err(TransportError::Protocol)
    }
}

/// Headroom for a status line and headers on top of the body cap.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// A timeout arrives as an ordinary IO error; it has to be told apart from a
/// real one, because only one of them is the watchdog's business.
fn classify(error: std::io::Error, otherwise: fn(String) -> TransportError) -> TransportError {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => TransportError::TimedOut,
        _ => otherwise(error.to_string()),
    }
}

/// Sends `request` and decodes a JSON body, mapping every failure onto a
/// [`ProviderError`]. Shared by all three adapters so they differ only in
/// the part that is genuinely different — the shape of the body they build
/// and the field they read an answer out of.
pub(crate) fn round_trip(
    transport: &mut dyn HttpTransport,
    request: &HttpRequest,
) -> Result<Value, ProviderError> {
    let response = transport.send(request)?;
    let document = json::parse(&response.body)
        .map_err(|error| ProviderError::Unavailable(format!("unreadable response: {error}")))?;

    if !response.is_success() {
        return Err(status_error(response.status, &document));
    }
    Ok(document)
}

/// Turns a non-2xx response into an error that says something useful. Both
/// the Anthropic and OpenAI shapes put a human-readable message at
/// `error.message`; anything else falls back to the status alone rather than
/// inventing a reason.
fn status_error(status: u16, document: &Value) -> ProviderError {
    let detail = document
        .path(&["error", "message"])
        .and_then(Value::as_str)
        .map(|message| format!("HTTP {status}: {message}"))
        .unwrap_or_else(|| format!("HTTP {status}"));

    // 401/403 are the ones an operator can actually fix, and they are not
    // going to resolve themselves on the next tick — but they are still not
    // fatal to the ship, only to this turn.
    ProviderError::Unavailable(detail)
}

/// Concatenates the text of a message that arrived as a list of blocks.
pub(crate) fn join_text_blocks(blocks: &[Value], type_key: &str, text_key: &str) -> String {
    blocks
        .iter()
        .filter(|block| block.get(type_key).and_then(Value::as_str) == Some(text_key))
        .filter_map(|block| block.get(text_key).and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

/// Reads an API key out of the environment. Keys are never held in
/// configuration or in a flight record — a config that carries a secret is a
/// config that leaks one.
pub fn api_key_from_env(variable: &str) -> Result<String, ProviderError> {
    match std::env::var(variable) {
        Ok(key) if !key.trim().is_empty() => Ok(key),
        _ => Err(ProviderError::Unavailable(format!("{variable} is not set"))),
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A transport that answers from a script instead of a socket, and keeps
    /// the requests it was given. Every adapter test runs through this, so
    /// the suite exercises the real request-building and response-parsing
    /// paths while staying entirely offline.
    pub struct StubTransport {
        pub replies: Vec<Result<HttpResponse, TransportError>>,
        pub seen: Vec<HttpRequest>,
    }

    impl StubTransport {
        pub fn answering(status: u16, body: &str) -> Self {
            Self {
                replies: vec![Ok(HttpResponse {
                    status,
                    body: body.to_string(),
                })],
                seen: Vec::new(),
            }
        }

        pub fn failing(error: TransportError) -> Self {
            Self {
                replies: vec![Err(error)],
                seen: Vec::new(),
            }
        }
    }

    impl HttpTransport for StubTransport {
        fn name(&self) -> &str {
            "stub"
        }

        fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
            self.seen.push(request.clone());
            if self.replies.len() > 1 {
                self.replies.remove(0)
            } else {
                self.replies[0].clone()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::StubTransport;
    use super::*;
    use crate::provider::{LlmProvider, ProviderRequest};

    #[test]
    fn a_transport_timeout_becomes_a_provider_timeout_and_so_a_watchdog_fallback() {
        assert_eq!(
            ProviderError::from(TransportError::TimedOut),
            ProviderError::TimedOut
        );
    }

    #[test]
    fn other_transport_failures_become_unavailable_carrying_the_detail() {
        assert_eq!(
            ProviderError::from(TransportError::Connect("refused".into())),
            ProviderError::Unavailable("connect failed: refused".into())
        );
    }

    /// The plaintext transport must never pretend it can do TLS.
    #[test]
    fn the_tcp_transport_refuses_https_rather_than_downgrading_it() {
        let request = HttpRequest::post_json(
            http::parse_url("https://api.anthropic.com/v1/messages").unwrap(),
            Vec::new(),
            "{}".into(),
        );

        let error = TcpTransport::new().send(&request).unwrap_err();

        assert!(matches!(error, TransportError::Unsupported(_)));
        assert!(error.to_string().contains("TLS"));
    }

    #[test]
    fn a_non_success_status_carries_the_apis_own_message() {
        let mut transport = StubTransport::answering(
            429,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        );
        let request = HttpRequest::post_json(
            http::parse_url("http://localhost:1/x").unwrap(),
            Vec::new(),
            "{}".into(),
        );

        let error = round_trip(&mut transport, &request).unwrap_err();

        assert_eq!(
            error,
            ProviderError::Unavailable("HTTP 429: slow down".into())
        );
    }

    #[test]
    fn a_non_success_status_with_no_message_still_reports_the_status() {
        let mut transport = StubTransport::answering(503, "{}");
        let request = HttpRequest::post_json(
            http::parse_url("http://localhost:1/x").unwrap(),
            Vec::new(),
            "{}".into(),
        );

        assert_eq!(
            round_trip(&mut transport, &request).unwrap_err(),
            ProviderError::Unavailable("HTTP 503".into())
        );
    }

    #[test]
    fn a_body_that_is_not_json_is_a_provider_error_rather_than_a_panic() {
        let mut transport = StubTransport::answering(200, "<html>gateway error</html>");
        let request = HttpRequest::post_json(
            http::parse_url("http://localhost:1/x").unwrap(),
            Vec::new(),
            "{}".into(),
        );

        assert!(matches!(
            round_trip(&mut transport, &request),
            Err(ProviderError::Unavailable(_))
        ));
    }

    #[test]
    fn join_text_blocks_concatenates_only_the_text_blocks() {
        let document = json::parse(
            r#"[{"type":"thinking","thinking":"..."},{"type":"text","text":"SAY: a"},{"type":"text","text":"\nDO: b"}]"#,
        )
        .unwrap();

        assert_eq!(
            join_text_blocks(document.as_array().unwrap(), "type", "text"),
            "SAY: a\nDO: b"
        );
    }

    #[test]
    fn a_missing_api_key_is_reported_rather_than_sent_as_an_empty_header() {
        let error = api_key_from_env("FCS_DEFINITELY_NOT_SET_KEY").unwrap_err();
        assert_eq!(
            error,
            ProviderError::Unavailable("FCS_DEFINITELY_NOT_SET_KEY is not set".into())
        );
    }

    /// Every adapter must be usable as a boxed `LlmProvider`, since that is
    /// how an actor holds one.
    #[test]
    fn every_adapter_is_an_llm_provider_an_actor_can_hold() {
        let providers: Vec<Box<dyn LlmProvider>> = vec![
            Box::new(AnthropicProvider::new(
                "key",
                Box::new(StubTransport::answering(200, "{}")),
            )),
            Box::new(OpenAiProvider::new(
                "key",
                "some-model",
                Box::new(StubTransport::answering(200, "{}")),
            )),
            Box::new(OllamaProvider::new(
                "some-model",
                Box::new(StubTransport::answering(200, "{}")),
            )),
        ];

        let names: Vec<&str> = providers.iter().map(|provider| provider.name()).collect();
        assert_eq!(names, vec!["anthropic", "openai", "ollama"]);
    }

    #[test]
    fn an_adapter_reports_a_transport_failure_rather_than_stalling_the_tick() {
        let mut provider = OllamaProvider::new(
            "some-model",
            Box::new(StubTransport::failing(TransportError::TimedOut)),
        );

        let result = provider.complete(&ProviderRequest::new("system", "prompt"));

        assert_eq!(result, Err(ProviderError::TimedOut));
    }
}
