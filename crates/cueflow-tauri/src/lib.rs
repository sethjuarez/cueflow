use cueflow_core::{AutomationDefinition, RunConfig};

#[derive(Debug, Clone, PartialEq)]
pub struct RunAutomationRequest {
    pub automation: AutomationDefinition,
    pub config: RunConfig,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_stable_plugin_identity() {
        let info = plugin_info();

        assert_eq!(info.name, "cueflow");
        assert!(info.description.contains("automation"));
    }
}
