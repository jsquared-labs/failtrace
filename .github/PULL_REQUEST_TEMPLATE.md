## Summary

<!-- Explain the problem and the user-visible or contract-visible result. -->

-

## Contract impact

- [ ] No emitted-schema or CLI-contract change
- [ ] Backward-compatible schema addition; `docs/schema.md`, fixtures, and version notes are updated
- [ ] Breaking schema or CLI change; migration notes and the appropriate version bump are included

<!-- If behavior changed, name the affected adapters and distinguish parsing changes from refactors. -->

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `python3 scripts/verify_fixtures.py`
- [ ] `python3 scripts/test-failtrace-enforce.py`
- [ ] `python3 scripts/test-claude-skill-install.py`
- [ ] `python3 scripts/test-codex-shims.py`
- [ ] `python3 scripts/test-corpus-harness.py`
- [ ] `python3 scripts/check-agents-skill-sync.py`
- [ ] `cargo clippy --all-targets --all-features --locked -- -D warnings`
- [ ] `cargo build --locked`
- [ ] `target/debug/failtrace run -- cargo test --all-features --locked` (or an equivalent `failtrace run -- ...` invocation); the printed `output.json` artifact was inspected
- [ ] `python3 scripts/test-crosswalk.py` (after the debug binary exists)
- [ ] Relevant narrow tests and a manual smoke run are described below

<!-- Mark checks that are genuinely not applicable and explain why. Do not claim a test result without its persisted failtrace output.json when the runner is supported. -->

Verification details:

```text

```

## Fixture and privacy review

- [ ] New or changed fixtures are synthetic or come from a public, pinned, license-compatible source with attribution
- [ ] I inspected every shared `log`, `meta.json`, and `output.json`; they contain no secrets, private source, personal paths, device identifiers, or other private data
- [ ] Byte ranges, hashes, expected subsets, and `failures_complete` expectations were regenerated or verified after fixture edits
- [ ] No run directory or unredacted artifact is attached to this PR

## Review notes

<!-- Call out parser rules, invariants, compatibility tradeoffs, or intentionally deferred work that deserve special attention. -->
