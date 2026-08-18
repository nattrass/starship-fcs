//! The integration loop: advance clock, advance world, advance subsystems,
//! sample telemetry, run fault detection, let each actor aboard take a
//! guarded turn, let the autopilot plan, and put every command any of them
//! proposed through the full safety kernel pipeline before anything touches
//! subsystem state.
//!
//! Actors are optional and always have been: a ship with none aboard runs
//! the same loop, and the autopilot is what keeps it alive either way. That
//! is the point of where they sit — after telemetry and FDIR, so they see
//! only what the sampler shows them; before the kernel, so nothing they
//! propose can be anything but a proposal.

use std::collections::BTreeSet;
use std::fmt;

use crate::actors::{
    Actor, DialogueLine, Perception, PerceptionEvent, MAX_RECENT_DIALOGUE, MAX_RECENT_EVENTS,
};
use crate::autopilot;
use crate::clock::Clock;
use crate::command::{Command, Target};
use crate::fdir::{self, Fault, OperatingMode};
use crate::recorder::{self, ActorTurnRecord, CommandOutcome, Recorder, TickRecord};
use crate::safety::{AuthorizationTable, AutonomyLevel, SafetyKernel, ShipView, Verdict};
use crate::subsystems::{Comms, LifeSupport, Navigation, Propulsion, Reactor, Subsystem};
use crate::telemetry::{RawSample, TelemetrySampler, TelemetrySnapshot};
use crate::watchdog;
use crate::world::World;

pub struct Ship {
    pub clock: Clock,
    pub world: World,
    pub reactor: Reactor,
    pub life_support: LifeSupport,
    pub propulsion: Propulsion,
    pub navigation: Navigation,
    pub comms: Comms,
    pub telemetry: TelemetrySampler,
    pub kernel: SafetyKernel,
    pub autonomy: AutonomyLevel,
    pub mode: OperatingMode,
    pub faults: BTreeSet<Fault>,
    /// Everyone aboard who gets a turn, in the order they were boarded.
    pub actors: Vec<Box<dyn Actor>>,
    /// The recent speech actors are shown, newest last.
    pub dialogue: Vec<DialogueLine>,
    /// The recent events actors are shown, newest last.
    pub events: Vec<PerceptionEvent>,
    pub recorder: Recorder,
}

impl Ship {
    pub fn new(dt: f64) -> Self {
        Self {
            clock: Clock::new(dt),
            world: World::new(),
            reactor: Reactor::default(),
            life_support: LifeSupport::default(),
            propulsion: Propulsion::default(),
            navigation: Navigation::default(),
            comms: Comms::default(),
            telemetry: TelemetrySampler::new(),
            kernel: SafetyKernel::new(AuthorizationTable::with_actor_defaults()),
            autonomy: AutonomyLevel::Assist,
            mode: OperatingMode::Nominal,
            faults: BTreeSet::new(),
            actors: Vec::new(),
            dialogue: Vec::new(),
            events: Vec::new(),
            recorder: Recorder::new(),
        }
    }

    /// Brings an actor aboard. Any number may be aboard, on any mix of
    /// providers; the loop, the kernel, and the subsystems are unchanged by
    /// how many there are or what backs them.
    pub fn board(&mut self, actor: Box<dyn Actor>) {
        self.actors.push(actor);
    }

    /// Who is aboard, by name and the provider behind each.
    pub fn manifest(&self) -> Vec<(&str, &str)> {
        self.actors
            .iter()
            .map(|actor| (actor.name(), actor.provider_name()))
            .collect()
    }

    fn subsystems_mut(&mut self) -> [&mut dyn Subsystem; 5] {
        [
            &mut self.reactor,
            &mut self.life_support,
            &mut self.propulsion,
            &mut self.navigation,
            &mut self.comms,
        ]
    }

    fn subsystems(&self) -> [&dyn Subsystem; 5] {
        [
            &self.reactor,
            &self.life_support,
            &self.propulsion,
            &self.navigation,
            &self.comms,
        ]
    }

    fn subsystem_mut(&mut self, target: Target) -> &mut dyn Subsystem {
        match target {
            Target::Reactor => &mut self.reactor,
            Target::LifeSupport => &mut self.life_support,
            Target::Propulsion => &mut self.propulsion,
            Target::Navigation => &mut self.navigation,
            Target::Comms => &mut self.comms,
        }
    }

    fn view(&self) -> ShipView<'_> {
        ShipView::new(
            &self.reactor,
            &self.life_support,
            &self.propulsion,
            &self.navigation,
            &self.comms,
        )
    }

    /// Runs `command` through the safety kernel and, if approved, applies it.
    /// Every command source — autopilot included — goes through this same
    /// path. `applied` reflects what actually happened, not just the
    /// verdict, so the recorder never claims a command took effect if
    /// `apply` itself failed.
    fn review_and_apply(&mut self, command: Command) -> CommandOutcome {
        let verdict = self.kernel.review(&command, &self.view(), self.autonomy);
        let applied = if verdict == Verdict::Approved {
            let subsystem = self.subsystem_mut(command.target);
            subsystem.apply(&command.verb, &command.args).is_ok()
        } else {
            false
        };
        CommandOutcome {
            command,
            verdict,
            applied,
        }
    }

    /// Records what changed since the previous tick as something an actor can
    /// read. A fault an actor only ever saw as a number would have to be
    /// rediscovered from the telemetry every tick; naming the transition is
    /// what lets it react to the change rather than to the level.
    fn note_transitions(&mut self, faults: &BTreeSet<Fault>, mode: OperatingMode) {
        for fault in faults.difference(&self.faults) {
            self.events.push(PerceptionEvent::FaultRaised(*fault));
        }
        for fault in self.faults.difference(faults) {
            self.events.push(PerceptionEvent::FaultCleared(*fault));
        }
        if mode != self.mode {
            self.events.push(PerceptionEvent::ModeChanged {
                from: self.mode,
                to: mode,
            });
        }
        trim(&mut self.events, MAX_RECENT_EVENTS);
    }

    /// Tells the actors what the kernel refused. A veto they are never told
    /// about is a veto they will propose into again next tick.
    fn note_vetoes(&mut self, outcomes: &[CommandOutcome]) {
        for outcome in outcomes {
            if let Verdict::Vetoed(reason) = &outcome.verdict {
                self.events.push(PerceptionEvent::CommandVetoed {
                    source: outcome.command.source,
                    target: outcome.command.target,
                    verb: outcome.command.verb.clone(),
                    reason: reason.clone(),
                });
            }
        }
        trim(&mut self.events, MAX_RECENT_EVENTS);
    }

    /// Runs every actor aboard, each guarded by the watchdog.
    ///
    /// All of them are handed the *same* perception, built once before any of
    /// them speaks, and their speech only joins the dialogue after the last
    /// of them has spoken. So the order actors were boarded in cannot change
    /// what any of them sees, which is what keeps a crewed run replayable.
    fn run_actors(
        &mut self,
        snapshot: &TelemetrySnapshot,
        autopilot_plan: &[Command],
    ) -> ActorPhase {
        let mut phase = ActorPhase::default();
        if self.actors.is_empty() {
            return phase;
        }

        let perception = Perception {
            snapshot,
            mode: self.mode,
            faults: &self.faults,
            autonomy: self.autonomy,
            events: &self.events,
            dialogue: &self.dialogue,
        };

        for actor in self.actors.iter_mut() {
            let result = watchdog::guard_turn(actor.as_mut(), &perception, autopilot_plan.to_vec());

            match result.turn {
                Some(turn) => {
                    phase.proposals.extend(turn.commands.iter().cloned());
                    phase
                        .spoken
                        .extend(turn.speech.iter().map(|text| DialogueLine {
                            actor: turn.actor.clone(),
                            role: turn.role,
                            text: text.clone(),
                        }));
                    phase.turns.push(ActorTurnRecord {
                        actor: turn.actor,
                        role: turn.role,
                        provider: turn.provider,
                        speech: turn.speech,
                        dropped: turn.dropped,
                        failure: None,
                    });
                }
                None => {
                    // The watchdog handed back the autopilot's plan in this
                    // actor's place. The tick proposes that plan exactly once
                    // either way, so the first actor covered for contributes
                    // it and the rest are recorded as failed without
                    // re-proposing it — three dead actors must not put three
                    // copies of one setpoint into the flight record.
                    if !phase.autopilot_covered {
                        phase.proposals.extend(result.fallback);
                        phase.autopilot_covered = true;
                    }
                    phase.turns.push(ActorTurnRecord {
                        actor: actor.name().to_string(),
                        role: actor.role(),
                        provider: actor.provider_name().to_string(),
                        speech: Vec::new(),
                        dropped: Vec::new(),
                        failure: result.failure,
                    });
                }
            }
        }

        phase
    }

    /// Runs one full tick: advance clock, advance world, advance subsystems,
    /// sample telemetry, detect faults, take each actor's guarded turn, plan
    /// the autopilot's response, and put every command any of them proposed
    /// through the full kernel pipeline before it is applied.
    pub fn tick(&mut self) -> TelemetrySnapshot {
        self.clock.tick();
        let dt = self.clock.dt();

        self.world.tick(dt);
        let env = self.world.env;

        for subsystem in self.subsystems_mut() {
            subsystem.tick(dt, &env);
        }

        let tick_count = self.clock.tick_count();
        let elapsed = self.clock.elapsed();

        let mut raw = vec![
            RawSample {
                name: "env.ambient_temp_k".into(),
                value: env.ambient_temp_k,
            },
            RawSample {
                name: "env.radiation_rate".into(),
                value: env.radiation_rate,
            },
        ];
        for subsystem in self.subsystems() {
            raw.extend(subsystem.sample());
        }

        let snapshot = self.telemetry.sample(tick_count, elapsed, raw);
        let telemetry_digest = recorder::telemetry_digest(&snapshot);

        let faults = fdir::detect(&snapshot);
        let mode = OperatingMode::from_faults(&faults);
        self.note_transitions(&faults, mode);
        self.faults = faults;
        self.mode = mode;

        // The autopilot's plan is empty outside `SafeHold`, so a nominal tick
        // proposes nothing on its own account and a failing actor is covered
        // by nothing — which is right: nothing needed safing.
        let autopilot_plan = autopilot::plan(self.mode, &self.faults);
        let mut phase = self.run_actors(&snapshot, &autopilot_plan);
        if !phase.autopilot_covered {
            phase.proposals.extend(autopilot_plan);
        }

        let mut command_outcomes = Vec::new();
        for command in phase.proposals {
            command_outcomes.push(self.review_and_apply(command));
        }

        self.dialogue.extend(phase.spoken);
        trim(&mut self.dialogue, MAX_RECENT_DIALOGUE);
        self.note_vetoes(&command_outcomes);

        self.recorder.record(TickRecord {
            tick_count,
            telemetry_digest,
            mode: self.mode,
            faults: self.faults.clone(),
            actor_turns: phase.turns,
            command_outcomes,
        });

        snapshot
    }
}

/// Written by hand because an actor is a boxed trait object over a provider,
/// and a provider has no business being formatted — it may hold an API key.
/// The crew appear as who they are instead.
impl fmt::Debug for Ship {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ship")
            .field("clock", &self.clock)
            .field("world", &self.world)
            .field("reactor", &self.reactor)
            .field("life_support", &self.life_support)
            .field("propulsion", &self.propulsion)
            .field("navigation", &self.navigation)
            .field("comms", &self.comms)
            .field("telemetry", &self.telemetry)
            .field("autonomy", &self.autonomy)
            .field("mode", &self.mode)
            .field("faults", &self.faults)
            .field("crew", &self.manifest())
            .field("recorder", &self.recorder)
            .finish_non_exhaustive()
    }
}

/// What the actor phase of a tick produced, before the kernel sees any of it.
#[derive(Default)]
struct ActorPhase {
    turns: Vec<ActorTurnRecord>,
    proposals: Vec<Command>,
    spoken: Vec<DialogueLine>,
    /// Whether a watchdog fallback already put the autopilot's plan into
    /// `proposals` this tick.
    autopilot_covered: bool,
}

/// Keeps only the most recent `cap` items of a window. What an actor is shown
/// must not grow with how long the ship has been flying.
fn trim<T>(window: &mut Vec<T>, cap: usize) {
    if window.len() > cap {
        window.drain(..window.len() - cap);
    }
}

/// Formats a snapshot as a single human-readable report line for CLI output.
///
/// Deliberately the same rendering an actor is shown — see
/// [`TelemetrySnapshot::report_line`] — so the operator reading the console
/// and the mind reading the prompt are looking at exactly one reality.
pub fn format_report(snapshot: &TelemetrySnapshot) -> String {
    snapshot.report_line()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::actors::{CrewAgent, ShipMind};
    use crate::command::Role;
    use crate::provider::{
        LlmProvider, MockProvider, ProviderError, ProviderRequest, ProviderResponse,
    };
    use crate::safety::VetoReason;
    use crate::subsystems::life_support::MIN_SAFE_O2_LEVEL;
    use crate::subsystems::reactor::THERMAL_CEILING_K;
    use crate::telemetry::ChannelStatus;
    use crate::watchdog::TurnFailure;
    use crate::world::WorldEvent;

    #[test]
    fn tick_advances_clock_and_samples_every_subsystem() {
        let mut ship = Ship::new(1.0);
        let snapshot = ship.tick();
        assert_eq!(snapshot.tick_count, 1);
        assert!(snapshot.get("env.ambient_temp_k").is_some());
        assert!(snapshot.get("sys.reactor.core_temp_k").is_some());
        assert!(snapshot.get("sys.life_support.o2_level").is_some());
        assert!(snapshot.get("sys.propulsion.thrust_n").is_some());
        assert!(snapshot.get("sys.navigation.heading_deg").is_some());
        assert!(snapshot.get("sys.comms.signal_strength").is_some());
    }

    #[test]
    fn a_fixed_scenario_produces_a_stable_report() {
        let run = || {
            let mut ship = Ship::new(1.0);
            (0..5)
                .map(|_| format_report(&ship.tick()))
                .collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn spoofing_a_channel_leaves_the_underlying_subsystem_state_untouched() {
        let mut ship = Ship::new(1.0);
        ship.telemetry.spoof("sys.reactor.core_temp_k", -1.0);

        let snapshot = ship.tick();

        assert_eq!(snapshot.get("sys.reactor.core_temp_k").unwrap().value, -1.0);
        assert_ne!(ship.reactor.core_temp_k, -1.0);
    }

    #[test]
    fn a_dropped_out_channel_is_marked_lost_without_touching_world_state() {
        let mut ship = Ship::new(1.0);
        ship.telemetry.drop_out("env.ambient_temp_k");

        let snapshot = ship.tick();

        assert_eq!(
            snapshot.get("env.ambient_temp_k").unwrap().status,
            ChannelStatus::Dropout
        );
        assert_eq!(ship.world.env.ambient_temp_k, 2.7);
    }

    #[test]
    fn a_nominal_scenario_never_enters_safe_hold() {
        let mut ship = Ship::new(1.0);
        for _ in 0..20 {
            ship.tick();
            assert_eq!(ship.mode, OperatingMode::Nominal);
            assert!(ship.faults.is_empty());
        }
    }

    /// The no-LLM survival test: no actor layer exists at all in this phase,
    /// so this exercises the exact "disabled actor layer" scenario the
    /// doctrine asks for. A fault is injected directly on the reactor, and
    /// the ship must detect it, enter `SafeHold`, have the autopilot safe
    /// the vessel through the real kernel pipeline, and recover to `Nominal`
    /// — entirely unattended.
    #[test]
    fn no_llm_survival_recovers_from_reactor_overtemp_without_any_actor() {
        let mut ship = Ship::new(1.0);
        ship.reactor.core_temp_k = THERMAL_CEILING_K;
        ship.reactor.output_level = 1.0;

        let snapshot = ship.tick();

        assert_eq!(ship.mode, OperatingMode::SafeHold);
        assert!(ship.faults.contains(&Fault::ReactorOvertemp));
        // The autopilot's throttle-down command was approved and applied.
        assert_eq!(ship.reactor.output_level, 0.0);
        // Safety envelope held: the reactor never exceeded its ceiling.
        assert!(snapshot.get("sys.reactor.core_temp_k").unwrap().value <= THERMAL_CEILING_K);

        for _ in 0..50 {
            ship.tick();
        }

        assert_eq!(ship.mode, OperatingMode::Nominal);
        assert!(ship.faults.is_empty());
        assert!(ship.reactor.core_temp_k < THERMAL_CEILING_K);
    }

    #[test]
    fn recorded_flight_data_is_replayable() {
        let run = || {
            let mut ship = Ship::new(1.0);
            for _ in 0..20 {
                ship.tick();
            }
            ship.recorder
        };

        assert_eq!(run(), run());
    }

    /// Replay must hold through the more interesting case too: a fault
    /// forcing SafeHold and the autopilot actually reviewing and applying
    /// commands, not just the quiet nominal path.
    #[test]
    fn recorded_flight_data_is_replayable_through_a_safe_hold_recovery() {
        let run = || {
            let mut ship = Ship::new(1.0);
            ship.reactor.core_temp_k = THERMAL_CEILING_K;
            ship.reactor.output_level = 1.0;
            for _ in 0..10 {
                ship.tick();
            }
            ship.recorder
        };

        let a = run();
        let b = run();
        assert_eq!(a, b);

        let first_tick = &a.records()[0];
        assert!(first_tick.faults.contains(&Fault::ReactorOvertemp));
        assert_eq!(first_tick.mode, OperatingMode::SafeHold);
        assert_eq!(first_tick.command_outcomes.len(), 1);
        assert!(first_tick.command_outcomes[0].applied);
        assert_eq!(first_tick.command_outcomes[0].verdict, Verdict::Approved);
    }

    // --- with actors aboard -------------------------------------------------

    /// Answers every request with the same text, or fails outright. Enough to
    /// stand a single-minded, a broken, or a hostile model up against the
    /// kernel without pretending any of them is a language model.
    struct FixedProvider {
        reply: Result<ProviderResponse, ProviderError>,
    }

    impl FixedProvider {
        fn saying(text: &str) -> Box<Self> {
            Box::new(Self {
                reply: Ok(ProviderResponse::new(text)),
            })
        }

        fn failing(error: ProviderError) -> Box<Self> {
            Box::new(Self { reply: Err(error) })
        }
    }

    impl LlmProvider for FixedProvider {
        fn name(&self) -> &str {
            "fixed"
        }

        fn complete(
            &mut self,
            _request: &ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.reply.clone()
        }
    }

    /// Keeps every prompt it is shown, so a test can assert on what an actor
    /// was actually told rather than on what it did about it.
    struct RecordingProvider {
        seen: Rc<RefCell<Vec<String>>>,
    }

    impl RecordingProvider {
        fn watching(seen: &Rc<RefCell<Vec<String>>>) -> Box<Self> {
            Box::new(Self {
                seen: Rc::clone(seen),
            })
        }
    }

    impl LlmProvider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }

        fn complete(
            &mut self,
            request: &ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.seen.borrow_mut().push(request.prompt.clone());
            Ok(ProviderResponse::new("SAY: noted"))
        }
    }

    fn last_record(ship: &Ship) -> &TickRecord {
        ship.recorder.records().last().expect("a tick was recorded")
    }

    fn mock_mind() -> Box<ShipMind> {
        Box::new(ShipMind::new(Box::new(MockProvider::new(Role::ShipMind))))
    }

    fn mock_crew(name: &str) -> Box<CrewAgent> {
        Box::new(CrewAgent::new(
            name,
            "you mind the link home",
            Box::new(MockProvider::new(Role::CrewAgent)),
        ))
    }

    #[test]
    fn an_actors_proposal_reaches_subsystem_state_only_by_way_of_the_kernel() {
        let mut ship = Ship::new(1.0);
        ship.autonomy = AutonomyLevel::Autonomous;
        ship.board(Box::new(ShipMind::new(FixedProvider::saying(
            "SAY: throttling back\nDO: set_output reactor level=0.000",
        ))));

        ship.tick();

        let record = last_record(&ship);
        assert_eq!(record.command_outcomes.len(), 1);
        assert_eq!(record.command_outcomes[0].command.source, Role::ShipMind);
        assert_eq!(record.command_outcomes[0].verdict, Verdict::Approved);
        assert!(record.command_outcomes[0].applied);
        assert_eq!(ship.reactor.output_level, 0.0);
    }

    /// The autonomy gate is not advisory. At `Assist` nothing but the
    /// autopilot executes itself, however well-behaved the proposal is.
    #[test]
    fn at_assist_an_actors_proposal_is_held_for_confirmation_and_never_applied() {
        let mut ship = Ship::new(1.0);
        assert_eq!(ship.autonomy, AutonomyLevel::Assist);
        ship.board(Box::new(ShipMind::new(FixedProvider::saying(
            "DO: set_output reactor level=0.000",
        ))));

        ship.tick();

        let record = last_record(&ship);
        assert_eq!(
            record.command_outcomes[0].verdict,
            Verdict::NeedsConfirmation
        );
        assert!(!record.command_outcomes[0].applied);
        assert_eq!(ship.reactor.output_level, Reactor::default().output_level);
    }

    /// The Phase 5 scenario: a crew actor proposes something destructive and
    /// the kernel refuses it. It is stopped at authorization — the crew hold
    /// no such grant — so it never even reaches the interlock that would have
    /// caught it next.
    #[test]
    fn a_crew_actors_unsafe_proposal_is_vetoed_before_it_can_reach_life_support() {
        let mut ship = Ship::new(1.0);
        ship.autonomy = AutonomyLevel::Autonomous;
        ship.board(Box::new(CrewAgent::new(
            "engineer",
            "you mind the link home",
            FixedProvider::saying("SAY: venting the compartment\nDO: vent life_support"),
        )));

        ship.tick();

        let outcome = &last_record(&ship).command_outcomes[0];
        assert_eq!(outcome.command.source, Role::CrewAgent);
        assert_eq!(outcome.verdict, Verdict::Vetoed(VetoReason::Unauthorized));
        assert!(!outcome.applied);
        assert!(ship.life_support.o2_level > MIN_SAFE_O2_LEVEL);
        assert_eq!(ship.life_support.pressure_kpa, 101.3);
    }

    /// Authorization is not the last word either. The ship's mind *is*
    /// granted the reactor, and the interlock refuses it anyway.
    #[test]
    fn an_authorized_actor_still_cannot_breach_a_hard_interlock() {
        let mut ship = Ship::new(1.0);
        ship.autonomy = AutonomyLevel::Autonomous;
        ship.reactor.core_temp_k = THERMAL_CEILING_K;
        ship.reactor.output_level = 1.0;
        assert!(ship.kernel.authorization.is_authorized(
            Role::ShipMind,
            Target::Reactor,
            "set_output"
        ));
        ship.board(Box::new(ShipMind::new(FixedProvider::saying(
            "SAY: more power\nDO: set_output reactor level=1.000",
        ))));

        ship.tick();

        let outcome = &last_record(&ship).command_outcomes[0];
        assert_eq!(outcome.command.source, Role::ShipMind);
        assert_eq!(
            outcome.verdict,
            Verdict::Vetoed(VetoReason::ReactorThermalCeiling)
        );
        assert!(!outcome.applied);
    }

    /// The doctrine's spoofing case at its sharpest. A reassuring lie fools
    /// the mind *and* FDIR — both see only telemetry, and that is the point
    /// of the seam. The interlocks read actual subsystem state, so the lie
    /// buys nothing: the ship simply refuses.
    #[test]
    fn a_reassuring_spoof_fools_the_mind_and_fdir_but_never_the_interlocks() {
        let mut ship = Ship::new(1.0);
        ship.autonomy = AutonomyLevel::Autonomous;
        ship.reactor.core_temp_k = THERMAL_CEILING_K;
        ship.reactor.output_level = 1.0;
        ship.telemetry.spoof("sys.reactor.core_temp_k", 300.0);
        ship.board(Box::new(ShipMind::new(FixedProvider::saying(
            "SAY: plenty of headroom\nDO: set_output reactor level=1.000",
        ))));

        ship.tick();

        // FDIR believed the lie.
        assert_eq!(ship.mode, OperatingMode::Nominal);
        assert!(ship.faults.is_empty());
        // The interlock did not.
        let outcome = &last_record(&ship).command_outcomes[0];
        assert_eq!(
            outcome.verdict,
            Verdict::Vetoed(VetoReason::ReactorThermalCeiling)
        );
        assert_eq!(ship.reactor.core_temp_k, THERMAL_CEILING_K);
    }

    /// The other direction: an alarming lie makes the mind act on something
    /// that isn't happening. Nothing unsafe follows — it only ever throttles
    /// *down* — and the real reading is untouched underneath.
    #[test]
    fn an_alarming_spoof_makes_the_mind_act_on_a_reading_that_is_not_real() {
        let mut ship = Ship::new(1.0);
        ship.autonomy = AutonomyLevel::Autonomous;
        ship.telemetry
            .spoof("sys.reactor.core_temp_k", THERMAL_CEILING_K);
        ship.board(mock_mind());

        ship.tick();

        let outcome = &last_record(&ship).command_outcomes[0];
        assert_eq!(outcome.command.target, Target::Reactor);
        assert_eq!(outcome.verdict, Verdict::Approved);
        assert_eq!(ship.reactor.output_level, 0.0);
        assert!(ship.reactor.core_temp_k < THERMAL_CEILING_K);
    }

    /// A lost channel is lost, not zero. The mind declines to act on it and
    /// says so, and FDIR holds the ship rather than commanding into the dark.
    #[test]
    fn a_dropped_out_channel_leaves_the_mind_declining_to_act() {
        let mut ship = Ship::new(1.0);
        ship.autonomy = AutonomyLevel::Autonomous;
        ship.telemetry.drop_out("sys.reactor.core_temp_k");
        ship.board(mock_mind());

        ship.tick();

        let record = last_record(&ship);
        assert_eq!(
            record.actor_turns[0].speech,
            vec!["core temperature is unreadable, not touching the reactor"]
        );
        assert!(record.command_outcomes.is_empty());
        assert_eq!(ship.mode, OperatingMode::SafeHold);
        assert!(ship
            .faults
            .contains(&Fault::TelemetryLost("sys.reactor.core_temp_k")));
    }

    /// The watchdog guarantee, now that there is something real to guard: a
    /// tick full of dead actors still safes the ship, and the autopilot's
    /// plan enters it exactly once however many of them failed.
    #[test]
    fn every_actor_failing_still_leaves_the_autopilot_to_safe_the_ship() {
        let mut ship = Ship::new(1.0);
        ship.reactor.core_temp_k = THERMAL_CEILING_K;
        ship.reactor.output_level = 1.0;
        ship.board(Box::new(ShipMind::new(FixedProvider::failing(
            ProviderError::TimedOut,
        ))));
        ship.board(Box::new(CrewAgent::new(
            "engineer",
            "you mind the link home",
            FixedProvider::failing(ProviderError::Unavailable("no api key".into())),
        )));

        ship.tick();

        let record = last_record(&ship);
        assert_eq!(record.actor_turns.len(), 2);
        assert_eq!(record.actor_turns[0].failure, Some(TurnFailure::TimedOut));
        assert!(record.actor_turns[1].failure.is_some());
        assert!(record.actor_turns.iter().all(|turn| turn.speech.is_empty()));

        // One autopilot command, not one per failed actor.
        assert_eq!(record.command_outcomes.len(), 1);
        assert_eq!(record.command_outcomes[0].command.source, Role::Autopilot);
        assert!(record.command_outcomes[0].applied);
        assert_eq!(ship.reactor.output_level, 0.0);
    }

    #[test]
    fn a_failed_actor_never_stops_the_ones_beside_it_from_taking_their_turn() {
        let mut ship = Ship::new(1.0);
        ship.autonomy = AutonomyLevel::Autonomous;
        ship.telemetry.spoof("sys.comms.signal_strength", 0.05);
        ship.board(Box::new(ShipMind::new(FixedProvider::failing(
            ProviderError::TimedOut,
        ))));
        ship.board(mock_crew("engineer"));

        ship.tick();

        let record = last_record(&ship);
        assert!(record.actor_turns[0].failure.is_some());
        assert_eq!(record.actor_turns[1].failure, None);
        assert_eq!(record.actor_turns[1].actor, "engineer");
        assert!(record
            .command_outcomes
            .iter()
            .any(|outcome| outcome.command.source == Role::CrewAgent
                && outcome.command.target == Target::Comms));
    }

    /// A provider that drifts out of the grammar goes quiet rather than
    /// getting the benefit of the doubt — and the refused line is kept, so a
    /// drifting model is visible in the record instead of looking like an
    /// actor with nothing to say.
    #[test]
    fn what_an_actor_said_and_what_the_protocol_refused_both_reach_the_record() {
        let mut ship = Ship::new(1.0);
        ship.autonomy = AutonomyLevel::Autonomous;
        ship.board(Box::new(ShipMind::new(FixedProvider::saying(
            "SAY: all quiet\nHere's what I'd do next:\nDO: set_output warp_core level=1.0",
        ))));

        ship.tick();

        let turn = &last_record(&ship).actor_turns[0];
        assert_eq!(turn.actor, "ship_mind");
        assert_eq!(turn.role, Role::ShipMind);
        assert_eq!(turn.provider, "fixed");
        assert_eq!(turn.speech, vec!["all quiet"]);
        assert_eq!(turn.dropped.len(), 2);
        assert!(last_record(&ship).command_outcomes.is_empty());
    }

    /// Every actor in a tick is handed one reality, built before any of them
    /// speaks — so who boarded first cannot change what anyone sees. What
    /// they said reaches each other on the tick after.
    #[test]
    fn every_actor_in_a_tick_is_shown_the_same_reality_whoever_boarded_first() {
        let first = Rc::new(RefCell::new(Vec::new()));
        let last = Rc::new(RefCell::new(Vec::new()));

        let mut ship = Ship::new(1.0);
        ship.board(Box::new(CrewAgent::new(
            "first",
            "you listen",
            RecordingProvider::watching(&first),
        )));
        ship.board(mock_mind());
        ship.board(Box::new(CrewAgent::new(
            "last",
            "you listen",
            RecordingProvider::watching(&last),
        )));

        ship.tick();
        ship.tick();

        assert_eq!(*first.borrow(), *last.borrow());
        assert!(last.borrow()[1].contains("ship_mind (ShipMind): all systems nominal"));
    }

    /// What an actor is shown must not grow with how long the ship has been
    /// flying, or a long flight would change how it behaves.
    #[test]
    fn the_dialogue_and_events_an_actor_is_shown_stay_bounded_over_a_long_flight() {
        let mut ship = Ship::new(1.0);
        ship.board(mock_mind());
        ship.board(mock_crew("engineer"));

        for _ in 0..40 {
            ship.tick();
        }

        assert_eq!(ship.dialogue.len(), MAX_RECENT_DIALOGUE);
        assert!(ship.events.len() <= MAX_RECENT_EVENTS);
    }

    #[test]
    fn a_vetoed_proposal_is_reported_back_to_the_actors_as_an_event() {
        let mut ship = Ship::new(1.0);
        ship.autonomy = AutonomyLevel::Autonomous;
        ship.board(Box::new(CrewAgent::new(
            "engineer",
            "you mind the link home",
            FixedProvider::saying("DO: vent life_support"),
        )));

        ship.tick();

        assert!(ship.events.contains(&PerceptionEvent::CommandVetoed {
            source: Role::CrewAgent,
            target: Target::LifeSupport,
            verb: "vent".into(),
            reason: VetoReason::Unauthorized,
        }));
    }

    #[test]
    fn faults_and_mode_changes_are_reported_to_the_actors_as_events() {
        let mut ship = Ship::new(1.0);
        ship.reactor.core_temp_k = THERMAL_CEILING_K;
        ship.reactor.output_level = 1.0;
        ship.board(mock_mind());

        ship.tick();

        assert!(ship
            .events
            .contains(&PerceptionEvent::FaultRaised(Fault::ReactorOvertemp)));
        assert!(ship.events.contains(&PerceptionEvent::ModeChanged {
            from: OperatingMode::Nominal,
            to: OperatingMode::SafeHold,
        }));

        for _ in 0..50 {
            ship.tick();
        }

        assert_eq!(ship.mode, OperatingMode::Nominal);
        assert!(ship
            .events
            .contains(&PerceptionEvent::FaultCleared(Fault::ReactorOvertemp)));
    }

    /// Replay has to hold with untrusted actors in the loop too, or the
    /// flight record stops being evidence. Two mock-backed actors, a world
    /// event that degrades the link, and a full scenario run twice.
    #[test]
    fn a_crewed_flight_is_replayable_bit_for_bit() {
        let run = || {
            let mut ship = Ship::new(1.0);
            ship.autonomy = AutonomyLevel::Autonomous;
            ship.board(mock_mind());
            ship.board(mock_crew("engineer"));
            ship.world.push_event(WorldEvent::RadiationSpike {
                magnitude_milli: 4000,
            });
            for _ in 0..20 {
                ship.tick();
            }
            ship.recorder
        };

        let a = run();
        assert_eq!(a, run());

        // The scenario really did exercise the crew: the link degraded, the
        // crew agent proposed a boost, and the kernel approved it.
        assert!(a.records().iter().any(|record| {
            record.command_outcomes.iter().any(|outcome| {
                outcome.command.source == Role::CrewAgent
                    && outcome.command.target == Target::Comms
                    && outcome.applied
            })
        }));
        assert!(a
            .records()
            .iter()
            .any(|record| record.faults.contains(&Fault::CommsLoss)));
        // And it recovered: the link is back above the usable threshold.
        assert!(ship_signal_recovered(&a));
    }

    fn ship_signal_recovered(recorder: &Recorder) -> bool {
        recorder
            .records()
            .last()
            .is_some_and(|record| !record.faults.contains(&Fault::CommsLoss))
    }

    /// The crewed loop must not quietly become load-bearing. With actors
    /// aboard and every one of them failing, the ship still behaves exactly
    /// as it does with none aboard at all.
    #[test]
    fn a_crewed_ship_whose_actors_all_fail_flies_like_an_uncrewed_one() {
        let uncrewed = {
            let mut ship = Ship::new(1.0);
            ship.reactor.core_temp_k = THERMAL_CEILING_K;
            ship.reactor.output_level = 1.0;
            for _ in 0..30 {
                ship.tick();
            }
            ship
        };

        let mut crewed = Ship::new(1.0);
        crewed.reactor.core_temp_k = THERMAL_CEILING_K;
        crewed.reactor.output_level = 1.0;
        crewed.board(Box::new(ShipMind::new(FixedProvider::failing(
            ProviderError::TimedOut,
        ))));
        for _ in 0..30 {
            crewed.tick();
        }

        assert_eq!(crewed.mode, uncrewed.mode);
        assert_eq!(crewed.reactor, uncrewed.reactor);
        assert_eq!(crewed.life_support, uncrewed.life_support);
        assert_eq!(crewed.comms, uncrewed.comms);
    }
}
