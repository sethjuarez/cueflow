use std::{collections::BTreeMap, env, fs, path::PathBuf, process::ExitCode};

use cueflow_adapters::{CurrentPlatformAdapter, current_platform};
use cueflow_core::{Artifact, ArtifactKind, RunConfig, Target, parse_definition_json};
use cueflow_executor::{
    AutomationExecutor, PreflightReport, RunControl, RunEventSink, RunOutcome, RunReport,
    SystemClock,
};
use serde::Deserialize;

struct JsonlSink;

impl RunEventSink for JsonlSink {
    fn emit(&mut self, event: &cueflow_core::RunEvent) {
        println!(
            "{}",
            serde_json::to_string(event).expect("run events serialize")
        );
    }
}

struct QuietSink;

impl RunEventSink for QuietSink {
    fn emit(&mut self, _event: &cueflow_core::RunEvent) {}
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

    if command == "repair-selector" {
        let Some((target, max_depth, max_nodes)) = parse_repair_selector_args(args) else {
            return usage();
        };
        let adapter = CurrentPlatformAdapter::new();
        match adapter.repair_selector(&target, max_depth, max_nodes) {
            Ok(report) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).expect("selector repair serializes")
                );
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("repair-selector failed: {error}");
                if let Some(source) = error.diagnostics() {
                    eprintln!("{source}");
                }
                return ExitCode::FAILURE;
            }
        }
    }

    if command == "screenshot" {
        let Some(options) = parse_screenshot_args(args) else {
            return usage();
        };
        let adapter = CurrentPlatformAdapter::new();
        let result = if let Some(target) = options.target {
            adapter.capture_window_screenshot(&target, &options.output)
        } else if options.allow_desktop {
            adapter.capture_screenshot(&options.output)
        } else {
            eprintln!(
                "screenshot failed: desktop screenshot capture requires --allow-desktop-screenshot or a window selector"
            );
            return ExitCode::FAILURE;
        };
        match result {
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

    if command == "run-drills" {
        let Some(manifest_path) = args.next().map(PathBuf::from) else {
            return usage();
        };
        if args.next().is_some() {
            return usage();
        }
        return run_drill_manifest(manifest_path);
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
        capture_step_evidence: options.capture_step_evidence,
        allow_screenshot_capture: options.allow_screenshot_capture,
        allow_image_targets: options.allow_image_targets,
        evidence_max_artifact_bytes: options.evidence_max_artifact_bytes,
        evidence_directory: options
            .evidence_dir
            .as_ref()
            .map(|path| path.display().to_string()),
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

    if command == "run" {
        match executor.preflight(&definition, &config, &adapter) {
            Ok(report) if report.can_run() => {}
            Ok(report) => {
                eprintln!(
                    "run failed: automation preflight failed: {}",
                    preflight_messages(&report)
                );
                return ExitCode::FAILURE;
            }
            Err(error) => {
                eprintln!("run failed: {error}");
                return ExitCode::FAILURE;
            }
        }
    }

    if command == "run"
        && options.prune_evidence_before_run
        && let Some(directory) = &options.evidence_dir
        && let Err(error) = prune_evidence_directory(directory)
    {
        eprintln!("failed to prune evidence directory: {error}");
        return ExitCode::FAILURE;
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
                && let Err(error) = write_evidence_bundle(
                    &evidence_dir,
                    &definition.id,
                    &report,
                    options.evidence_max_artifact_bytes,
                    options.prune_evidence_before_run,
                )
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
        Err(error) => {
            eprintln!("run failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: cueflow capabilities | cueflow inspect-window (--title-contains <text>|--window-title <text>) [--max-depth <n>] [--max-nodes <n>] [--include-values] [--output <path>] | cueflow repair-selector (--title-contains <text>|--window-title <text>) [--id <id>] [--name <name>] [--control-type <type>] [--path <indexes>] [--max-depth <n>] [--max-nodes <n>] | cueflow screenshot --output <path> [(--window-title <text>|--title-contains <text>)|--allow-desktop-screenshot] | cueflow run-drills <manifest.json> | cueflow <validate|preflight|dry-run|run> [--evidence-dir <dir>] [--capture-step-evidence] [--evidence-max-artifact-bytes <bytes>] [--prune-evidence-before-run] [--allow-coordinate-targets] [--allow-path-only-selectors] [--allow-value-capture] [--allow-screenshot-capture] [--allow-image-targets] <automation.json>"
    );
    ExitCode::from(2)
}

fn parse_screenshot_args(mut args: impl Iterator<Item = String>) -> Option<ScreenshotCliOptions> {
    let mut output = None;
    let mut window_title = None;
    let mut title_contains = None;
    let mut allow_desktop = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => output = Some(PathBuf::from(args.next()?)),
            "--window-title" => window_title = Some(args.next()?),
            "--title-contains" => title_contains = Some(args.next()?),
            "--allow-desktop-screenshot" => allow_desktop = true,
            _ => return None,
        }
    }
    if window_title.is_some() && title_contains.is_some() {
        return None;
    }
    Some(ScreenshotCliOptions {
        output: output?,
        target: (window_title.is_some() || title_contains.is_some()).then_some(Target {
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
        }),
        allow_desktop,
    })
}

struct ScreenshotCliOptions {
    output: PathBuf,
    target: Option<Target>,
    allow_desktop: bool,
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
            "--capture-step-evidence" => options.capture_step_evidence = true,
            "--allow-screenshot-capture" => options.allow_screenshot_capture = true,
            "--allow-image-targets" => options.allow_image_targets = true,
            "--evidence-max-artifact-bytes" if command == "run" || command == "dry-run" => {
                options.evidence_max_artifact_bytes = Some(args.next()?.parse().ok()?);
            }
            "--prune-evidence-before-run" if command == "run" => {
                options.prune_evidence_before_run = true;
            }
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
    capture_step_evidence: bool,
    allow_screenshot_capture: bool,
    allow_image_targets: bool,
    evidence_max_artifact_bytes: Option<u64>,
    prune_evidence_before_run: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DrillManifest {
    id: String,
    #[serde(default)]
    evidence_dir: Option<PathBuf>,
    drills: Vec<DrillEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DrillEntry {
    id: String,
    path: PathBuf,
    expected_outcome: DrillExpectedOutcome,
    #[serde(default)]
    allow_coordinate_targets: bool,
    #[serde(default)]
    allow_path_only_selectors: bool,
    #[serde(default)]
    allow_value_capture: bool,
    #[serde(default)]
    capture_step_evidence: bool,
    #[serde(default)]
    allow_screenshot_capture: bool,
    #[serde(default)]
    allow_image_targets: bool,
    #[serde(default)]
    evidence_max_artifact_bytes: Option<u64>,
    #[serde(default)]
    prune_evidence_before_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum DrillExpectedOutcome {
    Succeeded,
    Failed,
    ExecutorError,
}

fn run_drill_manifest(manifest_path: PathBuf) -> ExitCode {
    let manifest_input = match fs::read_to_string(&manifest_path) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("failed to read drill manifest: {error}");
            return ExitCode::FAILURE;
        }
    };
    let manifest: DrillManifest = match serde_json::from_str(&manifest_input) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("invalid drill manifest: {error}");
            return ExitCode::FAILURE;
        }
    };

    let base_dir = manifest_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let executor = AutomationExecutor::new();
    let mut results = Vec::new();
    let mut all_matched = true;

    for drill in &manifest.drills {
        let drill_path = if drill.path.is_absolute() {
            drill.path.clone()
        } else {
            base_dir.join(&drill.path)
        };
        let input = match fs::read_to_string(&drill_path) {
            Ok(input) => input,
            Err(error) => {
                all_matched = false;
                results.push(serde_json::json!({
                    "id": drill.id,
                    "path": drill_path,
                    "expectedOutcome": drill.expected_outcome.as_str(),
                    "actualOutcome": "loadFailed",
                    "matched": false,
                    "error": error.to_string(),
                }));
                continue;
            }
        };
        let definition = match parse_definition_json(&input) {
            Ok(definition) => definition,
            Err(error) => {
                all_matched = false;
                results.push(serde_json::json!({
                    "id": drill.id,
                    "path": drill_path,
                    "expectedOutcome": drill.expected_outcome.as_str(),
                    "actualOutcome": "invalid",
                    "matched": false,
                    "error": error.to_string(),
                }));
                continue;
            }
        };

        let evidence_dir = manifest.evidence_dir.as_ref().map(|dir| {
            if dir.is_absolute() {
                dir.join(&drill.id)
            } else {
                base_dir.join(dir).join(&drill.id)
            }
        });
        let config = RunConfig {
            dry_run: false,
            platform: Some(current_platform()),
            allow_coordinate_targets: drill.allow_coordinate_targets,
            allow_path_only_selectors: drill.allow_path_only_selectors,
            allow_value_capture: drill.allow_value_capture,
            capture_step_evidence: drill.capture_step_evidence,
            allow_screenshot_capture: drill.allow_screenshot_capture,
            allow_image_targets: drill.allow_image_targets,
            evidence_max_artifact_bytes: drill.evidence_max_artifact_bytes,
            evidence_directory: evidence_dir.as_ref().map(|path| path.display().to_string()),
            ..RunConfig::default()
        };

        match executor.preflight(&definition, &config, &CurrentPlatformAdapter::new()) {
            Ok(report) if report.can_run() => {}
            Ok(report) => {
                let matched = matches!(drill.expected_outcome, DrillExpectedOutcome::ExecutorError);
                all_matched &= matched;
                results.push(serde_json::json!({
                    "id": drill.id,
                    "path": drill_path,
                    "expectedOutcome": drill.expected_outcome.as_str(),
                    "actualOutcome": "executorError",
                    "matched": matched,
                    "error": format!("automation preflight failed: {}", preflight_messages(&report)),
                }));
                continue;
            }
            Err(error) => {
                let matched = matches!(drill.expected_outcome, DrillExpectedOutcome::ExecutorError);
                all_matched &= matched;
                results.push(serde_json::json!({
                    "id": drill.id,
                    "path": drill_path,
                    "expectedOutcome": drill.expected_outcome.as_str(),
                    "actualOutcome": "executorError",
                    "matched": matched,
                    "error": error.to_string(),
                }));
                continue;
            }
        }

        if drill.prune_evidence_before_run
            && let Some(directory) = &evidence_dir
            && let Err(error) = prune_evidence_directory(directory)
        {
            all_matched = false;
            results.push(serde_json::json!({
                "id": drill.id,
                "path": drill_path,
                "expectedOutcome": drill.expected_outcome.as_str(),
                "actualOutcome": "invalid",
                "matched": false,
                "error": format!("failed to prune evidence directory: {error}"),
            }));
            continue;
        }

        let mut adapter = CurrentPlatformAdapter::new();
        let control = RunControl::default();
        let mut sink = QuietSink;
        let clock = SystemClock::default();
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
                if let Some(evidence_dir) = evidence_dir.as_ref()
                    && let Err(error) = write_evidence_bundle(
                        evidence_dir,
                        &definition.id,
                        &report,
                        drill.evidence_max_artifact_bytes,
                        drill.prune_evidence_before_run,
                    )
                {
                    all_matched = false;
                    results.push(serde_json::json!({
                        "id": drill.id,
                        "path": drill_path,
                        "expectedOutcome": drill.expected_outcome.as_str(),
                        "actualOutcome": outcome_str(report.outcome),
                        "matched": false,
                        "error": format!("failed to write evidence bundle: {error}"),
                    }));
                    continue;
                }
                let matched = drill.expected_outcome.matches(report.outcome);
                all_matched &= matched;
                results.push(serde_json::json!({
                    "id": drill.id,
                    "path": drill_path,
                    "expectedOutcome": drill.expected_outcome.as_str(),
                    "actualOutcome": outcome_str(report.outcome),
                    "matched": matched,
                    "runId": report.run_id,
                    "eventCount": report.events.len(),
                }));
            }
            Err(error) => {
                let matched = matches!(drill.expected_outcome, DrillExpectedOutcome::ExecutorError);
                all_matched &= matched;
                results.push(serde_json::json!({
                    "id": drill.id,
                    "path": drill_path,
                    "expectedOutcome": drill.expected_outcome.as_str(),
                    "actualOutcome": "executorError",
                    "matched": matched,
                    "error": error.to_string(),
                }));
            }
        }
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "id": manifest.id,
            "matched": all_matched,
            "results": results,
        }))
        .expect("drill results serialize")
    );
    if all_matched {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn preflight_messages(report: &PreflightReport) -> String {
    report
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

impl DrillExpectedOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::ExecutorError => "executorError",
        }
    }

    fn matches(self, outcome: RunOutcome) -> bool {
        matches!(
            (self, outcome),
            (Self::Succeeded, RunOutcome::Succeeded) | (Self::Failed, RunOutcome::Failed)
        )
    }
}

fn outcome_str(outcome: RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Succeeded => "succeeded",
        RunOutcome::Failed => "failed",
        RunOutcome::Cancelled => "cancelled",
    }
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

fn parse_repair_selector_args(
    mut args: impl Iterator<Item = String>,
) -> Option<(Target, u32, usize)> {
    let mut window_title = None;
    let mut title_contains = None;
    let mut id = None;
    let mut name = None;
    let mut control_type = None;
    let mut path = None;
    let mut max_depth = 4;
    let mut max_nodes = 500;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--window-title" => window_title = Some(args.next()?),
            "--title-contains" => title_contains = Some(args.next()?),
            "--id" => id = Some(args.next()?),
            "--name" => name = Some(args.next()?),
            "--control-type" => control_type = Some(args.next()?),
            "--path" => path = Some(parse_path_indexes(&args.next()?)?),
            "--max-depth" => max_depth = args.next()?.parse().ok()?,
            "--max-nodes" => max_nodes = args.next()?.parse().ok()?,
            _ => return None,
        }
    }

    if window_title.is_none() && title_contains.is_none() {
        return None;
    }
    if window_title.is_some() && title_contains.is_some() {
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
            accessibility: Some(cueflow_core::AccessibilityTarget {
                id,
                name,
                control_type,
                path,
            }),
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        },
        max_depth,
        max_nodes,
    ))
}

fn parse_path_indexes(value: &str) -> Option<Vec<u32>> {
    if value.trim().is_empty() || value.trim() == "[]" {
        return Some(Vec::new());
    }
    value
        .trim_matches(['[', ']'])
        .split(',')
        .map(|part| part.trim().parse().ok())
        .collect()
}

fn artifact_uri(path: PathBuf) -> String {
    let absolute = path.canonicalize().unwrap_or(path);
    let path = absolute.display().to_string();
    format!("file://{}", path.strip_prefix(r"\\?\").unwrap_or(&path))
}

fn prune_evidence_directory(directory: &PathBuf) -> std::io::Result<()> {
    let events_path = directory.join("events.jsonl");
    match fs::remove_file(&events_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let summary_path = directory.join("summary.json");
    match fs::remove_file(&summary_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let steps_path = directory.join("steps");
    match fs::remove_dir_all(&steps_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    Ok(())
}

fn write_evidence_bundle(
    directory: &PathBuf,
    automation_id: &str,
    report: &RunReport,
    evidence_max_artifact_bytes: Option<u64>,
    pruned_before_run: bool,
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
    let artifacts = report
        .events
        .iter()
        .filter_map(|event| match event {
            cueflow_core::RunEvent::Artifact {
                step_id, artifact, ..
            } => Some(serde_json::json!({
                "stepId": step_id,
                "kind": artifact.kind,
                "uri": artifact.uri,
                "label": artifact.label,
            })),
            cueflow_core::RunEvent::StepSucceeded {
                step_id, artifacts, ..
            } => Some(serde_json::json!({
                "stepId": step_id,
                "artifacts": artifacts,
            })),
            _ => None,
        })
        .collect::<Vec<_>>();
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
            "artifactCount": artifacts.len(),
            "artifacts": artifacts,
            "retentionPolicy": {
                "evidenceIsLocal": true,
                "prunedBeforeRun": pruned_before_run,
                "valuesCapturedOnlyWhenAllowed": true,
                "desktopScreenshotsExcludedFromStepEvidence": true,
                "defaultMaxArtifactBytes": 26214400u64,
                "effectiveMaxArtifactBytes": evidence_max_artifact_bytes.unwrap_or(26214400)
            },
            "redactionPolicy": {
                "accessibilityValuesOmittedByDefault": true,
                "passwordValuesAlwaysSuppressed": true,
                "screenshotCaptureRequiresExplicitApproval": true,
                "imageTargetsRequireExplicitApproval": true
            }
        }))
        .expect("evidence summary serializes"),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drill_expected_outcomes_match_run_outcomes() {
        assert!(DrillExpectedOutcome::Succeeded.matches(RunOutcome::Succeeded));
        assert!(DrillExpectedOutcome::Failed.matches(RunOutcome::Failed));
        assert!(!DrillExpectedOutcome::Succeeded.matches(RunOutcome::Failed));
        assert!(!DrillExpectedOutcome::Failed.matches(RunOutcome::Succeeded));
        assert!(!DrillExpectedOutcome::Failed.matches(RunOutcome::Cancelled));
        assert!(!DrillExpectedOutcome::ExecutorError.matches(RunOutcome::Failed));
    }

    #[test]
    fn drill_expected_outcomes_have_stable_json_labels() {
        assert_eq!(DrillExpectedOutcome::Succeeded.as_str(), "succeeded");
        assert_eq!(DrillExpectedOutcome::Failed.as_str(), "failed");
        assert_eq!(
            DrillExpectedOutcome::ExecutorError.as_str(),
            "executorError"
        );
    }
}
