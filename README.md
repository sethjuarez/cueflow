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
| `cueflow-tauri` | Optional thin bridge shape for apps that submit automation run requests from a frontend. |

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

## Observability

Runs emit structured `RunEvent` values that apps can map into traces, logs, timelines, or artifacts. The executor also uses `tracing` fields for `automation_id`, `run_id`, `step_id`, `step_kind`, `target`, and `error`. Cueflow stays standalone and does not require Auditaur or any other tracing backend.

## Platform support

Windows is the first execution target. The Windows adapter uses native shell, window, and input APIs for application/URL/file launch, exact-title window focus, Unicode typing, normalized key chords, and wheel scrolling. Accessibility selectors and semantic click targets remain explicitly preflight-gated until a Windows UI Automation layer is added. macOS concepts remain in the portable schema and adapter capability boundary for a later native implementation.

## Development

```powershell
cargo test --workspace
```

`examples/edge-demo-ready.json` is the first idempotent Windows reliability fixture. It returns Edge to cueflow.dev using only portable actions; run it through a Cueflow host after confirming it is safe to foreground and type into Edge.
