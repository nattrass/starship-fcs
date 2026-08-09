//! The typed command model used on the control path. Nothing that reaches the
//! safety kernel is a raw, unparsed string: `target` and `source` are closed
//! enums, `verb` is checked against a subsystem's declared schema before
//! anything else runs, and `args` are typed numbers rather than free text.

use crate::subsystems::CommandArgs;

/// Which subsystem a command is aimed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Reactor,
    LifeSupport,
    Propulsion,
    Navigation,
    Comms,
}

/// Who is asking for a command to run. Used for both authorization and the
/// autonomy gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Autopilot,
    CrewAgent,
    ShipMind,
    Captain,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub source: Role,
    pub verb: String,
    pub target: Target,
    pub args: CommandArgs,
    pub rationale: String,
    /// Set only when a physical key/switch backs this command. Required for
    /// destructive verbs (vent/jettison/scuttle) regardless of role or
    /// autonomy level.
    pub physical_key: bool,
}

impl Command {
    pub fn new(
        source: Role,
        target: Target,
        verb: impl Into<String>,
        rationale: impl Into<String>,
    ) -> Self {
        Self {
            source,
            verb: verb.into(),
            target,
            args: CommandArgs::new(),
            rationale: rationale.into(),
            physical_key: false,
        }
    }

    pub fn with_arg(mut self, key: impl Into<String>, value: f64) -> Self {
        self.args.insert(key.into(), value);
        self
    }

    pub fn with_physical_key(mut self) -> Self {
        self.physical_key = true;
        self
    }
}
