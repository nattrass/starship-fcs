//! The integration loop: advance clock, advance world, advance subsystems,
//! sample telemetry, and format a per-tick report. Runs fully offline, with
//! no actors or providers involved.

use crate::clock::Clock;
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

    /// Runs one full tick: advance clock, advance world, advance subsystems,
    /// then sample telemetry from the resulting state.
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

        self.telemetry.sample(tick_count, elapsed, raw)
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
}
