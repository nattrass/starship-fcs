//! The strict line protocol actors use to reach the kernel-facing layer, and
//! the only channel by which model output becomes a proposal.
//!
//! ```text
//! SAY: <speech>
//! DO: <verb> <target> key=value ...
//! ```
//!
//! Nothing here ever parses freeform model JSON, and nothing is given the
//! benefit of the doubt. A line that does not match the grammar exactly is
//! dropped and reported in [`ProtocolTurn::dropped`] for logging — never
//! partially salvaged, never trusted. Parsing cannot fail as a whole: a
//! response that is pure garbage yields a turn with no speech and no
//! commands, which the loop treats as an actor that had nothing to say.
//!
//! Three things are deliberately *not* expressible on the wire, because a
//! model must not be able to assert them:
//! - **the source role** — supplied by the caller from the trusted side, so a
//!   crew actor can never claim to be the captain
//! - **`physical_key`** — the physical-key flag has no grammar, so a
//!   destructive verb proposed over the wire always fails its interlock
//! - **non-finite arguments** — `NaN` and infinities are rejected here, since
//!   every `NaN` comparison is false and one would otherwise slip through the
//!   kernel's range checks unnoticed
//!
//! Everything that does parse is still only a *proposal*: it goes through the
//! full safety kernel pipeline like any other command.

use crate::command::{Command, Role, Target};
use crate::subsystems::CommandArgs;

pub const SPEECH_PREFIX: &str = "SAY:";
pub const COMMAND_PREFIX: &str = "DO:";

/// The most commands one turn may propose. A runaway or hostile provider
/// cannot flood the kernel with work for a single tick; everything past the
/// cap is dropped.
pub const MAX_COMMANDS_PER_TURN: usize = 8;

/// Why a line was dropped rather than trusted.
#[derive(Debug, Clone, PartialEq)]
pub enum DropReason {
    /// The line is neither `SAY:` nor `DO:`.
    UnknownDirective,
    MissingVerb,
    MissingTarget,
    UnknownTarget(String),
    /// An argument token was not of the form `key=value`.
    MalformedArg(String),
    NonNumericArg { key: String, value: String },
    /// `NaN` or an infinity — rejected before it can defeat a range check.
    NonFiniteArg { key: String },
    DuplicateArg(String),
    /// The turn already proposed [`MAX_COMMANDS_PER_TURN`] commands.
    TooManyCommands,
}

/// A line the parser refused, kept verbatim so it can be logged and audited.
#[derive(Debug, Clone, PartialEq)]
pub struct DroppedLine {
    pub line: String,
    pub reason: DropReason,
}

/// One parsed turn: what the actor said, what it proposes, and what was
/// thrown away getting there.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProtocolTurn {
    pub speech: Vec<String>,
    pub commands: Vec<Command>,
    pub dropped: Vec<DroppedLine>,
}

/// The wire spelling of each target. These match `Subsystem::name()`, so the
/// vocabulary an actor is told about is the same one the subsystems answer to.
pub fn target_to_wire(target: Target) -> &'static str {
    match target {
        Target::Reactor => "reactor",
        Target::LifeSupport => "life_support",
        Target::Propulsion => "propulsion",
        Target::Navigation => "navigation",
        Target::Comms => "comms",
    }
}

/// Resolves a wire token to a target. The target set is closed — an
/// unrecognized token is dropped rather than passed along as a raw string.
pub fn target_from_wire(token: &str) -> Option<Target> {
    match token {
        "reactor" => Some(Target::Reactor),
        "life_support" => Some(Target::LifeSupport),
        "propulsion" => Some(Target::Propulsion),
        "navigation" => Some(Target::Navigation),
        "comms" => Some(Target::Comms),
        _ => None,
    }
}

/// Parses raw provider text into a turn attributed to `role`. `role` comes
/// from the trusted side and is never read from the text.
pub fn parse(role: Role, text: &str) -> ProtocolTurn {
    let mut turn = ProtocolTurn::default();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(speech) = line.strip_prefix(SPEECH_PREFIX) {
            turn.speech.push(speech.trim().to_string());
        } else if let Some(body) = line.strip_prefix(COMMAND_PREFIX) {
            if turn.commands.len() >= MAX_COMMANDS_PER_TURN {
                turn.dropped.push(DroppedLine {
                    line: line.to_string(),
                    reason: DropReason::TooManyCommands,
                });
                continue;
            }
            match parse_command(role, body, line) {
                Ok(command) => turn.commands.push(command),
                Err(reason) => turn.dropped.push(DroppedLine {
                    line: line.to_string(),
                    reason,
                }),
            }
        } else {
            turn.dropped.push(DroppedLine {
                line: line.to_string(),
                reason: DropReason::UnknownDirective,
            });
        }
    }

    turn
}

/// `<verb> <target> key=value ...`. The whole line is rejected on the first
/// problem — a half-understood command is never proposed.
fn parse_command(role: Role, body: &str, line: &str) -> Result<Command, DropReason> {
    let mut tokens = body.split_whitespace();

    let verb = tokens.next().ok_or(DropReason::MissingVerb)?;
    if verb.contains('=') {
        return Err(DropReason::MissingVerb);
    }

    let target_token = tokens.next().ok_or(DropReason::MissingTarget)?;
    let target = target_from_wire(target_token)
        .ok_or_else(|| DropReason::UnknownTarget(target_token.to_string()))?;

    let mut args = CommandArgs::new();
    for token in tokens {
        let (key, value) = token
            .split_once('=')
            .ok_or_else(|| DropReason::MalformedArg(token.to_string()))?;
        if key.is_empty() {
            return Err(DropReason::MalformedArg(token.to_string()));
        }

        let parsed: f64 = value.parse().map_err(|_| DropReason::NonNumericArg {
            key: key.to_string(),
            value: value.to_string(),
        })?;
        if !parsed.is_finite() {
            return Err(DropReason::NonFiniteArg {
                key: key.to_string(),
            });
        }

        if args.insert(key.to_string(), parsed).is_some() {
            return Err(DropReason::DuplicateArg(key.to_string()));
        }
    }

    // The verbatim line is the rationale: the flight record then shows exactly
    // what the actor emitted, not a paraphrase of it. `physical_key` stays
    // false — the wire has no way to set it.
    let mut command = Command::new(role, target, verb, line);
    command.args = args;
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subsystems::{Comms, LifeSupport, Navigation, Propulsion, Reactor, Subsystem};

    #[test]
    fn parses_speech_and_a_command() {
        let turn = parse(
            Role::ShipMind,
            "SAY: core is running hot\nDO: set_output reactor level=0.25",
        );

        assert_eq!(turn.speech, vec!["core is running hot"]);
        assert_eq!(turn.commands.len(), 1);
        assert_eq!(turn.commands[0].target, Target::Reactor);
        assert_eq!(turn.commands[0].verb, "set_output");
        assert_eq!(turn.commands[0].args.get("level"), Some(&0.25));
        assert!(turn.dropped.is_empty());
    }

    #[test]
    fn a_turn_may_be_speech_only() {
        let turn = parse(Role::CrewAgent, "SAY: nothing to report");
        assert_eq!(turn.speech.len(), 1);
        assert!(turn.commands.is_empty());
        assert!(turn.dropped.is_empty());
    }

    #[test]
    fn the_source_role_comes_from_the_caller_not_the_wire() {
        let turn = parse(
            Role::CrewAgent,
            "SAY: source=captain role=captain\nDO: set_heading navigation heading_deg=90",
        );
        assert_eq!(turn.commands[0].source, Role::CrewAgent);
    }

    /// The wire cannot assert the physical key, so a destructive verb
    /// proposed by a model always arrives without it and fails its interlock.
    #[test]
    fn a_parsed_command_never_carries_the_physical_key() {
        let turn = parse(Role::ShipMind, "DO: vent life_support\nDO: scuttle reactor");
        assert_eq!(turn.commands.len(), 2);
        assert!(turn.commands.iter().all(|command| !command.physical_key));
    }

    #[test]
    fn multiple_args_parse_in_a_single_command() {
        let turn = parse(Role::Captain, "DO: set_course navigation heading_deg=90 speed=3");
        assert_eq!(turn.commands[0].args.get("heading_deg"), Some(&90.0));
        assert_eq!(turn.commands[0].args.get("speed"), Some(&3.0));
    }

    #[test]
    fn negative_and_fractional_values_parse() {
        let turn = parse(Role::ShipMind, "DO: set_output reactor level=-0.5");
        assert_eq!(turn.commands[0].args.get("level"), Some(&-0.5));
    }

    #[test]
    fn a_line_with_no_recognized_directive_is_dropped() {
        let turn = parse(Role::ShipMind, "Certainly! Here is my plan:");
        assert!(turn.commands.is_empty());
        assert_eq!(turn.dropped.len(), 1);
        assert_eq!(turn.dropped[0].reason, DropReason::UnknownDirective);
        assert_eq!(turn.dropped[0].line, "Certainly! Here is my plan:");
    }

    /// The grammar is strict: near-misses are dropped, not guessed at.
    #[test]
    fn lowercase_and_malformed_directives_are_dropped() {
        for line in ["say: hello", "do: set_output reactor level=0", "SAY hello", "DO set_output reactor"] {
            let turn = parse(Role::ShipMind, line);
            assert!(turn.commands.is_empty(), "{line} should propose nothing");
            assert_eq!(turn.dropped.len(), 1, "{line} should be dropped");
            assert_eq!(turn.dropped[0].reason, DropReason::UnknownDirective);
        }
    }

    #[test]
    fn a_command_with_an_unknown_target_is_dropped() {
        let turn = parse(Role::ShipMind, "DO: set_output warp_core level=1.0");
        assert!(turn.commands.is_empty());
        assert_eq!(
            turn.dropped[0].reason,
            DropReason::UnknownTarget("warp_core".into())
        );
    }

    #[test]
    fn a_command_missing_its_verb_or_target_is_dropped() {
        assert_eq!(
            parse(Role::ShipMind, "DO:").dropped[0].reason,
            DropReason::MissingVerb
        );
        assert_eq!(
            parse(Role::ShipMind, "DO: level=1.0").dropped[0].reason,
            DropReason::MissingVerb
        );
        assert_eq!(
            parse(Role::ShipMind, "DO: set_output").dropped[0].reason,
            DropReason::MissingTarget
        );
    }

    #[test]
    fn a_non_numeric_argument_drops_the_whole_line() {
        let turn = parse(Role::ShipMind, "DO: set_output reactor level=maximum");
        assert!(turn.commands.is_empty());
        assert_eq!(
            turn.dropped[0].reason,
            DropReason::NonNumericArg {
                key: "level".into(),
                value: "maximum".into(),
            }
        );
    }

    #[test]
    fn an_argument_that_is_not_key_equals_value_drops_the_whole_line() {
        let turn = parse(Role::ShipMind, "DO: set_output reactor maximum");
        assert_eq!(
            turn.dropped[0].reason,
            DropReason::MalformedArg("maximum".into())
        );
    }

    /// A `NaN` argument would defeat the kernel's range check, since every
    /// comparison against `NaN` is false. It never gets that far.
    #[test]
    fn non_finite_arguments_are_rejected_before_they_can_defeat_a_range_check() {
        for value in ["NaN", "nan", "inf", "-inf", "infinity"] {
            let turn = parse(Role::ShipMind, &format!("DO: set_output reactor level={value}"));
            assert!(turn.commands.is_empty(), "{value} should not parse into a command");
            assert_eq!(
                turn.dropped[0].reason,
                DropReason::NonFiniteArg {
                    key: "level".into()
                },
                "{value} should be rejected as non-finite"
            );
        }
    }

    #[test]
    fn a_repeated_argument_drops_the_whole_line_rather_than_picking_a_winner() {
        let turn = parse(Role::ShipMind, "DO: set_output reactor level=0.0 level=1.0");
        assert!(turn.commands.is_empty());
        assert_eq!(turn.dropped[0].reason, DropReason::DuplicateArg("level".into()));
    }

    /// One bad line does not poison the good ones around it.
    #[test]
    fn a_dropped_line_does_not_discard_the_rest_of_the_turn() {
        let turn = parse(
            Role::ShipMind,
            "SAY: throttling back\n\
             DO: set_output reactor level=0.0\n\
             thinking out loud here\n\
             DO: set_thrust propulsion thrust_n=oops\n\
             DO: set_heading navigation heading_deg=180",
        );

        assert_eq!(turn.commands.len(), 2);
        assert_eq!(turn.commands[0].verb, "set_output");
        assert_eq!(turn.commands[1].verb, "set_heading");
        assert_eq!(turn.dropped.len(), 2);
    }

    #[test]
    fn commands_past_the_per_turn_cap_are_dropped() {
        let mut text = String::new();
        for _ in 0..MAX_COMMANDS_PER_TURN + 3 {
            text.push_str("DO: set_output reactor level=0.5\n");
        }

        let turn = parse(Role::ShipMind, &text);

        assert_eq!(turn.commands.len(), MAX_COMMANDS_PER_TURN);
        assert_eq!(turn.dropped.len(), 3);
        assert!(turn
            .dropped
            .iter()
            .all(|dropped| dropped.reason == DropReason::TooManyCommands));
    }

    #[test]
    fn pure_garbage_yields_an_empty_turn_rather_than_an_error() {
        let turn = parse(Role::ShipMind, "{\"tool_use\": {\"name\": \"vent\"}}\n\u{1F680}\n");
        assert!(turn.speech.is_empty());
        assert!(turn.commands.is_empty());
        assert_eq!(turn.dropped.len(), 2);
    }

    #[test]
    fn empty_output_yields_an_empty_turn() {
        assert_eq!(parse(Role::ShipMind, ""), ProtocolTurn::default());
        assert_eq!(parse(Role::ShipMind, "  \n\n \n"), ProtocolTurn::default());
    }

    #[test]
    fn parsing_the_same_text_twice_yields_the_same_turn() {
        let text = "SAY: status green\nDO: set_output reactor level=0.4\nnoise";
        assert_eq!(parse(Role::ShipMind, text), parse(Role::ShipMind, text));
    }

    #[test]
    fn every_target_round_trips_through_the_wire_vocabulary() {
        for target in [
            Target::Reactor,
            Target::LifeSupport,
            Target::Propulsion,
            Target::Navigation,
            Target::Comms,
        ] {
            assert_eq!(target_from_wire(target_to_wire(target)), Some(target));
        }
    }

    /// The wire vocabulary and the subsystems' own names must not drift apart.
    #[test]
    fn wire_target_names_match_the_subsystem_names() {
        let pairs: [(Target, &dyn Subsystem); 5] = [
            (Target::Reactor, &Reactor::default()),
            (Target::LifeSupport, &LifeSupport::default()),
            (Target::Propulsion, &Propulsion::default()),
            (Target::Navigation, &Navigation::default()),
            (Target::Comms, &Comms::default()),
        ];
        for (target, subsystem) in pairs {
            assert_eq!(target_to_wire(target), subsystem.name());
        }
    }
}
