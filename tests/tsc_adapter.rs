use std::{fs, path::Path};

use failtrace::{
    adapters::{AdapterError, AnalyzeOptions, tsc::analyze_log},
    schema::{
        FailureKind, OutputDocument, ParseStatus, ParseWarning, Phase, RankingStrategy,
        RunnerOutcome, validate,
    },
    storage::Sidecar,
};
use serde_json::Value;

const SCENARIOS: &[&str] = &[
    "basic_type_mismatch",
    "missing_symbol",
    "module_not_found",
    "syntax_error",
    "pretty_ansi_codeframe",
    "global_cli_error",
    "config_no_inputs",
    "multiple_errors_budgeted",
    "build_project_reference",
    "node_modules_location",
    "cross_file_chain",
    "destr_argument_type",
];

#[test]
fn tsc_adapter_reproduces_fixture_hashes_and_valid_documents() {
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

    reproduced.sort();
    assert_eq!(
        reproduced,
        [
            "000a99e561f92fd6",
            "1037a055ee38fc78",
            "11aeec65904fcaeb",
            "17dd4d2736ef362c",
            "241e7589d1d91419",
            "3d8c7f6f3b89d933",
            "655e161625abad08",
            "70cf08926cd33484",
            "80720e4d3ebecd68",
            "a5ec64164490500e",
            "d7e3e1d5d5c769d3",
            "e43ee1318ff615ae",
            "eebbf94448a57192",
            "fb4e796cb978c07f",
            "fffd8397a5fa6f49",
        ]
    );
}

#[test]
fn basic_type_mismatch_is_type_error_build_failure() {
    let document = analyze_fixture("basic_type_mismatch");
    let failure = &document.failures[0];

    assert_eq!(document.runner_outcome, RunnerOutcome::TypecheckFailed);
    assert_eq!(
        document.ranking.strategy,
        RankingStrategy::CompileBeforeRuntime
    );
    assert!(document.ranking.is_stable_across_runs);
    assert_eq!(failure.phase, Phase::Typecheck);
    assert_eq!(failure.kind, FailureKind::TypeError);
    assert_eq!(failure.diagnostic_code.as_deref(), Some("TS2322"));
    assert_eq!(failure.primary_location.file, "src/index.ts");
    assert_eq!(failure.primary_location.line, Some(1));
    assert_eq!(failure.primary_location.column, Some(7));
    assert_eq!(
        failure.extensions["tsc"]["column_unit"].as_str(),
        Some("utf16_code_unit")
    );
    assert_eq!(
        failure.extensions["tsc"]["phase_detail"].as_str(),
        Some("typecheck")
    );
}

#[test]
fn response_file_sidecar_is_rejected_during_replay() {
    let fixture = load_fixture("basic_type_mismatch");
    let mut sidecar = fixture.sidecar;
    sidecar.command = "tsc @watch.args".to_owned();
    sidecar.argv = vec!["tsc".to_owned(), "@watch.args".to_owned()];
    let result = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    );

    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { reason })
            if reason.contains("response files")
    ));
}

#[test]
fn wrapper_package_specs_are_not_mistaken_for_response_files() {
    let fixture = load_fixture("basic_type_mismatch");
    let mut sidecar = fixture.sidecar;
    sidecar.argv = [
        "npx",
        "--package",
        "typescript",
        "--package",
        "@types/node",
        "tsc",
        "--noEmit",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let document = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .expect("wrapper package specs should not affect tsc replay admission");
    assert_eq!(document.parse_status, ParseStatus::Complete);
}

#[test]
fn watch_mode_sidecar_is_rejected_during_replay() {
    let fixture = load_fixture("basic_type_mismatch");
    let mut sidecar = fixture.sidecar;
    sidecar.argv = vec!["tsc".to_owned(), "--watch".to_owned()];
    let result = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    );
    assert!(matches!(
        result,
        Err(AdapterError::UnrecognizedFormat { reason }) if reason.contains("watch mode")
    ));
}

#[test]
fn signal_after_parsed_diagnostic_marks_failure_list_incomplete() {
    let fixture = load_fixture("basic_type_mismatch");
    let mut sidecar = fixture.sidecar;
    sidecar.exit_code = None;
    sidecar.signal = Some("SIGKILL".to_owned());
    let document = analyze_log(
        &fixture.log,
        &sidecar,
        AnalyzeOptions { max_failures: None },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 1);
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::MissingTerminalEvent)
    );
    assert!(!document.failures_complete);
}

#[test]
fn empty_signal_kill_marks_synthetic_failure_list_incomplete() {
    let mut sidecar = load_fixture("basic_type_mismatch").sidecar;
    sidecar.exit_code = None;
    sidecar.signal = Some("SIGTERM".to_owned());
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(b"");
    let document = analyze_log(b"", &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 1);
    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::MissingTerminalEvent)
    );
    assert!(!document.failures_complete);
}

#[test]
fn missing_symbol_and_module_not_found_use_missing_symbol_kind() {
    for scenario in ["missing_symbol", "module_not_found"] {
        let document = analyze_fixture(scenario);
        assert_eq!(
            document.failures[0].kind,
            FailureKind::MissingSymbol,
            "{scenario}"
        );
    }
}

#[test]
fn syntax_error_uses_syntax_kind() {
    let document = analyze_fixture("syntax_error");

    assert_eq!(document.failures[0].kind, FailureKind::SyntaxError);
    assert_eq!(
        document.failures[0].diagnostic_code.as_deref(),
        Some("TS1005")
    );
}

#[test]
fn pretty_ansi_codeframe_strips_ansi_for_location_and_marks_excerpt() {
    let document = analyze_fixture("pretty_ansi_codeframe");
    let failure = &document.failures[0];

    assert_eq!(failure.primary_location.file, "src/index.ts");
    assert_eq!(failure.diagnostic_code.as_deref(), Some("TS2322"));
    assert!(
        failure.evidence.as_ref().unwrap().excerpts[0].ansi_stripped,
        "pretty fixture should preserve ANSI in evidence while parsing stripped text"
    );
}

#[test]
fn global_cli_error_uses_project_fallback_location() {
    let document = analyze_fixture("global_cli_error");
    let failure = &document.failures[0];

    assert_eq!(failure.kind, FailureKind::Unknown);
    assert_eq!(failure.diagnostic_code.as_deref(), Some("TS5023"));
    assert_eq!(failure.primary_location.file, "tsconfig.json");
    assert_eq!(failure.primary_location.line, None);
    assert!(
        document
            .parse_warnings
            .contains(&failtrace::schema::ParseWarning::MissingLineNumber)
    );
    assert!(
        document
            .parse_warnings
            .contains(&failtrace::schema::ParseWarning::UnstructuredDiagnostic)
    );
}

#[test]
fn config_no_inputs_keeps_native_tsconfig_location() {
    let document = analyze_fixture("config_no_inputs");
    let failure = &document.failures[0];

    assert_eq!(failure.diagnostic_code.as_deref(), Some("TS18002"));
    assert_eq!(failure.primary_location.file, "tsconfig.json");
    assert_eq!(failure.primary_location.line, Some(7));
    assert_eq!(document.parse_status, ParseStatus::Complete);
}

#[test]
fn multiple_errors_preserve_order_and_budgeting_metadata() {
    let fixture = load_fixture("multiple_errors_budgeted");
    let document = analyze_log(
        &fixture.log,
        &fixture.sidecar,
        AnalyzeOptions {
            max_failures: Some(2),
        },
    )
    .unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.emission.failures_emitted, 2);
    assert_eq!(document.emission.failures_total, 3);
    assert_eq!(
        document.failures[0].diagnostic_code.as_deref(),
        Some("TS2322")
    );
    assert_eq!(
        document.failures[1].diagnostic_code.as_deref(),
        Some("TS2304")
    );
    assert_eq!(document.counts.files_failed, Some(3));
}

#[test]
fn build_project_reference_parses_single_project_build_output() {
    let document = analyze_fixture("build_project_reference");
    let failure = &document.failures[0];

    assert_eq!(failure.primary_location.file, "packages/app/src/index.ts");
    assert_eq!(failure.kind, FailureKind::TypeError);
    assert_eq!(document.runner_outcome, RunnerOutcome::TypecheckFailed);
}

#[test]
fn node_modules_location_is_preserved_and_unknown_code_is_complete_with_advisory_warning() {
    let document = analyze_fixture("node_modules_location");
    let failure = &document.failures[0];

    assert_eq!(
        failure.primary_location.file,
        "node_modules/bad-lib/index.d.ts"
    );
    assert_eq!(failure.kind, FailureKind::Unknown);
    assert_eq!(document.parse_status, ParseStatus::Complete);
}

#[test]
fn standalone_non_error_diagnostics_are_dropped() {
    let log = b"src/index.ts(1,1): warning TS9999: synthetic warning\n";
    let mut sidecar = load_fixture("basic_type_mismatch").sidecar;
    sidecar.exit_code = Some(0);
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(document.runner_outcome, RunnerOutcome::Passed);
    assert!(document.failures.is_empty());
}

#[test]
fn companions_are_never_attached_via_adjacency() {
    // Locked behavior: adjacency-based companion attachment is
    // forbidden — only structurally chained diagnostics may become companions,
    // and v0.5-alpha does not parse tsc's relatedInformation chain (deferred to
    // v1.5 alongside tsc_intra_diagnostic_chain). A warning sitting between two
    // errors must NOT be attached as a companion to either.
    let log = b"src/index.ts(1,7): error TS2322: Type 'string' is not assignable to type 'number'.\nsrc/index.ts(1,1): warning TS9999: synthetic warning\nsrc/index.ts(2,1): error TS2304: Cannot find name 'foo'.\n";
    let mut sidecar = load_fixture("basic_type_mismatch").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(document.failures.len(), 2);
    assert!(document.failures[0].companion_diagnostics.is_empty());
    assert!(document.failures[1].companion_diagnostics.is_empty());
}

#[test]
fn global_fallback_prefers_project_then_build_then_json_then_default() {
    let cases = [
        (
            "tsc --project configs/tsconfig.app.json --bad",
            "configs/tsconfig.app.json",
        ),
        ("tsc -b packages/app --bad", "packages/app"),
        ("tsc custom.json --bad", "custom.json"),
        ("tsc --bad", "tsconfig.json"),
    ];

    for (command, expected_file) in cases {
        let log = b"error TS5023: Unknown compiler option '--bad'.\n";
        let mut sidecar = load_fixture("global_cli_error").sidecar;
        sidecar.command = command.to_owned();
        sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
        let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();
        assert_eq!(document.failures[0].primary_location.file, expected_file);
    }
}

#[test]
fn global_fallback_accepts_equals_form_of_project_and_build_flags() {
    // TypeScript's CLI accepts both space-separated and equals-form for
    // --project and --build (e.g. `tsc --project=tsconfig.json` is documented
    // tsc CLI). The global-diagnostic fallback accepts both forms rather than
    // only `argv[i+1]`. This test guards against regressing to the
    // space-separated-only behavior.
    let cases = [
        (
            "tsc --project=configs/tsconfig.app.json --bad",
            "configs/tsconfig.app.json",
        ),
        ("tsc --build=packages/app --bad", "packages/app"),
    ];

    for (command, expected_file) in cases {
        let log = b"error TS5023: Unknown compiler option '--bad'.\n";
        let mut sidecar = load_fixture("global_cli_error").sidecar;
        sidecar.command = command.to_owned();
        sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
        let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();
        assert_eq!(
            document.failures[0].primary_location.file, expected_file,
            "command: {command}"
        );
    }
}

#[test]
fn unparseable_reported_error_emits_unknown_failure_with_incomplete_list() {
    let log = b"Found 1 error in a compiler output shape this adapter cannot parse.\n";
    let mut sidecar = load_fixture("basic_type_mismatch").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(document.parse_status, ParseStatus::Partial);
    assert_eq!(document.failures.len(), 1);
    assert_eq!(document.failures[0].kind, FailureKind::Unknown);
    assert_eq!(document.failures[0].diagnostic_code, None);
    assert!(
        document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(!document.failures_complete);
}

#[test]
fn reported_error_count_above_extracted_diagnostics_marks_list_incomplete() {
    let log = b"src/index.ts(1,7): error TS2322: Type 'string' is not assignable to type 'number'.\nFound 2 errors in 1 file.\n";
    let mut sidecar = load_fixture("basic_type_mismatch").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.runner_outcome, RunnerOutcome::TypecheckFailed);
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
fn failure_coverage_uses_full_list_before_emission_budget() {
    let log = b"src/a.ts(1,7): error TS2322: Type 'string' is not assignable to type 'number'.\nsrc/b.ts(1,13): error TS2304: Cannot find name 'missing'.\nFound 2 errors in 2 files.\n";
    let mut sidecar = load_fixture("basic_type_mismatch").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(
        log,
        &sidecar,
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
fn multiple_error_summary_scopes_are_conservatively_incomplete() {
    let log = b"src/a.ts(1,7): error TS2322: Type 'string' is not assignable to type 'number'.\nFound 1 error in src/a.ts.\nFound 1 error in src/b.ts.\n";
    let mut sidecar = load_fixture("basic_type_mismatch").sidecar;
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
fn multiple_error_summary_scopes_are_complete_when_each_error_is_extracted() {
    let log = b"src/a.ts(1,7): error TS2322: Type 'string' is not assignable to type 'number'.\nFound 1 error in src/a.ts.\nsrc/b.ts(1,13): error TS2304: Cannot find name 'missing'.\nFound 1 error in src/b.ts.\n";
    let mut sidecar = load_fixture("basic_type_mismatch").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    let document = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None }).unwrap();

    assert_eq!(validate(&document), Vec::new());
    assert_eq!(document.failures.len(), 2);
    assert!(
        !document
            .parse_warnings
            .contains(&ParseWarning::RunnerSummaryMismatch)
    );
    assert!(document.failures_complete);
}

#[test]
fn log_without_sidecar_pin_is_rejected_at_admission() {
    // Sidecar pin is authoritative for tsc admission. When
    // no adapter in sidecar.adapters[] selects tsc, the analyzer must reject
    // at the InvalidSidecar gate — the alternative would let tsc claim
    // arbitrary captured output it didn't produce.
    let log = b"hello from a different program\n";
    let mut sidecar = load_fixture("basic_type_mismatch").sidecar;
    sidecar.raw_log_sha256 = failtrace::storage::sha256_hex(log);
    // Strip the tsc adapter pin: simulate a sidecar that came from a non-tsc
    // capture (e.g., someone routed an arbitrary log into tsc::analyze_log).
    sidecar.adapters.retain(|adapter| adapter.id != "tsc");
    let result = analyze_log(log, &sidecar, AnalyzeOptions { max_failures: None });

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

fn load_fixture(scenario: &str) -> Fixture {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tsc")
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
