#!/usr/bin/env python3
"""failtrace-enforce: PreToolUse hook that blocks bare pytest/vitest/jest/cargo test/tsc/go test.

Reads Claude Code PreToolUse JSON on stdin. Exits 2 + stderr message to deny
when a Bash command invokes pytest/vitest/jest/cargo test/tsc/go test without
wrapping through `failtrace run`. Exits 0 to allow.

Detector semantics aim to mirror the binary's `detect_pytest` / `detect_vitest` /
`detect_jest` / `detect_cargo_test` / `detect_tsc` / `detect_go_test` in src/cli.rs.
This is a modifier-aware guardrail layer, NOT a strict list of CLI-supported argv
forms: the hook's `strip_prefixes` peels `env` / `time` / `nice` / `command` / `exec`
upstream, so for `time go test` etc. the hook may block forms the binary would
not select if wrapped literally. The remediation in those cases is to invoke the
underlying `go test -json ...` through failtrace, not to wrap the exact modifier
segment. Tokenization uses shlex with punctuation_chars so separators
(`;`, `&&`, `||`, `|`, `&`) split commands into segments that are checked
independently. Subshell `( ... )` and brace-group `{ ... }` wrappers are
filtered before segment splitting so wrapped runners are detected the same
as unwrapped ones (`(uv run pytest) | tail` -> `uv run pytest | tail`).

Optional soak telemetry, off unless explicitly enabled: when the environment
variable `FAILTRACE_CC_SOAK` is exactly `"1"`, hook invocations that denied a
bare runner (`bare_runner_invocation`), recorded a deferred package-script
detection (`package_script_runner_detected_but_cli_unsupported`), or exempted a
runner that executes no tests (`non_executing_runner_invocation`) append a JSON
line to `~/.failtrace/agent-soak/cc-hooks.jsonl`. Nothing else is logged —
passes, skips and no-detection invocations are dropped, and the raw command is
never written (`command` is null; `normalized_command` and `suggested_command`
go through `redact_tokens` first). Enable it by setting `FAILTRACE_CC_SOAK=1`
in the environment Claude Code runs with — e.g. the `env` block of
`~/.claude/settings.json`. Directory existence carries no meaning: the writer
creates the directory when enabled, so an unrelated tool creating it cannot
switch logging on. The log file is created mode 0o600 and re-chmodded on each
write. The writer is fail-open — any error swallowed silently so telemetry
never blocks the hook.

Known limitations are documented in docs/adoption.md.
"""

from __future__ import annotations

import datetime
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
from pathlib import Path
from typing import NamedTuple, Optional

ENV_VAR = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
SEGMENT_SEPS = {";", "&&", "||", "|", "&", "|&"}

BUILD_IDENTITY = re.compile(
    rb"failtrace [0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.+-]+)? "
    rb"\(product=failtrace-rust revision=(?:[0-9a-f]{40}|[0-9a-f]{64}|unknown) "
    rb"state=(?:clean|dirty|unknown)\)\n"
)
IDENTITY_PROBE_ENV = "FAILTRACE_IDENTITY_PROBE"


def configured_failtrace(shim_dir, env=None):
    if "FAILTRACE_BIN" in (os.environ if env is None else env):
        return None
    marker = shim_dir / ".failtrace-codex-shims.marker"
    if not marker.exists():
        return None
    lines = marker.read_text().splitlines()
    if not lines or lines[0] != "failtrace-codex-shims-v1":
        raise ValueError("unrecognized shim marker")
    candidates = [line[len("binary="):] for line in lines if line.startswith("binary=")]
    if len(candidates) > 1:
        raise ValueError("multiple configured binaries")
    return candidates[0] if candidates else None


def resolve_failtrace(env=None, configured=None, excluded_dir=None):
    """Validate one candidate; never search past a missing/mismatched selection.

    FAILTRACE_BIN overrides an installer's configured path. Only when neither
    exists is the first PATH match considered. No cache survives a dispatch.
    The version probe is accidental-name-collision detection, not authentication.
    """
    env = dict(os.environ if env is None else env)
    candidate = env.get("FAILTRACE_BIN", configured)
    if candidate is None:
        entries = env.get("PATH", "").split(os.pathsep)
        if excluded_dir is not None:
            entries = [p for p in entries if Path(p or ".").resolve() != excluded_dir.resolve()]
        candidate = shutil.which("failtrace", path=os.pathsep.join(entries))
        if candidate is not None:
            candidate = os.path.abspath(candidate)
    if candidate == "":
        return None, "failtrace_binary_path_not_absolute"
    if candidate is None:
        return None, "failtrace_binary_not_found"
    if not os.path.isabs(candidate):
        return None, "failtrace_binary_path_not_absolute"
    try:
        path = Path(candidate)
        if not path.is_file() or not os.access(path, os.X_OK):
            return None, "failtrace_binary_not_found"
        if path.resolve().name == "failtrace-codex-shim-dispatcher.py":
            return None, "failtrace_binary_is_shim"
        env[IDENTITY_PROBE_ENV] = "1"
        probe = subprocess.run(
            [candidate, "--version"], env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=2,
        )
        if probe.returncode != 0 or BUILD_IDENTITY.fullmatch(probe.stdout) is None:
            return None, "failtrace_identity_mismatch"
    except subprocess.TimeoutExpired:
        return None, "failtrace_identity_timeout"
    except (OSError, ValueError, RuntimeError):
        return None, "failtrace_identity_error"
    return candidate, None

# Detection-source tags surface where a runner was identified. `argv` means
# the runner was invoked directly (e.g. `pytest`, `npx tsc`). `package_script`
# means the hook resolved a `<pkg-mgr> run <script>` invocation via
# package.json — the inner runner is detected, but enforcement is deferred
# because the Rust CLI's argv-based detectors reject `npm run X` shapes and
# would return AdapterUnavailable. The hook records the detection in
# telemetry without denying until the CLI resolves package scripts too.
DETECTION_SOURCE_ARGV = "argv"
DETECTION_SOURCE_PACKAGE_SCRIPT = "package_script"


class Detection(NamedTuple):
    """A runner identification with its detection source.

    Fields:
      runner: one of PYTEST / VITEST / JEST / CARGO_TEST / TSC / GOTEST.
      source: DETECTION_SOURCE_ARGV (direct argv invocation) or
              DETECTION_SOURCE_PACKAGE_SCRIPT (resolved via package.json).
      resolved_argv: when source is package_script, the inner argv the
              package.json script body resolves to. None for argv-source
              detections (the segment IS the argv). Not yet acted on; reserved
              for future package-script support.
      non_executing: the segment carries a NON_EXECUTING_FLAGS entry for this
              runner, so it runs no tests and `main()` passes instead of
              denying. Only ever set for DETECTION_SOURCE_ARGV detections.
              Detection itself is unaffected — this is an enforcement-policy
              annotation applied after the fact.

    NamedTuple (not @dataclass) chosen so the module loads cleanly via
    importlib.util.spec_from_file_location (used by the Codex PATH shim's
    detector loader); Python 3.9 dataclass field introspection chokes on
    `from __future__ import annotations` + spec-loaded modules. Fields are
    append-only with defaults for the same reason: the shim's detector loader
    and the parity harness construct and read these by keyword.
    """
    runner: str
    source: str
    resolved_argv: Optional[list] = None
    non_executing: bool = False

# Subshell `( ... )` and brace-group `{ ... }` wrappers. We filter tokens
# made entirely of these characters before segment splitting, so the
# contents are detected the same as unwrapped commands. shlex with
# punctuation_chars=True coalesces runs (`((pytest))` -> `((`, `pytest`, `))`),
# hence the "all chars in set" check. Quoted parens (`"(my-test)"`) survive
# as a mixed token like `(my-test)` and are not filtered.
_GROUPING_CHARS = frozenset("(){}")

# Opt-in soak telemetry, gated solely on SOAK_ENV_VAR == "1". Directory
# existence is deliberately NOT part of the gate: the Codex PATH-shim
# dispatcher (scripts/codex-shim-dispatcher.py) creates SOAK_DIR for its own
# `codex-shims.jsonl` stream, so the directory being present says nothing
# about whether the user opted in. The writer creates the directory itself
# when enabled. Tests monkey-patch these to a tmpdir.
SOAK_ENV_VAR = "FAILTRACE_CC_SOAK"
SOAK_DIR = Path.home() / ".failtrace" / "agent-soak"
SOAK_LOG_FILE = SOAK_DIR / "cc-hooks.jsonl"

# The only (decision, reason) pairs worth keeping. Denials are the enforcement
# record; package-script detections show demand for package-script support.
# Everything else (passes, no-detection rows, both skip reasons) is the bulk of
# the volume and carries no signal.
SOAK_LOGGED_EVENTS = frozenset({
    ("deny", "bare_runner_invocation"),
    ("pass", "package_script_runner_detected_but_cli_unsupported"),
    ("pass", "non_executing_runner_invocation"),
})

# Flag names whose value is assumed secret. Matched as a case-insensitive
# substring of the flag name, so `--api-key`, `--auth-token` and `--Password`
# all hit. Over-redaction is the intended failure mode.
SECRET_FLAG_HINTS = ("password", "passwd", "token", "secret", "key",
                     "credential", "auth")

REDACTED = "<redacted>"

PYTEST = "pytest"
VITEST = "vitest"
JEST = "jest"
CARGO_TEST = "cargo-test"
TSC = "tsc"
GOTEST = "gotest"
XCODEBUILD_TEST = "xcodebuild-test"

# Per-runner flags meaning "this invocation executes no tests". Presence of any
# of them in a detected segment's residual makes the hook pass instead of deny:
# there is nothing to wrap, so a denial only forces a meaningless `failtrace
# run` artifact or a workaround. Sourced from real denied invocations:
# `pytest --collect-only`, `pytest --version`/`--help`, `cargo test --no-run`.
#
# Safety argument for the whole table: an entry is only dangerous if it also
# has an *executing* meaning for that runner. A flag the runner doesn't know
# makes it error out, which is itself non-executing. So the risk is confined
# to case and spelling collisions with real executing flags — pytest `-v`,
# `go -v` (verbose) — which is why matching is exact and case-sensitive, and
# why the sets are per-runner rather than shared. `--co` must not match the
# longer `--continue-on-collection-errors`; exact token equality gives that
# for free.
#
# xcodebuild is deliberately absent: `-version`, `-help`, `-list` and
# `-showsdks` carry no `test` action, so `detect_xcodebuild_test` never fires
# on them and there is nothing to exempt.
NON_EXECUTING_FLAGS = {
    # `--collect-only`/`--co` collect and report without running; `--fixtures`
    # and `--markers` list and exit.
    PYTEST: frozenset({
        "--collect-only", "--co", "--fixtures", "--funcargs", "--markers",
        "--version", "--help", "-h",
    }),
    # `--no-run` is "Compile, but don't run tests" per cargo-test(1).
    CARGO_TEST: frozenset({"--no-run", "--help", "-h"}),
    JEST: frozenset({"--listTests", "--version", "--help"}),
    VITEST: frozenset({"--version", "--help"}),
    # `--noEmit` still runs the typecheck and must keep denying. `--watch`/`-w`
    # is already rejected inside `detect_tsc`, so it never reaches here.
    TSC: frozenset({
        "--version", "-v", "--help", "-h", "--showConfig", "--all",
    }),
    # Go's flag package treats `-h`, `-help` and `--help` alike. Per
    # `go help testflag`, `-list regexp` lists matching tests and "No tests,
    # benchmarks, fuzz tests, or examples will be run."
    GOTEST: frozenset({"-h", "-help", "--help", "-list", "--list"}),
}

# Where `scripts/install-codex-shims.sh` puts the dispatcher symlinks by
# default (its `prefix` variable). A runner invoked through one of these IS
# wrapped by failtrace, so denying it is the worst false positive class for
# adoption. Installs that overrode
# the installer's `--prefix` are not recognized; the marker file the installer
# drops would identify them, but that stat would run speculatively on every
# path-headed command, before anything has matched — unlike `_codex_shim_exists`,
# which is gated behind a lexical shim-dir match and so fires on approximately
# no traffic. Not worth it for a case never observed. Tests monkey-patch this.
CODEX_SHIM_DIR = Path.home() / ".failtrace" / "codex-shims"

# Mirrors src/cli.rs value-flag tables. Flags listed here consume the
# following token as their value, so the next non-option token is the
# wrapped command, not the flag's argument.
PYTHON_VALUE_FLAGS = {"-c", "-W", "-X", "--check-hash-based-pycs"}
UV_VALUE_FLAGS = {
    "--with", "--with-requirements", "--with-editable", "--python", "--from",
    "--package", "--directory", "--refresh-package", "--override",
    "--constraint", "--index-url", "--extra-index-url", "--find-links",
    "--config-setting", "--extra", "--group", "--cache-dir", "--config-file",
}
POETRY_VALUE_FLAGS = {"--env", "--with", "--without", "--only", "--extras", "--directory", "--project"}
PIPENV_VALUE_FLAGS = {"--python", "--where"}
COVERAGE_VALUE_FLAGS = {"--rcfile", "--source", "--include", "--omit", "--data-file", "--concurrency", "--debug"}
NPM_VALUE_FLAGS = {"--package", "-p", "--cache", "--userconfig", "--registry", "--prefix"}
PNPM_VALUE_FLAGS = {"--package", "-C", "--dir", "--workspace-root", "--store-dir", "--cache-dir", "--config", "--filter", "-F"}
YARN_VALUE_FLAGS = {"--cwd", "--cache-folder", "--global-folder", "--modules-folder", "--use-yarnrc"}
BUN_VALUE_FLAGS = {"--package", "-p", "--cwd"}
CARGO_VALUE_FLAGS = {"--config", "-Z", "--color", "-C"}

# Package-manager heads that support `<head> run <script>` resolving to
# package.json scripts.<script>. The mapped value-flag set lets us skip
# wrapper flags before checking for the `run` subcommand. Implicit short
# forms (yarn 1's `yarn typecheck`, pnpm 7+'s `pnpm typecheck`,
# bun's `bun typecheck`) are deliberately NOT included — distinguishing
# script names from builtin subcommands requires per-version knowledge
# that the hook doesn't carry. Adopters using short forms can either
# `<pkg-mgr> run <script>` explicitly or wrap the underlying runner.
PACKAGE_RUN_HEADS = {
    "npm": NPM_VALUE_FLAGS,
    "pnpm": PNPM_VALUE_FLAGS,
    "yarn": YARN_VALUE_FLAGS,
    "bun": BUN_VALUE_FLAGS,
}

# Subcommands that resolve a package.json script. npm and pnpm support
# `run-script` as an alias for `run`; yarn and bun do not.
_PACKAGE_RUN_SUBCOMMANDS = {
    "npm": ("run", "run-script"),
    "pnpm": ("run", "run-script"),
    "yarn": ("run",),
    "bun": ("run",),
}

# Directory-redirect / workspace-selection flags. When any of these appear
# in the package-manager arg block, we fail-open: we don't know which
# package.json the user is targeting, and resolving the wrong one would
# either over-block (false positive) or under-block (false negative).
# This is intentionally conservative — full monorepo introspection is
# real scope deferred to a future iteration.
_DIRECTORY_REDIRECT_FLAGS = {
    "--prefix", "-C", "--dir", "--cwd",
    "--workspace-root", "--workspace", "-w",
    "--filter", "-F",
}

# Bound script-resolution recursion (`scripts.a = "npm run b"` chains).
# Three hops is generous for real package.json conventions; deeper chains
# probably indicate a config issue, and uncapped recursion is a footgun.
_MAX_SCRIPT_RECURSION = 3

# Per-process cache for package.json reads, keyed by the start_dir as
# passed by the caller (NOT the resolved path). Two different start_dirs
# that walk up to the same package.json get two cache entries — wasteful
# but not buggy, and the hook is a short-lived process per Bash call so
# the cache doesn't persist across invocations.
_PACKAGE_JSON_CACHE: dict = {}


def strip_prefixes(tokens):
    """Skip env-var assignments and command modifiers; return the residual."""
    i = 0
    while i < len(tokens):
        t = tokens[i]
        if ENV_VAR.match(t):
            i += 1
            continue
        if t == "command":
            # `command [-pVv] [--] <cmd>` per POSIX. All option flags are
            # boolean; -- ends the option list.
            i += 1
            while i < len(tokens):
                u = tokens[i]
                if u == "--":
                    i += 1
                    break
                if u.startswith("-"):
                    i += 1
                    continue
                break
            continue
        if t == "exec":
            # `exec [-cl] [-a NAME] [--] <cmd>`. -a takes a value.
            i += 1
            while i < len(tokens):
                u = tokens[i]
                if u == "--":
                    i += 1
                    break
                if u == "-a" and i + 1 < len(tokens):
                    i += 2
                    continue
                if u.startswith("-"):
                    i += 1
                    continue
                break
            continue
        if t == "env":
            i += 1
            while i < len(tokens):
                u = tokens[i]
                if u == "--":
                    i += 1
                    break
                if u == "-u" and i + 1 < len(tokens):
                    i += 2
                    continue
                if u.startswith("-"):
                    i += 1
                    continue
                if ENV_VAR.match(u):
                    i += 1
                    continue
                break
            continue
        if t == "time":
            i += 1
            if i < len(tokens) and tokens[i].startswith("-"):
                i += 1
            continue
        if t == "nice":
            i += 1
            if i < len(tokens):
                u = tokens[i]
                # `nice -n N`, `nice --adjustment N`, `nice --adjustment=N`,
                # `nice -5` short form.
                if u == "-n" and i + 1 < len(tokens):
                    i += 2
                elif u == "--adjustment" and i + 1 < len(tokens):
                    i += 2
                elif u.startswith("-"):
                    i += 1
            continue
        break
    return tokens[i:]


def skip_wrapper_flags(args, value_flags):
    """Walk args skipping option flags; return the index of the first
    non-option token. Mirrors src/cli.rs::skip_wrapper_flags."""
    i = 0
    while i < len(args):
        arg = args[i]
        if arg == "--":
            return i + 1
        if not arg.startswith("-"):
            return i
        if "=" in arg:
            i += 1
        elif arg in value_flags:
            i += 2
        else:
            i += 1
    return i


def find_m_module_args(args, target_module, value_flags):
    """Return args after `-m target_module` when it follows only options."""
    i = 0
    while i < len(args):
        arg = args[i]
        if arg == "-m":
            if i + 1 < len(args) and args[i + 1] == target_module:
                return args[i + 2:]
            return None
        if not arg.startswith("-"):
            return None
        if "=" in arg:
            i += 1
        elif arg in value_flags:
            i += 2
        else:
            i += 1
    return None


def is_python_basename(s):
    return s == "python" or s == "python3" or s.startswith("python3.")


def basename(token):
    return os.path.basename(token)


def is_go_binary_basename(name):
    if name.endswith(".exe"):
        name = name[:-4]
    if name in ("go", "gotip"):
        return True
    if not name.startswith("go"):
        return False
    return is_go_version_suffix(name[2:])


_ASCII_DIGITS = frozenset("0123456789")


def _all_ascii_digits(s):
    return bool(s) and all(c in _ASCII_DIGITS for c in s)


def is_go_version_suffix(version):
    # Mirror Rust's `char::is_ascii_digit()` exactly. Python `str.isdigit()` accepts non-ASCII
    # numerals (e.g. `go1.٢`) which the CLI detector rejects; detector drift would let the hook
    # block commands the CLI can't parse.
    if not version.startswith("1."):
        return False
    rest = version[2:]
    i = 0
    while i < len(rest) and rest[i] in _ASCII_DIGITS:
        i += 1
    if i == 0:
        return False
    rest = rest[i:]
    if not rest:
        return True
    if rest.startswith("."):
        return _all_ascii_digits(rest[1:])
    for suffix in ("rc", "beta"):
        if rest.startswith(suffix):
            return _all_ascii_digits(rest[len(suffix):])
    return False


_SENTINEL = object()


def _load_package_json(start_dir=None):
    """Walk up from start_dir (default: os.getcwd()) looking for package.json.

    Returns the parsed dict, or None if not found or malformed. Reads at most
    one package.json (the first one encountered walking up). Cached per
    raw start_dir (NOT the resolved path) for the lifetime of the process.

    Fails open on any error — malformed JSON, unreadable file, missing CWD,
    pathological nesting that overflows Python's recursion limit, etc. The
    hook contract is "never crash on bad input"; falling back to allow is
    always safer than aborting the user's Bash call.
    """
    if start_dir is None:
        try:
            start_dir = os.getcwd()
        except (OSError, FileNotFoundError):
            return None
    cached = _PACKAGE_JSON_CACHE.get(start_dir, _SENTINEL)
    if cached is not _SENTINEL:
        return cached
    result = None
    try:
        d = Path(start_dir).resolve()
    except (OSError, RuntimeError):
        _PACKAGE_JSON_CACHE[start_dir] = None
        return None
    while True:
        p = d / "package.json"
        try:
            if p.is_file():
                with p.open() as f:
                    parsed = json.load(f)
                if isinstance(parsed, dict):
                    result = parsed
                break
        except (OSError, json.JSONDecodeError, ValueError, RecursionError):
            # Malformed, unreadable, or pathological package.json: stop
            # walking and treat as "no script resolution available."
            # Production code falls back to the existing argv-based
            # behavior (allow). Fail-open is the contract.
            break
        except Exception:
            # Defense in depth — any other unexpected exception from the
            # file read or JSON parse path also falls back to allow.
            break
        if d.parent == d:
            break
        d = d.parent
    _PACKAGE_JSON_CACHE[start_dir] = result
    return result


def _get_package_script_body(script_name, start_dir=None):
    """Return the string body of scripts.<script_name> from package.json, or None.

    Returns None when no package.json is found, when it doesn't have a
    scripts mapping, when the script isn't present, or when the body
    isn't a plain string (npm allows array forms in pre_/post_-hooks
    arrays but the canonical `scripts` entry is a string).
    """
    pkg = _load_package_json(start_dir)
    if not isinstance(pkg, dict):
        return None
    scripts = pkg.get("scripts")
    if not isinstance(scripts, dict):
        return None
    body = scripts.get(script_name)
    if isinstance(body, str):
        return body
    return None


def detect_pytest(argv):
    """Mirror src/cli.rs::detect_pytest. argv is a list of tokens."""
    if not argv:
        return False
    head = basename(argv[0])

    if head == PYTEST:
        return not pytest_mode_unsupported(argv[1:])

    if is_python_basename(head):
        args = find_m_module_args(argv[1:], PYTEST, PYTHON_VALUE_FLAGS)
        return args is not None and not pytest_mode_unsupported(args)

    if head == "coverage":
        if len(argv) < 2 or argv[1] != "run":
            return False
        args = find_m_module_args(argv[2:], PYTEST, COVERAGE_VALUE_FLAGS)
        return args is not None and not pytest_mode_unsupported(args)

    if head == "uv":
        wrapper_flags = UV_VALUE_FLAGS
    elif head == "poetry":
        wrapper_flags = POETRY_VALUE_FLAGS
    elif head == "pipenv":
        wrapper_flags = PIPENV_VALUE_FLAGS
    else:
        return False

    if len(argv) < 2 or argv[1] != "run":
        return False
    offset = skip_wrapper_flags(argv[2:], wrapper_flags)
    i = 2 + offset
    if i >= len(argv):
        return False
    return detect_pytest(argv[i:])


def pytest_mode_unsupported(args):
    """Reject modes whose output is not a single complete captured session."""
    value_options = frozenset("kmropWcn")
    i = 0
    while i < len(args):
        arg = args[i]
        if (arg == "--looponfail" or arg.startswith("--looponfail=")
                or arg in ("--pdb", "--trace", "--capture=no")
                or (arg == "--capture" and i + 1 < len(args)
                    and args[i + 1] == "no")
                or arg.startswith("@")):
            return True

        if arg.startswith("-") and not arg.startswith("--") and len(arg) > 1:
            cluster = arg[1:]
            for offset, option in enumerate(cluster):
                if option in ("s", "f"):
                    return True
                if option in value_options:
                    if offset == len(cluster) - 1:
                        i += 1
                    break
        i += 1
    return False


def detect_wrapped_vitest(args, value_flags):
    """Skip value flags, then require an explicit one-shot vitest invocation."""
    offset = skip_wrapper_flags(args, value_flags)
    if offset >= len(args):
        return False
    return (basename(args[offset]) == VITEST
            and vitest_args_are_explicitly_one_shot(args[offset + 1:]))


def detect_pnpm_vitest(args):
    offset = skip_wrapper_flags(args, PNPM_VALUE_FLAGS)
    if offset >= len(args):
        return False
    cmd = args[offset]
    if cmd == "vitest":
        return vitest_args_are_explicitly_one_shot(args[offset + 1:])
    if cmd in ("exec", "dlx"):
        return detect_wrapped_vitest(args[offset + 1:], PNPM_VALUE_FLAGS)
    return False


def detect_npm_vitest(args):
    offset = skip_wrapper_flags(args, NPM_VALUE_FLAGS)
    if offset >= len(args):
        return False
    cmd = args[offset]
    if cmd in ("exec", "x"):
        return detect_wrapped_vitest(args[offset + 1:], NPM_VALUE_FLAGS)
    return False


def detect_yarn_vitest(args):
    offset = skip_wrapper_flags(args, YARN_VALUE_FLAGS)
    if offset >= len(args):
        return False
    cmd = args[offset]
    if cmd == "vitest":
        return vitest_args_are_explicitly_one_shot(args[offset + 1:])
    if cmd == "exec":
        return detect_wrapped_vitest(args[offset + 1:], YARN_VALUE_FLAGS)
    return False


def boolean_flag_enabled(argv, long, short=None):
    for arg in argv:
        if arg == long or (short is not None and arg == short):
            return True
        prefix = long + "="
        if arg.startswith(prefix) and arg[len(prefix):] != "false":
            return True
    return False


def vitest_args_are_explicitly_one_shot(args):
    if any(arg in ("watch", "dev", "--run=false", "--no-run") for arg in args):
        return False
    if args and args[0] == "run":
        return True
    return any(arg in ("--run", "--run=true", "--watch=false", "--no-watch")
               for arg in args)


def detect_vitest(argv):
    """Mirror src/cli.rs::detect_vitest."""
    if not argv:
        return False
    if (boolean_flag_enabled(argv, "--watch", "-w")
            or boolean_flag_enabled(argv, "--ui")
            or boolean_flag_enabled(argv, "--standalone")):
        return False
    head = basename(argv[0])

    if head == VITEST:
        return vitest_args_are_explicitly_one_shot(argv[1:])
    if head == "npx":
        return detect_wrapped_vitest(argv[1:], NPM_VALUE_FLAGS)
    if head == "npm":
        return detect_npm_vitest(argv[1:])
    if head == "pnpm":
        return detect_pnpm_vitest(argv[1:])
    if head == "yarn":
        return detect_yarn_vitest(argv[1:])
    if head == "bunx":
        return detect_wrapped_vitest(argv[1:], BUN_VALUE_FLAGS)
    if head == "bun":
        offset = skip_wrapper_flags(argv[1:], BUN_VALUE_FLAGS)
        i = 1 + offset
        if i >= len(argv) or argv[i] != "x":
            return False
        return detect_wrapped_vitest(argv[i + 1:], BUN_VALUE_FLAGS)
    return False


def detect_wrapped_jest(args, value_flags):
    """Skip value flags, then check next token's basename is 'jest' or 'jest@<spec>'.

    The package-spec form (e.g. `npx jest@30.4`) is admitted to mirror
    src/cli.rs::detect_wrapped_jest after F5b. Vitest detection deliberately
    stays exact-match.
    """
    offset = skip_wrapper_flags(args, value_flags)
    if offset >= len(args):
        return False
    name = basename(args[offset])
    return name == JEST or name.startswith("jest@")


def detect_pnpm_jest(args):
    offset = skip_wrapper_flags(args, PNPM_VALUE_FLAGS)
    if offset >= len(args):
        return False
    cmd = args[offset]
    if cmd == "jest":
        return True
    if cmd in ("exec", "dlx"):
        return detect_wrapped_jest(args[offset + 1:], PNPM_VALUE_FLAGS)
    return False


def detect_npm_jest(args):
    offset = skip_wrapper_flags(args, NPM_VALUE_FLAGS)
    if offset >= len(args):
        return False
    cmd = args[offset]
    if cmd in ("exec", "x"):
        return detect_wrapped_jest(args[offset + 1:], NPM_VALUE_FLAGS)
    return False


def detect_yarn_jest(args):
    offset = skip_wrapper_flags(args, YARN_VALUE_FLAGS)
    if offset >= len(args):
        return False
    cmd = args[offset]
    if cmd == "jest":
        return True
    if cmd == "exec":
        return detect_wrapped_jest(args[offset + 1:], YARN_VALUE_FLAGS)
    return False


def detect_jest(argv):
    """Mirror src/cli.rs::detect_jest."""
    if not argv:
        return False
    if (boolean_flag_enabled(argv, "--watch")
            or boolean_flag_enabled(argv, "--watchAll")
            or boolean_flag_enabled(argv, "--watch-all")):
        return False
    head = basename(argv[0])

    if head == JEST:
        return True
    if head == "npx":
        return detect_wrapped_jest(argv[1:], NPM_VALUE_FLAGS)
    if head == "npm":
        return detect_npm_jest(argv[1:])
    if head == "pnpm":
        return detect_pnpm_jest(argv[1:])
    if head == "yarn":
        return detect_yarn_jest(argv[1:])
    if head == "bunx":
        return detect_wrapped_jest(argv[1:], BUN_VALUE_FLAGS)
    if head == "bun":
        offset = skip_wrapper_flags(argv[1:], BUN_VALUE_FLAGS)
        i = 1 + offset
        if i >= len(argv) or argv[i] != "x":
            return False
        return detect_wrapped_jest(argv[i + 1:], BUN_VALUE_FLAGS)
    return False


def detect_cargo_test(argv):
    """Mirror src/cli.rs::detect_cargo_test."""
    if not argv:
        return False
    head = basename(argv[0])

    if head == "rustup":
        if len(argv) < 4 or argv[1] != "run":
            return False
        return detect_cargo_test(argv[3:])

    if head != "cargo":
        return False

    i = 1
    if i < len(argv) and argv[i].startswith("+"):
        i += 1
    while i < len(argv):
        arg = argv[i]
        if arg in ("test", "t"):
            return True
        if arg == "--":
            return False
        if not arg.startswith("-"):
            return False
        if "=" in arg:
            i += 1
        elif arg in CARGO_VALUE_FLAGS:
            i += 2
        else:
            i += 1
    return False


def wrapped_tsc_args(args, value_flags):
    offset = skip_wrapper_flags(args, value_flags)
    if offset >= len(args):
        return None
    if basename(args[offset]) != TSC:
        return None
    return args[offset + 1:]


def tsc_command_args(argv):
    """Return argv after the real tsc executable, mirroring src/cli.rs."""
    if not argv:
        return None
    head = basename(argv[0])

    if head == TSC:
        return argv[1:]
    if head == "npx":
        return wrapped_tsc_args(argv[1:], NPM_VALUE_FLAGS)
    if head == "npm":
        args = argv[1:]
        offset = skip_wrapper_flags(args, NPM_VALUE_FLAGS)
        if offset < len(args) and args[offset] in ("exec", "x"):
            return wrapped_tsc_args(args[offset + 1:], NPM_VALUE_FLAGS)
        return None
    if head == "pnpm":
        args = argv[1:]
        offset = skip_wrapper_flags(args, PNPM_VALUE_FLAGS)
        if offset >= len(args):
            return None
        if args[offset] == TSC:
            return args[offset + 1:]
        if args[offset] in ("exec", "dlx"):
            return wrapped_tsc_args(args[offset + 1:], PNPM_VALUE_FLAGS)
        return None
    if head == "yarn":
        args = argv[1:]
        offset = skip_wrapper_flags(args, YARN_VALUE_FLAGS)
        if offset >= len(args):
            return None
        if args[offset] == TSC:
            return args[offset + 1:]
        if args[offset] == "exec":
            return wrapped_tsc_args(args[offset + 1:], YARN_VALUE_FLAGS)
        return None
    if head == "bunx":
        return wrapped_tsc_args(argv[1:], BUN_VALUE_FLAGS)
    if head == "bun":
        offset = skip_wrapper_flags(argv[1:], BUN_VALUE_FLAGS)
        i = 1 + offset
        if i >= len(argv) or argv[i] != "x":
            return None
        return wrapped_tsc_args(argv[i + 1:], BUN_VALUE_FLAGS)
    return None


def detect_tsc(argv):
    """Mirror src/cli.rs::detect_tsc."""
    args = tsc_command_args(argv)
    return args is not None and not any(
        arg in ("--watch", "-w") or arg.startswith("@") for arg in args
    )


def detect_go_test(argv):
    """Mirror src/cli.rs::detect_go_test, simplified.

    The Rust detector also handles `env` + plain KEY=VAL assignments inline; the
    hook's `strip_prefixes` already strips `env` (and `env -u`, `env -i`, etc.)
    upstream, so the residual passed here is the bare `go test ...` form.
    """
    return (
        len(argv) >= 2
        and is_go_binary_basename(basename(argv[0]))
        and argv[1] == "test"
    )


def detect_xcodebuild_test(argv):
    """Mirror src/cli.rs::is_xcodebuild_test_command.

    Claims `xcodebuild test`, `xcodebuild test-without-building`, and
    `xcodebuild clean test`. Also handles `xcrun xcodebuild test` with an
    explicit xcrun flag table (value flags consume the next token; reject
    options like `--find`/`--version` short-circuit; unknown flags are treated
    as boolean). Rejects `xcodebuild build`, `xcodebuild archive`, etc. when
    no `test` verb follows.
    """
    if not argv:
        return False
    head = basename(argv[0])
    if head == "xcrun":
        # xcrun value-flags (consume the next token). Per man xcrun: only
        # --sdk and --toolchain take values. `--run`/`-r` is boolean, not
        # a value flag.
        value_flags = {"-sdk", "--sdk", "-toolchain", "--toolchain"}
        # xcrun options whose presence means no execution happens
        # (find/help/version/show-*). Boolean modifiers like --run, --log,
        # --no-cache, --kill-cache, --verbose still execute and fall
        # through to the catch-all boolean branch.
        # Full --show-* family per man xcrun — each prints metadata and does
        # not execute the wrapped tool. (--show-platform-info is NOT a real
        # xcrun flag.)
        reject_options = {
            "-f", "--find", "-h", "--help", "--version",
            "--show-sdk-version", "--show-sdk-build-version",
            "--show-sdk-path", "--show-sdk-platform-path",
            "--show-sdk-platform-version", "--show-toolchain-path",
        }
        i = 1
        while i < len(argv):
            arg = argv[i]
            if arg in reject_options:
                return False
            if arg in value_flags:
                i += 2
                continue
            if arg.startswith("-"):
                # Unknown xcrun flag — treat as boolean (consumes 0 tokens).
                i += 1
                continue
            break
        argv = argv[i:]
        if not argv:
            return False
    if basename(argv[0]) != "xcodebuild":
        return False
    # xcodebuild value-flags (consume next token as the flag's value).
    # Without this skip, `xcodebuild -derivedDataPath build test ...` would
    # mis-classify the path value `build` as the action verb and false-reject.
    # Mirror of XCODEBUILD_VALUE_FLAGS in src/cli.rs.
    xcodebuild_value_flags = {
        "-project", "-target", "-scheme", "-configuration", "-sdk", "-arch",
        "-destination", "-destination-timeout", "-derivedDataPath",
        "-resultBundlePath", "-resultBundleVersion", "-toolchain",
        "-archivePath", "-exportPath", "-exportOptionsPlist",
        "-workspace", "-xcconfig", "-testPlan",
        "-parallel-testing-worker-count", "-maximum-parallel-testing-workers",
        "-maximum-concurrent-test-device-destinations",
        "-maximum-concurrent-test-simulator-destinations",
    }
    # xcodebuild(1) defines `action ...` as "Specify one or more actions to
    # perform" — `xcodebuild clean build test` is a valid multi-action
    # invocation where the test phase executes. Detector must NOT early-reject
    # on seeing `build`/`archive`/etc. before `test`; only correct rejection
    # is "no test action anywhere in the argv".
    rest = argv[1:]
    if "-workspace" in rest:
        return False
    i = 0
    while i < len(rest):
        arg = rest[i]
        if arg in xcodebuild_value_flags:
            i += 2
            continue
        if arg in ("test", "test-without-building"):
            return True
        i += 1
    return False


def detect(tokens):
    """Return a Detection for the runner this segment invokes, or None.

    Source-aware variant of `detect_runner`. Argv-based detectors produce
    Detection(source=DETECTION_SOURCE_ARGV); package.json script resolution
    produces Detection(source=DETECTION_SOURCE_PACKAGE_SCRIPT) with the
    inner segment captured in `resolved_argv`.

    Callers that need only the runner name should use `detect_runner` — it
    wraps this function and discards the source/resolved_argv fields.
    """
    return _detect_runner_at_depth(tokens, 0)


def detect_runner(tokens):
    """Return the runner name this segment invokes, or None.

    Legacy compatibility wrapper around `detect`. Preserves the original
    (runner_name | None) return shape for callers that don't need source
    information. New code should call `detect` directly.

    After argv-based detectors, falls through to package.json script
    resolution for `npm|pnpm|yarn|bun run <script>` patterns. That fallback
    finds `package.json` by walking up from cwd, looks up
    `scripts.<script>`, tokenizes the body, and recurses on each segment.
    Returns the wrapped runner name if the script ultimately invokes one.
    Bounded recursion handles `scripts.a = "npm run b"` chains; no
    package.json or no matching script falls through to None.
    """
    detection = _detect_runner_at_depth(tokens, 0)
    return detection.runner if detection is not None else None


def _detect_runner_at_depth(tokens, depth):
    residual = strip_prefixes(tokens)
    if not residual:
        return None
    if detect_pytest(residual):
        return Detection(runner=PYTEST, source=DETECTION_SOURCE_ARGV)
    if detect_vitest(residual):
        return Detection(runner=VITEST, source=DETECTION_SOURCE_ARGV)
    if detect_jest(residual):
        return Detection(runner=JEST, source=DETECTION_SOURCE_ARGV)
    if detect_cargo_test(residual):
        return Detection(runner=CARGO_TEST, source=DETECTION_SOURCE_ARGV)
    if detect_tsc(residual):
        return Detection(runner=TSC, source=DETECTION_SOURCE_ARGV)
    if detect_go_test(residual):
        return Detection(runner=GOTEST, source=DETECTION_SOURCE_ARGV)
    if detect_xcodebuild_test(residual):
        return Detection(runner=XCODEBUILD_TEST, source=DETECTION_SOURCE_ARGV)
    return detect_package_run_wrapping(residual, depth)


def detect_package_run_wrapping(tokens, depth=0):
    """Resolve `<npm|pnpm|yarn|bun> run <script>` via package.json scripts.<name>.

    Returns the inner runner if the resolved script body invokes one, else
    None. `depth` tracks recursion through chained `npm run X -> npm run Y`
    bodies; capped at `_MAX_SCRIPT_RECURSION` to prevent unbounded loops in
    cyclic or deeply-nested script graphs.

    Script-body tokenization uses `shlex` with `punctuation_chars=True` —
    a static, conservative shell approximation, NOT a faithful emulation
    of `sh -c`. Command substitution, shell functions, dynamic env
    expansion, and exotic quoting may diverge. The hook errs on
    fail-open (returns None) for ambiguous shapes.

    Conservative behaviors:
    - When any directory-redirect flag (`--prefix`, `-C`, `--cwd`,
      `--filter`, etc.) is present in the pkg-mgr arg block, we fail-open.
      Resolving the wrong package.json would either over-block or
      under-block; deferring to argv-based behavior is the honest call.
    - Forwarded args after `--` in `<pkg-mgr> run <name> -- <args>` are
      appended to the LAST segment of the resolved body, matching
      `sh -c "<body> <args>"` semantics (forwarded args attach to the
      tail of the script body, not every segment).
    """
    if depth >= _MAX_SCRIPT_RECURSION:
        return None
    if not tokens:
        return None
    head = basename(tokens[0])
    value_flags = PACKAGE_RUN_HEADS.get(head)
    if value_flags is None:
        return None
    args = tokens[1:]
    # Bail if any directory-redirect / workspace-filter flag is present;
    # we don't know which package.json the user is targeting.
    for tok in args:
        flag = tok.split("=", 1)[0] if "=" in tok else tok
        if flag in _DIRECTORY_REDIRECT_FLAGS:
            return None
    offset = skip_wrapper_flags(args, value_flags)
    if offset >= len(args):
        return None
    subcommand = args[offset]
    if subcommand not in _PACKAGE_RUN_SUBCOMMANDS.get(head, ()):
        return None
    script_args = args[offset + 1:]
    # Skip any `-*` flags between `run` and the script name
    # (`npm run --if-present typecheck`, `npm run --silent typecheck`,
    # yarn berry `yarn run -T typecheck`).
    i = 0
    while i < len(script_args) and script_args[i].startswith("-") and script_args[i] != "--":
        i += 1
    if i >= len(script_args) or script_args[i] == "--":
        return None
    script_name = script_args[i]
    body = _get_package_script_body(script_name)
    if body is None:
        return None
    # Forwarded args after `--` get appended to the resolved body's last
    # segment (npm semantics: `sh -c "<body> <forwarded>"`).
    forwarded = []
    if "--" in script_args[i + 1:]:
        dash_dash_pos = script_args.index("--", i + 1)
        forwarded = script_args[dash_dash_pos + 1:]
    try:
        body_tokens = tokenize(body)
    except ValueError:
        return None
    if not body_tokens:
        return None
    body_segments = split_segments(filter_grouping(body_tokens))
    if forwarded and body_segments:
        body_segments[-1] = body_segments[-1] + forwarded
    for segment in body_segments:
        inner = _detect_runner_at_depth(segment, depth + 1)
        if inner is not None:
            # Re-tag as package_script regardless of the inner detection's
            # source (the inner may itself have resolved through a chained
            # `scripts.a = "npm run b"`). The OUTER call is what the user
            # invoked; `resolved_argv` carries the concrete inner segment
            # for future package-script support.
            return Detection(
                runner=inner.runner,
                source=DETECTION_SOURCE_PACKAGE_SCRIPT,
                resolved_argv=segment,
            )
    return None


def is_non_executing_invocation(runner, tokens):
    """True if this segment invokes `runner` in a mode that runs no tests.

    This is an ENFORCEMENT-POLICY exemption applied *after* detection, not a
    change to detector semantics. The detectors still mirror their
    `src/cli.rs` counterparts shape-for-shape (see the module docstring):
    `pytest --collect-only` is still a pytest invocation and still reports as
    one in telemetry. What changes is only whether the hook denies it. Keeping
    the exemption out of the detectors is what lets the mirroring doctrine
    survive — the Rust CLI would happily wrap these; the hook simply has no
    reason to insist.

    Matching is exact and case-sensitive against the runner's own
    NON_EXECUTING_FLAGS set (see that table for the safety argument), over the
    segment's residual so a `failtrace`/`env`/`time` prefix cannot hide a
    flag. Values of value-taking flags are not distinguished from flags — a
    contrived `pytest -k --version` would be exempted — which is accepted:
    every such shape is nonsensical, and the failure mode is a missed denial,
    not a wrong one.
    """
    flags = NON_EXECUTING_FLAGS.get(runner)
    if not flags:
        return False
    return any(token in flags for token in strip_prefixes(tokens))


def _normalize_path_token(token):
    """Lexically expand `~` and normalize a path. Comparison only, no I/O.

    Deliberately not `resolve()`: path *comparison* stays a pure string
    operation, so it costs nothing on the hot path and does not change
    meaning based on how the shim directory happens to be symlinked. Whether a
    given shim actually exists is a separate question, asked separately and
    only where the filesystem is genuinely what decides — see
    `_codex_shim_exists`.
    """
    return os.path.normpath(os.path.expanduser(token))


def _expand_home_prefix(entry):
    """Expand a leading `$HOME` / `${HOME}` in a PATH entry.

    shlex leaves shell variables literal and `expanduser` only understands
    `~`, so `PATH="$HOME/.failtrace/codex-shims:$PATH"` — the spelling a shell
    profile or an agent writes by reflex — compared as the literal string
    `$HOME/...`, missed the shim dir, and the hook denied a genuinely wrapped
    invocation.

    Exactly these two spellings, only at the start of the entry. This is NOT a
    shell-variable expander: any other variable is left alone, so the entry
    simply fails to match and enforcement is unchanged. `expanduser("~")`
    reads $HOME at call time, which is also what the shell would have done.
    """
    for name in ("$HOME", "${HOME}"):
        if entry == name:
            return os.path.expanduser("~")
        if entry.startswith(name + "/"):
            return os.path.expanduser("~") + entry[len(name):]
    return entry


def _is_codex_shim_path(token):
    """True if `token` is a path whose parent directory is the shim dir.

    Purely lexical, and deliberately so: an explicitly typed shim path is
    unambiguous intent. Unlike the PATH heuristic below it asks nothing of the
    filesystem — the head is the binary the shell will exec, with no
    resolution step that could pick a different one.
    """
    if "/" not in token:
        return False
    shim_dir = _normalize_path_token(str(CODEX_SHIM_DIR))
    return os.path.dirname(_normalize_path_token(token)) == shim_dir


def _codex_shim_exists(head):
    """True if the shim directory holds a runnable entry named `head`.

    `isfile` follows symlinks, which is what `install-codex-shims.sh` creates,
    so a dangling symlink correctly reads as absent — the shell could not run
    it either. Callers guarantee `head` is a bare command name.
    """
    try:
        shim_dir = _normalize_path_token(str(CODEX_SHIM_DIR))
        return os.path.isfile(os.path.join(shim_dir, head))
    except OSError:
        return False


def _prepends_codex_shim_dir(prefix_tokens, head):
    """True if this segment's PATH assignment really routes `head` to a shim.

    Three conditions, mirroring how the shell actually resolves the command:

      1. The LAST `PATH=` assignment wins. `PATH=<shim>:$PATH PATH=/usr/bin
         pytest` runs /usr/bin/pytest, so an earlier qualifying assignment
         must not carry the segment.
      2. Its FIRST entry is the shim dir. Appending
         (`PATH="$PATH:<shim>"`) leaves the real binary winning, so
         enforcement there is unchanged.
      3. `<shim_dir>/<head>` exists. With the directory first but no shim for
         this command — a partial install, a deleted symlink, a hand-written
         prefix — the shell falls through to the later PATH entries and runs
         the real binary bare. Trusting the lexical match alone would be a
         silent enforcement hole.

    Condition 3 is a filesystem check. It is gated
    behind 1 and 2, which match on approximately no real traffic, so the hook
    still does zero I/O on essentially every Bash call — and here the
    filesystem is precisely what decides which binary runs, so a lexical
    answer would be a guess.
    """
    last_assignment = None
    for token in prefix_tokens:
        if token.startswith("PATH="):
            last_assignment = token
    if last_assignment is None:
        return False
    value = last_assignment[len("PATH="):]
    first_entry = _expand_home_prefix(value.split(os.pathsep, 1)[0])
    if not first_entry:
        return False
    if _normalize_path_token(first_entry) != _normalize_path_token(str(CODEX_SHIM_DIR)):
        return False
    return _codex_shim_exists(head)


def is_wrapped_by_failtrace(tokens):
    """True if this segment routes through failtrace, by either known path.

    1. The bare or absolute head form: `failtrace run ...` after env/modifiers.
    2. failtrace's own Codex PATH shims — either invoked by path
       (`~/.failtrace/codex-shims/xcodebuild test ...`) or reached through a
       `PATH=<shim-dir>:$PATH` prefix on this segment. The shims dispatch
       through failtrace, so the invocation IS wrapped; denying it was the
       worst false positive class for adoption.

    Shell scopes env assignments to the simple command they prefix, so the
    PATH check is naturally segment-scoped: `PATH=<shim> a && b` wraps `a`
    only. A separate `export PATH=...; runner` statement is not recognized —
    that would need cross-segment state and has no observed traffic.

    The two shim forms are judged differently on purpose. An explicit head
    path is taken at face value — the shell execs exactly that file, so the
    check stays lexical. The PATH form is a claim about *resolution*, so it is
    held to what the shell would really do: last assignment wins, the shim dir
    must be first, and the shim for this head must exist
    (`_prepends_codex_shim_dir`). It also applies only when the head is a bare
    command name — a head containing a path separator (`/usr/bin/pytest`) is
    executed directly, bypassing PATH entirely, so a shim-first prefix wraps
    nothing there.
    """
    residual = strip_prefixes(tokens)
    if (len(residual) >= 2 and residual[1] == "run"
            and (residual[0] == "failtrace"
                 or (os.path.isabs(residual[0])
                     and (basename(residual[0]) == "failtrace"
                          or residual[0] == os.environ.get("FAILTRACE_BIN"))))):
        return True
    if not residual:
        return False
    if _is_codex_shim_path(residual[0]):
        return True
    if "/" in residual[0]:
        return False
    prefix_tokens = tokens[:len(tokens) - len(residual)]
    return _prepends_codex_shim_dir(prefix_tokens, residual[0])


def is_grouping_token(t):
    return bool(t) and all(c in _GROUPING_CHARS for c in t)


def filter_grouping(tokens):
    return [t for t in tokens if not is_grouping_token(t)]


def split_segments(tokens):
    """Split a token stream on shell separators."""
    segments = []
    current = []
    for tok in tokens:
        if tok in SEGMENT_SEPS:
            if current:
                segments.append(current)
                current = []
        else:
            current.append(tok)
    if current:
        segments.append(current)
    return segments


def tokenize(cmd):
    """Tokenize with punctuation_chars so `;`, `&&`, etc. split correctly."""
    lex = shlex.shlex(cmd, posix=True, punctuation_chars=True)
    lex.whitespace_split = True
    return list(lex)


def check_command(cmd):
    """Return (detection, segment_tokens) for the first enforceable segment,
    or (None, None) when no segment carries an unwrapped runner.

    Segments are scanned in order and skipped when they are wrapped (literal
    `failtrace run`, or one of failtrace's Codex shims — see
    `is_wrapped_by_failtrace`). A segment whose runner is invoked in a
    non-executing mode (`pytest --collect-only`, `cargo test --no-run`, …) is
    also skipped for enforcement, but the first such detection is remembered
    and returned with `non_executing=True` if no enforceable segment turns up.
    That ordering is what keeps the exemption segment-scoped: a
    `--collect-only` in one segment cannot exempt a bare runner in another,
    because the bare one still wins the scan.

    What to enforce remains `main()`'s call, on two Detection fields:
    `source` (argv → deny, package_script → pass with telemetry) and
    `non_executing` (pass with telemetry). `check_command` still reports what
    was detected regardless of either, so callers that only need a runner name
    can keep reading `detection.runner`.

    Returns (None, None) on parse failure (unparseable shell) so the hook
    falls through to allow.
    """
    try:
        tokens = tokenize(cmd)
    except ValueError:
        # Unparseable shell — pass through rather than blocking.
        return None, None

    tokens = filter_grouping(tokens)
    exempt = None
    for seg in split_segments(tokens):
        detection = detect(seg)
        if detection is None or is_wrapped_by_failtrace(seg):
            continue
        # Package-script detections carry the OUTER `npm run ...` tokens, so
        # scanning them for a runner's flags would be a category error — and a
        # stray hit would reclassify the row out of the package-script
        # telemetry stream. Argv detections only.
        if (detection.source == DETECTION_SOURCE_ARGV
                and is_non_executing_invocation(detection.runner, seg)):
            if exempt is None:
                exempt = (detection._replace(non_executing=True), seg)
            continue
        return detection, seg
    if exempt is not None:
        return exempt
    return None, None


# Shell redirection operators. shlex with `punctuation_chars` groups runs of
# `< > &` into single tokens; `>|` (force-clobber) additionally carries a `|`
# and must be admitted explicitly rather than by broadening the character set
# (`|` alone is a pipe / SEGMENT_SEP).
_REDIRECTION_CHARS = {"<", ">", "&"}
_EXTRA_REDIRECTION_OPERATORS = {">|"}
# Descriptor-duplication operators — only these consume a preceding 1-2 digit
# file descriptor (`2>&1`, `1>&2`). `&>` / `&>>` redirect both streams and do
# NOT consume a preceding FD, so they are deliberately excluded.
_FD_DUPLICATION_OPERATORS = {">&", "<&"}


def _is_redirection_operator(token):
    """True if `token` is a shell redirection operator.

    shlex with `punctuation_chars` groups runs of `< > &` into single
    tokens, so most redirection operators are tokens made entirely of
    those characters containing at least one of `<` or `>`. `>|` is the
    one exception — it carries a `|` — and is admitted explicitly. Bare
    `&`, `&&`, `||`, `|`, `|&` are SEGMENT_SEPS and never reach a single
    segment.
    """
    if token in _EXTRA_REDIRECTION_OPERATORS:
        return True
    return bool(token) and set(token) <= _REDIRECTION_CHARS and (
        "<" in token or ">" in token
    )


def strip_redirections(tokens):
    """Drop shell redirection clauses from a denied segment's tokens.

    `build_suggested_command` must not echo redirections into the
    `failtrace run -- ...` suggestion: a `2>&1` would merge failtrace's
    stderr breadcrumb into its JSON stdout (the failtrace skill bans
    exactly that), and `> file` is redundant once failtrace persists
    every run under its run-dir.

    Each redirection clause — the operator, its following operand
    (target), and, for descriptor-duplication operators (`>&` / `<&`, as
    in `2>&1` / `1>&2`), a preceding 1-2 digit file descriptor — is
    excised wherever it appears, so non-redirection arguments that follow
    a redirection survive (`pytest > out.log -k smoke` keeps `-k smoke`).
    `&>` / `&>>` / `>|` / plain `>` keep the preceding token: it is
    almost always a real argument (`pytest -n 2 > out`), not an FD, and
    the two are indistinguishable once shlex has discarded whitespace.

    Known imperfections (pinned by explicit tests so they are not
    mistaken for regressions): `pytest 2>/dev/null` leaves a dangling
    `2`, and `pytest -n 2 >&2` mis-drops the `-n` argument — both are the
    same irreducible `<arg>` vs `<fd>` ambiguity.
    """
    result = []
    i = 0
    while i < len(tokens):
        token = tokens[i]
        if _is_redirection_operator(token):
            # Excise the clause: operator + its operand, plus a preceding
            # FD digit already collected for a descriptor-duplication op.
            if (
                token in _FD_DUPLICATION_OPERATORS
                and result
                and result[-1].isdigit()
                and len(result[-1]) <= 2
            ):
                result.pop()
            i += 2
            continue
        result.append(token)
        i += 1
    return result


def _is_secret_flag(token):
    """True if `token` looks like a flag whose value should not be logged."""
    name = token.lstrip("-").lower()
    return any(hint in name for hint in SECRET_FLAG_HINTS)


def redact_tokens(tokens):
    """Return `tokens` with probable secret values replaced by REDACTED.

    A deliberately dumb token filter, not a secrets-detection engine. Three
    rules, applied left to right:

      1. `KEY=value` (ENV_VAR shape) -> `KEY=<redacted>`. `strip_prefixes`
         already peels *leading* env assignments off, so the assignments
         reaching here are mid-command (`make DEPLOY_TOKEN=t`,
         `docker run -e API_KEY=k`) and would otherwise survive verbatim —
         this is not double-handling. The key name is kept; the value goes.
      2. `--flag=value` whose flag name matches SECRET_FLAG_HINTS ->
         `--flag=<redacted>`.
      3. A bare `--flag` matching those hints -> the FOLLOWING token is
         replaced, covering the `--flag value` pair form. The next token is
         redacted unconditionally, so `--token --verbose` loses the second
         flag name; over-redaction is preferred to leaking a value that
         happens to start with `-`.

    Deliberately NOT covered, because the raw command is never written and
    these only affect the normalized view: positional secrets with no flag
    or key (`curl https://user:pw@host`), glued short-flag forms
    (`mysql -psecret`), heredocs, and secrets nested inside a quoted
    subcommand string.
    """
    out = []
    redact_next = False
    for token in tokens:
        if redact_next:
            out.append(REDACTED)
            redact_next = False
            continue
        if ENV_VAR.match(token):
            out.append(token.split("=", 1)[0] + "=" + REDACTED)
            continue
        if token.startswith("-"):
            flag, sep, _value = token.partition("=")
            if sep and _is_secret_flag(flag):
                out.append(flag + "=" + REDACTED)
                continue
            if not sep and _is_secret_flag(token):
                redact_next = True
        out.append(token)
    return out


def build_suggested_command(segment_tokens, redact=False):
    """Return `failtrace run -- <residual>` for a denied segment, or None.

    The only caller is the agent-soak telemetry's `suggested_command` schema
    field, which passes `redact=True`; the hook's own stderr denial message
    includes a validated wrapper path but does not echo the command.
    `redact` defaults to False so a
    future user-facing caller gets the real, runnable command.

    Trailing shell redirections are stripped (see `strip_redirections`) so
    the suggestion never reintroduces a `2>&1` the failtrace skill bans.
    """
    if not segment_tokens:
        return None
    seg_residual = strip_redirections(strip_prefixes(segment_tokens))
    if not seg_residual:
        return None
    if redact:
        seg_residual = redact_tokens(seg_residual)
    return "failtrace run -- " + " ".join(seg_residual)


def _log_soak_generic(agent, log_filename, payload, cmd, decision, reason,
                      runner=None, segment_tokens=None, detection_source=None):
    """Append a JSONL entry to SOAK_DIR/log_filename, if opted in.

    Agent-agnostic writer used by the Claude Code hook (`_log_soak` wrapper). The
    PATH-shim dispatcher (`scripts/codex-shim-dispatcher.py`) writes
    `codex-shims.jsonl` directly with a compatible (not identical) schema
    — it adds `shim`, `real_path`, and `failtrace_path` fields specific
    to PATH-interception decisions.

    Two gates, both of which must pass before anything is written:

      1. `SOAK_ENV_VAR` is exactly "1" in the environment. Read per call,
         not snapshotted at import. Directory existence is NOT a gate
         (see the SOAK_DIR comment block); the writer mkdirs the directory
         itself.
      2. `(decision, reason)` is in SOAK_LOGGED_EVENTS: denials, deferred
         package-script detections, and non-executing-invocation exemptions
         only.

    Schema (per agent-soak-telemetry design — see ~/.failtrace/agent-soak/):
        ts, agent, event, session_id, tool_use_id, cwd, command,
        normalized_command, decision, reason, suggested_command, runner,
        detection_source

    `command` is always null. The key is retained so existing mining of the
    file keeps parsing, but the raw command is never written: it is the one
    field that cannot be redacted without destroying it. `normalized_command`
    and `suggested_command` carry the shape instead, both passed through
    `redact_tokens` first.

    `detection_source` distinguishes argv-source detections from
    package_script-source ones (which produce `pass` decisions with reason
    `package_script_runner_detected_but_cli_unsupported` until the Rust CLI
    supports package scripts). Argv-source rows split further by reason: `bare_runner_invocation`
    for denials, `non_executing_runner_invocation` for the exemption family
    (`pytest --collect-only`, `cargo test --no-run`, …) — logged so future
    mining can see how often it fires.

    Decisions: pass | deny | skip. `rewrite` is reserved for a future
    updatedInput-based rewriter and not currently emitted. Only the three
    allowlisted (decision, reason) pairs reach disk.

    The log file is opened with mode 0o600 and re-chmodded on every write.
    0o600 keeps the file owner-only on shared dev/CI hosts even if a prior
    version created it with looser perms — and redaction is best-effort, so
    the file is still treated as sensitive.

    All exceptions are swallowed: telemetry must never block the hook.
    """
    try:
        if os.environ.get(SOAK_ENV_VAR) != "1":
            return
        if (decision, reason) not in SOAK_LOGGED_EVENTS:
            return
        normalized = ""
        if cmd:
            try:
                normalized = " ".join(
                    redact_tokens(strip_prefixes(filter_grouping(tokenize(cmd))))
                )
            except ValueError:
                # Never fall back to the raw command — that is the content
                # this contract exists to keep off disk.
                normalized = "<tokenize_error>"
        suggested = (build_suggested_command(segment_tokens, redact=True)
                     if decision == "deny" else None)
        SOAK_DIR.mkdir(parents=True, exist_ok=True, mode=0o700)

        get = payload.get if isinstance(payload, dict) else (lambda _k: None)
        entry = {
            "ts": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "agent": agent,
            "event": "PreToolUse",
            "session_id": get("session_id"),
            "tool_use_id": get("tool_use_id"),
            "cwd": get("cwd"),
            "command": None,
            "normalized_command": normalized,
            "decision": decision,
            "reason": reason,
            "suggested_command": suggested,
            "runner": runner,
            "detection_source": detection_source,
        }
        log_file = SOAK_DIR / log_filename
        fd = os.open(log_file, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "a", encoding="utf-8") as f:
            f.write(json.dumps(entry, ensure_ascii=False) + "\n")
    except Exception:
        pass


def _log_soak(payload, cmd, decision, reason, runner=None, segment_tokens=None,
              detection_source=None):
    """Claude Code-side wrapper. See _log_soak_generic for schema and semantics."""
    _log_soak_generic("cc", "cc-hooks.jsonl", payload, cmd, decision, reason,
                      runner=runner, segment_tokens=segment_tokens,
                      detection_source=detection_source)


def _main():
    raw = sys.stdin.read()
    try:
        payload = json.loads(raw)
    except (json.JSONDecodeError, ValueError):
        _log_soak({}, "", "skip", "invalid_payload_json")
        return 0

    cmd = (payload.get("tool_input") or {}).get("command", "") or ""
    if not cmd:
        _log_soak(payload, cmd, "skip", "empty_command")
        return 0

    detection, seg = check_command(cmd)
    if detection is None:
        _log_soak(payload, cmd, "pass", "no_runner_detected")
        return 0

    # Package-script detections pass with telemetry instead of denying. The Rust CLI's argv-based detectors
    # reject `<pkg-mgr> run X` shapes (see src/cli.rs:973 and the
    # `npm_test_with_single_recognized_runner_in_scripts_test_stays_ambiguous`
    # test at src/cli.rs:2610), so suggesting `failtrace run -- npm run X`
    # would land in AdapterUnavailable. The detection still records in
    # telemetry; the CLI does not resolve package scripts yet.
    if detection.source == DETECTION_SOURCE_PACKAGE_SCRIPT:
        _log_soak(payload, cmd, "pass",
                  "package_script_runner_detected_but_cli_unsupported",
                  runner=detection.runner, segment_tokens=seg,
                  detection_source=detection.source)
        return 0

    # Non-executing invocation (`pytest --collect-only`, `cargo test --no-run`,
    # `tsc --version`, …). The runner was genuinely detected — detection
    # semantics still mirror src/cli.rs — but no tests will run, so there is
    # nothing for `failtrace run` to capture and denying only costs the agent a
    # workaround.
    if detection.non_executing:
        _log_soak(payload, cmd, "pass", "non_executing_runner_invocation",
                  runner=detection.runner, segment_tokens=seg,
                  detection_source=detection.source)
        return 0

    failtrace_path, reason = resolve_failtrace()
    if failtrace_path is None:
        sys.stderr.write(f"failtrace-enforce: {reason}; allowing original command unwrapped\n")
        return 0

    _log_soak(payload, cmd, "deny", "bare_runner_invocation",
              runner=detection.runner, segment_tokens=seg,
              detection_source=detection.source)
    sys.stderr.write(
        f"Bare {detection.runner} invocation blocked by failtrace-enforce hook. "
        f"Wrap with {shlex.quote(failtrace_path)} run -- ... (see the failtrace skill).\n"
    )
    return 2


def main():
    try:
        return _main()
    except Exception:
        sys.stderr.write("failtrace-enforce: hook_error; allowing original command unwrapped\n")
        return 0


if __name__ == "__main__":
    sys.exit(main())
