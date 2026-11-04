# Vitest

Use for explicit one-shot Vitest runs through supported JavaScript package-runner wrappers.

- Include one-shot evidence such as `run`, `--run`, or `--watch=false`.
- Files run in parallel workers, so `ranking.is_stable_across_runs` is false. Plan against `failure_id` and iterate `failures[]` for multi-failure runs.
- Vitest 4 is the validated major-version family. When the banner exposes an unsupported major, expect `runner_version_unsupported` in `parse_warnings`.
- The published parser contract targets the default human-readable reporter. Alternate reporters are unsupported even when a release aliases one reporter to another.
- Watch and UI modes must run outside Failtrace. Read [command-boundaries.md](command-boundaries.md) for the rejection boundary.
