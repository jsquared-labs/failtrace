# Cargo test

Use for `cargo test` / `cargo t`, including supported toolchain-qualified and `rustup run` forms.

- Build-phase rustc diagnostics, dependency resolution, linker errors, and build-script panics occur before the first `running N tests` line and lower to `pre_test_failed`.
- Test-phase libtest panics, assertions, `should_panic` mismatches, and doctest failures occur after that lifecycle boundary.
- Cargo has no stable runner-version banner, so `runner_version_unsupported` is not emitted.
- Rerun suggestions derive from Cargo's own `to rerun pass ...` directive and preserve the failing workspace package.
- Build/pre-test ordering is stable. Test-phase ordering is stable only with `-- --test-threads=1` or `-- --test-threads 1`. A mixed list follows the first failure's stability.
- A signal-kill mid-binary always makes `failures_complete` false, even when earlier test-binary summaries exist.
- Build failures with `diagnostic_code` may suggest `rustc --explain <code>`.
- `cargo test --message-format=json` is captured but lowers to `unparsed_output`. Rerun as `failtrace run -- cargo test ...` without Cargo's JSON message-format flag.
- `cargo build`, `cargo check`, nextest, miri, bench, and watcher wrappers are outside this adapter. Read [command-boundaries.md](command-boundaries.md).
