#!/usr/bin/env python3
# /// script
# requires-python = ">=3.9"
# dependencies = ["PyYAML>=6.0"]
# ///
"""Failtrace adapter gauntlet runner.

Reads scripts/corpus/corpus.yaml, runs each scenario against a real pinned
external repo with mutation patches applied, and emits a Markdown anomaly
report. Captured artifacts land under
.failtrace-corpus/out/runs/<run-id>/<scenario_id>/ for human review; a human
promotes useful ones into tests/fixtures/.

Skip contract: scenarios whose required toolchains are missing on PATH are
skipped, printed to stderr, and counted in the report. The runner exits 0
unless --strict is set or the harness itself errored.

Run with: uv run scripts/corpus/run_corpus.py
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

import yaml


# ---- Layout -----------------------------------------------------------------

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
CORPUS_ROOT = REPO_ROOT / ".failtrace-corpus"
REPO_CACHE = CORPUS_ROOT / "repos"
OUT_DIR = CORPUS_ROOT / "out"
RUNS_DIR = OUT_DIR / "runs"
DEFAULT_MANIFEST = SCRIPT_DIR / "corpus.yaml"
DEFAULT_REPORT = CORPUS_ROOT / "report.md"
DEFAULT_OUT_REPORT = OUT_DIR / "report.md"
LATEST_POINTER = OUT_DIR / "latest"
FAILTRACE_BIN_ENV = "FAILTRACE_BIN"

# Scenario IDs become directory names under .failtrace-corpus/out/ — enforce
# a strict slug shape to keep that path safe.
SCENARIO_ID_RE = re.compile(r"^[a-z][a-z0-9_]*$")

# Threshold ordering for parse_status. Lower = worse parse fidelity.
# Captured value strictly below the threshold is an anomaly; strictly above
# is a note ("improved beyond expected"); equal is silent OK.
# Values mirror the schema enum at src/schema/types.rs:135 (ParseStatus).
PARSE_STATUS_ORDER = {"unrecognized": 0, "partial": 1, "complete": 2}
KNOWN_ADAPTERS = {"pytest", "cargo", "gotest", "tsc", "vitest", "jest"}

# Env vars that the outer `uv run` invocation of this script bleeds into the
# subprocess and contaminate the captured log (most visibly: nested `uv run`
# emits a VIRTUAL_ENV-mismatch warning into stdout). Strip these from the
# scenario subprocess env so the captured artifact represents runner output,
# not corpus-runner tooling noise. Includes PYTHONPATH and the UV_PYTHON*
# family because outer dev-shell state can still change imports or alter uv's
# python-resolution behavior in the nested scenario subprocess.
ENV_VARS_TO_STRIP = (
    "VIRTUAL_ENV",
    "VIRTUAL_ENV_PROMPT",
    "UV_PROJECT_ENVIRONMENT",
    "PYTHONHOME",
    "PYTHONPATH",
    "UV_PYTHON",
    "UV_SYSTEM_PYTHON",
    "UV_NO_SYNC",
)

# GitHub owner / repo / SHA shapes. Used to validate manifest input before
# the values become parts of cache paths or subprocess arguments.
GITHUB_OWNER_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9-]{0,38}$")
GITHUB_REPO_RE = re.compile(r"^[A-Za-z0-9._-]{1,100}$")
GIT_SHA_RE = re.compile(r"^[0-9a-f]{40}$")


# ---- Result types -----------------------------------------------------------


@dataclass
class ScenarioResult:
    scenario_id: str
    adapter: str
    status: str  # "ok", "anomaly", "skipped", "errored"
    detail: str = ""
    duration_ms: int = 0
    anomalies: List[str] = field(default_factory=list)
    notes: List[str] = field(default_factory=list)
    follow_ups: List[str] = field(default_factory=list)
    captured: Optional[Dict[str, Any]] = None
    out_path: Optional[Path] = None


# ---- Manifest loading -------------------------------------------------------


def validate_manifest(path: Path) -> Tuple[List[Dict[str, Any]], List[str]]:
    try:
        with path.open("r", encoding="utf-8") as fh:
            data = yaml.safe_load(fh)
    except OSError as exc:
        return [], [f"manifest {path}: could not read manifest: {exc}"]
    except yaml.YAMLError as exc:
        return [], [f"manifest {path}: invalid YAML: {exc}"]

    issues: List[str] = []
    if not isinstance(data, dict):
        return [], [f"manifest {path}: top-level document must be a mapping"]
    if data.get("schema_version") != 1:
        issues.append(f"manifest {path}: expected schema_version: 1")
    scenarios = data.get("scenarios", [])
    if not isinstance(scenarios, list):
        issues.append(f"manifest {path}: scenarios must be a list")
        return [], issues

    patches_root = (SCRIPT_DIR / "patches").resolve()
    seen_ids: Dict[str, int] = {}
    valid_scenarios: List[Dict[str, Any]] = []
    for index, scenario in enumerate(scenarios, start=1):
        label = f"scenario {index}"
        if not isinstance(scenario, dict):
            issues.append(f"manifest {path}: {label}: must be a mapping")
            continue
        raw_id = scenario.get("id")
        if isinstance(raw_id, str):
            label = f"scenario {index} ({raw_id})"
            if raw_id in seen_ids:
                issues.append(
                    f"manifest {path}: {label}: duplicate id {raw_id!r} "
                    f"(first used by scenario {seen_ids[raw_id]})"
                )
            else:
                seen_ids[raw_id] = index
        valid_scenarios.append(scenario)

        for required in ("id", "adapter", "repo", "command"):
            if required not in scenario:
                issues.append(f"manifest {path}: {label}: missing '{required}'")
        scenario_id = scenario.get("id")
        if "id" in scenario and (
            not isinstance(scenario_id, str)
            or not SCENARIO_ID_RE.match(scenario_id)
        ):
            issues.append(
                f"manifest {path}: scenario id {scenario_id!r} must match "
                f"{SCENARIO_ID_RE.pattern} (it becomes a directory name)"
            )
        adapter = scenario.get("adapter")
        if "adapter" in scenario and (
            not isinstance(adapter, str) or adapter not in KNOWN_ADAPTERS
        ):
            issues.append(
                f"manifest {path}: {label}: adapter {adapter!r} must be one "
                f"of {sorted(KNOWN_ADAPTERS)}"
            )
        command = scenario.get("command")
        if "command" in scenario and (
            not isinstance(command, list)
            or not command
            or any(not isinstance(part, str) for part in command)
        ):
            issues.append(
                f"manifest {path}: scenario {scenario_id} command must be a "
                f"non-empty list of strings"
            )
        setup_commands = scenario.get("setup_commands", [])
        if not isinstance(setup_commands, list):
            issues.append(
                f"manifest {path}: scenario {scenario_id} setup_commands must "
                f"be a list of commands"
            )
            setup_commands = []
        for setup_index, setup_command in enumerate(setup_commands, start=1):
            if not isinstance(setup_command, list) or not setup_command or any(
                not isinstance(part, str) for part in setup_command
            ):
                issues.append(
                    f"manifest {path}: scenario {scenario_id} "
                    f"setup_commands[{setup_index}] must be a non-empty list "
                    f"of strings"
                )
        repo = scenario.get("repo")
        if "repo" in scenario:
            if not isinstance(repo, dict):
                issues.append(
                    f"manifest {path}: scenario {scenario_id} repo must be a mapping"
                )
                repo = {}
            for required in ("owner", "name", "sha"):
                if required not in repo:
                    issues.append(
                        f"manifest {path}: scenario {scenario_id} repo missing "
                        f"'{required}'"
                    )
            owner = repo.get("owner")
            name = repo.get("name")
            sha = repo.get("sha")
            if "owner" in repo and (
                not isinstance(owner, str) or not GITHUB_OWNER_RE.match(owner)
            ):
                issues.append(
                    f"manifest {path}: scenario {scenario_id} repo.owner {owner!r} "
                    f"must match {GITHUB_OWNER_RE.pattern}"
                )
            if "name" in repo and (
                not isinstance(name, str) or not GITHUB_REPO_RE.match(name)
            ):
                issues.append(
                    f"manifest {path}: scenario {scenario_id} repo.name {name!r} "
                    f"must match {GITHUB_REPO_RE.pattern}"
                )
            if "sha" in repo and (
                not isinstance(sha, str) or not GIT_SHA_RE.match(sha)
            ):
                issues.append(
                    f"manifest {path}: scenario {scenario_id} repo.sha {sha!r} "
                    f"must be a 40-char lowercase hex SHA"
                )
        # Patch paths must be relative, under patches/, suffix .patch, no
        # `..` traversal. Resolved path must stay inside SCRIPT_DIR/patches.
        patches = scenario.get("patches", [])
        if not isinstance(patches, list):
            issues.append(
                f"manifest {path}: scenario {scenario_id} patches must be a list"
            )
            patches = []
        for raw in patches:
            if not isinstance(raw, str):
                issues.append(
                    f"manifest {path}: scenario {scenario_id} patch entry {raw!r} "
                    f"must be a string"
                )
                continue
            if raw.startswith("/") or ".." in Path(raw).parts:
                issues.append(
                    f"manifest {path}: scenario {scenario_id} patch {raw!r} "
                    f"must be relative and contain no '..' components"
                )
            if not raw.endswith(".patch"):
                issues.append(
                    f"manifest {path}: scenario {scenario_id} patch {raw!r} "
                    f"must end with '.patch'"
                )
            resolved = (SCRIPT_DIR / raw).resolve()
            try:
                resolved.relative_to(patches_root)
            except ValueError as exc:
                issues.append(
                    f"manifest {path}: scenario {scenario_id} patch {raw!r} "
                    f"resolves outside {patches_root}: {exc}"
                )
        # `parse_status` (exact) and `min_parse_status` (threshold) are
        # mutually exclusive — accepting both produces double anomalies or
        # contradictory verdicts for one captured state.
        expected = scenario.get("expected", {}) or {}
        if not isinstance(expected, dict):
            issues.append(
                f"manifest {path}: scenario {scenario_id} expected must be a mapping"
            )
            expected = {}
        if "parse_status" in expected and "min_parse_status" in expected:
            issues.append(
                f"manifest {path}: scenario {scenario_id} expected: cannot "
                f"specify both 'parse_status' (exact) and 'min_parse_status' "
                f"(threshold) — pick one"
            )
        # follow_ups must be a list of strings. Bare `list(...)` would turn
        # a string into a list of characters; ints would crash Markdown
        # escaping downstream.
        follow_ups = scenario.get("follow_ups", [])
        if not isinstance(follow_ups, list):
            issues.append(
                f"manifest {path}: scenario {scenario_id} follow_ups must be a list"
            )
            follow_ups = []
        for entry in follow_ups:
            if not isinstance(entry, str):
                issues.append(
                    f"manifest {path}: scenario {scenario_id} follow_ups entry "
                    f"{entry!r} must be a string"
                )
        # `requires` must be a list of strings if present (same reasoning).
        requires = scenario.get("requires", [])
        if not isinstance(requires, list) or any(
            not isinstance(r, str) for r in requires
        ):
            issues.append(
                f"manifest {path}: scenario {scenario_id} requires must be a "
                f"list of strings"
            )
    return valid_scenarios, issues


def load_manifest(path: Path) -> List[Dict[str, Any]]:
    scenarios, issues = validate_manifest(path)
    if issues:
        raise SystemExit("\n".join(issues))
    return scenarios


# ---- Skip contract: toolchain probing --------------------------------------


def missing_tools(required: List[str], failtrace_path: Optional[str]) -> List[str]:
    missing = []
    for tool in required:
        if tool == "failtrace":
            if failtrace_path is None:
                missing.append(tool)
            continue
        if shutil.which(tool) is None:
            missing.append(tool)
    return missing


# ---- Repo cache -------------------------------------------------------------


def repo_cache_dir(repo: Dict[str, str]) -> Path:
    owner = repo["owner"]
    name = repo["name"]
    sha = repo["sha"]
    return REPO_CACHE / f"{owner}__{name}@{sha[:8]}"


def ensure_repo(repo: Dict[str, str], verbose: bool) -> Path:
    """Clone the repo at the pinned SHA into the cache; reset to clean state.

    Idempotent: if the cache dir already has the SHA checked out, just clean.
    Uses partial-clone (--depth 1, fetch-by-sha) to avoid pulling history.
    """
    cache = repo_cache_dir(repo)
    sha = repo["sha"]
    url = f"https://github.com/{repo['owner']}/{repo['name']}.git"

    if cache.exists() and (cache / ".git").exists():
        # Reset any prior patches/run artifacts back to clean SHA.
        run_quiet(["git", "reset", "--hard", sha], cwd=cache, verbose=verbose)
        run_quiet(["git", "clean", "-fdx"], cwd=cache, verbose=verbose)
        return cache

    cache.mkdir(parents=True, exist_ok=True)
    run_quiet(["git", "init", "-q"], cwd=cache, verbose=verbose)
    run_quiet(["git", "remote", "add", "origin", url], cwd=cache, verbose=verbose)
    run_quiet(
        ["git", "fetch", "--depth", "1", "-q", "origin", sha],
        cwd=cache,
        verbose=verbose,
    )
    run_quiet(["git", "checkout", "-q", "FETCH_HEAD"], cwd=cache, verbose=verbose)
    return cache


def apply_patches(checkout: Path, patches: List[str], verbose: bool) -> None:
    for rel in patches:
        patch_path = SCRIPT_DIR / rel
        if not patch_path.exists():
            raise FileNotFoundError(f"patch not found: {patch_path}")
        run_quiet(
            ["git", "apply", "--whitespace=nowarn", str(patch_path)],
            cwd=checkout,
            verbose=verbose,
        )


def run_quiet(cmd: List[str], cwd: Path, verbose: bool) -> None:
    if verbose:
        print(f"  $ {' '.join(cmd)} (cwd={cwd})", file=sys.stderr)
    result = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(
            f"command failed: {' '.join(cmd)}\nstdout: {result.stdout}\nstderr: {result.stderr}"
        )


def scenario_subprocess_env() -> Dict[str, str]:
    """Build a subprocess env stripped of outer-runner contamination.

    When this script is invoked via `uv run scripts/corpus/run_corpus.py`, uv
    sets VIRTUAL_ENV (and friends) for the outer interpreter. Nested `uv run`
    inside the scenario command then emits a `VIRTUAL_ENV does not match`
    warning into stdout, which lands in the captured log. The captured
    artifact should reflect runner output, not corpus-runner tooling noise.
    """
    env = os.environ.copy()
    for var in ENV_VARS_TO_STRIP:
        env.pop(var, None)
    return env


def resolve_failtrace_binary() -> Optional[str]:
    """Resolve the absolute path of the `failtrace` binary, or None.

    `FAILTRACE_BIN` wins over PATH so release rehearsals can point at a
    candidate-built binary without stale installs shadowing it. An explicit
    env-var path is a contract, not a hint: missing or non-executable values
    abort immediately.
    """
    override = os.environ.get(FAILTRACE_BIN_ENV)
    if override is not None:
        if not override.strip():
            raise SystemExit(f"{FAILTRACE_BIN_ENV} is set but empty")
        path = Path(override).expanduser()
        if not path.exists():
            raise SystemExit(f"{FAILTRACE_BIN_ENV} binary not found: {path}")
        if not path.is_file():
            raise SystemExit(f"{FAILTRACE_BIN_ENV} is not a file: {path}")
        if not os.access(path, os.X_OK):
            raise SystemExit(f"{FAILTRACE_BIN_ENV} binary is not executable: {path}")
        return str(path.resolve())

    resolved = shutil.which("failtrace")
    return str(Path(resolved).resolve()) if resolved else None


def capture_failtrace_version(failtrace_path: Optional[str]) -> Optional[str]:
    """Return the `failtrace --version` line from the resolved binary, or None.

    Takes the already-resolved absolute path so it can't disagree with the
    binary used for scenario runs.
    """
    if failtrace_path is None:
        return None
    try:
        proc = subprocess.run(
            [failtrace_path, "--version"],
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    line = (proc.stdout or proc.stderr or "").strip().splitlines()
    return line[0] if line else None


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def utc_now_iso() -> str:
    return (
        datetime.now(timezone.utc)
        .replace(microsecond=0)
        .isoformat()
        .replace("+00:00", "Z")
    )


def git_output(args: List[str]) -> Optional[str]:
    try:
        proc = subprocess.run(
            ["git", *args],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return None
    if proc.returncode != 0:
        return None
    return proc.stdout.strip()


def repo_state() -> Dict[str, Any]:
    status = git_output(["status", "--porcelain"])
    return {
        "sha": git_output(["rev-parse", "HEAD"]),
        "short_sha": git_output(["rev-parse", "--short=8", "HEAD"]),
        "dirty": bool(status),
    }


def run_id_suffix(state: Dict[str, Any]) -> str:
    short_sha = state.get("short_sha")
    if not short_sha:
        return "nogit"
    if state.get("dirty"):
        return f"{short_sha}-dirty"
    return short_sha


def create_run_dir(out_dir: Path, started_at: datetime) -> Tuple[str, Path]:
    suffix = run_id_suffix(repo_state())
    base = f"{started_at.strftime('%Y%m%dT%H%M%SZ')}-{suffix}"
    runs_dir = out_dir / "runs"
    runs_dir.mkdir(parents=True, exist_ok=True)
    for attempt in range(1, 100):
        run_id = base if attempt == 1 else f"{base}-{attempt}"
        run_dir = runs_dir / run_id
        try:
            run_dir.mkdir(mode=0o700)
            return run_id, run_dir
        except FileExistsError:
            continue
    raise RuntimeError(f"could not allocate unique run directory under {runs_dir}")


def relative_to_repo(path: Optional[Path]) -> Optional[str]:
    if path is None:
        return None
    try:
        return str(path.relative_to(REPO_ROOT))
    except ValueError:
        return str(path)


def capture_toolchain_versions() -> Dict[str, Dict[str, Any]]:
    commands = {
        "corepack": ["corepack", "--version"],
        "node": ["node", "--version"],
        "yarn_via_corepack": ["corepack", "yarn", "--version"],
        "cargo": ["cargo", "--version"],
        "go": ["go", "version"],
        "uv": ["uv", "--version"],
        "bun": ["bun", "--version"],
    }
    env = os.environ.copy()
    env["COREPACK_ENABLE_DOWNLOAD_PROMPT"] = "0"
    captured: Dict[str, Dict[str, Any]] = {}
    for name, command in commands.items():
        record: Dict[str, Any] = {"command": command}
        try:
            proc = subprocess.run(
                command,
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
                timeout=15,
                env=env,
                check=False,
            )
        except FileNotFoundError:
            record.update({"available": False, "error": "not found"})
        except subprocess.TimeoutExpired:
            record.update({"available": False, "error": "timed out"})
        except OSError as exc:
            record.update({"available": False, "error": str(exc)})
        else:
            text = (proc.stdout or proc.stderr or "").strip().splitlines()
            record.update(
                {
                    "available": proc.returncode == 0,
                    "exit_code": proc.returncode,
                    "version": text[0] if text else None,
                }
            )
        captured[name] = record
    return captured


def write_json(path: Path, payload: Dict[str, Any]) -> None:
    path.write_text(
        json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    path.chmod(0o600)


def write_latest_pointer(run_id: str) -> None:
    LATEST_POINTER.write_text(f"runs/{run_id}\n", encoding="utf-8")
    LATEST_POINTER.chmod(0o600)


def prune_legacy_out(out_dir: Path) -> List[Path]:
    """Remove pre-run-id flat capture directories directly under out_dir."""
    if not out_dir.exists():
        return []
    preserved = {"runs", "latest", "report.md"}
    removed: List[Path] = []
    for child in out_dir.iterdir():
        if child.name in preserved:
            continue
        if child.is_dir():
            shutil.rmtree(child)
            removed.append(child)
    return removed


def check_patches(scenarios: List[Dict[str, Any]], verbose: bool) -> int:
    """Verify each scenario's patch list applies in manifest sequence.

    Patches are applied in order against one fresh checkout per scenario.
    Each patch's `git apply --check` runs against the state with prior
    patches in that scenario already applied, then the patch is actually
    applied before the next one is checked. This catches:
      - patches that conflict only in sequence (B touches lines A changed)
      - patches that intentionally depend on an earlier patch having landed
    Does not run scenarios. Returns 0 if every scenario's chain applies,
    non-zero otherwise.
    """
    failures: List[str] = []
    for scenario in scenarios:
        scenario_id = scenario["id"]
        patches = scenario.get("patches", [])
        if not patches:
            continue
        try:
            checkout = ensure_repo(scenario["repo"], verbose)
        except RuntimeError as exc:
            failures.append(f"{scenario_id}: clone/setup failed: {exc}")
            continue
        chain_ok = True
        for rel in patches:
            patch_path = SCRIPT_DIR / rel
            if not patch_path.exists():
                failures.append(f"{scenario_id}: patch not found: {rel}")
                chain_ok = False
                break
            check_result = subprocess.run(
                ["git", "apply", "--check", "--whitespace=nowarn", str(patch_path)],
                cwd=checkout,
                capture_output=True,
                text=True,
            )
            if check_result.returncode != 0:
                failures.append(
                    f"{scenario_id}: {rel}: {check_result.stderr.strip() or 'rejected'}"
                )
                chain_ok = False
                break
            # Apply for real so the next patch in the chain sees the state
            # this patch produces.
            apply_result = subprocess.run(
                ["git", "apply", "--whitespace=nowarn", str(patch_path)],
                cwd=checkout,
                capture_output=True,
                text=True,
            )
            if apply_result.returncode != 0:
                failures.append(
                    f"{scenario_id}: {rel}: applied-check passed but apply "
                    f"failed: {apply_result.stderr.strip() or 'unknown'}"
                )
                chain_ok = False
                break
            print(f"ok  {scenario_id}: {rel}", file=sys.stderr)
        if chain_ok and len(patches) > 1:
            print(
                f"ok  {scenario_id}: chain of {len(patches)} patches applied in order",
                file=sys.stderr,
            )
    if failures:
        print("\npatch check failures:", file=sys.stderr)
        for line in failures:
            print(f"  {line}", file=sys.stderr)
        return 1
    print("\nall patches apply cleanly.", file=sys.stderr)
    return 0


# ---- Scenario execution -----------------------------------------------------


def elapsed_ms(start: float) -> int:
    return int((time.monotonic() - start) * 1000)


def run_setup_commands(
    commands: List[List[str]], checkout: Path, out_path: Path, verbose: bool
) -> Optional[str]:
    for index, command in enumerate(commands, start=1):
        if verbose:
            print(f"  $ {' '.join(command)} (cwd={checkout})", file=sys.stderr)
        log_path = out_path / f"setup-{index}.log"
        try:
            proc = subprocess.run(
                command,
                cwd=checkout,
                capture_output=True,
                text=True,
                env=scenario_subprocess_env(),
                check=False,
            )
        except FileNotFoundError as exc:
            log_path.write_text(
                "$ "
                + " ".join(command)
                + "\nexit=not-started\n\n--- stdout ---\n\n--- stderr ---\n"
                + str(exc),
                encoding="utf-8",
            )
            log_path.chmod(0o600)
            return f"setup command {index} failed to start: {exc}"
        log_path.write_text(
            "$ "
            + " ".join(command)
            + f"\nexit={proc.returncode}\n\n--- stdout ---\n"
            + proc.stdout
            + "\n--- stderr ---\n"
            + proc.stderr,
            encoding="utf-8",
        )
        log_path.chmod(0o600)
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout or "").strip().splitlines()
            suffix = f": {detail[0][:200]}" if detail else ""
            return f"setup command {index} failed with exit {proc.returncode}{suffix}"
    return None


def run_scenario(
    scenario: Dict[str, Any],
    verbose: bool,
    failtrace_path: Optional[str],
    run_root: Path,
) -> ScenarioResult:
    started = time.monotonic()
    scenario_id = scenario["id"]
    adapter = scenario["adapter"]
    # load_manifest validated follow_ups is a list of strings.
    follow_ups: List[str] = list(scenario.get("follow_ups", []))
    required = list(scenario.get("requires", []))
    # `failtrace` always required; we still PATH-probe it for the skip
    # contract even though we'll invoke via the resolved absolute path.
    if "failtrace" not in required:
        required.append("failtrace")

    missing = missing_tools(required, failtrace_path)
    if missing:
        return ScenarioResult(
            scenario_id=scenario_id,
            adapter=adapter,
            status="skipped",
            detail=f"missing tools: {', '.join(missing)}",
            duration_ms=elapsed_ms(started),
            follow_ups=follow_ups,
        )

    out_path = run_root / scenario_id
    out_path.mkdir(parents=True, mode=0o700, exist_ok=False)

    try:
        checkout = ensure_repo(scenario["repo"], verbose)
        apply_patches(checkout, scenario.get("patches", []), verbose)
    except (RuntimeError, FileNotFoundError) as exc:
        return ScenarioResult(
            scenario_id=scenario_id,
            adapter=adapter,
            status="errored",
            detail=f"setup failed: {exc}",
            duration_ms=elapsed_ms(started),
            follow_ups=follow_ups,
            out_path=out_path,
        )

    setup_error = run_setup_commands(
        list(scenario.get("setup_commands", [])), checkout, out_path, verbose
    )
    if setup_error is not None:
        return ScenarioResult(
            scenario_id=scenario_id,
            adapter=adapter,
            status="errored",
            detail=setup_error,
            duration_ms=elapsed_ms(started),
            follow_ups=follow_ups,
            out_path=out_path,
        )

    failtrace_cmd = [
        failtrace_path or "failtrace",
        "run",
        "--run-dir",
        str(out_path),
        "--",
        *scenario["command"],
    ]
    if verbose:
        print(f"  $ {' '.join(failtrace_cmd)} (cwd={checkout})", file=sys.stderr)
    proc = subprocess.run(
        failtrace_cmd,
        cwd=checkout,
        capture_output=True,
        text=True,
        env=scenario_subprocess_env(),
    )
    (run_root / "latest").unlink(missing_ok=True)

    # failtrace prints the authoritative `run-dir=<path>` on stderr; parse it.
    actual_run_dir = parse_run_dir_from_stderr(proc.stderr)
    captured = None
    if actual_run_dir is not None:
        output_json = actual_run_dir / "output.json"
        if output_json.exists():
            try:
                captured = json.loads(output_json.read_text(encoding="utf-8"))
            except json.JSONDecodeError as exc:
                return ScenarioResult(
                    scenario_id=scenario_id,
                    adapter=adapter,
                    status="errored",
                    detail=f"failtrace output.json was not valid JSON: {exc}",
                    duration_ms=elapsed_ms(started),
                    follow_ups=follow_ups,
                    out_path=actual_run_dir,
                )

    if captured is None:
        return ScenarioResult(
            scenario_id=scenario_id,
            adapter=adapter,
            status="errored",
            detail=(
                f"failtrace produced no output.json (exit={proc.returncode}); "
                f"stderr: {proc.stderr.strip()[:200]}"
            ),
            duration_ms=elapsed_ms(started),
            follow_ups=follow_ups,
            out_path=actual_run_dir,
        )

    anomalies, notes = compare_expected(scenario.get("expected", {}), captured)
    status = "anomaly" if anomalies else "ok"
    return ScenarioResult(
        scenario_id=scenario_id,
        adapter=adapter,
        status=status,
        anomalies=anomalies,
        notes=notes,
        duration_ms=elapsed_ms(started),
        follow_ups=follow_ups,
        captured=captured,
        out_path=actual_run_dir,
    )


def parse_run_dir_from_stderr(stderr: str) -> Optional[Path]:
    """Strict parser matching the contract at src/cli.rs:195.

    Mirrors the parser at tests/cli.rs:239 — the line shape is exactly
    `failtrace: run-dir=<abs> output=<abs>/output.json`, and the split is on
    ` output=` so paths with spaces inside `<abs>` survive. Returns None if
    a matching line isn't present in stderr; the caller treats that as an
    `errored` scenario rather than guessing.
    """
    prefix = "failtrace: run-dir="
    for line in stderr.splitlines():
        if line.startswith(prefix):
            after = line[len(prefix):]
            head, sep, _ = after.partition(" output=")
            if sep:
                return Path(head)
            return None
    return None


def compare_expected(
    expected: Dict[str, Any], captured: Dict[str, Any]
) -> Tuple[List[str], List[str]]:
    """Compare captured shape against the expected block.

    Returns (anomalies, notes). Anomalies fail the scenario in the report;
    notes are informational ("improved beyond expected" signals).

    Exact-match fields: runner_outcome, adapter_id.
    Threshold field: parse_status — declare `min_parse_status: <floor>` and
    the captured value is compared against PARSE_STATUS_ORDER. Below floor =
    anomaly; above floor = note; at floor = silent OK.
    """
    anomalies: List[str] = []
    notes: List[str] = []
    if "runner_outcome" in expected:
        actual = captured.get("runner_outcome")
        if actual != expected["runner_outcome"]:
            anomalies.append(
                f"runner_outcome: expected {expected['runner_outcome']!r}, got {actual!r}"
            )
    if "parse_status" in expected:
        actual = captured.get("parse_status")
        if actual != expected["parse_status"]:
            anomalies.append(
                f"parse_status: expected exact {expected['parse_status']!r}, got {actual!r}"
            )
    if "min_parse_status" in expected:
        floor = expected["min_parse_status"]
        actual = captured.get("parse_status")
        if floor not in PARSE_STATUS_ORDER:
            anomalies.append(
                f"min_parse_status: unknown floor value {floor!r} (expected one of {sorted(PARSE_STATUS_ORDER)})"
            )
        elif actual not in PARSE_STATUS_ORDER:
            anomalies.append(
                f"min_parse_status: captured parse_status {actual!r} is not in {sorted(PARSE_STATUS_ORDER)}"
            )
        else:
            floor_rank = PARSE_STATUS_ORDER[floor]
            actual_rank = PARSE_STATUS_ORDER[actual]
            if actual_rank < floor_rank:
                anomalies.append(
                    f"parse_status: regressed below floor {floor!r}, got {actual!r}"
                )
            elif actual_rank > floor_rank:
                notes.append(
                    f"parse_status: improved beyond floor {floor!r}, got {actual!r} "
                    f"(consider tightening the threshold)"
                )
    if "adapter_id" in expected:
        selected = next(
            (a for a in captured.get("adapters", []) if a.get("selected")), None
        )
        actual_id = selected.get("id") if selected else None
        if actual_id != expected["adapter_id"]:
            anomalies.append(
                f"selected adapter: expected {expected['adapter_id']!r}, got {actual_id!r}"
            )
    return anomalies, notes


# ---- Reporting --------------------------------------------------------------


def md_escape_inline(text: str) -> str:
    """Escape characters that would break inline Markdown rendering.

    Reports interpolate raw failure messages and detail strings from
    third-party output. Backticks would create unintended code spans;
    asterisks and underscores italicize/bold; brackets compose links; pipes
    break table rendering; angle brackets get HTML-parsed in some flavors.
    Keep the escape bounded to characters that have semantics in a
    bullet-list body — not arbitrary CommonMark.
    """
    return (
        text.replace("\\", "\\\\")
        .replace("`", "\\`")
        .replace("|", "\\|")
        .replace("<", "\\<")
        .replace(">", "\\>")
        .replace("*", "\\*")
        .replace("_", "\\_")
        .replace("[", "\\[")
        .replace("]", "\\]")
    )


def render_report(
    results: List[ScenarioResult],
    failtrace_version: Optional[str],
    failtrace_path: Optional[str],
    run_id: Optional[str] = None,
    run_dir: Optional[Path] = None,
) -> str:
    counts = {"ok": 0, "anomaly": 0, "skipped": 0, "errored": 0}
    for result in results:
        counts[result.status] = counts.get(result.status, 0) + 1

    lines: List[str] = []
    lines.append("# Failtrace adapter gauntlet — corpus report")
    lines.append("")
    if run_id:
        lines.append(f"**run id:** `{run_id}`")
        lines.append("")
    if run_dir:
        lines.append(f"**run directory:** `{run_dir}`")
        lines.append("")
    if failtrace_version or failtrace_path:
        version_str = failtrace_version or "(version probe failed)"
        path_str = failtrace_path or "(not found on PATH)"
        lines.append(f"**failtrace binary:** `{version_str}` at `{path_str}`")
        lines.append("")
    lines.append(
        f"**Summary:** {counts['ok']} ok · {counts['anomaly']} anomaly · "
        f"{counts['skipped']} skipped · {counts['errored']} errored"
    )
    lines.append("")
    for result in results:
        lines.append(f"## `{result.scenario_id}` — {result.status}")
        lines.append("")
        lines.append(f"- adapter: `{result.adapter}`")
        if result.detail:
            lines.append(f"- detail: {md_escape_inline(result.detail)}")
        if result.out_path:
            lines.append(f"- artifact: `{result.out_path}`")
        if result.captured:
            first_failure = None
            failures = result.captured.get("failures", [])
            if failures:
                first = failures[0]
                msg = first.get("message", "")
                first_failure = (
                    f"{first.get('phase')}/{first.get('kind')}: "
                    f"{md_escape_inline(msg[:120])}"
                )
            lines.append(
                f"- runner_outcome: `{result.captured.get('runner_outcome')}` · "
                f"parse_status: `{result.captured.get('parse_status')}` · "
                f"failures: {len(failures)}"
            )
            if first_failure:
                lines.append(f"- first failure: {first_failure}")
        if result.anomalies:
            lines.append("- anomalies:")
            for anomaly in result.anomalies:
                lines.append(f"  - {md_escape_inline(anomaly)}")
        if result.notes:
            lines.append("- notes:")
            for note in result.notes:
                lines.append(f"  - {md_escape_inline(note)}")
        if result.follow_ups:
            lines.append("- follow-ups (manifest):")
            for follow_up in result.follow_ups:
                lines.append(f"  - {md_escape_inline(follow_up)}")
        lines.append("")
    return "\n".join(lines)


def selected_adapter(captured: Optional[Dict[str, Any]]) -> Optional[str]:
    if not captured:
        return None
    selected = next(
        (a for a in captured.get("adapters", []) if a.get("selected")), None
    )
    return selected.get("id") if selected else None


def scenario_record(result: ScenarioResult) -> Dict[str, Any]:
    failures = result.captured.get("failures", []) if result.captured else []
    return {
        "id": result.scenario_id,
        "adapter": result.adapter,
        "status": result.status,
        "duration_ms": result.duration_ms,
        "artifact_path": relative_to_repo(result.out_path),
        "runner_outcome": result.captured.get("runner_outcome") if result.captured else None,
        "parse_status": result.captured.get("parse_status") if result.captured else None,
        "selected_adapter": selected_adapter(result.captured),
        "failure_count": len(failures),
        "skip_reason": result.detail if result.status == "skipped" else None,
        "detail": result.detail or None,
        "anomalies": result.anomalies,
        "notes": result.notes,
    }


def build_summary(
    run_id: str,
    run_dir: Path,
    started_at: str,
    finished_at: str,
    duration_ms: int,
    manifest_path: Path,
    manifest_hash: str,
    failtrace_path: Optional[str],
    failtrace_version: Optional[str],
    strict: bool,
    results: List[ScenarioResult],
) -> Dict[str, Any]:
    counts = {"ok": 0, "anomaly": 0, "skipped": 0, "errored": 0}
    for result in results:
        counts[result.status] = counts.get(result.status, 0) + 1
    return {
        "run_id": run_id,
        "run_dir": relative_to_repo(run_dir),
        "started_at": started_at,
        "finished_at": finished_at,
        "duration_ms": duration_ms,
        "repo": repo_state(),
        "manifest": {
            "path": relative_to_repo(manifest_path),
            "sha256": manifest_hash,
            "snapshot_path": relative_to_repo(run_dir / "manifest.snapshot.yaml"),
        },
        "binary": {
            "env_var": FAILTRACE_BIN_ENV,
            "path": failtrace_path,
            "version": failtrace_version,
            "source": "env" if os.environ.get(FAILTRACE_BIN_ENV) is not None else "path",
        },
        "strict": strict,
        "scenario_status_counts": counts,
        "scenarios": [scenario_record(result) for result in results],
    }


# ---- Main -------------------------------------------------------------------


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run the failtrace adapter gauntlet."
    )
    parser.add_argument(
        "--manifest",
        type=Path,
        default=DEFAULT_MANIFEST,
        help=f"Manifest path (default: {DEFAULT_MANIFEST.relative_to(REPO_ROOT)})",
    )
    parser.add_argument(
        "--report",
        type=Path,
        default=DEFAULT_REPORT,
        help=f"Report output path (default: {DEFAULT_REPORT.relative_to(REPO_ROOT)})",
    )
    parser.add_argument(
        "--scenario",
        action="append",
        default=[],
        help="Run only the named scenario (may be passed multiple times).",
    )
    parser.add_argument(
        "--adapter",
        action="append",
        default=[],
        help="Run only scenarios for the named adapter (may be passed multiple times).",
    )
    parser.add_argument(
        "--strict",
        action="store_true",
        help="Exit non-zero if any scenario was skipped, anomalous, or errored.",
    )
    parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        help="Print subcommands as they run.",
    )
    parser.add_argument(
        "--check-patches",
        action="store_true",
        help=(
            "Verify every patch in the manifest applies cleanly to its pinned "
            "SHA via `git apply --check`. Does not run scenarios. Exits 0 if "
            "every patch passes, non-zero otherwise."
        ),
    )
    parser.add_argument(
        "--validate-manifest",
        action="store_true",
        help=(
            "Validate manifest schema only. Does not resolve failtrace, clone "
            "repos, check patches, or run scenarios."
        ),
    )
    parser.add_argument(
        "--prune-out",
        action="store_true",
        help=(
            "Remove legacy flat capture directories directly under "
            ".failtrace-corpus/out before running. Historical runs under "
            "out/runs are preserved."
        ),
    )
    args = parser.parse_args(argv)

    scenarios, manifest_issues = validate_manifest(args.manifest)
    if args.validate_manifest:
        if manifest_issues:
            for issue in manifest_issues:
                print(issue, file=sys.stderr)
            return 1
        print(
            f"manifest validation ok: {len(scenarios)} scenario(s)",
            file=sys.stderr,
        )
        return 0
    if manifest_issues:
        raise SystemExit("\n".join(manifest_issues))
    if args.scenario:
        wanted = set(args.scenario)
        scenarios = [s for s in scenarios if s["id"] in wanted]
    if args.adapter:
        wanted = set(args.adapter)
        scenarios = [s for s in scenarios if s["adapter"] in wanted]
    if not scenarios:
        print("no scenarios match selection", file=sys.stderr)
        return 1

    CORPUS_ROOT.mkdir(parents=True, exist_ok=True)
    REPO_CACHE.mkdir(parents=True, exist_ok=True)
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    RUNS_DIR.mkdir(parents=True, exist_ok=True)

    if args.prune_out:
        removed = prune_legacy_out(OUT_DIR)
        if removed:
            print(
                "pruned legacy out dirs: "
                + ", ".join(path.name for path in removed),
                file=sys.stderr,
            )

    # Resolve `failtrace` once before the check-patches early return so an
    # explicit FAILTRACE_BIN is validated even for the release rehearsal's
    # patch-check phase.
    failtrace_path = resolve_failtrace_binary()

    if args.check_patches:
        return check_patches(scenarios, args.verbose)

    started_at_dt = datetime.now(timezone.utc).replace(microsecond=0)
    run_started = time.monotonic()
    started_at = started_at_dt.isoformat().replace("+00:00", "Z")
    run_id, run_dir = create_run_dir(OUT_DIR, started_at_dt)
    manifest_hash = sha256_file(args.manifest)
    shutil.copy2(args.manifest, run_dir / "manifest.snapshot.yaml")
    (run_dir / "manifest.snapshot.yaml").chmod(0o600)
    write_json(run_dir / "toolchain.json", capture_toolchain_versions())

    failtrace_version = capture_failtrace_version(failtrace_path)
    results: List[ScenarioResult] = []
    for scenario in scenarios:
        print(f"==> {scenario['id']}", file=sys.stderr)
        result = run_scenario(scenario, args.verbose, failtrace_path, run_dir)
        if result.status == "skipped":
            print(f"    skipped — {result.detail}", file=sys.stderr)
        elif result.status == "errored":
            print(f"    errored — {result.detail}", file=sys.stderr)
        elif result.status == "anomaly":
            print(f"    anomaly — {len(result.anomalies)} mismatch(es)", file=sys.stderr)
        else:
            note_count = len(result.notes)
            note_suffix = f" ({note_count} note{'s' if note_count != 1 else ''})" if note_count else ""
            print(f"    ok{note_suffix}", file=sys.stderr)
        results.append(result)

    finished_at_dt = datetime.now(timezone.utc).replace(microsecond=0)
    finished_at = finished_at_dt.isoformat().replace("+00:00", "Z")
    duration_ms = elapsed_ms(run_started)
    report = render_report(results, failtrace_version, failtrace_path, run_id, run_dir)
    (run_dir / "report.md").write_text(report, encoding="utf-8")
    (run_dir / "report.md").chmod(0o600)
    summary = build_summary(
        run_id,
        run_dir,
        started_at,
        finished_at,
        duration_ms,
        args.manifest,
        manifest_hash,
        failtrace_path,
        failtrace_version,
        args.strict,
        results,
    )
    write_json(run_dir / "summary.json", summary)
    write_latest_pointer(run_id)

    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(report, encoding="utf-8")
    DEFAULT_OUT_REPORT.write_text(report, encoding="utf-8")
    print(f"\nreport: {args.report.relative_to(REPO_ROOT)}", file=sys.stderr)
    print(f"run: {run_dir.relative_to(REPO_ROOT)}", file=sys.stderr)

    if args.strict:
        offending = [r for r in results if r.status != "ok"]
        return 1 if offending else 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
