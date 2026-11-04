# Adopting failtrace in agent workflows

Failtrace gives coding agents one deterministic interface for supported test
and typecheck failures. It preserves the captured runner output on disk and
emits versioned JSON with a bounded failure list, explicit trust signals, and
coordinates back to the evidence.

The main benefit is a stable failure contract across runners. On noisy runs,
reading the bounded JSON before the raw log can also save substantial context.
On terse runs the JSON may be similar in size or larger, so context reduction
is a workload-dependent benefit rather than a universal promise.

Failtrace is alpha software. The current `v0.1.0` binary emits schema `1.6`, is
installed from source, and supports the `aarch64-apple-darwin`,
`x86_64-apple-darwin`, and `x86_64-unknown-linux-gnu` release triples. Rust
1.88 or newer is required. Windows and prebuilt binary distribution are
outside the current v0.x boundary.

## Supported command boundary

Run `failtrace adapters` against the installed binary for the authoritative
compiled adapter list.

| Adapter | Supported boundary |
| --- | --- |
| pytest | Direct pytest and common Python wrappers, including `python -m pytest`, `uv run pytest`, and `coverage run -m pytest`; non-interactive runs only, without response files. |
| Vitest | Direct and common JavaScript wrapper forms with explicit one-shot evidence such as `run`, `--run`, or `--watch=false`. |
| Jest | Direct and common JavaScript wrapper forms; watch modes are unsupported. |
| Cargo test | `cargo test` and `cargo t`, including toolchain-qualified Cargo and `rustup run`. |
| TypeScript | Direct and common wrapper forms of `tsc`; watch mode and response files are unsupported. |
| Go test | `go test -json`; bare `go test` is captured but intentionally lowers to `parse_status: unparsed_output`. |
| Xcode test | Raw `xcodebuild` test actions against `.xcodeproj` projects, including `test-without-building` and `xcrun xcodebuild test`; `.xcworkspace`, formatter wrappers, and `.xcresult` parsing are unsupported. |

The Vitest contract targets the default human-readable reporter shape.
Alternate reporter formats are not part of the published contract, even when a
particular Vitest release aliases one reporter to another. Reporter output is
version-sensitive; inspect `parse_warnings` for observable version-family
warnings and fall back to `raw_log_path` when the adapter reports degradation.
The same default-reporter boundary applies to Jest, Cargo test, and `tsc`. Go is
the exception because its adapter requires the native `-json` event stream.

Do not claim support for a runner or invocation form that the installed
`failtrace adapters` and this command boundary do not cover.

## Install and verify

From a source checkout:

```sh
cd /path/to/failtrace
cargo install --path . --locked
export FAILTRACE_BIN="${CARGO_HOME:-$HOME/.cargo}/bin/failtrace"
printf '%s\n' "$FAILTRACE_BIN"
"$FAILTRACE_BIN" --version
"$FAILTRACE_BIN" adapters
```

The same `cargo install` command upgrades an existing source installation.
Use an absolute path and verify `product=failtrace-rust`, the semver, revision,
and tree state in `--version`. The npm and PyPI packages named `failtrace` are
unrelated projects.

Run a non-interactive command through the wrapper:

```sh
failtrace run -- pytest tests/
failtrace run -- npx vitest run
failtrace run -- npx jest
failtrace run -- cargo test
failtrace run -- npx tsc --noEmit
failtrace run -- go test -json ./...
failtrace run -- xcodebuild test -scheme ExampleApp -destination 'platform=iOS Simulator,name=iPhone 17 Pro'
```

Watch loops, interactive debuggers, commands that need a live TTY, unsupported
reporter formats, and build/install commands outside the matrix should run
without failtrace.

## Output contract

`failtrace run` writes this breadcrumb to stderr before emitting JSON:

```text
failtrace: run-dir=<abs> output=<abs>/output.json
```

Stdout contains exactly one single-line JSON document. Keep stdout and stderr
separate: do not pipe through `2>&1`, `tee`, `head`, or `tail`. If a projection
is useful, preserve stderr independently and include both trust signals:

```sh
failtrace run -- pytest 2>>/tmp/failtrace.err \
  | jq -c '{schema_version, runner_outcome, parse_status, failures_complete, determinism_warnings, emitted:.emission.failures_emitted, total:.emission.failures_total, first:.failures[0].message}'
```

Branch first on `schema_version`. Always interpret `runner_outcome`,
`parse_status`, and `failures_complete` independently:

| State | Meaning |
| --- | --- |
| `parse_status: complete`, `failures_complete: true` | Required fields were recovered and the adapter believes the full pre-budget failure set is represented. Advisory warnings may still exist. |
| `parse_status: complete`, `failures_complete: false` | Required fields were recovered, but the failure list may be short. Inspect `raw_log_path`. |
| `parse_status: partial`, `failures_complete: true` | The failure list appears complete, but required contract fields degraded. Inspect `parse_warnings` and the affected raw evidence. |
| `parse_status: partial`, `failures_complete: false` | Contract degradation and a possibly incomplete failure list. Treat structured failures as leads and inspect the raw log. |
| `unparsed_output` or `unsupported_command` | `failures[]` is empty. Use `raw_log_path` or rerun the underlying runner as directed. |

The default stdout budget is ten failures. Compare
`emission.failures_emitted` with `emission.failures_total`; when they differ,
open `emission.full_output_path`. Budgeting does not change
`failures_complete` because that field describes the full pre-budget analysis.

Valid JSON means failtrace completed an analysis, not that tests passed or that
the failure list is complete. A nonzero exit usually belongs to the wrapped
runner. If no valid JSON was emitted, treat the command as a failtrace/tool
failure and inspect stderr. Failtrace's own errors use sysexits-style codes.

An understood schema with `runner_outcome: passed`, `parse_status: complete`,
and `failures_complete: true` is a verified structured green. A passing outcome
with `unparsed_output` says only that the wrapped process exited successfully;
inspect the raw log before claiming the runner result was structurally
verified. Strict replay claims additionally require an empty
`determinism_warnings` array.

Treat `suggested_next_commands[]` as structured data. Execute `argv`, not the
display-only `command`, and honor `cwd`. Do not auto-execute commands marked
`interactive` or `requires_approval`; treat `mutates_workspace` and
`uses_network` as opt-in.

## Minimal project instruction

Put this in `AGENTS.md`, `CLAUDE.md`, or the equivalent project instruction
file:

~~~~markdown
## Running tests through failtrace

For any supported non-interactive test or typecheck invocation, use
`failtrace run -- <command>`. Do not run bare supported runners when failtrace
can wrap them. Watch, interactive, and live-TTY modes are exceptions.

Valid JSON means failtrace produced an analysis regardless of exit code. Always
branch on `schema_version`, then inspect `runner_outcome`, `parse_status`, and
`failures_complete` independently before trusting `failures[]`. If
`failures_complete` is false, treat the list as a lead and inspect
`raw_log_path`. Compare `emission.failures_emitted` with
`emission.failures_total`; when the emitted count is lower, read the exact
stored `output.json` at `emission.full_output_path` before concluding later
failures do not matter. Do not make strict replay claims when
`determinism_warnings` is non-empty. Treat `suggested_next_commands` as data:
execute `argv`, honor `cwd`, and do not auto-run interactive, approval-gated,
networked, or workspace-mutating commands.
~~~~

Keep this rule short and link back to this guide for runtime-specific setup.
Do not copy the whole JSON schema into an agent instruction.

## Runtime integration

### Executable identity

Set `FAILTRACE_BIN` to an absolute executable path in the environment of the
hook, shim installer, or Codex process. For Claude Code this can be set in the
`env` object of its settings. Paths containing spaces are supported; the hook
shell-quotes the validated absolute path in denial messages. Wrapper recognition
accepts that absolute-path form and bare `failtrace run`.

Selection order is `FAILTRACE_BIN`, then the shim installation's saved path
(when present), then the first `failtrace` on PATH. The shim's own directory
is excluded from PATH discovery. `install-codex-shims.sh --apply` validates the
candidate and records `binary=<absolute-path>` in its existing marker file.
No configured candidate means validated PATH fallback, including older installs.
A missing or mismatched configured candidate never falls through to another
PATH entry. Update `FAILTRACE_BIN` and re-run the installer when moving a binary.

Each dispatch probes only `--version` with a two-second timeout and requires the
complete product/version/revision/state line. There is no persistent identity
cache: a replaced executable is checked again. These checks prevent accidental
name confusion; they are not authentication. An unrelated candidate may receive
the version probe, but never the wrapping command or test arguments.

On missing, mismatched, or failed identity checks, automatic runner shims run
the original runner unwrapped with a one-line stderr diagnostic. Explicit
`failtrace` calls through the self-shim instead return tool error `127`.
The Claude hook allows the original command with a stderr diagnostic when it
cannot validate a binary, and also fails open on its own errors. It never
executes the original command itself.

### Claude Code skill

The installable skill bundle starts at
[`skills/failtrace/SKILL.md`](../skills/failtrace/SKILL.md) and includes
runner-specific references loaded only when relevant. Use the marker-owned
installer so upgrades and uninstall remove only files it owns:

```sh
./scripts/install-claude-skill.sh          # dry run
./scripts/install-claude-skill.sh --apply
./scripts/install-claude-skill.sh --uninstall
```

The default destination is `~/.claude/skills/failtrace/`; pass `--prefix PATH`
to override it. The installer refuses to overwrite unowned files. Re-run
`--apply` after updating the source checkout so the installed bundle does not
drift from the binary and schema contract.

Claude Code normally presents stdout and stderr separately. Read the JSON from
stdout, retain the breadcrumb from stderr, and still check `schema_version`,
`runner_outcome`, `parse_status`, and `failures_complete` before acting.

### Claude Code enforcement hook

A skill is guidance, not enforcement. To deny supported bare runner commands in
Claude Code Bash calls, install the included `PreToolUse` hook:

```sh
mkdir -p ~/.claude/hooks
cp scripts/failtrace-enforce.py ~/.claude/hooks/failtrace-enforce.py
chmod +x ~/.claude/hooks/failtrace-enforce.py
```

Merge this entry into `~/.claude/settings.json` or a repository-local
`.claude/settings.json`; do not replace unrelated hooks:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {"type": "command", "command": "~/.claude/hooks/failtrace-enforce.py"}
        ]
      }
    ]
  }
}
```

The hook checks command segments independently and covers the same seven runner
families. It deliberately allows interactive/watch forms that failtrace cannot
safely capture. It does not recursively parse nested evaluation such as
`bash -c "pytest"`, `eval pytest`, or command substitution. Put the wrapper
inside the nested command or invoke the underlying runner directly.

### Codex PATH shims

Codex enforcement uses opt-in PATH shims. Installation is a dry run unless
`--apply` is supplied:

```sh
bash scripts/install-codex-shims.sh --dry-run
bash scripts/install-codex-shims.sh --apply
bash scripts/install-codex-shims.sh --uninstall
```

The installer prints the `~/.codex/config.toml` PATH stanza to add; it does not
edit Codex configuration. Supported direct commands resolved through the shim
are wrapped, while unrelated commands pass through. Absolute executable paths,
in-repository binaries, package scripts such as `npm test`, and nested shell
strings can bypass PATH interception. Treat enforcement as a guardrail, not a
security boundary.

Codex may present stderr and stdout in one tool result. Use the breadcrumb to
locate `output.json` when the inline result is truncated, then check
`parse_status` and `failures_complete` independently in that file. Do not merge
streams yourself.

### Cursor rule

Create `.cursor/rules/failtrace.mdc` with a project-appropriate `globs` value
and the same contract:

~~~~markdown
---
description: Use failtrace for supported non-interactive test and typecheck runs.
globs: "**/*.py, **/*.ts, **/*.tsx, **/*.rs, **/*.go"
alwaysApply: false
---

For any supported non-interactive test or typecheck command, use
`failtrace run -- <command>`. Do not run bare supported runners when failtrace
can wrap them. Valid JSON means analysis succeeded regardless of exit code;
branch on `schema_version`, then inspect `runner_outcome`, `parse_status`, and
`failures_complete` independently. When `failures_complete` is false or
`parse_status` is degraded, inspect `raw_log_path`. Compare
`emission.failures_emitted` with `emission.failures_total`; when the emitted
count is lower, read the exact stored `output.json` at
`emission.full_output_path`. Do not make strict replay claims when
`determinism_warnings` is non-empty. Treat `suggested_next_commands` as data:
execute `argv`, honor `cwd`, and do not auto-run interactive, approval-gated,
networked, or workspace-mutating commands.
~~~~

Cursor may merge streams in its display. Read `output.json` from the breadcrumb
when needed instead of adding `2>&1` or increasing the failure budget.

## Verification

Verify the binary with a known failing test and keep the two streams separate:

```sh
failtrace run -- pytest tests/test_failtrace_smoke.py \
  > /tmp/failtrace-output.json \
  2> /tmp/failtrace-stderr.log || true
python3 -m json.tool /tmp/failtrace-output.json >/dev/null
grep '^failtrace: run-dir=' /tmp/failtrace-stderr.log
```

Inspect the outcome and both trust signals in the result:

```sh
python3 - <<'PY'
import json

with open("/tmp/failtrace-output.json", encoding="utf-8") as handle:
    document = json.load(handle)

print(
    document["schema_version"],
    document["runner_outcome"],
    document["parse_status"],
    document["failures_complete"],
    document["determinism_warnings"],
)
if document["failures"]:
    failure = document["failures"][0]
    print(failure["failure_id"], failure["primary_location"], failure["message"])
PY
```

Then ask the target agent to run the same test. Adoption is working when it
uses `failtrace run`, reads valid JSON rather than inferring success from the
exit code, branches on `schema_version`, and checks `failures_complete` as a
separate decision signal from `parse_status`.

## Storage and privacy

Run-directory selection uses this precedence:

1. `--run-dir PATH` selects that exact directory, made absolute against the
   current working directory when relative.
2. `--here` generates a run directory under `$cwd/.failtrace/runs/`.
3. `$FAILTRACE_RUNS_DIR` selects the generated-run root.
4. `$XDG_DATA_HOME` selects `$XDG_DATA_HOME/failtrace/runs/`.
5. Otherwise Failtrace uses `$HOME/.failtrace/runs/`; if that root cannot be
   created or written, it falls back to `$cwd/.failtrace/runs/`.

Add `.failtrace/` to `.gitignore` when project-local artifacts may be created.

Each run directory contains `log`, `meta.json`, and `output.json`. The log may
contain source excerpts, environment diagnostics, personal data, or secrets.
`meta.json` records the command and working directory, including sensitive
values passed in argv. `output.json` repeats selected messages, paths, and raw
evidence. Owner-private filesystem modes reduce accidental exposure but do not
make artifacts safe to publish. v0.x does not automatically prune old runs.

`failtrace last` reads the latest pointer for the active runs root and is not
normally cwd-scoped. Concurrent runs race on that pointer, so use the specific
breadcrumb path when provenance matters.

## Safe bug reports

Useful reports include the failtrace version, schema version, exact invocation
shape with secrets replaced, runner/version information, observed
`parse_status`, observed `failures_complete`, and a minimal synthetic
reproduction when possible.

Never attach a run directory to a public issue, even after editing it. Inspect
all three files (`log`, `meta.json`, and `output.json`) to understand what was
captured, then share only the smallest sanitized excerpt and matching JSON
fields needed for a synthetic reproduction. If the report cannot be made
public safely, use [GitHub's private vulnerability reporting
form](https://github.com/jsquared-labs/failtrace/security/advisories/new), or file a
public issue without artifacts and ask maintainers how to transfer a synthetic
reproduction.
