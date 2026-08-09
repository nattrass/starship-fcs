//! The integration loop: advance clock, advance world, advance subsystems,
//! sample telemetry, run fault detection, and — when a fault forces
//! `SafeHold` — let the autopilot propose recovery commands through the
//! full safety kernel pipeline. Runs fully offline, with no actors or
//! providers involved; the ship must be able to survive on this loop alone.

use std::collections::BTreeSet;

use crate::autopilot;
use crate::clock::Clock;
use crate::command::{Command, Target};
use crate::fdir::{self, Fault, OperatingMode};
use crate::safety::{AuthorizationTable, AutonomyLevel, SafetyKernel, ShipView, Verdict};
use crate::subsystems::{Comms, LifeSupport, Navigation, Propulsion, Reactor, Subsystem};
use crate::telemetry::{ChannelStatus, RawSample, TelemetrySampler, TelemetrySnapshot};
use crate::world::World;

#[derive(Debug, Clone)]
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
            kernel: SafetyKernel::new(AuthorizationTable::with_autopilot_defaults()),
            autonomy: AutonomyLevel::Assist,
            mode: OperatingMode::Nominal,
            faults: BTreeSet::new(),
        }
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
    /// Every command source — autopilot included — goes through this same path.
    fn review_and_apply(&mut self, command: &Command) {
        let verdict = self.kernel.review(command, &self.view(), self.autonomy);
        if verdict == Verdict::Approved {
            let subsystem = self.subsystem_mut(command.target);
            let _ = subsystem.apply(&command.verb, &command.args);
        }
    }

    /// Runs one full tick: advance clock, advance world, advance subsystems,
    /// sample telemetry, detect faults, and — in `SafeHold` — let the
    /// autopilot propose commands to safe the ship, each still passing
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

        self.faults = fdir::detect(&snapshot);
        self.mode = OperatingMode::from_faults(&self.faults);

        if self.mode == OperatingMode::SafeHold {
            for command in autopilot::plan(self.mode, &self.faults) {
                self.review_and_apply(&command);
            }
        }

        snapshot
    }
}

/// Formats a snapshot as a single human-readable report line for CLI output.
/// Channel order follows the snapshot's stable (`BTreeMap`) ordering, so a
/// fixed scenario always produces the same report line.
pub fn format_report(snapshot: &TelemetrySnapshot) -> String {
    let mut parts = vec![
        format!("tick={}", snapshot.tick_count),
        format!("t={:.2}s", snapshot.elapsed),
    ];
    for (name, channel) in snapshot.iter() {
        let marker = match channel.status {
            ChannelStatus::Nominal => "",
            ChannelStatus::Spoofed => "*",
            ChannelStatus::Dropout => "!",
        };
        parts.push(format!("{name}={:.3}{marker}", channel.value));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subsystems::reactor::THERMAL_CEILING_K;

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

        assert_eq!(
            snapshot.get("sys.reactor.core_temp_k").unwrap().value,
            -1.0
        );
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
}
