use std::{fs, path::Path};

use failtrace::{
    adapters::{AdapterError, AnalyzeOptions, pytest::analyze_log},
    schema::{
        Adapter, AdapterKind, FailureIdParts, FailureKind, Location, LocationRole, OutputDocument,
        ParseStatus, ParseWarning, Phase, RankingStrategy, RunnerOutcome, SelectionReason,
        derive_test_group_id, failure_id, strip_params_pytest, validate,
    },
    storage::{PathCaseFolding, Sidecar, StreamMerge, sha256_hex},
};
use serde_json::Value;

const SCENARIOS: &[&str] = &[
    "basic_assertion",
    "collection_error",
    "parameterized",
    "quiet_early_exit_assertion",
    "teardown_exception",
    "sqlite_foreign_key_error",
    "banner_only_no_summary",
    "green_summary_nonzero_exit",
    "truncated_capture",
];

#[test]
fn pytest_adapter_reproduces_fixture_hashes_and_valid_documents() {
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

        if let Some(expected_group_id) = fixture.expected["hash_inputs"]["test_group_id"].as_str() {
            let runner_test_id = document.failures[0].runner_test_id.as_deref().unwrap();
            let got = derive_test_group_id("pytest", runner_test_id, strip_params_pytest);
            assert_eq!(got, expected_group_id, "{scenario}: test_group_id");
            assert_eq!(
                document.failures[0].test_group_id.as_deref(),
                Some(expected_group_id),
                "{scenario}: emitted test_group_id"
            );
            reproduced.push(expected_group_id.to_owned());
        }
    }

    reproduced.sort();
    assert_eq!(
        reproduced,
        [
            "29dc8e69aeb2978d",
            "31e23017a78e7ba0",
            "49507ef118ce763f",
            "4b78b28e9378f472",
            "58af4b4b1fcb1036",
            "6f4cda653abb2c1c",
            "a28a10bf9c212128",
            "c345c28f273400ef",
            "e8247590883c29f6",
        ]
    );
}

/// Regenerate the hand-authored SQLite fixture after deliberately changing its
/// synthetic log. Invoke explicitly with:
/// `cargo test --test pytest_adapter generate_sqlite_foreign_key_expected -- --ignored --exact`.
#[test]
#[ignore = "one-shot synthetic fixture generator"]
fn generate_sqlite_foreign_key_expected() {
    use failtrace::schema::normalize_message;
    use serde_json::{Value, json};

    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pytest/sqlite_foreign_key_error");
    let log = fs::read(root.join("run/log")).unwrap();
    let mut sidecar: Sidecar =
        serde_json::from_slice(&fs::read(root.join("run/meta.json")).unwrap()).unwrap();
    sidecar.raw_log_sha256 = sha256_hex(&log);
    fs::write(
        root.join("run/meta.json"),
        serde_json::to_string_pretty(&sidecar).unwrap() + "\n",
    )
    .unwrap();

    let document = analyze_log(&log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();
    let hash_failures = document
        .failures
        .iter()
        .map(|failure| {
            let primary = &failure.primary_location;
            json!({
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
            })
        })
        .collect::<Vec<_>>();
    let expected = json!({
        "scenario": "sqlite_foreign_key_error",
        "captured_environment": {
            "provenance": "hand-authored synthetic log",
            "format": "representative pytest output",
        },
        "runner_argv": sidecar.argv,
        "exit_code": sidecar.exit_code,
        "hash_inputs": {
            "schema_version": "1.6",
            "normative": false,
            "test_group_id": Value::Null,
            "failures": hash_failures,
        },
        "expected_subset": serde_json::to_value(&document).unwrap(),
        "notes": "Synthetic SQLite foreign-key exception in a test body.",
    });
    fs::write(
        root.join("expected.json"),
        serde_json::to_string_pretty(&expected).unwrap() + "\n",
    )
    .unwrap();
}

#[test]
fn pytest_adapter_replay_of_v14_sidecar_emits_current_binary_owned_fields() {
    let fixture = load_fixture("basic_assertion");
    let mut sidecar = fixture.sidecar;
    sidecar.meta_schema_version = "1.1".to_owned();
    sidecar.schema_version = "1.4".to_owned();
    sidecar.failtrace_version = "legacy-binary".to_owned();
    sidecar.command = "legacy pytest".to_owned();
    sidecar.started_at = "2026-05-16T12:00:00Z".to_owned();
    sidecar.duration_ms = 42;

    let document = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.schema_version, "1.6");
    assert_eq!(document.failtrace_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        document.document_kind,
        failtrace::schema::DocumentKind::AnalysisResult
    );
    assert_eq!(document.command, "legacy pytest");
    assert_eq!(document.started_at, "2026-05-16T12:00:00Z");
    assert_eq!(document.duration_ms, 42);
}

#[test]
fn max_failures_projects_first_emitted_budget() {
    let fixture = load_fixture("parameterized");
    let document = analyze_log(
        &fixture.log,
        &fixture.sidecar,
        AnalyzeOptions {
            max_failures: Some(2),
        },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(
        document.emission.mode,
        failtrace::schema::EmissionMode::Budgeted
    );
    assert_eq!(document.emission.max_failures, Some(2));
    assert_eq!(document.emission.failures_emitted, 2);
    assert_eq!(document.emission.failures_total, 3);
    assert_eq!(
        document
            .failures
            .iter()
            .map(|failure| failure.failure_id.as_str())
            .collect::<Vec<_>>(),
        ["58af4b4b1fcb1036", "4b78b28e9378f472"]
    );
}

#[test]
fn pytest_failure_id_hashes_normalized_message() {
    let cwd = "/tmp/failtrace-normalize";
    let log = format!(
        "============================= test session starts ==============================\n\
platform darwin -- Python 3.11.15, pytest-8.4.2, pluggy-1.6.0\n\
rootdir: {cwd}\n\
collected 1 item\n\n\
tests/test_numbers.py F                                                    [100%]\n\n\
=================================== FAILURES ===================================\n\
________________________________ test_numbers _________________________________\n\n\
    def test_numbers():\n\
>       assert False\n\
E       AssertionError: saw {cwd}/src/value.py at 0xFF after 42 retries\n\n\
tests/test_numbers.py:4: AssertionError\n\
=========================== short test summary info ============================\n\
FAILED tests/test_numbers.py::test_numbers - AssertionError: saw {cwd}/src/value.py at 0xFF after 42 retries\n\
============================== 1 failed in 0.01s ===============================\n"
    );
    let mut sidecar = synthetic_sidecar(log.as_bytes(), Some(1));
    sidecar.cwd = cwd.to_owned();
    sidecar.raw_log_sha256 = sha256_hex(log.as_bytes());

    let document = analyze_log(
        log.as_bytes(),
        &sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();
    let failure = &document.failures[0];
    let expected = failure_id(FailureIdParts {
        adapter_id: "pytest",
        phase: Phase::Test,
        kind: FailureKind::Assertion,
        runner_test_id: Some("tests/test_numbers.py::test_numbers"),
        primary_location: &Location {
            file: "tests/test_numbers.py".to_owned(),
            line: Some(4),
            column: None,
            end_line: None,
            end_column: None,
            role: LocationRole::Primary,
            in_user_code: true,
        },
        param_id: None,
        message: "AssertionError: saw /tmp/failtrace-normalize/src/value.py at 0xFF after 42 retries",
        cwd,
    });

    assert_eq!(
        failure.message,
        "AssertionError: saw /tmp/failtrace-normalize/src/value.py at 0xFF after 42 retries"
    );
    assert_eq!(failure.failure_id, expected);
}

#[test]
fn quiet_green_lowers_to_complete_passed_document() {
    let log = b"..                                                                       [100%]\n2 passed in 0.00s\n";
    let document = analyze_log(
        log,
        &synthetic_sidecar(log, Some(0)),
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.parse_status, ParseStatus::Complete);
    assert_eq!(document.parse_warnings, Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::Passed);
    assert_eq!(document.ranking.strategy, RankingStrategy::None);
    assert_eq!(document.counts.tests_run, 2);
    assert_eq!(document.counts.tests_passed, 2);
    assert_eq!(document.counts.tests_failed, 0);
    assert_eq!(document.counts.tests_errored, 0);
    assert_eq!(document.counts.tests_skipped, 0);
}

#[test]
fn verbose_pytest_9_green_lowers_to_complete_passed_document() {
    let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-9.0.3, pluggy-1.6.0\ncollected 2 items\n\ntest_basic.py ..                                                         [100%]\n\n============================== 2 passed in 0.00s ===============================\n";
    let document = analyze_log(
        log,
        &synthetic_sidecar(log, Some(0)),
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.parse_status, ParseStatus::Complete);
    assert_eq!(document.parse_warnings, Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::Passed);
    assert_eq!(document.counts.tests_run, 2);
    assert_eq!(document.counts.tests_passed, 2);
}

#[test]
fn unsupported_pytest_majors_stay_complete_with_version_warning() {
    for version in ["7.0.0", "10.0.0"] {
        let log = format!(
            "============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-{version}, pluggy-1.6.0\ncollected 2 items\n\ntest_basic.py ..                                                         [100%]\n\n============================== 2 passed in 0.00s ===============================\n"
        );
        let document = analyze_log(
            log.as_bytes(),
            &synthetic_sidecar(log.as_bytes(), Some(0)),
            AnalyzeOptions { max_failures: None },
        )
        .unwrap();

        assert_eq!(validate(&document), Vec::new(), "{version}");
        assert_eq!(document.parse_status, ParseStatus::Complete, "{version}");
        assert_eq!(
            document.parse_warnings,
            vec![ParseWarning::RunnerVersionUnsupported],
            "{version}"
        );
    }
}

#[test]
fn quiet_green_with_warning_summary_lowers_to_complete_passed_document() {
    let log = b"..                                                                       [100%]\n2 passed, 1 warning in 0.05s\n";
    let document = analyze_log(
        log,
        &synthetic_sidecar(log, Some(0)),
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.parse_status, ParseStatus::Complete);
    assert_eq!(document.parse_warnings, Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::Passed);
    assert_eq!(document.ranking.strategy, RankingStrategy::None);
    assert_eq!(document.counts.tests_run, 2);
    assert_eq!(document.counts.tests_passed, 2);
    assert_eq!(document.counts.tests_failed, 0);
}

#[test]
fn banner_only_exit_1_lowers_to_harness_crashed_with_missing_summary() {
    let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2, pluggy-1.6.0\nrootdir: /tmp/proj\ncollected 1 item\n";
    let document = analyze_synthetic(log, Some(1), None);

    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert_missing_summary_crash(&document);
    assert_eq!(document.counts.tests_passed, 0);
}

#[test]
fn banner_only_sigkill_lowers_to_harness_crashed_with_missing_summary() {
    let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2, pluggy-1.6.0\nrootdir: /tmp/proj\ncollected 1 item\n";
    let document = analyze_synthetic(log, None, Some("SIGKILL"));

    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert_missing_summary_crash(&document);
}

#[test]
fn signal_after_failed_summary_marks_failure_list_incomplete() {
    let fixture = load_fixture("basic_assertion");
    let mut sidecar = fixture.sidecar;
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
    assert!(
        !document
            .parse_warnings
            .contains(&ParseWarning::MissingSummaryLine)
    );
    assert!(!document.failures_complete);
}

#[test]
fn internalerror_exit_3_lowers_to_harness_crashed_with_missing_summary() {
    let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2, pluggy-1.6.0\nrootdir: /tmp/proj\ncollected 1 item\nINTERNALERROR> Traceback (most recent call last):\nINTERNALERROR>   File \"/venv/lib/python3.11/site-packages/_pytest/main.py\", line 289, in wrap_session\nINTERNALERROR>     session.exitstatus = doit(config, session) or 0\nINTERNALERROR> RuntimeError: plugin exploded\n";
    let document = analyze_synthetic(log, Some(3), None);

    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert_missing_summary_crash(&document);
}

#[test]
fn no_tests_exit_5_lowers_to_no_tests() {
    let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2, pluggy-1.6.0\nrootdir: /tmp/proj\ncollected 0 items\n\n============================ no tests ran in 0.01s =============================\n";
    let document = analyze_synthetic(log, Some(5), None);

    assert_eq!(document.runner_outcome, RunnerOutcome::NoTests);
    assert_eq!(document.parse_warnings, Vec::new());
    assert!(document.failures_complete);
    assert_eq!(document.counts.tests_run, 0);
    assert_eq!(document.counts.tests_passed, 0);
}

#[test]
fn sigint_after_banner_lowers_to_harness_crashed_with_missing_summary() {
    let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2, pluggy-1.6.0\nrootdir: /tmp/proj\ncollected 1 item\n\ntests/test_slow.py \n!!!!!!!!!!!!!!!!!!!!!!!!!!!!!! KeyboardInterrupt !!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!\n/tmp/proj/tests/test_slow.py:5: KeyboardInterrupt\n";
    let document = analyze_synthetic(log, None, Some("SIGINT"));

    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert_missing_summary_crash(&document);
}

#[test]
fn xdist_worker_crash_lowers_to_harness_crashed_with_missing_summary() {
    let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2, pluggy-1.6.0\nplugins: xdist-3.6.1\ncreated: 2/2 workers\n2 workers [2 items]\n\nscheduling tests via LoadScheduling\n\ntests/test_worker.py::test_a \n[gw0] node down: Not properly terminated\n[gw0] worker crashed while running tests/test_worker.py::test_a\nreplacing crashed worker gw0\n";
    let document = analyze_synthetic(log, Some(1), None);

    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    assert_missing_summary_crash(&document);
}

#[test]
fn clean_summary_wrapper_exit_1_lowers_to_harness_crashed_without_missing_summary_warning() {
    let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2, pluggy-1.6.0\nrootdir: /tmp/proj\ncollected 1 item\n\ntests/test_ok.py .                                                       [100%]\n\n============================== 1 passed in 0.01s ===============================\ncoverage: error: No data was collected.\n";
    let document = analyze_synthetic(log, Some(1), None);

    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
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
    assert!(!document.failures_complete);
    assert_eq!(document.counts.tests_passed, 0);
}

#[test]
fn parser_missed_failure_lowers_to_failed_with_observable_mismatch() {
    // The runner's own summary says tests failed; classifying the run
    // harness_crashed would send consumers debugging infrastructure instead
    // of reading the log. The outcome trusts the summary, and the
    // unextracted failure list is observable via RunnerSummaryMismatch +
    // failures_complete=false.
    let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2, pluggy-1.6.0\nrootdir: /tmp/proj\ncollected 1 item\n\ntests/test_weird.py F                                                    [100%]\n\n=========================== short test summary info ============================\nFAILED tests/test_weird.py::test_weird - AssertionError: odd\n============================== 1 failed in 0.01s ===============================\n";
    let document = analyze_synthetic(log, Some(1), None);

    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
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
    assert!(!document.failures_complete);
    assert_eq!(document.failures.len(), 0);
}

#[test]
fn interactive_loop_and_response_file_sidecars_are_rejected_during_replay() {
    for argv in [
        vec!["pytest", "-f"],
        vec!["pytest", "-sx"],
        vec!["pytest", "-sv"],
        vec!["pytest", "-fx"],
        vec!["pytest", "-fs"],
        vec!["python", "-m", "pytest", "--looponfail"],
        vec!["pytest", "--pdb"],
        vec!["pytest", "--trace"],
        vec!["pytest", "-s"],
        vec!["pytest", "--capture=no"],
        vec!["pytest", "--capture", "no"],
        vec!["pytest", "@interactive.args"],
    ] {
        let fixture = load_fixture("basic_assertion");
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
                if reason.contains("interactive")
        ));
    }

    for argv in [
        vec!["python", "-s", "-m", "pytest"],
        vec!["pytest", "-ks"],
        vec!["pytest", "-ms"],
        vec!["pytest", "-rs"],
        vec!["pytest", "-osomething"],
        vec!["pytest", "-n2"],
    ] {
        let fixture = load_fixture("basic_assertion");
        let mut sidecar = fixture.sidecar;
        sidecar.argv = argv.into_iter().map(str::to_owned).collect();
        assert!(
            analyze_log(
                &fixture.log,
                &sidecar,
                AnalyzeOptions { max_failures: None },
            )
            .is_ok()
        );
    }
}

#[test]
fn multiple_completed_pytest_summaries_are_rejected() {
    let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2\ncollected 1 item\n\ntests/test_one.py F\n\n=================================== FAILURES ===================================\n__________________________________ test_one ___________________________________\nE       AssertionError: one\ntests/test_one.py:3: AssertionError\n============================== 1 failed in 0.01s ===============================\n============================== 1 passed in 0.01s ===============================\n";
    let mut sidecar = load_fixture("basic_assertion").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let result = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None });

    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { reason })
            if reason.contains("multiple completed pytest")
    ));
}

#[test]
fn captured_summary_shaped_output_is_not_a_second_pytest_session() {
    let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2\ncollected 1 item\n\ntests/test_one.py F\n\n=================================== FAILURES ===================================\n__________________________________ test_one ___________________________________\nE       AssertionError: one\n----------------------------- Captured stdout call -----------------------------\n============================== 1 passed in 0.01s ===============================\nno tests ran in 0.01s\ntests/test_one.py:3: AssertionError\n=========================== short test summary info ============================\nFAILED tests/test_one.py::test_one - AssertionError: one\n============================== 1 failed in 0.01s ===============================\n";
    let mut sidecar = load_fixture("basic_assertion").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);

    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None })
        .expect("captured summary-shaped text should not look like another pytest session");

    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert_eq!(document.counts.tests_failed, 1);
}

#[test]
fn partially_extracted_failure_list_is_marked_incomplete() {
    let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2, pluggy-1.6.0\nrootdir: /tmp/proj\ncollected 2 items\n\ntests/test_partial.py FF                                                 [100%]\n\n=================================== FAILURES ===================================\n________________________________ test_one __________________________________\nE       AssertionError: one\ntests/test_partial.py:3: AssertionError\n=========================== short test summary info ============================\nFAILED tests/test_partial.py::test_one - AssertionError: one\nFAILED tests/test_partial.py::test_two - AssertionError: two\n============================== 2 failed in 0.01s ===============================\n";
    let document = analyze_synthetic(log, Some(1), None);

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert_eq!(document.failures.len(), 1);
    assert_eq!(document.counts.tests_failed, 1);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(!document.failures_complete);
}

#[test]
fn coverage_failure_is_unaccounted_even_with_a_parsed_test_failure() {
    let fixture = load_fixture("basic_assertion");
    let mut log = fixture.log;
    log.extend_from_slice(
        b"FAIL Required test coverage of 100% not reached. Total coverage: 50.00%\n",
    );
    let mut sidecar = fixture.sidecar;
    sidecar.exit_code = Some(1);
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(&log);
    let document = analyze_log(&log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn max_failures_zero_preserves_unbudgeted_classification_and_counts() {
    let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2, pluggy-1.6.0\nrootdir: /tmp/proj\ncollected 3 items\n\ntests/test_many.py FFF                                                   [100%]\n\n=================================== FAILURES ===================================\n________________________________ test_one __________________________________\nE       AssertionError: one\ntests/test_many.py:3: AssertionError\n________________________________ test_two __________________________________\nE       AssertionError: two\ntests/test_many.py:7: AssertionError\n________________________________ test_three __________________________________\nE       AssertionError: three\ntests/test_many.py:11: AssertionError\n=========================== short test summary info ============================\nFAILED tests/test_many.py::test_one - AssertionError: one\nFAILED tests/test_many.py::test_two - AssertionError: two\nFAILED tests/test_many.py::test_three - AssertionError: three\n============================== 3 failed in 0.01s ===============================\n";
    let mut sidecar = synthetic_sidecar(log, Some(1));
    sidecar.cwd = "/tmp/proj".to_owned();
    let document = analyze_log(
        log,
        &sidecar,
        AnalyzeOptions {
            max_failures: Some(0),
        },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
    assert_eq!(document.emission.failures_total, 3);
    assert_eq!(document.emission.failures_emitted, 0);
    assert_eq!(document.failures.len(), 0);
    assert_eq!(document.counts.tests_failed, 3);
}

#[test]
fn failed_final_counts_only_is_rejected_at_admission() {
    let log = b"1 failed in 0.01s\n";
    let result = analyze_log(
        log,
        &synthetic_sidecar(log, Some(1)),
        AnalyzeOptions { max_failures: None },
    );

    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { .. })
    ));
}

#[test]
fn recover_runner_test_id_attributes_quiet_mode_failure_correctly() {
    // Regression anchor for a gauntlet capture
    // (`pytest_httpx_assertion_failure`). Symptom: a `-q -x` pytest run with a single failing test produces a FAILURES block whose
    // header banner has a simple underscored title (`__________ test_foo __________`)
    // but no entry in the `short test summary info` section (because `-q`
    // suppresses it). The adapter's `recover_runner_test_id` path
    // (src/adapters/pytest.rs) reconstructs `<primary_location.file>::<title>`
    // from those two signals; this test locks the recovery shape so a future
    // refactor cannot silently regress to `runner_test_id: null` (which would
    // then fall through to `counts.tests_passed += 1` and double-count the
    // failure on the passed side).
    let log: &[u8] =
        b"============================= test session starts ==============================\n\
platform darwin -- Python 3.13.12, pytest-9.0.2\n\
collected 1 item\n\
\n\
tests/test_status_codes.py F                                             [100%]\n\
\n\
=================================== FAILURES ===================================\n\
__________________________ test_status_code_as_int ___________________________\n\
\n\
    def test_status_code_as_int():\n\
>       assert codes.NOT_FOUND == 405\n\
E       AssertionError: assert <codes.NOT_FOUND: 404> == 405\n\
\n\
tests/test_status_codes.py:42: AssertionError\n\
=========================== 1 failed in 0.04s ================================\n";
    let mut sidecar = synthetic_sidecar(log, Some(1));
    sidecar.command = "pytest -q -x tests/test_status_codes.py::test_status_code_as_int".to_owned();
    sidecar.argv = vec![
        "pytest".to_owned(),
        "-q".to_owned(),
        "-x".to_owned(),
        "tests/test_status_codes.py::test_status_code_as_int".to_owned(),
    ];

    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(document.failures.len(), 1, "expected exactly one failure");
    assert_eq!(
        document.failures[0].runner_test_id.as_deref(),
        Some("tests/test_status_codes.py::test_status_code_as_int"),
        "recover_runner_test_id should reconstruct the nodeid from header + location"
    );
    assert_eq!(document.counts.tests_run, 1);
    assert_eq!(document.counts.tests_failed, 1);
    assert_eq!(
        document.counts.tests_passed, 0,
        "passed count must not absorb the recovered-nodeid failure (Phase 1 regression)"
    );
    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
}

#[test]
fn pytest_test_body_exception_is_errored_per_schema_invariant_not_pytest_taxonomy() {
    // Locks the current v1.5 schema contract: a `RuntimeError` raised in a test
    // body is classified as `FailureKind::Exception` and contributes to
    // `counts.tests_errored`, NOT `counts.tests_failed`, per schema invariant 11
    // (src/schema/invariants.rs::final_outcome — phase=Test + kind=Exception
    // maps to Errored) and docs/schema.md.
    //
    // This deliberately contradicts pytest's own terminal-summary taxonomy,
    // which counts test-body exceptions under `failed` (the recap below says
    // `2 failed, 1 passed`). The contradiction is a known schema-contract
    // decision deferred past v1.5; this test fails loudly if a future schema
    // revision flips the mapping so the boundary has a fixed reference point.
    //
    // A synthetic case pins this schema boundary.
    let log: &[u8] = b"============================= test session starts ==============================\n\
platform darwin -- Python 3.13.12, pytest-9.0.2\n\
collected 3 items\n\
\n\
tests/test_synth.py .FF                                                  [100%]\n\
\n\
=================================== FAILURES ===================================\n\
___________________________ test_failing_assertion ___________________________\n\
\n\
    def test_failing_assertion():\n\
>       assert 1 == 2\n\
E       AssertionError: assert 1 == 2\n\
\n\
tests/test_synth.py:5: AssertionError\n\
___________________________ test_failing_exception ___________________________\n\
\n\
    def test_failing_exception():\n\
>       raise RuntimeError(\"synthetic exception for failtrace dogfooding\")\n\
E       RuntimeError: synthetic exception for failtrace dogfooding\n\
\n\
tests/test_synth.py:9: RuntimeError\n\
=========================== short test summary info ============================\n\
FAILED tests/test_synth.py::test_failing_assertion - AssertionError: assert 1 == 2\n\
FAILED tests/test_synth.py::test_failing_exception - RuntimeError: synthetic exception for failtrace dogfooding\n\
========================= 2 failed, 1 passed in 0.01s ========================\n";
    let mut sidecar = synthetic_sidecar(log, Some(1));
    sidecar.command = "pytest tests/test_synth.py".to_owned();
    sidecar.argv = vec!["pytest".to_owned(), "tests/test_synth.py".to_owned()];

    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(document.failures.len(), 2, "two failures emitted");
    let assertion_count = document
        .failures
        .iter()
        .filter(|f| f.kind == FailureKind::Assertion)
        .count();
    let exception_count = document
        .failures
        .iter()
        .filter(|f| f.kind == FailureKind::Exception)
        .count();
    assert_eq!(assertion_count, 1, "one assertion-kind failure");
    assert_eq!(exception_count, 1, "one exception-kind failure");
    assert!(
        document.failures.iter().all(|f| f.runner_test_id.is_some()),
        "both failures must have runner_test_id (no recovery path needed here)"
    );

    assert_eq!(document.counts.tests_run, 3);
    assert_eq!(document.counts.tests_passed, 1);
    assert_eq!(
        document.counts.tests_failed, 1,
        "schema invariant 11: only the Assertion failure counts as `failed`"
    );
    assert_eq!(
        document.counts.tests_errored, 1,
        "schema invariant 11: the test-body Exception counts as `errored`, NOT `failed` (deferred contradiction with pytest's `2 failed, 1 passed` recap)"
    );
    // runner_outcome stays Failed (the document has parsed failures + a
    // final-counts line); the per-failure attribution is what diverges from
    // pytest's taxonomy, not the top-level outcome.
    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
}

#[test]
fn progress_line_only_garbage_is_rejected_at_admission() {
    let log = b"not pytest output at all                                      [100%]\n";
    let result = analyze_log(
        log,
        &synthetic_sidecar(log, Some(1)),
        AnalyzeOptions { max_failures: None },
    );

    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { .. })
    ));
}

fn analyze_synthetic(log: &[u8], exit_code: Option<i32>, signal: Option<&str>) -> OutputDocument {
    let mut sidecar = synthetic_sidecar(log, exit_code);
    sidecar.signal = signal.map(str::to_owned);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();
    assert_eq!(validate(&document), Vec::new());
    document
}

fn assert_missing_summary_crash(document: &OutputDocument) {
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::MissingSummaryLine)
    );
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(!document.failures_complete);
}

struct Fixture {
    log: Vec<u8>,
    expected: Value,
    sidecar: Sidecar,
}

fn load_fixture(scenario: &str) -> Fixture {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pytest")
        .join(scenario);
    let log = fs::read(root.join("run/log")).unwrap();
    let expected: Value =
        serde_json::from_slice(&fs::read(root.join("expected.json")).unwrap()).unwrap();
    let meta_path = root.join("run/meta.json");
    let sidecar = if meta_path.exists() {
        serde_json::from_slice(&fs::read(meta_path).unwrap()).unwrap()
    } else {
        sidecar_from_expected(&expected, &log, &root)
    };
    Fixture {
        log,
        expected,
        sidecar,
    }
}

fn sidecar_from_expected(expected: &Value, log: &[u8], root: &Path) -> Sidecar {
    let argv = expected["runner_argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect::<Vec<_>>();

    Sidecar {
        meta_schema_version: "1.2".to_owned(),
        schema_version: expected["expected_subset"]["schema_version"]
            .as_str()
            .unwrap()
            .to_owned(),
        failtrace_version: "0.1.0".to_owned(),
        capture_build: None,
        adapters: vec![Adapter {
            id: "pytest".to_owned(),
            version: "pytest@0.1.0".to_owned(),
            kind: AdapterKind::Runner,
            selected: true,
            selection_reason: SelectionReason::ArgvNameMatch,
        }],
        adapter_candidates: vec![],
        argv: argv.iter().map(|value| (*value).to_owned()).collect(),
        command: argv.join(" "),
        cwd: root.join("source").to_string_lossy().into_owned(),
        started_at: "2026-05-13T12:00:00Z".to_owned(),
        duration_ms: 10,
        exit_code: expected["exit_code"].as_i64().map(|value| value as i32),
        signal: None,
        early_exit: None,
        truncated: expected["expected_subset"]["truncated"].as_bool().unwrap(),
        truncation_reason: None,
        path_case_folding: PathCaseFolding::CaseSensitive,
        stream_merge: StreamMerge::StdoutStderrInterleaved,
        raw_log_sha256: sha256_hex(log),
    }
}

fn synthetic_sidecar(log: &[u8], exit_code: Option<i32>) -> Sidecar {
    Sidecar {
        meta_schema_version: "1.2".to_owned(),
        schema_version: "1.6".to_owned(),
        failtrace_version: "0.1.0".to_owned(),
        capture_build: None,
        adapters: vec![Adapter {
            id: "pytest".to_owned(),
            version: "pytest@0.1.0".to_owned(),
            kind: AdapterKind::Runner,
            selected: true,
            selection_reason: SelectionReason::ArgvNameMatch,
        }],
        adapter_candidates: vec![],
        argv: vec!["pytest".to_owned(), "-q".to_owned()],
        command: "pytest -q".to_owned(),
        cwd: "/tmp/ftgreen".to_owned(),
        started_at: "2026-05-13T12:00:00Z".to_owned(),
        duration_ms: 10,
        exit_code,
        signal: None,
        early_exit: None,
        truncated: false,
        truncation_reason: None,
        path_case_folding: PathCaseFolding::CaseSensitive,
        stream_merge: StreamMerge::StdoutStderrInterleaved,
        raw_log_sha256: sha256_hex(log),
    }
}

fn assert_expected_subset(scenario: &str, expected: &Value, document: &OutputDocument) {
    let mut expected = expected.clone();
    if expected["schema_version"] == "1.4" {
        expected["schema_version"] = Value::String("1.6".to_owned());
    }
    let actual = serde_json::to_value(document).unwrap();
    assert_subset(scenario, &expected, &actual, "root");
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
