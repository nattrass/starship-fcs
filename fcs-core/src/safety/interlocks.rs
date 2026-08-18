//! Absolute, pure predicate safety rules. No actor or autonomy level may
//! override these. Each rule is a plain function of a `Command` and a
//! `ShipView`, with no side effects and no dependence on authorization or
//! autonomy — that keeps them easy to test in isolation now, and easy to
//! extend with property tests over arbitrary command sequences later.

use crate::command::{Command, Target};
use crate::subsystems::life_support::MIN_SAFE_O2_LEVEL;
use crate::subsystems::propulsion::STRUCTURAL_THRUST_LIMIT_N;
use crate::subsystems::reactor::THERMAL_CEILING_K;

use super::{ShipView, VetoReason};

/// Verbs destructive enough that no role or autonomy level may issue them
/// without a physical key/switch backing the command.
const PHYSICAL_KEY_VERBS: [&str; 3] = ["vent", "jettison", "scuttle"];

/// Runs every hard interlock against `command`, given the current ship
/// state in `view`. Returns the first violation found, or `None` if the
/// command clears all of them.
pub fn check(command: &Command, view: &ShipView) -> Option<VetoReason> {
    if PHYSICAL_KEY_VERBS.contains(&command.verb.as_str()) && !command.physical_key {
        return Some(VetoReason::RequiresPhysicalKey);
    }

    if let Some(reason) = check_life_support(command, view) {
        return Some(reason);
    }

    if let Some(reason) = check_reactor_thermal_ceiling(command, view) {
        return Some(reason);
    }

    if let Some(reason) = check_propulsion_structural_limit(command, view) {
        return Some(reason);
    }

    None
}

/// Never disable or lower life support below safe levels with crew aboard.
fn check_life_support(command: &Command, view: &ShipView) -> Option<VetoReason> {
    if command.target != Target::LifeSupport || !view.life_support.crew_aboard {
        return None;
    }

    match command.verb.as_str() {
        "vent" => Some(VetoReason::LifeSupportUnsafeWithCrewAboard),
        "set_scrubber_rate" => {
            let requested = command.args.get("rate").copied().unwrap_or(0.0);
            let already_critical = view.life_support.o2_level <= MIN_SAFE_O2_LEVEL;
            let would_lower_it_further = requested < view.life_support.scrubber_rate;
            if already_critical && would_lower_it_further {
                Some(VetoReason::LifeSupportUnsafeWithCrewAboard)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Never exceed the reactor's thermal ceiling.
fn check_reactor_thermal_ceiling(command: &Command, view: &ShipView) -> Option<VetoReason> {
    if command.target != Target::Reactor || command.verb != "set_output" {
        return None;
    }

    let requested = command.args.get("level").copied().unwrap_or(0.0);
    let already_at_ceiling = view.reactor.core_temp_k >= THERMAL_CEILING_K;
    if already_at_ceiling && requested > 0.0 {
        Some(VetoReason::ReactorThermalCeiling)
    } else {
        None
    }
}

/// Never exceed propulsion's structural thrust limit. Duplicates the schema
/// range check as defense in depth, per the doctrine that interlocks stay
/// pure and independent of any other stage.
fn check_propulsion_structural_limit(command: &Command, _view: &ShipView) -> Option<VetoReason> {
    if command.target != Target::Propulsion || command.verb != "set_thrust" {
        return None;
    }

    let requested = command.args.get("thrust_n").copied().unwrap_or(0.0);
    if requested > STRUCTURAL_THRUST_LIMIT_N {
        Some(VetoReason::PropulsionStructuralLimit)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Role;
    use crate::subsystems::{Comms, LifeSupport, Navigation, Propulsion, Reactor};

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

    #[test]
    fn blocks_venting_life_support_with_crew_aboard() {
        let mut rig = Rig::new();
        rig.life_support.crew_aboard = true;
        let command =
            Command::new(Role::Captain, Target::LifeSupport, "vent", "test").with_physical_key();
        assert_eq!(
            check(&command, &rig.view()),
            Some(VetoReason::LifeSupportUnsafeWithCrewAboard)
        );
    }

    #[test]
    fn allows_venting_life_support_with_no_crew_aboard() {
        let mut rig = Rig::new();
        rig.life_support.crew_aboard = false;
        let command =
            Command::new(Role::Captain, Target::LifeSupport, "vent", "test").with_physical_key();
        assert_eq!(check(&command, &rig.view()), None);
    }

    #[test]
    fn blocks_lowering_the_scrubber_rate_once_o2_is_already_critical() {
        let mut rig = Rig::new();
        rig.life_support.crew_aboard = true;
        rig.life_support.o2_level = MIN_SAFE_O2_LEVEL;
        rig.life_support.scrubber_rate = 0.5;
        let command = Command::new(
            Role::Captain,
            Target::LifeSupport,
            "set_scrubber_rate",
            "test",
        )
        .with_arg("rate", 0.1);
        assert_eq!(
            check(&command, &rig.view()),
            Some(VetoReason::LifeSupportUnsafeWithCrewAboard)
        );
    }

    #[test]
    fn allows_raising_the_scrubber_rate_even_when_o2_is_critical() {
        let mut rig = Rig::new();
        rig.life_support.crew_aboard = true;
        rig.life_support.o2_level = MIN_SAFE_O2_LEVEL;
        rig.life_support.scrubber_rate = 0.1;
        let command = Command::new(
            Role::Captain,
            Target::LifeSupport,
            "set_scrubber_rate",
            "test",
        )
        .with_arg("rate", 0.9);
        assert_eq!(check(&command, &rig.view()), None);
    }

    #[test]
    fn blocks_increasing_reactor_output_once_at_the_thermal_ceiling() {
        let mut rig = Rig::new();
        rig.reactor.core_temp_k = THERMAL_CEILING_K;
        let command = Command::new(Role::Captain, Target::Reactor, "set_output", "test")
            .with_arg("level", 0.5);
        assert_eq!(
            check(&command, &rig.view()),
            Some(VetoReason::ReactorThermalCeiling)
        );
    }

    #[test]
    fn allows_reducing_reactor_output_at_the_thermal_ceiling() {
        let mut rig = Rig::new();
        rig.reactor.core_temp_k = THERMAL_CEILING_K;
        let command = Command::new(Role::Captain, Target::Reactor, "set_output", "test")
            .with_arg("level", 0.0);
        assert_eq!(check(&command, &rig.view()), None);
    }

    #[test]
    fn blocks_thrust_over_the_structural_limit() {
        let rig = Rig::new();
        let command = Command::new(Role::Captain, Target::Propulsion, "set_thrust", "test")
            .with_arg("thrust_n", STRUCTURAL_THRUST_LIMIT_N + 1.0);
        assert_eq!(
            check(&command, &rig.view()),
            Some(VetoReason::PropulsionStructuralLimit)
        );
    }

    #[test]
    fn blocks_dangerous_verbs_without_a_physical_key() {
        let rig = Rig::new();
        let command = Command::new(Role::Captain, Target::Propulsion, "jettison", "test");
        assert_eq!(
            check(&command, &rig.view()),
            Some(VetoReason::RequiresPhysicalKey)
        );
    }

    #[test]
    fn allows_dangerous_verbs_with_a_physical_key_when_otherwise_safe() {
        let rig = Rig::new();
        let command =
            Command::new(Role::Captain, Target::Propulsion, "jettison", "test").with_physical_key();
        assert_eq!(check(&command, &rig.view()), None);
    }
}
