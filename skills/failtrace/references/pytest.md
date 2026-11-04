# Pytest

Use for direct pytest and supported Python/coverage wrappers, including `python -m pytest`, `uv run pytest`, and `coverage run -m pytest`.

- Wrap green and failing non-interactive runs through Failtrace.
- Pytest runs sequentially by default, so `ranking.is_stable_across_runs` is normally true.
- Pytest-xdist execution, detected from `-n` / `--numprocesses` or xdist log markers, makes ranking unstable. Plan against `failure_id`, not a list slot.
- Pytest 8 and 9 are the validated major-version families. When the banner exposes an unsupported major, expect `runner_version_unsupported` in `parse_warnings`.
- Response files and interactive/live-capture forms are rejected. Read [command-boundaries.md](command-boundaries.md) when the invocation uses `@args.txt`, `-s`, `--capture=no`, `--pdb`, `--trace`, or a watcher.
