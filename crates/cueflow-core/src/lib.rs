use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationDefinition {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, VariableDefinition>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VariableDefinition {
    #[serde(rename = "type")]
    pub value_type: VariableType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<VariableSource>,
}

impl VariableDefinition {
    fn validate(&self, name: &str) -> Result<(), ValidationError> {
        if self.required && self.default.is_some() {
            return Err(ValidationError::RequiredVariableHasDefault(
                name.to_string(),
            ));
        }

        if let Some(VariableSource::Literal { value }) = &self.default
            && !self.value_type.accepts(value)
        {
            return Err(ValidationError::InvalidVariableType {
                name: name.to_string(),
                expected: self.value_type,
            });
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum VariableType {
    String,
    Boolean,
    Number,
    Json,
}

impl VariableType {
    fn accepts(self, value: &Value) -> bool {
        match self {
            VariableType::String => value.is_string(),
            VariableType::Boolean => value.is_boolean(),
            VariableType::Number => value.is_number(),
            VariableType::Json => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum VariableSource {
    Literal { value: Value },
    SecretReference { reference: String },
}

impl AutomationDefinition {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_identifier("automation id", &self.id)?;

        if self.title.trim().is_empty() {
            return Err(ValidationError::MissingField("title"));
        }

        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(ValidationError::UnsupportedSchemaVersion {
                expected: CURRENT_SCHEMA_VERSION,
                actual: self.schema_version,
            });
        }

        if self.steps.is_empty() {
            return Err(ValidationError::NoSteps);
        }

        for (name, definition) in &self.variables {
            validate_identifier("variable name", name)?;
            definition.validate(name)?;
        }

        let mut step_ids = BTreeSet::new();
        for step in &self.steps {
            step.validate()?;
            if !step_ids.insert(step.id.clone()) {
                return Err(ValidationError::DuplicateStepId(step.id.clone()));
            }
        }

        Ok(())
    }

    pub fn portability(&self) -> Portability {
        let mut platforms = BTreeSet::new();

        for step in &self.steps {
            step.collect_platforms(&mut platforms);
        }

        if platforms.is_empty() {
            return Portability::Portable;
        }

        if self
            .steps
            .iter()
            .any(|step| !step.platform_overrides.is_empty())
        {
            return Portability::HasPlatformOverrides;
        }

        match platforms.len() {
            0 => Portability::Portable,
            1 => match platforms.iter().next() {
                Some(Platform::Windows) => Portability::WindowsOnly,
                Some(Platform::MacOs) => Portability::MacOsOnly,
                Some(Platform::Linux) => Portability::LinuxOnly,
                None => Portability::Portable,
            },
            _ => Portability::HasPlatformOverrides,
        }
    }
}

pub fn automation_definition_schema() -> Value {
    let mut schema = serde_json::to_value(schema_for!(AutomationDefinition))
        .expect("generated schema must be serializable");
    let definitions = &mut schema["$defs"];

    definitions["AutomationDefinition"]["properties"]["schemaVersion"]["const"] =
        Value::from(CURRENT_SCHEMA_VERSION);
    definitions["AutomationDefinition"]["properties"]["steps"]["minItems"] = Value::from(1);
    definitions["AutomationDefinition"]["properties"]["id"]["pattern"] =
        Value::from("^[A-Za-z0-9][A-Za-z0-9._-]*$");
    definitions["Step"]["properties"]["id"]["pattern"] =
        Value::from("^[A-Za-z0-9][A-Za-z0-9._-]*$");
    definitions["DurationSpec"]["properties"]["millis"]["minimum"] = Value::from(1);
    definitions["ImageTarget"]["properties"]["confidence"]["minimum"] = Value::from(1);

    schema
}

pub fn parse_definition_json(input: &str) -> Result<AutomationDefinition, DefinitionParseError> {
    let value: Value = serde_json::from_str(input)?;
    let schema_version = value
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or(DefinitionParseError::MissingSchemaVersion)?;

    if schema_version != CURRENT_SCHEMA_VERSION {
        return Err(DefinitionParseError::UnsupportedSchemaVersion {
            expected: CURRENT_SCHEMA_VERSION,
            actual: schema_version,
        });
    }

    let definition: AutomationDefinition = serde_json::from_value(value)?;
    definition.validate()?;
    Ok(definition)
}

#[derive(Debug, Error)]
pub enum DefinitionParseError {
    #[error("automation definition JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "automation definition schemaVersion is required and must be an unsigned 32-bit integer"
    )]
    MissingSchemaVersion,
    #[error("automation definition schema version {actual} is not supported; expected {expected}")]
    UnsupportedSchemaVersion { expected: u32, actual: u32 },
    #[error("automation definition is invalid: {0}")]
    Validation(#[from] ValidationError),
}

pub trait SecretResolver {
    fn resolve_secret(&self, reference: &str) -> Result<Value, SecretResolutionError>;
}

#[derive(Debug, Default)]
pub struct NoSecrets;

impl SecretResolver for NoSecrets {
    fn resolve_secret(&self, reference: &str) -> Result<Value, SecretResolutionError> {
        Err(SecretResolutionError::Unavailable {
            reference: reference.to_string(),
        })
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SecretResolutionError {
    #[error("secret reference `{reference}` is unavailable")]
    Unavailable { reference: String },
    #[error("secret reference `{reference}` could not be resolved")]
    Failed { reference: String },
}

pub struct ResolvedRun {
    definition: AutomationDefinition,
    config: RunConfig,
    variables: BTreeMap<String, ResolvedVariable>,
}

impl ResolvedRun {
    pub fn definition(&self) -> &AutomationDefinition {
        &self.definition
    }

    pub fn config(&self) -> &RunConfig {
        &self.config
    }

    pub fn variables(&self) -> &BTreeMap<String, ResolvedVariable> {
        &self.variables
    }

    pub fn redacted_variables(&self) -> BTreeMap<String, Value> {
        self.variables
            .iter()
            .map(|(name, variable)| (name.clone(), variable.redacted_value()))
            .collect()
    }
}

pub struct ResolvedVariable {
    value: Value,
    secret: bool,
}

impl ResolvedVariable {
    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn is_secret(&self) -> bool {
        self.secret
    }

    pub fn redacted_value(&self) -> Value {
        if self.secret {
            Value::String("[REDACTED]".to_string())
        } else {
            self.value.clone()
        }
    }
}

impl fmt::Debug for ResolvedVariable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedVariable")
            .field("value", &self.redacted_value())
            .field("secret", &self.secret)
            .finish()
    }
}

pub fn resolve_run<R: SecretResolver>(
    definition: &AutomationDefinition,
    config: &RunConfig,
    secret_resolver: &R,
) -> Result<ResolvedRun, VariableResolutionError> {
    definition
        .validate()
        .map_err(VariableResolutionError::InvalidDefinition)?;

    let mut resolved = BTreeMap::new();
    let mut resolving = BTreeSet::new();
    for name in definition.variables.keys() {
        resolve_variable(
            name,
            &definition.variables,
            &config.variables,
            secret_resolver,
            &mut resolved,
            &mut resolving,
        )?;
    }

    for name in config.variables.keys() {
        if !definition.variables.contains_key(name) {
            return Err(VariableResolutionError::UndeclaredVariable(name.clone()));
        }
    }

    let mut resolved_definition = definition.clone();
    for step in &mut resolved_definition.steps {
        let step_value = serde_json::to_value(&*step)
            .expect("validated automation steps must always be serializable");
        let interpolated = interpolate_value(step_value, &resolved)?;
        *step = serde_json::from_value(interpolated)
            .map_err(|_| VariableResolutionError::InvalidInterpolatedStep)?;
    }
    resolved_definition
        .validate()
        .map_err(VariableResolutionError::InvalidResolvedDefinition)?;

    let mut resolved_config = config.clone();
    resolved_config.variables = resolved
        .iter()
        .filter(|(_, value)| !value.secret)
        .map(|(name, value)| (name.clone(), value.value.clone()))
        .collect();
    resolved_config.working_directory =
        interpolate_optional_string(&resolved_config.working_directory, &resolved)?;
    resolved_config.environment = resolved_config
        .environment
        .iter()
        .map(|(name, value)| {
            interpolate_string(value, &resolved).map(|interpolated| (name.clone(), interpolated))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;

    Ok(ResolvedRun {
        definition: resolved_definition,
        config: resolved_config,
        variables: resolved,
    })
}

fn resolve_variable<R: SecretResolver>(
    name: &str,
    definitions: &BTreeMap<String, VariableDefinition>,
    overrides: &BTreeMap<String, Value>,
    secret_resolver: &R,
    resolved: &mut BTreeMap<String, ResolvedVariable>,
    resolving: &mut BTreeSet<String>,
) -> Result<(), VariableResolutionError> {
    if resolved.contains_key(name) {
        return Ok(());
    }

    if !resolving.insert(name.to_string()) {
        return Err(VariableResolutionError::CircularReference(name.to_string()));
    }

    let definition = definitions
        .get(name)
        .expect("variable name must have a corresponding definition");
    let (raw_value, direct_secret) = match overrides.get(name) {
        Some(value) => (
            value.clone(),
            matches!(
                definition.default,
                Some(VariableSource::SecretReference { .. })
            ),
        ),
        None => match &definition.default {
            Some(VariableSource::Literal { value }) => (value.clone(), false),
            Some(VariableSource::SecretReference { reference }) => {
                let value = secret_resolver
                    .resolve_secret(reference)
                    .map_err(VariableResolutionError::Secret)?;
                (value, true)
            }
            None if definition.required => {
                return Err(VariableResolutionError::MissingRequiredVariable(
                    name.to_string(),
                ));
            }
            None => {
                return Err(VariableResolutionError::MissingVariableValue(
                    name.to_string(),
                ));
            }
        },
    };
    let references = collect_interpolation_references(&raw_value)?;

    let mut resolve = |referenced_name: &str| {
        resolve_variable(
            referenced_name,
            definitions,
            overrides,
            secret_resolver,
            resolved,
            resolving,
        )?;
        Ok(resolved
            .get(referenced_name)
            .expect("resolved reference must be present")
            .value
            .clone())
    };
    let interpolated = interpolate_value_with_resolver(raw_value, &mut resolve)?;

    if !definition.value_type.accepts(&interpolated) {
        return Err(VariableResolutionError::InvalidValueType {
            name: name.to_string(),
            expected: definition.value_type,
        });
    }

    let secret = direct_secret
        || references
            .iter()
            .any(|name| resolved.get(name).is_some_and(ResolvedVariable::is_secret));
    resolving.remove(name);
    resolved.insert(
        name.to_string(),
        ResolvedVariable {
            secret,
            value: interpolated,
        },
    );
    Ok(())
}

fn collect_interpolation_references(
    value: &Value,
) -> Result<BTreeSet<String>, VariableResolutionError> {
    let mut references = BTreeSet::new();
    match value {
        Value::String(value) => collect_references_from_string(value, &mut references)?,
        Value::Array(values) => {
            for value in values {
                references.extend(collect_interpolation_references(value)?);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                references.extend(collect_interpolation_references(value)?);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }

    Ok(references)
}

fn collect_references_from_string(
    value: &str,
    references: &mut BTreeSet<String>,
) -> Result<(), VariableResolutionError> {
    let mut remaining = value;
    while let Some(start) = remaining.find("${") {
        let after_start = &remaining[start + 2..];
        let end = after_start
            .find('}')
            .ok_or(VariableResolutionError::MalformedInterpolation)?;
        let name = &after_start[..end];
        if name.is_empty() {
            return Err(VariableResolutionError::MalformedInterpolation);
        }

        references.insert(name.to_string());
        remaining = &after_start[end + 1..];
    }

    Ok(())
}

fn interpolate_optional_string(
    value: &Option<String>,
    variables: &BTreeMap<String, ResolvedVariable>,
) -> Result<Option<String>, VariableResolutionError> {
    value
        .as_deref()
        .map(|value| interpolate_string(value, variables))
        .transpose()
}

fn interpolate_value(
    value: Value,
    variables: &BTreeMap<String, ResolvedVariable>,
) -> Result<Value, VariableResolutionError> {
    let mut resolve = |name: &str| {
        variables
            .get(name)
            .map(|variable| variable.value.clone())
            .ok_or_else(|| VariableResolutionError::UnknownVariable(name.to_string()))
    };
    interpolate_value_with_resolver(value, &mut resolve)
}

fn interpolate_value_with_resolver(
    value: Value,
    resolve: &mut dyn FnMut(&str) -> Result<Value, VariableResolutionError>,
) -> Result<Value, VariableResolutionError> {
    match value {
        Value::String(value) => interpolate_string_with_resolver(&value, resolve),
        Value::Array(values) => values
            .into_iter()
            .map(|value| interpolate_value_with_resolver(value, resolve))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(values) => values
            .into_iter()
            .map(|(key, value)| {
                interpolate_value_with_resolver(value, resolve).map(|value| (key, value))
            })
            .collect::<Result<serde_json::Map<_, _>, _>>()
            .map(Value::Object),
        value => Ok(value),
    }
}

fn interpolate_string(
    value: &str,
    variables: &BTreeMap<String, ResolvedVariable>,
) -> Result<String, VariableResolutionError> {
    let mut resolve = |name: &str| {
        variables
            .get(name)
            .map(|variable| variable.value.clone())
            .ok_or_else(|| VariableResolutionError::UnknownVariable(name.to_string()))
    };

    match interpolate_string_with_resolver(value, &mut resolve)? {
        Value::String(value) => Ok(value),
        value => scalar_to_string(value),
    }
}

fn interpolate_string_with_resolver(
    value: &str,
    resolve: &mut dyn FnMut(&str) -> Result<Value, VariableResolutionError>,
) -> Result<Value, VariableResolutionError> {
    let mut output = String::new();
    let mut remaining = value;
    let mut full_reference = None;

    while let Some(start) = remaining.find("${") {
        output.push_str(&remaining[..start]);
        let reference_start = start + 2;
        let after_start = &remaining[reference_start..];
        let end = after_start
            .find('}')
            .ok_or(VariableResolutionError::MalformedInterpolation)?;
        let name = &after_start[..end];
        if name.is_empty() {
            return Err(VariableResolutionError::MalformedInterpolation);
        }

        let resolved = resolve(name)?;
        let suffix = &after_start[end + 1..];
        if output.is_empty() && suffix.is_empty() && full_reference.is_none() {
            full_reference = Some(resolved);
        } else {
            output.push_str(&scalar_to_string(resolved)?);
        }
        remaining = suffix;
    }

    if let Some(value) = full_reference {
        return Ok(value);
    }

    output.push_str(remaining);
    Ok(Value::String(output))
}

fn scalar_to_string(value: Value) -> Result<String, VariableResolutionError> {
    match value {
        Value::String(value) => Ok(value),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(value.to_string()),
        _ => Err(VariableResolutionError::NonScalarInterpolation),
    }
}

#[derive(Debug, Error)]
pub enum VariableResolutionError {
    #[error("automation definition is invalid: {0}")]
    InvalidDefinition(ValidationError),
    #[error("run config provided undeclared variable `{0}`")]
    UndeclaredVariable(String),
    #[error("required variable `{0}` has no value")]
    MissingRequiredVariable(String),
    #[error("variable `{0}` has no value")]
    MissingVariableValue(String),
    #[error("unknown variable `{0}`")]
    UnknownVariable(String),
    #[error("variable reference `{0}` is circular")]
    CircularReference(String),
    #[error("variable interpolation is malformed")]
    MalformedInterpolation,
    #[error("only string, boolean, and number values may be embedded in text")]
    NonScalarInterpolation,
    #[error("variable `{name}` does not match declared type {expected:?}")]
    InvalidValueType {
        name: String,
        expected: VariableType,
    },
    #[error("interpolation produced a value incompatible with a string-only automation step field")]
    InvalidInterpolatedStep,
    #[error("interpolation produced an invalid automation definition: {0}")]
    InvalidResolvedDefinition(ValidationError),
    #[error(transparent)]
    Secret(#[from] SecretResolutionError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(flatten)]
    pub action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<DurationSpec>,
    #[serde(default)]
    pub retry: RetryPolicy,
    #[serde(default)]
    pub on_error: OnErrorPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<WaitCondition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platform_overrides: Vec<PlatformActionOverride>,
}

impl Step {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_identifier("step id", &self.id)?;
        self.action.validate()?;

        if let Some(timeout) = self.timeout {
            timeout.validate("step timeout")?;
        }

        self.retry.validate()?;

        for condition in &self.conditions {
            condition.validate()?;
        }

        let mut overridden_platforms = BTreeSet::new();
        for override_action in &self.platform_overrides {
            if !overridden_platforms.insert(override_action.platform) {
                return Err(ValidationError::DuplicatePlatformOverride(
                    override_action.platform,
                ));
            }
            override_action.action.validate()?;
        }

        Ok(())
    }

    fn collect_platforms(&self, platforms: &mut BTreeSet<Platform>) {
        self.action.collect_platforms(platforms);

        for condition in &self.conditions {
            condition.collect_platforms(platforms);
        }

        for override_action in &self.platform_overrides {
            platforms.insert(override_action.platform);
            override_action.action.collect_platforms(platforms);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Action {
    LaunchUrl {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
    },
    LaunchApp {
        app: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
    },
    FocusWindow {
        target: Target,
    },
    TypeText {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
    },
    PressKey {
        keys: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
    },
    ClickTarget {
        target: Target,
    },
    Scroll {
        delta_x: i32,
        delta_y: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
    },
    WaitFor {
        condition: WaitCondition,
    },
    Assert {
        assertion: Assertion,
    },
    RunCommand {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
    OpenFile {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
    },
}

impl Action {
    pub fn kind(&self) -> &'static str {
        match self {
            Action::LaunchUrl { .. } => "launchUrl",
            Action::LaunchApp { .. } => "launchApp",
            Action::FocusWindow { .. } => "focusWindow",
            Action::TypeText { .. } => "typeText",
            Action::PressKey { .. } => "pressKey",
            Action::ClickTarget { .. } => "clickTarget",
            Action::Scroll { .. } => "scroll",
            Action::WaitFor { .. } => "waitFor",
            Action::Assert { .. } => "assert",
            Action::RunCommand { .. } => "runCommand",
            Action::OpenFile { .. } => "openFile",
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Action::LaunchUrl { url, target } => {
                validate_non_empty("url", url)?;
                validate_optional_target(target)?;
            }
            Action::LaunchApp { app, target } => {
                validate_non_empty("app", app)?;
                validate_optional_target(target)?;
            }
            Action::FocusWindow { target } | Action::ClickTarget { target } => target.validate()?,
            Action::TypeText { target, .. }
            | Action::PressKey { target, .. }
            | Action::Scroll { target, .. }
            | Action::OpenFile { target, .. } => validate_optional_target(target)?,
            Action::WaitFor { condition } => condition.validate()?,
            Action::Assert { assertion } => assertion.validate()?,
            Action::RunCommand { command, .. } => validate_non_empty("command", command)?,
        }

        if let Action::TypeText { text, .. } = self {
            validate_non_empty("text", text)?;
        }

        if let Action::PressKey { keys, .. } = self {
            validate_non_empty("keys", keys)?;
        }

        if let Action::OpenFile { path, .. } = self {
            validate_non_empty("path", path)?;
        }

        Ok(())
    }

    pub fn for_platform(&self, platform: Option<Platform>) -> Self {
        match self {
            Self::LaunchUrl { url, target } => Self::LaunchUrl {
                url: url.clone(),
                target: target.as_ref().map(|target| target.for_platform(platform)),
            },
            Self::LaunchApp { app, target } => Self::LaunchApp {
                app: app.clone(),
                target: target.as_ref().map(|target| target.for_platform(platform)),
            },
            Self::FocusWindow { target } => Self::FocusWindow {
                target: target.for_platform(platform),
            },
            Self::TypeText { text, target } => Self::TypeText {
                text: text.clone(),
                target: target.as_ref().map(|target| target.for_platform(platform)),
            },
            Self::PressKey { keys, target } => Self::PressKey {
                keys: keys.clone(),
                target: target.as_ref().map(|target| target.for_platform(platform)),
            },
            Self::ClickTarget { target } => Self::ClickTarget {
                target: target.for_platform(platform),
            },
            Self::Scroll {
                delta_x,
                delta_y,
                target,
            } => Self::Scroll {
                delta_x: *delta_x,
                delta_y: *delta_y,
                target: target.as_ref().map(|target| target.for_platform(platform)),
            },
            Self::WaitFor { condition } => Self::WaitFor {
                condition: condition.for_platform(platform),
            },
            Self::Assert { assertion } => Self::Assert {
                assertion: assertion.for_platform(platform),
            },
            Self::RunCommand { command, args } => Self::RunCommand {
                command: command.clone(),
                args: args.clone(),
            },
            Self::OpenFile { path, target } => Self::OpenFile {
                path: path.clone(),
                target: target.as_ref().map(|target| target.for_platform(platform)),
            },
        }
    }

    fn collect_platforms(&self, platforms: &mut BTreeSet<Platform>) {
        match self {
            Action::LaunchUrl { target, .. }
            | Action::LaunchApp { target, .. }
            | Action::TypeText { target, .. }
            | Action::PressKey { target, .. }
            | Action::Scroll { target, .. }
            | Action::OpenFile { target, .. } => {
                if let Some(target) = target {
                    target.collect_platforms(platforms);
                }
            }
            Action::FocusWindow { target } | Action::ClickTarget { target } => {
                target.collect_platforms(platforms);
            }
            Action::WaitFor { condition } => condition.collect_platforms(platforms),
            Action::Assert { assertion } => assertion.collect_platforms(platforms),
            Action::RunCommand { .. } => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessibility: Option<AccessibilityTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinates: Option<Coordinates>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub platform_selectors: BTreeMap<Platform, PlatformSelector>,
}

impl Target {
    pub fn app(app_name: impl Into<String>) -> Self {
        Self {
            app_name: Some(app_name.into()),
            process_name: None,
            window_title: None,
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: None,
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_optional_non_empty("target appName", &self.app_name)?;
        validate_optional_non_empty("target processName", &self.process_name)?;
        validate_optional_non_empty("target windowTitle", &self.window_title)?;
        validate_optional_non_empty("target titleContains", &self.title_contains)?;
        validate_optional_non_empty("target url", &self.url)?;
        validate_optional_non_empty("target filePath", &self.file_path)?;

        if let Some(accessibility) = &self.accessibility {
            accessibility.validate()?;
        }

        if let Some(image) = &self.image {
            image.validate()?;
        }

        for selector in self.platform_selectors.values() {
            selector.validate()?;
        }

        let has_logical_target = self
            .app_name
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
            || self
                .process_name
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty())
            || self
                .window_title
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty())
            || self
                .title_contains
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty())
            || self
                .url
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty())
            || self
                .file_path
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty())
            || self.accessibility.is_some()
            || self.image.is_some()
            || self.coordinates.is_some()
            || !self.platform_selectors.is_empty();

        if !has_logical_target {
            return Err(ValidationError::EmptyTarget);
        }

        Ok(())
    }

    pub fn for_platform(&self, platform: Option<Platform>) -> Self {
        let Some(platform) = platform else {
            return self.clone();
        };

        let Some(selector) = self.platform_selectors.get(&platform) else {
            let mut target = self.clone();
            target.platform_selectors.clear();
            return target;
        };

        let mut target = Self {
            app_name: None,
            process_name: selector.process_name.clone(),
            window_title: selector.window_title.clone(),
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: selector
                .accessibility_query
                .as_ref()
                .map(|name| AccessibilityTarget {
                    id: None,
                    name: Some(name.clone()),
                    control_type: None,
                }),
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        };
        if selector.command_hint.is_some() {
            target.platform_selectors.insert(platform, selector.clone());
        }
        target
    }

    fn collect_platforms(&self, platforms: &mut BTreeSet<Platform>) {
        platforms.extend(self.platform_selectors.keys().copied());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccessibilityTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_type: Option<String>,
}

impl AccessibilityTarget {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_optional_non_empty("accessibility id", &self.id)?;
        validate_optional_non_empty("accessibility name", &self.name)?;
        validate_optional_non_empty("accessibility controlType", &self.control_type)?;

        if self.id.as_ref().is_none_or(|value| value.trim().is_empty())
            && self
                .name
                .as_ref()
                .is_none_or(|value| value.trim().is_empty())
            && self
                .control_type
                .as_ref()
                .is_none_or(|value| value.trim().is_empty())
        {
            return Err(ValidationError::EmptyAccessibilityTarget);
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageTarget {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<u8>,
}

impl ImageTarget {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_non_empty("image path", &self.path)?;
        if self.confidence.is_some_and(|confidence| confidence == 0) {
            return Err(ValidationError::InvalidImageConfidence);
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Coordinates {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlatformSelector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessibility_query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_hint: Option<String>,
}

impl PlatformSelector {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_optional_non_empty("platform selector processName", &self.process_name)?;
        validate_optional_non_empty("platform selector windowTitle", &self.window_title)?;
        validate_optional_non_empty(
            "platform selector accessibilityQuery",
            &self.accessibility_query,
        )?;
        validate_optional_non_empty("platform selector commandHint", &self.command_hint)?;

        if self
            .process_name
            .as_ref()
            .is_none_or(|value| value.trim().is_empty())
            && self
                .window_title
                .as_ref()
                .is_none_or(|value| value.trim().is_empty())
            && self
                .accessibility_query
                .as_ref()
                .is_none_or(|value| value.trim().is_empty())
            && self
                .command_hint
                .as_ref()
                .is_none_or(|value| value.trim().is_empty())
        {
            return Err(ValidationError::EmptyPlatformSelector);
        }

        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    Windows,
    MacOs,
    Linux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Portability {
    Portable,
    HasPlatformOverrides,
    WindowsOnly,
    MacOsOnly,
    LinuxOnly,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlatformActionOverride {
    pub platform: Platform,
    pub action: Box<Action>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurationSpec {
    pub millis: u64,
}

impl DurationSpec {
    pub fn from_millis(millis: u64) -> Self {
        Self { millis }
    }

    fn validate(self, field: &'static str) -> Result<(), ValidationError> {
        if self.millis == 0 {
            return Err(ValidationError::InvalidDuration(field));
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<DurationSpec>,
    #[serde(default)]
    pub backoff: BackoffPolicy,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            delay: None,
            backoff: BackoffPolicy::None,
        }
    }
}

impl RetryPolicy {
    fn validate(self) -> Result<(), ValidationError> {
        if self.max_attempts == 0 {
            return Err(ValidationError::InvalidRetryPolicy);
        }

        if let Some(delay) = self.delay {
            delay.validate("retry delay")?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BackoffPolicy {
    #[default]
    None,
    Linear,
    Exponential,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum OnErrorPolicy {
    #[default]
    Stop,
    Continue,
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum WaitCondition {
    Duration {
        duration: DurationSpec,
    },
    WindowExists {
        target: Target,
    },
    WindowFocused {
        target: Target,
    },
    ProcessRunning {
        target: Target,
    },
    FileExists {
        path: String,
    },
    CommandExits {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
}

impl WaitCondition {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            WaitCondition::Duration { duration } => duration.validate("wait duration"),
            WaitCondition::WindowExists { target }
            | WaitCondition::WindowFocused { target }
            | WaitCondition::ProcessRunning { target } => target.validate(),
            WaitCondition::FileExists { path } => validate_non_empty("path", path),
            WaitCondition::CommandExits { command, .. } => validate_non_empty("command", command),
        }
    }

    pub fn for_platform(&self, platform: Option<Platform>) -> Self {
        match self {
            Self::Duration { duration } => Self::Duration {
                duration: *duration,
            },
            Self::WindowExists { target } => Self::WindowExists {
                target: target.for_platform(platform),
            },
            Self::WindowFocused { target } => Self::WindowFocused {
                target: target.for_platform(platform),
            },
            Self::ProcessRunning { target } => Self::ProcessRunning {
                target: target.for_platform(platform),
            },
            Self::FileExists { path } => Self::FileExists { path: path.clone() },
            Self::CommandExits { command, args } => Self::CommandExits {
                command: command.clone(),
                args: args.clone(),
            },
        }
    }

    fn collect_platforms(&self, platforms: &mut BTreeSet<Platform>) {
        match self {
            WaitCondition::WindowExists { target }
            | WaitCondition::WindowFocused { target }
            | WaitCondition::ProcessRunning { target } => target.collect_platforms(platforms),
            WaitCondition::Duration { .. }
            | WaitCondition::FileExists { .. }
            | WaitCondition::CommandExits { .. } => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Assertion {
    Condition { condition: WaitCondition },
    TargetExists { target: Target },
}

impl Assertion {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Assertion::Condition { condition } => condition.validate(),
            Assertion::TargetExists { target } => target.validate(),
        }
    }

    pub fn for_platform(&self, platform: Option<Platform>) -> Self {
        match self {
            Self::Condition { condition } => Self::Condition {
                condition: condition.for_platform(platform),
            },
            Self::TargetExists { target } => Self::TargetExists {
                target: target.for_platform(platform),
            },
        }
    }

    fn collect_platforms(&self, platforms: &mut BTreeSet<Platform>) {
        match self {
            Assertion::Condition { condition } => condition.collect_platforms(platforms),
            Assertion::TargetExists { target } => target.collect_platforms(platforms),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunConfig {
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub approved_commands: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            dry_run: true,
            run_id: None,
            variables: BTreeMap::new(),
            working_directory: None,
            environment: BTreeMap::new(),
            approved_commands: BTreeSet::new(),
            platform: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RunEvent {
    Started {
        run_id: String,
        automation_id: String,
    },
    StepStarted {
        run_id: String,
        automation_id: String,
        step_id: String,
        step_kind: String,
    },
    StepSucceeded {
        run_id: String,
        automation_id: String,
        step_id: String,
        artifacts: Vec<Artifact>,
    },
    StepFailed {
        run_id: String,
        automation_id: String,
        step_id: String,
        error: RunError,
    },
    Paused {
        run_id: String,
        automation_id: String,
        step_id: Option<String>,
    },
    Resumed {
        run_id: String,
        automation_id: String,
        step_id: Option<String>,
    },
    ManualIntervention {
        run_id: String,
        automation_id: String,
        step_id: String,
        error: RunError,
    },
    Log {
        run_id: String,
        automation_id: String,
        step_id: Option<String>,
        level: LogLevel,
        message: String,
    },
    Artifact {
        run_id: String,
        automation_id: String,
        step_id: Option<String>,
        artifact: Artifact,
    },
    Completed {
        run_id: String,
        automation_id: String,
        status: RunStatus,
    },
    Cancelled {
        run_id: String,
        automation_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunStatus {
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunError {
    pub kind: RunErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightDiagnostic {
    pub severity: PreflightSeverity,
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PreflightSeverity {
    Warning,
    Error,
}

impl RunError {
    pub fn new(kind: RunErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            step_id: None,
            source: None,
        }
    }

    pub fn with_step_id(mut self, step_id: impl Into<String>) -> Self {
        self.step_id = Some(step_id.into());
        self
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunErrorKind {
    Validation,
    Adapter,
    Timeout,
    Assertion,
    Cancelled,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ArtifactKind {
    Screenshot,
    Log,
    Video,
    File,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("{0} is required")]
    MissingField(&'static str),
    #[error("{0} must not be blank when present")]
    BlankField(&'static str),
    #[error("{field} must be a non-empty identifier")]
    InvalidIdentifier { field: &'static str },
    #[error("schema version {actual} is not supported; expected {expected}")]
    UnsupportedSchemaVersion { expected: u32, actual: u32 },
    #[error("automation must contain at least one step")]
    NoSteps,
    #[error("duplicate step id `{0}`")]
    DuplicateStepId(String),
    #[error("duplicate platform override for {0:?}")]
    DuplicatePlatformOverride(Platform),
    #[error("target must include at least one selector")]
    EmptyTarget,
    #[error("accessibility target must include an id, name, or control type")]
    EmptyAccessibilityTarget,
    #[error("platform selector must include at least one selector")]
    EmptyPlatformSelector,
    #[error("image target confidence must be between 1 and 255")]
    InvalidImageConfidence,
    #[error("{0} must be greater than zero")]
    InvalidDuration(&'static str),
    #[error("retry max_attempts must be greater than zero")]
    InvalidRetryPolicy,
    #[error("required variable `{0}` cannot also define a default")]
    RequiredVariableHasDefault(String),
    #[error("variable `{name}` must match declared type {expected:?}")]
    InvalidVariableType {
        name: String,
        expected: VariableType,
    },
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), ValidationError> {
    let mut bytes = value.bytes();
    let starts_with_alphanumeric = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric());
    let has_only_identifier_characters =
        bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));

    if !starts_with_alphanumeric || !has_only_identifier_characters {
        return Err(ValidationError::InvalidIdentifier { field });
    }

    Ok(())
}

fn validate_non_empty(field: &'static str, value: &str) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        return Err(ValidationError::MissingField(field));
    }

    Ok(())
}

fn validate_optional_non_empty(
    field: &'static str,
    value: &Option<String>,
) -> Result<(), ValidationError> {
    if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
        return Err(ValidationError::BlankField(field));
    }

    Ok(())
}

fn validate_optional_target(target: &Option<Target>) -> Result<(), ValidationError> {
    if let Some(target) = target {
        target.validate()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_definition() -> AutomationDefinition {
        AutomationDefinition {
            id: "demo-ready".to_string(),
            title: "Prepare demo".to_string(),
            description: Some("Put the browser in a known state.".to_string()),
            schema_version: CURRENT_SCHEMA_VERSION,
            version: Some("0.1.0".to_string()),
            variables: BTreeMap::new(),
            metadata: BTreeMap::new(),
            steps: vec![
                Step {
                    id: "open-docs".to_string(),
                    label: Some("Open Cueflow docs".to_string()),
                    action: Action::LaunchUrl {
                        url: "https://cueflow.dev".to_string(),
                        target: None,
                    },
                    timeout: Some(DurationSpec::from_millis(5_000)),
                    retry: RetryPolicy::default(),
                    on_error: OnErrorPolicy::Stop,
                    conditions: Vec::new(),
                    platform_overrides: Vec::new(),
                },
                Step {
                    id: "focus-browser".to_string(),
                    label: None,
                    action: Action::FocusWindow {
                        target: Target::app("Browser"),
                    },
                    timeout: None,
                    retry: RetryPolicy::default(),
                    on_error: OnErrorPolicy::Stop,
                    conditions: vec![WaitCondition::Duration {
                        duration: DurationSpec::from_millis(100),
                    }],
                    platform_overrides: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn automation_definition_round_trips_as_json() {
        let definition = sample_definition();

        let json = serde_json::to_string_pretty(&definition).expect("serialize definition");
        assert!(json.contains("\"schemaVersion\""));
        assert!(json.contains("\"launchUrl\""));

        let round_trip: AutomationDefinition =
            serde_json::from_str(&json).expect("deserialize definition");
        assert_eq!(round_trip, definition);
        assert_eq!(round_trip.portability(), Portability::Portable);
    }

    #[test]
    fn validation_rejects_duplicate_step_ids() {
        let mut definition = sample_definition();
        definition.steps[1].id = definition.steps[0].id.clone();

        let error = definition.validate().expect_err("duplicate should fail");
        assert_eq!(
            error,
            ValidationError::DuplicateStepId("open-docs".to_string())
        );
    }

    #[test]
    fn validation_rejects_empty_targets() {
        let target = Target {
            app_name: None,
            process_name: None,
            window_title: None,
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: None,
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        };

        assert_eq!(target.validate(), Err(ValidationError::EmptyTarget));
    }

    #[test]
    fn portability_detects_platform_overrides() {
        let mut definition = sample_definition();
        definition.steps[0]
            .platform_overrides
            .push(PlatformActionOverride {
                platform: Platform::Windows,
                action: Box::new(Action::LaunchApp {
                    app: "msedge".to_string(),
                    target: None,
                }),
            });

        assert_eq!(definition.portability(), Portability::HasPlatformOverrides);

        definition.steps[1]
            .platform_overrides
            .push(PlatformActionOverride {
                platform: Platform::MacOs,
                action: Box::new(Action::LaunchApp {
                    app: "Safari".to_string(),
                    target: None,
                }),
            });

        assert_eq!(definition.portability(), Portability::HasPlatformOverrides);
    }
}
