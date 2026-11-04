# Command boundaries

Use this reference when a command is rejected, interactive, long-running, hidden inside another tool, or produces a non-default reporter.

## Never wrap

- Commands without supported failure structure, including `ls`, formatters, install commands, and linters in fix mode.
- Interactive or live-TTY sessions: pytest `--pdb`, `--trace`, `-s`, `--capture=no`; pytest-watch; Vitest watch/UI; Jest watch/watchAll; debuggers.
- Response-file invocations such as `pytest @args.txt` and `tsc @args.txt`; expand them into an explicit safe one-shot argv first.
- Lossy reporter/formatter wrappers that rewrite the supported raw output.

## Package and shell aliases

- `npm test`, `pnpm test`, and `yarn test` may be inspected for candidates, but v0.1 returns `unsupported_command` rather than attributing a shared pretest/test/posttest stream. Invoke the underlying supported runner directly through Failtrace.
- Commands hidden inside `make`, `just`, task runners, shell aliases, `bash -c`, `eval`, command substitution, or project scripts are not recursively attributed. Wrap the underlying runner explicitly, or run the outer orchestration normally.

## Runner-specific exclusions

- Cargo: `build`, `check`, nextest, bench, miri, and watcher forms are outside `cargo-test`.
- TSC: watch mode, response files, and package-script aliases are rejected.
- Go: gotestsum, richgo, gore, dlv, entr, mage/task/just wrappers, IDE runners, and project scripts are not detected. Invoke `go test -json` directly.
- Xcode: non-test actions, `.xcworkspace`, xcpretty/xcbeautify, fastlane scan, SwiftPM `swift test`, and xcrun introspection modes are outside v0.1.

When Failtrace returns adapter-unavailable exit `65` with no JSON, do not keep retrying the same wrapper. Read stderr, confirm the command against `failtrace adapters`, and use the repository's normal command when no supported direct form exists.
