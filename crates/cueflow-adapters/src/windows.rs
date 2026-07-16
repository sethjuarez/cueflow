use std::collections::BTreeMap;
use std::fs;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

use crate::{
    AccessibilityBounds, AccessibilityNode, AccessibilityPoint, AccessibilitySelectorCandidate,
    AccessibilityTree, AdapterCapabilities, SelectorConfidence, SelectorRepairReport,
    WindowIdentity,
};
use cueflow_core::{
    Action, Artifact, Assertion, FailureKind, ImageRegion, ImageTarget, Platform,
    PreflightDiagnostic, PreflightSeverity, RunConfig, Target, WaitCondition,
};
use cueflow_executor::{AdapterError, ConditionState, EvidencePhase, ExecutionAdapter, RunControl};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HWND, INVALID_HANDLE_VALUE, LPARAM, RECT, RPC_E_CHANGED_MODE},
        Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap,
            CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, GetWindowDC,
            ReleaseDC, SRCCOPY, SelectObject,
        },
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject},
        UI::{
            Accessibility::{
                CUIAutomation, IUIAutomation, IUIAutomationCondition, IUIAutomationElement,
                IUIAutomationInvokePattern, IUIAutomationScrollPattern, IUIAutomationValuePattern,
                ScrollAmount, ScrollAmount_NoAmount, ScrollAmount_SmallDecrement,
                ScrollAmount_SmallIncrement, TreeScope_Children, UIA_InvokePatternId,
                UIA_ScrollPatternId, UIA_ValuePatternId,
            },
            Input::KeyboardAndMouse::{
                INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
                KEYEVENTF_UNICODE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_WHEEL,
                MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN,
                VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_MENU, VK_RETURN, VK_RIGHT, VK_SHIFT,
                VK_SPACE, VK_TAB, VK_UP,
            },
            Shell::ShellExecuteW,
            WindowsAndMessaging::{
                EnumWindows, GW_OWNER, GetClassNameW, GetDesktopWindow, GetForegroundWindow,
                GetSystemMetrics, GetWindow, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
                GetWindowThreadProcessId, IsIconic, IsWindowVisible, SM_CXSCREEN, SM_CYSCREEN,
                SW_SHOWNORMAL, SetCursorPos, SetForegroundWindow,
            },
        },
    },
    core::{BOOL, BSTR, HSTRING, Interface},
};

#[derive(Debug, Default)]
pub struct WindowsDesktopAdapter;

const SEMANTIC_SEARCH_MAX_DEPTH: u32 = 16;
const SEMANTIC_SEARCH_MAX_NODES: usize = 2_000;
const VISUAL_MATCH_MAX_PIXEL_COMPARISONS: u64 = 50_000_000;

impl WindowsDesktopAdapter {
    pub fn capabilities() -> AdapterCapabilities {
        AdapterCapabilities {
            platform: Platform::Windows,
            supports_launch: true,
            supports_focus: true,
            supports_input: true,
            supports_semantic_targets: true,
            supports_coordinate_targets: true,
            supports_window_queries: true,
            supports_process_queries: true,
            supports_accessibility_tree: true,
        }
    }

    pub fn inspect_window(
        &self,
        target: &Target,
        max_depth: u32,
        max_nodes: usize,
    ) -> Result<AccessibilityTree, AdapterError> {
        self.inspect_window_with_options(target, max_depth, max_nodes, false)
    }

    pub fn inspect_window_with_options(
        &self,
        target: &Target,
        max_depth: u32,
        max_nodes: usize,
        include_values: bool,
    ) -> Result<AccessibilityTree, AdapterError> {
        let window = find_window(target)?;
        let window_title = window_title(window)?;
        let window_identity = window_identity(window);
        let selector = window_target_summary(target);
        let max_nodes = max_nodes.max(1);

        let initialization = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let should_uninitialize = initialization.is_ok();
        if !should_uninitialize && initialization != RPC_E_CHANGED_MODE {
            return Err(AdapterError::new(
                "Windows could not initialize UI Automation",
            ));
        }

        let result = (|| {
            let automation: IUIAutomation = unsafe {
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(|_| {
                    AdapterError::new("Windows could not create a UI Automation client")
                })?
            };
            let root = unsafe {
                automation.ElementFromHandle(window).map_err(|_| {
                    AdapterError::new("Windows could not inspect the requested window")
                })?
            };
            let condition = unsafe {
                automation.CreateTrueCondition().map_err(|_| {
                    AdapterError::new("Windows could not create a UI Automation query")
                })?
            };
            let mut remaining = max_nodes;
            let mut truncated = false;
            let root = inspect_accessibility_node(
                &root,
                &condition,
                &[],
                &window_title,
                max_depth,
                include_values,
                &mut remaining,
                &mut truncated,
            )?;

            Ok(AccessibilityTree {
                platform: Platform::Windows,
                window_title,
                window: window_identity,
                selector,
                max_depth,
                max_nodes,
                truncated,
                root,
            })
        })();

        if should_uninitialize {
            unsafe {
                CoUninitialize();
            }
        }

        result
    }

    pub fn capture_screenshot(&self, path: impl AsRef<Path>) -> Result<Artifact, AdapterError> {
        capture_desktop_screenshot(path.as_ref())
    }

    pub fn capture_window_screenshot(
        &self,
        target: &Target,
        path: impl AsRef<Path>,
    ) -> Result<Artifact, AdapterError> {
        let window = find_window(target)?;
        capture_window_screenshot(window, path.as_ref())
    }

    pub fn repair_selector(
        &self,
        target: &Target,
        max_depth: u32,
        max_nodes: usize,
    ) -> Result<SelectorRepairReport, AdapterError> {
        let tree = self.inspect_window_with_options(target, max_depth, max_nodes, false)?;
        let matched = if target.accessibility.is_some() {
            semantic_target_exists(target).unwrap_or(false)
        } else {
            true
        };
        let mut candidates = Vec::new();
        collect_repair_candidates(&tree.root, target.accessibility.as_ref(), &mut candidates);
        for candidate in &mut candidates {
            candidate.changes = selector_candidate_changes(
                target.accessibility.as_ref(),
                candidate.target.accessibility.as_ref(),
            );
        }
        candidates.sort_by(|left, right| right.score.cmp(&left.score));
        let mut seen_targets = std::collections::BTreeSet::new();
        candidates.retain(|candidate| {
            serde_json::to_string(&candidate.target)
                .map(|target| seen_targets.insert(target))
                .unwrap_or(true)
        });
        candidates.truncate(20);
        Ok(SelectorRepairReport {
            original: target.clone(),
            matched,
            candidates,
            diagnostics: vec![format!(
                "inspected {} with maxDepth={} maxNodes={} truncated={}",
                tree.window_title, tree.max_depth, tree.max_nodes, tree.truncated
            )],
        })
    }
}

fn collect_repair_candidates(
    node: &AccessibilityNode,
    desired: Option<&cueflow_core::AccessibilityTarget>,
    candidates: &mut Vec<AccessibilitySelectorCandidate>,
) {
    for candidate in &node.selector_candidates {
        if repair_candidate_matches(candidate, desired) {
            candidates.push(candidate.clone());
        }
    }
    for child in &node.children {
        collect_repair_candidates(child, desired, candidates);
    }
}

fn repair_candidate_matches(
    candidate: &AccessibilitySelectorCandidate,
    desired: Option<&cueflow_core::AccessibilityTarget>,
) -> bool {
    let Some(desired) = desired else {
        return true;
    };
    let Some(accessibility) = &candidate.target.accessibility else {
        return false;
    };
    desired
        .id
        .as_ref()
        .is_some_and(|id| accessibility.id.as_ref() == Some(id))
        || desired
            .name
            .as_ref()
            .is_some_and(|name| accessibility.name.as_ref() == Some(name))
        || desired
            .control_type
            .as_ref()
            .is_some_and(|control_type| accessibility.control_type.as_ref() == Some(control_type))
        || desired.id.is_none()
            && desired.name.is_none()
            && desired.control_type.is_none()
            && accessibility.path.is_some()
}

fn window_search_diagnostics(target: &Target, windows: &[HWND]) -> String {
    let candidates = windows
        .iter()
        .take(8)
        .filter_map(|window| {
            window_identity(*window).map(|identity| format_window_identity(&identity))
        })
        .collect::<Vec<_>>();
    format!(
        "selector: {}; visible window matches: {}",
        window_target_summary(target),
        list_or_none(&candidates)
    )
}

fn format_window_identity(identity: &WindowIdentity) -> String {
    format!(
        "{} (handle={}, class={}, pid={}, process={}, bounds={}, foreground={}, minimized={}, owner={})",
        quote(&identity.title),
        identity.handle,
        quote(&identity.class_name),
        identity.process_id,
        identity
            .process_name
            .as_deref()
            .map(quote)
            .unwrap_or_else(|| "unknown".to_string()),
        identity
            .bounds
            .map(|bounds| format!(
                "{},{},{},{}",
                bounds.left, bounds.top, bounds.right, bounds.bottom
            ))
            .unwrap_or_else(|| "unknown".to_string()),
        identity.is_foreground,
        identity.is_minimized,
        identity.owner.as_deref().unwrap_or("none")
    )
}

fn window_target_summary(target: &Target) -> String {
    let mut fields = Vec::new();
    if let Some(title) = &target.window_title {
        fields.push(format!("windowTitle={}", quote(title)));
    }
    if let Some(title) = &target.title_contains {
        fields.push(format!("titleContains={}", quote(title)));
    }
    if let Some(process_name) = &target.process_name {
        fields.push(format!("processName={}", quote(process_name)));
    }
    if let Some(app_name) = &target.app_name {
        fields.push(format!("appName={}", quote(app_name)));
    }
    if fields.is_empty() {
        "no supported window selector fields".to_string()
    } else {
        fields.join(", ")
    }
}

impl ExecutionAdapter for WindowsDesktopAdapter {
    fn execute(
        &mut self,
        action: &Action,
        config: &RunConfig,
    ) -> Result<Vec<Artifact>, AdapterError> {
        match action {
            Action::LaunchUrl {
                url,
                target: Some(target),
            } => {
                reject_unsupported_target(unsupported_launch_target_reason(target, config))?;
                shell_open(url)
            }
            Action::LaunchUrl { url, target: None } => shell_open(url),
            Action::LaunchApp {
                app,
                target: Some(target),
            } => {
                reject_unsupported_target(unsupported_launch_target_reason(target, config))?;
                shell_open(app)
            }
            Action::LaunchApp { app, target: None } => shell_open(app),
            Action::FocusWindow { target } => {
                reject_unsupported_target(unsupported_window_target_reason_with_config(
                    target, config,
                ))?;
                focus_window(target).map(|_| Vec::new())
            }
            Action::TypeText {
                text,
                target: Some(target),
            } => {
                reject_unsupported_target(unsupported_semantic_target_reason_with_config(
                    target, config,
                ))?;
                self.set_target_text(target, text, config)
                    .map(|_| Vec::new())
            }
            Action::TypeText { text, target: None } => send_text(text).map(|_| Vec::new()),
            Action::PressKey {
                keys,
                target: Some(target),
            } => {
                reject_unsupported_target(unsupported_semantic_target_reason_with_config(
                    target, config,
                ))?;
                focus_target(target, config)
                    .and_then(|_| send_key_chord(keys))
                    .map(|_| Vec::new())
            }
            Action::PressKey { keys, target: None } => send_key_chord(keys).map(|_| Vec::new()),
            Action::Scroll {
                delta_y,
                target: Some(target),
                ..
            } => {
                reject_unsupported_target(unsupported_semantic_target_reason_with_config(
                    target, config,
                ))?;
                focus_target(target, config)
                    .and_then(|_| send_scroll(*delta_y))
                    .map(|_| Vec::new())
            }
            Action::Scroll {
                delta_y,
                target: None,
                ..
            } => send_scroll(*delta_y).map(|_| Vec::new()),
            Action::ClickTarget { target } if target.coordinates.is_some() => {
                reject_unsupported_target(unsupported_coordinate_target_reason(target, config))?;
                click_coordinates(target).map(|_| Vec::new())
            }
            Action::ClickTarget { target } if target.image.is_some() => {
                reject_unsupported_target(unsupported_image_target_reason(target, config))?;
                click_image_target(target, config).map(|_| Vec::new())
            }
            Action::ClickTarget { target } => {
                reject_unsupported_target(unsupported_semantic_target_reason_with_config(
                    target, config,
                ))?;
                self.invoke_target(target, config).map(|_| Vec::new())
            }
            Action::RunCommand { command, args } => {
                run_command(command, args, config, &RunControl::default(), None).map(|_| Vec::new())
            }
            Action::OpenFile {
                path,
                target: Some(target),
            } => {
                reject_unsupported_target(unsupported_launch_target_reason(target, config))?;
                shell_open_file(path)
            }
            Action::OpenFile { path, target: None } => shell_open_file(path),
            _ => Err(AdapterError::unsupported(format!(
                "Windows adapter does not yet support {}",
                action.kind()
            ))),
        }
    }

    fn evaluate_wait(
        &mut self,
        condition: &WaitCondition,
        config: &RunConfig,
    ) -> Result<ConditionState, AdapterError> {
        match condition {
            WaitCondition::FileExists { path } => Ok(if Path::new(path).exists() {
                ConditionState::Satisfied
            } else {
                ConditionState::Pending
            }),
            WaitCondition::WindowExists { target } => {
                if target.accessibility.is_some() {
                    return semantic_target_exists(target).map(|exists| {
                        if exists {
                            ConditionState::Satisfied
                        } else {
                            ConditionState::Pending
                        }
                    });
                }
                window_exists(target).map(|exists| {
                    if exists {
                        ConditionState::Satisfied
                    } else {
                        ConditionState::Pending
                    }
                })
            }
            WaitCondition::WindowFocused { target } => window_is_focused(target).map(|focused| {
                if focused {
                    ConditionState::Satisfied
                } else {
                    ConditionState::Pending
                }
            }),
            WaitCondition::TargetExists { target } if target.image.is_some() => {
                image_target_exists(target, config).map(condition_state)
            }
            WaitCondition::TargetExists { target } => target_exists_state(target),
            WaitCondition::TargetFocused { target } => {
                semantic_target_focused(target).map(condition_state)
            }
            WaitCondition::TargetEnabled { target } => {
                semantic_target_enabled(target).map(condition_state)
            }
            WaitCondition::TargetVisible { target } => {
                semantic_target_visible(target).map(condition_state)
            }
            WaitCondition::TargetActionable { target } => {
                semantic_target_actionable(target).map(condition_state)
            }
            WaitCondition::TargetNotExists { target } if target.image.is_some() => {
                image_target_exists(target, config).map(|exists| condition_state(!exists))
            }
            WaitCondition::TargetNotExists { target } => {
                semantic_target_exists(target).map(|exists| condition_state(!exists))
            }
            WaitCondition::TargetNameContains { target, text } => {
                semantic_target_name_contains(target, text).map(condition_state)
            }
            WaitCondition::TargetValueContains { target, text } => {
                semantic_target_value_contains(target, text).map(condition_state)
            }
            WaitCondition::ProcessRunning { target } => process_is_running(target).map(|running| {
                if running {
                    ConditionState::Satisfied
                } else {
                    ConditionState::Pending
                }
            }),
            WaitCondition::CommandExits { command, args } => {
                command_exits(command, args, config, &RunControl::default(), None).map(
                    |succeeded| {
                        if succeeded {
                            ConditionState::Satisfied
                        } else {
                            ConditionState::Pending
                        }
                    },
                )
            }
            _ => ExecutionAdapter::evaluate_wait(self, condition, config),
        }
    }

    fn evaluate_wait_with_control(
        &mut self,
        condition: &WaitCondition,
        config: &RunConfig,
        control: &RunControl,
        timeout: Option<Duration>,
    ) -> Result<ConditionState, AdapterError> {
        match condition {
            WaitCondition::CommandExits { command, args } => {
                command_exits(command, args, config, control, timeout).map(|succeeded| {
                    if succeeded {
                        ConditionState::Satisfied
                    } else {
                        ConditionState::Pending
                    }
                })
            }
            _ => self.evaluate_wait(condition, config),
        }
    }

    fn execute_with_control(
        &mut self,
        action: &Action,
        config: &RunConfig,
        control: &RunControl,
        timeout: Option<Duration>,
    ) -> Result<Vec<Artifact>, AdapterError> {
        match action {
            Action::RunCommand { command, args } => {
                run_command(command, args, config, control, timeout).map(|_| Vec::new())
            }
            Action::LaunchUrl {
                url,
                target: Some(target),
            } => launch_and_wait_for_window(url, target, config, control, timeout),
            Action::LaunchApp {
                app,
                target: Some(target),
            } => launch_and_wait_for_window(app, target, config, control, timeout),
            _ => self.execute(action, config),
        }
    }

    fn evaluate_assertion(
        &mut self,
        assertion: &Assertion,
        config: &RunConfig,
    ) -> Result<bool, AdapterError> {
        match assertion {
            Assertion::TargetExists { target } if target.image.is_some() => {
                image_target_exists(target, config)
            }
            Assertion::TargetExists { target } if target.accessibility.is_some() => {
                semantic_target_exists(target)
            }
            Assertion::TargetExists { target } => window_exists(target),
            Assertion::Condition { condition } => self
                .evaluate_wait(condition, config)
                .map(|state| state == ConditionState::Satisfied),
        }
    }

    fn target_exists(
        &mut self,
        target: &Target,
        _config: &RunConfig,
    ) -> Result<bool, AdapterError> {
        if target.image.is_some() {
            return image_target_exists(target, _config);
        }
        semantic_target_exists(target)
    }

    fn invoke_target(&mut self, target: &Target, config: &RunConfig) -> Result<(), AdapterError> {
        if target.coordinates.is_some() {
            reject_unsupported_target(unsupported_coordinate_target_reason(target, config))?;
            return click_coordinates(target);
        }
        reject_unsupported_target(unsupported_semantic_target_reason_with_config(
            target, config,
        ))?;
        with_semantic_target(target, |element| {
            ensure_semantic_target_actionable(element)?;
            let pattern = unsafe {
                element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
            }
            .map_err(|_| {
                AdapterError::unsupported("target does not support semantic invocation")
            })?;
            unsafe { pattern.Invoke() }
                .map_err(|_| AdapterError::new("Windows could not invoke the requested target"))
        })
    }

    fn set_target_text(
        &mut self,
        target: &Target,
        text: &str,
        config: &RunConfig,
    ) -> Result<(), AdapterError> {
        reject_unsupported_target(unsupported_semantic_target_reason_with_config(
            target, config,
        ))?;
        with_semantic_target(target, |element| {
            ensure_semantic_target_actionable(element)?;
            let pattern = unsafe {
                element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            }
            .map_err(|_| {
                AdapterError::unsupported("target does not support semantic text input")
            })?;
            unsafe { pattern.SetValue(&BSTR::from(text)) }
                .map_err(|_| AdapterError::new("Windows could not set the requested target text"))
        })
    }

    fn target_is_focused(
        &mut self,
        target: &Target,
        _config: &RunConfig,
    ) -> Result<bool, AdapterError> {
        semantic_target_focused(target)
    }

    fn scroll_target(
        &mut self,
        target: &Target,
        delta_x: i32,
        delta_y: i32,
        config: &RunConfig,
    ) -> Result<(), AdapterError> {
        reject_unsupported_target(unsupported_semantic_target_reason_with_config(
            target, config,
        ))?;
        with_semantic_target(target, |element| {
            ensure_semantic_target_actionable(element)?;
            let pattern = unsafe {
                element.GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
            }
            .map_err(|_| AdapterError::unsupported("target does not support semantic scrolling"))?;
            unsafe { pattern.Scroll(scroll_amount(delta_x), scroll_amount(delta_y)) }
                .map_err(|_| AdapterError::new("Windows could not scroll the requested target"))
        })
    }

    fn preflight(&self, action: &Action, config: &RunConfig) -> Vec<PreflightDiagnostic> {
        unsupported_action_reason(action, config)
            .map(|message| {
                vec![PreflightDiagnostic {
                    severity: PreflightSeverity::Error,
                    code: "capability-unavailable".to_string(),
                    message: message.to_string(),
                    step_id: None,
                }]
            })
            .unwrap_or_default()
    }

    fn capture_step_evidence(
        &mut self,
        phase: EvidencePhase,
        action: &Action,
        config: &RunConfig,
        run_id: &str,
        automation_id: &str,
        step_id: &str,
    ) -> Result<Vec<Artifact>, AdapterError> {
        if !config.capture_step_evidence {
            return Ok(Vec::new());
        }
        reject_unsupported_target(unsupported_action_reason(action, config))?;
        let Some(directory) = &config.evidence_directory else {
            return Ok(Vec::new());
        };
        let Some(target) = evidence_target(action) else {
            return Ok(Vec::new());
        };
        if target.window_title.is_none() && target.title_contains.is_none() {
            return Ok(Vec::new());
        }

        let evidence_dir = Path::new(directory).join("steps").join(step_id);
        let safe_run_id = safe_artifact_component(run_id);
        let safe_automation_id = safe_artifact_component(automation_id);
        let tree_path = evidence_dir.join(format!(
            "{safe_automation_id}-{safe_run_id}-{step_id}-{}-accessibility.json",
            phase.as_str()
        ));
        let tree = self.inspect_window_with_options(target, 3, 300, config.allow_value_capture)?;
        let tree_json =
            serde_json::to_string_pretty(&tree).expect("accessibility evidence serializes");
        enforce_evidence_artifact_size(tree_json.len() as u64, config)?;

        let screenshot_path = if config.allow_screenshot_capture {
            Some(evidence_dir.join(format!(
                "{safe_automation_id}-{safe_run_id}-{step_id}-{}-window.bmp",
                phase.as_str()
            )))
        } else {
            None
        };
        let screenshot_window = if config.allow_screenshot_capture {
            let window = find_window(target)?;
            enforce_evidence_artifact_size(window_screenshot_bmp_size(window)?, config)?;
            Some(window)
        } else {
            None
        };

        fs::create_dir_all(&evidence_dir)
            .map_err(|_| transient_error("Windows could not create step evidence directory"))?;
        let screenshot = if let (Some(window), Some(screenshot_path)) =
            (screenshot_window, screenshot_path.as_ref())
        {
            Some(capture_window_screenshot_with_limit(
                window,
                screenshot_path,
                Some(config),
            )?)
        } else {
            None
        };
        fs::write(&tree_path, tree_json)
            .map_err(|_| transient_error("Windows could not write accessibility evidence"))?;

        let mut artifacts = vec![Artifact {
            kind: cueflow_core::ArtifactKind::AccessibilityTree,
            uri: format!(
                "file://{}",
                strip_verbatim_path_prefix(&path_display(&tree_path))
            ),
            label: Some(format!("{} accessibility tree: {step_id}", phase.as_str())),
        }];

        if let Some(screenshot) = screenshot {
            artifacts.push(screenshot);
        }

        Ok(artifacts)
    }
}

const DEFAULT_EVIDENCE_MAX_ARTIFACT_BYTES: u64 = 25 * 1024 * 1024;

fn enforce_evidence_artifact_size(size: u64, config: &RunConfig) -> Result<(), AdapterError> {
    let limit = config
        .evidence_max_artifact_bytes
        .unwrap_or(DEFAULT_EVIDENCE_MAX_ARTIFACT_BYTES);
    if size > limit {
        return Err(AdapterError::new(format!(
            "evidence artifact exceeded configured size limit ({size} > {limit} bytes)"
        ))
        .with_failure_kind(FailureKind::PolicyDenied)
        .with_source("failureKind=policyDenied"));
    }
    Ok(())
}

fn transient_error(message: impl Into<String>) -> AdapterError {
    AdapterError::new(message)
        .with_failure_kind(FailureKind::Transient)
        .with_source("failureKind=transient")
}

fn focus_denied_error(message: impl Into<String>) -> AdapterError {
    AdapterError::new(message)
        .with_failure_kind(FailureKind::FocusDenied)
        .with_source("failureKind=focusDenied")
}

fn evidence_target(action: &Action) -> Option<&Target> {
    match action {
        Action::LaunchUrl { target, .. }
        | Action::LaunchApp { target, .. }
        | Action::TypeText { target, .. }
        | Action::PressKey { target, .. }
        | Action::Scroll { target, .. } => target.as_ref(),
        Action::FocusWindow { target } | Action::ClickTarget { target } => Some(target),
        Action::WaitFor { condition } => condition_target(condition),
        Action::Assert {
            assertion: Assertion::TargetExists { target },
        } => Some(target),
        Action::Assert {
            assertion: Assertion::Condition { condition },
        } => condition_target(condition),
        _ => None,
    }
}

fn condition_target(condition: &WaitCondition) -> Option<&Target> {
    match condition {
        WaitCondition::WindowExists { target }
        | WaitCondition::WindowFocused { target }
        | WaitCondition::ProcessRunning { target }
        | WaitCondition::TargetExists { target }
        | WaitCondition::TargetFocused { target }
        | WaitCondition::TargetEnabled { target }
        | WaitCondition::TargetVisible { target }
        | WaitCondition::TargetActionable { target }
        | WaitCondition::TargetNotExists { target }
        | WaitCondition::TargetNameContains { target, .. }
        | WaitCondition::TargetValueContains { target, .. } => Some(target),
        _ => None,
    }
}

fn safe_artifact_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn send_text(text: &str) -> Result<(), AdapterError> {
    let mut inputs = Vec::with_capacity(text.len() * 2);
    for code_unit in text.encode_utf16() {
        inputs.push(keyboard_input(VIRTUAL_KEY(0), code_unit, KEYEVENTF_UNICODE));
        inputs.push(keyboard_input(
            VIRTUAL_KEY(0),
            code_unit,
            KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
        ));
    }
    send_inputs(&inputs)
}

fn send_key_chord(keys: &str) -> Result<(), AdapterError> {
    let parts = keys.split('+').map(str::trim).collect::<Vec<_>>();
    let (last, modifiers) = parts
        .split_last()
        .ok_or_else(|| AdapterError::new("key chord is required"))?;
    let primary = parse_virtual_key(last)?;
    let modifiers = modifiers
        .iter()
        .map(|modifier| match *modifier {
            "CmdOrControl" | "Control" | "Ctrl" => Ok(VK_CONTROL),
            "Shift" => Ok(VK_SHIFT),
            "Alt" => Ok(VK_MENU),
            "Meta" | "Win" => Ok(VIRTUAL_KEY(0x5B)),
            _ => Err(AdapterError::unsupported(
                "key chord contains an unsupported modifier",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut inputs = Vec::with_capacity(modifiers.len() * 2 + 2);
    inputs.extend(
        modifiers
            .iter()
            .copied()
            .map(|key| keyboard_input(key, 0, Default::default())),
    );
    inputs.push(keyboard_input(primary, 0, Default::default()));
    inputs.push(keyboard_input(primary, 0, KEYEVENTF_KEYUP));
    inputs.extend(
        modifiers
            .iter()
            .rev()
            .copied()
            .map(|key| keyboard_input(key, 0, KEYEVENTF_KEYUP)),
    );
    send_inputs(&inputs)
}

fn send_scroll(delta_y: i32) -> Result<(), AdapterError> {
    if delta_y == 0 {
        return Ok(());
    }
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: MOUSEEVENTF_WHEEL,
                mouseData: delta_y as u32,
                ..Default::default()
            },
        },
    };
    send_inputs(&[input])
}

fn click_coordinates(target: &Target) -> Result<(), AdapterError> {
    let coordinates = target
        .coordinates
        .ok_or_else(|| AdapterError::unsupported("coordinate clicks require coordinates"))?;
    click_screen_point(coordinates.x, coordinates.y)
}

fn click_screen_point(x: i32, y: i32) -> Result<(), AdapterError> {
    unsafe { SetCursorPos(x, y) }
        .map_err(|_| AdapterError::new("Windows could not move the pointer to the target"))?;
    let inputs = [
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dwFlags: MOUSEEVENTF_LEFTDOWN,
                    ..Default::default()
                },
            },
        },
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dwFlags: MOUSEEVENTF_LEFTUP,
                    ..Default::default()
                },
            },
        },
    ];
    send_inputs(&inputs)
}

fn keyboard_input(
    virtual_key: VIRTUAL_KEY,
    scan_code: u16,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS,
) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: virtual_key,
                wScan: scan_code,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

fn send_inputs(inputs: &[INPUT]) -> Result<(), AdapterError> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(AdapterError::new(
            "Windows could not inject the requested input",
        ));
    }
    Ok(())
}

fn parse_virtual_key(key: &str) -> Result<VIRTUAL_KEY, AdapterError> {
    let virtual_key = match key {
        "Enter" => VK_RETURN,
        "Escape" => VK_ESCAPE,
        "Tab" => VK_TAB,
        "Space" => VK_SPACE,
        "Backspace" => VK_BACK,
        "Delete" => VK_DELETE,
        "Home" => VK_HOME,
        "End" => VK_END,
        "ArrowUp" => VK_UP,
        "ArrowDown" => VK_DOWN,
        "ArrowLeft" => VK_LEFT,
        "ArrowRight" => VK_RIGHT,
        key if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() => {
            VIRTUAL_KEY(key.as_bytes()[0].to_ascii_uppercase() as u16)
        }
        _ => {
            return Err(AdapterError::unsupported(
                "key chord contains an unsupported key",
            ));
        }
    };
    Ok(virtual_key)
}

fn shell_open(target: &str) -> Result<Vec<Artifact>, AdapterError> {
    let target = HSTRING::from(target);
    let result = unsafe { ShellExecuteW(None, None, &target, None, None, SW_SHOWNORMAL) };
    if result.0 as isize <= 32 {
        return Err(
            AdapterError::new("Windows could not open the requested target")
                .with_failure_kind(FailureKind::Transient),
        );
    }

    Ok(Vec::new())
}

fn launch_and_wait_for_window(
    launch_target: &str,
    window_target: &Target,
    config: &RunConfig,
    control: &RunControl,
    timeout: Option<Duration>,
) -> Result<Vec<Artifact>, AdapterError> {
    if let Some(reason) = unsupported_launch_target_reason(window_target, config) {
        return Err(AdapterError::unsupported(reason).with_failure_kind(FailureKind::PolicyDenied));
    }
    let artifacts = shell_open(launch_target)?;
    let timeout = timeout.unwrap_or(Duration::from_secs(5));
    let started_at = Instant::now();
    let mut last_error = None;
    while started_at.elapsed() < timeout {
        if control.is_cancelled() {
            return Err(AdapterError::cancelled());
        }
        match find_window(window_target) {
            Ok(_) => return Ok(artifacts),
            Err(error)
                if matches!(
                    error.failure_kind(),
                    Some(FailureKind::NotFound | FailureKind::Ambiguous)
                ) =>
            {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(AdapterError::timeout))
}

fn shell_open_file(path: &str) -> Result<Vec<Artifact>, AdapterError> {
    let path = Path::new(path);
    let path = path
        .canonicalize()
        .map_err(|_| AdapterError::new("Windows could not resolve the requested file"))?;
    shell_open(&strip_verbatim_path_prefix(&path.to_string_lossy()))
}

fn strip_verbatim_path_prefix(path: &str) -> String {
    path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
}

fn capture_desktop_screenshot(path: &Path) -> Result<Artifact, AdapterError> {
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if width <= 0 || height <= 0 {
        return Err(AdapterError::new(
            "Windows could not determine screen dimensions",
        ));
    }

    let desktop = unsafe { GetDesktopWindow() };
    let screen_dc = unsafe { GetWindowDC(Some(desktop)) };
    if screen_dc.is_invalid() {
        return Err(AdapterError::new(
            "Windows could not acquire the screen device context",
        ));
    }

    let result = (|| {
        let memory_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
        if memory_dc.is_invalid() {
            return Err(AdapterError::new(
                "Windows could not create a screenshot device context",
            ));
        }
        let bitmap = unsafe { CreateCompatibleBitmap(screen_dc, width, height) };
        if bitmap.is_invalid() {
            unsafe {
                let _ = DeleteDC(memory_dc);
            }
            return Err(AdapterError::new(
                "Windows could not create a screenshot bitmap",
            ));
        }

        let previous_object = unsafe { SelectObject(memory_dc, bitmap.into()) };
        if previous_object.is_invalid() {
            unsafe {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(memory_dc);
            }
            return Err(AdapterError::new(
                "Windows could not select the screenshot bitmap",
            ));
        }

        let result = (|| {
            unsafe {
                BitBlt(
                    memory_dc,
                    0,
                    0,
                    width,
                    height,
                    Some(screen_dc),
                    0,
                    0,
                    SRCCOPY,
                )
                .map_err(|_| AdapterError::new("Windows could not capture the screen"))?;
            }

            let pixels = bitmap_pixels(memory_dc, bitmap, width, height)?;
            write_bmp(path, width, height, &pixels)?;
            Ok(Artifact {
                kind: cueflow_core::ArtifactKind::Screenshot,
                uri: format!("file://{}", strip_verbatim_path_prefix(&path_display(path))),
                label: Some("Desktop screenshot".to_string()),
            })
        })();

        unsafe {
            let _ = SelectObject(memory_dc, previous_object);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(memory_dc);
        }
        result
    })();

    unsafe {
        let _ = ReleaseDC(Some(desktop), screen_dc);
    }
    result
}

fn window_screenshot_bmp_size(window: HWND) -> Result<u64, AdapterError> {
    let rect = window_bounds(window)
        .ok_or_else(|| AdapterError::new("Windows could not determine window bounds"))?;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return Err(AdapterError::new(
            "Windows window has empty screenshot bounds",
        ));
    }
    bmp_file_size(width, height)
}

fn capture_window_screenshot(window: HWND, path: &Path) -> Result<Artifact, AdapterError> {
    capture_window_screenshot_with_limit(window, path, None)
}

fn capture_window_screenshot_with_limit(
    window: HWND,
    path: &Path,
    evidence_config: Option<&RunConfig>,
) -> Result<Artifact, AdapterError> {
    let bitmap = capture_window_bitmap(window, evidence_config)?;
    write_bmp(path, bitmap.width, bitmap.height, &bitmap.pixels)?;
    Ok(Artifact {
        kind: cueflow_core::ArtifactKind::Screenshot,
        uri: format!("file://{}", strip_verbatim_path_prefix(&path_display(path))),
        label: Some("Window screenshot".to_string()),
    })
}

fn capture_window_bitmap(
    window: HWND,
    evidence_config: Option<&RunConfig>,
) -> Result<BmpImage, AdapterError> {
    let rect = window_bounds(window)
        .ok_or_else(|| AdapterError::new("Windows could not determine window bounds"))?;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return Err(AdapterError::new(
            "Windows window has empty screenshot bounds",
        ));
    }
    if let Some(config) = evidence_config {
        enforce_evidence_artifact_size(bmp_file_size(width, height)?, config)?;
    }
    let window_dc = unsafe { GetWindowDC(Some(window)) };
    if window_dc.is_invalid() {
        return Err(AdapterError::new(
            "Windows could not acquire the window device context",
        ));
    }

    let result = (|| {
        let memory_dc = unsafe { CreateCompatibleDC(Some(window_dc)) };
        if memory_dc.is_invalid() {
            return Err(AdapterError::new(
                "Windows could not create a window screenshot device context",
            ));
        }
        let bitmap = unsafe { CreateCompatibleBitmap(window_dc, width, height) };
        if bitmap.is_invalid() {
            unsafe {
                let _ = DeleteDC(memory_dc);
            }
            return Err(AdapterError::new(
                "Windows could not create a window screenshot bitmap",
            ));
        }

        let previous_object = unsafe { SelectObject(memory_dc, bitmap.into()) };
        if previous_object.is_invalid() {
            unsafe {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(memory_dc);
            }
            return Err(AdapterError::new(
                "Windows could not select the window screenshot bitmap",
            ));
        }

        let result = (|| {
            unsafe {
                BitBlt(
                    memory_dc,
                    0,
                    0,
                    width,
                    height,
                    Some(window_dc),
                    0,
                    0,
                    SRCCOPY,
                )
                .map_err(|_| AdapterError::new("Windows could not capture the window"))?;
            }

            let pixels = bitmap_pixels(memory_dc, bitmap, width, height)?;
            Ok(BmpImage {
                width,
                height,
                pixels,
            })
        })();

        unsafe {
            let _ = SelectObject(memory_dc, previous_object);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(memory_dc);
        }
        result
    })();

    unsafe {
        let _ = ReleaseDC(Some(window), window_dc);
    }
    result
}

fn bitmap_pixels(
    memory_dc: windows::Win32::Graphics::Gdi::HDC,
    bitmap: windows::Win32::Graphics::Gdi::HBITMAP,
    width: i32,
    height: i32,
) -> Result<Vec<u8>, AdapterError> {
    let buffer_size = width as usize * height as usize * 4;
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: buffer_size as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u8; buffer_size];
    let rows = unsafe {
        GetDIBits(
            memory_dc,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    if rows == 0 {
        return Err(transient_error("Windows could not read screenshot pixels"));
    }
    Ok(pixels)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BmpImage {
    width: i32,
    height: i32,
    pixels: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VisualMatch {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    confidence: u8,
}

fn bmp_file_size(width: i32, height: i32) -> Result<u64, AdapterError> {
    let width =
        u64::try_from(width).map_err(|_| transient_error("Windows screenshot width is invalid"))?;
    let height = u64::try_from(height)
        .map_err(|_| transient_error("Windows screenshot height is invalid"))?;
    let pixels = width
        .checked_mul(height)
        .and_then(|value| value.checked_mul(4u64))
        .ok_or_else(|| transient_error("Windows screenshot size overflowed"))?;
    (14u64 + 40u64)
        .checked_add(pixels)
        .ok_or_else(|| transient_error("Windows screenshot size overflowed"))
}

fn write_bmp(path: &Path, width: i32, height: i32, pixels: &[u8]) -> Result<(), AdapterError> {
    let header_size = 14usize + 40usize;
    let file_size = header_size + pixels.len();
    let mut output = Vec::with_capacity(file_size);
    output.extend_from_slice(b"BM");
    output.extend_from_slice(&(file_size as u32).to_le_bytes());
    output.extend_from_slice(&[0, 0, 0, 0]);
    output.extend_from_slice(&(header_size as u32).to_le_bytes());
    output.extend_from_slice(&40u32.to_le_bytes());
    output.extend_from_slice(&width.to_le_bytes());
    output.extend_from_slice(&(-height).to_le_bytes());
    output.extend_from_slice(&1u16.to_le_bytes());
    output.extend_from_slice(&32u16.to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    output.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
    output.extend_from_slice(&2835u32.to_le_bytes());
    output.extend_from_slice(&2835u32.to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    output.extend_from_slice(pixels);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| {
            transient_error("Windows could not create the screenshot output directory")
        })?;
    }
    fs::write(path, output).map_err(|_| transient_error("Windows could not write screenshot"))
}

fn read_bmp_image(path: &Path) -> Result<BmpImage, AdapterError> {
    let bytes =
        fs::read(path).map_err(|_| AdapterError::new("Windows could not read image target"))?;
    parse_bmp_image(&bytes)
}

fn parse_bmp_image(bytes: &[u8]) -> Result<BmpImage, AdapterError> {
    if bytes.len() < 54 || &bytes[0..2] != b"BM" {
        return Err(AdapterError::new(
            "image target must be an uncompressed 32bpp BMP",
        ));
    }
    let pixel_offset = read_u32_le(bytes, 10)? as usize;
    let dib_size = read_u32_le(bytes, 14)?;
    if dib_size < 40 || bytes.len() < pixel_offset {
        return Err(AdapterError::new("image target BMP header is invalid"));
    }
    let width = read_i32_le(bytes, 18)?;
    let raw_height = read_i32_le(bytes, 22)?;
    let planes = read_u16_le(bytes, 26)?;
    let bits_per_pixel = read_u16_le(bytes, 28)?;
    let compression = read_u32_le(bytes, 30)?;
    if width <= 0 || raw_height == 0 || planes != 1 || bits_per_pixel != 32 || compression != 0 {
        return Err(AdapterError::new(
            "image target must be an uncompressed 32bpp BMP",
        ));
    }

    let height = raw_height.unsigned_abs() as i32;
    let row_stride = width as usize * 4;
    let pixel_len = row_stride
        .checked_mul(height as usize)
        .ok_or_else(|| AdapterError::new("image target BMP dimensions overflowed"))?;
    if bytes.len() < pixel_offset + pixel_len {
        return Err(AdapterError::new(
            "image target BMP pixel data is truncated",
        ));
    }

    let mut pixels = vec![0u8; pixel_len];
    let source = &bytes[pixel_offset..pixel_offset + pixel_len];
    if raw_height < 0 {
        pixels.copy_from_slice(source);
    } else {
        for row in 0..height as usize {
            let source_start = (height as usize - 1 - row) * row_stride;
            let target_start = row * row_stride;
            pixels[target_start..target_start + row_stride]
                .copy_from_slice(&source[source_start..source_start + row_stride]);
        }
    }

    Ok(BmpImage {
        width,
        height,
        pixels,
    })
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16, AdapterError> {
    bytes
        .get(offset..offset + 2)
        .and_then(|value| value.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| AdapterError::new("image target BMP header is truncated"))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, AdapterError> {
    bytes
        .get(offset..offset + 4)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| AdapterError::new("image target BMP header is truncated"))
}

fn read_i32_le(bytes: &[u8], offset: usize) -> Result<i32, AdapterError> {
    bytes
        .get(offset..offset + 4)
        .and_then(|value| value.try_into().ok())
        .map(i32::from_le_bytes)
        .ok_or_else(|| AdapterError::new("image target BMP header is truncated"))
}

fn find_template_match(
    screenshot: &BmpImage,
    template: &BmpImage,
    image: &ImageTarget,
) -> Result<Option<VisualMatch>, AdapterError> {
    if template.width <= 0
        || template.height <= 0
        || template.width > screenshot.width
        || template.height > screenshot.height
    {
        return Ok(None);
    }
    let threshold = image.confidence.unwrap_or(100).min(100);
    let (left, top, right, bottom) =
        match visual_search_bounds(screenshot, template, image.region.as_ref()) {
            Some(bounds) => bounds,
            None => return Ok(None),
        };
    enforce_visual_search_budget(left, top, right, bottom, template)?;
    let mut best = None;
    for y in top..=bottom {
        for x in left..=right {
            let confidence = template_confidence(screenshot, template, x, y);
            if confidence >= threshold {
                return Ok(Some(VisualMatch {
                    left: x,
                    top: y,
                    width: template.width,
                    height: template.height,
                    confidence,
                }));
            }
            if best
                .as_ref()
                .is_none_or(|candidate: &VisualMatch| confidence > candidate.confidence)
            {
                best = Some(VisualMatch {
                    left: x,
                    top: y,
                    width: template.width,
                    height: template.height,
                    confidence,
                });
            }
        }
    }
    Ok(best.filter(|candidate| candidate.confidence >= threshold))
}

fn visual_search_bounds(
    screenshot: &BmpImage,
    template: &BmpImage,
    region: Option<&ImageRegion>,
) -> Option<(i32, i32, i32, i32)> {
    let max_left = screenshot.width - template.width;
    let max_top = screenshot.height - template.height;
    match region {
        Some(region) => {
            let region_right = region.left.checked_add(i32::try_from(region.width).ok()?)?;
            let region_bottom = region.top.checked_add(i32::try_from(region.height).ok()?)?;
            let left = region.left.max(0);
            let top = region.top.max(0);
            let right = (region_right - template.width).min(max_left);
            let bottom = (region_bottom - template.height).min(max_top);
            (left <= right && top <= bottom).then_some((left, top, right, bottom))
        }
        None => Some((0, 0, max_left, max_top)),
    }
}

fn enforce_visual_search_budget(
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    template: &BmpImage,
) -> Result<(), AdapterError> {
    let positions = u64::try_from(right - left + 1)
        .ok()
        .and_then(|width| {
            u64::try_from(bottom - top + 1)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| AdapterError::new("image target search bounds overflowed"))?;
    let template_pixels = u64::try_from(template.width)
        .ok()
        .and_then(|width| {
            u64::try_from(template.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| AdapterError::new("image target template size overflowed"))?;
    let comparisons = positions
        .checked_mul(template_pixels)
        .ok_or_else(|| AdapterError::new("image target search budget overflowed"))?;
    if comparisons > VISUAL_MATCH_MAX_PIXEL_COMPARISONS {
        return Err(AdapterError::unsupported(
            "image target search is too large; provide a bounded image region or smaller template",
        ));
    }
    Ok(())
}

fn template_confidence(screenshot: &BmpImage, template: &BmpImage, left: i32, top: i32) -> u8 {
    let mut matching = 0usize;
    let total = template.width as usize * template.height as usize;
    for y in 0..template.height {
        for x in 0..template.width {
            if rgb_at(screenshot, left + x, top + y) == rgb_at(template, x, y) {
                matching += 1;
            }
        }
    }
    ((matching * 100) / total) as u8
}

fn rgb_at(image: &BmpImage, x: i32, y: i32) -> &[u8] {
    let offset = ((y as usize * image.width as usize) + x as usize) * 4;
    &image.pixels[offset..offset + 3]
}

fn path_display(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn focus_window(target: &Target) -> Result<(), AdapterError> {
    let window = find_window(target)?;
    unsafe {
        if !SetForegroundWindow(window).as_bool() {
            return Err(focus_denied_error(
                "Windows could not focus the requested window",
            ));
        }
    }
    wait_for_foreground(window, "requested window")
}

fn wait_for_foreground(window: HWND, label: &str) -> Result<(), AdapterError> {
    let started_at = Instant::now();
    while started_at.elapsed() < Duration::from_millis(750) {
        if unsafe { GetForegroundWindow() == window } {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(focus_denied_error(format!(
        "Windows did not foreground the {label}"
    )))
}

fn window_exists(target: &Target) -> Result<bool, AdapterError> {
    match find_window(target) {
        Ok(_) => Ok(true),
        Err(error) if has_failure_kind(&error, FailureKind::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

fn window_is_focused(target: &Target) -> Result<bool, AdapterError> {
    let window = match find_window(target) {
        Ok(window) => window,
        Err(error) if has_failure_kind(&error, FailureKind::NotFound) => return Ok(false),
        Err(error) => return Err(error),
    };

    Ok(unsafe { GetForegroundWindow() == window })
}

fn process_is_running(target: &Target) -> Result<bool, AdapterError> {
    let process_name = process_name(target)?;
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|_| AdapterError::new("Windows could not enumerate processes"))?;
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(AdapterError::new("Windows could not enumerate processes"));
    }

    let result = (|| {
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if unsafe { Process32FirstW(snapshot, &mut entry) }.is_err() {
            return Ok(false);
        }

        loop {
            if process_entry_name(&entry).eq_ignore_ascii_case(process_name) {
                return Ok(true);
            }
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                return Ok(false);
            }
        }
    })();
    unsafe {
        CloseHandle(snapshot)
            .map_err(|_| AdapterError::new("Windows could close the process snapshot"))?;
    }
    result
}

fn focus_target(target: &Target, _config: &RunConfig) -> Result<(), AdapterError> {
    let window = find_semantic_window(target)?;
    unsafe {
        if !SetForegroundWindow(window).as_bool() {
            return Err(AdapterError::new(
                "Windows could not foreground the semantic target window",
            ));
        }
    }
    wait_for_foreground(window, "semantic target window")?;
    with_semantic_target(target, |element| unsafe {
        ensure_semantic_target_actionable(element)?;
        element.SetFocus().map_err(|_| {
            AdapterError::new("Windows could not focus the requested semantic target")
        })?;
        if element
            .CurrentHasKeyboardFocus()
            .map_err(|_| AdapterError::new("Windows could verify semantic target focus"))?
            .as_bool()
        {
            Ok(())
        } else {
            Err(AdapterError::new(
                "Windows did not give keyboard focus to the requested semantic target",
            )
            .with_failure_kind(FailureKind::FocusDenied)
            .with_source("failureKind=focusDenied"))
        }
    })
}

fn semantic_search_diagnostics(
    target: &Target,
    accessibility: &cueflow_core::AccessibilityTarget,
    matched: &[String],
    inspected: &[String],
) -> String {
    format!(
        "window selector: {}; accessibility selector: {}; matched elements: {}; inspected elements: {}",
        window_target_summary(target),
        accessibility_summary(accessibility),
        list_or_none(matched),
        list_or_none(inspected)
    )
}

fn accessibility_summary(accessibility: &cueflow_core::AccessibilityTarget) -> String {
    let mut fields = Vec::new();
    if let Some(id) = &accessibility.id {
        fields.push(format!("id={}", quote(id)));
    }
    if let Some(name) = &accessibility.name {
        fields.push(format!("name={}", quote(name)));
    }
    if let Some(control_type) = &accessibility.control_type {
        fields.push(format!("controlType={}", quote(control_type)));
    }
    if let Some(path) = &accessibility.path {
        fields.push(format!("path={}", format_accessibility_path(path)));
    }
    fields.join(", ")
}

fn element_summary(element: &IUIAutomationElement, path: &[u32]) -> Result<String, AdapterError> {
    let id = unsafe {
        element
            .CurrentAutomationId()
            .map_err(|_| AdapterError::new("Windows could read a target automation id"))?
    };
    let name = unsafe {
        element
            .CurrentName()
            .map_err(|_| AdapterError::new("Windows could read a target name"))?
    };
    let control_type = unsafe {
        element
            .CurrentLocalizedControlType()
            .map_err(|_| AdapterError::new("Windows could read a target control type"))?
    };
    Ok(format!(
        "path={}, id={}, name={}, controlType={}, clickPoint={}",
        format_accessibility_path(path),
        quote(&id.to_string()),
        quote(&name.to_string()),
        quote(&control_type.to_string()),
        current_bounds(element)
            .and_then(click_point_for_bounds)
            .map(|point| format!("{},{}", point.x, point.y))
            .unwrap_or_else(|| "unknown".to_string())
    ))
}

fn format_accessibility_path(path: &[u32]) -> String {
    if path.is_empty() {
        "[]".to_string()
    } else {
        format!(
            "[{}]",
            path.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

fn list_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join("; ")
    }
}

fn quote(value: &str) -> String {
    format!("{value:?}")
}

fn process_name(target: &Target) -> Result<&str, AdapterError> {
    if target.app_name.is_some()
        || target.window_title.is_some()
        || target.title_contains.is_some()
        || target.url.is_some()
        || target.file_path.is_some()
        || target.accessibility.is_some()
        || target.image.is_some()
        || target.coordinates.is_some()
        || !target.platform_selectors.is_empty()
    {
        return Err(AdapterError::unsupported(
            "Windows process queries currently support only a processName selector",
        ));
    }
    target.process_name.as_deref().ok_or_else(|| {
        AdapterError::unsupported("Windows process queries require a processName selector")
    })
}

fn process_entry_name(entry: &PROCESSENTRY32W) -> String {
    let end = entry
        .szExeFile
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(entry.szExeFile.len());
    String::from_utf16_lossy(&entry.szExeFile[..end])
}

fn run_command(
    command: &str,
    args: &[String],
    config: &RunConfig,
    control: &RunControl,
    timeout: Option<Duration>,
) -> Result<(), AdapterError> {
    let status = command_status(command, args, config, control, timeout)?;
    if status.success() {
        Ok(())
    } else {
        Err(AdapterError::new("approved command exited unsuccessfully"))
    }
}

fn command_exits(
    command: &str,
    args: &[String],
    config: &RunConfig,
    control: &RunControl,
    timeout: Option<Duration>,
) -> Result<bool, AdapterError> {
    command_status(command, args, config, control, timeout).map(|status| status.success())
}

fn command_status(
    executable: &str,
    args: &[String],
    config: &RunConfig,
    control: &RunControl,
    timeout: Option<Duration>,
) -> Result<ExitStatus, AdapterError> {
    if !config.approved_commands.contains(executable) {
        return Err(AdapterError::unsupported(
            "command is not approved for this run",
        ));
    }

    let mut command = Command::new(executable);
    command.args(args).envs(&config.environment);
    if let Some(working_directory) = &config.working_directory {
        command.current_dir(working_directory);
    }
    let child = command
        .spawn()
        .map_err(|_| transient_error("Windows could not start the approved command"))?;
    let job = CommandJob::assign(&child)?;
    wait_for_command(child, &job, control, timeout)
}

fn wait_for_command(
    mut child: Child,
    job: &CommandJob,
    control: &RunControl,
    timeout: Option<Duration>,
) -> Result<ExitStatus, AdapterError> {
    let started_at = Instant::now();
    loop {
        if control.is_cancelled() {
            terminate_command(&mut child, job)?;
            return Err(AdapterError::cancelled());
        }
        if timeout.is_some_and(|timeout| started_at.elapsed() >= timeout) {
            terminate_command(&mut child, job)?;
            return Err(AdapterError::timeout());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|_| transient_error("Windows could not observe the approved command"))?
        {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn terminate_command(child: &mut Child, job: &CommandJob) -> Result<(), AdapterError> {
    unsafe { TerminateJobObject(job.handle, 1) }
        .map_err(|_| transient_error("Windows could not stop the approved command tree"))?;
    child
        .wait()
        .map_err(|_| transient_error("Windows could reap the approved command"))?;
    Ok(())
}

struct CommandJob {
    handle: windows::Win32::Foundation::HANDLE,
}

impl CommandJob {
    fn assign(child: &Child) -> Result<Self, AdapterError> {
        let handle = unsafe { CreateJobObjectW(None, None) }
            .map_err(|_| transient_error("Windows could not create a command job"))?;
        let job = Self { handle };
        let process = windows::Win32::Foundation::HANDLE(child.as_raw_handle());
        unsafe { AssignProcessToJobObject(job.handle, process) }
            .map_err(|_| transient_error("Windows could assign the command to its job"))?;
        Ok(job)
    }
}

impl Drop for CommandJob {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

fn find_window(target: &Target) -> Result<HWND, AdapterError> {
    let matcher = WindowTitleMatcher::from_target(target)?;
    let mut search = WindowSearch {
        matcher,
        matches: Vec::new(),
    };
    unsafe {
        EnumWindows(
            Some(enumerate_matching_windows),
            LPARAM((&mut search as *mut WindowSearch<'_>) as isize),
        )
        .map_err(|_| AdapterError::new("Windows could not enumerate top-level windows"))?;
    }

    match search.matches.as_slice() {
        [] => Err(AdapterError::new("requested window was not found")
            .with_failure_kind(FailureKind::NotFound)
            .with_source(format!(
                "failureKind=notFound; {}",
                window_search_diagnostics(target, &search.matches)
            ))),
        [window] => Ok(*window),
        _ => Err(
            AdapterError::new("requested window selector matched multiple visible windows")
                .with_failure_kind(FailureKind::Ambiguous)
                .with_source(format!(
                    "failureKind=ambiguous; {}",
                    window_search_diagnostics(target, &search.matches)
                )),
        ),
    }
}

struct WindowSearch<'a> {
    matcher: WindowTitleMatcher<'a>,
    matches: Vec<HWND>,
}

enum WindowTitleMatcher<'a> {
    Exact(&'a str),
    Contains(&'a str),
}

impl<'a> WindowTitleMatcher<'a> {
    fn from_target(target: &'a Target) -> Result<Self, AdapterError> {
        match (
            target.window_title.as_deref(),
            target.title_contains.as_deref(),
        ) {
            (Some(title), None) => Ok(Self::Exact(title)),
            (None, Some(title)) => Ok(Self::Contains(title)),
            (Some(_), Some(_)) => Err(AdapterError::unsupported(
                "Windows window queries require exactly one of windowTitle or titleContains",
            )),
            (None, None) => Err(AdapterError::unsupported(
                "Windows window queries require windowTitle or titleContains",
            )),
        }
    }

    fn matches(&self, window_title: &str) -> bool {
        let window_title = normalize_window_title(window_title);
        match self {
            Self::Exact(title) => window_title.eq_ignore_ascii_case(&normalize_window_title(title)),
            Self::Contains(title) => window_title
                .to_lowercase()
                .contains(&normalize_window_title(title).to_lowercase()),
        }
    }
}

fn normalize_window_title(title: &str) -> String {
    let mut normalized = String::with_capacity(title.len());
    let mut separator_pending = false;

    for character in title.chars() {
        if character.is_whitespace()
            || matches!(character, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}')
        {
            separator_pending = !normalized.is_empty();
        } else {
            if separator_pending {
                normalized.push(' ');
                separator_pending = false;
            }
            normalized.push(character);
        }
    }

    normalized
}

fn scroll_amount(delta: i32) -> ScrollAmount {
    match delta.cmp(&0) {
        std::cmp::Ordering::Less => ScrollAmount_SmallDecrement,
        std::cmp::Ordering::Equal => ScrollAmount_NoAmount,
        std::cmp::Ordering::Greater => ScrollAmount_SmallIncrement,
    }
}

unsafe extern "system" fn enumerate_matching_windows(window: HWND, lparam: LPARAM) -> BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut WindowSearch<'_>) };
    if unsafe { !IsWindowVisible(window).as_bool() } {
        return BOOL(1);
    }

    if let Ok(title) = window_title(window)
        && search.matcher.matches(&title)
    {
        search.matches.push(window);
    }
    BOOL(1)
}

fn window_title(window: HWND) -> Result<String, AdapterError> {
    let length = unsafe { GetWindowTextLengthW(window) };
    if length == 0 {
        return Ok(String::new());
    }

    let mut buffer = vec![0; length as usize + 1];
    let copied = unsafe { GetWindowTextW(window, &mut buffer) };
    if copied == 0 {
        return Err(AdapterError::new(
            "Windows could not read a visible window title",
        ));
    }
    Ok(String::from_utf16_lossy(&buffer[..copied as usize]))
}

fn window_identity(window: HWND) -> Option<WindowIdentity> {
    let title = window_title(window).ok()?;
    let mut class_buffer = vec![0; 256];
    let class_length = unsafe { GetClassNameW(window, &mut class_buffer) };
    let class_name = if class_length == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&class_buffer[..class_length as usize])
    };

    let mut process_id = 0;
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut process_id));
    }
    let bounds = window_bounds(window);

    Some(WindowIdentity {
        handle: format!("{window:?}"),
        title,
        class_name,
        process_id,
        process_name: process_name_by_id(process_id).ok().flatten(),
        bounds,
        is_foreground: unsafe { GetForegroundWindow() == window },
        is_minimized: unsafe { IsIconic(window).as_bool() },
        owner: window_owner(window).map(|owner| format!("{owner:?}")),
    })
}

fn window_owner(window: HWND) -> Option<HWND> {
    let owner = unsafe { GetWindow(window, GW_OWNER) }.ok()?;
    (owner.0 != std::ptr::null_mut()).then_some(owner)
}

fn window_bounds(window: HWND) -> Option<AccessibilityBounds> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(window, &mut rect) }.ok()?;
    Some(AccessibilityBounds {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    })
}

fn process_name_by_id(process_id: u32) -> Result<Option<String>, AdapterError> {
    if process_id == 0 {
        return Ok(None);
    }
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|_| AdapterError::new("Windows could not enumerate processes"))?;
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(AdapterError::new("Windows could not enumerate processes"));
    }

    let result = (|| {
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if unsafe { Process32FirstW(snapshot, &mut entry) }.is_err() {
            return Ok(None);
        }

        loop {
            if entry.th32ProcessID == process_id {
                return Ok(Some(process_entry_name(&entry)));
            }
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                return Ok(None);
            }
        }
    })();
    unsafe {
        CloseHandle(snapshot).ok();
    }
    result
}

fn semantic_target_exists(target: &Target) -> Result<bool, AdapterError> {
    match with_semantic_target(target, |_| Ok(())) {
        Ok(()) => Ok(true),
        Err(error) if is_target_absent(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn image_target_exists(target: &Target, config: &RunConfig) -> Result<bool, AdapterError> {
    visual_match(target, config).map(|match_result| match_result.is_some())
}

fn click_image_target(target: &Target, config: &RunConfig) -> Result<(), AdapterError> {
    let window = find_window(target)?;
    let bounds = window_bounds(window)
        .ok_or_else(|| AdapterError::new("Windows could not determine window bounds"))?;
    let matched = visual_match_in_window(target, config, window)?.ok_or_else(|| {
        AdapterError::new("requested image target was not found")
            .with_failure_kind(FailureKind::NotFound)
            .with_source("failureKind=notFound; visualTarget=image")
    })?;
    click_screen_point(
        bounds.left + matched.left + (matched.width / 2),
        bounds.top + matched.top + (matched.height / 2),
    )
}

fn visual_match(target: &Target, config: &RunConfig) -> Result<Option<VisualMatch>, AdapterError> {
    let window = find_window(target)?;
    visual_match_in_window(target, config, window)
}

fn visual_match_in_window(
    target: &Target,
    config: &RunConfig,
    window: HWND,
) -> Result<Option<VisualMatch>, AdapterError> {
    reject_unsupported_target(unsupported_image_action_target_reason(target, config))?;
    let image = target
        .image
        .as_ref()
        .ok_or_else(|| AdapterError::unsupported("visual matching requires an image target"))?;
    let screenshot = capture_window_bitmap(window, Some(config))?;
    let template = read_bmp_image(Path::new(&image.path))?;
    find_template_match(&screenshot, &template, image)
}

fn ensure_semantic_target_actionable(element: &IUIAutomationElement) -> Result<(), AdapterError> {
    let enabled = unsafe {
        element
            .CurrentIsEnabled()
            .map_err(|_| AdapterError::new("Windows could not verify target enabled state"))?
            .as_bool()
    };
    if !enabled {
        return Err(AdapterError::new("semantic target is disabled")
            .with_failure_kind(FailureKind::Disabled)
            .with_source("failureKind=disabled"));
    }

    let offscreen = unsafe {
        element
            .CurrentIsOffscreen()
            .map_err(|_| AdapterError::new("Windows could not verify target visibility state"))?
            .as_bool()
    };
    if offscreen {
        return Err(AdapterError::new("semantic target is offscreen")
            .with_failure_kind(FailureKind::Offscreen)
            .with_source("failureKind=offscreen"));
    }

    let bounds = unsafe {
        element
            .CurrentBoundingRectangle()
            .map_err(|_| AdapterError::new("Windows could not verify target bounds"))?
    };
    if bounds.right <= bounds.left || bounds.bottom <= bounds.top {
        return Err(AdapterError::new("semantic target has empty bounds")
            .with_failure_kind(FailureKind::Offscreen)
            .with_source("failureKind=offscreen; bounds=empty"));
    }

    Ok(())
}

fn target_exists_state(target: &Target) -> Result<ConditionState, AdapterError> {
    semantic_target_exists(target).map(condition_state)
}

fn condition_state(satisfied: bool) -> ConditionState {
    if satisfied {
        ConditionState::Satisfied
    } else {
        ConditionState::Pending
    }
}

fn semantic_target_focused(target: &Target) -> Result<bool, AdapterError> {
    semantic_target_readiness(target, |element| unsafe {
        element
            .CurrentHasKeyboardFocus()
            .map(|focused| focused.as_bool())
            .map_err(|_| AdapterError::new("Windows could read semantic target focus state"))
    })
}

fn semantic_target_enabled(target: &Target) -> Result<bool, AdapterError> {
    semantic_target_readiness(target, |element| unsafe {
        element
            .CurrentIsEnabled()
            .map(|enabled| enabled.as_bool())
            .map_err(|_| AdapterError::new("Windows could read semantic target enabled state"))
    })
}

fn semantic_target_visible(target: &Target) -> Result<bool, AdapterError> {
    semantic_target_readiness(target, |element| unsafe {
        element
            .CurrentIsOffscreen()
            .map(|offscreen| !offscreen.as_bool())
            .map_err(|_| AdapterError::new("Windows could read semantic target visibility state"))
    })
}

fn semantic_target_actionable(target: &Target) -> Result<bool, AdapterError> {
    semantic_target_readiness(target, |element| unsafe {
        let offscreen = element
            .CurrentIsOffscreen()
            .map_err(|_| AdapterError::new("Windows could read semantic target visibility state"))?
            .as_bool();
        if offscreen {
            return Ok(false);
        }
        let control_type = element
            .CurrentLocalizedControlType()
            .map_err(|_| AdapterError::new("Windows could read semantic target control type"))?;
        if normalize_window_title(&control_type.to_string()).eq_ignore_ascii_case("window") {
            return Ok(true);
        }
        let bounds = element
            .CurrentBoundingRectangle()
            .map_err(|_| AdapterError::new("Windows could read semantic target bounds"))?;
        if bounds.right <= bounds.left || bounds.bottom <= bounds.top {
            return Ok(false);
        }
        element
            .CurrentIsEnabled()
            .map(|enabled| enabled.as_bool())
            .map_err(|_| AdapterError::new("Windows could read semantic target enabled state"))
    })
}

fn semantic_target_name_contains(target: &Target, text: &str) -> Result<bool, AdapterError> {
    semantic_target_readiness(target, |element| unsafe {
        element
            .CurrentName()
            .map(|name| name.to_string().contains(text))
            .map_err(|_| AdapterError::new("Windows could read semantic target name"))
    })
}

fn semantic_target_value_contains(target: &Target, text: &str) -> Result<bool, AdapterError> {
    semantic_target_readiness(target, |element| {
        Ok(current_value(element, true)
            .as_deref()
            .is_some_and(|value| value.contains(text)))
    })
}

fn semantic_target_readiness(
    target: &Target,
    operation: impl FnOnce(&IUIAutomationElement) -> Result<bool, AdapterError>,
) -> Result<bool, AdapterError> {
    match with_semantic_target(target, operation) {
        Ok(satisfied) => Ok(satisfied),
        Err(error) if is_target_absent(&error) => Ok(false),
        Err(error) if is_transient_readiness_error(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn is_target_absent(error: &AdapterError) -> bool {
    has_failure_kind(error, FailureKind::NotFound)
}

fn is_transient_readiness_error(error: &AdapterError) -> bool {
    matches!(error.failure_kind(), Some(FailureKind::Transient))
        || (error.diagnostics().is_none() && error.to_string().starts_with("Windows could "))
}

fn has_failure_kind(error: &AdapterError, kind: FailureKind) -> bool {
    error.failure_kind() == Some(kind)
}

fn inspect_accessibility_node(
    element: &IUIAutomationElement,
    condition: &IUIAutomationCondition,
    path: &[u32],
    window_title: &str,
    depth_remaining: u32,
    include_values: bool,
    remaining_nodes: &mut usize,
    truncated: &mut bool,
) -> Result<AccessibilityNode, AdapterError> {
    if *remaining_nodes == 0 {
        *truncated = true;
        return Err(AdapterError::new(
            "Windows accessibility inspection exceeded the node limit",
        ));
    }
    *remaining_nodes -= 1;

    let mut children = Vec::new();
    if depth_remaining > 0 {
        let Ok(child_elements) = (unsafe { element.FindAll(TreeScope_Children, condition) }) else {
            *truncated = true;
            return Ok(inspected_accessibility_node(
                element,
                path,
                window_title,
                include_values,
                children,
            ));
        };
        let Ok(child_count) = (unsafe { child_elements.Length() }) else {
            *truncated = true;
            return Ok(inspected_accessibility_node(
                element,
                path,
                window_title,
                include_values,
                children,
            ));
        };
        for index in 0..child_count {
            if *remaining_nodes == 0 {
                *truncated = true;
                break;
            }
            let Ok(child) = (unsafe { child_elements.GetElement(index) }) else {
                *truncated = true;
                continue;
            };
            children.push(inspect_accessibility_node(
                &child,
                condition,
                &[path, &[index as u32]].concat(),
                window_title,
                depth_remaining - 1,
                include_values,
                remaining_nodes,
                truncated,
            )?);
        }
    } else {
        match has_children(element, condition) {
            Ok(true) => *truncated = true,
            Ok(false) => {}
            Err(_) => *truncated = true,
        }
    }

    Ok(inspected_accessibility_node(
        element,
        path,
        window_title,
        include_values,
        children,
    ))
}

fn inspected_accessibility_node(
    element: &IUIAutomationElement,
    path: &[u32],
    window_title: &str,
    include_values: bool,
    children: Vec<AccessibilityNode>,
) -> AccessibilityNode {
    let name = current_bstr(element, |element| unsafe { element.CurrentName() });
    let automation_id = current_bstr(element, |element| unsafe { element.CurrentAutomationId() });
    let control_type = current_bstr(element, |element| unsafe {
        element.CurrentLocalizedControlType()
    });
    let bounds = current_bounds(element);
    AccessibilityNode {
        path: path.to_vec(),
        depth: path.len() as u32,
        name: name.clone(),
        automation_id: automation_id.clone(),
        control_type: control_type.clone(),
        class_name: current_bstr(element, |element| unsafe { element.CurrentClassName() }),
        bounds,
        click_point: bounds.and_then(click_point_for_bounds),
        enabled: current_bool(element, |element| unsafe { element.CurrentIsEnabled() }),
        keyboard_focusable: current_bool(element, |element| unsafe {
            element.CurrentIsKeyboardFocusable()
        }),
        has_keyboard_focus: current_bool(element, |element| unsafe {
            element.CurrentHasKeyboardFocus()
        }),
        value: current_value(element, include_values),
        actions: current_actions(element),
        selector_candidates: selector_candidates(
            window_title,
            path,
            &automation_id,
            &name,
            &control_type,
        ),
        children,
    }
}

fn selector_candidates(
    window_title: &str,
    path: &[u32],
    automation_id: &str,
    name: &str,
    control_type: &str,
) -> Vec<AccessibilitySelectorCandidate> {
    let mut candidates = Vec::new();
    if !automation_id.is_empty() && !control_type.is_empty() {
        candidates.push(selector_candidate(
            window_title,
            Some(automation_id),
            None,
            Some(control_type),
            None,
            SelectorConfidence::High,
            95,
            "Automation id plus control type is usually the most stable UIA selector.",
            Vec::new(),
        ));
    }
    if !automation_id.is_empty() {
        candidates.push(selector_candidate(
            window_title,
            Some(automation_id),
            None,
            None,
            None,
            SelectorConfidence::High,
            90,
            "Automation id is stable when the application provides it.",
            Vec::new(),
        ));
    }
    if !name.is_empty() && !control_type.is_empty() {
        candidates.push(selector_candidate(
            window_title,
            None,
            Some(name),
            Some(control_type),
            None,
            SelectorConfidence::Medium,
            70,
            "Name plus control type is readable but can change with localization or content.",
            vec!["Name selectors can change with UI text, localization, or user data.".to_string()],
        ));
    }
    if !path.is_empty() {
        candidates.push(selector_candidate(
            window_title,
            None,
            None,
            if control_type.is_empty() {
                None
            } else {
                Some(control_type)
            },
            Some(path),
            SelectorConfidence::Low,
            45,
            "Path can target elements without names or ids, but sibling insertions can shift it.",
            vec![
                "Path-only selectors are positional and should be treated as fragile.".to_string(),
            ],
        ));
    }
    candidates
}

fn selector_candidate(
    window_title: &str,
    id: Option<&str>,
    name: Option<&str>,
    control_type: Option<&str>,
    path: Option<&[u32]>,
    confidence: SelectorConfidence,
    score: u8,
    rationale: &str,
    warnings: Vec<String>,
) -> AccessibilitySelectorCandidate {
    AccessibilitySelectorCandidate {
        confidence,
        score,
        target: Target {
            app_name: None,
            process_name: None,
            window_title: Some(window_title.to_string()),
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: Some(cueflow_core::AccessibilityTarget {
                id: id.map(str::to_string),
                name: name.map(str::to_string),
                control_type: control_type.map(str::to_string),
                path: path.map(<[u32]>::to_vec),
            }),
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        },
        rationale: rationale.to_string(),
        changes: Vec::new(),
        warnings,
    }
}

fn selector_candidate_changes(
    original: Option<&cueflow_core::AccessibilityTarget>,
    repaired: Option<&cueflow_core::AccessibilityTarget>,
) -> Vec<String> {
    let Some(repaired) = repaired else {
        return Vec::new();
    };

    let mut changes = Vec::new();
    push_optional_string_change(
        &mut changes,
        "id",
        original.and_then(|target| target.id.as_deref()),
        repaired.id.as_deref(),
    );
    push_optional_string_change(
        &mut changes,
        "name",
        original.and_then(|target| target.name.as_deref()),
        repaired.name.as_deref(),
    );
    push_optional_string_change(
        &mut changes,
        "controlType",
        original.and_then(|target| target.control_type.as_deref()),
        repaired.control_type.as_deref(),
    );
    let original_path = original.and_then(|target| target.path.as_deref());
    if original_path != repaired.path.as_deref() {
        changes.push(format!(
            "path: {} -> {}",
            format_optional_path(original_path),
            format_optional_path(repaired.path.as_deref())
        ));
    }
    changes
}

fn push_optional_string_change(
    changes: &mut Vec<String>,
    label: &str,
    original: Option<&str>,
    repaired: Option<&str>,
) {
    if original == repaired {
        return;
    }
    changes.push(format!(
        "{label}: {} -> {}",
        original.map(quote).unwrap_or_else(|| "none".to_string()),
        repaired.map(quote).unwrap_or_else(|| "none".to_string())
    ));
}

fn format_optional_path(path: Option<&[u32]>) -> String {
    path.map(format_accessibility_path)
        .unwrap_or_else(|| "none".to_string())
}

fn has_children(
    element: &IUIAutomationElement,
    condition: &IUIAutomationCondition,
) -> Result<bool, AdapterError> {
    let children = unsafe {
        element
            .FindAll(TreeScope_Children, condition)
            .map_err(|_| AdapterError::new("Windows could not query accessibility children"))?
    };
    let count = unsafe {
        children
            .Length()
            .map_err(|_| AdapterError::new("Windows could not count accessibility children"))?
    };
    Ok(count > 0)
}

fn current_bstr(
    element: &IUIAutomationElement,
    read: impl FnOnce(&IUIAutomationElement) -> windows::core::Result<BSTR>,
) -> String {
    read(element)
        .map(|value| value.to_string())
        .unwrap_or_default()
}

fn current_bool(
    element: &IUIAutomationElement,
    read: impl FnOnce(&IUIAutomationElement) -> windows::core::Result<BOOL>,
) -> Option<bool> {
    read(element).map(|value| value.as_bool()).ok()
}

fn current_bounds(element: &IUIAutomationElement) -> Option<AccessibilityBounds> {
    let RECT {
        left,
        top,
        right,
        bottom,
    } = unsafe { element.CurrentBoundingRectangle().ok()? };
    Some(AccessibilityBounds {
        left,
        top,
        right,
        bottom,
    })
}

fn click_point_for_bounds(bounds: AccessibilityBounds) -> Option<AccessibilityPoint> {
    if bounds.right <= bounds.left || bounds.bottom <= bounds.top {
        return None;
    }
    Some(AccessibilityPoint {
        x: bounds.left + ((bounds.right - bounds.left) / 2),
        y: bounds.top + ((bounds.bottom - bounds.top) / 2),
    })
}

fn current_value(element: &IUIAutomationElement, include_values: bool) -> Option<String> {
    if !include_values
        || current_bool(element, |element| unsafe { element.CurrentIsPassword() }) == Some(true)
    {
        return None;
    }
    let pattern = unsafe { element.GetCurrentPattern(UIA_ValuePatternId).ok()? };
    let value_pattern: IUIAutomationValuePattern = pattern.cast().ok()?;
    unsafe { value_pattern.CurrentValue().ok() }.map(|value| value.to_string())
}

fn current_actions(element: &IUIAutomationElement) -> Vec<String> {
    let mut actions = Vec::new();
    if unsafe { element.GetCurrentPattern(UIA_InvokePatternId) }.is_ok() {
        actions.push("invoke".to_string());
    }
    if unsafe { element.GetCurrentPattern(UIA_ValuePatternId) }.is_ok() {
        actions.push("setValue".to_string());
    }
    if unsafe { element.GetCurrentPattern(UIA_ScrollPatternId) }.is_ok() {
        actions.push("scroll".to_string());
    }
    actions
}

struct SemanticMatch {
    element: IUIAutomationElement,
    summary: String,
}

fn with_semantic_target<T>(
    target: &Target,
    operation: impl FnOnce(&IUIAutomationElement) -> Result<T, AdapterError>,
) -> Result<T, AdapterError> {
    let accessibility = target.accessibility.as_ref().ok_or_else(|| {
        AdapterError::unsupported("semantic target operations require an accessibility selector")
    })?;
    let window = find_semantic_window(target)?;

    let initialization = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let should_uninitialize = initialization.is_ok();
    if !should_uninitialize && initialization != RPC_E_CHANGED_MODE {
        return Err(AdapterError::new(
            "Windows could not initialize UI Automation",
        ));
    }
    let result = (|| {
        let automation: IUIAutomation = unsafe {
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .map_err(|_| AdapterError::new("Windows could not create a UI Automation client"))?
        };
        let root = unsafe {
            automation
                .ElementFromHandle(window)
                .map_err(|_| AdapterError::new("Windows could not inspect the requested window"))?
        };
        let condition = unsafe {
            automation
                .CreateTrueCondition()
                .map_err(|_| AdapterError::new("Windows could not create a UI Automation query"))?
        };
        let (matching, inspected) = if let Some(path) = &accessibility.path {
            let inspected = element_summary(&root, &[]).map(|summary| vec![summary])?;
            match find_element_by_path(&root, &condition, path) {
                Ok(element) if accessibility_matches(&element, accessibility, path)? => {
                    let summary = element_summary(&element, path)?;
                    (
                        vec![SemanticMatch {
                            element,
                            summary: summary.clone(),
                        }],
                        [inspected, vec![summary]].concat(),
                    )
                }
                Ok(element) => {
                    let summary = element_summary(&element, path)?;
                    (Vec::new(), [inspected, vec![summary]].concat())
                }
                Err(error) if is_target_absent(&error) => (Vec::new(), inspected),
                Err(error) => return Err(error),
            }
        } else {
            let mut matching = Vec::new();
            let mut inspected = Vec::new();
            let mut remaining_nodes = SEMANTIC_SEARCH_MAX_NODES;
            let mut truncated = false;
            collect_semantic_matches(
                &root,
                &condition,
                accessibility,
                &[],
                SEMANTIC_SEARCH_MAX_DEPTH,
                &mut remaining_nodes,
                &mut truncated,
                &mut matching,
                &mut inspected,
            )?;
            if truncated {
                return Err(
                    AdapterError::new("requested semantic target search was truncated")
                        .with_failure_kind(FailureKind::TruncatedSearch)
                        .with_source(format!(
                            "failureKind=truncatedSearch; {}",
                            semantic_search_diagnostics(target, accessibility, &[], &inspected)
                        )),
                );
            }
            (matching, inspected)
        };

        match matching.as_slice() {
            [] => Err(AdapterError::new("requested semantic target was not found")
                .with_failure_kind(FailureKind::NotFound)
                .with_source(format!(
                    "failureKind=notFound; {}",
                    semantic_search_diagnostics(target, accessibility, &[], &inspected)
                ))),
            [element] => operation(&element.element),
            _ => {
                let matched = matching
                    .iter()
                    .map(|matched| matched.summary.clone())
                    .collect::<Vec<_>>();
                Err(
                    AdapterError::new("requested semantic target matched multiple elements")
                        .with_failure_kind(FailureKind::Ambiguous)
                        .with_source(format!(
                            "failureKind=ambiguous; {}",
                            semantic_search_diagnostics(
                                target,
                                accessibility,
                                &matched,
                                &inspected,
                            )
                        )),
                )
            }
        }
    })();
    if should_uninitialize {
        unsafe {
            CoUninitialize();
        }
    }
    result
}

fn find_element_by_path(
    root: &IUIAutomationElement,
    condition: &IUIAutomationCondition,
    path: &[u32],
) -> Result<IUIAutomationElement, AdapterError> {
    let mut element = root.clone();
    for index in path {
        let child_elements = unsafe {
            element
                .FindAll(TreeScope_Children, condition)
                .map_err(|_| AdapterError::new("Windows could not query UI Automation children"))?
        };
        let child_count = unsafe {
            child_elements
                .Length()
                .map_err(|_| AdapterError::new("Windows could not count UI Automation children"))?
        };
        if *index >= child_count as u32 {
            return Err(semantic_target_not_found());
        }
        element = unsafe {
            child_elements
                .GetElement(*index as i32)
                .map_err(|_| semantic_target_not_found())?
        };
    }
    Ok(element)
}

fn semantic_target_not_found() -> AdapterError {
    AdapterError::new("requested semantic target was not found")
        .with_failure_kind(FailureKind::NotFound)
        .with_source("failureKind=notFound")
}

fn collect_semantic_matches(
    element: &IUIAutomationElement,
    condition: &IUIAutomationCondition,
    accessibility: &cueflow_core::AccessibilityTarget,
    path: &[u32],
    depth_remaining: u32,
    remaining_nodes: &mut usize,
    truncated: &mut bool,
    matching: &mut Vec<SemanticMatch>,
    inspected: &mut Vec<String>,
) -> Result<(), AdapterError> {
    if *remaining_nodes == 0 {
        *truncated = true;
        return Ok(());
    }
    *remaining_nodes -= 1;

    let summary = element_summary(element, path)?;
    if inspected.len() < 12 {
        inspected.push(summary.clone());
    }
    if accessibility_matches(element, accessibility, path)? {
        matching.push(SemanticMatch {
            element: element.clone(),
            summary,
        });
    }
    if depth_remaining == 0 {
        *truncated = true;
        return Ok(());
    }

    let Ok(child_elements) = (unsafe { element.FindAll(TreeScope_Children, condition) }) else {
        *truncated = true;
        return Ok(());
    };
    let Ok(child_count) = (unsafe { child_elements.Length() }) else {
        *truncated = true;
        return Ok(());
    };
    for index in 0..child_count {
        if *remaining_nodes == 0 {
            *truncated = true;
            break;
        }
        let Ok(child) = (unsafe { child_elements.GetElement(index) }) else {
            *truncated = true;
            continue;
        };
        collect_semantic_matches(
            &child,
            condition,
            accessibility,
            &[path, &[index as u32]].concat(),
            depth_remaining - 1,
            remaining_nodes,
            truncated,
            matching,
            inspected,
        )?;
    }
    Ok(())
}

fn find_semantic_window(target: &Target) -> Result<HWND, AdapterError> {
    let mut window_target = target.clone();
    window_target.accessibility = None;
    find_window(&window_target)
}

fn accessibility_matches(
    element: &IUIAutomationElement,
    accessibility: &cueflow_core::AccessibilityTarget,
    path: &[u32],
) -> Result<bool, AdapterError> {
    if let Some(expected_path) = &accessibility.path
        && expected_path != path
    {
        return Ok(false);
    }

    if let Some(id) = &accessibility.id {
        let current_id = unsafe {
            element
                .CurrentAutomationId()
                .map_err(|_| AdapterError::new("Windows could read a target automation id"))?
        };
        if current_id != id.as_str() {
            return Ok(false);
        }
    }

    if let Some(name) = &accessibility.name {
        let current_name = unsafe {
            element
                .CurrentName()
                .map_err(|_| AdapterError::new("Windows could read a target name"))?
        };
        if !normalize_window_title(&current_name.to_string())
            .eq_ignore_ascii_case(&normalize_window_title(name))
        {
            return Ok(false);
        }
    }

    if let Some(control_type) = &accessibility.control_type {
        let current_control_type = unsafe {
            element
                .CurrentLocalizedControlType()
                .map_err(|_| AdapterError::new("Windows could read a target control type"))?
        };
        if !normalize_window_title(&current_control_type.to_string())
            .eq_ignore_ascii_case(&normalize_window_title(control_type))
        {
            return Ok(false);
        }
    }

    Ok(true)
}

fn unsupported_action_reason(action: &Action, config: &RunConfig) -> Option<&'static str> {
    match action {
        Action::LaunchUrl {
            target: Some(target),
            ..
        }
        | Action::LaunchApp {
            target: Some(target),
            ..
        }
        | Action::OpenFile {
            target: Some(target),
            ..
        } => unsupported_launch_target_reason(target, config),
        Action::FocusWindow { target } => {
            unsupported_window_target_reason_with_config(target, config)
        }
        Action::ClickTarget { target } if target.coordinates.is_some() => {
            unsupported_coordinate_target_reason(target, config)
        }
        Action::ClickTarget { target } if target.image.is_some() => {
            unsupported_image_action_target_reason(target, config)
        }
        Action::ClickTarget { target }
        | Action::TypeText {
            target: Some(target),
            ..
        }
        | Action::PressKey {
            target: Some(target),
            ..
        } => unsupported_semantic_target_reason_with_config(target, config),
        Action::Scroll {
            target: Some(target),
            ..
        } => unsupported_semantic_target_reason_with_config(target, config),
        Action::RunCommand { command, .. } => unsupported_command_reason(command, config),
        Action::WaitFor { condition } => unsupported_wait_reason(condition, config),
        Action::Assert { assertion } => match assertion {
            Assertion::TargetExists { target } if target.image.is_some() => {
                unsupported_image_action_target_reason(target, config)
            }
            Assertion::TargetExists { target } if target.accessibility.is_some() => {
                unsupported_semantic_target_reason_with_config(target, config)
            }
            Assertion::TargetExists { target } => {
                unsupported_window_target_reason_with_config(target, config)
            }
            Assertion::Condition { condition } => unsupported_wait_reason(condition, config),
        },
        _ => None,
    }
}

fn unsupported_wait_reason(condition: &WaitCondition, config: &RunConfig) -> Option<&'static str> {
    match condition {
        WaitCondition::WindowExists { target } => {
            if target.accessibility.is_some() {
                unsupported_semantic_target_reason_with_config(target, config)
            } else {
                unsupported_window_target_reason(target)
            }
        }
        WaitCondition::WindowFocused { target } if target.accessibility.is_some() => {
            unsupported_semantic_target_reason_with_config(target, config)
        }
        WaitCondition::WindowFocused { target } => unsupported_window_target_reason(target),
        WaitCondition::TargetExists { target } | WaitCondition::TargetNotExists { target }
            if target.image.is_some() =>
        {
            unsupported_image_action_target_reason(target, config)
        }
        WaitCondition::TargetExists { target }
        | WaitCondition::TargetFocused { target }
        | WaitCondition::TargetEnabled { target }
        | WaitCondition::TargetVisible { target }
        | WaitCondition::TargetActionable { target }
        | WaitCondition::TargetNotExists { target }
        | WaitCondition::TargetNameContains { target, .. } => {
            unsupported_semantic_target_reason_with_config(target, config)
        }
        WaitCondition::TargetValueContains { target, .. } => {
            if !config.allow_value_capture {
                Some("targetValueContains requires explicit allowValueCapture approval")
            } else {
                unsupported_semantic_target_reason_with_config(target, config)
            }
        }
        WaitCondition::ProcessRunning { target } => unsupported_process_target_reason(target),
        WaitCondition::CommandExits { command, .. } => unsupported_command_reason(command, config),
        _ => None,
    }
}

fn unsupported_semantic_target_reason_with_config(
    target: &Target,
    config: &RunConfig,
) -> Option<&'static str> {
    if let Some(reason) = unsupported_image_target_reason(target, config) {
        return Some(reason);
    }
    if target.image.is_some() {
        return Some(
            "Windows semantic target operations do not support image selectors; use accessibility selectors or clickTarget image matching",
        );
    }
    if target.accessibility.is_none() {
        return Some("semantic target operations require an accessibility selector");
    }
    if let Some(accessibility) = &target.accessibility
        && accessibility.path.is_some()
        && accessibility.id.is_none()
        && accessibility.name.is_none()
        && accessibility.control_type.is_none()
        && !config.allow_path_only_selectors
    {
        return Some(
            "path-only accessibility selectors require explicit allowPathOnlySelectors approval",
        );
    }

    let mut window_target = target.clone();
    window_target.accessibility = None;
    unsupported_window_target_reason(&window_target)
}

fn unsupported_window_target_reason_with_config(
    target: &Target,
    config: &RunConfig,
) -> Option<&'static str> {
    if let Some(reason) = unsupported_image_target_reason(target, config) {
        return Some(reason);
    }
    unsupported_window_target_reason(target)
}

fn unsupported_launch_target_reason(target: &Target, config: &RunConfig) -> Option<&'static str> {
    if let Some(reason) = unsupported_image_target_reason(target, config) {
        return Some(reason);
    }
    unsupported_window_target_reason(target)
}

fn unsupported_image_action_target_reason(
    target: &Target,
    config: &RunConfig,
) -> Option<&'static str> {
    if let Some(reason) = unsupported_image_target_reason(target, config) {
        return Some(reason);
    }
    let mut window_target = target.clone();
    window_target.image = None;
    unsupported_window_target_reason(&window_target)
}

fn reject_unsupported_target(reason: Option<&'static str>) -> Result<(), AdapterError> {
    if let Some(reason) = reason {
        return Err(AdapterError::unsupported(reason).with_failure_kind(FailureKind::PolicyDenied));
    }
    Ok(())
}

fn unsupported_coordinate_target_reason(
    target: &Target,
    config: &RunConfig,
) -> Option<&'static str> {
    if let Some(reason) = unsupported_image_target_reason(target, config) {
        return Some(reason);
    }
    if !config.allow_coordinate_targets {
        return Some("coordinate targets require explicit allowCoordinateTargets approval");
    }
    if target.app_name.is_some()
        || target.process_name.is_some()
        || target.window_title.is_some()
        || target.title_contains.is_some()
        || target.url.is_some()
        || target.file_path.is_some()
        || target.accessibility.is_some()
        || target.image.is_some()
        || !target.platform_selectors.is_empty()
    {
        return Some(
            "Windows coordinate clicks currently support only absolute screen coordinates",
        );
    }
    None
}

fn unsupported_window_target_reason(target: &Target) -> Option<&'static str> {
    if target.app_name.is_some()
        || target.process_name.is_some()
        || target.url.is_some()
        || target.file_path.is_some()
        || target.accessibility.is_some()
        || target.image.is_some()
        || target.coordinates.is_some()
        || !target.platform_selectors.is_empty()
    {
        return Some(
            "Windows window queries currently support only a windowTitle or titleContains selector",
        );
    }

    if target.window_title.is_some() && target.title_contains.is_some() {
        return Some("Windows window queries require exactly one of windowTitle or titleContains");
    }
    if target.window_title.is_none() && target.title_contains.is_none() {
        return Some("Windows window queries require windowTitle or titleContains");
    }

    None
}

fn unsupported_image_target_reason(target: &Target, config: &RunConfig) -> Option<&'static str> {
    target.image.as_ref()?;
    if !config.allow_image_targets {
        return Some("image targets require explicit allowImageTargets approval");
    }
    if !config.allow_screenshot_capture {
        return Some("image target matching requires explicit allowScreenshotCapture approval");
    }
    None
}

fn unsupported_process_target_reason(target: &Target) -> Option<&'static str> {
    process_name(target).err().map(|error| {
        if error.to_string().contains("require a processName") {
            "Windows process queries require a processName selector"
        } else {
            "Windows process queries currently support only a processName selector"
        }
    })
}

fn unsupported_command_reason(command: &str, config: &RunConfig) -> Option<&'static str> {
    (!config.approved_commands.contains(command)).then_some("command is not approved for this run")
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use cueflow_core::{AccessibilityTarget, FailureKind, PlatformSelector};
    use cueflow_executor::{EvidencePhase, ExecutionAdapter};

    use super::*;

    fn window_target_with_path_only_accessibility() -> Target {
        Target {
            app_name: None,
            process_name: None,
            window_title: Some("Cueflow Impossible Window".to_string()),
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: Some(AccessibilityTarget {
                id: None,
                name: None,
                control_type: None,
                path: Some(Vec::new()),
            }),
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        }
    }

    fn window_target_with_image(path: &str) -> Target {
        Target {
            app_name: None,
            process_name: None,
            window_title: Some("Settings".to_string()),
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: None,
            image: Some(cueflow_core::ImageTarget {
                path: path.to_string(),
                confidence: None,
                region: None,
            }),
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        }
    }

    #[test]
    fn windows_capabilities_expose_supported_and_gated_features() {
        let capabilities = WindowsDesktopAdapter::capabilities();

        assert_eq!(capabilities.platform, Platform::Windows);
        assert!(capabilities.supports_launch);
        assert!(capabilities.supports_focus);
        assert!(capabilities.supports_input);
        assert!(capabilities.supports_semantic_targets);
        assert!(capabilities.supports_coordinate_targets);
        assert!(capabilities.supports_process_queries);
    }

    #[test]
    fn preflight_rejects_unscoped_accessibility_and_non_title_window_selectors() {
        let adapter = WindowsDesktopAdapter;
        let accessibility_target = Target {
            app_name: None,
            process_name: None,
            window_title: None,
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: Some(AccessibilityTarget {
                id: Some("submit".to_string()),
                name: None,
                control_type: None,
                path: None,
            }),
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        };
        let accessibility_diagnostics = adapter.preflight(
            &Action::ClickTarget {
                target: accessibility_target,
            },
            &RunConfig::default(),
        );
        assert_eq!(accessibility_diagnostics.len(), 1);

        let mut partial_target = Target::app("Browser");
        partial_target.window_title = Some("Demo".to_string());
        let selector_diagnostics = adapter.preflight(
            &Action::FocusWindow {
                target: partial_target,
            },
            &RunConfig::default(),
        );
        assert_eq!(selector_diagnostics.len(), 1);
    }

    #[test]
    fn preflight_accepts_scoped_accessibility_targets() {
        let adapter = WindowsDesktopAdapter;
        let target = Target {
            app_name: None,
            process_name: None,
            window_title: Some("Settings".to_string()),
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: Some(AccessibilityTarget {
                id: Some("submit".to_string()),
                name: Some("Submit".to_string()),
                control_type: Some("button".to_string()),
                path: None,
            }),
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        };

        assert!(
            adapter
                .preflight(
                    &Action::ClickTarget {
                        target: target.clone(),
                    },
                    &RunConfig::default(),
                )
                .is_empty()
        );
        assert_eq!(
            adapter
                .preflight(
                    &Action::WaitFor {
                        condition: WaitCondition::WindowFocused {
                            target: target.clone(),
                        },
                    },
                    &RunConfig::default(),
                )
                .len(),
            0
        );
        for condition in [
            WaitCondition::TargetExists {
                target: target.clone(),
            },
            WaitCondition::TargetFocused {
                target: target.clone(),
            },
            WaitCondition::TargetEnabled {
                target: target.clone(),
            },
            WaitCondition::TargetVisible {
                target: target.clone(),
            },
            WaitCondition::TargetNameContains {
                target: target.clone(),
                text: "Ready".to_string(),
            },
            WaitCondition::TargetValueContains {
                target,
                text: "ready".to_string(),
            },
        ] {
            let config = RunConfig {
                allow_value_capture: true,
                ..RunConfig::default()
            };
            assert!(
                adapter
                    .preflight(&Action::WaitFor { condition }, &config)
                    .is_empty()
            );
        }
    }

    #[test]
    fn preflight_accepts_targeted_key_chords_for_scoped_accessibility_targets() {
        let adapter = WindowsDesktopAdapter;
        let target = Target {
            app_name: None,
            process_name: None,
            window_title: Some("Settings".to_string()),
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: Some(AccessibilityTarget {
                id: Some("search".to_string()),
                name: None,
                control_type: Some("edit".to_string()),
                path: None,
            }),
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        };

        assert!(
            adapter
                .preflight(
                    &Action::PressKey {
                        keys: "Ctrl+A".to_string(),
                        target: Some(target),
                    },
                    &RunConfig::default(),
                )
                .is_empty()
        );
    }

    #[test]
    fn preflight_accepts_coordinate_only_clicks_as_last_resort_targets() {
        let adapter = WindowsDesktopAdapter;
        let config = RunConfig {
            allow_coordinate_targets: true,
            ..RunConfig::default()
        };
        assert!(
            adapter
                .preflight(
                    &Action::ClickTarget {
                        target: Target {
                            app_name: None,
                            process_name: None,
                            window_title: None,
                            title_contains: None,
                            url: None,
                            file_path: None,
                            accessibility: None,
                            image: None,
                            coordinates: Some(cueflow_core::Coordinates { x: 10, y: 20 }),
                            platform_selectors: BTreeMap::new(),
                        },
                    },
                    &config,
                )
                .is_empty()
        );
    }

    #[test]
    fn preflight_rejects_ambiguous_coordinate_clicks() {
        let adapter = WindowsDesktopAdapter;
        let config = RunConfig {
            allow_coordinate_targets: true,
            ..RunConfig::default()
        };
        let diagnostics = adapter.preflight(
            &Action::ClickTarget {
                target: Target {
                    app_name: None,
                    process_name: None,
                    window_title: Some("Settings".to_string()),
                    title_contains: None,
                    url: None,
                    file_path: None,
                    accessibility: None,
                    image: None,
                    coordinates: Some(cueflow_core::Coordinates { x: 10, y: 20 }),
                    platform_selectors: BTreeMap::new(),
                },
            },
            &config,
        );

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].message,
            "Windows coordinate clicks currently support only absolute screen coordinates"
        );
    }

    #[test]
    fn preflight_rejects_launch_targets_with_image_selectors() {
        let adapter = WindowsDesktopAdapter;
        let diagnostics = adapter.preflight(
            &Action::LaunchApp {
                app: "ms-settings:".to_string(),
                target: Some(Target {
                    app_name: None,
                    process_name: None,
                    window_title: Some("Settings".to_string()),
                    title_contains: None,
                    url: None,
                    file_path: None,
                    accessibility: None,
                    image: Some(cueflow_core::ImageTarget {
                        path: "fixtures/settings-search.bmp".to_string(),
                        confidence: None,
                        region: None,
                    }),
                    coordinates: None,
                    platform_selectors: BTreeMap::new(),
                }),
            },
            &RunConfig::default(),
        );

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].message,
            "image targets require explicit allowImageTargets approval"
        );
    }

    #[test]
    fn preflight_accepts_image_target_exists_only_with_visual_policy() {
        let adapter = WindowsDesktopAdapter;
        let action = Action::WaitFor {
            condition: WaitCondition::TargetExists {
                target: window_target_with_image("fixtures/settings-search.bmp"),
            },
        };

        let default_diagnostics = adapter.preflight(&action, &RunConfig::default());
        assert_eq!(default_diagnostics.len(), 1);
        assert_eq!(
            default_diagnostics[0].message,
            "image targets require explicit allowImageTargets approval"
        );

        let missing_screenshot_policy = RunConfig {
            allow_image_targets: true,
            ..RunConfig::default()
        };
        let missing_screenshot_diagnostics = adapter.preflight(&action, &missing_screenshot_policy);
        assert_eq!(missing_screenshot_diagnostics.len(), 1);
        assert_eq!(
            missing_screenshot_diagnostics[0].message,
            "image target matching requires explicit allowScreenshotCapture approval"
        );

        let visual_policy = RunConfig {
            allow_image_targets: true,
            allow_screenshot_capture: true,
            ..RunConfig::default()
        };
        assert!(adapter.preflight(&action, &visual_policy).is_empty());
    }

    #[test]
    fn preflight_rejects_open_file_targets_with_image_selectors() {
        let adapter = WindowsDesktopAdapter;
        let diagnostics = adapter.preflight(
            &Action::OpenFile {
                path: "C:\\Windows\\System32\\notepad.exe".to_string(),
                target: Some(Target {
                    app_name: None,
                    process_name: None,
                    window_title: Some("Untitled - Notepad".to_string()),
                    title_contains: None,
                    url: None,
                    file_path: None,
                    accessibility: None,
                    image: Some(cueflow_core::ImageTarget {
                        path: "fixtures/notepad.bmp".to_string(),
                        confidence: None,
                        region: None,
                    }),
                    coordinates: None,
                    platform_selectors: BTreeMap::new(),
                }),
            },
            &RunConfig::default(),
        );

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].message,
            "image targets require explicit allowImageTargets approval"
        );
    }

    #[test]
    fn direct_execute_rejects_launch_targets_with_image_selectors_before_opening() {
        let mut adapter = WindowsDesktopAdapter;
        let error = adapter
            .execute(
                &Action::LaunchUrl {
                    url: "cueflow-test-do-not-open:".to_string(),
                    target: Some(Target {
                        app_name: None,
                        process_name: None,
                        window_title: Some("Cueflow Impossible Window".to_string()),
                        title_contains: None,
                        url: None,
                        file_path: None,
                        accessibility: None,
                        image: Some(cueflow_core::ImageTarget {
                            path: "fixtures/impossible.bmp".to_string(),
                            confidence: None,
                            region: None,
                        }),
                        coordinates: None,
                        platform_selectors: BTreeMap::new(),
                    }),
                },
                &RunConfig::default(),
            )
            .expect_err("image launch target is policy denied before shell open");

        assert_eq!(error.failure_kind(), Some(FailureKind::PolicyDenied));
        assert_eq!(
            error.to_string(),
            "image targets require explicit allowImageTargets approval"
        );
    }

    #[test]
    fn direct_execute_rejects_open_file_targets_with_image_selectors_before_opening() {
        let mut adapter = WindowsDesktopAdapter;
        let error = adapter
            .execute(
                &Action::OpenFile {
                    path: "C:\\cueflow\\missing-file-that-must-not-open.txt".to_string(),
                    target: Some(Target {
                        app_name: None,
                        process_name: None,
                        window_title: Some("Cueflow Impossible Window".to_string()),
                        title_contains: None,
                        url: None,
                        file_path: None,
                        accessibility: None,
                        image: Some(cueflow_core::ImageTarget {
                            path: "fixtures/impossible.bmp".to_string(),
                            confidence: None,
                            region: None,
                        }),
                        coordinates: None,
                        platform_selectors: BTreeMap::new(),
                    }),
                },
                &RunConfig::default(),
            )
            .expect_err("image open-file target is policy denied before shell open");

        assert_eq!(error.failure_kind(), Some(FailureKind::PolicyDenied));
        assert_eq!(
            error.to_string(),
            "image targets require explicit allowImageTargets approval"
        );
    }

    #[test]
    fn direct_execute_rejects_accessibility_bearing_window_targets_before_side_effects() {
        let mut adapter = WindowsDesktopAdapter;

        let launch_error = adapter
            .execute(
                &Action::LaunchUrl {
                    url: "cueflow-test-do-not-open:".to_string(),
                    target: Some(window_target_with_path_only_accessibility()),
                },
                &RunConfig::default(),
            )
            .expect_err("launch window target cannot carry accessibility selectors");
        let open_file_error = adapter
            .execute(
                &Action::OpenFile {
                    path: "C:\\cueflow\\missing-file-that-must-not-open.txt".to_string(),
                    target: Some(window_target_with_path_only_accessibility()),
                },
                &RunConfig::default(),
            )
            .expect_err("open-file window target cannot carry accessibility selectors");
        let focus_error = adapter
            .execute(
                &Action::FocusWindow {
                    target: window_target_with_path_only_accessibility(),
                },
                &RunConfig::default(),
            )
            .expect_err("focus window target cannot carry accessibility selectors");

        for error in [launch_error, open_file_error, focus_error] {
            assert_eq!(error.failure_kind(), Some(FailureKind::PolicyDenied));
            assert_eq!(
                error.to_string(),
                "Windows window queries currently support only a windowTitle or titleContains selector"
            );
        }
    }

    #[test]
    fn direct_execute_rejects_path_only_semantic_targets_before_resolving() {
        let mut adapter = WindowsDesktopAdapter;
        let error = adapter
            .execute(
                &Action::TypeText {
                    text: "should-not-type".to_string(),
                    target: Some(Target {
                        app_name: None,
                        process_name: None,
                        window_title: Some("Cueflow Impossible Window".to_string()),
                        title_contains: None,
                        url: None,
                        file_path: None,
                        accessibility: Some(AccessibilityTarget {
                            id: None,
                            name: None,
                            control_type: None,
                            path: Some(Vec::new()),
                        }),
                        image: None,
                        coordinates: None,
                        platform_selectors: BTreeMap::new(),
                    }),
                },
                &RunConfig::default(),
            )
            .expect_err("path-only semantic target is policy denied before UIA lookup");

        assert_eq!(error.failure_kind(), Some(FailureKind::PolicyDenied));
        assert_eq!(
            error.to_string(),
            "path-only accessibility selectors require explicit allowPathOnlySelectors approval"
        );
    }

    #[test]
    fn trait_invoke_rejects_image_targets_before_uia_lookup() {
        let mut adapter = WindowsDesktopAdapter;
        let error = adapter
            .invoke_target(
                &Target {
                    app_name: None,
                    process_name: None,
                    window_title: Some("Cueflow Impossible Window".to_string()),
                    title_contains: None,
                    url: None,
                    file_path: None,
                    accessibility: Some(AccessibilityTarget {
                        id: Some("submit".to_string()),
                        name: None,
                        control_type: None,
                        path: None,
                    }),
                    image: Some(cueflow_core::ImageTarget {
                        path: "fixtures/impossible.bmp".to_string(),
                        confidence: None,
                        region: None,
                    }),
                    coordinates: None,
                    platform_selectors: BTreeMap::new(),
                },
                &RunConfig::default(),
            )
            .expect_err("image target is policy denied before UIA lookup");

        assert_eq!(error.failure_kind(), Some(FailureKind::PolicyDenied));
        assert_eq!(
            error.to_string(),
            "image targets require explicit allowImageTargets approval"
        );
    }

    #[test]
    fn trait_text_and_scroll_reject_path_only_targets_before_uia_lookup() {
        let mut adapter = WindowsDesktopAdapter;
        let target = Target {
            app_name: None,
            process_name: None,
            window_title: Some("Cueflow Impossible Window".to_string()),
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: Some(AccessibilityTarget {
                id: None,
                name: None,
                control_type: None,
                path: Some(Vec::new()),
            }),
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        };

        let text_error = adapter
            .set_target_text(&target, "should-not-type", &RunConfig::default())
            .expect_err("path-only text target is policy denied before UIA lookup");
        let scroll_error = adapter
            .scroll_target(&target, 0, 1, &RunConfig::default())
            .expect_err("path-only scroll target is policy denied before UIA lookup");

        assert_eq!(text_error.failure_kind(), Some(FailureKind::PolicyDenied));
        assert_eq!(scroll_error.failure_kind(), Some(FailureKind::PolicyDenied));
        assert_eq!(
            text_error.to_string(),
            "path-only accessibility selectors require explicit allowPathOnlySelectors approval"
        );
        assert_eq!(
            scroll_error.to_string(),
            "path-only accessibility selectors require explicit allowPathOnlySelectors approval"
        );
    }

    #[test]
    fn capture_step_evidence_rejects_policy_denied_targets_before_writing_files() {
        let mut adapter = WindowsDesktopAdapter;
        let config = RunConfig {
            capture_step_evidence: true,
            evidence_directory: Some("C:\\cueflow\\must-not-be-created".to_string()),
            ..RunConfig::default()
        };
        let error = adapter
            .capture_step_evidence(
                EvidencePhase::Before,
                &Action::ClickTarget {
                    target: Target {
                        app_name: None,
                        process_name: None,
                        window_title: Some("Cueflow Impossible Window".to_string()),
                        title_contains: None,
                        url: None,
                        file_path: None,
                        accessibility: Some(AccessibilityTarget {
                            id: Some("submit".to_string()),
                            name: None,
                            control_type: None,
                            path: None,
                        }),
                        image: Some(cueflow_core::ImageTarget {
                            path: "fixtures/impossible.bmp".to_string(),
                            confidence: None,
                            region: None,
                        }),
                        coordinates: None,
                        platform_selectors: BTreeMap::new(),
                    },
                },
                &config,
                "run-test",
                "automation-test",
                "step-test",
            )
            .expect_err("policy-denied evidence target is rejected before file writes");

        assert_eq!(error.failure_kind(), Some(FailureKind::PolicyDenied));
        assert_eq!(
            error.to_string(),
            "image targets require explicit allowImageTargets approval"
        );
    }

    #[test]
    fn capture_step_evidence_rejects_accessibility_bearing_window_targets_before_writing_files() {
        let mut adapter = WindowsDesktopAdapter;
        let config = RunConfig {
            capture_step_evidence: true,
            evidence_directory: Some("C:\\cueflow\\must-not-be-created".to_string()),
            ..RunConfig::default()
        };
        let error = adapter
            .capture_step_evidence(
                EvidencePhase::Before,
                &Action::FocusWindow {
                    target: window_target_with_path_only_accessibility(),
                },
                &config,
                "run-test",
                "automation-test",
                "step-test",
            )
            .expect_err("accessibility-bearing window target is rejected before file writes");

        assert_eq!(error.failure_kind(), Some(FailureKind::PolicyDenied));
        assert_eq!(
            error.to_string(),
            "Windows window queries currently support only a windowTitle or titleContains selector"
        );
    }

    #[test]
    fn preflight_rejects_targeted_key_chords_without_accessibility_scope() {
        let adapter = WindowsDesktopAdapter;
        let diagnostics = adapter.preflight(
            &Action::PressKey {
                keys: "Ctrl+A".to_string(),
                target: Some(Target {
                    app_name: None,
                    process_name: None,
                    window_title: Some("Settings".to_string()),
                    title_contains: None,
                    url: None,
                    file_path: None,
                    accessibility: None,
                    image: None,
                    coordinates: None,
                    platform_selectors: BTreeMap::new(),
                }),
            },
            &RunConfig::default(),
        );

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "capability-unavailable");
        assert_eq!(
            diagnostics[0].message,
            "semantic target operations require an accessibility selector"
        );
    }

    #[test]
    fn process_queries_require_an_exact_process_name_selector() {
        let adapter = WindowsDesktopAdapter;
        let valid_target = Target {
            app_name: None,
            process_name: Some("msedge.exe".to_string()),
            window_title: None,
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: None,
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        };
        let valid_action = Action::WaitFor {
            condition: WaitCondition::ProcessRunning {
                target: valid_target.clone(),
            },
        };
        assert!(
            adapter
                .preflight(&valid_action, &RunConfig::default())
                .is_empty()
        );

        let mut invalid_target = valid_target;
        invalid_target.window_title = Some("Edge".to_string());
        let invalid_action = Action::WaitFor {
            condition: WaitCondition::ProcessRunning {
                target: invalid_target,
            },
        };
        assert_eq!(
            adapter
                .preflight(&invalid_action, &RunConfig::default())
                .len(),
            1
        );
    }

    #[test]
    fn commands_require_explicit_approval_and_run_without_shell_interpolation() {
        let mut adapter = WindowsDesktopAdapter;
        let action = Action::RunCommand {
            command: "cmd.exe".to_string(),
            args: vec!["/c".to_string(), "exit".to_string(), "0".to_string()],
        };
        let unapproved = RunConfig::default();
        assert_eq!(adapter.preflight(&action, &unapproved).len(), 1);

        let approved = RunConfig {
            dry_run: false,
            approved_commands: BTreeSet::from(["cmd.exe".to_string()]),
            ..RunConfig::default()
        };
        assert!(adapter.preflight(&action, &approved).is_empty());
        adapter
            .execute(&action, &approved)
            .expect("approved command succeeds");
        assert_eq!(
            adapter
                .evaluate_wait(
                    &WaitCondition::CommandExits {
                        command: "cmd.exe".to_string(),
                        args: vec!["/c".to_string(), "exit".to_string(), "0".to_string()],
                    },
                    &approved,
                )
                .expect("approved command condition evaluates"),
            ConditionState::Satisfied
        );
    }

    #[test]
    fn window_title_matchers_are_case_insensitive() {
        assert!(
            WindowTitleMatcher::Exact("Google - Microsoft Edge").matches("google - microsoft edge")
        );
        assert!(WindowTitleMatcher::Contains("Microsoft Edge").matches("Google - Microsoft Edge"));
        assert!(
            WindowTitleMatcher::Contains("Microsoft Edge")
                .matches("New tab - Microsoft\u{200B} Edge")
        );
        assert!(!WindowTitleMatcher::Contains("Firefox").matches("Google - Microsoft Edge"));
    }

    #[test]
    fn window_identity_diagnostics_include_environment_state() {
        let identity = WindowIdentity {
            handle: "HWND(0x2a)".to_string(),
            title: "Save As".to_string(),
            class_name: "#32770".to_string(),
            process_id: 42,
            process_name: Some("notepad.exe".to_string()),
            bounds: Some(AccessibilityBounds {
                left: -10,
                top: 20,
                right: 640,
                bottom: 480,
            }),
            is_foreground: false,
            is_minimized: true,
            owner: Some("HWND(0x10)".to_string()),
        };

        let diagnostic = format_window_identity(&identity);

        assert!(diagnostic.contains("bounds=-10,20,640,480"));
        assert!(diagnostic.contains("foreground=false"));
        assert!(diagnostic.contains("minimized=true"));
        assert!(diagnostic.contains("owner=HWND(0x10)"));
        assert!(diagnostic.contains("process=\"notepad.exe\""));
    }

    #[test]
    fn accessibility_paths_use_root_and_child_index_notation() {
        assert_eq!(format_accessibility_path(&[]), "[]");
        assert_eq!(format_accessibility_path(&[0, 12, 3]), "[0,12,3]");
    }

    #[test]
    fn selector_candidates_prefer_stable_identifiers_before_paths() {
        let candidates = selector_candidates("Demo", &[2, 1], "submitButton", "Submit", "button");

        assert_eq!(candidates.len(), 4);
        assert_eq!(candidates[0].confidence, SelectorConfidence::High);
        assert_eq!(candidates[0].score, 95);
        assert_eq!(
            candidates[0]
                .target
                .accessibility
                .as_ref()
                .unwrap()
                .id
                .as_deref(),
            Some("submitButton")
        );
        assert_eq!(candidates[2].confidence, SelectorConfidence::Medium);
        assert_eq!(candidates[3].confidence, SelectorConfidence::Low);
        assert_eq!(
            candidates[3].target.accessibility.as_ref().unwrap().path,
            Some(vec![2, 1])
        );
        assert_eq!(
            candidates[3].warnings,
            vec!["Path-only selectors are positional and should be treated as fragile."]
        );
    }

    #[test]
    fn selector_repair_changes_explain_candidate_differences() {
        let original = cueflow_core::AccessibilityTarget {
            id: Some("oldButton".to_string()),
            name: Some("Old".to_string()),
            control_type: Some("button".to_string()),
            path: Some(vec![0, 1]),
        };
        let repaired = cueflow_core::AccessibilityTarget {
            id: Some("submitButton".to_string()),
            name: None,
            control_type: Some("button".to_string()),
            path: Some(vec![2, 1]),
        };

        let changes = selector_candidate_changes(Some(&original), Some(&repaired));

        assert_eq!(
            changes,
            vec![
                "id: \"oldButton\" -> \"submitButton\"",
                "name: \"Old\" -> none",
                "path: [0,1] -> [2,1]",
            ]
        );
    }

    #[test]
    fn write_bmp_emits_top_down_32bpp_bitmap() {
        let path = std::env::temp_dir().join(format!(
            "cueflow-write-bmp-{}-{}.bmp",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let pixels = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];

        write_bmp(&path, 2, 1, &pixels).expect("bmp writes");
        let bytes = fs::read(&path).expect("bmp can be read");
        let _ = fs::remove_file(&path);

        assert_eq!(&bytes[0..2], b"BM");
        assert_eq!(u32::from_le_bytes(bytes[10..14].try_into().unwrap()), 54);
        assert_eq!(u32::from_le_bytes(bytes[14..18].try_into().unwrap()), 40);
        assert_eq!(i32::from_le_bytes(bytes[18..22].try_into().unwrap()), 2);
        assert_eq!(i32::from_le_bytes(bytes[22..26].try_into().unwrap()), -1);
        assert_eq!(u16::from_le_bytes(bytes[28..30].try_into().unwrap()), 32);
        assert_eq!(&bytes[54..], &pixels);
    }

    #[test]
    fn bmp_reader_round_trips_top_down_32bpp_bitmaps() {
        let path = std::env::temp_dir().join(format!(
            "cueflow-read-bmp-{}-{}.bmp",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let pixels = [
            0x01, 0x02, 0x03, 0xff, 0x04, 0x05, 0x06, 0xff, 0x07, 0x08, 0x09, 0xff, 0x0a, 0x0b,
            0x0c, 0xff,
        ];

        write_bmp(&path, 2, 2, &pixels).expect("bmp writes");
        let image = read_bmp_image(&path).expect("bmp reads");
        let _ = fs::remove_file(&path);

        assert_eq!(image.width, 2);
        assert_eq!(image.height, 2);
        assert_eq!(image.pixels, pixels);
    }

    #[test]
    fn visual_template_match_respects_region_and_confidence() {
        let screenshot = BmpImage {
            width: 3,
            height: 3,
            pixels: vec![
                0, 0, 0, 255, 1, 1, 1, 255, 2, 2, 2, 255, 3, 3, 3, 255, 9, 9, 9, 255, 8, 8, 8, 255,
                4, 4, 4, 255, 7, 7, 7, 255, 6, 6, 6, 255,
            ],
        };
        let template = BmpImage {
            width: 2,
            height: 2,
            pixels: vec![9, 9, 9, 255, 8, 8, 8, 255, 7, 7, 7, 255, 6, 6, 6, 255],
        };
        let image = ImageTarget {
            path: "template.bmp".to_string(),
            confidence: Some(100),
            region: Some(ImageRegion {
                left: 1,
                top: 1,
                width: 2,
                height: 2,
            }),
        };

        let matched = find_template_match(&screenshot, &template, &image)
            .expect("search succeeds")
            .expect("match");

        assert_eq!(
            matched,
            VisualMatch {
                left: 1,
                top: 1,
                width: 2,
                height: 2,
                confidence: 100,
            }
        );
    }

    #[test]
    fn visual_template_match_ignores_alpha_channel() {
        let screenshot = BmpImage {
            width: 1,
            height: 1,
            pixels: vec![10, 20, 30, 0],
        };
        let template = BmpImage {
            width: 1,
            height: 1,
            pixels: vec![10, 20, 30, 255],
        };
        let image = ImageTarget {
            path: "template.bmp".to_string(),
            confidence: Some(100),
            region: None,
        };

        let matched = find_template_match(&screenshot, &template, &image)
            .expect("search succeeds")
            .expect("match ignores alpha");

        assert_eq!(matched.confidence, 100);
    }

    #[test]
    fn visual_template_match_requires_bounded_search_budget() {
        let screenshot = BmpImage {
            width: 8_000,
            height: 8_000,
            pixels: Vec::new(),
        };
        let template = BmpImage {
            width: 100,
            height: 100,
            pixels: Vec::new(),
        };
        let image = ImageTarget {
            path: "template.bmp".to_string(),
            confidence: Some(100),
            region: None,
        };

        let error = find_template_match(&screenshot, &template, &image)
            .expect_err("unbounded large search is rejected");

        assert_eq!(
            error.to_string(),
            "image target search is too large; provide a bounded image region or smaller template"
        );
    }

    #[test]
    fn bmp_file_size_matches_32bpp_header_and_pixels() {
        assert_eq!(bmp_file_size(2, 1).expect("size"), 62);
    }

    #[test]
    fn typed_windows_error_helpers_attach_failure_kinds() {
        let transient = transient_error("temporary Windows failure");
        let focus = focus_denied_error("foreground denied");

        assert_eq!(transient.failure_kind(), Some(FailureKind::Transient));
        assert_eq!(focus.failure_kind(), Some(FailureKind::FocusDenied));
        assert_eq!(transient.diagnostics(), Some("failureKind=transient"));
        assert_eq!(focus.diagnostics(), Some("failureKind=focusDenied"));
    }

    #[test]
    fn preflight_accepts_a_title_contains_window_selector() {
        let adapter = WindowsDesktopAdapter;
        let action = Action::FocusWindow {
            target: Target {
                app_name: None,
                process_name: None,
                window_title: None,
                title_contains: Some("Microsoft Edge".to_string()),
                url: None,
                file_path: None,
                accessibility: None,
                image: None,
                coordinates: None,
                platform_selectors: BTreeMap::new(),
            },
        };

        assert!(adapter.preflight(&action, &RunConfig::default()).is_empty());
    }

    #[test]
    fn preflight_accepts_a_resolved_windows_window_title_selector() {
        let adapter = WindowsDesktopAdapter;
        let action = Action::FocusWindow {
            target: Target {
                app_name: Some("Browser".to_string()),
                process_name: None,
                window_title: None,
                title_contains: Some("Google".to_string()),
                url: None,
                file_path: None,
                accessibility: None,
                image: None,
                coordinates: None,
                platform_selectors: BTreeMap::from([(
                    Platform::Windows,
                    PlatformSelector {
                        process_name: None,
                        window_title: Some("Google".to_string()),
                        accessibility_query: None,
                        command_hint: None,
                    },
                )]),
            },
        }
        .for_platform(Some(Platform::Windows));

        assert!(adapter.preflight(&action, &RunConfig::default()).is_empty());
    }
}
