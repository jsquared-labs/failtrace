use std::{fs, path::Path};

use failtrace::{
    adapters::{AdapterError, AnalyzeOptions, cargo::analyze_log},
    schema::{
        FailureKind, LocationRole, OutputDocument, ParseStatus, ParseWarning, Phase,
        RankingStrategy, RunnerOutcome, validate,
    },
    storage::Sidecar,
};
use serde_json::Value;

const SCENARIOS: &[&str] = &[
    "borrow_checker_e0502",
    "trait_resolution_e0277",
    "dependency_resolution",
    "build_script_panic",
    "runtime_assertion",
    "runtime_panic",
    "should_panic_mismatch",
    "doctest_failure",
    "workspace_multi_binary",
    "green_passing",
    "clap_macros_assertion",
];

#[test]
fn cargo_adapter_reproduces_fixture_hashes_and_valid_documents() {
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
        [
            "013ab298536da477",
            "194a6dbf548c69fd",
            "52410b235114d751",
            "5998b617019fa9d2",
            "5df14fde59dae1f9",
            "6637ca33ea1a36d7",
            "6e21bb5afb48a42e",
            "796acd601500dfb3",
            "8a209dd25158395f",
            "b68f824c74595445",
        ]
    );
}

#[test]
fn borrow_checker_spans_match_normative_shape() {
    let document = analyze_fixture("borrow_checker_e0502");
    let failure = &document.failures[0];

    assert_eq!(document.runner_outcome, RunnerOutcome::PreTestFailed);
    assert_eq!(
        document.ranking.strategy,
        RankingStrategy::CompileBeforeRuntime
    );
    assert!(document.ranking.is_stable_across_runs);
    assert_eq!(failure.phase, Phase::Build);
    assert_eq!(failure.kind, FailureKind::TypeError);
    assert_eq!(failure.diagnostic_code.as_deref(), Some("E0502"));
    assert_eq!(failure.primary_location.file, "src/lib.rs");
    assert_eq!(failure.primary_location.line, Some(4));
    assert_eq!(failure.primary_location.column, Some(5));
    assert_eq!(failure.primary_location.end_column, Some(17));
    assert_eq!(failure.extra_locations.len(), 2);
    assert_eq!(failure.extra_locations[0].role, LocationRole::Related);
    assert_eq!(
        document.suggested_next_commands[0].command,
        "rustc --explain E0502"
    );
}

#[test]
fn runtime_failures_start_after_first_harness_boundary() {
    let document = analyze_fixture("workspace_multi_binary");

    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert_eq!(document.counts.tests_run, 2);
    assert_eq!(document.counts.tests_passed, 1);
    assert_eq!(document.counts.tests_failed, 1);
    assert!(!document.ranking.is_stable_across_runs);
    assert_eq!(
        document.failures[0].runner_test_id.as_deref(),
        Some("tests::beta_fails")
    );
    assert_eq!(
        document.failures[0].primary_location.file,
        "crates/beta/src/lib.rs"
    );
}

#[test]
fn argv_test_threads_one_marks_runtime_ranking_stable() {
    let fixture = load_fixture("runtime_assertion");
    let mut sidecar = fixture.sidecar;
    sidecar.command = "cargo test -- --test-threads=1".to_owned();
    let document = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(document.ranking.strategy, RankingStrategy::FirstEmitted);
    assert!(document.ranking.is_stable_across_runs);
}

#[test]
fn rust_test_threads_env_is_not_inferred_from_log_or_sidecar() {
    let fixture = load_fixture("runtime_assertion");
    let mut sidecar = fixture.sidecar;
    sidecar.command = "RUST_TEST_THREADS=1 cargo test".to_owned();
    let document = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(document.ranking.strategy, RankingStrategy::FirstEmitted);
    assert!(!document.ranking.is_stable_across_runs);
}

#[test]
fn doctest_failure_is_test_phase_not_missing_harness_partial() {
    let document = analyze_fixture("doctest_failure");

    assert_eq!(document.parse_status, ParseStatus::Complete);
    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert_eq!(
        document.failures[0].runner_test_id.as_deref(),
        Some("src/lib.rs - next (line 3)")
    );
    assert_eq!(document.failures[0].phase, Phase::Test);
}

#[test]
fn upstream_build_failure_with_no_doctest_block_is_complete() {
    let document = analyze_fixture("borrow_checker_e0502");

    assert_eq!(document.parse_status, ParseStatus::Complete);
    assert!(document.parse_warnings.is_empty());
    assert_eq!(document.counts.tests_run, 0);
}

#[test]
fn message_format_json_is_detected_but_rejected_with_default_reporter_suggestion() {
    let log = b"   Compiling demo v0.1.0 (/tmp/demo)\n{\"reason\":\"compiler-message\"}\nerror: could not compile `demo` (lib) due to 1 previous error\n";
    let mut sidecar = load_fixture("borrow_checker_e0502").sidecar;
    sidecar.command = "cargo test --message-format=json".to_owned();
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.parse_status, ParseStatus::UnparsedOutput);
    assert!(document.failures.is_empty());
    assert_eq!(document.suggested_next_commands[0].command, "cargo test");
}

#[test]
fn message_format_json_space_form_is_rejected() {
    let log = b"{\"reason\":\"build-finished\",\"success\":false}\n";
    let mut sidecar = load_fixture("borrow_checker_e0502").sidecar;
    sidecar.command = "cargo test --message-format json".to_owned();
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.parse_status, ParseStatus::UnparsedOutput);
    assert_eq!(document.suggested_next_commands[0].argv, ["cargo", "test"]);
}

#[test]
fn bare_rustc_error_is_admitted_when_sidecar_pins_cargo_test() {
    let log = b"error[E0502]: cannot borrow `data` as mutable because it is also borrowed as immutable\n --> src/lib.rs:4:5\n  |\n4 |     data.push(4);\n  |     ^^^^^^^^^^^^ mutable borrow occurs here\n";
    let mut sidecar = load_fixture("borrow_checker_e0502").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::PreTestFailed);
    assert_eq!(
        document.failures[0].diagnostic_code.as_deref(),
        Some("E0502")
    );
}

#[test]
fn unrelated_log_is_rejected_at_admission() {
    let log = b"hello from a different program\n";
    let mut sidecar = load_fixture("green_passing").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let result = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None });

    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { .. })
    ));
}

#[test]
fn trait_resolution_lowers_to_type_error() {
    let document = analyze_fixture("trait_resolution_e0277");
    let failure = &document.failures[0];

    assert_eq!(failure.phase, Phase::Build);
    assert_eq!(failure.kind, FailureKind::TypeError);
    assert_eq!(failure.diagnostic_code.as_deref(), Some("E0277"));
    assert_eq!(failure.primary_location.line, Some(8));
    assert_eq!(failure.primary_location.column, Some(19));
}

#[test]
fn dependency_resolution_uses_cargo_resolver_details() {
    let document = analyze_fixture("dependency_resolution");
    let failure = &document.failures[0];

    assert_eq!(failure.phase, Phase::Build);
    assert_eq!(failure.kind, FailureKind::DependencyResolutionError);
    assert_eq!(failure.primary_location.file, "Cargo.toml");
    assert_eq!(document.runner_outcome, RunnerOutcome::PreTestFailed);
}

#[test]
fn build_script_panic_is_pre_test_failed_build_panic() {
    let document = analyze_fixture("build_script_panic");
    let failure = &document.failures[0];

    assert_eq!(failure.phase, Phase::Build);
    assert_eq!(failure.kind, FailureKind::Panic);
    assert_eq!(failure.primary_location.file, "build.rs");
    assert_eq!(
        failure.message,
        "build script panic: generated bindings are missing"
    );
}

#[test]
fn runtime_assertion_counts_as_failed_test() {
    let document = analyze_fixture("runtime_assertion");

    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert_eq!(document.failures[0].kind, FailureKind::Assertion);
    assert_eq!(document.counts.tests_failed, 1);
    assert_eq!(document.counts.tests_errored, 0);
}

#[test]
fn runtime_panic_counts_as_errored_test() {
    let document = analyze_fixture("runtime_panic");

    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert_eq!(document.failures[0].kind, FailureKind::Panic);
    assert_eq!(document.counts.tests_failed, 0);
    assert_eq!(document.counts.tests_errored, 1);
}

#[test]
fn should_panic_mismatch_counts_as_assertion_failure() {
    let document = analyze_fixture("should_panic_mismatch");

    assert_eq!(document.failures[0].kind, FailureKind::Assertion);
    assert_eq!(document.failures[0].message, "actual panic text");
    assert_eq!(document.counts.tests_failed, 1);
}

#[test]
fn green_passing_lowers_to_passed_with_no_failures() {
    let document = analyze_fixture("green_passing");

    assert_eq!(document.runner_outcome, RunnerOutcome::Passed);
    assert_eq!(document.parse_status, ParseStatus::Complete);
    assert_eq!(document.ranking.strategy, RankingStrategy::None);
    assert!(document.failures.is_empty());
    assert_eq!(document.counts.tests_run, 1);
    assert_eq!(document.counts.tests_passed, 1);
}

#[test]
fn budget_zero_preserves_full_failure_count() {
    let fixture = load_fixture("workspace_multi_binary");
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
    assert_eq!(document.emission.failures_total, 1);
    assert!(document.failures.is_empty());
}

// F4 path: the libtest summary reports failed tests but no `---- <name> stdout
// ----` failure block could be parsed, so the failure list is empty. The outcome
// must still be Failed (summary_reports_failures), with the summary/evidence
// disagreement observable as RunnerSummaryMismatch and the list marked
// incomplete — not HarnessCrashed and not a silently complete parse.
#[test]
fn summary_reports_failures_without_block_lowers_to_failed_with_mismatch() {
    let log = b"running 1 test\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
    let mut sidecar = load_fixture("runtime_assertion").sidecar;
    sidecar.exit_code = Some(101);
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert!(document.failures.is_empty());
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch),
        "summary/evidence disagreement must be observable: got {:?}",
        document.parse_warnings
    );
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(!document.failures_complete);
    assert_eq!(validate(&document), Vec::new());
}

#[test]
fn partially_extracted_failure_list_is_marked_incomplete() {
    let log = b"running 2 tests\ntest tests::one ... FAILED\ntest tests::two ... FAILED\n\nfailures:\n\n---- tests::one stdout ----\n\nthread 'tests::one' panicked at src/lib.rs:3:5:\nassertion failed: one\n\nfailures:\n    tests::one\n    tests::two\n\ntest result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
    let mut sidecar = load_fixture("runtime_assertion").sidecar;
    sidecar.exit_code = Some(101);
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn later_aborted_harness_cannot_borrow_an_earlier_targets_summary() {
    let log = b"running 1 test\ntest tests::ordinary ... FAILED\n\nfailures:\n\n---- tests::ordinary stdout ----\nthread 'tests::ordinary' panicked at src/lib.rs:3:5:\nassertion failed: ordinary\n\nfailures:\n    tests::ordinary\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\nerror: test failed, to rerun pass `--lib`\n\nrunning 1 test\ntest aborts_process ... error: test failed, to rerun pass `--test aborting`\n";
    let mut sidecar = load_fixture("runtime_assertion").sidecar;
    sidecar.command = "cargo test --no-fail-fast".to_owned();
    sidecar.exit_code = Some(101);
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert_eq!(document.failures.len(), 1);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::MissingSummaryLine)
    );
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(!document.failures_complete);
}

#[test]
fn bannerless_failed_target_cannot_borrow_an_earlier_targets_summary() {
    let log = b"running 1 test\ntest tests::ordinary ... FAILED\n\nfailures:\n\n---- tests::ordinary stdout ----\nthread 'tests::ordinary' panicked at src/lib.rs:3:5:\nassertion failed: ordinary\n\nfailures:\n    tests::ordinary\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\nerror: test failed, to rerun pass `--lib`\nerror: test failed, to rerun pass `--test custom-harness`\nerror: 2 targets failed:\n    `--lib`\n    `--test custom-harness`\n";
    let mut sidecar = load_fixture("runtime_assertion").sidecar;
    sidecar.command = "cargo test --no-fail-fast".to_owned();
    sidecar.exit_code = Some(101);
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
fn partially_extracted_rustc_diagnostics_are_marked_incomplete() {
    let log = b"   Compiling demo v0.1.0 (/tmp/demo)\nerror[E0308]: mismatched types\n --> src/lib.rs:3:5\n  |\n3 |     value\n  |     ^^^^^ expected u32, found &str\nerror: future compiler diagnostic without a parseable span\nnote: reporter-specific context\nerror: could not compile `demo` (lib) due to 2 previous errors\n";
    let mut sidecar = load_fixture("borrow_checker_e0502").sidecar;
    sidecar.exit_code = Some(101);
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::PreTestFailed);
    assert_eq!(document.failures.len(), 1);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(!document.failures_complete);
}

fn analyze_fixture(scenario: &str) -> OutputDocument {
    let fixture = load_fixture(scenario);
    analyze_log(
        &fixture.log,
        &fixture.sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap_or_else(|error| panic!("{scenario}: analyze_log failed: {error}"))
}

struct Fixture {
    log: Vec<u8>,
    expected: Value,
    sidecar: Sidecar,
}

fn load_fixture(scenario: &str) -> Fixture {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cargo")
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
