//! Who is aboard, and what is behind each of them.
//!
//! The point of this module is what it does *not* reach. Swapping the ship's
//! mind from the mock provider to a hosted model, or giving one crew agent a
//! local Ollama and another something else entirely, is a change to the data
//! here — [`safety`](crate::safety), [`subsystems`](crate::subsystems),
//! [`fdir`](crate::fdir) and [`ship`](crate::ship) are not involved and never
//! learn which provider answered. That is the swappability the whole design
//! was arranged around, stated as a type.
//!
//! Two things are deliberately **not** configurable:
//!
//! - **What a role may ask for.** The authorization table is kernel policy,
//!   not deployment configuration. A config file that could widen it would be
//!   a config file that could disarm the ship.
//! - **API keys.** A spec names an *environment variable*; the key itself is
//!   read at build time and never stored, printed, or recorded. A config that
//!   carries a secret is a config that leaks one.
//!
//! A spec is plain data and stays meaningful in a build without the `online`
//! feature: one naming Anthropic still constructs, compares, and describes
//! itself the same way. It simply cannot be *built* into a running actor, and
//! [`ConfigError::OnlineDisabled`] says exactly that rather than failing to
//! compile somewhere unhelpful.

use std::fmt;

use crate::actors::{Actor, CrewAgent, ShipMind, SHIP_MIND_PERSONA};
use crate::command::Role;
use crate::provider::{LlmProvider, MockProvider};
use crate::safety::AutonomyLevel;
use crate::ship::Ship;

#[cfg(feature = "online")]
use crate::provider::online::{
    anthropic, AnthropicProvider, HttpTransport, OllamaProvider, OpenAiProvider, TcpTransport,
};

/// Which implementation backs an actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// Deterministic, in-process, always available.
    Mock,
    Anthropic,
    OpenAi,
    Ollama,
}

impl ProviderKind {
    pub fn name(self) -> &'static str {
        match self {
            ProviderKind::Mock => "mock",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::OpenAi => "openai",
            ProviderKind::Ollama => "ollama",
        }
    }

    /// Resolves a name as written on a command line or in a config file. The
    /// set is closed — an unknown name is `None` rather than something to
    /// interpret.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "mock" => Some(ProviderKind::Mock),
            "anthropic" => Some(ProviderKind::Anthropic),
            "openai" => Some(ProviderKind::OpenAi),
            "ollama" => Some(ProviderKind::Ollama),
            _ => None,
        }
    }

    pub const ALL: [ProviderKind; 4] = [
        ProviderKind::Mock,
        ProviderKind::Anthropic,
        ProviderKind::OpenAi,
        ProviderKind::Ollama,
    ];

    /// Whether this kind needs the `online` feature and a network.
    pub fn is_online(self) -> bool {
        self != ProviderKind::Mock
    }

    /// The environment variable this kind reads a key from by default.
    /// `None` where no key is involved.
    pub fn default_api_key_env(self) -> Option<&'static str> {
        match self {
            ProviderKind::Anthropic => Some("ANTHROPIC_API_KEY"),
            ProviderKind::OpenAi => Some("OPENAI_API_KEY"),
            ProviderKind::Mock | ProviderKind::Ollama => None,
        }
    }

    /// The model id used when a spec does not name one. Only the Anthropic
    /// adapter has one: which OpenAI model an account may use, and which
    /// model a local Ollama has pulled, are not things to guess at.
    pub fn default_model(self) -> Option<&'static str> {
        match self {
            #[cfg(feature = "online")]
            ProviderKind::Anthropic => Some(anthropic::DEFAULT_MODEL),
            _ => None,
        }
    }
}

/// How to reach one provider. Everything but `kind` is optional and falls
/// back to the adapter's own default.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderSpec {
    pub kind: ProviderKind,
    pub model: Option<String>,
    /// The environment variable holding the API key — never the key.
    pub api_key_env: Option<String>,
    /// Overrides the adapter's endpoint: a proxy, a gateway, another host.
    pub endpoint: Option<String>,
}

impl ProviderSpec {
    pub fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            model: None,
            api_key_env: None,
            endpoint: None,
        }
    }

    pub fn mock() -> Self {
        Self::new(ProviderKind::Mock)
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_api_key_env(mut self, variable: impl Into<String>) -> Self {
        self.api_key_env = Some(variable.into());
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// The model this spec resolves to, or `None` when neither the spec nor
    /// the adapter names one.
    pub fn resolved_model(&self) -> Option<&str> {
        self.model.as_deref().or_else(|| self.kind.default_model())
    }

    /// The environment variable this spec would read a key from.
    pub fn resolved_api_key_env(&self) -> Option<&str> {
        self.api_key_env
            .as_deref()
            .or_else(|| self.kind.default_api_key_env())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorRole {
    ShipMind,
    CrewAgent,
}

impl ActorRole {
    /// The authority the kernel will judge this actor's proposals under.
    pub fn command_role(self) -> Role {
        match self {
            ActorRole::ShipMind => Role::ShipMind,
            ActorRole::CrewAgent => Role::CrewAgent,
        }
    }
}

/// One actor to bring aboard.
#[derive(Debug, Clone, PartialEq)]
pub struct ActorSpec {
    pub role: ActorRole,
    pub name: String,
    pub persona: String,
    pub provider: ProviderSpec,
}

impl ActorSpec {
    pub fn ship_mind(provider: ProviderSpec) -> Self {
        Self {
            role: ActorRole::ShipMind,
            name: "ship_mind".to_string(),
            persona: SHIP_MIND_PERSONA.to_string(),
            provider,
        }
    }

    pub fn crew(
        name: impl Into<String>,
        persona: impl Into<String>,
        provider: ProviderSpec,
    ) -> Self {
        Self {
            role: ActorRole::CrewAgent,
            name: name.into(),
            persona: persona.into(),
            provider,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_persona(mut self, persona: impl Into<String>) -> Self {
        self.persona = persona.into();
        self
    }
}

/// A whole flight: the tick length, how much latitude proposals get, and who
/// is aboard.
#[derive(Debug, Clone, PartialEq)]
pub struct ShipConfig {
    pub dt: f64,
    pub autonomy: AutonomyLevel,
    pub actors: Vec<ActorSpec>,
}

impl ShipConfig {
    /// An empty ship: no actors, `Assist` autonomy. This is the
    /// configuration the no-LLM survival case runs under, and it is the
    /// default for a reason — nothing has to be configured for the ship to
    /// fly.
    pub fn new(dt: f64) -> Self {
        Self {
            dt,
            autonomy: AutonomyLevel::Assist,
            actors: Vec::new(),
        }
    }

    pub fn with_autonomy(mut self, autonomy: AutonomyLevel) -> Self {
        self.autonomy = autonomy;
        self
    }

    pub fn with_actor(mut self, actor: ActorSpec) -> Self {
        self.actors.push(actor);
        self
    }

    /// Replaces every actor's provider kind, keeping their names, personas,
    /// and roles. This is the operation the whole module exists for: one call
    /// re-backs a crew without a line of the kernel, the subsystems, or the
    /// loop changing.
    pub fn with_provider_kind(mut self, kind: ProviderKind) -> Self {
        for actor in &mut self.actors {
            actor.provider.kind = kind;
        }
        self
    }

    /// Sets the model on every actor's provider.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        let model = model.into();
        for actor in &mut self.actors {
            actor.provider.model = Some(model.clone());
        }
        self
    }

    /// Builds the ship, giving each online adapter the bundled plaintext
    /// transport. That transport refuses `https`, so a hosted endpoint needs
    /// [`ShipConfig::build_with`] and a TLS transport of your own.
    pub fn build(&self) -> Result<Ship, ConfigError> {
        self.assemble(default_provider_for)
    }

    /// Builds the ship with `transport` behind every online adapter. This is
    /// the TLS seam: hand it a factory for whatever HTTPS client you already
    /// depend on, and nothing else about the ship changes.
    #[cfg(feature = "online")]
    pub fn build_with(
        &self,
        mut transport: impl FnMut() -> Box<dyn HttpTransport>,
    ) -> Result<Ship, ConfigError> {
        self.assemble(move |spec, role| provider_for(spec, role, transport()))
    }

    fn assemble(
        &self,
        mut provider_for: impl FnMut(&ProviderSpec, Role) -> Result<Box<dyn LlmProvider>, ConfigError>,
    ) -> Result<Ship, ConfigError> {
        let mut ship = Ship::new(self.dt);
        ship.autonomy = self.autonomy;

        for spec in &self.actors {
            let provider = provider_for(&spec.provider, spec.role.command_role())?;
            let actor: Box<dyn Actor> = match spec.role {
                ActorRole::ShipMind => Box::new(
                    ShipMind::new(provider)
                        .with_name(spec.name.clone())
                        .with_persona(spec.persona.clone()),
                ),
                ActorRole::CrewAgent => Box::new(CrewAgent::new(
                    spec.name.clone(),
                    spec.persona.clone(),
                    provider,
                )),
            };
            ship.board(actor);
        }

        Ok(ship)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigError {
    /// This build has no `online` feature, so the adapter does not exist in
    /// it. Rebuild with `--features online`.
    OnlineDisabled(ProviderKind),
    /// No model id, and this adapter has none worth guessing at.
    ModelRequired(ProviderKind),
    /// The adapter could not be constructed — most often a key whose
    /// environment variable is not set.
    Unavailable { kind: ProviderKind, detail: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::OnlineDisabled(kind) => write!(
                f,
                "provider '{}' needs the `online` feature; rebuild with --features online",
                kind.name()
            ),
            ConfigError::ModelRequired(kind) => write!(
                f,
                "provider '{}' has no default model; name one in the config",
                kind.name()
            ),
            ConfigError::Unavailable { kind, detail } => {
                write!(f, "provider '{}' unavailable: {detail}", kind.name())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// The mock is the one provider that exists in every build, needs no
/// network, and cannot fail to construct.
fn mock_provider(role: Role) -> Box<dyn LlmProvider> {
    Box::new(MockProvider::new(role))
}

#[cfg(not(feature = "online"))]
fn default_provider_for(
    spec: &ProviderSpec,
    role: Role,
) -> Result<Box<dyn LlmProvider>, ConfigError> {
    match spec.kind {
        ProviderKind::Mock => Ok(mock_provider(role)),
        online => Err(ConfigError::OnlineDisabled(online)),
    }
}

#[cfg(feature = "online")]
fn default_provider_for(
    spec: &ProviderSpec,
    role: Role,
) -> Result<Box<dyn LlmProvider>, ConfigError> {
    provider_for(spec, role, Box::new(TcpTransport::new()))
}

#[cfg(feature = "online")]
fn provider_for(
    spec: &ProviderSpec,
    role: Role,
    transport: Box<dyn HttpTransport>,
) -> Result<Box<dyn LlmProvider>, ConfigError> {
    match spec.kind {
        ProviderKind::Mock => Ok(mock_provider(role)),

        ProviderKind::Anthropic => {
            let mut provider =
                AnthropicProvider::new(api_key(spec)?, transport).with_model(resolved_model(spec)?);
            if let Some(endpoint) = &spec.endpoint {
                provider = provider.with_endpoint(endpoint);
            }
            Ok(Box::new(provider))
        }

        ProviderKind::OpenAi => {
            let mut provider =
                OpenAiProvider::new(api_key(spec)?, resolved_model(spec)?, transport);
            if let Some(endpoint) = &spec.endpoint {
                provider = provider.with_endpoint(endpoint);
            }
            Ok(Box::new(provider))
        }

        ProviderKind::Ollama => {
            let mut provider = OllamaProvider::new(resolved_model(spec)?, transport);
            if let Some(endpoint) = &spec.endpoint {
                provider = provider.with_endpoint(endpoint);
            }
            Ok(Box::new(provider))
        }
    }
}

#[cfg(feature = "online")]
fn resolved_model(spec: &ProviderSpec) -> Result<&str, ConfigError> {
    spec.resolved_model()
        .ok_or(ConfigError::ModelRequired(spec.kind))
}

/// Reads the key at build time. It goes straight into the adapter and is
/// never held in the spec, so nothing that gets logged or recorded has ever
/// seen it.
#[cfg(feature = "online")]
fn api_key(spec: &ProviderSpec) -> Result<String, ConfigError> {
    let Some(variable) = spec.resolved_api_key_env() else {
        return Ok(String::new());
    };
    // The detail is written here rather than borrowed from the provider
    // error, whose own `Display` would nest a second "provider unavailable:"
    // inside this one.
    crate::provider::online::api_key_from_env(variable).map_err(|_| ConfigError::Unavailable {
        kind: spec.kind,
        detail: format!("{variable} is not set"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crewed() -> ShipConfig {
        ShipConfig::new(1.0)
            .with_autonomy(AutonomyLevel::Autonomous)
            .with_actor(ActorSpec::ship_mind(ProviderSpec::mock()))
            .with_actor(ActorSpec::crew(
                "engineer",
                "you mind the link home",
                ProviderSpec::mock(),
            ))
    }

    #[test]
    fn an_empty_config_builds_the_ship_that_flies_itself() {
        let ship = ShipConfig::new(1.0).build().unwrap();

        assert!(ship.actors.is_empty());
        assert_eq!(ship.autonomy, AutonomyLevel::Assist);
    }

    #[test]
    fn a_crewed_config_boards_its_actors_in_order_with_their_names_and_roles() {
        let ship = crewed().build().unwrap();

        assert_eq!(ship.actors.len(), 2);
        assert_eq!(ship.actors[0].name(), "ship_mind");
        assert_eq!(ship.actors[0].role(), Role::ShipMind);
        assert_eq!(ship.actors[0].provider_name(), "mock:ship_mind");
        assert_eq!(ship.actors[1].name(), "engineer");
        assert_eq!(ship.actors[1].role(), Role::CrewAgent);
        assert_eq!(ship.actors[1].provider_name(), "mock:crew_agent");
        assert_eq!(ship.autonomy, AutonomyLevel::Autonomous);
    }

    #[test]
    fn a_built_ship_flies_the_same_loop_as_a_hand_assembled_one() {
        let mut ship = crewed().build().unwrap();

        for _ in 0..5 {
            ship.tick();
        }

        assert_eq!(ship.recorder.records().len(), 5);
        assert_eq!(ship.recorder.records()[0].actor_turns.len(), 2);
    }

    /// The headline property: re-backing a crew is one call, and nothing
    /// about the actors, their roles, or the ship changes with it.
    #[test]
    fn changing_the_provider_kind_leaves_the_crew_and_their_authority_intact() {
        let mock = crewed();
        let online = crewed().with_provider_kind(ProviderKind::Ollama);

        assert_eq!(mock.actors.len(), online.actors.len());
        for (before, after) in mock.actors.iter().zip(&online.actors) {
            assert_eq!(before.role, after.role);
            assert_eq!(before.name, after.name);
            assert_eq!(before.persona, after.persona);
            assert_ne!(before.provider.kind, after.provider.kind);
        }
    }

    #[test]
    fn provider_names_round_trip_and_the_set_is_closed() {
        for kind in ProviderKind::ALL {
            assert_eq!(ProviderKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(ProviderKind::from_name("gpt"), None);
        assert_eq!(ProviderKind::from_name(""), None);
    }

    #[test]
    fn only_the_mock_is_available_without_a_network() {
        assert!(!ProviderKind::Mock.is_online());
        for kind in [
            ProviderKind::Anthropic,
            ProviderKind::OpenAi,
            ProviderKind::Ollama,
        ] {
            assert!(kind.is_online());
        }
    }

    /// A spec names a variable, never a key. Nothing here can leak one
    /// because nothing here holds one.
    #[test]
    fn a_spec_names_an_environment_variable_rather_than_carrying_a_key() {
        let spec = ProviderSpec::new(ProviderKind::Anthropic);
        assert_eq!(spec.resolved_api_key_env(), Some("ANTHROPIC_API_KEY"));

        let overridden = spec.clone().with_api_key_env("MY_OWN_KEY_VAR");
        assert_eq!(overridden.resolved_api_key_env(), Some("MY_OWN_KEY_VAR"));

        // The keyless kinds say so rather than inventing a variable name.
        assert_eq!(ProviderSpec::mock().resolved_api_key_env(), None);
        assert_eq!(
            ProviderSpec::new(ProviderKind::Ollama).resolved_api_key_env(),
            None
        );
    }

    #[test]
    fn a_spec_model_overrides_the_adapters_default() {
        let spec = ProviderSpec::new(ProviderKind::Anthropic).with_model("claude-sonnet-5");
        assert_eq!(spec.resolved_model(), Some("claude-sonnet-5"));
    }

    /// Which OpenAI model an account may use, and which model a local Ollama
    /// has pulled, are not things to guess at.
    #[test]
    fn the_adapters_with_no_sensible_default_model_do_not_invent_one() {
        assert_eq!(ProviderKind::OpenAi.default_model(), None);
        assert_eq!(ProviderKind::Ollama.default_model(), None);
    }

    /// The kernel's capability table is policy, not deployment config —
    /// there is no way to reach it from here, and a config-built ship gets
    /// exactly the same one as any other.
    #[test]
    fn a_config_cannot_widen_what_a_role_is_allowed_to_ask_for() {
        use crate::command::Target;

        let ship = crewed().build().unwrap();

        assert!(!ship.kernel.authorization.is_authorized(
            Role::CrewAgent,
            Target::LifeSupport,
            "vent"
        ));
        assert!(!ship.kernel.authorization.is_authorized(
            Role::CrewAgent,
            Target::Reactor,
            "set_output"
        ));
        assert!(ship.kernel.authorization.is_authorized(
            Role::ShipMind,
            Target::Reactor,
            "set_output"
        ));
    }

    #[cfg(not(feature = "online"))]
    #[test]
    fn an_online_provider_in_an_offline_build_says_so_rather_than_failing_obscurely() {
        let config = ShipConfig::new(1.0).with_actor(ActorSpec::ship_mind(ProviderSpec::new(
            ProviderKind::Anthropic,
        )));

        assert_eq!(
            config.build().unwrap_err(),
            ConfigError::OnlineDisabled(ProviderKind::Anthropic)
        );
    }

    #[cfg(not(feature = "online"))]
    #[test]
    fn the_offline_build_still_describes_an_online_spec_correctly() {
        // The data is meaningful even where the adapter is not compiled in.
        let spec = ProviderSpec::new(ProviderKind::OpenAi).with_model("some-model");
        assert_eq!(spec.kind.name(), "openai");
        assert_eq!(spec.resolved_model(), Some("some-model"));
    }

    #[cfg(feature = "online")]
    mod online_tests {
        use super::*;
        use crate::provider::online::testing::StubTransport;
        use crate::provider::online::{openai, HttpTransport};

        fn stub() -> Box<dyn HttpTransport> {
            Box::new(StubTransport::answering(
                200,
                r#"{"message":{"content":"SAY: all nominal"}}"#,
            ))
        }

        #[test]
        fn an_online_actor_is_built_with_the_transport_it_is_given() {
            let config = ShipConfig::new(1.0).with_actor(ActorSpec::ship_mind(
                ProviderSpec::new(ProviderKind::Ollama).with_model("llama3.1"),
            ));

            let ship = config.build_with(stub).unwrap();

            assert_eq!(ship.actors.len(), 1);
            assert_eq!(ship.actors[0].provider_name(), "ollama");
            assert_eq!(ship.actors[0].role(), Role::ShipMind);
        }

        /// A whole tick through a "networked" actor, without a socket: the
        /// adapter, the protocol parser, the kernel and the recorder all run
        /// for real.
        #[test]
        fn a_ship_built_on_an_online_adapter_flies_a_tick_end_to_end() {
            let mut ship = ShipConfig::new(1.0)
                .with_autonomy(AutonomyLevel::Autonomous)
                .with_actor(ActorSpec::ship_mind(
                    ProviderSpec::new(ProviderKind::Ollama).with_model("llama3.1"),
                ))
                .build_with(stub)
                .unwrap();

            ship.tick();

            let record = ship.recorder.records().last().unwrap();
            assert_eq!(record.actor_turns.len(), 1);
            assert_eq!(record.actor_turns[0].provider, "ollama");
            assert_eq!(record.actor_turns[0].speech, vec!["all nominal"]);
            assert_eq!(record.actor_turns[0].failure, None);
        }

        /// Mixing providers across a crew is the case the seam exists for.
        #[test]
        fn a_crew_can_mix_an_online_actor_with_a_mock_one() {
            let ship = ShipConfig::new(1.0)
                .with_actor(ActorSpec::ship_mind(
                    ProviderSpec::new(ProviderKind::Ollama).with_model("llama3.1"),
                ))
                .with_actor(ActorSpec::crew(
                    "engineer",
                    "you mind the link home",
                    ProviderSpec::mock(),
                ))
                .build_with(stub)
                .unwrap();

            assert_eq!(ship.actors[0].provider_name(), "ollama");
            assert_eq!(ship.actors[1].provider_name(), "mock:crew_agent");
        }

        #[test]
        fn an_adapter_with_no_model_named_is_refused_rather_than_guessed_at() {
            let config = ShipConfig::new(1.0).with_actor(ActorSpec::ship_mind(ProviderSpec::new(
                ProviderKind::Ollama,
            )));

            assert_eq!(
                config.build_with(stub).unwrap_err(),
                ConfigError::ModelRequired(ProviderKind::Ollama)
            );
        }

        #[test]
        fn a_missing_api_key_is_reported_at_build_time_rather_than_mid_flight() {
            let config = ShipConfig::new(1.0).with_actor(ActorSpec::ship_mind(
                ProviderSpec::new(ProviderKind::Anthropic)
                    .with_api_key_env("FCS_DEFINITELY_NOT_SET_KEY"),
            ));

            let error = config.build_with(stub).unwrap_err();

            assert!(matches!(
                error,
                ConfigError::Unavailable {
                    kind: ProviderKind::Anthropic,
                    ..
                }
            ));
            // The message names the variable, and cannot name a key.
            assert!(error.to_string().contains("FCS_DEFINITELY_NOT_SET_KEY"));
        }

        #[test]
        fn the_anthropic_adapter_has_a_default_model_the_others_do_not() {
            assert_eq!(
                ProviderKind::Anthropic.default_model(),
                Some(anthropic::DEFAULT_MODEL)
            );
            assert_eq!(
                ProviderSpec::new(ProviderKind::Anthropic).resolved_model(),
                Some(anthropic::DEFAULT_MODEL)
            );
        }

        #[test]
        fn the_default_endpoints_are_the_documented_ones() {
            assert_eq!(
                anthropic::DEFAULT_ENDPOINT,
                "https://api.anthropic.com/v1/messages"
            );
            assert_eq!(
                openai::DEFAULT_ENDPOINT,
                "https://api.openai.com/v1/chat/completions"
            );
        }
    }
}
