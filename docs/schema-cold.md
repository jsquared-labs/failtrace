# Failtrace JSON Output Schema Cold Draft

> **Historical, non-normative artifact.** This pre-implementation draft is retained to document early design exploration. It does not describe the shipped CLI and must not be used to implement a producer or consumer. Use the [current schema contract](schema.md) instead.

**Schema version:** `1.0.0-cold-draft`

**Status:** archived design draft; not an implementation commitment or compatibility contract.

**Audience:** adapter authors and programmatic consumers, especially AI coding agents and CI scripts.

**Scope:** the single JSON document emitted by `failtrace run`, `failtrace analyze`, `failtrace last`, `failtrace explain`, and `failtrace adapters`.

**Non-goals:** CLI flags, Rust module layout, adapter trait design, fixture format, terminal UI, MCP shape, and code-edit suggestions.

---

## 1. Design Goals

Failtrace turns noisy command output into a deterministic, structured summary of what an agent should act on first. The JSON contract must support:

1. A stable way to find the first actionable failure.
2. Enough surrounding evidence to recover when parsing is incomplete.
3. Multiple runner families with very different diagnostic shapes.
4. Independent adapter versions under one schema version.
5. Byte-identical output for identical captured inputs and parser versions, except for explicitly enumerated run-environment fields.

The schema is optimized for machines, not terminal readers. Human readability is secondary to stable keys, explicit enums, deterministic ordering, and recoverable evidence.

---

## 2. Top-Level Shape

All primary commands emit one JSON object with this wrapper:

```json
{
  "schema_version": "1.0.0",
  "document_kind": "run_result",
  "producer": {},
  "command": {},
  "capture": {},
  "classification": {},
  "failures": [],
  "primary_failure": null,
  "next_actions": [],
  "warnings": [],
  "links": {}
}
```

`document_kind` selects the payload contract:

| Command | `document_kind` | Meaning |
|---|---|---|
| `failtrace run` | `run_result` | Command was executed, captured, then classified. |
| `failtrace analyze` | `analysis_result` | Existing captured artifact was re-classified. |
| `failtrace last` | `run_result` or `analysis_result` | Exact stored document from the selected prior run, with retrieval metadata added. |
| `failtrace explain` | `explanation_result` | Deterministic explanation of a stored run or failure. |
| `failtrace adapters` | `adapters_result` | Machine-readable adapter inventory. |

### Rationale

A single wrapper keeps consumers simple. The alternative is one unrelated schema per command. That would make `last` and `explain` harder because consumers would need different discovery paths for the same facts. `document_kind` gives each command room while keeping common fields stable.

---

## 3. JSON Rules

### 3.1 Required Keys

For `run_result` and `analysis_result`, every top-level key listed below is required:

| Key | Type | Nullable | Notes |
|---|---:|---:|---|
| `schema_version` | string | no | Semver string. |
| `document_kind` | string enum | no | `run_result` or `analysis_result`. |
| `producer` | object | no | Failtrace binary and adapter versions. |
| `command` | object | no | Requested command and observed process result. |
| `capture` | object | no | Raw log artifact metadata. |
| `classification` | object | no | Parser outcome and summary. |
| `failures` | array | no | Zero or more parsed failures. |
| `primary_failure` | object or null | yes | The selected first actionable failure. |
| `next_actions` | array | no | Suggested commands only. |
| `warnings` | array | no | Deterministic schema/parser warnings. |
| `links` | object | no | Optional references to related local artifacts. |

Objects should include all required keys even when values are `null` or empty arrays. Optional extension keys are allowed only under documented `extensions` objects.

### 3.2 Nullability

Use `null` only when a field is meaningful but unknown or not applicable. Use an empty array when the concept exists but has no entries. Do not omit required fields.

| Situation | Field value |
|---|---|
| No source location was parsed | `"location": null` |
| Source location exists but no column | `"column": null` |
| No related spans | `"related": []` |
| No parsed failures | `"failures": []` and `"primary_failure": null` |

### 3.3 Ordering

All arrays have deterministic order. Object key ordering is not semantically meaningful, but canonical serialization for fingerprints uses sorted keys.

| Array | Order |
|---|---|
| `failures` | Primary ranking order, then stable tie-breakers. |
| `failure.locations.related` | Emission order in raw log. |
| `failure.evidence.excerpts` | Raw-log byte order. |
| `failure.attempts` | Attempt index ascending. |
| `next_actions` | Most useful first by deterministic rule. |
| `producer.adapters` | Adapter resolution order. |

### 3.4 String Encoding

All JSON strings are UTF-8. Raw log bytes are never embedded wholesale. Excerpts are normalized text derived from raw bytes according to `producer.normalization`.

If input bytes are invalid UTF-8, adapters must use deterministic replacement and set `producer.normalization.encoding` accordingly.

---

## 4. Top-Level Fields

### 4.1 `producer`

```json
{
  "failtrace_version": "0.1.0",
  "schema_version": "1.0.0",
  "schema_profile": "default",
  "binary_id": "failtrace-rust-static",
  "adapters": [
    {
      "id": "pytest",
      "version": "0.3.0",
      "kind": "runner",
      "selected": true,
      "selection_reason": "argv_name_match"
    }
  ],
  "normalization": {
    "ansi": "stripped_with_offsets",
    "newlines": "lf",
    "encoding": "utf-8-lossless-or-replacement",
    "path_style": "as_observed"
  }
}
```

| Field | Type | Required | Meaning |
|---|---:|---:|---|
| `failtrace_version` | string | yes | Binary version. |
| `schema_version` | string | yes | Same as top-level, repeated for local consumers. |
| `schema_profile` | string | yes | `default` in v1. Reserved for future strict/minimal profiles. |
| `binary_id` | string | yes | Stable binary family id, not a build hash. |
| `adapters` | array | yes | Adapter versions used or considered. |
| `normalization` | object | yes | Text normalization policy used before parsing. |

Adapter object:

| Field | Type | Required | Meaning |
|---|---:|---:|---|
| `id` | string enum | yes | `pytest`, `vitest`, `jest`, `go-test`, `cargo-test`, `tsc`, or `generic`. |
| `version` | string | yes | Adapter semver independent from schema. |
| `kind` | string enum | yes | `runner`, `compiler`, `orchestrator`, or `fallback`. |
| `selected` | boolean | yes | Whether this adapter produced parsed failures or process classification. |
| `selection_reason` | string enum | yes | Deterministic reason. |

`selection_reason` enum:

| Value | Meaning |
|---|---|
| `explicit_adapter` | User selected adapter. |
| `argv_name_match` | Executable name matched adapter rule. |
| `log_signature_match` | Raw log matched adapter signature. |
| `composite_command_component` | Adapter parsed one component of a composite command. |
| `fallback_after_no_match` | Generic fallback parser used. |
| `not_selected` | Adapter listed but unused. |

### 4.2 `command`

```json
{
  "argv": ["npm", "test"],
  "display": "npm test",
  "cwd": "/repo",
  "env_policy": "redacted",
  "started_at": "2026-05-12T22:00:00Z",
  "ended_at": "2026-05-12T22:02:00Z",
  "duration_ms": 120000,
  "process": {
    "status": "timeout",
    "exit_code": null,
    "signal": null,
    "termination": {
      "kind": "watchdog_timeout",
      "reason": "silent_timeout",
      "detail": "No stdout or stderr bytes observed for 120000 ms.",
      "observed_exit_code": null
    }
  }
}
```

| Field | Type | Required | Determinism class |
|---|---:|---:|---|
| `argv` | array of strings | yes | deterministic input |
| `display` | string | yes | derived from argv |
| `cwd` | string | yes | environment-varying |
| `env_policy` | string enum | yes | config |
| `started_at` | RFC3339 string or null | yes | environment-varying |
| `ended_at` | RFC3339 string or null | yes | environment-varying |
| `duration_ms` | integer or null | yes | environment-varying |
| `process` | object | yes | captured |

`env_policy` enum:

| Value | Meaning |
|---|---|
| `not_captured` | Environment was not captured. |
| `redacted` | Environment metadata was captured only after deterministic redaction. |
| `names_only` | Environment variable names were captured, not values. |

`process.status` enum:

| Value | Meaning |
|---|---|
| `passed` | Process succeeded and no failures parsed. |
| `failed` | Process exited non-zero and at least one failure or diagnostic was parsed. |
| `failed_unparsed` | Process exited non-zero but no structured failure was parsed. |
| `timeout` | Failtrace killed the process due to configured timeout or silence watchdog. |
| `killed` | Process was externally killed or signal-terminated. |
| `error` | Failtrace could not start or monitor the command. |

`termination.kind` enum:

| Value | Meaning |
|---|---|
| `none` | Normal process exit. |
| `exit_code` | Process returned an exit code. |
| `signal` | Signal termination observed directly. |
| `watchdog_timeout` | Failtrace killed process after timeout or silence. |
| `probable_oom` | Exit code, signal, or captured text matched deterministic OOM rules. |
| `spawn_error` | Command could not be started. |
| `unknown` | Non-normal termination but no deterministic subtype. |

### 4.3 `capture`

```json
{
  "artifact_id": "cap_01HX2K7Q9Y4E6ZQ8V8Y6J3P2M0",
  "raw_log_path": ".failtrace/runs/2026-05-12T220000Z/raw.log",
  "raw_log_sha256": "3f0c1aa7b5e1c42b4fdcb8f116bacee508d1b9a2f3e2c0d99f57ab4e0b5dbb28",
  "raw_log_bytes": 184233,
  "stdout_bytes": 90210,
  "stderr_bytes": 94023,
  "stream_merge": "stdout_stderr_interleaved",
  "truncated": false,
  "truncation": null,
  "redactions": []
}
```

| Field | Type | Required | Meaning |
|---|---:|---:|---|
| `artifact_id` | string | yes | Stable local artifact id. |
| `raw_log_path` | string | yes | Always present for `run`; input path for `analyze`. |
| `raw_log_sha256` | string | yes | SHA-256 of raw captured bytes. |
| `raw_log_bytes` | integer | yes | Byte length. |
| `stdout_bytes` | integer or null | yes | Captured stdout bytes, if available. |
| `stderr_bytes` | integer or null | yes | Captured stderr bytes, if available. |
| `stream_merge` | enum | yes | How streams were captured. |
| `truncated` | boolean | yes | Whether failtrace truncated capture. |
| `truncation` | object or null | yes | Limit details when truncated. |
| `redactions` | array | yes | Deterministic redactions applied by failtrace, if any. |

`stream_merge` enum:

| Value | Meaning |
|---|---|
| `stdout_stderr_interleaved` | Failtrace preserved observed interleaving. |
| `stdout_then_stderr` | Streams captured separately and concatenated. |
| `combined_by_child` | Child command merged streams before failtrace. |
| `unknown` | Source artifact does not preserve stream metadata. |

`truncation` object when present:

```json
{
  "limit_bytes": 10485760,
  "kept": "head_and_tail",
  "omitted_bytes": 24576
}
```

`redactions` object when present:

```json
{
  "kind": "env_value",
  "label": "TOKEN",
  "replacement": "[REDACTED_ENV_TOKEN]",
  "count": 2
}
```

Redaction must be deterministic. Redaction rules must not use a network service or model.

### 4.4 `classification`

```json
{
  "status": "classified",
  "summary": "3 pytest parameterized cases failed",
  "primary_failure_id": "fail_96b4b1d4e597",
  "failure_count": 3,
  "actionable_failure_count": 3,
  "phases_observed": ["test"],
  "primary_phase": "test",
  "cascade": {
    "detected": false,
    "root_failure_ids": ["fail_96b4b1d4e597"],
    "dependent_failure_ids": [],
    "rule": "none"
  }
}
```

| Field | Type | Required | Meaning |
|---|---:|---:|---|
| `status` | enum | yes | Parser outcome. |
| `summary` | string | yes | Short deterministic summary. |
| `primary_failure_id` | string or null | yes | Id of selected first actionable failure. |
| `failure_count` | integer | yes | `failures.length`. |
| `actionable_failure_count` | integer | yes | Count where `actionability != not_actionable`. |
| `phases_observed` | array | yes | Distinct phases in failures. |
| `primary_phase` | enum or null | yes | Phase of primary failure. |
| `cascade` | object | yes | Deterministic cascade grouping. |

`classification.status` enum:

| Value | Meaning |
|---|---|
| `classified` | At least one structured failure parsed. |
| `no_failure` | Command passed or log had no failure. |
| `unparsed_failure` | Failure occurred but only raw log is available. |
| `partial` | Some structured failures parsed but capture was incomplete or ambiguous. |
| `capture_error` | Failtrace failed before meaningful parsing. |

### 4.5 `failures`

Failure object:

```json
{
  "id": "fail_96b4b1d4e597",
  "identity": {},
  "rank": 1,
  "phase": "test",
  "kind": "assertion",
  "actionability": "primary",
  "status": "failed",
  "tool": {},
  "label": "test_addition[negative]",
  "message": "assert -1 == 1",
  "locations": {},
  "diagnostic": {},
  "attempts": [],
  "evidence": {},
  "relationships": {},
  "extensions": {}
}
```

Required fields:

| Field | Type | Nullable | Meaning |
|---|---:|---:|---|
| `id` | string | no | Document-local stable id derived from identity. |
| `identity` | object | no | Identity components and hash. |
| `rank` | integer | no | 1-based action ranking. |
| `phase` | enum | no | Where failure occurred. |
| `kind` | enum | no | Diagnostic class. |
| `actionability` | enum | no | Whether agent should act on it directly. |
| `status` | enum | no | Final status after retries, if known. |
| `tool` | object | no | Runner/compiler that emitted this failure. |
| `label` | string | no | Stable short label. |
| `message` | string | no | Primary deterministic message. |
| `locations` | object | no | Primary and related source locations. |
| `diagnostic` | object | no | Structured diagnostic payload. |
| `attempts` | array | no | Retry/flakiness observations. |
| `evidence` | object | no | Raw-log excerpts and offsets. |
| `relationships` | object | no | Cascade/sibling links. |
| `extensions` | object | no | Namespaced adapter-specific data. |

#### `phase` enum

| Value | Meaning |
|---|---|
| `setup` | Tool setup/config loading before build or collection. |
| `build` | Build/compile phase before tests execute. |
| `collection` | Test discovery, collection, or import. |
| `test` | Test body execution. |
| `teardown` | Post-test teardown or fixture cleanup. |
| `snapshot` | Snapshot comparison/update phase. |
| `typecheck` | Static type checking. |
| `runtime` | Runtime crash outside a specific test. |
| `process` | Process-level failure such as timeout, OOM, or spawn error. |
| `unknown` | Phase could not be classified deterministically. |

#### `kind` enum

| Value | Meaning |
|---|---|
| `assertion` | Assertion or expectation failed. |
| `snapshot_mismatch` | Snapshot mismatch. |
| `compile_error` | Compiler/build diagnostic. |
| `type_error` | Static type error. |
| `import_error` | Import/module loading error. |
| `panic` | Panic, crash, unhandled exception. |
| `timeout` | Timeout or silence watchdog. |
| `oom` | Deterministic probable OOM. |
| `signal` | Signal termination not classified as OOM. |
| `runner_error` | Test runner internal/config error. |
| `unknown_failure` | Failure detected but kind unknown. |

#### `actionability` enum

| Value | Meaning |
|---|---|
| `primary` | First failure failtrace recommends acting on. |
| `actionable` | Directly actionable but not primary. |
| `dependent` | Likely caused by another parsed failure. |
| `not_actionable` | Informational or superseded by retry/pass. |
| `unknown` | Actionability could not be determined. |

#### `status` enum

| Value | Meaning |
|---|---|
| `failed` | Failure remains failing. |
| `passed_after_retry` | Failed first, passed in a later attempt. |
| `failed_after_retries` | Failed across all observed attempts. |
| `skipped` | Runner marked skipped after an earlier event. |
| `unknown` | Final state unavailable. |

### 4.6 `identity`

```json
{
  "stable_id": "ftid_v1_849f236b508f8e5e",
  "algorithm": "failtrace-identity-v1",
  "components": {
    "adapter_id": "pytest",
    "phase": "test",
    "kind": "assertion",
    "symbol": "tests/test_math.py::test_addition[negative]",
    "primary_path": "tests/test_math.py",
    "primary_line": null,
    "diagnostic_code": null,
    "message_fingerprint": "pytest_assert_equal"
  },
  "stability": {
    "stable_under_line_drift": true,
    "stable_under_message_drift": true,
    "stable_under_param_reordering": true
  }
}
```

Identity has two ids:

| Field | Scope | Meaning |
|---|---|---|
| `failure.id` | document-local | Short deterministic id for links inside one document. |
| `identity.stable_id` | cross-run | Used by agents to ask whether this is the same failure. |

`identity.stable_id` is computed as:

```text
ftid_v1_ + first_16_hex(
  sha256(canonical_json(identity.components))
)
```

`failure.id` is computed as:

```text
fail_ + first_12_hex(sha256(identity.stable_id))
```

Identity component rules:

| Component | Rule |
|---|---|
| `adapter_id` | Adapter that classified the failure. |
| `phase` | Classified phase. |
| `kind` | Classified kind. |
| `symbol` | Best stable test/diagnostic symbol, such as pytest nodeid or rustc error anchor. |
| `primary_path` | Normalized as observed relative path when available. |
| `primary_line` | Included for compiler diagnostics and collection errors; excluded for test assertions when a stable test id exists. |
| `diagnostic_code` | Compiler/test code such as `E0502` or TypeScript error code. |
| `message_fingerprint` | Deterministic coarse fingerprint, not full message. |

Parameterized tests must include the parameter case identity when the runner exposes one. Reordering sibling cases does not change identity. If a runner emits duplicate indistinguishable failures, append `occurrence_index` to components using raw-log order.

### 4.7 `tool`

```json
{
  "adapter_id": "cargo-test",
  "adapter_version": "0.1.0",
  "emitter": "rustc",
  "emitter_version": null,
  "framework": "cargo-test"
}
```

| Field | Type | Required | Meaning |
|---|---:|---:|---|
| `adapter_id` | string | yes | Adapter that parsed this failure. |
| `adapter_version` | string | yes | Adapter semver. |
| `emitter` | string | yes | Actual tool that emitted diagnostic, if known. |
| `emitter_version` | string or null | yes | Parsed version only if in captured bytes. |
| `framework` | string | yes | User-facing runner/compiler family. |

No version should be discovered by executing extra commands. If the raw log does not contain it, `emitter_version` is `null`.

### 4.8 `locations`

```json
{
  "primary": {
    "path": "src/lib.rs",
    "line": 34,
    "column": 5,
    "end_line": 34,
    "end_column": 12,
    "role": "primary",
    "label": "mutable borrow occurs here"
  },
  "related": [
    {
      "path": "src/lib.rs",
      "line": 32,
      "column": 13,
      "end_line": 32,
      "end_column": 20,
      "role": "origin",
      "label": "immutable borrow occurs here"
    }
  ],
  "symbol": "rustc:E0502:src/lib.rs",
  "package": null,
  "module": null
}
```

Location object:

| Field | Type | Nullable | Meaning |
|---|---:|---:|---|
| `path` | string | no | Path as observed, normalized only for separators if configured. |
| `line` | integer | no | 1-based source line. |
| `column` | integer or null | yes | 1-based Unicode scalar column if known. |
| `end_line` | integer or null | yes | Inclusive end line if known. |
| `end_column` | integer or null | yes | Inclusive end column if known. |
| `role` | enum | no | Span role. |
| `label` | string or null | yes | Tool-emitted label. |

`role` enum:

| Value | Meaning |
|---|---|
| `primary` | Main location to inspect first. |
| `origin` | Earlier cause/source span. |
| `related` | Related context. |
| `suggested_location` | Command/tool points here, but not primary. |
| `stack_frame` | Location from a stack trace. |

### 4.9 `diagnostic`

```json
{
  "code": "E0502",
  "title": "cannot borrow `items` as mutable because it is also borrowed as immutable",
  "severity": "error",
  "body": "cannot borrow `items` as mutable because it is also borrowed as immutable",
  "diff": null,
  "stack": null,
  "notes": [
    {
      "kind": "note",
      "text": "immutable borrow later used here",
      "location": {
        "path": "src/lib.rs",
        "line": 36,
        "column": 9,
        "end_line": 36,
        "end_column": 14,
        "role": "related",
        "label": "immutable borrow later used here"
      }
    }
  ]
}
```

| Field | Type | Nullable | Meaning |
|---|---:|---:|---|
| `code` | string or null | yes | Tool diagnostic code. |
| `title` | string | no | Short deterministic title. |
| `severity` | enum | no | Tool severity when known. |
| `body` | string | no | Main message with ANSI stripped. |
| `diff` | object or null | yes | Assertion/snapshot diff if parsed. |
| `stack` | object or null | yes | Stack trace if parsed. |
| `notes` | array | no | Related notes/help emitted by tool. |

`severity` enum:

| Value |
|---|
| `error` |
| `failure` |
| `panic` |
| `warning` |
| `info` |
| `unknown` |

Diff object:

```json
{
  "kind": "unified",
  "truncated": true,
  "lines": [
    {"op": "context", "text": "Object {"},
    {"op": "remove", "text": "  \"count\": 1,"},
    {"op": "add", "text": "  \"count\": 2,"}
  ],
  "omitted_line_count": 31
}
```

`diff.kind` enum:

| Value | Meaning |
|---|---|
| `unified` | Unified diff or equivalent plus/minus form. |
| `expected_actual` | Expected and actual blocks without line diff. |
| `inline` | Single-line inline diff. |
| `unknown` | Diff-like content without a deterministic structure. |

`diff.lines[].op` enum:

| Value |
|---|
| `add` |
| `remove` |
| `context` |
| `meta` |
| `unknown` |

Stack object:

```json
{
  "kind": "go-goroutine",
  "truncated": true,
  "frames": [
    {
      "function": "example.com/project/pkg.TestWorker.func1",
      "location": {
        "path": "pkg/worker_test.go",
        "line": 48,
        "column": null,
        "end_line": null,
        "end_column": null,
        "role": "stack_frame",
        "label": null
      }
    }
  ],
  "omitted_frame_count": 42
}
```

### 4.10 `attempts`

```json
[
  {
    "attempt": 1,
    "status": "failed",
    "evidence_ref": "ex_1"
  },
  {
    "attempt": 2,
    "status": "passed",
    "evidence_ref": "ex_2"
  }
]
```

Retries are part of the failure object, not separate sibling failures, when the adapter can deterministically associate attempts to the same identity.

| Case | Representation |
|---|---|
| Failed attempt then passed retry | One failure with `status: passed_after_retry`, `actionability: not_actionable`. |
| Failed all retries | One failure with `status: failed_after_retries`. |
| Retry association uncertain | Separate failures, each with `relationships.retry_group_id` if available. |

### 4.11 `evidence`

```json
{
  "raw_log_byte_range": {"start": 1053, "end": 1820},
  "normalized_text_sha256": "a1b7f0b1f8d4512124b42f7d72fdf2f66fe922d3be6debc0111ad58c6dfd1328",
  "excerpts": [
    {
      "id": "ex_1",
      "kind": "failure_block",
      "byte_range": {"start": 1053, "end": 1820},
      "text": "FAILED tests/test_math.py::test_addition[negative] - assert -1 == 1",
      "truncated": false,
      "ansi_stripped": true
    }
  ]
}
```

| Field | Type | Required | Meaning |
|---|---:|---:|---|
| `raw_log_byte_range` | object or null | yes | Best enclosing raw-log range. |
| `normalized_text_sha256` | string | yes | Hash of normalized evidence text. |
| `excerpts` | array | yes | Bounded excerpts for programmatic context. |

Excerpt object:

| Field | Type | Required | Meaning |
|---|---:|---:|---|
| `id` | string | yes | Local excerpt id. |
| `kind` | enum | yes | `failure_block`, `diff`, `stack`, `summary`, or `process`. |
| `byte_range` | object or null | yes | Raw-log byte offsets. |
| `text` | string | yes | ANSI-stripped excerpt. |
| `truncated` | boolean | yes | Whether excerpt was shortened. |
| `ansi_stripped` | boolean | yes | Whether ANSI was stripped. |

Byte ranges are half-open: `start` inclusive, `end` exclusive.

### 4.12 `relationships`

```json
{
  "parameterized_group": "pytest:tests/test_math.py::test_addition",
  "sibling_failure_ids": ["fail_2d74053edb0e", "fail_659a51da16e2"],
  "cascade_root_id": null,
  "dependent_failure_ids": [],
  "retry_group_id": null
}
```

| Field | Type | Required | Meaning |
|---|---:|---:|---|
| `parameterized_group` | string or null | yes | Shared group for parameterized siblings. |
| `sibling_failure_ids` | array | yes | Same group but independent failures. |
| `cascade_root_id` | string or null | yes | Root id if this failure is dependent. |
| `dependent_failure_ids` | array | yes | Failures likely caused by this one. |
| `retry_group_id` | string or null | yes | Retry association id if needed. |

Cascade rules are intentionally conservative. A failure is dependent only when a deterministic adapter rule says so, for example:

1. `tsc && jest` where `tsc` failed and `jest` never started.
2. Cargo build failure before tests execute.
3. Pytest collection import error causing zero collected tests.

Do not infer cascade from semantic similarity.

### 4.13 `primary_failure`

```json
{
  "id": "fail_96b4b1d4e597",
  "rank": 1,
  "stable_id": "ftid_v1_849f236b508f8e5e",
  "reason": "earliest_actionable_failure"
}
```

This duplicates the selection target so consumers do not need to scan the full array. It must reference an entry in `failures`.

Selection order:

1. Process-level failures that prevented meaningful execution.
2. Pre-test build/typecheck/collection failures.
3. Test failures in raw-log order.
4. Snapshot failures in raw-log order.
5. Runtime panics/crashes in raw-log order.
6. Unparsed failures.

Tie-breakers:

1. Lower raw-log byte offset.
2. More specific source location.
3. Lexicographic `identity.stable_id`.

### 4.14 `next_actions`

```json
[
  {
    "kind": "rerun",
    "argv": ["pytest", "tests/test_math.py::test_addition[negative]", "-q"],
    "reason": "rerun_single_failure",
    "targets": ["fail_96b4b1d4e597"],
    "destructive": false
  }
]
```

Only commands are allowed. No code edits, no prose instructions to change code.

| Field | Type | Required | Meaning |
|---|---:|---:|---|
| `kind` | enum | yes | `rerun`, `increase_verbosity`, `list_tests`, `show_explain`, `inspect_raw_log`. |
| `argv` | array of strings | yes | Command to run, or empty for non-shell internal action. |
| `reason` | enum | yes | Deterministic reason. |
| `targets` | array | yes | Failure ids this action concerns. |
| `destructive` | boolean | yes | Must be false in v1. |

`reason` values should be deterministic and adapter-owned. Initial shared reasons:

| Value | Meaning |
|---|---|
| `rerun_single_failure` | Runner supports targeting the primary failure. |
| `structured_compiler_output_available` | Rerun can request more structured output. |
| `timeout_needs_more_runner_signal` | More verbose or serialized run may expose the hang location. |
| `inspect_raw_log` | Parser has insufficient structure; raw log is recovery path. |
| `show_failure_explanation` | `failtrace explain` can restate this failure. |

### 4.15 `warnings`

Warnings are deterministic and machine-readable.

```json
[
  {
    "code": "capture.truncated",
    "message": "Raw log exceeded configured capture limit.",
    "targets": []
  }
]
```

Common codes:

| Code | Meaning |
|---|---|
| `capture.truncated` | Raw log was truncated. |
| `parse.partial` | Parser skipped ambiguous sections. |
| `parse.ansi_offsets_approximate` | ANSI stripping made character offsets approximate. |
| `identity.low_specificity` | Identity lacks symbol and line. |
| `adapter.fallback_used` | Generic fallback adapter was used. |

### 4.16 `links`

```json
{
  "run_json_path": ".failtrace/runs/2026-05-12T220000Z/result.json",
  "analysis_json_path": null,
  "explain_json_path": null
}
```

All paths are local paths. No remote URLs are generated in v1.

---

## 5. Command-Specific Documents

### 5.1 `run_result`

`run_result` uses the full schema above. It is produced by executing a command, capturing raw output, classifying it, writing raw log to disk, and printing JSON to stdout.

### 5.2 `analysis_result`

`analysis_result` has the same shape as `run_result` with these differences:

| Field | Difference |
|---|---|
| `document_kind` | `analysis_result`. |
| `command.started_at` | Original value if available, otherwise `null`. |
| `command.process` | Reconstructed from artifact metadata if available. |
| `capture.raw_log_path` | Input artifact path. |
| `producer.adapters` | Adapter versions from current analyzer, not original run. |
| `analysis` | Additional required object. |

`analysis` object:

```json
{
  "input_artifact_kind": "raw_log",
  "input_result_json_sha256": null,
  "analyzed_at": "2026-05-12T23:00:00Z",
  "reclassification_of": {
    "artifact_id": "cap_01HX2K7Q9Y4E6ZQ8V8Y6J3P2M0",
    "raw_log_sha256": "3f0c1aa7b5e1c42b4fdcb8f116bacee508d1b9a2f3e2c0d99f57ab4e0b5dbb28"
  },
  "reproducibility": {
    "canonical_json_sha256": "b7f12dd6189a5fd0c0bcf9eb96c2c0e7d756d940a31d0c3b4da9d2cbe0c8fb69",
    "deterministic_inputs_sha256": "6b86b273ff34fce19d6b804eff5a3f5747ada4eaa22f1d49c01e52ddb7875b4b"
  }
}
```

`analyzed_at` is environment-varying and excluded from deterministic canonical hashes unless explicitly included by caller policy.

### 5.3 `last`

`failtrace last` should emit the stored document exactly as written, plus a single top-level retrieval object:

```json
{
  "retrieval": {
    "selected": "most_recent",
    "retrieved_at": "2026-05-12T23:05:00Z",
    "source_result_json_path": ".failtrace/runs/2026-05-12T220000Z/result.json"
  }
}
```

The retrieval object is excluded from the original run's canonical hash.

### 5.4 `explanation_result`

`explain` is deterministic interpretation, not a new classification.

```json
{
  "schema_version": "1.0.0",
  "document_kind": "explanation_result",
  "source": {
    "result_json_path": ".failtrace/runs/2026-05-12T220000Z/result.json",
    "result_json_sha256": "9d47a77a97dff7a4d75587e628ccdd7a61b912d1eb2e5a27b798a9d1c98cf56b",
    "failure_id": "fail_96b4b1d4e597"
  },
  "failure": {
    "id": "fail_96b4b1d4e597",
    "stable_id": "ftid_v1_849f236b508f8e5e",
    "phase": "test",
    "kind": "assertion",
    "title": "pytest assertion failed in parameterized case"
  },
  "explanation": {
    "summary": "The parameterized pytest case `negative` failed an assertion.",
    "inspect": [
      {
        "path": "tests/test_math.py",
        "line": 12,
        "reason": "primary failure location"
      }
    ],
    "commands": [
      ["pytest", "tests/test_math.py::test_addition[negative]", "-q"]
    ],
    "limits": [
      "No code-edit suggestions are generated by failtrace."
    ]
  }
}
```

`explain` may restate what to inspect and which command to run. It must not suggest code edits.

### 5.5 `adapters_result`

```json
{
  "schema_version": "1.0.0",
  "document_kind": "adapters_result",
  "producer": {
    "failtrace_version": "0.1.0",
    "schema_version": "1.0.0"
  },
  "adapters": [
    {
      "id": "pytest",
      "version": "0.3.0",
      "status": "available",
      "detects": {
        "argv_names": ["pytest", "python -m pytest"],
        "log_signatures": ["pytest summary line", "FAILED nodeid"]
      },
      "emits": {
        "phases": ["collection", "test", "teardown"],
        "kinds": ["assertion", "import_error", "runner_error"]
      }
    }
  ]
}
```

---

## 6. Determinism Specification

### 6.1 Deterministic Inputs

For `run`, the deterministic classification is a function of:

1. Raw captured log bytes.
2. Stream metadata captured by failtrace.
3. Command argv.
4. Process exit status, signal, or termination observation.
5. Failtrace schema version.
6. Selected adapter ids and versions.
7. Failtrace parser configuration.
8. Capture truncation/redaction policy.

For `analyze`, the deterministic classification is a function of:

1. Input raw log bytes or stored artifact bytes.
2. Input artifact metadata supplied to analyze.
3. Current selected adapter ids and versions.
4. Current schema version.
5. Current parser configuration.

No filesystem probing, dependency version lookup, network access, telemetry, LLM calls, or project-state learning may affect classification unless their bytes are present in the captured artifact or explicit analyze input metadata.

### 6.2 Environment-Varying Fields

These fields may legitimately vary between runs over identical command output:

| Field | Reason |
|---|---|
| `command.cwd` | Different checkout location. |
| `command.started_at` | Clock time. |
| `command.ended_at` | Clock time. |
| `command.duration_ms` | Runtime variance. |
| `capture.artifact_id` | Storage id may include time or randomness. |
| `capture.raw_log_path` | Storage path may include time/location. |
| `analysis.analyzed_at` | Clock time. |
| `retrieval.retrieved_at` | Clock time. |
| `links.*_path` | Local storage path. |

Everything else must be deterministic for identical deterministic inputs.

### 6.3 Canonical Hashes

Failtrace should expose two hashes:

| Hash | Definition |
|---|---|
| `capture.raw_log_sha256` | SHA-256 of raw captured bytes. |
| `analysis.reproducibility.canonical_json_sha256` | SHA-256 of canonical JSON excluding environment-varying fields. |

Canonical JSON rules:

1. UTF-8.
2. Object keys sorted lexicographically.
3. No insignificant whitespace.
4. Arrays preserved in schema order.
5. Environment-varying fields replaced by the literal string `"<varying>"`.

Consumers can verify byte-identical classification by comparing:

1. `capture.raw_log_sha256`.
2. `producer.adapters`.
3. `schema_version`.
4. `canonical_json_sha256` when present.

### 6.4 `analyze` Reclassification

`failtrace analyze` must not mutate the original run artifact. It emits a new `analysis_result` with current adapter versions. If a new adapter version classifies differently, both documents can coexist because adapter versions and canonical hashes differ.

---

## 7. Failure Identity Semantics

Failure identity is load-bearing. Agents need to know whether a failure is the same one they saw before, even if line numbers drift or sibling failures appear.

### 7.1 Stable Under

A good `identity.stable_id` should stay stable under:

1. ANSI color changes.
2. Log ordering noise outside the failure block.
3. Reordering parameterized sibling cases when the runner exposes case ids.
4. Line drift for test failures with stable test ids.
5. Repeated `run`, `last`, and `analyze` over the same raw bytes and adapter versions.

### 7.2 Not Stable Under

A stable id may change under:

1. Test function rename.
2. Parameter case id rename.
3. File rename.
4. Compiler diagnostic moving to a different primary source span.
5. Adapter version changes that intentionally improve identity extraction.

### 7.3 Identity Specificity

Adapters should prefer identity components in this order:

1. Runner-native test id or diagnostic id.
2. Diagnostic code plus primary path.
3. Primary path plus line plus coarse message fingerprint.
4. Raw-log occurrence index only as a last resort.

When identity is weak, emit warning `identity.low_specificity`.

---

## 8. Ambiguity Positions

### 8.1 Process Termination

Represent termination as a process-level failure when no more specific failure is available.

| Observation | Representation |
|---|---|
| Exit code non-zero with parsed diagnostics | Normal failures plus `process.status: failed`. |
| Exit 137 with no better signal and no OOM marker | `kind: signal`, `termination.kind: signal` or `unknown`. |
| Exit 137 plus known shell convention or OOM marker | `kind: oom`, `termination.kind: probable_oom`. |
| Failtrace watchdog kills process | `kind: timeout`, `termination.kind: watchdog_timeout`. |
| SIGKILL observed directly | `kind: signal`, unless OOM rule matches. |

OOM is `probable_oom`, not guaranteed, unless the platform provides explicit OOM-kill evidence in captured bytes or process metadata.

### 8.2 Multi-Span Diagnostics

Use `locations.primary` for the span the compiler marks as primary. Use `locations.related` for all other spans in raw emission order. Do not flatten related spans into the message. This preserves rustc and TypeScript diagnostic shape without inventing a language-specific schema.

### 8.3 Parameterized Siblings

Each failing parameterized case is a separate failure because agents may need to fix one input-specific edge case without conflating it with others.

Use:

1. Full runner node id as `identity.components.symbol`.
2. Shared non-parameterized prefix as `relationships.parameterized_group`.
3. Other failed cases as `sibling_failure_ids`.

### 8.4 Retries and Flakiness

Retries attach to the same failure identity when deterministic association is possible. A pass after retry makes the failure `not_actionable` by default, but it remains present because agents and CI scripts may track flakes.

### 8.5 Pre-Test-Phase Failures

Build, typecheck, and collection errors outrank test failures. If tests never ran, emit only the pre-test failure and mark cascade accordingly. If a composite command failed before later components ran, do not invent failures for commands that did not execute.

### 8.6 Cascade Detection

Cascade detection must be rule-based and conservative. Use explicit runner structure, not semantic inference. When in doubt, keep failures independent.

### 8.7 ANSI and Unicode

Parsing should strip ANSI while preserving byte offsets to the raw log. Columns are 1-based Unicode scalar positions if parsed from tool output. If a tool uses byte columns or display columns and that is known, adapters may add a namespaced extension:

```json
{
  "extensions": {
    "tsc": {
      "column_unit": "utf16_code_unit"
    }
  }
}
```

The core schema does not force all tools into one column model beyond stating what failtrace emits.

---

## 9. Examples

### 9.1 Pytest Parameterized Test: 3 of 17 Cases Fail

```json
{
  "schema_version": "1.0.0",
  "document_kind": "run_result",
  "producer": {
    "failtrace_version": "0.1.0",
    "schema_version": "1.0.0",
    "schema_profile": "default",
    "binary_id": "failtrace-rust-static",
    "adapters": [
      {
        "id": "pytest",
        "version": "0.3.0",
        "kind": "runner",
        "selected": true,
        "selection_reason": "argv_name_match"
      }
    ],
    "normalization": {
      "ansi": "stripped_with_offsets",
      "newlines": "lf",
      "encoding": "utf-8-lossless-or-replacement",
      "path_style": "as_observed"
    }
  },
  "command": {
    "argv": ["pytest", "tests/test_math.py", "-q"],
    "display": "pytest tests/test_math.py -q",
    "cwd": "/repo",
    "env_policy": "redacted",
    "started_at": "2026-05-12T22:00:00Z",
    "ended_at": "2026-05-12T22:00:04Z",
    "duration_ms": 4180,
    "process": {
      "status": "failed",
      "exit_code": 1,
      "signal": null,
      "termination": {
        "kind": "exit_code",
        "reason": "non_zero_exit",
        "detail": "Process exited with code 1.",
        "observed_exit_code": 1
      }
    }
  },
  "capture": {
    "artifact_id": "cap_01HX_PYTEST",
    "raw_log_path": ".failtrace/runs/2026-05-12T220000Z/raw.log",
    "raw_log_sha256": "e6c3d2588f27fb4b6d1e3b7d7a7647ec63cfd0c1abf74f4ed8f0eaa6c84b70a9",
    "raw_log_bytes": 18422,
    "stdout_bytes": 18422,
    "stderr_bytes": 0,
    "stream_merge": "stdout_stderr_interleaved",
    "truncated": false,
    "truncation": null,
    "redactions": []
  },
  "classification": {
    "status": "classified",
    "summary": "3 pytest parameterized cases failed",
    "primary_failure_id": "fail_96b4b1d4e597",
    "failure_count": 3,
    "actionable_failure_count": 3,
    "phases_observed": ["test"],
    "primary_phase": "test",
    "cascade": {
      "detected": false,
      "root_failure_ids": ["fail_96b4b1d4e597"],
      "dependent_failure_ids": [],
      "rule": "none"
    }
  },
  "failures": [
    {
      "id": "fail_96b4b1d4e597",
      "identity": {
        "stable_id": "ftid_v1_849f236b508f8e5e",
        "algorithm": "failtrace-identity-v1",
        "components": {
          "adapter_id": "pytest",
          "phase": "test",
          "kind": "assertion",
          "symbol": "tests/test_math.py::test_addition[negative]",
          "primary_path": "tests/test_math.py",
          "primary_line": null,
          "diagnostic_code": null,
          "message_fingerprint": "pytest_assert_equal"
        },
        "stability": {
          "stable_under_line_drift": true,
          "stable_under_message_drift": true,
          "stable_under_param_reordering": true
        }
      },
      "rank": 1,
      "phase": "test",
      "kind": "assertion",
      "actionability": "primary",
      "status": "failed",
      "tool": {
        "adapter_id": "pytest",
        "adapter_version": "0.3.0",
        "emitter": "pytest",
        "emitter_version": null,
        "framework": "pytest"
      },
      "label": "tests/test_math.py::test_addition[negative]",
      "message": "assert -1 == 1",
      "locations": {
        "primary": {
          "path": "tests/test_math.py",
          "line": 12,
          "column": 5,
          "end_line": 12,
          "end_column": 18,
          "role": "primary",
          "label": null
        },
        "related": [],
        "symbol": "tests/test_math.py::test_addition[negative]",
        "package": null,
        "module": "tests.test_math"
      },
      "diagnostic": {
        "code": null,
        "title": "pytest assertion failed",
        "severity": "failure",
        "body": "assert -1 == 1",
        "diff": null,
        "stack": null,
        "notes": []
      },
      "attempts": [],
      "evidence": {
        "raw_log_byte_range": {"start": 2100, "end": 3520},
        "normalized_text_sha256": "7c3caa833a46ca92d3b6c44b24139e8f40f0b78f4e6c66f941edcb9f12dfdb07",
        "excerpts": [
          {
            "id": "ex_1",
            "kind": "failure_block",
            "byte_range": {"start": 2100, "end": 3520},
            "text": "FAILED tests/test_math.py::test_addition[negative] - assert -1 == 1",
            "truncated": false,
            "ansi_stripped": true
          }
        ]
      },
      "relationships": {
        "parameterized_group": "pytest:tests/test_math.py::test_addition",
        "sibling_failure_ids": ["fail_2d74053edb0e", "fail_659a51da16e2"],
        "cascade_root_id": null,
        "dependent_failure_ids": [],
        "retry_group_id": null
      },
      "extensions": {}
    },
    {
      "id": "fail_2d74053edb0e",
      "identity": {
        "stable_id": "ftid_v1_2921433f906c1821",
        "algorithm": "failtrace-identity-v1",
        "components": {
          "adapter_id": "pytest",
          "phase": "test",
          "kind": "assertion",
          "symbol": "tests/test_math.py::test_addition[zero]",
          "primary_path": "tests/test_math.py",
          "primary_line": null,
          "diagnostic_code": null,
          "message_fingerprint": "pytest_assert_equal"
        },
        "stability": {
          "stable_under_line_drift": true,
          "stable_under_message_drift": true,
          "stable_under_param_reordering": true
        }
      },
      "rank": 2,
      "phase": "test",
      "kind": "assertion",
      "actionability": "actionable",
      "status": "failed",
      "tool": {
        "adapter_id": "pytest",
        "adapter_version": "0.3.0",
        "emitter": "pytest",
        "emitter_version": null,
        "framework": "pytest"
      },
      "label": "tests/test_math.py::test_addition[zero]",
      "message": "assert 0 == 1",
      "locations": {
        "primary": {
          "path": "tests/test_math.py",
          "line": 12,
          "column": 5,
          "end_line": 12,
          "end_column": 17,
          "role": "primary",
          "label": null
        },
        "related": [],
        "symbol": "tests/test_math.py::test_addition[zero]",
        "package": null,
        "module": "tests.test_math"
      },
      "diagnostic": {
        "code": null,
        "title": "pytest assertion failed",
        "severity": "failure",
        "body": "assert 0 == 1",
        "diff": null,
        "stack": null,
        "notes": []
      },
      "attempts": [],
      "evidence": {
        "raw_log_byte_range": {"start": 3521, "end": 5010},
        "normalized_text_sha256": "fb2ad987ad0dd2d17bdba4b1be7b790e1db8cb02d7394a17b5ab3f63c8e91a2d",
        "excerpts": [
          {
            "id": "ex_2",
            "kind": "failure_block",
            "byte_range": {"start": 3521, "end": 5010},
            "text": "FAILED tests/test_math.py::test_addition[zero] - assert 0 == 1",
            "truncated": false,
            "ansi_stripped": true
          }
        ]
      },
      "relationships": {
        "parameterized_group": "pytest:tests/test_math.py::test_addition",
        "sibling_failure_ids": ["fail_96b4b1d4e597", "fail_659a51da16e2"],
        "cascade_root_id": null,
        "dependent_failure_ids": [],
        "retry_group_id": null
      },
      "extensions": {}
    },
    {
      "id": "fail_659a51da16e2",
      "identity": {
        "stable_id": "ftid_v1_fedc88d08867e9cb",
        "algorithm": "failtrace-identity-v1",
        "components": {
          "adapter_id": "pytest",
          "phase": "test",
          "kind": "assertion",
          "symbol": "tests/test_math.py::test_addition[large]",
          "primary_path": "tests/test_math.py",
          "primary_line": null,
          "diagnostic_code": null,
          "message_fingerprint": "pytest_assert_equal"
        },
        "stability": {
          "stable_under_line_drift": true,
          "stable_under_message_drift": true,
          "stable_under_param_reordering": true
        }
      },
      "rank": 3,
      "phase": "test",
      "kind": "assertion",
      "actionability": "actionable",
      "status": "failed",
      "tool": {
        "adapter_id": "pytest",
        "adapter_version": "0.3.0",
        "emitter": "pytest",
        "emitter_version": null,
        "framework": "pytest"
      },
      "label": "tests/test_math.py::test_addition[large]",
      "message": "assert 1000001 == 1000000",
      "locations": {
        "primary": {
          "path": "tests/test_math.py",
          "line": 12,
          "column": 5,
          "end_line": 12,
          "end_column": 30,
          "role": "primary",
          "label": null
        },
        "related": [],
        "symbol": "tests/test_math.py::test_addition[large]",
        "package": null,
        "module": "tests.test_math"
      },
      "diagnostic": {
        "code": null,
        "title": "pytest assertion failed",
        "severity": "failure",
        "body": "assert 1000001 == 1000000",
        "diff": null,
        "stack": null,
        "notes": []
      },
      "attempts": [],
      "evidence": {
        "raw_log_byte_range": {"start": 5011, "end": 6500},
        "normalized_text_sha256": "67ffbfa77b7f4b517c52dba28667f8ad3fa112d841577489f8659df56c7cda9a",
        "excerpts": [
          {
            "id": "ex_3",
            "kind": "failure_block",
            "byte_range": {"start": 5011, "end": 6500},
            "text": "FAILED tests/test_math.py::test_addition[large] - assert 1000001 == 1000000",
            "truncated": false,
            "ansi_stripped": true
          }
        ]
      },
      "relationships": {
        "parameterized_group": "pytest:tests/test_math.py::test_addition",
        "sibling_failure_ids": ["fail_96b4b1d4e597", "fail_2d74053edb0e"],
        "cascade_root_id": null,
        "dependent_failure_ids": [],
        "retry_group_id": null
      },
      "extensions": {}
    }
  ],
  "primary_failure": {
    "id": "fail_96b4b1d4e597",
    "rank": 1,
    "stable_id": "ftid_v1_849f236b508f8e5e",
    "reason": "earliest_actionable_failure"
  },
  "next_actions": [
    {
      "kind": "rerun",
      "argv": ["pytest", "tests/test_math.py::test_addition[negative]", "-q"],
      "reason": "rerun_single_failure",
      "targets": ["fail_96b4b1d4e597"],
      "destructive": false
    }
  ],
  "warnings": [],
  "links": {
    "run_json_path": ".failtrace/runs/2026-05-12T220000Z/result.json",
    "analysis_json_path": null,
    "explain_json_path": null
  }
}
```

### 9.2 Cargo Test: rustc E0502 with Three Related Spans

```json
{
  "schema_version": "1.0.0",
  "document_kind": "run_result",
  "producer": {
    "failtrace_version": "0.1.0",
    "schema_version": "1.0.0",
    "schema_profile": "default",
    "binary_id": "failtrace-rust-static",
    "adapters": [
      {
        "id": "cargo-test",
        "version": "0.1.0",
        "kind": "runner",
        "selected": true,
        "selection_reason": "argv_name_match"
      }
    ],
    "normalization": {
      "ansi": "stripped_with_offsets",
      "newlines": "lf",
      "encoding": "utf-8-lossless-or-replacement",
      "path_style": "as_observed"
    }
  },
  "command": {
    "argv": ["cargo", "test"],
    "display": "cargo test",
    "cwd": "/repo",
    "env_policy": "redacted",
    "started_at": "2026-05-12T22:10:00Z",
    "ended_at": "2026-05-12T22:10:02Z",
    "duration_ms": 2310,
    "process": {
      "status": "failed",
      "exit_code": 101,
      "signal": null,
      "termination": {
        "kind": "exit_code",
        "reason": "non_zero_exit",
        "detail": "Process exited with code 101.",
        "observed_exit_code": 101
      }
    }
  },
  "capture": {
    "artifact_id": "cap_01HX_CARGO",
    "raw_log_path": ".failtrace/runs/2026-05-12T221000Z/raw.log",
    "raw_log_sha256": "a08b4fc44d5c2018187eac737c7ec6076b2a013711a2db28525d28de4f622f2e",
    "raw_log_bytes": 9120,
    "stdout_bytes": 1200,
    "stderr_bytes": 7920,
    "stream_merge": "stdout_stderr_interleaved",
    "truncated": false,
    "truncation": null,
    "redactions": []
  },
  "classification": {
    "status": "classified",
    "summary": "cargo test failed during build with rustc E0502",
    "primary_failure_id": "fail_d09036dbec10",
    "failure_count": 1,
    "actionable_failure_count": 1,
    "phases_observed": ["build"],
    "primary_phase": "build",
    "cascade": {
      "detected": true,
      "root_failure_ids": ["fail_d09036dbec10"],
      "dependent_failure_ids": [],
      "rule": "build_failed_before_tests"
    }
  },
  "failures": [
    {
      "id": "fail_d09036dbec10",
      "identity": {
        "stable_id": "ftid_v1_dff0e30b6af01a7e",
        "algorithm": "failtrace-identity-v1",
        "components": {
          "adapter_id": "cargo-test",
          "phase": "build",
          "kind": "compile_error",
          "symbol": "rustc:E0502:src/lib.rs",
          "primary_path": "src/lib.rs",
          "primary_line": 34,
          "diagnostic_code": "E0502",
          "message_fingerprint": "rustc_borrow_conflict"
        },
        "stability": {
          "stable_under_line_drift": false,
          "stable_under_message_drift": true,
          "stable_under_param_reordering": true
        }
      },
      "rank": 1,
      "phase": "build",
      "kind": "compile_error",
      "actionability": "primary",
      "status": "failed",
      "tool": {
        "adapter_id": "cargo-test",
        "adapter_version": "0.1.0",
        "emitter": "rustc",
        "emitter_version": null,
        "framework": "cargo-test"
      },
      "label": "rustc E0502 in src/lib.rs:34",
      "message": "cannot borrow `items` as mutable because it is also borrowed as immutable",
      "locations": {
        "primary": {
          "path": "src/lib.rs",
          "line": 34,
          "column": 5,
          "end_line": 34,
          "end_column": 25,
          "role": "primary",
          "label": "mutable borrow occurs here"
        },
        "related": [
          {
            "path": "src/lib.rs",
            "line": 32,
            "column": 13,
            "end_line": 32,
            "end_column": 18,
            "role": "origin",
            "label": "immutable borrow occurs here"
          },
          {
            "path": "src/lib.rs",
            "line": 36,
            "column": 9,
            "end_line": 36,
            "end_column": 14,
            "role": "related",
            "label": "immutable borrow later used here"
          }
        ],
        "symbol": "rustc:E0502:src/lib.rs",
        "package": "failtrace-example",
        "module": null
      },
      "diagnostic": {
        "code": "E0502",
        "title": "borrow checker conflict",
        "severity": "error",
        "body": "cannot borrow `items` as mutable because it is also borrowed as immutable",
        "diff": null,
        "stack": null,
        "notes": [
          {
            "kind": "note",
            "text": "immutable borrow occurs here",
            "location": {
              "path": "src/lib.rs",
              "line": 32,
              "column": 13,
              "end_line": 32,
              "end_column": 18,
              "role": "origin",
              "label": "immutable borrow occurs here"
            }
          },
          {
            "kind": "note",
            "text": "immutable borrow later used here",
            "location": {
              "path": "src/lib.rs",
              "line": 36,
              "column": 9,
              "end_line": 36,
              "end_column": 14,
              "role": "related",
              "label": "immutable borrow later used here"
            }
          }
        ]
      },
      "attempts": [],
      "evidence": {
        "raw_log_byte_range": {"start": 1300, "end": 4120},
        "normalized_text_sha256": "39f9549a52c1b8c4bb6f0b518d95cfd8ce3f72524f82625f0eb377ff1b3f39e1",
        "excerpts": [
          {
            "id": "ex_1",
            "kind": "failure_block",
            "byte_range": {"start": 1300, "end": 4120},
            "text": "error[E0502]: cannot borrow `items` as mutable because it is also borrowed as immutable",
            "truncated": false,
            "ansi_stripped": true
          }
        ]
      },
      "relationships": {
        "parameterized_group": null,
        "sibling_failure_ids": [],
        "cascade_root_id": null,
        "dependent_failure_ids": [],
        "retry_group_id": null
      },
      "extensions": {
        "rustc": {
          "explain_code_command": ["rustc", "--explain", "E0502"]
        }
      }
    }
  ],
  "primary_failure": {
    "id": "fail_d09036dbec10",
    "rank": 1,
    "stable_id": "ftid_v1_dff0e30b6af01a7e",
    "reason": "pre_test_failure"
  },
  "next_actions": [
    {
      "kind": "increase_verbosity",
      "argv": ["cargo", "test", "--message-format", "json"],
      "reason": "structured_compiler_output_available",
      "targets": ["fail_d09036dbec10"],
      "destructive": false
    }
  ],
  "warnings": [],
  "links": {
    "run_json_path": ".failtrace/runs/2026-05-12T221000Z/result.json",
    "analysis_json_path": null,
    "explain_json_path": null
  }
}
```

### 9.3 npm test Watchdog Timeout After 120s of Silence

```json
{
  "schema_version": "1.0.0",
  "document_kind": "run_result",
  "producer": {
    "failtrace_version": "0.1.0",
    "schema_version": "1.0.0",
    "schema_profile": "default",
    "binary_id": "failtrace-rust-static",
    "adapters": [
      {
        "id": "generic",
        "version": "0.1.0",
        "kind": "fallback",
        "selected": true,
        "selection_reason": "fallback_after_no_match"
      }
    ],
    "normalization": {
      "ansi": "stripped_with_offsets",
      "newlines": "lf",
      "encoding": "utf-8-lossless-or-replacement",
      "path_style": "as_observed"
    }
  },
  "command": {
    "argv": ["npm", "test"],
    "display": "npm test",
    "cwd": "/repo",
    "env_policy": "redacted",
    "started_at": "2026-05-12T22:20:00Z",
    "ended_at": "2026-05-12T22:22:00Z",
    "duration_ms": 120000,
    "process": {
      "status": "timeout",
      "exit_code": null,
      "signal": "SIGKILL",
      "termination": {
        "kind": "watchdog_timeout",
        "reason": "silent_timeout",
        "detail": "No stdout or stderr bytes observed for 120000 ms; failtrace terminated the process.",
        "observed_exit_code": null
      }
    }
  },
  "capture": {
    "artifact_id": "cap_01HX_TIMEOUT",
    "raw_log_path": ".failtrace/runs/2026-05-12T222000Z/raw.log",
    "raw_log_sha256": "0a3ef2d74f9b111db1ac31ef8a2bb402a91f7ab65fdd90fd21a9cc26db69a7d3",
    "raw_log_bytes": 2210,
    "stdout_bytes": 2110,
    "stderr_bytes": 100,
    "stream_merge": "stdout_stderr_interleaved",
    "truncated": false,
    "truncation": null,
    "redactions": []
  },
  "classification": {
    "status": "classified",
    "summary": "command timed out after 120000 ms of silence",
    "primary_failure_id": "fail_8d58f1547d62",
    "failure_count": 1,
    "actionable_failure_count": 1,
    "phases_observed": ["process"],
    "primary_phase": "process",
    "cascade": {
      "detected": false,
      "root_failure_ids": ["fail_8d58f1547d62"],
      "dependent_failure_ids": [],
      "rule": "none"
    }
  },
  "failures": [
    {
      "id": "fail_8d58f1547d62",
      "identity": {
        "stable_id": "ftid_v1_1111111111111111",
        "algorithm": "failtrace-identity-v1",
        "components": {
          "adapter_id": "generic",
          "phase": "process",
          "kind": "timeout",
          "symbol": "npm test",
          "primary_path": null,
          "primary_line": null,
          "diagnostic_code": "silent_timeout",
          "message_fingerprint": "watchdog_silent_timeout_120000"
        },
        "stability": {
          "stable_under_line_drift": true,
          "stable_under_message_drift": true,
          "stable_under_param_reordering": true
        }
      },
      "rank": 1,
      "phase": "process",
      "kind": "timeout",
      "actionability": "primary",
      "status": "failed",
      "tool": {
        "adapter_id": "generic",
        "adapter_version": "0.1.0",
        "emitter": "failtrace",
        "emitter_version": "0.1.0",
        "framework": "process"
      },
      "label": "npm test timed out after 120000 ms of silence",
      "message": "No stdout or stderr bytes observed for 120000 ms; failtrace terminated the process.",
      "locations": {
        "primary": null,
        "related": [],
        "symbol": "npm test",
        "package": null,
        "module": null
      },
      "diagnostic": {
        "code": "silent_timeout",
        "title": "watchdog timeout",
        "severity": "error",
        "body": "No stdout or stderr bytes observed for 120000 ms; failtrace terminated the process.",
        "diff": null,
        "stack": null,
        "notes": []
      },
      "attempts": [],
      "evidence": {
        "raw_log_byte_range": {"start": 0, "end": 2210},
        "normalized_text_sha256": "61e2f2bf730b58b6a5c6c3f72f027c5d28ad81f0bba18c1c9470e79c3d635d84",
        "excerpts": [
          {
            "id": "ex_1",
            "kind": "process",
            "byte_range": {"start": 0, "end": 2210},
            "text": "> app@1.0.0 test\n> tsc && jest\n\nStarting type check\nStarting test runner\n",
            "truncated": false,
            "ansi_stripped": true
          }
        ]
      },
      "relationships": {
        "parameterized_group": null,
        "sibling_failure_ids": [],
        "cascade_root_id": null,
        "dependent_failure_ids": [],
        "retry_group_id": null
      },
      "extensions": {
        "failtrace": {
          "timeout_policy": "silent_timeout",
          "timeout_ms": 120000
        }
      }
    }
  ],
  "primary_failure": {
    "id": "fail_8d58f1547d62",
    "rank": 1,
    "stable_id": "ftid_v1_1111111111111111",
    "reason": "process_prevented_completion"
  },
  "next_actions": [
    {
      "kind": "increase_verbosity",
      "argv": ["npm", "test", "--", "--runInBand"],
      "reason": "timeout_needs_more_runner_signal",
      "targets": ["fail_8d58f1547d62"],
      "destructive": false
    }
  ],
  "warnings": [
    {
      "code": "adapter.fallback_used",
      "message": "No specific adapter classified the log before timeout.",
      "targets": ["fail_8d58f1547d62"]
    }
  ],
  "links": {
    "run_json_path": ".failtrace/runs/2026-05-12T222000Z/result.json",
    "analysis_json_path": null,
    "explain_json_path": null
  }
}
```

---

## 10. Coverage of Required Realistic Shapes

| Shape | Schema treatment |
|---|---|
| Pytest parameterized, 3 of 17 fail | Separate failures, shared `parameterized_group`, identities include full nodeids. |
| Jest snapshot mismatch, 40-line ANSI diff | `kind: snapshot_mismatch`, `diagnostic.diff`, ANSI-stripped excerpts with raw byte ranges. |
| `tsc && jest`, tsc errors before jest | `phase: typecheck`, cascade rule `composite_command_stopped_after_first_failure`; no invented jest failures. |
| `go test ./...`, compile failure and goroutine panic | Multiple failures; package in `locations.package`; panic stack in `diagnostic.stack`; compile failure ranks before panic if earlier or pre-test. |
| Cargo borrow checker E0502 | `compile_error` with primary and related spans. |
| Vitest 4 files, 9 tests fail | Nine failures; file/module labels; primary by raw-log order. |
| Pytest `ImportError` in `conftest.py` | `phase: collection`, `kind: import_error`, primary location is `conftest.py` if parsed. |
| Watchdog kills after silence | Process failure `kind: timeout`, `termination.kind: watchdog_timeout`. |
| Exit 137 OOM | `kind: oom` only if deterministic OOM rule matches; otherwise `signal`. |
| Flaky pytest rerun passes | One failure with attempts and `status: passed_after_retry`, usually `not_actionable`. |

---

## 11. Rejected Alternatives

### 11.1 Emit Only the First Failure

Rejected. The product promise is first actionable failure, but adapters need to represent siblings, cascades, and flakes. Keeping `failures[]` lets agents detect whether they are looking at one isolated issue or a family of failures.

### 11.2 One Schema Per Adapter

Rejected. Adapter-specific extensions are allowed, but the core shape must be portable. Agents should not need five unrelated parsers.

### 11.3 Confidence Scores

Rejected. Scores imply probabilistic semantics and invite non-deterministic classification. Use deterministic warnings and actionability enums instead.

### 11.4 Human-Oriented Markdown Fields

Rejected for v1. `explain` can produce deterministic summaries, but the main JSON should not spend tokens on human narrative.

### 11.5 Full Raw Logs Inline

Rejected. The raw log is always on disk. JSON includes bounded excerpts and byte ranges to keep stdout useful for agents.

### 11.6 Treat Line Number as the Main Identity

Rejected. Line numbers drift too often. Prefer runner symbols and diagnostic codes. Use line numbers only when no better stable symbol exists or for compiler diagnostics where the location is the diagnostic identity.

### 11.7 Model Cascade Detection Semantically

Rejected. Similar messages do not prove dependency. Cascade rules must trace to runner structure or process ordering.

### 11.8 Force All Diagnostics into Test Failures

Rejected. Compiler/typecheck/collection/process failures are first-class. Agents should not have to infer that "no tests ran" means the actionable failure happened earlier.

---

## 12. Riskiest Parts

### 12.1 Identity Stability

Identity is the highest-risk contract. Test nodeids are strong for pytest but less uniform for Go subtests, vitest.each cases, and compiler diagnostics. The adapter fixture suite should measure identity churn under:

1. Line insertion above the failure.
2. Reordering parameter cases.
3. Renaming a test file.
4. Changing assertion message text.
5. Adding a second identical failure in the same file.

### 12.2 Column Semantics

Unicode paths and columns are easy to misrepresent. Rust, TypeScript, and JS runners may report different column units. The core schema states Unicode scalar columns, but adapters may need explicit extension metadata. Before lock, validate against non-ASCII identifiers and Windows paths.

### 12.3 Composite Commands

`npm test` may hide `tsc && jest`, `vitest run`, package-manager wrappers, and shell scripts. Without executing extra introspection commands, failtrace can only classify what appears in captured bytes. The schema handles this, but adapter authors may over-claim command structure. Fixture design should force unknown composite cases.

### 12.4 OOM Detection

Exit 137 is not always OOM. The schema uses `probable_oom`, but consumers may treat it as certain. Documentation and enum names should keep this conservative.

### 12.5 `next_actions`

Command suggestions are useful but risk becoming pseudo-fixes. The schema must keep them restricted to rerun/inspect/verbosity commands and avoid code-edit language.

### 12.6 Example Size vs Token Budget

Large JSON examples help adapter authors but can cost agent context. A future spec may split examples into fixtures while keeping the normative schema small.

---

## 13. Open Questions Before Lock

1. Should `schema_version` permit prerelease strings in emitted v1, or only stable semver?
2. Should canonical hashes be top-level for `run_result`, not just under `analysis.reproducibility`?
3. Should `cwd` be captured by default if it may reveal local usernames in CI artifacts?
4. Should `next_actions.argv` preserve the original package-manager wrapper or prefer direct runner commands when known?
5. Should adapters be allowed to emit multiple primary roots for genuinely independent first failures, or must one always win?
6. Should Windows path normalization be `as_observed` or a normalized portable representation in identity components?

---

## 14. Minimal Consumer Algorithm

An agent can consume v1 as follows:

1. Check `schema_version` major is supported.
2. If `classification.status == "no_failure"`, stop.
3. If `primary_failure != null`, find `failures[]` entry with matching id.
4. Inspect `phase`, `kind`, `message`, `locations.primary`, and `diagnostic`.
5. Use `evidence.excerpts` for bounded context.
6. If still ambiguous, read `capture.raw_log_path` and use byte ranges.
7. Use `identity.stable_id` to compare with prior observations.
8. Use only `next_actions` as commands, not as code-edit guidance.

This is the contract the schema is designed to make boring.
