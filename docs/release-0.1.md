# Failtrace v0.1 Release Contract

This document defines the first releasable milestone contract. It is user-facing and narrower than the full schema rationale in `docs/schema.md`.

## Command Surface

v0.1 exposes five commands:

| Command | Contract |
|---|---|
| `failtrace run <cmd...>` | Run a command, capture stdout/stderr, persist a run directory, analyze the result, write the full document to `<run-dir>/output.json`, and emit one single-line JSON document to stdout. |
| `failtrace analyze <run-dir>` | Re-analyze an existing failtrace run directory from `log` + `meta.json`, emit one single-line JSON document to stdout, and leave `output.json` unchanged. Supports the same `--max-failures` emission budget as `run`. |
| `failtrace last` | Print the most recent run's `output.json` without re-analysis. |
| `failtrace explain [--run-dir <run-dir>] <failure_id>` | Print the stored failure, its captured evidence/raw-log excerpts, and the run's suggested next commands. Without `--run-dir`, lookup is limited to the latest run in the active runs root; it does not scan older runs. v0.1 does not promise exhaustive adapter-specific rule attribution. |
| `failtrace adapters` | List compiled adapters, adapter versions, representative supported command forms, and declared capabilities. The command list is illustrative rather than an exhaustive detector inventory. |

## Exit Codes

Runner exit codes pass through untouched when a wrapped runner executes and failtrace successfully emits valid JSON.

Failtrace internal failures use sysexits-style codes:

| Code | Meaning |
|---:|---|
| 64 | Command-line misuse or invalid flags. |
| 65 | Requested adapter unavailable. |
| 66 | Stored input data is unusable: the sidecar is corrupted, incomplete, or incompatible, or `explain` cannot find the requested failure id in the selected run. |
| 70 | Internal software error. |
| 74 | I/O error reading or writing logs, sidecars, output, or run directories. |
| 78 | Configuration or environment error. |

The agent discriminator is output shape, not exit code: valid failtrace JSON in stdout means failtrace produced an analysis document. Invalid or absent JSON means the agent should treat the command as a failtrace/tool failure and inspect stderr.

## Stdout And Stderr

`failtrace run` and `failtrace analyze` write a breadcrumb to stderr first:

```text
failtrace: run-dir=<abs> output=<abs>/output.json
```

Then they write exactly one JSON document serialized on a single line to stdout, followed by a mandatory trailing newline. "Single-line" describes the wire format, not a promise that every structured result is smaller than the raw runner log.

Some agent runtimes merge stdout and stderr. The stderr breadcrumb is therefore first and line-oriented so it remains discoverable in either merged- or split-stream environments.

## Failure Emission Budget

The default stdout budget is `--max-failures 10`. `--max-failures all` emits the full document on stdout.

The full document is always written to `<run-dir>/output.json` with `emission.channel: "file"` and `emission.mode: "full"`. Stdout documents carry `emission.channel: "stdout"` and either `emission.mode: "budgeted"` or `emission.mode: "full"` depending on the chosen budget.

## Run Directories

Run-directory resolution uses the first applicable mechanism:

1. `--run-dir <exact>` uses the exact directory path supplied by the caller (made absolute against the cwd when relative).
2. `--here` generates a run directory under `$cwd/.failtrace/runs/`.
3. `FAILTRACE_RUNS_DIR` sets the generated-run root.
4. `XDG_DATA_HOME` sets the root to `$XDG_DATA_HOME/failtrace/runs/`.
5. Otherwise failtrace uses `$HOME/.failtrace/runs/`; if that root cannot be created or written, it silently falls back to `$cwd/.failtrace/runs/`.

The stderr breadcrumb always shows the actual run directory selected.

Each generated run directory contains `log`, `meta.json`, and `output.json`.

## Schema And Binary Versions

The v0.1.0 binary emits schema v1.6. Binary semver and schema versions evolve independently: a later v0.x binary may continue to emit schema v1.6, while any schema change is declared separately in `docs/schema.md`.

Consumers should branch first on `schema_version`, then on `failtrace_version` only for implementation-specific bugs or feature gates.

`failtrace --version` prints one line:

```text
failtrace 0.1.0 (product=failtrace-rust revision=<git-commit-or-unknown> state=<clean-or-dirty-or-unknown>)
```

Identity is embedded at build time, without timestamps, usernames, hostnames,
or local paths. Git is never consulted at runtime or during classification.
The build script rechecks Git on each Cargo build using a deliberately absent
`rerun-if-changed` input. Git resolves linked worktrees, detached HEADs, and
packed refs; staged, unstaged, and non-ignored untracked changes mark the tree
dirty. An archive without its own `.git` metadata reports unknown revision and
state, even inside another repository. Unavailable Git information is explicit
as `unknown`.

Fresh captures store the version line in optional sidecar `capture_build`.
Older sidecars remain unknown. Analysis never rewrites the sidecar or stored
output, and capture identity does not affect emitted JSON. Sidecar format stays
`1.2`, output schema stays `1.6`, and document `failtrace_version` stays `0.1.0`.

## Integration Identity Checks

`FAILTRACE_BIN` selects an absolute executable path. Otherwise shims use the
path validated and saved by `install-codex-shims.sh --apply` in their marker;
without configuration, integrations validate the first PATH candidate.
Validation probes `--version` on every dispatch, with a two-second timeout and
an exact product/version/revision/state format check. It prevents accidental
confusion with the unrelated npm/PyPI projects, not deliberate impersonation.

An invalid configured path never triggers a search for another executable.
Automatic shims pass through to the original runner with a diagnostic when
validation fails; explicit self-shim `failtrace` invocations return `127`.
The Claude hook recommends the validated, shell-quoted absolute path, recognizes
absolute and bare wrapper forms, and allows the original command with a
diagnostic when validation or the hook itself fails. See
[adoption configuration](adoption.md#executable-identity).

## Platform Support

In scope for v0.x:

- macOS arm64
- macOS x86_64
- Linux x86_64

Windows is explicitly out of scope for v0.x.

## Distribution

The supported initial distribution is a source checkout installed with `cargo install --path . --locked` (Rust 1.88+). Prebuilt GitHub Release artifacts and crates.io publication are not part of the v0.1.0 contract.
