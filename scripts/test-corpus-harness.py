#!/usr/bin/env python3
"""Focused test battery for scripts/corpus/run_corpus.py helpers.

Run with:

    python3 scripts/test-corpus-harness.py
"""

from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import types
from datetime import datetime, timezone
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
RUNNER_PATH = ROOT / "scripts" / "corpus" / "run_corpus.py"
_RUNNER = None


def install_yaml_fallback() -> None:
    module = types.ModuleType("yaml")
    module.safe_load = lambda stream: json.loads(
        stream.read() if hasattr(stream, "read") else stream
    )
    sys.modules["yaml"] = module


def import_runner():
    global _RUNNER
    if _RUNNER is not None:
        return _RUNNER
    spec = importlib.util.spec_from_file_location("corpus_runner", RUNNER_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    sys.modules["corpus_runner"] = module
    try:
        spec.loader.exec_module(module)
    except ModuleNotFoundError as exc:
        if exc.name != "yaml":
            raise
        sys.modules.pop("corpus_runner", None)
        install_yaml_fallback()
        spec = importlib.util.spec_from_file_location("corpus_runner", RUNNER_PATH)
        module = importlib.util.module_from_spec(spec)
        assert spec.loader is not None
        sys.modules["corpus_runner"] = module
        spec.loader.exec_module(module)
    _RUNNER = module
    return module


class LazyRunner:
    def __getattr__(self, name):
        return getattr(import_runner(), name)

    def __setattr__(self, name, value):
        setattr(import_runner(), name, value)


runner = LazyRunner()


def make_executable(path: Path) -> None:
    path.write_text("#!/bin/sh\necho failtrace 0.0-test\n", encoding="utf-8")
    path.chmod(0o755)


def expect(condition: bool, message: str, failures: list[str]) -> None:
    if not condition:
        failures.append(message)


def expect_system_exit(fn, contains: str, failures: list[str]) -> None:
    try:
        fn()
    except SystemExit as exc:
        expect(contains in str(exc), f"SystemExit did not mention {contains!r}: {exc}", failures)
    else:
        failures.append("expected SystemExit")


def valid_manifest(scenarios=None) -> str:
    if scenarios is None:
        scenarios = [
            {
                "id": "pytest_sample_failure",
                "adapter": "pytest",
                "repo": {
                    "owner": "example",
                    "name": "repo",
                    "sha": "0000000000000000000000000000000000000000",
                },
                "requires": ["uv", "git"],
                "setup_commands": [["python3", "-c", "pass"]],
                "command": ["pytest"],
            }
        ]
    return json.dumps({"schema_version": 1, "scenarios": scenarios})


def test_env_binary_wins(failures: list[str]) -> int:
    checks = 0
    old_env = os.environ.copy()
    try:
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            env_bin = root / "candidate"
            path_dir = root / "path-bin"
            path_dir.mkdir()
            path_bin = path_dir / "failtrace"
            make_executable(env_bin)
            make_executable(path_bin)
            os.environ[runner.FAILTRACE_BIN_ENV] = str(env_bin)
            os.environ["PATH"] = str(path_dir)
            checks += 1
            expect(
                runner.resolve_failtrace_binary() == str(env_bin.resolve()),
                "FAILTRACE_BIN did not win over PATH",
                failures,
            )
    finally:
        os.environ.clear()
        os.environ.update(old_env)
    return checks


def test_env_binary_validation(failures: list[str]) -> int:
    checks = 0
    old_env = os.environ.copy()
    try:
        with tempfile.TemporaryDirectory() as td:
            missing = Path(td) / "missing"
            os.environ[runner.FAILTRACE_BIN_ENV] = str(missing)
            checks += 1
            expect_system_exit(runner.resolve_failtrace_binary, "binary not found", failures)

            non_exec = Path(td) / "not-executable"
            non_exec.write_text("#!/bin/sh\n", encoding="utf-8")
            os.environ[runner.FAILTRACE_BIN_ENV] = str(non_exec)
            checks += 1
            expect_system_exit(runner.resolve_failtrace_binary, "not executable", failures)
    finally:
        os.environ.clear()
        os.environ.update(old_env)
    return checks


def test_manifest_setup_commands(failures: list[str]) -> int:
    checks = 0

    def manifest_with(setup_commands):
        return json.dumps(
            {
                "schema_version": 1,
                "scenarios": [
                    {
                        "id": "pytest_sample_failure",
                        "adapter": "pytest",
                        "repo": {
                            "owner": "example",
                            "name": "repo",
                            "sha": "0000000000000000000000000000000000000000",
                        },
                        "setup_commands": setup_commands,
                        "command": ["pytest"],
                    }
                ],
            }
        )

    manifest = manifest_with([["python3", "-c", "pass"]])
    bad_manifest = manifest_with(["python3 -c pass"])
    with tempfile.TemporaryDirectory() as td:
        good = Path(td) / "good.yaml"
        bad = Path(td) / "bad.yaml"
        good.write_text(manifest, encoding="utf-8")
        bad.write_text(bad_manifest, encoding="utf-8")
        checks += 1
        expect(
            runner.load_manifest(good)[0]["setup_commands"][0][0] == "python3",
            "setup_commands did not load as list-of-argv",
            failures,
        )
        checks += 1
        expect_system_exit(lambda: runner.load_manifest(bad), "setup_commands[1]", failures)
    return checks


def test_manifest_validation_cases(failures: list[str]) -> int:
    checks = 0
    base = json.loads(valid_manifest())["scenarios"][0]

    def issues_for(scenarios):
        with tempfile.TemporaryDirectory() as td:
            manifest = Path(td) / "corpus.yaml"
            manifest.write_text(valid_manifest(scenarios), encoding="utf-8")
            return runner.validate_manifest(manifest)[1]

    checks += 1
    expect(
        issues_for([base]) == [],
        "valid manifest produced validation issues",
        failures,
    )

    duplicate = [dict(base), dict(base)]
    duplicate[1]["adapter"] = "cargo"
    checks += 1
    expect(
        any(
            "duplicate id 'pytest_sample_failure'" in issue
            for issue in issues_for(duplicate)
        ),
        "duplicate scenario id was not reported",
        failures,
    )

    missing = dict(base)
    missing.pop("command")
    checks += 1
    expect(
        any("missing 'command'" in issue for issue in issues_for([missing])),
        "missing required field was not reported",
        failures,
    )

    bad_adapter = dict(base, adapter="mocha")
    checks += 1
    expect(
        any(
            "adapter 'mocha' must be one" in issue
            for issue in issues_for([bad_adapter])
        ),
        "bad adapter was not reported",
        failures,
    )

    bad_command = dict(base, command="pytest")
    checks += 1
    expect(
        any(
            "command must be a non-empty list of strings" in issue
            for issue in issues_for([bad_command])
        ),
        "bad command shape was not reported",
        failures,
    )

    bad_setup = dict(base, setup_commands=["python3 -c pass"])
    checks += 1
    expect(
        any("setup_commands[1]" in issue for issue in issues_for([bad_setup])),
        "bad setup_commands shape was not reported",
        failures,
    )

    bad_requires = dict(base, requires="uv")
    checks += 1
    expect(
        any(
            "requires must be a list of strings" in issue
            for issue in issues_for([bad_requires])
        ),
        "bad requires shape was not reported",
        failures,
    )

    bad_repo = dict(base, repo={"owner": "example", "name": "repo"})
    checks += 1
    expect(
        any("repo missing 'sha'" in issue for issue in issues_for([bad_repo])),
        "missing repo.sha was not reported",
        failures,
    )
    return checks


def test_validate_manifest_cli_returns_before_execution(failures: list[str]) -> int:
    checks = 0
    original_resolve = runner.resolve_failtrace_binary
    try:
        runner.resolve_failtrace_binary = lambda: (_ for _ in ()).throw(
            AssertionError("resolve_failtrace_binary should not run")
        )
        with tempfile.TemporaryDirectory() as td:
            manifest = Path(td) / "corpus.yaml"
            manifest.write_text(valid_manifest(), encoding="utf-8")
            checks += 1
            expect(
                runner.main(["--manifest", str(manifest), "--validate-manifest"]) == 0,
                "--validate-manifest did not exit 0 for a valid manifest",
                failures,
            )

            broken = json.loads(valid_manifest())
            broken["scenarios"][0].pop("command")
            manifest.write_text(json.dumps(broken), encoding="utf-8")
            checks += 1
            expect(
                runner.main(["--manifest", str(manifest), "--validate-manifest"]) == 1,
                "--validate-manifest did not exit 1 for an invalid manifest",
                failures,
            )
    finally:
        runner.resolve_failtrace_binary = original_resolve
    return checks


def test_run_setup_commands(failures: list[str]) -> int:
    checks = 0
    with tempfile.TemporaryDirectory() as td:
        checkout = Path(td) / "checkout"
        out_path = Path(td) / "out"
        checkout.mkdir()
        out_path.mkdir()
        command = [
            sys.executable,
            "-c",
            "from pathlib import Path; Path('marker').write_text('ok')",
        ]
        checks += 1
        expect(
            runner.run_setup_commands([command], checkout, out_path, verbose=False) is None,
            "successful setup command returned an error",
            failures,
        )
        checks += 1
        expect((checkout / "marker").read_text(encoding="utf-8") == "ok", "setup did not run in checkout", failures)
        checks += 1
        expect((out_path / "setup-1.log").exists(), "setup log was not written", failures)

        failing = [sys.executable, "-c", "import sys; print('boom'); sys.exit(7)"]
        checks += 1
        detail = runner.run_setup_commands([failing], checkout, out_path, verbose=False)
        expect(
            detail is not None and "setup command 1 failed with exit 7" in detail,
            f"bad setup failure detail: {detail}",
            failures,
        )
        missing = ["failtrace-missing-setup-tool-for-test", "arg"]
        checks += 1
        detail = runner.run_setup_commands([missing], checkout, out_path, verbose=False)
        expect(
            detail is not None and "setup command 1 failed to start" in detail,
            f"bad missing setup-tool detail: {detail}",
            failures,
        )
        checks += 1
        missing_log = out_path / "setup-1.log"
        expect(missing_log.exists(), "missing setup-tool log was not written", failures)
        checks += 1
        expect(
            "exit=not-started" in missing_log.read_text(encoding="utf-8"),
            "missing setup-tool log did not record not-started state",
            failures,
        )
    return checks


def test_prune_legacy_out(failures: list[str]) -> int:
    checks = 0
    with tempfile.TemporaryDirectory() as td:
        out_dir = Path(td) / "out"
        legacy = out_dir / "pytest_old"
        historical = out_dir / "runs" / "20260517T120000Z-deadbeef"
        out_dir.mkdir()
        legacy.mkdir()
        historical.mkdir(parents=True)
        (out_dir / "latest").write_text("runs/20260517T120000Z-deadbeef\n", encoding="utf-8")
        (out_dir / "report.md").write_text("# report\n", encoding="utf-8")
        removed = runner.prune_legacy_out(out_dir)
        checks += 1
        expect(removed == [legacy], f"unexpected prune list: {removed}", failures)
        checks += 1
        expect(not legacy.exists(), "legacy flat dir survived prune", failures)
        checks += 1
        expect(historical.exists(), "historical run dir was pruned", failures)
        checks += 1
        expect((out_dir / "latest").exists(), "latest pointer was pruned", failures)
        checks += 1
        expect((out_dir / "report.md").exists(), "report copy was pruned", failures)
    return checks


def test_run_id_suffix(failures: list[str]) -> int:
    checks = 0
    checks += 1
    expect(
        runner.run_id_suffix({"short_sha": "b652de1c", "dirty": False}) == "b652de1c",
        "clean worktree suffix changed",
        failures,
    )
    checks += 1
    expect(
        runner.run_id_suffix({"short_sha": "b652de1c", "dirty": True}) == "b652de1c-dirty",
        "dirty worktree suffix did not include marker",
        failures,
    )
    checks += 1
    expect(
        runner.run_id_suffix({"short_sha": None, "dirty": True}) == "nogit",
        "nogit suffix changed",
        failures,
    )
    return checks


def test_repo_state_missing_git_falls_back(failures: list[str]) -> int:
    checks = 0
    original_run = runner.subprocess.run

    def missing_git(cmd, *args, **kwargs):
        if cmd and cmd[0] == "git":
            raise FileNotFoundError("git")
        return original_run(cmd, *args, **kwargs)

    try:
        runner.subprocess.run = missing_git
        state = runner.repo_state()
        checks += 1
        expect(state["sha"] is None, f"missing git sha was not None: {state}", failures)
        checks += 1
        expect(
            state["short_sha"] is None,
            f"missing git short_sha was not None: {state}",
            failures,
        )
        checks += 1
        expect(state["dirty"] is False, f"missing git dirty was not false: {state}", failures)
        checks += 1
        expect(
            runner.run_id_suffix(state) == "nogit",
            f"missing git did not use nogit suffix: {state}",
            failures,
        )
    finally:
        runner.subprocess.run = original_run
    return checks


def test_create_run_dir_dirty_suffix(failures: list[str]) -> int:
    checks = 0
    original_repo_state = runner.repo_state
    try:
        runner.repo_state = lambda: {"short_sha": "b652de1c", "dirty": True}
        with tempfile.TemporaryDirectory() as td:
            run_id, run_dir = runner.create_run_dir(
                Path(td),
                datetime(2026, 5, 17, 15, 0, 0, tzinfo=timezone.utc),
            )
            checks += 1
            expect(
                run_id == "20260517T150000Z-b652de1c-dirty",
                f"dirty run id missing marker: {run_id}",
                failures,
            )
            checks += 1
            expect(run_dir.exists(), "dirty run dir was not created", failures)
    finally:
        runner.repo_state = original_repo_state
    return checks


def test_scenario_record(failures: list[str]) -> int:
    checks = 0
    result = runner.ScenarioResult(
        scenario_id="pytest_sample_failure",
        adapter="pytest",
        status="ok",
        duration_ms=12,
        captured={
            "runner_outcome": "failed",
            "parse_status": "complete",
            "adapters": [{"id": "pytest", "selected": True}],
            "failures": [{"message": "nope"}],
        },
    )
    record = runner.scenario_record(result)
    checks += 1
    expect(record["selected_adapter"] == "pytest", "selected adapter missing", failures)
    checks += 1
    expect(record["failure_count"] == 1, "failure count missing", failures)
    return checks


def test_promoted_fixture_license_notices(failures: list[str]) -> int:
    checks = 0
    fixture_root = ROOT / "tests" / "fixtures"
    attributions = sorted(fixture_root.glob("**/attribution.md"))
    checks += 1
    expect(bool(attributions), "no promoted fixture attributions found", failures)

    for attribution in attributions:
        source_dir = attribution.parent / "source"
        if not source_dir.is_dir():
            continue
        notices = [
            path
            for pattern in ("LICENSE*", "COPYING*", "NOTICE*")
            for path in attribution.parent.glob(pattern)
            if path.is_file() and path.read_text(encoding="utf-8").strip()
        ]
        checks += 1
        expect(
            bool(notices),
            f"promoted fixture is missing an upstream license notice: {attribution.parent}",
            failures,
        )
    return checks


def test_public_gauntlet_upload_is_metadata_only(failures: list[str]) -> int:
    workflow = (ROOT / ".github" / "workflows" / "gauntlet.yml").read_text(
        encoding="utf-8"
    )
    upload_block = workflow.split("uses: actions/upload-artifact@", maxsplit=1)[1]
    checks = 0

    for forbidden in ("/runs/**", "/report.md"):
        checks += 1
        expect(
            forbidden not in upload_block,
            f"public gauntlet upload includes unreviewed corpus evidence: {forbidden}",
            failures,
        )
    for required in ("/summary.json", "/manifest.snapshot.yaml", "/toolchain.json"):
        checks += 1
        expect(
            required in upload_block,
            f"public gauntlet upload is missing reviewed metadata: {required}",
            failures,
        )
    checks += 1
    expect(
        "include-hidden-files: true" in upload_block,
        "hidden .failtrace-corpus metadata would not be uploaded",
        failures,
    )
    return checks


def main() -> int:
    failures: list[str] = []
    checks = 0
    for test in [
        test_env_binary_wins,
        test_env_binary_validation,
        test_manifest_setup_commands,
        test_manifest_validation_cases,
        test_validate_manifest_cli_returns_before_execution,
        test_run_setup_commands,
        test_prune_legacy_out,
        test_run_id_suffix,
        test_repo_state_missing_git_falls_back,
        test_create_run_dir_dirty_suffix,
        test_scenario_record,
        test_promoted_fixture_license_notices,
        test_public_gauntlet_upload_is_metadata_only,
    ]:
        checks += test(failures)

    if failures:
        print(f"{len(failures)} failure(s) across {checks} checks:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print(f"ok - {checks} corpus harness checks")
    return 0


if __name__ == "__main__":
    sys.exit(main())
