//! The world model: environment state plus a deterministic event queue.
//! World advancement never depends on wall-clock time, only on `dt`.

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Environment {
    pub ambient_temp_k: f64,
    pub radiation_rate: f64,
}

impl Default for Environment {
    fn default() -> Self {
        Self {
            ambient_temp_k: 2.7, // deep-space background temperature
            radiation_rate: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldEvent {
    RadiationSpike { magnitude_milli: u64 },
}

#[derive(Debug, Clone, Default)]
pub struct World {
    pub env: Environment,
    events: VecDeque<WorldEvent>,
}

impl World {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_event(&mut self, event: WorldEvent) {
        self.events.push_back(event);
    }

    /// Advances the world by one tick, draining and applying queued events in
    /// FIFO order so behavior is reproducible for a fixed sequence of pushes.
    pub fn tick(&mut self, _dt: f64) {
        while let Some(event) = self.events.pop_front() {
            match event {
                WorldEvent::RadiationSpike { magnitude_milli } => {
                    self.env.radiation_rate += magnitude_milli as f64 / 1000.0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticking_with_no_events_leaves_environment_unchanged() {
        let mut world = World::new();
        let before = world.env;
        world.tick(1.0);
        assert_eq!(world.env, before);
    }

    #[test]
    fn queued_events_apply_deterministically_in_order() {
        let mut world = World::new();
        world.push_event(WorldEvent::RadiationSpike { magnitude_milli: 500 });
        world.push_event(WorldEvent::RadiationSpike { magnitude_milli: 250 });

        world.tick(1.0);

        assert!((world.env.radiation_rate - 0.75).abs() < f64::EPSILON);
    }
}
