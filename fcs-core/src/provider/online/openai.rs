//! The OpenAI chat completions API as an [`LlmProvider`].
//!
//! ```text
//! POST https://api.openai.com/v1/chat/completions
//! authorization: Bearer <key>
//! {"model": …, "messages": [{"role": "system", …}, {"role": "user", …}]}
//! ```
//!
//! The system prompt is a message here rather than a top-level field — the
//! one real difference from the Anthropic adapter, along with where the
//! answer is read from (`choices[0].message.content`).
//!
//! There is no default model. The right one depends on the account, and a
//! wrong guess baked into a default would fail at the worst possible moment
//! with a confusing error; naming it is the operator's job.

use crate::provider::{LlmProvider, ProviderError, ProviderRequest, ProviderResponse};

use super::http::{self, HttpRequest, Url};
use super::json::Value;
use super::{round_trip, HttpTransport};

pub const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
pub const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";

pub struct OpenAiProvider {
    endpoint: String,
    model: String,
    api_key: String,
    transport: Box<dyn HttpTransport>,
}

impl OpenAiProvider {
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        transport: Box<dyn HttpTransport>,
    ) -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_string(),
            model: model.into(),
            api_key: api_key.into(),
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

impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn complete(&mut self, request: &ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let http_request = HttpRequest::post_json(
            self.url()?,
            vec![("authorization".into(), format!("Bearer {}", self.api_key))],
            self.body(request),
        );

        let document = round_trip(self.transport.as_mut(), &http_request)?;

        let choice = document
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .ok_or_else(|| ProviderError::Unavailable("response had no choices".into()))?;

        if choice.get("finish_reason").and_then(Value::as_str) == Some("content_filter") {
            return Err(ProviderError::Refused(
                "content filter stopped this turn".into(),
            ));
        }

        let text = choice
            .path(&["message", "content"])
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::Unavailable("choice had no message content".into()))?;

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

    fn answering(body: &str) -> OpenAiProvider {
        OpenAiProvider::new(
            "test-key",
            "some-model",
            Box::new(StubTransport::answering(200, body)),
        )
    }

    #[test]
    fn sends_the_system_prompt_as_a_message_rather_than_a_top_level_field() {
        let provider = answering("{}");
        let body = json::parse(&provider.body(&request())).unwrap();

        assert!(body.get("system").is_none());
        let messages = body.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].get("role").unwrap().as_str(), Some("system"));
        assert_eq!(
            messages[0].get("content").unwrap().as_str(),
            Some("you are the ship mind")
        );
        assert_eq!(messages[1].get("role").unwrap().as_str(), Some("user"));
        assert_eq!(body.get("model").unwrap().as_str(), Some("some-model"));
    }

    #[test]
    fn authenticates_with_a_bearer_token() {
        let mut transport = StubTransport::answering(200, "{}");
        let provider = answering("{}");
        let http_request = HttpRequest::post_json(
            provider.url().unwrap(),
            vec![("authorization".into(), "Bearer test-key".into())],
            provider.body(&request()),
        );
        let _ = super::round_trip(&mut transport, &http_request);

        assert!(transport.seen[0]
            .headers
            .contains(&("authorization".to_string(), "Bearer test-key".to_string())));
        assert_eq!(transport.seen[0].url.host, "api.openai.com");
        assert_eq!(transport.seen[0].url.path, "/v1/chat/completions");
    }

    #[test]
    fn returns_the_first_choices_message_verbatim() {
        let mut provider = answering(
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"SAY: all nominal"},"finish_reason":"stop"}]}"#,
        );

        assert_eq!(
            provider.complete(&request()).unwrap().text,
            "SAY: all nominal"
        );
    }

    #[test]
    fn a_content_filter_finish_becomes_a_refused_error() {
        let mut provider = answering(
            r#"{"choices":[{"message":{"content":""},"finish_reason":"content_filter"}]}"#,
        );

        assert!(matches!(
            provider.complete(&request()),
            Err(ProviderError::Refused(_))
        ));
    }

    #[test]
    fn an_api_error_carries_its_message_back() {
        let mut provider = OpenAiProvider::new(
            "test-key",
            "some-model",
            Box::new(StubTransport::answering(
                429,
                r#"{"error":{"message":"rate limit reached","type":"requests"}}"#,
            )),
        );

        assert_eq!(
            provider.complete(&request()),
            Err(ProviderError::Unavailable(
                "HTTP 429: rate limit reached".into()
            ))
        );
    }

    #[test]
    fn a_response_with_no_choices_is_an_error_rather_than_empty_speech() {
        let mut provider = answering(r#"{"choices":[]}"#);
        assert!(matches!(
            provider.complete(&request()),
            Err(ProviderError::Unavailable(_))
        ));
    }

    #[test]
    fn a_timeout_reaches_the_watchdog_as_a_timeout() {
        let mut provider = OpenAiProvider::new(
            "test-key",
            "some-model",
            Box::new(StubTransport::failing(TransportError::TimedOut)),
        );

        assert_eq!(provider.complete(&request()), Err(ProviderError::TimedOut));
    }

    /// An OpenAI-compatible server (vLLM, LM Studio, a gateway) is reached by
    /// pointing the endpoint at it — no code change.
    #[test]
    fn the_endpoint_can_be_pointed_at_any_compatible_server() {
        let provider = answering("{}").with_endpoint("http://localhost:8000/v1/chat/completions");
        let url = provider.url().unwrap();

        assert_eq!(url.host, "localhost");
        assert_eq!(url.port, 8000);
    }
}
