//! The seam between the ship and any language model. The interface is
//! deliberately narrow and synchronous: text in, text out. A provider is
//! handed an already-rendered prompt and returns raw text — it never sees a
//! [`Command`](crate::command::Command), a
//! [`TelemetrySnapshot`](crate::telemetry::TelemetrySnapshot), or any
//! mutable ship state, and it has no way to reach the kernel. Turning that
//! raw text into proposals is the wire protocol's job (Phase 5, step 2), and
//! every proposal it yields still passes the full safety kernel pipeline.
//!
//! Nothing here pulls in a dependency or opens a socket. The default build
//! stays offline: the only providers that exist in it are in-process and
//! deterministic (the mock provider lands in the next step), and the
//! networked adapters arrive later behind the `online` feature, implementing
//! this same trait without the kernel, actors, or subsystems changing.

pub mod mock;

#[cfg(feature = "online")]
pub mod online;

pub use mock::MockProvider;

use std::fmt;

use crate::watchdog::TurnFailure;

/// One request to a provider: a role/persona and protocol contract in
/// `system`, and the rendered perception for this tick in `prompt`. Both are
/// plain text — the actor layer owns the rendering, so a provider can never
/// smuggle structure past it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequest {
    pub system: String,
    pub prompt: String,
}

impl ProviderRequest {
    pub fn new(system: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            prompt: prompt.into(),
        }
    }
}

/// Raw, untrusted text from a provider. It is not parsed, validated, or
/// believed at this layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResponse {
    pub text: String,
}

impl ProviderResponse {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// Why a provider produced no usable response. Every variant is recoverable
/// from the ship's point of view: the watchdog converts it into a fallback to
/// autopilot for that tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// The provider could not be reached or is not configured.
    Unavailable(String),
    /// The provider exceeded its time budget for this turn.
    TimedOut,
    /// The provider answered, but declined to produce a turn.
    Refused(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::Unavailable(detail) => write!(f, "provider unavailable: {detail}"),
            ProviderError::TimedOut => write!(f, "provider timed out"),
            ProviderError::Refused(detail) => write!(f, "provider refused: {detail}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// A provider failure is never fatal to the loop — it becomes a watchdog
/// failure, and the watchdog falls back to the autopilot's plan for the tick.
impl From<ProviderError> for TurnFailure {
    fn from(error: ProviderError) -> Self {
        match error {
            ProviderError::TimedOut => TurnFailure::TimedOut,
            other => TurnFailure::Errored(other.to_string()),
        }
    }
}

/// A source of model completions. Synchronous by design: the ship loop is a
/// fixed-step, deterministic sequence of phases, and an actor's turn is one
/// bounded step inside it, not an awaited future the loop has to schedule
/// around. Object-safe, so each actor can hold its own boxed provider and a
/// scenario can mix providers without any kernel changes.
pub trait LlmProvider {
    /// A stable identifier for this provider, used in logs and flight records.
    fn name(&self) -> &str;

    /// Produces one completion for `request`. Implementations must not block
    /// indefinitely; exceeding a time budget is reported as
    /// [`ProviderError::TimedOut`] rather than stalling the tick.
    fn complete(&mut self, request: &ProviderRequest) -> Result<ProviderResponse, ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal in-process provider: echoes a canned reply, or fails with a
    /// canned error. Stands in for the real mock provider arriving in the
    /// next step.
    struct StubProvider {
        name: String,
        reply: Result<ProviderResponse, ProviderError>,
        calls: usize,
    }

    impl StubProvider {
        fn replying(text: &str) -> Self {
            Self {
                name: "stub".into(),
                reply: Ok(ProviderResponse::new(text)),
                calls: 0,
            }
        }

        fn failing(error: ProviderError) -> Self {
            Self {
                name: "stub".into(),
                reply: Err(error),
                calls: 0,
            }
        }
    }

    impl LlmProvider for StubProvider {
        fn name(&self) -> &str {
            &self.name
        }

        fn complete(
            &mut self,
            _request: &ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.calls += 1;
            self.reply.clone()
        }
    }

    fn request() -> ProviderRequest {
        ProviderRequest::new(
            "you are the ship mind",
            "tick=1 sys.reactor.core_temp_k=300.000",
        )
    }

    #[test]
    fn a_provider_returns_raw_text_for_a_rendered_request() {
        let mut provider = StubProvider::replying("SAY: all nominal");

        let response = provider.complete(&request()).unwrap();

        assert_eq!(response.text, "SAY: all nominal");
        assert_eq!(provider.name(), "stub");
    }

    /// Each actor holds its own provider instance, so the trait has to be
    /// usable behind a box and a `dyn` reference.
    #[test]
    fn the_trait_is_object_safe_so_actors_can_hold_boxed_providers() {
        let mut providers: Vec<Box<dyn LlmProvider>> = vec![
            Box::new(StubProvider::replying("SAY: captain here")),
            Box::new(StubProvider::replying("SAY: engineer here")),
        ];

        let texts: Vec<String> = providers
            .iter_mut()
            .map(|provider| match provider.complete(&request()) {
                Ok(response) => response.text,
                Err(error) => error.to_string(),
            })
            .collect();

        assert_eq!(texts, vec!["SAY: captain here", "SAY: engineer here"]);
    }

    #[test]
    fn an_in_process_provider_is_deterministic_across_identical_requests() {
        let mut provider = StubProvider::replying("SAY: all nominal");

        let first = provider.complete(&request());
        let second = provider.complete(&request());

        assert_eq!(first, second);
        assert_eq!(provider.calls, 2);
    }

    #[test]
    fn a_failing_provider_surfaces_an_error_rather_than_a_response() {
        let mut provider = StubProvider::failing(ProviderError::Unavailable("no api key".into()));

        let result = provider.complete(&request());

        assert_eq!(result, Err(ProviderError::Unavailable("no api key".into())));
    }

    #[test]
    fn a_provider_timeout_becomes_a_watchdog_timeout() {
        assert_eq!(
            TurnFailure::from(ProviderError::TimedOut),
            TurnFailure::TimedOut
        );
    }

    #[test]
    fn other_provider_failures_become_watchdog_errors_carrying_the_detail() {
        for error in [
            ProviderError::Unavailable("connection reset".into()),
            ProviderError::Refused("cannot comply".into()),
        ] {
            let detail = error.to_string();
            assert_eq!(TurnFailure::from(error), TurnFailure::Errored(detail));
        }
    }
}
