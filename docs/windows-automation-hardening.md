# Windows automation hardening

This document summarizes the Windows automation foundation hardened so far. The current scope intentionally excludes macOS parity and recorder/visual authoring.

## Foundation

Cueflow's Windows runtime is built around semantic automation first. Portable definitions flow through `cueflow-core` validation, `cueflow-executor` preflight, and the Windows adapter. Host-facing surfaces, including the CLI and Tauri bridge, must enter through the executor path so validation and policy checks happen before side effects.

The Windows adapter now includes:

- Native launch, focus, input, wait, assertion, process, file, and window operations.
- UI Automation inspection with bounded depth/node limits and value redaction by default.
- Semantic target readiness checks for enabled, visible, focused, actionable, disabled, offscreen, timeout, ambiguity, and not-found cases.
- Window identity diagnostics including title, class, process, bounds, foreground state, minimized state, and owner.
- Selector candidate and repair generation with confidence, warnings, rationale, and change summaries.

## Policy gates

Fragile or sensitive behavior is fail-closed unless explicitly approved in `RunConfig`:

- Coordinate targets require `allowCoordinateTargets`.
- Path-only accessibility selectors require `allowPathOnlySelectors`.
- Runtime value capture requires `allowValueCapture`.
- Step screenshot evidence requires `allowScreenshotCapture`.
- Image targets require `allowImageTargets` and, for matching, `allowScreenshotCapture`.
- Full desktop screenshots remain a separate explicit CLI approval.

Named CLI/drill policy profiles make common approval sets explicit:

| Profile | Purpose |
| --- | --- |
| `strict` | Default fail-closed behavior. |
| `evidence` | Enables step evidence capture. |
| `visual-fallback` | Enables step evidence plus screenshot/image fallback approvals. |
| `unsafe-lab` | Enables all fragile/sensitive approvals for controlled experiments. |

Profiles only add approvals. Explicit flags remain additive and cannot weaken policy.

## Visual fallback

Windows image targets now support deterministic fallback for target existence, target absence, assertions, and click-target image routes. Matching uses uncompressed 32bpp BMP templates, normalizes top-down and bottom-up BMPs, compares RGB channels only, supports confidence thresholds and optional regions, and enforces a search budget before scanning pixels.

Visual fallback is intentionally not the happy path. It is bounded, explicit-policy, and diagnostic-oriented so semantic selectors remain the primary automation model.

## Evidence and diagnostics

Evidence bundles include JSONL run events and a summary file. Step evidence prefers target-scoped accessibility trees and window-scoped screenshots. Evidence remains local by default, records retention/redaction policy, supports Cueflow-generated pruning after successful preflight, and enforces max artifact sizes.

Run and drill output now include structured diagnostics:

- Typed failure kinds from adapter/executor failures.
- Terminal `failureSummary` with step id, error kind, failure kind, message, and source.
- Evidence prune reporting.
- Per-attempt duration and optional duration ceilings for deterministic drills.

## Drill coverage

The Windows drill manifest is the live regression boundary. It covers:

- Repeated Settings actionability/readiness success.
- Missing-window timeout failure with typed timeout assertion.
- Image target, path-only selector, and coordinate policy denials.
- Evidence artifact size warning behavior.
- Duration ceilings for deterministic timeout and preflight-policy paths.

The live drill command is:

```powershell
cargo run -p cueflow-cli -- run-drills examples\windows-drill-manifest.json
```

The full workspace validation command is:

```powershell
cargo test --workspace
```

## Current status

Absent macOS and recorder/visual authoring, the foundational Windows runtime has validation, policy gates, deterministic visual fallback, evidence lifecycle reporting, failure taxonomy, selector repair explainability, host gateway protection, and live regression drills in place.
