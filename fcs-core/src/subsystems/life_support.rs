use std::collections::BTreeMap;

use crate::telemetry::RawSample;
use crate::world::Environment;

use super::{ApplyError, ArgRange, CommandArgs, CommandSpec, Subsystem};

/// Minimum safe O2 level fraction with crew aboard. The kernel doesn't exist
/// yet to enforce it as an interlock, but the limit is modeled here now so
/// Phase 2 has a real value to check against.
pub const MIN_SAFE_O2_LEVEL: f64 = 0.18;

#[derive(Debug, Clone, PartialEq)]
pub struct LifeSupport {
    pub o2_level: f64,
    pub scrubber_rate: f64,
    pub crew_aboard: bool,
}

impl Default for LifeSupport {
    fn default() -> Self {
        Self {
            o2_level: 0.21,
            scrubber_rate: 0.5,
            crew_aboard: true,
        }
    }
}

impl Subsystem for LifeSupport {
    fn name(&self) -> &'static str {
        "life_support"
    }

    fn tick(&mut self, dt: f64, _env: &Environment) {
        let consumption = if self.crew_aboard { 0.01 } else { 0.0 };
        self.o2_level += (self.scrubber_rate * 0.02 - consumption) * dt;
        self.o2_level = self.o2_level.clamp(0.0, 1.0);
    }

    fn sample(&self) -> Vec<RawSample> {
        vec![
            RawSample {
                name: "sys.life_support.o2_level".into(),
                value: self.o2_level,
            },
            RawSample {
                name: "sys.life_support.scrubber_rate".into(),
                value: self.scrubber_rate,
            },
            RawSample {
                name: "sys.life_support.crew_aboard".into(),
                value: if self.crew_aboard { 1.0 } else { 0.0 },
            },
        ]
    }

    fn commands(&self) -> CommandSpec {
        let mut spec = CommandSpec::new();
        let mut args: BTreeMap<String, Option<ArgRange>> = BTreeMap::new();
        args.insert("rate".into(), Some(ArgRange { min: 0.0, max: 1.0 }));
        spec.insert("set_scrubber_rate".into(), args);
        spec
    }

    fn apply(&mut self, verb: &str, args: &CommandArgs) -> Result<(), ApplyError> {
        match verb {
            "set_scrubber_rate" => {
                let rate = args
                    .get("rate")
                    .ok_or_else(|| ApplyError::MissingArg("rate".into()))?;
                self.scrubber_rate = *rate;
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
    fn scrubbing_raises_o2_level_over_time() {
        let mut life_support = LifeSupport {
            o2_level: 0.1,
            scrubber_rate: 1.0,
            crew_aboard: false,
        };
        let env = Environment::default();
        let before = life_support.o2_level;
        life_support.tick(1.0, &env);
        assert!(life_support.o2_level > before);
    }

    #[test]
    fn o2_level_stays_within_the_unit_range() {
        let mut life_support = LifeSupport {
            o2_level: 0.99,
            scrubber_rate: 1.0,
            crew_aboard: false,
        };
        let env = Environment::default();
        for _ in 0..100 {
            life_support.tick(1.0, &env);
        }
        assert!(life_support.o2_level <= 1.0);
    }

    #[test]
    fn publishes_namespaced_telemetry() {
        let life_support = LifeSupport::default();
        let names: Vec<_> = life_support.sample().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"sys.life_support.o2_level".to_string()));
        assert!(names.contains(&"sys.life_support.crew_aboard".to_string()));
    }
}
