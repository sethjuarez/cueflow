# Cueflow

Cueflow is a Rust-first automation foundation for portable, demo-ready desktop workflows.

The core primitive is an intentional automation workflow: a reusable definition that can put a demo environment into a known state. Recording can be an authoring layer later, but recorded macros are not the center of the product.

## Workspace layout

| Crate | Purpose |
| --- | --- |
| `cueflow-core` | OS-agnostic automation schema, validation, serialization, run state, errors, artifacts, and portability analysis. |
| `cueflow-executor` | Run orchestration, dry-run/no-op execution, retry/stop policy handling, run events, and structured tracing fields. |
| `cueflow-adapters` | Platform adapter boundary plus a no-op/current-platform stub. Real Windows, macOS, and Linux adapters are follow-up work. |
| `cueflow-recorder` | Optional capture-to-DSL surface. It intentionally emits `AutomationDefinition` rather than a separate macro format. |
| `cueflow-tauri` | Optional host bridge that executes automation requests with run control and structured event forwarding. |

## DSL sketch

Definitions are portable by default and can add platform-specific overrides only where necessary.

```json
{
  "id": "demo-ready",
  "title": "Prepare demo",
  "schemaVersion": 1,
  "steps": [
    {
      "id": "open-cueflow",
      "kind": "launchUrl",
      "url": "https://cueflow.dev"
    },
    {
      "id": "focus-browser",
      "kind": "focusWindow",
      "target": {
        "appName": "Browser",
        "titleContains": "Cueflow"
      }
    },
    {
      "id": "open-command-palette",
      "kind": "pressKey",
      "keys": "CmdOrControl+Shift+P"
    }
  ]
}
```

Prefer semantic actions such as `launchUrl`, `launchApp`, `focusWindow`, `typeText`, `pressKey`, `clickTarget`, `scroll`, `waitFor`, `assert`, `runCommand`, and `openFile`. Avoid saving platform shell commands, raw scan codes, or absolute coordinates as the happy path.

## Portability model

Cueflow analyzes definitions into these categories:

- `portable`
- `hasPlatformOverrides`
- `windowsOnly`
- `macOsOnly`
- `linuxOnly`

Targets should use logical descriptors first: app name, process name, window title/title contains, URL, file path, accessibility descriptors, image targets, and coordinates only as a last resort.

## Contract compatibility

`cueflow-core` exposes `automation_definition_schema()` for generated JSON Schema and `parse_definition_json()` as the version-validation boundary for persisted definitions. The schema version is required. Unsupported versions fail explicitly until a migration is provided; persisted workflow JSON must not rely on undocumented Rust implementation details.

## Variables and secrets

Definitions declare typed variables with optional literal defaults or a named `secretReference`. Run configuration values override defaults. `resolve_run()` resolves `${variable}` references in step fields, working directories, and environment values; recursive references are supported and cycles fail explicitly. Callers supply secret material through the `SecretResolver` trait. Resolved secret variables provide redacted values for logs and diagnostics, and Cueflow does not emit resolved variable values in run events.

## Command policy

`runCommand` and `commandExits` execute a command directly without shell interpolation. They are disabled unless the host supplies the exact executable name in `RunConfig.approved_commands`; the configured working directory and environment are applied. Command processes are terminated when the step reaches its timeout or the host cancels the run. Hosts should approve only idempotent commands for `commandExits`, because the condition can be evaluated repeatedly while waiting.

## Observability

Runs emit structured `RunEvent` values that apps can map into traces, logs, timelines, or artifacts. The executor also uses `tracing` fields for `automation_id`, `run_id`, `step_id`, `step_kind`, `target`, and `error`. Cueflow stays standalone and does not require Auditaur or any other tracing backend.

## Platform support

Windows is the first execution target. The Windows adapter uses native shell, window, and input APIs for application/URL/file launch, case-insensitive exact or fragment title window focus, Unicode typing, normalized key chords, and wheel scrolling. Window title selectors must resolve to exactly one visible top-level window, and focus is verified immediately after activation.

Windows UI Automation provides the semantic path for `clickTarget`, targeted `typeText` and `scroll`, `windowExists` and `windowFocused`, and `targetExists` assertions. A semantic target must combine a unique window title selector with an `accessibility` selector (`id`, `name`, and/or `controlType`) that resolves to exactly one element; invocation, value assignment, scrolling, and focus inspection do not require foreground activation. Targeted key chords remain preflight-gated until their UI Automation equivalent is implemented. macOS concepts remain in the portable schema and adapter capability boundary for a later native implementation.

Windows also supports `processRunning` for an exact `processName` selector (for example, `msedge.exe`). Other target fields are rejected for process checks until process-path and application-identity matching are implemented.

## Confidence

Windows execution confidence is currently 8/10; overall project confidence is 7/10. The workspace test suite passes, and the Edge-to-Google workflow has completed successfully on Windows after platform selection, condition gating, and platform selector resolution were covered by regression tests.

The remaining risk is execution breadth rather than a known correctness failure: live mutating UIA actions have not yet been run against a real application, and validation has been limited to this Windows environment and browser workflow.

## Development status

Cueflow is in active development. Windows is the first supported execution target, with fail-closed window selection, scoped UI Automation actions, process readiness checks, and host-approved command execution. macOS and Linux retain the same portable contract but currently reject real execution. Native recording, macOS Accessibility support, and broader live application coverage remain planned work.

## Development

```powershell
cargo test --workspace
```

`examples/edge-demo-ready.json` is the first idempotent Windows reliability fixture. It returns Edge to google.com using only portable actions; run it through a Cueflow host after confirming it is safe to foreground and type into Edge.
