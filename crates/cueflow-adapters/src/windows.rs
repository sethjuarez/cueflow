use std::path::Path;
use std::process::Command;

use crate::AdapterCapabilities;
use cueflow_core::{
    Action, Artifact, Assertion, Platform, PreflightDiagnostic, PreflightSeverity, RunConfig,
    Target, WaitCondition,
};
use cueflow_executor::{AdapterError, ConditionState, ExecutionAdapter};
use windows::{
    Win32::UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
            KEYEVENTF_UNICODE, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_BACK,
            VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_MENU,
            VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
        },
        Shell::ShellExecuteW,
        WindowsAndMessaging::{
            FindWindowW, GetForegroundWindow, SW_SHOWNORMAL, SetForegroundWindow,
        },
    },
    core::HSTRING,
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
            supports_window_queries: true,
            supports_process_queries: false,
        }
    }
}

impl ExecutionAdapter for WindowsDesktopAdapter {
    fn execute(
        &mut self,
        action: &Action,
        _config: &RunConfig,
    ) -> Result<Vec<Artifact>, AdapterError> {
        match action {
            Action::LaunchUrl { url, .. } => shell_open(url),
            Action::LaunchApp { app, .. } => Command::new(app)
                .spawn()
                .map(|_| Vec::new())
                .map_err(|_| AdapterError::new("failed to launch application")),
            Action::FocusWindow { target } => focus_window(target).map(|_| Vec::new()),
            Action::TypeText { text, .. } => send_text(text).map(|_| Vec::new()),
            Action::PressKey { keys, .. } => send_key_chord(keys).map(|_| Vec::new()),
            Action::Scroll { delta_y, .. } => send_scroll(*delta_y).map(|_| Vec::new()),
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
            WaitCondition::WindowExists { target } => window_exists(target).map(|exists| {
                if exists {
                    ConditionState::Satisfied
                } else {
                    ConditionState::Pending
                }
            }),
            WaitCondition::WindowFocused { target } => window_is_focused(target).map(|focused| {
                if focused {
                    ConditionState::Satisfied
                } else {
                    ConditionState::Pending
                }
            }),
            _ => ExecutionAdapter::evaluate_wait(self, condition, config),
        }
    }

    fn evaluate_assertion(
        &mut self,
        assertion: &Assertion,
        config: &RunConfig,
    ) -> Result<bool, AdapterError> {
        match assertion {
            Assertion::TargetExists { target } => window_exists(target),
            Assertion::Condition { condition } => self
                .evaluate_wait(condition, config)
                .map(|state| state == ConditionState::Satisfied),
        }
    }

    fn preflight(&self, action: &Action, _config: &RunConfig) -> Vec<PreflightDiagnostic> {
        unsupported_action_reason(action)
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

fn find_window(target: &Target) -> Result<windows::Win32::Foundation::HWND, AdapterError> {
    let title = target.window_title.as_ref().ok_or_else(|| {
        AdapterError::unsupported("Windows title-based lookup requires windowTitle")
    })?;
    let title = HSTRING::from(title);
    unsafe {
        FindWindowW(None, &title).map_err(|_| AdapterError::new("requested window was not found"))
    }
}

fn unsupported_action_reason(action: &Action) -> Option<&'static str> {
    match action {
        Action::FocusWindow { target } => unsupported_window_target_reason(target),
        Action::ClickTarget { .. } => {
            Some("semantic click targets require Windows UI Automation support")
        }
        Action::TypeText {
            target: Some(_), ..
        }
        | Action::PressKey {
            target: Some(_), ..
        }
        | Action::Scroll {
            target: Some(_), ..
        } => Some("targeted input requires Windows UI Automation support"),
        Action::WaitFor { condition } => unsupported_wait_reason(condition),
        Action::Assert { assertion } => match assertion {
            Assertion::TargetExists { target } => unsupported_window_target_reason(target),
            Assertion::Condition { condition } => unsupported_wait_reason(condition),
        },
        _ => None,
    }
}

fn unsupported_wait_reason(condition: &WaitCondition) -> Option<&'static str> {
    match condition {
        WaitCondition::WindowExists { target } | WaitCondition::WindowFocused { target } => {
            unsupported_window_target_reason(target)
        }
        _ => None,
    }
}

fn unsupported_window_target_reason(target: &Target) -> Option<&'static str> {
    if target.accessibility.is_some() {
        return Some("accessibility selectors require Windows UI Automation support");
    }
    if target.app_name.is_some()
        || target.process_name.is_some()
        || target.title_contains.is_some()
        || target.url.is_some()
        || target.file_path.is_some()
        || target.image.is_some()
        || target.coordinates.is_some()
        || !target.platform_selectors.is_empty()
    {
        return Some("Windows window queries currently support only an exact windowTitle selector");
    }
    if target.window_title.is_none() {
        return Some("Windows window queries require an exact windowTitle selector");
    }

    None
}

#[cfg(test)]
mod tests {
    use cueflow_core::AccessibilityTarget;

    use super::*;

    #[test]
    fn windows_capabilities_expose_supported_and_gated_features() {
        let capabilities = WindowsDesktopAdapter::capabilities();

        assert_eq!(capabilities.platform, Platform::Windows);
        assert!(capabilities.supports_launch);
        assert!(capabilities.supports_focus);
        assert!(capabilities.supports_input);
    }

    #[test]
    fn preflight_rejects_unsupported_accessibility_and_partial_window_selectors() {
        let adapter = WindowsDesktopAdapter;
        let mut accessibility_target = Target::app("Browser");
        accessibility_target.window_title = Some("Demo".to_string());
        accessibility_target.accessibility = Some(AccessibilityTarget {
            id: Some("submit".to_string()),
            name: None,
            control_type: None,
        });
        let accessibility_diagnostics = adapter.preflight(
            &Action::WaitFor {
                condition: WaitCondition::WindowExists {
                    target: accessibility_target,
                },
            },
            &RunConfig::default(),
        );
        assert_eq!(accessibility_diagnostics.len(), 1);

        let mut partial_target = Target::app("Browser");
        partial_target.window_title = Some("Demo".to_string());
        partial_target.title_contains = Some("Demo".to_string());
        let selector_diagnostics = adapter.preflight(
            &Action::FocusWindow {
                target: partial_target,
            },
            &RunConfig::default(),
        );
        assert_eq!(selector_diagnostics.len(), 1);
    }
}
