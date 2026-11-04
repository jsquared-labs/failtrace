#!/usr/bin/env python3
"""PATH shim dispatcher for Codex failtrace coverage.

Installed as one real file with runner-name symlinks pointing at it. The
detector is a vendored copy of scripts/failtrace-enforce.py copied into the
same directory by scripts/install-codex-shims.sh.
"""

from __future__ import annotations

import datetime
import importlib.util
import json
import os
import shlex
import stat
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


DETECTOR_FILENAME = "failtrace-detector.py"
GUARD_ENV = "FAILTRACE_CODEX_SHIM_ACTIVE"
SOAK_LOG_FILENAME = "codex-shims.jsonl"
MARKER_FILENAME = ".failtrace-codex-shims.marker"


@dataclass(frozen=True)
class Decision:
    action: str
    reason: str
    argv: list[str]
    runner: Optional[str] = None
    real_path: Optional[str] = None
    failtrace_path: Optional[str] = None
    # Tags whether a detection came from direct argv ("argv")
    # or package.json script resolution ("package_script"). Package-script
    # hits are downgraded to `pass` because the Rust CLI's argv-based
    # detectors reject `npm run X` shapes — wrapping would produce
    # AdapterUnavailable exit 65. None for non-detection decisions
    # (pass/error/empty).
    detection_source: Optional[str] = None


def shim_dir() -> Path:
    return Path(__file__).resolve().parent


def load_detector(detector_path: Optional[Path] = None):
    path = detector_path or shim_dir() / DETECTOR_FILENAME
    spec = importlib.util.spec_from_file_location("failtrace_codex_shim_detector", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load detector from {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def command_name(argv0: str) -> str:
    return os.path.basename(argv0)


def command_argv(argv: list[str]) -> list[str]:
    if not argv:
        return []
    return [command_name(argv[0]), *argv[1:]]


def same_dir(left: Path, right: Path) -> bool:
    try:
        return left.resolve() == right.resolve()
    except OSError:
        return os.path.abspath(left) == os.path.abspath(right)


def is_executable_file(path: Path) -> bool:
    return path.is_file() and os.access(path, os.X_OK)


def resolve_real_binary(name: str, own_shim_dir: Path, path_value: str) -> Optional[str]:
    for raw_dir in path_value.split(os.pathsep):
        if not raw_dir:
            continue
        candidate_dir = Path(raw_dir)
        if same_dir(candidate_dir, own_shim_dir):
            continue
        candidate = candidate_dir / name
        if is_executable_file(candidate):
            return os.path.abspath(candidate)
    return None


def decide(
    argv: list[str],
    env: dict[str, str],
    own_shim_dir: Path,
    detector=None,
) -> Decision:
    normalized = command_argv(argv)
    if not normalized:
        return Decision("error", "empty_argv", normalized)

    name = normalized[0]
    detector = detector or load_detector()
    if name == "failtrace":
        if env.get(detector.IDENTITY_PROBE_ENV):
            return Decision("error", "failtrace_binary_is_shim", normalized)
        failtrace_path, reason = detector.resolve_failtrace(
            env, detector.configured_failtrace(own_shim_dir, env), own_shim_dir,
        )
        if failtrace_path is None:
            return Decision("error", reason, normalized)
        return Decision("pass", "failtrace_self_shim", normalized, real_path=failtrace_path)

    real_path = resolve_real_binary(name, own_shim_dir, env.get("PATH", ""))
    if real_path is None:
        return Decision("error", "real_binary_not_found", normalized)

    if env.get(GUARD_ENV):
        return Decision("pass", "recursion_guard_active", normalized, real_path=real_path)

    detection = detector.detect(normalized)
    if detection is None:
        return Decision("pass", "no_runner_detected", normalized, real_path=real_path)

    # Package-script-resolved runners pass through unwrapped.
    # Wrapping would produce `failtrace run -- npm run X`, which the
    # Rust CLI rejects with AdapterUnavailable (see src/cli.rs:973 and
    # the npm_test_with_single_recognized_runner_in_scripts_test_stays_ambiguous
    # test at src/cli.rs:2610). Silent shim wrap would land in exit 65
    # with no agent recourse. Telemetry still records the detection.
    if detection.source == detector.DETECTION_SOURCE_PACKAGE_SCRIPT:
        return Decision(
            "pass",
            "package_script_runner_detected_but_cli_unsupported",
            normalized,
            runner=detection.runner,
            real_path=real_path,
            detection_source=detection.source,
        )

    failtrace_path, reason = detector.resolve_failtrace(
        env, detector.configured_failtrace(own_shim_dir, env), own_shim_dir,
    )
    if failtrace_path is None:
        return Decision(
            "pass",
            reason,
            normalized,
            runner=detection.runner,
            real_path=real_path,
            detection_source=detection.source,
        )

    return Decision(
        "wrap",
        "runner_detected",
        normalized,
        runner=detection.runner,
        real_path=real_path,
        failtrace_path=failtrace_path,
        detection_source=detection.source,
    )


def env_with_guard(env: dict[str, str]) -> dict[str, str]:
    child_env = dict(env)
    child_env[GUARD_ENV] = "1"
    return child_env


def shell_join(argv: list[str]) -> str:
    return shlex.join(argv)


def soak_dirs() -> list[Path]:
    candidates = [
        Path.home() / ".failtrace" / "agent-soak",
        Path.cwd() / ".failtrace" / "agent-soak",
    ]
    seen: set[str] = set()
    unique = []
    for candidate in candidates:
        key = os.path.abspath(candidate)
        if key not in seen:
            seen.add(key)
            unique.append(candidate)
    return unique


def write_soak_entry(entry: dict) -> bool:
    for soak_dir in soak_dirs():
        try:
            soak_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
            log_file = soak_dir / SOAK_LOG_FILENAME
            fd = os.open(log_file, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
            os.fchmod(fd, 0o600)
            with os.fdopen(fd, "a", encoding="utf-8") as handle:
                handle.write(json.dumps(entry, ensure_ascii=False) + "\n")
            os.chmod(log_file, stat.S_IRUSR | stat.S_IWUSR)
            return True
        except Exception:
            continue
    return False


def log_decision(decision: Decision) -> None:
    try:
        entry = {
            "ts": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "agent": "codex-shim",
            "event": "PATHShim",
            "shim": decision.argv[0] if decision.argv else None,
            "argv0": decision.argv[0] if decision.argv else None,
            "command": shell_join(decision.argv),
            "normalized_command": shell_join(decision.argv),
            "decision": decision.action,
            "reason": decision.reason,
            "suggested_command": (
                "failtrace run -- " + shell_join(decision.argv)
                if decision.action == "wrap"
                else None
            ),
            "runner": decision.runner,
            "real_path": decision.real_path,
            "failtrace_path": decision.failtrace_path,
            "detection_source": decision.detection_source,
        }
        write_soak_entry(entry)
    except Exception:
        pass


def exec_decision(decision: Decision) -> int:
    if decision.action == "wrap":
        assert decision.failtrace_path is not None
        argv = ["failtrace", "run", "--", *decision.argv]
        os.execvpe(decision.failtrace_path, argv, env_with_guard(os.environ))
    if decision.action == "pass":
        assert decision.real_path is not None
        if decision.reason.startswith("failtrace_") and decision.reason != "failtrace_self_shim":
            sys.stderr.write(f"failtrace codex shim: {decision.reason}; running original runner unwrapped\n")
        os.execvpe(decision.real_path, decision.argv, env_with_guard(os.environ))

    message = "failtrace codex shim: "
    if decision.reason == "real_binary_not_found":
        message += f"could not find real binary for {decision.argv[0]!r} after shim dir\n"
    elif decision.reason == "failtrace_binary_not_found":
        message += "could not find real failtrace binary after shim dir\n"
    else:
        message += f"{decision.reason}\n"
    sys.stderr.write(message)
    return 127


def main() -> int:
    try:
        decision = decide(sys.argv, os.environ, shim_dir())
    except Exception:
        argv = command_argv(sys.argv)
        real_path = (resolve_real_binary(argv[0], shim_dir(), os.environ.get("PATH", ""))
                     if argv and argv[0] != "failtrace" else None)
        fallback = Decision("pass" if real_path else "error", "failtrace_integration_error",
                            argv, real_path=real_path)
        log_decision(fallback)
        return exec_decision(fallback)
    log_decision(decision)
    return exec_decision(decision)


if __name__ == "__main__":
    sys.exit(main())
