use std::collections::BTreeMap;

use cueflow_core::{
    Action, NoSecrets, RunConfig, SecretResolutionError, SecretResolver, Step, VariableDefinition,
    VariableResolutionError, VariableSource, VariableType, parse_definition_json, resolve_run,
};
use serde_json::{Value, json};

const AUTOMATION_V1: &str = include_str!("fixtures/automation-v1.json");

struct TestSecrets;

impl SecretResolver for TestSecrets {
    fn resolve_secret(&self, reference: &str) -> Result<Value, SecretResolutionError> {
        match reference {
            "demo-token" => Ok(Value::String("super-secret-token".to_string())),
            _ => Err(SecretResolutionError::Unavailable {
                reference: reference.to_string(),
            }),
        }
    }
}

fn definition_with_variables() -> cueflow_core::AutomationDefinition {
    let mut definition = parse_definition_json(AUTOMATION_V1).expect("fixture parses");
    definition.variables = BTreeMap::from([
        (
            "name".to_string(),
            VariableDefinition {
                value_type: VariableType::String,
                description: None,
                required: false,
                default: Some(VariableSource::Literal {
                    value: Value::String("Cueflow".to_string()),
                }),
            },
        ),
        (
            "greeting".to_string(),
            VariableDefinition {
                value_type: VariableType::String,
                description: None,
                required: false,
                default: Some(VariableSource::Literal {
                    value: Value::String("Hello ${name}".to_string()),
                }),
            },
        ),
        (
            "token".to_string(),
            VariableDefinition {
                value_type: VariableType::String,
                description: None,
                required: false,
                default: Some(VariableSource::SecretReference {
                    reference: "demo-token".to_string(),
                }),
            },
        ),
        (
            "timeout".to_string(),
            VariableDefinition {
                value_type: VariableType::Number,
                description: None,
                required: false,
                default: Some(VariableSource::Literal { value: json!(30) }),
            },
        ),
        (
            "secret-derived".to_string(),
            VariableDefinition {
                value_type: VariableType::String,
                description: None,
                required: false,
                default: Some(VariableSource::Literal {
                    value: Value::String("token=${token}".to_string()),
                }),
            },
        ),
    ]);
    definition.steps.push(Step {
        id: "type-greeting".to_string(),
        label: None,
        action: Action::TypeText {
            text: "${greeting}: ${name}".to_string(),
            target: None,
        },
        timeout: None,
        retry: Default::default(),
        on_error: Default::default(),
        conditions: Vec::new(),
        platform_overrides: Vec::new(),
    });
    definition
}

#[test]
fn resolves_recursive_defaults_and_run_configuration() {
    let definition = definition_with_variables();
    let config = RunConfig {
        environment: BTreeMap::from([("GREETING".to_string(), "${greeting}".to_string())]),
        working_directory: Some("C:\\demo\\${name}".to_string()),
        ..RunConfig::default()
    };

    let resolved = resolve_run(&definition, &config, &TestSecrets).expect("run resolves");

    assert_eq!(
        resolved.variables()["greeting"].value(),
        &Value::String("Hello Cueflow".to_string())
    );
    assert_eq!(
        resolved.config().environment["GREETING"],
        "Hello Cueflow".to_string()
    );
    assert_eq!(
        resolved.config().working_directory.as_deref(),
        Some("C:\\demo\\Cueflow")
    );
    assert_eq!(
        resolved
            .definition()
            .steps
            .last()
            .expect("type step")
            .action,
        Action::TypeText {
            text: "Hello Cueflow: Cueflow".to_string(),
            target: None,
        }
    );
}

#[test]
fn run_config_values_override_defaults_and_are_type_checked() {
    let definition = definition_with_variables();
    let config = RunConfig {
        variables: BTreeMap::from([
            ("name".to_string(), Value::String("Override".to_string())),
            ("timeout".to_string(), json!(45)),
        ]),
        ..RunConfig::default()
    };

    let resolved = resolve_run(&definition, &config, &TestSecrets).expect("overrides resolve");
    assert_eq!(
        resolved.variables()["name"].value(),
        &Value::String("Override".to_string())
    );

    let invalid_config = RunConfig {
        variables: BTreeMap::from([("timeout".to_string(), Value::String("fast".to_string()))]),
        ..RunConfig::default()
    };
    assert!(matches!(
        resolve_run(&definition, &invalid_config, &TestSecrets),
        Err(VariableResolutionError::InvalidValueType { .. })
    ));
}

#[test]
fn secret_references_are_redacted_and_unavailable_secrets_fail() {
    let definition = definition_with_variables();
    let resolved =
        resolve_run(&definition, &RunConfig::default(), &TestSecrets).expect("secret resolves");

    assert!(resolved.variables()["token"].is_secret());
    assert_eq!(
        resolved.redacted_variables()["token"],
        Value::String("[REDACTED]".to_string())
    );
    assert_eq!(
        resolved.redacted_variables()["secret-derived"],
        Value::String("[REDACTED]".to_string())
    );
    assert!(!resolved.config().variables.contains_key("token"));
    assert!(!format!("{:?}", resolved.variables()["token"]).contains("super-secret-token"));

    assert!(matches!(
        resolve_run(&definition, &RunConfig::default(), &NoSecrets),
        Err(VariableResolutionError::Secret(
            SecretResolutionError::Unavailable { .. }
        ))
    ));
}

#[test]
fn secret_declarations_remain_redacted_when_overridden() {
    let definition = definition_with_variables();
    let config = RunConfig {
        variables: BTreeMap::from([(
            "token".to_string(),
            Value::String("runtime-secret".to_string()),
        )]),
        ..RunConfig::default()
    };

    let resolved = resolve_run(&definition, &config, &TestSecrets).expect("override resolves");

    assert!(resolved.variables()["token"].is_secret());
    assert_eq!(
        resolved.redacted_variables()["token"],
        Value::String("[REDACTED]".to_string())
    );
}

#[test]
fn invalid_string_field_interpolation_returns_an_error_instead_of_panicking() {
    let mut definition = definition_with_variables();
    definition.steps.last_mut().expect("type step").action = Action::TypeText {
        text: "${timeout}".to_string(),
        target: None,
    };

    assert!(matches!(
        resolve_run(&definition, &RunConfig::default(), &TestSecrets),
        Err(VariableResolutionError::InvalidInterpolatedStep)
    ));

    definition.steps.last_mut().expect("type step").action = Action::TypeText {
        text: "safe".to_string(),
        target: None,
    };
    definition.steps[0].action = Action::LaunchUrl {
        url: "${name}".to_string(),
        target: None,
    };
    definition
        .variables
        .get_mut("name")
        .expect("name variable")
        .default = Some(VariableSource::Literal {
        value: Value::String(String::new()),
    });

    assert!(matches!(
        resolve_run(&definition, &RunConfig::default(), &TestSecrets),
        Err(VariableResolutionError::InvalidResolvedDefinition(_))
    ));
}

#[test]
fn undeclared_and_circular_variables_are_rejected() {
    let mut definition = definition_with_variables();
    definition.variables.insert(
        "first".to_string(),
        VariableDefinition {
            value_type: VariableType::String,
            description: None,
            required: false,
            default: Some(VariableSource::Literal {
                value: Value::String("${second}".to_string()),
            }),
        },
    );
    definition.variables.insert(
        "second".to_string(),
        VariableDefinition {
            value_type: VariableType::String,
            description: None,
            required: false,
            default: Some(VariableSource::Literal {
                value: Value::String("${first}".to_string()),
            }),
        },
    );

    assert!(matches!(
        resolve_run(&definition, &RunConfig::default(), &TestSecrets),
        Err(VariableResolutionError::CircularReference(_))
    ));

    let config = RunConfig {
        variables: BTreeMap::from([("unknown".to_string(), Value::String("value".to_string()))]),
        ..RunConfig::default()
    };
    assert!(matches!(
        resolve_run(&definition_with_variables(), &config, &TestSecrets),
        Err(VariableResolutionError::UndeclaredVariable(name)) if name == "unknown"
    ));
}
