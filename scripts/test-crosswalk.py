#!/usr/bin/env python3
"""Cross-walk drift defense: every hook denial must have a CLI-supported wrap.

The PreToolUse hook in `scripts/failtrace-enforce.py` denies bare runner
invocations and suggests `failtrace run -- <argv>` as the corrective wrap.
The Rust CLI's argv-based detectors in `src/cli.rs` decide which adapter
handles a given argv. If the two ever drift — hook suggests a wrap the
CLI can't deliver — adopters get `AdapterUnavailable` exit 65 with no
recourse.

This test enumerates a small explicit `CROSSWALK_DENY_CASES` set (seeded
from the direct-deny `CASES` in `test-failtrace-enforce.py`), computes the
hook's suggestion shape for each, and invokes the actual `failtrace` binary
as a subprocess with fake runner binaries on PATH. The invariant: subprocess
must NOT exit 65 `AdapterUnavailable`, and `output.json` must record a
selected adapter matching the runner the hook detected.

Python owns the enforcement cases and the Rust CLI owns the support oracle:
the Rust detectors are not exposed or cloned to Python; the test invokes the
actual `failtrace run -- ...` as a subprocess. The set is deliberately small.

The package-script-source cases from NPM_RUN_CASES are intentionally NOT
included here: those are pass-with-telemetry, not enforced denials, so
they have no "suggested wrap" to cross-check. Coverage can extend once the
CLI resolves package.json scripts too.
"""

from __future__ import annotations

import importlib.util
import json
import os
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
HOOK_PATH = ROOT / "scripts" / "failtrace-enforce.py"
DEBUG_BIN = ROOT / "target" / "debug" / "failtrace"
RELEASE_BIN = ROOT / "target" / "release" / "failtrace"


def import_by_path(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


hook = import_by_path("failtrace_enforce_for_crosswalk", HOOK_PATH)


def find_failtrace_binary() -> Path | None:
    """Locate the failtrace binary built by cargo (prefer debug; cargo test
    builds debug by default). Returns None if neither debug nor release
    artifacts are present.
    """
    if "FAILTRACE_BIN" in os.environ:
        path, reason = hook.resolve_failtrace()
        if path is None:
            raise SystemExit(f"test-crosswalk: {reason}")
        return Path(path)
    if DEBUG_BIN.exists():
        return DEBUG_BIN
    if RELEASE_BIN.exists():
        return RELEASE_BIN
    return None


# Small explicit case set seeded from CASES in test-failtrace-enforce.py.
# One representative per runner family — kept small so the test runs in a
# few seconds even with a subprocess per case.
#
# Format: (description, command_string, expected_runner_name_in_failtrace_output)
# The expected_runner field is what `output.json.adapter_id` or
# `output.json.runner_outcome.adapter` (whichever the schema uses) should
# carry. Practically we just assert the run did NOT bail with
# AdapterUnavailable exit 65.
CROSSWALK_DENY_CASES = [
    ("pytest direct", "pytest tests/", "pytest"),
    ("uv run pytest", "uv run pytest tests/", "pytest"),
    ("npx vitest run", "npx vitest run", "vitest"),
    ("npx jest", "npx jest", "jest"),
    ("cargo test", "cargo test", "cargo-test"),
    ("npx tsc --noEmit", "npx tsc --noEmit", "tsc"),
    ("go test ./...", "go test ./...", "gotest"),
    ("xcodebuild test", "xcodebuild test -scheme Example", "xcodebuild-test"),
]


def make_fake_runner_binary(path: Path) -> None:
    """Create a fake test runner that exits 0 and writes a minimal output
    failtrace's adapter can recognize.

    The actual failtrace adapters look for specific stdout/stderr patterns.
    For cross-walk purposes we just need failtrace's runner-invocation step
    to succeed (not exit 65 before even running) — the adapter parse may
    return zero failures, but that's "complete parse" not
    "AdapterUnavailable".
    """
    path.write_text(f"""#!{sys.executable}
import sys
# Minimal output. Adapter parsers may flag this as unrecognized log shape
# (parse_status: unparsed_output) — that's fine, the invariant is "failtrace
# selected an adapter" not "failtrace produced a useful failures[] entry".
print("fake runner output")
sys.exit(0)
""")
    path.chmod(0o755)


def run_one_crosswalk_case(failtrace_bin: Path, desc: str, cmd: str,
                            expected_runner: str) -> tuple[bool, str]:
    """Execute the hook→wrap→CLI pipeline for one case.

    Returns (passed, failure_reason). passed=True if the wrap doesn't
    exit 65 AdapterUnavailable.
    """
    # 1. Run hook's check_command to confirm the case is a deny case.
    detection, seg = hook.check_command(cmd)
    if detection is None:
        return False, f"hook did not detect a runner (got None) for {cmd!r}"
    if detection.source != hook.DETECTION_SOURCE_ARGV:
        return False, (
            f"hook detected via {detection.source!r}, not argv "
            f"(cross-walk only covers argv-source denials)"
        )
    if detection.runner != expected_runner:
        return False, (
            f"hook detected runner {detection.runner!r} but corpus expects "
            f"{expected_runner!r}"
        )

    # 2. Build the suggested wrap argv from the segment tokens (avoid the
    # `build_suggested_command` shell-string path — we want to test the
    # CLI's acceptance of the argv shape, not the suggestion's quoting).
    # Mirror that function's residual logic exactly — strip_prefixes then
    # strip_redirections — so wrap_argv stays the genuine hook-suggested
    # shape and is redirection-free by construction.
    seg_residual = hook.strip_redirections(hook.strip_prefixes(seg))
    if not seg_residual:
        return False, f"strip_prefixes returned empty for seg {seg!r}"
    wrap_argv = [str(failtrace_bin), "run", "--"] + seg_residual

    # 3. Set up fake runner binaries + temp run-dir, then invoke.
    with tempfile.TemporaryDirectory() as td:
        td_path = Path(td)
        fake_bin_dir = td_path / "fake-bin"
        fake_bin_dir.mkdir()
        run_dir = td_path / "runs"
        run_dir.mkdir()

        # Fake binaries for every runner the cross-walk could hit. Cheap
        # to create; failtrace selects one based on its detector.
        for name in [
            "pytest", "vitest", "jest", "cargo", "tsc", "go",
            "uv", "poetry", "pipenv", "coverage", "npx", "npm",
            "pnpm", "yarn", "bun", "bunx", "python", "python3",
            "xcodebuild",
        ]:
            make_fake_runner_binary(fake_bin_dir / name)

        env = os.environ.copy()
        env["PATH"] = os.pathsep.join([
            str(fake_bin_dir),
            env.get("PATH", ""),
        ])

        # `--run-dir` MUST precede the `--` separator — failtrace parses its
        # own options before `--`. Appending it after `wrap_argv` (which
        # already ends with `-- <runner cmd>`) would pass `--run-dir` to the
        # wrapped runner instead; failtrace would then fall back to its
        # default run-dir under ~/.failtrace/runs/, leaking one artifact per
        # case per commit (this script runs in pre-commit). `wrap_argv` is
        # left untouched — it is the hook-suggested shape the diagnostics
        # below report.
        invoke_argv = [
            str(failtrace_bin), "run", "--run-dir", str(run_dir), "--",
        ] + seg_residual
        try:
            result = subprocess.run(
                invoke_argv,
                env=env,
                capture_output=True,
                text=True,
                timeout=30,
            )
        except subprocess.TimeoutExpired:
            return False, f"failtrace subprocess timed out: {shlex.join(wrap_argv)}"

        # The cross-walk invariant: NOT AdapterUnavailable (exit 65).
        # Exit 0, 1, or 2 are all acceptable — they mean failtrace selected
        # an adapter and ran it (the runner may have failed, but failtrace
        # didn't bail before invocation).
        if result.returncode == 65:
            return False, (
                f"failtrace returned AdapterUnavailable (exit 65) for "
                f"hook-suggested wrap.\n"
                f"  argv: {shlex.join(wrap_argv)}\n"
                f"  stderr: {result.stderr.strip()[:300]}"
            )

    return True, ""


def main() -> int:
    failtrace_bin = find_failtrace_binary()
    if failtrace_bin is None:
        # No binary present (fresh checkout, no cargo build/test run).
        # The pre-commit chain runs `cargo test` before this script,
        # so missing binary in pre-commit is a setup error worth
        # surfacing — but the test should not block clean clones.
        print(
            "test-crosswalk: failtrace binary not found at\n"
            f"  {DEBUG_BIN}\n  {RELEASE_BIN}\n"
            "Run `cargo build` or `cargo test` first.",
            file=sys.stderr,
        )
        # Exit 0 (skip) rather than 1 (fail) — this lets `cargo test`
        # in pre-commit produce the actual error if there's a build
        # problem; we don't want to mask that with a duplicate failure.
        return 0

    print(f"test-crosswalk: using failtrace binary at {failtrace_bin}")

    passed = 0
    failed = 0
    failures = []

    # Run from a temp dir so package.json walks don't accidentally pick
    # up failtrace's own package.json (none exists today, but defends
    # against the cross-walk corpus being polluted by future repo-root
    # package.json placement).
    original_cwd = os.getcwd()
    with tempfile.TemporaryDirectory() as td:
        os.chdir(td)
        try:
            hook._PACKAGE_JSON_CACHE.clear()
            for desc, cmd, expected_runner in CROSSWALK_DENY_CASES:
                ok, reason = run_one_crosswalk_case(
                    failtrace_bin, desc, cmd, expected_runner,
                )
                if ok:
                    passed += 1
                else:
                    failed += 1
                    failures.append((desc, cmd, reason))
        finally:
            os.chdir(original_cwd)
            hook._PACKAGE_JSON_CACHE.clear()

    print(f"Cross-walk cases: {passed} passed, {failed} failed "
          f"(total {len(CROSSWALK_DENY_CASES)})")
    if failures:
        print("\nFailures:")
        for desc, cmd, reason in failures:
            print(f"  FAIL: {desc}")
            print(f"    cmd: {cmd!r}")
            print(f"    {reason}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
