#![forbid(unsafe_code)]

use std::env;
use std::process::ExitCode;

use fcs_core::config::{ActorSpec, ProviderKind, ProviderSpec, ShipConfig};
use fcs_core::protocol::target_to_wire;
use fcs_core::safety::AutonomyLevel;
use fcs_core::ship::{format_report, Ship};
use fcs_core::world::WorldEvent;

const DEFAULT_TICKS: u32 = 20;
const DEFAULT_DT: f64 = 1.0;
const KNOWN_SCENARIOS: &str = "deep-space, crewed-deep-space";

/// The tick the crewed scenario's radiation spike lands on, and how hard it
/// hits. Enough interference to drive the link below its usable threshold, so
/// there is something real for the crew to notice and the kernel to rule on.
const SPIKE_TICK: u32 = 5;
const SPIKE_MAGNITUDE_MILLI: u64 = 4000;

/// A flight: who is aboard and what the world does to them, by the tick it
/// does it on. Events are scheduled rather than pushed as the run goes, so a
/// scenario stays a fixed, replayable description rather than a script.
struct Scenario {
    config: ShipConfig,
    events: Vec<(u32, WorldEvent)>,
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    match args.as_slice() {
        [cmd, scenario] if cmd == "run" => run_scenario(scenario),
        _ => {
            eprintln!("usage: fcs run <scenario>");
            eprintln!("scenarios: {KNOWN_SCENARIOS}");
            eprintln!(
                "environment: FCS_PROVIDER={} FCS_MODEL=<model id>",
                provider_names().join("|")
            );
            ExitCode::FAILURE
        }
    }
}

fn run_scenario(name: &str) -> ExitCode {
    let scenario = match name {
        "deep-space" => deep_space(),
        "crewed-deep-space" => crewed_deep_space(),
        _ => {
            eprintln!("unknown scenario: {name} (known scenarios: {KNOWN_SCENARIOS})");
            return ExitCode::FAILURE;
        }
    };

    let config = match apply_overrides(scenario.config) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    // A provider that cannot be constructed is reported here, before the
    // flight starts, rather than as a failed turn twenty ticks in.
    let ship = match config.build() {
        Ok(ship) => ship,
        Err(error) => {
            eprintln!("cannot build the ship: {error}");
            return ExitCode::FAILURE;
        }
    };

    fly(ship, &scenario.events);
    ExitCode::SUCCESS
}

/// The baseline: nobody aboard, nothing to go wrong that the autopilot cannot
/// handle alone.
fn deep_space() -> Scenario {
    Scenario {
        config: ShipConfig::new(DEFAULT_DT),
        events: Vec::new(),
    }
}

/// The same flight with a mind and a crew member aboard, each on its own
/// provider instance, and a radiation spike partway through to give them
/// something to say. Autonomy is `Autonomous` so their proposals can actually
/// execute once the kernel has cleared them — which is the point of watching
/// this one run.
fn crewed_deep_space() -> Scenario {
    Scenario {
        config: ShipConfig::new(DEFAULT_DT)
            .with_autonomy(AutonomyLevel::Autonomous)
            .with_actor(ActorSpec::ship_mind(ProviderSpec::mock()))
            .with_actor(ActorSpec::crew(
                "engineer",
                "You mind the link home and speak up the moment it degrades.",
                ProviderSpec::mock(),
            )),
        events: vec![(
            SPIKE_TICK,
            WorldEvent::RadiationSpike {
                magnitude_milli: SPIKE_MAGNITUDE_MILLI,
            },
        )],
    }
}

/// `FCS_PROVIDER` re-backs every actor in the scenario; `FCS_MODEL` names the
/// model they run on. That is the whole configuration surface, and it makes
/// the point the config layer exists to make: putting a different model
/// behind the same ship is an environment variable, not a code change, and it
/// touches neither the kernel nor the loop.
fn apply_overrides(mut config: ShipConfig) -> Result<ShipConfig, String> {
    if let Ok(name) = env::var("FCS_PROVIDER") {
        let name = name.trim();
        let kind = ProviderKind::from_name(name).ok_or_else(|| {
            format!(
                "unknown provider: {name} (known providers: {})",
                provider_names().join(", ")
            )
        })?;
        config = config.with_provider_kind(kind);
    }

    if let Ok(model) = env::var("FCS_MODEL") {
        config = config.with_model(model);
    }

    Ok(config)
}

fn provider_names() -> Vec<&'static str> {
    ProviderKind::ALL.iter().map(|kind| kind.name()).collect()
}

fn fly(mut ship: Ship, events: &[(u32, WorldEvent)]) {
    for (name, provider) in ship.manifest() {
        println!("# aboard: {name} [{provider}]");
    }

    for tick in 1..=DEFAULT_TICKS {
        for (at, event) in events {
            if *at == tick {
                ship.world.push_event(event.clone());
            }
        }

        let snapshot = ship.tick();
        println!("{}", format_report(&snapshot));
        report_turns(&ship);
    }
}

/// Prints what the actors said and what the kernel made of what they
/// proposed, indented under the tick's report line. A ship with nobody aboard
/// and nothing to rule on prints nothing extra.
fn report_turns(ship: &Ship) {
    let Some(record) = ship.recorder.records().last() else {
        return;
    };

    for turn in &record.actor_turns {
        match &turn.failure {
            Some(failure) => println!("  {} [{}] failed: {failure:?}", turn.actor, turn.provider),
            None => {
                for line in &turn.speech {
                    println!("  {} [{}]: {line}", turn.actor, turn.provider);
                }
            }
        }
    }

    for outcome in &record.command_outcomes {
        let applied = if outcome.applied { " (applied)" } else { "" };
        println!(
            "  -> {:?}: {} {} {:?}{applied}",
            outcome.command.source,
            outcome.command.verb,
            target_to_wire(outcome.command.target),
            outcome.verdict,
        );
    }
}
