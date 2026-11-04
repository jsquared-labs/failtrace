# Adapter gauntlet

The adapter gauntlet is a **fixture-generation and reconnaissance tool**, not a
test gate. It clones pinned external repos, applies mutation patches that
inject known failure shapes, runs `failtrace run` against the result, and
captures the artifacts under
`.failtrace-corpus/out/runs/<run-id>/<scenario_id>/` for human inspection.
The top-level report copy remains at `.failtrace-corpus/report.md` for quick
discovery. A human (or a follow-up tool) promotes useful captures into
`tests/fixtures/`, which remains the hash-locked validation surface.

The contract is intentionally one-way: corpus runs feed fixtures; fixtures
do not depend on corpus runs being green.

## Why this exists

Failtrace adapters are hardened against curated fixtures (`tests/fixtures/`),
including captured runner output and deliberately synthetic regression logs.
Expected identities are hash-locked through `scripts/verify_fixtures.py`. The
current tree contains 65 expected-output scenarios across all seven shipped
adapters. The gauntlet complements that population by exposing adapters to
real failure output from pinned third-party repositories, under controlled
mutations whose shape we can name in advance.

Pure CI-log scraping was considered and deferred — see the design discussion;
the short version is that captured runs with real sidecar metadata are
stronger artifacts than scraped logs without exit codes, cwds, or argv. CI-log
ingestion lands later, under `failtrace ingest <raw-log>`, as part of the
v2 backlog.

## How to run

```sh
uv run scripts/corpus/run_corpus.py
```

The first invocation in a fresh checkout clones each pinned repo into
`.failtrace-corpus/repos/<owner>__<name>@<short-sha>/`. Subsequent runs reuse
the cache and reset to clean state before applying patches. Captured artifacts
land under `.failtrace-corpus/out/runs/<run-id>/<scenario_id>/`. The per-run
directory also contains `summary.json`, `report.md`,
`manifest.snapshot.yaml`, and `toolchain.json`; `.failtrace-corpus/out/latest`
points at the most recent run. The convenience report is copied to
`.failtrace-corpus/report.md`.

The scheduled public workflow uploads only `latest`, `summary.json`, the
manifest snapshot, and toolchain metadata. It deliberately excludes both report
copies and every per-scenario run directory: rendered reports include the first
raw third-party failure message, while run directories contain full logs and
paths. Share either only after the privacy, provenance, and license review below.

For release-gate rehearsal, use:

```sh
make gauntlet
```

That target builds `target/release/failtrace`, runs the patch check against
that exact binary via `FAILTRACE_BIN`, then runs the full gauntlet. Make stops
between phases if the patch check fails.

Flags:

- `--scenario <id>` — run only the named scenario (repeatable).
- `--adapter <name>` — run only scenarios for the named adapter (repeatable).
- `--strict` — exit non-zero if any scenario was skipped, anomalous, or
  errored. Default exit code is 0 regardless of outcome (the harness is
  advisory).
- `--verbose` / `-v` — print subcommands as they run.
- `--check-patches` — verify every manifest patch applies cleanly to its
  pinned SHA via `git apply --check`. Does not run scenarios. Useful as a
  pre-flight that catches text-encoding rot (e.g., stripped trailing spaces
  in unified-diff context lines) and upstream bit-rot at pinned SHAs.
- `--prune-out` — remove legacy flat capture directories directly under
  `.failtrace-corpus/out/` before running. Historical run directories under
  `.failtrace-corpus/out/runs/`, the `latest` pointer, and report copies are
  preserved.

The runner resolves the binary once. If `FAILTRACE_BIN` is set, that exact
executable is used instead of PATH and must exist and be executable. The
report header and `summary.json` record the exact path and version that
produced the captured artifacts.

### Concurrency

The harness does **not** acquire a process lock on the cache or output
directories. Don't run two gauntlet invocations against the same working
tree simultaneously — `git reset --hard` and `git clean -fdx` race against
the concurrent patch apply / scenario run. Single-developer use is the
intended model; CI invocations should hold a separate working tree per
job.

## Skip contract

A scenario declares its required tools (e.g. `[uv, git]`). If any required
tool is not on `PATH`, the scenario is **skipped**:

- Printed to stderr as `scenario X skipped: missing tools: uv`.
- Counted under `skipped` in the summary report.
- The harness exits 0 (unless `--strict` is set).

This keeps the developer loop frictionless when toolchains are partial. The
Docker-based release audit (separate, not yet built) is where missing
toolchains become a hard error.

## How to add a scenario

1. **Pin the repo.** `git ls-remote --tags https://github.com/<owner>/<repo>.git`
   to find a stable tag, take its SHA. Pin by SHA in the manifest, not by
   tag — tags are mutable.
2. **Author the patch.** Clone the repo to a scratch dir, make the surgical
   change that produces the failure shape, capture the diff with `git diff >
   patches/<adapter>/<repo>/<scenario>.patch`. Revert the scratch clone.
3. **Add the manifest entry** in `corpus.yaml`. Required keys: `id`, `adapter`,
   `description`, `repo` (`owner`/`name`/`sha`), `command`, `requires`,
   `patches`. Scenario `id` must match `^[a-z][a-z0-9_]*$` (it becomes a
   directory name). Optional `expected` block describes what the captured
   output should look like — see "Expected block semantics" below. Optional
   `follow_ups` is a list of plain-string notes parsed by the runner and
   surfaced under "follow-ups (manifest)" in the report; use it to record
   out-of-scope observations a scenario surfaces without losing them in YAML.
   Optional `setup_commands` is a list of argv lists that run in the patched
   checkout before `failtrace run`; use it for dependency installation or
   other prep that must not be hidden inside a composite `bash -c` command.
4. **Verify the patch applies.** `uv run scripts/corpus/run_corpus.py
   --check-patches --scenario <id>`. This is a `git apply --check` round-trip
   against the pinned SHA and catches text-encoding rot before you spend
   minutes on a smoke run.
5. **Smoke test.** `uv run scripts/corpus/run_corpus.py --scenario <id>`. The
   first run will clone; review the captured `output.json` under
   `.failtrace-corpus/out/runs/<run-id>/<id>/`.

### Expected block semantics

Three keys are supported. All are optional.

- `runner_outcome: <value>` — exact match. Mismatch is an anomaly.
- `adapter_id: <value>` — exact match against the `selected: true` adapter
  in the captured `adapters[]` array. Mismatch is an anomaly.
- `min_parse_status: <floor>` — threshold for the comparable recognized
  states `partial < complete`. Captured value below the floor is an anomaly;
  equal to the floor is a silent OK; above the floor is a
  **note** ("improved beyond expected, consider tightening the threshold").
  Use this instead of exact `parse_status` for scenarios where the captured
  shape is constrained by test-scaffolding choices (e.g., `-q` suppressing
  the session banner) and you want regression detection, not exact pinning.
  Structural states `unparsed_output` and `unsupported_command` are not on
  that fidelity ladder; assert them with exact `parse_status` when they are
  the intended result.

The legacy exact-match form `parse_status: <value>` is still recognized but
prefer `min_parse_status:` for new scenarios. The two are mutually exclusive
— specifying both keys on the same `expected:` block is rejected at
manifest-load time.

### Patch authoring conventions

- **Surgical changes only.** A patch should inject one named failure shape.
  If a patch touches multiple files or multiple shapes, split it into
  separate scenarios.
- **No package-code mutations unless the scenario needs them.** If the
  failure can be expressed by editing the test file, do that — it lets the
  scenario use `uv run --with <pkg> --with pytest pytest ...` for a fast
  ephemeral install instead of `uv pip install -e .` for the whole repo.
- **Patches must apply cleanly at the pinned SHA.** Re-pin only when the
  patch needs upstream features that don't exist at the current SHA.

## Promotion to `tests/fixtures/`

The gauntlet does not auto-promote anything. Promotion is a deliberate
**curated translation**, not a copy. The promoting human (or a follow-up
script) translates the captured `output.json` into the existing fixture
shape rather than dropping a raw run-dir into the tree.

### Promoted fixture shape

Gauntlet promotions match the existing fixture convention exactly. Reusing
`pytest_httpx_assertion_failure` as a hypothetical promotion target:

```
tests/fixtures/pytest/httpx_assertion_failure/
  expected.json    ← derived from .failtrace-corpus/out/runs/<run-id>/<id>/output.json
                     (canonical fields the existing fixture tests already check)
  run/log          ← copied from .failtrace-corpus/out/runs/<run-id>/<id>/log
  source/          ← snapshot of the patched files from the cached checkout
                     (NOT the whole pinned repo — just what the patch touches)
  attribution.md   ← upstream provenance (see below)
```

This is the pytest promotion shape. `meta.json` from the captured run-dir is
**not** carried over — the pytest test harness in
[tests/pytest_adapter.rs](../../tests/pytest_adapter.rs)
synthesizes the sidecar at test time, and gauntlet-promoted fixtures
participate in that same path.

### Promotion philosophy: standalone-checkout, curated translation

Earlier drafts allowed "shared-project" promotions that fit into the
adapter-root `pyproject.toml`. That option is retired. Going forward:

- **All gauntlet promotions are standalone-checkout in spirit:** each
  promoted scenario is self-contained, owns its own `source/` snapshot,
  and does not depend on the adapter-root project's deps or config.
- **Existing hand-authored fixtures stay as is.** They predate the gauntlet
  and live inside the shared-project structure; don't migrate them.
- **If a corpus capture *could* be re-expressed cleanly as a hand-authored
  shared-project fixture, treat that as new hand-authored fixture work —
  not as direct corpus promotion.** The gauntlet captured the shape; the
  human authors a minimal repro that doesn't need the external repo at all.
  This is fine and often the right move.

### Attribution requirement

Every promoted fixture carries upstream provenance. A sibling
`attribution.md` is the lightest acceptable form:

```markdown
upstream: encode/httpx@26d48e0634e6ee9cdc0533996db289ce4b430177
license:  BSD-3-Clause
captured: 2026-05-14 via scripts/corpus/run_corpus.py
patch:    scripts/corpus/patches/pytest/httpx/assertion.patch
```

If a fixture retains copied upstream source, keep the applicable upstream
license notice under the fixture directory alongside `attribution.md`.
Permissive licenses such as MIT and BSD still require their notices to travel
with redistributed source. The promotion script (not yet written; see
"Promotion workflow" below) should refuse to promote without both provenance
and the applicable notice.

### Promotion workflow

The gauntlet does **not** auto-promote. Promotion currently happens by hand
because:

1. Each promoted fixture needs human review of whether the captured shape
   adds value beyond what's already covered.
2. Each promoted fixture needs human authorship of `expected.json` (deciding
   what the canonical contract is for this shape — which fields matter,
   what tolerances to express).
3. Each promoted fixture needs attribution.

When the corpus has 2-3 captures worth promoting, that's the right time to
write a `scripts/corpus/promote.py` helper that handles the mechanical
parts (translating `output.json` → `expected.json` skeleton, copying
`log`, snapshotting patched files into `source/`, generating
`attribution.md`) and leaves the human in charge of the `expected.json`
edits and the "is this worth promoting?" judgment. One capture isn't
enough signal to design that helper against.

After a promotion:

1. Update `scripts/verify_fixtures.py`'s normative-hash list to include the
   new fixture's expected hash.
2. Run `python3 scripts/verify_fixtures.py` to confirm reproducibility.
3. Run `cargo build --locked`, then
   `target/debug/failtrace run -- cargo test --all-features --locked` to confirm
   the adapter test harness picks the new fixture up cleanly; inspect the
   printed `output.json` artifact.

## Current scenarios

`corpus.yaml` is the source of truth. Current manifest state:

| Scenario | Adapter | Shape |
| --- | --- | --- |
| `pytest_httpx_assertion_failure` | pytest | parser-shape |
| `cargo_clap_assertion_failure` | cargo-test | parser-shape |
| `gotest_testify_assertion_failure` | gotest | parser-shape |
| `tsc_destr_type_error` | tsc | parser-shape |
| `vitest_zustand_assertion_failure` | vitest | composite-defer |
| `vitest_zustand_direct_assertion_failure` | vitest | parser-shape |
| `jest_jestmonorepo_assertion_failure` | jest | composite-defer |
| `jest_swr_direct_assertion_failure` | jest | parser-shape |
| Total | 8 scenarios | 6 parser-shape + 2 composite-defer |

When `corpus.yaml` changes, update this README in the same commit if the
manifest contract, run command, or promotion workflow changed. Do not maintain
a hand-copied scenario list here unless the manifest's shape coverage changes.

## What this is NOT

- Not a CI gate. The validation surface is `tests/fixtures/`, hash-locked.
- Not a CI-log scraper. That's `failtrace ingest`, v2.
- Not a schema-change driver. The current schema is v1.6; the gauntlet exercises
  what already ships.
- Not a refactor surface. Adapters keep their current
  `analyze_log(log, sidecar, options)` signature.
