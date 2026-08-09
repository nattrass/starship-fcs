#![forbid(unsafe_code)]

use std::env;
use std::process::ExitCode;

use fcs_core::ship::{format_report, Ship};

const DEFAULT_TICKS: u32 = 20;
const DEFAULT_DT: f64 = 1.0;

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

fn run_scenario(scenario: &str) -> ExitCode {
    if scenario != "deep-space" {
        eprintln!("unknown scenario: {scenario} (known scenarios: deep-space)");
        return ExitCode::FAILURE;
    }

    let mut ship = Ship::new(DEFAULT_DT);
    for _ in 0..DEFAULT_TICKS {
        let snapshot = ship.tick();
        println!("{}", format_report(&snapshot));
    }

    ExitCode::SUCCESS
}
