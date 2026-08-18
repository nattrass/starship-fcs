use std::collections::BTreeMap;

use crate::telemetry::RawSample;
use crate::world::Environment;

use super::{ApplyError, ArgRange, CommandArgs, CommandSpec, Subsystem};

const TURN_RATE_DEG_S: f64 = 5.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Navigation {
    pub heading_deg: f64,
    pub target_heading_deg: f64,
}

impl Default for Navigation {
    fn default() -> Self {
        Self {
            heading_deg: 0.0,
            target_heading_deg: 0.0,
        }
    }
}

impl Subsystem for Navigation {
    fn name(&self) -> &'static str {
        "navigation"
    }

    fn tick(&mut self, dt: f64, _env: &Environment) {
        let delta = self.target_heading_deg - self.heading_deg;
        let step = TURN_RATE_DEG_S * dt;
        if delta.abs() <= step {
            self.heading_deg = self.target_heading_deg;
        } else {
            self.heading_deg += step * delta.signum();
        }
    }

    fn sample(&self) -> Vec<RawSample> {
        vec![
            RawSample {
                name: "sys.navigation.heading_deg".into(),
                value: self.heading_deg,
            },
            RawSample {
                name: "sys.navigation.target_heading_deg".into(),
                value: self.target_heading_deg,
            },
        ]
    }

    fn commands(&self) -> CommandSpec {
        let mut spec = CommandSpec::new();
        let mut args: BTreeMap<String, Option<ArgRange>> = BTreeMap::new();
        args.insert(
            "heading_deg".into(),
            Some(ArgRange {
                min: 0.0,
                max: 360.0,
            }),
        );
        spec.insert("set_heading".into(), args);
        spec
    }

    fn apply(&mut self, verb: &str, args: &CommandArgs) -> Result<(), ApplyError> {
        match verb {
            "set_heading" => {
                let heading = args
                    .get("heading_deg")
                    .ok_or_else(|| ApplyError::MissingArg("heading_deg".into()))?;
                self.target_heading_deg = *heading;
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
    fn turns_toward_the_target_heading_at_a_fixed_rate() {
        let mut nav = Navigation {
            heading_deg: 0.0,
            target_heading_deg: 90.0,
        };
        let env = Environment::default();
        nav.tick(1.0, &env);
        assert!((nav.heading_deg - TURN_RATE_DEG_S).abs() < f64::EPSILON);
    }

    #[test]
    fn does_not_overshoot_the_target_heading() {
        let mut nav = Navigation {
            heading_deg: 0.0,
            target_heading_deg: 2.0,
        };
        let env = Environment::default();
        nav.tick(1.0, &env);
        assert_eq!(nav.heading_deg, 2.0);
    }

    #[test]
    fn publishes_namespaced_telemetry() {
        let nav = Navigation::default();
        let names: Vec<_> = nav.sample().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"sys.navigation.heading_deg".to_string()));
    }
}
