# macOS automation pass

This document is the handoff guide for implementing macOS automation parity. The goal is to match the hardened Windows runtime contract, not to build recorder/visual authoring.

## Scope

Do this pass:

- Implement a real macOS adapter behind the existing `ExecutionAdapter` contract.
- Preserve the executor path: validate, preflight, then execute.
- Enforce the same policy gates before side effects.
- Add macOS-specific diagnostics, evidence, and live drills.
- Validate with targeted tests, full workspace tests, live drills, and rubber-duck review.

Do not do this pass:

- Do not add recorder/visual authoring.
- Do not introduce a second replay format.
- Do not bypass `AutomationExecutor` from host/Tauri code.
- Do not make coordinates, screenshots, OCR, or image matching the default path.

## Implementation order

1. Create the macOS adapter skeleton under `crates/cueflow-adapters`.
   - Use `cfg(target_os = "macos")`.
   - Keep non-macOS builds compiling with the existing no-op/current-platform behavior.
   - Expose macOS capabilities truthfully; unsupported features should preflight-fail clearly.

2. Add native window/app basics.
   - Launch URLs/apps/files through macOS-native APIs.
   - Enumerate windows with stable diagnostics: title, app/process name, pid, bounds, focused/frontmost state, minimized/hidden state when available, and owner/parent/modal hints when available.
   - Implement exact title and title-contains matching.
   - Return typed not-found and ambiguous failures with candidate diagnostics.

3. Add accessibility inspection with AXUIElement.
   - Use the macOS Accessibility API (`AXUIElement`) as the semantic foundation.
   - Build bounded accessibility trees with max depth and max nodes.
   - Include role, subrole, title/name, identifier when available, enabled/focused/value/actionability signals, bounds, click point, actions, and child paths.
   - Omit values by default; only capture values with `allowValueCapture` or the existing explicit inspection value opt-in.
   - Detect and report missing Accessibility permission as a typed/preflight-visible capability problem.

4. Implement semantic operations.
   - Target lookup by window plus accessibility selector.
   - Invoke/click semantic target using AX actions where possible.
   - Set text/value using AX value APIs where possible.
   - Focus windows and semantic targets.
   - Send text/key/scroll input only through approved, platform-appropriate APIs.
   - Add readiness checks for exists, focused, enabled, visible, actionable, name contains, and value contains.

5. Preserve policy gates.
   - Path-only selectors require `allowPathOnlySelectors`.
   - Coordinates require `allowCoordinateTargets`.
   - Runtime values require `allowValueCapture`.
   - Screenshot evidence requires `allowScreenshotCapture`.
   - Image targets require `allowImageTargets` and screenshot approval for matching.
   - Full-desktop screenshots must remain a separate explicit approval.
   - Execution-time methods must re-check policy before side effects, even when preflight already ran.

6. Add evidence support.
   - Emit target-scoped accessibility tree evidence before/after steps when enabled.
   - Prefer window-scoped screenshots; do not capture full desktop by default.
   - Enforce max artifact size before writing.
   - Preserve the evidence summary shape used by Windows: retention policy, redaction policy, artifacts, failure summary, and prune reporting.

7. Add visual fallback only after semantic basics are stable.
   - Reuse the existing image target contract.
   - Keep matching deterministic, bounded, and explicit-policy only.
   - Prefer bounded regions and enforce a search budget before pixel scanning.
   - Treat image matching as fallback, not the authoring/happy path.

8. Add selector repair parity.
   - Generate candidate selectors from fresh bounded AX trees.
   - Prefer stable identifiers/roles before titles and paths.
   - Include confidence, score, rationale, warnings, and change summaries.
   - Warn when relying on path-only or localized text.

## Failure taxonomy expectations

Map macOS failures into the existing typed model wherever possible:

| Situation | Expected kind |
| --- | --- |
| Window/app/target absent | `notFound` |
| Multiple matching windows or elements | `ambiguous` |
| Target disabled | `disabled` |
| Target hidden/offscreen/empty bounds | `offscreen` |
| Focus denied/frontmost app refused | `focusDenied` |
| Accessibility permission missing | `capabilityUnavailable` or equivalent preflight diagnostic |
| Automation timeout | `timeout` |
| OS/transient API failure | `transient` |
| Policy approval missing | `policyDenied` |

Every adapter error that can reach users should include useful source/diagnostic text. Avoid broad catch-all failures that lose the underlying reason.

## macOS drills

Create a macOS drill manifest alongside the Windows manifest, for example:

```text
examples/macos-drill-manifest.json
```

The first stable live drill target should be a built-in app with predictable accessibility, such as System Settings or TextEdit. Prefer drills that are deterministic on a clean macOS machine.

Minimum drill coverage:

- A successful semantic actionability/readiness drill, repeated at least twice.
- A missing-window timeout drill with `expectedFailureKind: "timeout"`.
- A path-only selector policy denial.
- A coordinate target policy denial.
- An image target policy denial.
- An evidence-size warning/success drill.
- A permission/capability diagnostic drill if Accessibility permission is unavailable or intentionally denied.

Use the same manifest assertion fields as Windows:

- `expectedOutcome`
- `expectedFailureKind`
- `expectedErrorContains`
- `expectedLogContains`
- `expectedMaxDurationMillis`
- `repeat`
- `policyProfile`

## Validation loop

Use the same confidence loop as Windows:

1. Implement one foundational piece.
2. Run targeted tests for that piece.
3. Run `cargo test --workspace`.
4. Run the macOS live drill manifest.
5. Run a blocking-only rubber-duck review.
6. Ask: absent recording and non-macOS work, what remains to be best-in-class?
7. Continue until rubber duck says there is nothing foundational left in scope.

Before launching a live validation app, close any existing instance of that app so drills start from a known state.

## Acceptance criteria

The macOS pass is complete when:

- The macOS adapter supports the same core semantic runtime contract as Windows.
- Policy gates are enforced in both preflight and execution-time methods.
- Accessibility inspection is bounded, redacted by default, and useful for selector repair.
- Evidence is local, bounded, prunable, and summary-compatible with Windows.
- Failure kinds and diagnostics are typed and actionable.
- Live macOS drills pass repeatedly.
- `cargo test --workspace` passes.
- A final rubber-duck review finds no blocking foundational runtime gaps, excluding recorder/visual authoring.

## Current validation status

The macOS adapter implementation now covers the Windows adapter contract surfaces behind the same executor path: native launch/window/process/input/screenshot APIs, AXUIElement semantic inspection/actions/readiness checks, evidence, selector repair, command policy, coordinate policy, value-capture policy, and bounded image fallback behind image + screenshot approvals.

Validated in this environment:

- `cargo test --workspace`.
- `examples/macos-drill-manifest.json` for launch/window readiness, timeout taxonomy, path-only/coordinate/image policy denial, evidence, and Accessibility-permission diagnostics.
- Adapter unit tests for approved image preflight, bounded template matching, coordinate/path-only policy, process selector rejection, and capability shape.
- `examples/macos-semantic-drill-manifest.json` after granting Accessibility permission, validating live AX semantic actionability success twice.

Still not live-validated in this environment:

- Windows live runtime behavior and Windows drill execution, because this is not a Windows host.

Before claiming fully validated cross-platform parity, run the Windows tests and `examples/windows-drill-manifest.json` on a Windows host. From macOS, the available Windows-side signal is cross-target compilation, not live runtime validation.

## Notes for future implementers

- Prefer semantic AX operations over synthesized input whenever possible.
- Treat macOS Accessibility permission as a first-class preflight/capability concern.
- Keep selectors portable at the schema layer; macOS-specific details belong behind platform selectors or adapter diagnostics.
- Do not silently ignore selector constraints. If a target includes fields the macOS adapter cannot honor, preflight-fail before side effects.
- Keep visual fallback behind policy and bounded regions. It should help recover from missing semantic affordances, not replace semantic automation.
