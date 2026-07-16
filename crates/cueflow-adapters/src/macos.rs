use std::ffi::{CStr, CString, c_char, c_double, c_void};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::ptr;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{
    AccessibilityBounds, AccessibilityNode, AccessibilityPoint, AccessibilitySelectorCandidate,
    AccessibilityTree, AdapterCapabilities, SelectorConfidence, SelectorRepairReport,
    WindowIdentity,
};
use cueflow_core::{
    Action, Artifact, ArtifactKind, Assertion, FailureKind, ImageRegion, ImageTarget, Platform,
    PreflightDiagnostic, PreflightSeverity, RunConfig, Target, WaitCondition,
};
use cueflow_executor::{AdapterError, ConditionState, EvidencePhase, ExecutionAdapter, RunControl};
use libc::{pid_t, size_t};

#[derive(Debug, Default)]
pub struct MacOsDesktopAdapter;

const SEMANTIC_SEARCH_MAX_DEPTH: u32 = 16;
const SEMANTIC_SEARCH_MAX_NODES: usize = 2_000;
const DEFAULT_EVIDENCE_MAX_ARTIFACT_BYTES: u64 = 25 * 1024 * 1024;
const VISUAL_MATCH_MAX_PIXEL_COMPARISONS: u64 = 50_000_000;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFNumberRef = *const c_void;
type CFBooleanRef = *const c_void;
type AXUIElementRef = *const c_void;
type AXError = i32;
type CGWindowID = u32;
type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;

const K_AX_ERROR_SUCCESS: AXError = 0;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_CF_NUMBER_SINT32_TYPE: i32 = 3;
const K_CF_NUMBER_CG_FLOAT64_TYPE: i32 = 16;
const K_AX_VALUE_CG_POINT_TYPE: i32 = 1;
const K_AX_VALUE_CG_SIZE_TYPE: i32 = 2;
const K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1;
const K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 16;
const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
const K_CG_MOUSE_BUTTON_LEFT: u32 = 0;
const K_CG_HID_EVENT_TAP: u32 = 0;
const K_CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;
const K_CG_EVENT_FLAG_MASK_SHIFT: u64 = 1 << 17;
const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 1 << 18;
const K_CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 1 << 19;
const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 1 << 20;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGPoint {
    x: c_double,
    y: c_double,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGSize {
    width: c_double,
    height: c_double,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    fn AXUIElementCreateApplication(pid: pid_t) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementCopyAttributeValues(
        element: AXUIElementRef,
        attribute: CFStringRef,
        index: isize,
        max_values: isize,
        values: *mut CFArrayRef,
    ) -> AXError;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    fn AXUIElementCopyActionNames(element: AXUIElementRef, names: *mut CFArrayRef) -> AXError;
    fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> AXError;
    fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, timeout: c_double) -> AXError;
    fn AXValueGetValue(value: CFTypeRef, value_type: i32, out: *mut c_void) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> CFTypeRef;
    fn CFBooleanGetValue(value: CFBooleanRef) -> bool;
    fn CFDictionaryGetValue(dictionary: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        num_values: isize,
        key_callbacks: CFTypeRef,
        value_callbacks: CFTypeRef,
    ) -> CFDictionaryRef;
    fn CFNumberGetValue(number: CFNumberRef, the_type: i32, value_ptr: *mut c_void) -> bool;
    fn CFRelease(value: CFTypeRef);
    fn CFStringCreateWithCString(
        allocator: CFTypeRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringGetCString(
        value: CFStringRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;
    static kCFBooleanTrue: CFBooleanRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
    ) -> CGEventRef;
    fn CGEventKeyboardSetUnicodeString(
        event: CGEventRef,
        string_length: size_t,
        unicode_string: *const u16,
    );
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: CGWindowID) -> CFArrayRef;
}

impl MacOsDesktopAdapter {
    pub fn capabilities() -> AdapterCapabilities {
        AdapterCapabilities {
            platform: Platform::MacOs,
            supports_launch: true,
            supports_focus: true,
            supports_input: true,
            supports_semantic_targets: accessibility_is_trusted(),
            supports_coordinate_targets: true,
            supports_window_queries: true,
            supports_process_queries: true,
            supports_accessibility_tree: accessibility_is_trusted(),
        }
    }

    pub fn request_accessibility_permission() -> bool {
        request_accessibility_permission_with_prompt()
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
        ensure_accessibility_permission()?;
        reject_unsupported_target(unsupported_window_target_reason(target))?;
        let window = find_window(target)?;
        let ax_window = ax_window_for(&window)?;
        let max_nodes = max_nodes.max(1);
        let mut remaining = max_nodes;
        let mut truncated = false;
        let root = inspect_ax_node(
            ax_window.as_ref(),
            &[],
            0,
            max_depth,
            include_values,
            &mut remaining,
            &mut truncated,
        )?;
        Ok(AccessibilityTree {
            platform: Platform::MacOs,
            window_title: window.title.clone(),
            window: Some(window.identity()),
            selector: window_target_summary(target),
            max_depth,
            max_nodes,
            truncated,
            root,
        })
    }

    pub fn capture_screenshot(&self, path: impl AsRef<Path>) -> Result<Artifact, AdapterError> {
        capture_screenshot(None, path.as_ref())
    }

    pub fn capture_window_screenshot(
        &self,
        target: &Target,
        path: impl AsRef<Path>,
    ) -> Result<Artifact, AdapterError> {
        let window = find_window(target)?;
        capture_screenshot(Some(window.window_id), path.as_ref())
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

impl ExecutionAdapter for MacOsDesktopAdapter {
    fn execute(
        &mut self,
        action: &Action,
        config: &RunConfig,
    ) -> Result<Vec<Artifact>, AdapterError> {
        match action {
            Action::LaunchUrl { url, target } => {
                if let Some(target) = target {
                    reject_unsupported_target(unsupported_launch_target_reason(target, config))?;
                }
                open_target(&["open", url])
            }
            Action::LaunchApp { app, target } => {
                if let Some(target) = target {
                    reject_unsupported_target(unsupported_launch_target_reason(target, config))?;
                }
                open_target(&["open", "-a", app])
            }
            Action::OpenFile { path, target } => {
                if let Some(target) = target {
                    reject_unsupported_target(unsupported_launch_target_reason(target, config))?;
                }
                open_target(&["open", path])
            }
            Action::FocusWindow { target } => {
                reject_unsupported_target(unsupported_focus_window_target_reason(target, config))?;
                focus_window(target).map(|_| Vec::new())
            }
            Action::TypeText {
                text,
                target: Some(target),
            } => self
                .set_target_text(target, text, config)
                .map(|_| Vec::new()),
            Action::TypeText { text, target: None } => send_text(text).map(|_| Vec::new()),
            Action::PressKey {
                keys,
                target: Some(target),
            } => {
                focus_target(target, config)?;
                send_key_chord(keys).map(|_| Vec::new())
            }
            Action::PressKey { keys, target: None } => send_key_chord(keys).map(|_| Vec::new()),
            Action::Scroll {
                delta_y,
                target: Some(target),
                ..
            } => self
                .scroll_target(target, 0, *delta_y, config)
                .map(|_| Vec::new()),
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
                self.invoke_target(target, config).map(|_| Vec::new())
            }
            Action::RunCommand { command, args } => {
                run_command(command, args, config, &RunControl::default(), None).map(|_| Vec::new())
            }
            _ => Err(AdapterError::unsupported(format!(
                "macOS adapter does not yet support {}",
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
            WaitCondition::Duration { .. } => {
                ExecutionAdapter::evaluate_wait(self, condition, config)
            }
            WaitCondition::FileExists { path } => Ok(condition_state(Path::new(path).exists())),
            WaitCondition::WindowExists { target } => {
                if target.accessibility.is_some() {
                    semantic_target_exists(target).map(condition_state)
                } else {
                    window_exists(target).map(condition_state)
                }
            }
            WaitCondition::WindowFocused { target } => {
                window_is_focused(target).map(condition_state)
            }
            WaitCondition::ProcessRunning { target } => {
                process_is_running(target).map(condition_state)
            }
            WaitCondition::TargetExists { target } if target.image.is_some() => {
                image_target_exists(target, config).map(condition_state)
            }
            WaitCondition::TargetExists { target } => {
                semantic_target_exists(target).map(condition_state)
            }
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
                semantic_target_value_contains(target, text, config).map(condition_state)
            }
            WaitCondition::CommandExits { command, args } => {
                command_exits(command, args, config, &RunControl::default(), None)
                    .map(condition_state)
            }
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
                command_exits(command, args, config, control, timeout).map(condition_state)
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
            } => launch_and_wait_for_window(&["open", url], target, config, control, timeout),
            Action::LaunchApp {
                app,
                target: Some(target),
            } => launch_and_wait_for_window(&["open", "-a", app], target, config, control, timeout),
            Action::OpenFile {
                path,
                target: Some(target),
            } => launch_and_wait_for_window(&["open", path], target, config, control, timeout),
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

    fn target_exists(&mut self, target: &Target, config: &RunConfig) -> Result<bool, AdapterError> {
        if target.image.is_some() {
            return image_target_exists(target, config);
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
            perform_first_supported_action(element, &["AXPress", "AXConfirm", "AXShowMenu"])
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
        let value = CfString::new(text)?;
        with_semantic_target(target, |element| {
            ensure_semantic_target_actionable(element)?;
            ax_set_attribute(element, "AXValue", value.as_type_ref()).map_err(|_| {
                AdapterError::unsupported("target does not support semantic text input")
            })
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
        _delta_x: i32,
        delta_y: i32,
        config: &RunConfig,
    ) -> Result<(), AdapterError> {
        reject_unsupported_target(unsupported_semantic_target_reason_with_config(
            target, config,
        ))?;
        with_semantic_target(target, |element| {
            ensure_semantic_target_actionable(element)?;
            if delta_y < 0 {
                perform_first_supported_action(element, &["AXScrollDown", "AXScrollPageDown"])
            } else if delta_y > 0 {
                perform_first_supported_action(element, &["AXScrollUp", "AXScrollPageUp"])
            } else {
                Ok(())
            }
        })
    }

    fn preflight(&self, action: &Action, config: &RunConfig) -> Vec<PreflightDiagnostic> {
        unsupported_action_reason(action, config)
            .map(|message| {
                vec![PreflightDiagnostic {
                    severity: PreflightSeverity::Error,
                    code: if message.contains("Accessibility permission") {
                        "accessibility-permission-missing".to_string()
                    } else {
                        "capability-unavailable".to_string()
                    },
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
        if target.window_title.is_none()
            && target.title_contains.is_none()
            && target.app_name.is_none()
        {
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

        fs::create_dir_all(&evidence_dir)
            .map_err(|_| transient_error("macOS could not create step evidence directory"))?;
        fs::write(&tree_path, tree_json)
            .map_err(|_| transient_error("macOS could not write accessibility evidence"))?;

        let mut artifacts = vec![Artifact {
            kind: ArtifactKind::AccessibilityTree,
            uri: format!("file://{}", path_display(&tree_path)),
            label: Some(format!("{} accessibility tree: {step_id}", phase.as_str())),
        }];

        if config.allow_screenshot_capture {
            let screenshot_path = evidence_dir.join(format!(
                "{safe_automation_id}-{safe_run_id}-{step_id}-{}-window.png",
                phase.as_str()
            ));
            artifacts.push(capture_window_screenshot_with_limit(
                target,
                &screenshot_path,
                config,
            )?);
        }

        Ok(artifacts)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct MacWindow {
    window_id: CGWindowID,
    title: String,
    app_name: String,
    process_id: u32,
    bounds: Option<AccessibilityBounds>,
}

impl MacWindow {
    fn identity(&self) -> WindowIdentity {
        WindowIdentity {
            handle: self.window_id.to_string(),
            title: self.title.clone(),
            class_name: "AXWindow".to_string(),
            process_id: self.process_id,
            process_name: Some(self.app_name.clone()),
            bounds: self.bounds,
            is_foreground: frontmost_process_name()
                .is_some_and(|name| name.eq_ignore_ascii_case(&self.app_name)),
            is_minimized: false,
            owner: None,
        }
    }
}

#[derive(Debug)]
struct CfOwned {
    value: CFTypeRef,
}

impl CfOwned {
    fn new(value: CFTypeRef) -> Self {
        Self { value }
    }

    fn as_ref(&self) -> CFTypeRef {
        self.value
    }
}

impl Drop for CfOwned {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.value);
        }
    }
}

struct CfString {
    value: CFStringRef,
}

impl CfString {
    fn new(value: &str) -> Result<Self, AdapterError> {
        let value = CString::new(value)
            .map_err(|_| AdapterError::new("macOS string contains an interior NUL byte"))?;
        let cf = unsafe {
            CFStringCreateWithCString(ptr::null(), value.as_ptr(), K_CF_STRING_ENCODING_UTF8)
        };
        if cf.is_null() {
            return Err(transient_error(
                "macOS could not allocate a CoreFoundation string",
            ));
        }
        Ok(Self { value: cf })
    }

    fn as_ref(&self) -> CFStringRef {
        self.value
    }

    fn as_type_ref(&self) -> CFTypeRef {
        self.value
    }
}

impl Drop for CfString {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.value);
        }
    }
}

fn accessibility_is_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

fn request_accessibility_permission_with_prompt() -> bool {
    let Ok(prompt_key) = CfString::new("AXTrustedCheckOptionPrompt") else {
        return accessibility_is_trusted();
    };
    let keys = [prompt_key.as_type_ref()];
    let values = [unsafe { kCFBooleanTrue as CFTypeRef }];
    let options = unsafe {
        CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            ptr::null(),
            ptr::null(),
        )
    };
    if options.is_null() {
        return accessibility_is_trusted();
    }
    let options = CfOwned::new(options);
    unsafe { AXIsProcessTrustedWithOptions(options.as_ref() as CFDictionaryRef) }
}

fn ensure_accessibility_permission() -> Result<(), AdapterError> {
    if accessibility_is_trusted() {
        Ok(())
    } else {
        Err(
            AdapterError::new("macOS Accessibility permission is required for semantic automation")
                .with_failure_kind(FailureKind::CapabilityUnavailable)
                .with_source("failureKind=capabilityUnavailable"),
        )
    }
}

fn open_target(args: &[&str]) -> Result<Vec<Artifact>, AdapterError> {
    let (program, args) = args
        .split_first()
        .expect("open_target requires at least the open executable");
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|_| transient_error("macOS could not launch the requested target"))?;
    if !status.success() {
        return Err(transient_error(
            "macOS open failed for the requested target",
        ));
    }
    Ok(Vec::new())
}

fn launch_and_wait_for_window(
    open_args: &[&str],
    window_target: &Target,
    config: &RunConfig,
    control: &RunControl,
    timeout: Option<Duration>,
) -> Result<Vec<Artifact>, AdapterError> {
    reject_unsupported_target(unsupported_launch_target_reason(window_target, config))?;
    let artifacts = open_target(open_args)?;
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

fn run_command(
    command: &str,
    args: &[String],
    config: &RunConfig,
    control: &RunControl,
    timeout: Option<Duration>,
) -> Result<(), AdapterError> {
    if !config.approved_commands.contains(command) {
        return Err(
            AdapterError::new(format!("command `{command}` is not approved for this run"))
                .with_failure_kind(FailureKind::PolicyDenied)
                .with_source("failureKind=policyDenied"),
        );
    }

    let mut child = Command::new(command)
        .args(args)
        .current_dir(config.working_directory.as_deref().unwrap_or("."))
        .envs(&config.environment)
        .spawn()
        .map_err(|_| transient_error("macOS could not start the approved command"))?;
    let started_at = Instant::now();
    loop {
        if control.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AdapterError::cancelled());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|_| transient_error("macOS could not read command status"))?
        {
            return if status.success() {
                Ok(())
            } else {
                Err(transient_error(format!(
                    "approved command `{command}` exited with {status}"
                )))
            };
        }
        if timeout.is_some_and(|timeout| started_at.elapsed() >= timeout) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AdapterError::timeout());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn command_exits(
    command: &str,
    args: &[String],
    config: &RunConfig,
    control: &RunControl,
    timeout: Option<Duration>,
) -> Result<bool, AdapterError> {
    match run_command(command, args, config, control, timeout) {
        Ok(()) => Ok(true),
        Err(error) if error.failure_kind() == Some(FailureKind::Transient) => Ok(false),
        Err(error) => Err(error),
    }
}

fn enumerate_windows() -> Result<Vec<MacWindow>, AdapterError> {
    let options =
        K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS;
    let array = unsafe { CGWindowListCopyWindowInfo(options, 0) };
    if array.is_null() {
        return Err(transient_error("macOS could not enumerate windows"));
    }
    let array = CfOwned::new(array);
    let count = unsafe { CFArrayGetCount(array.as_ref()) };
    let mut windows = Vec::new();
    for index in 0..count {
        let dictionary =
            unsafe { CFArrayGetValueAtIndex(array.as_ref(), index) as CFDictionaryRef };
        if dictionary.is_null() {
            continue;
        }
        let layer = dictionary_i32(dictionary, "kCGWindowLayer").unwrap_or_default();
        if layer != 0 {
            continue;
        }
        let app_name = dictionary_string(dictionary, "kCGWindowOwnerName").unwrap_or_default();
        let process_id = dictionary_i32(dictionary, "kCGWindowOwnerPID").unwrap_or_default() as u32;
        let window_id = dictionary_i32(dictionary, "kCGWindowNumber").unwrap_or_default() as u32;
        let title = dictionary_string(dictionary, "kCGWindowName").unwrap_or_default();
        if app_name.trim().is_empty() || window_id == 0 {
            continue;
        }
        windows.push(MacWindow {
            window_id,
            title,
            app_name,
            process_id,
            bounds: dictionary_bounds(dictionary, "kCGWindowBounds"),
        });
    }
    Ok(windows)
}

fn find_window(target: &Target) -> Result<MacWindow, AdapterError> {
    reject_unsupported_target(unsupported_window_target_reason(target))?;
    let mut matches = enumerate_windows()?
        .into_iter()
        .filter(|window| window_matches_target(window, target))
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Err(AdapterError::new(format!(
            "macOS could not find a window matching {}",
            window_target_summary(target)
        ))
        .with_failure_kind(FailureKind::NotFound)
        .with_source(window_candidate_diagnostics(target)?));
    }
    if matches.len() > 1 {
        let diagnostics = matches
            .iter()
            .map(|window| {
                format!(
                    "{} pid={} title={}",
                    window.app_name,
                    window.process_id,
                    quote(&window.title)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Err(AdapterError::new(format!(
            "macOS found multiple windows matching {}",
            window_target_summary(target)
        ))
        .with_failure_kind(FailureKind::Ambiguous)
        .with_source(diagnostics));
    }
    Ok(matches.remove(0))
}

fn window_matches_target(window: &MacWindow, target: &Target) -> bool {
    if let Some(title) = &target.window_title
        && window.title != *title
    {
        return false;
    }
    if let Some(fragment) = &target.title_contains
        && !window
            .title
            .to_lowercase()
            .contains(&fragment.to_lowercase())
    {
        return false;
    }
    if let Some(app_name) = &target.app_name
        && !window.app_name.eq_ignore_ascii_case(app_name)
    {
        return false;
    }
    if let Some(process_name) = &target.process_name
        && !window.app_name.eq_ignore_ascii_case(process_name)
    {
        return false;
    }
    target.window_title.is_some()
        || target.title_contains.is_some()
        || target.app_name.is_some()
        || target.process_name.is_some()
}

fn window_exists(target: &Target) -> Result<bool, AdapterError> {
    match find_window(target) {
        Ok(_) => Ok(true),
        Err(error) if error.failure_kind() == Some(FailureKind::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

fn window_is_focused(target: &Target) -> Result<bool, AdapterError> {
    let window = match find_window(target) {
        Ok(window) => window,
        Err(error) if error.failure_kind() == Some(FailureKind::NotFound) => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(frontmost_process_name().is_some_and(|name| name.eq_ignore_ascii_case(&window.app_name)))
}

fn process_is_running(target: &Target) -> Result<bool, AdapterError> {
    reject_unsupported_target(unsupported_process_target_reason(target))?;
    let expected = target
        .process_name
        .as_ref()
        .or(target.app_name.as_ref())
        .ok_or_else(|| {
            AdapterError::unsupported("macOS processRunning requires processName or appName")
        })?;
    let output = Command::new("ps")
        .args(["-axo", "comm="])
        .output()
        .map_err(|_| transient_error("macOS could not enumerate processes"))?;
    if !output.status.success() {
        return Err(transient_error("macOS process enumeration failed"));
    }
    let expected_lower = expected.to_lowercase();
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| process_basename(line).eq_ignore_ascii_case(&expected_lower)))
}

fn unsupported_process_target_reason(target: &Target) -> Option<&'static str> {
    if target.window_title.is_some()
        || target.title_contains.is_some()
        || target.url.is_some()
        || target.file_path.is_some()
        || target.accessibility.is_some()
        || target.image.is_some()
        || target.coordinates.is_some()
        || !target.platform_selectors.is_empty()
    {
        return Some(
            "macOS process queries currently support only processName or appName selectors",
        );
    }
    if target.process_name.is_none() && target.app_name.is_none() {
        return Some("macOS process queries require a processName or appName selector");
    }
    None
}

fn process_basename(path: &str) -> String {
    Path::new(path.trim())
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(path.trim())
        .to_lowercase()
}

fn focus_window(target: &Target) -> Result<(), AdapterError> {
    ensure_accessibility_permission()?;
    let window = find_window(target)?;
    let ax_window = ax_window_for(&window)?;
    let app = unsafe { AXUIElementCreateApplication(window.process_id as pid_t) };
    if app.is_null() {
        return Err(transient_error(
            "macOS could not access the target application",
        ));
    }
    let app = CfOwned::new(app);
    ax_set_attribute(app.as_ref(), "AXFrontmost", unsafe { kCFBooleanTrue })
        .map_err(|_| focus_denied_error("macOS could not make the target app frontmost"))?;
    let _ = ax_set_attribute(app.as_ref(), "AXFocusedWindow", ax_window.as_ref());
    let _ = ax_perform_action(ax_window.as_ref(), "AXRaise");
    wait_for_frontmost(&window.app_name, "requested window")
}

fn wait_for_frontmost(app_name: &str, label: &str) -> Result<(), AdapterError> {
    let started_at = Instant::now();
    while started_at.elapsed() < Duration::from_millis(750) {
        if frontmost_process_name().is_some_and(|name| name.eq_ignore_ascii_case(app_name)) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(focus_denied_error(format!(
        "macOS did not foreground the {label}"
    )))
}

fn focus_target(target: &Target, config: &RunConfig) -> Result<(), AdapterError> {
    reject_unsupported_target(unsupported_semantic_target_reason_with_config(
        target, config,
    ))?;
    focus_window(&window_only_target(target))?;
    with_semantic_target(target, |element| {
        ensure_semantic_target_actionable(element)?;
        ax_set_attribute(element, "AXFocused", unsafe { kCFBooleanTrue }).map_err(|_| {
            focus_denied_error("macOS could not focus the requested semantic target")
        })?;
        if ax_bool_attribute(element, "AXFocused").unwrap_or(false) {
            Ok(())
        } else {
            Err(focus_denied_error(
                "macOS did not give focus to the requested semantic target",
            ))
        }
    })
}

fn ax_window_for(window: &MacWindow) -> Result<CfOwned, AdapterError> {
    ensure_accessibility_permission()?;
    let app = unsafe { AXUIElementCreateApplication(window.process_id as pid_t) };
    if app.is_null() {
        return Err(transient_error(
            "macOS could not access the target application",
        ));
    }
    let app = CfOwned::new(app);
    let _ = unsafe { AXUIElementSetMessagingTimeout(app.as_ref(), 1.0) };
    let windows = ax_attribute_array(app.as_ref(), "AXWindows")?;
    let count = unsafe { CFArrayGetCount(windows.as_ref()) };
    for index in 0..count {
        let ax_window =
            unsafe { CFArrayGetValueAtIndex(windows.as_ref(), index) as AXUIElementRef };
        let title = ax_string_attribute(ax_window, "AXTitle").unwrap_or_default();
        if (window.title.is_empty() || title == window.title)
            && ax_bounds(ax_window)
                .is_some_and(|bounds| bounds_roughly_equal(bounds, window.bounds))
        {
            return Ok(CfOwned::new(retain_ax(ax_window)));
        }
        if title == window.title {
            return Ok(CfOwned::new(retain_ax(ax_window)));
        }
    }
    Err(
        AdapterError::new("macOS could not correlate the window with Accessibility")
            .with_failure_kind(FailureKind::NotFound),
    )
}

fn retain_ax(element: AXUIElementRef) -> AXUIElementRef {
    // AX arrays keep child elements alive while the array is retained; returning a retained child
    // avoids coupling callers to the parent array lifetime.
    unsafe extern "C" {
        fn CFRetain(value: CFTypeRef) -> CFTypeRef;
    }
    unsafe { CFRetain(element) as AXUIElementRef }
}

fn inspect_ax_node(
    element: AXUIElementRef,
    path: &[u32],
    depth: u32,
    max_depth: u32,
    include_values: bool,
    remaining: &mut usize,
    truncated: &mut bool,
) -> Result<AccessibilityNode, AdapterError> {
    if *remaining == 0 {
        *truncated = true;
        return Err(
            AdapterError::new("macOS accessibility tree node limit reached")
                .with_failure_kind(FailureKind::TruncatedSearch),
        );
    }
    *remaining -= 1;
    let role = ax_string_attribute(element, "AXRole").unwrap_or_default();
    let subrole = ax_string_attribute(element, "AXSubrole").unwrap_or_default();
    let control_type = if subrole.is_empty() {
        role.clone()
    } else {
        format!("{role}:{subrole}")
    };
    let title = ax_string_attribute(element, "AXTitle")
        .or_else(|| ax_string_attribute(element, "AXDescription"))
        .unwrap_or_default();
    let identifier = ax_string_attribute(element, "AXIdentifier").unwrap_or_default();
    let bounds = ax_bounds(element);
    let click_point = bounds.and_then(|bounds| {
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;
        (width > 0 && height > 0).then_some(AccessibilityPoint {
            x: bounds.left + width / 2,
            y: bounds.top + height / 2,
        })
    });
    let actions = ax_action_names(element).unwrap_or_default();
    let mut node = AccessibilityNode {
        path: path.to_vec(),
        depth,
        name: title,
        automation_id: identifier,
        control_type,
        class_name: role,
        bounds,
        click_point,
        enabled: ax_bool_attribute(element, "AXEnabled"),
        keyboard_focusable: None,
        has_keyboard_focus: ax_bool_attribute(element, "AXFocused"),
        value: if include_values {
            ax_string_attribute(element, "AXValue")
        } else {
            None
        },
        actions,
        selector_candidates: Vec::new(),
        children: Vec::new(),
    };
    node.selector_candidates = selector_candidates_for_node(&node);
    if depth >= max_depth {
        return Ok(node);
    }
    let children = match ax_attribute_array(element, "AXChildren") {
        Ok(children) => children,
        Err(_) => return Ok(node),
    };
    let child_count = unsafe { CFArrayGetCount(children.as_ref()) };
    for index in 0..child_count {
        if *remaining == 0 {
            *truncated = true;
            break;
        }
        let child = unsafe { CFArrayGetValueAtIndex(children.as_ref(), index) as AXUIElementRef };
        let mut child_path = path.to_vec();
        child_path.push(index as u32);
        match inspect_ax_node(
            child,
            &child_path,
            depth + 1,
            max_depth,
            include_values,
            remaining,
            truncated,
        ) {
            Ok(child_node) => node.children.push(child_node),
            Err(error) if error.failure_kind() == Some(FailureKind::TruncatedSearch) => {
                *truncated = true;
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(node)
}

fn semantic_target_exists(target: &Target) -> Result<bool, AdapterError> {
    reject_unsupported_target(unsupported_semantic_target_reason(target))?;
    match with_semantic_target(target, |_| Ok(())) {
        Ok(()) => Ok(true),
        Err(error) if error.failure_kind() == Some(FailureKind::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

fn semantic_target_focused(target: &Target) -> Result<bool, AdapterError> {
    semantic_target_readiness(target, |element| {
        Ok(ax_bool_attribute(element, "AXFocused").unwrap_or(false))
    })
}

fn semantic_target_enabled(target: &Target) -> Result<bool, AdapterError> {
    semantic_target_readiness(target, |element| {
        Ok(ax_bool_attribute(element, "AXEnabled").unwrap_or(true))
    })
}

fn semantic_target_visible(target: &Target) -> Result<bool, AdapterError> {
    semantic_target_readiness(target, |element| {
        Ok(ax_bounds(element).is_some_and(non_empty_bounds))
    })
}

fn semantic_target_actionable(target: &Target) -> Result<bool, AdapterError> {
    semantic_target_readiness(target, |element| {
        Ok(ax_bool_attribute(element, "AXEnabled").unwrap_or(true)
            && ax_bounds(element).is_some_and(non_empty_bounds))
    })
}

fn semantic_target_name_contains(target: &Target, text: &str) -> Result<bool, AdapterError> {
    let expected = text.to_lowercase();
    semantic_target_readiness(target, |element| {
        let name = ax_string_attribute(element, "AXTitle")
            .or_else(|| ax_string_attribute(element, "AXDescription"))
            .unwrap_or_default();
        Ok(name.to_lowercase().contains(&expected))
    })
}

fn semantic_target_value_contains(
    target: &Target,
    text: &str,
    config: &RunConfig,
) -> Result<bool, AdapterError> {
    if !config.allow_value_capture {
        return Err(policy_denied_error(
            "runtime value reads require explicit allowValueCapture approval",
        ));
    }
    let expected = text.to_lowercase();
    semantic_target_readiness(target, |element| {
        Ok(ax_string_attribute(element, "AXValue")
            .unwrap_or_default()
            .to_lowercase()
            .contains(&expected))
    })
}

fn semantic_target_readiness(
    target: &Target,
    operation: impl FnOnce(AXUIElementRef) -> Result<bool, AdapterError>,
) -> Result<bool, AdapterError> {
    match with_semantic_target(target, operation) {
        Ok(value) => Ok(value),
        Err(error) if error.failure_kind() == Some(FailureKind::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

fn with_semantic_target<T>(
    target: &Target,
    operation: impl FnOnce(AXUIElementRef) -> Result<T, AdapterError>,
) -> Result<T, AdapterError> {
    ensure_accessibility_permission()?;
    let accessibility = target.accessibility.as_ref().ok_or_else(|| {
        AdapterError::unsupported("semantic targets require an accessibility selector")
    })?;
    let window = find_window(target)?;
    let ax_window = ax_window_for(&window)?;
    let element = if let Some(path) = &accessibility.path {
        ax_descendant_by_path(ax_window.as_ref(), path)?
    } else {
        find_ax_descendant(ax_window.as_ref(), accessibility)?
    };
    operation(element.as_ref())
}

fn ax_descendant_by_path(root: AXUIElementRef, path: &[u32]) -> Result<CfOwned, AdapterError> {
    let mut current = CfOwned::new(retain_ax(root));
    for index in path {
        let children = ax_attribute_array(current.as_ref(), "AXChildren")
            .map_err(|_| semantic_target_not_found())?;
        if *index as isize >= unsafe { CFArrayGetCount(children.as_ref()) } {
            return Err(semantic_target_not_found());
        }
        let child =
            unsafe { CFArrayGetValueAtIndex(children.as_ref(), *index as isize) as AXUIElementRef };
        current = CfOwned::new(retain_ax(child));
    }
    Ok(current)
}

fn find_ax_descendant(
    root: AXUIElementRef,
    desired: &cueflow_core::AccessibilityTarget,
) -> Result<CfOwned, AdapterError> {
    let mut matches = Vec::new();
    let mut visited = 0usize;
    collect_ax_matches(root, desired, 0, &mut visited, &mut matches)?;
    if matches.is_empty() {
        return Err(semantic_target_not_found());
    }
    if matches.len() > 1 {
        return Err(AdapterError::new(
            "macOS found multiple semantic targets matching the selector",
        )
        .with_failure_kind(FailureKind::Ambiguous)
        .with_source(format!("matched {} accessibility elements", matches.len())));
    }
    Ok(CfOwned::new(matches.remove(0)))
}

fn collect_ax_matches(
    element: AXUIElementRef,
    desired: &cueflow_core::AccessibilityTarget,
    depth: u32,
    visited: &mut usize,
    matches: &mut Vec<AXUIElementRef>,
) -> Result<(), AdapterError> {
    if *visited >= SEMANTIC_SEARCH_MAX_NODES {
        return Err(
            AdapterError::new("macOS semantic search exceeded the node limit")
                .with_failure_kind(FailureKind::TruncatedSearch),
        );
    }
    *visited += 1;
    if ax_element_matches(element, desired) {
        matches.push(retain_ax(element));
    }
    if depth >= SEMANTIC_SEARCH_MAX_DEPTH {
        return Ok(());
    }
    let Ok(children) = ax_attribute_array(element, "AXChildren") else {
        return Ok(());
    };
    let child_count = unsafe { CFArrayGetCount(children.as_ref()) };
    for index in 0..child_count {
        let child = unsafe { CFArrayGetValueAtIndex(children.as_ref(), index) as AXUIElementRef };
        collect_ax_matches(child, desired, depth + 1, visited, matches)?;
    }
    Ok(())
}

fn ax_element_matches(
    element: AXUIElementRef,
    desired: &cueflow_core::AccessibilityTarget,
) -> bool {
    if let Some(id) = &desired.id
        && ax_string_attribute(element, "AXIdentifier").as_deref() != Some(id.as_str())
    {
        return false;
    }
    if let Some(name) = &desired.name {
        let title = ax_string_attribute(element, "AXTitle")
            .or_else(|| ax_string_attribute(element, "AXDescription"))
            .unwrap_or_default();
        if title != *name {
            return false;
        }
    }
    if let Some(control_type) = &desired.control_type {
        let role = ax_string_attribute(element, "AXRole").unwrap_or_default();
        let subrole = ax_string_attribute(element, "AXSubrole").unwrap_or_default();
        if !role.eq_ignore_ascii_case(control_type)
            && !subrole.eq_ignore_ascii_case(control_type)
            && !format!("{role}:{subrole}").eq_ignore_ascii_case(control_type)
        {
            return false;
        }
    }
    true
}

fn semantic_target_not_found() -> AdapterError {
    AdapterError::new("macOS could not find the requested semantic target")
        .with_failure_kind(FailureKind::NotFound)
}

fn ensure_semantic_target_actionable(element: AXUIElementRef) -> Result<(), AdapterError> {
    if ax_bool_attribute(element, "AXEnabled") == Some(false) {
        return Err(AdapterError::new("macOS semantic target is disabled")
            .with_failure_kind(FailureKind::Disabled));
    }
    if !ax_bounds(element).is_some_and(non_empty_bounds) {
        return Err(
            AdapterError::new("macOS semantic target is offscreen or has empty bounds")
                .with_failure_kind(FailureKind::Offscreen),
        );
    }
    Ok(())
}

fn perform_first_supported_action(
    element: AXUIElementRef,
    candidates: &[&str],
) -> Result<(), AdapterError> {
    let actions = ax_action_names(element).unwrap_or_default();
    for candidate in candidates {
        if actions.iter().any(|action| action == candidate) {
            return ax_perform_action(element, candidate).map_err(|_| {
                AdapterError::new("macOS could not perform the requested semantic action")
            });
        }
    }
    Err(AdapterError::unsupported(
        "target does not support the requested semantic action",
    ))
}

fn ax_attribute(element: AXUIElementRef, attribute: &str) -> Result<CfOwned, AXError> {
    let attribute = CfString::new(attribute).map_err(|_| -1)?;
    let mut value = ptr::null();
    let error = unsafe { AXUIElementCopyAttributeValue(element, attribute.as_ref(), &mut value) };
    if error == K_AX_ERROR_SUCCESS && !value.is_null() {
        Ok(CfOwned::new(value))
    } else {
        Err(error)
    }
}

fn ax_attribute_array(element: AXUIElementRef, attribute: &str) -> Result<CfOwned, AdapterError> {
    let attribute = CfString::new(attribute)?;
    let mut value = ptr::null();
    let error = unsafe {
        AXUIElementCopyAttributeValues(element, attribute.as_ref(), 0, 10_000, &mut value)
    };
    if error == K_AX_ERROR_SUCCESS && !value.is_null() {
        Ok(CfOwned::new(value))
    } else {
        Err(AdapterError::new(format!(
            "macOS could not read accessibility attribute {attribute_name}",
            attribute_name = attribute_name_for_error(attribute.as_ref())
        )))
    }
}

fn ax_set_attribute(
    element: AXUIElementRef,
    attribute: &str,
    value: CFTypeRef,
) -> Result<(), AXError> {
    let attribute = CfString::new(attribute).map_err(|_| -1)?;
    let error = unsafe { AXUIElementSetAttributeValue(element, attribute.as_ref(), value) };
    if error == K_AX_ERROR_SUCCESS {
        Ok(())
    } else {
        Err(error)
    }
}

fn ax_perform_action(element: AXUIElementRef, action: &str) -> Result<(), AXError> {
    let action = CfString::new(action).map_err(|_| -1)?;
    let error = unsafe { AXUIElementPerformAction(element, action.as_ref()) };
    if error == K_AX_ERROR_SUCCESS {
        Ok(())
    } else {
        Err(error)
    }
}

fn ax_string_attribute(element: AXUIElementRef, attribute: &str) -> Option<String> {
    ax_attribute(element, attribute)
        .ok()
        .and_then(|value| cf_string_to_string(value.as_ref() as CFStringRef))
}

fn ax_bool_attribute(element: AXUIElementRef, attribute: &str) -> Option<bool> {
    ax_attribute(element, attribute)
        .ok()
        .map(|value| unsafe { CFBooleanGetValue(value.as_ref() as CFBooleanRef) })
}

fn ax_action_names(element: AXUIElementRef) -> Result<Vec<String>, AdapterError> {
    let mut names = ptr::null();
    let error = unsafe { AXUIElementCopyActionNames(element, &mut names) };
    if error != K_AX_ERROR_SUCCESS || names.is_null() {
        return Ok(Vec::new());
    }
    let names = CfOwned::new(names);
    let count = unsafe { CFArrayGetCount(names.as_ref()) };
    let mut actions = Vec::new();
    for index in 0..count {
        let value = unsafe { CFArrayGetValueAtIndex(names.as_ref(), index) as CFStringRef };
        if let Some(action) = cf_string_to_string(value) {
            actions.push(action);
        }
    }
    Ok(actions)
}

fn ax_bounds(element: AXUIElementRef) -> Option<AccessibilityBounds> {
    let position = ax_attribute(element, "AXPosition").ok()?;
    let size = ax_attribute(element, "AXSize").ok()?;
    let mut point = CGPoint::default();
    let mut dimensions = CGSize::default();
    let point_ok = unsafe {
        AXValueGetValue(
            position.as_ref(),
            K_AX_VALUE_CG_POINT_TYPE,
            (&mut point as *mut CGPoint).cast(),
        )
    };
    let size_ok = unsafe {
        AXValueGetValue(
            size.as_ref(),
            K_AX_VALUE_CG_SIZE_TYPE,
            (&mut dimensions as *mut CGSize).cast(),
        )
    };
    if !point_ok || !size_ok {
        return None;
    }
    Some(AccessibilityBounds {
        left: point.x.round() as i32,
        top: point.y.round() as i32,
        right: (point.x + dimensions.width).round() as i32,
        bottom: (point.y + dimensions.height).round() as i32,
    })
}

fn dictionary_string(dictionary: CFDictionaryRef, key: &str) -> Option<String> {
    let key = CfString::new(key).ok()?;
    let value = unsafe { CFDictionaryGetValue(dictionary, key.as_type_ref()) };
    if value.is_null() {
        return None;
    }
    cf_string_to_string(value as CFStringRef)
}

fn dictionary_i32(dictionary: CFDictionaryRef, key: &str) -> Option<i32> {
    let key = CfString::new(key).ok()?;
    let value = unsafe { CFDictionaryGetValue(dictionary, key.as_type_ref()) };
    if value.is_null() {
        return None;
    }
    let mut output = 0i32;
    unsafe {
        CFNumberGetValue(
            value as CFNumberRef,
            K_CF_NUMBER_SINT32_TYPE,
            (&mut output as *mut i32).cast(),
        )
    }
    .then_some(output)
}

fn dictionary_f64(dictionary: CFDictionaryRef, key: &str) -> Option<f64> {
    let key = CfString::new(key).ok()?;
    let value = unsafe { CFDictionaryGetValue(dictionary, key.as_type_ref()) };
    if value.is_null() {
        return None;
    }
    let mut output = 0f64;
    unsafe {
        CFNumberGetValue(
            value as CFNumberRef,
            K_CF_NUMBER_CG_FLOAT64_TYPE,
            (&mut output as *mut f64).cast(),
        )
    }
    .then_some(output)
}

fn dictionary_bounds(dictionary: CFDictionaryRef, key: &str) -> Option<AccessibilityBounds> {
    let key = CfString::new(key).ok()?;
    let bounds = unsafe { CFDictionaryGetValue(dictionary, key.as_type_ref()) as CFDictionaryRef };
    if bounds.is_null() {
        return None;
    }
    let x = dictionary_f64(bounds, "X")?;
    let y = dictionary_f64(bounds, "Y")?;
    let width = dictionary_f64(bounds, "Width")?;
    let height = dictionary_f64(bounds, "Height")?;
    Some(AccessibilityBounds {
        left: x.round() as i32,
        top: y.round() as i32,
        right: (x + width).round() as i32,
        bottom: (y + height).round() as i32,
    })
}

fn cf_string_to_string(value: CFStringRef) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let mut buffer = vec![0i8; 4096];
    let ok = unsafe {
        CFStringGetCString(
            value,
            buffer.as_mut_ptr(),
            buffer.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
        )
    };
    if !ok {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn attribute_name_for_error(_attribute: CFStringRef) -> &'static str {
    "requested"
}

fn send_text(text: &str) -> Result<(), AdapterError> {
    for code_unit in text.encode_utf16() {
        let down = unsafe { CGEventCreateKeyboardEvent(ptr::null_mut(), 0, true) };
        let up = unsafe { CGEventCreateKeyboardEvent(ptr::null_mut(), 0, false) };
        if down.is_null() || up.is_null() {
            return Err(transient_error(
                "macOS could not create keyboard input events",
            ));
        }
        unsafe {
            CGEventKeyboardSetUnicodeString(down, 1, &code_unit);
            CGEventKeyboardSetUnicodeString(up, 1, &code_unit);
            CGEventPost(K_CG_HID_EVENT_TAP, down);
            CGEventPost(K_CG_HID_EVENT_TAP, up);
            CFRelease(down.cast());
            CFRelease(up.cast());
        }
    }
    Ok(())
}

fn send_key_chord(keys: &str) -> Result<(), AdapterError> {
    let parts = keys.split('+').map(str::trim).collect::<Vec<_>>();
    let (last, modifiers) = parts
        .split_last()
        .ok_or_else(|| AdapterError::new("key chord is required"))?;
    let key = parse_virtual_key(last)?;
    let flags = modifiers.iter().try_fold(0u64, |flags, modifier| {
        Ok::<u64, AdapterError>(
            flags
                | match *modifier {
                    "CmdOrControl" | "Command" | "Cmd" | "Meta" => K_CG_EVENT_FLAG_MASK_COMMAND,
                    "Control" | "Ctrl" => K_CG_EVENT_FLAG_MASK_CONTROL,
                    "Shift" => K_CG_EVENT_FLAG_MASK_SHIFT,
                    "Alt" | "Option" => K_CG_EVENT_FLAG_MASK_ALTERNATE,
                    _ => {
                        return Err(AdapterError::unsupported(
                            "key chord contains an unsupported modifier",
                        ));
                    }
                },
        )
    })?;
    post_key_event(key, true, flags)?;
    post_key_event(key, false, flags)
}

fn post_key_event(key: u16, down: bool, flags: u64) -> Result<(), AdapterError> {
    let event = unsafe { CGEventCreateKeyboardEvent(ptr::null_mut(), key, down) };
    if event.is_null() {
        return Err(transient_error(
            "macOS could not create keyboard input events",
        ));
    }
    unsafe {
        CGEventSetFlags(event, flags);
        CGEventPost(K_CG_HID_EVENT_TAP, event);
        CFRelease(event.cast());
    }
    Ok(())
}

fn parse_virtual_key(key: &str) -> Result<u16, AdapterError> {
    Ok(match key {
        "Enter" => 0x24,
        "Tab" => 0x30,
        "Space" => 0x31,
        "Backspace" => 0x33,
        "Escape" => 0x35,
        "Delete" => 0x75,
        "Home" => 0x73,
        "End" => 0x77,
        "ArrowLeft" => 0x7B,
        "ArrowRight" => 0x7C,
        "ArrowDown" => 0x7D,
        "ArrowUp" => 0x7E,
        key if key.len() == 1 => match key.as_bytes()[0].to_ascii_lowercase() {
            b'a' => 0x00,
            b's' => 0x01,
            b'd' => 0x02,
            b'f' => 0x03,
            b'h' => 0x04,
            b'g' => 0x05,
            b'z' => 0x06,
            b'x' => 0x07,
            b'c' => 0x08,
            b'v' => 0x09,
            b'b' => 0x0B,
            b'q' => 0x0C,
            b'w' => 0x0D,
            b'e' => 0x0E,
            b'r' => 0x0F,
            b'y' => 0x10,
            b't' => 0x11,
            b'1' => 0x12,
            b'2' => 0x13,
            b'3' => 0x14,
            b'4' => 0x15,
            b'6' => 0x16,
            b'5' => 0x17,
            b'=' => 0x18,
            b'9' => 0x19,
            b'7' => 0x1A,
            b'-' => 0x1B,
            b'8' => 0x1C,
            b'0' => 0x1D,
            b']' => 0x1E,
            b'o' => 0x1F,
            b'u' => 0x20,
            b'[' => 0x21,
            b'i' => 0x22,
            b'p' => 0x23,
            b'l' => 0x25,
            b'j' => 0x26,
            b'\'' => 0x27,
            b'k' => 0x28,
            b';' => 0x29,
            b'\\' => 0x2A,
            b',' => 0x2B,
            b'/' => 0x2C,
            b'n' => 0x2D,
            b'm' => 0x2E,
            b'.' => 0x2F,
            _ => {
                return Err(AdapterError::unsupported(
                    "key chord contains an unsupported key",
                ));
            }
        },
        _ => {
            return Err(AdapterError::unsupported(
                "key chord contains an unsupported key",
            ));
        }
    })
}

fn send_scroll(delta_y: i32) -> Result<(), AdapterError> {
    if delta_y == 0 {
        return Ok(());
    }
    let event = unsafe {
        CGEventCreateScrollWheelEvent(
            ptr::null_mut(),
            K_CG_SCROLL_EVENT_UNIT_LINE,
            1,
            delta_y.clamp(-10, 10),
        )
    };
    if event.is_null() {
        return Err(transient_error(
            "macOS could not create scroll input events",
        ));
    }
    unsafe {
        CGEventPost(K_CG_HID_EVENT_TAP, event);
        CFRelease(event.cast());
    }
    Ok(())
}

fn click_coordinates(target: &Target) -> Result<(), AdapterError> {
    let coordinates = target
        .coordinates
        .ok_or_else(|| AdapterError::unsupported("coordinate clicks require coordinates"))?;
    let point = CGPoint {
        x: coordinates.x as c_double,
        y: coordinates.y as c_double,
    };
    for event_type in [K_CG_EVENT_LEFT_MOUSE_DOWN, K_CG_EVENT_LEFT_MOUSE_UP] {
        let event = unsafe {
            CGEventCreateMouseEvent(ptr::null_mut(), event_type, point, K_CG_MOUSE_BUTTON_LEFT)
        };
        if event.is_null() {
            return Err(transient_error(
                "macOS could not create pointer input events",
            ));
        }
        unsafe {
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event.cast());
        }
    }
    Ok(())
}

fn capture_screenshot(
    window_id: Option<CGWindowID>,
    path: &Path,
) -> Result<Artifact, AdapterError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| transient_error("macOS could not create screenshot output directory"))?;
    }
    let mut command = Command::new("screencapture");
    command.arg("-x");
    if let Some(window_id) = window_id {
        command.arg("-l").arg(window_id.to_string());
    }
    command.arg(path);
    let status = command
        .status()
        .map_err(|_| transient_error("macOS could not start screenshot capture"))?;
    if !status.success() {
        return Err(transient_error("macOS screenshot capture failed"));
    }
    Ok(Artifact {
        kind: ArtifactKind::Screenshot,
        uri: format!("file://{}", path_display(path)),
        label: Some(if window_id.is_some() {
            "Window screenshot".to_string()
        } else {
            "Desktop screenshot".to_string()
        }),
    })
}

fn capture_window_screenshot_with_limit(
    target: &Target,
    path: &Path,
    config: &RunConfig,
) -> Result<Artifact, AdapterError> {
    let window = find_window(target)?;
    let temp_path = unique_temp_path("cueflow-macos-evidence-window", "png");
    let result = (|| {
        let artifact = capture_screenshot(Some(window.window_id), &temp_path)?;
        let size = fs::metadata(&temp_path)
            .map_err(|_| transient_error("macOS could not read screenshot artifact metadata"))?
            .len();
        enforce_evidence_artifact_size(size, config)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|_| {
                transient_error("macOS could not create screenshot output directory")
            })?;
        }
        fs::copy(&temp_path, path)
            .map_err(|_| transient_error("macOS could not write screenshot artifact"))?;
        Ok(Artifact {
            kind: artifact.kind,
            uri: format!("file://{}", path_display(path)),
            label: artifact.label,
        })
    })();
    let _ = fs::remove_file(&temp_path);
    result
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

fn image_target_exists(target: &Target, config: &RunConfig) -> Result<bool, AdapterError> {
    visual_match(target, config).map(|match_result| match_result.is_some())
}

fn click_image_target(target: &Target, config: &RunConfig) -> Result<(), AdapterError> {
    let window = find_window(target)?;
    let bounds = window
        .bounds
        .ok_or_else(|| transient_error("macOS could not determine window bounds"))?;
    let matched = visual_match_in_window(target, config, &window)?.ok_or_else(|| {
        AdapterError::new("requested image target was not found")
            .with_failure_kind(FailureKind::NotFound)
            .with_source("failureKind=notFound; visualTarget=image")
    })?;
    let target = Target {
        app_name: None,
        process_name: None,
        window_title: None,
        title_contains: None,
        url: None,
        file_path: None,
        accessibility: None,
        image: None,
        coordinates: Some(cueflow_core::Coordinates {
            x: bounds.left + matched.left + (matched.width / 2),
            y: bounds.top + matched.top + (matched.height / 2),
        }),
        platform_selectors: Default::default(),
    };
    click_coordinates(&target)
}

fn visual_match(target: &Target, config: &RunConfig) -> Result<Option<VisualMatch>, AdapterError> {
    let window = find_window(target)?;
    visual_match_in_window(target, config, &window)
}

fn visual_match_in_window(
    target: &Target,
    config: &RunConfig,
    window: &MacWindow,
) -> Result<Option<VisualMatch>, AdapterError> {
    reject_unsupported_target(unsupported_image_target_reason(target, config))?;
    let image = target
        .image
        .as_ref()
        .ok_or_else(|| AdapterError::unsupported("visual matching requires an image target"))?;
    let screenshot = capture_window_bitmap(window, config)?;
    let template = read_bmp_image(Path::new(&image.path))?;
    find_template_match(&screenshot, &template, image)
}

fn capture_window_bitmap(window: &MacWindow, config: &RunConfig) -> Result<BmpImage, AdapterError> {
    let png = unique_temp_path("cueflow-macos-window", "png");
    let bmp = png.with_extension("bmp");
    let result = (|| {
        capture_screenshot(Some(window.window_id), &png)?;
        convert_image_to_bmp(&png, &bmp)?;
        let size = fs::metadata(&bmp)
            .map_err(|_| transient_error("macOS could not read screenshot artifact metadata"))?
            .len();
        enforce_evidence_artifact_size(size, config)?;
        read_bmp_image(&bmp)
    })();
    let _ = fs::remove_file(&png);
    let _ = fs::remove_file(&bmp);
    result
}

fn convert_image_to_bmp(input: &Path, output: &Path) -> Result<(), AdapterError> {
    let status = Command::new("sips")
        .args(["-s", "format", "bmp"])
        .arg(input)
        .arg("--out")
        .arg(output)
        .status()
        .map_err(|_| transient_error("macOS could not start image conversion"))?;
    if !status.success() {
        return Err(transient_error("macOS could not convert screenshot to BMP"));
    }
    Ok(())
}

fn unique_temp_path(prefix: &str, extension: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{nanos}.{extension}",
        std::process::id()
    ))
}

fn read_bmp_image(path: &Path) -> Result<BmpImage, AdapterError> {
    let bytes =
        fs::read(path).map_err(|_| AdapterError::new("macOS could not read image target"))?;
    parse_bmp_image(&bytes)
}

fn parse_bmp_image(bytes: &[u8]) -> Result<BmpImage, AdapterError> {
    if bytes.len() < 54 || &bytes[0..2] != b"BM" {
        return Err(AdapterError::new(
            "image target must be an uncompressed 24bpp or 32bpp BMP",
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
    if width <= 0
        || raw_height == 0
        || planes != 1
        || !matches!(bits_per_pixel, 24 | 32)
        || compression != 0
    {
        return Err(AdapterError::new(
            "image target must be an uncompressed 24bpp or 32bpp BMP",
        ));
    }

    let height = raw_height.unsigned_abs() as i32;
    let bytes_per_pixel = usize::from(bits_per_pixel / 8);
    let row_stride = (usize::from(bits_per_pixel) * width as usize).div_ceil(32) * 4;
    let pixel_len = row_stride
        .checked_mul(height as usize)
        .ok_or_else(|| AdapterError::new("image target BMP dimensions overflowed"))?;
    if bytes.len() < pixel_offset + pixel_len {
        return Err(AdapterError::new(
            "image target BMP pixel data is truncated",
        ));
    }

    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let source = &bytes[pixel_offset..pixel_offset + pixel_len];
    for row in 0..height as usize {
        let source_row = if raw_height < 0 {
            row
        } else {
            height as usize - 1 - row
        };
        for column in 0..width as usize {
            let source_offset = source_row * row_stride + column * bytes_per_pixel;
            let target_offset = (row * width as usize + column) * 4;
            pixels[target_offset..target_offset + 3]
                .copy_from_slice(&source[source_offset..source_offset + 3]);
            pixels[target_offset + 3] = if bytes_per_pixel == 4 {
                source[source_offset + 3]
            } else {
                255
            };
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

fn condition_state(value: bool) -> ConditionState {
    if value {
        ConditionState::Satisfied
    } else {
        ConditionState::Pending
    }
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
        Action::FocusWindow { target } => unsupported_focus_window_target_reason(target, config),
        Action::TypeText {
            target: Some(target),
            ..
        }
        | Action::PressKey {
            target: Some(target),
            ..
        }
        | Action::Scroll {
            target: Some(target),
            ..
        }
        | Action::ClickTarget { target } => {
            if target.image.is_some() {
                return unsupported_image_target_reason(target, config);
            }
            if target.coordinates.is_some() {
                unsupported_coordinate_target_reason(target, config)
            } else {
                unsupported_semantic_target_reason_with_config(target, config)
            }
        }
        Action::WaitFor {
            condition:
                WaitCondition::WindowExists { target } | WaitCondition::WindowFocused { target },
        } => unsupported_window_target_reason_with_config(target, config),
        Action::WaitFor {
            condition: WaitCondition::ProcessRunning { target },
        } => unsupported_process_target_reason(target),
        Action::WaitFor {
            condition:
                WaitCondition::TargetExists { target }
                | WaitCondition::TargetFocused { target }
                | WaitCondition::TargetEnabled { target }
                | WaitCondition::TargetVisible { target }
                | WaitCondition::TargetActionable { target }
                | WaitCondition::TargetNotExists { target }
                | WaitCondition::TargetNameContains { target, .. },
        } => {
            if target.image.is_some() {
                unsupported_image_target_reason(target, config)
            } else {
                unsupported_semantic_target_reason_with_config(target, config)
            }
        }
        Action::WaitFor {
            condition: WaitCondition::TargetValueContains { target, .. },
        } => {
            if !config.allow_value_capture {
                Some("runtime value reads require explicit allowValueCapture approval")
            } else {
                unsupported_semantic_target_reason_with_config(target, config)
            }
        }
        Action::Assert {
            assertion: Assertion::TargetExists { target },
        } => {
            if target.image.is_some() {
                unsupported_image_target_reason(target, config)
            } else if target.accessibility.is_some() {
                unsupported_semantic_target_reason_with_config(target, config)
            } else {
                unsupported_window_target_reason_with_config(target, config)
            }
        }
        Action::Assert {
            assertion: Assertion::Condition { condition },
        } => unsupported_action_reason(
            &Action::WaitFor {
                condition: condition.clone(),
            },
            config,
        ),
        _ => None,
    }
}

fn unsupported_launch_target_reason(target: &Target, config: &RunConfig) -> Option<&'static str> {
    if target.image.is_some() {
        return unsupported_image_target_reason(target, config);
    }
    if target.coordinates.is_some() {
        return Some("launch targets do not support coordinate selectors");
    }
    unsupported_window_target_reason_with_config(target, config)
}

fn unsupported_window_target_reason_with_config(
    target: &Target,
    config: &RunConfig,
) -> Option<&'static str> {
    if target.image.is_some() {
        return unsupported_image_target_reason(target, config);
    }
    if target.coordinates.is_some() {
        return unsupported_coordinate_target_reason(target, config);
    }
    unsupported_window_target_reason(target)
}

fn unsupported_focus_window_target_reason(
    target: &Target,
    config: &RunConfig,
) -> Option<&'static str> {
    if !accessibility_is_trusted() {
        return Some("macOS Accessibility permission is required for window focus automation");
    }
    unsupported_window_target_reason_with_config(target, config)
}

fn unsupported_window_target_reason(target: &Target) -> Option<&'static str> {
    if target.url.is_some() || target.file_path.is_some() {
        return Some(
            "macOS window selectors currently support appName, processName, windowTitle, and titleContains",
        );
    }
    None
}

fn unsupported_semantic_target_reason_with_config(
    target: &Target,
    config: &RunConfig,
) -> Option<&'static str> {
    if let Some(reason) = unsupported_image_target_reason(target, config) {
        return Some(reason);
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
    if !accessibility_is_trusted() {
        return Some("macOS Accessibility permission is required for semantic automation");
    }
    unsupported_semantic_target_reason(target)
}

fn unsupported_semantic_target_reason(target: &Target) -> Option<&'static str> {
    if target.accessibility.is_none() {
        return Some("semantic targets require an accessibility selector");
    }
    if target.window_title.is_none()
        && target.title_contains.is_none()
        && target.app_name.is_none()
        && target.process_name.is_none()
    {
        return Some("semantic targets require a window/app selector");
    }
    unsupported_window_target_reason(target)
}

fn unsupported_coordinate_target_reason(
    target: &Target,
    config: &RunConfig,
) -> Option<&'static str> {
    if !config.allow_coordinate_targets {
        return Some("coordinate targets require explicit allowCoordinateTargets approval");
    }
    if target.window_title.is_some()
        || target.title_contains.is_some()
        || target.app_name.is_some()
        || target.process_name.is_some()
        || target.url.is_some()
        || target.file_path.is_some()
        || target.accessibility.is_some()
        || target.image.is_some()
    {
        return Some("macOS coordinate clicks currently support only absolute screen coordinates");
    }
    None
}

fn unsupported_image_target_reason(target: &Target, config: &RunConfig) -> Option<&'static str> {
    if target.image.is_none() {
        return None;
    }
    if !config.allow_image_targets {
        return Some("image targets require explicit allowImageTargets approval");
    }
    if !config.allow_screenshot_capture {
        return Some("image targets require explicit allowScreenshotCapture approval");
    }
    None
}

fn reject_unsupported_target(reason: Option<&'static str>) -> Result<(), AdapterError> {
    match reason {
        Some(message) if message.contains("require explicit") => Err(policy_denied_error(message)),
        Some(message) => Err(AdapterError::unsupported(message)),
        None => Ok(()),
    }
}

fn policy_denied_error(message: impl Into<String>) -> AdapterError {
    AdapterError::new(message)
        .with_failure_kind(FailureKind::PolicyDenied)
        .with_source("failureKind=policyDenied")
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
        | Action::Scroll { target, .. }
        | Action::OpenFile { target, .. } => target.as_ref(),
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

fn enforce_evidence_artifact_size(size: u64, config: &RunConfig) -> Result<(), AdapterError> {
    let limit = config
        .evidence_max_artifact_bytes
        .unwrap_or(DEFAULT_EVIDENCE_MAX_ARTIFACT_BYTES);
    if size > limit {
        return Err(policy_denied_error(format!(
            "evidence artifact exceeded configured size limit ({size} > {limit} bytes)"
        )));
    }
    Ok(())
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

fn path_display(path: &Path) -> String {
    path.to_string_lossy().replace(' ', "%20")
}

fn window_only_target(target: &Target) -> Target {
    Target {
        app_name: target.app_name.clone(),
        process_name: target.process_name.clone(),
        window_title: target.window_title.clone(),
        title_contains: target.title_contains.clone(),
        url: None,
        file_path: None,
        accessibility: None,
        image: None,
        coordinates: None,
        platform_selectors: Default::default(),
    }
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

fn window_candidate_diagnostics(target: &Target) -> Result<String, AdapterError> {
    Ok(enumerate_windows()?
        .into_iter()
        .take(20)
        .map(|window| {
            format!(
                "{} pid={} title={} while looking for {}",
                window.app_name,
                window.process_id,
                quote(&window.title),
                window_target_summary(target)
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

fn frontmost_process_name() -> Option<String> {
    let output = Command::new("osascript")
        .args([
            "-e",
            "tell application \"System Events\" to get name of first application process whose frontmost is true",
        ])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|name| !name.is_empty())
}

fn bounds_roughly_equal(left: AccessibilityBounds, right: Option<AccessibilityBounds>) -> bool {
    let Some(right) = right else {
        return true;
    };
    (left.left - right.left).abs() <= 2
        && (left.top - right.top).abs() <= 2
        && (left.right - right.right).abs() <= 2
        && (left.bottom - right.bottom).abs() <= 2
}

fn non_empty_bounds(bounds: AccessibilityBounds) -> bool {
    bounds.right > bounds.left && bounds.bottom > bounds.top
}

fn selector_candidates_for_node(node: &AccessibilityNode) -> Vec<AccessibilitySelectorCandidate> {
    let mut candidates = Vec::new();
    if !node.automation_id.is_empty() {
        candidates.push(selector_candidate(
            node,
            SelectorConfidence::High,
            95,
            Some(node.automation_id.clone()),
            None,
            Some(node.control_type.clone()),
            "AXIdentifier plus role is the most stable macOS selector",
            Vec::new(),
        ));
    }
    if !node.name.is_empty() && !node.control_type.is_empty() {
        candidates.push(selector_candidate(
            node,
            SelectorConfidence::Medium,
            75,
            None,
            Some(node.name.clone()),
            Some(node.control_type.clone()),
            "title/name plus role is readable but may be localized",
            vec!["macOS accessibility titles can be localized or user-content-derived".to_string()],
        ));
    }
    if !node.path.is_empty() {
        candidates.push(selector_candidate(
            node,
            SelectorConfidence::LastResort,
            35,
            None,
            None,
            (!node.control_type.is_empty()).then_some(node.control_type.clone()),
            "child-index path is a last-resort fallback",
            vec!["path-only selectors require allowPathOnlySelectors approval".to_string()],
        ));
    }
    candidates
}

fn selector_candidate(
    node: &AccessibilityNode,
    confidence: SelectorConfidence,
    score: u8,
    id: Option<String>,
    name: Option<String>,
    control_type: Option<String>,
    rationale: &str,
    warnings: Vec<String>,
) -> AccessibilitySelectorCandidate {
    AccessibilitySelectorCandidate {
        confidence,
        score,
        target: Target {
            app_name: None,
            process_name: None,
            window_title: None,
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: Some(cueflow_core::AccessibilityTarget {
                id,
                name,
                control_type,
                path: Some(node.path.clone()),
            }),
            image: None,
            coordinates: None,
            platform_selectors: Default::default(),
        },
        rationale: rationale.to_string(),
        changes: Vec::new(),
        warnings,
    }
}

fn collect_repair_candidates(
    node: &AccessibilityNode,
    desired: Option<&cueflow_core::AccessibilityTarget>,
    candidates: &mut Vec<AccessibilitySelectorCandidate>,
) {
    let node_matches = desired.is_none_or(|desired| node_matches_accessibility(node, desired));
    if node_matches {
        candidates.extend(node.selector_candidates.clone());
    }
    for child in &node.children {
        collect_repair_candidates(child, desired, candidates);
    }
}

fn node_matches_accessibility(
    node: &AccessibilityNode,
    desired: &cueflow_core::AccessibilityTarget,
) -> bool {
    if let Some(id) = &desired.id
        && node.automation_id != *id
    {
        return false;
    }
    if let Some(name) = &desired.name
        && node.name != *name
    {
        return false;
    }
    if let Some(control_type) = &desired.control_type
        && !node.control_type.eq_ignore_ascii_case(control_type)
        && !node.class_name.eq_ignore_ascii_case(control_type)
    {
        return false;
    }
    true
}

fn selector_candidate_changes(
    original: Option<&cueflow_core::AccessibilityTarget>,
    candidate: Option<&cueflow_core::AccessibilityTarget>,
) -> Vec<String> {
    let Some(candidate) = candidate else {
        return Vec::new();
    };
    let mut changes = Vec::new();
    let original = original
        .cloned()
        .unwrap_or(cueflow_core::AccessibilityTarget {
            id: None,
            name: None,
            control_type: None,
            path: None,
        });
    if original.id != candidate.id {
        changes.push(format!(
            "id: {} -> {}",
            optional_value(original.id.as_deref()),
            optional_value(candidate.id.as_deref())
        ));
    }
    if original.name != candidate.name {
        changes.push(format!(
            "name: {} -> {}",
            optional_value(original.name.as_deref()),
            optional_value(candidate.name.as_deref())
        ));
    }
    if original.control_type != candidate.control_type {
        changes.push(format!(
            "controlType: {} -> {}",
            optional_value(original.control_type.as_deref()),
            optional_value(candidate.control_type.as_deref())
        ));
    }
    if original.path != candidate.path {
        changes.push("path updated from current accessibility tree".to_string());
    }
    changes
}

fn optional_value(value: Option<&str>) -> String {
    value.map(quote).unwrap_or_else(|| "None".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target_with_path_only_selector() -> Target {
        Target {
            app_name: Some("TextEdit".to_string()),
            process_name: None,
            window_title: Some("Untitled".to_string()),
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: Some(cueflow_core::AccessibilityTarget {
                id: None,
                name: None,
                control_type: None,
                path: Some(vec![0, 1]),
            }),
            image: None,
            coordinates: None,
            platform_selectors: Default::default(),
        }
    }

    fn target_with_image_selector() -> Target {
        Target {
            app_name: Some("TextEdit".to_string()),
            process_name: None,
            window_title: None,
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: None,
            image: Some(ImageTarget {
                path: "examples/missing-image-target.bmp".to_string(),
                confidence: Some(100),
                region: Some(ImageRegion {
                    left: 0,
                    top: 0,
                    width: 20,
                    height: 20,
                }),
            }),
            coordinates: None,
            platform_selectors: Default::default(),
        }
    }

    #[test]
    fn capabilities_report_macos_platform() {
        let capabilities = MacOsDesktopAdapter::capabilities();

        assert_eq!(capabilities.platform, Platform::MacOs);
        assert!(capabilities.supports_launch);
        assert!(capabilities.supports_focus);
        assert!(capabilities.supports_window_queries);
    }

    #[test]
    fn preflight_rejects_path_only_semantic_targets_without_policy() {
        let adapter = MacOsDesktopAdapter;
        let diagnostics = adapter.preflight(
            &Action::ClickTarget {
                target: target_with_path_only_selector(),
            },
            &RunConfig {
                allow_path_only_selectors: false,
                ..RunConfig::default()
            },
        );

        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("allowPathOnlySelectors")
                || diagnostic.message.contains("Accessibility permission")
        }));
    }

    #[test]
    fn preflight_rejects_coordinate_targets_without_policy() {
        let adapter = MacOsDesktopAdapter;
        let diagnostics = adapter.preflight(
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
                    platform_selectors: Default::default(),
                },
            },
            &RunConfig::default(),
        );

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("allowCoordinateTargets"));
    }

    #[test]
    fn preflight_rejects_focus_without_accessibility_permission() {
        if accessibility_is_trusted() {
            return;
        }
        let adapter = MacOsDesktopAdapter;
        let diagnostics = adapter.preflight(
            &Action::FocusWindow {
                target: Target {
                    app_name: None,
                    process_name: None,
                    window_title: None,
                    title_contains: Some("Google".to_string()),
                    url: None,
                    file_path: None,
                    accessibility: None,
                    image: None,
                    coordinates: None,
                    platform_selectors: Default::default(),
                },
            },
            &RunConfig::default(),
        );

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Accessibility permission"));
    }

    #[test]
    fn preflight_accepts_approved_image_targets() {
        let adapter = MacOsDesktopAdapter;
        let diagnostics = adapter.preflight(
            &Action::ClickTarget {
                target: target_with_image_selector(),
            },
            &RunConfig {
                allow_image_targets: true,
                allow_screenshot_capture: true,
                ..RunConfig::default()
            },
        );

        assert!(diagnostics.is_empty());
    }

    #[test]
    fn preflight_rejects_process_queries_with_window_selectors() {
        let adapter = MacOsDesktopAdapter;
        let diagnostics = adapter.preflight(
            &Action::WaitFor {
                condition: WaitCondition::ProcessRunning {
                    target: Target {
                        app_name: None,
                        process_name: Some("TextEdit".to_string()),
                        window_title: Some("Untitled".to_string()),
                        title_contains: None,
                        url: None,
                        file_path: None,
                        accessibility: None,
                        image: None,
                        coordinates: None,
                        platform_selectors: Default::default(),
                    },
                },
            },
            &RunConfig::default(),
        );

        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0]
                .message
                .contains("processName or appName selectors")
        );
    }

    #[test]
    fn visual_match_finds_bounded_template() {
        let screenshot = BmpImage {
            width: 3,
            height: 3,
            pixels: vec![
                0, 0, 0, 255, 1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255, 5, 0, 0, 255,
                6, 0, 0, 255, 7, 0, 0, 255, 8, 0, 0, 255,
            ],
        };
        let template = BmpImage {
            width: 2,
            height: 2,
            pixels: vec![4, 0, 0, 255, 5, 0, 0, 255, 7, 0, 0, 255, 8, 0, 0, 255],
        };
        let image = ImageTarget {
            path: "unused.bmp".to_string(),
            confidence: Some(100),
            region: Some(ImageRegion {
                left: 1,
                top: 1,
                width: 2,
                height: 2,
            }),
        };

        let matched = find_template_match(&screenshot, &template, &image)
            .expect("match succeeds")
            .expect("template is found");

        assert_eq!(matched.left, 1);
        assert_eq!(matched.top, 1);
        assert_eq!(matched.confidence, 100);
    }

    #[test]
    fn window_target_summary_includes_app_and_title() {
        let target = Target {
            app_name: Some("TextEdit".to_string()),
            process_name: None,
            window_title: Some("Untitled".to_string()),
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: None,
            image: None,
            coordinates: None,
            platform_selectors: Default::default(),
        };

        assert_eq!(
            window_target_summary(&target),
            "windowTitle=\"Untitled\", appName=\"TextEdit\""
        );
    }
}
