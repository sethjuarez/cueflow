use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

use crate::AdapterCapabilities;
use cueflow_core::{
    Action, Artifact, Assertion, Platform, PreflightDiagnostic, PreflightSeverity, RunConfig,
    Target, WaitCondition,
};
use cueflow_executor::{AdapterError, ConditionState, ExecutionAdapter, RunControl};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HWND, INVALID_HANDLE_VALUE, LPARAM, RPC_E_CHANGED_MODE},
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject},
        UI::{
            Accessibility::{
                CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
                IUIAutomationScrollPattern, IUIAutomationValuePattern, ScrollAmount,
                ScrollAmount_NoAmount, ScrollAmount_SmallDecrement, ScrollAmount_SmallIncrement,
                TreeScope_Subtree, UIA_InvokePatternId, UIA_ScrollPatternId, UIA_ValuePatternId,
            },
            Input::KeyboardAndMouse::{
                INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
                KEYEVENTF_UNICODE, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_BACK,
                VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_MENU,
                VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
            },
            Shell::ShellExecuteW,
            WindowsAndMessaging::{
                EnumWindows, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
                IsWindowVisible, SW_SHOWNORMAL, SetForegroundWindow,
            },
        },
    },
    core::{BOOL, BSTR, HSTRING},
};

#[derive(Debug, Default)]
pub struct WindowsDesktopAdapter;

impl WindowsDesktopAdapter {
    pub fn capabilities() -> AdapterCapabilities {
        AdapterCapabilities {
            platform: Platform::Windows,
            supports_launch: true,
            supports_focus: true,
            supports_input: true,
            supports_semantic_targets: true,
            supports_window_queries: true,
            supports_process_queries: true,
        }
    }
}

impl ExecutionAdapter for WindowsDesktopAdapter {
    fn execute(
        &mut self,
        action: &Action,
        config: &RunConfig,
    ) -> Result<Vec<Artifact>, AdapterError> {
        match action {
            Action::LaunchUrl { url, .. } => shell_open(url),
            Action::LaunchApp { app, .. } => shell_open(app),
            Action::FocusWindow { target } => focus_window(target).map(|_| Vec::new()),
            Action::TypeText {
                text,
                target: Some(target),
            } => self
                .set_target_text(target, text, config)
                .map(|_| Vec::new()),
            Action::TypeText { text, target: None } => send_text(text).map(|_| Vec::new()),
            Action::PressKey { keys, .. } => send_key_chord(keys).map(|_| Vec::new()),
            Action::Scroll { delta_y, .. } => send_scroll(*delta_y).map(|_| Vec::new()),
            Action::ClickTarget { target } => {
                self.invoke_target(target, config).map(|_| Vec::new())
            }
            Action::RunCommand { command, args } => {
                run_command(command, args, config, &RunControl::default(), None).map(|_| Vec::new())
            }
            Action::OpenFile { path, .. } => shell_open(path),
            _ => Err(AdapterError::unsupported(format!(
                "Windows adapter does not yet support {}",
                action.kind()
            ))),
        }
    }

    fn evaluate_wait(
        &mut self,
        condition: &WaitCondition,
        config: &RunConfig,
    ) -> Result<ConditionState, AdapterError> {
        match condition {
            WaitCondition::FileExists { path } => Ok(if Path::new(path).exists() {
                ConditionState::Satisfied
            } else {
                ConditionState::Pending
            }),
            WaitCondition::WindowExists { target } => {
                if target.accessibility.is_some() {
                    return semantic_target_exists(target).map(|exists| {
                        if exists {
                            ConditionState::Satisfied
                        } else {
                            ConditionState::Pending
                        }
                    });
                }
                window_exists(target).map(|exists| {
                    if exists {
                        ConditionState::Satisfied
                    } else {
                        ConditionState::Pending
                    }
                })
            }
            WaitCondition::WindowFocused { target } => window_is_focused(target).map(|focused| {
                if focused {
                    ConditionState::Satisfied
                } else {
                    ConditionState::Pending
                }
            }),
            WaitCondition::ProcessRunning { target } => process_is_running(target).map(|running| {
                if running {
                    ConditionState::Satisfied
                } else {
                    ConditionState::Pending
                }
            }),
            WaitCondition::CommandExits { command, args } => {
                command_exits(command, args, config, &RunControl::default(), None).map(
                    |succeeded| {
                        if succeeded {
                            ConditionState::Satisfied
                        } else {
                            ConditionState::Pending
                        }
                    },
                )
            }
            _ => ExecutionAdapter::evaluate_wait(self, condition, config),
        }
    }

    fn evaluate_wait_with_control(
        &mut self,
        condition: &WaitCondition,
        config: &RunConfig,
        control: &RunControl,
        timeout: Option<Duration>,
    ) -> Result<ConditionState, AdapterError> {
        match condition {
            WaitCondition::CommandExits { command, args } => {
                command_exits(command, args, config, control, timeout).map(|succeeded| {
                    if succeeded {
                        ConditionState::Satisfied
                    } else {
                        ConditionState::Pending
                    }
                })
            }
            _ => self.evaluate_wait(condition, config),
        }
    }

    fn execute_with_control(
        &mut self,
        action: &Action,
        config: &RunConfig,
        control: &RunControl,
        timeout: Option<Duration>,
    ) -> Result<Vec<Artifact>, AdapterError> {
        match action {
            Action::RunCommand { command, args } => {
                run_command(command, args, config, control, timeout).map(|_| Vec::new())
            }
            _ => self.execute(action, config),
        }
    }

    fn evaluate_assertion(
        &mut self,
        assertion: &Assertion,
        config: &RunConfig,
    ) -> Result<bool, AdapterError> {
        match assertion {
            Assertion::TargetExists { target } if target.accessibility.is_some() => {
                semantic_target_exists(target)
            }
            Assertion::TargetExists { target } => window_exists(target),
            Assertion::Condition { condition } => self
                .evaluate_wait(condition, config)
                .map(|state| state == ConditionState::Satisfied),
        }
    }

    fn target_exists(
        &mut self,
        target: &Target,
        _config: &RunConfig,
    ) -> Result<bool, AdapterError> {
        semantic_target_exists(target)
    }

    fn invoke_target(&mut self, target: &Target, _config: &RunConfig) -> Result<(), AdapterError> {
        with_semantic_target(target, |element| {
            let pattern = unsafe {
                element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
            }
            .map_err(|_| {
                AdapterError::unsupported("target does not support semantic invocation")
            })?;
            unsafe { pattern.Invoke() }
                .map_err(|_| AdapterError::new("Windows could not invoke the requested target"))
        })
    }

    fn set_target_text(
        &mut self,
        target: &Target,
        text: &str,
        _config: &RunConfig,
    ) -> Result<(), AdapterError> {
        with_semantic_target(target, |element| {
            let pattern = unsafe {
                element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            }
            .map_err(|_| {
                AdapterError::unsupported("target does not support semantic text input")
            })?;
            unsafe { pattern.SetValue(&BSTR::from(text)) }
                .map_err(|_| AdapterError::new("Windows could not set the requested target text"))
        })
    }

    fn target_is_focused(
        &mut self,
        target: &Target,
        _config: &RunConfig,
    ) -> Result<bool, AdapterError> {
        with_semantic_target(target, |element| unsafe {
            element
                .CurrentHasKeyboardFocus()
                .map(|focused| focused.as_bool())
                .map_err(|_| AdapterError::new("Windows could read semantic target focus state"))
        })
    }

    fn scroll_target(
        &mut self,
        target: &Target,
        delta_x: i32,
        delta_y: i32,
        _config: &RunConfig,
    ) -> Result<(), AdapterError> {
        with_semantic_target(target, |element| {
            let pattern = unsafe {
                element.GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
            }
            .map_err(|_| AdapterError::unsupported("target does not support semantic scrolling"))?;
            unsafe { pattern.Scroll(scroll_amount(delta_x), scroll_amount(delta_y)) }
                .map_err(|_| AdapterError::new("Windows could not scroll the requested target"))
        })
    }

    fn preflight(&self, action: &Action, config: &RunConfig) -> Vec<PreflightDiagnostic> {
        unsupported_action_reason(action, config)
            .map(|message| {
                vec![PreflightDiagnostic {
                    severity: PreflightSeverity::Error,
                    code: "capability-unavailable".to_string(),
                    message: message.to_string(),
                    step_id: None,
                }]
            })
            .unwrap_or_default()
    }
}

fn send_text(text: &str) -> Result<(), AdapterError> {
    let mut inputs = Vec::with_capacity(text.len() * 2);
    for code_unit in text.encode_utf16() {
        inputs.push(keyboard_input(VIRTUAL_KEY(0), code_unit, KEYEVENTF_UNICODE));
        inputs.push(keyboard_input(
            VIRTUAL_KEY(0),
            code_unit,
            KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
        ));
    }
    send_inputs(&inputs)
}

fn send_key_chord(keys: &str) -> Result<(), AdapterError> {
    let parts = keys.split('+').map(str::trim).collect::<Vec<_>>();
    let (last, modifiers) = parts
        .split_last()
        .ok_or_else(|| AdapterError::new("key chord is required"))?;
    let primary = parse_virtual_key(last)?;
    let modifiers = modifiers
        .iter()
        .map(|modifier| match *modifier {
            "CmdOrControl" | "Control" | "Ctrl" => Ok(VK_CONTROL),
            "Shift" => Ok(VK_SHIFT),
            "Alt" => Ok(VK_MENU),
            "Meta" | "Win" => Ok(VIRTUAL_KEY(0x5B)),
            _ => Err(AdapterError::unsupported(
                "key chord contains an unsupported modifier",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut inputs = Vec::with_capacity(modifiers.len() * 2 + 2);
    inputs.extend(
        modifiers
            .iter()
            .copied()
            .map(|key| keyboard_input(key, 0, Default::default())),
    );
    inputs.push(keyboard_input(primary, 0, Default::default()));
    inputs.push(keyboard_input(primary, 0, KEYEVENTF_KEYUP));
    inputs.extend(
        modifiers
            .iter()
            .rev()
            .copied()
            .map(|key| keyboard_input(key, 0, KEYEVENTF_KEYUP)),
    );
    send_inputs(&inputs)
}

fn send_scroll(delta_y: i32) -> Result<(), AdapterError> {
    if delta_y == 0 {
        return Ok(());
    }
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: MOUSEEVENTF_WHEEL,
                mouseData: delta_y as u32,
                ..Default::default()
            },
        },
    };
    send_inputs(&[input])
}

fn keyboard_input(
    virtual_key: VIRTUAL_KEY,
    scan_code: u16,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS,
) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: virtual_key,
                wScan: scan_code,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

fn send_inputs(inputs: &[INPUT]) -> Result<(), AdapterError> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(AdapterError::new(
            "Windows could not inject the requested input",
        ));
    }
    Ok(())
}

fn parse_virtual_key(key: &str) -> Result<VIRTUAL_KEY, AdapterError> {
    let virtual_key = match key {
        "Enter" => VK_RETURN,
        "Escape" => VK_ESCAPE,
        "Tab" => VK_TAB,
        "Space" => VK_SPACE,
        "Backspace" => VK_BACK,
        "Delete" => VK_DELETE,
        "Home" => VK_HOME,
        "End" => VK_END,
        "ArrowUp" => VK_UP,
        "ArrowDown" => VK_DOWN,
        "ArrowLeft" => VK_LEFT,
        "ArrowRight" => VK_RIGHT,
        key if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() => {
            VIRTUAL_KEY(key.as_bytes()[0].to_ascii_uppercase() as u16)
        }
        _ => {
            return Err(AdapterError::unsupported(
                "key chord contains an unsupported key",
            ));
        }
    };
    Ok(virtual_key)
}

fn shell_open(target: &str) -> Result<Vec<Artifact>, AdapterError> {
    let target = HSTRING::from(target);
    let result = unsafe { ShellExecuteW(None, None, &target, None, None, SW_SHOWNORMAL) };
    if result.0 as isize <= 32 {
        return Err(AdapterError::new(
            "Windows could not open the requested target",
        ));
    }

    Ok(Vec::new())
}

fn focus_window(target: &Target) -> Result<(), AdapterError> {
    let window = find_window(target)?;
    unsafe {
        if !SetForegroundWindow(window).as_bool() {
            return Err(AdapterError::new(
                "Windows could not focus the requested window",
            ));
        }
        if GetForegroundWindow() != window {
            return Err(AdapterError::new(
                "Windows did not foreground the requested window",
            ));
        }
    }
    Ok(())
}

fn window_exists(target: &Target) -> Result<bool, AdapterError> {
    match find_window(target) {
        Ok(_) => Ok(true),
        Err(error) if error.to_string() == "requested window was not found" => Ok(false),
        Err(error) => Err(error),
    }
}

fn window_is_focused(target: &Target) -> Result<bool, AdapterError> {
    let window = match find_window(target) {
        Ok(window) => window,
        Err(error) if error.to_string() == "requested window was not found" => return Ok(false),
        Err(error) => return Err(error),
    };

    Ok(unsafe { GetForegroundWindow() == window })
}

fn process_is_running(target: &Target) -> Result<bool, AdapterError> {
    let process_name = process_name(target)?;
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|_| AdapterError::new("Windows could not enumerate processes"))?;
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(AdapterError::new("Windows could not enumerate processes"));
    }

    let result = (|| {
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if unsafe { Process32FirstW(snapshot, &mut entry) }.is_err() {
            return Ok(false);
        }

        loop {
            if process_entry_name(&entry).eq_ignore_ascii_case(process_name) {
                return Ok(true);
            }
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                return Ok(false);
            }
        }
    })();
    unsafe {
        CloseHandle(snapshot)
            .map_err(|_| AdapterError::new("Windows could close the process snapshot"))?;
    }
    result
}

fn process_name(target: &Target) -> Result<&str, AdapterError> {
    if target.app_name.is_some()
        || target.window_title.is_some()
        || target.title_contains.is_some()
        || target.url.is_some()
        || target.file_path.is_some()
        || target.accessibility.is_some()
        || target.image.is_some()
        || target.coordinates.is_some()
        || !target.platform_selectors.is_empty()
    {
        return Err(AdapterError::unsupported(
            "Windows process queries currently support only a processName selector",
        ));
    }
    target.process_name.as_deref().ok_or_else(|| {
        AdapterError::unsupported("Windows process queries require a processName selector")
    })
}

fn process_entry_name(entry: &PROCESSENTRY32W) -> String {
    let end = entry
        .szExeFile
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(entry.szExeFile.len());
    String::from_utf16_lossy(&entry.szExeFile[..end])
}

fn run_command(
    command: &str,
    args: &[String],
    config: &RunConfig,
    control: &RunControl,
    timeout: Option<Duration>,
) -> Result<(), AdapterError> {
    let status = command_status(command, args, config, control, timeout)?;
    if status.success() {
        Ok(())
    } else {
        Err(AdapterError::new("approved command exited unsuccessfully"))
    }
}

fn command_exits(
    command: &str,
    args: &[String],
    config: &RunConfig,
    control: &RunControl,
    timeout: Option<Duration>,
) -> Result<bool, AdapterError> {
    command_status(command, args, config, control, timeout).map(|status| status.success())
}

fn command_status(
    executable: &str,
    args: &[String],
    config: &RunConfig,
    control: &RunControl,
    timeout: Option<Duration>,
) -> Result<ExitStatus, AdapterError> {
    if !config.approved_commands.contains(executable) {
        return Err(AdapterError::unsupported(
            "command is not approved for this run",
        ));
    }

    let mut command = Command::new(executable);
    command.args(args).envs(&config.environment);
    if let Some(working_directory) = &config.working_directory {
        command.current_dir(working_directory);
    }
    let child = command
        .spawn()
        .map_err(|_| AdapterError::new("Windows could not start the approved command"))?;
    let job = CommandJob::assign(&child)?;
    wait_for_command(child, &job, control, timeout)
}

fn wait_for_command(
    mut child: Child,
    job: &CommandJob,
    control: &RunControl,
    timeout: Option<Duration>,
) -> Result<ExitStatus, AdapterError> {
    let started_at = Instant::now();
    loop {
        if control.is_cancelled() {
            terminate_command(&mut child, job)?;
            return Err(AdapterError::cancelled());
        }
        if timeout.is_some_and(|timeout| started_at.elapsed() >= timeout) {
            terminate_command(&mut child, job)?;
            return Err(AdapterError::timeout());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|_| AdapterError::new("Windows could not observe the approved command"))?
        {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn terminate_command(child: &mut Child, job: &CommandJob) -> Result<(), AdapterError> {
    unsafe { TerminateJobObject(job.handle, 1) }
        .map_err(|_| AdapterError::new("Windows could not stop the approved command tree"))?;
    child
        .wait()
        .map_err(|_| AdapterError::new("Windows could reap the approved command"))?;
    Ok(())
}

struct CommandJob {
    handle: windows::Win32::Foundation::HANDLE,
}

impl CommandJob {
    fn assign(child: &Child) -> Result<Self, AdapterError> {
        let handle = unsafe { CreateJobObjectW(None, None) }
            .map_err(|_| AdapterError::new("Windows could not create a command job"))?;
        let job = Self { handle };
        let process = windows::Win32::Foundation::HANDLE(child.as_raw_handle());
        unsafe { AssignProcessToJobObject(job.handle, process) }
            .map_err(|_| AdapterError::new("Windows could assign the command to its job"))?;
        Ok(job)
    }
}

impl Drop for CommandJob {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

fn find_window(target: &Target) -> Result<HWND, AdapterError> {
    let matcher = WindowTitleMatcher::from_target(target)?;
    let mut search = WindowSearch {
        matcher,
        matches: Vec::new(),
    };
    unsafe {
        EnumWindows(
            Some(enumerate_matching_windows),
            LPARAM((&mut search as *mut WindowSearch<'_>) as isize),
        )
        .map_err(|_| AdapterError::new("Windows could not enumerate top-level windows"))?;
    }

    match search.matches.as_slice() {
        [] => Err(AdapterError::new("requested window was not found")),
        [window] => Ok(*window),
        _ => Err(AdapterError::new(
            "requested window selector matched multiple visible windows",
        )),
    }
}

struct WindowSearch<'a> {
    matcher: WindowTitleMatcher<'a>,
    matches: Vec<HWND>,
}

enum WindowTitleMatcher<'a> {
    Exact(&'a str),
    Contains(&'a str),
}

impl<'a> WindowTitleMatcher<'a> {
    fn from_target(target: &'a Target) -> Result<Self, AdapterError> {
        match (
            target.window_title.as_deref(),
            target.title_contains.as_deref(),
        ) {
            (Some(title), None) => Ok(Self::Exact(title)),
            (None, Some(title)) => Ok(Self::Contains(title)),
            (Some(_), Some(_)) => Err(AdapterError::unsupported(
                "Windows window queries require exactly one of windowTitle or titleContains",
            )),
            (None, None) => Err(AdapterError::unsupported(
                "Windows window queries require windowTitle or titleContains",
            )),
        }
    }

    fn matches(&self, window_title: &str) -> bool {
        let window_title = normalize_window_title(window_title);
        match self {
            Self::Exact(title) => window_title.eq_ignore_ascii_case(&normalize_window_title(title)),
            Self::Contains(title) => window_title
                .to_lowercase()
                .contains(&normalize_window_title(title).to_lowercase()),
        }
    }
}

fn normalize_window_title(title: &str) -> String {
    let mut normalized = String::with_capacity(title.len());
    let mut separator_pending = false;

    for character in title.chars() {
        if character.is_whitespace()
            || matches!(character, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}')
        {
            separator_pending = !normalized.is_empty();
        } else {
            if separator_pending {
                normalized.push(' ');
                separator_pending = false;
            }
            normalized.push(character);
        }
    }

    normalized
}

fn scroll_amount(delta: i32) -> ScrollAmount {
    match delta.cmp(&0) {
        std::cmp::Ordering::Less => ScrollAmount_SmallDecrement,
        std::cmp::Ordering::Equal => ScrollAmount_NoAmount,
        std::cmp::Ordering::Greater => ScrollAmount_SmallIncrement,
    }
}

unsafe extern "system" fn enumerate_matching_windows(window: HWND, lparam: LPARAM) -> BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut WindowSearch<'_>) };
    if unsafe { !IsWindowVisible(window).as_bool() } {
        return BOOL(1);
    }

    if let Ok(title) = window_title(window)
        && search.matcher.matches(&title)
    {
        search.matches.push(window);
    }
    BOOL(1)
}

fn window_title(window: HWND) -> Result<String, AdapterError> {
    let length = unsafe { GetWindowTextLengthW(window) };
    if length == 0 {
        return Ok(String::new());
    }

    let mut buffer = vec![0; length as usize + 1];
    let copied = unsafe { GetWindowTextW(window, &mut buffer) };
    if copied == 0 {
        return Err(AdapterError::new(
            "Windows could not read a visible window title",
        ));
    }
    Ok(String::from_utf16_lossy(&buffer[..copied as usize]))
}

fn semantic_target_exists(target: &Target) -> Result<bool, AdapterError> {
    match with_semantic_target(target, |_| Ok(())) {
        Ok(()) => Ok(true),
        Err(error) if error.to_string() == "requested semantic target was not found" => Ok(false),
        Err(error) => Err(error),
    }
}

fn with_semantic_target<T>(
    target: &Target,
    operation: impl FnOnce(&IUIAutomationElement) -> Result<T, AdapterError>,
) -> Result<T, AdapterError> {
    let accessibility = target.accessibility.as_ref().ok_or_else(|| {
        AdapterError::unsupported("semantic target operations require an accessibility selector")
    })?;
    let window = find_semantic_window(target)?;

    let initialization = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let should_uninitialize = initialization.is_ok();
    if !should_uninitialize && initialization != RPC_E_CHANGED_MODE {
        return Err(AdapterError::new(
            "Windows could not initialize UI Automation",
        ));
    }
    let result = (|| {
        let automation: IUIAutomation = unsafe {
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .map_err(|_| AdapterError::new("Windows could not create a UI Automation client"))?
        };
        let root = unsafe {
            automation
                .ElementFromHandle(window)
                .map_err(|_| AdapterError::new("Windows could not inspect the requested window"))?
        };
        let condition = unsafe {
            automation
                .CreateTrueCondition()
                .map_err(|_| AdapterError::new("Windows could not create a UI Automation query"))?
        };
        let elements = unsafe {
            root.FindAll(TreeScope_Subtree, &condition)
                .map_err(|_| AdapterError::new("Windows could not query the requested target"))?
        };
        let mut matching = Vec::new();
        let count = unsafe {
            elements
                .Length()
                .map_err(|_| AdapterError::new("Windows could not count UI Automation targets"))?
        };
        for index in 0..count {
            let element = unsafe {
                elements
                    .GetElement(index)
                    .map_err(|_| AdapterError::new("Windows could read a UI Automation target"))?
            };
            if accessibility_matches(&element, accessibility)? {
                matching.push(element);
            }
        }

        match matching.as_slice() {
            [] => Err(AdapterError::new("requested semantic target was not found")),
            [element] => operation(element),
            _ => Err(AdapterError::new(
                "requested semantic target matched multiple elements",
            )),
        }
    })();
    if should_uninitialize {
        unsafe {
            CoUninitialize();
        }
    }
    result
}

fn find_semantic_window(target: &Target) -> Result<HWND, AdapterError> {
    let mut window_target = target.clone();
    window_target.accessibility = None;
    find_window(&window_target)
}

fn accessibility_matches(
    element: &IUIAutomationElement,
    accessibility: &cueflow_core::AccessibilityTarget,
) -> Result<bool, AdapterError> {
    if let Some(id) = &accessibility.id {
        let current_id = unsafe {
            element
                .CurrentAutomationId()
                .map_err(|_| AdapterError::new("Windows could read a target automation id"))?
        };
        if current_id != id.as_str() {
            return Ok(false);
        }
    }

    if let Some(name) = &accessibility.name {
        let current_name = unsafe {
            element
                .CurrentName()
                .map_err(|_| AdapterError::new("Windows could read a target name"))?
        };
        if !normalize_window_title(&current_name.to_string())
            .eq_ignore_ascii_case(&normalize_window_title(name))
        {
            return Ok(false);
        }
    }

    if let Some(control_type) = &accessibility.control_type {
        let current_control_type = unsafe {
            element
                .CurrentLocalizedControlType()
                .map_err(|_| AdapterError::new("Windows could read a target control type"))?
        };
        if !normalize_window_title(&current_control_type.to_string())
            .eq_ignore_ascii_case(&normalize_window_title(control_type))
        {
            return Ok(false);
        }
    }

    Ok(true)
}

fn unsupported_action_reason(action: &Action, config: &RunConfig) -> Option<&'static str> {
    match action {
        Action::FocusWindow { target } => unsupported_window_target_reason(target),
        Action::ClickTarget { target }
        | Action::TypeText {
            target: Some(target),
            ..
        } => unsupported_semantic_target_reason(target),
        Action::PressKey {
            target: Some(_), ..
        } => Some("targeted key chords are not yet supported by the Windows UI Automation adapter"),
        Action::Scroll {
            target: Some(target),
            ..
        } => unsupported_semantic_target_reason(target),
        Action::RunCommand { command, .. } => unsupported_command_reason(command, config),
        Action::WaitFor { condition } => unsupported_wait_reason(condition, config),
        Action::Assert { assertion } => match assertion {
            Assertion::TargetExists { target } => unsupported_window_target_reason(target),
            Assertion::Condition { condition } => unsupported_wait_reason(condition, config),
        },
        _ => None,
    }
}

fn unsupported_wait_reason(condition: &WaitCondition, config: &RunConfig) -> Option<&'static str> {
    match condition {
        WaitCondition::WindowExists { target } => {
            if target.accessibility.is_some() {
                unsupported_semantic_target_reason(target)
            } else {
                unsupported_window_target_reason(target)
            }
        }
        WaitCondition::WindowFocused { target } if target.accessibility.is_some() => {
            unsupported_semantic_target_reason(target)
        }
        WaitCondition::WindowFocused { target } => unsupported_window_target_reason(target),
        WaitCondition::ProcessRunning { target } => unsupported_process_target_reason(target),
        WaitCondition::CommandExits { command, .. } => unsupported_command_reason(command, config),
        _ => None,
    }
}

fn unsupported_semantic_target_reason(target: &Target) -> Option<&'static str> {
    if target.accessibility.is_none() {
        return Some("semantic target operations require an accessibility selector");
    }

    let mut window_target = target.clone();
    window_target.accessibility = None;
    unsupported_window_target_reason(&window_target)
}

fn unsupported_window_target_reason(target: &Target) -> Option<&'static str> {
    if target.app_name.is_some()
        || target.process_name.is_some()
        || target.url.is_some()
        || target.file_path.is_some()
        || target.image.is_some()
        || target.coordinates.is_some()
        || !target.platform_selectors.is_empty()
    {
        return Some(
            "Windows window queries currently support only a windowTitle or titleContains selector",
        );
    }
    if target.window_title.is_some() && target.title_contains.is_some() {
        return Some("Windows window queries require exactly one of windowTitle or titleContains");
    }
    if target.window_title.is_none() && target.title_contains.is_none() {
        return Some("Windows window queries require windowTitle or titleContains");
    }

    None
}

fn unsupported_process_target_reason(target: &Target) -> Option<&'static str> {
    process_name(target).err().map(|error| {
        if error.to_string().contains("require a processName") {
            "Windows process queries require a processName selector"
        } else {
            "Windows process queries currently support only a processName selector"
        }
    })
}

fn unsupported_command_reason(command: &str, config: &RunConfig) -> Option<&'static str> {
    (!config.approved_commands.contains(command)).then_some("command is not approved for this run")
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use cueflow_core::{AccessibilityTarget, PlatformSelector};
    use cueflow_executor::ExecutionAdapter;

    use super::*;

    #[test]
    fn windows_capabilities_expose_supported_and_gated_features() {
        let capabilities = WindowsDesktopAdapter::capabilities();

        assert_eq!(capabilities.platform, Platform::Windows);
        assert!(capabilities.supports_launch);
        assert!(capabilities.supports_focus);
        assert!(capabilities.supports_input);
        assert!(capabilities.supports_semantic_targets);
        assert!(capabilities.supports_process_queries);
    }

    #[test]
    fn preflight_rejects_unscoped_accessibility_and_non_title_window_selectors() {
        let adapter = WindowsDesktopAdapter;
        let accessibility_target = Target {
            app_name: None,
            process_name: None,
            window_title: None,
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: Some(AccessibilityTarget {
                id: Some("submit".to_string()),
                name: None,
                control_type: None,
            }),
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        };
        let accessibility_diagnostics = adapter.preflight(
            &Action::ClickTarget {
                target: accessibility_target,
            },
            &RunConfig::default(),
        );
        assert_eq!(accessibility_diagnostics.len(), 1);

        let mut partial_target = Target::app("Browser");
        partial_target.window_title = Some("Demo".to_string());
        let selector_diagnostics = adapter.preflight(
            &Action::FocusWindow {
                target: partial_target,
            },
            &RunConfig::default(),
        );
        assert_eq!(selector_diagnostics.len(), 1);
    }

    #[test]
    fn preflight_accepts_scoped_accessibility_targets() {
        let adapter = WindowsDesktopAdapter;
        let target = Target {
            app_name: None,
            process_name: None,
            window_title: Some("Settings".to_string()),
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: Some(AccessibilityTarget {
                id: Some("submit".to_string()),
                name: Some("Submit".to_string()),
                control_type: Some("button".to_string()),
            }),
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        };

        assert!(
            adapter
                .preflight(
                    &Action::ClickTarget {
                        target: target.clone(),
                    },
                    &RunConfig::default(),
                )
                .is_empty()
        );
        assert_eq!(
            adapter
                .preflight(
                    &Action::WaitFor {
                        condition: WaitCondition::WindowFocused { target },
                    },
                    &RunConfig::default(),
                )
                .len(),
            0
        );
    }

    #[test]
    fn process_queries_require_an_exact_process_name_selector() {
        let adapter = WindowsDesktopAdapter;
        let valid_target = Target {
            app_name: None,
            process_name: Some("msedge.exe".to_string()),
            window_title: None,
            title_contains: None,
            url: None,
            file_path: None,
            accessibility: None,
            image: None,
            coordinates: None,
            platform_selectors: BTreeMap::new(),
        };
        let valid_action = Action::WaitFor {
            condition: WaitCondition::ProcessRunning {
                target: valid_target.clone(),
            },
        };
        assert!(
            adapter
                .preflight(&valid_action, &RunConfig::default())
                .is_empty()
        );

        let mut invalid_target = valid_target;
        invalid_target.window_title = Some("Edge".to_string());
        let invalid_action = Action::WaitFor {
            condition: WaitCondition::ProcessRunning {
                target: invalid_target,
            },
        };
        assert_eq!(
            adapter
                .preflight(&invalid_action, &RunConfig::default())
                .len(),
            1
        );
    }

    #[test]
    fn commands_require_explicit_approval_and_run_without_shell_interpolation() {
        let mut adapter = WindowsDesktopAdapter;
        let action = Action::RunCommand {
            command: "cmd.exe".to_string(),
            args: vec!["/c".to_string(), "exit".to_string(), "0".to_string()],
        };
        let unapproved = RunConfig::default();
        assert_eq!(adapter.preflight(&action, &unapproved).len(), 1);

        let approved = RunConfig {
            dry_run: false,
            approved_commands: BTreeSet::from(["cmd.exe".to_string()]),
            ..RunConfig::default()
        };
        assert!(adapter.preflight(&action, &approved).is_empty());
        adapter
            .execute(&action, &approved)
            .expect("approved command succeeds");
        assert_eq!(
            adapter
                .evaluate_wait(
                    &WaitCondition::CommandExits {
                        command: "cmd.exe".to_string(),
                        args: vec!["/c".to_string(), "exit".to_string(), "0".to_string()],
                    },
                    &approved,
                )
                .expect("approved command condition evaluates"),
            ConditionState::Satisfied
        );
    }

    #[test]
    fn window_title_matchers_are_case_insensitive() {
        assert!(
            WindowTitleMatcher::Exact("Google - Microsoft Edge").matches("google - microsoft edge")
        );
        assert!(WindowTitleMatcher::Contains("Microsoft Edge").matches("Google - Microsoft Edge"));
        assert!(
            WindowTitleMatcher::Contains("Microsoft Edge")
                .matches("New tab - Microsoft\u{200B} Edge")
        );
        assert!(!WindowTitleMatcher::Contains("Firefox").matches("Google - Microsoft Edge"));
    }

    #[test]
    fn preflight_accepts_a_title_contains_window_selector() {
        let adapter = WindowsDesktopAdapter;
        let action = Action::FocusWindow {
            target: Target {
                app_name: None,
                process_name: None,
                window_title: None,
                title_contains: Some("Microsoft Edge".to_string()),
                url: None,
                file_path: None,
                accessibility: None,
                image: None,
                coordinates: None,
                platform_selectors: BTreeMap::new(),
            },
        };

        assert!(adapter.preflight(&action, &RunConfig::default()).is_empty());
    }

    #[test]
    fn preflight_accepts_a_resolved_windows_window_title_selector() {
        let adapter = WindowsDesktopAdapter;
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
        }
        .for_platform(Some(Platform::Windows));

        assert!(adapter.preflight(&action, &RunConfig::default()).is_empty());
    }
}
