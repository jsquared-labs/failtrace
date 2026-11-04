# Go test

Use `go test -json` for structured parsing.

- Failtrace recognizes an explicit `-json` token and a captured assignment such as `env GOFLAGS=-json go test ./...`. Ambient `GOFLAGS` inherited by Failtrace is not recorded in sidecar argv and cannot be detected.
- Bare `go test` is admitted but lowers to `unparsed_output`; exit zero may still produce `runner_outcome: passed`. Inspect the raw log or rerun with explicit JSON before claiming structured verification.
- Build events produce compiler failures and `pre_test_failed`. Test events cover assertions, panics, races, and timeouts.
- Test ids are `<package>::<test>`. Go subtest names are lossy; ambiguous slash-containing names may set `extensions.go.subtest_name_ambiguous`.
- Cascade-parent subtests are suppressed so leaf failures appear.
- Go's per-test timeout lowers to `timed_out` with `early_exit.reason: per_test_timeout`. Signal or truncation without timeout remains `harness_crashed`; signal-kill makes `failures_complete` false.
- Ranking is unstable for multi-package runs and runs with `t.Parallel()` pause/continue events. Single-package non-parallel runs are stable.
- Malformed JSONL events emit `malformed_structured_event`, degrade parsing to `partial`, and may make the failure list incomplete. Unknown actions alone do not degrade parsing.
- `raw_log_path` points to Go's native JSONL event stream, not Failtrace schema JSON.
- `go test -c` and `go test -fuzz=` are admitted but can produce atypical output. Validated-version enforcement is deferred.
- Versioned launchers such as `go1.22 test`, release-candidate/beta launchers, `gotip test`, and `.exe` forms are recognized.
- Third-party and script wrappers are not introspected. Read [command-boundaries.md](command-boundaries.md).
