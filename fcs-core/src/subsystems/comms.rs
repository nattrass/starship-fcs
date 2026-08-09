use std::collections::BTreeMap;

use crate::telemetry::RawSample;
use crate::world::Environment;

use super::{ApplyError, ArgRange, CommandArgs, CommandSpec, Subsystem};

/// Minimum signal strength still considered a usable link. Phase 3's fault
/// monitor checks this to detect comms loss.
pub const MIN_USABLE_SIGNAL_STRENGTH: f64 = 0.1;

#[derive(Debug, Clone, PartialEq)]
pub struct Comms {
    pub signal_strength: f64,
    pub transmit_power: f64,
}

impl Default for Comms {
    fn default() -> Self {
        Self {
            signal_strength: 1.0,
            transmit_power: 0.5,
        }
    }
}

impl Subsystem for Comms {
    fn name(&self) -> &'static str {
        "comms"
    }

    fn tick(&mut self, dt: f64, env: &Environment) {
        let interference = env.radiation_rate * 0.1;
        let target = (self.transmit_power - interference).clamp(0.0, 1.0);
        self.signal_strength += (target - self.signal_strength) * dt.min(1.0);
    }

    fn sample(&self) -> Vec<RawSample> {
        vec![
            RawSample {
                name: "sys.comms.signal_strength".into(),
                value: self.signal_strength,
            },
            RawSample {
                name: "sys.comms.transmit_power".into(),
                value: self.transmit_power,
            },
        ]
    }

    fn commands(&self) -> CommandSpec {
        let mut spec = CommandSpec::new();
        let mut args: BTreeMap<String, Option<ArgRange>> = BTreeMap::new();
        args.insert("power".into(), Some(ArgRange { min: 0.0, max: 1.0 }));
        spec.insert("set_transmit_power".into(), args);
        spec
    }

    fn apply(&mut self, verb: &str, args: &CommandArgs) -> Result<(), ApplyError> {
        match verb {
            "set_transmit_power" => {
                let power = args
                    .get("power")
                    .ok_or_else(|| ApplyError::MissingArg("power".into()))?;
                self.transmit_power = *power;
                Ok(())
            }
            other => Err(ApplyError::UnknownVerb(other.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radiation_interference_degrades_signal_strength() {
        let mut comms = Comms {
            signal_strength: 1.0,
            transmit_power: 1.0,
        };
        let env = Environment {
            ambient_temp_k: 2.7,
            radiation_rate: 5.0,
        };
        comms.tick(1.0, &env);
        assert!(comms.signal_strength < 1.0);
    }

    #[test]
    fn publishes_namespaced_telemetry() {
        let comms = Comms::default();
        let names: Vec<_> = comms.sample().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"sys.comms.signal_strength".to_string()));
    }
}
