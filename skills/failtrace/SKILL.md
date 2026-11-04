---
name: failtrace
description: >-
  Use for running supported non-interactive tests and typechecks through
  Failtrace, or for interpreting Failtrace JSON, output.json artifacts,
  `last`, `analyze`, and `explain` results. Covers pytest, Vitest, Jest,
  cargo test, tsc, go test, and xcodebuild test. Verify the stored artifact
  and trust fields before reporting results; use raw logs only as fallback.
  Do not wrap build, install, watch, debugger, or live-TTY commands.
---

# Failtrace

Use Failtrace as the deterministic failure contract around supported test and typecheck commands. The JSON is the contract; terminal rendering is incidental. The current binary emits schema v1.6. See the normative schema at <https://github.com/jsquared-labs/failtrace/blob/main/docs/schema.md>.

## Core workflow

1. For any supported non-interactive invocation, run `failtrace run -- <command>`. Do not run bare supported runners when Failtrace can wrap them.
2. Capture the stderr breadcrumb, then inspect its exact `output.json` before reporting a verified result. Treat stdout as a convenient projection, not durable proof.
3. Branch first on `schema_version`, then inspect `runner_outcome`, `parse_status`, and `failures_complete` independently.
4. Compare `emission.failures_emitted` with `emission.failures_total`; when the emitted count is lower, read the exact stored `output.json` from the breadcrumb before concluding later failures do not matter.
5. If `determinism_warnings` is non-empty, do not claim byte-identical replay. Branch on the warning or report the limitation.
6. Fix the recommended root failure, using stored evidence and only safe suggested commands. Fall back to `raw_log_path` whenever the structured result is degraded, incomplete, or contradicted by the captured log.

On noisy runs, the bounded JSON can materially reduce inline context use. On terse runs it may be similar in size or larger; the stable, replayable contract is the primary benefit.

## Run a command

```sh
failtrace run -- pytest tests/
failtrace run -- npx vitest run
failtrace run -- npx jest --ci
failtrace run -- cargo test --workspace
failtrace run -- npx tsc --noEmit
failtrace run -- go test -json ./...
failtrace run -- xcodebuild test -project ExampleApp.xcodeproj -scheme ExampleApp
```

The `--` separator is recommended when runner arguments contain hyphens. Run `failtrace adapters` for the compiled adapter list.

Read only the reference for the active runner when its invocation boundary, ranking, parser behavior, or rerun shape matters:

- [pytest](references/pytest.md)
- [Vitest](references/vitest.md)
- [Jest](references/jest.md)
- [Cargo test](references/cargo-test.md)
- [TypeScript compiler](references/tsc.md)
- [Go test](references/go-test.md)
- [xcodebuild test](references/xcodebuild-test.md)
- [Unsupported, interactive, and wrapped forms](references/command-boundaries.md) — read when a command is rejected, long-running, interactive, hidden inside another tool, or uses a non-default reporter.

## Verify the artifact

Failtrace writes this breadcrumb to stderr before emitting one single-line JSON document on stdout:

```text
failtrace: run-dir=<abs> output=<abs>/output.json
```

Use the matching `output.json`, not `failtrace last`, when provenance matters. A compact reader is:

```sh
jq -c '{schema_version, runner_outcome, parse_status, failures_complete, determinism_warnings, emitted:.emission.failures_emitted, total:.emission.failures_total, first:.failures[0].message}' < /path/from/breadcrumb/output.json
```

A verified structured green requires an understood `schema_version`, `runner_outcome: passed`, `parse_status: complete`, and `failures_complete: true` in that artifact. `runner_outcome: passed` with `unparsed_output` only proves the wrapped process exited successfully; inspect the raw log before claiming the runner's result was structurally verified.

| State | Action |
|---|---|
| `failed` | Read the recommended root failure and its evidence. |
| `pre_test_failed` | Fix the build, collection, dependency, or setup failure before test-level failures. |
| `typecheck_failed` | Fix the first typecheck diagnostic. Test counts are not meaningful. |
| `harness_crashed` | The runner or harness terminated abnormally before its native completed-run summary. Inspect `early_exit`, `parse_warnings`, and `raw_log_path`. |
| `timed_out` | Inspect `early_exit.reason` and `early_exit.killer`; do not report an ordinary test failure. |
| `no_tests` | Report that no tests ran; do not call it a green test run unless zero tests were intended. |
| `parse_status: partial` | Required fields degraded. Read `parse_warnings` and the affected raw evidence. |
| `unparsed_output` | The runner was recognized but its output shape was not. `failures[]` is empty; inspect `raw_log_path`. |
| `unsupported_command` | No adapter owns the composite/ambiguous command. Rerun supported segments separately or use the normal bare command. |
| `failures_complete: false` | Treat structured failures as leads; the pre-budget list may be short. Inspect the raw log. |

Valid JSON means Failtrace completed analysis, regardless of exit code. A nonzero exit usually belongs to the wrapped runner. No valid JSON means Failtrace itself failed; inspect stderr rather than inferring a test result.

If Failtrace or the underlying executable is missing, use the repository-native environment or package runner already declared by the project, such as `uv run`, an in-repo virtual environment, or `npx`. Do not install global dependencies or loop through the same failing shim. If Failtrace remains unavailable, state the fallback and run the project's normal command directly.

## Act on failures safely

For each failure, prioritize:

- `failure_id` — stable 16-hex identifier; use it with `failtrace explain`.
- `phase`, `kind`, and `primary_location` — route and open the likely root.
- `message`, `details`, and `evidence.excerpts` — understand the failure without rereading the whole log.
- `cascade_root` — when non-null, fix that referenced failure first.

Plan against `failure_id`, not a list slot, when `ranking.is_stable_across_runs` is false.

Treat each `suggested_next_commands[]` object as data:

- Execute `argv`; `command` is display-only.
- Run from its declared `cwd`.
- Do not auto-execute when `interactive` or `requires_approval` is true.
- Treat `mutates_workspace` and `uses_network` as opt-in, even when `requires_approval` is absent due to producer drift.

Prefer these structured suggestions over hand-built rerun patterns only after applying those checks.

## Preserve the stream contract

Keep stdout and stderr separate. Do not pipe through `2>&1`, `tee`, `head`, or `tail`; merged or truncated streams can hide the breadcrumb or corrupt the JSON document. Agent runtimes may present both streams in one tool result, but do not merge them yourself.

For a projection, preserve stderr independently:

```sh
failtrace run -- pytest 2>>/tmp/failtrace.err \
  | jq -c '{schema_version, runner_outcome, parse_status, failures_complete, determinism_warnings, first:.failures[0].message}'
```

For durable capture, redirect each stream separately:

```sh
failtrace run -- pytest > stdout.json 2> stderr.log
```

## Stored runs and privacy

The full result is always at `emission.full_output_path`; `raw_log_path` points to captured runner output. Run directories also contain `meta.json`, which records argv and cwd. Any of these files may contain source, paths, personal data, or secrets.

Run-directory selection uses this precedence: `--run-dir <path>`, `--here`, `FAILTRACE_RUNS_DIR`, `XDG_DATA_HOME`, then `$HOME/.failtrace/runs/` with `$cwd/.failtrace/runs/` as the unwritable-home fallback. Add `.failtrace/` to the project's ignore rules before using project-local storage. v0.x does not prune old runs automatically.

Never attach a complete run directory to a public issue, even after editing it. Prefer a minimal synthetic reproduction and the smallest inspected, sanitized excerpt. Use [GitHub's private vulnerability reporting form](https://github.com/jsquared-labs/failtrace/security/advisories/new) when a safe public report is impossible.

## Follow-up commands

- `failtrace explain [--run-dir <path>] <failure_id>` — read stored evidence and deterministic classification metadata. Without `--run-dir`, it checks only the latest run.
- `failtrace analyze <run-dir>` — re-analyze stored `log` plus `meta.json` without changing the stored `output.json`.
- `failtrace last` — print the active run root's latest `output.json` without re-analysis.
- `failtrace adapters` — list compiled adapters and representative command forms.

Concurrent runs race on the global latest pointer, and `last` is normally not cwd-scoped. Prefer the breadcrumb's exact path whenever more than one process or agent may be running. Strict replay claims additionally require an empty `determinism_warnings` array.
