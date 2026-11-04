# failtrace

[![CI](https://github.com/jsquared-labs/failtrace/actions/workflows/ci.yml/badge.svg)](https://github.com/jsquared-labs/failtrace/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/jsquared-labs/failtrace)](https://github.com/jsquared-labs/failtrace/releases)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![rust](https://img.shields.io/badge/rust-1.88%2B-orange)](https://www.rust-lang.org/)
[![platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey)](docs/release-0.1.md#platform-support)

**Give your coding agent the failure, not the whole test log.**

Failtrace is a deterministic test-failure interface for coding agents. Wrap a supported test or typecheck command and get one versioned JSON contract: what failed, where to start, whether the failure list appears complete, and where the captured evidence lives.

```sh
failtrace run -- pytest tests/
```

Failtrace writes a run-directory breadcrumb to stderr, then one structured JSON document to stdout. A selected-field projection looks like this:

```json
{
  "runner_outcome": "failed",
  "parse_status": "complete",
  "failures_complete": true,
  "failures": [{
    "kind": "assertion",
    "primary_location": {"file": "tests/test_basic.py", "line": 4},
    "message": "AssertionError: expected 2 to equal 3"
  }],
  "emission": {"failures_emitted": 1, "failures_total": 1, "full_output_path": "<run-dir>/output.json"}
}
```

`pytest` · `vitest` · `jest` · `cargo test` · `tsc` · `go test -json` · `xcodebuild test`

**v0.1.0 alpha** · local deterministic classification · no LLM in the parse path · MIT OR Apache-2.0

## Install

Source installation is the current supported path and requires Rust 1.88 or newer:

```sh
git clone https://github.com/jsquared-labs/failtrace.git
cd failtrace
cargo install --path . --locked
export FAILTRACE_BIN="${CARGO_HOME:-$HOME/.cargo}/bin/failtrace"
printf '%s\n' "$FAILTRACE_BIN"
"$FAILTRACE_BIN" --version
"$FAILTRACE_BIN" adapters
```

Use an absolute installation path. Verify that `--version` identifies
`product=failtrace-rust`, version `0.1.0`, and your source revision with
`state=clean` or `state=dirty`. Builds without Git metadata explicitly show
`revision=unknown state=unknown`. For example:

```text
failtrace 0.1.0 (product=failtrace-rust revision=<git-commit> state=clean)
```

The npm and PyPI packages named `failtrace` are unrelated projects. The optional
agent integrations validate this product's identity before wrapping commands;
see [configuration and fallback behavior](docs/adoption.md#executable-identity).

Prebuilt release binaries and crates.io publication are not available yet. The current v0.x targets are macOS arm64, macOS x86_64, and Linux x86_64; Windows is out of scope for v0.x.

## Why I built this

I build most of my projects with coding agents, Claude Code and Codex mostly, and we were
wasting time and tokens parsing test outcomes. An agent would run the suite, scroll through hundreds
of lines of output, and come back with a summary I couldn't check: sometimes it missed a
failure, sometimes it called a run green that wasn't. Every agent, in every project, was
re-solving the same "read this runner's output" problem from scratch.

I wanted one answer instead: what failed, where to start, and an honest flag when that answer
might be incomplete, saved to disk so I (or a second agent) can check the claim against the
same artifact instead of taking it on trust. I also wanted something that can give the agent a
predictable outcome in a digestible format. Failtrace is that answer. I use it every day on
my own work; this is the first time I'm sharing it.

## Why a failure contract?

Coding agents can read raw terminal output, but that makes every agent repeatedly solve the same runner-specific parsing problem. Some runners already offer JSON, JUnit, or minimal agent reporters; those are useful, but their schemas and failure surfaces differ.

Failtrace is the normalization layer above them:

- **One interface across seven adapters.** Outcomes, phases, failure kinds, locations, counts, and evidence use the same schema.
- **Explicit trust signals.** `parse_status` reports contract health; `failures_complete` says whether the adapter believes it accounted for every failure in the captured output.
- **A bounded agent view without discarding captured evidence.** Stdout emits at most ten failures by default; `output.json` retains the full structured result, and `log` retains the captured runner output.
- **Workload-dependent context savings.** For noisy runs, reading the bounded JSON first can materially reduce inline context use. For already terse output, the JSON may be similar in size or larger; the stable contract remains the primary benefit.
- **Replayable classification.** Re-analysis of the same run is deterministic under the documented binary, adapter, sidecar, and emission conditions. No model call sits in the classification path.

Failtrace does not just extract failures. It signals when it detects evidence that the extraction may be incomplete.

## Use it in an existing project

Failtrace wraps the runner you already use; it does not replace it.

```sh
failtrace run -- pytest tests/
failtrace run -- npx vitest run
failtrace run -- npx jest
failtrace run -- cargo test
failtrace run -- npx tsc --noEmit
failtrace run -- go test -json ./...
failtrace run -- xcodebuild test -scheme MyApp -destination 'platform=iOS Simulator,name=iPhone 17 Pro'
```

## Supported command boundary

| Adapter | Current public boundary |
| --- | --- |
| pytest | `pytest` plus common Python wrappers such as `python -m pytest`, `uv run pytest`, and `coverage run -m pytest`; non-interactive runs only, without `@response-file` arguments. |
| Vitest | Common JavaScript wrappers with explicit one-shot evidence such as `run`, `--run`, or `--watch=false`. |
| Jest | Common JavaScript wrappers; watch modes are unsupported. |
| Cargo test | `cargo test` and `cargo t`, including toolchain-qualified Cargo and `rustup run`. |
| TypeScript | `tsc` through direct and common JavaScript wrapper forms; watch mode and `@response-file` arguments are unsupported. |
| Go test | `go test -json`; bare `go test` is captured but intentionally not parsed as a structured Go event stream. |
| Xcode test | Raw `xcodebuild` test actions against `.xcodeproj` projects, including `test-without-building` and `xcrun xcodebuild test`; `.xcworkspace`, formatter wrappers, and `.xcresult` parsing are unsupported. |

The detailed supported boundary, reporter assumptions, and interactive-mode limits live in [`docs/adoption.md`](docs/adoption.md). Detector behavior and its tests remain authoritative for individual argv forms.

## A reproducible upstream example

The checked-in [`clap_macros_assertion`](tests/fixtures/cargo/clap_macros_assertion/) fixture captures an 89-line `cargo test` failure from the public [`clap`](https://github.com/clap-rs/clap) project at a pinned revision. From a source checkout, analyze it through the current checkout's binary:

```sh
cargo run --quiet --locked -- analyze --max-failures 1 tests/fixtures/cargo/clap_macros_assertion/run \
  | jq '{runner_outcome, parse_status, failures_complete, failures: [.failures[0] | {phase, kind, runner_test_id, primary_location: (.primary_location | {file, line, column}), message, raw_log_lines}]}'
```

The projection points directly to the failed test, panic site, message, and raw-log coordinate:

```json
{
  "runner_outcome": "failed",
  "parse_status": "complete",
  "failures_complete": true,
  "failures": [{
    "phase": "test",
    "kind": "assertion",
    "runner_test_id": "arg::name_explicit",
    "primary_location": {"file": "tests/macros.rs", "line": 5, "column": 9},
    "message": "assertion `left == right` failed",
    "raw_log_lines": [75, 83]
  }]
}
```

That is the utility: convert runner-specific output into a bounded, actionable first move while keeping the captured evidence available for inspection.

## How an agent reads the contract

| Question | Field to inspect |
| --- | --- |
| What happened, and where should I start? | `runner_outcome` and `failures[0]`—especially `primary_location`, `message`, `phase`, and `kind`. |
| Did stdout omit later failures? | Compare `emission.failures_emitted` with `emission.failures_total`; open `emission.full_output_path` when they differ. |
| Could parsing have degraded or missed failures? | Check `parse_status`, `parse_warnings`, and `failures_complete`. |
| Can I verify the extraction? | Use `raw_log_path`, `raw_log_sha256`, and each failure's `raw_log_lines` or byte-range evidence. |

## Adopt it in an agent workflow

The smallest useful project instruction is:

```markdown
## Running tests

Run supported non-interactive test and typecheck commands through
`failtrace run -- <command>`. Valid JSON means failtrace produced an analysis;
then inspect `parse_status`, `failures_complete`, and `emission` before trusting
the displayed failure list. The full result is at `emission.full_output_path`.
```

For Claude Code's optional enforcement hook and installable skill, Codex CLI's PATH shims, and Cursor rules, see [`docs/adoption.md`](docs/adoption.md). These integrations steer agents toward failtrace; the CLI and schema are the product contract.

## Commands

```sh
failtrace run -- <cmd>          # run, capture, analyze, persist, emit JSON
failtrace analyze <run-dir>     # re-analyze captured log + metadata
failtrace last                  # print the most recent stored output.json
failtrace explain [--run-dir <run-dir>] <failure_id>  # defaults to the latest run
failtrace adapters              # list compiled adapters and capabilities
```

The wrapped runner's exit code normally passes through `failtrace run`; a signal-killed runner has no exit code, so failtrace returns `1` and records `signal` plus `runner_outcome` in JSON. Valid JSON on stdout means the analysis command completed; it does not by itself mean the runner passed or that parsing was complete. If stdout contains no valid JSON, failtrace's own failures use codes `64`, `65`, `66`, `70`, `74`, or `78` and write the diagnostic to stderr.

## Trust model and limits

- Classification is deterministic adapter code. Failtrace is not an AI debugger, autofixer, or code-edit suggestion engine.
- `output.json` is the full structured source of truth. `log` and `meta.json` preserve the captured evidence needed for replay; `raw_log_sha256` identifies the captured bytes.
- Run artifacts can contain secrets from stderr or command arguments. They use owner-private permissions on supported Unix platforms; never publish a complete run directory, and share only the smallest inspected, sanitized excerpt needed for a report.
- `analyze` is deterministic for the same captured run under the conditions and exclusions in [`docs/schema.md`](docs/schema.md#determinism-contract).
- Watch loops, interactive debuggers, live-TTY capture, unsupported reporter formats, and commands outside the published matrix should run outside failtrace. If a captured run is unparsed, fall back to its `raw_log_path`.

## Status and verification

- The binary is `v0.1.0` alpha and emits schema `v1.6`.
- CI tests Ubuntu and macOS at stable Rust, checks MSRV 1.88 on Linux, runs formatting and clippy, and verifies fixture hashes.
- Fixture-based regressions with hash-verified failure IDs cover every shipped adapter. A scheduled advisory corpus gauntlet also exercises pinned external repositories; it is evidence gathering, not a release gate.
- The public CLI contract is [`docs/release-0.1.md`](docs/release-0.1.md); the JSON contract is [`docs/schema.md`](docs/schema.md).

## Contributing

After cloning, activate the pre-commit hook with `git config core.hooksPath .githooks`. It runs the full local contributor checks: formatting, clippy, Rust tests, fixture-hash verification, enforcement and installer batteries, and documentation/skill crosswalk checks.

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, is dual-licensed as above without additional terms or conditions.
