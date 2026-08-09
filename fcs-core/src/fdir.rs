//! Fault detection, isolation, and recovery. `detect` is a pure function of
//! a tick's telemetry snapshot — never the world or subsystems directly.
//! Telemetry is the only reality FDIR sees too, so a spoofed channel can
//! mislead FDIR exactly the way it could mislead an actor, and a
//! dropped-out channel is treated as lost visibility rather than silently
//! read as a safe zero.

use std::collections::BTreeSet;

use crate::subsystems::comms::MIN_USABLE_SIGNAL_STRENGTH;
use crate::subsystems::life_support::{MIN_SAFE_O2_LEVEL, MIN_SAFE_PRESSURE_KPA};
use crate::subsystems::reactor::THERMAL_CEILING_K;
use crate::telemetry::{ChannelStatus, TelemetrySnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fault {
    ReactorOvertemp,
    O2Low,
    PressureLoss,
    CommsLoss,
    /// A channel FDIR depends on is lost/stale this tick; its value can't be trusted.
    TelemetryLost(&'static str),
}

const CHANNEL_REACTOR_TEMP: &str = "sys.reactor.core_temp_k";
const CHANNEL_O2_LEVEL: &str = "sys.life_support.o2_level";
const CHANNEL_PRESSURE: &str = "sys.life_support.pressure_kpa";
const CHANNEL_SIGNAL: &str = "sys.comms.signal_strength";

/// Evaluates one tick's telemetry snapshot and reports every fault found.
pub fn detect(snapshot: &TelemetrySnapshot) -> BTreeSet<Fault> {
    let mut faults = BTreeSet::new();

    check_channel(snapshot, CHANNEL_REACTOR_TEMP, Fault::ReactorOvertemp, &mut faults, |v| {
        v >= THERMAL_CEILING_K
    });
    check_channel(snapshot, CHANNEL_O2_LEVEL, Fault::O2Low, &mut faults, |v| {
        v <= MIN_SAFE_O2_LEVEL
    });
    check_channel(snapshot, CHANNEL_PRESSURE, Fault::PressureLoss, &mut faults, |v| {
        v <= MIN_SAFE_PRESSURE_KPA
    });
    check_channel(snapshot, CHANNEL_SIGNAL, Fault::CommsLoss, &mut faults, |v| {
        v <= MIN_USABLE_SIGNAL_STRENGTH
    });

    faults
}

fn check_channel(
    snapshot: &TelemetrySnapshot,
    name: &'static str,
    fault: Fault,
    faults: &mut BTreeSet<Fault>,
    is_unsafe: impl Fn(f64) -> bool,
) {
    let Some(channel) = snapshot.get(name) else {
        return;
    };
    if channel.status == ChannelStatus::Dropout {
        faults.insert(Fault::TelemetryLost(name));
        return;
    }
    if is_unsafe(channel.value) {
        faults.insert(fault);
    }
}

/// The ship's current safety posture. Every currently-defined fault kind is
/// safety-critical, so any fault at all forces `SafeHold` — this is a pure
/// function of the current tick's fault set, not sticky/latched state, so
/// the ship returns to `Nominal` as soon as a tick's faults clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatingMode {
    Nominal,
    SafeHold,
}

impl OperatingMode {
    pub fn from_faults(faults: &BTreeSet<Fault>) -> Self {
        if faults.is_empty() {
            OperatingMode::Nominal
        } else {
            OperatingMode::SafeHold
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{RawSample, TelemetrySampler};

    fn snapshot_from(readings: &[(&str, f64)]) -> TelemetrySnapshot {
        let sampler = TelemetrySampler::new();
        let raw = readings
            .iter()
            .map(|(name, value)| RawSample {
                name: (*name).to_string(),
                value: *value,
            })
            .collect();
        sampler.sample(1, 1.0, raw)
    }

    #[test]
    fn detects_reactor_overtemp() {
        let snapshot = snapshot_from(&[(CHANNEL_REACTOR_TEMP, THERMAL_CEILING_K)]);
        assert!(detect(&snapshot).contains(&Fault::ReactorOvertemp));
    }

    #[test]
    fn detects_o2_low() {
        let snapshot = snapshot_from(&[(CHANNEL_O2_LEVEL, MIN_SAFE_O2_LEVEL)]);
        assert!(detect(&snapshot).contains(&Fault::O2Low));
    }

    #[test]
    fn detects_pressure_loss() {
        let snapshot = snapshot_from(&[(CHANNEL_PRESSURE, MIN_SAFE_PRESSURE_KPA)]);
        assert!(detect(&snapshot).contains(&Fault::PressureLoss));
    }

    #[test]
    fn detects_comms_loss() {
        let snapshot = snapshot_from(&[(CHANNEL_SIGNAL, MIN_USABLE_SIGNAL_STRENGTH)]);
        assert!(detect(&snapshot).contains(&Fault::CommsLoss));
    }

    #[test]
    fn nominal_readings_produce_no_faults() {
        let snapshot = snapshot_from(&[
            (CHANNEL_REACTOR_TEMP, 300.0),
            (CHANNEL_O2_LEVEL, 0.21),
            (CHANNEL_PRESSURE, 101.3),
            (CHANNEL_SIGNAL, 0.9),
        ]);
        assert!(detect(&snapshot).is_empty());
    }

    #[test]
    fn a_dropped_out_channel_is_reported_as_lost_rather_than_a_safe_zero() {
        let mut sampler = TelemetrySampler::new();
        sampler.drop_out(CHANNEL_REACTOR_TEMP);
        let raw = vec![RawSample {
            name: CHANNEL_REACTOR_TEMP.to_string(),
            value: 300.0,
        }];
        let snapshot = sampler.sample(1, 1.0, raw);

        let faults = detect(&snapshot);
        assert!(faults.contains(&Fault::TelemetryLost(CHANNEL_REACTOR_TEMP)));
        assert!(!faults.contains(&Fault::ReactorOvertemp));
    }

    #[test]
    fn mode_is_nominal_with_no_faults_and_safe_hold_with_any_fault() {
        assert_eq!(OperatingMode::from_faults(&BTreeSet::new()), OperatingMode::Nominal);

        let mut faults = BTreeSet::new();
        faults.insert(Fault::O2Low);
        assert_eq!(OperatingMode::from_faults(&faults), OperatingMode::SafeHold);
    }
}
