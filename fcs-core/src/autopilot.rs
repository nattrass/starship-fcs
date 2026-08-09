//! A pure, deterministic controller that drives the ship toward safety while
//! in `SafeHold`. Like every other command source, its proposals are never
//! applied directly — they still go through the full safety kernel pipeline
//! (defense in depth) before anything touches subsystem state.

use std::collections::BTreeSet;

use crate::command::{Command, Role, Target};
use crate::fdir::{Fault, OperatingMode};

/// Proposes the commands the autopilot would issue this tick. Returns
/// nothing outside `SafeHold` — the autopilot only takes over once the ship
/// needs safing.
pub fn plan(mode: OperatingMode, faults: &BTreeSet<Fault>) -> Vec<Command> {
    if mode != OperatingMode::SafeHold {
        return Vec::new();
    }

    faults.iter().filter_map(command_for).collect()
}

fn command_for(fault: &Fault) -> Option<Command> {
    match fault {
        Fault::ReactorOvertemp => Some(
            Command::new(
                Role::Autopilot,
                Target::Reactor,
                "set_output",
                "autopilot: reactor overtemp, throttling down",
            )
            .with_arg("level", 0.0),
        ),
        Fault::O2Low => Some(
            Command::new(
                Role::Autopilot,
                Target::LifeSupport,
                "set_scrubber_rate",
                "autopilot: O2 low, scrubbing at max",
            )
            .with_arg("rate", 1.0),
        ),
        Fault::PressureLoss => Some(
            Command::new(
                Role::Autopilot,
                Target::Propulsion,
                "set_thrust",
                "autopilot: pressure loss, holding position",
            )
            .with_arg("thrust_n", 0.0),
        ),
        Fault::CommsLoss => Some(
            Command::new(
                Role::Autopilot,
                Target::Comms,
                "set_transmit_power",
                "autopilot: comms loss, boosting transmit power",
            )
            .with_arg("power", 1.0),
        ),
        // Nothing safe to command against telemetry that can't be trusted;
        // SafeHold itself is the response.
        Fault::TelemetryLost(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposes_nothing_when_nominal() {
        let mut faults = BTreeSet::new();
        faults.insert(Fault::ReactorOvertemp);
        assert!(plan(OperatingMode::Nominal, &faults).is_empty());
    }

    #[test]
    fn throttles_down_the_reactor_on_overtemp() {
        let mut faults = BTreeSet::new();
        faults.insert(Fault::ReactorOvertemp);
        let commands = plan(OperatingMode::SafeHold, &faults);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].target, Target::Reactor);
        assert_eq!(commands[0].verb, "set_output");
        assert_eq!(commands[0].args.get("level"), Some(&0.0));
    }

    #[test]
    fn maxes_the_scrubber_on_o2_low() {
        let mut faults = BTreeSet::new();
        faults.insert(Fault::O2Low);
        let commands = plan(OperatingMode::SafeHold, &faults);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].target, Target::LifeSupport);
        assert_eq!(commands[0].verb, "set_scrubber_rate");
        assert_eq!(commands[0].args.get("rate"), Some(&1.0));
    }

    #[test]
    fn holds_position_on_pressure_loss() {
        let mut faults = BTreeSet::new();
        faults.insert(Fault::PressureLoss);
        let commands = plan(OperatingMode::SafeHold, &faults);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].target, Target::Propulsion);
        assert_eq!(commands[0].verb, "set_thrust");
        assert_eq!(commands[0].args.get("thrust_n"), Some(&0.0));
    }

    #[test]
    fn boosts_transmit_power_on_comms_loss() {
        let mut faults = BTreeSet::new();
        faults.insert(Fault::CommsLoss);
        let commands = plan(OperatingMode::SafeHold, &faults);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].target, Target::Comms);
        assert_eq!(commands[0].verb, "set_transmit_power");
        assert_eq!(commands[0].args.get("power"), Some(&1.0));
    }

    #[test]
    fn proposes_nothing_for_telemetry_lost_alone() {
        let mut faults = BTreeSet::new();
        faults.insert(Fault::TelemetryLost("sys.reactor.core_temp_k"));
        assert!(plan(OperatingMode::SafeHold, &faults).is_empty());
    }

    #[test]
    fn a_command_per_fault_is_deterministically_ordered() {
        let mut faults = BTreeSet::new();
        faults.insert(Fault::CommsLoss);
        faults.insert(Fault::ReactorOvertemp);
        faults.insert(Fault::O2Low);

        let run = || plan(OperatingMode::SafeHold, &faults);
        assert_eq!(run(), run());
        assert_eq!(run().len(), 3);
    }
}
