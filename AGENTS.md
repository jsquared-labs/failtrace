# Failtrace contributor instructions

This file is for AI coding agents working on failtrace itself. It is **not** the downstream adopter template — that lives in [`docs/adoption.md`](docs/adoption.md) and describes how downstream repos wire agents to *use* failtrace.

## What this codebase is

A Rust CLI that wraps test, build, and typecheck commands and emits deterministic JSON about failures. The current contract is documented in [`docs/schema.md`](docs/schema.md) (schema v1.6) and [`docs/release-0.1.md`](docs/release-0.1.md).

## Repo layout

```
src/
  main.rs                       3-line shim — exits with cli::main()
  lib.rs                        public module re-exports
  cli.rs                        the five subcommands; stdout/stderr protocol; command detection and dispatch
  schema.rs, schema/            v1.6 types, hashing, normalization, and cross-field validator
  storage.rs                    run-dir + sidecar + output.json + latest pointer
  adapters.rs, adapters/        adapter trait + seven shipped runner/typechecker adapters
tests/
  schema_core.rs                reproduces fixture hashes through the Rust path
  storage.rs                    run-dir round-trip + determinism contract test
  *_adapter.rs                  end-to-end checks for each shipped adapter
  cli.rs                        CLI integration (Unix-only)
  fixtures/                     captured and synthetic scenarios for every shipped adapter
docs/
  schema.md                     v1.6 output schema — source of truth
  release-0.1.md                user-facing release contract for v0.1
  adoption.md                   adopter guide + snippets for downstream repos
  schema-cold.md                historical, non-normative pre-implementation draft
skills/
  failtrace/SKILL.md            installable agent operating guide
scripts/
  verify_fixtures.py            reproduces fixture hashes from canonical hash_inputs (CI-gated)
  corpus/                       advisory external-repository adapter gauntlet
```

## Locked design decisions

Do not relitigate these during routine implementation. Surface a separate proposal before reopening:

1. **Rust, single static binary.** No language-specific runtime dependency on the user's machine.
2. **CLI-first.** Argv in, JSON on stdout, raw log on disk. MCP wrapper is v2.
3. **No LLM in the classify path.** Every confidence/category value traces back to a named deterministic rule.
4. **No code-edit suggestions.** Only command suggestions.
5. **Deterministic classification.** With the same run directory, binary, adapter version, emission budget, and no determinism warnings, repeated analysis is byte-identical after the command-mode fields listed in the schema are excluded. Enforced by `tests/storage.rs`.
6. **Schema and binary versions evolve independently.** A later v0.x binary may continue to emit schema v1.6.

## Shipped adapters

The v0.1 binary ships seven adapters:

1. **pytest** — direct invocation and supported Python/coverage wrappers.
2. **vitest** — explicit one-shot runs, including supported JavaScript package-runner wrappers.
3. **jest** — non-watch runs, including supported JavaScript package-runner wrappers.
4. **cargo test** — `cargo test` / `cargo t`, including supported toolchain wrappers.
5. **tsc** — direct and supported JavaScript package-runner wrappers.
6. **go test** — structured failure parsing requires `-json`.
7. **xcodebuild test** — raw `xcodebuild` test output for XCTest and Swift Testing, including `test-without-building`.

[`README.md`](README.md#supported-command-boundary) is the public command matrix; detector details live in [`src/cli.rs`](src/cli.rs). The adapter trait remains private through v0.x.

## Before editing

Read the relevant contract first:

- CLI behavior: [`docs/release-0.1.md`](docs/release-0.1.md).
- JSON shape and invariants: [`docs/schema.md`](docs/schema.md).
- Fixture expectations: the relevant `tests/fixtures/*/expected.json`.
- Existing adapter style: [`src/adapters/pytest.rs`](src/adapters/pytest.rs).

Keep edits narrow. Avoid unrelated refactors and formatting churn.

## Required check suite

Run the relevant narrow tests while working, then run the full suite before declaring a change done:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo build --locked
target/debug/failtrace run -- cargo test --all-features --locked
python3 scripts/verify_fixtures.py
python3 scripts/test-failtrace-enforce.py
python3 scripts/test-claude-skill-install.py
python3 scripts/test-codex-shims.py
python3 scripts/test-corpus-harness.py
python3 scripts/check-agents-skill-sync.py
python3 scripts/test-crosswalk.py
```

GitHub CI uses bare `cargo test` as a bootstrap exception, with tests on Ubuntu and macOS at stable Rust and MSRV 1.88 on Linux; it also runs `fmt`, `clippy`, and fixture-hash verification. Local contributor runs use the explicitly built Failtrace binary above so the project's own supported test command exercises the public wrapper path. The remaining scripts exercise agent enforcement, installers/shims, corpus harness behavior, and documentation/CLI crosswalks; run them for the full contributor/release gate and whenever their surfaces change. The scheduled corpus gauntlet is advisory, not a merge gate.

## CLI output rules

`failtrace run` and `failtrace analyze` must write stderr first:

```text
failtrace: run-dir=<abs> output=<abs>/output.json
```

Then stdout must contain exactly one single-line JSON document and a trailing newline. This is load-bearing for agent runtimes that merge stdout and stderr: they need to find the JSON in a single combined stream. Do not add human prose to stdout.

When failtrace emits valid JSON, pass through the wrapped runner's exit code for `run`. On signal-kill the runner's `status.code()` is `None`; surface `1` and populate `signal` + `runner_outcome` in the JSON. Failtrace's own failures use sysexits-style codes from [`src/cli.rs`](src/cli.rs) (64/65/66/70/74/78).

## Storage gotchas

Run-directory resolution (first match wins):

1. `--run-dir <path>` uses that exact path (made absolute against the cwd when needed).
2. `--here` generates a run directory under `$cwd/.failtrace/runs/`.
3. `FAILTRACE_RUNS_DIR` sets the generated-run root.
4. `XDG_DATA_HOME` sets the root to `$XDG_DATA_HOME/failtrace/runs/`.
5. Otherwise use `$HOME/.failtrace/runs/`; if that root cannot be created or written, fall back to `$cwd/.failtrace/runs/`.

Each run directory contains `log`, `meta.json`, `output.json`. Directories are created mode `0700`; files inside are `0600`. Raw logs and `meta.json` may capture secrets (env vars in stderr, full shell-joined command including `--password=...`). Preserve private modes on supported platforms; do not loosen these.

`output.json` is always the full source-of-truth document. Stdout may be a budgeted projection.

`failtrace last` reads the latest pointer under the runs root and is NOT cwd-scoped. Concurrent `failtrace run` invocations race on `latest`.

## Schema-change recipe

Schema changes are high blast radius. For any schema bump or invariant change, in this order:

1. Update [`docs/schema.md`](docs/schema.md) with a changelog entry at the top.
2. Update Rust schema types in `src/schema/types.rs`.
3. Update normalization/hash logic in `src/schema/normalize.rs` and `src/schema/hash.rs` only if the formula intentionally changes.
4. Update invariants in [`src/schema/invariants.rs`](src/schema/invariants.rs).
5. Update adapters to produce the new shape.
6. Recompute affected fixture hashes; update `tests/fixtures/*/expected.json`.
7. Update [`scripts/verify_fixtures.py`](scripts/verify_fixtures.py) if fixture semantics changed.
8. Add or update tests that fail on the old behavior.
9. Bump `SCHEMA_VERSION` in [`src/cli.rs`](src/cli.rs) if the change warrants a version bump.
10. Run the full required check suite (above).

The schema doc is the source of truth. If schema doc and code disagree, the schema doc is right and the code is wrong. Do not change normative fixture hashes casually — if a hash changes, document exactly which hash input changed and why.

## Adapter-add recipe

When adding an adapter:

1. Add fixture scenarios under `tests/fixtures/<runner>/<scenario>/` covering the adapter's minimum useful failure shapes. Every scenario has `run/log` plus `expected.json` (inputs, hash inputs, and expected subset). Add `source/` for a minimal reproducible project and `run/meta.json` when the harness replays a captured sidecar. State whether a log is captured or synthetic, and record upstream provenance/license for promoted third-party material. See the existing fixtures for the adapter-specific harness shape.
2. Add the adapter under `src/adapters/<runner>/`. Implement parsing into an intermediate model first, then lower to `OutputDocument`.
3. Wire it into [`src/adapters.rs`](src/adapters.rs) and dispatch in [`src/cli.rs`](src/cli.rs) — the existing `is_pytest_command` / `detect_pytest` check needs a sibling for the new runner's argv shapes (direct + common wrappers).
4. Add the adapter's `id`, `version`, capabilities to the `failtrace adapters` output.
5. Add per-adapter integration tests at `tests/<runner>_adapter.rs`.
6. Use deterministic named rules for classification and ranking.
7. Emit `parse_status: partial` with `parse_warnings` when the adapter recognized output but couldn't recover everything.
8. Keep `raw_log_lines` and `evidence.byte_range` tied to raw bytes (ANSI in place).
9. Ensure exactly one `adapters[].selected == true` whenever `parse_status` is `complete` or `partial`; `unparsed_output` and `unsupported_command` select none.
10. Update [`docs/schema.md`](docs/schema.md)'s adapter appendix and [`docs/adoption.md`](docs/adoption.md)'s "supported runners" wording only after the adapter actually works end-to-end.

## Determinism contract

The byte-identical guarantee requires the same run directory, binary, selected adapter version, and emission budget, with an empty `determinism_warnings` array. Comparing `run`, stored `output.json`, and `analyze` outputs requires excluding `document_kind` and `emission.channel`; exclude `failtrace_version` if the binary changed, and emission-budget fields if the budgets differ. See [`docs/schema.md`](docs/schema.md#determinism-contract) for the normative list and exclusions.

Be careful with:

- **Map ordering.** Use `BTreeMap` for `extensions` and any other map that goes into the canonical JSON; never `HashMap`.
- **Path normalization.** Follow schema rules; do not `realpath` symlinks.
- **Timestamps.** Use sidecar-captured `started_at` and `duration_ms` during `analyze`; never re-clock.
- **Parallel runner ordering.** Mark `ranking.is_stable_across_runs` honestly; cross-run ranking stability is separate from replay determinism for one captured log.
- **Budgeted stdout.** Keep `output.json` independent of stdout caps; only the stdout projection is trimmed.

Do not "fix" determinism test failures by widening the exemption list without surfacing a design proposal first.

## Code style

- Edit existing files; don't create new ones unless the layout above doesn't have a natural home.
- No trailing comments on what code does — name things instead.
- No "// removed for X" comments. If it's dead, it's gone.
- No abstraction speculation. If there's one adapter and you're adding a second, share what's actually shared; don't design for adapters 3–5.
- Use structured JSON APIs (serde), not string-spliced JSON. Keep stdout machine-only; put human-readable diagnostics on stderr.
- Keep adapter-specific parsing inside the adapter module. Keep schema invariants centralized in `src/schema/invariants.rs`; do not fork invariant logic inside adapters.

## When something is wrong

If a test fails and the fix is non-obvious, surface a root-cause analysis before patching. Implementation has exposed contract contradictions that design review missed; a schema bug discovered while implementing an adapter may mean the specification is wrong, not merely that the code is.
