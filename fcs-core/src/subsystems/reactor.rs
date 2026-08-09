use std::collections::BTreeMap;

use crate::telemetry::RawSample;
use crate::world::Environment;

use super::{ApplyError, ArgRange, CommandArgs, CommandSpec, Subsystem};

/// Hard thermal ceiling in kelvin. No command may ever push `core_temp_k` above
/// this. The kernel doesn't exist yet to enforce it as an interlock, but the
/// limit is modeled here now so Phase 2 has a real value to check against.
pub const THERMAL_CEILING_K: f64 = 1200.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Reactor {
    pub core_temp_k: f64,
    pub output_level: f64,
}

impl Default for Reactor {
    fn default() -> Self {
        Self {
            core_temp_k: 300.0,
            output_level: 0.2,
        }
    }
}

impl Subsystem for Reactor {
    fn name(&self) -> &'static str {
        "reactor"
    }

    fn tick(&mut self, dt: f64, env: &Environment) {
        let heating = self.output_level * 50.0;
        let radiative_loss = (self.core_temp_k - env.ambient_temp_k) * 0.01;
        self.core_temp_k += (heating - radiative_loss) * dt;
        self.core_temp_k = self.core_temp_k.min(THERMAL_CEILING_K);
    }

    fn sample(&self) -> Vec<RawSample> {
        vec![
            RawSample {
                name: "sys.reactor.core_temp_k".into(),
                value: self.core_temp_k,
            },
            RawSample {
                name: "sys.reactor.output_level".into(),
                value: self.output_level,
            },
        ]
    }

    fn commands(&self) -> CommandSpec {
        let mut spec = CommandSpec::new();
        let mut args: BTreeMap<String, Option<ArgRange>> = BTreeMap::new();
        args.insert("level".into(), Some(ArgRange { min: 0.0, max: 1.0 }));
        spec.insert("set_output".into(), args);
        spec
    }

    fn apply(&mut self, verb: &str, args: &CommandArgs) -> Result<(), ApplyError> {
        match verb {
            "set_output" => {
                let level = args
                    .get("level")
                    .ok_or_else(|| ApplyError::MissingArg("level".into()))?;
                self.output_level = *level;
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
    fn set_output_changes_throttle_and_ticking_heats_the_core() {
        let mut reactor = Reactor::default();
        let env = Environment::default();

        let mut args = CommandArgs::new();
        args.insert("level".into(), 1.0);
        reactor.apply("set_output", &args).unwrap();

        let before = reactor.core_temp_k;
        reactor.tick(1.0, &env);
        assert!(reactor.core_temp_k > before);
    }

    #[test]
    fn never_exceeds_the_thermal_ceiling() {
        let mut reactor = Reactor {
            core_temp_k: THERMAL_CEILING_K,
            output_level: 1.0,
        };
        let env = Environment::default();
        reactor.tick(10.0, &env);
        assert!(reactor.core_temp_k <= THERMAL_CEILING_K);
    }

    #[test]
    fn declares_set_output_bounded_to_the_unit_range() {
        let reactor = Reactor::default();
        let spec = reactor.commands();
        let range = spec.get("set_output").unwrap().get("level").unwrap().unwrap();
        assert_eq!(range, ArgRange { min: 0.0, max: 1.0 });
    }

    #[test]
    fn publishes_namespaced_telemetry() {
        let reactor = Reactor::default();
        let names: Vec<_> = reactor.sample().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"sys.reactor.core_temp_k".to_string()));
        assert!(names.contains(&"sys.reactor.output_level".to_string()));
    }

    #[test]
    fn rejects_unknown_verbs() {
        let mut reactor = Reactor::default();
        let args = CommandArgs::new();
        assert_eq!(
            reactor.apply("scuttle", &args),
            Err(ApplyError::UnknownVerb("scuttle".into()))
        );
    }
}
