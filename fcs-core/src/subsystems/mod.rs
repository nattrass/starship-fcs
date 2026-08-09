//! Subsystem primitives. Each subsystem owns its state, advances on `tick`,
//! publishes telemetry, and declares its accepted commands separately from
//! applying them — `apply()` is meant to be called only after a caller (the
//! Phase 2 safety kernel) has validated a command against `commands()`.

pub mod comms;
pub mod life_support;
pub mod navigation;
pub mod propulsion;
pub mod reactor;

pub use comms::Comms;
pub use life_support::LifeSupport;
pub use navigation::Navigation;
pub use propulsion::Propulsion;
pub use reactor::Reactor;

use std::collections::BTreeMap;

use crate::telemetry::RawSample;
use crate::world::Environment;

/// An inclusive numeric range a command argument must fall within.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArgRange {
    pub min: f64,
    pub max: f64,
}

/// The accepted arguments for one verb: arg name -> optional numeric range.
/// `None` means the argument is accepted but unconstrained.
pub type ArgSpec = BTreeMap<String, Option<ArgRange>>;

/// The full command surface a subsystem declares: verb -> accepted args.
pub type CommandSpec = BTreeMap<String, ArgSpec>;

/// A validated, typed argument set ready to apply.
pub type CommandArgs = BTreeMap<String, f64>;

#[derive(Debug, Clone, PartialEq)]
pub enum ApplyError {
    UnknownVerb(String),
    MissingArg(String),
}

pub trait Subsystem {
    fn name(&self) -> &'static str;

    fn tick(&mut self, dt: f64, env: &Environment);

    fn sample(&self) -> Vec<RawSample>;

    fn commands(&self) -> CommandSpec;

    fn apply(&mut self, verb: &str, args: &CommandArgs) -> Result<(), ApplyError>;
}
