use std::{collections::BTreeMap, fs, path::Path};

use failtrace::{
    adapters::{AnalyzeOptions, xcodebuild::analyze_log},
    schema::{OutputDocument, ParseStatus, ParseWarning, RunnerOutcome, validate},
    storage::Sidecar,
};
use serde_json::Value;

const SCENARIOS: &[&str] = &[
    "green_passing",
    "build_failure",
    "swift_testing_expect_failure",
    "sim_launch_failure",
    "swift_testing_multi_expect",
    "linker_failure",
    "swift_semantic_build_failure",
    "destination_resolution_failure",
    "multi_suite_skipped",
    "swift_testing_skipped",
];

struct Fixture {
    log: Vec<u8>,
    expected: Value,
    sidecar: Sidecar,
}

fn load_fixture(scenario: &str) -> Fixture {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/xcodebuild")
        .join(scenario);
    Fixture {
        log: fs::read(root.join("run/log")).unwrap(),
        expected: serde_json::from_slice(&fs::read(root.join("expected.json")).unwrap()).unwrap(),
        sidecar: serde_json::from_slice(&fs::read(root.join("run/meta.json")).unwrap()).unwrap(),
    }
}

/// Mirror of `gotest_adapter::gotest_adapter_reproduces_fixture_hashes_and_valid_documents`.
/// For every fixture: load it, run the adapter, validate the document, assert
/// every `hash_inputs.failures[].failure_id` is emitted in the document, and
/// assert that `expected_subset` is a subset of the actual document. This is
/// the load-bearing regression test — any change to the adapter that flips
/// a `failure_id` or alters the lowered document surfaces here.
#[test]
fn xcodebuild_adapter_reproduces_fixture_hashes_and_valid_documents() {
    let mut reproduced = Vec::new();

    for scenario in SCENARIOS {
        let fixture = load_fixture(scenario);
        let document = analyze_log(
            &fixture.log,
            &fixture.sidecar,
            AnalyzeOptions { max_failures: None },
        )
        .unwrap_or_else(|err| panic!("{scenario}: analyze_log failed: {err}"));

        assert_eq!(
            validate(&document),
            Vec::new(),
            "{scenario}: document violates schema invariants"
        );
        assert_expected_subset(scenario, &fixture.expected, &document);

        for expected in fixture.expected["hash_inputs"]["failures"]
            .as_array()
            .expect("hash_inputs.failures is an array")
        {
            let expected_id = expected["failure_id"].as_str().unwrap();
            assert!(
                document
                    .failures
                    .iter()
                    .any(|failure| failure.failure_id == expected_id),
                "{scenario}: expected fixture anchor {expected_id} was not emitted"
            );
            reproduced.push(expected_id.to_owned());
        }
    }

    // At least one anchor must exist across all fixtures, or the test is
    // vacuous. Today's set: build_failure (1), swift_testing_expect_failure (1),
    // swift_testing_multi_expect (2), linker_failure (2). Green and sim_launch
    // fixtures emit zero failures, so they contribute zero anchors.
    assert!(!reproduced.is_empty(), "no fixture anchors were checked");
}

fn assert_expected_subset(scenario: &str, expected: &Value, document: &OutputDocument) {
    let actual = serde_json::to_value(document).unwrap();
    let stable = expected["hash_inputs"]["failure_order_stable"]
        .as_bool()
        .unwrap_or(true);
    assert_subset(
        scenario,
        &expected["expected_subset"],
        &actual,
        "root",
        stable,
    );
}

fn assert_subset(scenario: &str, expected: &Value, actual: &Value, path: &str, stable: bool) {
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
                assert_subset(scenario, expected_value, actual_value, &child_path, stable);
            }
        }
        Value::Array(expected_array) => {
            let actual_array = actual
                .as_array()
                .unwrap_or_else(|| panic!("{scenario}: {path} is not an array"));
            if !stable && path == "root.failures" {
                assert_unordered_failure_subset(scenario, expected_array, actual_array);
                return;
            }
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
                    stable,
                );
            }
        }
        _ => assert_eq!(actual, expected, "{scenario}: {path}"),
    }
}

fn assert_unordered_failure_subset(scenario: &str, expected: &[Value], actual: &[Value]) {
    let expected_by_id = by_key(expected, "failure_id");
    let actual_by_id = by_key(actual, "failure_id");
    assert_eq!(
        actual_by_id.keys().collect::<Vec<_>>(),
        expected_by_id.keys().collect::<Vec<_>>(),
        "{scenario}: unordered failure ids"
    );
    for (failure_id, expected_value) in expected_by_id {
        assert_subset(
            scenario,
            expected_value,
            actual_by_id[failure_id],
            &format!("root.failures[{failure_id}]"),
            true,
        );
    }
}

fn by_key<'a>(values: &'a [Value], key: &str) -> BTreeMap<&'a str, &'a Value> {
    values
        .iter()
        .map(|value| (value[key].as_str().expect("key must be a string"), value))
        .collect()
}

#[test]
fn xcodebuild_adapter_produces_valid_documents() {
    for scenario in SCENARIOS {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/xcodebuild")
            .join(scenario);
        let log = fs::read(root.join("run/log")).unwrap_or_else(|e| {
            panic!("{scenario}: failed to read log: {e}");
        });
        let sidecar: Sidecar =
            serde_json::from_slice(&fs::read(root.join("run/meta.json")).unwrap())
                .unwrap_or_else(|e| panic!("{scenario}: failed to parse meta.json: {e}"));
        let document = analyze_log(&log, &sidecar, AnalyzeOptions { max_failures: None })
            .unwrap_or_else(|e| panic!("{scenario}: analyze_log failed: {e}"));
        let violations = validate(&document);
        assert!(
            violations.is_empty(),
            "{scenario}: document violates schema invariants: {violations:?}"
        );

        // Print a compact summary for manual inspection during fixture development.
        eprintln!(
            "{scenario}: runner_outcome={:?} failures={} tests_run={} tests_failed={}",
            document.runner_outcome,
            document.failures.len(),
            document.counts.tests_run,
            document.counts.tests_failed,
        );
        for failure in &document.failures {
            eprintln!(
                "  - phase={:?} kind={:?} file={} line={} id={} msg={}",
                failure.phase,
                failure.kind,
                failure.primary_location.file,
                failure
                    .primary_location
                    .line
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".to_owned()),
                failure.failure_id,
                truncate_for_print(&failure.message, 80),
            );
        }
    }
}

fn truncate_for_print(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

#[test]
fn green_passing_emits_zero_failures_and_passed_outcome() {
    let document = run_scenario("green_passing");
    assert_eq!(document.failures.len(), 0);
    assert_eq!(
        document.runner_outcome,
        failtrace::schema::RunnerOutcome::Passed
    );
    assert_eq!(document.counts.tests_run, 2);
}

#[test]
fn swift_testing_issue_shortfall_marks_failure_list_incomplete() {
    let log = b"\xe2\x97\x87 Test run started.\n\xe2\x9c\x98 Test alpha() recorded an issue at AlphaTests.swift:10:5: Expectation failed: 1 == 2\n\xe2\x9c\x98 Test run with 2 tests in 1 suite failed after 0.002 seconds with 2 issues.\n** TEST FAILED **\n";
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert_eq!(document.failures.len(), 1);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(!document.failures_complete);
}

#[test]
fn missing_terminal_event_clears_failure_list_completeness() {
    let log = b"\xe2\x97\x87 Test run started.\n\xe2\x9c\x98 Test alpha() recorded an issue at AlphaTests.swift:10:5: Expectation failed: 1 == 2\n";
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 1);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::MissingTerminalEvent)
    );
    assert!(!document.failures_complete);
}

#[test]
fn repeated_swift_testing_summaries_are_conservatively_incomplete() {
    let log = b"\xe2\x97\x87 Test run started.\n\xe2\x9c\x98 Test alpha() recorded an issue at AlphaTests.swift:10:5: Expectation failed: 1 == 2\n\xe2\x9c\x98 Test run with 1 test in 1 suite failed after 0.002 seconds with 1 issue.\n\xe2\x97\x87 Test run started.\n\xe2\x9c\x98 Test run with 1 test in 1 suite failed after 0.002 seconds with 1 issue.\n** TEST FAILED **\n";
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 1);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(!document.failures_complete);
}

#[test]
fn later_swift_epoch_without_run_total_cannot_borrow_earlier_authority() {
    let log = "◇ Test run started.\n◇ Suite FirstRunSuite started.\n✘ Test first() recorded an issue at FirstTests.swift:10:5: Expectation failed: 1 == 2\n✘ Suite FirstRunSuite failed after 0.001 seconds with 1 issue.\n✘ Test run with 1 test in 1 suite failed after 0.001 seconds with 1 issue.\n◇ Test run started.\n◇ Suite SecondRunFirstSuite started.\n✘ Test second() recorded an issue at SecondTests.swift:20:5: Expectation failed: 2 == 3\n✘ Suite SecondRunFirstSuite failed after 0.001 seconds with 1 issue.\n◇ Suite SecondRunMissingSuite started.\n✘ Suite SecondRunMissingSuite failed after 0.001 seconds with 1 issue.\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 2);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(!document.failures_complete);
}

#[test]
fn disjoint_swift_epochs_add_their_suite_issue_lower_bounds() {
    let log = "◇ Test run started.\n◇ Suite FirstSuite started.\n✘ Test first() recorded an issue at FirstTests.swift:10:5: Expectation failed: 1 == 2\n✘ Suite FirstSuite failed after 0.001 seconds with 1 issue.\n◇ Test run started.\n◇ Suite SecondSuite started.\n✘ Suite SecondSuite failed after 0.001 seconds with 1 issue.\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn disjoint_swift_epochs_cannot_mask_counts_across_evidence_sources() {
    let log = "◇ Test run started.\n✘ Test first() recorded an issue at FirstTests.swift:10:5: Expectation failed: 1 == 2\n✘ Test first() failed after 0.001 seconds with 1 issue.\n✘ Test run with 1 test in 1 suite failed after 0.001 seconds with 1 issue.\n◇ Test run started.\n◇ Suite SecondSuite started.\n✘ Suite SecondSuite failed after 0.001 seconds with 1 issue.\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn xctest_assertion_shortfall_marks_failure_list_incomplete() {
    let log = b"Test Suite 'All tests' started at 2026-07-19 12:00:00.000\nAlphaTests.swift:10: error: -[AlphaTests testOne] : XCTAssertEqual failed\nTest Suite 'All tests' failed at 2026-07-19 12:00:00.001.\n\t Executed 2 tests, with 1 test skipped and 2 failures (0 unexpected) in 0.001 seconds\n** TEST FAILED **\n";
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert_eq!(document.failures.len(), 1);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(!document.failures_complete);
}

#[test]
fn nested_xctest_envelope_summaries_are_not_double_counted() {
    let log = b"Test Suite 'Selected tests' started at 2026-07-19 12:00:00.000.\nTest Suite 'ExampleTests.xctest' started at 2026-07-19 12:00:00.000.\nTest Suite 'AlphaTests' started at 2026-07-19 12:00:00.000.\nAlphaTests.swift:10: error: -[ExampleTests.AlphaTests testOne] : XCTAssertEqual failed\nTest Suite 'AlphaTests' failed at 2026-07-19 12:00:00.001.\n\t Executed 1 test, with 1 failure (0 unexpected) in 0.001 seconds\nTest Suite 'ExampleTests.xctest' failed at 2026-07-19 12:00:00.001.\n\t Executed 1 test, with 1 failure (0 unexpected) in 0.001 seconds\nTest Suite 'Selected tests' failed at 2026-07-19 12:00:00.001.\n\t Executed 1 test, with 1 failure (0 unexpected) in 0.001 seconds\n** TEST FAILED **\n";
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 1);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(document.failures_complete);
    assert_eq!(document.counts.tests_run, 1);
    assert_eq!(document.parse_status, ParseStatus::Partial);
}

#[test]
fn sequential_xctest_outer_summaries_are_reconciled_cumulatively() {
    let log = b"Test Suite 'Selected tests' started at 2026-07-19 12:00:00.000.\nAlphaTests.swift:10: error: -[ExampleTests.AlphaTests testOne] : XCTAssertEqual failed\nTest Suite 'Selected tests' failed at 2026-07-19 12:00:00.001.\n\t Executed 1 test, with 1 failure (0 unexpected) in 0.001 seconds\nTest Suite 'Selected tests' started at 2026-07-19 12:00:01.000.\nBetaTests.swift:20: error: -[ExampleTests.BetaTests testTwo] : XCTAssertTrue failed\nTest Suite 'Selected tests' failed at 2026-07-19 12:00:01.001.\n\t Executed 1 test, with 1 failure (0 unexpected) in 0.001 seconds\n** TEST FAILED **\n";
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 2);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(document.failures_complete);
    assert_eq!(document.counts.tests_run, 2);
    assert_eq!(document.parse_status, ParseStatus::Partial);
}

#[test]
fn sequential_xctest_outer_summary_shortfall_is_incomplete() {
    let log = b"Test Suite 'Selected tests' started at 2026-07-19 12:00:00.000.\nAlphaTests.swift:10: error: -[ExampleTests.AlphaTests testOne] : XCTAssertEqual failed\nTest Suite 'Selected tests' failed at 2026-07-19 12:00:00.001.\n\t Executed 1 test, with 1 failure (0 unexpected) in 0.001 seconds\nTest Suite 'Selected tests' started at 2026-07-19 12:00:01.000.\nTest Suite 'Selected tests' failed at 2026-07-19 12:00:01.001.\n\t Executed 1 test, with 1 failure (0 unexpected) in 0.001 seconds\n** TEST FAILED **\n";
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn interleaved_xctest_outer_summaries_are_both_accounted() {
    let log = b"Test Suite 'Selected tests' started at 2026-07-19 12:00:00.000.\nTest Suite 'Selected tests' started at 2026-07-19 12:00:00.000.\nAlphaTests.swift:10: error: -[ExampleTests.AlphaTests testOne] : XCTAssertEqual failed\nTest Suite 'Selected tests' failed at 2026-07-19 12:00:00.001.\n\t Executed 1 test, with 1 failure (0 unexpected) in 0.001 seconds\nTest Suite 'Selected tests' failed at 2026-07-19 12:00:00.001.\n\t Executed 1 test, with 1 failure (0 unexpected) in 0.001 seconds\n** TEST FAILED **\n";
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn outer_xctest_end_without_its_summary_is_incomplete() {
    let log = b"Test Suite 'All tests' started at 2026-07-19 12:00:00.000.\nAlphaTests.swift:10: error: -[ExampleTests.AlphaTests testOne] : XCTAssertEqual failed\nTest Suite 'All tests' failed at 2026-07-19 12:00:00.001.\nsummary omitted by future reporter\n** TEST FAILED **\n";
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn terminal_failing_tests_list_catches_crash_only_failure() {
    let log = "◇ Test run started.\n◇ Suite ExampleTests started.\n✘ Test parsedFailure() recorded an issue at ExampleTests.swift:10:5: Expectation failed: 1 == 2\n✘ Test parsedFailure() failed after 0.001 seconds with 1 issue.\n✘ Suite ExampleTests failed after 0.001 seconds with 1 issue.\n✘ Test run with 2 tests in 1 suite failed after 0.001 seconds with 1 issue.\nFailing tests:\n\tExampleTests.parsedFailure()\n\tExampleTests.crashedWithoutIssue()\n\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn terminal_failing_tests_list_preserves_duplicate_identity_multiplicity() {
    let log = "◇ Test run started.\n✘ Test repeated() recorded an issue at ExampleTests.swift:10:5: Expectation failed: 1 == 2\nFailing tests:\n\tExampleTests.repeated()\n\tExampleTests.repeated()\n\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn swift_testing_restart_resets_unclosed_suite_stack() {
    let log = "◇ Test run started.\n◇ Suite FirstSuite started.\n✘ Test first() recorded an issue at FirstTests.swift:10:5: Expectation failed: 1 == 2\n✘ Test run with 1 test in 1 suite failed after 0.001 seconds with 1 issue.\n◇ Test run started.\n◇ Suite SecondSuite started.\n✘ Test second() recorded an issue at SecondTests.swift:20:5: Expectation failed: 2 == 3\n✘ Suite SecondSuite failed after 0.001 seconds with 1 issue.\n✘ Test run with 1 test in 1 suite failed after 0.001 seconds with 1 issue.\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 2);
    assert_eq!(
        document.failures[1].runner_test_id.as_deref(),
        Some("SecondSuite.second")
    );
    assert!(document.failures_complete);
}

#[test]
fn loose_swift_issue_marker_cannot_be_masked_by_strict_issue() {
    let log = "◇ Test run started.\n◇ Suite ExampleTests started.\n✘ Test parsed() recorded an issue at ExampleTests.swift:10:5: Expectation failed: 1 == 2\n✘ Test futureShape() recorded an issue using a future reporter shape\n✘ Test run with 2 tests in 1 suite failed after 0.001 seconds with 2 issues.\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn suite_issue_marker_and_parenthetical_totals_are_not_dropped() {
    let log = "◇ Test run started.\n◇ Suite ExampleSuite started.\n✘ Test parsed() recorded an issue at ExampleTests.swift:10:5: Expectation failed: 1 == 2\n✘ Suite ExampleSuite recorded an issue at ExampleTests.swift:20:5: Suite setup failed\n✘ Test parsed() failed after 0.001 seconds with 1 issue.\n✘ Suite ExampleSuite failed after 0.001 seconds with 3 issues (including 1 warning).\n✘ Test run with 1 test in 1 suite failed after 0.001 seconds with 3 issues (including 1 warning).\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn disjoint_swift_test_issue_summaries_catch_missing_issue_line() {
    let log = "◇ Test run started.\n✘ Test alpha() recorded an issue at AlphaTests.swift:10:5: Expectation failed: 1 == 2\n✘ Test alpha() failed after 0.001 seconds with 1 issue.\n✘ Test beta() failed after 0.001 seconds with 1 issue.\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn disjoint_swift_test_issue_summaries_are_complete_when_fully_extracted() {
    let log = "◇ Test run started.\n✘ Test alpha() recorded an issue at AlphaTests.swift:10:5: Expectation failed: 1 == 2\n✘ Test alpha() failed after 0.001 seconds with 1 issue.\n✘ Test beta() recorded an issue at BetaTests.swift:20:5: Expectation failed: 2 == 3\n✘ Test beta() failed after 0.001 seconds with 1 issue.\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 2);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(document.failures_complete);
    assert_eq!(document.counts.tests_run, 2);
    assert_eq!(document.parse_status, ParseStatus::Partial);
}

#[test]
fn disjoint_top_level_swift_suite_summaries_catch_missing_issue_line() {
    let log = "◇ Test run started.\n◇ Suite FirstSuite started.\n✘ Test first() recorded an issue at FirstTests.swift:10:5: Expectation failed: 1 == 2\n✘ Suite FirstSuite failed after 0.001 seconds with 1 issue.\n◇ Suite SecondSuite started.\n✘ Suite SecondSuite failed after 0.001 seconds with 1 issue.\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn disjoint_nested_swift_suite_summaries_are_additive_without_outer_summary() {
    let log = "◇ Test run started.\n◇ Suite OuterSuite started.\n◇ Suite FirstChild started.\n✘ Test first() recorded an issue at FirstTests.swift:10:5: Expectation failed: 1 == 2\n✘ Suite FirstChild failed after 0.001 seconds with 1 issue.\n◇ Suite SecondChild started.\n✘ Suite SecondChild failed after 0.001 seconds with 1 issue.\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn interleaved_swift_suite_scopes_are_conservatively_incomplete() {
    let log = "◇ Test run started.\n◇ Suite FirstSuite started.\n◇ Suite SecondSuite started.\n✘ Test first() recorded an issue at FirstTests.swift:10:5: Expectation failed: 1 == 2\n✘ Suite FirstSuite failed after 0.001 seconds with 1 issue.\n✘ Suite SecondSuite failed after 0.001 seconds with 1 issue.\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn duplicate_swift_suite_display_paths_are_conservatively_incomplete() {
    let log = "◇ Test run started.\n◇ Suite SharedName started.\n✘ Test first() recorded an issue at FirstTests.swift:10:5: Expectation failed: 1 == 2\n✘ Suite SharedName failed after 0.001 seconds with 1 issue.\n◇ Suite SharedName started.\n✘ Suite SharedName failed after 0.001 seconds with 1 issue.\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn lifo_parallel_swift_suite_events_are_not_assumed_to_be_nested() {
    let log = "◇ Test run started.\n◇ Suite FirstSuite started.\n◇ Suite SecondSuite started.\n✘ Test first() recorded an issue at FirstTests.swift:10:5: Expectation failed: 1 == 2\n✘ Suite SecondSuite failed after 0.001 seconds with 1 issue.\n✘ Suite FirstSuite failed after 0.001 seconds with 1 issue.\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn overlapping_swift_suite_scope_without_run_total_is_incomplete() {
    let log = "◇ Test run started.\n◇ Suite FirstSuite started.\n◇ Suite SecondSuite started.\n✘ Test second() recorded an issue at SecondTests.swift:10:5: Expectation failed: 1 == 2\n✘ Suite SecondSuite failed after 0.001 seconds with 1 issue.\n** TEST FAILED **\n".as_bytes();
    let mut sidecar = load_fixture("swift_testing_expect_failure").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn xcode_failure_coverage_uses_full_list_before_emission_budget() {
    let fixture = load_fixture("swift_testing_multi_expect");
    let document = analyze_log(
        &fixture.log,
        &fixture.sidecar,
        AnalyzeOptions {
            max_failures: Some(1),
        },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.emission.failures_total, 2);
    assert_eq!(document.emission.failures_emitted, 1);
    assert!(document.failures_complete);
    assert!(
        !document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
}

#[test]
fn build_failure_emits_pre_test_failed_with_missing_symbol() {
    let document = run_scenario("build_failure");
    assert_eq!(
        document.runner_outcome,
        failtrace::schema::RunnerOutcome::PreTestFailed
    );
    assert!(
        document
            .failures
            .iter()
            .any(|f| f.kind == failtrace::schema::FailureKind::MissingSymbol
                && f.message.contains("ExampleCatalog")),
        "expected a missing_symbol failure mentioning ExampleCatalog; got {:#?}",
        document.failures
    );
}

#[test]
fn swift_semantic_build_failure_classifies_known_swift_families() {
    let document = run_scenario("swift_semantic_build_failure");
    assert_eq!(
        document.runner_outcome,
        failtrace::schema::RunnerOutcome::PreTestFailed
    );
    assert_eq!(document.failures.len(), 6);

    assert_eq!(
        document
            .failures
            .iter()
            .filter(|f| f.kind == failtrace::schema::FailureKind::TypeError)
            .count(),
        5
    );
    assert_eq!(
        document
            .failures
            .iter()
            .filter(|f| f.kind == failtrace::schema::FailureKind::SyntaxError)
            .count(),
        1
    );
    assert_eq!(
        document
            .failures
            .iter()
            .filter(|f| f.message.contains("must be declared private because")
                && f.kind == failtrace::schema::FailureKind::TypeError)
            .count(),
        4,
        "expected all access-control private-type diagnostics to be TypeError: {:#?}",
        document.failures
    );
    assert_eq!(
        document
            .failures
            .iter()
            .filter(|f| f.message.contains("'try' cannot appear")
                && f.kind == failtrace::schema::FailureKind::SyntaxError)
            .count(),
        1,
        "expected the illegal-try diagnostic to be SyntaxError: {:#?}",
        document.failures
    );
    assert_eq!(
        document
            .failures
            .iter()
            .filter(|f| f
                .message
                .contains("can throw but expression is not marked with 'try'")
                && f.kind == failtrace::schema::FailureKind::TypeError)
            .count(),
        1,
        "expected the missing-try diagnostic to be TypeError: {:#?}",
        document.failures
    );
    assert!(
        document
            .failures
            .iter()
            .all(|f| f.kind != failtrace::schema::FailureKind::Unknown),
        "known Swift compiler diagnostic families must not lower to unknown: {:#?}",
        document.failures
    );
}

#[test]
fn swift_testing_expect_failure_emits_assertion_with_actual_and_expected() {
    let document = run_scenario("swift_testing_expect_failure");
    assert_eq!(
        document.runner_outcome,
        failtrace::schema::RunnerOutcome::Failed
    );
    assert_eq!(document.failures.len(), 1);
    let failure = &document.failures[0];
    assert_eq!(failure.kind, failtrace::schema::FailureKind::Assertion);
    assert_eq!(failure.phase, failtrace::schema::Phase::Test);
    assert!(
        failure.message.contains("Expectation failed"),
        "message should contain 'Expectation failed': {}",
        failure.message
    );
    if let failtrace::schema::Details::Assertion {
        actual, expected, ..
    } = &failure.details
    {
        assert_eq!(actual.as_deref(), Some("42"));
        assert_eq!(expected.as_deref(), Some("99"));
    } else {
        panic!("expected Details::Assertion, got {:?}", failure.details);
    }
}

fn interaction_document(log: &str, max_failures: Option<u64>) -> OutputDocument {
    let mut sidecar = load_fixture("green_passing").sidecar;
    sidecar.exit_code = Some(if log.contains("** TEST FAILED **") {
        65
    } else {
        0
    });
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log.as_bytes());
    let document = analyze_log(log.as_bytes(), &sidecar, AnalyzeOptions { max_failures }).unwrap();
    assert_eq!(validate(&document), Vec::new());
    document
}

fn xctest_occurrence(bundle: &str, extracted: usize, reported: usize, skipped: bool) -> String {
    let mut log = format!(
        "Test Suite 'Selected tests' started at 2026-01-01 00:00:00.000.\nTest Suite '{bundle}.xctest' started at 2026-01-01 00:00:00.000.\nTest Case '-[{bundle}.Checks testValue]' started.\n"
    );
    for incident in 0..extracted {
        log.push_str(&format!("Checks.swift:{}: error: -[{bundle}.Checks testValue] : XCTAssertEqual failed: incident {incident}\n", 10 + incident));
    }
    let outcome = if reported > 0 { "failed" } else { "passed" };
    log.push_str(&format!(
        "Test Case '-[{bundle}.Checks testValue]' {outcome} (0.001 seconds).\n"
    ));
    if skipped {
        log.push_str(&format!("Test Case '-[{bundle}.Checks testDisabled]' started.\nTest Case '-[{bundle}.Checks testDisabled]' skipped (0.001 seconds).\n"));
    }
    for name in [format!("{bundle}.xctest"), "Selected tests".to_owned()] {
        log.push_str(&format!("Test Suite '{name}' {outcome} at 2026-01-01 00:00:01.000.\n Executed {} tests, with {}{reported} failures (0 unexpected) in 0.001 seconds\n", if skipped { 2 } else { 1 }, if skipped { "1 test skipped and " } else { "" }));
    }
    log
}

fn assert_coverage(
    document: &OutputDocument,
    run: u64,
    skipped: u64,
    failures: usize,
    complete: bool,
) {
    assert_eq!(document.counts.tests_run, run);
    assert_eq!(document.counts.tests_skipped, skipped);
    assert_eq!(document.failures.len(), failures);
    assert_eq!(document.failures_complete, complete);
    if !complete {
        assert_eq!(document.parse_status, ParseStatus::Partial);
        assert!(
            document
                .parse_warnings
                .contains(&ParseWarning::RunnerSummaryMismatch)
        );
    }
}

#[test]
fn skipped_xctest_summary_retains_counts_with_missing_incident() {
    let log = xctest_occurrence("ExampleAppTests", 1, 2, true) + "** TEST FAILED **\n";
    assert_coverage(&interaction_document(&log, None), 2, 1, 1, false);
}

#[test]
fn skipped_xctest_summary_is_complete_with_every_incident() {
    let log = xctest_occurrence("ExampleAppTests", 2, 2, true) + "** TEST FAILED **\n";
    let doc = interaction_document(&log, None);
    assert_coverage(&doc, 2, 1, 2, true);
    assert_eq!(doc.parse_status, ParseStatus::Complete);
    assert_eq!(doc.counts.tests_failed, 1);
}

#[test]
fn two_bundles_retain_skips_and_missing_failure_evidence() {
    let log = xctest_occurrence("ExampleAppTests", 0, 0, true)
        + &xctest_occurrence("ExampleAppUITests", 0, 1, false)
        + "** TEST FAILED **\n";
    assert_coverage(&interaction_document(&log, None), 3, 1, 0, false);
}

#[test]
fn earlier_bundle_failure_evidence_survives_count_selection() {
    for extracted in [0, 1] {
        let log = xctest_occurrence("ExampleAppTests", extracted, 1, false)
            + &xctest_occurrence("ExampleAppTests", 0, 0, true)
            + "** TEST FAILED **\n";
        assert_coverage(
            &interaction_document(&log, None),
            2,
            1,
            extracted,
            extracted == 1,
        );
    }
}

#[test]
fn two_xctest_assertions_are_incidents_of_one_failed_test() {
    let log = xctest_occurrence("ExampleAppTests", 2, 2, false) + "** TEST FAILED **\n";
    let doc = interaction_document(&log, None);
    assert_coverage(&doc, 1, 0, 2, true);
    assert_eq!(doc.counts.tests_failed, 1);
    assert_eq!(doc.parse_status, ParseStatus::Complete);
}

#[test]
fn mixed_harness_extraction_cannot_cover_the_other_harness_shortfall() {
    for missing_in_swift in [true, false] {
        let mut log = xctest_occurrence(
            "ExampleAppTests",
            if missing_in_swift { 2 } else { 1 },
            2,
            true,
        );
        log.push_str("◇ Test run started.\n✘ Test alpha() recorded an issue at Checks.swift:30:5: Expectation failed: first\n");
        if !missing_in_swift {
            log.push_str("✘ Test alpha() recorded an issue at Checks.swift:31:5: Expectation failed: second\n");
        }
        log.push_str("✘ Test run with 1 test in 1 suite failed after 0.001 seconds with 2 issues.\n** TEST FAILED **\n");
        assert_coverage(&interaction_document(&log, None), 3, 1, 3, false);
    }
}

#[test]
fn known_issue_summary_counts_without_missing_failure_evidence() {
    let log = "◇ Test run started.\n◇ Suite ExampleSuite started.\n➜ Test disabled() skipped.\n━ Suite ExampleSuite passed after 0.001 seconds with 1 known issue.\n━ Test run with 2 tests in 1 suite passed after 0.001 seconds with 1 known issue.\n** TEST SUCCEEDED **\n";
    let doc = interaction_document(log, None);
    assert_coverage(&doc, 2, 1, 0, true);
    assert_eq!(doc.runner_outcome, RunnerOutcome::Passed);
    assert_eq!(doc.parse_status, ParseStatus::Complete);
}

#[test]
fn summary_prefix_preserves_totals_with_supported_and_unfamiliar_suffixes() {
    for suffix in [
        " after 0.001 seconds with 2 issues (including details).",
        " with a future summary format",
    ] {
        let log = format!(
            "◇ Test run started.\n➜ Test disabled() skipped.\n✘ Test alpha() recorded an issue at Checks.swift:10:5: Expectation failed: first\n✘ Test beta() recorded an issue with an unfamiliar location\n✘ Test run with 3 tests in 1 suite failed{suffix}\n** TEST FAILED **\n"
        );
        assert_coverage(&interaction_document(&log, None), 3, 1, 1, false);
    }
}

#[test]
fn restart_retains_earlier_issues_but_resets_suite_and_pending_skips() {
    let log = "◇ Test run started.\n◇ Suite Abandoned started.\n➜ Test abandonedSkip() skipped.\n✘ Test missing() recorded an issue with an unfamiliar location\nRestarting after unexpected exit, crash, or test timeout; summary will include totals from previous launches.\n◇ Test run started.\n✘ Test later() recorded an issue at Checks.swift:20:5: Expectation failed: later\n✘ Test run with 1 test in 1 suite failed after 0.001 seconds with 1 issue.\n** TEST FAILED **\n";
    let doc = interaction_document(log, None);
    assert_coverage(&doc, 1, 0, 1, false);
    assert_eq!(doc.failures[0].runner_test_id.as_deref(), Some("later"));
}

#[test]
fn terminal_only_and_duplicate_identities_do_not_synthesize_failures() {
    for extracted in [0, 1] {
        let mut log = "◇ Test run started.\n".to_owned();
        if extracted == 1 {
            log.push_str("✘ Test alpha() recorded an issue at Checks.swift:10:5: Expectation failed: value\n");
        }
        log.push_str("✘ Test run with 2 tests in 1 suite failed after 0.001 seconds.\nFailing tests:\n\talpha()\n\talpha()\n\n** TEST FAILED **\n");
        assert_coverage(&interaction_document(&log, None), 2, 0, extracted, false);
    }
}

#[test]
fn zero_width_prefix_keeps_failure_coverage_and_raw_evidence() {
    let plain = xctest_occurrence("ExampleAppTests", 2, 2, true) + "** TEST FAILED **\n";
    let decorated = plain
        .lines()
        .map(|line| format!("\u{200b}{line}\n"))
        .collect::<String>();
    let a = interaction_document(&plain, None);
    let b = interaction_document(&decorated, None);
    assert_coverage(&b, 2, 1, 2, true);
    assert_eq!(a.counts, b.counts);
    for (original, failure) in a.failures.iter().zip(&b.failures) {
        assert_eq!(original.failure_id, failure.failure_id);
        for excerpt in &failure.evidence.as_ref().unwrap().excerpts {
            let raw = &decorated.as_bytes()
                [excerpt.byte_range.start as usize..excerpt.byte_range.end as usize];
            assert_eq!(
                String::from_utf8_lossy(raw).trim_end(),
                excerpt.text.trim_end()
            );
            assert!(excerpt.text.starts_with('\u{200b}'));
        }
    }
}

#[test]
fn budget_does_not_change_combined_prebudget_coverage() {
    for reported in [2, 3] {
        let log = xctest_occurrence("ExampleAppTests", 2, reported, true) + "** TEST FAILED **\n";
        let full = interaction_document(&log, None);
        let budget = interaction_document(&log, Some(1));
        assert_eq!(full.failures_complete, reported == 2);
        assert_eq!(full.failures_complete, budget.failures_complete);
        assert_eq!(full.counts, budget.counts);
        assert_eq!(budget.emission.failures_total, 2);
        assert_eq!(budget.failures.len(), 1);
    }
}

#[test]
fn count_only_mismatch_can_leave_fully_extracted_failures_complete() {
    let log = xctest_occurrence("ExampleAppTests", 1, 1, false)
        .replace(
            "Test Case '-[ExampleAppTests.Checks testValue]' started.\n",
            "",
        )
        .replace(
            "Test Case '-[ExampleAppTests.Checks testValue]' failed (0.001 seconds).\n",
            "",
        )
        + "** TEST FAILED **\n";
    let doc = interaction_document(&log, None);
    assert_coverage(&doc, 1, 0, 1, true);
    assert_eq!(doc.parse_status, ParseStatus::Partial);
    assert_eq!(
        doc.parse_warnings,
        vec![ParseWarning::RunnerSummaryMismatch]
    );
}

#[test]
fn synthetic_green_passing_keeps_full_and_budgeted_completeness() {
    let fixture = load_fixture("green_passing");
    for budget in [None, Some(0)] {
        let doc = interaction_document(std::str::from_utf8(&fixture.log).unwrap(), budget);
        assert!(doc.failures_complete);
        assert_eq!(doc.runner_outcome, RunnerOutcome::Passed);
        assert_eq!(doc.parse_status, ParseStatus::Complete);
    }
}

#[test]
fn restart_without_terminal_keeps_both_degradation_witnesses() {
    let doc = interaction_document(
        "◇ Test run started.\nRestarting after unexpected exit, crash, or test timeout; summary will include totals from previous launches.\n◇ Test run started.\n✔ Test run with 1 test in 1 suite passed after 0.001 seconds.\n",
        None,
    );
    assert_eq!(doc.parse_status, ParseStatus::Partial);
    assert!(!doc.failures_complete);
    assert_eq!(
        doc.parse_warnings,
        vec![
            ParseWarning::MissingTerminalEvent,
            ParseWarning::RunnerSummaryMismatch
        ]
    );
}

fn run_scenario(scenario: &str) -> failtrace::schema::OutputDocument {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/xcodebuild")
        .join(scenario);
    let log = fs::read(root.join("run/log")).unwrap();
    let sidecar: Sidecar =
        serde_json::from_slice(&fs::read(root.join("run/meta.json")).unwrap()).unwrap();
    analyze_log(&log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap()
}

/// One-shot generator that writes `expected.json` for every fixture in
/// [`SCENARIOS`] using the same shape `gotest_adapter.rs` consumes. Invoke
/// explicitly with `cargo test --test xcodebuild_adapter -- --ignored
/// generate_expected_jsons`. The output is hand-tunable afterward — drop
/// fields, add `captured_environment` keys, etc.
#[test]
#[ignore = "one-shot fixture generator; invoke with --ignored"]
fn generate_expected_jsons() {
    use failtrace::schema::normalize_message;
    use serde_json::{Value, json};
    for scenario in SCENARIOS {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/xcodebuild")
            .join(scenario);
        let log = fs::read(root.join("run/log")).unwrap();
        let mut sidecar: Sidecar =
            serde_json::from_slice(&fs::read(root.join("run/meta.json")).unwrap()).unwrap();
        sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(&log);
        fs::write(
            root.join("run/meta.json"),
            serde_json::to_string_pretty(&sidecar).unwrap() + "\n",
        )
        .unwrap();
        let document = analyze_log(&log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();
        let actual = serde_json::to_value(&document).unwrap();

        // Build hash_inputs.failures: the (failure_id + derivation-inputs)
        // tuple for each emitted failure. `normalized_message` must be the
        // OUTPUT of normalize_message(raw, cwd), not the raw message —
        // verify_fixtures.py independently re-hashes from these inputs.
        let mut hash_failures = Vec::new();
        for failure in &document.failures {
            let primary = &failure.primary_location;
            hash_failures.push(json!({
                "adapter_id": failure.adapter_id,
                "phase": failure.phase.to_string(),
                "kind": failure.kind.to_string(),
                "runner_test_id": failure.runner_test_id,
                "param_id": failure.param_id,
                "primary_location": {
                    "file": primary.file,
                    "line": primary.line,
                    "column": primary.column,
                },
                "normalized_message": normalize_message(&failure.message, &sidecar.cwd),
                "failure_id": failure.failure_id,
            }));
        }

        let expected = json!({
            "scenario": scenario,
            "captured_environment": {
                "provenance": "hand-authored synthetic log",
                "format": "representative xcodebuild and Swift Testing output",
            },
            "runner_argv": sidecar.argv,
            "exit_code": sidecar.exit_code,
            "hash_inputs": {
                "schema_version": "1.6",
                "normative": false,
                "test_group_id": Value::Null,
                "failure_order_stable": document.ranking.is_stable_across_runs,
                "failures": hash_failures,
            },
            "expected_subset": actual,
        });

        let out = serde_json::to_string_pretty(&expected).unwrap() + "\n";
        let path = root.join("expected.json");
        fs::write(&path, out).unwrap();
        eprintln!("wrote {}", path.display());
    }
}

#[test]
fn sim_launch_failure_locks_harness_crashed_and_missing_terminal_event() {
    // Sim/IPC failure happened before any test phase started. Adapter must:
    //   - lower runner_outcome to HarnessCrashed (not Failed, not Passed)
    //   - emit parse_warnings: [MissingTerminalEvent] so the path can't
    //     silently regress to a passing analysis
    //   - emit zero failures (no parse path extracts the Mach error text;
    //     this is deliberately the runtime-environment-failure code path)
    use failtrace::schema::{ParseWarning, RunnerOutcome};
    let document = run_scenario("sim_launch_failure");
    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert!(
        document
            .parse_warnings
            .iter()
            .any(|w| matches!(w, ParseWarning::MissingTerminalEvent)),
        "expected parse_warnings to include MissingTerminalEvent; got {:?}",
        document.parse_warnings
    );
    assert_eq!(document.failures.len(), 0);
}

#[test]
fn destination_resolution_failure_lowers_to_setup_dependency_error_with_reruns() {
    use failtrace::schema::{CommandIntent, Details, FailureKind, Phase, RunnerOutcome};
    let document = run_scenario("destination_resolution_failure");
    assert_eq!(document.runner_outcome, RunnerOutcome::PreTestFailed);
    assert_eq!(
        document.parse_status,
        failtrace::schema::ParseStatus::Complete
    );
    assert_eq!(document.failures.len(), 1);
    let failure = &document.failures[0];
    assert_eq!(failure.phase, Phase::Setup);
    assert_eq!(failure.kind, FailureKind::DependencyResolutionError);
    assert_eq!(
        failure.message,
        "xcodebuild: error: Unable to find a device matching the provided destination specifier:"
    );
    assert_eq!(failure.primary_location.file, "<xcodebuild>");
    assert_eq!(failure.primary_location.line, None);
    assert_eq!(failure.primary_location.column, None);
    assert_eq!(
        failure.raw_log_lines,
        [12, 19],
        "failure evidence must span the full destination-resolution block"
    );
    assert!(matches!(
        &failure.details,
        Details::DependencyResolutionError {
            package: None,
            resolver: failtrace::schema::Resolver::Unknown
        }
    ));
    let excerpt = &failure.evidence.as_ref().unwrap().excerpts[0].text;
    assert!(excerpt.contains("{ platform:iOS Simulator, OS:latest, name:Unavailable Phone }"));
    assert!(excerpt.contains("Available destinations for the \"ExampleApp\" scheme:"));
    assert!(excerpt.contains("name:Example Tablet"));

    assert_eq!(document.suggested_next_commands.len(), 2);
    for command in &document.suggested_next_commands {
        assert_eq!(command.intent, CommandIntent::RerunIsolated);
        assert!(!command.interactive);
        assert!(!command.mutates_workspace);
        assert!(!command.uses_network);
        assert!(!command.requires_approval);
        let destination_index = command
            .argv
            .iter()
            .position(|arg| arg == "-destination")
            .expect("expected separate -destination token");
        let destination = &command.argv[destination_index + 1];
        assert!(destination.starts_with("platform=iOS Simulator,id="));
        assert!(
            !destination.contains("placeholder"),
            "suggestions must use concrete simulator ids"
        );
        assert!(
            command
                .command
                .contains("-destination 'platform=iOS Simulator,id=")
        );
    }
}
