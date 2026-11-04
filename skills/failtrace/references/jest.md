# Jest

Use for non-watch Jest runs through direct or supported JavaScript package-runner forms.

- Parallel workers make `ranking.is_stable_across_runs` false. Plan against `failure_id`, not a slot.
- Jest 30 is the validated major-version family. The default reporter normally prints no version banner, so an out-of-family version may not emit `runner_version_unsupported`.
- `● <suite> > <test name>` is a per-test failure. `runner_test_id` is populated and the suggested rerun normally uses `jest <file> -t <pattern>`.
- `● Test suite failed to run` is a pre-test transform, import, or setup failure. `runner_test_id` is null and `phase` is `build`, `collect`, or `setup`; fix it before test-level failures in that file.
- The published contract targets the default human-readable reporter. Watch modes and alternate reporters are outside the boundary.
