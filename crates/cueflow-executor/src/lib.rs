use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cueflow_core::{
    Action, Artifact, Assertion, AutomationDefinition, BackoffPolicy, FailureKind, OnErrorPolicy,
    Platform, PreflightDiagnostic, PreflightSeverity, RunConfig, RunError, RunErrorKind, RunEvent,
    RunStatus, Step, Target, WaitCondition,
};
use thiserror::Error;
use tracing::{error, info, instrument, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    pub run_id: String,
    pub outcome: RunOutcome,
    pub events: Vec<RunEvent>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ExecutorError {
    #[error("automation validation failed: {0}")]
    Validation(String),
    #[error("automation preflight failed: {0}")]
    Preflight(String),
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("{public_message}")]
pub struct AdapterError {
    public_message: String,
    kind: RunErrorKind,
    failure_kind: Option<FailureKind>,
    diagnostics: Option<String>,
}

impl AdapterError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            public_message: message.into(),
            kind: RunErrorKind::Adapter,
            failure_kind: None,
            diagnostics: None,
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            public_message: message.into(),
            kind: RunErrorKind::Unsupported,
            failure_kind: Some(FailureKind::UnsupportedAction),
            diagnostics: None,
        }
    }

    pub fn timeout() -> Self {
        Self {
            public_message: "step timed out".to_string(),
            kind: RunErrorKind::Timeout,
            failure_kind: Some(FailureKind::Timeout),
            diagnostics: None,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            public_message: "run cancelled".to_string(),
            kind: RunErrorKind::Cancelled,
            failure_kind: Some(FailureKind::Cancelled),
            diagnostics: None,
        }
    }

    fn assertion(message: impl Into<String>) -> Self {
        Self {
            public_message: message.into(),
            kind: RunErrorKind::Assertion,
            failure_kind: None,
            diagnostics: None,
        }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.diagnostics = Some(source.into());
        self
    }

    pub fn with_failure_kind(mut self, failure_kind: FailureKind) -> Self {
        self.failure_kind = Some(failure_kind);
        self
    }

    pub fn failure_kind(&self) -> Option<FailureKind> {
        self.failure_kind
    }

    pub fn diagnostics(&self) -> Option<&str> {
        self.diagnostics.as_deref()
    }

    fn into_run_error(self, step_id: String) -> RunError {
        let mut error = RunError::new(self.kind, self.public_message).with_step_id(step_id);
        if let Some(failure_kind) = self.failure_kind {
            error = error.with_failure_kind(failure_kind);
        }
        match self.diagnostics {
            Some(diagnostics) => error.with_source(diagnostics),
            None => error,
        }
    }
}

pub trait ExecutionAdapter {
    fn execute(
        &mut self,
        action: &Action,
        config: &RunConfig,
    ) -> Result<Vec<Artifact>, AdapterError>;

    fn evaluate_wait(
        &mut self,
        condition: &WaitCondition,
        config: &RunConfig,
    ) -> Result<ConditionState, AdapterError> {
        self.execute(
            &Action::WaitFor {
                condition: condition.clone(),
            },
            config,
        )
        .map(|_| ConditionState::Satisfied)
    }

    fn evaluate_wait_with_control(
        &mut self,
        condition: &WaitCondition,
        config: &RunConfig,
        _control: &RunControl,
        _timeout: Option<Duration>,
    ) -> Result<ConditionState, AdapterError> {
        self.evaluate_wait(condition, config)
    }

    fn execute_with_control(
        &mut self,
        action: &Action,
        config: &RunConfig,
        _control: &RunControl,
        _timeout: Option<Duration>,
    ) -> Result<Vec<Artifact>, AdapterError> {
        self.execute(action, config)
    }

    fn evaluate_assertion(
        &mut self,
        assertion: &Assertion,
        config: &RunConfig,
    ) -> Result<bool, AdapterError> {
        self.execute(
            &Action::Assert {
                assertion: assertion.clone(),
            },
            config,
        )
        .map(|_| true)
    }

    fn target_exists(
        &mut self,
        _target: &Target,
        _config: &RunConfig,
    ) -> Result<bool, AdapterError> {
        Err(AdapterError::unsupported(
            "semantic target queries are not supported by this adapter",
        ))
    }

    fn invoke_target(&mut self, _target: &Target, _config: &RunConfig) -> Result<(), AdapterError> {
        Err(AdapterError::unsupported(
            "semantic target invocation is not supported by this adapter",
        ))
    }

    fn set_target_text(
        &mut self,
        _target: &Target,
        _text: &str,
        _config: &RunConfig,
    ) -> Result<(), AdapterError> {
        Err(AdapterError::unsupported(
            "semantic target text input is not supported by this adapter",
        ))
    }

    fn target_is_focused(
        &mut self,
        _target: &Target,
        _config: &RunConfig,
    ) -> Result<bool, AdapterError> {
        Err(AdapterError::unsupported(
            "semantic target focus queries are not supported by this adapter",
        ))
    }

    fn scroll_target(
        &mut self,
        _target: &Target,
        _delta_x: i32,
        _delta_y: i32,
        _config: &RunConfig,
    ) -> Result<(), AdapterError> {
        Err(AdapterError::unsupported(
            "semantic target scrolling is not supported by this adapter",
        ))
    }

    fn preflight(&self, _action: &Action, _config: &RunConfig) -> Vec<PreflightDiagnostic> {
        Vec::new()
    }

    fn capture_step_evidence(
        &mut self,
        _phase: EvidencePhase,
        _action: &Action,
        _config: &RunConfig,
        _run_id: &str,
        _automation_id: &str,
        _step_id: &str,
    ) -> Result<Vec<Artifact>, AdapterError> {
        Ok(Vec::new())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidencePhase {
    Before,
    After,
    Failure,
}

impl EvidencePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
            Self::Failure => "failure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionState {
    Pending,
    Satisfied,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreflightReport {
    pub diagnostics: Vec<PreflightDiagnostic>,
}

impl PreflightReport {
    pub fn can_run(&self) -> bool {
        !self
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == PreflightSeverity::Error)
    }
}

pub trait RunEventSink {
    fn emit(&mut self, event: &RunEvent);
}

#[derive(Debug, Default)]
pub struct NoopEventSink;

impl RunEventSink for NoopEventSink {
    fn emit(&mut self, _event: &RunEvent) {}
}

pub trait ExecutionClock {
    fn now(&self) -> Duration;
    fn sleep(&self, duration: Duration);
}

#[derive(Debug)]
pub struct SystemClock {
    started_at: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }
}

impl ExecutionClock for SystemClock {
    fn now(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunControl {
    inner: Arc<RunControlInner>,
}

#[derive(Debug, Default)]
struct RunControlInner {
    cancelled: AtomicBool,
    paused: AtomicBool,
    gate: Mutex<()>,
    resumed: Condvar,
}

impl RunControl {
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.resumed.notify_all();
    }

    pub fn pause(&self) {
        self.inner.paused.store(true, Ordering::Release);
    }

    pub fn resume(&self) {
        self.inner.paused.store(false, Ordering::Release);
        self.inner.resumed.notify_all();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::Acquire)
    }

    fn wait_for_resume(&self) -> bool {
        let mut guard = self.inner.gate.lock().expect("run control mutex poisoned");
        while self.is_paused() && !self.is_cancelled() {
            guard = self
                .inner
                .resumed
                .wait(guard)
                .expect("run control mutex poisoned");
        }
        !self.is_cancelled()
    }
}

#[derive(Debug, Default)]
pub struct AutomationExecutor;

impl AutomationExecutor {
    pub fn new() -> Self {
        Self
    }

    #[instrument(
        skip_all,
        fields(
            automation_id = %definition.id,
            run_id = tracing::field::Empty
        )
    )]
    pub fn run<A: ExecutionAdapter>(
        &self,
        definition: &AutomationDefinition,
        config: RunConfig,
        adapter: &mut A,
    ) -> Result<RunReport, ExecutorError> {
        let control = RunControl::default();
        let mut sink = NoopEventSink;
        let clock = SystemClock::default();
        self.run_with(definition, config, adapter, &control, &mut sink, &clock)
    }

    pub fn preflight<A: ExecutionAdapter>(
        &self,
        definition: &AutomationDefinition,
        config: &RunConfig,
        adapter: &A,
    ) -> Result<PreflightReport, ExecutorError> {
        definition
            .validate()
            .map_err(|error| ExecutorError::Validation(error.to_string()))?;

        let mut diagnostics = Vec::new();
        if definition.portability() != cueflow_core::Portability::Portable {
            diagnostics.push(PreflightDiagnostic {
                severity: PreflightSeverity::Warning,
                code: "portability-constrained".to_string(),
                message: format!(
                    "automation portability is {:?}; configure a matching platform before running",
                    definition.portability()
                ),
                step_id: None,
            });
        }

        for step in &definition.steps {
            let action = select_action(step, config.platform).for_platform(config.platform);
            diagnostics.extend(
                std::iter::once(action)
                    .chain(step.conditions.iter().map(|condition| Action::WaitFor {
                        condition: condition.for_platform(config.platform),
                    }))
                    .flat_map(|action| adapter.preflight(&action, config))
                    .map(|mut diagnostic| {
                        if diagnostic.step_id.is_none() {
                            diagnostic.step_id = Some(step.id.clone());
                        }
                        diagnostic
                    }),
            );
        }

        Ok(PreflightReport { diagnostics })
    }

    pub fn run_with<A: ExecutionAdapter, S: RunEventSink, C: ExecutionClock>(
        &self,
        definition: &AutomationDefinition,
        config: RunConfig,
        adapter: &mut A,
        control: &RunControl,
        sink: &mut S,
        clock: &C,
    ) -> Result<RunReport, ExecutorError> {
        definition
            .validate()
            .map_err(|error| ExecutorError::Validation(error.to_string()))?;
        let preflight = self.preflight(definition, &config, adapter)?;
        if !preflight.can_run() {
            let messages = preflight
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.severity == PreflightSeverity::Error)
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ExecutorError::Preflight(messages));
        }

        let run_id = config.run_id.clone().unwrap_or_else(generate_run_id);
        let mut events = Vec::new();
        emit(
            &mut events,
            sink,
            RunEvent::Started {
                run_id: run_id.clone(),
                automation_id: definition.id.clone(),
            },
        );

        info!(
            automation_id = %definition.id,
            run_id = %run_id,
            step_count = definition.steps.len(),
            dry_run = config.dry_run,
            "automation run started"
        );

        for step in &definition.steps {
            if !wait_until_runnable(
                definition,
                &run_id,
                Some(&step.id),
                control,
                &mut events,
                sink,
            ) {
                return Ok(finish_cancelled(definition, run_id, events, sink));
            }

            match self.run_step(
                definition,
                &config,
                adapter,
                control,
                &run_id,
                step,
                &mut events,
                sink,
                clock,
            ) {
                StepOutcome::Succeeded => {}
                StepOutcome::Cancelled => {
                    return Ok(finish_cancelled(definition, run_id, events, sink));
                }
                StepOutcome::Failed(error) => {
                    if step.on_error == OnErrorPolicy::Prompt {
                        emit(
                            &mut events,
                            sink,
                            RunEvent::ManualIntervention {
                                run_id: run_id.clone(),
                                automation_id: definition.id.clone(),
                                step_id: step.id.clone(),
                                error: error.clone(),
                            },
                        );
                    }

                    if step.on_error != OnErrorPolicy::Continue {
                        emit(
                            &mut events,
                            sink,
                            RunEvent::Completed {
                                run_id: run_id.clone(),
                                automation_id: definition.id.clone(),
                                status: RunStatus::Failed,
                            },
                        );
                        return Ok(RunReport {
                            run_id,
                            outcome: RunOutcome::Failed,
                            events,
                        });
                    }
                }
            }
        }

        if control.is_cancelled() {
            return Ok(finish_cancelled(definition, run_id, events, sink));
        }

        emit(
            &mut events,
            sink,
            RunEvent::Completed {
                run_id: run_id.clone(),
                automation_id: definition.id.clone(),
                status: RunStatus::Succeeded,
            },
        );
        Ok(RunReport {
            run_id,
            outcome: RunOutcome::Succeeded,
            events,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn run_step<A: ExecutionAdapter, S: RunEventSink, C: ExecutionClock>(
        &self,
        definition: &AutomationDefinition,
        config: &RunConfig,
        adapter: &mut A,
        control: &RunControl,
        run_id: &str,
        step: &Step,
        events: &mut Vec<RunEvent>,
        sink: &mut S,
        clock: &C,
    ) -> StepOutcome {
        let action = select_action(step, config.platform).for_platform(config.platform);
        emit(
            events,
            sink,
            RunEvent::StepStarted {
                run_id: run_id.to_string(),
                automation_id: definition.id.clone(),
                step_id: step.id.clone(),
                step_kind: action.kind().to_string(),
            },
        );
        emit_step_evidence(
            adapter,
            EvidencePhase::Before,
            &action,
            config,
            definition,
            run_id,
            &step.id,
            events,
            sink,
        );

        let max_attempts = step.retry.max_attempts;
        let mut last_error = None;
        for attempt in 1..=max_attempts {
            if !wait_until_runnable(definition, run_id, Some(&step.id), control, events, sink) {
                return StepOutcome::Cancelled;
            }

            let started_at = clock.now();
            let result = if config.dry_run {
                Ok(Vec::new())
            } else {
                execute_step(
                    adapter,
                    &step.conditions,
                    &action,
                    config,
                    step.timeout
                        .map(|timeout| Duration::from_millis(timeout.millis)),
                    control,
                    clock,
                )
            };
            let elapsed = clock.now().saturating_sub(started_at);
            let duration_wait = matches!(
                &action,
                Action::WaitFor {
                    condition: WaitCondition::Duration { .. }
                }
            );
            let result = if !duration_wait
                && step
                    .timeout
                    .is_some_and(|timeout| elapsed >= Duration::from_millis(timeout.millis))
            {
                Err(AdapterError::timeout())
            } else {
                result
            };

            if control.is_cancelled() {
                return StepOutcome::Cancelled;
            }

            match result {
                Ok(artifacts) => {
                    emit_step_evidence(
                        adapter,
                        EvidencePhase::After,
                        &action,
                        config,
                        definition,
                        run_id,
                        &step.id,
                        events,
                        sink,
                    );
                    emit(
                        events,
                        sink,
                        RunEvent::StepSucceeded {
                            run_id: run_id.to_string(),
                            automation_id: definition.id.clone(),
                            step_id: step.id.clone(),
                            artifacts,
                        },
                    );
                    return StepOutcome::Succeeded;
                }
                Err(error) => {
                    warn!(
                        automation_id = %definition.id,
                        run_id = %run_id,
                        step_id = %step.id,
                        step_kind = action.kind(),
                        attempts = attempt,
                        error = %error,
                        "automation step attempt failed"
                    );
                    last_error = Some(error);
                    if attempt < max_attempts {
                        let delay = retry_delay(step, attempt);
                        if !sleep_with_control(clock, control, delay) {
                            return StepOutcome::Cancelled;
                        }
                    }
                }
            }
        }

        let error = last_error
            .unwrap_or_else(|| AdapterError::new("step failed"))
            .into_run_error(step.id.clone());
        emit_step_evidence(
            adapter,
            EvidencePhase::Failure,
            &action,
            config,
            definition,
            run_id,
            &step.id,
            events,
            sink,
        );
        error!(
            automation_id = %definition.id,
            run_id = %run_id,
            step_id = %step.id,
            step_kind = action.kind(),
            error = %error.message,
            "automation step failed"
        );
        emit(
            events,
            sink,
            RunEvent::StepFailed {
                run_id: run_id.to_string(),
                automation_id: definition.id.clone(),
                step_id: step.id.clone(),
                error: error.clone(),
            },
        );
        StepOutcome::Failed(error)
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_step_evidence<A: ExecutionAdapter, S: RunEventSink>(
    adapter: &mut A,
    phase: EvidencePhase,
    action: &Action,
    config: &RunConfig,
    definition: &AutomationDefinition,
    run_id: &str,
    step_id: &str,
    events: &mut Vec<RunEvent>,
    sink: &mut S,
) {
    if !config.capture_step_evidence {
        return;
    }
    match adapter.capture_step_evidence(phase, action, config, run_id, &definition.id, step_id) {
        Ok(artifacts) => {
            for artifact in artifacts {
                emit(
                    events,
                    sink,
                    RunEvent::Artifact {
                        run_id: run_id.to_string(),
                        automation_id: definition.id.clone(),
                        step_id: Some(step_id.to_string()),
                        artifact,
                    },
                );
            }
        }
        Err(error) => {
            emit(
                events,
                sink,
                RunEvent::Log {
                    run_id: run_id.to_string(),
                    automation_id: definition.id.clone(),
                    step_id: Some(step_id.to_string()),
                    level: cueflow_core::LogLevel::Warn,
                    message: format!(
                        "step evidence capture failed during {}: {error}",
                        phase.as_str()
                    ),
                },
            );
        }
    }
}

fn execute_action<A: ExecutionAdapter, C: ExecutionClock>(
    adapter: &mut A,
    action: &Action,
    config: &RunConfig,
    timeout: Option<Duration>,
    control: &RunControl,
    clock: &C,
) -> Result<Vec<Artifact>, AdapterError> {
    match action {
        Action::ClickTarget { target } => {
            adapter.invoke_target(target, config)?;
            Ok(Vec::new())
        }
        Action::TypeText {
            text,
            target: Some(target),
        } => {
            adapter.set_target_text(target, text, config)?;
            Ok(Vec::new())
        }
        Action::Scroll {
            delta_x,
            delta_y,
            target: Some(target),
        } => {
            adapter.scroll_target(target, *delta_x, *delta_y, config)?;
            Ok(Vec::new())
        }
        Action::WaitFor { condition } => {
            wait_for_condition(adapter, condition, config, timeout, control, clock)?;
            Ok(Vec::new())
        }
        Action::Assert { assertion } => {
            let passed = match assertion {
                Assertion::TargetExists { target } if target.accessibility.is_some() => {
                    adapter.target_exists(target, config)?
                }
                Assertion::Condition {
                    condition: WaitCondition::WindowFocused { target },
                } if target.accessibility.is_some() => adapter.target_is_focused(target, config)?,
                _ => adapter.evaluate_assertion(assertion, config)?,
            };
            if passed {
                Ok(Vec::new())
            } else {
                Err(AdapterError::assertion("assertion failed"))
            }
        }
        _ => adapter.execute_with_control(action, config, control, timeout),
    }
}

fn execute_step<A: ExecutionAdapter, C: ExecutionClock>(
    adapter: &mut A,
    conditions: &[WaitCondition],
    action: &Action,
    config: &RunConfig,
    timeout: Option<Duration>,
    control: &RunControl,
    clock: &C,
) -> Result<Vec<Artifact>, AdapterError> {
    let started_at = clock.now();
    for condition in conditions {
        let condition = condition.for_platform(config.platform);
        let timeout = remaining_timeout(timeout, started_at, clock)?;
        wait_for_condition(adapter, &condition, config, timeout, control, clock)?;
    }
    let timeout = remaining_timeout(timeout, started_at, clock)?;
    execute_action(adapter, action, config, timeout, control, clock)
}

fn remaining_timeout<C: ExecutionClock>(
    timeout: Option<Duration>,
    started_at: Duration,
    clock: &C,
) -> Result<Option<Duration>, AdapterError> {
    timeout
        .map(|timeout| {
            timeout
                .checked_sub(clock.now().saturating_sub(started_at))
                .ok_or_else(AdapterError::timeout)
        })
        .transpose()
}

fn wait_for_condition<A: ExecutionAdapter, C: ExecutionClock>(
    adapter: &mut A,
    condition: &WaitCondition,
    config: &RunConfig,
    timeout: Option<Duration>,
    control: &RunControl,
    clock: &C,
) -> Result<(), AdapterError> {
    if let WaitCondition::Duration { duration } = condition {
        let requested = Duration::from_millis(duration.millis);
        let sleep_for = timeout.map_or(requested, |timeout| requested.min(timeout));
        if !sleep_with_control(clock, control, sleep_for) {
            return Err(AdapterError::cancelled());
        }
        if timeout.is_some_and(|timeout| requested > timeout) {
            return Err(AdapterError::timeout());
        }
        return Ok(());
    }

    let started_at = clock.now();
    let timeout = timeout.unwrap_or(Duration::from_secs(30));
    loop {
        if control.is_cancelled() {
            return Err(AdapterError::cancelled());
        }

        let state = match condition {
            WaitCondition::WindowExists { target } if target.accessibility.is_some() => {
                if adapter.target_exists(target, config)? {
                    ConditionState::Satisfied
                } else {
                    ConditionState::Pending
                }
            }
            WaitCondition::WindowFocused { target } if target.accessibility.is_some() => {
                if adapter.target_is_focused(target, config)? {
                    ConditionState::Satisfied
                } else {
                    ConditionState::Pending
                }
            }
            _ => adapter.evaluate_wait_with_control(
                condition,
                config,
                control,
                Some(timeout.saturating_sub(clock.now().saturating_sub(started_at))),
            )?,
        };
        match state {
            ConditionState::Satisfied => return Ok(()),
            ConditionState::Pending => {}
        }

        let elapsed = clock.now().saturating_sub(started_at);
        if elapsed >= timeout {
            return Err(AdapterError::timeout());
        }
        let remaining = timeout.saturating_sub(elapsed);
        if !sleep_with_control(clock, control, remaining.min(Duration::from_millis(100))) {
            return Err(AdapterError::cancelled());
        }
    }
}

#[derive(Debug)]
enum StepOutcome {
    Succeeded,
    Failed(RunError),
    Cancelled,
}

fn emit<S: RunEventSink>(events: &mut Vec<RunEvent>, sink: &mut S, event: RunEvent) {
    sink.emit(&event);
    events.push(event);
}

fn wait_until_runnable<S: RunEventSink>(
    definition: &AutomationDefinition,
    run_id: &str,
    step_id: Option<&str>,
    control: &RunControl,
    events: &mut Vec<RunEvent>,
    sink: &mut S,
) -> bool {
    if control.is_cancelled() {
        return false;
    }

    if control.is_paused() {
        let step_id = step_id.map(str::to_string);
        emit(
            events,
            sink,
            RunEvent::Paused {
                run_id: run_id.to_string(),
                automation_id: definition.id.clone(),
                step_id: step_id.clone(),
            },
        );
        if !control.wait_for_resume() {
            return false;
        }
        emit(
            events,
            sink,
            RunEvent::Resumed {
                run_id: run_id.to_string(),
                automation_id: definition.id.clone(),
                step_id,
            },
        );
    }

    !control.is_cancelled()
}

fn finish_cancelled<S: RunEventSink>(
    definition: &AutomationDefinition,
    run_id: String,
    mut events: Vec<RunEvent>,
    sink: &mut S,
) -> RunReport {
    emit(
        &mut events,
        sink,
        RunEvent::Cancelled {
            run_id: run_id.clone(),
            automation_id: definition.id.clone(),
        },
    );
    RunReport {
        run_id,
        outcome: RunOutcome::Cancelled,
        events,
    }
}

fn select_action(step: &Step, platform: Option<Platform>) -> &Action {
    platform
        .and_then(|platform| {
            step.platform_overrides
                .iter()
                .find(|override_action| override_action.platform == platform)
                .map(|override_action| override_action.action.as_ref())
        })
        .unwrap_or(&step.action)
}

fn retry_delay(step: &Step, failed_attempt: u32) -> Duration {
    let Some(delay) = step.retry.delay else {
        return Duration::ZERO;
    };
    let multiplier = match step.retry.backoff {
        BackoffPolicy::None => 1,
        BackoffPolicy::Linear => failed_attempt as u64,
        BackoffPolicy::Exponential => 1_u64
            .checked_shl(failed_attempt.saturating_sub(1))
            .unwrap_or(u64::MAX),
    };
    Duration::from_millis(delay.millis.saturating_mul(multiplier))
}

fn sleep_with_control<C: ExecutionClock>(
    clock: &C,
    control: &RunControl,
    duration: Duration,
) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if control.is_cancelled() {
            return false;
        }

        let slice = remaining.min(Duration::from_millis(10));
        clock.sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }

    !control.is_cancelled()
}

fn generate_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("run-{nanos}")
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};

    use cueflow_core::{
        AutomationDefinition, CURRENT_SCHEMA_VERSION, DurationSpec, PlatformActionOverride,
        RetryPolicy, Target,
    };

    use super::*;

    #[derive(Default)]
    struct RecordingAdapter {
        calls: usize,
        actions: Vec<String>,
        failures_before_success: usize,
    }

    impl ExecutionAdapter for RecordingAdapter {
        fn execute(
            &mut self,
            action: &Action,
            _config: &RunConfig,
        ) -> Result<Vec<Artifact>, AdapterError> {
            self.calls += 1;
            self.actions.push(action.kind().to_string());
            if self.calls <= self.failures_before_success {
                Err(AdapterError::new("simulated adapter failure"))
            } else {
                Ok(Vec::new())
            }
        }
    }

    #[derive(Default)]
    struct CollectingSink(Vec<RunEvent>);

    impl RunEventSink for CollectingSink {
        fn emit(&mut self, event: &RunEvent) {
            self.0.push(event.clone());
        }
    }

    #[derive(Default)]
    struct FakeClock {
        millis: AtomicU64,
    }

    impl FakeClock {
        fn advance(&self, duration: Duration) {
            self.millis
                .fetch_add(duration.as_millis() as u64, Ordering::Relaxed);
        }
    }

    impl ExecutionClock for FakeClock {
        fn now(&self) -> Duration {
            Duration::from_millis(self.millis.load(Ordering::Relaxed))
        }

        fn sleep(&self, duration: Duration) {
            self.advance(duration);
        }
    }

    struct TimedAdapter<'a> {
        clock: &'a FakeClock,
        elapsed: Duration,
    }

    struct CancellingAdapter {
        control: RunControl,
    }

    impl ExecutionAdapter for CancellingAdapter {
        fn execute(
            &mut self,
            _action: &Action,
            _config: &RunConfig,
        ) -> Result<Vec<Artifact>, AdapterError> {
            self.control.cancel();
            Ok(Vec::new())
        }
    }

    struct CancellingClock {
        clock: FakeClock,
        control: RunControl,
        cancelled: AtomicBool,
    }

    impl ExecutionClock for CancellingClock {
        fn now(&self) -> Duration {
            self.clock.now()
        }

        fn sleep(&self, duration: Duration) {
            self.clock.sleep(duration);
            if !self.cancelled.swap(true, Ordering::Relaxed) {
                self.control.cancel();
            }
        }
    }

    impl ExecutionAdapter for TimedAdapter<'_> {
        fn execute(
            &mut self,
            _action: &Action,
            _config: &RunConfig,
        ) -> Result<Vec<Artifact>, AdapterError> {
            self.clock.advance(self.elapsed);
            Ok(Vec::new())
        }
    }

    struct QueryAdapter {
        wait_states: VecDeque<ConditionState>,
        assertion_result: bool,
        execute_calls: usize,
        preflight_diagnostics: Vec<PreflightDiagnostic>,
    }

    struct DiagnosticFailingAdapter;

    impl ExecutionAdapter for DiagnosticFailingAdapter {
        fn execute(
            &mut self,
            _action: &Action,
            _config: &RunConfig,
        ) -> Result<Vec<Artifact>, AdapterError> {
            Err(AdapterError::new("simulated adapter failure")
                .with_failure_kind(FailureKind::NotFound)
                .with_source("selector: titleContains=\"Missing\"; candidates: none"))
        }
    }

    #[derive(Default)]
    struct SemanticRecordingAdapter {
        invoked_targets: usize,
        text_targets: Vec<String>,
        queried_targets: usize,
        focused_targets: usize,
        scroll_targets: Vec<(i32, i32)>,
        execute_calls: usize,
    }

    impl ExecutionAdapter for SemanticRecordingAdapter {
        fn execute(
            &mut self,
            _action: &Action,
            _config: &RunConfig,
        ) -> Result<Vec<Artifact>, AdapterError> {
            self.execute_calls += 1;
            Ok(Vec::new())
        }

        fn invoke_target(
            &mut self,
            _target: &Target,
            _config: &RunConfig,
        ) -> Result<(), AdapterError> {
            self.invoked_targets += 1;
            Ok(())
        }

        fn target_exists(
            &mut self,
            _target: &Target,
            _config: &RunConfig,
        ) -> Result<bool, AdapterError> {
            self.queried_targets += 1;
            Ok(true)
        }

        fn target_is_focused(
            &mut self,
            _target: &Target,
            _config: &RunConfig,
        ) -> Result<bool, AdapterError> {
            self.focused_targets += 1;
            Ok(true)
        }

        fn scroll_target(
            &mut self,
            _target: &Target,
            delta_x: i32,
            delta_y: i32,
            _config: &RunConfig,
        ) -> Result<(), AdapterError> {
            self.scroll_targets.push((delta_x, delta_y));
            Ok(())
        }

        fn set_target_text(
            &mut self,
            _target: &Target,
            text: &str,
            _config: &RunConfig,
        ) -> Result<(), AdapterError> {
            self.text_targets.push(text.to_string());
            Ok(())
        }
    }

    impl ExecutionAdapter for QueryAdapter {
        fn execute(
            &mut self,
            _action: &Action,
            _config: &RunConfig,
        ) -> Result<Vec<Artifact>, AdapterError> {
            self.execute_calls += 1;
            Ok(Vec::new())
        }

        fn evaluate_wait(
            &mut self,
            _condition: &WaitCondition,
            _config: &RunConfig,
        ) -> Result<ConditionState, AdapterError> {
            Ok(self
                .wait_states
                .pop_front()
                .unwrap_or(ConditionState::Pending))
        }

        fn evaluate_assertion(
            &mut self,
            _assertion: &Assertion,
            _config: &RunConfig,
        ) -> Result<bool, AdapterError> {
            Ok(self.assertion_result)
        }

        fn preflight(&self, _action: &Action, _config: &RunConfig) -> Vec<PreflightDiagnostic> {
            self.preflight_diagnostics.clone()
        }
    }

    fn definition() -> AutomationDefinition {
        AutomationDefinition {
            id: "demo-ready".to_string(),
            title: "Prepare demo".to_string(),
            description: None,
            schema_version: CURRENT_SCHEMA_VERSION,
            version: None,
            variables: Default::default(),
            metadata: Default::default(),
            steps: vec![Step {
                id: "focus-browser".to_string(),
                label: None,
                action: Action::FocusWindow {
                    target: Target::app("Browser"),
                },
                timeout: None,
                retry: RetryPolicy::default(),
                on_error: OnErrorPolicy::Stop,
                conditions: Vec::new(),
                platform_overrides: Vec::new(),
            }],
        }
    }

    #[test]
    fn dry_run_emits_deterministic_success_events_without_adapter_calls() {
        let executor = AutomationExecutor::new();
        let mut adapter = RecordingAdapter::default();
        let config = RunConfig {
            run_id: Some("run-test".to_string()),
            dry_run: true,
            ..RunConfig::default()
        };

        let report = executor
            .run(&definition(), config, &mut adapter)
            .expect("dry run");

        assert_eq!(adapter.calls, 0);
        assert_eq!(report.outcome, RunOutcome::Succeeded);
        assert_eq!(report.events.len(), 4);
    }

    #[test]
    fn cancellation_before_a_step_is_terminal() {
        let executor = AutomationExecutor::new();
        let mut adapter = RecordingAdapter::default();
        let control = RunControl::default();
        control.cancel();
        let mut sink = CollectingSink::default();
        let report = executor
            .run_with(
                &definition(),
                RunConfig::default(),
                &mut adapter,
                &control,
                &mut sink,
                &FakeClock::default(),
            )
            .expect("run is cancelled");

        assert_eq!(report.outcome, RunOutcome::Cancelled);
        assert_eq!(adapter.calls, 0);
        assert!(matches!(
            report.events.last(),
            Some(RunEvent::Cancelled { .. })
        ));
        assert_eq!(sink.0, report.events);
    }

    #[test]
    fn retries_obey_backoff_and_emit_a_single_terminal_success() {
        let executor = AutomationExecutor::new();
        let mut definition = definition();
        definition.steps[0].retry = RetryPolicy {
            max_attempts: 3,
            delay: Some(DurationSpec::from_millis(10)),
            backoff: BackoffPolicy::Exponential,
        };
        let mut adapter = RecordingAdapter {
            failures_before_success: 2,
            ..RecordingAdapter::default()
        };
        let clock = FakeClock::default();
        let control = RunControl::default();
        let mut sink = NoopEventSink;

        let report = executor
            .run_with(
                &definition,
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
                &control,
                &mut sink,
                &clock,
            )
            .expect("eventual success");

        assert_eq!(adapter.calls, 3);
        assert_eq!(clock.now(), Duration::from_millis(30));
        assert_eq!(report.outcome, RunOutcome::Succeeded);
        assert_eq!(
            report
                .events
                .iter()
                .filter(|event| matches!(event, RunEvent::Completed { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn configured_platform_selects_a_step_override() {
        let executor = AutomationExecutor::new();
        let mut definition = definition();
        definition.steps[0]
            .platform_overrides
            .push(PlatformActionOverride {
                platform: Platform::Windows,
                action: Box::new(Action::LaunchApp {
                    app: "demo.exe".to_string(),
                    target: None,
                }),
            });
        let mut adapter = RecordingAdapter::default();
        let control = RunControl::default();
        let mut sink = NoopEventSink;

        let report = executor
            .run_with(
                &definition,
                RunConfig {
                    dry_run: false,
                    platform: Some(Platform::Windows),
                    ..RunConfig::default()
                },
                &mut adapter,
                &control,
                &mut sink,
                &FakeClock::default(),
            )
            .expect("run succeeds");

        assert_eq!(adapter.actions, vec!["launchApp"]);
        assert!(matches!(
            report.events[1],
            RunEvent::StepStarted { ref step_kind, .. } if step_kind == "launchApp"
        ));
    }

    #[test]
    fn semantic_actions_use_explicit_adapter_operations() {
        let executor = AutomationExecutor::new();
        let mut definition = definition();
        definition.steps = vec![
            Step {
                id: "click".to_string(),
                label: None,
                action: Action::ClickTarget {
                    target: Target::app("Browser"),
                },
                timeout: None,
                retry: RetryPolicy::default(),
                on_error: OnErrorPolicy::Stop,
                conditions: Vec::new(),
                platform_overrides: Vec::new(),
            },
            Step {
                id: "type".to_string(),
                label: None,
                action: Action::TypeText {
                    text: "Cueflow".to_string(),
                    target: Some(Target::app("Browser")),
                },
                timeout: None,
                retry: RetryPolicy::default(),
                on_error: OnErrorPolicy::Stop,
                conditions: Vec::new(),
                platform_overrides: Vec::new(),
            },
            Step {
                id: "assert".to_string(),
                label: None,
                action: Action::Assert {
                    assertion: Assertion::TargetExists {
                        target: Target {
                            app_name: None,
                            process_name: None,
                            window_title: Some("Demo".to_string()),
                            title_contains: None,
                            url: None,
                            file_path: None,
                            accessibility: Some(cueflow_core::AccessibilityTarget {
                                id: Some("submit".to_string()),
                                name: None,
                                control_type: None,
                                path: None,
                            }),
                            image: None,
                            coordinates: None,
                            platform_selectors: Default::default(),
                        },
                    },
                },
                timeout: None,
                retry: RetryPolicy::default(),
                on_error: OnErrorPolicy::Stop,
                conditions: Vec::new(),
                platform_overrides: Vec::new(),
            },
            Step {
                id: "scroll".to_string(),
                label: None,
                action: Action::Scroll {
                    delta_x: 0,
                    delta_y: -1,
                    target: Some(Target::app("Browser")),
                },
                timeout: None,
                retry: RetryPolicy::default(),
                on_error: OnErrorPolicy::Stop,
                conditions: vec![WaitCondition::WindowFocused {
                    target: Target {
                        app_name: None,
                        process_name: None,
                        window_title: Some("Demo".to_string()),
                        title_contains: None,
                        url: None,
                        file_path: None,
                        accessibility: Some(cueflow_core::AccessibilityTarget {
                            id: Some("submit".to_string()),
                            name: None,
                            control_type: None,
                            path: None,
                        }),
                        image: None,
                        coordinates: None,
                        platform_selectors: Default::default(),
                    },
                }],
                platform_overrides: Vec::new(),
            },
        ];
        let mut adapter = SemanticRecordingAdapter::default();

        let report = executor
            .run(
                &definition,
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
            )
            .expect("semantic actions succeed");

        assert_eq!(report.outcome, RunOutcome::Succeeded);
        assert_eq!(adapter.invoked_targets, 1);
        assert_eq!(adapter.text_targets, vec!["Cueflow"]);
        assert_eq!(adapter.queried_targets, 1);
        assert_eq!(adapter.focused_targets, 1);
        assert_eq!(adapter.scroll_targets, vec![(0, -1)]);
        assert_eq!(adapter.execute_calls, 0);
    }

    #[test]
    fn step_conditions_are_satisfied_before_the_action_executes() {
        let executor = AutomationExecutor::new();
        let mut definition = definition();
        definition.steps[0].conditions = vec![WaitCondition::WindowExists {
            target: Target::app("Browser"),
        }];
        definition.steps[0].timeout = Some(DurationSpec::from_millis(500));
        let mut adapter = QueryAdapter {
            wait_states: VecDeque::from([ConditionState::Pending, ConditionState::Satisfied]),
            assertion_result: true,
            execute_calls: 0,
            preflight_diagnostics: Vec::new(),
        };
        let clock = FakeClock::default();
        let control = RunControl::default();
        let mut sink = NoopEventSink;

        let report = executor
            .run_with(
                &definition,
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
                &control,
                &mut sink,
                &clock,
            )
            .expect("run succeeds");

        assert_eq!(report.outcome, RunOutcome::Succeeded);
        assert_eq!(adapter.execute_calls, 1);
        assert_eq!(clock.now(), Duration::from_millis(100));
    }

    #[test]
    fn failed_step_conditions_prevent_action_execution() {
        let executor = AutomationExecutor::new();
        let mut definition = definition();
        definition.steps[0].conditions = vec![WaitCondition::WindowExists {
            target: Target::app("Browser"),
        }];
        definition.steps[0].timeout = Some(DurationSpec::from_millis(100));
        let mut adapter = QueryAdapter {
            wait_states: VecDeque::new(),
            assertion_result: true,
            execute_calls: 0,
            preflight_diagnostics: Vec::new(),
        };
        let control = RunControl::default();
        let mut sink = NoopEventSink;

        let report = executor
            .run_with(
                &definition,
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
                &control,
                &mut sink,
                &FakeClock::default(),
            )
            .expect("condition timeout returns a failed report");

        assert_eq!(report.outcome, RunOutcome::Failed);
        assert_eq!(adapter.execute_calls, 0);
        assert!(matches!(
            report.events[2],
            RunEvent::StepFailed {
                error: RunError {
                    kind: RunErrorKind::Timeout,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn prompt_policy_emits_manual_intervention_before_failing() {
        let executor = AutomationExecutor::new();
        let mut definition = definition();
        definition.steps[0].on_error = OnErrorPolicy::Prompt;
        let mut adapter = RecordingAdapter {
            failures_before_success: 1,
            ..RecordingAdapter::default()
        };
        let control = RunControl::default();
        let mut sink = NoopEventSink;

        let report = executor
            .run_with(
                &definition,
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
                &control,
                &mut sink,
                &FakeClock::default(),
            )
            .expect("failed report");

        assert_eq!(report.outcome, RunOutcome::Failed);
        assert!(
            report
                .events
                .iter()
                .any(|event| matches!(event, RunEvent::ManualIntervention { .. }))
        );
    }

    #[test]
    fn timeout_after_a_synchronous_adapter_call_is_reported_as_a_failure() {
        let executor = AutomationExecutor::new();
        let mut definition = definition();
        definition.steps[0].timeout = Some(DurationSpec::from_millis(5));
        let clock = FakeClock::default();
        let mut adapter = TimedAdapter {
            clock: &clock,
            elapsed: Duration::from_millis(6),
        };
        let control = RunControl::default();
        let mut sink = NoopEventSink;

        let report = executor
            .run_with(
                &definition,
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
                &control,
                &mut sink,
                &clock,
            )
            .expect("failed report");

        assert_eq!(report.outcome, RunOutcome::Failed);
        assert!(matches!(
            report.events[2],
            RunEvent::StepFailed {
                error: RunError {
                    kind: RunErrorKind::Timeout,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn adapter_failure_diagnostics_are_preserved_in_run_errors() {
        let executor = AutomationExecutor::new();
        let mut adapter = DiagnosticFailingAdapter;

        let report = executor
            .run(
                &definition(),
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
            )
            .expect("failed adapter returns a run report");

        let RunEvent::StepFailed { error, .. } = &report.events[2] else {
            panic!("failed step event is emitted");
        };
        assert_eq!(
            error.source.as_deref(),
            Some("selector: titleContains=\"Missing\"; candidates: none")
        );
        assert_eq!(error.failure_kind, Some(FailureKind::NotFound));
    }

    #[test]
    fn cancellation_during_adapter_execution_wins_over_success() {
        let executor = AutomationExecutor::new();
        let control = RunControl::default();
        let mut adapter = CancellingAdapter {
            control: control.clone(),
        };
        let mut sink = NoopEventSink;

        let report = executor
            .run_with(
                &definition(),
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
                &control,
                &mut sink,
                &FakeClock::default(),
            )
            .expect("cancelled report");

        assert_eq!(report.outcome, RunOutcome::Cancelled);
        assert!(matches!(
            report.events.last(),
            Some(RunEvent::Cancelled { .. })
        ));
        assert!(
            !report
                .events
                .iter()
                .any(|event| matches!(event, RunEvent::StepSucceeded { .. }))
        );
    }

    #[test]
    fn cancellation_during_retry_backoff_is_observed_before_the_full_delay() {
        let executor = AutomationExecutor::new();
        let mut definition = definition();
        definition.steps[0].retry = RetryPolicy {
            max_attempts: 2,
            delay: Some(DurationSpec::from_millis(100)),
            backoff: BackoffPolicy::None,
        };
        let control = RunControl::default();
        let clock = CancellingClock {
            clock: FakeClock::default(),
            control: control.clone(),
            cancelled: AtomicBool::new(false),
        };
        let mut adapter = RecordingAdapter {
            failures_before_success: 1,
            ..RecordingAdapter::default()
        };
        let mut sink = NoopEventSink;

        let report = executor
            .run_with(
                &definition,
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
                &control,
                &mut sink,
                &clock,
            )
            .expect("cancelled report");

        assert_eq!(report.outcome, RunOutcome::Cancelled);
        assert_eq!(adapter.calls, 1);
        assert_eq!(clock.now(), Duration::from_millis(10));
    }

    #[test]
    fn wait_conditions_poll_adapter_queries_until_satisfied() {
        let executor = AutomationExecutor::new();
        let mut definition = definition();
        definition.steps[0].action = Action::WaitFor {
            condition: WaitCondition::ProcessRunning {
                target: Target::app("Browser"),
            },
        };
        definition.steps[0].timeout = Some(DurationSpec::from_millis(500));
        let mut adapter = QueryAdapter {
            wait_states: VecDeque::from([ConditionState::Pending, ConditionState::Satisfied]),
            assertion_result: true,
            execute_calls: 0,
            preflight_diagnostics: Vec::new(),
        };
        let clock = FakeClock::default();
        let control = RunControl::default();
        let mut sink = NoopEventSink;

        let report = executor
            .run_with(
                &definition,
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
                &control,
                &mut sink,
                &clock,
            )
            .expect("wait succeeds");

        assert_eq!(report.outcome, RunOutcome::Succeeded);
        assert_eq!(clock.now(), Duration::from_millis(100));
        assert_eq!(adapter.execute_calls, 0);
    }

    #[test]
    fn assertions_produce_structured_assertion_failures() {
        let executor = AutomationExecutor::new();
        let mut definition = definition();
        definition.steps[0].action = Action::Assert {
            assertion: Assertion::TargetExists {
                target: Target::app("Browser"),
            },
        };
        let mut adapter = QueryAdapter {
            wait_states: VecDeque::new(),
            assertion_result: false,
            execute_calls: 0,
            preflight_diagnostics: Vec::new(),
        };
        let control = RunControl::default();
        let mut sink = NoopEventSink;

        let report = executor
            .run_with(
                &definition,
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
                &control,
                &mut sink,
                &FakeClock::default(),
            )
            .expect("assertion failure returns report");

        assert!(matches!(
            report.events[2],
            RunEvent::StepFailed {
                error: RunError {
                    kind: RunErrorKind::Assertion,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn preflight_blocks_side_effects_when_an_adapter_reports_errors() {
        let executor = AutomationExecutor::new();
        let mut adapter = QueryAdapter {
            wait_states: VecDeque::new(),
            assertion_result: true,
            execute_calls: 0,
            preflight_diagnostics: vec![PreflightDiagnostic {
                severity: PreflightSeverity::Error,
                code: "missing-permission".to_string(),
                message: "Accessibility permission is required".to_string(),
                step_id: None,
            }],
        };
        let control = RunControl::default();
        let mut sink = NoopEventSink;

        assert!(matches!(
            executor.run_with(
                &definition(),
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
                &control,
                &mut sink,
                &FakeClock::default(),
            ),
            Err(ExecutorError::Preflight(_))
        ));
        assert_eq!(adapter.execute_calls, 0);
    }

    #[test]
    fn duration_wait_honors_its_step_timeout_without_sleeping_past_it() {
        let executor = AutomationExecutor::new();
        let mut definition = definition();
        definition.steps[0].action = Action::WaitFor {
            condition: WaitCondition::Duration {
                duration: DurationSpec::from_millis(100),
            },
        };
        definition.steps[0].timeout = Some(DurationSpec::from_millis(5));
        let mut adapter = RecordingAdapter::default();
        let clock = FakeClock::default();
        let control = RunControl::default();
        let mut sink = NoopEventSink;

        let report = executor
            .run_with(
                &definition,
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
                &control,
                &mut sink,
                &clock,
            )
            .expect("timeout returns a report");

        assert_eq!(report.outcome, RunOutcome::Failed);
        assert_eq!(clock.now(), Duration::from_millis(5));
        assert_eq!(adapter.calls, 0);
    }

    #[test]
    fn adapters_that_only_implement_execute_keep_wait_support() {
        let executor = AutomationExecutor::new();
        let mut definition = definition();
        definition.steps[0].action = Action::WaitFor {
            condition: WaitCondition::ProcessRunning {
                target: Target::app("Browser"),
            },
        };
        let mut adapter = RecordingAdapter::default();
        let control = RunControl::default();
        let mut sink = NoopEventSink;

        let report = executor
            .run_with(
                &definition,
                RunConfig {
                    dry_run: false,
                    ..RunConfig::default()
                },
                &mut adapter,
                &control,
                &mut sink,
                &FakeClock::default(),
            )
            .expect("legacy adapter wait succeeds");

        assert_eq!(report.outcome, RunOutcome::Succeeded);
        assert_eq!(adapter.actions, vec!["waitFor"]);
    }
}
