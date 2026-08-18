//! The Anthropic Messages API as an [`LlmProvider`].
//!
//! ```text
//! POST https://api.anthropic.com/v1/messages
//! x-api-key: <key>
//! anthropic-version: 2023-06-01
//! {"model": …, "max_tokens": …, "system": <system>,
//!  "messages": [{"role": "user", "content": <prompt>}]}
//! ```
//!
//! The response carries a list of content blocks, and only the `text` ones
//! are the answer — a thinking-capable model puts `thinking` blocks in front
//! of them, so reading `content[0]` would hand the actor the wrong thing.
//! Every text block is concatenated and passed on verbatim: interpreting it
//! is the [`protocol`](crate::protocol) parser's job, not this module's.
//!
//! A `stop_reason` of `refusal` means the model declined, which is a
//! perfectly ordinary outcome for a ship to have to handle — it becomes
//! [`ProviderError::Refused`], the watchdog notes it, and the autopilot has
//! the tick.

use crate::provider::{LlmProvider, ProviderError, ProviderRequest, ProviderResponse};

use super::http::{self, HttpRequest, Url};
use super::json::Value;
use super::{join_text_blocks, round_trip, HttpTransport};

pub const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
pub const DEFAULT_MODEL: &str = "claude-opus-5";
pub const API_VERSION: &str = "2023-06-01";
pub const DEFAULT_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// Room for a turn. An actor's answer is a few lines of protocol, but a
/// thinking model spends tokens before it writes any of them.
pub const DEFAULT_MAX_TOKENS: u32 = 16000;

pub struct AnthropicProvider {
    endpoint: String,
    model: String,
    api_key: String,
    max_tokens: u32,
    transport: Box<dyn HttpTransport>,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>, transport: Box<dyn HttpTransport>) -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_string(),
            model: DEFAULT_MODEL.to_string(),
            api_key: api_key.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            transport,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    fn body(&self, request: &ProviderRequest) -> String {
        Value::object(vec![
            ("model", self.model.as_str().into()),
            ("max_tokens", self.max_tokens.into()),
            ("system", request.system.as_str().into()),
            (
                "messages",
                Value::array(vec![Value::object(vec![
                    ("role", "user".into()),
                    ("content", request.prompt.as_str().into()),
                ])]),
            ),
        ])
        .encode()
    }

    fn url(&self) -> Result<Url, ProviderError> {
        http::parse_url(&self.endpoint)
            .map_err(|error| ProviderError::Unavailable(error.to_string()))
    }
}

impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn complete(&mut self, request: &ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let http_request = HttpRequest::post_json(
            self.url()?,
            vec![
                ("x-api-key".into(), self.api_key.clone()),
                ("anthropic-version".into(), API_VERSION.into()),
            ],
            self.body(request),
        );

        let document = round_trip(self.transport.as_mut(), &http_request)?;

        if document.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
            return Err(ProviderError::Refused("model declined this turn".into()));
        }

        let blocks = document
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::Unavailable("response had no content blocks".into()))?;

        Ok(ProviderResponse::new(join_text_blocks(
            blocks, "type", "text",
        )))
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

    fn answering(body: &str) -> AnthropicProvider {
        AnthropicProvider::new("test-key", Box::new(StubTransport::answering(200, body)))
    }

    #[test]
    fn builds_the_request_the_messages_api_documents() {
        let mut transport = StubTransport::answering(
            200,
            r#"{"content":[{"type":"text","text":"SAY: ok"}],"stop_reason":"end_turn"}"#,
        );
        // Build by hand so the sent request can be inspected afterwards.
        let provider =
            AnthropicProvider::new("test-key", Box::new(StubTransport::answering(200, "{}")));
        let http_request = HttpRequest::post_json(
            provider.url().unwrap(),
            vec![
                ("x-api-key".into(), "test-key".into()),
                ("anthropic-version".into(), API_VERSION.into()),
            ],
            provider.body(&request()),
        );
        let _ = super::round_trip(&mut transport, &http_request);

        let sent = &transport.seen[0];
        assert_eq!(sent.url.host, "api.anthropic.com");
        assert_eq!(sent.url.path, "/v1/messages");
        assert!(sent
            .headers
            .contains(&("x-api-key".to_string(), "test-key".to_string())));
        assert!(sent
            .headers
            .contains(&("anthropic-version".to_string(), API_VERSION.to_string())));

        let body = json::parse(&sent.body).unwrap();
        assert_eq!(body.get("model").unwrap().as_str(), Some(DEFAULT_MODEL));
        assert_eq!(
            body.get("max_tokens").unwrap(),
            &Value::Number(DEFAULT_MAX_TOKENS as f64)
        );
        assert_eq!(
            body.get("system").unwrap().as_str(),
            Some("you are the ship mind")
        );
        let messages = body.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages[0].get("role").unwrap().as_str(), Some("user"));
        assert_eq!(
            messages[0].get("content").unwrap().as_str(),
            Some("tick=1 sys.reactor.core_temp_k=300.000")
        );
    }

    #[test]
    fn returns_the_models_text_verbatim_for_the_protocol_to_parse() {
        let mut provider = answering(
            r#"{"content":[{"type":"text","text":"SAY: core is hot\nDO: set_output reactor level=0.000"}],"stop_reason":"end_turn"}"#,
        );

        let response = provider.complete(&request()).unwrap();

        assert_eq!(
            response.text,
            "SAY: core is hot\nDO: set_output reactor level=0.000"
        );
    }

    /// A thinking-capable model puts `thinking` blocks ahead of its answer.
    /// Reading `content[0]` would hand the actor reasoning instead of
    /// protocol, and the whole turn would be dropped as malformed.
    #[test]
    fn thinking_blocks_are_skipped_rather_than_mistaken_for_the_answer() {
        let mut provider = answering(
            r#"{"content":[{"type":"thinking","thinking":"the core is climbing"},{"type":"text","text":"SAY: throttling"}],"stop_reason":"end_turn"}"#,
        );

        assert_eq!(
            provider.complete(&request()).unwrap().text,
            "SAY: throttling"
        );
    }

    #[test]
    fn several_text_blocks_are_concatenated_in_order() {
        let mut provider = answering(
            r#"{"content":[{"type":"text","text":"SAY: a\n"},{"type":"text","text":"DO: set_output reactor level=0.000"}]}"#,
        );

        assert_eq!(
            provider.complete(&request()).unwrap().text,
            "SAY: a\nDO: set_output reactor level=0.000"
        );
    }

    /// A refusal is an ordinary outcome, not a crash: the ship loses the
    /// turn, the autopilot keeps flying.
    #[test]
    fn a_refusal_stop_reason_becomes_a_refused_error() {
        let mut provider = answering(r#"{"content":[],"stop_reason":"refusal"}"#);

        assert!(matches!(
            provider.complete(&request()),
            Err(ProviderError::Refused(_))
        ));
    }

    #[test]
    fn an_api_error_carries_its_message_back_to_the_flight_record() {
        let mut provider = AnthropicProvider::new(
            "test-key",
            Box::new(StubTransport::answering(
                401,
                r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
            )),
        );

        assert_eq!(
            provider.complete(&request()),
            Err(ProviderError::Unavailable(
                "HTTP 401: invalid x-api-key".into()
            ))
        );
    }

    #[test]
    fn a_response_missing_its_content_is_an_error_rather_than_empty_speech() {
        let mut provider = answering(r#"{"stop_reason":"end_turn"}"#);

        assert!(matches!(
            provider.complete(&request()),
            Err(ProviderError::Unavailable(_))
        ));
    }

    #[test]
    fn a_timeout_reaches_the_watchdog_as_a_timeout() {
        let mut provider = AnthropicProvider::new(
            "test-key",
            Box::new(StubTransport::failing(TransportError::TimedOut)),
        );

        assert_eq!(provider.complete(&request()), Err(ProviderError::TimedOut));
    }

    #[test]
    fn the_model_endpoint_and_token_budget_are_configurable() {
        let provider = answering("{}")
            .with_model("claude-sonnet-5")
            .with_endpoint("http://localhost:8080/v1/messages")
            .with_max_tokens(4096);

        let body = json::parse(&provider.body(&request())).unwrap();
        assert_eq!(body.get("model").unwrap().as_str(), Some("claude-sonnet-5"));
        assert_eq!(body.get("max_tokens").unwrap(), &Value::Number(4096.0));
        assert_eq!(provider.url().unwrap().port, 8080);
    }

    /// A rendered perception is full of newlines and quotes; the body has to
    /// survive them intact or the model is shown a different reality than the
    /// ship recorded.
    #[test]
    fn a_prompt_with_newlines_and_quotes_survives_the_round_trip_into_the_body() {
        let prompt = "tick=1\nMODE: SafeHold\nDIALOGUE:\n- mind: \"core is hot\"";
        let provider = answering("{}");

        let body = json::parse(&provider.body(&ProviderRequest::new("sys", prompt))).unwrap();
        let messages = body.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages[0].get("content").unwrap().as_str(), Some(prompt));
    }
}
