use cueflow_core::{Action, Artifact, Platform, RunConfig, Target};
use cueflow_executor::{AdapterError, ExecutionAdapter};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterCapabilities {
    pub platform: Platform,
    pub supports_launch: bool,
    pub supports_focus: bool,
    pub supports_input: bool,
    pub supports_semantic_targets: bool,
    pub supports_coordinate_targets: bool,
    pub supports_window_queries: bool,
    pub supports_process_queries: bool,
    pub supports_accessibility_tree: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessibilityTree {
    pub platform: Platform,
    pub window_title: String,
    pub selector: String,
    pub max_depth: u32,
    pub max_nodes: usize,
    pub truncated: bool,
    pub root: AccessibilityNode,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessibilityNode {
    pub path: Vec<u32>,
    pub depth: u32,
    pub name: String,
    pub automation_id: String,
    pub control_type: String,
    pub class_name: String,
    pub bounds: Option<AccessibilityBounds>,
    pub enabled: Option<bool>,
    pub keyboard_focusable: Option<bool>,
    pub has_keyboard_focus: Option<bool>,
    pub value: Option<String>,
    pub actions: Vec<String>,
    pub selector_candidates: Vec<AccessibilitySelectorCandidate>,
    pub children: Vec<AccessibilityNode>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessibilitySelectorCandidate {
    pub confidence: SelectorConfidence,
    pub score: u8,
    pub target: Target,
    pub rationale: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SelectorConfidence {
    High,
    Medium,
    Low,
    LastResort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessibilityBounds {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Debug, Default)]
pub struct NoopDesktopAdapter;

impl ExecutionAdapter for NoopDesktopAdapter {
    fn execute(
        &mut self,
        _action: &Action,
        _config: &RunConfig,
    ) -> Result<Vec<Artifact>, AdapterError> {
        Ok(Vec::new())
    }
}

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::WindowsDesktopAdapter as CurrentPlatformAdapter;

#[cfg(target_os = "windows")]
impl CurrentPlatformAdapter {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(not(target_os = "windows"))]
#[derive(Debug, Default)]
pub struct CurrentPlatformAdapter {
    noop: NoopDesktopAdapter,
}

#[cfg(not(target_os = "windows"))]
impl CurrentPlatformAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn capabilities() -> AdapterCapabilities {
        AdapterCapabilities {
            platform: current_platform(),
            supports_launch: false,
            supports_focus: false,
            supports_input: false,
            supports_semantic_targets: false,
            supports_coordinate_targets: false,
            supports_window_queries: false,
            supports_process_queries: false,
            supports_accessibility_tree: false,
        }
    }

    pub fn inspect_window(
        &self,
        _target: &cueflow_core::Target,
        _max_depth: u32,
        _max_nodes: usize,
    ) -> Result<AccessibilityTree, AdapterError> {
        self.inspect_window_with_options(_target, _max_depth, _max_nodes, false)
    }

    pub fn inspect_window_with_options(
        &self,
        _target: &cueflow_core::Target,
        _max_depth: u32,
        _max_nodes: usize,
        _include_values: bool,
    ) -> Result<AccessibilityTree, AdapterError> {
        Err(AdapterError::unsupported(
            "accessibility tree inspection is not implemented on this platform",
        ))
    }

    pub fn capture_screenshot(
        &self,
        _path: impl AsRef<std::path::Path>,
    ) -> Result<Artifact, AdapterError> {
        Err(AdapterError::unsupported(
            "screenshot capture is not implemented on this platform",
        ))
    }
}

#[cfg(not(target_os = "windows"))]
impl ExecutionAdapter for CurrentPlatformAdapter {
    fn execute(
        &mut self,
        action: &Action,
        config: &RunConfig,
    ) -> Result<Vec<Artifact>, AdapterError> {
        if config.dry_run {
            return self.noop.execute(action, config);
        }

        Err(AdapterError::unsupported(format!(
            "real execution is not implemented for {} on this platform",
            action.kind()
        )))
    }
}

#[cfg(target_os = "windows")]
pub fn current_platform() -> Platform {
    Platform::Windows
}

#[cfg(target_os = "macos")]
pub fn current_platform() -> Platform {
    Platform::MacOs
}

#[cfg(target_os = "linux")]
pub fn current_platform() -> Platform {
    Platform::Linux
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
compile_error!("cueflow-adapters currently recognizes only Windows, macOS, and Linux targets");

#[cfg(test)]
mod tests {
    use cueflow_core::{Action, RunConfig};
    use cueflow_executor::ExecutionAdapter;

    use super::*;

    #[test]
    fn noop_adapter_accepts_actions() {
        let mut adapter = NoopDesktopAdapter;
        let artifacts = adapter
            .execute(
                &Action::LaunchUrl {
                    url: "https://cueflow.dev".to_string(),
                    target: None,
                },
                &RunConfig::default(),
            )
            .expect("noop execution succeeds");

        assert!(artifacts.is_empty());
    }
}
