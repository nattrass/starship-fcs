use std::collections::BTreeMap;

use crate::telemetry::RawSample;
use crate::world::Environment;

use super::{ApplyError, ArgRange, CommandArgs, CommandSpec, Subsystem};

/// Hard structural limit on commanded thrust, in newtons. The kernel doesn't
/// exist yet to enforce it as an interlock, but the limit is modeled here now
/// so Phase 2 has a real value to check against.
pub const STRUCTURAL_THRUST_LIMIT_N: f64 = 500_000.0;

const SHIP_MASS_KG: f64 = 50_000.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Propulsion {
    pub thrust_n: f64,
    pub velocity_mps: f64,
}

impl Default for Propulsion {
    fn default() -> Self {
        Self {
            thrust_n: 0.0,
            velocity_mps: 0.0,
        }
    }
}

impl Subsystem for Propulsion {
    fn name(&self) -> &'static str {
        "propulsion"
    }

    fn tick(&mut self, dt: f64, _env: &Environment) {
        self.velocity_mps += (self.thrust_n / SHIP_MASS_KG) * dt;
    }

    fn sample(&self) -> Vec<RawSample> {
        vec![
            RawSample {
                name: "sys.propulsion.thrust_n".into(),
                value: self.thrust_n,
            },
            RawSample {
                name: "sys.propulsion.velocity_mps".into(),
                value: self.velocity_mps,
            },
        ]
    }

    fn commands(&self) -> CommandSpec {
        let mut spec = CommandSpec::new();
        let mut args: BTreeMap<String, Option<ArgRange>> = BTreeMap::new();
        args.insert(
            "thrust_n".into(),
            Some(ArgRange {
                min: 0.0,
                max: STRUCTURAL_THRUST_LIMIT_N,
            }),
        );
        spec.insert("set_thrust".into(), args);
        spec
    }

    fn apply(&mut self, verb: &str, args: &CommandArgs) -> Result<(), ApplyError> {
        match verb {
            "set_thrust" => {
                let thrust = args
                    .get("thrust_n")
                    .ok_or_else(|| ApplyError::MissingArg("thrust_n".into()))?;
                self.thrust_n = *thrust;
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
    fn thrust_accelerates_the_ship() {
        let mut propulsion = Propulsion {
            thrust_n: 50_000.0,
            velocity_mps: 0.0,
        };
        let env = Environment::default();
        propulsion.tick(1.0, &env);
        assert!((propulsion.velocity_mps - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn declares_set_thrust_bounded_by_the_structural_limit() {
        let propulsion = Propulsion::default();
        let spec = propulsion.commands();
        let range = spec
            .get("set_thrust")
            .unwrap()
            .get("thrust_n")
            .unwrap()
            .unwrap();
        assert_eq!(range.max, STRUCTURAL_THRUST_LIMIT_N);
    }

    #[test]
    fn publishes_namespaced_telemetry() {
        let propulsion = Propulsion::default();
        let names: Vec<_> = propulsion.sample().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"sys.propulsion.thrust_n".to_string()));
        assert!(names.contains(&"sys.propulsion.velocity_mps".to_string()));
    }
}
