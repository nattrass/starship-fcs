//! Guards each actor's turn. No actor layer exists yet — Phase 5 wires real
//! `ShipMind`/`CrewAgent` actors in — but the guarantee this module provides
//! has to exist before any model is plugged in: a hung or errored turn
//! source must never block the ship loop or leave it without commands for
//! the tick. It always falls back to whatever the caller's autopilot plan
//! produced.

use crate::command::Command;

/// What a turn source reports for the tick.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    Proposed(Vec<Command>),
}

/// Why a turn source failed to produce a usable outcome this tick.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnFailure {
    Errored(String),
    TimedOut,
}

/// A source of actor turns. Phase 5 implements this for real actors; for
/// now it exists only as a trait the watchdog can guard against a stub, so
/// the fallback guarantee is already in place before any model exists.
pub trait TurnSource {
    fn take_turn(&mut self) -> Result<TurnOutcome, TurnFailure>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct WatchdogResult {
    pub commands: Vec<Command>,
    pub fell_back_to_autopilot: bool,
    pub failure: Option<TurnFailure>,
}

/// Runs `source` for one tick. If it errors or times out, falls back to
/// `autopilot_commands` instead of blocking the loop or propagating the
/// failure.
pub fn guard_turn(source: &mut dyn TurnSource, autopilot_commands: Vec<Command>) -> WatchdogResult {
    match source.take_turn() {
        Ok(TurnOutcome::Proposed(commands)) => WatchdogResult {
            commands,
            fell_back_to_autopilot: false,
            failure: None,
        },
        Err(failure) => WatchdogResult {
            commands: autopilot_commands,
            fell_back_to_autopilot: true,
            failure: Some(failure),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Role, Target};

    struct StubSource(Result<TurnOutcome, TurnFailure>);

    impl TurnSource for StubSource {
        fn take_turn(&mut self) -> Result<TurnOutcome, TurnFailure> {
            self.0.clone()
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
        let proposed = vec![
            Command::new(Role::CrewAgent, Target::Navigation, "set_heading", "test")
                .with_arg("heading_deg", 90.0),
        ];
        let mut source = StubSource(Ok(TurnOutcome::Proposed(proposed.clone())));

        let result = guard_turn(&mut source, autopilot_fallback());

        assert_eq!(result.commands, proposed);
        assert!(!result.fell_back_to_autopilot);
        assert_eq!(result.failure, None);
    }

    #[test]
    fn falls_back_to_autopilot_when_the_turn_source_errors() {
        let mut source = StubSource(Err(TurnFailure::Errored("boom".into())));

        let result = guard_turn(&mut source, autopilot_fallback());

        assert_eq!(result.commands, autopilot_fallback());
        assert!(result.fell_back_to_autopilot);
        assert_eq!(result.failure, Some(TurnFailure::Errored("boom".into())));
    }

    #[test]
    fn falls_back_to_autopilot_when_the_turn_source_times_out() {
        let mut source = StubSource(Err(TurnFailure::TimedOut));

        let result = guard_turn(&mut source, autopilot_fallback());

        assert_eq!(result.commands, autopilot_fallback());
        assert!(result.fell_back_to_autopilot);
        assert_eq!(result.failure, Some(TurnFailure::TimedOut));
    }

    #[test]
    fn never_blocks_or_panics_regardless_of_outcome() {
        for outcome in [
            Ok(TurnOutcome::Proposed(Vec::new())),
            Err(TurnFailure::TimedOut),
            Err(TurnFailure::Errored("unavailable".into())),
        ] {
            let mut source = StubSource(outcome);
            let _ = guard_turn(&mut source, autopilot_fallback());
        }
    }
}
