#!/usr/bin/env python3
"""Verify recorded hashes in every fixture's expected.json.

Reproduces v1.3's failure_id and test_group_id derivation rules from
docs/schema.md against each fixture's hash_inputs block. Exit 0 iff
every recorded hash matches; nonzero otherwise.

This is the first real test in the project: it locks the contract
between docs/schema.md and tests/fixtures/<adapter>/<scenario>/ before
any adapter code exists. Must keep passing on every CI run.
"""

from __future__ import annotations

import hashlib
import json
import pathlib
import sys


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
FIXTURES_ROOT = REPO_ROOT / "tests" / "fixtures"
EXPECTED_FIXTURE_PATHS = [
    pathlib.Path(path)
    for path in [
        "tests/fixtures/jest/basic_assertion/expected.json",
        "tests/fixtures/jest/build_error/expected.json",
        "tests/fixtures/jest/green/expected.json",
        "tests/fixtures/jest/mixed_pass_fail/expected.json",
        "tests/fixtures/jest/no_test_files/expected.json",
        "tests/fixtures/cargo/borrow_checker_e0502/expected.json",
        "tests/fixtures/cargo/build_script_panic/expected.json",
        "tests/fixtures/cargo/clap_macros_assertion/expected.json",
        "tests/fixtures/cargo/dependency_resolution/expected.json",
        "tests/fixtures/cargo/doctest_failure/expected.json",
        "tests/fixtures/cargo/green_passing/expected.json",
        "tests/fixtures/cargo/runtime_assertion/expected.json",
        "tests/fixtures/cargo/runtime_panic/expected.json",
        "tests/fixtures/cargo/should_panic_mismatch/expected.json",
        "tests/fixtures/cargo/trait_resolution_e0277/expected.json",
        "tests/fixtures/cargo/workspace_multi_binary/expected.json",
        "tests/fixtures/gotest/basic_failure/expected.json",
        "tests/fixtures/gotest/build_failure/expected.json",
        "tests/fixtures/gotest/cached_pass/expected.json",
        "tests/fixtures/gotest/green_passing/expected.json",
        "tests/fixtures/gotest/init_panic/expected.json",
        "tests/fixtures/gotest/malformed_event_preamble/expected.json",
        "tests/fixtures/gotest/multi_package/expected.json",
        "tests/fixtures/gotest/no_tests/expected.json",
        "tests/fixtures/gotest/panic_in_test/expected.json",
        "tests/fixtures/gotest/parallel_interleaved/expected.json",
        "tests/fixtures/gotest/race_detected/expected.json",
        "tests/fixtures/gotest/skipped/expected.json",
        "tests/fixtures/gotest/subtests_nested/expected.json",
        "tests/fixtures/gotest/table_driven_failure/expected.json",
        "tests/fixtures/gotest/test_timeout/expected.json",
        "tests/fixtures/tsc/basic_type_mismatch/expected.json",
        "tests/fixtures/tsc/build_project_reference/expected.json",
        "tests/fixtures/tsc/config_no_inputs/expected.json",
        "tests/fixtures/tsc/cross_file_chain/expected.json",
        "tests/fixtures/tsc/destr_argument_type/expected.json",
        "tests/fixtures/tsc/global_cli_error/expected.json",
        "tests/fixtures/tsc/missing_symbol/expected.json",
        "tests/fixtures/tsc/module_not_found/expected.json",
        "tests/fixtures/tsc/multiple_errors_budgeted/expected.json",
        "tests/fixtures/tsc/node_modules_location/expected.json",
        "tests/fixtures/tsc/pretty_ansi_codeframe/expected.json",
        "tests/fixtures/tsc/syntax_error/expected.json",
        "tests/fixtures/pytest/banner_only_no_summary/expected.json",
        "tests/fixtures/pytest/basic_assertion/expected.json",
        "tests/fixtures/pytest/collection_error/expected.json",
        "tests/fixtures/pytest/sqlite_foreign_key_error/expected.json",
        "tests/fixtures/pytest/green_summary_nonzero_exit/expected.json",
        "tests/fixtures/pytest/parameterized/expected.json",
        "tests/fixtures/pytest/quiet_early_exit_assertion/expected.json",
        "tests/fixtures/pytest/teardown_exception/expected.json",
        "tests/fixtures/pytest/truncated_capture/expected.json",
        "tests/fixtures/vitest/basic_assertion/expected.json",
        "tests/fixtures/vitest/build_error/expected.json",
        "tests/fixtures/vitest/green/expected.json",
        "tests/fixtures/vitest/mixed_pass_fail/expected.json",
        "tests/fixtures/vitest/no_test_files/expected.json",
        "tests/fixtures/xcodebuild/build_failure/expected.json",
        "tests/fixtures/xcodebuild/destination_resolution_failure/expected.json",
        "tests/fixtures/xcodebuild/green_passing/expected.json",
        "tests/fixtures/xcodebuild/linker_failure/expected.json",
        "tests/fixtures/xcodebuild/multi_suite_skipped/expected.json",
        "tests/fixtures/xcodebuild/sim_launch_failure/expected.json",
        "tests/fixtures/xcodebuild/swift_semantic_build_failure/expected.json",
        "tests/fixtures/xcodebuild/swift_testing_expect_failure/expected.json",
        "tests/fixtures/xcodebuild/swift_testing_skipped/expected.json",
        "tests/fixtures/xcodebuild/swift_testing_multi_expect/expected.json",
    ]
]


def failure_id(adapter_id, phase, kind, test_id_or_loc, param_id, normalized_message):
    payload = (
        f"{adapter_id}|{phase}|{kind}|{test_id_or_loc}|"
        f"{param_id or ''}|{normalized_message}"
    )
    return hashlib.sha1(payload.encode()).hexdigest()[:16]


def test_group_id(adapter_id, stripped_test_id):
    payload = f"{adapter_id}|{stripped_test_id}"
    return hashlib.sha1(payload.encode()).hexdigest()[:16]


def strip_params_pytest(runner_test_id):
    # v1.3 pytest adapter strip_params rule: drop the [...] parameter
    # suffix. Non-parameterized tests pass through unchanged.
    bracket = runner_test_id.find("[")
    return runner_test_id if bracket == -1 else runner_test_id[:bracket]


def verify_scenario(expected_path):
    expected = json.loads(expected_path.read_text())
    inputs = expected.get("hash_inputs")
    if not inputs:
        return [(expected_path, "no hash_inputs block", True)]

    results = []
    normative = inputs.get("normative", False)
    label = "normative" if normative else "local-anchor"

    for failure in inputs["failures"]:
        runner_test_id = failure.get("runner_test_id")
        if runner_test_id is not None:
            test_id_or_loc = runner_test_id
        else:
            loc = failure["primary_location"]
            line = loc.get("line")
            test_id_or_loc = f"{loc['file']}:{line if line is not None else '?'}"

        got = failure_id(
            failure["adapter_id"],
            failure["phase"],
            failure["kind"],
            test_id_or_loc,
            failure.get("param_id"),
            failure["normalized_message"],
        )
        expected_hash = failure["failure_id"]
        results.append(
            (
                expected_path,
                f"failure_id {expected_hash} ({label})",
                got == expected_hash,
            )
        )

    expected_group_id = inputs.get("test_group_id")
    if expected_group_id is not None:
        first = inputs["failures"][0]
        runner_test_id = first["runner_test_id"]
        if runner_test_id is None:
            results.append(
                (
                    expected_path,
                    f"test_group_id {expected_group_id}: runner_test_id is null",
                    False,
                )
            )
        else:
            adapter_id = first["adapter_id"]
            # v1.3 declares strip_params for pytest only.
            stripper = {"pytest": strip_params_pytest}.get(adapter_id)
            if stripper is None:
                results.append(
                    (
                        expected_path,
                        f"test_group_id {expected_group_id}: no strip_params rule for adapter {adapter_id!r}",
                        False,
                    )
                )
            else:
                got = test_group_id(adapter_id, stripper(runner_test_id))
                results.append(
                    (
                        expected_path,
                        f"test_group_id {expected_group_id} ({label})",
                        got == expected_group_id,
                    )
                )

    return results


def main():
    expected_paths = {REPO_ROOT / path for path in EXPECTED_FIXTURE_PATHS}
    discovered_paths = set(FIXTURES_ROOT.rglob("expected.json"))
    missing = sorted(expected_paths - discovered_paths)
    extra = sorted(discovered_paths - expected_paths)
    if missing or extra:
        for path in missing:
            print(f"MISSING expected fixture: {path.relative_to(REPO_ROOT)}")
        for path in extra:
            print(f"EXTRA expected fixture: {path.relative_to(REPO_ROOT)}")
        return 2

    all_results = []
    for expected_path in sorted(expected_paths):
        all_results.extend(verify_scenario(expected_path))

    if not all_results:
        print("ERROR: no expected.json files found under", FIXTURES_ROOT)
        return 2

    failures = [r for r in all_results if not r[2]]
    for path, label, ok in all_results:
        rel = path.relative_to(REPO_ROOT)
        flag = "OK  " if ok else "FAIL"
        print(f"{flag}  {rel}  {label}")

    print()
    if failures:
        print(f"{len(failures)} failure(s) of {len(all_results)} check(s)")
        return 1
    print(f"All {len(all_results)} checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
