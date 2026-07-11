use cueflow_core::AutomationDefinition;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderConfig {
    pub capture_screenshots: bool,
    pub consolidate_events: bool,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self {
            capture_screenshots: false,
            consolidate_events: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecorderEvent {
    Started,
    InputCaptured { description: String },
    ScreenshotCaptured { uri: String },
    Stopped,
}

pub trait AutomationRecorder {
    fn start(&mut self, config: RecorderConfig) -> Result<(), RecorderError>;
    fn stop(&mut self) -> Result<AutomationDefinition, RecorderError>;
}

#[derive(Debug, Default)]
pub struct UnsupportedRecorder;

impl AutomationRecorder for UnsupportedRecorder {
    fn start(&mut self, _config: RecorderConfig) -> Result<(), RecorderError> {
        Err(RecorderError::Unsupported(
            "recording capture is intentionally not implemented in the foundation crate",
        ))
    }

    fn stop(&mut self) -> Result<AutomationDefinition, RecorderError> {
        Err(RecorderError::Unsupported(
            "recording capture is intentionally not implemented in the foundation crate",
        ))
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RecorderError {
    #[error("{0}")]
    Unsupported(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_recorder_makes_recording_optional() {
        let mut recorder = UnsupportedRecorder;
        let error = recorder
            .start(RecorderConfig::default())
            .expect_err("recorder is unsupported in the first scaffold");

        assert!(matches!(error, RecorderError::Unsupported(_)));
    }
}
