use std::{fs, path::Path};

use failtrace::{
    adapters::{AdapterError, AnalyzeOptions, vitest::analyze_log},
    schema::{
        FailureIdParts, FailureKind, Location, LocationRole, OutputDocument, ParseStatus,
        ParseWarning, Phase, RankingStrategy, RunnerOutcome, failure_id, validate,
    },
    storage::Sidecar,
};
use serde_json::Value;

const SCENARIOS: &[&str] = &[
    "green",
    "basic_assertion",
    "mixed_pass_fail",
    "no_test_files",
    "build_error",
];

#[test]
fn vitest_adapter_reproduces_fixture_hashes_and_valid_documents() {
    let mut reproduced = Vec::new();

    for scenario in SCENARIOS {
        let fixture = load_fixture(scenario);
        let document = analyze_log(
            &fixture.log,
            &fixture.sidecar,
            AnalyzeOptions { max_failures: None },
        )
        .unwrap_or_else(|error| panic!("{scenario}: analyze_log failed: {error}"));

        assert_eq!(
            validate(&document),
            Vec::new(),
            "{scenario}: document violates schema invariants"
        );
        assert_expected_subset(scenario, &fixture.expected["expected_subset"], &document);

        let expected_failures = fixture.expected["hash_inputs"]["failures"]
            .as_array()
            .expect("hash_inputs.failures is an array");
        assert_eq!(
            document.failures.len(),
            expected_failures.len(),
            "{scenario}: failure count mismatch"
        );
        for (failure, expected) in document.failures.iter().zip(expected_failures) {
            let expected_id = expected["failure_id"].as_str().unwrap();
            assert_eq!(failure.failure_id, expected_id, "{scenario}: failure_id");
            reproduced.push(expected_id.to_owned());
        }
    }

    reproduced.sort();
    assert_eq!(
        reproduced,
        ["3a112a30443a423f", "5aea3a2bfd9eee1d", "a16682981b253700",]
    );
}

#[test]
fn vitest_green_lowers_to_complete_passed_document() {
    let fixture = load_fixture("green");
    let document = analyze_log(
        &fixture.log,
        &fixture.sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.parse_status, ParseStatus::Complete);
    assert_eq!(document.parse_warnings, Vec::<ParseWarning>::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::Passed);
    assert_eq!(document.ranking.strategy, RankingStrategy::None);
    assert!(!document.ranking.is_stable_across_runs);
    assert_eq!(document.counts.tests_run, 2);
    assert_eq!(document.counts.tests_passed, 2);
    assert_eq!(document.counts.tests_failed, 0);
    assert_eq!(document.counts.tests_errored, 0);
    assert_eq!(document.counts.tests_skipped, 0);
}

#[test]
fn count_only_log_is_rejected_at_admission() {
    let log = b"      Tests  2 passed (2)\n";
    let mut sidecar = load_fixture("green").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let result = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None });

    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { .. })
    ));
}

#[test]
fn mode_ambiguous_bare_vitest_sidecar_is_rejected_during_replay() {
    let fixture = load_fixture("green");
    let mut sidecar = fixture.sidecar;
    sidecar.argv = vec!["vitest".to_owned()];
    sidecar.command = "vitest".to_owned();
    let result = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    );

    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { reason }) if reason.contains("watch/UI")
    ));
}

#[test]
fn package_wrapper_flags_do_not_confuse_vitest_replay_gate() {
    let fixture = load_fixture("green");
    let mut sidecar = fixture.sidecar;
    sidecar.argv = ["npm", "exec", "--package", "vitest", "vitest", "run"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let document = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .expect("CLI-supported wrapper should remain analyzable during replay");
    assert_eq!(document.runner_outcome, RunnerOutcome::Passed);
}

#[test]
fn watch_and_ui_sidecars_are_rejected_during_replay() {
    for argv in [
        vec!["vitest", "--watch"],
        vec!["vitest", "-w"],
        vec!["vitest", "--ui=true"],
        vec!["vitest", "run", "--standalone"],
        vec!["vitest", "run", "--run=false"],
        vec!["vitest", "run", "--no-run"],
    ] {
        let fixture = load_fixture("green");
        let mut sidecar = fixture.sidecar;
        sidecar.argv = argv.into_iter().map(str::to_owned).collect();
        let result = analyze_log(
            &fixture.log,
            &sidecar,
            AnalyzeOptions { max_failures: None },
        );
        assert!(matches!(
            result,
            Err(AdapterError::UnrecognizedFormat { reason })
                if reason.contains("watch/UI")
        ));
    }

    let fixture = load_fixture("green");
    let mut legacy_sidecar = fixture.sidecar;
    legacy_sidecar.argv.clear();
    legacy_sidecar.command = "vitest --ui".to_owned();
    let result = analyze_log(
        &fixture.log,
        &legacy_sidecar,
        AnalyzeOptions { max_failures: None },
    );
    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { reason }) if reason.contains("watch/UI")
    ));
}

#[test]
fn explicitly_disabled_watch_and_ui_modes_are_replayable() {
    let fixture = load_fixture("green");
    let mut sidecar = fixture.sidecar;
    sidecar.argv = vec![
        "vitest".to_owned(),
        "--watch=false".to_owned(),
        "--ui=false".to_owned(),
    ];
    let document = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(document.parse_status, ParseStatus::Complete);
    assert!(document.failures_complete);
}

#[test]
fn multiple_completed_run_summaries_are_incomplete_without_watch_argv() {
    let log = " RUN  v4.1.6 /tmp/vitest\n\n------- Failed Tests 1 -------\n FAIL  tests/one.test.ts > suite > one\nAssertionError: one\n\n Test Files  1 failed (1)\n      Tests  1 failed (1)\n   Duration  109ms\n\n RERUN  tests/two.test.ts x1\n\n Test Files  1 passed (1)\n      Tests  1 passed (1)\n   Duration  20ms\n".as_bytes();
    let mut sidecar = load_fixture("green").sidecar;
    sidecar.cwd = "/tmp/vitest".to_owned();
    sidecar.exit_code = Some(0);
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let result = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None });

    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { reason })
            if reason.contains("multiple completed vitest")
    ));
}

#[test]
fn captured_summary_shaped_output_is_not_a_second_vitest_cycle() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n\n \xe2\x9c\x93 tests/one.test.ts (2 tests) 2ms\n\nstdout | tests/one.test.ts > suite > one\n      Tests  99 passed (99)\nNo test files found, exiting with code 1\n\n Test Files  1 passed (1)\n      Tests  2 passed (2)\n   Duration  109ms\n";
    let mut sidecar = load_fixture("green").sidecar;
    sidecar.cwd = "/tmp/vitest".to_owned();
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);

    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None })
        .expect("captured summary-shaped text should not look like another vitest cycle");

    assert_eq!(document.runner_outcome, RunnerOutcome::Passed);
    assert_eq!(document.counts.tests_passed, 2);
}

#[test]
fn snapshot_mismatch_lowers_to_partial_exception_until_snapshot_adapter_support() {
    let log = "\n RUN  v4.1.6 /tmp/vitest\n\n ❯ tests/snap.test.ts (1 test | 1 failed) 3ms\n     × renders output 3ms\n\n⎯⎯⎯⎯⎯⎯⎯ Failed Tests 1 ⎯⎯⎯⎯⎯⎯⎯\n\n FAIL  tests/snap.test.ts > snapshot > renders output\nError: Snapshot `renders output 1` mismatched\n\n ❯ tests/snap.test.ts:5:19\n⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯[1/1]⎯\n\n Test Files  1 failed (1)\n      Tests  1 failed (1)\n  Start at  09:25:30\n  Duration  109ms\n";
    let mut sidecar = load_fixture("green").sidecar;
    sidecar.cwd = "/tmp/vitest".to_owned();
    sidecar.exit_code = Some(1);
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log.as_bytes());
    let document = analyze_log(
        log.as_bytes(),
        &sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert_eq!(
        document.parse_warnings,
        vec![ParseWarning::UnstructuredDiagnostic]
    );
    assert_eq!(
        document.failures[0].kind,
        failtrace::schema::FailureKind::Exception
    );
    assert_eq!(
        document.extensions["vitest"]["partial_reason"],
        "snapshot_mismatch_not_structured"
    );
}

#[test]
fn coverage_threshold_is_unaccounted_even_with_a_parsed_test_failure() {
    let fixture = load_fixture("basic_assertion");
    let mut log = fixture.log;
    log.extend_from_slice(
        b"ERROR: Coverage for lines (50%) does not meet global threshold (100%)\n",
    );
    let document = analyze_with(&log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = Some(1);
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 1);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(!document.failures_complete);
}

#[test]
fn parser_missed_failure_lands_failed_with_observable_mismatch() {
    // The runner's own summary says tests failed; classifying the run
    // harness_crashed would send consumers debugging infrastructure instead
    // of reading the log. The outcome trusts the summary, and the
    // unextracted failure list is observable via RunnerSummaryMismatch +
    // failures_complete=false.
    let log = b" RUN  v4.1.6 /tmp/vitest\n\n FAIL  tests/weird.test.ts (1 test | 1 failed) 3ms\n\n Test Files  1 failed (1)\n      Tests  1 failed (1)\n   Start at  09:25:30\n   Duration  109ms\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = Some(1);
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert!(document.failures.is_empty());
    assert_eq!(document.counts.tests_passed, 0);
    assert_eq!(document.counts.tests_failed, 0);
    assert!(
        !document
            .parse_warnings
            .contains(&ParseWarning::MissingSummaryLine)
    );
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(!document.failures_complete);
}

#[test]
fn partially_extracted_failure_list_is_marked_incomplete() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n\n FAIL  tests/partial.test.ts > partial > one\nAssertionError: one\n\n Test Files  1 failed (1)\n      Tests  2 failed (2)\n   Start at  09:25:30\n   Duration  109ms\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = Some(1);
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert_eq!(document.failures.len(), 1);
    assert_eq!(
        document.counts.tests_failed + document.counts.tests_errored,
        1
    );
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(!document.failures_complete);
}

#[test]
fn failed_file_shortfall_is_marked_incomplete() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n\n------- Failed Tests 1 -------\n FAIL  tests/one.test.ts > suite > one\nAssertionError: one\n\n Test Files  2 failed (2)\n      Tests  1 failed (1)\n   Duration  109ms\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = Some(1);
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 1);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(!document.failures_complete);
}

#[test]
fn failed_suite_block_does_not_mask_missing_failed_test_block() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n\n------- Failed Suites 1 -------\n FAIL  tests/mixed.test.ts [ tests/mixed.test.ts ]\nError: beforeAll failed\n\n------- Failed Tests 1 -------\n\n Test Files  1 failed (1)\n      Tests  1 failed (1)\n   Duration  109ms\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = Some(1);
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 1);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(!document.failures_complete);
}

#[test]
fn unparsed_global_error_is_not_credited_to_a_test_failure() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n\n------- Failed Tests 1 -------\n FAIL  tests/mixed.test.ts > suite > one\nAssertionError: one\n\n------- Unhandled Errors -------\nError: background rejection\n\n Test Files  1 failed (1)\n      Tests  1 failed (1)\n     Errors  1 error\n   Duration  109ms\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = Some(1);
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 1);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(!document.failures_complete);
}

#[test]
fn failed_test_section_accounts_for_collect_classified_test_block() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n\n------- Failed Tests 1 -------\n FAIL  tests/dynamic.test.ts > suite > imports lazily\nError: Cannot find module './missing'\n\n Test Files  1 failed (1)\n      Tests  1 failed (1)\n   Duration  109ms\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = Some(1);
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 1);
    assert_eq!(document.failures[0].phase, Phase::Collect);
    assert!(
        !document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(document.failures_complete);
}

#[test]
fn duplicate_retry_headers_keep_unique_test_outcome_counts_valid() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n\n------- Failed Tests 1 -------\n FAIL  tests/retry.test.ts > suite > retries\nAssertionError: first attempt\n FAIL  tests/retry.test.ts > suite > retries\nAssertionError: final attempt\n\n Test Files  1 failed (1)\n      Tests  1 failed (1)\n   Duration  109ms\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = Some(1);
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 2);
    assert_eq!(document.counts.tests_failed, 1);
    assert_eq!(document.counts.tests_errored, 0);
    assert!(document.failures_complete);
}

#[test]
fn coverage_threshold_exit_cannot_hide_behind_green_test_counts() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n\n \xe2\x9c\x93 tests/basic.test.ts (2 tests) 2ms\n\n Test Files  1 passed (1)\n      Tests  2 passed (2)\n   Start at  09:25:30\n   Duration  109ms\nERROR: Coverage for lines (50%) does not meet global threshold (100%)\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = Some(1);
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert!(document.failures.is_empty());
    assert_eq!(document.counts.tests_passed, 0);
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(!document.failures_complete);
}

#[test]
fn no_test_files_exit_one_lands_harness_crashed() {
    let fixture = load_fixture("no_test_files");
    let document = analyze_log(
        &fixture.log,
        &fixture.sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_expected_subset(
        "no_test_files",
        &fixture.expected["expected_subset"],
        &document,
    );
    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert_eq!(document.parse_status, ParseStatus::Complete);
    assert!(document.failures.is_empty());
    assert_eq!(document.counts.tests_run, 0);
}

#[test]
fn no_test_files_exit_zero_lands_no_tests() {
    let fixture = load_fixture("no_test_files");
    let mut sidecar = fixture.sidecar;
    sidecar.exit_code = Some(0);
    let document = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::NoTests);
    assert_eq!(document.parse_status, ParseStatus::Complete);
    assert_eq!(document.parse_warnings, Vec::<ParseWarning>::new());
    assert!(document.failures.is_empty());
    assert_eq!(document.counts.tests_run, 0);
}

#[test]
fn errors_line_with_clean_tests_summary_lands_harness_crashed() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n\n \xe2\x9c\x93 tests/basic.test.ts (2 tests) 2ms\n\n Test Files  1 passed (1)\n      Tests  2 passed (2)\n     Errors  1 error\n   Start at  09:25:30\n   Duration  109ms\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = Some(0);
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert_eq!(
        document.parse_warnings,
        vec![ParseWarning::RunnerSummaryMismatch]
    );
    assert!(!document.failures_complete);
    assert_eq!(document.counts.tests_run, 2);
    assert_eq!(document.counts.tests_passed, 0);
    assert_eq!(document.counts.tests_failed, 0);
    assert_eq!(document.counts.tests_errored, 0);
}

#[test]
fn plural_errors_line_with_clean_tests_summary_lands_harness_crashed() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n\n \xe2\x9c\x93 tests/basic.test.ts (2 tests) 2ms\n\n Test Files  1 passed (1)\n      Tests  2 passed (2)\n     Errors  2 errors\n   Start at  09:25:30\n   Duration  109ms\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = Some(0);
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert_eq!(
        document.parse_warnings,
        vec![ParseWarning::RunnerSummaryMismatch]
    );
    assert!(!document.failures_complete);
}

#[test]
fn signal_kill_with_green_summary_lands_harness_crashed() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n\n \xe2\x9c\x93 tests/basic.test.ts (2 tests) 2ms\n\n Test Files  1 passed (1)\n      Tests  2 passed (2)\n   Start at  09:25:30\n   Duration  109ms\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = None;
        sidecar.signal = Some("SIGKILL".to_owned());
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(!document.failures_complete);
}

#[test]
fn signal_kill_with_parsed_failure_preserves_failures() {
    let fixture = load_fixture("basic_assertion");
    let mut sidecar = fixture.sidecar.clone();
    sidecar.exit_code = None;
    sidecar.signal = Some("SIGTERM".to_owned());
    let document = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert_eq!(document.failures.len(), 1);
    assert_eq!(document.counts.tests_passed, 0);
    assert!(
        !document
            .parse_warnings
            .contains(&ParseWarning::MissingSummaryLine)
    );
    assert!(!document.failures_complete);
}

#[test]
fn missing_summary_pushes_warning_and_clears_failures_complete() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n\n FAIL  tests/x.test.ts > suite > case\nAssertionError: boom\n\n \xe2\x9d\xaf tests/x.test.ts:5:19\n      3| describe(\"suite\", () => {\n      4|   it(\"case\", () => {\n      5|     expect(1).toBe(2);\n       |               ^\n      6|   });\n      7| });\n\n\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf\xe2\x8e\xaf[1/1]\xe2\x8e\xaf\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = None;
        sidecar.signal = Some("SIGKILL".to_owned());
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::MissingSummaryLine)
    );
    assert!(!document.failures_complete);
}

#[test]
fn banner_only_signal_kill_lands_harness_crashed() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = None;
        sidecar.signal = Some("SIGKILL".to_owned());
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::MissingSummaryLine)
    );
    assert!(!document.failures_complete);
}

#[test]
fn banner_only_exit_one_lands_harness_crashed() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/vitest".to_owned();
        sidecar.exit_code = Some(1);
    });

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::MissingSummaryLine)
    );
    assert!(!document.failures_complete);
}

#[test]
fn banner_only_clean_exit_is_rejected_at_admission() {
    let log = b" RUN  v4.1.6 /tmp/vitest\n";
    let mut sidecar = load_fixture("green").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    sidecar.cwd = "/tmp/vitest".to_owned();
    sidecar.exit_code = Some(0);
    let result = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None });

    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { .. })
    ));
}

#[test]
fn budget_zero_truncation_preserves_failed_classification() {
    let fixture = load_fixture("mixed_pass_fail");
    let document = analyze_log(
        &fixture.log,
        &fixture.sidecar,
        AnalyzeOptions {
            max_failures: Some(0),
        },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert_eq!(document.emission.failures_emitted, 0);
    assert!(document.emission.failures_total > 0);
    assert!(document.failures.is_empty());
}

#[test]
fn vitest_failure_id_hashes_normalized_message() {
    let cwd = "/tmp/failtrace-normalize";
    let fixture = load_fixture("build_error");
    let original_cwd = "/Users/dev/failtrace/tests/fixtures/vitest/build_error/source";
    let log = String::from_utf8(fixture.log)
        .unwrap()
        .replace(original_cwd, cwd)
        .replace("./this-module-does-not-exist", "./missing-0xFF-42");
    let document = analyze_with(log.as_bytes(), |sidecar| {
        sidecar.cwd = cwd.to_owned();
        sidecar.exit_code = Some(1);
    });
    let failure = &document.failures[0];
    let expected = failure_id(FailureIdParts {
        adapter_id: "vitest",
        phase: Phase::Collect,
        kind: FailureKind::MissingSymbol,
        runner_test_id: None,
        primary_location: &Location {
            file: "tests/build_error.test.ts".to_owned(),
            line: Some(2),
            column: Some(1),
            end_line: None,
            end_column: None,
            role: LocationRole::ImportSite,
            in_user_code: true,
        },
        param_id: None,
        message: "Error: Cannot find module './missing-0xFF-42' imported from /tmp/failtrace-normalize/tests/build_error.test.ts",
        cwd,
    });

    assert_eq!(
        failure.message,
        "Error: Cannot find module './missing-0xFF-42' imported from /tmp/failtrace-normalize/tests/build_error.test.ts"
    );
    assert_eq!(failure.failure_id, expected);
}

fn analyze_with(log: &[u8], mutate: impl FnOnce(&mut Sidecar)) -> OutputDocument {
    let mut sidecar = load_fixture("green").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    mutate(&mut sidecar);
    analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None })
        .unwrap_or_else(|error| panic!("analyze_log failed: {error}"))
}

struct Fixture {
    log: Vec<u8>,
    expected: Value,
    sidecar: Sidecar,
}

fn load_fixture(scenario: &str) -> Fixture {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/vitest")
        .join(scenario);
    let log = fs::read(root.join("run/log")).unwrap();
    let expected: Value =
        serde_json::from_slice(&fs::read(root.join("expected.json")).unwrap()).unwrap();
    let sidecar: Sidecar =
        serde_json::from_slice(&fs::read(root.join("run/meta.json")).unwrap()).unwrap();
    Fixture {
        log,
        expected,
        sidecar,
    }
}

fn assert_expected_subset(scenario: &str, expected: &Value, document: &OutputDocument) {
    let actual = serde_json::to_value(document).unwrap();
    assert_subset(scenario, expected, &actual, "root");
}

fn assert_subset(scenario: &str, expected: &Value, actual: &Value, path: &str) {
    match expected {
        Value::Object(expected_object) => {
            let actual_object = actual
                .as_object()
                .unwrap_or_else(|| panic!("{scenario}: {path} is not an object"));
            for (key, expected_value) in expected_object {
                let child_path = format!("{path}.{key}");
                let actual_value = actual_object
                    .get(key)
                    .unwrap_or_else(|| panic!("{scenario}: missing {child_path}"));
                assert_subset(scenario, expected_value, actual_value, &child_path);
            }
        }
        Value::Array(expected_array) => {
            let actual_array = actual
                .as_array()
                .unwrap_or_else(|| panic!("{scenario}: {path} is not an array"));
            assert_eq!(
                actual_array.len(),
                expected_array.len(),
                "{scenario}: {path} length"
            );
            for (index, (expected_value, actual_value)) in
                expected_array.iter().zip(actual_array).enumerate()
            {
                assert_subset(
                    scenario,
                    expected_value,
                    actual_value,
                    &format!("{path}[{index}]"),
                );
            }
        }
        _ => assert_eq!(actual, expected, "{scenario}: {path}"),
    }
}
