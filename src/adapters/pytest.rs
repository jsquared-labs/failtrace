use std::{path::Path, sync::OnceLock};

use regex::Regex;

use crate::{
    adapters::{
        AdapterError, AdapterImpl, AnalyzeOptions,
        shared::{
            AnalysisDocumentParts, FailureListCoverage, SummaryParseSignals,
            SummaryRunnerOutcomeSignals, UserCodeEcosystem, build_analysis_document,
            distinct_failure_files, failure_outcome_counts, is_user_code_path,
            sidecar_command_tokens, sidecar_selected_adapter, summary_parse_result,
            summary_runner_outcome, truncate_for_budget, validated_major_version, zero_test_counts,
        },
    },
    schema::{
        ByteRange, Command, Counts, Details, DiagnosticSeverity, Diff, Evidence, Excerpt,
        ExcerptKind, ExcerptSource, Failure, FailureKind, Frame, Location, LocationRole,
        OutputDocument, ParseWarning, Phase, Ranking, RankingStrategy, RunnerOutcome,
        derive_failure_id, derive_test_group_id, strip_params_pytest, validate,
    },
    storage::{PathCaseFolding, Sidecar},
};

const ADAPTER_ID: &str = "pytest";
// Pytest's banner version is the output-family boundary. Add a major only after
// fixture or real-world evidence shows this adapter handles that family correctly.
const VALIDATED_PYTEST_MAJOR_VERSIONS: &[u64] = &[8, 9];

const PYTHON_VALUE_FLAGS: &[&str] = &["-c", "-W", "-X", "--check-hash-based-pycs"];
const UV_VALUE_FLAGS: &[&str] = &[
    "--with",
    "--with-requirements",
    "--with-editable",
    "--python",
    "--from",
    "--package",
    "--directory",
    "--refresh-package",
    "--override",
    "--constraint",
    "--index-url",
    "--extra-index-url",
    "--find-links",
    "--config-setting",
    "--extra",
    "--group",
    "--cache-dir",
    "--config-file",
];
const POETRY_VALUE_FLAGS: &[&str] = &[
    "--env",
    "--with",
    "--without",
    "--only",
    "--extras",
    "--directory",
    "--project",
];
const PIPENV_VALUE_FLAGS: &[&str] = &["--python", "--where"];
const COVERAGE_VALUE_FLAGS: &[&str] = &[
    "--rcfile",
    "--source",
    "--include",
    "--omit",
    "--data-file",
    "--concurrency",
    "--debug",
];

pub(crate) fn command_is_supported(argv: &[String]) -> bool {
    pytest_command_args(argv).is_some_and(|args| !pytest_mode_unsupported(args))
}

fn command_uses_xdist(argv: &[String]) -> bool {
    let Some(args) = pytest_command_args(argv) else {
        return false;
    };
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let workers = if matches!(arg.as_str(), "-n" | "--numprocesses") {
            args.get(index + 1).map(String::as_str)
        } else if let Some(value) = arg.strip_prefix("--numprocesses=") {
            Some(value)
        } else {
            arg.strip_prefix("-n")
                .and_then(|value| value.strip_prefix('=').or(Some(value)))
                .filter(|value| !value.is_empty())
        };
        if workers.is_some_and(|value| value != "0") {
            return true;
        }
        index += 1;
    }
    false
}

fn pytest_command_args(argv: &[String]) -> Option<&[String]> {
    let first = argv.first()?;
    let basename = Path::new(first).file_name()?.to_str()?;

    if basename == "pytest" {
        return Some(&argv[1..]);
    }

    if is_python_basename(basename) {
        return find_m_module_args(&argv[1..], "pytest", PYTHON_VALUE_FLAGS);
    }

    if basename == "coverage" {
        if argv.get(1).map(String::as_str) != Some("run") {
            return None;
        }
        return find_m_module_args(&argv[2..], "pytest", COVERAGE_VALUE_FLAGS);
    }

    let wrapper_flags: &[&str] = match basename {
        "uv" => UV_VALUE_FLAGS,
        "poetry" => POETRY_VALUE_FLAGS,
        "pipenv" => PIPENV_VALUE_FLAGS,
        _ => return None,
    };
    if argv.get(1).map(String::as_str) != Some("run") {
        return None;
    }
    let offset = skip_wrapper_flags(&argv[2..], wrapper_flags);
    pytest_command_args(argv.get(2 + offset..)?)
}

fn pytest_mode_unsupported(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--looponfail"
            || arg.starts_with("--looponfail=")
            || matches!(arg.as_str(), "--pdb" | "--trace" | "--capture=no")
            || (arg == "--capture" && args.get(index + 1).is_some_and(|value| value == "no"))
            || arg.starts_with('@')
        {
            return true;
        }

        if let Some(cluster) = arg
            .strip_prefix('-')
            .filter(|rest| !rest.is_empty() && !rest.starts_with('-'))
        {
            let mut options = cluster.chars();
            while let Some(option) = options.next() {
                if matches!(option, 's' | 'f') {
                    return true;
                }
                if pytest_short_option_takes_value(option) {
                    if options.as_str().is_empty() {
                        index += 1;
                    }
                    break;
                }
            }
        }
        index += 1;
    }
    false
}

fn pytest_short_option_takes_value(option: char) -> bool {
    matches!(option, 'k' | 'm' | 'r' | 'o' | 'p' | 'W' | 'c' | 'n')
}

fn is_python_basename(basename: &str) -> bool {
    basename == "python" || basename == "python3" || basename.starts_with("python3.")
}

fn skip_wrapper_flags(args: &[String], value_flags: &[&str]) -> usize {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return index + 1;
        }
        if !arg.starts_with('-') {
            return index;
        }
        if arg.contains('=') {
            index += 1;
        } else if value_flags.contains(&arg.as_str()) {
            index += 2;
        } else {
            index += 1;
        }
    }
    index
}

fn find_m_module_args<'a>(
    args: &'a [String],
    target_module: &str,
    value_flags: &[&str],
) -> Option<&'a [String]> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "-m" {
            return (args.get(index + 1).map(String::as_str) == Some(target_module))
                .then(|| &args[index + 2..]);
        }
        if !arg.starts_with('-') {
            return None;
        }
        if arg.contains('=') {
            index += 1;
        } else if value_flags.contains(&arg.as_str()) {
            index += 2;
        } else {
            index += 1;
        }
    }
    None
}

pub fn analyze_log(
    log: &[u8],
    sidecar: &Sidecar,
    options: AnalyzeOptions,
) -> Result<OutputDocument, AdapterError> {
    PytestAdapter::analyze_log(log, sidecar, options)
}

struct PytestAdapter;

impl AdapterImpl for PytestAdapter {
    fn analyze_log(
        log: &[u8],
        sidecar: &Sidecar,
        options: AnalyzeOptions,
    ) -> Result<OutputDocument, AdapterError> {
        if !sidecar_selected_adapter(sidecar, ADAPTER_ID) {
            return Err(AdapterError::InvalidSidecar {
                reason: "sidecar does not select pytest adapter".to_owned(),
            });
        }
        let command = sidecar_command_tokens(sidecar);
        if pytest_command_args(&command).is_some_and(pytest_mode_unsupported) {
            return Err(AdapterError::UnrecognizedFormat {
                reason: "pytest interactive, loop-on-fail, and response-file modes are unsupported"
                    .to_owned(),
            });
        }

        let parsed = PytestParse::from_log_with_path_case_folding(log, sidecar.path_case_folding);
        if parsed.completed_summary_count > 1 {
            return Err(AdapterError::UnrecognizedFormat {
                reason: "multiple completed pytest session summaries are unsupported".to_owned(),
            });
        }
        if !parsed.looks_like_pytest(sidecar) {
            return Err(AdapterError::UnrecognizedFormat {
                reason: "no pytest banner, collected line, parsed failure block, summary entry, or recognizable final-counts line found".to_owned(),
            });
        }

        let mut document = lower(parsed, log, sidecar, options);
        let violations = validate(&document);
        if !violations.is_empty() {
            return Err(AdapterError::InvariantViolation(violations));
        }

        document = sidecar.apply_replay_fields_to_document(document);
        let violations = validate(&document);
        if !violations.is_empty() {
            return Err(AdapterError::InvariantViolation(violations));
        }

        Ok(document)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PytestParse {
    banner: Option<PytestBanner>,
    collected_count: Option<u64>,
    progress_lines: Vec<String>,
    failure_blocks: Vec<PytestBlock>,
    summary_entries: Vec<SummaryEntry>,
    final_counts: Option<FinalCounts>,
    completed_summary_count: u64,
    has_unaccounted_run_failure: bool,
    has_xdist_parallelism: bool,
    parse_warnings: Vec<ParseWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PytestBanner {
    pytest_version: String,
}

impl PytestBanner {
    // EXTRACT-CANDIDATE: adapter version family validation is shared policy with runner-specific validated majors.
    fn has_validated_output_family(&self) -> bool {
        validated_major_version(&self.pytest_version, VALIDATED_PYTEST_MAJOR_VERSIONS)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PytestBlock {
    title: String,
    outcome: SummaryOutcome,
    phase: Phase,
    kind: FailureKind,
    primary_location: Location,
    message: String,
    exception_type: Option<String>,
    raw_log_lines: [u64; 2],
    byte_range: ByteRange,
    excerpt_text: String,
    message_ansi_stripped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SummaryEntry {
    outcome: SummaryOutcome,
    runner_test_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummaryOutcome {
    Failed,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct FinalCounts {
    failed: u64,
    passed: u64,
    errors: u64,
    skipped: u64,
    deselected: u64,
    xfailed: u64,
    xpassed: u64,
    warnings: u64,
    reruns: u64,
    no_tests_ran: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogLine {
    number: u64,
    start: u64,
    end: u64,
    raw: String,
    stripped: String,
}

impl PytestParse {
    #[cfg(test)]
    fn from_log(log: &[u8]) -> Self {
        Self::from_log_with_path_case_folding(log, PathCaseFolding::CaseSensitive)
    }

    fn from_log_with_path_case_folding(log: &[u8], path_case_folding: PathCaseFolding) -> Self {
        let lines = split_lines(log);
        let banner = parse_banner(&lines);
        let collected_count = parse_collected_count(&lines);
        let progress_lines = parse_progress_lines(&lines);
        let summary_entries = parse_summary_entries(&lines);
        let final_counts = parse_final_counts(&lines);
        let completed_summary_count = completed_summary_count(&lines);
        let has_unaccounted_run_failure = lines.iter().any(|line| {
            let lower = line.stripped.to_ascii_lowercase();
            lower.starts_with("fail required test coverage") || lower.contains("coverage: error:")
        });
        let has_xdist_parallelism = lines.iter().any(|line| {
            let text = line.stripped.trim();
            text.starts_with("scheduling tests via ")
                || (text.starts_with("created: ") && text.ends_with(" workers"))
                || text
                    .split_once(" workers [")
                    .is_some_and(|(count, _)| count.parse::<u64>().is_ok())
        });
        let failure_blocks = parse_blocks(&lines, path_case_folding);

        let mut parse_warnings = Vec::new();
        let no_failure_blocks = failure_blocks.is_empty();
        let only_collection_failures = !no_failure_blocks
            && failure_blocks
                .iter()
                .all(|block| block.phase == Phase::Collect);
        // Quiet green output omits the session banner; standalone collection errors are still pytest-shaped.
        let missing_banner_is_expected = no_failure_blocks || only_collection_failures;
        match banner.as_ref() {
            Some(banner) if !banner.has_validated_output_family() => {
                parse_warnings.push(ParseWarning::RunnerVersionUnsupported);
            }
            None if !missing_banner_is_expected => {
                parse_warnings.push(ParseWarning::MissingSessionBanner);
            }
            Some(_) => {}
            None => {}
        }

        if has_summary_header(&lines) && summary_entries.is_empty() {
            parse_warnings.push(ParseWarning::MissingSummaryLine);
        }

        Self {
            banner,
            collected_count,
            progress_lines,
            failure_blocks,
            summary_entries,
            final_counts,
            completed_summary_count,
            has_unaccounted_run_failure,
            has_xdist_parallelism,
            parse_warnings,
        }
    }

    // EXTRACT-CANDIDATE: runner recognition gates now share a multi-signal shape with adapter-specific signatures.
    fn looks_like_pytest(&self, sidecar: &Sidecar) -> bool {
        self.banner.is_some()
            || self.collected_count.is_some()
            || !self.failure_blocks.is_empty()
            || !self.summary_entries.is_empty()
            || self.final_counts.as_ref().is_some_and(|counts| {
                // Quiet-mode summaries are still pytest-shaped when nothing
                // ran: `no tests ran in 0.12s`, `26 deselected in 0.05s`.
                // Warnings-only lines remain unrecognized. The no-tests and
                // deselected-only signals are exit-gated: a pytest usage error
                // (exit 4, e.g. `ERROR: file or directory not found: tests/x.py`)
                // also prints a `no tests ran in 0.00s` footer, and admitting it
                // would hide the raw ERROR line behind parse_status=complete with
                // failures_complete=true. no-tests-ran is trustworthy only under
                // exit 0 or 5; deselected-only only under exit 5. Admissible-green
                // stays ungated so a green summary under a nonzero exit is still
                // recognized (green_summary_nonzero_exit).
                counts.is_admissible_green()
                    || (counts.is_no_tests() && matches!(sidecar.exit_code, Some(0) | Some(5)))
                    || (counts.deselected > 0 && sidecar.exit_code == Some(5))
            })
    }
}

fn lower(
    parsed: PytestParse,
    log: &[u8],
    sidecar: &Sidecar,
    options: AnalyzeOptions,
) -> OutputDocument {
    let all_failures = reconcile_failures(&parsed, &sidecar.cwd);
    let runner_outcome = runner_outcome(&all_failures, &parsed, sidecar);
    let counts = counts(&all_failures, &parsed, runner_outcome);
    let missing_summary =
        runner_outcome == RunnerOutcome::HarnessCrashed && parsed.final_counts.is_none();
    let runner_summary_mismatch = parsed.has_unaccounted_run_failure
        || parsed.final_counts.as_ref().is_some_and(|final_counts| {
            final_counts.is_admissible_green()
                && (sidecar.exit_code != Some(0) || sidecar.signal.is_some())
        });
    let failure_list_coverage = parsed
        .final_counts
        .as_ref()
        .map_or_else(Vec::new, |counts| {
            vec![
                FailureListCoverage {
                    reported_units: counts.failed,
                    accounted_units: parsed
                        .failure_blocks
                        .iter()
                        .filter(|block| block.outcome == SummaryOutcome::Failed)
                        .count() as u64,
                },
                FailureListCoverage {
                    reported_units: counts.errors,
                    accounted_units: parsed
                        .failure_blocks
                        .iter()
                        .filter(|block| block.outcome == SummaryOutcome::Error)
                        .count() as u64,
                },
            ]
        });
    let unstable_ordering =
        parsed.has_xdist_parallelism || command_uses_xdist(&sidecar_command_tokens(sidecar));
    let mut parse_warnings = parsed.parse_warnings;
    if runner_summary_mismatch && !parse_warnings.contains(&ParseWarning::RunnerSummaryMismatch) {
        parse_warnings.push(ParseWarning::RunnerSummaryMismatch);
    }
    let parse = summary_parse_result(SummaryParseSignals {
        parse_warnings,
        contextual_degrading_warnings: vec![],
        sidecar_truncated: sidecar.truncated,
        missing_summary,
        incomplete_failure_list: missing_summary
            || runner_summary_mismatch
            || sidecar.signal.is_some(),
        failure_list_coverage,
    });
    let budgeted_failures = truncate_for_budget(all_failures, options);
    let ranking = ranking(&budgeted_failures.failures, unstable_ordering);

    build_analysis_document(AnalysisDocumentParts {
        sidecar,
        log,
        budgeted_failures,
        runner_outcome,
        early_exit: None,
        parse_status: parse.parse_status,
        parse_warnings: parse.parse_warnings,
        failures_complete: parse.failures_complete,
        ranking,
        counts,
        suggested_next_commands: Vec::<Command>::new(),
        extensions: Default::default(),
        options,
    })
}

fn reconcile_failures(parsed: &PytestParse, cwd: &str) -> Vec<Failure> {
    parsed
        .failure_blocks
        .iter()
        .map(|block| {
            let summary = parsed
                .summary_entries
                .iter()
                .find(|entry| summary_matches_block(entry, block));
            failure_from_block(block, summary, cwd)
        })
        .collect()
}

fn failure_from_block(block: &PytestBlock, summary: Option<&SummaryEntry>, cwd: &str) -> Failure {
    let runner_test_id = summary
        .map(|entry| entry.runner_test_id.clone())
        .or_else(|| recover_runner_test_id(block));
    let test_name = runner_test_id.as_deref().map(test_name_from_nodeid);
    let param_id = runner_test_id.as_deref().and_then(param_id_from_nodeid);
    let test_group_id = runner_test_id.as_deref().and_then(|runner_test_id| {
        param_id
            .as_ref()
            .map(|_| derive_test_group_id(ADAPTER_ID, runner_test_id, strip_params_pytest))
    });

    let mut failure = Failure {
        failure_id: String::new(),
        adapter_id: ADAPTER_ID.to_owned(),
        phase: block.phase,
        kind: block.kind,
        runner_package_id: None,
        runner_test_id,
        test_segments: vec![],
        test_name,
        param_id,
        test_group_id,
        primary_location: block.primary_location.clone(),
        extra_locations: vec![],
        message: block.message.clone(),
        diagnostic_code: None,
        diagnostic_severity: DiagnosticSeverity::Error,
        details: details_for(block),
        raw_log_lines: block.raw_log_lines,
        evidence: Some(Evidence {
            byte_range: block.byte_range,
            excerpts: vec![Excerpt {
                source: ExcerptSource::RawLog,
                kind: ExcerptKind::FailureBlock,
                byte_range: block.byte_range,
                text: block.excerpt_text.clone(),
                ansi_stripped: block.message_ansi_stripped,
                truncated: false,
            }],
        }),
        cascade_root: None,
        cascade_reason: None,
        dependent_failure_ids: vec![],
        retries: vec![],
        companion_diagnostics: vec![],
        extensions: Default::default(),
    };
    failure.failure_id = derive_failure_id(&failure, cwd);
    failure
}

fn details_for(block: &PytestBlock) -> Details {
    match block.kind {
        FailureKind::Assertion => Details::Assertion {
            expected: None,
            actual: None,
            diff: None::<Diff>,
        },
        FailureKind::MissingSymbol => Details::MissingSymbol {
            symbol: missing_symbol(&block.message).unwrap_or_else(|| block.message.clone()),
            import_chain: vec![],
        },
        FailureKind::TypeError => Details::TypeError {
            spans: vec![span_from_location(&block.primary_location)],
            explanation_command: None,
        },
        FailureKind::SyntaxError => Details::SyntaxError {
            spans: vec![span_from_location(&block.primary_location)],
        },
        FailureKind::Exception => Details::Exception {
            exception_type: block.exception_type.clone(),
            stack: vec![frame_from_location(&block.primary_location)],
        },
        _ => Details::Unknown {},
    }
}

fn runner_outcome(failures: &[Failure], parsed: &PytestParse, sidecar: &Sidecar) -> RunnerOutcome {
    // EXTRACT-CANDIDATE: this 7-row runner outcome decision table recurs across test-runner adapters.
    summary_runner_outcome(SummaryRunnerOutcomeSignals {
        has_pre_test_failure: failures
            .iter()
            .any(|failure| failure.phase == Phase::Collect),
        has_failures: !failures.is_empty(),
        signal_received: sidecar.signal.is_some(),
        no_tests: matches!(sidecar.exit_code, Some(0) | Some(5))
            && (parsed.collected_count == Some(0)
                || parsed
                    .final_counts
                    .as_ref()
                    .is_some_and(FinalCounts::is_no_tests)
                // Exit 5 is pytest's "no tests collected"; with a summary that
                // has no test outcomes (e.g. `26 deselected in 0.05s` from a
                // -k filter matching nothing) that verdict is trustworthy.
                || (sidecar.exit_code == Some(5)
                    && parsed
                        .final_counts
                        .as_ref()
                        .is_some_and(|counts| counts.test_outcome_total() == 0))),
        passed: sidecar.exit_code == Some(0)
            && parsed
                .final_counts
                .as_ref()
                .is_some_and(FinalCounts::is_admissible_green),
        failed: sidecar.exit_code.is_some() && parsed.final_counts.is_some(),
        summary_reports_failures: sidecar.exit_code.is_some_and(|code| code != 0)
            && parsed
                .final_counts
                .as_ref()
                .is_some_and(|counts| counts.failed + counts.errors > 0),
    })
}

fn ranking(failures: &[Failure], unstable_ordering: bool) -> Ranking {
    let strategy = if failures.is_empty() {
        RankingStrategy::None
    } else if failures
        .iter()
        .any(|failure| failure.phase == Phase::Collect)
    {
        RankingStrategy::RunnerPriority
    } else {
        RankingStrategy::FirstEmitted
    };
    Ranking {
        strategy,
        is_stable_across_runs: !unstable_ordering,
    }
}

fn counts(failures: &[Failure], parsed: &PytestParse, runner_outcome: RunnerOutcome) -> Counts {
    if failures
        .iter()
        .any(|failure| failure.phase == Phase::Collect)
    {
        return zero_test_counts();
    }

    let outcome_counts = failure_outcome_counts(failures);

    let tests_run = parsed
        .collected_count
        .or_else(|| {
            parsed
                .final_counts
                .as_ref()
                .map(|counts| counts.test_outcome_total())
        })
        .unwrap_or(outcome_counts.failed + outcome_counts.errored);
    let tests_skipped = parsed
        .final_counts
        .as_ref()
        .map_or(0, |counts| counts.skipped + counts.xfailed);
    let inferred_tests_passed =
        tests_run.saturating_sub(outcome_counts.failed + outcome_counts.errored + tests_skipped);
    let tests_passed = if runner_outcome == RunnerOutcome::HarnessCrashed {
        0
    } else {
        parsed
            .final_counts
            .as_ref()
            .map_or(inferred_tests_passed, |counts| {
                inferred_tests_passed.min(counts.passed)
            })
    };

    Counts {
        tests_run,
        tests_passed,
        tests_failed: outcome_counts.failed,
        tests_errored: outcome_counts.errored,
        tests_skipped,
        tests_not_run: None,
        files_failed: distinct_failure_files(failures),
    }
}

impl FinalCounts {
    fn test_outcome_total(&self) -> u64 {
        self.failed + self.passed + self.errors + self.skipped + self.xfailed + self.xpassed
    }

    fn is_admissible_green(&self) -> bool {
        !self.has_only_non_test_categories()
            && self.test_outcome_total() > 0
            && self.failed == 0
            && self.errors == 0
    }

    fn is_no_tests(&self) -> bool {
        self.no_tests_ran
    }

    fn has_only_non_test_categories(&self) -> bool {
        self.test_outcome_total() == 0 && self.non_test_total() > 0
    }

    fn non_test_total(&self) -> u64 {
        self.deselected + self.warnings + self.reruns
    }
}

fn summary_matches_block(entry: &SummaryEntry, block: &PytestBlock) -> bool {
    if entry.outcome != block.outcome {
        return false;
    }
    let name = test_name_from_nodeid(&entry.runner_test_id);
    name == block.title
        || (block.phase == Phase::Teardown && block.title == format!("ERROR at teardown of {name}"))
}

fn recover_runner_test_id(block: &PytestBlock) -> Option<String> {
    if block.phase != Phase::Test
        || block.outcome != SummaryOutcome::Failed
        || !is_recoverable_test_function_title(&block.title)
    {
        return None;
    }
    Some(format!("{}::{}", block.primary_location.file, block.title))
}

fn is_recoverable_test_function_title(title: &str) -> bool {
    title.starts_with("test")
        && title
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn split_lines(log: &[u8]) -> Vec<LogLine> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in log.iter().enumerate() {
        if *byte == b'\n' {
            push_line(&mut lines, log, start, index + 1);
            start = index + 1;
        }
    }
    if start < log.len() {
        push_line(&mut lines, log, start, log.len());
    }
    lines
}

fn push_line(lines: &mut Vec<LogLine>, log: &[u8], start: usize, end: usize) {
    let without_newline = if end > start && log[end - 1] == b'\n' {
        let mut trimmed = &log[start..end - 1];
        if trimmed.last() == Some(&b'\r') {
            trimmed = &trimmed[..trimmed.len() - 1];
        }
        trimmed
    } else {
        &log[start..end]
    };
    let raw = String::from_utf8_lossy(without_newline).into_owned();
    lines.push(LogLine {
        number: lines.len() as u64 + 1,
        start: start as u64,
        end: end as u64,
        stripped: strip_ansi(&raw),
        raw,
    });
}

fn parse_banner(lines: &[LogLine]) -> Option<PytestBanner> {
    lines.iter().find_map(|line| {
        banner_regex()
            .captures(&line.stripped)
            .map(|captures| PytestBanner {
                pytest_version: captures["version"].to_owned(),
            })
    })
}

fn parse_collected_count(lines: &[LogLine]) -> Option<u64> {
    lines.iter().find_map(|line| {
        collected_regex()
            .captures(&line.stripped)
            .and_then(|captures| captures["count"].parse().ok())
    })
}

fn parse_progress_lines(lines: &[LogLine]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| line.stripped.contains("[") && line.stripped.contains("%]"))
        .map(|line| line.stripped.clone())
        .collect()
}

fn parse_summary_entries(lines: &[LogLine]) -> Vec<SummaryEntry> {
    let Some(summary_start) = lines
        .iter()
        .position(|line| line.stripped.contains("short test summary info"))
    else {
        return vec![];
    };

    lines[summary_start + 1..]
        .iter()
        .take_while(|line| !is_final_counts_line(&line.stripped))
        .filter_map(|line| {
            summary_regex()
                .captures(&line.stripped)
                .map(|captures| SummaryEntry {
                    outcome: if &captures["outcome"] == "FAILED" {
                        SummaryOutcome::Failed
                    } else {
                        SummaryOutcome::Error
                    },
                    runner_test_id: captures["nodeid"].to_owned(),
                })
        })
        .collect()
}

fn parse_final_counts(lines: &[LogLine]) -> Option<FinalCounts> {
    lines
        .iter()
        .rev()
        .find(|line| is_final_counts_line(&line.stripped))
        .map(|line| {
            if line.stripped.contains("no tests ran") {
                return FinalCounts {
                    no_tests_ran: true,
                    ..Default::default()
                };
            }
            let mut counts = FinalCounts::default();
            for captures in count_part_regex().captures_iter(&line.stripped) {
                let count = captures["count"].parse::<u64>().unwrap_or(0);
                match &captures["name"] {
                    "failed" => counts.failed = count,
                    "passed" => counts.passed = count,
                    "error" | "errors" => counts.errors = count,
                    "skipped" => counts.skipped = count,
                    "deselected" => counts.deselected = count,
                    "xfailed" => counts.xfailed = count,
                    "xpassed" => counts.xpassed = count,
                    "warning" | "warnings" => counts.warnings = count,
                    "rerun" | "reruns" => counts.reruns = count,
                    _ => {}
                }
            }
            counts
        })
}

fn parse_blocks(lines: &[LogLine], path_case_folding: PathCaseFolding) -> Vec<PytestBlock> {
    if let Some(collection) = parse_collection_error(lines, path_case_folding) {
        return vec![collection];
    }

    let mut blocks = Vec::new();
    let mut index = first_failure_section_line(lines);
    while let Some(start) = next_block_start(lines, index) {
        let end = next_block_end(lines, start + 1);
        if let Some(block) = parse_block(&lines[start..end], path_case_folding) {
            blocks.push(block);
        }
        index = end;
    }
    blocks
}

fn parse_collection_error(
    lines: &[LogLine],
    path_case_folding: PathCaseFolding,
) -> Option<PytestBlock> {
    if lines.first().is_none_or(|line| {
        !line
            .stripped
            .starts_with("ImportError while loading conftest")
    }) {
        return None;
    }
    let message_line = lines
        .iter()
        .find(|line| line.stripped.trim_start().starts_with("E "))?;
    let message = strip_error_prefix(&message_line.stripped)?;
    let location_line = lines
        .iter()
        .find_map(|line| file_line_regex().captures(&line.stripped))?;
    let first = lines.first()?;
    let last = lines.last()?;
    Some(PytestBlock {
        title: "collection error".to_owned(),
        outcome: SummaryOutcome::Error,
        phase: Phase::Collect,
        kind: kind_for_message(&message),
        primary_location: Location {
            file: location_line["file"].to_owned(),
            line: location_line["line"].parse().ok(),
            column: None,
            end_line: None,
            end_column: None,
            role: LocationRole::ImportSite,
            in_user_code: is_user_code_path(
                &location_line["file"],
                UserCodeEcosystem::Python,
                path_case_folding,
            ),
        },
        exception_type: exception_type(&message),
        message,
        raw_log_lines: [first.number, last.number],
        byte_range: ByteRange {
            start: first.start,
            end: last.end,
        },
        excerpt_text: excerpt_text(lines),
        message_ansi_stripped: false,
    })
}

fn parse_block(block_lines: &[LogLine], path_case_folding: PathCaseFolding) -> Option<PytestBlock> {
    let header = block_lines.first()?;
    let title = header_title(&header.stripped)?;
    let (outcome, phase) = if title.starts_with("ERROR at teardown of ") {
        (SummaryOutcome::Error, Phase::Teardown)
    } else if title.starts_with("ERROR ") {
        (SummaryOutcome::Error, Phase::Setup)
    } else {
        (SummaryOutcome::Failed, Phase::Test)
    };

    let message_line = block_lines
        .iter()
        .find(|line| canonical_message_candidate(&line.stripped))
        // Exception types that don't end in Error/Exception (e.g. a custom
        // `GateRefused`) never produce a canonical candidate; fall back to the
        // first `E `-prefixed line rather than dropping the whole block.
        .or_else(|| {
            block_lines
                .iter()
                .find(|line| strip_error_prefix(&line.stripped).is_some())
        });
    let message_line = message_line?;
    let stripped_message = strip_error_prefix(&message_line.stripped)?;
    let raw_message =
        strip_error_prefix(&message_line.raw).unwrap_or_else(|| stripped_message.clone());
    let message_ansi_stripped = raw_message != stripped_message;
    let location = block_lines
        .iter()
        .rev()
        .find_map(|line| failure_location(&line.stripped, phase, path_case_folding))?;
    let first = block_lines.first()?;
    let last = block_lines.last()?;

    Some(PytestBlock {
        title,
        outcome,
        phase,
        kind: kind_for_message(&stripped_message),
        primary_location: location,
        exception_type: exception_type(&stripped_message),
        message: stripped_message,
        raw_log_lines: [first.number, last.number],
        byte_range: ByteRange {
            start: first.start,
            end: last.end,
        },
        excerpt_text: excerpt_text(block_lines),
        message_ansi_stripped,
    })
}

fn first_failure_section_line(lines: &[LogLine]) -> usize {
    lines
        .iter()
        .position(|line| line.stripped.contains(" FAILURES ") || line.stripped.contains(" ERRORS "))
        .map_or(0, |index| index + 1)
}

fn next_block_start(lines: &[LogLine], from: usize) -> Option<usize> {
    lines[from..]
        .iter()
        .position(|line| header_title(&line.stripped).is_some())
        .map(|offset| from + offset)
}

fn next_block_end(lines: &[LogLine], from: usize) -> usize {
    for (index, line) in lines.iter().enumerate().skip(from) {
        let line = &line.stripped;
        if line.contains("short test summary info") || header_title(line).is_some() {
            return index;
        }
    }
    lines.len()
}

fn failure_location(
    line: &str,
    phase: Phase,
    path_case_folding: PathCaseFolding,
) -> Option<Location> {
    let captures = location_regex().captures(line)?;
    Some(Location {
        file: captures["file"].to_owned(),
        line: captures["line"].parse().ok(),
        column: None,
        end_line: None,
        end_column: None,
        role: if phase == Phase::Collect {
            LocationRole::ImportSite
        } else {
            LocationRole::Primary
        },
        in_user_code: is_user_code_path(
            &captures["file"],
            UserCodeEcosystem::Python,
            path_case_folding,
        ),
    })
}

fn header_title(line: &str) -> Option<String> {
    if !line.starts_with('_') || !line.ends_with('_') {
        return None;
    }
    let title = line.trim_matches('_').trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_owned())
    }
}

fn canonical_message_candidate(line: &str) -> bool {
    let Some(message) = strip_error_prefix(line) else {
        return false;
    };
    exception_type(&message).is_some()
}

fn strip_error_prefix(line: &str) -> Option<String> {
    error_prefix_regex()
        .captures(line)
        .map(|captures| captures["message"].to_owned())
}

// EXTRACT-CANDIDATE: phase/failure-kind dispatch has the same adapter-local policy shape across test runners.
// Pytest splits phase derivation (from summary-row header at lines 733-737) from kind derivation here;
// vitest/jest combine both into a single classify(rest, message). The future shared trait can either
// expose a unified classify or keep the split.
fn kind_for_message(message: &str) -> FailureKind {
    match exception_type(message).as_deref() {
        Some("AssertionError") => FailureKind::Assertion,
        Some("ModuleNotFoundError") | Some("ImportError") => FailureKind::MissingSymbol,
        Some("TypeError") => FailureKind::TypeError,
        Some("SyntaxError") => FailureKind::SyntaxError,
        Some(_) => FailureKind::Exception,
        None => FailureKind::Unknown,
    }
}

fn exception_type(message: &str) -> Option<String> {
    exception_regex()
        .captures(message)
        .map(|captures| captures["name"].to_owned())
}

fn missing_symbol(message: &str) -> Option<String> {
    missing_symbol_regex()
        .captures(message)
        .map(|captures| captures["symbol"].to_owned())
}

fn test_name_from_nodeid(nodeid: &str) -> String {
    nodeid.rsplit("::").next().unwrap_or(nodeid).to_owned()
}

fn param_id_from_nodeid(nodeid: &str) -> Option<String> {
    let test_name = test_name_from_nodeid(nodeid);
    let start = test_name.find('[')?;
    let end = test_name.rfind(']')?;
    (end > start).then(|| test_name[start + 1..end].to_owned())
}

fn span_from_location(location: &Location) -> crate::schema::Span {
    crate::schema::Span {
        file: location.file.clone(),
        line: location.line,
        column: location.column,
        end_line: location.end_line,
        end_column: location.end_column,
        role: location.role,
        note: None,
    }
}

fn frame_from_location(location: &Location) -> Frame {
    Frame {
        file: location.file.clone(),
        line: location.line,
        function: None,
        in_user_code: location.in_user_code,
    }
}

fn excerpt_text(lines: &[LogLine]) -> String {
    let mut text = lines
        .iter()
        .map(|line| line.raw.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    text
}

fn has_summary_header(lines: &[LogLine]) -> bool {
    lines
        .iter()
        .any(|line| line.stripped.contains("short test summary info"))
}

fn completed_summary_count(lines: &[LogLine]) -> u64 {
    let mut inside_captured_output = false;
    let mut count = 0;

    for line in lines {
        if is_captured_output_header(&line.stripped) {
            inside_captured_output = true;
            continue;
        }
        if inside_captured_output
            && (line.stripped.contains("short test summary info")
                || line.stripped.contains(" test session starts ")
                || header_title(&line.stripped).is_some())
        {
            inside_captured_output = false;
        }
        if !inside_captured_output && is_final_counts_line(&line.stripped) {
            count += 1;
        }
    }

    count
}

fn is_captured_output_header(line: &str) -> bool {
    line.starts_with('-')
        && line.ends_with('-')
        && ["Captured stdout", "Captured stderr", "Captured log"]
            .iter()
            .any(|marker| line.contains(marker))
}

fn is_final_counts_line(line: &str) -> bool {
    if line.starts_with('=') && line.contains("no tests ran") && line.contains(" in ") {
        return true;
    }
    // Quiet mode drops the `=` decoration: `no tests ran in 0.12s`.
    if line.trim().starts_with("no tests ran in ") {
        return true;
    }
    if line.starts_with('=') && line.contains(" in ") && count_part_regex().is_match(line) {
        return true;
    }
    quiet_final_counts_regex().is_match(line)
}

fn strip_ansi(text: &str) -> String {
    ansi_regex().replace_all(text, "").into_owned()
}

// EXTRACT-CANDIDATE: OnceLock<Regex> accessor wrappers are duplicated across adapters.
fn banner_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"pytest-(?<version>\d+\.\d+\.\d+)").unwrap())
}

fn collected_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^collected (?<count>\d+) items?$").unwrap())
}

fn summary_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"^(?<outcome>FAILED|ERROR) (?<nodeid>\S+)(?: - (?<message>.*))?$").unwrap()
    })
}

fn count_part_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?<count>\d+) (?<name>failed|passed|errors?|skipped|deselected|xfailed|xpassed|warnings?|reruns?)",
        )
        .unwrap()
    })
}

fn quiet_final_counts_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        // Runs of >=60s append a wall-clock suffix: `... in 130.49s (0:02:10)`.
        // The suffix is str(datetime.timedelta), which past 24h renders
        // `(1 day, 1:01:01)` / `(2 days, 0:00:01)`.
        Regex::new(
            r"^\s*\d+ (?:failed|passed|errors?|skipped|deselected|xfailed|xpassed|warnings?|reruns?)(?:, \d+ (?:failed|passed|errors?|skipped|deselected|xfailed|xpassed|warnings?|reruns?))* in [\d.]+(?:s| seconds?)(?: \((?:\d+ days?, )?\d+:\d{2}:\d{2}\))?\s*$",
        )
        .unwrap()
    })
}

fn location_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"^(?<file>[^:\s][^:]*\.py):(?<line>\d+): (?<tail>.+)$").unwrap()
    })
}

fn file_line_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^(?<file>[^:\s][^:]*\.py):(?<line>\d+):").unwrap())
}

fn error_prefix_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^\s*E\s+(?<message>.+)$").unwrap())
}

fn exception_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"^(?<name>[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*(?:Error|Exception))\b",
        )
        .unwrap()
    })
}

fn missing_symbol_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"No module named '(?<symbol>[^']+)'").unwrap())
}

fn ansi_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]").unwrap()
    })
}

#[cfg(test)]
mod tests {
    use crate::schema::{RetryOutcome, final_outcome};

    use super::*;

    const PARAMETERIZED: &[u8] =
        include_bytes!("../../tests/fixtures/pytest/parameterized/run/log");
    const TEARDOWN: &[u8] =
        include_bytes!("../../tests/fixtures/pytest/teardown_exception/run/log");
    const COLLECTION: &[u8] =
        include_bytes!("../../tests/fixtures/pytest/collection_error/run/log");

    #[test]
    fn parses_sections_and_summary_entries() {
        let parsed = PytestParse::from_log(PARAMETERIZED);
        assert_eq!(
            parsed.banner,
            Some(PytestBanner {
                pytest_version: "8.4.2".to_owned()
            })
        );
        assert_eq!(parsed.collected_count, Some(17));
        assert_eq!(parsed.progress_lines.len(), 1);
        assert_eq!(parsed.failure_blocks.len(), 3);
        assert_eq!(parsed.summary_entries.len(), 3);
        assert_eq!(parsed.final_counts.unwrap().failed, 3);
    }

    #[test]
    fn block_parser_extracts_canonical_assertion_not_diff_line() {
        let parsed = PytestParse::from_log(PARAMETERIZED);
        assert_eq!(
            parsed.failure_blocks[0].message,
            "AssertionError: expected '' to equal None"
        );
        assert_eq!(
            parsed.failure_blocks[1].message,
            "AssertionError: expected 'foo\\n' to equal 'foo'"
        );
        assert_eq!(parsed.failure_blocks[0].primary_location.line, Some(41));
    }

    #[test]
    fn reconciles_teardown_error_to_final_errored_outcome() {
        let parsed = PytestParse::from_log(TEARDOWN);
        let failure = failure_from_block(
            &parsed.failure_blocks[0],
            parsed.summary_entries.first(),
            "/Users/dev/failtrace/tests/fixtures/pytest/teardown_exception/source",
        );
        assert_eq!(failure.phase, Phase::Teardown);
        assert_eq!(
            failure.runner_test_id.as_deref(),
            Some("tests/test_db.py::test_query")
        );
        assert!(failure.retries.is_empty());
        assert_eq!(final_outcome(&failure), RetryOutcome::Errored);
    }

    #[test]
    fn collection_errors_use_fallback_without_summary_entry() {
        let parsed = PytestParse::from_log(COLLECTION);
        assert_eq!(parsed.failure_blocks.len(), 1);
        assert_eq!(parsed.summary_entries.len(), 0);
        assert_eq!(parsed.failure_blocks[0].phase, Phase::Collect);
        assert_eq!(parsed.failure_blocks[0].kind, FailureKind::MissingSymbol);
    }

    #[test]
    fn ansi_stripping_preserves_canonical_message_equivalence() {
        let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2, pluggy-1.6.0\ncollected 1 item\n=================================== FAILURES ===================================\n________________________________ test_color __________________________________\n>       assert False\nE       \x1b[31mAssertionError: expected 2 to equal 3\x1b[0m\ntests/test_color.py:5: AssertionError\n=========================== short test summary info ============================\nFAILED tests/test_color.py::test_color - AssertionError: expected 2 to equal 3\n============================== 1 failed in 0.01s ===============================\n";
        let parsed = PytestParse::from_log(log);
        assert_eq!(
            parsed.failure_blocks[0].message,
            "AssertionError: expected 2 to equal 3"
        );
        assert!(parsed.failure_blocks[0].message_ansi_stripped);
    }

    #[test]
    fn green_run_verbose_parses_counts_with_no_failures() {
        let log = b"============================= test session starts ==============================\nplatform darwin -- Python 3.11.15, pytest-8.4.2, pluggy-1.6.0\ncollected 2 items\n\ntest_basic.py ..                                                         [100%]\n\n============================== 2 passed in 0.00s ===============================\n";
        let parsed = PytestParse::from_log(log);
        assert!(parsed.looks_like_pytest(&quiet_sidecar(Some(0))));
        assert!(parsed.failure_blocks.is_empty());
        assert!(parsed.summary_entries.is_empty());
        assert_eq!(parsed.collected_count, Some(2));
        let final_counts = parsed.final_counts.expect("final counts parsed");
        assert_eq!(final_counts.passed, 2);
        assert_eq!(final_counts.failed, 0);
        assert!(parsed.parse_warnings.is_empty());
    }

    #[test]
    fn green_run_quiet_parses_counts_without_banner() {
        let log = b"..                                                                       [100%]\n2 passed in 0.00s\n";
        let parsed = PytestParse::from_log(log);
        assert!(parsed.looks_like_pytest(&quiet_sidecar(Some(0))));
        assert!(parsed.failure_blocks.is_empty());
        assert!(parsed.summary_entries.is_empty());
        assert_eq!(parsed.collected_count, None);
        let final_counts = parsed.final_counts.expect("final counts parsed");
        assert_eq!(final_counts.passed, 2);
        assert_eq!(final_counts.failed, 0);
        assert!(
            parsed.parse_warnings.is_empty(),
            "expected no warnings on quiet green, got {:?}",
            parsed.parse_warnings
        );
    }

    #[test]
    fn empty_log_remains_unrecognized() {
        let parsed = PytestParse::from_log(b"");
        assert!(!parsed.looks_like_pytest(&quiet_sidecar(Some(0))));
    }

    #[test]
    fn pytest_version_policy_accepts_only_validated_majors() {
        for version in ["8.4.2", "8.0.0", "9.0.0", "9.0.3"] {
            assert!(
                (PytestBanner {
                    pytest_version: version.to_owned()
                })
                .has_validated_output_family(),
                "{version} should be supported"
            );
        }

        for version in ["7.4.4", "10.0.0", "91.0.0", "9plus"] {
            assert!(
                !(PytestBanner {
                    pytest_version: version.to_owned()
                })
                .has_validated_output_family(),
                "{version} should be unsupported"
            );
        }
    }

    #[test]
    fn xdist_worker_configuration_marks_ranking_unstable() {
        for argv in [
            vec!["pytest", "-n2"],
            vec!["pytest", "-n", "auto"],
            vec!["python", "-m", "pytest", "--numprocesses=2"],
        ] {
            let mut sidecar = quiet_sidecar(Some(1));
            sidecar.argv = argv.into_iter().map(str::to_owned).collect();
            let document = analyze_log(PARAMETERIZED, &sidecar, AnalyzeOptions::default())
                .expect("xdist command should remain analyzable");
            assert!(!document.ranking.is_stable_across_runs);
        }

        let mut sidecar = quiet_sidecar(Some(1));
        sidecar.argv = vec!["pytest".to_owned(), "-n0".to_owned()];
        let document = analyze_log(PARAMETERIZED, &sidecar, AnalyzeOptions::default())
            .expect("disabled xdist command should remain analyzable");
        assert!(document.ranking.is_stable_across_runs);
    }

    #[test]
    fn xdist_log_markers_mark_ranking_unstable_when_argv_comes_from_config() {
        let mut log =
            b"created: 2/2 workers\n2 workers [17 items]\nscheduling tests via LoadScheduling\n"
                .to_vec();
        log.extend_from_slice(PARAMETERIZED);
        let document = analyze_log(&log, &quiet_sidecar(Some(1)), AnalyzeOptions::default())
            .expect("xdist output should remain analyzable");
        assert!(!document.ranking.is_stable_across_runs);
    }

    #[test]
    fn pytest_9_banner_does_not_emit_version_warning() {
        let log = b"platform darwin -- Python 3.11.15, pytest-9.0.3, pluggy-1.6.0\n=================================== FAILURES ===================================\n________________________________ test_x __________________________________\nE       AssertionError: nope\ntests/test_x.py:1: AssertionError\n=========================== short test summary info ============================\nFAILED tests/test_x.py::test_x - AssertionError: nope\n============================== 1 failed in 0.01s ===============================\n";
        let parsed = PytestParse::from_log(log);
        assert!(parsed.parse_warnings.is_empty());
    }

    #[test]
    fn qualified_exception_names_are_canonical_messages() {
        for (message, expected) in [
            (
                "sqlite3.IntegrityError: unique constraint failed",
                Some("sqlite3.IntegrityError"),
            ),
            (
                "urllib3.exceptions.HTTPError: connection reset",
                Some("urllib3.exceptions.HTTPError"),
            ),
            ("MyError: failed", Some("MyError")),
            ("MyException: failed", Some("MyException")),
            ("NotAnErrorWord in the middle", None),
        ] {
            assert_eq!(exception_type(message).as_deref(), expected, "{message}");
        }
    }

    #[test]
    fn unsupported_pytest_majors_emit_version_warning() {
        for version in ["7.0.0", "10.0.0"] {
            let log = format!(
                "platform darwin -- Python 3.11.15, pytest-{version}, pluggy-1.6.0\n=================================== FAILURES ===================================\n________________________________ test_x __________________________________\nE       AssertionError: nope\ntests/test_x.py:1: AssertionError\n=========================== short test summary info ============================\nFAILED tests/test_x.py::test_x - AssertionError: nope\n============================== 1 failed in 0.01s ===============================\n"
            );
            let parsed = PytestParse::from_log(log.as_bytes());
            assert_eq!(
                parsed.parse_warnings,
                vec![ParseWarning::RunnerVersionUnsupported],
                "{version}"
            );
        }
    }

    #[test]
    fn missing_banner_with_failure_blocks_emits_missing_banner_warning() {
        let missing_banner = b"=================================== FAILURES ===================================\n________________________________ test_x __________________________________\nE       AssertionError: nope\ntests/test_x.py:1: AssertionError\n=========================== short test summary info ============================\nFAILED tests/test_x.py::test_x - AssertionError: nope\n============================== 1 failed in 0.01s ===============================\n";
        let parsed = PytestParse::from_log(missing_banner);
        assert_eq!(
            parsed.parse_warnings,
            vec![ParseWarning::MissingSessionBanner]
        );
    }

    // Runs of >=60s append a wall-clock suffix (`... in 67.65s (0:01:07)`).
    // The quiet final-counts regex must accept it; otherwise green -q runs
    // downgrade to unparsed_output and failing ones are misclassified as
    // harness_crashed.
    #[test]
    fn quiet_final_counts_accept_wall_clock_suffix() {
        let log = b"..                                                                       [100%]\n29 failed, 1768 passed, 21 skipped, 5 warnings in 67.65s (0:01:07)\n";
        let parsed = PytestParse::from_log(log);
        let final_counts = parsed.final_counts.expect("final counts parsed");
        assert_eq!(final_counts.failed, 29);
        assert_eq!(final_counts.passed, 1768);
        assert_eq!(final_counts.skipped, 21);

        // >=24h runs render the suffix via str(timedelta): `(1 day, 1:01:01)`.
        let log = b"2 passed in 90061.00s (1 day, 1:01:01)\n";
        let parsed = PytestParse::from_log(log);
        assert_eq!(parsed.final_counts.expect("day-form parsed").passed, 2);
    }

    #[test]
    fn quiet_no_tests_ran_line_is_recognized() {
        let log = b"no tests ran in 0.12s\n";
        let parsed = PytestParse::from_log(log);
        let final_counts = parsed.final_counts.expect("final counts parsed");
        assert!(final_counts.is_no_tests());
    }

    // Exception types that don't end in Error/Exception (e.g. a custom
    // `GateRefused`) must still yield a canonical message candidate;
    // otherwise the whole FAILURES block is dropped and the run lowers to
    // harness_crashed with `failures: []`.
    #[test]
    fn non_error_suffixed_exception_block_falls_back_to_first_e_line() {
        let log = b"F                                                                        [100%]\n=================================== FAILURES ===================================\n____ test_prepare_runs_suite_without_candidate_override_and_smoke_read_only ____\n\n>           raise GateRefused(f\"HTTP smoke failed\")\nE           src.storage.corpus_write_gate.GateRefused: HTTP smoke failed with exit code 1\n\nsrc/storage/corpus_write_gate.py:435: GateRefused\n=========================== short test summary info ============================\nFAILED tests/test_corpus_write_gate.py::test_prepare_runs_suite_without_candidate_override_and_smoke_read_only\n1 failed in 0.08s\n";
        let parsed = PytestParse::from_log(log);
        assert_eq!(parsed.failure_blocks.len(), 1);
        assert_eq!(
            parsed.failure_blocks[0].message,
            "src.storage.corpus_write_gate.GateRefused: HTTP smoke failed with exit code 1"
        );
        assert_eq!(parsed.failure_blocks[0].kind, FailureKind::Unknown);
        assert_eq!(
            parsed.failure_blocks[0].primary_location.file,
            "src/storage/corpus_write_gate.py"
        );

        let sidecar = quiet_sidecar(Some(1));
        let document = analyze_log(log, &sidecar, AnalyzeOptions::default())
            .expect("recognized pytest output");
        assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
        assert_eq!(document.failures.len(), 1);
        // Non-assertion kinds map to errored under final_outcome semantics.
        assert_eq!(document.counts.tests_errored, 1);
    }

    // When the summary line reports failures but no failure block could be parsed at all (here: no file:line in the
    // block), the outcome must still be Failed — with the incomplete failure
    // list observable — not harness_crashed.
    #[test]
    fn summary_failures_without_parsed_blocks_lower_to_failed_with_mismatch_warning() {
        let log = b"F                                                                        [100%]\n=================================== FAILURES ===================================\n________________________________ test_x __________________________________\nE       SomethingBroke: nope\n=========================== short test summary info ============================\nFAILED tests/test_x.py::test_x\n1 failed in 0.08s\n";
        let parsed = PytestParse::from_log(log);
        assert!(parsed.failure_blocks.is_empty());

        let sidecar = quiet_sidecar(Some(1));
        let document = analyze_log(log, &sidecar, AnalyzeOptions::default())
            .expect("recognized pytest output");
        assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
        assert!(document.failures.is_empty());
        assert!(!document.failures_complete);
        assert!(
            document
                .parse_warnings
                .contains(&ParseWarning::RunnerSummaryMismatch),
            "summary/evidence disagreement must be observable: got {:?}",
            document.parse_warnings
        );
        assert_eq!(document.parse_status, crate::schema::ParseStatus::Partial);
    }

    // `pytest -k nomatch -q` exits 5 with a
    // deselected-only summary; that verdict is NoTests, not harness_crashed.
    #[test]
    fn exit_five_with_deselected_only_summary_is_no_tests() {
        let log = b"26 deselected in 0.05s\n";
        let sidecar = quiet_sidecar(Some(5));
        let document = analyze_log(log, &sidecar, AnalyzeOptions::default())
            .expect("recognized pytest output");
        assert_eq!(document.runner_outcome, RunnerOutcome::NoTests);
    }

    // A pytest usage error exits 4 and prints a `no tests ran`
    // footer with no other pytest structure. Recognition must stay exit-gated so
    // the raw ERROR line is not hidden behind parse_status=complete /
    // failures_complete=true; the run stays unparsed_output.
    #[test]
    fn exit_four_usage_error_with_no_tests_footer_stays_unrecognized() {
        let log = b"ERROR: file or directory not found: tests/test_x.py\n\nno tests ran in 0.00s\n";
        let sidecar = quiet_sidecar(Some(4));
        let result = analyze_log(log, &sidecar, AnalyzeOptions::default());
        assert!(
            matches!(result, Err(AdapterError::UnrecognizedFormat { .. })),
            "exit-4 usage error must stay unrecognized: got {result:?}"
        );
    }

    fn quiet_sidecar(exit_code: Option<i32>) -> Sidecar {
        use crate::{
            schema::{Adapter, AdapterKind, SelectionReason},
            storage::{PathCaseFolding, StreamMerge},
        };

        Sidecar {
            meta_schema_version: "1.2".to_owned(),
            schema_version: "1.6".to_owned(),
            failtrace_version: "0.1.0".to_owned(),
            capture_build: None,
            adapters: vec![Adapter {
                id: ADAPTER_ID.to_owned(),
                version: "pytest@0.1.0".to_owned(),
                kind: AdapterKind::Runner,
                selected: true,
                selection_reason: SelectionReason::ArgvNameMatch,
            }],
            command: "python -m pytest -q".to_owned(),
            cwd: "/tmp/project".to_owned(),
            started_at: "2026-07-16T00:00:00Z".to_owned(),
            duration_ms: 1,
            exit_code,
            signal: None,
            early_exit: None,
            truncated: false,
            truncation_reason: None,
            path_case_folding: PathCaseFolding::CaseSensitive,
            stream_merge: StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
            adapter_candidates: vec![],
            argv: vec!["python".to_owned(), "-m".to_owned(), "pytest".to_owned()],
        }
    }
}
