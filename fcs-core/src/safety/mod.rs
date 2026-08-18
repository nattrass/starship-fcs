//! The safety kernel: the sole authority for every command. Every command
//! passes through a fixed, ordered pipeline —
//! schema validation -> authorization -> interlocks -> autonomy gate —
//! and interlocks are enforced identically regardless of authorization
//! outcome or autonomy level.

pub mod interlocks;

use std::collections::{BTreeMap, BTreeSet};

use crate::command::{Command, Role, Target};
use crate::subsystems::{Comms, LifeSupport, Navigation, Propulsion, Reactor, Subsystem};

/// How much latitude proposed commands get before a human must confirm them.
/// Interlocks are never affected by this — they run the same way at every level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutonomyLevel {
    /// Nothing auto-executes; every schema-valid, authorized, interlock-clear
    /// command still needs confirmation.
    Observe,
    /// Only the autopilot's own commands auto-execute.
    Assist,
    /// The autopilot and the captain's commands auto-execute.
    Supervised,
    /// Any schema-valid, authorized, interlock-clear command auto-executes.
    Autonomous,
}

impl AutonomyLevel {
    fn auto_approves(self, role: Role) -> bool {
        match self {
            AutonomyLevel::Observe => false,
            AutonomyLevel::Assist => role == Role::Autopilot,
            AutonomyLevel::Supervised => matches!(role, Role::Autopilot | Role::Captain),
            AutonomyLevel::Autonomous => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum VetoReason {
    UnknownVerb,
    UnknownArg(String),
    MissingArg(String),
    ArgOutOfRange {
        arg: String,
        value: f64,
        min: f64,
        max: f64,
    },
    Unauthorized,
    LifeSupportUnsafeWithCrewAboard,
    ReactorThermalCeiling,
    PropulsionStructuralLimit,
    RequiresPhysicalKey,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Approved,
    NeedsConfirmation,
    Vetoed(VetoReason),
}

/// A read-only view of current subsystem state — wide enough for schema
/// validation and interlocks, without the kernel depending on `Ship`.
pub struct ShipView<'a> {
    pub reactor: &'a Reactor,
    pub life_support: &'a LifeSupport,
    pub propulsion: &'a Propulsion,
    pub navigation: &'a Navigation,
    pub comms: &'a Comms,
}

impl<'a> ShipView<'a> {
    pub fn new(
        reactor: &'a Reactor,
        life_support: &'a LifeSupport,
        propulsion: &'a Propulsion,
        navigation: &'a Navigation,
        comms: &'a Comms,
    ) -> Self {
        Self {
            reactor,
            life_support,
            propulsion,
            navigation,
            comms,
        }
    }

    fn subsystem(&self, target: Target) -> &dyn Subsystem {
        match target {
            Target::Reactor => self.reactor,
            Target::LifeSupport => self.life_support,
            Target::Propulsion => self.propulsion,
            Target::Navigation => self.navigation,
            Target::Comms => self.comms,
        }
    }
}

/// Role -> the (target, verb) pairs it may issue. Checked after schema
/// validation and before interlocks; interlocks apply no matter what this
/// table grants.
#[derive(Debug, Clone, Default)]
pub struct AuthorizationTable {
    grants: BTreeMap<Role, BTreeSet<(Target, String)>>,
}

impl AuthorizationTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn grant(&mut self, role: Role, target: Target, verb: impl Into<String>) {
        self.grants
            .entry(role)
            .or_default()
            .insert((target, verb.into()));
    }

    pub fn is_authorized(&self, role: Role, target: Target, verb: &str) -> bool {
        self.grants
            .get(&role)
            .is_some_and(|granted| granted.contains(&(target, verb.to_string())))
    }

    /// Grants the autopilot the benign, non-destructive verbs it needs to
    /// safe the ship in `SafeHold`. It is never granted the physical-key
    /// verbs (`vent`/`jettison`/`scuttle`) — those stay operator-only, and
    /// authorization is checked like any other command's, even for the
    /// autopilot.
    pub fn with_autopilot_defaults() -> Self {
        let mut table = Self::new();
        table.grant(Role::Autopilot, Target::Reactor, "set_output");
        table.grant(Role::Autopilot, Target::LifeSupport, "set_scrubber_rate");
        table.grant(Role::Autopilot, Target::Propulsion, "set_thrust");
        table.grant(Role::Autopilot, Target::Comms, "set_transmit_power");
        table
    }

    /// The autopilot's grants plus the benign verbs the model-backed roles
    /// need to be useful. This is a standing capability policy, not a roster:
    /// it says what a ship's mind or a crew agent *would* be allowed to ask
    /// for, and holds whether or not any such actor is aboard.
    ///
    /// The ship's mind gets the engineering verbs; the crew get the link home
    /// and the heading, nothing that can cook the ship. No role is granted a
    /// destructive verb here — those stay operator-only, and since the wire
    /// has no grammar for `physical_key`, they are unreachable from a model
    /// even if a grant were added by mistake.
    pub fn with_actor_defaults() -> Self {
        let mut table = Self::with_autopilot_defaults();
        table.grant(Role::ShipMind, Target::Reactor, "set_output");
        table.grant(Role::ShipMind, Target::LifeSupport, "set_scrubber_rate");
        table.grant(Role::ShipMind, Target::Comms, "set_transmit_power");
        table.grant(Role::CrewAgent, Target::Comms, "set_transmit_power");
        table.grant(Role::CrewAgent, Target::Navigation, "set_heading");
        table
    }
}

#[derive(Debug, Clone, Default)]
pub struct SafetyKernel {
    pub authorization: AuthorizationTable,
}

impl SafetyKernel {
    pub fn new(authorization: AuthorizationTable) -> Self {
        Self { authorization }
    }

    /// Runs `command` through the fixed four-stage review pipeline.
    pub fn review(&self, command: &Command, view: &ShipView, autonomy: AutonomyLevel) -> Verdict {
        if let Err(reason) = validate_schema(command, view) {
            return Verdict::Vetoed(reason);
        }

        if !self
            .authorization
            .is_authorized(command.source, command.target, &command.verb)
        {
            return Verdict::Vetoed(VetoReason::Unauthorized);
        }

        if let Some(reason) = interlocks::check(command, view) {
            return Verdict::Vetoed(reason);
        }

        if autonomy.auto_approves(command.source) {
            Verdict::Approved
        } else {
            Verdict::NeedsConfirmation
        }
    }
}

fn validate_schema(command: &Command, view: &ShipView) -> Result<(), VetoReason> {
    let subsystem = view.subsystem(command.target);
    let spec = subsystem.commands();
    let Some(arg_spec) = spec.get(&command.verb) else {
        return Err(VetoReason::UnknownVerb);
    };

    for key in command.args.keys() {
        if !arg_spec.contains_key(key) {
            return Err(VetoReason::UnknownArg(key.clone()));
        }
    }

    for (arg_name, range) in arg_spec {
        let Some(&value) = command.args.get(arg_name) else {
            return Err(VetoReason::MissingArg(arg_name.clone()));
        };
        if let Some(range) = range {
            if value < range.min || value > range.max {
                return Err(VetoReason::ArgOutOfRange {
                    arg: arg_name.clone(),
                    value,
                    min: range.min,
                    max: range.max,
                });
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rig {
        reactor: Reactor,
        life_support: LifeSupport,
        propulsion: Propulsion,
        navigation: Navigation,
        comms: Comms,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                reactor: Reactor::default(),
                life_support: LifeSupport::default(),
                propulsion: Propulsion::default(),
                navigation: Navigation::default(),
                comms: Comms::default(),
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
    }

    fn allow_everything() -> AuthorizationTable {
        let mut table = AuthorizationTable::new();
        for role in [
            Role::Autopilot,
            Role::CrewAgent,
            Role::ShipMind,
            Role::Captain,
        ] {
            table.grant(role, Target::Reactor, "set_output");
            table.grant(role, Target::Reactor, "scuttle");
            table.grant(role, Target::LifeSupport, "set_scrubber_rate");
            table.grant(role, Target::LifeSupport, "vent");
            table.grant(role, Target::Propulsion, "set_thrust");
            table.grant(role, Target::Propulsion, "jettison");
            table.grant(role, Target::Navigation, "set_heading");
        }
        table
    }

    #[test]
    fn schema_rejects_an_unknown_verb() {
        let rig = Rig::new();
        let kernel = SafetyKernel::new(allow_everything());
        let command = Command::new(Role::Captain, Target::Reactor, "implode", "test");
        assert_eq!(
            kernel.review(&command, &rig.view(), AutonomyLevel::Autonomous),
            Verdict::Vetoed(VetoReason::UnknownVerb)
        );
    }

    #[test]
    fn schema_rejects_an_out_of_range_argument() {
        let rig = Rig::new();
        let kernel = SafetyKernel::new(allow_everything());
        let command = Command::new(Role::Captain, Target::Reactor, "set_output", "test")
            .with_arg("level", 5.0);
        assert!(matches!(
            kernel.review(&command, &rig.view(), AutonomyLevel::Autonomous),
            Verdict::Vetoed(VetoReason::ArgOutOfRange { .. })
        ));
    }

    #[test]
    fn schema_rejects_a_missing_argument() {
        let rig = Rig::new();
        let kernel = SafetyKernel::new(allow_everything());
        let command = Command::new(Role::Captain, Target::Reactor, "set_output", "test");
        assert_eq!(
            kernel.review(&command, &rig.view(), AutonomyLevel::Autonomous),
            Verdict::Vetoed(VetoReason::MissingArg("level".into()))
        );
    }

    #[test]
    fn authorization_blocks_a_role_with_no_grant() {
        let rig = Rig::new();
        let kernel = SafetyKernel::new(AuthorizationTable::new());
        let command = Command::new(Role::Captain, Target::Reactor, "set_output", "test")
            .with_arg("level", 0.5);
        assert_eq!(
            kernel.review(&command, &rig.view(), AutonomyLevel::Autonomous),
            Verdict::Vetoed(VetoReason::Unauthorized)
        );
    }

    #[test]
    fn approved_when_every_stage_passes_and_autonomy_auto_approves() {
        let rig = Rig::new();
        let kernel = SafetyKernel::new(allow_everything());
        let command = Command::new(Role::Autopilot, Target::Reactor, "set_output", "test")
            .with_arg("level", 0.5);
        assert_eq!(
            kernel.review(&command, &rig.view(), AutonomyLevel::Assist),
            Verdict::Approved
        );
    }

    #[test]
    fn needs_confirmation_when_autonomy_level_does_not_auto_approve_the_role() {
        let rig = Rig::new();
        let kernel = SafetyKernel::new(allow_everything());
        let command = Command::new(Role::CrewAgent, Target::Reactor, "set_output", "test")
            .with_arg("level", 0.5);
        assert_eq!(
            kernel.review(&command, &rig.view(), AutonomyLevel::Supervised),
            Verdict::NeedsConfirmation
        );
    }

    #[test]
    fn observe_level_never_auto_approves_anyone() {
        let rig = Rig::new();
        let kernel = SafetyKernel::new(allow_everything());
        let command = Command::new(Role::Autopilot, Target::Reactor, "set_output", "test")
            .with_arg("level", 0.5);
        assert_eq!(
            kernel.review(&command, &rig.view(), AutonomyLevel::Observe),
            Verdict::NeedsConfirmation
        );
    }

    #[test]
    fn interlocks_hold_at_every_autonomy_level() {
        let rig = Rig::new();
        let kernel = SafetyKernel::new(allow_everything());
        let command = Command::new(Role::Captain, Target::Reactor, "scuttle", "test");

        for autonomy in [
            AutonomyLevel::Observe,
            AutonomyLevel::Assist,
            AutonomyLevel::Supervised,
            AutonomyLevel::Autonomous,
        ] {
            assert_eq!(
                kernel.review(&command, &rig.view(), autonomy),
                Verdict::Vetoed(VetoReason::RequiresPhysicalKey),
                "interlock should hold at {autonomy:?}"
            );
        }
    }

    /// The actor grants are a capability floor, not a convenience: no role a
    /// model can speak as may ask for a destructive verb, whatever it
    /// proposes and whatever the autonomy level.
    #[test]
    fn the_actor_grants_never_reach_a_destructive_verb() {
        let table = AuthorizationTable::with_actor_defaults();

        for role in [Role::ShipMind, Role::CrewAgent, Role::Autopilot] {
            assert!(!table.is_authorized(role, Target::LifeSupport, "vent"));
            assert!(!table.is_authorized(role, Target::Propulsion, "jettison"));
            assert!(!table.is_authorized(role, Target::Reactor, "scuttle"));
        }
    }

    /// The ship's mind answers for the ship's own systems; the crew do not.
    #[test]
    fn the_actor_grants_separate_the_ship_minds_authority_from_the_crews() {
        let table = AuthorizationTable::with_actor_defaults();

        assert!(table.is_authorized(Role::ShipMind, Target::Reactor, "set_output"));
        assert!(!table.is_authorized(Role::CrewAgent, Target::Reactor, "set_output"));
        assert!(table.is_authorized(Role::CrewAgent, Target::Navigation, "set_heading"));
        assert!(!table.is_authorized(Role::ShipMind, Target::Navigation, "set_heading"));
    }

    #[test]
    fn captain_authority_cannot_bypass_a_hard_interlock() {
        let rig = Rig::new();
        assert!(rig.life_support.crew_aboard);

        let kernel = SafetyKernel::new(allow_everything());
        // Fully authorized, fully autonomous, and carrying the physical key —
        // none of that overrides the crew-aboard interlock on venting life support.
        let command =
            Command::new(Role::Captain, Target::LifeSupport, "vent", "test").with_physical_key();

        assert_eq!(
            kernel.review(&command, &rig.view(), AutonomyLevel::Autonomous),
            Verdict::Vetoed(VetoReason::LifeSupportUnsafeWithCrewAboard)
        );
    }
}
