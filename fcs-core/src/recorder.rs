//! The append-only flight recorder: one `TickRecord` per tick, capturing a
//! telemetry digest, the operating mode, detected faults, and every command
//! the kernel reviewed that tick (with its verdict and whether it was
//! actually applied). Nothing here is ever removed or rewritten, only
//! appended, so the log is a faithful, replayable, auditable account of a run.

use std::collections::BTreeSet;

use crate::command::Command;
use crate::fdir::{Fault, OperatingMode};
use crate::safety::Verdict;
use crate::telemetry::TelemetrySnapshot;

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn fnv1a_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// A compact, deterministic fingerprint of a telemetry snapshot. Computed
/// from the channels in their stable (`BTreeMap`) order and from each
/// value's raw bits (never a formatted string), so two snapshots with
/// identical data always digest to the same value bit-for-bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetryDigest(pub u64);

pub fn telemetry_digest(snapshot: &TelemetrySnapshot) -> TelemetryDigest {
    let mut hash = FNV_OFFSET_BASIS;
    for (name, channel) in snapshot.iter() {
        hash = fnv1a_update(hash, name.as_bytes());
        hash = fnv1a_update(hash, &channel.value.to_bits().to_le_bytes());
        hash = fnv1a_update(hash, &[channel.status as u8]);
    }
    TelemetryDigest(hash)
}

/// One command the kernel reviewed this tick, and what happened to it.
/// `applied` reflects reality — a `Verdict::Approved` command whose `apply`
/// call itself failed is recorded as approved-but-not-applied, never
/// silently rounded up to a clean success.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOutcome {
    pub command: Command,
    pub verdict: Verdict,
    pub applied: bool,
}

/// Everything recorded for a single tick.
#[derive(Debug, Clone, PartialEq)]
pub struct TickRecord {
    pub tick_count: u64,
    pub telemetry_digest: TelemetryDigest,
    pub mode: OperatingMode,
    pub faults: BTreeSet<Fault>,
    pub command_outcomes: Vec<CommandOutcome>,
}

/// An append-only log of `TickRecord`s.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Recorder {
    records: Vec<TickRecord>,
}

impl Recorder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, record: TickRecord) {
        self.records.push(record);
    }

    pub fn records(&self) -> &[TickRecord] {
        &self.records
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{RawSample, TelemetrySampler};

    fn snapshot(value: f64) -> TelemetrySnapshot {
        let sampler = TelemetrySampler::new();
        sampler.sample(
            1,
            1.0,
            vec![RawSample {
                name: "sys.reactor.core_temp_k".into(),
                value,
            }],
        )
    }

    #[test]
    fn digest_is_deterministic_for_identical_snapshots() {
        assert_eq!(telemetry_digest(&snapshot(300.0)), telemetry_digest(&snapshot(300.0)));
    }

    #[test]
    fn digest_differs_for_different_readings() {
        assert_ne!(telemetry_digest(&snapshot(300.0)), telemetry_digest(&snapshot(301.0)));
    }

    #[test]
    fn recorder_is_append_only_and_preserves_order() {
        let mut recorder = Recorder::new();
        recorder.record(TickRecord {
            tick_count: 1,
            telemetry_digest: telemetry_digest(&snapshot(300.0)),
            mode: OperatingMode::Nominal,
            faults: BTreeSet::new(),
            command_outcomes: Vec::new(),
        });
        recorder.record(TickRecord {
            tick_count: 2,
            telemetry_digest: telemetry_digest(&snapshot(301.0)),
            mode: OperatingMode::Nominal,
            faults: BTreeSet::new(),
            command_outcomes: Vec::new(),
        });

        let records = recorder.records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].tick_count, 1);
        assert_eq!(records[1].tick_count, 2);
    }
}
