use failtrace::schema::*;
use serde_json::Value;

const BASIC_ASSERTION: &str = include_str!("fixtures/pytest/basic_assertion/expected.json");
const COLLECTION_ERROR: &str = include_str!("fixtures/pytest/collection_error/expected.json");
const PARAMETERIZED: &str = include_str!("fixtures/pytest/parameterized/expected.json");
const QUIET_EARLY_EXIT_ASSERTION: &str =
    include_str!("fixtures/pytest/quiet_early_exit_assertion/expected.json");
const TEARDOWN_EXCEPTION: &str = include_str!("fixtures/pytest/teardown_exception/expected.json");

#[test]
fn fixture_hashes_match_schema_normative_and_local_anchors() {
    let mut checked = Vec::new();

    for fixture in [
        BASIC_ASSERTION,
        COLLECTION_ERROR,
        PARAMETERIZED,
        QUIET_EARLY_EXIT_ASSERTION,
        TEARDOWN_EXCEPTION,
    ] {
        let parsed: Value = serde_json::from_str(fixture).expect("fixture is valid JSON");
        let inputs = parsed.get("hash_inputs").expect("fixture has hash_inputs");

        for failure in inputs["failures"].as_array().unwrap() {
            let location = Location {
                file: failure["primary_location"]["file"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                line: failure["primary_location"]["line"].as_u64(),
                column: None,
                end_line: None,
                end_column: None,
                role: LocationRole::Primary,
                in_user_code: true,
            };
            let got = failure_id(FailureIdParts {
                adapter_id: failure["adapter_id"].as_str().unwrap(),
                phase: phase(failure["phase"].as_str().unwrap()),
                kind: kind(failure["kind"].as_str().unwrap()),
                runner_test_id: failure["runner_test_id"].as_str(),
                primary_location: &location,
                param_id: failure["param_id"].as_str(),
                message: failure["normalized_message"].as_str().unwrap(),
                cwd: "",
            });
            let expected = failure["failure_id"].as_str().unwrap();
            assert_eq!(got, expected);
            checked.push(expected.to_owned());
        }

        if let Some(expected_group_id) = inputs["test_group_id"].as_str() {
            let first = &inputs["failures"].as_array().unwrap()[0];
            let runner_test_id = first["runner_test_id"].as_str().unwrap();
            let got = derive_test_group_id(
                first["adapter_id"].as_str().unwrap(),
                runner_test_id,
                strip_params_pytest,
            );
            assert_eq!(got, expected_group_id);
            checked.push(expected_group_id.to_owned());
        }
    }

    checked.sort();
    assert_eq!(
        checked,
        [
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

    for normative in [
        "a28a10bf9c212128",
        "58af4b4b1fcb1036",
        "4b78b28e9378f472",
        "6f4cda653abb2c1c",
        "49507ef118ce763f",
    ] {
        assert!(checked.iter().any(|hash| hash == normative));
    }
}

#[test]
fn normalize_message_obeys_spec_ordering() {
    assert_eq!(
        normalize_message(
            "\u{1b}[31m/Users/dev/repo-2024/tests/test_thing.py:123\u{1b}[0m",
            "/Users/dev/repo-2024"
        ),
        "<cwd>/tests/test_thing.py:<N>"
    );

    let precomposed = normalize_message("/tmp/cafe\u{301}/x.py:9", "/tmp/caf\u{e9}");
    let decomposed = normalize_message("/tmp/caf\u{e9}/x.py:9", "/tmp/cafe\u{301}");
    assert_eq!(precomposed, "<cwd>/x.py:<N>");
    assert_eq!(precomposed, decomposed);

    assert_eq!(
        normalize_message("pointer changed from 0x1234 to 0xABCD", "/tmp/repo"),
        "pointer changed from <N> to <N>"
    );

    assert_eq!(
        normalize_message(
            "western 2024 eastern \u{0662}\u{0660}\u{0662}\u{0664}",
            "/tmp/repo"
        ),
        "western <N> eastern \u{0662}\u{0660}\u{0662}\u{0664}"
    );
}

#[test]
fn serde_json_round_trips_typed_document() {
    let document = valid_document();
    let encoded = serde_json::to_string(&document).unwrap();
    let decoded: OutputDocument = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, document);
}

#[test]
fn invariant_validator_accepts_positive_document() {
    assert_eq!(validate(&valid_document()), Vec::<Violation>::new());
}

#[test]
fn invariant_13_fires_for_passed_run_with_no_selected_adapter() {
    // Strengthened in the v1.4 correction: parse_status != "unrecognized" requires
    // exactly one selected adapter, even when failures is empty.
    let mut document = valid_document();
    document.failures = vec![];
    document.runner_outcome = RunnerOutcome::Passed;
    document.exit_code = Some(0);
    document.counts.tests_run = 1;
    document.counts.tests_passed = 1;
    document.counts.tests_failed = 0;
    document.counts.tests_errored = 0;
    document.emission.failures_emitted = 0;
    document.emission.failures_total = 0;

    assert!(
        validate(&document).is_empty(),
        "passed run with selected adapter should validate cleanly"
    );

    document.adapters[0].selected = false;
    let violations = validate(&document);
    assert!(
        violations.iter().any(|v| v.invariant == 13),
        "expected invariant 13 violation, got {violations:?}"
    );
}

#[test]
fn invariant_validator_reports_each_spec_rule() {
    assert_violation(1, |document| {
        document.truncation_reason = Some(TruncationReason::Timeout);
    });
    assert_violation(2, |document| {
        document.emission.failures_emitted = 2;
    });
    assert_violation(3, |document| {
        document.emission.max_failures = Some(10);
    });
    assert_violation(4, |document| {
        document.emission.mode = EmissionMode::Budgeted;
        document.emission.max_failures = None;
    });
    assert_violation(5, |document| {
        document.runner_outcome = RunnerOutcome::TimedOut;
        document.early_exit = None;
    });
    assert_violation(6, |document| {
        document.runner_outcome = RunnerOutcome::Passed;
    });
    assert_violation(7, |document| {
        document.failures[0].cascade_root = Some("failure-one".to_owned());
    });
    assert_violation(8, |document| {
        document.parse_status = ParseStatus::UnparsedOutput;
    });
    assert_violation(9, |document| {
        document.failures[0].details = Details::Unknown {};
    });
    assert_violation(10, |document| {
        document.failures[0]
            .dependent_failure_ids
            .push("missing".to_owned());
    });
    assert_violation(11, |document| {
        document.counts.tests_failed = 0;
    });
    assert_violation(12, |document| {
        document.counts.tests_run = 0;
    });
    assert_violation(13, |document| {
        document.adapters[0].selected = false;
    });
    assert_violation(14, |document| {
        document.failures[0].companion_diagnostics.push(Diagnostic {
            diagnostic_id: "diag-one".to_owned(),
            kind: FailureKind::Assertion,
            diagnostic_severity: DiagnosticSeverity::Error,
            message: "context".to_owned(),
            primary_location: None,
            details: assertion_details(),
            evidence: None,
            companion_diagnostics: vec![Diagnostic {
                diagnostic_id: "diag-nested".to_owned(),
                kind: FailureKind::Assertion,
                diagnostic_severity: DiagnosticSeverity::Error,
                message: "nested".to_owned(),
                primary_location: None,
                details: assertion_details(),
                evidence: None,
                companion_diagnostics: vec![],
            }],
        });
    });
    assert_violation(15, |document| {
        document.failures = vec![];
        document.emission.failures_emitted = 0;
        document.emission.failures_total = 0;
        document.runner_outcome = RunnerOutcome::Passed;
        document.counts.tests_run = 1;
        document.counts.tests_passed = 1;
        document.counts.tests_failed = 0;
        document.counts.tests_errored = 0;
        document.exit_code = Some(1);
    });
}

fn assert_violation(invariant: u8, mutate: impl FnOnce(&mut OutputDocument)) {
    let mut document = valid_document();
    mutate(&mut document);
    let violations = validate(&document);
    assert!(
        violations
            .iter()
            .any(|violation| violation.invariant == invariant),
        "expected invariant {invariant} violation, got {violations:?}",
    );
}

fn valid_document() -> OutputDocument {
    OutputDocument {
        schema_version: "1.4".to_owned(),
        document_kind: DocumentKind::RunResult,
        failtrace_version: "0.1.0".to_owned(),
        emission: Emission {
            channel: EmissionChannel::File,
            mode: EmissionMode::Full,
            max_failures: None,
            failures_emitted: 1,
            failures_total: 1,
            full_output_path: "/tmp/failtrace/output.json".to_owned(),
        },
        command: "pytest tests/".to_owned(),
        cwd: "/tmp/project".to_owned(),
        started_at: "2026-05-13T12:00:00Z".to_owned(),
        duration_ms: 100,
        exit_code: Some(1),
        signal: None,
        runner_outcome: RunnerOutcome::Failed,
        early_exit: None,
        adapters: vec![Adapter {
            id: "pytest".to_owned(),
            version: "pytest@0.1.0".to_owned(),
            kind: AdapterKind::Runner,
            selected: true,
            selection_reason: SelectionReason::ArgvNameMatch,
        }],
        parse_status: ParseStatus::Complete,
        parse_warnings: vec![],
        determinism_warnings: vec![],
        truncated: false,
        truncation_reason: None,
        failures_complete: true,
        failures: vec![Failure {
            failure_id: "failure-one".to_owned(),
            adapter_id: "pytest".to_owned(),
            phase: Phase::Test,
            kind: FailureKind::Assertion,
            runner_package_id: None,
            runner_test_id: Some("tests/test_basic.py::test_addition".to_owned()),
            test_segments: vec![],
            test_name: Some("test_addition".to_owned()),
            param_id: None,
            test_group_id: None,
            primary_location: Location {
                file: "tests/test_basic.py".to_owned(),
                line: Some(4),
                column: None,
                end_line: None,
                end_column: None,
                role: LocationRole::Primary,
                in_user_code: true,
            },
            extra_locations: vec![],
            message: "AssertionError: expected 2 to equal 3".to_owned(),
            diagnostic_code: None,
            diagnostic_severity: DiagnosticSeverity::Error,
            details: assertion_details(),
            raw_log_lines: [10, 19],
            evidence: None,
            cascade_root: None,
            cascade_reason: None,
            dependent_failure_ids: vec![],
            retries: vec![Retry {
                attempt: 1,
                outcome: RetryOutcome::Failed,
                duration_ms: 10,
            }],
            companion_diagnostics: vec![],
            extensions: Extensions::new(),
        }],
        ranking: Ranking {
            strategy: RankingStrategy::FirstEmitted,
            is_stable_across_runs: true,
        },
        counts: Counts {
            tests_run: 1,
            tests_passed: 0,
            tests_failed: 1,
            tests_errored: 0,
            tests_skipped: 0,
            tests_not_run: None,
            files_failed: Some(1),
        },
        raw_log_path: "/tmp/failtrace/log".to_owned(),
        raw_log_sha256: "abc".to_owned(),
        suggested_next_commands: vec![],
        extensions: Extensions::new(),
    }
}

fn assertion_details() -> Details {
    Details::Assertion {
        expected: Some("3".to_owned()),
        actual: Some("2".to_owned()),
        diff: None,
    }
}

fn phase(value: &str) -> Phase {
    match value {
        "build" => Phase::Build,
        "collect" => Phase::Collect,
        "setup" => Phase::Setup,
        "test" => Phase::Test,
        "teardown" => Phase::Teardown,
        "typecheck" => Phase::Typecheck,
        other => panic!("unknown phase {other}"),
    }
}

fn kind(value: &str) -> FailureKind {
    match value {
        "assertion" => FailureKind::Assertion,
        "exception" => FailureKind::Exception,
        "type_error" => FailureKind::TypeError,
        "syntax_error" => FailureKind::SyntaxError,
        "missing_symbol" => FailureKind::MissingSymbol,
        "dependency_resolution_error" => FailureKind::DependencyResolutionError,
        "linker_error" => FailureKind::LinkerError,
        "panic" => FailureKind::Panic,
        "signal" => FailureKind::Signal,
        "timeout" => FailureKind::Timeout,
        "oom" => FailureKind::Oom,
        "snapshot" => FailureKind::Snapshot,
        "unhandled_rejection" => FailureKind::UnhandledRejection,
        "race" => FailureKind::Race,
        "unknown" => FailureKind::Unknown,
        other => panic!("unknown kind {other}"),
    }
}
