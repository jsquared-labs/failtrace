# `xcodebuild-test` adapter design

**Status:** implemented
**Adapter ID:** `xcodebuild-test`

This adapter turns raw `xcodebuild test` output into the same bounded,
deterministic failure document used by failtrace's other runners. The design is
based on public Xcode and Swift Testing output formats; the checked-in fixtures
are hand-authored synthetic logs and contain no application source.

## Scope

The detector claims these command shapes:

- `xcodebuild test`
- `xcodebuild test-without-building`
- `xcodebuild clean test`
- `xcrun xcodebuild test`, including `xcrun --sdk ...`
- absolute-path `xcodebuild` invocations with a test action

Project, scheme, destination, derived-data, test-filter, and build-setting
arguments do not affect classification. Build-only actions, `swift test`,
`fastlane scan`, and streams already rewritten by `xcpretty` or `xcbeautify`
are outside this adapter's contract.

## Parse model

`xcodebuild` interleaves compiler, linker, test, and simulator output. The
parser walks the captured bytes once and records boundaries from runner-owned
markers:

| Marker | Meaning |
|---|---|
| `Test Suite '...' started at ...` | XCTest execution began |
| `◇ Test run started.` | Swift Testing execution began |
| `** TEST SUCCEEDED **` / `** TEST EXECUTE SUCCEEDED **` | successful terminal event |
| `** TEST FAILED **` / `** TEST EXECUTE FAILED **` | failed terminal event |
| `Testing failed:` | build or runtime failure summary |

The adapter recognizes these failure families:

| Log shape | Phase | Kind |
|---|---|---|
| `<file>:<line>:<column>: error: <message>` | `build` | `syntax_error`, `type_error`, `missing_symbol`, `linker_error`, or `unknown` |
| `Undefined symbols for architecture ...` and `ld: ...` | `build` | `linker_error` |
| XCTest assertion diagnostics | `test` | `assertion` |
| Swift Testing `recorded an issue` expectation | `test` | `assertion` |
| Swift Testing required-value failure | `test` | `exception` |
| Unavailable destination block | `setup` | `dependency_resolution_error` |

Simulator launch failures are currently represented as
`runner_outcome: harness_crashed` with
`parse_warnings: [missing_terminal_event]`. They do not produce a fabricated
source failure.

## Completeness and counting

Execution counts and failure coverage have separate owners. XCTest totals use
bundle occurrences opened by `Test Suite 'Name.xctest' started` and closed by
the `Executed` line following that bundle's end marker. Each occurrence retains
its largest inner summary as a fallback if it never closes. For each bundle
name, select the occurrence with the largest run total (first wins ties), then
sum across names. With no bundle occurrences, use the largest summary line.
The optional `with S tests skipped and` clause supplies that summary's skips.

Per-occurrence started/terminal test identities independently witness run and
skip totals. A disagreement, unclosed occurrence, orphan case outside a bundle,
or a bundle's combined population exceeding its selected occurrence degrades
counts with `runner_summary_mismatch`. The restart marker also degrades counts,
including when a missing terminal event already degrades the stream.
Swift Testing run totals accumulate across recognized summaries; pending
per-test skips fold exactly once at a summary and reset at a new run start.
Suite skip markers do not count again. The two harness populations add.
`tests_run` is floored at extracted failed-test identities plus skipped tests
to preserve schema invariants; using the floor also degrades count reliability.

Swift Testing can emit multiple issue records for one test. Failures therefore
remain one record per issue while `counts.tests_failed` is derived from unique
test identities. The parser reconciles extracted issues with XCTest summaries,
Swift Testing run/suite/test issue totals, terminal failing-test lists, and
observed issue markers. Any shortfall sets `failures_complete: false` and adds
`runner_summary_mismatch`.

XCTest incident accounting takes the maximum nested summary and reconciles it
against the sum of separate outer execution summaries. `unexpected` is already
included in the failure total. Missing outer summaries leave coverage ambiguous.
This accounting spans every execution, independent of the count-winning
occurrence. Swift Testing compares run, test, suite and observed issue evidence
within an epoch, taking the maximum across overlapping sources, then sums
disjoint epochs. A new run start clears the suite stack and advances the epoch
without discarding earlier evidence. Ambiguous suite envelopes cannot establish
full coverage. Extraction counters advance only when a failure is appended.

The Swift run-summary prefix accepts `✔`, `✘`, and `━`. Execution totals survive
unfamiliar suffixes; issue counts are parsed separately, including parenthetical
suffixes. A passing `with N known issue(s)` summary is not missing-failure
evidence. `Failing tests:` is a coverage witness with identity multiplicity,
never a source of synthesized failure objects.

Before emission budgeting, `failures_complete` requires no truncation, no
missing terminal event, and no untrusted failure coverage. Count degradation
alone may produce `parse_status: partial` with `failures_complete: true`.
Matching ignores leading zero-width spaces; evidence retains the raw bytes.

Test failures may arrive in nondeterministic execution order, so
`ranking.is_stable_across_runs` is false when a test-phase failure is present.
Build/setup diagnostics retain stable observed ordering.

## Test identity

- Legacy XCTest `-[ExampleAppTests.ParserTests testInvalidInput]` becomes
  `ExampleAppTests.ParserTests.testInvalidInput`.
- Swift Testing `ExampleSuite.comparesValues()` becomes
  `ExampleSuite.comparesValues`.
- Display-name and parameterized Swift Testing issue forms are accepted.

The shared schema code derives `failure_id`; the adapter does not implement a
separate hashing rule.

## Synthetic fixture coverage

| Scenario | Contract pinned |
|---|---|
| `green_passing` | Swift Testing count supersedes a zero-test XCTest envelope; terminal success |
| `build_failure` | pre-test missing-symbol diagnostic |
| `swift_testing_expect_failure` | one `#expect` issue with actual/expected extraction |
| `swift_testing_multi_expect` | two issue records in one failed test |
| `linker_failure` | undefined-symbol block plus terminal `ld` diagnostic |
| `sim_launch_failure` | harness crash and missing-terminal warning |
| `swift_semantic_build_failure` | access-control and `try` compiler families |
| `destination_resolution_failure` | setup failure and safe destination rerun suggestions |
| `multi_suite_skipped` | 72 XCTest tests, 2 skipped, 70 passed, with coherent per-case and bundle witnesses |
| `swift_testing_skipped` | 6 Swift Testing tests, 5 skipped, 1 passed; suite skips do not double-count members |

Each fixture contains only `run/log`, `run/meta.json`, `expected.json`, and,
where useful, a provenance README. The logs are minimal synthetic examples,
not edited captures from another repository. `expected.json` files can be
regenerated with the ignored generator documented in
[`tests/xcodebuild_adapter.rs`](../../tests/xcodebuild_adapter.rs).

## Deferred work

- `.xcresult` parsing. If added, capture and hash structured artifacts during
  `run`; `analyze` must never re-query external state.
- First-class codesigning, provisioning, and simulator setup failures.
- A separate `swift-test` adapter for Swift Package Manager output.
- Lossy third-party formatter support.

The text log and sidecar remain the complete replay input. Parsing must not
shell out, inspect Xcode state, or read project files.
