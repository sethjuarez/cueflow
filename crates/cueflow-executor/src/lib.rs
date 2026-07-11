use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cueflow_core::{
    Action, Artifact, AutomationDefinition, BackoffPolicy, OnErrorPolicy, Platform, RunConfig,
    RunError, RunErrorKind, RunEvent, RunStatus, Step,
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
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("{public_message}")]
pub struct AdapterError {
    public_message: String,
    kind: RunErrorKind,
    diagnostics: Option<String>,
}

impl AdapterError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            public_message: message.into(),
            kind: RunErrorKind::Adapter,
            diagnostics: None,
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            public_message: message.into(),
            kind: RunErrorKind::Unsupported,
            diagnostics: None,
        }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.diagnostics = Some(source.into());
        self
    }

    fn into_run_error(self, step_id: String) -> RunError {
        RunError::new(self.kind, self.public_message).with_step_id(step_id)
    }
}

pub trait ExecutionAdapter {
    fn execute(
        &mut self,
        action: &Action,
        config: &RunConfig,
    ) -> Result<Vec<Artifact>, AdapterError>;
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
        let action = select_action(step, config.platform);
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
                adapter.execute(action, config)
            };
            let elapsed = clock.now().saturating_sub(started_at);
            let result = match (result, step.timeout) {
                (_, Some(timeout)) if elapsed >= Duration::from_millis(timeout.millis) => {
                    Err(AdapterError {
                        public_message: "step timed out".to_string(),
                        kind: RunErrorKind::Timeout,
                        diagnostics: None,
                    })
                }
                (result, _) => result,
            };

            if control.is_cancelled() {
                return StepOutcome::Cancelled;
            }

            match result {
                Ok(artifacts) => {
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
}
