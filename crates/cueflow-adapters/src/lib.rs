use cueflow_core::{Action, Artifact, Platform, RunConfig};
use cueflow_executor::{AdapterError, ExecutionAdapter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterCapabilities {
    pub platform: Platform,
    pub supports_launch: bool,
    pub supports_focus: bool,
    pub supports_input: bool,
    pub supports_window_queries: bool,
    pub supports_process_queries: bool,
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

#[cfg(not(target_os = "windows"))]
#[derive(Debug, Default)]
pub struct CurrentPlatformAdapter {
    noop: NoopDesktopAdapter,
}

#[cfg(not(target_os = "windows"))]
impl CurrentPlatformAdapter {
    pub fn capabilities() -> AdapterCapabilities {
        AdapterCapabilities {
            platform: current_platform(),
            supports_launch: false,
            supports_focus: false,
            supports_input: false,
            supports_window_queries: false,
            supports_process_queries: false,
        }
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
