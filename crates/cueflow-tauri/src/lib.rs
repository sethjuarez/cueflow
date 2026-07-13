use cueflow_adapters::{CurrentPlatformAdapter, current_platform};
use cueflow_core::{AutomationDefinition, RunConfig};
use cueflow_executor::{
    AutomationExecutor, ExecutorError, RunControl, RunEventSink, RunReport, SystemClock,
};

#[derive(Debug, Clone, PartialEq)]
pub struct RunAutomationRequest {
    pub automation: AutomationDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    pub name: &'static str,
    pub description: &'static str,
}

pub fn plugin_info() -> PluginInfo {
    PluginInfo {
        name: "cueflow",
        description: "Thin application bridge for submitting Cueflow automation run requests.",
    }
}

pub fn run_automation<S: RunEventSink>(
    request: RunAutomationRequest,
    mut config: RunConfig,
    control: &RunControl,
    sink: &mut S,
) -> Result<RunReport, ExecutorError> {
    let executor = AutomationExecutor::new();
    let mut adapter = CurrentPlatformAdapter::new();
    let clock = SystemClock::default();
    config.platform = Some(current_platform());

    executor.run_with(
        &request.automation,
        config,
        &mut adapter,
        control,
        sink,
        &clock,
    )
}

#[cfg(test)]
mod tests {
    use cueflow_core::{Action, CURRENT_SCHEMA_VERSION, RunConfig, RunEvent, Step};
    use cueflow_executor::{RunEventSink, RunOutcome};

    use super::*;

    #[derive(Default)]
    struct CollectingSink(Vec<RunEvent>);

    impl RunEventSink for CollectingSink {
        fn emit(&mut self, event: &RunEvent) {
            self.0.push(event.clone());
        }
    }

    #[test]
    fn exposes_stable_plugin_identity() {
        let info = plugin_info();

        assert_eq!(info.name, "cueflow");
        assert!(info.description.contains("automation"));
    }

    #[test]
    fn bridge_runs_a_dry_run_with_structured_events() {
        let request = RunAutomationRequest {
            automation: AutomationDefinition {
                id: "dry-run".to_string(),
                title: "Dry run".to_string(),
                description: None,
                schema_version: CURRENT_SCHEMA_VERSION,
                version: None,
                variables: Default::default(),
                metadata: Default::default(),
                steps: vec![Step {
                    id: "launch".to_string(),
                    label: None,
                    action: Action::LaunchUrl {
                        url: "https://cueflow.dev".to_string(),
                        target: None,
                    },
                    timeout: None,
                    retry: Default::default(),
                    on_error: Default::default(),
                    conditions: Vec::new(),
                    platform_overrides: Vec::new(),
                }],
            },
        };
        let control = RunControl::default();
        let mut sink = CollectingSink::default();

        let report = run_automation(request, RunConfig::default(), &control, &mut sink)
            .expect("dry run succeeds");

        assert_eq!(report.outcome, RunOutcome::Succeeded);
        assert_eq!(sink.0, report.events);
    }
}
