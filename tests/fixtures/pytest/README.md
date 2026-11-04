# Pytest adapter fixtures

Each subdirectory is one captured or hand-authored synthetic pytest scenario
with the expected schema-shaped output the adapter must produce.

## Layout

```
<scenario>/
  source/         # the deliberately-broken pytest project; omitted by
    tests/        # hand-authored synthetic and cross-repo capture fixtures
      ...
  run/
    log           # combined stdout+stderr from pytest
    meta.json     # the run sidecar (captured env, argv, exit code,
                  # truncation state); older fixtures derive it from
                  # expected.json when this file is absent
  expected.json   # adapter test contract — see below
```

## `expected.json` shape

Top-level fields:

- `scenario` — name matching the directory.
- `captured_environment` — runner-version metadata for captured fixtures or a
  synthetic-provenance declaration for hand-authored fixtures.
- `runner_argv` — the logical pytest argv the adapter should match for
  selection. Not the `uv run` wrapper; see "Regenerate" below.
- `exit_code` — pytest's actual exit code on this scenario.
- `hash_inputs` — the canonical fields that, run through the current
  `failure_id`/`test_group_id` derivation, produce the recorded hashes.
  `normative: true` means the hash is mandated by `docs/schema.md`;
  `normative: false` means it's a locally-computed regression anchor.
- `expected_subset` — the schema-shaped output the adapter must produce
  for this fixture: `runner_outcome`, `parse_status`, `failures_complete`,
  `ranking`, full `counts`, `adapters[]`, and per-failure
  `runner_test_id` / `test_name` / `primary_location` / `message` /
  `raw_log_lines` / `failure_id`.
- `notes` — free-text explanation of anything non-obvious about this
  scenario.

## Scenarios

| Scenario | Normative hashes? | Exercises |
|---|---|---|
| `collection_error` | yes (`a28a10bf9c212128`) | `phase: collect`, `kind: missing_symbol`, null `runner_test_id`, `runner_outcome: pre_test_failed`, `tests_run: 0` |
| `basic_assertion` | no | standard `E   AssertionError: ...` extraction path, `phase: test`, `kind: assertion` |
| `parameterized` | yes (`58af4b4b1fcb1036` / `b0782aebe12ab236` / `1bbb49abdd90e17e`, group `49507ef118ce763f`) | `param_id`, `test_group_id` derivation, `first_emitted` ranking, 3-of-17 progress |
| `quiet_early_exit_assertion` | no | gauntlet-promoted `pytest -q -x` assertion where the short summary is suppressed; simple function node ID recovery and passed-count clamp |
| `teardown_exception` | no | `phase: teardown`, `kind: exception`, `counts.tests_errored`, invariant 9 (final-outcome classification) |
| `sqlite_foreign_key_error` | no (local anchor) | synthetic `phase: test`, `kind: exception`, `sqlite3.IntegrityError`, `counts.tests_errored` case |
| `banner_only_no_summary` | no | regression pin for `ParseWarning::MissingSummaryLine` — banner + `collected` then crash before any summary line, `parse_status: partial`, 0 failures |
| `green_summary_nonzero_exit` | no | regression pin for `ParseWarning::RunnerSummaryMismatch` (meaning 1 of 2) — admissible-green summary vs. nonzero sidecar exit, `parse_status: partial`, 0 failures. Meaning 2 — a runner summary or structured terminal stream reports more comparable failure units than the adapter accounted for (`failures` empty or short, `failures_complete: false`) — is covered by adapter regression tests, not a fixture here |
| `truncated_capture` | no | regression pin for `ParseWarning::TruncatedTraceback` — sidecar `truncated: true`, `parse_status: partial`, 0 failures |

`sqlite_foreign_key_error`, `banner_only_no_summary`, `green_summary_nonzero_exit`,
and `truncated_capture` are hand-authored synthetic regression fixtures. They
have no `source/` project and are **not** part of the captured-project regen loop
below. The SQLite fixture has an ignored expectation generator in
`tests/pytest_adapter.rs`; edit the other synthetic fixtures directly.

## Python interpreter pin

Python is pinned via `.python-version` to `3.11.15`; pytest via `uv.lock`
to `8.4.2`. uv will download the interpreter on regeneration if not
present.

## Regenerate

```sh
cd tests/fixtures/pytest
uv sync

for scenario in collection_error basic_assertion parameterized teardown_exception; do
  cd "$scenario/source"
  uv run --project ../.. --quiet pytest --color=no -p no:cacheprovider tests/ > ../run/log 2>&1
  cd ../..
done
```

After regeneration, re-run the verification script (see project root)
to ensure the recorded hashes still hold. If a regen produces logs that
no longer satisfy `expected.json`, the adapter contract has changed —
that's a deliberate decision point, not a test failure to paper over.

## Cross-host log drift

Captured logs include host-specific content that will differ across
regeneration environments:

- **Platform banner** — `platform darwin -- Python 3.11.15, pytest-8.4.2`
  becomes `platform linux -- ...` on Linux. Patch-version drift is
  possible if `.python-version` allows it (currently it doesn't).
- **`rootdir:` absolute path** — `/home/example/failtrace/...` varies with the
  home directory used for the capture.
- **Exact line numbers** in the banner section may shift if pytest's
  output format changes between minor versions.

The adapter doesn't depend on any of these — it reads `cwd` from the
sidecar, not from the log banner. Cross-host regen producing
banner-different but adapter-equivalent logs is expected and acceptable.

## Gotcha: `app/` lives inside `tests/`

The `collection_error` scenario has its `app` package at
`source/tests/app/`, NOT at `source/app/`. This is intentional: pytest
loads `conftest.py` *before* fully populating `sys.path` from rootdir
discovery, so an `app` package at the source root isn't importable from
conftest yet. Moving it next to conftest (inside `tests/`) makes the
import resolve, producing the schema-required
`ModuleNotFoundError: No module named 'app.fixtures'` (not the wrong
`No module named 'app'` that an empty `tests/app/` would produce).
