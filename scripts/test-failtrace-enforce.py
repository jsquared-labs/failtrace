#!/usr/bin/env python3
"""Test battery for scripts/failtrace-enforce.py.

Imports the hook directly and asserts that its detector mirrors the binary's
detect_pytest / detect_vitest / detect_jest / detect_cargo_test semantics
(src/cli.rs). Run with:

    python3 scripts/test-failtrace-enforce.py

Exits 0 on full pass, 1 with a failure summary otherwise.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import stat
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch

# Import the Claude Code hook module by file path (it has a hyphenated name).
_hook_path = Path(__file__).parent / "failtrace-enforce.py"
_spec = importlib.util.spec_from_file_location("failtrace_enforce", _hook_path)
hook = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(hook)


CASES = [
    # (expected_decision, description, command)
    # "block" = hook should return runner != None
    # "allow" = hook should return runner == None

    # --- Bare invocations the binary recognizes (must block) ---
    ("block", "bare pytest", "pytest"),
    ("block", "bare pytest with args", "pytest tests/ -q"),
    ("block", "pytest absolute path", "/usr/local/bin/pytest"),
    ("block", "pytest absolute path with args", "/usr/local/bin/pytest tests/"),
    ("block", "python -m pytest", "python -m pytest"),
    ("block", "python3 -m pytest", "python3 -m pytest"),
    ("block", "python3.11 -m pytest", "python3.11 -m pytest"),
    ("block", "absolute python -m pytest", "/usr/bin/python3 -m pytest"),
    ("block", "python -W ignore -m pytest", "python -W ignore -m pytest"),
    ("block", "python3 -X dev -m pytest", "python3 -X dev -m pytest tests/"),
    ("block", "python -W=ignore -m pytest", "python -W=ignore -m pytest"),
    ("block", "uv run pytest", "uv run pytest tests/"),
    ("block", "uv run --no-sync pytest", "uv run --no-sync pytest"),
    ("block", "uv run --with X pytest", "uv run --with requests pytest"),
    ("block", "uv run --with=X pytest", "uv run --with=requests pytest"),
    ("block", "uv run --python V pytest", "uv run --python 3.11 pytest"),
    ("block", "uv run -- pytest", "uv run -- pytest"),
    ("block", "uv run nested python -m pytest", "uv run python -m pytest"),
    ("block", "uv run nested with full flags", "uv run --with requests --python 3.11 python -m pytest"),
    ("block", "poetry run pytest", "poetry run pytest"),
    ("block", "poetry run --with dev pytest", "poetry run --with dev pytest"),
    ("block", "poetry run nested python -m pytest", "poetry run python -m pytest"),
    ("block", "pipenv run pytest", "pipenv run pytest"),
    ("block", "pipenv run --python V pytest", "pipenv run --python 3.11 pytest"),
    ("block", "coverage run -m pytest", "coverage run -m pytest"),
    ("block", "coverage run -m pytest args", "coverage run -m pytest tests/"),
    ("block", "coverage run --source=X -m pytest", "coverage run --source=src -m pytest"),
    ("block", "coverage run --source X -m pytest", "coverage run --source src -m pytest"),
    ("block", "coverage run --rcfile= -m pytest", "coverage run --rcfile=.coveragerc -m pytest"),
    ("allow", "pytest loop on fail short", "pytest -f"),
    ("allow", "pytest grouped live capture x", "pytest -sx"),
    ("allow", "pytest grouped live capture verbose", "pytest -sv"),
    ("allow", "pytest grouped loop on fail x", "pytest -fx"),
    ("allow", "pytest grouped loop on fail capture", "pytest -fs"),
    ("allow", "pytest loop on fail long", "python -m pytest --looponfail"),
    ("allow", "pytest pdb", "pytest --pdb"),
    ("allow", "pytest trace", "python -m pytest --trace"),
    ("allow", "pytest live capture short", "pytest -s"),
    ("allow", "pytest live capture long", "pytest --capture=no"),
    ("allow", "pytest live capture split", "pytest --capture no"),
    ("allow", "pytest response file may hide interactive mode", "pytest @interactive.args"),
    ("block", "python no-user-site is not pytest capture", "python -s -m pytest"),
    ("block", "pytest k expression consumes short remainder", "pytest -ks"),
    ("block", "pytest mark expression consumes short remainder", "pytest -ms"),
    ("block", "pytest report chars consume short remainder", "pytest -rs"),
    ("block", "pytest ini override consumes short remainder", "pytest -osomething"),
    ("block", "pytest xdist worker count consumes short remainder", "pytest -n2"),

    # --- Vitest (only explicit one-shot forms are supported) ---
    ("allow", "bare vitest defaults to watch", "vitest"),
    ("allow", "vitest watch subcommand", "vitest watch"),
    ("allow", "vitest dev subcommand", "vitest dev"),
    ("block", "vitest with run subcommand", "vitest run"),
    ("block", "vitest with run flag", "vitest --run"),
    ("block", "vitest with no-watch flag", "vitest --no-watch"),
    ("block", "vitest absolute path", "/usr/local/bin/vitest run"),
    ("block", "npx vitest run", "npx vitest run"),
    ("block", "npx --cache vitest run", "npx --cache .npm-cache vitest run"),
    ("block", "npm exec vitest run", "npm exec vitest run"),
    ("block", "npm x vitest run", "npm x vitest run"),
    ("block", "npm exec -- vitest run", "npm exec -- vitest run"),
    ("block", "npm --prefix exec vitest run", "npm --prefix web exec vitest run"),
    ("block", "npm exec package vitest run", "npm exec --package vitest vitest run"),
    ("block", "pnpm vitest run", "pnpm vitest run"),
    ("block", "pnpm exec vitest run", "pnpm exec vitest run"),
    ("block", "pnpm dlx vitest run", "pnpm dlx vitest run"),
    ("block", "pnpm --dir exec vitest run", "pnpm --dir web exec vitest run"),
    ("block", "yarn vitest run", "yarn vitest run"),
    ("block", "yarn exec vitest run", "yarn exec vitest run"),
    ("block", "yarn --cwd exec vitest run", "yarn --cwd web exec vitest run"),
    ("block", "bunx vitest run", "bunx vitest run"),
    ("block", "bunx package vitest run", "bunx --package vitest vitest run"),
    ("block", "bunx --bun vitest run", "bunx --bun vitest run"),
    ("block", "bun x vitest run", "bun x vitest run"),
    ("block", "bun x --bun vitest run", "bun x --bun vitest run"),
    ("allow", "vitest watch", "vitest --watch"),
    ("allow", "vitest short watch", "vitest -w"),
    ("allow", "npx vitest ui", "npx vitest --ui"),
    ("allow", "vitest standalone", "vitest run --standalone"),
    ("allow", "vitest disabled run", "vitest run --run=false"),
    ("allow", "vitest no-run", "vitest run --no-run"),
    ("block", "vitest explicit watch false", "vitest run --watch=false"),
    # Versioned package specs (vitest@<spec>) are intentionally NOT tested:
    # detect_wrapped_vitest stays exact-match per the slice scope; the F5b
    # versioned-spec admission was a jest-only fix. If vitest gains the same
    # admission later, mirror the jest@<version> block cases here.

    # --- Bare jest (must block) ---
    ("block", "bare jest", "jest"),
    ("block", "jest with args", "jest --ci --runInBand"),
    ("block", "jest absolute path", "/usr/local/bin/jest --ci"),
    ("block", "npx jest", "npx jest"),
    ("block", "npx --cache jest", "npx --cache .npm-cache jest"),
    ("block", "npx jest@version", "npx jest@30.4"),
    ("block", "npx jest@latest", "npx jest@latest"),
    ("block", "npm exec jest", "npm exec jest"),
    ("block", "npm x jest", "npm x jest"),
    ("block", "npm exec -- jest", "npm exec -- jest"),
    ("block", "npm --prefix exec jest", "npm --prefix web exec jest"),
    ("block", "npm exec --package jest jest", "npm exec --package jest jest"),
    ("block", "npm exec jest@version", "npm exec jest@30.4"),
    ("block", "pnpm jest", "pnpm jest"),
    ("block", "pnpm exec jest", "pnpm exec jest"),
    ("block", "pnpm dlx jest", "pnpm dlx jest"),
    ("block", "pnpm dlx jest@version", "pnpm dlx jest@29"),
    ("block", "pnpm --dir exec jest", "pnpm --dir web exec jest"),
    ("block", "yarn jest", "yarn jest"),
    ("block", "yarn exec jest", "yarn exec jest"),
    ("block", "yarn --cwd exec jest", "yarn --cwd web exec jest"),
    ("block", "bunx jest", "bunx jest"),
    ("block", "bunx --package jest jest", "bunx --package jest jest"),
    ("block", "bunx --bun jest", "bunx --bun jest"),
    ("block", "bun x jest", "bun x jest"),
    ("block", "bun x --bun jest", "bun x --bun jest"),
    ("allow", "jest watch", "jest --watch"),
    ("allow", "npx jest watch all", "npx jest --watchAll"),
    ("allow", "jest dashed watch all", "jest --watch-all"),
    ("block", "jest explicit watch false", "jest --watch=false"),
    ("block", "jest max workers short", "jest -w 2"),

    # --- Cargo test (must block) ---
    ("block", "cargo test", "cargo test"),
    ("block", "cargo t", "cargo t"),
    ("block", "cargo absolute path", "/usr/local/bin/cargo test"),
    ("block", "cargo toolchain test", "cargo +stable test"),
    ("block", "cargo global flags before test", "cargo --locked --offline --color never -Z unstable-options -C crates/app test"),
    ("block", "cargo config equals and verbose alias", "cargo --config=term.color=never -vv t"),
    ("block", "cargo subcommand flags passive", "cargo test --workspace -p failtrace --release --features foo --target x86_64-unknown-linux-gnu --manifest-path Cargo.toml -- --test-threads=1"),
    ("block", "cargo message format json", "cargo test --message-format=json"),
    ("block", "rustup run cargo test", "rustup run stable cargo test"),
    ("block", "cargo config space separated value", "cargo --config foo=bar test"),
    ("block", "cargo frozen flag", "cargo --frozen test"),
    ("block", "cargo quiet flag", "cargo -q test"),
    ("block", "cargo verbose long flag", "cargo --verbose test"),
    ("block", "cargo toolchain plus alias", "cargo +nightly t"),
    ("block", "rustup run cargo alias", "rustup run stable cargo t"),

    # --- tsc (must block) ---
    ("block", "tsc", "tsc"),
    ("block", "tsc noEmit", "tsc --noEmit"),
    ("block", "tsc project short", "tsc -p tsconfig.json"),
    ("block", "tsc project long", "tsc --project tsconfig.json"),
    ("block", "tsc build", "tsc --build"),
    ("block", "tsc build short", "tsc -b"),
    ("block", "npx tsc", "npx tsc"),
    ("block", "pnpm tsc", "pnpm tsc"),
    ("block", "yarn tsc", "yarn tsc"),
    ("block", "bunx tsc", "bunx tsc"),
    ("block", "bun x tsc", "bun x tsc"),
    ("block", "npm exec tsc", "npm exec tsc"),
    ("block", "npm x tsc", "npm x tsc"),
    ("block", "pnpm exec tsc", "pnpm exec tsc"),
    ("block", "pnpm dlx tsc", "pnpm dlx tsc"),
    ("block", "tsc then jest", "tsc && jest"),
    ("block", "tsc build then jest", "tsc -b && jest"),
    ("block", "pnpm tsc then pnpm jest", "pnpm tsc && pnpm jest"),

    # --- tsc watch mode (must allow) ---
    ("allow", "tsc watch", "tsc --watch"),
    ("allow", "tsc watch short", "tsc -w"),
    ("allow", "tsc build watch", "tsc --build --watch"),
    ("allow", "tsc build watch short", "tsc --build -w"),
    ("allow", "tsc short build short watch", "tsc -b -w"),
    ("allow", "tsc short build watch", "tsc -b --watch"),
    ("allow", "npx tsc watch", "npx tsc --watch"),
    ("allow", "pnpm tsc watch short", "pnpm tsc -w"),
    ("allow", "yarn tsc watch", "yarn tsc --watch"),
    ("allow", "bunx tsc watch short", "bunx tsc -w"),
    ("allow", "tsc response file may hide watch", "tsc @watch.args"),
    ("allow", "npx tsc response file may hide watch", "npx tsc @watch.args"),
    (
        "block",
        "npx scoped package value before tsc",
        "npx --package typescript --package @types/node tsc --noEmit",
    ),

    # --- tsc false-positive checks (must allow) ---
    ("allow", "tsc-something", "tsc-something"),
    ("allow", "tsc at latest", "tsc@latest"),
    ("allow", "bash tsc", 'bash -c "tsc"'),
    ("allow", "npm run typecheck", "npm run typecheck"),
    ("allow", "tsc-helper", "tsc-helper"),

    # --- gotest (must block) ---
    ("block", "go test", "go test"),
    ("block", "go test -json", "go test -json"),
    ("block", "go test recurse", "go test ./..."),
    ("block", "go test -json recurse", "go test -json ./..."),
    ("block", "go test -race recurse", "go test -race ./..."),
    ("block", "go test -run -count", "go test -run TestFoo -count=1"),
    ("block", "go test -json -cover", "go test -json -cover ./..."),
    ("block", "go test -json -coverprofile", "go test -json -coverprofile=coverage.out ./..."),
    ("block", "go test -json -exec wrapper", "go test -json -exec /tmp/wrapper ./..."),
    ("block", "go test absolute path", "/opt/homebrew/bin/go test"),
    ("block", "env single var go test", "env CGO_ENABLED=0 go test"),
    ("block", "env multi var go test", "env CGO_ENABLED=0 GOOS=linux go test -json ./..."),
    ("block", "command go test", "command go test -json"),
    ("block", "time go test", "time go test -json"),
    ("block", "two go test chained", "go test ./... && go test ./pkg"),
    ("block", "go1.21 versioned launcher", "go1.21 test ./..."),
    ("block", "go1.21.5 versioned launcher", "go1.21.5 test ./..."),
    ("block", "go1.22rc1 versioned launcher", "go1.22rc1 test ./..."),
    ("block", "go1.22beta1 versioned launcher", "go1.22beta1 test ./..."),
    ("block", "gotip launcher", "gotip test ./..."),
    ("block", "go.exe launcher", "go.exe test ./..."),

    # --- gotest false-positive checks (must allow) ---
    ("allow", "go build", "go build"),
    ("allow", "go vet", "go vet ./..."),
    ("allow", "go run", "go run main.go"),
    ("allow", "go env", "go env"),
    ("allow", "go generate", "go generate ./..."),
    ("allow", "go install", "go install ./..."),
    ("allow", "gotest single token (package name)", "gotest"),
    ("allow", "go-test wrapper name", "go-test"),
    ("allow", "goimports", "goimports -w ."),
    ("allow", "gofmt", "gofmt test"),
    ("allow", "gopher", "gopher test"),
    ("allow", "gone test confusable", "gone test"),
    ("allow", "go1.٢ unicode digit basename", "go1.٢ test ./..."),
    ("allow", "go1.٠ unicode digit basename", "go1.٠.5 test ./..."),
    ("allow", "gotestsum third-party wrapper", "gotestsum ./..."),

    # --- xcodebuild-test (must block) ---
    ("block", "xcodebuild test plain", "xcodebuild test"),
    ("block", "xcodebuild test with project/scheme", "xcodebuild test -project Foo.xcodeproj -scheme Foo"),
    ("block", "xcodebuild test-without-building", "xcodebuild test-without-building"),
    ("block", "xcodebuild clean test", "xcodebuild clean test"),
    ("block", "xcodebuild test with destination", "xcodebuild test -project Foo.xcodeproj -scheme Foo -destination 'platform=iOS Simulator,name=iPhone 17'"),
    ("block", "xcodebuild test with only-testing filter", "xcodebuild test -project Foo.xcodeproj -scheme Foo -only-testing:FooTests/BarTests"),
    ("block", "xcrun xcodebuild test", "xcrun xcodebuild test"),
    ("block", "xcrun --sdk macosx xcodebuild test", "xcrun --sdk macosx xcodebuild test"),
    ("block", "xcrun --toolchain swift-... xcodebuild test", "xcrun --toolchain swift-DEVELOPMENT-SNAPSHOT-2026-01-01 xcodebuild test"),
    ("block", "absolute-path xcodebuild test", "/usr/bin/xcodebuild test"),
    # xcrun with boolean long flag: previously the heuristic would eat `xcodebuild`
    # as the flag's value and fail-open. New flag table treats unknown flags as boolean.
    ("block", "xcrun --verbose xcodebuild test", "xcrun --verbose xcodebuild test"),
    ("block", "xcrun --asdf xcodebuild test", "xcrun --asdf xcodebuild test"),
    ("block", "xcrun -sdk macosx xcodebuild test", "xcrun -sdk macosx xcodebuild test"),
    ("block", "xcrun -toolchain swift-... xcodebuild test", "xcrun -toolchain swift-DEVELOPMENT-SNAPSHOT-2026-01-01 xcodebuild test"),
    # Value-flag-with-disqualifying-looking-value BEFORE the action verb
    # The path value `build` for `-derivedDataPath`
    # must NOT be classified as the action verb; the actual `test` verb
    # following must still trigger the claim.
    ("block", "xcodebuild -derivedDataPath build test", "xcodebuild -derivedDataPath build test"),
    ("block", "xcodebuild -project Foo.xcodeproj test", "xcodebuild -project Foo.xcodeproj test"),
    ("block", "xcodebuild -scheme build test", "xcodebuild -scheme build test"),
    ("block", "xcodebuild -configuration archive test", "xcodebuild -configuration archive test"),
    ("block", "xcodebuild -archivePath build -derivedDataPath archive test", "xcodebuild -archivePath build -derivedDataPath archive test"),
    # Multi-action invocations where `test` follows another action verb
    # xcodebuild(1) `action ...` allows combinations; `clean build test` is valid and runs all three actions.
    # Detector must claim because a test phase will execute.
    ("block", "xcodebuild build test", "xcodebuild build test"),
    ("block", "xcodebuild clean build test", "xcodebuild clean build test"),
    ("block", "xcodebuild build-for-testing test-without-building", "xcodebuild build-for-testing test-without-building"),
    ("block", "xcodebuild -exportArchive test", "xcodebuild -exportArchive test -scheme Foo"),
    ("block", "xcodebuild build test -project Foo.xcodeproj", "xcodebuild build test -project Foo.xcodeproj"),
    ("block", "xcodebuild clean build test -scheme Foo", "xcodebuild clean build test -scheme Foo"),

    # --- xcodebuild false-positive checks (must allow) ---
    # xcrun reject-options short-circuit even when xcodebuild follows.
    # `-f`/`--find` is the real lookup-only short/long pair per man xcrun.
    ("allow", "xcrun --find xcodebuild (lookup only)", "xcrun --find xcodebuild"),
    ("allow", "xcrun -f xcodebuild test (find short form, lookup only)", "xcrun -f xcodebuild test"),
    ("allow", "xcrun --version", "xcrun --version"),
    ("allow", "xcrun --show-sdk-path xcodebuild test", "xcrun --show-sdk-path xcodebuild test"),
    ("allow", "xcrun --show-sdk-version", "xcrun --show-sdk-version"),
    # Full --show-* family per man xcrun.
    ("allow", "xcrun --show-sdk-build-version", "xcrun --show-sdk-build-version"),
    ("allow", "xcrun --show-sdk-platform-path", "xcrun --show-sdk-platform-path"),
    ("allow", "xcrun --show-sdk-platform-version", "xcrun --show-sdk-platform-version"),
    ("allow", "xcrun --show-toolchain-path xcodebuild test", "xcrun --show-toolchain-path xcodebuild test"),
    # Malformed xcrun value flags mirrored from Rust detector unit tests:
    # detector must fail open cleanly, not crash or consume a missing value.
    ("allow", "xcrun --sdk trailing value flag", "xcrun --sdk"),
    ("allow", "xcrun -sdk trailing value flag", "xcrun -sdk"),
    ("allow", "xcrun --toolchain trailing value flag", "xcrun --toolchain"),
    ("allow", "xcrun -toolchain trailing value flag", "xcrun -toolchain"),
    ("allow", "xcrun --sdk followed by --help", "xcrun --sdk --help"),
    # Boolean xcrun flags that DO still execute the wrapped command — must claim.
    ("block", "xcrun --run xcodebuild test (run-mode, default)", "xcrun --run xcodebuild test"),
    ("block", "xcrun -r xcodebuild test (run short form)", "xcrun -r xcodebuild test"),
    ("block", "xcrun -n xcodebuild test (no-cache modifier, still executes)", "xcrun -n xcodebuild test"),
    ("block", "xcrun --no-cache xcodebuild test", "xcrun --no-cache xcodebuild test"),
    ("block", "xcrun -k xcodebuild test (kill-cache modifier)", "xcrun -k xcodebuild test"),
    ("block", "xcrun -l xcodebuild test (log-then-execute)", "xcrun -l xcodebuild test"),
    ("block", "xcrun -v xcodebuild test (verbose)", "xcrun -v xcodebuild test"),
    ("allow", "xcodebuild build", "xcodebuild build"),
    ("allow", "xcodebuild build-for-testing", "xcodebuild build-for-testing"),
    ("allow", "xcodebuild archive", "xcodebuild archive"),
    ("allow", "xcodebuild analyze", "xcodebuild analyze"),
    ("allow", "xcodebuild -list", "xcodebuild -list"),
    ("allow", "xcodebuild workspace is outside v0.1", "xcodebuild test -workspace Example.xcworkspace -scheme Example"),
    ("allow", "xcrun simctl boot", "xcrun simctl boot 'iPhone 17'"),
    ("allow", "xcodebuild lone token", "xcodebuild"),
    ("allow", "swift test (handled by swift-test adapter, not xcodebuild-test)", "swift test"),
    ("allow", "xcbeautify-wrapped (lossy pre-formatter)", 'bash -c "xcodebuild test | xcbeautify"'),
    # Value-flag-with-action-looking-value (NO test verb) — must still reject.
    ("allow", "xcodebuild -derivedDataPath build build (build action)", "xcodebuild -derivedDataPath build build"),
    ("allow", "xcodebuild -scheme test build (build action)", "xcodebuild -scheme test build"),

    # --- Modifier/builtin bypasses (must block) ---
    ("block", "env-var prefix", "PYTHONPATH=. pytest"),
    ("block", "env-var prefix variant", "FOO=bar pytest"),
    ("block", "env modifier", "env X=1 pytest"),
    ("block", "env -i pytest", "env -i pytest"),
    ("block", "env -u VAR pytest", "env -u FOO pytest"),
    ("block", "env -- pytest", "env -- pytest"),
    ("block", "env multiple options", "env -i -u FOO X=1 pytest"),
    ("block", "time modifier", "time pytest"),
    ("block", "time -p pytest", "time -p pytest"),
    ("block", "nice modifier", "nice pytest"),
    ("block", "nice -n N modifier", "nice -n 10 pytest"),
    ("block", "nice -5 modifier", "nice -5 pytest"),
    ("block", "nice --adjustment N", "nice --adjustment 5 pytest"),
    ("block", "nice --adjustment=N", "nice --adjustment=5 pytest"),
    ("block", "command builtin", "command pytest"),
    ("block", "command -p pytest", "command -p pytest"),
    ("block", "command -v pytest", "command -v pytest"),
    ("block", "command -- pytest", "command -- pytest"),
    ("block", "exec builtin", "exec pytest"),
    ("block", "exec -c pytest", "exec -c pytest"),
    ("block", "exec -l pytest", "exec -l pytest"),
    ("block", "exec -a NAME pytest", "exec -a my-pytest pytest"),
    ("block", "exec -- pytest", "exec -- pytest"),

    # --- Chained-command bypasses (must block: substring-allow) ---
    ("block", "substring-allow bypass", "failtrace run -- pytest a && pytest b"),
    ("block", "echo-fake bypass", "echo failtrace run; pytest"),
    ("block", "chained two bare pytests", "pytest a; pytest b"),
    ("block", "wrapped then bare via pipe", "failtrace run -- pytest | pytest"),
    ("block", "|& separator: echo |& pytest", "echo x |& pytest"),
    ("block", "|& wrapped-then-bare", "failtrace run -- pytest |& pytest"),

    # --- Subshell `(...)` and brace-group `{...}` wrapping (must block) ---
    ("block", "subshell pytest", "(pytest)"),
    ("block", "subshell uv run pytest", "(uv run pytest tests/)"),
    ("block", "subshell uv run pytest piped", "(uv run pytest tests/) | tail -5"),
    ("block", "subshell pytest spaced", "( pytest )"),
    ("block", "subshell nested pytest", "((pytest))"),
    ("block", "subshell triple-nested pytest", "(((pytest)))"),
    ("block", "subshell mixed pair first segment", "(cd ./ && pytest)"),
    ("block", "subshell second segment after wrapped first", "(failtrace run -- pytest a; pytest b)"),
    ("allow", "subshell implicit-watch vitest via npx", "(npx vitest)"),
    ("block", "subshell one-shot vitest via npx", "(npx vitest run)"),
    ("block", "subshell jest via npx", "(npx jest --ci)"),
    ("block", "subshell cargo test", "(cargo test)"),
    ("block", "subshell tsc", "(tsc --noEmit)"),
    ("block", "subshell go test json", "(go test -json ./...)"),
    ("block", "subshell pytest backgrounded", "(pytest) &"),
    ("block", "subshell pytest piped then bare cargo", "(pytest) | cargo test"),
    ("block", "brace group pytest", "{ pytest; }"),
    ("block", "brace group cargo test", "{ cargo test; }"),
    ("block", "brace group go test", "{ go test -json ./...; }"),

    # --- Wrapped subshell forms (must allow) ---
    ("allow", "subshell wrapped pytest", "(failtrace run -- pytest)"),
    ("allow", "subshell wrapped pytest piped", "(failtrace run -- pytest tests/) | tail -5"),
    ("allow", "subshell wrapped go test", "(failtrace run -- go test -json ./...)"),
    ("allow", "brace group wrapped pytest", "{ failtrace run -- pytest; }"),

    # --- Wrapped forms (must allow) ---
    ("allow", "wrapped pytest", "failtrace run -- pytest"),
    ("allow", "wrapped uv run pytest", "failtrace run -- uv run pytest"),
    ("allow", "wrapped vitest", "failtrace run -- npx vitest"),
    ("allow", "wrapped jest", "failtrace run -- npx jest --ci --runInBand"),
    ("allow", "wrapped cargo test", "failtrace run -- cargo test"),
    ("allow", "wrapped go test", "failtrace run -- go test"),
    ("allow", "wrapped go test -json", "failtrace run -- go test -json ./..."),
    ("allow", "wrapped env-prefix go test", "failtrace run -- env CGO_ENABLED=0 go test -json ./..."),
    ("allow", "wrapped both segments", "failtrace run -- pytest a && failtrace run -- pytest b"),
    ("allow", "wrapped after env modifier", "PYTHONPATH=. failtrace run -- pytest"),

    # --- Binary REJECTS these (must allow: false-positive cases) ---
    ("allow", "uv --with pytest python -c", 'uv run --with pytest python -c "print(1)"'),
    ("allow", "uv --with=pytest python script.py", "uv run --with=pytest python script.py"),
    ("allow", "npx --package vitest node", "npx --package vitest node"),
    ("allow", "pnpm --package vitest node", "pnpm --package vitest node"),
    ("allow", "npx --package jest node", "npx --package jest node"),
    ("allow", "pnpm --package jest node", "pnpm --package jest node"),
    ("allow", "coverage run (no -m)", "coverage run"),
    ("allow", "coverage erase", "coverage erase"),
    ("allow", "coverage run script.py", "coverage run script.py"),
    ("allow", "coverage run -m unittest", "coverage run -m unittest"),
    ("allow", "pytest-helper", "pytest-helper"),
    ("allow", "python script.py", "python script.py"),
    ("allow", "python -c print", 'python -c "print(1)"'),
    ("allow", "uv pip install pytest", "uv pip install pytest"),
    ("allow", "poetry add pytest", "poetry add pytest"),
    ("allow", "uv run (no command)", "uv run"),
    ("allow", "npm test", "npm test"),
    ("allow", "npm run vitest", "npm run vitest"),
    ("allow", "npm run test:unit", "npm run test:unit"),
    ("allow", "pnpm test:vitest", "pnpm test:vitest"),
    ("allow", "npm run jest", "npm run jest"),
    ("allow", "react-scripts test", "react-scripts test"),
    ("allow", "next test", "next test"),
    ("allow", "pnpm test", "pnpm test"),
    ("allow", "yarn test", "yarn test"),
    ("allow", "bun test", "bun test"),
    ("allow", "cargo build", "cargo build"),
    ("allow", "cargo check", "cargo check"),
    ("allow", "cargo nextest", "cargo nextest run"),
    ("allow", "bare cargo no subcommand", "cargo"),
    ("allow", "cargo double dash before test", "cargo -- test"),
    ("allow", "rustup run cargo build", "rustup run stable cargo build"),
    ("allow", "cargo-make", "cargo-make test"),
    ("allow", "just cargo test", "just test"),
    ("allow", "make cargo test", "make test"),
    ("allow", "bash cargo test", 'bash -c "cargo test"'),

    # --- False-positive sanity (must allow) ---
    ("allow", "mv pytest.ini", "mv pytest.ini pytest.ini.bak"),
    ("allow", "cd path with pytest", "cd /path/to/pytest-stuff"),
    ("allow", "cat pytest.log", "cat pytest.log"),
    ("allow", "ls", "ls -la"),
    ("allow", "echo plain", "echo 'pytest is a test runner'"),

    # --- Known limitations (currently allow — documented in adoption.md) ---
    ("allow", "[LIMITATION] bash -c pytest", 'bash -c "pytest"'),
    ("allow", "[LIMITATION] eval pytest", "eval pytest"),
    ("allow", "[LIMITATION] echo $(pytest)", "echo $(pytest)"),
    ("allow", "[LIMITATION] backticks", "echo `pytest`"),
    ("allow", "[LIMITATION] bash -c go test", 'bash -c "go test -json ./..."'),
    ("allow", "[LIMITATION] eval go test", "eval go test"),
    ("allow", "[LIMITATION] echo $(go test)", "echo $(go test)"),
    ("allow", "[LIMITATION] make test wrapping go test", "make test"),

    # --- Edge cases ---
    ("allow", "empty command", ""),
    ("allow", "unparseable shell", 'echo "unterminated'),
]


def _run_hook_with_stdin(raw_stdin):
    """Invoke hook.main() with the given raw string on stdin. Returns exit code."""
    original_stdin = sys.stdin
    original_stderr = sys.stderr
    sys.stdin = io.StringIO(raw_stdin)
    sys.stderr = io.StringIO()
    try:
        with patch.object(hook, "resolve_failtrace", return_value=("/synthetic/failtrace", None)):
            return hook.main()
    finally:
        sys.stdin = original_stdin
        sys.stderr = original_stderr


@contextlib.contextmanager
def _soak_sandbox(dir_present=True, env_value="1"):
    """Redirect the hook's soak paths into a tmpdir and set the opt-in env var.

    `dir_present` pre-creates the directory — the state the Codex shim
    dispatcher's `mkdir(parents=True, exist_ok=True)` leaves behind, and the
    one that used to be the whole gate. `env_value=None` leaves
    FAILTRACE_CC_SOAK unset; any other value is set verbatim so tests can
    check that only the exact string "1" enables logging.

    Yields the soak directory. Restores the module globals and the process
    environment on exit, so cases cannot leak into each other.
    """
    original_dir = hook.SOAK_DIR
    original_file = hook.SOAK_LOG_FILE
    original_env = os.environ.get(hook.SOAK_ENV_VAR)
    with tempfile.TemporaryDirectory() as tmp_root:
        soak_dir = Path(tmp_root) / "agent-soak"
        if dir_present:
            soak_dir.mkdir(parents=True, exist_ok=True)
        hook.SOAK_DIR = soak_dir
        hook.SOAK_LOG_FILE = soak_dir / "cc-hooks.jsonl"
        if env_value is None:
            os.environ.pop(hook.SOAK_ENV_VAR, None)
        else:
            os.environ[hook.SOAK_ENV_VAR] = env_value
        try:
            yield soak_dir
        finally:
            hook.SOAK_DIR = original_dir
            hook.SOAK_LOG_FILE = original_file
            if original_env is None:
                os.environ.pop(hook.SOAK_ENV_VAR, None)
            else:
                os.environ[hook.SOAK_ENV_VAR] = original_env


def _read_log(soak_dir):
    log = soak_dir / "cc-hooks.jsonl"
    if not log.exists():
        return []
    return [json.loads(line) for line in log.read_text().splitlines() if line.strip()]


# Required schema fields per agent-soak-telemetry design.
_REQUIRED_FIELDS = {
    "ts", "agent", "event", "session_id", "tool_use_id", "cwd",
    "command", "normalized_command", "decision", "reason",
    "suggested_command", "runner", "detection_source",
}


# (description, dir_present, env_value, payload, expected_rc,
#  expected_n_entries, field_checks)
#
# env_value is the FAILTRACE_CC_SOAK value ("1" enables; None leaves it unset).
# Directory existence is orthogonal to the gate and is varied only to prove
# that. field_checks: dict of {field: expected_value} applied to entries[0];
# None skips checks.
SOAK_CASES = [
    # --- gating: the env var alone decides ---------------------------------
    # Installing the Codex PATH shims creates ~/.failtrace/agent-soak/ for
    # the dispatcher's own stream, so the directory being there must not
    # enable hook logging.
    ("REGRESSION: dir present, env unset, deny path: no log",
     True, None,
     {"session_id": "s", "cwd": "/p", "tool_input": {"command": "pytest"}},
     2, 0, None),

    ("dir present, env unset, deny with a secret-bearing arg: no log",
     True, None,
     {"session_id": "s", "cwd": "/p",
      "tool_input": {"command": "pytest --token=hunter2"}},
     2, 0, None),

    ("env='0' is not opt-in: no log",
     True, "0",
     {"session_id": "s", "cwd": "/p", "tool_input": {"command": "pytest"}},
     2, 0, None),

    ("env='true' is not opt-in: only the exact string '1' enables",
     True, "true",
     {"session_id": "s", "cwd": "/p", "tool_input": {"command": "pytest"}},
     2, 0, None),

    ("dir absent, env on, deny path: writer creates the dir and logs",
     False, "1",
     {"session_id": "s", "cwd": "/p", "tool_input": {"command": "pytest"}},
     2, 1,
     {"decision": "deny", "reason": "bare_runner_invocation"}),

    # --- event filtering: only deny + package-script survive ---------------
    ("env on, non-test command: no_runner_detected is dropped",
     True, "1",
     {"session_id": "s2", "cwd": "/p2", "tool_input": {"command": "ls -la"}},
     0, 0, None),

    ("env on, empty command: skip is dropped",
     True, "1",
     {"session_id": "s3", "cwd": "/p3", "tool_input": {"command": ""}},
     0, 0, None),

    ("env on, wrapped pytest: pass is dropped",
     True, "1",
     {"session_id": "s4", "cwd": "/p4",
      "tool_input": {"command": "failtrace run -- pytest"}},
     0, 0, None),

    ("env on, bare pytest: deny entry, command null",
     True, "1",
     {"session_id": "s1", "cwd": "/p1", "tool_input": {"command": "pytest"}},
     2, 1,
     {"agent": "cc", "event": "PreToolUse", "session_id": "s1", "cwd": "/p1",
      "command": None, "normalized_command": "pytest",
      "decision": "deny", "reason": "bare_runner_invocation",
      "runner": "pytest", "suggested_command": "failtrace run -- pytest"}),

    # --- redaction end-to-end through the writer ---------------------------
    ("env on, --flag=value secret redacted in both command fields",
     True, "1",
     {"session_id": "r1", "cwd": "/r1",
      "tool_input": {"command": "pytest --token=hunter2"}},
     2, 1,
     {"command": None,
      "normalized_command": "pytest --token=<redacted>",
      "suggested_command": "failtrace run -- pytest --token=<redacted>"}),

    ("env on, --flag value pair form secret redacted",
     True, "1",
     {"session_id": "r2", "cwd": "/r2",
      "tool_input": {"command": "pytest --password hunter2 -q"}},
     2, 1,
     {"command": None,
      "normalized_command": "pytest --password <redacted> -q",
      "suggested_command": "failtrace run -- pytest --password <redacted> -q"}),

    ("env on, mid-command KEY=value redacted (strip_prefixes only peels leading)",
     True, "1",
     {"session_id": "r3", "cwd": "/r3",
      "tool_input": {"command": "pytest API_KEY=abc123 -q"}},
     2, 1,
     {"command": None,
      "normalized_command": "pytest API_KEY=<redacted> -q",
      "suggested_command": "failtrace run -- pytest API_KEY=<redacted> -q"}),

    ("env on, non-secret flags survive redaction intact",
     True, "1",
     {"session_id": "r4", "cwd": "/r4",
      "tool_input": {"command": "pytest -k smoke --maxfail=2"}},
     2, 1,
     {"normalized_command": "pytest -k smoke --maxfail=2",
      "suggested_command": "failtrace run -- pytest -k smoke --maxfail=2"}),

    # --- the non-executing exemption family, end to end -------------------
    ("env on, exempted invocation: pass row, rc 0, runner populated",
     True, "1",
     {"session_id": "n1", "cwd": "/n1",
      "tool_input": {"command": "pytest --collect-only -q"}},
     0, 1,
     {"agent": "cc", "command": None,
      "normalized_command": "pytest --collect-only -q",
      "decision": "pass", "reason": "non_executing_runner_invocation",
      "runner": "pytest", "detection_source": "argv",
      "suggested_command": None}),

    ("env on, exempted cargo compile-only: pass row",
     True, "1",
     {"session_id": "n2", "cwd": "/n2",
      "tool_input": {"command": "cargo test --no-run"}},
     0, 1,
     {"decision": "pass", "reason": "non_executing_runner_invocation",
      "runner": "cargo-test", "detection_source": "argv", "command": None}),

    ("env on, exempted invocation still redacts",
     True, "1",
     {"session_id": "n3", "cwd": "/n3",
      "tool_input": {"command": "pytest --collect-only --token=hunter2"}},
     0, 1,
     {"command": None,
      "normalized_command": "pytest --collect-only --token=<redacted>",
      "reason": "non_executing_runner_invocation"}),

    ("env unset, exempted invocation: still nothing written",
     True, None,
     {"session_id": "n4", "cwd": "/n4",
      "tool_input": {"command": "pytest --collect-only"}},
     0, 0, None),

    ("env on, chained: first segment logged",
     True, "1",
     {"session_id": "s5", "cwd": "/p5",
      "tool_input": {"command": "pytest a && cargo test"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "suggested_command": "failtrace run -- pytest a"}),

    ("dir present, modifier-stripped normalization",
     True, "1",
     {"session_id": "s6", "cwd": "/p6",
      "tool_input": {"command": "env PYTHONPATH=. pytest -q"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "normalized_command": "pytest -q",
      "suggested_command": "failtrace run -- pytest -q"}),

    # --- suggested_command strips trailing shell redirections (B1) ---
    ("dir present, 2>&1 stripped from suggestion",
     True, "1",
     {"session_id": "s7", "cwd": "/p7",
      "tool_input": {"command": "pytest 2>&1"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "suggested_command": "failtrace run -- pytest"}),

    ("dir present, 2>&1 and pipe both stripped from suggestion",
     True, "1",
     {"session_id": "s8", "cwd": "/p8",
      "tool_input": {"command": "pytest -q 2>&1 | tail -20"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "suggested_command": "failtrace run -- pytest -q"}),

    ("dir present, > file redirection stripped from suggestion",
     True, "1",
     {"session_id": "s9", "cwd": "/p9",
      "tool_input": {"command": "pytest -q > out.log"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "suggested_command": "failtrace run -- pytest -q"}),

    ("dir present, plain > keeps preceding numeric argument",
     True, "1",
     {"session_id": "s10", "cwd": "/p10",
      "tool_input": {"command": "pytest -n 2 > out.log"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "suggested_command": "failtrace run -- pytest -n 2"}),

    ("dir present, 2>&1 FD digit dropped but argument digit kept",
     True, "1",
     {"session_id": "s11", "cwd": "/p11",
      "tool_input": {"command": "pytest -n 2 2>&1"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "suggested_command": "failtrace run -- pytest -n 2"}),

    ("dir present, &> keeps preceding numeric argument (not an FD)",
     True, "1",
     {"session_id": "s12", "cwd": "/p12",
      "tool_input": {"command": "pytest -n 2 &> out.log"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "suggested_command": "failtrace run -- pytest -n 2"}),

    ("dir present, &>> keeps preceding numeric argument (not an FD)",
     True, "1",
     {"session_id": "s13", "cwd": "/p13",
      "tool_input": {"command": "pytest -n 2 &>> out.log"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "suggested_command": "failtrace run -- pytest -n 2"}),

    ("dir present, >| force-clobber redirection stripped",
     True, "1",
     {"session_id": "s14", "cwd": "/p14",
      "tool_input": {"command": "pytest >| out.log"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "suggested_command": "failtrace run -- pytest"}),

    # Known limitations — pinned so they are not mistaken for regressions.
    # `<arg> > file` and `<fd>> file` are indistinguishable once shlex has
    # discarded whitespace, so a digit adjacent to a redirection cannot be
    # classified perfectly. See the strip_redirections docstring.
    ("dir present, KNOWN LIMITATION: 2>/dev/null leaves dangling 2",
     True, "1",
     {"session_id": "s15", "cwd": "/p15",
      "tool_input": {"command": "pytest 2>/dev/null"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "suggested_command": "failtrace run -- pytest 2"}),

    ("dir present, KNOWN LIMITATION: >&2 after numeric arg mis-drops it",
     True, "1",
     {"session_id": "s16", "cwd": "/p16",
      "tool_input": {"command": "pytest -n 2 >&2"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "suggested_command": "failtrace run -- pytest -n"}),

    # --- redirection clauses excised in place, not truncated-at-first, so
    # runner args after a redirection survive ---
    ("dir present, > redirection mid-command keeps trailing args",
     True, "1",
     {"session_id": "s17", "cwd": "/p17",
      "tool_input": {"command": "pytest > out.log -k smoke"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "suggested_command": "failtrace run -- pytest -k smoke"}),

    ("dir present, 2>&1 mid-command keeps trailing args",
     True, "1",
     {"session_id": "s18", "cwd": "/p18",
      "tool_input": {"command": "pytest 2>&1 -k smoke"}},
     2, 1,
     {"decision": "deny", "runner": "pytest",
      "suggested_command": "failtrace run -- pytest -k smoke"}),
]


def run_soak_writer_cases():
    """Verify the soak JSONL writer behavior. Returns (passed, failed, failures)."""
    passed = 0
    failed = 0
    failures = []

    for (desc, dir_present, env_value, payload, expected_rc, expected_n,
         field_checks) in SOAK_CASES:
        with _soak_sandbox(dir_present, env_value) as soak_dir:
            rc = _run_hook_with_stdin(json.dumps(payload))
            entries = _read_log(soak_dir)

            problems = []
            if rc != expected_rc:
                problems.append(f"rc: expected {expected_rc}, got {rc}")
            if len(entries) != expected_n:
                problems.append(f"entries: expected {expected_n}, got {len(entries)}")
            if entries and field_checks is not None:
                entry = entries[0]
                missing = _REQUIRED_FIELDS - set(entry)
                if missing:
                    problems.append(f"missing required fields: {sorted(missing)}")
                for field, expected_value in field_checks.items():
                    actual = entry.get(field)
                    if actual != expected_value:
                        problems.append(
                            f"field {field!r}: expected {expected_value!r}, got {actual!r}"
                        )

            if problems:
                failed += 1
                failures.append((desc, problems))
            else:
                passed += 1

    # Extra case: invalid JSON payload is a skip, and skips are not logged.
    with _soak_sandbox() as soak_dir:
        rc = _run_hook_with_stdin("this is not json")
        entries = _read_log(soak_dir)
        problems = []
        if rc != 0:
            problems.append(f"rc: expected 0, got {rc}")
        if entries:
            problems.append(f"expected no entries for a skip, got {entries!r}")
        if problems:
            failed += 1
            failures.append(("invalid JSON payload: skip is filtered out", problems))
        else:
            passed += 1

    # Extra case: multiple invocations append (not overwrite). The `ls` in the
    # middle is filtered, so two rows land, not three.
    with _soak_sandbox() as soak_dir:
        for cmd in ("pytest", "ls", "cargo test"):
            _run_hook_with_stdin(json.dumps(
                {"session_id": "s", "cwd": "/p", "tool_input": {"command": cmd}}
            ))
        entries = _read_log(soak_dir)
        if len(entries) != 2:
            failed += 1
            failures.append(("multiple invocations append",
                             [f"expected 2 entries, got {len(entries)}"]))
        else:
            runners = [e["runner"] for e in entries]
            if runners != ["pytest", "cargo-test"]:
                failed += 1
                failures.append(("multiple invocations append",
                                 [f"runner sequence: {runners}"]))
            else:
                passed += 1

    # Extra case: newly-created log file has owner-only perms (0o600), and the
    # writer created the soak directory itself (0o700) rather than requiring it.
    with _soak_sandbox(dir_present=False) as soak_dir:
        _run_hook_with_stdin(json.dumps(
            {"session_id": "s", "cwd": "/p", "tool_input": {"command": "pytest"}}
        ))
        problems = []
        if not soak_dir.is_dir():
            problems.append("writer did not create SOAK_DIR")
        else:
            dir_mode = stat.S_IMODE(os.stat(soak_dir).st_mode)
            if dir_mode != 0o700:
                problems.append(f"dir perms: expected 0o700, got {oct(dir_mode)}")
            log_path = soak_dir / "cc-hooks.jsonl"
            if not log_path.exists():
                problems.append("no log file written")
            else:
                mode = stat.S_IMODE(os.stat(log_path).st_mode)
                if mode != 0o600:
                    problems.append(f"log perms: expected 0o600, got {oct(mode)}")
        if problems:
            failed += 1
            failures.append(("writer creates 0o700 dir and 0o600 log", problems))
        else:
            passed += 1

    # Extra case: pre-existing log file with looser perms is corrected. Uses a
    # denied command — a filtered one would never open the file.
    with _soak_sandbox() as soak_dir:
        # Pre-seed the log file with 0o644 perms to simulate a file
        # created by an older hook version.
        log_path = soak_dir / "cc-hooks.jsonl"
        log_path.write_text('{"agent":"cc","decision":"deny"}\n')
        os.chmod(log_path, 0o644)
        assert stat.S_IMODE(os.stat(log_path).st_mode) == 0o644
        # Next hook invocation should chmod to 0o600.
        _run_hook_with_stdin(json.dumps(
            {"session_id": "s", "cwd": "/p", "tool_input": {"command": "pytest"}}
        ))
        mode = stat.S_IMODE(os.stat(log_path).st_mode)
        if mode != 0o600:
            failed += 1
            failures.append(("pre-existing 0o644 log file is corrected to 0o600",
                             [f"expected 0o600, got {oct(mode)}"]))
        else:
            passed += 1

    # Extra case: the raw command never reaches disk, even when the log
    # already exists and the command is dense with secrets.
    with _soak_sandbox() as soak_dir:
        secret_cmd = ("pytest --api-key=sk-live-999 --Password hunter2 "
                      "DB_TOKEN=deadbeef -q")
        _run_hook_with_stdin(json.dumps(
            {"session_id": "s", "cwd": "/p", "tool_input": {"command": secret_cmd}}
        ))
        blob = (soak_dir / "cc-hooks.jsonl").read_text()
        leaked = [s for s in ("sk-live-999", "hunter2", "deadbeef") if s in blob]
        if leaked:
            failed += 1
            failures.append(("no secret material reaches the log file",
                             [f"leaked: {leaked}", f"blob: {blob!r}"]))
        else:
            passed += 1

    return passed, failed, failures


# (description, input_tokens, expected_tokens) — direct unit cases for the
# pure redactor, independent of the writer.
REDACT_CASES = [
    ("plain command untouched",
     ["pytest", "-q"], ["pytest", "-q"]),

    ("mid-command KEY=value keeps the key, drops the value",
     ["make", "DEPLOY_TOKEN=abc"], ["make", "DEPLOY_TOKEN=<redacted>"]),

    ("KEY=value with = in the value drops all of it",
     ["make", "SECRET=a=b=c"], ["make", "SECRET=<redacted>"]),

    ("non-secret KEY=value is still redacted (any assignment may be a secret)",
     ["make", "TARGET=release"], ["make", "TARGET=<redacted>"]),

    ("--flag=value secret",
     ["curl", "--token=abc"], ["curl", "--token=<redacted>"]),

    ("--flag=value secret, mixed case flag name",
     ["curl", "--API-Key=abc"], ["curl", "--API-Key=<redacted>"]),

    ("--flag value pair form secret",
     ["curl", "--password", "hunter2"], ["curl", "--password", "<redacted>"]),

    ("--flag value pair form, trailing args survive",
     ["curl", "--secret", "s3cr3t", "https://example.com"],
     ["curl", "--secret", "<redacted>", "https://example.com"]),

    ("hyphenated secret flag matches on substring",
     ["gh", "--auth-token", "ghp_x"], ["gh", "--auth-token", "<redacted>"]),

    ("non-secret --flag=value untouched",
     ["pytest", "--maxfail=2"], ["pytest", "--maxfail=2"]),

    ("non-secret bare flag does not consume its value",
     ["pytest", "-k", "smoke"], ["pytest", "-k", "smoke"]),

    ("secret flag at end of tokens has nothing to redact",
     ["curl", "--token"], ["curl", "--token"]),

    ("over-redaction is accepted: a flag following a secret flag is eaten",
     ["curl", "--token", "--verbose"], ["curl", "--token", "<redacted>"]),

    ("multiple secrets in one command",
     ["run", "--token=a", "KEY=b", "--secret", "c"],
     ["run", "--token=<redacted>", "KEY=<redacted>", "--secret", "<redacted>"]),

    ("empty token list",
     [], []),
]


def run_redaction_cases():
    """Unit-test the pure token redactor. Returns (passed, failed, failures)."""
    passed = 0
    failed = 0
    failures = []
    for desc, tokens, expected in REDACT_CASES:
        got = hook.redact_tokens(list(tokens))
        if got == expected:
            passed += 1
        else:
            failed += 1
            failures.append((desc, [f"expected {expected!r}, got {got!r}"]))
    return passed, failed, failures


# ======================================================================
# Enforcement-policy cases: what the hook DOES with a detection, as
# distinct from whether a runner is detected at all (that's CASES).
#
# Three verdicts:
#   deny   — enforceable bare runner
#   exempt — runner detected, but the invocation executes no tests
#            (non_executing=True, main() passes with telemetry)
#   allow  — no enforceable detection at all (includes wrapped segments)
#
# Seeded from real denied invocations.
# ======================================================================

# Commands write `{SHIM}` where the shim directory goes; `run_policy_cases`
# substitutes the real temp path it built for the run. The directory is real
# and holds stub files, because the PATH heuristic now requires the shim for
# the head command to actually exist — a lexical-only fake would make every
# PATH "allow" case pass for the wrong reason. Stubs exist for exactly these
# two runners, so a case using any other head doubles as the
# missing-shim/partial-install case.
_SHIM_STUB_NAMES = ("pytest", "xcodebuild")

POLICY_CASES = [
    # --- item 1: non-executing invocations -----------------------------
    ("exempt", "pytest collection through python -m",
     ".venv/bin/python -m pytest --collect-only -q -p no:cacheprovider"),
    ("exempt", "cargo test compile-only",
     "cargo test -p example-package --test integration --no-run"),
    ("exempt", "pytest capability probe", "pytest --version"),

    ("exempt", "pytest --collect-only", "pytest --collect-only"),
    ("exempt", "pytest --co short alias", "pytest --co"),
    ("exempt", "pytest --help", "pytest --help"),
    ("exempt", "pytest -h", "pytest -h"),
    ("exempt", "pytest --fixtures lists and exits", "pytest --fixtures"),
    ("exempt", "pytest --markers lists and exits", "pytest --markers"),
    ("exempt", "pytest exemption survives a uv run wrapper",
     "uv run pytest --collect-only"),
    ("exempt", "cargo test --no-run", "cargo test --no-run"),
    ("exempt", "cargo test --help", "cargo test --help"),
    ("exempt", "cargo test -h", "cargo test -h"),
    ("exempt", "jest --listTests", "npx jest --listTests"),
    ("exempt", "jest --version", "npx jest --version"),
    ("exempt", "jest --help", "npx jest --help"),
    ("allow", "vitest --version without one-shot mode", "npx vitest --version"),
    ("allow", "vitest --help without one-shot mode", "npx vitest --help"),
    ("exempt", "vitest run --version", "npx vitest run --version"),
    ("exempt", "vitest run --help", "npx vitest run --help"),
    ("exempt", "tsc --version", "npx tsc --version"),
    ("exempt", "tsc -v", "npx tsc -v"),
    ("exempt", "tsc --help", "npx tsc --help"),
    ("exempt", "tsc -h", "npx tsc -h"),
    ("exempt", "tsc --showConfig", "npx tsc --showConfig"),
    ("exempt", "tsc --all", "npx tsc --all"),
    ("exempt", "go test --help", "go test --help"),
    ("exempt", "go test -h", "go test -h"),
    ("exempt", "go test -help (Go flag package spelling)", "go test -help"),
    ("exempt", "go test -list runs nothing", "go test -list ."),

    # Near misses: these all still execute tests and must keep denying.
    ("deny", "pytest --continue-on-collection-errors is not --co",
     "pytest --continue-on-collection-errors"),
    ("deny", "tsc --noEmit still typechecks", "npx tsc --noEmit"),
    ("deny", "cargo test without --no-run", "cargo test"),
    ("deny", "plain pytest with a path", "pytest tests/"),
    ("deny", "pytest -v is verbose, not --version", "pytest -v"),
    ("deny", "vitest run is a real run", "vitest run"),
    ("deny", "go test with a package pattern", "go test ./..."),
    ("deny", "xcodebuild test has no exemption family", "xcodebuild test"),
    ("deny", "--version belonging to another runner does not cross over",
     "pytest --listTests"),

    # Segment scoping: an exemption in one segment must not cover another.
    ("deny", "exempt segment then bare runner: the bare one still wins",
     "pytest --collect-only && pytest tests/"),
    ("deny", "bare runner then exempt segment",
     "cargo test && cargo test --no-run"),
    ("exempt", "two exempt segments stay exempt",
     "pytest --collect-only && cargo test --no-run"),

    # --- item 2: failtrace's own codex shims are wrapped ----------------
    # Head-path form. Judged lexically — no stub file is required for any of
    # these, which is the point of the `vitest` case below: an explicitly
    # typed shim path is unambiguous intent and must never acquire a
    # filesystem dependency the way the PATH heuristic did.
    ("allow", "xcodebuild invoked through the shim path",
     "{SHIM}/xcodebuild test -scheme App"),
    ("allow", "shim path via ~ expansion",
     "~/.failtrace/codex-shims/pytest tests/"),
    ("allow", "head-path form needs no stub on disk (stays lexical)",
     "{SHIM}/vitest run"),
    ("allow", "literal failtrace run is still wrapped",
     "failtrace run -- pytest tests/"),

    # PATH form. Held to what the shell would actually resolve.
    ("allow", "shim dir prepended to PATH",
     'PATH="{SHIM}:$PATH" xcodebuild test -scheme App'),
    ("allow", "shim dir prepended via env prefix",
     'env PATH="{SHIM}:$PATH" pytest tests/'),
    # shlex leaves `$HOME` literal and expanduser only
    # knows `~`, so this genuinely-wrapped shape was being denied.
    ("allow", "$HOME-relative shim dir in PATH",
     'PATH="$HOME/.failtrace/codex-shims:$PATH" pytest tests/'),
    ("allow", "${HOME}-relative shim dir in PATH",
     'PATH="${HOME}/.failtrace/codex-shims:$PATH" pytest tests/'),
    ("allow", "last PATH assignment wins, and it is the shim one",
     'PATH=/usr/bin PATH="{SHIM}:$PATH" pytest tests/'),

    # Near misses: nothing about a path or a PATH= may weaken enforcement.
    ("deny", "non-shim absolute path to a real runner",
     "/usr/local/bin/pytest tests/"),
    ("deny", "shim dir APPENDED to PATH: real binary still wins",
     'PATH="$PATH:{SHIM}" pytest tests/'),
    ("deny", "unrelated PATH assignment changes nothing",
     'PATH="/opt/homebrew/bin:$PATH" pytest tests/'),
    ("deny", "a sibling directory of the shim dir is not the shim dir",
     "{SHIM}-old/pytest tests/"),
    ("deny", "a subdirectory of the shim dir is not the shim dir",
     "{SHIM}/nested/pytest tests/"),
    ("deny", "shim-shaped relative path is not the shim dir",
     "./codex-shims/pytest tests/"),
    ("deny", "PATH shim dir prepended but a LATER segment is bare",
     'PATH="{SHIM}:$PATH" pytest a && pytest b'),
    # An explicit-path head bypasses PATH resolution,
    # so a shim-first PATH prefix wraps nothing — the runner still runs bare.
    ("deny", "PATH shim prefix does not wrap an explicit-path head",
     'PATH="{SHIM}:$PATH" /usr/bin/pytest tests/'),
    ("deny", "PATH shim prefix does not wrap a relative-path head",
     'PATH="{SHIM}:$PATH" ./bin/pytest tests/'),
    # The shell uses the FINAL assignment.
    ("deny", "later PATH assignment overrides the shim-first one",
     'PATH="{SHIM}:$PATH" PATH=/usr/bin pytest tests/'),
    # Without a shim for this head the shell falls
    # through to the later PATH entries and runs the real binary bare.
    ("deny", "shim dir first but no shim installed for this runner",
     'PATH="{SHIM}:$PATH" cargo test'),
    ("deny", "shim dir first, no shim for this runner, $HOME form",
     'PATH="$HOME/.failtrace/codex-shims:$PATH" go test ./...'),
    ("deny", "only $HOME/${HOME} are expanded, not arbitrary variables",
     'PATH="$MYROOT/.failtrace/codex-shims:$PATH" pytest tests/'),
]


def _policy_verdict(cmd):
    detection, _seg = hook.check_command(cmd)
    if detection is None:
        return "allow"
    if detection.non_executing:
        return "exempt"
    return "deny"


def run_policy_cases():
    """Verify enforcement policy on top of detection. Returns (p, f, failures)."""
    passed = 0
    failed = 0
    failures = []
    original_shim_dir = hook.CODEX_SHIM_DIR
    original_home = os.environ.get("HOME")
    original_cwd = os.getcwd()
    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        # A real fake home: $HOME is pinned to it and the shim directory lives
        # under it at the installer's canonical relative location, so the `~`,
        # `$HOME` and absolute `{SHIM}` spellings in the table all denote the
        # same directory on any machine (and in CI). `os.path.expanduser`
        # reads $HOME at call time, which is what makes the pin take effect.
        fake_home = root / "home"
        shim_dir = fake_home / ".failtrace" / "codex-shims"
        shim_dir.mkdir(parents=True)
        for name in _SHIM_STUB_NAMES:
            stub = shim_dir / name
            stub.write_text("#!/bin/sh\n")
            stub.chmod(0o755)
        hook.CODEX_SHIM_DIR = shim_dir
        os.environ["HOME"] = str(fake_home)
        # Run from a package.json-free directory so script resolution stays
        # inert, and so no `{SHIM}`-relative path accidentally resolves.
        workdir = root / "work"
        workdir.mkdir()
        os.chdir(workdir)
        try:
            hook._PACKAGE_JSON_CACHE.clear()
            for expected, desc, template in POLICY_CASES:
                cmd = template.replace("{SHIM}", str(shim_dir))
                got = _policy_verdict(cmd)
                if got == expected:
                    passed += 1
                else:
                    failed += 1
                    failures.append((desc, cmd, [f"expected {expected}, got {got}"]))
        finally:
            os.chdir(original_cwd)
            hook._PACKAGE_JSON_CACHE.clear()
            hook.CODEX_SHIM_DIR = original_shim_dir
            if original_home is None:
                os.environ.pop("HOME", None)
            else:
                os.environ["HOME"] = original_home
    return passed, failed, failures


# ======================================================================
# ======================================================================
# Package.json script-resolution cases.
#
# Each case sets up a package.json in a temp dir, chdirs into it, runs
# check_command, then restores cwd and clears the hook's per-process
# cache. Tests both positive (script body wraps a runner -> block) and
# negative (script body doesn't, package.json missing, script absent,
# malformed JSON, deep recursion) paths.
# ======================================================================
# (expected, description, command, package_json_dict)
# expected = "block" / "allow"
# package_json_dict = None -> no package.json written; {} -> empty file;
# otherwise the dict is JSON-encoded into package.json at the temp root.
NPM_RUN_CASES = [
    # --- npm run resolves to a runner -> block ---
    ("block", "npm run typecheck -> tsc", "npm run typecheck",
     {"scripts": {"typecheck": "tsc -p tsconfig.app.json --noEmit"}}),
    ("block", "npm run test -> pytest", "npm run test",
     {"scripts": {"test": "pytest tests/ -q"}}),
    ("block", "npm run unit -> vitest", "npm run unit",
     {"scripts": {"unit": "vitest run"}}),
    ("block", "npm run e2e -> jest", "npm run e2e",
     {"scripts": {"e2e": "jest --ci --runInBand"}}),
    ("block", "npm run typecheck script body has env-prefix",
     "npm run typecheck",
     {"scripts": {"typecheck": "NODE_ENV=test tsc -p tsconfig.app.json"}}),
    ("block", "npm run typecheck via uv-style wrapper inside body",
     "npm run typecheck",
     {"scripts": {"typecheck": "uv run pytest tests/"}}),
    ("block", "npm run chain depth 1: a -> npm run b -> tsc", "npm run a",
     {"scripts": {"a": "npm run b", "b": "tsc -p ."}}),
    ("block", "npm run chain depth 2: ci -> check -> typecheck -> tsc", "npm run ci",
     {"scripts": {"ci": "npm run check", "check": "npm run typecheck", "typecheck": "tsc -p ."}}),

    # --- run-script alias (npm + pnpm) -> block ---
    ("block", "npm run-script typecheck (alias) -> tsc", "npm run-script typecheck",
     {"scripts": {"typecheck": "tsc -p tsconfig.json"}}),
    ("block", "pnpm run-script typecheck (alias) -> tsc", "pnpm run-script typecheck",
     {"scripts": {"typecheck": "tsc -p tsconfig.json"}}),

    # --- flags between `run` and the script name are skipped ---
    ("block", "npm run --if-present typecheck -> tsc",
     "npm run --if-present typecheck",
     {"scripts": {"typecheck": "tsc -p tsconfig.json"}}),
    ("block", "npm run --silent typecheck -> tsc",
     "npm run --silent typecheck",
     {"scripts": {"typecheck": "tsc -p tsconfig.json"}}),
    ("block", "yarn berry: yarn run -T typecheck -> tsc",
     "yarn run -T typecheck",
     {"scripts": {"typecheck": "tsc -p tsconfig.json"}}),

    # --- pnpm run resolves to a runner -> block ---
    ("block", "pnpm run typecheck -> tsc", "pnpm run typecheck",
     {"scripts": {"typecheck": "tsc -p tsconfig.json"}}),

    # --- yarn run resolves to a runner -> block ---
    ("block", "yarn run typecheck -> tsc", "yarn run typecheck",
     {"scripts": {"typecheck": "tsc -p tsconfig.json"}}),

    # --- bun run resolves to a runner -> block ---
    ("block", "bun run typecheck -> tsc", "bun run typecheck",
     {"scripts": {"typecheck": "tsc -p tsconfig.json"}}),

    # --- compound script body: any segment wrapping a runner -> block ---
    ("block", "npm run check -> tsc && eslint", "npm run check",
     {"scripts": {"check": "tsc -p . && eslint ."}}),
    ("block", "npm run check -> eslint && tsc (later segment)", "npm run check",
     {"scripts": {"check": "eslint . && tsc -p ."}}),

    # --- forwarded `-- <args>` attaches to last segment (npm sh -c semantics) ---
    # Single-segment body: forwarded --watch attaches to tsc, triggering watch-mode reject.
    ("allow", "npm run typecheck -- --watch -> tsc watch-mode reject",
     "npm run typecheck -- --watch",
     {"scripts": {"typecheck": "tsc -p ."}}),
    # Multi-segment body: forwarded --watch attaches to the LAST (eslint) segment,
    # so the first (tsc) segment is still bare and triggers a block.
    ("block", "npm run check -- --no-cache (forwarded attaches to last segment)",
     "npm run check -- --no-cache",
     {"scripts": {"check": "tsc -p . && eslint ."}}),
    # Trailing `--` with no forwarded args is a no-op.
    ("block", "npm run typecheck -- (trailing dash dash, no args) -> tsc",
     "npm run typecheck --",
     {"scripts": {"typecheck": "tsc -p ."}}),

    # --- script body does NOT wrap a runner -> allow ---
    ("allow", "npm run dev -> vite", "npm run dev",
     {"scripts": {"dev": "vite"}}),
    ("allow", "npm run build -> webpack", "npm run build",
     {"scripts": {"build": "webpack --config webpack.config.js"}}),
    ("allow", "npm run lint -> eslint", "npm run lint",
     {"scripts": {"lint": "eslint . --max-warnings 0"}}),
    ("allow", "npm run format -> prettier", "npm run format",
     {"scripts": {"format": "prettier --write ."}}),

    # --- directory-redirect / workspace-filter flags fail-open (don't know which package.json) ---
    ("allow", "npm --prefix web run typecheck (directory redirect, fail-open)",
     "npm --prefix web run typecheck",
     {"scripts": {"typecheck": "tsc -p ."}}),
    ("allow", "pnpm -C web run typecheck (directory redirect, fail-open)",
     "pnpm -C web run typecheck",
     {"scripts": {"typecheck": "tsc -p ."}}),
    ("allow", "pnpm --filter app run typecheck (workspace filter, fail-open)",
     "pnpm --filter app run typecheck",
     {"scripts": {"typecheck": "tsc -p ."}}),
    ("allow", "yarn --cwd web run typecheck (directory redirect, fail-open)",
     "yarn --cwd web run typecheck",
     {"scripts": {"typecheck": "tsc -p ."}}),
    ("allow", "pnpm --filter=app run typecheck (=value form, fail-open)",
     "pnpm --filter=app run typecheck",
     {"scripts": {"typecheck": "tsc -p ."}}),

    # --- script not in package.json -> allow ---
    ("allow", "npm run nonexistent -> no entry", "npm run typecheck",
     {"scripts": {"dev": "vite"}}),
    ("allow", "npm run typecheck with no scripts mapping",
     "npm run typecheck", {"name": "x", "version": "0.0.0"}),
    ("allow", "npm run with empty scripts mapping",
     "npm run typecheck", {"scripts": {}}),

    # --- pre-existing allow-listed forms remain allowed when matching script is absent ---
    # These are the legacy "binary REJECTS these" cases (CASES line 308) with a
    # real package.json present but no matching script entry. The cross-doc
    # contract: argv-based allows still apply when introspection finds nothing.
    ("allow", "npm run vitest (script absent in real package.json)", "npm run vitest",
     {"scripts": {"dev": "vite", "build": "webpack"}}),
    ("allow", "npm run jest (script absent in real package.json)", "npm run jest",
     {"scripts": {"dev": "vite"}}),
    ("allow", "npm run test:unit (script absent in real package.json)", "npm run test:unit",
     {"scripts": {"build": "webpack"}}),

    # --- malformed / missing package.json -> allow ---
    ("allow", "npm run typecheck with no package.json", "npm run typecheck", None),
    ("allow", "npm run typecheck with malformed JSON (raw string)",
     "npm run typecheck", "MALFORMED_RAW_JSON"),
    ("allow", "npm run typecheck with non-dict JSON root (array)",
     "npm run typecheck", "JSON_ARRAY_ROOT"),

    # --- non-string script bodies are skipped (allow) ---
    ("allow", "npm run script body is array (allow)", "npm run typecheck",
     {"scripts": {"typecheck": ["tsc", "-p", "."]}}),
    ("allow", "npm run script body is object (allow)", "npm run typecheck",
     {"scripts": {"typecheck": {"cmd": "tsc"}}}),

    # --- recursion bounds: depth-limited chains terminate without crash ---
    ("allow", "npm run cycle a->b->a hits recursion limit",
     "npm run a",
     {"scripts": {"a": "npm run b", "b": "npm run a"}}),

    # --- short forms (yarn 1 / pnpm 7+ / bun) NOT introspected ---
    # Deliberately skipped — distinguishing builtin vs script needs per-version data.
    ("allow", "yarn typecheck (short form, not introspected)", "yarn typecheck",
     {"scripts": {"typecheck": "tsc -p ."}}),
    ("allow", "pnpm typecheck (short form, not introspected)", "pnpm typecheck",
     {"scripts": {"typecheck": "tsc -p ."}}),
    ("allow", "bun typecheck (short form, not introspected)", "bun typecheck",
     {"scripts": {"typecheck": "tsc -p ."}}),
]


def _write_package_json_for_case(td_path, pkg):
    """Materialize a test-case package.json. Supports dict (normal) plus two
    string sentinels for fail-open-on-bad-input coverage."""
    if pkg is None:
        return
    target = td_path / "package.json"
    if pkg == "MALFORMED_RAW_JSON":
        target.write_text("{this is not valid JSON")
    elif pkg == "JSON_ARRAY_ROOT":
        target.write_text("[]")
    else:
        target.write_text(json.dumps(pkg))


def run_npm_run_script_cases():
    """Run npm-run script-resolution cases against per-case package.json fixtures.

    Returns (passed, failed, failures).
    """
    passed = 0
    failed = 0
    failures = []
    original_cwd = os.getcwd()

    for expected, desc, cmd, pkg in NPM_RUN_CASES:
        with tempfile.TemporaryDirectory() as td:
            _write_package_json_for_case(Path(td), pkg)
            os.chdir(td)
            try:
                hook._PACKAGE_JSON_CACHE.clear()
                runner, _seg = hook.check_command(cmd)
                got = "block" if runner is not None else "allow"
                if got == expected:
                    passed += 1
                else:
                    failed += 1
                    failures.append((desc, cmd, expected, got, runner))
            finally:
                os.chdir(original_cwd)
                hook._PACKAGE_JSON_CACHE.clear()

    # --- Ancestor-walk: package.json in parent, command from child dir ---
    # Pin that _load_package_json walks up from cwd to find the nearest package.json.
    with tempfile.TemporaryDirectory() as td:
        td_path = Path(td)
        (td_path / "package.json").write_text(
            json.dumps({"scripts": {"typecheck": "tsc -p ."}})
        )
        child = td_path / "src"
        child.mkdir()
        os.chdir(child)
        try:
            hook._PACKAGE_JSON_CACHE.clear()
            detection, _seg = hook.check_command("npm run typecheck")
            # The ancestor walk must still resolve to tsc at the detector
            # level. The main() boundary now passes (with telemetry) rather
            # than denying — that's exercised by run_main_boundary_npm_run_cases.
            if (detection is not None
                    and detection.runner == "tsc"
                    and detection.source == hook.DETECTION_SOURCE_PACKAGE_SCRIPT):
                passed += 1
            else:
                failed += 1
                failures.append(
                    ("npm run from child dir walks up to parent package.json",
                     "npm run typecheck", "tsc/package_script",
                     "no detection" if detection is None else repr(detection), detection)
                )
        finally:
            os.chdir(original_cwd)
            hook._PACKAGE_JSON_CACHE.clear()

    # --- Deeply nested JSON pin: fail-open against RecursionError ---
    # Build a value with many nested arrays; assert no crash, decision = allow.
    with tempfile.TemporaryDirectory() as td:
        depth = 4000
        nested = "[" * depth + "0" + "]" * depth
        (Path(td) / "package.json").write_text(
            '{"scripts": {"typecheck": "tsc -p ."}, "deeply": ' + nested + "}"
        )
        os.chdir(td)
        try:
            hook._PACKAGE_JSON_CACHE.clear()
            try:
                runner, _seg = hook.check_command("npm run typecheck")
                # Either the JSON parser caught the nesting (returns allow)
                # or it parsed and we get block. Both are acceptable —
                # the contract is "do not crash."
                passed += 1
            except RecursionError:
                failed += 1
                failures.append(
                    ("deeply nested package.json must not crash hook (fail-open)",
                     "npm run typecheck", "no-crash", "RecursionError", None)
                )
        finally:
            os.chdir(original_cwd)
            hook._PACKAGE_JSON_CACHE.clear()

    return passed, failed, failures


def run_main_boundary_npm_run_cases():
    """Boundary test: run hook.main() against NPM_RUN_CASES and assert
    that package-script-resolved runners pass (not deny) at the main()
    boundary, with the right telemetry tag.

    NPM_RUN_CASES "block" rows now exercise the package-script path:
    detector identifies the inner runner, source = package_script, main()
    exits 0 with reason "package_script_runner_detected_but_cli_unsupported"
    and detection_source = "package_script". "allow" rows exercise the
    no-detection path: detector returns None, main() exits 0 with reason
    "no_runner_detected" and detection_source = None.

    Returns (passed, failed, failures).
    """
    passed = 0
    failed = 0
    failures = []
    original_cwd = os.getcwd()

    for label, desc, cmd, pkg in NPM_RUN_CASES:
        with tempfile.TemporaryDirectory() as case_root:
            _write_package_json_for_case(Path(case_root), pkg)
            os.chdir(case_root)
            with _soak_sandbox() as soak_dir:
                try:
                    hook._PACKAGE_JSON_CACHE.clear()
                    payload = {
                        "session_id": "s",
                        "tool_use_id": "t",
                        "cwd": case_root,
                        "tool_input": {"command": cmd},
                    }
                    rc = _run_hook_with_stdin(json.dumps(payload))
                    entries = _read_log(soak_dir)
                    problems = []
                    if rc != 0:
                        problems.append(f"rc: expected 0 (package-script runs are never denied), got {rc}")
                    # Only package-script detections survive the event filter;
                    # "allow" rows are no_runner_detected and are dropped.
                    expected_n = 1 if label == "block" else 0
                    if len(entries) != expected_n:
                        problems.append(f"entries: expected {expected_n}, got {len(entries)}")
                    if entries and label == "block":
                        entry = entries[0]
                        expected_reason = "package_script_runner_detected_but_cli_unsupported"
                        if entry.get("decision") != "pass":
                            problems.append(f"decision: expected pass, got {entry.get('decision')!r}")
                        if entry.get("reason") != expected_reason:
                            problems.append(f"reason: expected {expected_reason!r}, got {entry.get('reason')!r}")
                        if entry.get("detection_source") != "package_script":
                            problems.append(
                                "detection_source: expected 'package_script', "
                                f"got {entry.get('detection_source')!r}"
                            )
                        if entry.get("suggested_command") is not None:
                            problems.append(
                                f"suggested_command: expected None for pass, "
                                f"got {entry.get('suggested_command')!r}"
                            )
                        if entry.get("command") is not None:
                            problems.append(
                                f"command: expected null, got {entry.get('command')!r}"
                            )
                    if problems:
                        failed += 1
                        failures.append((desc, cmd, problems))
                    else:
                        passed += 1
                finally:
                    os.chdir(original_cwd)
                    hook._PACKAGE_JSON_CACHE.clear()

    return passed, failed, failures


def run_identity_cases():
    checks = 0
    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        binary_dir = root / "tool's directory with spaces"
        binary_dir.mkdir()
        binary = binary_dir / "failtrace"
        identity = "failtrace 0.1.0 (product=failtrace-rust revision=unknown state=unknown)"
        binary.write_text("#!/bin/sh\nprintf '%s\\n' " + shlex.quote(identity) + "\n")
        binary.chmod(0o755)
        impostor_dir = root / "impostor"
        impostor_dir.mkdir()
        impostor = impostor_dir / "failtrace"
        calls = root / "impostor-calls"
        impostor.write_text("#!/bin/sh\nprintf '%s\\n' \"$*\" >> " + shlex.quote(str(calls))
                            + "\nprintf 'unrelated npm product\\n'\n")
        impostor.chmod(0o755)
        env = dict(os.environ, PATH=str(impostor_dir), FAILTRACE_CC_SOAK="0")
        env.pop("FAILTRACE_BIN", None)

        def invoke(command, overrides=None):
            current = dict(env)
            current.update(overrides or {})
            return subprocess.run([sys.executable, str(_hook_path.resolve())], env=current,
                                  input=json.dumps({"tool_input": {"command": command}}),
                                  text=True, capture_output=True, timeout=5)

        proc = invoke("pytest")
        assert proc.returncode == 0 and "failtrace_identity_mismatch" in proc.stderr
        assert proc.stdout == "" and "Wrap with" not in proc.stderr
        assert calls.read_text().splitlines() == ["--version"]
        checks += 1
        calls.unlink()
        proc = invoke("pytest", {"FAILTRACE_BIN": str(binary)})
        assert proc.returncode == 2 and not calls.exists()
        assert f"Wrap with {shlex.quote(str(binary))} run -- ..." in proc.stderr
        for head in [str(binary), "/opt/bin/failtrace", "failtrace"]:
            command = shlex.join([head, "run", "--", "pytest"])
            assert hook.is_wrapped_by_failtrace(hook.tokenize(command))
            assert invoke(command, {"FAILTRACE_BIN": str(binary)}).returncode == 0
            checks += 1
        checks += 1
        proc = invoke("pytest", {"PATH": str(binary_dir)})
        assert proc.returncode == 2 and shlex.quote(str(binary)) in proc.stderr
        checks += 1
        for configured in [str(root / "missing"), "relative/failtrace", ""]:
            proc = invoke("pytest", {"FAILTRACE_BIN": configured})
            assert proc.returncode == 0 and "allowing original command unwrapped" in proc.stderr
            assert not calls.exists()
            checks += 1
        binary.write_text("#!/bin/sh\necho replaced\n")
        assert invoke("pytest", {"FAILTRACE_BIN": str(binary)}).returncode == 0
        checks += 1
        with patch.object(hook.subprocess, "run", side_effect=subprocess.TimeoutExpired("probe", 2)):
            assert hook.resolve_failtrace(dict(env, FAILTRACE_BIN=str(binary)))[1] == "failtrace_identity_timeout"
        checks += 1
        with patch.object(hook, "_main", side_effect=RuntimeError("synthetic error")):
            with contextlib.redirect_stderr(io.StringIO()) as output:
                assert hook.main() == 0
            assert "hook_error" in output.getvalue()
        checks += 1
        for raw in ["[]", "null", '{"tool_input": 42}']:
            proc = subprocess.run([sys.executable, str(_hook_path.resolve())], env=env,
                                  input=raw, text=True, capture_output=True)
            assert proc.returncode == 0 and "hook_error" in proc.stderr
            checks += 1
    return checks


def main():
    passed = 0
    failed = 0
    failures = []

    # Run the legacy CASES from a no-package-json directory so that
    # script-resolution stays inert and existing argv-based assertions
    # remain deterministic regardless of where the test is invoked from.
    original_cwd = os.getcwd()
    with tempfile.TemporaryDirectory() as td:
        os.chdir(td)
        try:
            hook._PACKAGE_JSON_CACHE.clear()
            for expected, desc, cmd in CASES:
                runner, _seg = hook.check_command(cmd)
                got = "block" if runner is not None else "allow"
                if got == expected:
                    passed += 1
                else:
                    failed += 1
                    failures.append((desc, cmd, expected, got, runner))
        finally:
            os.chdir(original_cwd)
            hook._PACKAGE_JSON_CACHE.clear()

    print(f"\nDetector cases: {passed} passed, {failed} failed (total {len(CASES)})")

    npm_run_passed, npm_run_failed, npm_run_failures = run_npm_run_script_cases()
    print(f"npm-run script cases: {npm_run_passed} passed, {npm_run_failed} failed "
          f"(total {npm_run_passed + npm_run_failed})")

    boundary_passed, boundary_failed, boundary_failures = run_main_boundary_npm_run_cases()
    print(f"npm-run boundary cases: {boundary_passed} passed, {boundary_failed} failed "
          f"(total {boundary_passed + boundary_failed})")

    soak_passed, soak_failed, soak_failures = run_soak_writer_cases()
    print(f"Soak writer cases: {soak_passed} passed, {soak_failed} failed "
          f"(total {soak_passed + soak_failed})")

    redact_passed, redact_failed, redact_failures = run_redaction_cases()
    print(f"Redaction cases: {redact_passed} passed, {redact_failed} failed "
          f"(total {redact_passed + redact_failed})")

    policy_passed, policy_failed, policy_failures = run_policy_cases()
    print(f"Enforcement policy cases: {policy_passed} passed, "
          f"{policy_failed} failed (total {policy_passed + policy_failed})")

    identity_passed = run_identity_cases()
    print(f"Identity cases: {identity_passed} passed")
    total_passed = (passed + npm_run_passed + boundary_passed + soak_passed
                    + redact_passed + policy_passed + identity_passed)
    total_failed = (failed + npm_run_failed + boundary_failed + soak_failed
                    + redact_failed + policy_failed)
    print(f"Grand total: {total_passed} passed, {total_failed} failed")

    if (failures or npm_run_failures or boundary_failures or soak_failures
            or redact_failures or policy_failures):
        print("\nFailures:")
        for desc, cmd, expected, got, runner in failures:
            extra = f" (runner={runner!r})" if runner else ""
            print(f"  FAIL (detector): {desc}")
            print(f"    cmd: {cmd!r}")
            print(f"    expected {expected}, got {got}{extra}")
        for desc, cmd, expected, got, runner in npm_run_failures:
            extra = f" (runner={runner!r})" if runner else ""
            print(f"  FAIL (npm-run): {desc}")
            print(f"    cmd: {cmd!r}")
            print(f"    expected {expected}, got {got}{extra}")
        for desc, cmd, problems in boundary_failures:
            print(f"  FAIL (npm-run boundary): {desc}")
            print(f"    cmd: {cmd!r}")
            for p in problems:
                print(f"    {p}")
        for desc, problems in soak_failures:
            print(f"  FAIL (soak): {desc}")
            for p in problems:
                print(f"    {p}")
        for desc, problems in redact_failures:
            print(f"  FAIL (redaction): {desc}")
            for p in problems:
                print(f"    {p}")
        for desc, cmd, problems in policy_failures:
            print(f"  FAIL (policy): {desc}")
            print(f"    cmd: {cmd!r}")
            for p in problems:
                print(f"    {p}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
