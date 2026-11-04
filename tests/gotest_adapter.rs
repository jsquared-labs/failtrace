use std::{collections::BTreeMap, fs, path::Path};

use failtrace::{
    adapters::{AdapterError, AnalyzeOptions, gotest::analyze_log},
    schema::{
        Adapter, AdapterKind, EarlyExitKiller, EarlyExitReason, FailureKind, OutputDocument,
        ParseStatus, ParseWarning, Phase, RankingStrategy, RunnerOutcome, SelectionReason,
        validate,
    },
    storage::{PathCaseFolding, Sidecar, StreamMerge},
};
use serde_json::Value;

const SCENARIOS: &[&str] = &[
    "green_passing",
    "cached_pass",
    "no_tests",
    "basic_failure",
    "subtests_nested",
    "table_driven_failure",
    "panic_in_test",
    "build_failure",
    "skipped",
    "race_detected",
    "test_timeout",
    "parallel_interleaved",
    "multi_package",
    "init_panic",
    "malformed_event_preamble",
];

#[test]
fn gotest_adapter_reproduces_fixture_hashes_and_valid_documents() {
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

    assert_eq!(reproduced.len(), 15);
}

#[test]
fn no_json_command_is_unrecognized_and_suggests_minimal_json_rewrite() {
    let fixture = load_fixture("basic_failure");
    let mut sidecar = fixture.sidecar;
    sidecar.command = "env GOFLAGS=-count=1 go test ./... -run TestFails".to_owned();
    let document = analyze_log(
        b"plain go test output\n",
        &sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(document.parse_status, ParseStatus::UnparsedOutput);
    assert!(document.failures.is_empty());
    assert_eq!(
        document.suggested_next_commands[0].argv,
        [
            "env",
            "GOFLAGS=-count=1",
            "go",
            "test",
            "-json",
            "./...",
            "-run",
            "TestFails"
        ]
    );
}

#[test]
fn build_failures_are_pre_test_compile_diagnostics() {
    let document = analyze_fixture("build_failure");

    assert_eq!(document.runner_outcome, RunnerOutcome::PreTestFailed);
    assert_eq!(
        document.ranking.strategy,
        RankingStrategy::CompileBeforeRuntime
    );
    assert_eq!(document.failures.len(), 2);
    assert_eq!(document.failures[0].phase, Phase::Build);
    assert_eq!(document.failures[0].kind, FailureKind::TypeError);
    assert_eq!(document.failures[0].primary_location.file, "broken_test.go");
    assert_eq!(document.failures[0].primary_location.line, Some(6));
    assert_eq!(document.failures[0].primary_location.column, Some(14));
    assert_eq!(document.failures[1].kind, FailureKind::MissingSymbol);
    assert_eq!(document.failures[1].runner_test_id, None);
}

#[test]
fn cascade_subtest_parents_are_suppressed_and_ambiguous_names_are_flagged() {
    let document = analyze_fixture("subtests_nested");

    assert_eq!(document.failures.len(), 2);
    assert!(
        document
            .failures
            .iter()
            .all(|failure| failure.test_name.as_deref() != Some("TestOuter"))
    );
    let ambiguous = document
        .failures
        .iter()
        .find(|failure| failure.test_name.as_deref() == Some("TestOuter/Beta/with/slash"))
        .unwrap();
    assert_eq!(
        ambiguous.extensions["go"]["subtest_name_ambiguous"].as_bool(),
        Some(true)
    );
    assert_eq!(document.parse_status, ParseStatus::Complete);
}

#[test]
fn panic_timeout_race_and_init_panic_use_contract_shapes() {
    let panic_doc = analyze_fixture("panic_in_test");
    assert_eq!(panic_doc.failures[0].kind, FailureKind::Panic);
    assert_eq!(panic_doc.failures[0].phase, Phase::Test);
    assert_eq!(panic_doc.failures[0].primary_location.file, "panic_test.go");

    let timeout_doc = analyze_fixture("test_timeout");
    assert_eq!(timeout_doc.failures[0].kind, FailureKind::Timeout);
    assert_eq!(timeout_doc.failures[0].phase, Phase::Test);
    assert_eq!(timeout_doc.runner_outcome, RunnerOutcome::TimedOut);

    let race_doc = analyze_fixture("race_detected");
    assert_eq!(race_doc.failures[0].kind, FailureKind::Race);
    assert_eq!(race_doc.failures[0].phase, Phase::Test);
    assert_eq!(race_doc.failures[0].primary_location.file, "race_test.go");

    let init_doc = analyze_fixture("init_panic");
    assert_eq!(init_doc.failures[0].kind, FailureKind::Panic);
    assert_eq!(init_doc.failures[0].phase, Phase::Setup);
    assert_eq!(init_doc.failures[0].runner_test_id, None);
    assert_eq!(init_doc.failures[0].primary_location.file, "init_test.go");
}

#[test]
fn os_signal_outranks_timeout_shaped_partial_log() {
    let fixture = load_fixture("test_timeout");
    let mut sidecar = fixture.sidecar;
    sidecar.exit_code = None;
    sidecar.signal = Some("SIGTERM".to_owned());
    sidecar.early_exit = None;

    let document = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .expect("signaled timeout-shaped output should remain analyzable");

    assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
    let early_exit = document.early_exit.expect("signal early exit");
    assert_eq!(early_exit.reason, EarlyExitReason::SignalReceived);
    assert_eq!(early_exit.killer, EarlyExitKiller::Os);
    assert_eq!(early_exit.details, "SIGTERM");
}

#[test]
fn parallel_and_multi_package_rankings_are_marked_unstable() {
    for scenario in ["parallel_interleaved", "multi_package"] {
        let document = analyze_fixture(scenario);
        assert!(!document.ranking.is_stable_across_runs, "{scenario}");
    }
}

#[test]
fn go_downloading_preamble_does_not_crash_parse_status() {
    // Regression: a freshly-cloned Go module triggers `go: downloading <pkg> <ver>`
    // lines on stderr that get merged into the captured log. These are not JSON
    // events; they should not crash the gotest parser into ParseStatus::UnparsedOutput.
    // Expected behavior: the preamble lines count as malformed_event_lines,
    // ParseWarning::MalformedStructuredEvent is emitted, parse_status lowers to
    // Partial (not Unparsed), and the embedded JSON failure is still extracted.
    let log: &[u8] = br#"go: downloading github.com/stretchr/testify v1.9.0
go: downloading gopkg.in/yaml.v3 v3.0.1
go: downloading github.com/davecgh/go-spew v1.1.1
{"Time":"2026-05-16T12:00:00Z","Action":"run","Package":"example.com/proj","Test":"TestThing"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Test":"TestThing","Output":"--- FAIL: TestThing (0.00s)\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Test":"TestThing","Output":"    main_test.go:7: want 2, got 3\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Test":"TestThing","Elapsed":0.0}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Elapsed":0.0}
"#;
    let sidecar = synthetic_gotest_sidecar(log, Some(1));

    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None })
        .expect("preamble + JSON events should parse, not error");

    assert_eq!(
        document.parse_status,
        ParseStatus::Partial,
        "preamble triggers malformed-event warning; expected Partial, not Unparsed"
    );
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::MalformedStructuredEvent),
        "expected MalformedStructuredEvent warning for `go: downloading` preamble"
    );
    assert!(
        !document.failures.is_empty(),
        "TestThing fail event should still be extracted as a failure despite preamble"
    );
    assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
}

#[test]
fn package_fail_without_per_test_events_marks_failure_list_incomplete() {
    // Regression: a TestMain/package-setup failure fails the package without
    // emitting any per-test `fail` event, so no failure block can be extracted.
    // runner_outcome is still Failed, but the failure list must not claim to be
    // complete: expected RunnerSummaryMismatch, parse_status Partial, and
    // failures_complete false.
    let log: &[u8] = br#"{"Time":"2026-05-16T12:00:00Z","Action":"start","Package":"example.com/proj"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Output":"setup: cannot connect to test database\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Output":"FAIL\texample.com/proj\t0.002s\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Elapsed":0.002}
"#;
    let sidecar = synthetic_gotest_sidecar(log, Some(1));

    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None })
        .expect("package-level fail should parse, not error");

    assert_eq!(
        document.runner_outcome,
        RunnerOutcome::Failed,
        "package terminal `fail` still means the run failed"
    );
    assert!(
        document.failures.is_empty(),
        "no per-test fail event means no failure block is extractable"
    );
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch),
        "failed run with an empty failure list must surface RunnerSummaryMismatch"
    );
    assert_eq!(
        document.parse_status,
        ParseStatus::Partial,
        "RunnerSummaryMismatch degrades parse_status"
    );
    assert!(
        !document.failures_complete,
        "an empty failure list on a failed run must not claim completeness"
    );
    assert_eq!(validate(&document), Vec::new());
}

#[test]
fn failed_test_terminal_without_a_matching_block_marks_partial_list_incomplete() {
    let log: &[u8] = br#"{"Time":"2026-05-16T12:00:00Z","Action":"start","Package":"example.com/proj"}
{"Time":"2026-05-16T12:00:00Z","Action":"run","Package":"example.com/proj","Test":"TestOne"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Test":"TestOne","Output":"--- FAIL: TestOne (0.00s)\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Test":"TestOne","Output":"    main_test.go:7: want 2, got 3\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Test":"TestOne","Elapsed":0.0}
{"Time":"2026-05-16T12:00:00Z","Action":"run","Package":"example.com/proj","Test":"TestTwo"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Test":"TestTwo","Output":"failure shape the adapter does not recognize\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Test":"TestTwo","Elapsed":0.0}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Elapsed":0.0}
"#;
    let sidecar = synthetic_gotest_sidecar(log, Some(1));
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

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
fn repeated_failed_terminal_for_same_test_marks_collapsed_history_incomplete() {
    let log: &[u8] = br#"{"Time":"2026-05-16T12:00:00Z","Action":"start","Package":"example.com/proj"}
{"Time":"2026-05-16T12:00:00Z","Action":"run","Package":"example.com/proj","Test":"TestOne"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Test":"TestOne","Output":"    main_test.go:7: first failure\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Test":"TestOne","Elapsed":0.0}
{"Time":"2026-05-16T12:00:01Z","Action":"fail","Package":"example.com/proj","Elapsed":0.0}
{"Time":"2026-05-16T12:00:02Z","Action":"run","Package":"example.com/proj","Test":"TestOne"}
{"Time":"2026-05-16T12:00:02Z","Action":"output","Package":"example.com/proj","Test":"TestOne","Output":"    main_test.go:7: second failure\n"}
{"Time":"2026-05-16T12:00:02Z","Action":"fail","Package":"example.com/proj","Test":"TestOne","Elapsed":0.0}
{"Time":"2026-05-16T12:00:03Z","Action":"fail","Package":"example.com/proj","Elapsed":0.0}
"#;
    let sidecar = synthetic_gotest_sidecar(log, Some(1));
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
fn repeated_cascade_parent_terminal_is_not_fully_explained_by_one_child() {
    let log: &[u8] = br#"{"Time":"2026-05-16T12:00:00Z","Action":"start","Package":"example.com/proj"}
{"Time":"2026-05-16T12:00:00Z","Action":"run","Package":"example.com/proj","Test":"TestParent"}
{"Time":"2026-05-16T12:00:00Z","Action":"run","Package":"example.com/proj","Test":"TestParent/child"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Test":"TestParent/child","Output":"    main_test.go:9: child failed\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Test":"TestParent/child","Elapsed":0.0}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Test":"TestParent","Elapsed":0.0}
{"Time":"2026-05-16T12:00:01Z","Action":"fail","Package":"example.com/proj","Test":"TestParent","Elapsed":0.0}
{"Time":"2026-05-16T12:00:01Z","Action":"fail","Package":"example.com/proj","Elapsed":0.0}
"#;
    let sidecar = synthetic_gotest_sidecar(log, Some(1));
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
fn failed_parent_with_its_own_panic_is_not_suppressed_as_a_cascade() {
    let log: &[u8] = br#"{"Time":"2026-05-16T12:00:00Z","Action":"start","Package":"example.com/proj"}
{"Time":"2026-05-16T12:00:00Z","Action":"run","Package":"example.com/proj","Test":"TestParent"}
{"Time":"2026-05-16T12:00:00Z","Action":"run","Package":"example.com/proj","Test":"TestParent/child"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Test":"TestParent/child","Output":"    main_test.go:9: child failed\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Test":"TestParent/child","Elapsed":0.0}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Test":"TestParent","Output":"panic: cleanup exploded\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Test":"TestParent","Elapsed":0.0}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Elapsed":0.0}
"#;
    let sidecar = synthetic_gotest_sidecar(log, Some(1));
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 2);
    assert!(
        document
            .failures
            .iter()
            .any(|failure| failure.kind == FailureKind::Panic)
    );
    assert!(
        !document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(document.failures_complete);
}

#[test]
fn failed_parent_with_fatal_runtime_error_is_not_suppressed_as_a_cascade() {
    let log: &[u8] = br#"{"Time":"2026-05-16T12:00:00Z","Action":"start","Package":"example.com/proj"}
{"Time":"2026-05-16T12:00:00Z","Action":"run","Package":"example.com/proj","Test":"TestParent"}
{"Time":"2026-05-16T12:00:00Z","Action":"run","Package":"example.com/proj","Test":"TestParent/child"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Test":"TestParent/child","Output":"    main_test.go:9: child failed\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Test":"TestParent/child","Elapsed":0.0}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Test":"TestParent","Output":"fatal error: concurrent map writes\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Test":"TestParent","Elapsed":0.0}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Elapsed":0.0}
"#;
    let sidecar = synthetic_gotest_sidecar(log, Some(1));
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
fn repeated_build_epochs_are_incomplete_when_epoch_boundaries_are_collapsed() {
    let log: &[u8] = br#"{"Time":"2026-05-16T12:00:00Z","Action":"build-output","ImportPath":"example.com/proj [example.com/proj.test]","Output":"./one.go:1:1: undefined: first\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"build-output","ImportPath":"example.com/proj [example.com/proj.test]","Output":"./two.go:1:1: undefined: second\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"build-fail","ImportPath":"example.com/proj [example.com/proj.test]"}
{"Time":"2026-05-16T12:00:01Z","Action":"build-fail","ImportPath":"example.com/proj [example.com/proj.test]"}
"#;
    let sidecar = synthetic_gotest_sidecar(log, Some(1));
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
fn repeated_package_epochs_are_incomplete_when_epoch_boundaries_are_collapsed() {
    let log: &[u8] = br#"{"Time":"2026-05-16T12:00:00Z","Action":"start","Package":"example.com/proj"}
{"Time":"2026-05-16T12:00:00Z","Action":"run","Package":"example.com/proj","Test":"TestOne"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Test":"TestOne","Output":"    one_test.go:7: first failed\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Test":"TestOne","Elapsed":0.0}
{"Time":"2026-05-16T12:00:00Z","Action":"run","Package":"example.com/proj","Test":"TestTwo"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/proj","Test":"TestTwo","Output":"    two_test.go:7: second failed\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Test":"TestTwo","Elapsed":0.0}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/proj","Elapsed":0.0}
{"Time":"2026-05-16T12:00:01Z","Action":"start","Package":"example.com/proj"}
{"Time":"2026-05-16T12:00:01Z","Action":"output","Package":"example.com/proj","Output":"second package epoch failed without diagnostics\n"}
{"Time":"2026-05-16T12:00:01Z","Action":"fail","Package":"example.com/proj","Elapsed":0.0}
"#;
    let sidecar = synthetic_gotest_sidecar(log, Some(1));
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
fn one_parsed_package_cannot_mask_an_unparsed_failed_package() {
    let log: &[u8] = br#"{"Time":"2026-05-16T12:00:00Z","Action":"start","Package":"example.com/one"}
{"Time":"2026-05-16T12:00:00Z","Action":"run","Package":"example.com/one","Test":"TestOne"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/one","Test":"TestOne","Output":"    one_test.go:7: want 2, got 3\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/one","Test":"TestOne","Elapsed":0.0}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/one","Elapsed":0.0}
{"Time":"2026-05-16T12:00:00Z","Action":"start","Package":"example.com/two"}
{"Time":"2026-05-16T12:00:00Z","Action":"output","Package":"example.com/two","Output":"TestMain failed without a parseable diagnostic\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"fail","Package":"example.com/two","Elapsed":0.0}
"#;
    let sidecar = synthetic_gotest_sidecar(log, Some(1));
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
fn build_fail_without_a_parseable_diagnostic_marks_list_incomplete() {
    let log: &[u8] = br#"{"Time":"2026-05-16T12:00:00Z","Action":"build-output","ImportPath":"example.com/proj","Output":"compiler failed in an unknown shape\n"}
{"Time":"2026-05-16T12:00:00Z","Action":"build-fail","ImportPath":"example.com/proj"}
"#;
    let sidecar = synthetic_gotest_sidecar(log, Some(1));
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::PreTestFailed);
    assert!(document.failures.is_empty());
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(!document.failures_complete);
}

#[test]
fn sidecar_without_gotest_pin_is_rejected() {
    let fixture = load_fixture("basic_failure");
    let mut sidecar = fixture.sidecar;
    sidecar.adapters.retain(|adapter| adapter.id != "gotest");
    let result = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    );

    assert!(matches!(result, Err(AdapterError::InvalidSidecar { .. })));
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

fn synthetic_gotest_sidecar(log: &[u8], exit_code: Option<i32>) -> Sidecar {
    use sha2::{Digest, Sha256};
    Sidecar {
        meta_schema_version: "1.2".to_owned(),
        schema_version: "1.6".to_owned(),
        failtrace_version: "0.1.0".to_owned(),
        capture_build: None,
        adapters: vec![Adapter {
            id: "gotest".to_owned(),
            version: "gotest@0.1.0".to_owned(),
            kind: AdapterKind::Runner,
            selected: true,
            selection_reason: SelectionReason::ArgvNameMatch,
        }],
        adapter_candidates: vec![],
        argv: vec![
            "go".to_owned(),
            "test".to_owned(),
            "-json".to_owned(),
            "./...".to_owned(),
        ],
        command: "go test -json ./...".to_owned(),
        cwd: "/tmp/gotest_preamble".to_owned(),
        started_at: "2026-05-16T12:00:00Z".to_owned(),
        duration_ms: 100,
        exit_code,
        signal: None,
        early_exit: None,
        truncated: false,
        truncation_reason: None,
        path_case_folding: PathCaseFolding::CaseSensitive,
        stream_merge: StreamMerge::StdoutStderrInterleaved,
        raw_log_sha256: {
            let mut hasher = Sha256::new();
            hasher.update(log);
            format!("{:x}", hasher.finalize())
        },
    }
}

fn load_fixture(scenario: &str) -> Fixture {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/gotest")
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
            if !stable && path == "root.suggested_next_commands" {
                assert_unordered_command_subset(scenario, expected_array, actual_array);
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

fn assert_unordered_command_subset(scenario: &str, expected: &[Value], actual: &[Value]) {
    let expected_by_command = by_key(expected, "command");
    let actual_by_command = by_key(actual, "command");
    assert_eq!(
        actual_by_command.keys().collect::<Vec<_>>(),
        expected_by_command.keys().collect::<Vec<_>>(),
        "{scenario}: unordered suggested command keys"
    );
    for (command, expected_value) in expected_by_command {
        assert_subset(
            scenario,
            expected_value,
            actual_by_command[command],
            &format!("root.suggested_next_commands[{command}]"),
            true,
        );
    }
}

fn by_key<'a>(values: &'a [Value], key: &str) -> BTreeMap<&'a str, &'a Value> {
    values
        .iter()
        .map(|value| {
            (
                value[key]
                    .as_str()
                    .unwrap_or_else(|| panic!("missing string key {key}")),
                value,
            )
        })
        .collect()
}
