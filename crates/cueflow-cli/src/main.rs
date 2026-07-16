use std::{collections::BTreeMap, env, fs, path::PathBuf, process::ExitCode, time::Instant};

use cueflow_adapters::{CurrentPlatformAdapter, current_platform};
use cueflow_core::{
    Artifact, ArtifactKind, FailureKind, RunConfig, RunEvent, Target, parse_definition_json,
};
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

    if command == "request-accessibility-permission" {
        if args.next().is_some() {
            return usage();
        }
        let trusted = CurrentPlatformAdapter::request_accessibility_permission();
        println!(
            "{}",
            serde_json::json!({
                "platform": current_platform(),
                "accessibilityTrusted": trusted,
            })
        );
        return if trusted {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
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
    let mut config = RunConfig {
        dry_run: command == "dry-run",
        platform: Some(current_platform()),
        evidence_max_artifact_bytes: options.evidence_max_artifact_bytes,
        evidence_directory: options
            .evidence_dir
            .as_ref()
            .map(|path| path.display().to_string()),
        ..RunConfig::default()
    };
    if let Some(profile) = options.policy_profile {
        profile.apply_to(&mut config);
    }
    config.allow_coordinate_targets |= options.allow_coordinate_targets;
    config.allow_path_only_selectors |= options.allow_path_only_selectors;
    config.allow_value_capture |= options.allow_value_capture;
    config.capture_step_evidence |= options.capture_step_evidence;
    config.allow_screenshot_capture |= options.allow_screenshot_capture;
    config.allow_image_targets |= options.allow_image_targets;

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

    let mut evidence_prune_report = None;
    if command == "run"
        && options.prune_evidence_before_run
        && let Some(directory) = &options.evidence_dir
    {
        match prune_evidence_directory(directory) {
            Ok(report) => evidence_prune_report = Some(report),
            Err(error) => {
                eprintln!("failed to prune evidence directory: {error}");
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
                && let Err(error) = write_evidence_bundle(
                    &evidence_dir,
                    &definition.id,
                    &report,
                    options.evidence_max_artifact_bytes,
                    evidence_prune_report.as_ref(),
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
        "usage: cueflow capabilities | cueflow request-accessibility-permission | cueflow inspect-window (--title-contains <text>|--window-title <text>) [--max-depth <n>] [--max-nodes <n>] [--include-values] [--output <path>] | cueflow repair-selector (--title-contains <text>|--window-title <text>) [--id <id>] [--name <name>] [--control-type <type>] [--path <indexes>] [--max-depth <n>] [--max-nodes <n>] | cueflow screenshot --output <path> [(--window-title <text>|--title-contains <text>)|--allow-desktop-screenshot] | cueflow run-drills <manifest.json> | cueflow <validate|preflight|dry-run|run> [--policy-profile <strict|evidence|visual-fallback|unsafe-lab>] [--evidence-dir <dir>] [--capture-step-evidence] [--evidence-max-artifact-bytes <bytes>] [--prune-evidence-before-run] [--allow-coordinate-targets] [--allow-path-only-selectors] [--allow-value-capture] [--allow-screenshot-capture] [--allow-image-targets] <automation.json>"
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
            "--policy-profile"
                if command == "run" || command == "dry-run" || command == "preflight" =>
            {
                options.policy_profile = Some(AutomationPolicyProfile::from_cli(&args.next()?)?);
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
    policy_profile: Option<AutomationPolicyProfile>,
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
    policy_profile: Option<AutomationPolicyProfile>,
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
    #[serde(default)]
    expected_failure_kind: Option<FailureKind>,
    #[serde(default)]
    expected_error_contains: Option<String>,
    #[serde(default)]
    expected_log_contains: Option<String>,
    #[serde(default)]
    expected_max_duration_millis: Option<u64>,
    #[serde(default)]
    repeat: Option<u32>,
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

        let repeat = drill.repeat.unwrap_or(1).max(1);
        for attempt in 1..=repeat {
            let attempt_started_at = Instant::now();
            let evidence_dir = manifest.evidence_dir.as_ref().map(|dir| {
                let base = if dir.is_absolute() {
                    dir.join(&drill.id)
                } else {
                    base_dir.join(dir).join(&drill.id)
                };
                if repeat > 1 {
                    base.join(format!("attempt-{attempt}"))
                } else {
                    base
                }
            });
            let mut config = RunConfig {
                dry_run: false,
                platform: Some(current_platform()),
                evidence_max_artifact_bytes: drill.evidence_max_artifact_bytes,
                evidence_directory: evidence_dir.as_ref().map(|path| path.display().to_string()),
                ..RunConfig::default()
            };
            if let Some(profile) = drill.policy_profile {
                profile.apply_to(&mut config);
            }
            config.allow_coordinate_targets |= drill.allow_coordinate_targets;
            config.allow_path_only_selectors |= drill.allow_path_only_selectors;
            config.allow_value_capture |= drill.allow_value_capture;
            config.capture_step_evidence |= drill.capture_step_evidence;
            config.allow_screenshot_capture |= drill.allow_screenshot_capture;
            config.allow_image_targets |= drill.allow_image_targets;

            match executor.preflight(&definition, &config, &CurrentPlatformAdapter::new()) {
                Ok(report) if report.can_run() => {}
                Ok(report) => {
                    let duration_millis = attempt_started_at.elapsed().as_millis();
                    let error = format!(
                        "automation preflight failed: {}",
                        preflight_messages(&report)
                    );
                    let duration_matched = expected_duration_matches(drill, duration_millis);
                    let matched =
                        matches!(drill.expected_outcome, DrillExpectedOutcome::ExecutorError)
                            && drill.expected_failure_kind.is_none()
                            && expected_error_matches(drill, &error)
                            && duration_matched;
                    all_matched &= matched;
                    results.push(serde_json::json!({
                        "id": drill.id,
                        "attempt": attempt,
                        "repeat": repeat,
                        "durationMillis": duration_millis,
                        "expectedMaxDurationMillis": drill.expected_max_duration_millis,
                        "durationMatched": duration_matched,
                        "path": drill_path,
                        "expectedOutcome": drill.expected_outcome.as_str(),
                        "actualOutcome": "executorError",
                        "matched": matched,
                        "error": error,
                    }));
                    continue;
                }
                Err(error) => {
                    let duration_millis = attempt_started_at.elapsed().as_millis();
                    let error = error.to_string();
                    let duration_matched = expected_duration_matches(drill, duration_millis);
                    let matched =
                        matches!(drill.expected_outcome, DrillExpectedOutcome::ExecutorError)
                            && drill.expected_failure_kind.is_none()
                            && expected_error_matches(drill, &error)
                            && duration_matched;
                    all_matched &= matched;
                    results.push(serde_json::json!({
                        "id": drill.id,
                        "attempt": attempt,
                        "repeat": repeat,
                        "durationMillis": duration_millis,
                        "expectedMaxDurationMillis": drill.expected_max_duration_millis,
                        "durationMatched": duration_matched,
                        "path": drill_path,
                        "expectedOutcome": drill.expected_outcome.as_str(),
                        "actualOutcome": "executorError",
                        "matched": matched,
                        "error": error,
                    }));
                    continue;
                }
            }

            let mut evidence_prune_report = None;
            if drill.prune_evidence_before_run
                && let Some(directory) = &evidence_dir
            {
                match prune_evidence_directory(directory) {
                    Ok(report) => evidence_prune_report = Some(report),
                    Err(error) => {
                        let duration_millis = attempt_started_at.elapsed().as_millis();
                        let duration_matched = expected_duration_matches(drill, duration_millis);
                        all_matched = false;
                        results.push(serde_json::json!({
                            "id": drill.id,
                            "attempt": attempt,
                            "repeat": repeat,
                            "durationMillis": duration_millis,
                            "expectedMaxDurationMillis": drill.expected_max_duration_millis,
                            "durationMatched": duration_matched,
                            "path": drill_path,
                            "expectedOutcome": drill.expected_outcome.as_str(),
                            "actualOutcome": "invalid",
                            "matched": false,
                            "error": format!("failed to prune evidence directory: {error}"),
                        }));
                        continue;
                    }
                }
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
                    let duration_millis = attempt_started_at.elapsed().as_millis();
                    if let Some(evidence_dir) = evidence_dir.as_ref()
                        && let Err(error) = write_evidence_bundle(
                            evidence_dir,
                            &definition.id,
                            &report,
                            drill.evidence_max_artifact_bytes,
                            evidence_prune_report.as_ref(),
                        )
                    {
                        all_matched = false;
                        results.push(serde_json::json!({
                            "id": drill.id,
                            "attempt": attempt,
                            "repeat": repeat,
                            "durationMillis": duration_millis,
                            "expectedMaxDurationMillis": drill.expected_max_duration_millis,
                            "durationMatched": expected_duration_matches(drill, duration_millis),
                            "path": drill_path,
                            "expectedOutcome": drill.expected_outcome.as_str(),
                            "actualOutcome": outcome_str(report.outcome),
                            "matched": false,
                            "error": format!("failed to write evidence bundle: {error}"),
                        }));
                        continue;
                    }
                    let actual_failure_kind = report_failure_kind(&report);
                    let actual_error = report_error_message(&report);
                    let actual_log_matched = expected_log_matches(drill, &report);
                    let duration_matched = expected_duration_matches(drill, duration_millis);
                    let matched = drill.expected_outcome.matches(report.outcome)
                        && expected_failure_kind_matches(drill, actual_failure_kind)
                        && expected_optional_error_matches(drill, actual_error.as_deref())
                        && actual_log_matched
                        && duration_matched;
                    all_matched &= matched;
                    results.push(serde_json::json!({
                        "id": drill.id,
                        "attempt": attempt,
                        "repeat": repeat,
                        "durationMillis": duration_millis,
                        "expectedMaxDurationMillis": drill.expected_max_duration_millis,
                        "durationMatched": duration_matched,
                        "path": drill_path,
                        "expectedOutcome": drill.expected_outcome.as_str(),
                        "actualOutcome": outcome_str(report.outcome),
                        "expectedFailureKind": drill.expected_failure_kind,
                        "actualFailureKind": actual_failure_kind,
                        "expectedLogMatched": actual_log_matched,
                        "matched": matched,
                        "runId": report.run_id,
                        "eventCount": report.events.len(),
                        "failureSummary": report_failure_summary(&report),
                        "evidencePrune": evidence_prune_json(evidence_prune_report.as_ref()),
                    }));
                }
                Err(error) => {
                    let duration_millis = attempt_started_at.elapsed().as_millis();
                    let error = error.to_string();
                    let duration_matched = expected_duration_matches(drill, duration_millis);
                    let matched =
                        matches!(drill.expected_outcome, DrillExpectedOutcome::ExecutorError)
                            && drill.expected_failure_kind.is_none()
                            && expected_error_matches(drill, &error)
                            && duration_matched;
                    all_matched &= matched;
                    results.push(serde_json::json!({
                        "id": drill.id,
                        "attempt": attempt,
                        "repeat": repeat,
                        "durationMillis": duration_millis,
                        "expectedMaxDurationMillis": drill.expected_max_duration_millis,
                        "durationMatched": duration_matched,
                        "path": drill_path,
                        "expectedOutcome": drill.expected_outcome.as_str(),
                        "actualOutcome": "executorError",
                        "matched": matched,
                        "error": error,
                    }));
                }
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

fn report_failure_kind(report: &RunReport) -> Option<FailureKind> {
    report.events.iter().rev().find_map(|event| match event {
        RunEvent::StepFailed { error, .. } | RunEvent::ManualIntervention { error, .. } => {
            error.failure_kind
        }
        _ => None,
    })
}

fn report_error_message(report: &RunReport) -> Option<String> {
    report.events.iter().rev().find_map(|event| match event {
        RunEvent::StepFailed { error, .. } | RunEvent::ManualIntervention { error, .. } => {
            Some(error.message.clone())
        }
        _ => None,
    })
}

fn report_failure_summary(report: &RunReport) -> serde_json::Value {
    report
        .events
        .iter()
        .rev()
        .find_map(|event| match event {
            RunEvent::StepFailed { step_id, error, .. } => Some(serde_json::json!({
                "event": "stepFailed",
                "stepId": step_id,
                "errorKind": error.kind,
                "failureKind": error.failure_kind,
                "message": error.message,
                "source": error.source,
            })),
            RunEvent::ManualIntervention { step_id, error, .. } => Some(serde_json::json!({
                "event": "manualIntervention",
                "stepId": step_id,
                "errorKind": error.kind,
                "failureKind": error.failure_kind,
                "message": error.message,
                "source": error.source,
            })),
            _ => None,
        })
        .unwrap_or_else(|| match report.outcome {
            RunOutcome::Succeeded => serde_json::Value::Null,
            RunOutcome::Failed => serde_json::json!({
                "event": "runFailed",
                "message": "run failed without a terminal step failure event",
            }),
            RunOutcome::Cancelled => serde_json::json!({
                "event": "runCancelled",
                "message": "run was cancelled without a terminal step failure event",
            }),
        })
}

fn expected_failure_kind_matches(
    drill: &DrillEntry,
    actual_failure_kind: Option<FailureKind>,
) -> bool {
    drill
        .expected_failure_kind
        .is_none_or(|expected| Some(expected) == actual_failure_kind)
}

fn expected_error_matches(drill: &DrillEntry, actual_error: &str) -> bool {
    drill
        .expected_error_contains
        .as_ref()
        .is_none_or(|expected| actual_error.contains(expected))
}

fn expected_optional_error_matches(drill: &DrillEntry, actual_error: Option<&str>) -> bool {
    match (&drill.expected_error_contains, actual_error) {
        (Some(expected), Some(actual)) => actual.contains(expected),
        (Some(_), None) => false,
        (None, _) => true,
    }
}

fn expected_log_matches(drill: &DrillEntry, report: &RunReport) -> bool {
    let Some(expected) = &drill.expected_log_contains else {
        return true;
    };
    report.events.iter().any(|event| match event {
        RunEvent::Log { message, .. } => message.contains(expected),
        _ => false,
    })
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

fn expected_duration_matches(drill: &DrillEntry, actual_duration_millis: u128) -> bool {
    drill
        .expected_max_duration_millis
        .is_none_or(|expected| actual_duration_millis <= u128::from(expected))
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

#[derive(Debug, Default)]
struct EvidencePruneReport {
    events_removed: bool,
    summary_removed: bool,
    steps_removed: bool,
}

fn prune_evidence_directory(directory: &std::path::Path) -> std::io::Result<EvidencePruneReport> {
    let mut report = EvidencePruneReport::default();
    let events_path = directory.join("events.jsonl");
    match fs::remove_file(&events_path) {
        Ok(()) => report.events_removed = true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let summary_path = directory.join("summary.json");
    match fs::remove_file(&summary_path) {
        Ok(()) => report.summary_removed = true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let steps_path = directory.join("steps");
    match fs::remove_dir_all(&steps_path) {
        Ok(()) => report.steps_removed = true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    Ok(report)
}

fn evidence_prune_json(report: Option<&EvidencePruneReport>) -> serde_json::Value {
    match report {
        Some(report) => serde_json::json!({
            "requested": true,
            "eventsRemoved": report.events_removed,
            "summaryRemoved": report.summary_removed,
            "stepsRemoved": report.steps_removed,
        }),
        None => serde_json::json!({
            "requested": false,
            "eventsRemoved": false,
            "summaryRemoved": false,
            "stepsRemoved": false,
        }),
    }
}

fn write_evidence_bundle(
    directory: &PathBuf,
    automation_id: &str,
    report: &RunReport,
    evidence_max_artifact_bytes: Option<u64>,
    prune_report: Option<&EvidencePruneReport>,
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
            "failureSummary": report_failure_summary(report),
            "retentionPolicy": {
                "evidenceIsLocal": true,
                "prunedBeforeRun": prune_report.is_some(),
                "pruneBeforeRun": evidence_prune_json(prune_report),
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

    #[test]
    fn policy_profiles_apply_named_approval_sets() {
        let mut evidence = RunConfig::default();
        AutomationPolicyProfile::Evidence.apply_to(&mut evidence);
        assert!(evidence.capture_step_evidence);
        assert!(!evidence.allow_image_targets);

        let mut visual = RunConfig::default();
        AutomationPolicyProfile::VisualFallback.apply_to(&mut visual);
        assert!(visual.capture_step_evidence);
        assert!(visual.allow_screenshot_capture);
        assert!(visual.allow_image_targets);
        assert!(!visual.allow_coordinate_targets);

        let mut lab = RunConfig::default();
        AutomationPolicyProfile::UnsafeLab.apply_to(&mut lab);
        assert!(lab.allow_coordinate_targets);
        assert!(lab.allow_path_only_selectors);
        assert!(lab.allow_value_capture);
        assert!(lab.capture_step_evidence);
        assert!(lab.allow_screenshot_capture);
        assert!(lab.allow_image_targets);
    }

    #[test]
    fn policy_profiles_parse_cli_aliases() {
        assert_eq!(
            AutomationPolicyProfile::from_cli("visual-fallback"),
            Some(AutomationPolicyProfile::VisualFallback)
        );
        assert_eq!(
            AutomationPolicyProfile::from_cli("unsafeLab"),
            Some(AutomationPolicyProfile::UnsafeLab)
        );
        assert_eq!(AutomationPolicyProfile::from_cli("unknown"), None);
    }

    #[test]
    fn expected_duration_matches_optional_maximum() {
        let mut drill = DrillEntry {
            id: "duration".to_string(),
            path: PathBuf::from("duration.json"),
            expected_outcome: DrillExpectedOutcome::Succeeded,
            policy_profile: None,
            allow_coordinate_targets: false,
            allow_path_only_selectors: false,
            allow_value_capture: false,
            capture_step_evidence: false,
            allow_screenshot_capture: false,
            allow_image_targets: false,
            evidence_max_artifact_bytes: None,
            prune_evidence_before_run: false,
            expected_failure_kind: None,
            expected_error_contains: None,
            expected_log_contains: None,
            expected_max_duration_millis: None,
            repeat: None,
        };

        assert!(expected_duration_matches(&drill, 10_000));
        drill.expected_max_duration_millis = Some(500);
        assert!(expected_duration_matches(&drill, 500));
        assert!(!expected_duration_matches(&drill, 501));
    }

    #[test]
    fn failure_summary_reports_terminal_step_failure() {
        let report = RunReport {
            run_id: "run-test".to_string(),
            outcome: RunOutcome::Failed,
            events: vec![RunEvent::StepFailed {
                run_id: "run-test".to_string(),
                automation_id: "automation-test".to_string(),
                step_id: "step-test".to_string(),
                error: cueflow_core::RunError {
                    kind: cueflow_core::RunErrorKind::Adapter,
                    message: "semantic target is disabled".to_string(),
                    failure_kind: Some(FailureKind::Disabled),
                    step_id: Some("step-test".to_string()),
                    source: Some("failureKind=disabled".to_string()),
                },
            }],
        };

        assert_eq!(
            report_failure_summary(&report),
            serde_json::json!({
                "event": "stepFailed",
                "stepId": "step-test",
                "errorKind": "adapter",
                "failureKind": "disabled",
                "message": "semantic target is disabled",
                "source": "failureKind=disabled",
            })
        );
    }

    #[test]
    fn failure_summary_reports_run_level_failures_without_step_events() {
        let report = RunReport {
            run_id: "run-test".to_string(),
            outcome: RunOutcome::Failed,
            events: Vec::new(),
        };

        assert_eq!(
            report_failure_summary(&report),
            serde_json::json!({
                "event": "runFailed",
                "message": "run failed without a terminal step failure event",
            })
        );
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum AutomationPolicyProfile {
    Strict,
    Evidence,
    #[serde(alias = "visual-fallback")]
    VisualFallback,
    #[serde(alias = "unsafe-lab")]
    UnsafeLab,
}

impl AutomationPolicyProfile {
    fn from_cli(value: &str) -> Option<Self> {
        match value {
            "strict" => Some(Self::Strict),
            "evidence" => Some(Self::Evidence),
            "visual-fallback" | "visualFallback" => Some(Self::VisualFallback),
            "unsafe-lab" | "unsafeLab" => Some(Self::UnsafeLab),
            _ => None,
        }
    }

    fn apply_to(self, config: &mut RunConfig) {
        match self {
            Self::Strict => {}
            Self::Evidence => {
                config.capture_step_evidence = true;
            }
            Self::VisualFallback => {
                config.capture_step_evidence = true;
                config.allow_screenshot_capture = true;
                config.allow_image_targets = true;
            }
            Self::UnsafeLab => {
                config.allow_coordinate_targets = true;
                config.allow_path_only_selectors = true;
                config.allow_value_capture = true;
                config.capture_step_evidence = true;
                config.allow_screenshot_capture = true;
                config.allow_image_targets = true;
            }
        }
    }
}
