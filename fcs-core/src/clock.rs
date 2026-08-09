//! A fixed-step deterministic clock. Wall-clock time never enters the simulation;
//! advancing the clock is the only way ship time moves.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clock {
    dt: f64,
    tick_count: u64,
    elapsed: f64,
}

impl Clock {
    pub fn new(dt: f64) -> Self {
        Self {
            dt,
            tick_count: 0,
            elapsed: 0.0,
        }
    }

    pub fn dt(&self) -> f64 {
        self.dt
    }

    pub fn tick_count(&self) -> u64 {
        self.tick_count
    }

    pub fn elapsed(&self) -> f64 {
        self.elapsed
    }

    pub fn tick(&mut self) {
        self.tick_count += 1;
        self.elapsed += self.dt;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_deterministically_with_fixed_step() {
        let mut clock = Clock::new(0.5);
        assert_eq!(clock.tick_count(), 0);
        assert_eq!(clock.elapsed(), 0.0);

        clock.tick();
        clock.tick();
        clock.tick();

        assert_eq!(clock.tick_count(), 3);
        assert!((clock.elapsed() - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn two_clocks_given_the_same_step_count_end_up_identical() {
        let mut a = Clock::new(0.25);
        let mut b = Clock::new(0.25);
        for _ in 0..10 {
            a.tick();
            b.tick();
        }
        assert_eq!(a, b);
    }
}
