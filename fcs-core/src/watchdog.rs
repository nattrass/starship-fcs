//! Guards each actor's turn. An actor is the one part of the ship that can
//! hang, error, or simply never answer, so nothing above this module is ever
//! allowed to depend on one succeeding: a failed turn costs the ship that
//! actor's proposals for the tick and nothing else. The loop is not blocked,
//! the failure is not propagated, and the autopilot's plan is handed back in
//! the actor's place so the tick still has a way to safe the ship.
//!
//! The failure is kept, not swallowed — [`TurnFailure`] reaches the flight
//! record, so an actor that has gone quiet is visible as a fault rather than
//! as an actor with nothing to say.

use crate::actors::{Actor, ActorTurn, Perception};
use crate::command::Command;

/// Why a turn produced nothing usable this tick. Providers convert their own
/// errors into these (see
/// [`ProviderError`](crate::provider::ProviderError)), so the loop never has
/// to know what kind of thing failed behind an actor.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnFailure {
    Errored(String),
    TimedOut,
}

/// One guarded turn.
#[derive(Debug, Clone, PartialEq)]
pub struct WatchdogResult {
    /// The actor's turn, or `None` if it failed.
    pub turn: Option<ActorTurn>,
    /// The autopilot's plan, handed over in the actor's place. Empty unless
    /// the turn failed.
    pub fallback: Vec<Command>,
    /// Why the turn failed, if it did.
    pub failure: Option<TurnFailure>,
}

impl WatchdogResult {
    /// What this actor contributes to the tick: its own proposals, or the
    /// autopilot's plan if it failed. Derived rather than stored, so the
    /// commands and the reason for them can never drift apart.
    pub fn commands(&self) -> &[Command] {
        match &self.turn {
            Some(turn) => &turn.commands,
            None => &self.fallback,
        }
    }

    pub fn fell_back_to_autopilot(&self) -> bool {
        self.turn.is_none()
    }
}

/// Runs `actor` for one tick. If it errors or times out, hands back
/// `autopilot_commands` instead of blocking the loop or propagating the
/// failure.
pub fn guard_turn(
    actor: &mut dyn Actor,
    perception: &Perception,
    autopilot_commands: Vec<Command>,
) -> WatchdogResult {
    match actor.take_turn(perception) {
        Ok(turn) => WatchdogResult {
            turn: Some(turn),
            fallback: Vec::new(),
            failure: None,
        },
        Err(failure) => WatchdogResult {
            turn: None,
            fallback: autopilot_commands,
            failure: Some(failure),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::{DialogueLine, PerceptionEvent};
    use crate::command::{Role, Target};
    use crate::fdir::{Fault, OperatingMode};
    use crate::safety::AutonomyLevel;
    use crate::telemetry::{TelemetrySampler, TelemetrySnapshot};
    use std::collections::BTreeSet;

    /// An actor that answers with a fixed outcome, standing in for anything a
    /// provider-backed one might do.
    struct StubActor(Result<ActorTurn, TurnFailure>);

    impl Actor for StubActor {
        fn name(&self) -> &str {
            "stub"
        }

        fn role(&self) -> Role {
            Role::CrewAgent
        }

        fn provider_name(&self) -> &str {
            "stub"
        }

        fn take_turn(&mut self, _perception: &Perception) -> Result<ActorTurn, TurnFailure> {
            self.0.clone()
        }
    }

    struct Reality {
        snapshot: TelemetrySnapshot,
        faults: BTreeSet<Fault>,
        events: Vec<PerceptionEvent>,
        dialogue: Vec<DialogueLine>,
    }

    impl Reality {
        fn new() -> Self {
            Self {
                snapshot: TelemetrySampler::new().sample(1, 1.0, Vec::new()),
                faults: BTreeSet::new(),
                events: Vec::new(),
                dialogue: Vec::new(),
            }
        }

        fn perception(&self) -> Perception<'_> {
            Perception {
                snapshot: &self.snapshot,
                mode: OperatingMode::Nominal,
                faults: &self.faults,
                autonomy: AutonomyLevel::Assist,
                events: &self.events,
                dialogue: &self.dialogue,
            }
        }
    }

    fn proposal() -> Vec<Command> {
        vec![
            Command::new(Role::CrewAgent, Target::Navigation, "set_heading", "test")
                .with_arg("heading_deg", 90.0),
        ]
    }

    fn actor_turn() -> ActorTurn {
        ActorTurn {
            actor: "stub".into(),
            role: Role::CrewAgent,
            provider: "stub".into(),
            speech: vec!["coming about".into()],
            commands: proposal(),
            dropped: Vec::new(),
        }
    }

    fn autopilot_fallback() -> Vec<Command> {
        vec![
            Command::new(Role::Autopilot, Target::Reactor, "set_output", "fallback")
                .with_arg("level", 0.0),
        ]
    }

    #[test]
    fn passes_through_a_successful_turn_without_falling_back() {
        let reality = Reality::new();
        let mut actor = StubActor(Ok(actor_turn()));

        let result = guard_turn(&mut actor, &reality.perception(), autopilot_fallback());

        assert_eq!(result.commands(), proposal());
        assert_eq!(result.turn, Some(actor_turn()));
        assert!(!result.fell_back_to_autopilot());
        assert_eq!(result.failure, None);
    }

    #[test]
    fn falls_back_to_autopilot_when_the_actor_errors() {
        let reality = Reality::new();
        let mut actor = StubActor(Err(TurnFailure::Errored("boom".into())));

        let result = guard_turn(&mut actor, &reality.perception(), autopilot_fallback());

        assert_eq!(result.commands(), autopilot_fallback());
        assert!(result.fell_back_to_autopilot());
        assert_eq!(result.failure, Some(TurnFailure::Errored("boom".into())));
    }

    #[test]
    fn falls_back_to_autopilot_when_the_actor_times_out() {
        let reality = Reality::new();
        let mut actor = StubActor(Err(TurnFailure::TimedOut));

        let result = guard_turn(&mut actor, &reality.perception(), autopilot_fallback());

        assert_eq!(result.commands(), autopilot_fallback());
        assert!(result.fell_back_to_autopilot());
        assert_eq!(result.failure, Some(TurnFailure::TimedOut));
    }

    /// A failed actor loses its voice as well as its proposals — the loop
    /// must not invent speech for it, and the record must not imply it spoke.
    #[test]
    fn a_failed_turn_yields_no_turn_to_record() {
        let reality = Reality::new();
        let mut actor = StubActor(Err(TurnFailure::TimedOut));

        let result = guard_turn(&mut actor, &reality.perception(), autopilot_fallback());

        assert_eq!(result.turn, None);
    }

    #[test]
    fn never_blocks_or_panics_regardless_of_outcome() {
        let reality = Reality::new();
        for outcome in [
            Ok(actor_turn()),
            Err(TurnFailure::TimedOut),
            Err(TurnFailure::Errored("unavailable".into())),
        ] {
            let mut actor = StubActor(outcome);
            let _ = guard_turn(&mut actor, &reality.perception(), autopilot_fallback());
        }
    }
}
