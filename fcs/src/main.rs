#![forbid(unsafe_code)]

use std::env;
use std::process::ExitCode;

use fcs_core::actors::{CrewAgent, ShipMind};
use fcs_core::command::Role;
use fcs_core::protocol::target_to_wire;
use fcs_core::provider::MockProvider;
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

/// A ship plus the world events to inject, by the tick they land on. Events
/// are scheduled rather than pushed as the run goes, so a scenario stays a
/// fixed, replayable description of a flight rather than a script.
struct Scenario {
    ship: Ship,
    events: Vec<(u32, WorldEvent)>,
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    match args.as_slice() {
        [cmd, scenario] if cmd == "run" => run_scenario(scenario),
        _ => {
            eprintln!("usage: fcs run <scenario>");
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

    fly(scenario);
    ExitCode::SUCCESS
}

/// The baseline: no actors, no providers, nothing to go wrong that the
/// autopilot cannot handle alone.
fn deep_space() -> Scenario {
    Scenario {
        ship: Ship::new(DEFAULT_DT),
        events: Vec::new(),
    }
}

/// The same flight with a mind and a crew member aboard, each on its own
/// provider instance, and a radiation spike partway through to give them
/// something to say. Autonomy is `Autonomous` so their proposals can actually
/// execute once the kernel has cleared them — which is the whole point of
/// watching this one run.
fn crewed_deep_space() -> Scenario {
    let mut ship = Ship::new(DEFAULT_DT);
    ship.autonomy = AutonomyLevel::Autonomous;
    ship.board(Box::new(ShipMind::new(Box::new(MockProvider::new(
        Role::ShipMind,
    )))));
    ship.board(Box::new(CrewAgent::new(
        "engineer",
        "You mind the link home and speak up the moment it degrades.",
        Box::new(MockProvider::new(Role::CrewAgent)),
    )));

    Scenario {
        ship,
        events: vec![(
            SPIKE_TICK,
            WorldEvent::RadiationSpike {
                magnitude_milli: SPIKE_MAGNITUDE_MILLI,
            },
        )],
    }
}

fn fly(mut scenario: Scenario) {
    for tick in 1..=DEFAULT_TICKS {
        for (at, event) in &scenario.events {
            if *at == tick {
                scenario.ship.world.push_event(event.clone());
            }
        }

        let snapshot = scenario.ship.tick();
        println!("{}", format_report(&snapshot));
        report_turns(&scenario.ship);
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
