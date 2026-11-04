# Contributing to Failtrace

Failtrace is an alpha Rust CLI with a machine-readable contract. Small parser changes can affect hashes, byte ranges, agent behavior, and every downstream consumer, so contributions should be narrow and evidence-backed.

By contributing, you agree that your contribution may be distributed under either the MIT License or the Apache License 2.0, matching this repository.

## Set up a checkout

You need Git, Python 3, and Rust 1.88 or newer. Some adapter and corpus work also needs the corresponding runner toolchain (Node.js, Go, or Xcode) and `uv`.

```sh
git clone https://github.com/oh-meh/failtrace.git
cd failtrace
cargo build --locked
```

The supported installation path for trying the CLI is:

```sh
cargo install --path . --locked
```

Before editing, read the contract closest to the change:

- [docs/release-0.1.md](docs/release-0.1.md) for CLI behavior;
- [docs/schema.md](docs/schema.md) for the emitted JSON and invariants;
- the relevant `tests/fixtures/` expectations for parser behavior; and
- a sibling module under `src/adapters/` for adapter conventions.

## Development rules

- Keep stdout machine-only. Human diagnostics and the run-directory breadcrumb belong on stderr.
- Keep classification deterministic. Every category and confidence value must trace to a named rule; the classify path must not call an LLM.
- Preserve the raw-log byte coordinate system. A source-path rewrite or edited excerpt can invalidate evidence ranges and fixture hashes.
- Use ordered maps for serialized maps and avoid clocks, filesystem canonicalization, or nondeterministic iteration in analysis.
- Do not widen determinism-test exemptions to make a failure disappear. Explain the contract conflict first.
- Do not add code-edit suggestions. Failtrace may suggest commands.

## Run the required checks

Supported test runners must be invoked through Failtrace. After `cargo build --locked` creates the debug binary, run the full local gate from the repository root:

```sh
cargo fmt --all -- --check
python3 scripts/verify_fixtures.py
python3 scripts/test-failtrace-enforce.py
python3 scripts/test-claude-skill-install.py
python3 scripts/test-codex-shims.py
python3 scripts/test-corpus-harness.py
python3 scripts/check-agents-skill-sync.py
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo build --locked
target/debug/failtrace run -- cargo test --all-features --locked
python3 scripts/test-crosswalk.py
```

`failtrace run` prints a breadcrumb naming the persisted `output.json`. A test result is verified only when that artifact exists and agrees with the result you report. Read the JSON first; inspect the raw `log` only when the structured report is insufficient. Do not commit `.failtrace/` or share its contents.

You can enable the repository's local pre-commit checks with:

```sh
git config core.hooksPath .githooks
```

Run a focused test while iterating, then the complete gate before requesting review. `make gauntlet` is a networked, multi-toolchain release rehearsal rather than a routine unit-test requirement; run it for adapter changes that can affect the public corpus and describe any skips.

## Schema changes

Treat `docs/schema.md` as the source of truth. For a schema or invariant change:

1. Document the change and its compatibility effect.
2. Update schema types, normalization or hashing only where intentionally required, and centralized invariants.
3. Update every affected adapter.
4. Regenerate fixture expectations and hashes from the documented inputs.
5. Add a test that fails under the old behavior.
6. Bump the schema version when the contract requires it, independently of the binary version.
7. Run the complete check suite.

Breaking changes need an explicit migration note. Do not casually change normative fixture hashes; state which hash input changed and why.

## Fixtures, provenance, and privacy

Prefer the smallest synthetic fixture that preserves the runner-output shape. A fixture derived from a third-party project must use a public, pinned revision, include license-compatible attribution, and contain only the minimum files and output needed for the parser case.

Before committing a fixture:

1. Inspect `log`, `meta.json`, `output.json`, source snapshots, and attribution.
2. Remove credentials, tokens, private source, personal names, home-directory paths, device names and identifiers, and proprietary test or product terminology.
3. Confirm the captured material's license permits redistribution.
4. Regenerate evidence byte ranges, stable IDs, expected subsets, and hashes after any content edit.
5. Run `python3 scripts/verify_fixtures.py` and the relevant adapter tests through Failtrace.

Search-and-replace is not sufficient privacy review: context around a replaced string may still disclose the original project, and length changes can invalidate byte offsets.

## Report an adapter bug safely

For ordinary misclassification or missing-failure bugs, use the adapter issue form and share the least data that demonstrates the problem:

1. Record `failtrace --version`, `schema_version`, platform, selected adapter, `parse_status`, `parse_warnings`, and `failures_complete`.
2. Reduce the command to a public or synthetic reproduction and redact tokens, usernames, paths, repository names, and test data.
3. Quote only the smallest relevant raw-log excerpt plus the corresponding sanitized JSON fields.
4. Inspect all three run files before sharing anything. Do not attach the run directory, even if you edited only `meta.json`.
5. If safe public reproduction is impossible, describe the shape without artifacts. If the issue could have security impact, stop and use the private process in [SECURITY.md](SECURITY.md).

## Pull requests

Keep each pull request focused. Explain behavior changes separately from refactors, name affected adapters and invariants, list exact verification commands, and call out deferred work. Complete the privacy and provenance checklist in the pull request template; reviewers may ask for a synthetic replacement for any questionable artifact.
