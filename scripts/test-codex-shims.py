#!/usr/bin/env python3
"""Test battery for Codex PATH shims.

Run with:

    python3 scripts/test-codex-shims.py
"""

from __future__ import annotations

import importlib.util
import json
import os
import shlex
import stat
import subprocess
import sys
import tempfile
import shutil
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DISPATCHER_PATH = ROOT / "scripts" / "codex-shim-dispatcher.py"
DETECTOR_PATH = ROOT / "scripts" / "failtrace-enforce.py"
HOOK_TEST_PATH = ROOT / "scripts" / "test-failtrace-enforce.py"
INSTALLER_PATH = ROOT / "scripts" / "install-codex-shims.sh"
PYTHON_BIN_DIR = str(Path(sys.executable).resolve().parent)


def import_by_path(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


dispatcher = import_by_path("codex_shim_dispatcher", DISPATCHER_PATH)
hook_tests = import_by_path("failtrace_enforce_tests", HOOK_TEST_PATH)
detector = import_by_path("failtrace_enforce_for_shims", DETECTOR_PATH)


def make_fake_binary(path: Path, exit_code: int = 0) -> None:
    # Use the running interpreter for the shebang so fake binaries work on
    # hosts where Python is at a non-standard path (pyenv, nix, etc.) —
    # not just systems where /usr/bin/python3 exists.
    body = f"#!{sys.executable}\n" + (
        """import json
import os
import sys
if os.path.basename(sys.argv[0]) == "failtrace" and sys.argv[1:] == ["--version"]:
    print("failtrace 0.1.0 (product=failtrace-rust revision=unknown state=unknown)")
    sys.exit(0)
record = {
    "name": os.path.basename(sys.argv[0]),
    "argv": sys.argv,
    "guard": os.environ.get("FAILTRACE_CODEX_SHIM_ACTIVE"),
}
with open(os.environ["FAKE_LOG"], "a", encoding="utf-8") as handle:
    handle.write(json.dumps(record) + "\\n")
sys.exit(int(os.environ.get("FAKE_EXIT", "%d")))
"""
        % exit_code
    )
    path.write_text(body)
    path.chmod(0o755)


def read_fake_log(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def read_jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def make_env(shim_dir: Path, fake_bin: Path, home: Path, log_path: Path) -> dict[str, str]:
    env = os.environ.copy()
    env["PATH"] = os.pathsep.join([str(shim_dir), str(fake_bin), env.get("PATH", "")])
    env["HOME"] = str(home)
    env["FAKE_LOG"] = str(log_path)
    env.pop(dispatcher.GUARD_ENV, None)
    env.pop("FAILTRACE_BIN", None)
    return env


def command_to_shim_argv(command: str):
    """Return (runner_name | None, argv) for a shell command.

    `runner_name` is a string runner identifier (or None when no detection
    happens). `argv` is the argv tokens for the first segment after prefix
    stripping. The caller compares `runner_name` to `decision.runner` from
    the shim dispatcher, which is also a string.
    """
    try:
        detection, segment = detector.check_command(command)
    except ValueError:
        return None, None
    if detection is None:
        try:
            tokens = detector.tokenize(command)
        except ValueError:
            return None, None
        segments = detector.split_segments(tokens)
        if not segments:
            return None, None
        residual = detector.strip_prefixes(segments[0])
        return None, residual
    return detection.runner, detector.strip_prefixes(segment)


def is_covered_shim_name(name: str) -> bool:
    if name in {
        "cargo", "rustup", "pytest", "python", "python3", "uv", "poetry",
        "pipenv", "coverage", "vitest", "jest", "tsc", "npx", "npm",
        "pnpm", "yarn", "bun", "bunx", "go", "gotip", "failtrace",
        "xcodebuild",
    }:
        return True
    return name.startswith("go1.") and detector.is_go_binary_basename(name)


def run_parity_cases() -> tuple[int, list[str]]:
    failures: list[str] = []
    checked = 0
    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        shim_dir = root / "shims"
        fake_bin = root / "bin"
        shim_dir.mkdir()
        fake_bin.mkdir()
        for name in [
            "cargo", "rustup", "pytest", "python", "python3", "uv", "poetry",
            "pipenv", "coverage", "vitest", "jest", "tsc", "npx", "npm",
            "pnpm", "yarn", "bun", "bunx", "go", "gotip", "go1.22",
            "go1.21", "go1.21.5", "go1.22rc1", "go1.22beta1",
            "failtrace", "xcodebuild",
        ]:
            make_fake_binary(fake_bin / name)
        env = {"PATH": os.pathsep.join([str(shim_dir), str(fake_bin), os.environ.get("PATH", "")])}

        for expected, desc, command in hook_tests.CASES:
            runner, argv = command_to_shim_argv(command)
            if not argv:
                continue
            head = os.path.basename(argv[0])
            if not is_covered_shim_name(head):
                continue
            checked += 1
            decision = dispatcher.decide(argv, env, shim_dir, detector=detector)
            if expected == "block":
                if decision.action != "wrap" or decision.runner != runner:
                    failures.append(
                        f"{desc}: expected wrap {runner}, got {decision.action} {decision.runner}"
                    )
            else:
                if decision.action == "wrap":
                    failures.append(f"{desc}: expected pass, got wrap {decision.runner}")
    return checked, failures


def run_installer(prefix: Path, env: dict[str, str], *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(INSTALLER_PATH), *args, "--prefix", str(prefix)],
        cwd=ROOT,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def run_integration() -> tuple[int, list[str]]:
    failures: list[str] = []
    checks = 0
    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        home = root / "home"
        prefix = root / "shims"
        fake_bin = root / "bin"
        fake_log = root / "fake.jsonl"
        home.mkdir()
        fake_bin.mkdir()
        for name in ["cargo", "failtrace", "go1.22rc1"]:
            make_fake_binary(fake_bin / name)
        env = os.environ.copy()
        env["PATH"] = os.pathsep.join([str(fake_bin), PYTHON_BIN_DIR, env.get("PATH", "")])
        env["HOME"] = str(home)
        env["FAKE_LOG"] = str(fake_log)
        env.pop("FAILTRACE_BIN", None)

        first = run_installer(prefix, env, "--apply")
        second = run_installer(prefix, env, "--apply")
        checks += 2
        if first.returncode != 0:
            failures.append(f"installer first apply failed: {first.stderr or first.stdout}")
        if second.returncode != 0:
            failures.append(f"installer second apply failed: {second.stderr or second.stdout}")
        if not (prefix / "cargo").is_symlink():
            failures.append("installer did not create cargo symlink")
        if not (prefix / "go1.22rc1").is_symlink():
            failures.append("installer did not discover go1.x binary")
        if "[shell_environment_policy]" not in first.stdout or "[shell_environment_policy.set]" not in first.stdout:
            failures.append("installer did not print Codex config stanza on apply")

        run_env = make_env(prefix, fake_bin, home, fake_log)

        fake_log.unlink(missing_ok=True)
        proc = subprocess.run(
            ["cargo", "test", "--workspace"],
            env=run_env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        checks += 1
        records = read_fake_log(fake_log)
        if proc.returncode != 0 or len(records) != 1:
            failures.append(f"cargo test wrap smoke failed: rc={proc.returncode} records={records}")
        elif records[0]["name"] != "failtrace" or records[0]["argv"][1:4] != ["run", "--", "cargo"]:
            failures.append(f"cargo test did not exec failtrace run: {records[0]}")
        elif records[0]["guard"] != "1":
            failures.append("wrapped failtrace did not receive recursion guard")

        fake_log.unlink(missing_ok=True)
        proc = subprocess.run(
            ["cargo", "build"],
            env=run_env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        checks += 1
        records = read_fake_log(fake_log)
        if proc.returncode != 0 or len(records) != 1:
            failures.append(f"cargo build pass-through smoke failed: rc={proc.returncode} records={records}")
        elif records[0]["name"] != "cargo" or records[0]["argv"][1:] != ["build"]:
            failures.append(f"cargo build did not pass through to cargo: {records[0]}")
        elif records[0]["guard"] != "1":
            failures.append("pass-through cargo did not receive recursion guard")

        fake_log.unlink(missing_ok=True)
        guard_env = dict(run_env)
        guard_env[dispatcher.GUARD_ENV] = "1"
        proc = subprocess.run(
            ["cargo", "test"],
            env=guard_env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        checks += 1
        records = read_fake_log(fake_log)
        if proc.returncode != 0 or len(records) != 1:
            failures.append(f"recursion guard smoke failed: rc={proc.returncode} records={records}")
        elif records[0]["name"] != "cargo" or records[0]["argv"][1:] != ["test"]:
            failures.append(f"recursion guard did not pass through: {records[0]}")

        fake_log.unlink(missing_ok=True)
        proc = subprocess.run(
            ["failtrace", "run", "--", "cargo", "test"],
            env=run_env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        checks += 1
        records = read_fake_log(fake_log)
        if proc.returncode != 0 or len(records) != 1:
            failures.append(f"self-shim smoke failed: rc={proc.returncode} records={records}")
        elif records[0]["name"] != "failtrace" or records[0]["guard"] != "1":
            failures.append(f"failtrace self-shim did not pass through with guard: {records[0]}")

        soak_dir = home / ".failtrace" / "agent-soak"
        soak_dir.mkdir(parents=True, exist_ok=True)
        fake_log.unlink(missing_ok=True)
        proc = subprocess.run(
            ["cargo", "test"],
            env=run_env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        checks += 1
        log_path = soak_dir / "codex-shims.jsonl"
        if not log_path.exists():
            failures.append("shim soak log was not written")
        else:
            entries = read_jsonl(log_path)
            mode = stat.S_IMODE(log_path.stat().st_mode)
            if mode != 0o600:
                failures.append(f"shim soak log mode: expected 0o600, got {oct(mode)}")
            if not entries or entries[-1].get("decision") != "wrap":
                failures.append(f"shim soak log did not record wrap decision: {entries}")
            elif entries[-1].get("argv0") != "cargo":
                failures.append(f"shim soak log argv0 not populated from symlink basename: {entries[-1]}")

        uninstall = run_installer(prefix, env, "--uninstall")
        checks += 1
        if uninstall.returncode != 0:
            failures.append(f"installer uninstall failed: {uninstall.stderr or uninstall.stdout}")
        if (prefix / "cargo").exists() or (prefix / "failtrace-codex-shim-dispatcher.py").exists():
            failures.append("uninstall left marker-owned files behind")
    return checks, failures


def run_logging_regressions() -> tuple[int, list[str]]:
    failures: list[str] = []
    checks = 0
    old_home = os.environ.get("HOME")
    old_cwd = Path.cwd()

    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        project = root / "project"
        project.mkdir()
        blocked_home = root / "home-is-a-file"
        blocked_home.write_text("not a directory")

        try:
            os.environ["HOME"] = str(blocked_home)
            os.chdir(project)
            decision = dispatcher.Decision(
                "wrap",
                "runner_detected",
                ["cargo", "test"],
                runner="cargo-test",
                real_path="/usr/bin/cargo",
                failtrace_path="/usr/bin/failtrace",
            )
            dispatcher.log_decision(decision)

            checks += 1
            fallback_log = project / ".failtrace" / "agent-soak" / "codex-shims.jsonl"
            entries = read_jsonl(fallback_log)
            if not entries:
                failures.append("shim soak fallback did not write under cwd when HOME was unwritable")
            elif entries[-1].get("argv0") != "cargo":
                failures.append(f"shim soak fallback argv0 mismatch: {entries[-1]}")

            checks += 1
            if (blocked_home / ".failtrace").exists():
                failures.append("shim soak fallback unexpectedly created a child under non-directory HOME")

            guard_project = root / "guard-project"
            guard_project.mkdir()
            os.chdir(guard_project)
            os.environ["HOME"] = str(blocked_home)
            guard_decision = dispatcher.Decision(
                "pass",
                "recursion_guard_active",
                ["cargo", "test"],
                real_path="/usr/bin/cargo",
            )
            dispatcher.log_decision(guard_decision)

            checks += 1
            guard_log = guard_project / ".failtrace" / "agent-soak" / "codex-shims.jsonl"
            guard_entries = read_jsonl(guard_log)
            if not guard_entries:
                failures.append("shim soak fallback did not write recursion guard decision")
            elif guard_entries[-1].get("reason") != "recursion_guard_active":
                failures.append(f"recursion guard reason not distinguishable: {guard_entries[-1]}")
        finally:
            os.chdir(old_cwd)
            if old_home is None:
                os.environ.pop("HOME", None)
            else:
                os.environ["HOME"] = old_home
    return checks, failures


def run_marker_union_regression() -> tuple[int, list[str]]:
    failures: list[str] = []
    checks = 0
    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        home = root / "home"
        prefix = root / "shims"
        fake_bin = root / "bin-with-go"
        home.mkdir()
        fake_bin.mkdir()
        make_fake_binary(fake_bin / "go1.22rc1")
        make_fake_binary(fake_bin / "failtrace")

        env_with_go = os.environ.copy()
        env_with_go["PATH"] = os.pathsep.join([str(fake_bin), PYTHON_BIN_DIR, env_with_go.get("PATH", "")])
        env_with_go["HOME"] = str(home)
        env_with_go["FAILTRACE_BIN"] = str(fake_bin / "failtrace")

        env_without_go = os.environ.copy()
        env_without_go["PATH"] = os.pathsep.join([PYTHON_BIN_DIR, env_without_go.get("PATH", "")])
        env_without_go["HOME"] = str(home)
        env_without_go["FAILTRACE_BIN"] = str(fake_bin / "failtrace")

        first = run_installer(prefix, env_with_go, "--apply")
        checks += 1
        if first.returncode != 0:
            failures.append(f"marker-union first apply failed: {first.stderr or first.stdout}")
        if not (prefix / "go1.22rc1").is_symlink():
            failures.append("marker-union first apply did not install go1.22rc1 symlink")

        second = run_installer(prefix, env_without_go, "--apply")
        checks += 1
        if second.returncode != 0:
            failures.append(f"marker-union second apply failed: {second.stderr or second.stdout}")
        if not (prefix / "go1.22rc1").is_symlink():
            failures.append("marker-union second apply dropped go1.22rc1 symlink before uninstall")

        uninstall = run_installer(prefix, env_without_go, "--uninstall")
        checks += 1
        if uninstall.returncode != 0:
            failures.append(f"marker-union uninstall failed: {uninstall.stderr or uninstall.stdout}")
        if prefix.exists() and any(prefix.iterdir()):
            leftovers = sorted(path.name for path in prefix.iterdir())
            failures.append(f"marker-union uninstall left files behind: {leftovers}")
    return checks, failures


def run_path_e_package_script_cases() -> tuple[int, list[str]]:
    """Shim dispatcher must downgrade package-script detections from
    `wrap` to `pass` with the right telemetry tag, so Codex's
    `npm run typecheck` doesn't get silently exec'd as
    `failtrace run -- npm run typecheck` (which lands in AdapterUnavailable).
    """
    failures: list[str] = []
    checks = 0

    # (description, package.json, argv passed to shim, expected runner)
    package_script_cases = [
        ("npm run typecheck -> tsc",
         {"scripts": {"typecheck": "tsc -p tsconfig.app.json --noEmit"}},
         ["npm", "run", "typecheck"], "tsc"),
        ("pnpm run test -> pytest",
         {"scripts": {"test": "pytest tests/ -q"}},
         ["pnpm", "run", "test"], "pytest"),
        ("yarn run unit -> vitest",
         {"scripts": {"unit": "vitest run"}},
         ["yarn", "run", "unit"], "vitest"),
        ("bun run e2e -> jest",
         {"scripts": {"e2e": "jest --ci --runInBand"}},
         ["bun", "run", "e2e"], "jest"),
    ]

    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        shim_dir = root / "shims"
        fake_bin = root / "bin"
        shim_dir.mkdir()
        fake_bin.mkdir()
        for name in ["npm", "pnpm", "yarn", "bun", "failtrace"]:
            make_fake_binary(fake_bin / name)
        env = {"PATH": os.pathsep.join([str(shim_dir), str(fake_bin), os.environ.get("PATH", "")])}

        original_cwd = os.getcwd()
        for desc, pkg_json, argv, expected_runner in package_script_cases:
            with tempfile.TemporaryDirectory() as case_root:
                case_path = Path(case_root)
                (case_path / "package.json").write_text(json.dumps(pkg_json))
                os.chdir(case_path)
                try:
                    detector._PACKAGE_JSON_CACHE.clear()
                    decision = dispatcher.decide(argv, env, shim_dir, detector=detector)
                    checks += 1
                    if decision.action != "pass":
                        failures.append(
                            f"{desc}: expected action=pass, got action={decision.action!r} "
                            f"reason={decision.reason!r}"
                        )
                    if decision.reason != "package_script_runner_detected_but_cli_unsupported":
                        failures.append(
                            f"{desc}: expected reason=package_script_runner_detected_but_cli_unsupported, "
                            f"got reason={decision.reason!r}"
                        )
                    if decision.runner != expected_runner:
                        failures.append(
                            f"{desc}: expected runner={expected_runner!r}, "
                            f"got runner={decision.runner!r}"
                        )
                    if decision.detection_source != "package_script":
                        failures.append(
                            f"{desc}: expected detection_source=package_script, "
                            f"got {decision.detection_source!r}"
                        )
                finally:
                    os.chdir(original_cwd)
                    detector._PACKAGE_JSON_CACHE.clear()

    return checks, failures


def run_identity_integration() -> int:
    checks = 0
    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        prefix = root / "shims"
        home = root / "home"
        home.mkdir()
        good_dir = root / "tool's directory with spaces"
        good_dir.mkdir()
        good = good_dir / "failtrace"
        make_fake_binary(good)
        fake_bin = root / "bin"
        fake_bin.mkdir()
        make_fake_binary(fake_bin / "cargo")
        impostor = fake_bin / "failtrace"
        calls = root / "impostor-calls"
        impostor.write_text("#!/bin/sh\nprintf '%s\\n' \"$*\" >> " + shlex.quote(str(calls))
                            + "\nprintf 'unrelated npm product\\n'\n")
        impostor.chmod(0o755)
        log = root / "fake.jsonl"
        env = make_env(prefix, fake_bin, home, log)
        env["PATH"] = os.pathsep.join([str(fake_bin), str(good_dir), PYTHON_BIN_DIR, "/usr/bin", "/bin"])
        rejected = run_installer(prefix, env, "--apply")
        assert rejected.returncode == 78 and not prefix.exists(), rejected.stderr
        assert calls.read_text().splitlines() == ["--version"]
        calls.unlink()
        checks += 1
        env["FAILTRACE_BIN"] = str(good)
        installed = run_installer(prefix, env, "--apply")
        assert installed.returncode == 0, installed.stderr
        assert f"binary={good}\n" in (prefix / dispatcher.MARKER_FILENAME).read_text()
        assert not calls.exists()
        env.pop("FAILTRACE_BIN")
        env["PATH"] = str(prefix) + os.pathsep + env["PATH"]
        reapplied = run_installer(prefix, env, "--apply")
        assert reapplied.returncode == 0 and not calls.exists(), reapplied.stderr
        checks += 1

        def invoke(argv, overrides=None):
            current = dict(env)
            current.update(overrides or {})
            log.unlink(missing_ok=True)
            proc = subprocess.run(argv, env=current, capture_output=True, text=True, timeout=10)
            return proc, read_fake_log(log)

        proc, rows = invoke(["cargo", "test"])
        assert proc.returncode == 0 and len(rows) == 1 and rows[0]["name"] == "failtrace", (proc, rows)
        assert not calls.exists()
        checks += 1
        proc, rows = invoke(["failtrace", "analyze", "example-run"])
        assert proc.returncode == 0 and len(rows) == 1 and rows[0]["name"] == "failtrace"
        assert not calls.exists()
        checks += 1
        good.write_text("#!/bin/sh\necho replaced\n")
        for state in ["replaced", "missing"]:
            if state == "missing":
                good.unlink()
            proc, rows = invoke(["cargo", "test"])
            assert proc.returncode == 0 and len(rows) == 1 and rows[0]["name"] == "cargo"
            assert "running original runner unwrapped" in proc.stderr and len(proc.stderr.splitlines()) == 1
            proc, rows = invoke(["failtrace", "analyze", "example-run"])
            assert proc.returncode == 127 and not rows and "failtrace" in proc.stderr
            assert not calls.exists()
            checks += 2
        make_fake_binary(good)
        for configured in ["relative/failtrace", str(prefix / "failtrace")]:
            proc, rows = invoke(["cargo", "test"], {"FAILTRACE_BIN": configured})
            assert proc.returncode == 0 and rows[0]["name"] == "cargo"
            proc, rows = invoke(["failtrace", "--version"], {"FAILTRACE_BIN": configured,
                                                                   dispatcher.GUARD_ENV: "1"})
            assert proc.returncode == 127 and not rows
            checks += 2
        marker = prefix / dispatcher.MARKER_FILENAME
        marker.write_text("\n".join(line for line in marker.read_text().splitlines()
                                     if not line.startswith("binary=")) + "\n")
        proc, rows = invoke(["cargo", "test"])
        assert proc.returncode == 0 and rows[0]["name"] == "cargo" and "identity_mismatch" in proc.stderr
        assert calls.read_text().splitlines() == ["--version"]
        checks += 1
        fallback_path = os.pathsep.join([str(prefix), str(good_dir), str(fake_bin), PYTHON_BIN_DIR, "/usr/bin", "/bin"])
        proc, rows = invoke(["cargo", "test"], {"PATH": fallback_path})
        assert proc.returncode == 0 and rows[0]["name"] == "failtrace"
        checks += 1

        candidate = Path(os.environ.get("FAILTRACE_BIN", str(ROOT / "target/debug/failtrace")))
        if candidate.is_file():
            shutil.copy2(candidate, good)
            runner = fake_bin / "cargo"
            runner.write_text("#!/bin/sh\nprintf 'running 1 test\\ntest smoke ... ok\\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\\n'\n")
            proc, _ = invoke(["cargo", "test"], {"FAILTRACE_BIN": str(good),
                                                  "FAILTRACE_RUNS_DIR": str(root / "runs")})
            assert proc.returncode == 0, proc.stderr
            document = json.loads(proc.stdout)
            stored = Path(document["emission"]["full_output_path"])
            assert json.loads(stored.read_text())["runner_outcome"] == "passed"
            assert "product=failtrace-rust" in json.loads((stored.parent / "meta.json").read_text())["capture_build"]
            assert proc.stderr.startswith("failtrace: run-dir=")
            checks += 1
    return checks


def main() -> int:
    checked, parity_failures = run_parity_cases()
    integration_checks, integration_failures = run_integration()
    logging_checks, logging_failures = run_logging_regressions()
    marker_checks, marker_failures = run_marker_union_regression()
    path_e_checks, path_e_failures = run_path_e_package_script_cases()
    identity_checks = run_identity_integration()
    failures = (parity_failures + integration_failures + logging_failures
                + marker_failures + path_e_failures)
    total = (checked + integration_checks + logging_checks
             + marker_checks + path_e_checks + identity_checks)

    if failures:
        print(f"FAIL: {len(failures)} failures across {total} checks")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print(
        f"OK: {total} Codex shim checks passed "
        f"({checked} detector-parity, {integration_checks} integration, "
        f"{logging_checks} logging, {marker_checks} marker-union, "
        f"{path_e_checks} package-script, {identity_checks} identity)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
