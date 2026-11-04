# Changelog

All notable public changes to Failtrace will be documented here. Binary versions follow semantic versioning; emitted schema versions evolve independently and are documented in [docs/schema.md](docs/schema.md).

## [0.1.0] - Unreleased

Initial public alpha.

### Added

- Deterministic `run`, `analyze`, `last`, `explain`, and `adapters` commands.
- Adapters for pytest, Vitest, Jest, Cargo test, TypeScript type checking, `go test -json`, and Xcode test output.
- Schema v1.6 documents with completeness signaling, bounded stdout emission, stable failure identifiers, evidence ranges, and persisted full output.
- Hash-locked captured and synthetic fixtures, plus a pinned public-project adapter gauntlet.
- An optional Claude Code enforcement hook and compact progressive-disclosure skill bundle with a marker-owned installer, Codex PATH shims, and downstream adoption guidance.
- Public contribution, security-reporting, fixture-privacy, and benchmark-methodology documentation.
- Build provenance: `failtrace --version` reports the source revision and clean/dirty state, and each new run's `meta.json` records the build that captured it.
- Executable identity checks: the optional hook and Codex shims confirm they have found this `failtrace` (not an unrelated package of the same name) before wrapping a command, and otherwise let the original command run.

### Support notes

- This is alpha software. The supported initial distribution is installation from a source checkout with Rust 1.88 or newer.
- macOS arm64, macOS x86_64, and Linux x86_64 are in scope for `0.x`; Windows is not yet supported.
- There are no prebuilt release artifacts or crates.io package in the `0.1.0` contract.
