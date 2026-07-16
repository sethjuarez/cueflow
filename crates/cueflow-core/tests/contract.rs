use std::collections::BTreeMap;

use cueflow_core::{
    Action, DefinitionParseError, Platform, PlatformSelector, Target, ValidationError,
    automation_definition_schema, parse_definition_json,
};
use serde_json::Value;

const AUTOMATION_V1: &str = include_str!("fixtures/automation-v1.json");
const AUTOMATION_V0: &str = include_str!("fixtures/automation-v0.json");
const EDGE_DEMO_READY: &str = include_str!("../../../examples/edge-demo-ready.json");
const EDGE_PAGE_DRILL: &str = include_str!("../../../examples/edge-page-drill.json");
const EDGE_SEARCH_CUEFLOW: &str = include_str!("../../../examples/edge-search-cueflow.json");
const WINDOWS_SETTINGS_READINESS_DRILL: &str =
    include_str!("../../../examples/windows-settings-readiness-drill.json");
const WINDOWS_ACTIONABILITY_READINESS_DRILL: &str =
    include_str!("../../../examples/windows-actionability-readiness-drill.json");
const WINDOWS_MISSING_WINDOW_TIMEOUT_DRILL: &str =
    include_str!("../../../examples/windows-missing-window-timeout-drill.json");
const WINDOWS_IMAGE_TARGET_POLICY_DRILL: &str =
    include_str!("../../../examples/windows-image-target-policy-drill.json");
const WINDOWS_PATH_ONLY_POLICY_DRILL: &str =
    include_str!("../../../examples/windows-path-only-policy-drill.json");
const WINDOWS_COORDINATE_POLICY_DRILL: &str =
    include_str!("../../../examples/windows-coordinate-policy-drill.json");
const WINDOWS_DRILL_MANIFEST: &str = include_str!("../../../examples/windows-drill-manifest.json");

#[test]
fn v1_fixture_is_a_stable_json_round_trip() {
    let definition = parse_definition_json(AUTOMATION_V1).expect("v1 fixture parses");

    let expected: Value = serde_json::from_str(AUTOMATION_V1).expect("fixture is JSON");
    let actual = serde_json::to_value(definition).expect("definition serializes");

    assert_eq!(actual, expected);
}

#[test]
fn generated_schema_describes_the_public_definition() {
    let schema = automation_definition_schema();

    assert_eq!(
        schema["$defs"]["AutomationDefinition"]["properties"]["schemaVersion"]["const"],
        1
    );
    assert_eq!(
        schema["$defs"]["AutomationDefinition"]["properties"]["steps"]["minItems"],
        1
    );
    assert_eq!(
        schema["$defs"]["DurationSpec"]["properties"]["millis"]["minimum"],
        1
    );
    assert_eq!(
        schema["$defs"]["ImageTarget"]["properties"]["confidence"]["minimum"],
        1
    );
    assert_eq!(
        schema["$defs"]["ImageRegion"]["properties"]["width"]["minimum"],
        1
    );
    assert_eq!(
        schema["$defs"]["ImageRegion"]["properties"]["height"]["minimum"],
        1
    );
}

#[test]
fn old_schema_versions_are_rejected_at_the_migration_boundary() {
    let error = parse_definition_json(AUTOMATION_V0).expect_err("v0 is unsupported");

    assert!(matches!(
        error,
        DefinitionParseError::UnsupportedSchemaVersion {
            expected: 1,
            actual: 0
        }
    ));
}

#[test]
fn unknown_persisted_fields_are_rejected_instead_of_lost() {
    let mut document: Value = serde_json::from_str(AUTOMATION_V1).expect("fixture is JSON");
    document["futureTopLevel"] = Value::Bool(true);

    let top_level_error =
        parse_definition_json(&document.to_string()).expect_err("unknown top-level field fails");
    assert!(matches!(top_level_error, DefinitionParseError::Json(_)));

    document
        .as_object_mut()
        .expect("document object")
        .remove("futureTopLevel");
    document["steps"][0]["futureActionField"] = Value::Bool(true);

    let nested_error =
        parse_definition_json(&document.to_string()).expect_err("unknown action field fails");
    assert!(matches!(nested_error, DefinitionParseError::Json(_)));
}

#[test]
fn blank_selectors_are_rejected_even_when_another_selector_is_present() {
    let mut document: Value = serde_json::from_str(AUTOMATION_V1).expect("fixture is JSON");
    document["steps"][1]["target"]["processName"] = Value::String("   ".to_string());

    let error = parse_definition_json(&document.to_string()).expect_err("blank selector fails");
    assert!(matches!(
        error,
        DefinitionParseError::Validation(ValidationError::BlankField("target processName"))
    ));
}

#[test]
fn image_target_regions_validate_non_zero_dimensions() {
    let mut document: Value = serde_json::from_str(AUTOMATION_V1).expect("fixture is JSON");
    document["steps"][1]["target"] = serde_json::json!({
        "image": {
            "path": "fixtures/search-button.bmp",
            "region": {
                "left": 0,
                "top": 0,
                "width": 0,
                "height": 100
            }
        }
    });

    let error = parse_definition_json(&document.to_string()).expect_err("empty region fails");
    assert!(matches!(
        error,
        DefinitionParseError::Validation(ValidationError::InvalidImageRegion)
    ));
}

#[test]
fn edge_demo_fixture_remains_a_valid_portable_definition() {
    let definition = parse_definition_json(EDGE_DEMO_READY).expect("edge fixture parses");

    assert_eq!(definition.id, "edge-demo-ready");
    assert_eq!(
        definition.portability(),
        cueflow_core::Portability::Portable
    );
}

#[test]
fn edge_search_fixture_remains_a_valid_portable_definition() {
    let definition =
        parse_definition_json(EDGE_SEARCH_CUEFLOW).expect("edge search fixture parses");

    assert_eq!(definition.id, "edge-search-cueflow");
    assert_eq!(
        definition.portability(),
        cueflow_core::Portability::Portable
    );
}

#[test]
fn edge_page_drill_fixture_remains_a_valid_portable_definition() {
    let definition = parse_definition_json(EDGE_PAGE_DRILL).expect("edge page drill parses");

    assert_eq!(definition.id, "edge-page-drill");
    assert_eq!(
        definition.portability(),
        cueflow_core::Portability::Portable
    );
}

#[test]
fn windows_settings_readiness_drill_remains_a_valid_definition() {
    let definition =
        parse_definition_json(WINDOWS_SETTINGS_READINESS_DRILL).expect("settings drill parses");

    assert_eq!(definition.id, "windows-settings-readiness-drill");
}

#[test]
fn windows_actionability_readiness_drill_remains_a_valid_definition() {
    let definition = parse_definition_json(WINDOWS_ACTIONABILITY_READINESS_DRILL)
        .expect("actionability drill parses");

    assert_eq!(definition.id, "windows-actionability-readiness-drill");
}

#[test]
fn windows_missing_window_timeout_drill_remains_a_valid_definition() {
    let definition = parse_definition_json(WINDOWS_MISSING_WINDOW_TIMEOUT_DRILL)
        .expect("missing window drill parses");

    assert_eq!(definition.id, "windows-missing-window-timeout-drill");
}

#[test]
fn windows_image_target_policy_drill_remains_a_valid_definition() {
    let definition =
        parse_definition_json(WINDOWS_IMAGE_TARGET_POLICY_DRILL).expect("image drill parses");

    assert_eq!(definition.id, "windows-image-target-policy-drill");
}

#[test]
fn windows_path_only_policy_drill_remains_a_valid_definition() {
    let definition =
        parse_definition_json(WINDOWS_PATH_ONLY_POLICY_DRILL).expect("path-only drill parses");

    assert_eq!(definition.id, "windows-path-only-policy-drill");
}

#[test]
fn windows_coordinate_policy_drill_remains_a_valid_definition() {
    let definition =
        parse_definition_json(WINDOWS_COORDINATE_POLICY_DRILL).expect("coordinate drill parses");

    assert_eq!(definition.id, "windows-coordinate-policy-drill");
}

#[test]
fn windows_drill_manifest_remains_well_formed() {
    let manifest: Value =
        serde_json::from_str(WINDOWS_DRILL_MANIFEST).expect("drill manifest is JSON");

    assert_eq!(manifest["id"], "windows-foundation-drills");
    assert!(
        manifest["drills"]
            .as_array()
            .expect("drills is an array")
            .len()
            >= 2
    );
}

#[test]
fn platform_selector_replaces_generic_target_fields_for_execution() {
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
    };

    let Action::FocusWindow { target } = action.for_platform(Some(Platform::Windows)) else {
        panic!("action remains a window focus action");
    };
    assert_eq!(target.window_title.as_deref(), Some("Google"));
    assert!(target.app_name.is_none());
    assert!(target.title_contains.is_none());
    assert!(target.platform_selectors.is_empty());
}

#[test]
fn accessibility_path_can_stabilize_a_target_without_textual_identifiers() {
    let document = serde_json::json!({
        "id": "path-target",
        "title": "Path target",
        "schemaVersion": 1,
        "steps": [{
            "id": "wait-for-path",
            "kind": "waitFor",
            "condition": {
                "kind": "targetExists",
                "target": {
                    "windowTitle": "GitHub Copilot",
                    "accessibility": {
                        "path": [1, 0, 2]
                    }
                }
            }
        }]
    });

    let definition = parse_definition_json(&document.to_string()).expect("path selector parses");
    let condition = match &definition.steps[0].action {
        Action::WaitFor { condition } => condition,
        _ => panic!("step remains a wait"),
    };

    let cueflow_core::WaitCondition::TargetExists { target } = condition else {
        panic!("condition remains a target existence wait");
    };
    assert_eq!(
        target
            .accessibility
            .as_ref()
            .and_then(|accessibility| accessibility.path.as_deref()),
        Some([1, 0, 2].as_slice())
    );
}
