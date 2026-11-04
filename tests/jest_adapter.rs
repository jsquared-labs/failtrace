use std::{fs, path::Path};

use failtrace::{
    adapters::{AdapterError, AnalyzeOptions, jest::analyze_log},
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
fn jest_adapter_reproduces_fixture_hashes_and_valid_documents() {
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
        ["076f7a0d768298e5", "3dc55f38554b75ff", "ec8940cf8327f903",]
    );
}

#[test]
fn jest_green_lowers_to_complete_passed_document() {
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
    let log = b"Tests:       2 passed, 2 total\n";
    let mut sidecar = load_fixture("green").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let result = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None });

    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { .. })
    ));
}

#[test]
fn watch_mode_sidecars_are_rejected_during_replay() {
    for argv in [
        vec!["jest", "--watch"],
        vec!["jest", "--watchAll=true"],
        vec!["jest", "--watch-all"],
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
            Err(AdapterError::UnrecognizedFormat { reason }) if reason.contains("watch")
        ));
    }

    let fixture = load_fixture("green");
    let mut legacy_sidecar = fixture.sidecar;
    legacy_sidecar.argv.clear();
    legacy_sidecar.command = "jest --watchAll".to_owned();
    let result = analyze_log(
        &fixture.log,
        &legacy_sidecar,
        AnalyzeOptions { max_failures: None },
    );
    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { reason }) if reason.contains("watch")
    ));
}

#[test]
fn explicitly_disabled_watch_mode_is_replayable() {
    let fixture = load_fixture("green");
    let mut sidecar = fixture.sidecar;
    sidecar.argv = vec!["jest".to_owned(), "--watch=false".to_owned()];
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
    let log = b"FAIL tests/one.test.js\n\n  \xe2\x97\x8f suite \xe2\x80\xba one\n\n    Error: one\n\n      at Object.<anonymous> (tests/one.test.js:3:11)\n\nTest Suites: 1 failed, 1 total\nTests:       1 failed, 1 total\nSnapshots:   0 total\nTime:        0.109 s\nPASS tests/two.test.js\n\nTest Suites: 1 passed, 1 total\nTests:       1 passed, 1 total\nSnapshots:   0 total\nTime:        0.020 s\n";
    let mut sidecar = load_fixture("green").sidecar;
    sidecar.cwd = "/tmp/jest".to_owned();
    sidecar.exit_code = Some(0);
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let result = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None });

    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { reason })
            if reason.contains("multiple completed jest")
    ));
}

#[test]
fn captured_summary_shaped_output_is_not_a_second_jest_cycle() {
    let log = b"PASS tests/one.test.js\nNo tests found, exiting with code 1\nTests:       99 passed, 99 total\n\nTest Suites: 1 passed, 1 total\nTests:       2 passed, 2 total\nSnapshots:   0 total\nTime:        0.109 s\n";
    let mut sidecar = load_fixture("green").sidecar;
    sidecar.cwd = "/tmp/jest".to_owned();
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);

    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None })
        .expect("captured summary-shaped text should not look like another jest cycle");

    assert_eq!(document.runner_outcome, RunnerOutcome::Passed);
    assert_eq!(document.counts.tests_passed, 2);
}

#[test]
fn snapshot_mismatch_lowers_to_partial_exception_until_snapshot_adapter_support() {
    let log = b"FAIL tests/snap.test.js\n  snapshots\n    \xe2\x9c\x95 renders output\n\n  \xe2\x97\x8f snapshots \xe2\x80\xba renders output\n\n    Snapshot `renders output 1` mismatched\n\n      at Object.toMatchSnapshot (tests/snap.test.js:5:19)\n\nTest Suites: 1 failed, 1 total\nTests:       1 failed, 1 total\nSnapshots:   1 failed, 1 total\nTime:        0.109 s\n";
    let mut sidecar = load_fixture("green").sidecar;
    sidecar.cwd = "/tmp/jest".to_owned();
    sidecar.exit_code = Some(1);
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
        document.extensions["jest"]["partial_reason"],
        "snapshot_mismatch_not_structured"
    );
}

#[test]
fn obsolete_snapshot_is_unaccounted_even_with_a_parsed_test_failure() {
    let fixture = load_fixture("basic_assertion");
    let log = String::from_utf8(fixture.log)
        .unwrap()
        .replace("Snapshots:   0 total", "Snapshots:   1 obsolete, 1 total");
    let document = analyze_with(log.as_bytes(), |sidecar| {
        sidecar.cwd = "/tmp/jest".to_owned();
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
fn global_hook_error_is_unaccounted_even_with_a_parsed_test_failure() {
    for hook_error in [
        "Jest: Got error running globalTeardown - /tmp/teardown.js, reason: teardown failed",
        "Jest: Got error running globalSetup - /tmp/setup.js, reason: setup failed",
    ] {
        let fixture = load_fixture("basic_assertion");
        let mut log = String::from_utf8(fixture.log).unwrap();
        log.push_str(hook_error);
        log.push('\n');
        let document = analyze_with(log.as_bytes(), |sidecar| {
            sidecar.cwd = "/tmp/jest".to_owned();
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
}

#[test]
fn parser_missed_failure_lands_failed_with_observable_mismatch() {
    // The runner's own summary says tests failed; classifying the run
    // harness_crashed would send consumers debugging infrastructure instead
    // of reading the log. The outcome trusts the summary, and the
    // unextracted failure list is observable via RunnerSummaryMismatch +
    // failures_complete=false.
    let log = b"FAIL tests/weird.test.js\n\nTest Suites: 1 failed, 1 total\nTests:       1 failed, 1 total\nSnapshots:   0 total\nTime:        0.109 s\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/jest".to_owned();
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
    let log = b"FAIL tests/partial.test.js\n  partial\n    \xe2\x9c\x95 one (1 ms)\n\n  \xe2\x97\x8f partial \xe2\x80\xba one\n\n    Error: one\n\n      at Object.<anonymous> (tests/partial.test.js:3:11)\n\nTest Suites: 1 failed, 1 total\nTests:       2 failed, 2 total\nSnapshots:   0 total\nTime:        0.109 s\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/jest".to_owned();
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
fn failed_suite_shortfall_is_marked_incomplete() {
    let log = "FAIL tests/one.test.js\n\n  ● suite › one\n\n    Error: one\n\n      at Object.<anonymous> (tests/one.test.js:3:11)\n\nFAIL tests/two.test.js\n\nTest Suites: 2 failed, 2 total\nTests:       1 failed, 1 total\nSnapshots:   0 total\nTime:        0.109 s\n".as_bytes();
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/jest".to_owned();
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
fn collect_classified_test_block_still_accounts_for_reported_test() {
    let log = "FAIL tests/dynamic.test.js\n\n  ● suite › imports lazily\n\n    Cannot find module './missing' from 'tests/dynamic.test.js'\n\n      at Object.<anonymous> (tests/dynamic.test.js:3:11)\n\nTest Suites: 1 failed, 1 total\nTests:       1 failed, 1 total\nSnapshots:   0 total\nTime:        0.109 s\n".as_bytes();
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/jest".to_owned();
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
fn obsolete_snapshot_exit_cannot_hide_behind_green_test_counts() {
    let log = b"PASS tests/basic.test.js\n\nTest Suites: 1 passed, 1 total\nTests:       2 passed, 2 total\nSnapshots:   1 obsolete, 1 total\nTime:        0.109 s\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/jest".to_owned();
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
fn signal_kill_with_green_summary_lands_harness_crashed() {
    let log = b"PASS tests/basic.test.js\n\nTest Suites: 1 passed, 1 total\nTests:       2 passed, 2 total\nSnapshots:   0 total\nTime:        0.109 s\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/jest".to_owned();
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
    let log = b"FAIL tests/x.test.js\n  suite\n    \xe2\x9c\x95 case\n\n  \xe2\x97\x8f suite \xe2\x80\xba case\n\n    expect(received).toBe(expected)\n\n      at Object.toBe (tests/x.test.js:5:19)\n";
    let document = analyze_with(log, |sidecar| {
        sidecar.cwd = "/tmp/jest".to_owned();
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
fn lone_fail_word_is_rejected_at_admission() {
    let log = b"FAIL\n";
    let mut sidecar = load_fixture("green").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    sidecar.cwd = "/tmp/jest".to_owned();
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
fn test_suite_failed_to_run_lands_pre_test_failed() {
    let fixture = load_fixture("build_error");
    let document = analyze_log(
        &fixture.log,
        &fixture.sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::PreTestFailed);
    assert_eq!(document.failures[0].phase, Phase::Collect);
    assert_eq!(document.failures[0].kind, FailureKind::MissingSymbol);
    assert_eq!(document.failures[0].runner_test_id, None);
    assert_eq!(document.counts.tests_run, 0);
}

#[test]
fn jest_failure_id_hashes_normalized_message() {
    let fixture = load_fixture("build_error");
    let log = String::from_utf8(fixture.log)
        .unwrap()
        .replace("./this-module-does-not-exist", "./missing-0xFF-42");
    let document = analyze_with(log.as_bytes(), |sidecar| {
        sidecar.cwd = "/tmp/failtrace-normalize".to_owned();
        sidecar.exit_code = Some(1);
    });
    let failure = &document.failures[0];
    let expected = failure_id(FailureIdParts {
        adapter_id: "jest",
        phase: Phase::Collect,
        kind: FailureKind::MissingSymbol,
        runner_test_id: None,
        primary_location: &Location {
            file: "tests/build_error.test.js".to_owned(),
            line: Some(1),
            column: Some(17),
            end_line: None,
            end_column: None,
            role: LocationRole::ImportSite,
            in_user_code: true,
        },
        param_id: None,
        message: "Cannot find module './missing-0xFF-42' from 'tests/build_error.test.js'",
        cwd: "/tmp/failtrace-normalize",
    });

    assert_eq!(
        failure.message,
        "Cannot find module './missing-0xFF-42' from 'tests/build_error.test.js'"
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
        .join("tests/fixtures/jest")
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
