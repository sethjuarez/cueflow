use std::{env, fs, process::ExitCode};

use cueflow_adapters::{CurrentPlatformAdapter, current_platform};
use cueflow_core::{RunConfig, parse_definition_json};
use cueflow_executor::{AutomationExecutor, RunControl, RunEventSink, RunOutcome, SystemClock};

struct JsonlSink;

impl RunEventSink for JsonlSink {
    fn emit(&mut self, event: &cueflow_core::RunEvent) {
        println!(
            "{}",
            serde_json::to_string(event).expect("run events serialize")
        );
    }
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        return usage();
    };
    let Some(path) = args.next() else {
        return usage();
    };
    if args.next().is_some() {
        return usage();
    }

    let input = match fs::read_to_string(path) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("failed to read automation: {error}");
            return ExitCode::FAILURE;
        }
    };
    let definition = match parse_definition_json(&input) {
        Ok(definition) => definition,
        Err(error) => {
            eprintln!("invalid automation: {error}");
            return ExitCode::FAILURE;
        }
    };

    if command == "validate" {
        println!("valid: {}", definition.id);
        return ExitCode::SUCCESS;
    }
    if command != "dry-run" && command != "run" {
        return usage();
    }

    let executor = AutomationExecutor::new();
    let mut adapter = CurrentPlatformAdapter;
    let mut sink = JsonlSink;
    let control = RunControl::default();
    let clock = SystemClock::default();
    let config = RunConfig {
        dry_run: command == "dry-run",
        platform: Some(current_platform()),
        ..RunConfig::default()
    };
    match executor.run_with(
        &definition,
        config,
        &mut adapter,
        &control,
        &mut sink,
        &clock,
    ) {
        Ok(report) if report.outcome == RunOutcome::Succeeded => ExitCode::SUCCESS,
        Ok(_) | Err(_) => ExitCode::FAILURE,
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: cueflow <validate|dry-run|run> <automation.json>");
    ExitCode::from(2)
}
