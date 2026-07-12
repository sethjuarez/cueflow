use std::collections::BTreeMap;

use cueflow_core::{
    Action, DefinitionParseError, Platform, PlatformSelector, Target, ValidationError,
    automation_definition_schema, parse_definition_json,
};
use serde_json::Value;

const AUTOMATION_V1: &str = include_str!("fixtures/automation-v1.json");
const AUTOMATION_V0: &str = include_str!("fixtures/automation-v0.json");
const EDGE_DEMO_READY: &str = include_str!("../../../examples/edge-demo-ready.json");

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
fn edge_demo_fixture_remains_a_valid_portable_definition() {
    let definition = parse_definition_json(EDGE_DEMO_READY).expect("edge fixture parses");

    assert_eq!(definition.id, "edge-demo-ready");
    assert_eq!(
        definition.portability(),
        cueflow_core::Portability::Portable
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
