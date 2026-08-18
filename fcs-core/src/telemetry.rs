//! The perception seam: the only view of ship state any actor is ever allowed to see.
//!
//! No actor may read `world` or subsystem state directly. Everything is funneled
//! through a [`TelemetrySampler`], which can also spoof a channel to an arbitrary
//! value or mark it as a dropout, without ever touching the underlying state that
//! produced the raw reading.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelStatus {
    Nominal,
    Spoofed,
    Dropout,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Channel {
    pub value: f64,
    pub status: ChannelStatus,
}

/// A single named reading sampled from the world or a subsystem, before any
/// spoof/dropout override is applied.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSample {
    pub name: String,
    pub value: f64,
}

/// A flat, namespaced snapshot of ship state (`env.*`, `sys.<subsystem>.*`) for one tick.
#[derive(Debug, Clone, Default)]
pub struct TelemetrySnapshot {
    pub tick_count: u64,
    pub elapsed: f64,
    channels: BTreeMap<String, Channel>,
}

impl TelemetrySnapshot {
    pub fn get(&self, name: &str) -> Option<&Channel> {
        self.channels.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Channel)> {
        self.channels.iter()
    }

    /// Renders the snapshot as one line: tick, elapsed time, then every
    /// channel in the snapshot's stable order, with `*` marking a spoofed
    /// channel and `!` a dropout.
    ///
    /// This is the *only* rendering of ship state that leaves the core. The
    /// CLI's per-tick report and the text an actor's provider is shown are
    /// the same line, produced here, so an actor can never be shown a
    /// reality the flight report would not — markers included, since whether
    /// a channel is trustworthy is part of the perception, not a debugging
    /// aid bolted onto it.
    pub fn report_line(&self) -> String {
        let mut parts = vec![
            format!("tick={}", self.tick_count),
            format!("t={:.2}s", self.elapsed),
        ];
        for (name, channel) in self.iter() {
            let marker = match channel.status {
                ChannelStatus::Nominal => "",
                ChannelStatus::Spoofed => "*",
                ChannelStatus::Dropout => "!",
            };
            parts.push(format!("{name}={:.3}{marker}", channel.value));
        }
        parts.join(" ")
    }
}

/// Samples raw readings into a [`TelemetrySnapshot`], applying any configured
/// spoof or dropout overrides. Overrides live only here — they never write back
/// to the world or subsystem state that produced the raw reading.
#[derive(Debug, Clone, Default)]
pub struct TelemetrySampler {
    spoofed: BTreeMap<String, f64>,
    dropped_out: BTreeSet<String>,
}

impl TelemetrySampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forces `channel` to report `value` regardless of what is sampled.
    pub fn spoof(&mut self, channel: impl Into<String>, value: f64) {
        let channel = channel.into();
        self.dropped_out.remove(&channel);
        self.spoofed.insert(channel, value);
    }

    /// Marks `channel` as lost/stale; it resolves in the snapshot with [`ChannelStatus::Dropout`].
    pub fn drop_out(&mut self, channel: impl Into<String>) {
        let channel = channel.into();
        self.spoofed.remove(&channel);
        self.dropped_out.insert(channel);
    }

    /// Removes any spoof or dropout override on `channel`, restoring nominal sampling.
    pub fn clear(&mut self, channel: &str) {
        self.spoofed.remove(channel);
        self.dropped_out.remove(channel);
    }

    pub fn sample(&self, tick_count: u64, elapsed: f64, raw: Vec<RawSample>) -> TelemetrySnapshot {
        let mut channels = BTreeMap::new();
        for RawSample { name, value } in raw {
            let channel = if self.dropped_out.contains(&name) {
                Channel {
                    value: 0.0,
                    status: ChannelStatus::Dropout,
                }
            } else if let Some(&spoofed_value) = self.spoofed.get(&name) {
                Channel {
                    value: spoofed_value,
                    status: ChannelStatus::Spoofed,
                }
            } else {
                Channel {
                    value,
                    status: ChannelStatus::Nominal,
                }
            };
            channels.insert(name, channel);
        }
        TelemetrySnapshot {
            tick_count,
            elapsed,
            channels,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nominal_channel_reports_the_sampled_value() {
        let sampler = TelemetrySampler::new();
        let raw = vec![RawSample {
            name: "sys.reactor.core_temp_k".into(),
            value: 300.0,
        }];
        let snapshot = sampler.sample(1, 1.0, raw);
        let channel = snapshot.get("sys.reactor.core_temp_k").unwrap();
        assert_eq!(channel.value, 300.0);
        assert_eq!(channel.status, ChannelStatus::Nominal);
    }

    #[test]
    fn spoofed_channel_overrides_the_sampled_value() {
        let mut sampler = TelemetrySampler::new();
        sampler.spoof("sys.reactor.core_temp_k", 9999.0);
        let raw = vec![RawSample {
            name: "sys.reactor.core_temp_k".into(),
            value: 300.0,
        }];
        let snapshot = sampler.sample(1, 1.0, raw);
        let channel = snapshot.get("sys.reactor.core_temp_k").unwrap();
        assert_eq!(channel.value, 9999.0);
        assert_eq!(channel.status, ChannelStatus::Spoofed);
    }

    #[test]
    fn dropped_out_channel_is_reported_as_lost() {
        let mut sampler = TelemetrySampler::new();
        sampler.drop_out("sys.comms.signal_strength");
        let raw = vec![RawSample {
            name: "sys.comms.signal_strength".into(),
            value: 0.8,
        }];
        let snapshot = sampler.sample(1, 1.0, raw);
        let channel = snapshot.get("sys.comms.signal_strength").unwrap();
        assert_eq!(channel.status, ChannelStatus::Dropout);
    }

    #[test]
    fn overrides_never_mutate_the_raw_sample_that_produced_them() {
        let mut sampler = TelemetrySampler::new();
        sampler.spoof("env.ambient_temp_k", 500.0);
        let raw = vec![RawSample {
            name: "env.ambient_temp_k".into(),
            value: 2.7,
        }];
        assert_eq!(raw[0].value, 2.7);
        let snapshot = sampler.sample(1, 1.0, raw);
        assert_eq!(snapshot.get("env.ambient_temp_k").unwrap().value, 500.0);
    }

    #[test]
    fn a_report_line_carries_every_channel_with_its_spoof_and_dropout_markers() {
        let mut sampler = TelemetrySampler::new();
        sampler.spoof("sys.reactor.core_temp_k", 1150.0);
        sampler.drop_out("sys.comms.signal_strength");
        let raw = vec![
            RawSample {
                name: "sys.reactor.core_temp_k".into(),
                value: 300.0,
            },
            RawSample {
                name: "sys.comms.signal_strength".into(),
                value: 0.8,
            },
        ];

        let line = sampler.sample(3, 3.0, raw).report_line();

        assert_eq!(
            line,
            "tick=3 t=3.00s sys.comms.signal_strength=0.000! sys.reactor.core_temp_k=1150.000*"
        );
    }

    #[test]
    fn clearing_an_override_restores_nominal_sampling() {
        let mut sampler = TelemetrySampler::new();
        sampler.drop_out("sys.comms.signal_strength");
        sampler.clear("sys.comms.signal_strength");
        let raw = vec![RawSample {
            name: "sys.comms.signal_strength".into(),
            value: 0.8,
        }];
        let snapshot = sampler.sample(1, 1.0, raw);
        let channel = snapshot.get("sys.comms.signal_strength").unwrap();
        assert_eq!(channel.value, 0.8);
        assert_eq!(channel.status, ChannelStatus::Nominal);
    }
}
