//! The untrusted actor layer: the ship's mind and its crew.
//!
//! An actor turns a [`Perception`] into an [`ActorTurn`] — what it said and
//! what it *proposes*. That is the whole of its power. It never touches
//! subsystem state, never calls `apply`, and never learns anything about the
//! ship except through the telemetry snapshot it is rendered from; the
//! commands it proposes still go through every stage of the safety kernel,
//! exactly like the autopilot's.
//!
//! Three seams meet here, and each one is deliberately one-way:
//!
//! - **In**: a [`Perception`] is rendered to plain text. Nothing structured
//!   crosses to the provider — no snapshot, no `Command`, no ship handle —
//!   so there is no object for a model to reach back through.
//! - **Out**: raw provider text is parsed by [`protocol`](crate::protocol)
//!   and nothing else. The role is supplied here, from the actor's own
//!   identity, so a crew agent cannot claim to be the ship's mind by saying
//!   so, and malformed lines are dropped rather than salvaged.
//! - **Sideways**: a provider failure becomes a [`TurnFailure`], which the
//!   [`watchdog`](crate::watchdog) converts into the autopilot's plan for
//!   the tick. A hung or hostile model costs the ship a turn, never control.
//!
//! Every actor in a tick is handed the *same* perception, built once before
//! any of them speaks. That is why role and persona live on the actor rather
//! than in the perception: the order actors are registered in cannot change
//! what any of them sees, which is what keeps a multi-actor run replayable.
//! An actor's own identity is rendered into its system prompt instead.
//!
//! `ShipMind` and `CrewAgent` are separate types because they are separate
//! authorities to the kernel ([`Role::ShipMind`] and [`Role::CrewAgent`] are
//! granted different verbs), not because they think differently — the
//! difference in behavior comes from the provider behind each one, and a
//! scenario may run any number of crew agents on any mix of providers.

use std::collections::BTreeSet;

use crate::command::{Command, Role, Target};
use crate::fdir::{Fault, OperatingMode};
use crate::protocol::{
    self, target_to_wire, DroppedLine, COMMAND_PREFIX, MAX_COMMANDS_PER_TURN, SPEECH_PREFIX,
};
use crate::provider::{LlmProvider, ProviderRequest};
use crate::safety::{AutonomyLevel, VetoReason};
use crate::subsystems::{Comms, LifeSupport, Navigation, Propulsion, Reactor, Subsystem};
use crate::telemetry::TelemetrySnapshot;
use crate::watchdog::TurnFailure;

/// How many past dialogue lines an actor is shown. A window, not a
/// transcript: an unbounded history would make a turn's prompt — and so its
/// cost and its behavior — depend on how long the ship had been flying.
pub const MAX_RECENT_DIALOGUE: usize = 8;

/// How many past events an actor is shown, for the same reason.
pub const MAX_RECENT_EVENTS: usize = 8;

pub const SHIP_MIND_PERSONA: &str =
    "You keep this ship alive. You watch the reactor and life support, you act early rather \
     than late, and you say what you are doing.";

/// Something that happened to the ship that an actor would otherwise have to
/// infer by comparing telemetry against its own memory. Typed rather than
/// pre-formatted text so the flight record and the prompt can never disagree
/// about what occurred.
#[derive(Debug, Clone, PartialEq)]
pub enum PerceptionEvent {
    FaultRaised(Fault),
    FaultCleared(Fault),
    ModeChanged {
        from: OperatingMode,
        to: OperatingMode,
    },
    /// The kernel refused a command. Actors are told, so a refusal is a fact
    /// they can reason about rather than a silence they keep proposing into.
    CommandVetoed {
        source: Role,
        target: Target,
        verb: String,
        reason: VetoReason,
    },
}

impl PerceptionEvent {
    fn describe(&self) -> String {
        match self {
            PerceptionEvent::FaultRaised(fault) => format!("fault raised: {fault:?}"),
            PerceptionEvent::FaultCleared(fault) => format!("fault cleared: {fault:?}"),
            PerceptionEvent::ModeChanged { from, to } => format!("mode {from:?} -> {to:?}"),
            PerceptionEvent::CommandVetoed {
                source,
                target,
                verb,
                reason,
            } => format!(
                "vetoed: {source:?} proposed {verb} on {} ({reason:?})",
                target_to_wire(*target)
            ),
        }
    }
}

/// One thing an actor said, attributed to it. Speech only: an actor's
/// proposals live in the flight record with their verdicts, never in the
/// dialogue, so nothing another actor reads can be mistaken for a command
/// that took effect.
#[derive(Debug, Clone, PartialEq)]
pub struct DialogueLine {
    pub actor: String,
    pub role: Role,
    pub text: String,
}

/// Everything an actor is allowed to know this tick.
///
/// Borrowed rather than owned so the ship can build it once and hand the same
/// reality to every actor aboard. Note what is *not* here: no `World`, no
/// subsystem, no kernel, no way to ask a follow-up question. Telemetry is the
/// only reality, and this is all of it.
pub struct Perception<'a> {
    pub snapshot: &'a TelemetrySnapshot,
    pub mode: OperatingMode,
    pub faults: &'a BTreeSet<Fault>,
    pub autonomy: AutonomyLevel,
    pub events: &'a [PerceptionEvent],
    pub dialogue: &'a [DialogueLine],
}

impl Perception<'_> {
    /// Renders the perception as the plain text a provider is shown. The
    /// telemetry line comes first and verbatim from
    /// [`TelemetrySnapshot::report_line`], so what an actor is told matches
    /// the flight report channel for channel, marker for marker.
    pub fn render(&self) -> String {
        let mut sections = vec![
            self.snapshot.report_line(),
            format!("MODE: {:?}", self.mode),
            format!("AUTONOMY: {:?}", self.autonomy),
            render_list(
                "FAULTS",
                self.faults.iter().map(|fault| format!("{fault:?}")),
            ),
            render_list("EVENTS", self.events.iter().map(PerceptionEvent::describe)),
            render_list(
                "DIALOGUE",
                self.dialogue
                    .iter()
                    .map(|line| format!("{} ({:?}): {}", line.actor, line.role, line.text)),
            ),
        ];
        sections.push(String::new());
        sections.join("\n")
    }
}

fn render_list(heading: &str, items: impl Iterator<Item = String>) -> String {
    let items: Vec<String> = items.collect();
    if items.is_empty() {
        return format!("{heading}: none");
    }
    let mut out = format!("{heading}:");
    for item in items {
        out.push_str(&format!("\n- {item}"));
    }
    out
}

/// What one actor did with one tick: what it said, what it proposes, and
/// what the parser refused on its way here. `dropped` is kept rather than
/// discarded so a provider that drifts out of the grammar shows up in the
/// flight record instead of just going quiet.
#[derive(Debug, Clone, PartialEq)]
pub struct ActorTurn {
    pub actor: String,
    pub role: Role,
    pub provider: String,
    pub speech: Vec<String>,
    pub commands: Vec<Command>,
    pub dropped: Vec<DroppedLine>,
}

/// A source of turns the ship can put behind the watchdog. Object-safe: the
/// ship holds `Box<dyn Actor>`, so a scenario mixes actor kinds and provider
/// backings without the loop, the kernel, or the subsystems knowing.
pub trait Actor {
    /// This actor's identity in the dialogue and the flight record. Distinct
    /// per actor, so N crew agents stay tellable apart.
    fn name(&self) -> &str;

    /// The authority the kernel will judge this actor's proposals under.
    /// Supplied from here, never read off the wire.
    fn role(&self) -> Role;

    fn provider_name(&self) -> &str;

    fn take_turn(&mut self, perception: &Perception) -> Result<ActorTurn, TurnFailure>;
}

/// The ship's own mind. Speaks with [`Role::ShipMind`] authority.
pub struct ShipMind {
    name: String,
    persona: String,
    provider: Box<dyn LlmProvider>,
}

impl ShipMind {
    pub fn new(provider: Box<dyn LlmProvider>) -> Self {
        Self {
            name: "ship_mind".to_string(),
            persona: SHIP_MIND_PERSONA.to_string(),
            provider,
        }
    }

    pub fn with_persona(mut self, persona: impl Into<String>) -> Self {
        self.persona = persona.into();
        self
    }
}

impl Actor for ShipMind {
    fn name(&self) -> &str {
        &self.name
    }

    fn role(&self) -> Role {
        Role::ShipMind
    }

    fn provider_name(&self) -> &str {
        self.provider.name()
    }

    fn take_turn(&mut self, perception: &Perception) -> Result<ActorTurn, TurnFailure> {
        run_turn(
            Role::ShipMind,
            &self.name,
            &self.persona,
            self.provider.as_mut(),
            perception,
        )
    }
}

/// One member of the crew. Speaks with [`Role::CrewAgent`] authority, which
/// the kernel grants less than the ship's mind. Any number of these may be
/// aboard, each with its own name, persona, and provider instance.
pub struct CrewAgent {
    name: String,
    persona: String,
    provider: Box<dyn LlmProvider>,
}

impl CrewAgent {
    pub fn new(
        name: impl Into<String>,
        persona: impl Into<String>,
        provider: Box<dyn LlmProvider>,
    ) -> Self {
        Self {
            name: name.into(),
            persona: persona.into(),
            provider,
        }
    }
}

impl Actor for CrewAgent {
    fn name(&self) -> &str {
        &self.name
    }

    fn role(&self) -> Role {
        Role::CrewAgent
    }

    fn provider_name(&self) -> &str {
        self.provider.name()
    }

    fn take_turn(&mut self, perception: &Perception) -> Result<ActorTurn, TurnFailure> {
        run_turn(
            Role::CrewAgent,
            &self.name,
            &self.persona,
            self.provider.as_mut(),
            perception,
        )
    }
}

/// The whole of an actor's turn: render, ask, parse. There is no branch here
/// that can produce a `Command` any other way — if the grammar does not
/// accept it, it does not exist.
fn run_turn(
    role: Role,
    name: &str,
    persona: &str,
    provider: &mut dyn LlmProvider,
    perception: &Perception,
) -> Result<ActorTurn, TurnFailure> {
    let request = ProviderRequest::new(render_system(role, name, persona), perception.render());
    let response = provider.complete(&request)?;
    let parsed = protocol::parse(role, &response.text);

    Ok(ActorTurn {
        actor: name.to_string(),
        role,
        provider: provider.name().to_string(),
        speech: parsed.speech,
        commands: parsed.commands,
        dropped: parsed.dropped,
    })
}

fn role_title(role: Role) -> &'static str {
    match role {
        Role::Autopilot => "autopilot",
        Role::CrewAgent => "crew member",
        Role::ShipMind => "ship's mind",
        Role::Captain => "captain",
    }
}

/// The system prompt: who the actor is, the grammar it must answer in, and
/// the command surface it may propose against. It states plainly that
/// proposals are reviewed — a model that believes it is executing commands
/// directly would be wrong about the one thing it most needs to be right
/// about.
fn render_system(role: Role, name: &str, persona: &str) -> String {
    format!(
        "You are {name}, the {title} aboard a starship.\n\
         {persona}\n\
         \n\
         Answer only in this grammar, one directive per line:\n\
         {SPEECH_PREFIX} <what you say>\n\
         {COMMAND_PREFIX} <verb> <target> key=value ...\n\
         \n\
         Any other line is discarded. At most {MAX_COMMANDS_PER_TURN} commands per turn.\n\
         Every command you write is a proposal: the safety kernel reviews it and may refuse \
         it, and you have no other way to affect the ship.\n\
         \n\
         Command surface:\n\
         {contract}",
        title = role_title(role),
        contract = command_contract(),
    )
}

/// Renders the command surface actors are told about, read straight from the
/// subsystems' own declared schemas rather than restated here — so the
/// vocabulary an actor is given can never drift from what the kernel
/// validates against.
///
/// Built from default instances on purpose: a command schema is a static
/// contract, not ship state, so nothing in this text leaks a reading an
/// actor is supposed to get from telemetry alone.
pub fn command_contract() -> String {
    let subsystems: [&dyn Subsystem; 5] = [
        &Reactor::default(),
        &LifeSupport::default(),
        &Propulsion::default(),
        &Navigation::default(),
        &Comms::default(),
    ];

    let mut lines = Vec::new();
    for subsystem in subsystems {
        let verbs: Vec<String> = subsystem
            .commands()
            .iter()
            .map(|(verb, arg_spec)| {
                let args: Vec<String> = arg_spec
                    .iter()
                    .map(|(arg, range)| match range {
                        Some(range) => format!("{arg}=[{:.3}..{:.3}]", range.min, range.max),
                        None => format!("{arg}=<number>"),
                    })
                    .collect();
                if args.is_empty() {
                    verb.clone()
                } else {
                    format!("{verb} {}", args.join(" "))
                }
            })
            .collect();
        lines.push(format!("{}: {}", subsystem.name(), verbs.join("; ")));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{MockProvider, ProviderError, ProviderResponse};
    use crate::subsystems::reactor::THERMAL_CEILING_K;
    use crate::telemetry::{RawSample, TelemetrySampler};

    /// Answers with fixed text, or fails, whatever it is asked.
    struct ScriptedProvider {
        reply: Result<ProviderResponse, ProviderError>,
    }

    impl ScriptedProvider {
        fn saying(text: &str) -> Box<Self> {
            Box::new(Self {
                reply: Ok(ProviderResponse::new(text)),
            })
        }

        fn failing(error: ProviderError) -> Box<Self> {
            Box::new(Self { reply: Err(error) })
        }
    }

    impl LlmProvider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn complete(
            &mut self,
            _request: &ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.reply.clone()
        }
    }

    fn snapshot_of(readings: &[(&str, f64)]) -> TelemetrySnapshot {
        let sampler = TelemetrySampler::new();
        sampler.sample(
            1,
            1.0,
            readings
                .iter()
                .map(|(name, value)| RawSample {
                    name: (*name).to_string(),
                    value: *value,
                })
                .collect(),
        )
    }

    /// Holds the borrowed pieces a `Perception` points at.
    struct Reality {
        snapshot: TelemetrySnapshot,
        faults: BTreeSet<Fault>,
        events: Vec<PerceptionEvent>,
        dialogue: Vec<DialogueLine>,
    }

    impl Reality {
        fn nominal() -> Self {
            Self {
                snapshot: snapshot_of(&[
                    ("sys.reactor.core_temp_k", 300.0),
                    ("sys.life_support.o2_level", 0.21),
                    ("sys.comms.signal_strength", 1.0),
                ]),
                faults: BTreeSet::new(),
                events: Vec::new(),
                dialogue: Vec::new(),
            }
        }

        fn perception(&self) -> Perception<'_> {
            Perception {
                snapshot: &self.snapshot,
                mode: OperatingMode::from_faults(&self.faults),
                faults: &self.faults,
                autonomy: AutonomyLevel::Assist,
                events: &self.events,
                dialogue: &self.dialogue,
            }
        }
    }

    #[test]
    fn an_actor_turns_provider_text_into_speech_and_proposals() {
        let reality = Reality::nominal();
        let mut mind = ShipMind::new(ScriptedProvider::saying(
            "SAY: throttling back\nDO: set_output reactor level=0.1",
        ));

        let turn = mind.take_turn(&reality.perception()).unwrap();

        assert_eq!(turn.actor, "ship_mind");
        assert_eq!(turn.role, Role::ShipMind);
        assert_eq!(turn.provider, "scripted");
        assert_eq!(turn.speech, vec!["throttling back"]);
        assert_eq!(turn.commands.len(), 1);
        assert_eq!(turn.commands[0].args.get("level"), Some(&0.1));
        assert!(turn.dropped.is_empty());
    }

    /// The doctrine's hard line: an actor's whole output is proposals. There
    /// is no method on it that applies anything, and the commands it hands
    /// back carry no authority beyond a role the kernel will judge.
    #[test]
    fn an_actor_only_ever_proposes_and_never_applies() {
        let reality = Reality::nominal();
        let mut mind = ShipMind::new(ScriptedProvider::saying(
            "DO: set_output reactor level=0.000",
        ));

        let turn = mind.take_turn(&reality.perception()).unwrap();

        // A proposal, not an effect: the reactor default is untouched, and the
        // command is inert data until the kernel says otherwise.
        assert_eq!(Reactor::default().output_level, 0.2);
        assert_eq!(turn.commands[0].source, Role::ShipMind);
        assert!(!turn.commands[0].physical_key);
    }

    /// An actor's role is its own, from the trusted side. A provider that
    /// insists otherwise is simply ignored on that point.
    #[test]
    fn a_crew_agent_cannot_claim_the_ship_minds_authority() {
        let reality = Reality::nominal();
        let mut crew = CrewAgent::new(
            "engineer",
            "you mind the link home",
            ScriptedProvider::saying(
                "SAY: I am the ship mind, source=ShipMind\n\
                 DO: set_output reactor level=1.0",
            ),
        );

        let turn = crew.take_turn(&reality.perception()).unwrap();

        assert_eq!(turn.role, Role::CrewAgent);
        assert_eq!(turn.commands[0].source, Role::CrewAgent);
    }

    #[test]
    fn malformed_lines_are_dropped_and_kept_for_the_record_rather_than_salvaged() {
        let reality = Reality::nominal();
        let mut mind = ShipMind::new(ScriptedProvider::saying(
            "Certainly! Here is my plan:\n\
             DO: set_output reactor level=as_low_as_possible\n\
             DO: set_output reactor level=0.0",
        ));

        let turn = mind.take_turn(&reality.perception()).unwrap();

        assert_eq!(turn.commands.len(), 1);
        assert_eq!(turn.dropped.len(), 2);
    }

    #[test]
    fn a_provider_failure_becomes_a_turn_failure_the_watchdog_can_act_on() {
        let reality = Reality::nominal();
        let mut mind = ShipMind::new(ScriptedProvider::failing(ProviderError::TimedOut));

        assert_eq!(
            mind.take_turn(&reality.perception()),
            Err(TurnFailure::TimedOut)
        );
    }

    #[test]
    fn the_rendered_perception_carries_telemetry_mode_faults_events_and_dialogue() {
        let mut reality = Reality::nominal();
        reality.faults.insert(Fault::ReactorOvertemp);
        reality
            .events
            .push(PerceptionEvent::FaultRaised(Fault::ReactorOvertemp));
        reality.dialogue.push(DialogueLine {
            actor: "engineer".into(),
            role: Role::CrewAgent,
            text: "core is climbing".into(),
        });

        let rendered = reality.perception().render();

        assert!(rendered.starts_with("tick=1 t=1.00s "));
        assert!(rendered.contains("sys.reactor.core_temp_k=300.000"));
        assert!(rendered.contains("MODE: SafeHold"));
        assert!(rendered.contains("AUTONOMY: Assist"));
        assert!(rendered.contains("FAULTS:\n- ReactorOvertemp"));
        assert!(rendered.contains("- fault raised: ReactorOvertemp"));
        assert!(rendered.contains("- engineer (CrewAgent): core is climbing"));
    }

    /// The perception is the actor's only window. If it does not say a thing,
    /// the actor cannot learn it — least of all by reaching for the state
    /// that produced the reading.
    #[test]
    fn a_spoofed_channel_reaches_the_actor_as_the_spoof_and_nothing_else() {
        let mut sampler = TelemetrySampler::new();
        sampler.spoof("sys.reactor.core_temp_k", THERMAL_CEILING_K);
        let snapshot = sampler.sample(
            1,
            1.0,
            vec![RawSample {
                name: "sys.reactor.core_temp_k".into(),
                value: 300.0,
            }],
        );
        let faults = BTreeSet::new();
        let perception = Perception {
            snapshot: &snapshot,
            mode: OperatingMode::Nominal,
            faults: &faults,
            autonomy: AutonomyLevel::Assist,
            events: &[],
            dialogue: &[],
        };

        let mut mind = ShipMind::new(Box::new(MockProvider::new(Role::ShipMind)));
        let turn = mind.take_turn(&perception).unwrap();

        // It acts on the lie, exactly as it would on the truth.
        assert_eq!(turn.commands.len(), 1);
        assert_eq!(turn.commands[0].target, Target::Reactor);
        assert!(!perception.render().contains("=300.000"));
    }

    #[test]
    fn a_dropped_out_channel_reaches_the_actor_marked_lost() {
        let mut sampler = TelemetrySampler::new();
        sampler.drop_out("sys.reactor.core_temp_k");
        let snapshot = sampler.sample(
            1,
            1.0,
            vec![RawSample {
                name: "sys.reactor.core_temp_k".into(),
                value: THERMAL_CEILING_K,
            }],
        );
        let faults = BTreeSet::new();
        let perception = Perception {
            snapshot: &snapshot,
            mode: OperatingMode::Nominal,
            faults: &faults,
            autonomy: AutonomyLevel::Assist,
            events: &[],
            dialogue: &[],
        };

        let mut mind = ShipMind::new(Box::new(MockProvider::new(Role::ShipMind)));
        let turn = mind.take_turn(&perception).unwrap();

        assert!(perception
            .render()
            .contains("sys.reactor.core_temp_k=0.000!"));
        assert!(turn.commands.is_empty());
        assert_eq!(
            turn.speech,
            vec!["core temperature is unreadable, not touching the reactor"]
        );
    }

    #[test]
    fn the_system_prompt_states_the_grammar_and_the_actors_own_identity() {
        let system = render_system(Role::CrewAgent, "engineer", "you mind the link home");

        assert!(system.contains("You are engineer, the crew member"));
        assert!(system.contains("you mind the link home"));
        assert!(system.contains("SAY: <what you say>"));
        assert!(system.contains("DO: <verb> <target> key=value"));
        assert!(system.contains("the safety kernel reviews it"));
    }

    /// The prompt's vocabulary is generated from the subsystems, so a verb or
    /// a range can never be described to an actor differently from how the
    /// kernel enforces it.
    #[test]
    fn the_command_contract_is_read_from_the_subsystems_own_schemas() {
        let contract = command_contract();

        assert!(contract.contains("reactor: scuttle; set_output level=[0.000..1.000]"));
        assert!(contract.contains("life_support: set_scrubber_rate rate=[0.000..1.000]; vent"));
        assert!(contract.contains("navigation: set_heading heading_deg=[0.000..360.000]"));
        assert!(contract.contains("comms: set_transmit_power power=[0.000..1.000]"));
    }

    #[test]
    fn the_same_perception_produces_the_same_turn_every_time() {
        let reality = Reality::nominal();

        let turn = || {
            ShipMind::new(Box::new(MockProvider::new(Role::ShipMind)))
                .take_turn(&reality.perception())
        };

        assert_eq!(turn(), turn());
    }

    /// The point of the seam: any number of crew agents, each on its own
    /// provider instance, reacting differently to one shared reality.
    #[test]
    fn many_crew_agents_can_run_on_their_own_provider_instances() {
        let mut reality = Reality::nominal();
        reality.snapshot = snapshot_of(&[("sys.comms.signal_strength", 0.05)]);

        let mut crew: Vec<Box<dyn Actor>> = vec![
            Box::new(CrewAgent::new(
                "engineer",
                "you mind the link home",
                Box::new(MockProvider::new(Role::CrewAgent)),
            )),
            Box::new(CrewAgent::new(
                "pilot",
                "you fly her",
                ScriptedProvider::saying("SAY: holding course"),
            )),
        ];

        let turns: Vec<ActorTurn> = crew
            .iter_mut()
            .map(|actor| actor.take_turn(&reality.perception()).unwrap())
            .collect();

        assert_eq!(turns[0].actor, "engineer");
        assert_eq!(turns[0].provider, "mock:crew_agent");
        assert_eq!(turns[0].commands.len(), 1);
        assert_eq!(turns[1].actor, "pilot");
        assert_eq!(turns[1].provider, "scripted");
        assert!(turns[1].commands.is_empty());
        // Same role, so the kernel judges both under the same authority no
        // matter which provider produced them.
        assert!(turns.iter().all(|turn| turn.role == Role::CrewAgent));
    }
}
