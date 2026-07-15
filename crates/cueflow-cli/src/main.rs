use std::{collections::BTreeMap, env, fs, path::PathBuf, process::ExitCode};

use cueflow_adapters::{CurrentPlatformAdapter, current_platform};
use cueflow_core::{Artifact, ArtifactKind, RunConfig, Target, parse_definition_json};
use cueflow_executor::{
    AutomationExecutor, RunControl, RunEventSink, RunOutcome, RunReport, SystemClock,
};

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

    if command == "capabilities" {
        if args.next().is_some() {
            return usage();
        }
        let capabilities = CurrentPlatformAdapter::capabilities();
        println!(
            "{}",
            serde_json::json!({
                "platform": capabilities.platform,
                "supportsLaunch": capabilities.supports_launch,
                "supportsFocus": capabilities.supports_focus,
                "supportsInput": capabilities.supports_input,
                "supportsSemanticTargets": capabilities.supports_semantic_targets,
                "supportsCoordinateTargets": capabilities.supports_coordinate_targets,
                "supportsWindowQueries": capabilities.supports_window_queries,
                "supportsProcessQueries": capabilities.supports_process_queries,
                "supportsAccessibilityTree": capabilities.supports_accessibility_tree,
            })
        );
        return ExitCode::SUCCESS;
    }

    if command == "inspect-window" {
        let Some((target, max_depth, max_nodes, include_values, output)) =
            parse_inspect_window_args(args)
        else {
            return usage();
        };
        let adapter = CurrentPlatformAdapter::new();
        match adapter.inspect_window_with_options(&target, max_depth, max_nodes, include_values) {
            Ok(tree) => {
                let tree_json =
                    serde_json::to_string_pretty(&tree).expect("accessibility tree serializes");
                if let Some(output) = output {
                    if let Err(error) = fs::write(&output, &tree_json) {
                        eprintln!("failed to write accessibility tree artifact: {error}");
                        return ExitCode::FAILURE;
                    }
                    println!(
                        "{}",
                        serde_json::to_string(&Artifact {
                            kind: ArtifactKind::AccessibilityTree,
                            uri: artifact_uri(output),
                            label: Some(format!("Accessibility tree: {}", tree.window_title)),
                        })
                        .expect("artifact serializes")
                    );
                } else {
                    println!("{tree_json}");
                }
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("inspect-window failed: {error}");
                if let Some(source) = error.diagnostics() {
                    eprintln!("{source}");
                }
                return ExitCode::FAILURE;
            }
        }
    }

    if command == "screenshot" {
        let Some(output) = parse_screenshot_args(args) else {
            return usage();
        };
        let adapter = CurrentPlatformAdapter::new();
        match adapter.capture_screenshot(&output) {
            Ok(artifact) => {
                println!(
                    "{}",
                    serde_json::to_string(&artifact).expect("screenshot artifact serializes")
                );
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("screenshot failed: {error}");
                if let Some(source) = error.diagnostics() {
                    eprintln!("{source}");
                }
                return ExitCode::FAILURE;
            }
        }
    }

    let Some(options) = parse_automation_args(command.as_str(), args) else {
        return usage();
    };

    let input = match fs::read_to_string(&options.path) {
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
    if command != "preflight" && command != "dry-run" && command != "run" {
        return usage();
    }

    let executor = AutomationExecutor::new();
    let mut adapter = CurrentPlatformAdapter::new();
    let mut sink = JsonlSink;
    let control = RunControl::default();
    let clock = SystemClock::default();
    let config = RunConfig {
        dry_run: command == "dry-run",
        platform: Some(current_platform()),
        allow_coordinate_targets: options.allow_coordinate_targets,
        allow_path_only_selectors: options.allow_path_only_selectors,
        allow_value_capture: options.allow_value_capture,
        ..RunConfig::default()
    };

    if command == "preflight" {
        match executor.preflight(&definition, &config, &adapter) {
            Ok(report) => {
                let can_run = report.can_run();
                println!(
                    "{}",
                    serde_json::json!({
                        "canRun": can_run,
                        "diagnostics": report.diagnostics,
                    })
                );
                return if can_run {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                };
            }
            Err(error) => {
                eprintln!("preflight failed: {error}");
                return ExitCode::FAILURE;
            }
        }
    }

    let report = executor.run_with(
        &definition,
        config,
        &mut adapter,
        &control,
        &mut sink,
        &clock,
    );
    match report {
        Ok(report) => {
            if let Some(evidence_dir) = options.evidence_dir
                && let Err(error) = write_evidence_bundle(&evidence_dir, &definition.id, &report)
            {
                eprintln!("failed to write evidence bundle: {error}");
                return ExitCode::FAILURE;
            }
            if report.outcome == RunOutcome::Succeeded {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(_) => ExitCode::FAILURE,
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: cueflow capabilities | cueflow inspect-window (--title-contains <text>|--window-title <text>) [--max-depth <n>] [--max-nodes <n>] [--include-values] [--output <path>] | cueflow screenshot --output <path> | cueflow <validate|preflight|dry-run|run> [--evidence-dir <dir>] [--allow-coordinate-targets] [--allow-path-only-selectors] [--allow-value-capture] <automation.json>"
    );
    ExitCode::from(2)
}

fn parse_screenshot_args(mut args: impl Iterator<Item = String>) -> Option<PathBuf> {
    let mut output = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => output = Some(PathBuf::from(args.next()?)),
            _ => return None,
        }
    }
    output
}

fn parse_automation_args(
    command: &str,
    mut args: impl Iterator<Item = String>,
) -> Option<AutomationCliOptions> {
    let mut options = AutomationCliOptions::default();
    let mut evidence_dir = None;
    let mut path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--evidence-dir" if command == "run" || command == "dry-run" => {
                evidence_dir = Some(PathBuf::from(args.next()?));
            }
            "--allow-coordinate-targets" => options.allow_coordinate_targets = true,
            "--allow-path-only-selectors" => options.allow_path_only_selectors = true,
            "--allow-value-capture" => options.allow_value_capture = true,
            _ if path.is_none() => path = Some(arg),
            _ => return None,
        }
    }
    options.path = path?;
    options.evidence_dir = evidence_dir;
    Some(options)
}

#[derive(Default)]
struct AutomationCliOptions {
    path: String,
    evidence_dir: Option<PathBuf>,
    allow_coordinate_targets: bool,
    allow_path_only_selectors: bool,
    allow_value_capture: bool,
}

fn parse_inspect_window_args(
    mut args: impl Iterator<Item = String>,
) -> Option<(Target, u32, usize, bool, Option<PathBuf>)> {
    let mut window_title = None;
    let mut title_contains = None;
    let mut max_depth = 4;
    let mut max_nodes = 250;
    let mut include_values = false;
    let mut output = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--window-title" => window_title = Some(args.next()?),
            "--title-contains" => title_contains = Some(args.next()?),
            "--max-depth" => max_depth = args.next()?.parse().ok()?,
            "--max-nodes" => max_nodes = args.next()?.parse().ok()?,
            "--include-values" => include_values = true,
            "--output" => output = Some(PathBuf::from(args.next()?)),
            _ => return None,
        }
    }

    if window_title.is_none() && title_contains.is_none() {
        return None;
    }

    Some((
        Target {
            app_name: None,
            process_name: None,
            window_title,
            title_contains,
            url: None,
            file_path: None,
            accessibility: None,
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        },
        max_depth,
        max_nodes,
        include_values,
        output,
    ))
}

fn artifact_uri(path: PathBuf) -> String {
    let absolute = path.canonicalize().unwrap_or(path);
    let path = absolute.display().to_string();
    format!("file://{}", path.strip_prefix(r"\\?\").unwrap_or(&path))
}

fn write_evidence_bundle(
    directory: &PathBuf,
    automation_id: &str,
    report: &RunReport,
) -> std::io::Result<()> {
    fs::create_dir_all(directory)?;
    let events_path = directory.join("events.jsonl");
    let mut events = String::new();
    for event in &report.events {
        events.push_str(&serde_json::to_string(event).expect("run event serializes"));
        events.push('\n');
    }
    fs::write(&events_path, events)?;

    let summary_path = directory.join("summary.json");
    fs::write(
        summary_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "automationId": automation_id,
            "runId": report.run_id,
            "outcome": match report.outcome {
                RunOutcome::Succeeded => "succeeded",
                RunOutcome::Failed => "failed",
                RunOutcome::Cancelled => "cancelled",
            },
            "events": artifact_uri(events_path),
        }))
        .expect("evidence summary serializes"),
    )?;
    Ok(())
}
