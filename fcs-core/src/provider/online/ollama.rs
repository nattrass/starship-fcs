//! A local [Ollama](https://ollama.com) server as an [`LlmProvider`].
//!
//! ```text
//! POST http://localhost:11434/api/chat
//! {"model": …, "stream": false,
//!  "messages": [{"role": "system", …}, {"role": "user", …}]}
//! ```
//!
//! The one adapter that works out of the box with the bundled
//! [`TcpTransport`](super::TcpTransport): Ollama listens on plain HTTP on the
//! loopback interface, so no TLS is involved and nothing needs plugging in.
//! It is the honest way to see a real model fly this ship without adding a
//! single crate.
//!
//! `stream: false` is not an optimization — a streamed reply arrives as a
//! sequence of JSON documents, and an actor's turn is one bounded step in a
//! fixed-step loop, not something to assemble incrementally.
//!
//! No default model: which one is installed is a property of the machine.

use crate::provider::{LlmProvider, ProviderError, ProviderRequest, ProviderResponse};

use super::http::{self, HttpRequest, Url};
use super::json::Value;
use super::{round_trip, HttpTransport};

pub const DEFAULT_ENDPOINT: &str = "http://localhost:11434/api/chat";

pub struct OllamaProvider {
    endpoint: String,
    model: String,
    transport: Box<dyn HttpTransport>,
}

impl OllamaProvider {
    pub fn new(model: impl Into<String>, transport: Box<dyn HttpTransport>) -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_string(),
            model: model.into(),
            transport,
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    fn body(&self, request: &ProviderRequest) -> String {
        Value::object(vec![
            ("model", self.model.as_str().into()),
            ("stream", false.into()),
            (
                "messages",
                Value::array(vec![
                    Value::object(vec![
                        ("role", "system".into()),
                        ("content", request.system.as_str().into()),
                    ]),
                    Value::object(vec![
                        ("role", "user".into()),
                        ("content", request.prompt.as_str().into()),
                    ]),
                ]),
            ),
        ])
        .encode()
    }

    fn url(&self) -> Result<Url, ProviderError> {
        http::parse_url(&self.endpoint)
            .map_err(|error| ProviderError::Unavailable(error.to_string()))
    }
}

impl LlmProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    fn complete(&mut self, request: &ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let http_request = HttpRequest::post_json(self.url()?, Vec::new(), self.body(request));

        let document = round_trip(self.transport.as_mut(), &http_request)?;

        let text = document
            .path(&["message", "content"])
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::Unavailable("response had no message content".into()))?;

        Ok(ProviderResponse::new(text))
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::StubTransport;
    use super::super::{json, TransportError};
    use super::*;

    fn request() -> ProviderRequest {
        ProviderRequest::new(
            "you are the ship mind",
            "tick=1 sys.reactor.core_temp_k=300.000",
        )
    }

    fn answering(body: &str) -> OllamaProvider {
        OllamaProvider::new("llama3.1", Box::new(StubTransport::answering(200, body)))
    }

    /// A streamed reply is several documents rather than one, which a
    /// fixed-step turn has nowhere to put.
    #[test]
    fn asks_for_a_single_document_rather_than_a_stream() {
        let body = json::parse(&answering("{}").body(&request())).unwrap();
        assert_eq!(body.get("stream").unwrap(), &Value::Bool(false));
    }

    #[test]
    fn sends_both_prompts_as_messages() {
        let body = json::parse(&answering("{}").body(&request())).unwrap();

        assert_eq!(body.get("model").unwrap().as_str(), Some("llama3.1"));
        let messages = body.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages[0].get("role").unwrap().as_str(), Some("system"));
        assert_eq!(messages[1].get("role").unwrap().as_str(), Some("user"));
        assert_eq!(
            messages[1].get("content").unwrap().as_str(),
            Some("tick=1 sys.reactor.core_temp_k=300.000")
        );
    }

    #[test]
    fn returns_the_messages_content_verbatim() {
        let mut provider = answering(
            r#"{"model":"llama3.1","message":{"role":"assistant","content":"SAY: all nominal"},"done":true}"#,
        );

        assert_eq!(
            provider.complete(&request()).unwrap().text,
            "SAY: all nominal"
        );
    }

    /// The default endpoint is loopback plaintext, so the bundled transport
    /// can reach it without any TLS being supplied.
    #[test]
    fn the_default_endpoint_needs_no_tls_transport() {
        let url = answering("{}").url().unwrap();

        assert_eq!(url.scheme, http::Scheme::Http);
        assert_eq!(url.host, "localhost");
        assert_eq!(url.port, 11434);
        assert_eq!(url.path, "/api/chat");
    }

    #[test]
    fn the_endpoint_can_point_at_another_host() {
        let url = answering("{}")
            .with_endpoint("http://gpu-box.local:11434/api/chat")
            .url()
            .unwrap();

        assert_eq!(url.host, "gpu-box.local");
    }

    #[test]
    fn a_server_error_carries_its_message_back() {
        let mut provider = OllamaProvider::new(
            "llama3.1",
            Box::new(StubTransport::answering(
                404,
                r#"{"error":{"message":"model 'llama3.1' not found"}}"#,
            )),
        );

        assert_eq!(
            provider.complete(&request()),
            Err(ProviderError::Unavailable(
                "HTTP 404: model 'llama3.1' not found".into()
            ))
        );
    }

    #[test]
    fn a_response_with_no_message_is_an_error_rather_than_empty_speech() {
        let mut provider = answering(r#"{"done":true}"#);

        assert!(matches!(
            provider.complete(&request()),
            Err(ProviderError::Unavailable(_))
        ));
    }

    #[test]
    fn an_unreachable_server_reports_rather_than_stalling_the_tick() {
        let mut provider = OllamaProvider::new(
            "llama3.1",
            Box::new(StubTransport::failing(TransportError::Connect(
                "connection refused".into(),
            ))),
        );

        assert!(matches!(
            provider.complete(&request()),
            Err(ProviderError::Unavailable(_))
        ));
    }
}
