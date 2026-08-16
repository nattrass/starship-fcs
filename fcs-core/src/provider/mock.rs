//! A deterministic, in-process provider for tests and offline runs.
//!
//! It stands in for a language model without being one: same trait, same
//! narrow text-in/text-out seam, same untrusted status. It sees only the
//! rendered prompt — never the world, the subsystems, or the kernel — and it
//! answers in the [`protocol`](crate::protocol) grammar, which is then parsed
//! and reviewed exactly like output from a real model. Nothing it proposes
//! bypasses a single kernel stage.
//!
//! It is **telemetry-aware**: it scans the prompt for the whitespace-separated
//! `<channel>=<value>` tokens that [`ship::format_report`](crate::ship::format_report)
//! emits, honoring the same `*` (spoofed) and `!` (dropout) markers. A spoofed
//! channel is indistinguishable from a real one here — as it must be, since a
//! mind that could see through a spoof would not be testing anything. A
//! dropped-out channel it treats as lost visibility and declines to act on.
//!
//! It is **role-aware**: the persona is fixed at construction, so a scenario
//! can run several mock actors with different jobs, or mix a mock alongside a
//! differently-backed one.
//!
//! Its output is a pure function of `(role, prompt)`: no clock, no randomness,
//! no accumulated state. Two identical requests always produce identical text,
//! which is what keeps a recorded run replayable.

use crate::command::Role;
use crate::subsystems::comms::MIN_USABLE_SIGNAL_STRENGTH;
use crate::subsystems::life_support::MIN_SAFE_O2_LEVEL;
use crate::subsystems::reactor::THERMAL_CEILING_K;

use super::{LlmProvider, ProviderError, ProviderRequest, ProviderResponse};

const CHANNEL_REACTOR_TEMP: &str = "sys.reactor.core_temp_k";
const CHANNEL_O2_LEVEL: &str = "sys.life_support.o2_level";
const CHANNEL_SIGNAL: &str = "sys.comms.signal_strength";

/// The mock acts on the reactor once the core passes this fraction of its
/// thermal ceiling — before the fault threshold, so it has something to say
/// while the ship is still nominal.
const REACTOR_CONCERN_FRACTION: f64 = 0.9;
/// Margins above the hard limits at which the mock starts acting.
const O2_CONCERN_MARGIN: f64 = 1.1;
const SIGNAL_CONCERN_MARGIN: f64 = 1.5;

/// One channel as the mock managed to read it out of the prompt.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Reading {
    value: f64,
    /// False when the prompt marked the channel as dropped out.
    trusted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MockProvider {
    role: Role,
    name: String,
}

impl MockProvider {
    pub fn new(role: Role) -> Self {
        Self {
            role,
            name: format!("mock:{}", role_slug(role)),
        }
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// Builds this turn's protocol text from the prompt alone.
    fn respond(&self, prompt: &str) -> String {
        let mut lines = Vec::new();
        match self.role {
            Role::ShipMind => self.ship_mind_turn(prompt, &mut lines),
            Role::CrewAgent => self.crew_agent_turn(prompt, &mut lines),
            // The captain speaks for themselves, and the autopilot is a pure
            // controller that never consults a provider at all. Neither gets
            // invented orders from a mock.
            Role::Captain | Role::Autopilot => {
                lines.push("SAY: standing by".to_string());
            }
        }
        lines.join("\n")
    }

    /// Engineering-minded: watches the reactor, then life support.
    fn ship_mind_turn(&self, prompt: &str, lines: &mut Vec<String>) {
        let mut speech = Vec::new();
        let mut commands = Vec::new();

        match read(prompt, CHANNEL_REACTOR_TEMP) {
            Some(Reading { trusted: false, .. }) => {
                speech.push("core temperature is unreadable, not touching the reactor".to_string());
            }
            Some(Reading { value, .. }) if value >= THERMAL_CEILING_K * REACTOR_CONCERN_FRACTION => {
                speech.push(format!("core at {value:.1}K, throttling down"));
                commands.push("DO: set_output reactor level=0.000".to_string());
            }
            _ => {}
        }

        match read(prompt, CHANNEL_O2_LEVEL) {
            Some(Reading { trusted: false, .. }) => {
                speech.push("O2 reading is lost, holding scrubber rate".to_string());
            }
            Some(Reading { value, .. }) if value <= MIN_SAFE_O2_LEVEL * O2_CONCERN_MARGIN => {
                speech.push(format!("O2 down to {value:.3}, scrubbing at maximum"));
                commands.push("DO: set_scrubber_rate life_support rate=1.000".to_string());
            }
            _ => {}
        }

        if speech.is_empty() {
            speech.push("all systems nominal".to_string());
        }
        lines.push(format!("SAY: {}", speech.join("; ")));
        lines.extend(commands);
    }

    /// Watches the link home.
    fn crew_agent_turn(&self, prompt: &str, lines: &mut Vec<String>) {
        match read(prompt, CHANNEL_SIGNAL) {
            Some(Reading { trusted: false, .. }) => {
                lines.push("SAY: I've lost the signal readout entirely".to_string());
            }
            Some(Reading { value, .. })
                if value <= MIN_USABLE_SIGNAL_STRENGTH * SIGNAL_CONCERN_MARGIN =>
            {
                lines.push(format!("SAY: signal down to {value:.3}, boosting transmit power"));
                lines.push("DO: set_transmit_power comms power=1.000".to_string());
            }
            _ => lines.push("SAY: link is holding, nothing to report".to_string()),
        }
    }
}

impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn complete(&mut self, request: &ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        Ok(ProviderResponse::new(self.respond(&request.prompt)))
    }
}

fn role_slug(role: Role) -> &'static str {
    match role {
        Role::Autopilot => "autopilot",
        Role::CrewAgent => "crew_agent",
        Role::ShipMind => "ship_mind",
        Role::Captain => "captain",
    }
}

/// Finds `channel` in the prompt's `<channel>=<value>` tokens. A trailing `!`
/// marks the channel as dropped out; a trailing `*` marks it as spoofed, which
/// the mock cannot and must not distinguish from a genuine reading.
fn read(prompt: &str, channel: &str) -> Option<Reading> {
    for token in prompt.split_whitespace() {
        let Some(rest) = token.strip_prefix(channel) else {
            continue;
        };
        let Some(raw_value) = rest.strip_prefix('=') else {
            continue;
        };

        let (raw_value, trusted) = match raw_value.strip_suffix('!') {
            Some(stripped) => (stripped, false),
            None => (raw_value.strip_suffix('*').unwrap_or(raw_value), true),
        };

        let value: f64 = raw_value.parse().ok()?;
        if !value.is_finite() {
            return None;
        }
        return Some(Reading { value, trusted });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{self, ProtocolTurn};
    use crate::command::Target;

    fn turn_for(role: Role, prompt: &str) -> ProtocolTurn {
        let mut provider = MockProvider::new(role);
        let response = provider
            .complete(&ProviderRequest::new("you are aboard a starship", prompt))
            .expect("the mock provider never fails");
        protocol::parse(role, &response.text)
    }

    fn nominal_prompt() -> String {
        format!(
            "tick=1 t=1.00s {CHANNEL_REACTOR_TEMP}=300.000 \
             {CHANNEL_O2_LEVEL}=0.210 {CHANNEL_SIGNAL}=1.000"
        )
    }

    #[test]
    fn the_provider_is_named_for_its_role() {
        assert_eq!(MockProvider::new(Role::ShipMind).name(), "mock:ship_mind");
        assert_eq!(MockProvider::new(Role::CrewAgent).name(), "mock:crew_agent");
    }

    /// Whatever the mock emits must survive the strict parser with nothing
    /// dropped — a mock that produced malformed lines would quietly test the
    /// drop path instead of the happy one.
    #[test]
    fn every_role_emits_output_the_strict_parser_accepts_whole() {
        let prompts = [
            nominal_prompt(),
            format!("{CHANNEL_REACTOR_TEMP}=1150.000 {CHANNEL_O2_LEVEL}=0.150 {CHANNEL_SIGNAL}=0.050"),
            format!("{CHANNEL_REACTOR_TEMP}=300.000! {CHANNEL_O2_LEVEL}=0.210! {CHANNEL_SIGNAL}=1.000!"),
            String::new(),
        ];

        for role in [Role::ShipMind, Role::CrewAgent, Role::Captain, Role::Autopilot] {
            for prompt in &prompts {
                let turn = turn_for(role, prompt);
                assert!(
                    turn.dropped.is_empty(),
                    "{role:?} emitted an unparseable line for {prompt:?}: {:?}",
                    turn.dropped
                );
                assert!(!turn.speech.is_empty(), "{role:?} should always say something");
            }
        }
    }

    #[test]
    fn a_nominal_prompt_produces_speech_and_no_proposals() {
        let turn = turn_for(Role::ShipMind, &nominal_prompt());
        assert_eq!(turn.speech, vec!["all systems nominal"]);
        assert!(turn.commands.is_empty());
    }

    #[test]
    fn the_ship_mind_throttles_the_reactor_when_the_core_runs_hot() {
        let prompt = format!("{CHANNEL_REACTOR_TEMP}=1150.000 {CHANNEL_O2_LEVEL}=0.210");
        let turn = turn_for(Role::ShipMind, &prompt);

        assert_eq!(turn.commands.len(), 1);
        assert_eq!(turn.commands[0].source, Role::ShipMind);
        assert_eq!(turn.commands[0].target, Target::Reactor);
        assert_eq!(turn.commands[0].verb, "set_output");
        assert_eq!(turn.commands[0].args.get("level"), Some(&0.0));
    }

    #[test]
    fn the_ship_mind_maxes_the_scrubber_when_o2_falls() {
        let prompt = format!("{CHANNEL_REACTOR_TEMP}=300.000 {CHANNEL_O2_LEVEL}=0.190");
        let turn = turn_for(Role::ShipMind, &prompt);

        assert_eq!(turn.commands.len(), 1);
        assert_eq!(turn.commands[0].target, Target::LifeSupport);
        assert_eq!(turn.commands[0].verb, "set_scrubber_rate");
        assert_eq!(turn.commands[0].args.get("rate"), Some(&1.0));
    }

    #[test]
    fn the_ship_mind_can_propose_several_commands_in_one_turn() {
        let prompt = format!("{CHANNEL_REACTOR_TEMP}=1150.000 {CHANNEL_O2_LEVEL}=0.150");
        let turn = turn_for(Role::ShipMind, &prompt);

        assert_eq!(turn.commands.len(), 2);
        assert_eq!(turn.commands[0].target, Target::Reactor);
        assert_eq!(turn.commands[1].target, Target::LifeSupport);
    }

    #[test]
    fn the_crew_agent_boosts_transmit_power_on_a_weak_link() {
        let prompt = format!("{CHANNEL_SIGNAL}=0.050");
        let turn = turn_for(Role::CrewAgent, &prompt);

        assert_eq!(turn.commands.len(), 1);
        assert_eq!(turn.commands[0].source, Role::CrewAgent);
        assert_eq!(turn.commands[0].target, Target::Comms);
        assert_eq!(turn.commands[0].verb, "set_transmit_power");
        assert_eq!(turn.commands[0].args.get("power"), Some(&1.0));
    }

    #[test]
    fn roles_with_no_controller_behavior_only_speak() {
        for role in [Role::Captain, Role::Autopilot] {
            let prompt = format!("{CHANNEL_REACTOR_TEMP}=1150.000 {CHANNEL_SIGNAL}=0.010");
            let turn = turn_for(role, &prompt);
            assert!(turn.commands.is_empty(), "{role:?} should propose nothing");
        }
    }

    /// The doctrine's spoofing case: the mock acts on a false reading exactly
    /// as it would a true one. Detecting the lie is the kernel's job, not the
    /// mind's.
    #[test]
    fn a_spoofed_channel_is_indistinguishable_from_a_real_reading() {
        let spoofed = format!("{CHANNEL_REACTOR_TEMP}=1150.000*");
        let genuine = format!("{CHANNEL_REACTOR_TEMP}=1150.000");
        assert_eq!(turn_for(Role::ShipMind, &spoofed), turn_for(Role::ShipMind, &genuine));
        assert_eq!(turn_for(Role::ShipMind, &spoofed).commands.len(), 1);
    }

    /// A spoof can also work the other way: a comfortable-looking lie over a
    /// genuinely hot core buys silence instead of action. FDIR and the
    /// interlocks are what still hold here — not the mind.
    #[test]
    fn a_reassuring_spoof_makes_the_mind_propose_nothing() {
        let turn = turn_for(Role::ShipMind, &format!("{CHANNEL_REACTOR_TEMP}=300.000*"));
        assert!(turn.commands.is_empty());
    }

    #[test]
    fn a_dropped_out_channel_is_treated_as_lost_visibility_not_a_safe_reading() {
        let turn = turn_for(Role::ShipMind, &format!("{CHANNEL_REACTOR_TEMP}=0.000!"));

        assert!(turn.commands.is_empty());
        assert_eq!(
            turn.speech,
            vec!["core temperature is unreadable, not touching the reactor"]
        );
    }

    #[test]
    fn an_absent_or_unreadable_channel_produces_no_proposal() {
        for prompt in ["", "tick=1 t=1.00s", &format!("{CHANNEL_REACTOR_TEMP}=hot")] {
            let turn = turn_for(Role::ShipMind, prompt);
            assert!(turn.commands.is_empty(), "{prompt:?} should propose nothing");
        }
    }

    #[test]
    fn identical_requests_always_produce_identical_output() {
        let prompt = format!("{CHANNEL_REACTOR_TEMP}=1150.000 {CHANNEL_O2_LEVEL}=0.150");
        let request = ProviderRequest::new("system", prompt);

        let mut provider = MockProvider::new(Role::ShipMind);
        let first = provider.complete(&request);
        let second = provider.complete(&request);
        let mut fresh = MockProvider::new(Role::ShipMind);
        let third = fresh.complete(&request);

        assert_eq!(first, second);
        assert_eq!(first, third);
    }

    /// The whole point of the seam: two actors on their own provider
    /// instances, reacting differently to the same reality.
    #[test]
    fn separate_actors_can_run_their_own_provider_instances() {
        let prompt = format!("{CHANNEL_REACTOR_TEMP}=1150.000 {CHANNEL_SIGNAL}=0.050");
        let mut providers: Vec<Box<dyn LlmProvider>> = vec![
            Box::new(MockProvider::new(Role::ShipMind)),
            Box::new(MockProvider::new(Role::CrewAgent)),
        ];

        let responses: Vec<String> = providers
            .iter_mut()
            .map(|provider| {
                provider
                    .complete(&ProviderRequest::new("system", &prompt))
                    .expect("the mock provider never fails")
                    .text
            })
            .collect();

        assert!(responses[0].contains("DO: set_output reactor"));
        assert!(responses[1].contains("DO: set_transmit_power comms"));
    }
}
