# Failtrace Output Schema — v1.6

This document is the source of truth for the JSON shape that `failtrace run`, `failtrace analyze`, and `failtrace last` emit on stdout. The `failtrace explain` and `failtrace adapters` commands emit related but distinct document shapes — defined in "Other document kinds" below.

`schema_version: "1.6"` is the contract. Breaking changes increment the major version. Non-breaking additions (new optional fields, new enum members marked `// added in 1.x`) increment the minor version.

> **v1.6 changelog vs v1.5.** Makes `parse_status` a contract-health signal instead of a synonym for "any parse warning exists." `complete` documents MAY carry advisory `parse_warnings`; `partial` is reserved for contract-affecting parser degradation. Adds `runner_outcome: "typecheck_failed"` for typecheck-only runners whose typecheck failed. Narrows `pre_test_failed` to test runners/build harnesses that failed before any test item began. Expands `runner_summary_mismatch` to cover partially extracted failure lists when a completed runner summary, structured terminal stream, or recognized run-level failure marker reports more comparable failure units than the adapter accounted for; adapter-specific irreconcilable multi-summary streams fail closed as `unparsed_output`. Comparable units are adapter-defined because one failed test may legitimately emit multiple failure objects; public test counts remain derived from extracted failure identities under invariant 11. `failure_id` and `test_group_id` formulas are unchanged.

> **v1.5 changelog vs v1.4.** Adds composite-command candidate recording, structured sidecar `argv` and `adapter_candidates`, `Location.in_user_code`, `Failure.runner_package_id`, `Failure.test_segments`, `Excerpt.source`, `Phase::Typecheck`, a split parse-status surface (`unparsed_output` vs `unsupported_command`), and the parse-warning surface used by gotest, tsc, and composite-command deferral. Composite candidate detection ships in v1.5, but attributing captured output back to one segment is deferred; multi-candidate composites emit `unsupported_command` with `composite_runtime_deferred`. `UnknownDiagnosticCode` is renamed to `UnstructuredDiagnostic`. Invariants 7, 8, 11, 12, and 13 are hardened to match the validator. The failure-id and test-group-id formulas are unchanged; `Location.in_user_code` is not part of either hash input.

> **v1.4 changelog vs v1.3.** Adds required top-level `emission` metadata to `run_result` and `analysis_result` documents. `output.json` is now the full source of truth, while stdout MAY emit a budgeted `failures[]` projection for agent runtime display limits. Also refreshes illustrative examples: pytest collection error exit code `4`, example `schema_version: "1.5"`, and parameterized pytest line `41`.
>
> *Correction made before the v1.4 release:* invariant 11's implicit-outcome rule for empty `retries[]` is phase- and kind-derived rather than always `failed` — see invariant 11 prose below. Invariant 13 was strengthened from "exactly one `selected` when `failures` is non-empty" to "exactly one `selected` when `parse_status` is `complete` or `partial`," matching the composite-commands section's load-bearing intent.
>
> *No formula or fixture hash changes.* `failure_id` and `test_group_id` derivations are unchanged from v1.2; all normative hashes remain valid.

> **v1.3 changelog vs v1.2.** Contract-text corrections after implementation review. No new architecture. Five contract-level bugs, five cleanups, five judgment-call clarifications.
>
> *Bugs fixed:* (1) Flaky-test contradiction — failures[] now means "currently failing only"; prose about a flake-that-passed emitting a Failure with retries removed. (2) Determinism contract exemption list was wrong (the listed fields are IN the sidecar, so they're identical between run and analyze); rewritten to call out `document_kind` and optionally `failtrace_version`. (3) `document_kind` shape ambiguity resolved: this top-level schema covers `run_result` and `analysis_result`; minimal separate schemas for `explanation_result` and `adapters_result` added below. (4) Example `raw_log_sha256` values were the SHA-256 of empty string; replaced with realistic placeholders + a note that example hashes are illustrative. (5) `parse_status: complete` had a circular definition; rewritten operationally.
>
> *Cleanups:* (6) `adapters[].selected` condition broadened to `parse_status` in `{complete, partial}` (was "non-empty failures"). (7) `failures_complete` redefined operationally. (8) Run directories now include a unique suffix and SHOULD be mode 0700. (9) `Excerpt.text` decoding specified (UTF-8, replacement for invalid sequences). (10) Path normalization explicitly does NOT realpath symlinks.
>
> *Clarifications:* (11) `companion_diagnostics` now uses a slimmer `Diagnostic` object instead of reusing `Failure` (cleaner; no awkward "doesn't participate" invariants). (12) `diagnostic_severity: warning` only appears in failures[] when the warning triggered the non-success outcome (e.g. `pytest -W error`). (13) `Command` object adds side-effect fields (`mutates_workspace`, `uses_network`, `requires_approval`). (14) Enum forward-compat policy reworded: consumers READ; only re-emitters add determinism_warnings. (15) Document kinds: `run_result`/`analysis_result` use this schema; `explanation_result`/`adapters_result` have their own (now defined).
>
> *No formula or fixture changes.* `failure_id` and `test_group_id` derivations are unchanged from v1.2; all three example fixture hashes remain valid.

---

## Design principles

1. **Determinism over richness.** With the same run directory, binary, adapter version, emission budget, and no determinism warnings, analysis is byte-identical after excluding the command-mode fields listed in the determinism contract. No LLM, no heuristics we can't name, no semantic scores without a deterministic rule.
2. **Raw log plus sidecar is the replayable artifact.** Every classification points to `raw_log_path` and a coordinate range; the sidecar carries the run-time metadata bytes alone can't recover. Raw log alone is human recovery only.
3. **Buffered classification.** Failtrace captures the entire raw log before emitting JSON. Streaming/incremental output is explicitly a v2 concern. This lets adapters classify `failed` vs `pre_test_failed` vs `harness_crashed` with full lookahead.
4. **One axis per field.** When a field carries two unrelated signals, split it.
5. **No privileged failure.** Multi-failure runs return a ranked flat list. First element is the recommendation; the ranking strategy is stated.
6. **No code-edit suggestions.** Only command suggestions. The agent owns repair.
7. **Extensions never load-bearing.** Per-adapter `extensions` namespaces may carry rich adapter-specific data; consumers ignoring all `extensions` produce semantically valid output. Extensions MUST NOT feed core IDs or invariants.

---

## Per-run directory layout

A successfully analyzed `failtrace run` persists three artifacts in its selected run directory. Generated directories use `<selected-root>/<timestamp>-<suffix>/`; `--run-dir` instead uses the exact supplied path. Run-root precedence is defined in [`release-0.1.md`](release-0.1.md#run-directories). `<timestamp>` is RFC 3339 in UTC with hyphen-safe punctuation (`YYYY-MM-DDTHH-MM-SS.sssZ`). `<suffix>` is the first 8 hex of a random per-invocation nonce, ensuring two invocations in the same second do not collide. Directories SHOULD be created mode `0700` and files mode `0600`; raw logs may carry secrets (env vars, tokens that leaked into stderr, etc.) and are not safe to share-read by default. A failtrace-internal failure may leave a partial directory; absent valid JSON, consumers must treat the operation as a tool failure rather than a completed run.

| File | Producer | Description |
|------|----------|-------------|
| `log` | `failtrace run` | Combined stdout+stderr capture, unmodified bytes. |
| `meta.json` | `failtrace run` | Sidecar metadata. |
| `output.json` | `failtrace run` | The full failtrace JSON document. This is the source of truth even when stdout emits a budgeted projection. |

`failtrace last` reads `output.json` directly and prints it byte-faithful; it does not re-analyze. The printed `document_kind` and `schema_version` are whatever the stored file carries. A v1.5 run remains v1.5 when printed by `last`.

`failtrace run` and `failtrace analyze <run-dir>` both emit one JSON document serialized on a single line to stdout. This describes the wire format, not a size guarantee. By default stdout uses `emission.mode: "budgeted"` and includes at most `--max-failures 10` failures; `--max-failures all` emits the full document. Regardless of stdout budgeting, `<run-dir>/output.json` is always the full document with `emission.channel: "file"` and `emission.mode: "full"`.

`failtrace analyze <run-dir>` reads `log` + `meta.json` and emits a fresh JSON to stdout with `document_kind: "analysis_result"` and the current binary's output `schema_version`. It never overwrites `output.json` in v1.6. A future `--rewrite-output` flag may enable that path; v1.6 explicitly excludes it.

`raw_log_path` in the output is the absolute path to `log` (not the directory). This preserves single-file `sed`/`head`/`tail` pipelines.

### Sidecar metadata (`meta.json`)

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `meta_schema_version` | string | yes | Semver of the sidecar format. v1.6 emits `"1.2"`. |
| `schema_version` | string | yes | Output schema version this run targeted (e.g. `"1.6"`). v1.6 `analyze` can read v1.4 and v1.5 sidecars for compatibility, but v1.6 output declares the current schema. |
| `failtrace_version` | string | yes | Failtrace binary version. |
| `capture_build` | string or null | no | Capturing binary's build identity, exactly its `--version` line without the trailing newline. Includes product, semver, revision (or `unknown`), and tree state (`clean`, `dirty`, or `unknown`). Absent in older captures. Informational only; never used in classification or emitted run/analysis documents. |
| `adapters` | array of object | yes | Adapters involved in producing the output. See below. |
| `adapter_candidates` | array of object | yes | Candidate adapters considered in source-text order before selection. Same entry shape as `adapters[]`; used by `analyze` replay and composite-command determinism. |
| `argv` | array of string | yes | Structured argv captured by `failtrace run`; this is the replay source for command detection and composite expansion. |
| `command` | string | yes | Shell-quoted command. POSIX quoting on Unix; Windows unspecified. |
| `cwd` | string | yes | Absolute path the command ran in, NFC-normalized. |
| `started_at` | string (RFC 3339, UTC) | yes | When failtrace launched the command. |
| `duration_ms` | integer | yes | Wall-clock duration. |
| `exit_code` | integer or null | yes | Process exit code; null if killed before reporting. |
| `signal` | string or null | yes | POSIX signal name when killed. |
| `early_exit` | object or null | yes | See top-level `early_exit`. An object is a captured replay override. Null means no captured override, so the adapter deterministically re-derives any runner-reported early exit from `log` and the other sidecar fields; accepted legacy v1.4/v1.5 sidecars may omit this field with the same behavior. |
| `truncated` | boolean | yes | Whether captured output was truncated. |
| `truncation_reason` | string or null | yes | `buffer_exceeded` / `timeout` / `signal`. |
| `path_case_folding` | enum | yes | `"case_sensitive"` or `"case_insensitive"`. `run` first probes `cwd` with an owner-private temporary case-variant file, then uses read-only directory evidence if the probe is impossible, and falls back to the platform default only when neither can decide. `analyze` reuses the captured value so `in_user_code` stays deterministic across replay environments. |
| `stream_merge` | enum | yes | `stdout_stderr_interleaved` / `stdout_then_stderr` / `stderr_then_stdout` / `stdout_only` / `stderr_only`. How captured streams compose in `log`. |
| `raw_log_sha256` | string | yes | SHA-256 hex of `log` bytes. Mirrors the top-level field. |

`capture_build` is an additive optional extension to sidecar format `1.2`;
`meta_schema_version` remains `"1.2"` because existing readers ignore unknown
fields and no required field or replay semantics changed. Only a fresh `run`
sets this field. Missing or null means the capturing build is unknown; an
analyzer must never substitute its own identity. `analyze` leaves both
`meta.json` and `output.json` untouched. Changing only `capture_build` does not
change analysis stdout. Output schema remains `1.6`, and `failtrace_version`
remains plain binary semver (`0.1.0`).

#### Sidecar `adapters[]` entries

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Adapter identifier (e.g. `"pytest"`, `"npm-script"`). |
| `version` | string | Adapter semver (e.g. `"pytest@0.2.0"`). |
| `kind` | enum | `runner` / `compiler` / `typechecker` / `wrapper` (e.g. `npm-script`). |
| `selected` | boolean | Whether this adapter produced the document. Exactly one adapter has `selected: true` whenever `parse_status` is `complete` or `partial`. |
| `selection_reason` | enum | `argv_name_match` / `composite_command_component` / `log_signature_match` / `not_selected`. |

### `analyze` behavior with respect to the sidecar

- **Sidecar absent or unreadable** → `failtrace analyze` exits non-zero with `"sidecar incomplete"`.
- **Sidecar `schema_version` is neither the running binary's schema nor a documented compatibility input** → refuse with `"incompatible schema version"`. v1.6 accepts v1.4 and v1.5 sidecars and emits v1.6 output.
- **Sidecar `schema_version` is an accepted older minor version** → proceed with `determinism_warnings: [{code: "schema_version_drift_minor", adapter_id, meta_version, current_version}]`. **The byte-identical guarantee no longer holds when this warning fires.**
- **Sidecar `adapter_version` differs from running binary for the `selected` adapter** → proceed with `determinism_warnings: [{code: "adapter_version_drift", adapter_id, meta_version, current_version}]`. **The byte-identical guarantee no longer holds when this warning fires.**

---

## Top-level object (`run_result` / `analysis_result`)

This section describes the shape produced by `failtrace run` (`document_kind: "run_result"`) and `failtrace analyze` (`document_kind: "analysis_result"`). The two shapes are identical except for the discriminator value. `explanation_result` and `adapters_result` have their own minimal shapes (see "Other document kinds" near the end).

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `schema_version` | string | yes | `"1.6"`. |
| `document_kind` | enum | yes | `run_result` (from `failtrace run`/`last`) or `analysis_result` (from `failtrace analyze`). |
| `failtrace_version` | string | yes | Failtrace binary version. |
| `emission` | object | yes | How this document was emitted and whether `failures[]` is the full list or a budgeted projection. See `emission`. |
| `command` | string | yes | Shell-quoted command. See "Command quoting". |
| `cwd` | string | yes | Absolute path the command ran in, NFC-normalized. |
| `started_at` | string (RFC 3339, UTC) | yes | When failtrace launched the command. |
| `duration_ms` | integer | yes | Wall-clock duration. |
| `exit_code` | integer or null | yes | Process exit code. Null if killed before reporting (`signal` will be set). |
| `signal` | string or null | yes | POSIX signal name when killed. Always null on platforms without POSIX signals. |
| `runner_outcome` | enum | yes | What the underlying runner did. |
| `early_exit` | object or null | yes | If the run terminated early by request. |
| `adapters` | array of object | yes | Same shape as sidecar `adapters[]`. Mirrors `meta.json`. |
| `parse_status` | enum | yes | Whether the adapter parsed the output successfully. |
| `parse_warnings` | array of enum | yes | Specific parser-side conditions. Empty if none. |
| `determinism_warnings` | array of object | yes | Conditions affecting the byte-identical guarantee. Empty if none. |
| `truncated` | boolean | yes | Captured output truncated. |
| `truncation_reason` | string or null | yes | When `truncated: true`. |
| `failures_complete` | boolean | yes | `true` iff failtrace believes its full pre-budget analysis accounted for every failure reported by the runner in the captured log. `false` when truncation cut mid-failure-stream, when a missing summary line prevented the adapter from confirming the full count, or whenever the adapter has any reason to suspect failures it should have surfaced are missing. Emission budgeting does not change this value. |
| `failures` | array of Failure | yes | Failures present in this emission, ranked. A budgeted emission may contain only a prefix of the full analysis; `emission.failures_total` records the pre-budget count. May be empty when `runner_outcome` is `passed` or `no_tests`. |
| `ranking` | object | yes | How `failures` is ordered. |
| `counts` | object | yes | Test counts. |
| `raw_log_path` | string | yes | Absolute path to `<run-dir>/log`. |
| `raw_log_sha256` | string | yes | SHA-256 hex of the `log` bytes. Lets two consumers verify they're analyzing the same input. |
| `suggested_next_commands` | array of Command | yes | May be empty. |
| `extensions` | object | yes | Per-adapter overflow keyed by `adapter_id`. May be empty. Opaque to core schema. |

### `emission`

`emission` is a required top-level sibling of `failures`, not a nested adapter or extension field. It lets consumers distinguish a full document from a stdout projection whose `failures[]` array was intentionally shortened for agent runtime display limits.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `channel` | enum | yes | `"stdout"` or `"file"`. `output.json` uses `"file"`; command output uses `"stdout"`. |
| `mode` | enum | yes | `"budgeted"` when `failures[]` may be capped by `max_failures`; `"full"` when every emitted failure is present. |
| `max_failures` | integer or null | yes | Failure emission budget. Null only when `mode: "full"`. |
| `failures_emitted` | integer | yes | Number of entries present in this document's `failures[]`. |
| `failures_total` | integer | yes | Number of failures in the full analysis result before any emission budget. |
| `full_output_path` | string | yes | Absolute path to `<run-dir>/output.json`, the full source-of-truth document. |

`truncated`, `failures_complete`, and `emission.mode` are separate concepts. `truncated` means failtrace's raw runner capture was cut off before the adapter analyzed it. `failures_complete` means the selected adapter believes its full pre-budget analysis accounted for every failure in the analyzed log. `emission.mode: "budgeted"` means a `max_failures` cap was applied to this document's emission — trimming may or may not have actually occurred; `failures_emitted < failures_total` indicates the cap actually bit. `emission.mode: "full"` means no cap was applied (and consequently `failures_emitted == failures_total`). A budgeted document can therefore be `truncated: false`, `failures_complete: true`, AND `failures_emitted < failures_total`: the analysis was complete, while this particular emission intentionally carries only a prefix and points to the full `output.json`.

### `runner_outcome` enum

| Value | Meaning |
|-------|---------|
| `passed` | Runner exited cleanly with no failures. |
| `failed` | Runner completed normally and reported test failures or test errors, **including** setup/teardown errors and ordinary test-body panics that the runner caught and reported. |
| `pre_test_failed` | A test runner or build harness failed during build, dependency resolution, collection, suite loading, or setup before any test item began. |
| `typecheck_failed` | A typecheck-only runner failed its typecheck. This is defined by command role, not by TypeScript specifically. |
| `harness_crashed` | Runner process or harness terminated abnormally *before producing its native completed-run summary*. |
| `timed_out` | Any wall-clock or silence budget elapsed (failtrace watchdog, runner-internal timeout, per-test timeout). `early_exit.killer` identifies the enforcer. |
| `no_tests` | Runner completed and reported zero tests. |

**Decisive test for `failed` vs `harness_crashed` vs `pre_test_failed` vs `typecheck_failed`:**

> Is this a typecheck-only command?
>
> - Yes, and the typecheck failed → `typecheck_failed`.
> - No → decide among the test-runner outcomes below.
>
> For test runners and build harnesses: did at least one test item begin, AND did the runner emit its native completed-run summary?
>
> - Both yes → `failed`.
> - Test item began, summary missing → `harness_crashed`.
> - No test item began → `pre_test_failed`, regardless of any summary-like text the runner emitted.

### `parse_status` enum

| Value | Meaning |
|-------|---------|
| `complete` | An adapter recognized the runner output and extracted every semantic field its adapter contract requires. Advisory `parse_warnings` MAY still be present. |
| `partial` | An adapter recognized the output, but one or more contract-affecting `parse_warnings` means some required structured field or consistency check degraded. |
| `unparsed_output` | The command was in scope for an adapter, but the captured output did not satisfy that adapter's minimum parse signature. `failures` is empty; `raw_log_path` is authoritative. |
| `unsupported_command` | Failtrace captured a recognized composite or package-test command, but v1.6 cannot attribute the shared log to one candidate adapter. `failures` is empty and no adapter is selected. Unknown direct commands are rejected before capture with exit `65` and no analysis document; bare `go test` is admitted but emits `unparsed_output` because the required JSON event stream is absent. |

`parse_status`, `parse_warnings`, and `failures_complete` are orthogonal:

| Combination | Meaning |
|-------------|---------|
| `complete` + `failures_complete: true` | Trusted fields, complete failure list. |
| `complete` + `failures_complete: false` | Required fields recovered, but the failure list may be short. |
| `partial` + `failures_complete: true` | All failures surfaced, but some required contract fields degraded. |
| `partial` + `failures_complete: false` | Contract degradation and possible dropped failures. |

### `parse_warnings` enum

Closed set, parser-side observations only. Adapters may emit zero or more.

`missing_session_banner`, `missing_summary_line`, `truncated_traceback`, `interleaved_output`, `unstructured_diagnostic`, `malformed_structured_event`, `missing_terminal_event`, `runner_summary_mismatch`, `composite_runtime_deferred`, `missing_line_number`, `missing_test_name`, `ansi_codes_present_in_message`, `runner_version_unsupported`.

> Removed in v1.2: `pre_test_phase_only` (derive from `failures[0].phase != "test"`), `adapter_version_drift` (moved to `determinism_warnings`).
>
> Renamed in v1.5: `unknown_diagnostic_code` became `unstructured_diagnostic`. The warning means the adapter saw a diagnostic-like line but could not recover a closed diagnostic code or fully structured payload.
>
> Added in v1.5: `composite_runtime_deferred` means failtrace recognized more than one adapter candidate for a composite command, recorded those candidates in the sidecar, and intentionally did not assign the captured log to one segment.

`parse_status` is derived from warning classification for recognized adapter output:

| Warning | Classification | Effect |
|---------|----------------|--------|
| `missing_summary_line` | degrading | `parse_status: "partial"` |
| `truncated_traceback` | degrading | `parse_status: "partial"` |
| `malformed_structured_event` | degrading | `parse_status: "partial"` |
| `missing_terminal_event` | degrading | `parse_status: "partial"` |
| `runner_summary_mismatch` | degrading | `parse_status: "partial"`. Two triggers: (1) the summary reports green but the sidecar exit code is nonzero; (2) a completed runner summary, structured terminal stream, or recognized run-level failure marker reports more comparable failure units than the adapter accounted for in its full pre-budget failure set. The emitted list may be empty or short, `runner_outcome` may be `failed`, `pre_test_failed`, or `typecheck_failed`, and `failures_complete` is `false`. Comparable units are adapter-defined because one failed test may legitimately emit multiple failure objects. |
| `missing_session_banner` | advisory | `parse_status` stays `"complete"` unless another warning degrades it |
| `runner_version_unsupported` | advisory | `parse_status` stays `"complete"` unless another warning degrades it |
| `unstructured_diagnostic` | adapter-contextual | Adapter decides whether this degrades required structured fields; default is advisory. |
| `missing_line_number` | adapter-contextual | Adapter decides whether this degrades required structured fields; default is advisory. |
| `composite_runtime_deferred` | structural | Emitted only with `parse_status: "unsupported_command"` for composite deferral; excluded from complete/partial classification. |
| `interleaved_output` | reserved | Defined, not yet emitted. |
| `missing_test_name` | reserved | Defined, not yet emitted. |
| `ansi_codes_present_in_message` | reserved | Defined, not yet emitted. |

`truncated_traceback`, `malformed_structured_event`, and `missing_terminal_event` may also force `failures_complete: false`. That co-occurrence is intentional: `parse_status` reports contract health, while `failures_complete` reports whether the failure list may be short.

`xcodebuild-test` also emits `runner_summary_mismatch` for unreliable execution
counts: per-occurrence XCTest case witnesses disagree with a bundle summary,
a bundle occurrence never closes, cases occur outside a bundle, a bundle's
combined population exceeds its selected occurrence, a runner restart is
observed, or extracted failed-test identities plus skips exceed the reported
total. Counts retain the selected summary totals with the invariant-preserving
floor described below. These count-only disagreements do not by themselves
make the failure list incomplete.

### `determinism_warnings` array

Conditions that affect the byte-identical determinism guarantee. Each entry:

| Field | Type | Description |
|-------|------|-------------|
| `code` | enum | Closed set; initial members: `adapter_version_drift`, `schema_version_drift_minor`. |
| `adapter_id` | string | Which adapter is affected. |
| `meta_version` | string | Version recorded in the sidecar. |
| `current_version` | string | Version of the running binary. |

When any entry is present, the byte-identical guarantee does NOT hold for this output.

### `early_exit` (or null)

| Field | Type | Description |
|-------|------|-------------|
| `reason` | enum | `maxfail` / `bail` / `watchdog_silence` / `watchdog_wallclock` / `runner_internal_timeout` / `per_test_timeout` / `signal_received`. |
| `killer` | enum | `runner` / `failtrace` / `os`. |
| `details` | string | Short human-readable reason. |

### `ranking`

| Field | Type | Description |
|-------|------|-------------|
| `strategy` | enum | `cascade_root` / `compile_before_runtime` / `first_emitted` / `runner_priority` / `none`. |
| `is_stable_across_runs` | boolean | Whether a fresh runner execution on the same code is expected to produce the same failure ordering. **False** when the adapter observes or must assume scheduling-dependent ordering, including jest/vitest workers, Go multi-package or pause/continue events, pytest-xdist, and xcodebuild test-phase failures. |

### `counts`

| Field | Type | Description |
|-------|------|-------------|
| `tests_run` | integer | Unique `runner_test_id`s attempted at least once. |
| `tests_passed` | integer | Unique tests whose **final** retry attempt was `passed`. |
| `tests_failed` | integer | See Invariants. |
| `tests_errored` | integer | See Invariants. |
| `tests_skipped` | integer | Reported by runner. |
| `tests_not_run` | integer or null | Tests that exist but did not execute. Null when not knowable. |
| `files_failed` | integer or null | Test files (or Go packages) with ≥1 failure. Null when the runner doesn't expose file-level rollups. |

**Flakes that finally passed do NOT appear in `failures[]`.** v1.3 resolves the v1.2 contradiction here: `failures[]` means "things to fix now," not "historical incident log." A test that failed attempts 1–2 and passed attempt 3 counts as `tests_passed`, contributes nothing to `failures[]`, and produces `runner_outcome: passed` (assuming no other tests failed). The retry history of a flake-that-passed is not preserved in v1.3 output; if a real adopter wants it, a future minor version may add a separate `flaky_passes: [...]` field. A test whose final retry was `failed` or `errored` DOES appear in `failures[]`, and its `retries[]` carries the full attempt history including any earlier passes.

---

## Failure object

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `failure_id` | string | yes | Stable 16-hex identifier. See derivation. |
| `adapter_id` | string | yes | Must equal one of `adapters[].id`. The adapter that produced this failure. |
| `phase` | enum | yes | When in the lifecycle. |
| `kind` | enum | yes | Shape of failure. |
| `runner_package_id` | string or null | yes | Runner-native package/module/project id when the runner reports one (for example a Go import path). Null when not applicable or not exposed. |
| `runner_test_id` | string or null | yes | Runner's canonical test identifier. Null for non-test failures. |
| `test_segments` | array of string | yes | Structured test-id path segments when the runner exposes hierarchy (for example Go subtests split on `/`). Empty when not applicable. |
| `test_name` | string or null | yes | Human-readable test name. |
| `param_id` | string or null | yes | Raw parameter identifier (pytest bracket content). Adapter-defined extraction rule. Null for non-parameterized or unsupported. |
| `test_group_id` | string or null | yes | Stable identifier shared by parameterized siblings. Null unless adapter declares a `strip_params` rule. |
| `primary_location` | Location | yes | Location to open first. |
| `extra_locations` | array of Location | yes | Additional relevant locations. |
| `message` | string | yes | Single-line summary. |
| `diagnostic_code` | string or null | yes | Runner/compiler error code: `"E0502"`, `"TS2322"`. |
| `diagnostic_severity` | enum | yes | `error` / `warning` / `info` / `hint`. Maps to LSP DiagnosticSeverity 1–4. `warning` appears in `failures[]` ONLY when the warning triggered non-success (e.g. `pytest -W error`, `-Werror` in a C build, `--deny warnings` in cargo). Non-fatal warnings the runner emitted but did not act on do NOT appear as standalone failures. |
| `details` | Details | yes | Discriminated by `kind`. |
| `raw_log_lines` | [integer, integer] | yes | Inclusive 1-indexed line range in `raw_log_path`. Counts `\n` boundaries in the raw stream; ANSI NOT stripped before counting. |
| `evidence` | Evidence or null | yes | Byte-addressable excerpts. Null only when adapter cannot emit byte coordinates. |
| `cascade_root` | string or null | yes | `failure_id` of an upstream failure that, by named deterministic rule, caused this one. Authoritative for the cascade relationship. |
| `cascade_reason` | enum or null | yes | Which rule populated `cascade_root`. |
| `dependent_failure_ids` | array of string | yes | Mirror of `cascade_root`: when populated, lists every failure in this document whose `cascade_root` equals this failure's `failure_id`, sorted by their order in `failures[]`. Derived; if it disagrees with `cascade_root`, trust `cascade_root`. |
| `retries` | array of Retry | yes | Each attempt; empty when not retried. |
| `companion_diagnostics` | array of Diagnostic | yes | Per-failure annotations (e.g. `go test -race` firing alongside an assertion). Uses the slimmer `Diagnostic` object (see below), NOT `Failure`. Companions never participate in counts, ranking, or cascade — they're context attached to a failure, not standalone failures. Empty array when none. |
| `extensions` | object | yes | Per-adapter overflow keyed by `adapter_id`. Opaque to core. |

### `phase` enum

`build` / `collect` / `setup` / `test` / `teardown` / `typecheck`.

### `kind` enum

`assertion`, `exception`, `type_error`, `syntax_error`, `missing_symbol`, `dependency_resolution_error`, `linker_error`, `panic`, `signal`, `timeout`, `oom`, `snapshot`, `unhandled_rejection`, `race`, `unknown`.

> Adapters SHOULD NOT route a known-shape failure to `unknown` rather than picking the closest existing `kind`.

### `kind` selection precedence

Listed in priority order; first matching rule wins.

0. **Multiple-match arbitration.** Rules below are listed in priority order. Apply in order; first match wins.
1. **OOM.** Log contains an explicit memory-exhaustion diagnostic → `oom`. Examples: Python `MemoryError`, Node `JavaScript heap out of memory`, Go `runtime: out of memory`, kernel `Out of memory: Killed process`.
2. **Timeout.** Failtrace watchdog or runner timeout budget fired → `timeout`. `details.source` distinguishes.
3. **Signal.** Process killed by a signal and no rule above applies → `signal`.
4. **Pre-kill diagnostics.** If the adapter satisfied its minimum-failure rule against bytes emitted before termination, those failures are emitted normally. The schema does NOT synthesize an additional `kind: signal` failure. Top-level `signal` still populated.

Per-kind selection examples (non-termination):

- Python `SyntaxError`, TS parse errors, rustc parse errors → `syntax_error`.
- pip / npm / yarn / pnpm / cargo / go-mod resolver failures, as well as deterministic runner/tool resolver failures such as target or destination resolution → `dependency_resolution_error`.
- Local import / name-lookup failures → `missing_symbol`.
- Linker diagnostics (`ld`, `link.exe`, rustc linking) → `linker_error`.

### Location object

| Field | Type | Description |
|-------|------|-------------|
| `file` | string | Path relative to `cwd` when possible, else absolute. |
| `line` | integer or null | 1-indexed start line. |
| `column` | integer or null | 1-indexed start column. |
| `end_line` | integer or null | 1-indexed end line (inclusive). Null when runner doesn't report. |
| `end_column` | integer or null | 1-indexed end column. Null when runner doesn't report. |
| `role` | enum | `primary` / `definition` / `import_site` / `panic_site` / `test_site` / `related`. |
| `in_user_code` | boolean | Whether the adapter's ecosystem-specific classifier treats this path as user code. The classifier is lexical only: replace `\` with `/`, split on `/` into non-empty components, ASCII-lowercase the normalized key when sidecar `path_case_folding` is `case_insensitive`, then apply ecosystem component-prefix/suffix rules. No realpath, filesystem access, symlink resolution, or substring matching is allowed. |

### Span object

A source span with an optional adapter-emitted annotation. Used in `details.type_error` and `details.syntax_error`. `Span` intentionally does not carry `in_user_code`; only `Location` and `Frame` do.

| Field | Type | Description |
|-------|------|-------------|
| `file`, `line`, `column`, `end_line`, `end_column`, `role` | | Same meanings as `Location`. |
| `note` | string or null | Adapter-extracted prose attached to the span ("immutable borrow occurs here", "Type 'X' is not assignable to 'Y'"). |

### Evidence object

| Field | Type | Description |
|-------|------|-------------|
| `byte_range` | object | `{start: int, end: int}`, half-open `[start, end)`, indexing raw `log` bytes (unmodified, ANSI in place). |
| `excerpts` | array of Excerpt | May be empty. Each excerpt slices a sub-range of `byte_range` and optionally provides ANSI-stripped text. |

#### Excerpt object

| Field | Type | Description |
|-------|------|-------------|
| `source` | enum | `raw_log` / `runner_structured_event` / `adapter_synthesized`. Closed set describing where the excerpt text came from. |
| `kind` | enum | `failure_block` / `stack` / `diff` / `summary` / `process` / `notes`. |
| `byte_range` | object | Same `{start, end}` shape. Indexes raw `log` bytes. |
| `text` | string | The excerpt's content. **Decoded from the raw bytes as UTF-8 with replacement (`U+FFFD`) for invalid byte sequences**, optionally with ANSI codes stripped after decode. |
| `ansi_stripped` | boolean | Whether `text` had ANSI codes removed. |
| `truncated` | boolean | Whether `text` was cut shorter than the underlying byte range would suggest. |

### Diagnostic object (for `companion_diagnostics`)

A slimmer record than `Failure`, used only as a per-failure annotation. Diagnostics do NOT have `failure_id`, `cascade_root`, `dependent_failure_ids`, `retries`, or their own `companion_diagnostics`. They are context, not standalone failures.

| Field | Type | Description |
|-------|------|-------------|
| `diagnostic_id` | string | Document-local 16-hex id. Computed `sha1(parent_failure_id + "|" + kind + "|" + index_in_companion_array)[:16]`. Only needs to be unique within this document. |
| `kind` | enum | Same `kind` enum as `Failure`. |
| `diagnostic_severity` | enum | Same enum as `Failure`. |
| `message` | string | Single-line summary. |
| `primary_location` | Location or null | Location of the diagnostic, when one exists. |
| `details` | Details | Same discriminated union as `Failure.details`. |
| `evidence` | Evidence or null | Byte-addressable excerpt, when adapter can emit one. |

### Details discriminated union

Every variant includes `type` for dispatch.

- `kind: assertion` → `details: { type: "assertion", expected: string|null, actual: string|null, diff: Diff|null }`
- `kind: exception` → `details: { type: "exception", exception_type: string|null, stack: [Frame] }`
- `kind: type_error` → `details: { type: "type_error", spans: [Span], explanation_command: string|null }`
- `kind: syntax_error` → `details: { type: "syntax_error", spans: [Span] }`
- `kind: missing_symbol` → `details: { type: "missing_symbol", symbol: string, import_chain: [string] }`
- `kind: dependency_resolution_error` → `details: { type: "dependency_resolution_error", package: string|null, resolver: "pip"|"npm"|"yarn"|"pnpm"|"cargo"|"go_mod"|"unknown" }`
- `kind: linker_error` → `details: { type: "linker_error", unresolved_symbols: [string] }`
- `kind: panic` → `details: { type: "panic", panic_message: string, stack: [Frame], goroutine_id: integer|null }`
- `kind: signal` → `details: { type: "signal", signal_name: string, exit_code_raw: integer|null }`
- `kind: timeout` → `details: { type: "timeout", source: "runner"|"failtrace_watchdog"|"per_test", limit_ms: integer|null, silence_ms: integer|null }`
- `kind: oom` → `details: { type: "oom", quoted_message: string, source: "process"|"language_runtime"|"kernel" }`
- `kind: snapshot` → `details: { type: "snapshot", snapshot_file: string|null, snapshot_name: string|null, diff: Diff }`
- `kind: unhandled_rejection` → `details: { type: "unhandled_rejection", rejection_value: string, stack: [Frame] }`
- `kind: race` → `details: { type: "race", participants: [{ stack: [Frame] }] }`
- `kind: unknown` → `details: { type: "unknown" }`

`Frame` = `{ file: string, line: integer|null, function: string|null, in_user_code: boolean }`. `in_user_code` uses the same ecosystem classifier as `Location.in_user_code`; Go stack frames may emit a portable basename in `Frame.file` while classifying against the raw stack path.

#### Diff object

For structured diffs in `assertion` and `snapshot` details.

| Field | Type | Description |
|-------|------|-------------|
| `format` | enum | `unified` / `expected_actual` / `inline`. |
| `lines` | array of Line | Each line: `{op: "add"|"remove"|"context"|"meta", text: string}`. |
| `truncated` | boolean | Whether `lines` was cut shorter than the runner's full diff. |
| `omitted_line_count` | integer | When `truncated: true`. |

### `cascade_reason` enum

Listed in priority order. If multiple rules match, the first listed wins.

- `pytest_collection_error` — collection-phase failure prevents subsequent tests.
- `cargo_build_failed` — build error means no tests executed.
- `tsc_intra_diagnostic_chain` — notes within one tsc diagnostic.
- `tsc_cross_file_chain` — downstream tsc diagnostics referencing earlier failed definitions.
- `go_binary_aborted` — panic ended the test binary; subsequent `FAIL:` lines reflect not-run tests.
- `npm_script_chain` — failure in a preceding script (`tsc && jest`) prevented the next from starting.

If none apply, `cascade_root` is null. Failtrace does NOT infer causality from message similarity, import graph alone, or temporal proximity.

### Retry object

| Field | Type | Description |
|-------|------|-------------|
| `attempt` | integer | 1-based. |
| `outcome` | enum | `passed` / `failed` / `errored`. |
| `duration_ms` | integer | This attempt's wall-clock. |

A test whose `retries` contains both `passed` and `failed` outcomes is flaky. No separate `flaky: true` flag; data is in `retries`.

---

## Command object (for `suggested_next_commands`)

| Field | Type | Description |
|-------|------|-------------|
| `argv` | array of string | **Authoritative** for execution. No quoting bugs, no Windows escape hatches. |
| `command` | string | Display-only, POSIX-quoted on Unix; Windows unspecified. For composite commands, represents the wrapper invocation as `argv` (e.g. `["npm", "test"]`), NOT the inner expansion. |
| `cwd` | string | Working directory the command expects. |
| `intent` | enum | `rerun_isolated` / `rerun_verbose` / `rerun_with_debugger` / `update_snapshots` / `regenerate_lockfile` / `explain_diagnostic` / `inspect_raw_log`. |
| `interactive` | boolean | True for commands that block on human input (pdb, lldb, dlv). Autonomous agents MUST NOT auto-execute commands with `interactive: true`. Required `true` for `rerun_with_debugger`. |
| `mutates_workspace` | boolean | True if running the command writes files inside `cwd` (e.g. `update_snapshots`, `regenerate_lockfile`). Agents should treat as opt-in. |
| `uses_network` | boolean | True if running the command issues network requests (e.g. `regenerate_lockfile` pulls registry data). Agents on metered or sandboxed networks should treat as opt-in. |
| `requires_approval` | boolean | True if failtrace recommends the user authorize before this runs. Implied `true` whenever any of `interactive`, `mutates_workspace`, or `uses_network` is true; may also be set independently when the command's effect is unusual for the kind. |

If `argv` and `command` semantically disagree, `argv` wins.

`update_snapshots` is allowed despite "no code edits": it's a runner-native command whose side effect is a generated artifact, not code an author writes. `mutates_workspace: true` flags it for agents that auto-execute suggestions.

---

## `failure_id` derivation

```
failure_id = sha1(
    adapter_id + "|" +
    phase + "|" +
    kind + "|" +
    (runner_test_id || primary_location.file + ":" + (primary_location.line || "?")) + "|" +
    (param_id || "") + "|" +
    normalize(message)
)[:16]
```

16 hex characters (64 bits). Collision-resistant up to ~4 billion failures.

## `test_group_id` derivation

```
test_group_id = sha1(
    adapter_id + "|" +
    strip_params(runner_test_id || primary_location.file + ":" + (primary_location.line || "?"))
)[:16]
```

Excludes `phase`, `kind`, `param_id`, `message`. Pure parameter-family identity.

Null unless adapter declares a documented `strip_params` rule. v1.2 declares pytest only.

## `normalize(message)` algorithm

Applied in strict order:

1. **Strip ANSI** escape sequences (CSI, OSC, plain SGR; full table referenced).
2. **NFC** Unicode normalization on `message` and on `cwd`.
3. **Replace cwd prefix.** Literal byte/string prefix match: if `message` starts with the NFC'd `cwd + "/"`, replace with `<cwd>/`. NOT a regex. Match-anywhere replacement (literal substring) replaces all occurrences.
4. **Hex replace.** `0x[0-9a-fA-F]+` → `<N>`.
5. **Decimal replace.** ASCII-only `\b\d+\b` (Rust regex `(?-u)\b\d+\b`) → `<N>`.
6. **Whitespace collapse.** Runs of whitespace → single space.
7. **Trim.**

Step ordering rationale:
- NFC before cwd-match: combined and decomposed forms of the same path match identically.
- Literal cwd before any regex digit replacement: a cwd containing digits (`/Users/example/repo-2024/`) is replaced before digit substitution sees it.
- Hex before decimal: `0x1234` produces `<N>`, not `0x<N>`.
- ASCII-only decimal regex: locale-independent results.

---

## Determinism contract

Given the same run directory (same `log` bytes, same `meta.json`), the same failtrace binary, the same selected `adapter_version`, and the same emission budget, `failtrace analyze <run-dir>` emits JSON that is **byte-identical across invocations except for fields intentionally changed by command mode**:

- `document_kind`: `run_result` from `failtrace run`, `analysis_result` from `failtrace analyze`. Byte-comparing a stored `output.json` (from `run`) against fresh `analyze` output requires ignoring this field.
- `failtrace_version`: if the binary changed between when `output.json` was written and when `analyze` is invoked.
- `emission.channel`: `output.json` records `"file"`; stdout records `"stdout"`.
- `emission.mode`, `emission.max_failures`, and `emission.failures_emitted`: if the compared outputs use different `--max-failures` settings.

All other fields — including `cwd`, `started_at`, `duration_ms`, `raw_log_path`, all per-failure content — are byte-identical because they're either captured into the sidecar at `run` time (and replayed from there) or derived deterministically from the sidecar + log bytes.

Composite-command candidate order is source-text order: top-level `&&` segments are recorded left to right, and `adapter_candidates` preserves that order in the sidecar. v1.6 does not assign a multi-candidate composite log to one segment. `Location.in_user_code` derivation depends on sidecar `path_case_folding`; replay uses the captured value, not the analyzing machine's current filesystem behavior.

**Verification recipe.** `failtrace analyze --max-failures all <run-dir> | jq 'del(.document_kind, .failtrace_version, .emission.channel) | ...' == jq 'del(.document_kind, .failtrace_version, .emission.channel) | ...' <run-dir>/output.json`. (Same `jq` filter both sides; failtrace's own conformance test ships an exact filter.)

**The byte-identical guarantee does NOT hold when `determinism_warnings` is non-empty.** Adapter version drift, minor schema-version drift, and any future codes added to that array signal that the produced output may differ from the original. Consumers needing strict guarantees should refuse to act on the document when this array is non-empty, or branch on the specific code.

`ranking.is_stable_across_runs` is separate from deterministic replay. It
describes ordering across fresh runner executions; it does not weaken the
byte-identical guarantee for repeatedly analyzing one captured run directory.

---

## Invariants

Testable cross-field rules. A v1.6-conformant output satisfies all of them.

1. `truncated == false ⇒ truncation_reason == null`.
2. `emission.failures_emitted == len(failures)` and `emission.failures_emitted <= emission.failures_total`.
3. `emission.mode == "full" ⇒ emission.max_failures == null AND emission.failures_emitted == emission.failures_total`.
4. `emission.mode == "budgeted" ⇒ emission.max_failures != null AND emission.failures_emitted <= emission.max_failures`.
5. `runner_outcome == "timed_out" ⇒ early_exit.reason ∈ {watchdog_silence, watchdog_wallclock, runner_internal_timeout, per_test_timeout}`.
6. `runner_outcome == "passed" ⇒ emission.failures_total == 0 AND counts.tests_failed == 0 AND counts.tests_errored == 0`. A test whose final retry passed (a flake-that-resolved) does NOT appear in `failures[]`; it counts in `tests_passed` and produces `runner_outcome: passed` (assuming no other failing tests).
7. `cascade_root != null ⇔ cascade_reason != null`. A failure cannot name itself as its `cascade_root`. A dependent can point only to a one-level root; a failure that itself has `cascade_root != null` cannot be named as another failure's root. `cascade_root` and `dependent_failure_ids` must reciprocate whenever both referenced failures are present.
8. `parse_status ∈ {"unparsed_output", "unsupported_command"} ⇒ failures == []`.
9. `details.type == kind` (discriminator and payload agree).
10. Every `failure_id` referenced by `cascade_root` MUST exist in this document's `failures[]`. Every entry of `dependent_failure_ids` MUST exist in this document's `failures[]`.
11. For full emissions, each non-null `runner_test_id` may contribute at most one final outcome; two failures with the same `runner_test_id` but different derived final outcomes are invalid. `counts.tests_failed` is the count of unique non-null `runner_test_id`s whose final outcome is `failed`; `counts.tests_errored` is the same for outcome `errored`. Budgeted emissions may omit failures needed to recompute these counts from `failures[]`; in that case the counts remain derived from the adapter's pre-budget full failure set. The final outcome is the `outcome` of the last `retries[]` entry when present; otherwise derived from `phase` and `kind` — `failed` only when `phase == "test"` AND `kind ∈ {assertion, snapshot}`; `errored` otherwise, including `build`, `collect`, `setup`, `teardown`, and `typecheck`.
12. `counts.tests_run >= count(unique non-null runner_test_id in failures[])`. Also, `tests_passed + tests_failed + tests_errored + tests_skipped <= tests_run`. `tests_run` is adapter-sourced from the runner's attempted-test count when available; it is not recomputed solely from `failures[]`.
13. Exactly one `adapters[].selected == true` whenever `parse_status` is `complete` or `partial`. Passed runs still have a selected adapter (the one that parsed the runner output and produced the empty `failures[]`).
14. `companion_diagnostics[]` entries have empty `companion_diagnostics` (depth limit 1).
15. `runner_outcome == "passed" ⇒ exit_code == 0 AND signal == null AND early_exit == null`.

---

## Enum forward-compat policy

- **Producers** MUST NOT emit enum members added after their declared `schema_version`. A v1.6 binary emits only v1.6-defined members.
- **Consumers** SHOULD map unknown enum values to the closed-set fallback for that enum in their own logic. Consumers READ failtrace output; they typically do not emit it. Consumers that re-emit a *transformed* failtrace document (e.g., a tool that wraps failtrace and reshapes its output) SHOULD add a `determinism_warnings: [{code: "schema_version_drift_minor", ...}]` entry to the re-emitted document to surface the gap. Fallback per enum:
  - `kind` → `"unknown"`.
  - `phase` → `"test"` (most common; consumers should treat unknown phase as "best-guess test").
  - `runner_outcome` → `"failed"` (most conservative).
  - `parse_status` → `"unparsed_output"`.
  - Other enums: nearest documented member; if none obviously fits, treat as parse error.

---

## Composite commands and adapter selection

When the user-invoked command expands to multiple supported adapter candidates
(`bash -c "tsc && jest"`) or a recognized package-test script (`npm test`),
v1.6 records the candidate set but does not select one candidate after capture:

- All candidate adapters appear in sidecar `adapter_candidates[]`, each with its own `version`, `kind`, and source-text order.
- The emitted output has `parse_status: "unsupported_command"`, `parse_warnings: ["composite_runtime_deferred"]`, no failures, and no selected adapter.
- Single-adapter command paths still select exactly one adapter when `parse_status` is `complete` or `partial`.
- A future wrapper-level adapter may appear with `kind: "wrapper"` when the wrapper itself produces cascade information (for example setting `cascade_reason: "npm_script_chain"`).

**v1.6 composite scope:** failtrace records candidates for top-level
`bash|sh|zsh -c "X && Y && ..."` chains with at least two simple argv
segments. A single command hidden behind `bash|sh|zsh -c` is not claimed.
Failtrace also inspects `npm|pnpm|yarn test` when
`package.json:scripts.test` resolves to one or more simple top-level `&&`
segments. Package-test scripts always emit `unsupported_command` with
`composite_runtime_deferred`, even when only one runner is recognized, because
implicit pretest/posttest lifecycle hooks can contribute to the same log.
Other package aliases such as `npm run typecheck` are not introspected, and
`bun test` is Bun's built-in test runner rather than package-script dispatch.

Determinism warnings for adapter drift fire per adapter; the warning's `adapter_id` identifies which.

---

## Buffered classification

Failtrace captures the entire raw log before emitting JSON. This is a Design Principle (#3) and enables:

- Lookahead to distinguish `failed` vs `pre_test_failed` vs `harness_crashed` (requires reading until the summary line, or its absence).
- Stable `failures_complete` computation.
- Correct cascade detection (downstream rules need the upstream root to be visible).

Streaming/incremental JSON output is explicitly a v2 concern. v1.x outputs are single documents emitted after the runner exits.

---

> **Example payload hashes.** `failure_id` and `test_group_id` values in the examples below are *computed* under the v1.2 derivation rules (unchanged through v1.6) and are normative test fixtures. **`raw_log_sha256` values in the examples are illustrative placeholders** — the literal characters shown are recognizable as fake (not actual SHA-256 of any real `log` payload). Real `raw_log_sha256` is computed at run time from actual log bytes.

## Example: pytest with collection error

```json
{
  "schema_version": "1.6",
  "document_kind": "run_result",
  "failtrace_version": "0.1.0",
  "emission": {
    "channel": "stdout",
    "mode": "budgeted",
    "max_failures": 10,
    "failures_emitted": 1,
    "failures_total": 1,
    "full_output_path": "/Users/example/.failtrace/runs/2026-05-12T14-23-01Z/output.json"
  },
  "command": "pytest tests/",
  "cwd": "/Users/example/proj",
  "started_at": "2026-05-12T14:23:01Z",
  "duration_ms": 180,
  "exit_code": 4,
  "signal": null,
  "runner_outcome": "pre_test_failed",
  "early_exit": null,
  "adapters": [
    { "id": "pytest", "version": "pytest@0.1.0", "kind": "runner", "selected": true, "selection_reason": "argv_name_match" }
  ],
  "parse_status": "complete",
  "parse_warnings": [],
  "determinism_warnings": [],
  "truncated": false,
  "truncation_reason": null,
  "failures_complete": true,
  "failures": [
    {
      "failure_id": "a28a10bf9c212128",
      "adapter_id": "pytest",
      "phase": "collect",
      "kind": "missing_symbol",
      "runner_package_id": null,
      "runner_test_id": null,
      "test_segments": [],
      "test_name": null,
      "param_id": null,
      "test_group_id": null,
      "primary_location": { "file": "tests/conftest.py", "line": 8, "column": null, "end_line": null, "end_column": null, "role": "import_site", "in_user_code": true },
      "extra_locations": [],
      "message": "ModuleNotFoundError: No module named 'app.fixtures'",
      "diagnostic_code": null,
      "diagnostic_severity": "error",
      "details": {
        "type": "missing_symbol",
        "symbol": "app.fixtures",
        "import_chain": ["tests/conftest.py:8"]
      },
      "raw_log_lines": [1, 4],
      "evidence": {
        "byte_range": { "start": 0, "end": 280 },
        "excerpts": [
          { "source": "raw_log", "kind": "failure_block", "byte_range": { "start": 0, "end": 280 },
            "text": "ImportError while loading conftest '/Users/example/proj/tests/conftest.py'.\ntests/conftest.py:8: in <module>\n    from app.fixtures import sample_db\nE   ModuleNotFoundError: No module named 'app.fixtures'",
            "ansi_stripped": true, "truncated": false }
        ]
      },
      "cascade_root": null,
      "cascade_reason": null,
      "dependent_failure_ids": [],
      "retries": [],
      "companion_diagnostics": [],
      "extensions": {}
    }
  ],
  "ranking": { "strategy": "runner_priority", "is_stable_across_runs": true },
  "counts": { "tests_run": 0, "tests_passed": 0, "tests_failed": 0, "tests_errored": 0, "tests_skipped": 0, "tests_not_run": null, "files_failed": null },
  "raw_log_path": "/Users/example/.failtrace/runs/2026-05-12T14-23-01Z/log",
  "raw_log_sha256": "EXAMPLE0000000000000000000000000000000000000000000000000pytest01",
  "suggested_next_commands": [
    { "argv": ["python", "-c", "import app.fixtures"], "command": "python -c \"import app.fixtures\"", "cwd": "/Users/example/proj", "intent": "rerun_isolated", "interactive": false, "mutates_workspace": false, "uses_network": false, "requires_approval": false }
  ],
  "extensions": {}
}
```

## Example: cargo test with borrow-checker error

```json
{
  "schema_version": "1.6",
  "document_kind": "run_result",
  "failtrace_version": "0.1.0",
  "emission": {
    "channel": "stdout",
    "mode": "budgeted",
    "max_failures": 10,
    "failures_emitted": 1,
    "failures_total": 1,
    "full_output_path": "/Users/example/.failtrace/runs/2026-05-12T14-25-00Z/output.json"
  },
  "command": "cargo test",
  "cwd": "/Users/example/proj",
  "started_at": "2026-05-12T14:25:00Z",
  "duration_ms": 2204,
  "exit_code": 101,
  "signal": null,
  "runner_outcome": "pre_test_failed",
  "early_exit": null,
  "adapters": [
    { "id": "cargo-test", "version": "cargo-test@0.1.0", "kind": "runner", "selected": true, "selection_reason": "argv_name_match" }
  ],
  "parse_status": "complete",
  "parse_warnings": [],
  "determinism_warnings": [],
  "truncated": false,
  "truncation_reason": null,
  "failures_complete": true,
  "failures": [
    {
      "failure_id": "412eb01806c5b512",
      "adapter_id": "cargo-test",
      "phase": "build",
      "kind": "type_error",
      "runner_package_id": null,
      "runner_test_id": null,
      "test_segments": [],
      "test_name": null,
      "param_id": null,
      "test_group_id": null,
      "primary_location": { "file": "src/lib.rs", "line": 34, "column": 5, "end_line": 34, "end_column": 17, "role": "primary", "in_user_code": true },
      "extra_locations": [
        { "file": "src/lib.rs", "line": 32, "column": 17, "end_line": 32, "end_column": 22, "role": "related", "in_user_code": true },
        { "file": "src/lib.rs", "line": 36, "column": 20, "end_line": 36, "end_column": 21, "role": "related", "in_user_code": true }
      ],
      "message": "E0502: cannot borrow `data` as mutable because it is also borrowed as immutable",
      "diagnostic_code": "E0502",
      "diagnostic_severity": "error",
      "details": {
        "type": "type_error",
        "spans": [
          { "file": "src/lib.rs", "line": 32, "column": 17, "end_line": 32, "end_column": 22, "role": "related", "note": "immutable borrow occurs here" },
          { "file": "src/lib.rs", "line": 34, "column": 5, "end_line": 34, "end_column": 17, "role": "primary", "note": "mutable borrow occurs here" },
          { "file": "src/lib.rs", "line": 36, "column": 20, "end_line": 36, "end_column": 21, "role": "related", "note": "immutable borrow later used here" }
        ],
        "explanation_command": "rustc --explain E0502"
      },
      "raw_log_lines": [3, 17],
      "evidence": {
        "byte_range": { "start": 1300, "end": 4120 },
        "excerpts": [
          { "source": "raw_log", "kind": "failure_block", "byte_range": { "start": 1300, "end": 4120 },
            "text": "error[E0502]: cannot borrow `data` as mutable because it is also borrowed as immutable",
            "ansi_stripped": true, "truncated": false }
        ]
      },
      "cascade_root": null,
      "cascade_reason": null,
      "dependent_failure_ids": [],
      "retries": [],
      "companion_diagnostics": [],
      "extensions": {}
    }
  ],
  "ranking": { "strategy": "compile_before_runtime", "is_stable_across_runs": true },
  "counts": { "tests_run": 0, "tests_passed": 0, "tests_failed": 0, "tests_errored": 0, "tests_skipped": 0, "tests_not_run": null, "files_failed": null },
  "raw_log_path": "/Users/example/.failtrace/runs/2026-05-12T14-25-00Z/log",
  "raw_log_sha256": "EXAMPLE00000000000000000000000000000000000000000000cargoBORROW02",
  "suggested_next_commands": [
    { "argv": ["rustc", "--explain", "E0502"], "command": "rustc --explain E0502", "cwd": "/Users/example/proj", "intent": "explain_diagnostic", "interactive": false, "mutates_workspace": false, "uses_network": false, "requires_approval": false }
  ],
  "extensions": {}
}
```

## Example: parameterized pytest, 3 of 17 params failing

`failure_id`s and `test_group_id` are computed from v1.2 derivation rules, unchanged through v1.6. Tests of failtrace assert equality against these fixtures.

```json
{
  "schema_version": "1.6",
  "document_kind": "run_result",
  "failtrace_version": "0.1.0",
  "emission": {
    "channel": "stdout",
    "mode": "budgeted",
    "max_failures": 10,
    "failures_emitted": 3,
    "failures_total": 3,
    "full_output_path": "/Users/example/.failtrace/runs/2026-05-12T14-27-00Z/output.json"
  },
  "command": "pytest tests/test_parser.py",
  "cwd": "/Users/example/proj",
  "started_at": "2026-05-12T14:27:00Z",
  "duration_ms": 3120,
  "exit_code": 1,
  "signal": null,
  "runner_outcome": "failed",
  "early_exit": null,
  "adapters": [
    { "id": "pytest", "version": "pytest@0.1.0", "kind": "runner", "selected": true, "selection_reason": "argv_name_match" }
  ],
  "parse_status": "complete",
  "parse_warnings": [],
  "determinism_warnings": [],
  "truncated": false,
  "truncation_reason": null,
  "failures_complete": true,
  "failures": [
    {
      "failure_id": "58af4b4b1fcb1036",
      "adapter_id": "pytest",
      "phase": "test",
      "kind": "assertion",
      "runner_test_id": "tests/test_parser.py::test_parse[empty-string]",
      "runner_package_id": null,
      "test_name": "test_parse[empty-string]",
      "test_segments": [],
      "param_id": "empty-string",
      "test_group_id": "49507ef118ce763f",
      "primary_location": { "file": "tests/test_parser.py", "line": 41, "column": null, "end_line": null, "end_column": null, "role": "primary", "in_user_code": true },
      "extra_locations": [],
      "message": "AssertionError: expected '' to equal None",
      "diagnostic_code": null,
      "diagnostic_severity": "error",
      "details": { "type": "assertion", "expected": "None", "actual": "''", "diff": null },
      "raw_log_lines": [128, 145],
      "evidence": null,
      "cascade_root": null,
      "cascade_reason": null,
      "dependent_failure_ids": [],
      "retries": [],
      "companion_diagnostics": [],
      "extensions": {}
    },
    {
      "failure_id": "b0782aebe12ab236",
      "adapter_id": "pytest",
      "phase": "test",
      "kind": "assertion",
      "runner_test_id": "tests/test_parser.py::test_parse[trailing-newline]",
      "runner_package_id": null,
      "test_name": "test_parse[trailing-newline]",
      "test_segments": [],
      "param_id": "trailing-newline",
      "test_group_id": "49507ef118ce763f",
      "primary_location": { "file": "tests/test_parser.py", "line": 41, "column": null, "end_line": null, "end_column": null, "role": "primary", "in_user_code": true },
      "extra_locations": [],
      "message": "AssertionError: expected 'foo\\n' to equal 'foo'",
      "diagnostic_code": null,
      "diagnostic_severity": "error",
      "details": { "type": "assertion", "expected": "'foo'", "actual": "'foo\\n'", "diff": null },
      "raw_log_lines": [146, 163],
      "evidence": null,
      "cascade_root": null,
      "cascade_reason": null,
      "dependent_failure_ids": [],
      "retries": [],
      "companion_diagnostics": [],
      "extensions": {}
    },
    {
      "failure_id": "1bbb49abdd90e17e",
      "adapter_id": "pytest",
      "phase": "test",
      "kind": "assertion",
      "runner_test_id": "tests/test_parser.py::test_parse[unicode-bom]",
      "runner_package_id": null,
      "test_name": "test_parse[unicode-bom]",
      "test_segments": [],
      "param_id": "unicode-bom",
      "test_group_id": "49507ef118ce763f",
      "primary_location": { "file": "tests/test_parser.py", "line": 41, "column": null, "end_line": null, "end_column": null, "role": "primary", "in_user_code": true },
      "extra_locations": [],
      "message": "AssertionError: expected '\\ufefffoo' to equal 'foo'",
      "diagnostic_code": null,
      "diagnostic_severity": "error",
      "details": { "type": "assertion", "expected": "'foo'", "actual": "'\\ufefffoo'", "diff": null },
      "raw_log_lines": [164, 181],
      "evidence": null,
      "cascade_root": null,
      "cascade_reason": null,
      "dependent_failure_ids": [],
      "retries": [],
      "companion_diagnostics": [],
      "extensions": {}
    }
  ],
  "ranking": { "strategy": "first_emitted", "is_stable_across_runs": true },
  "counts": { "tests_run": 17, "tests_passed": 14, "tests_failed": 3, "tests_errored": 0, "tests_skipped": 0, "tests_not_run": null, "files_failed": 1 },
  "raw_log_path": "/Users/example/.failtrace/runs/2026-05-12T14-27-00Z/log",
  "raw_log_sha256": "EXAMPLE00000000000000000000000000000000000000000000pytestPARAM03",
  "suggested_next_commands": [
    { "argv": ["pytest", "tests/test_parser.py::test_parse", "-v"], "command": "pytest 'tests/test_parser.py::test_parse' -v", "cwd": "/Users/example/proj", "intent": "rerun_verbose", "interactive": false, "mutates_workspace": false, "uses_network": false, "requires_approval": false }
  ],
  "extensions": {}
}
```

---

## Other document kinds

`failtrace explain` and `failtrace adapters` emit shapes distinct from `run_result`/`analysis_result`. Both share `schema_version` and `document_kind` discriminators; everything else is per-kind.

### `explanation_result` (from `failtrace explain [--run-dir <run-dir>] <failure_id>`)

Without `--run-dir`, `explain` reads only the latest run in the active runs
root. It does not search historical run directories for a matching id.
If the selected run does not contain the requested id, `explain` emits no JSON
and exits `66` rather than reporting an adapter-availability failure.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `schema_version` | string | yes | `"1.6"`. |
| `document_kind` | enum | yes | `"explanation_result"`. |
| `failtrace_version` | string | yes | Failtrace binary version. |
| `failure_id` | string | yes | The 16-hex id this explanation pertains to. |
| `run_dir` | string | yes | Absolute path to the run directory containing the failure. |
| `summary` | string | yes | One-sentence prose summary of what failed. |
| `why_classified` | array of string | yes | Deterministic classification metadata emitted by the binary. In v0.1 this is not guaranteed to be exhaustive or fully adapter-specific; consumers should use the stored `failure` and evidence as authoritative. |
| `matched_rules` | array of object | yes | Each: `{rule_id, description, span: {byte_range}}`. v0.1 emits a classification span but does not promise exhaustive adapter-rule attribution. |
| `raw_log_path` | string | yes | Absolute path to the raw log. |
| `failure` | Failure object | yes | The stored failure object selected by `failure_id`. |
| `raw_log_excerpts` | array of Excerpt | yes | Evidence excerpts stored on that failure; empty when it has no evidence. |
| `suggested_next_commands` | array of Command | yes | The stored run's suggested next commands. |

### `adapters_result` (from `failtrace adapters`)

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `schema_version` | string | yes | `"1.6"`. |
| `document_kind` | enum | yes | `"adapters_result"`. |
| `failtrace_version` | string | yes | Failtrace binary version. |
| `adapters` | array of object | yes | Each: `{id, version, kind, supported_commands: [string], minimum_failure_rule: string, strip_params_rule: string|null, capabilities: [string]}`. The adapters compiled into this binary. `supported_commands` contains representative command spellings, not an exhaustive detector inventory; use the published supported boundary and the installed binary's detector behavior for individual argv forms. |

These shapes are intentionally minimal in v1.6; they exist so consumers have a stable contract for those commands. They may expand in future minor versions.

## Adapter appendix

Each adapter declares its **minimum-failure rule**, **`in_user_code` denylist**, **`strip_params` rule** (or null), **`param_id` extraction rule**, and **`diagnostic_severity` mapping**.

### pytest

- **Minimum-failure rule.** A line matching `^FAILED |^ERROR ` with a test identifier and message, OR a `=+ FAILURES =+` / `=+ ERRORS =+` section followed by at least one `_+ <test_name> _+` traceback.
- **`in_user_code` denylist (path components):** `.venv`, `venv`, `__pypackages__`, `.tox`, `.nox`, `site-packages`.
- **`strip_params` rule.** If `runner_test_id` ends with `[<param-id>]`, remove the trailing bracketed segment.
- **`param_id` extraction.** Content inside the trailing `[...]` of the nodeid. Null when nodeid has no bracket.
- **`diagnostic_severity` mapping.** Assertion failures and most runtime errors → `error`. Collected `PytestWarning` → `warning`.

### jest / vitest

- **Minimum-failure rule.** Recap-section attribution plus a `FAIL ` line anchored on ` FAIL  <path> > <rest>` parse.
- **`in_user_code` denylist:** `node_modules`, `.yarn`, `.yarn/cache`, `.pnpm-store`.
- **`strip_params` rule.** None in v1.6; `test_group_id` is null.
- **`param_id` extraction.** None in v1.6; `param_id` is null.
- **`diagnostic_severity` mapping.** All identified failures → `error`. (Jest/vitest do not expose a per-failure severity distinct from "this test failed.")

### go test (`gotest`)

- **Minimum-failure rule.** `go test -json` event stream only. Build failures come from `build-fail` plus preceding `build-output` diagnostics. Test failures come from terminal test `fail` events with assertion diagnostics, panic output, timeout output, or race-detector output. A `go test` command without `-json` is intentionally `parse_status: "unparsed_output"` with a suggested rerun that inserts `-json`.
- **`in_user_code` denylist:** Go standard-library `testing`, `runtime`, `internal/runtime`, and `time` frames. Adapter output keeps Go stack paths portable by emitting the file basename in `Frame.file` / `primary_location.file`.
- **`strip_params` rule.** None. Subtest segments separated by `/` are NOT treated as parameters; `test_group_id` is null.
- **`param_id` extraction.** None; null.
- **`diagnostic_severity` mapping.** All identified failures → `error`.
- **Failure extensions.** Ambiguous slash-containing subtest names carry `extensions.go.subtest_name_ambiguous = true` on that failure. Malformed JSONL lines and unknown Action verbs are counted under top-level `extensions.go`. Malformed lines also emit `malformed_structured_event`, degrade `parse_status` to `partial`, and may make `failures_complete` false; unknown actions alone do not add a parse warning.

### cargo test

- **Minimum-failure rule.** A `---- <name> stdout ----` section terminated by `thread '<name>' panicked at` or `failures:`, OR rustc `error[E\d+]:` / `error:` diagnostic before any test ran.
- **`in_user_code` denylist:** `target`.
- **`strip_params` rule.** None in v1.6; null.
- **`param_id` extraction.** None in v1.6; null.
- **`diagnostic_severity` mapping.** rustc `error` → `error`, `warning` → `warning`, `note` → `info`, `help` → `hint`.

### tsc

- **Minimum-failure rule.** A line matching `^[^\s].+\(\d+,\d+\): error TS\d+:` (the default non-pretty "listed" form emitted without `--pretty`) or the pretty form `^[^\s].+:\d+:\d+ - error TS\d+:` emitted with `--pretty`.
- **`in_user_code` denylist:** `node_modules`.
- **`strip_params` rule.** None; null.
- **`param_id` extraction.** None; null.
- **`diagnostic_severity` mapping.** tsc `error` → `error`, `warning` → `warning`, `suggestion` → `info`, `message` → `hint`.
- **Failure extensions.** Every tsc failure carries `extensions.tsc.column_unit = "utf16_code_unit"` and `extensions.tsc.phase_detail = "typecheck"`. These keys are adapter metadata only; they do not participate in `failure_id`, normalization, invariants, ranking, or counts.

### xcodebuild test (`xcodebuild-test`)

- **Supported input.** Raw text from `xcodebuild test` or `xcodebuild test-without-building` against `.xcodeproj` projects, including supported multi-action forms and an optional `xcrun` prefix. Logs transformed by `xcpretty` or `xcbeautify` and `.xcresult` bundle parsing are outside the v0.1 contract.
- **Minimum-failure rule.** The sidecar must select `xcodebuild-test`, and the log must contain at least one recognized xcodebuild test marker: an extracted build/test/destination failure, a terminal `** TEST ... **` marker, an XCTest suite envelope, a Swift Testing run envelope, a `Testing failed:` summary, or a recognized pre-test runtime-environment failure.
- **Failure extraction.** XCTest assertion lines and Swift Testing issue lines lower to test-phase failures. Compiler diagnostics, `ld:` diagnostics, undefined-symbol blocks, and destination-resolution errors lower to pre-test failures. Summary and terminal markers are reconciled against extracted failures; a shortfall emits `runner_summary_mismatch` and sets `failures_complete: false`.
- **Execution counts.** XCTest uses the largest occurrence per bundle name (first wins ties), summing across names. Unclosed occurrences use their largest inner summary; without bundles, the largest summary is the fallback. Skips come from the selected summaries. Swift Testing run-summary totals accumulate, folding pending per-test skips exactly once per summary and resetting pending skips at a new run start. Suite skips are not counted again. Harness totals add; `tests_run` is floored at unique extracted failed-test identities plus skips. Count witnesses and restart markers degrade reliability independently of failure coverage.
- **Failure coverage.** XCTest compares extracted assertion incidents with the maximum nested summary reconciled against accumulated outer execution summaries; `unexpected` is not added again. Swift Testing takes the maximum overlapping issue evidence within each epoch and sums disjoint epochs, retaining earlier evidence across restarts. Ambiguous summary scopes are untrusted. `Failing tests:` preserves terminal identity multiplicity as a coverage witness but never creates failures. Broad `✔`/`✘`/`━` run-summary recognition preserves execution totals with unfamiliar suffixes; passing known-issue summaries are not missing-failure evidence. Completeness is computed before budgeting and requires no truncation, missing terminal event, or untrusted failure coverage. See the [adapter design](design/xcodebuild-test-adapter.md) for the independent accounting rules.
- **`in_user_code` rule.** Source locations emitted by the runner are marked as user code. Synthetic `<linker>` and `<xcodebuild>` locations are not user code. v0.1 does not apply a cwd-boundary or dependency-path denylist in this adapter.
- **`strip_params` rule.** None; `test_group_id` is null.
- **`param_id` extraction.** None; `param_id` is null. Swift Testing argument-label syntax remains part of the normalized test identifier.
- **`diagnostic_severity` mapping.** Every extracted failure is `error`.
- **Ranking stability.** Build-only failure ordering is stable across runs. Any test-phase failure sets `ranking.is_stable_across_runs: false` because XCTest and Swift Testing may execute concurrently.

Where an adapter applies path-component matching, it uses post-normalization paths (separators → `/`, `.`/`..` resolved lexically, case-folded per sidecar `path_case_folding`). **Symlinks are NOT resolved by failtrace** — emitted paths preserve whatever form the runner produced. If the runner emitted a resolved path, failtrace passes it through resolved; if the runner emitted a symlinked path, failtrace passes it through unresolved. This keeps replay deterministic across machines where realpath would differ. The adapter-specific rules above are authoritative for `in_user_code`, including any cwd/workspace boundary behavior.

The adapter-specific ranking rules above are authoritative. Unsupported
long-running modes such as `tsc --watch` do not produce an analysis document.

---

## Rationale (by field group)

### Why per-run directory and sidecar

`failtrace last` reads `output.json` in O(1) without re-analyzing. `failtrace analyze` reads `log` + `meta.json`. `output.json` remains the full source of truth even when stdout emits a budgeted projection for agent runtime display limits. A directory groups all of this cleanly and leaves room for future per-run adjuncts (junit-xml export, runner-native artifacts).

### Why `emission` is top-level

Agent runtimes impose practical stdout limits, and some truncate or merge streams without preserving a side file. v1.4 therefore allows stdout to carry a top-N `failures[]` projection while `<run-dir>/output.json` keeps the full document. `emission` is top-level because completeness is a property of the document the consumer is reading, not of an adapter, failure, or extension namespace.

### Why `analyze` requires the sidecar

`command`, `exit_code`, `signal`, and `truncated` are not recoverable from runner stdout alone. The sidecar carries them, plus any captured `early_exit` override that the runner log does not encode. When that field is null, adapters may re-derive a runner-reported early exit from the captured log and the other sidecar fields. A degraded `--no-meta` mode was considered and rejected: emitting an output that omits required keys violates the schema.

### Why `runner_outcome` was split

v1.1's `aborted` conflated "tool exited normally by design" (cargo build error, tsc typecheck error) with "harness crashed." v1.2 split test-runner pre-test failures from harness crashes. v1.6 further splits typecheck-only failures into `typecheck_failed`, because `pre_test_failed` implies a test phase that a typechecker never has. The decisive test for test runners is still the runner's summary line — `failed` requires both "tests ran" AND "summary present."

### Why `failure_id` was widened to 16 hex

8 hex = 32 bits → 50% collision at ~65k failures (birthday paradox). A Go binary panicking across 10k subtests under `go test ./...` passes this threshold. 16 hex = 64 bits → collision-resistant at any realistic scale.

### Why `failure_id` includes `adapter_id` and `param_id`

`adapter_id`: composite runs may have two adapters producing structurally identical failures; including it prevents collision. `param_id`: vitest table tests can have identical runner_test_id and message-fragment-after-normalize for different params; `param_id` disambiguates without dragging full parameter JSON through the hash. (Vitest does not populate param_id in v1.4; runner_test_id carries the interpolated title and disambiguates table-test failures.)

### Why `parameters` was replaced by `param_id`

Pytest's nodeid carries the parameter *value* but not the parameter variable name. The v1.1 schema's `parameters: {input_case: "empty-string"}` was inventing the `input_case` key. Replacing with `param_id: "empty-string"` removes the fiction; adapters that recover named parameter dictionaries can put them under `extensions.<adapter>.parameter_map` without affecting core IDs.

### Why `normalize(message)` order was tightened

v1.1's order placed digit-replace before cwd-replace. A cwd like `/Users/example/repo-2024/` would be digit-substituted to `/Users/example/repo-<N>/` before cwd-replace could match the original prefix — same test, two engineers, different `failure_id`s. v1.2 puts cwd-replace first (and uses literal-prefix matching, not regex). Hex-before-decimal prevents `0x1234` → `0x<N>`. NFC normalization on both message and cwd handles composed/decomposed Unicode forms. ASCII-only regex mode makes digit-matching locale-independent.

### Why structured diffs replaced opaque strings

v1.1's `details.snapshot.diff: string` lost structural information (which lines added vs removed, how many omitted). v1.2's `Diff` object carries per-line ops plus `omitted_line_count`, enabling agents to render or summarize without re-parsing diff syntax.

### Why `dependent_failure_ids` is denormalized

The reverse direction (root → dependents) saves an O(N) scan per query. Bidirectional pointers make "what got killed because of this panic?" an O(1) lookup. `cascade_root` is authoritative for correctness; `dependent_failure_ids` is the derived mirror with explicit invariants.

### Why `actionability` was NOT added

It would have been pure denormalization of three existing fields (ranking-position, `cascade_root`, `retries[].last.outcome`). The "convenience" — one field lookup vs three — was judged not worth a fourth field that could drift from the three it summarizes. Consumers compute the predicate. See "Rejected alternatives."

### Why `stable_id` was NOT added

The cold-draft schema proposed a separate stable cross-run identifier. The proposal depended on an unspecified `message_fingerprint` algorithm. v1.1's `failure_id` is already stable under whitespace edits and parameter reordering; renames are rare and best handled by consumers. Deferred until real adopter demand justifies a cross-rename dedup contract.

### Why `canonical_json_sha256` was NOT added

It would have required a canonical-JSON serializer spec, env-field substitution rules, self-field exclusion rules — substantial machinery for a self-referential conformance check. Conformance against `output.json` works via byte-comparison. Deferred until the first adapter's fixture suite stabilizes.

### Why `argv` is authoritative alongside `command`

Shell-quoted strings are a bug surface: pytest nodeids contain `[`, `]`, `'`; Windows quoting differs; consumers parsing the field as a single shell line will sometimes get it wrong. `argv` removes the ambiguity. `command` remains as a human-readable display string.

### Why `companion_diagnostics` depth-limited to 1

Recursive companion chains would let adapters surface arbitrarily deep nested context, defeating the schema's "one actionable failure with concise companions" principle. Depth 1 captures the realistic cases (race-detector + assertion, thread-dump + timeout) without inviting deeper nesting.

### Why `extensions` is opaque to core

Per-adapter overflow valves let adapters carry rich data (pytest session_id, tsc column_unit, go race-id) without contract churn. The strict rule — extensions never feed core IDs or invariants — keeps the core schema consistent and lets consumers safely ignore `extensions` they don't understand.

### Why buffered classification is mandated

Streaming classification would force adapters to commit to `failed` vs `harness_crashed` before seeing the runner's summary (or absence thereof). Buffered classification has full lookahead, enabling deterministic decisions. Streaming output is a v2 concern when it has real consumers.

---

## Rejected alternatives

Alternatives considered across the v0, v1.0, and v1.1 design cycles, with brief reasons for each decision.

### v0/v1 rejections (carried forward)

- **`primary_failure` + `additional_failures` shape** — arbitrary in parallel/multi-file cases; `failures[]` + `ranking.strategy` is the honest replacement.
- **Single `category` enum** — collapsed orthogonal lifecycle and shape dimensions; `phase` + `kind` split.
- **Float `adapter_confidence`** — no calibration corpus; replaced by `parse_status` + `parse_warnings`.
- **Open string set for `kind`** — pushes category logic back to consumers; closed enum + `unknown` valve is contract-friendlier.
- **Top-level `flaky: bool`** — derived from `retries`.
- **Run-level `correlation` enum** — recoverable from `ranking.strategy` + per-failure `cascade_root`.
- **Byte offsets v0 `raw_log_offset`** — added back in v1.2 as `evidence.byte_range`, but as half-open offsets into raw bytes, not as a parallel "second authoritative" coordinate to `raw_log_lines`.
- **Widened determinism exemption list** — would make the contract vacuous; sidecar approach taken instead.
- **`analyze --no-meta`** — degraded mode omitting required keys violates the schema; future `failtrace ingest` will handle third-party logs.
- **Global `in_user_code` denylist** — ecosystem-specific; per-adapter declarations.

### v1.0 → v1.1 rejections

- **Expand env-fields exemption to cover `command`, `exit_code`, etc.** — would make the determinism contract vacuous. Sidecar is the right answer.
- **Keep `aborted` as one enum value** — overloaded "exited normally by design" with "harness blew up." Split.

### v1.1 → v1.2 rejections

**From the cold draft (rejected as baseline; some elements grafted):**
- **`primary_failure` denormalized pointer** — two sources of truth for "which one to act on."
- **`process.status` collapsing runner outcome with process termination** — exact bug `runner_outcome` was designed to avoid.
- **`identity.stability` self-attested booleans** — adapter version-level properties masquerading as per-instance facts.
- **`document_kind: last_result`** — `failtrace last` is byte-faithful replay of the stored document, not a new kind.
- **`stream_merge: "separate"`** — conflicts with single-`log`-byte coordinate space; truly separate streams deferred to v2.
- **Required full-capture `raw_log_sha256` computed eagerly blocking JSON emission** — adopted, but spec'd to allow streaming-friendly computation (not a v1.2 concern since buffered classification is mandated anyway).
- **`canonical_json_sha256`** — self-referential serializer spec deferred until adapter fixtures exist.
- **Separate `stable_id` with unspecified `message_fingerprint`** — deferred; v1.1 `failure_id` already handles common cases.
- **`actionability` enum** — pure denormalization of three other fields; consumers compute.

**Additional contract review:**
- **`argv` replacing `command`** — kept both; `argv` authoritative, `command` display-only.

### v1.2 → v1.3 rejections

- **Rename `failures[]` → `incidents[]`.** Rejected: `failures` is the right name for "things to fix now"; renaming would break every consumer that already references the field. The fix is the prose (state that flake-passed tests don't appear), not the field name.
- **Add `flaky_passes: [...]` field for retried-but-finally-passed tests.** Considered. Deferred to a future minor version. v1.3 elides this history; real adopters who want it can push for the field.
- **Replace `companion_diagnostics` array with a separate top-level `diagnostics: {by_failure_id: ...}` map.** Considered. Rejected: a per-failure array is easier for adapters to populate adjacent to the parent failure; the map indirection saves nothing.
- **Make `selected_adapter` a string ID at the top level (not a boolean on each adapter entry).** Considered. The boolean-per-entry is more explicit about which adapters were considered; a single-string field hides the unselected adapters and breaks composite-command auditability.
- **Define `canonical_json_sha256` minimally and ship it.** Considered, deferred (third time). Rationale unchanged from v1.2: self-referential serializer spec without adapter fixtures to test against would invite drift.

### v1.1 things that remained right (not changed)

- Flat ranked `failures[]` with explicit `ranking.strategy`.
- Phase + kind orthogonal closed enums.
- `cascade_root` with whitelisted `cascade_reason` rules.
- Per-adapter `strip_params` rule (rather than global).
- Per-adapter `in_user_code` denylist.
- Per-run directory layout.
- `failtrace analyze` requires sidecar.
- Determinism contract carve-out for `adapter_version_drift`.
