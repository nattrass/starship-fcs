# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
cargo run -p fcs -- run deep-space          # no actors; 20 fixed 1s ticks, one report line each
cargo run -p fcs -- run crewed-deep-space   # same flight with a mock mind and crew member aboard
cargo test --workspace                      # full suite (all tests are colocated unit tests)
cargo clippy --workspace --all-targets      # expected to be warning-clean
```

Run a single test or a module's tests by path — tests live inside the crate, so filter on the module:

```sh
cargo test -p fcs-core no_llm_survival_recovers_from_reactor_overtemp_without_any_actor
cargo test -p fcs-core protocol::tests::
```

## Working from the plan

[plans/001-incremental-development-plan.md](plans/001-incremental-development-plan.md) drives the
build. Its **Progress** section is the single source of truth for how far things have got — "do the
next step" means the next unchecked item there.

The rest of that document is the stable reference plan and should not be rewritten as work lands.
After finishing a step: tick its checkbox and rewrite **Current stage** to describe what now exists
and what comes next. Each step must leave the workspace compiling and the suite green.

## Architecture

[README.md](README.md) carries the full picture. The essentials for changing code:

Every tick runs one fixed sequence in [`Ship::tick`](fcs-core/src/ship.rs):

```
clock ──▶ world ──▶ subsystems ──▶ telemetry ──▶ FDIR ──▶ actors ──▶ autopilot ──▶ safety kernel ──▶ apply ──▶ recorder
```

Two seams carry the whole design, and most review comments come back to one of them:

- **Telemetry is the only reality.** Nothing above [telemetry](fcs-core/src/telemetry.rs) reads the
  world or subsystem state directly — not FDIR, not actors. The sampler can spoof a channel or mark
  it a dropout without writing back to the state that produced it, which is what makes "the mind
  behaves correctly under false senses" testable.
- **The kernel is the only path to state.** Every command, autopilot's included, goes through
  [`SafetyKernel::review`](fcs-core/src/safety/mod.rs): schema → authorization → interlocks →
  autonomy gate. `apply()` is deliberately separate from `commands()` on the `Subsystem` trait so
  nothing but a post-review caller invokes it. Interlocks are pure predicates that run identically
  at every autonomy level and for every role; nothing overrides them.

The LLM seam is narrow on purpose. A [provider](fcs-core/src/provider/mod.rs) is text-in/text-out and
never sees a `Command`, a snapshot, or mutable state; its raw output only becomes a proposal by
passing the strict `SAY:`/`DO:` grammar in [protocol](fcs-core/src/protocol.rs), which drops anything
malformed rather than salvaging it. The role is supplied by the caller rather than read from the
wire, `physical_key` has no grammar at all, and non-finite args are rejected there — a `NaN` would
otherwise pass the kernel's range check, since every `NaN` comparison is false. A failing provider
becomes a watchdog `TurnFailure` and the tick falls back to the autopilot.

[Actors](fcs-core/src/actors.rs) sit on that seam: `ShipMind` and `CrewAgent` render a `Perception`
to plain text, hand it to their own provider instance, and parse the reply back through `protocol`
and nothing else. Two invariants there are load-bearing and easy to break by accident:

- **Every actor in a tick gets the same `Perception`**, built once before any of them speaks, and
  their speech only joins the dialogue after the last one has. Building it per-actor, or folding
  speech in as you go, would make boarding order change what actors see and break replay.
- **What an actor is shown is a window, not a transcript** (`MAX_RECENT_DIALOGUE`,
  `MAX_RECENT_EVENTS`). An unbounded history would make behavior depend on flight length.

The prompt's command vocabulary comes from `actors::command_contract()`, which reads the subsystems'
own `commands()` schemas — don't hand-write a verb or a range into a prompt.

Physical limits (`THERMAL_CEILING_K`, `MIN_SAFE_O2_LEVEL`, `MIN_USABLE_SIGNAL_STRENGTH`, …) are
declared as `pub const` next to the subsystem that owns them, and FDIR, the interlocks, the autopilot
and the mock provider all import from there. Don't restate a threshold at its point of use.

## Constraints

These are load-bearing, not preferences:

- **Zero external crates.** `[dependencies]` is empty and `Cargo.lock` contains only the two
  workspace members. Later online provider adapters are the sole exception and must be
  `optional = true` behind an `online` feature.
- **No `HashMap`/`HashSet` on any decision or recording path** — `BTreeMap`/`BTreeSet`/`Vec` only, so
  iteration order is stable and replay is bit-for-bit.
- **No wall-clock time, no randomness** anywhere in the core. Determinism is asserted directly: tests
  run a scenario twice and compare.
- `#![forbid(unsafe_code)]` at every crate root; errors surface via `Result`; no `unwrap()` on the
  control path (test code aside).
- Actors only ever *propose*. Any change that lets a proposal reach subsystem state without a kernel
  verdict is wrong regardless of how convenient it is.

## Conventions

- Every module opens with a `//!` doc comment explaining *why* the module exists and what property it
  guarantees — not what its functions do. Match that when adding one.
- Tests are colocated in a `#[cfg(test)] mod tests` at the bottom of the module, named as full
  sentences stating the property under test
  (`a_dropped_out_channel_is_reported_as_lost_rather_than_a_safe_zero`). Safety-critical behavior
  gets a test that names the doctrine it defends.
- Prefer adding a `Vec` of typed reasons over a boolean: `Verdict`/`VetoReason`, `DropReason`,
  `TurnFailure` all exist so a refusal is auditable in the recorder rather than silent.
