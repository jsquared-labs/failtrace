#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    adapters::AnalyzeOptions,
    schema::{
        Command, Counts, DeterminismWarning, DeterminismWarningCode, DocumentKind, EarlyExit,
        Emission, EmissionChannel, EmissionMode, Extensions, Failure, OutputDocument, ParseStatus,
        ParseWarning, Ranking, RetryOutcome, RunnerOutcome, final_outcome,
    },
    storage::{PathCaseFolding, Sidecar, sha256_hex},
};

// Layer 1: all-adapter document mechanics.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UserCodeEcosystem {
    Python,
    JavaScript,
    Rust,
    Go,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedPathKey {
    path: String,
    components: Vec<String>,
    basename: String,
}

pub(super) fn is_user_code_path(
    file: &str,
    ecosystem: UserCodeEcosystem,
    path_case_folding: PathCaseFolding,
) -> bool {
    let key = NormalizedPathKey::new(file, path_case_folding);
    !match ecosystem {
        UserCodeEcosystem::Python => is_python_dependency_path(&key),
        UserCodeEcosystem::JavaScript => is_javascript_dependency_path(&key),
        UserCodeEcosystem::Rust => is_rust_dependency_path(&key),
        UserCodeEcosystem::Go => is_go_dependency_path(&key),
    }
}

impl NormalizedPathKey {
    fn new(file: &str, path_case_folding: PathCaseFolding) -> Self {
        let path = match path_case_folding {
            PathCaseFolding::CaseSensitive => file.replace('\\', "/"),
            PathCaseFolding::CaseInsensitive => file.replace('\\', "/").to_ascii_lowercase(),
        };
        let components = path
            .split('/')
            .filter(|component| !component.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let basename = components.last().cloned().unwrap_or_default();
        Self {
            path,
            components,
            basename,
        }
    }
}

fn is_python_dependency_path(key: &NormalizedPathKey) -> bool {
    contains_component_sequence(key, &["site-packages"])
        || contains_component_sequence(key, &["dist-packages"])
        || starts_with_python_stdlib_prefix(key, &["usr", "lib"])
        || starts_with_python_stdlib_prefix(key, &["usr", "local", "lib"])
        || starts_with_component_sequence(key, &["library", "frameworks", "python.framework"])
}

fn is_javascript_dependency_path(key: &NormalizedPathKey) -> bool {
    contains_component_sequence(key, &["node_modules"])
        || contains_component_sequence(key, &[".pnpm"])
        || contains_component_sequence(key, &[".yarn", "cache"])
        || contains_component_sequence(key, &[".npm", "_cacache"])
        || contains_component_sequence(key, &["_npx"])
        || contains_component_sequence(key, &[".bun", "install", "cache"])
}

fn is_rust_dependency_path(key: &NormalizedPathKey) -> bool {
    contains_component_sequence(key, &[".cargo", "registry"])
        || contains_component_sequence(key, &[".rustup", "toolchains"])
        || contains_component_sequence(key, &["rustc"])
}

fn is_go_dependency_path(key: &NormalizedPathKey) -> bool {
    contains_component_sequence(key, &["src", "testing"])
        || contains_component_sequence(key, &["src", "runtime"])
        || contains_component_sequence(key, &["src", "internal", "runtime"])
        || contains_component_sequence(key, &["internal", "runtime"])
        || contains_component_sequence(key, &["src", "time"])
        || contains_component_sequence(key, &["pkg", "mod"])
        || ends_with_component_sequence(key, &["testing", "testing.go"])
        || ends_with_component_sequence(key, &["runtime", "panic.go"])
        || ends_with_component_sequence(key, &["runtime", "time.go"])
        || ends_with_component_sequence(key, &["time", "sleep.go"])
        || key.basename == "_testmain.go"
}

fn starts_with_python_stdlib_prefix(key: &NormalizedPathKey, prefix: &[&str]) -> bool {
    key.components.len() > prefix.len()
        && starts_with_component_sequence(key, prefix)
        && key.components[prefix.len()].starts_with("python")
}

fn starts_with_component_sequence(key: &NormalizedPathKey, sequence: &[&str]) -> bool {
    key.components.len() >= sequence.len()
        && key
            .components
            .iter()
            .take(sequence.len())
            .map(String::as_str)
            .eq(sequence.iter().copied())
}

fn ends_with_component_sequence(key: &NormalizedPathKey, sequence: &[&str]) -> bool {
    key.components.len() >= sequence.len()
        && key
            .components
            .iter()
            .skip(key.components.len() - sequence.len())
            .map(String::as_str)
            .eq(sequence.iter().copied())
}

fn contains_component_sequence(key: &NormalizedPathKey, sequence: &[&str]) -> bool {
    !sequence.is_empty()
        && key.components.len() >= sequence.len()
        && key.components.windows(sequence.len()).any(|window| {
            window
                .iter()
                .map(String::as_str)
                .eq(sequence.iter().copied())
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BudgetedFailures {
    pub(super) failures: Vec<Failure>,
    pub(super) failures_total: u64,
    pub(super) budgeted: bool,
}

#[derive(Debug)]
pub(super) struct AnalysisDocumentParts<'a> {
    pub(super) sidecar: &'a Sidecar,
    pub(super) log: &'a [u8],
    pub(super) budgeted_failures: BudgetedFailures,
    pub(super) runner_outcome: RunnerOutcome,
    pub(super) early_exit: Option<EarlyExit>,
    pub(super) parse_status: ParseStatus,
    pub(super) parse_warnings: Vec<ParseWarning>,
    pub(super) failures_complete: bool,
    pub(super) ranking: Ranking,
    pub(super) counts: Counts,
    pub(super) suggested_next_commands: Vec<Command>,
    pub(super) extensions: Extensions,
    pub(super) options: AnalyzeOptions,
}

pub(super) fn build_analysis_document(parts: AnalysisDocumentParts<'_>) -> OutputDocument {
    let failures_emitted = parts.budgeted_failures.failures.len() as u64;
    let failures_total = parts.budgeted_failures.failures_total;
    OutputDocument {
        // Always emit the binary's schema version. When `analyze` replays a
        // legacy sidecar, the produced document carries the current shape
        // and must declare it to stay schema_version-consistent.
        schema_version: crate::cli::SCHEMA_VERSION.to_owned(),
        document_kind: DocumentKind::AnalysisResult,
        failtrace_version: env!("CARGO_PKG_VERSION").to_owned(),
        emission: Emission {
            channel: EmissionChannel::File,
            mode: if parts.budgeted_failures.budgeted {
                EmissionMode::Budgeted
            } else {
                EmissionMode::Full
            },
            max_failures: if parts.budgeted_failures.budgeted {
                parts.options.max_failures
            } else {
                None
            },
            failures_emitted,
            failures_total,
            full_output_path: "<run-dir>/output.json".to_owned(),
        },
        command: parts.sidecar.command.clone(),
        cwd: parts.sidecar.cwd.clone(),
        started_at: parts.sidecar.started_at.clone(),
        duration_ms: parts.sidecar.duration_ms,
        exit_code: parts.sidecar.exit_code,
        signal: parts.sidecar.signal.clone(),
        runner_outcome: parts.runner_outcome,
        early_exit: parts.early_exit,
        adapters: parts.sidecar.adapters.clone(),
        parse_status: parts.parse_status,
        parse_warnings: parts.parse_warnings,
        determinism_warnings: determinism_warnings_for_sidecar(parts.sidecar),
        truncated: parts.sidecar.truncated,
        truncation_reason: parts.sidecar.truncation_reason,
        failures_complete: parts.failures_complete,
        failures: parts.budgeted_failures.failures,
        ranking: parts.ranking,
        counts: parts.counts,
        raw_log_path: "<run-dir>/log".to_owned(),
        raw_log_sha256: if parts.sidecar.raw_log_sha256.is_empty() {
            sha256_hex(parts.log)
        } else {
            parts.sidecar.raw_log_sha256.clone()
        },
        suggested_next_commands: parts.suggested_next_commands,
        extensions: parts.extensions,
    }
}

pub(super) fn parse_status_from_warnings(parse_warnings: &[ParseWarning]) -> ParseStatus {
    parse_status_from_warnings_with_context(parse_warnings, |_| false)
}

pub(super) fn parse_status_from_warnings_with_context(
    parse_warnings: &[ParseWarning],
    adapter_contextual_degrades: impl Fn(ParseWarning) -> bool,
) -> ParseStatus {
    if parse_warnings
        .iter()
        .copied()
        .any(|warning| warning_degrades_parse_status(warning, &adapter_contextual_degrades))
    {
        ParseStatus::Partial
    } else {
        ParseStatus::Complete
    }
}

fn warning_degrades_parse_status(
    warning: ParseWarning,
    adapter_contextual_degrades: &impl Fn(ParseWarning) -> bool,
) -> bool {
    match warning {
        ParseWarning::MissingSummaryLine
        | ParseWarning::TruncatedTraceback
        | ParseWarning::MalformedStructuredEvent
        | ParseWarning::MissingTerminalEvent
        | ParseWarning::RunnerSummaryMismatch => true,
        ParseWarning::UnstructuredDiagnostic | ParseWarning::MissingLineNumber => {
            adapter_contextual_degrades(warning)
        }
        ParseWarning::MissingSessionBanner
        | ParseWarning::RunnerVersionUnsupported
        | ParseWarning::CompositeRuntimeDeferred
        | ParseWarning::InterleavedOutput
        | ParseWarning::MissingTestName
        | ParseWarning::AnsiCodesPresentInMessage => false,
    }
}

fn determinism_warnings_for_sidecar(sidecar: &Sidecar) -> Vec<DeterminismWarning> {
    if sidecar.schema_version == crate::cli::SCHEMA_VERSION {
        return vec![];
    }
    vec![DeterminismWarning {
        code: DeterminismWarningCode::SchemaVersionDriftMinor,
        adapter_id: selected_adapter_id(sidecar),
        meta_version: sidecar.schema_version.clone(),
        current_version: crate::cli::SCHEMA_VERSION.to_owned(),
    }]
}

fn selected_adapter_id(sidecar: &Sidecar) -> String {
    sidecar
        .adapters
        .iter()
        .find(|adapter| adapter.selected)
        .or_else(|| sidecar.adapters.first())
        .map(|adapter| adapter.id.clone())
        .unwrap_or_else(|| "core".to_owned())
}

pub(super) fn sidecar_selected_adapter(sidecar: &Sidecar, adapter_id: &str) -> bool {
    sidecar
        .adapters
        .iter()
        .any(|adapter| adapter.id == adapter_id && adapter.selected)
}

pub(super) fn sidecar_boolean_flag_enabled(
    sidecar: &Sidecar,
    long: &str,
    short: Option<&str>,
) -> bool {
    let argv = sidecar_command_tokens(sidecar);
    argv.iter().any(|arg| {
        arg == long
            || short.is_some_and(|short| arg == short)
            || arg
                .strip_prefix(long)
                .and_then(|value| value.strip_prefix('='))
                .is_some_and(|value| value != "false")
    })
}

pub(super) fn sidecar_command_tokens(sidecar: &Sidecar) -> Vec<String> {
    if sidecar.argv.is_empty() {
        sidecar
            .command
            .split_ascii_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    } else {
        sidecar.argv.clone()
    }
}

pub(super) fn truncate_for_budget(
    mut failures: Vec<Failure>,
    options: AnalyzeOptions,
) -> BudgetedFailures {
    let failures_total = failures.len() as u64;
    let budgeted = options
        .max_failures
        .is_some_and(|max_failures| max_failures < failures_total);
    if let Some(max_failures) = options.max_failures
        && failures_total > max_failures
    {
        failures.truncate(max_failures as usize);
    }
    BudgetedFailures {
        failures,
        failures_total,
        budgeted,
    }
}

pub(super) fn validated_major_version(version: &str, validated_majors: &[u64]) -> bool {
    version
        .split_once('.')
        .and_then(|(major, _)| major.parse::<u64>().ok())
        .is_some_and(|major| validated_majors.contains(&major))
}

pub(super) fn zero_test_counts() -> Counts {
    Counts {
        tests_run: 0,
        tests_passed: 0,
        tests_failed: 0,
        tests_errored: 0,
        tests_skipped: 0,
        tests_not_run: None,
        files_failed: None,
    }
}

// Layer 2: summary-runner helpers for pytest, vitest, jest, and cargo.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FailureOutcomeCounts {
    pub(super) failed: u64,
    pub(super) errored: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SummaryRunnerOutcomeSignals {
    pub(super) has_pre_test_failure: bool,
    pub(super) has_failures: bool,
    pub(super) signal_received: bool,
    pub(super) no_tests: bool,
    pub(super) passed: bool,
    pub(super) failed: bool,
    /// The runner's own summary reported failed/errored tests, independent of
    /// whether any failure blocks were parsed. Lets the table classify Failed
    /// when the failure list is empty (unparseable blocks) instead of falling
    /// through to HarnessCrashed; callers must mark the failure list
    /// incomplete when this path can fire.
    pub(super) summary_reports_failures: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SummaryParseResult {
    pub(super) parse_status: ParseStatus,
    pub(super) parse_warnings: Vec<ParseWarning>,
    pub(super) failures_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SummaryParseSignals {
    pub(super) parse_warnings: Vec<ParseWarning>,
    pub(super) contextual_degrading_warnings: Vec<ParseWarning>,
    pub(super) sidecar_truncated: bool,
    pub(super) missing_summary: bool,
    pub(super) incomplete_failure_list: bool,
    pub(super) failure_list_coverage: Vec<FailureListCoverage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FailureListCoverage {
    pub(super) reported_units: u64,
    pub(super) accounted_units: u64,
}

pub(super) fn distinct_failure_files(failures: &[Failure]) -> Option<u64> {
    Some(
        failures
            .iter()
            .map(|failure| failure.primary_location.file.as_str())
            .collect::<BTreeSet<_>>()
            .len() as u64,
    )
}

pub(super) fn failure_outcome_counts(failures: &[Failure]) -> FailureOutcomeCounts {
    let outcomes_by_test = failures
        .iter()
        .filter_map(|failure| {
            failure
                .runner_test_id
                .as_deref()
                .map(|runner_test_id| (runner_test_id, final_outcome(failure)))
        })
        .collect::<BTreeMap<_, _>>();
    FailureOutcomeCounts {
        failed: outcomes_by_test
            .values()
            .filter(|outcome| **outcome == RetryOutcome::Failed)
            .count() as u64,
        errored: outcomes_by_test
            .values()
            .filter(|outcome| **outcome == RetryOutcome::Errored)
            .count() as u64,
    }
}

pub(super) fn summary_parse_result(signals: SummaryParseSignals) -> SummaryParseResult {
    let mut parse_warnings = signals.parse_warnings;
    let mut failures_complete = !signals.sidecar_truncated;
    let failure_list_shortfall = signals
        .failure_list_coverage
        .iter()
        .any(|coverage| coverage.reported_units > coverage.accounted_units);
    if signals.missing_summary && !parse_warnings.contains(&ParseWarning::MissingSummaryLine) {
        parse_warnings.push(ParseWarning::MissingSummaryLine);
    }
    if failure_list_shortfall && !parse_warnings.contains(&ParseWarning::RunnerSummaryMismatch) {
        parse_warnings.push(ParseWarning::RunnerSummaryMismatch);
    }
    if signals.incomplete_failure_list || failure_list_shortfall {
        failures_complete = false;
    }
    if signals.sidecar_truncated && !parse_warnings.contains(&ParseWarning::TruncatedTraceback) {
        parse_warnings.push(ParseWarning::TruncatedTraceback);
    }
    let parse_status = parse_status_from_warnings_with_context(&parse_warnings, |warning| {
        signals.contextual_degrading_warnings.contains(&warning)
    });
    SummaryParseResult {
        parse_status,
        parse_warnings,
        failures_complete,
    }
}

pub(super) fn summary_runner_outcome(signals: SummaryRunnerOutcomeSignals) -> RunnerOutcome {
    if signals.has_pre_test_failure {
        return RunnerOutcome::PreTestFailed;
    }
    if signals.signal_received {
        return RunnerOutcome::HarnessCrashed;
    }
    if !signals.has_failures {
        if signals.no_tests {
            return RunnerOutcome::NoTests;
        }
        if signals.passed {
            return RunnerOutcome::Passed;
        }
        if signals.summary_reports_failures {
            return RunnerOutcome::Failed;
        }
        return RunnerOutcome::HarnessCrashed;
    }
    if signals.failed {
        RunnerOutcome::Failed
    } else {
        RunnerOutcome::HarnessCrashed
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        adapters::AnalyzeOptions,
        schema::{
            Adapter, AdapterKind, Counts, EmissionMode, Location, LocationRole, Ranking,
            RankingStrategy, SelectionReason,
        },
        storage::{PathCaseFolding, StreamMerge},
    };
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn major_version_validation_requires_a_validated_numeric_major() {
        assert!(validated_major_version("8.4.2", &[8, 9]));
        assert!(validated_major_version("9.0.0", &[8, 9]));
        assert!(!validated_major_version("7.4.4", &[8, 9]));
        assert!(!validated_major_version("9plus", &[8, 9]));
    }

    #[test]
    fn user_code_path_uses_component_boundaries_and_ecosystem_denylists() {
        assert!(!is_user_code_path(
            "node_modules/bad-lib/index.d.ts",
            UserCodeEcosystem::JavaScript,
            PathCaseFolding::CaseSensitive
        ));
        assert!(is_user_code_path(
            "src/node_modules_helper.py",
            UserCodeEcosystem::JavaScript,
            PathCaseFolding::CaseSensitive
        ));
        assert!(is_user_code_path(
            "node_modules/pkg/test_mod.py",
            UserCodeEcosystem::Python,
            PathCaseFolding::CaseSensitive
        ));
        assert!(!is_user_code_path(
            ".venv/lib/python3.12/site-packages/pkg/mod.py",
            UserCodeEcosystem::Python,
            PathCaseFolding::CaseSensitive
        ));
        assert!(!is_user_code_path(
            "/home/me/.cargo/registry/src/index.crates.io/lib.rs",
            UserCodeEcosystem::Rust,
            PathCaseFolding::CaseSensitive
        ));
    }

    #[test]
    fn user_code_path_honors_separator_and_case_normalization() {
        assert!(!is_user_code_path(
            r"C:\PROJECT\NODE_MODULES\pkg\index.js",
            UserCodeEcosystem::JavaScript,
            PathCaseFolding::CaseInsensitive
        ));
        assert!(is_user_code_path(
            r"C:\PROJECT\NODE_MODULES\pkg\index.js",
            UserCodeEcosystem::JavaScript,
            PathCaseFolding::CaseSensitive
        ));
    }

    #[test]
    fn go_user_code_path_preserves_runtime_and_generated_file_exclusions() {
        for file in [
            "/usr/local/go/src/testing/testing.go",
            "/usr/local/go/src/runtime/panic.go",
            "/usr/local/go/src/time/sleep.go",
            "/tmp/go-build123/b001/_testmain.go",
            "/Users/me/go/pkg/mod/github.com/acme/lib@v1.2.3/lib.go",
        ] {
            assert!(
                !is_user_code_path(file, UserCodeEcosystem::Go, PathCaseFolding::CaseSensitive),
                "{file} should be non-user Go code"
            );
        }
        assert!(is_user_code_path(
            "internal/runtime_check/check.go",
            UserCodeEcosystem::Go,
            PathCaseFolding::CaseSensitive
        ));
    }

    #[test]
    fn summary_runner_outcome_preserves_the_common_decision_order() {
        assert_eq!(
            summary_runner_outcome(SummaryRunnerOutcomeSignals {
                has_pre_test_failure: true,
                has_failures: false,
                signal_received: true,
                no_tests: false,
                passed: false,
                failed: false,
                summary_reports_failures: false,
            }),
            RunnerOutcome::PreTestFailed
        );
        assert_eq!(
            summary_runner_outcome(SummaryRunnerOutcomeSignals {
                has_pre_test_failure: false,
                has_failures: false,
                signal_received: true,
                no_tests: true,
                passed: true,
                failed: false,
                summary_reports_failures: false,
            }),
            RunnerOutcome::HarnessCrashed
        );
        assert_eq!(
            summary_runner_outcome(SummaryRunnerOutcomeSignals {
                has_pre_test_failure: false,
                has_failures: false,
                signal_received: false,
                no_tests: true,
                passed: false,
                failed: false,
                summary_reports_failures: false,
            }),
            RunnerOutcome::NoTests
        );
        assert_eq!(
            summary_runner_outcome(SummaryRunnerOutcomeSignals {
                has_pre_test_failure: false,
                has_failures: false,
                signal_received: false,
                no_tests: false,
                passed: true,
                failed: false,
                summary_reports_failures: false,
            }),
            RunnerOutcome::Passed
        );
        assert_eq!(
            summary_runner_outcome(SummaryRunnerOutcomeSignals {
                has_pre_test_failure: false,
                has_failures: true,
                signal_received: false,
                no_tests: false,
                passed: false,
                failed: true,
                summary_reports_failures: false,
            }),
            RunnerOutcome::Failed
        );
    }

    #[test]
    fn summary_runner_outcome_trusts_summary_failures_when_no_blocks_parsed() {
        // Runner summary reported failures but no failure blocks were parsed
        // (e.g. pytest -q with an exception type the block parser rejects):
        // Failed, not HarnessCrashed.
        assert_eq!(
            summary_runner_outcome(SummaryRunnerOutcomeSignals {
                has_pre_test_failure: false,
                has_failures: false,
                signal_received: false,
                no_tests: false,
                passed: false,
                failed: true,
                summary_reports_failures: true,
            }),
            RunnerOutcome::Failed
        );
        // An external signal kill still wins over the summary evidence.
        assert_eq!(
            summary_runner_outcome(SummaryRunnerOutcomeSignals {
                has_pre_test_failure: false,
                has_failures: false,
                signal_received: true,
                no_tests: false,
                passed: false,
                failed: true,
                summary_reports_failures: true,
            }),
            RunnerOutcome::HarnessCrashed
        );
    }

    #[test]
    fn summary_parse_result_adds_missing_summary_and_truncation_warnings_once() {
        let result = summary_parse_result(SummaryParseSignals {
            parse_warnings: vec![ParseWarning::MissingSummaryLine],
            contextual_degrading_warnings: vec![],
            sidecar_truncated: true,
            missing_summary: true,
            incomplete_failure_list: true,
            failure_list_coverage: vec![],
        });
        assert_eq!(
            result.parse_warnings,
            vec![
                ParseWarning::MissingSummaryLine,
                ParseWarning::TruncatedTraceback
            ]
        );
        assert_eq!(result.parse_status, ParseStatus::Partial);
        assert!(!result.failures_complete);
    }

    #[test]
    fn summary_parse_result_reconciles_reported_and_accounted_failure_units() {
        for (reported_units, accounted_units, expects_shortfall) in
            [(2, 1, true), (2, 2, false), (1, 2, false)]
        {
            let result = summary_parse_result(SummaryParseSignals {
                parse_warnings: vec![],
                contextual_degrading_warnings: vec![],
                sidecar_truncated: false,
                missing_summary: false,
                incomplete_failure_list: false,
                failure_list_coverage: vec![FailureListCoverage {
                    reported_units,
                    accounted_units,
                }],
            });
            assert_eq!(
                result
                    .parse_warnings
                    .contains(&ParseWarning::RunnerSummaryMismatch),
                expects_shortfall
            );
            assert_eq!(
                result.parse_status == ParseStatus::Partial,
                expects_shortfall
            );
            assert_eq!(!result.failures_complete, expects_shortfall);
        }
    }

    #[test]
    fn summary_parse_result_deduplicates_existing_mismatch_warning() {
        let result = summary_parse_result(SummaryParseSignals {
            parse_warnings: vec![ParseWarning::RunnerSummaryMismatch],
            contextual_degrading_warnings: vec![],
            sidecar_truncated: false,
            missing_summary: false,
            incomplete_failure_list: false,
            failure_list_coverage: vec![FailureListCoverage {
                reported_units: 2,
                accounted_units: 1,
            }],
        });
        assert_eq!(
            result.parse_warnings,
            vec![ParseWarning::RunnerSummaryMismatch]
        );
        assert!(!result.failures_complete);
    }

    #[test]
    fn parse_status_degrades_only_for_contract_affecting_warnings() {
        for warning in [
            ParseWarning::MissingSummaryLine,
            ParseWarning::TruncatedTraceback,
            ParseWarning::MalformedStructuredEvent,
            ParseWarning::MissingTerminalEvent,
            ParseWarning::RunnerSummaryMismatch,
        ] {
            assert_eq!(parse_status_from_warnings(&[warning]), ParseStatus::Partial);
        }

        for warning in [
            ParseWarning::MissingSessionBanner,
            ParseWarning::RunnerVersionUnsupported,
            ParseWarning::InterleavedOutput,
            ParseWarning::MissingTestName,
            ParseWarning::AnsiCodesPresentInMessage,
            ParseWarning::CompositeRuntimeDeferred,
            ParseWarning::UnstructuredDiagnostic,
            ParseWarning::MissingLineNumber,
        ] {
            assert_eq!(
                parse_status_from_warnings(&[warning]),
                ParseStatus::Complete
            );
        }
    }

    #[test]
    fn adapter_contextual_warnings_can_degrade_parse_status() {
        assert_eq!(
            parse_status_from_warnings_with_context(
                &[ParseWarning::UnstructuredDiagnostic],
                |warning| warning == ParseWarning::UnstructuredDiagnostic
            ),
            ParseStatus::Partial
        );
    }

    #[test]
    fn budget_truncation_keeps_total_separate_from_emitted_failures() {
        let failures = vec![failure("a.py"), failure("b.py"), failure("c.py")];
        let budgeted = truncate_for_budget(
            failures,
            AnalyzeOptions {
                max_failures: Some(2),
            },
        );
        assert!(budgeted.budgeted);
        assert_eq!(budgeted.failures_total, 3);
        assert_eq!(budgeted.failures.len(), 2);
    }

    #[test]
    fn build_analysis_document_uses_sidecar_fields_and_budget_accounting() {
        let sidecar = sidecar("pytest");
        let budgeted_failures = truncate_for_budget(
            vec![failure("a.py"), failure("b.py")],
            AnalyzeOptions {
                max_failures: Some(1),
            },
        );
        let document = build_analysis_document(AnalysisDocumentParts {
            sidecar: &sidecar,
            log: b"log bytes",
            budgeted_failures,
            runner_outcome: RunnerOutcome::Failed,
            early_exit: None,
            parse_status: ParseStatus::Complete,
            parse_warnings: vec![],
            failures_complete: true,
            ranking: Ranking {
                strategy: RankingStrategy::FirstEmitted,
                is_stable_across_runs: true,
            },
            counts: zero_test_counts(),
            suggested_next_commands: vec![],
            extensions: BTreeMap::new(),
            options: AnalyzeOptions {
                max_failures: Some(1),
            },
        });

        assert_eq!(document.document_kind, DocumentKind::AnalysisResult);
        assert_eq!(document.emission.mode, EmissionMode::Budgeted);
        assert_eq!(document.emission.max_failures, Some(1));
        assert_eq!(document.emission.failures_emitted, 1);
        assert_eq!(document.emission.failures_total, 2);
        assert_eq!(document.raw_log_sha256, sha256_hex(b"log bytes"));
    }

    #[test]
    fn failure_count_primitives_ignore_non_test_failures_for_outcomes() {
        let failures = vec![failure("a.py"), pre_test_failure("b.py")];
        let counts = failure_outcome_counts(&failures);
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.errored, 0);
        assert_eq!(distinct_failure_files(&failures), Some(2));
    }

    #[test]
    fn failure_outcome_counts_are_unique_by_runner_test_id() {
        let first = failure("a.py");
        let mut retry = failure("retry.py");
        retry.runner_test_id = first.runner_test_id.clone();

        let counts = failure_outcome_counts(&[first, retry]);

        assert_eq!(counts.failed, 1);
        assert_eq!(counts.errored, 0);
    }

    #[test]
    fn sidecar_selected_adapter_requires_selected_match() {
        let sidecar = sidecar("pytest");
        assert!(sidecar_selected_adapter(&sidecar, "pytest"));
        assert!(!sidecar_selected_adapter(&sidecar, "vitest"));
    }

    fn sidecar(adapter_id: &str) -> Sidecar {
        Sidecar {
            meta_schema_version: "1.1".to_owned(),
            schema_version: "1.4".to_owned(),
            failtrace_version: "0.1.0".to_owned(),
            capture_build: None,
            adapters: vec![Adapter {
                id: adapter_id.to_owned(),
                version: format!("{adapter_id}@0.1.0"),
                kind: AdapterKind::Runner,
                selected: true,
                selection_reason: SelectionReason::ArgvNameMatch,
            }],
            adapter_candidates: vec![],
            argv: vec![adapter_id.to_owned()],
            command: adapter_id.to_owned(),
            cwd: "/tmp/failtrace".to_owned(),
            started_at: "2026-05-13T12:00:00Z".to_owned(),
            duration_ms: 10,
            exit_code: Some(1),
            signal: None,
            early_exit: None,
            truncated: false,
            truncation_reason: None,
            path_case_folding: PathCaseFolding::CaseSensitive,
            stream_merge: StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
        }
    }

    fn failure(file: &str) -> Failure {
        let mut failure = pre_test_failure(file);
        failure.phase = crate::schema::Phase::Test;
        failure.runner_test_id = Some(format!("{file}::test_x"));
        failure
    }

    fn pre_test_failure(file: &str) -> Failure {
        Failure {
            failure_id: String::new(),
            adapter_id: "pytest".to_owned(),
            phase: crate::schema::Phase::Collect,
            kind: crate::schema::FailureKind::Assertion,
            runner_package_id: None,
            runner_test_id: None,
            test_segments: vec![],
            test_name: None,
            param_id: None,
            test_group_id: None,
            primary_location: Location {
                file: file.to_owned(),
                line: Some(1),
                column: None,
                end_line: None,
                end_column: None,
                role: LocationRole::Primary,
                in_user_code: true,
            },
            extra_locations: vec![],
            message: "failed".to_owned(),
            diagnostic_code: None,
            diagnostic_severity: crate::schema::DiagnosticSeverity::Error,
            details: crate::schema::Details::Unknown {},
            raw_log_lines: [1, 1],
            evidence: None,
            cascade_root: None,
            cascade_reason: None,
            dependent_failure_ids: vec![],
            retries: vec![],
            companion_diagnostics: vec![],
            extensions: BTreeMap::new(),
        }
    }

    #[test]
    fn zero_test_counts_are_empty_and_unattributed() {
        assert_eq!(
            zero_test_counts(),
            Counts {
                tests_run: 0,
                tests_passed: 0,
                tests_failed: 0,
                tests_errored: 0,
                tests_skipped: 0,
                tests_not_run: None,
                files_failed: None,
            }
        );
    }
}
