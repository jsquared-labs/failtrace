use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use regex::Regex;

use crate::{
    adapters::{
        AdapterError, AdapterImpl, AnalyzeOptions,
        shared::{
            AnalysisDocumentParts, FailureListCoverage, SummaryParseSignals,
            SummaryRunnerOutcomeSignals, UserCodeEcosystem, build_analysis_document,
            distinct_failure_files, failure_outcome_counts, is_user_code_path,
            sidecar_boolean_flag_enabled, sidecar_selected_adapter, summary_parse_result,
            summary_runner_outcome, truncate_for_budget, validated_major_version, zero_test_counts,
        },
    },
    schema::{
        ByteRange, Command, CommandIntent, Counts, Details, DiagnosticSeverity, Diff, Evidence,
        Excerpt, ExcerptKind, ExcerptSource, Failure, FailureKind, Frame, Location, LocationRole,
        OutputDocument, ParseWarning, Phase, Ranking, RankingStrategy, Resolver, RunnerOutcome,
        derive_failure_id, validate,
    },
    storage::{PathCaseFolding, Sidecar},
};

const ADAPTER_ID: &str = "jest";
const VALIDATED_JEST_MAJOR_VERSIONS: &[u64] = &[30];

pub fn analyze_log(
    log: &[u8],
    sidecar: &Sidecar,
    options: AnalyzeOptions,
) -> Result<OutputDocument, AdapterError> {
    JestAdapter::analyze_log(log, sidecar, options)
}

struct JestAdapter;

impl AdapterImpl for JestAdapter {
    fn analyze_log(
        log: &[u8],
        sidecar: &Sidecar,
        options: AnalyzeOptions,
    ) -> Result<OutputDocument, AdapterError> {
        if !sidecar_selected_adapter(sidecar, ADAPTER_ID) {
            return Err(AdapterError::InvalidSidecar {
                reason: "sidecar does not select jest adapter".to_owned(),
            });
        }
        if sidecar_boolean_flag_enabled(sidecar, "--watch", None)
            || sidecar_boolean_flag_enabled(sidecar, "--watchAll", None)
            || sidecar_boolean_flag_enabled(sidecar, "--watch-all", None)
        {
            return Err(AdapterError::UnrecognizedFormat {
                reason: "jest watch output can contain multiple run cycles and is unsupported"
                    .to_owned(),
            });
        }

        let parsed = JestParse::from_log_with_path_case_folding(log, sidecar.path_case_folding);
        if parsed.completed_summary_count > 1 {
            return Err(AdapterError::UnrecognizedFormat {
                reason: "multiple completed jest run summaries are unsupported".to_owned(),
            });
        }
        if !parsed.looks_like_jest(sidecar) {
            return Err(AdapterError::UnrecognizedFormat {
                reason: "no jest PASS/FAIL recap, summary block, or failure block found".to_owned(),
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
struct JestParse {
    version: Option<JestVersion>,
    file_recaps: Vec<FileRecap>,
    failure_blocks: Vec<JestBlock>,
    final_counts: Option<FinalCounts>,
    completed_summary_count: u64,
    has_summary_block: bool,
    has_failure_bullet: bool,
    has_unaccounted_run_failure: bool,
    parse_warnings: Vec<ParseWarning>,
    partial_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JestVersion {
    version: String,
}

impl JestVersion {
    // EXTRACT-CANDIDATE: adapter version family validation is shared policy with runner-specific validated majors.
    // Jest default reporter does not print a version banner, so parse_version returns None
    // for ordinary runs. Until the sidecar carries a structured runner_version (planned for
    // the v1.5 schema patch alongside cargo test / tsc / go test adapters), this gate is
    // dormant in practice — it only fires when a version line happens to appear in the log.
    fn has_validated_output_family(&self) -> bool {
        validated_major_version(&self.version, VALIDATED_JEST_MAJOR_VERSIONS)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileRecap {
    status: FileStatus,
    path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileStatus {
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JestBlock {
    path: String,
    rest: String,
    runner_test_id: Option<String>,
    phase: Phase,
    kind: FailureKind,
    primary_location: Location,
    message: String,
    exception_type: Option<String>,
    raw_log_lines: [u64; 2],
    byte_range: ByteRange,
    excerpt_text: String,
    ansi_stripped: bool,
    partial_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct FinalCounts {
    failed: u64,
    passed: u64,
    skipped: u64,
    todo: u64,
    errors: u64,
    total: u64,
}

impl FinalCounts {
    fn is_admissible_green(&self) -> bool {
        self.total > 0 && self.failed == 0 && self.errors == 0
    }

    fn is_no_tests(&self) -> bool {
        self.total == 0 && self.failed == 0 && self.errors == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogLine {
    number: u64,
    start: u64,
    end: u64,
    raw: String,
    stripped: String,
}

impl JestParse {
    #[cfg(test)]
    fn from_log(log: &[u8]) -> Self {
        Self::from_log_with_path_case_folding(log, PathCaseFolding::CaseSensitive)
    }

    fn from_log_with_path_case_folding(log: &[u8], path_case_folding: PathCaseFolding) -> Self {
        let lines = split_lines(log);
        let version = parse_version(&lines);
        let file_recaps = parse_file_recaps(&lines);
        let final_counts = parse_final_counts(&lines);
        let completed_summary_count = completed_summary_count(&lines);
        let has_summary_block = has_summary_block(&lines);
        let has_failure_bullet = lines
            .iter()
            .any(|line| bullet_regex().is_match(&line.stripped));
        let has_unaccounted_run_failure = has_unaccounted_run_failure(&lines);
        let failure_blocks = parse_blocks(&lines, &file_recaps, path_case_folding);

        let mut parse_warnings = Vec::new();
        match version.as_ref() {
            Some(version) if !version.has_validated_output_family() => {
                parse_warnings.push(ParseWarning::RunnerVersionUnsupported);
            }
            Some(_) => {}
            None => {}
        }

        if !failure_blocks.is_empty() && final_counts.is_none() {
            parse_warnings.push(ParseWarning::MissingSummaryLine);
        }

        let partial_reason = failure_blocks
            .iter()
            .find_map(|block| block.partial_reason.clone());
        if partial_reason.is_some()
            && !parse_warnings.contains(&ParseWarning::UnstructuredDiagnostic)
        {
            parse_warnings.push(ParseWarning::UnstructuredDiagnostic);
        }

        Self {
            version,
            file_recaps,
            failure_blocks,
            final_counts,
            completed_summary_count,
            has_summary_block,
            has_failure_bullet,
            has_unaccounted_run_failure,
            parse_warnings,
            partial_reason,
        }
    }

    fn looks_like_jest(&self, _sidecar: &Sidecar) -> bool {
        // EXTRACT-CANDIDATE: runner recognition gates now share a multi-signal shape with adapter-specific signatures.
        !self.file_recaps.is_empty() || self.has_summary_block || self.has_failure_bullet
    }
}

fn lower(
    parsed: JestParse,
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
    let accounted_test_failures = parsed
        .failure_blocks
        .iter()
        .filter(|block| block.rest != "Test suite failed to run")
        .map(|block| (block.path.as_str(), block.rest.as_str()))
        .collect::<BTreeSet<_>>()
        .len() as u64;
    let accounted_failed_suites = parsed
        .failure_blocks
        .iter()
        .map(|block| block.path.as_str())
        .collect::<BTreeSet<_>>()
        .len() as u64;
    let failure_list_coverage = parsed
        .final_counts
        .as_ref()
        .map_or_else(Vec::new, |counts| {
            vec![
                FailureListCoverage {
                    reported_units: counts.failed,
                    accounted_units: accounted_test_failures,
                },
                FailureListCoverage {
                    reported_units: counts.errors,
                    accounted_units: accounted_failed_suites,
                },
            ]
        });
    let mut parse_warnings = parsed.parse_warnings;
    if runner_summary_mismatch && !parse_warnings.contains(&ParseWarning::RunnerSummaryMismatch) {
        parse_warnings.push(ParseWarning::RunnerSummaryMismatch);
    }
    let parse = summary_parse_result(SummaryParseSignals {
        parse_warnings,
        contextual_degrading_warnings: vec![ParseWarning::UnstructuredDiagnostic],
        sidecar_truncated: sidecar.truncated,
        missing_summary,
        incomplete_failure_list: missing_summary
            || runner_summary_mismatch
            || sidecar.signal.is_some(),
        failure_list_coverage,
    });
    let budgeted_failures = truncate_for_budget(all_failures, options);
    let suggested_next_commands =
        suggested_next_commands(&budgeted_failures.failures, &sidecar.cwd);
    let ranking = ranking(&budgeted_failures.failures);
    let mut extensions = BTreeMap::new();
    if let Some(partial_reason) = parsed.partial_reason {
        extensions.insert(
            "jest".to_owned(),
            serde_json::json!({ "partial_reason": partial_reason }),
        );
    }

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
        suggested_next_commands,
        extensions,
        options,
    })
}

fn reconcile_failures(parsed: &JestParse, cwd: &str) -> Vec<Failure> {
    parsed
        .failure_blocks
        .iter()
        .map(|block| failure_from_block(block, cwd))
        .collect()
}

fn failure_from_block(block: &JestBlock, cwd: &str) -> Failure {
    let runner_test_id = block.runner_test_id.clone();
    let test_name = runner_test_id
        .as_ref()
        .map(|_| test_name_from_fail_rest(&block.rest));

    let mut failure = Failure {
        failure_id: String::new(),
        adapter_id: ADAPTER_ID.to_owned(),
        phase: block.phase,
        kind: block.kind,
        runner_package_id: None,
        runner_test_id,
        test_segments: vec![],
        test_name,
        param_id: None,
        test_group_id: None,
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
                ansi_stripped: block.ansi_stripped,
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

fn details_for(block: &JestBlock) -> Details {
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
        FailureKind::DependencyResolutionError => Details::DependencyResolutionError {
            package: missing_package(&block.message),
            resolver: resolver_for_message(&block.message),
        },
        FailureKind::TypeError => Details::TypeError {
            spans: vec![span_from_location(&block.primary_location)],
            explanation_command: None,
        },
        FailureKind::SyntaxError => Details::SyntaxError {
            spans: vec![span_from_location(&block.primary_location)],
        },
        FailureKind::UnhandledRejection => Details::UnhandledRejection {
            rejection_value: block.message.clone(),
            stack: vec![frame_from_location(&block.primary_location)],
        },
        FailureKind::Exception => Details::Exception {
            exception_type: block.exception_type.clone(),
            stack: vec![frame_from_location(&block.primary_location)],
        },
        _ => Details::Unknown {},
    }
}

fn runner_outcome(failures: &[Failure], parsed: &JestParse, sidecar: &Sidecar) -> RunnerOutcome {
    // EXTRACT-CANDIDATE: this 7-row runner outcome decision table recurs across test-runner adapters.
    summary_runner_outcome(SummaryRunnerOutcomeSignals {
        has_pre_test_failure: failures.iter().any(is_pre_test_failure),
        has_failures: !failures.is_empty(),
        signal_received: sidecar.signal.is_some(),
        no_tests: sidecar.exit_code == Some(0)
            && parsed
                .final_counts
                .as_ref()
                .is_some_and(FinalCounts::is_no_tests),
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

fn ranking(failures: &[Failure]) -> Ranking {
    Ranking {
        strategy: if failures.is_empty() {
            RankingStrategy::None
        } else if failures.iter().any(is_pre_test_failure) {
            RankingStrategy::RunnerPriority
        } else {
            RankingStrategy::FirstEmitted
        },
        is_stable_across_runs: false,
    }
}

fn counts(failures: &[Failure], parsed: &JestParse, runner_outcome: RunnerOutcome) -> Counts {
    if failures.iter().any(is_pre_test_failure) {
        return zero_test_counts();
    }

    let outcome_counts = failure_outcome_counts(failures);
    let tests_run = parsed
        .final_counts
        .as_ref()
        .map(|counts| counts.total)
        .unwrap_or(outcome_counts.failed + outcome_counts.errored);
    let tests_skipped = parsed
        .final_counts
        .as_ref()
        .map_or(0, |counts| counts.skipped + counts.todo);
    let tests_passed = if runner_outcome == RunnerOutcome::HarnessCrashed {
        0
    } else {
        parsed.final_counts.as_ref().map_or_else(
            || {
                tests_run
                    .saturating_sub(outcome_counts.failed + outcome_counts.errored + tests_skipped)
            },
            |counts| counts.passed,
        )
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

fn is_pre_test_failure(failure: &Failure) -> bool {
    matches!(failure.phase, Phase::Build | Phase::Collect)
        || (failure.phase == Phase::Setup && failure.runner_test_id.is_none())
}

fn suggested_next_commands(failures: &[Failure], cwd: &str) -> Vec<Command> {
    failures
        .iter()
        .map(|failure| {
            if let Some(runner_test_id) = failure.runner_test_id.as_deref() {
                let file = runner_test_id
                    .split_once(" > ")
                    .map_or(runner_test_id, |(file, _)| file);
                let test_name = failure.test_name.as_deref().unwrap_or(runner_test_id);
                let pattern = jest_test_name_pattern(test_name);
                let argv = vec![
                    "jest".to_owned(),
                    file.to_owned(),
                    "-t".to_owned(),
                    pattern.clone(),
                ];
                Command {
                    command: format!("jest {file} -t \"{}\"", pattern.replace('"', "\\\"")),
                    argv,
                    cwd: cwd.to_owned(),
                    intent: CommandIntent::RerunIsolated,
                    interactive: false,
                    mutates_workspace: false,
                    uses_network: false,
                    requires_approval: false,
                }
            } else {
                let argv = vec![
                    "failtrace".to_owned(),
                    "explain".to_owned(),
                    failure.failure_id.clone(),
                ];
                Command {
                    command: format!("failtrace explain {}", failure.failure_id),
                    argv,
                    cwd: cwd.to_owned(),
                    intent: CommandIntent::ExplainDiagnostic,
                    interactive: false,
                    mutates_workspace: false,
                    uses_network: false,
                    requires_approval: false,
                }
            }
        })
        .collect()
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

fn has_unaccounted_run_failure(lines: &[LogLine]) -> bool {
    lines.iter().any(|line| {
        let lower = line.stripped.to_ascii_lowercase();
        let coverage_threshold_failure = lower.contains("coverage")
            && lower.contains("threshold")
            && (lower.contains("not met") || lower.contains("does not meet"));
        let snapshot_maintenance_failure = lower.contains("snapshot")
            && snapshot_maintenance_count_regex()
                .captures_iter(&lower)
                .any(|captures| captures["count"].parse::<u64>().unwrap_or(0) > 0);
        let global_hook_failure = lower.contains("jest: got error running globalteardown")
            || lower.contains("jest: got error running globalsetup");
        coverage_threshold_failure || snapshot_maintenance_failure || global_hook_failure
    })
}

fn snapshot_maintenance_count_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?P<count>\d+)\s+(?:(?:files?\s+)?obsolete|unchecked)\b").unwrap()
    })
}

fn parse_version(lines: &[LogLine]) -> Option<JestVersion> {
    lines.iter().find_map(|line| {
        version_regex()
            .captures(&line.stripped)
            .map(|captures| JestVersion {
                version: captures["version"].to_owned(),
            })
    })
}

fn parse_file_recaps(lines: &[LogLine]) -> Vec<FileRecap> {
    lines
        .iter()
        .filter_map(|line| {
            let captures = file_recap_regex().captures(&line.stripped)?;
            let status = match &captures["status"] {
                "PASS" => FileStatus::Pass,
                "FAIL" => FileStatus::Fail,
                _ => return None,
            };
            Some(FileRecap {
                status,
                path: captures["path"].to_owned(),
            })
        })
        .collect()
}

fn parse_final_counts(lines: &[LogLine]) -> Option<FinalCounts> {
    let errors = parse_errors_summary(lines).unwrap_or(0);
    if let Some(mut counts) = lines.iter().rev().find_map(|line| {
        tests_summary_regex()
            .captures(&line.stripped)
            .map(|captures| parse_tests_summary(&captures["body"]))
    }) {
        counts.errors = errors;
        return Some(counts);
    }

    has_no_test_files_summary(lines).then(FinalCounts::default)
}

fn has_summary_block(lines: &[LogLine]) -> bool {
    let has_no_tests = lines
        .iter()
        .any(|line| no_test_files_regex().is_match(&line.stripped));
    let has_suite_summary = lines
        .iter()
        .any(|line| test_suites_summary_regex().is_match(&line.stripped));
    let has_run_summary_tail = lines
        .iter()
        .any(|line| snapshots_summary_regex().is_match(&line.stripped))
        && lines
            .iter()
            .any(|line| time_summary_regex().is_match(&line.stripped));
    has_no_tests || (has_suite_summary && has_run_summary_tail)
}

fn parse_tests_summary(body: &str) -> FinalCounts {
    let mut counts = FinalCounts::default();
    for captures in count_segment_regex().captures_iter(body) {
        let count = captures["count"].parse::<u64>().unwrap_or(0);
        match &captures["name"] {
            "failed" => counts.failed = count,
            "passed" => counts.passed = count,
            "skipped" => counts.skipped = count,
            "todo" => counts.todo = count,
            "total" => counts.total = count,
            _ => {}
        }
    }
    if counts.total == 0 {
        counts.total = tests_total_regex()
            .captures(body)
            .and_then(|captures| captures["total"].parse().ok())
            .unwrap_or(counts.failed + counts.passed + counts.skipped + counts.todo);
    }
    counts
}

fn parse_errors_summary(lines: &[LogLine]) -> Option<u64> {
    lines.iter().rev().find_map(|line| {
        test_suites_summary_regex()
            .captures(&line.stripped)
            .map(|captures| parse_tests_summary(&captures["body"]).failed)
            .filter(|failed| *failed > 0)
    })
}

fn has_no_test_files_summary(lines: &[LogLine]) -> bool {
    lines
        .iter()
        .any(|line| no_test_files_regex().is_match(&line.stripped))
}

fn completed_summary_count(lines: &[LogLine]) -> u64 {
    let mut count = 0;
    let mut saw_file_recap = false;
    let mut saw_test_suites_summary = false;
    let mut saw_tests_summary = false;
    let mut saw_snapshots_summary = false;

    for line in lines {
        if file_recap_regex().is_match(&line.stripped) {
            saw_file_recap = true;
            saw_test_suites_summary = false;
            saw_tests_summary = false;
            saw_snapshots_summary = false;
            continue;
        }
        if no_test_files_regex().is_match(&line.stripped) && !saw_file_recap {
            count += 1;
            saw_test_suites_summary = false;
            saw_tests_summary = false;
            saw_snapshots_summary = false;
            continue;
        }
        if test_suites_summary_regex().is_match(&line.stripped) {
            saw_test_suites_summary = true;
            saw_tests_summary = false;
            saw_snapshots_summary = false;
        } else if saw_test_suites_summary && tests_summary_regex().is_match(&line.stripped) {
            saw_tests_summary = true;
        } else if saw_tests_summary && snapshots_summary_regex().is_match(&line.stripped) {
            saw_snapshots_summary = true;
        } else if saw_file_recap
            && saw_snapshots_summary
            && time_summary_regex().is_match(&line.stripped)
        {
            count += 1;
            saw_file_recap = false;
            saw_test_suites_summary = false;
            saw_tests_summary = false;
            saw_snapshots_summary = false;
        }
    }

    count
}

fn parse_blocks(
    lines: &[LogLine],
    file_recaps: &[FileRecap],
    path_case_folding: PathCaseFolding,
) -> Vec<JestBlock> {
    let mut blocks = Vec::new();
    let mut index = 0;
    while let Some(start) = next_fail_recap_line(lines, index) {
        let path = file_recap_regex()
            .captures(&lines[start].stripped)
            .map(|captures| captures["path"].to_owned());
        let end = next_file_section_end(lines, start + 1);
        if let Some(path) = path
            && file_recaps
                .iter()
                .any(|recap| recap.status == FileStatus::Fail && recap.path == path)
        {
            blocks.extend(parse_file_blocks(
                &lines[start + 1..end],
                &path,
                path_case_folding,
            ));
        }
        index = end;
    }
    blocks
}

fn next_fail_recap_line(lines: &[LogLine], from: usize) -> Option<usize> {
    lines[from..]
        .iter()
        .position(|line| {
            file_recap_regex()
                .captures(&line.stripped)
                .is_some_and(|captures| &captures["status"] == "FAIL")
        })
        .map(|offset| from + offset)
}

fn next_file_section_end(lines: &[LogLine], from: usize) -> usize {
    for (index, line) in lines.iter().enumerate().skip(from) {
        if file_recap_regex().is_match(&line.stripped)
            || test_suites_summary_regex().is_match(&line.stripped)
            || tests_summary_regex().is_match(&line.stripped)
        {
            return index;
        }
    }
    lines.len()
}

fn parse_file_blocks(
    lines: &[LogLine],
    path: &str,
    path_case_folding: PathCaseFolding,
) -> Vec<JestBlock> {
    let mut blocks = Vec::new();
    let mut index = 0;
    while let Some(start) = next_bullet_line(lines, index) {
        let end = next_bullet_or_section_end(lines, start + 1);
        if let Some(block) = parse_block(path, &lines[start..end], path_case_folding) {
            blocks.push(block);
        }
        index = end;
    }
    blocks
}

fn next_bullet_line(lines: &[LogLine], from: usize) -> Option<usize> {
    lines[from..]
        .iter()
        .position(|line| bullet_regex().is_match(&line.stripped))
        .map(|offset| from + offset)
}

fn next_bullet_or_section_end(lines: &[LogLine], from: usize) -> usize {
    for (index, line) in lines.iter().enumerate().skip(from) {
        if bullet_regex().is_match(&line.stripped)
            || file_recap_regex().is_match(&line.stripped)
            || test_suites_summary_regex().is_match(&line.stripped)
            || tests_summary_regex().is_match(&line.stripped)
        {
            return index;
        }
    }
    lines.len()
}

fn parse_block(
    path: &str,
    block_lines: &[LogLine],
    path_case_folding: PathCaseFolding,
) -> Option<JestBlock> {
    let title_line = block_lines.first()?;
    let captures = bullet_regex().captures(&title_line.stripped)?;
    let rest = captures["rest"].to_owned();

    let message_line = first_message_line(&block_lines[1..])?;
    let stripped_message = strip_matcher_comment(message_line.stripped.trim());
    let raw_message = strip_matcher_comment(message_line.raw.trim());
    let ansi_stripped =
        raw_message != stripped_message || block_lines.iter().any(|line| line.raw != line.stripped);
    let location = primary_location(path, block_lines, &stripped_message, path_case_folding);

    let (phase, kind, partial_reason) = classify(&rest, &stripped_message);
    let first = block_lines.first()?;
    let last = block_lines.last()?;
    let runner_test_id = runner_test_id(path, &rest, phase);

    Some(JestBlock {
        path: path.to_owned(),
        rest,
        runner_test_id,
        phase,
        kind,
        primary_location: location,
        exception_type: exception_type(&stripped_message),
        message: stripped_message,
        raw_log_lines: [first.number, last.number],
        byte_range: ByteRange {
            start: first.start,
            end: last.end,
        },
        excerpt_text: excerpt_text(block_lines),
        ansi_stripped,
        partial_reason,
    })
}

fn first_message_line(lines: &[LogLine]) -> Option<&LogLine> {
    lines.iter().find(|line| {
        !line.stripped.trim().is_empty() && !code_frame_regex().is_match(&line.stripped)
    })
}

fn primary_location(
    path: &str,
    block_lines: &[LogLine],
    message: &str,
    path_case_folding: PathCaseFolding,
) -> Location {
    block_lines
        .iter()
        .filter_map(|line| stack_frame_regex().captures(&line.stripped))
        .find(|captures| user_stack_path(&captures["path"], path_case_folding))
        .map(|captures| Location {
            file: captures["path"].to_owned(),
            line: captures["line"].parse().ok(),
            column: captures["column"].parse().ok(),
            end_line: None,
            end_column: None,
            role: location_role_for(message),
            in_user_code: is_user_code_path(
                &captures["path"],
                UserCodeEcosystem::JavaScript,
                path_case_folding,
            ),
        })
        .or_else(|| {
            block_lines
                .iter()
                .find_map(|line| code_frame_regex().captures(&line.stripped))
                .map(|captures| Location {
                    file: path.to_owned(),
                    line: captures["line"].parse().ok(),
                    column: None,
                    end_line: None,
                    end_column: None,
                    role: location_role_for(message),
                    in_user_code: is_user_code_path(
                        path,
                        UserCodeEcosystem::JavaScript,
                        path_case_folding,
                    ),
                })
        })
        .unwrap_or_else(|| Location {
            file: path.to_owned(),
            line: None,
            column: None,
            end_line: None,
            end_column: None,
            role: location_role_for(message),
            in_user_code: is_user_code_path(path, UserCodeEcosystem::JavaScript, path_case_folding),
        })
}

fn user_stack_path(path: &str, path_case_folding: PathCaseFolding) -> bool {
    is_user_code_path(path, UserCodeEcosystem::JavaScript, path_case_folding)
        && !path.starts_with("node:")
        && !path.contains("/jest-")
        && !path.contains("/@jest/")
}

fn runner_test_id(path: &str, rest: &str, phase: Phase) -> Option<String> {
    if rest == "Test suite failed to run" || matches!(phase, Phase::Build | Phase::Collect) {
        None
    } else {
        Some(format!("{} > {}", path, test_name_from_fail_rest(rest)))
    }
}

// EXTRACT-CANDIDATE: phase/failure-kind dispatch has the same adapter-local policy shape across JS test runners.
fn classify(rest: &str, message: &str) -> (Phase, FailureKind, Option<String>) {
    let hook_phase = hook_phase(rest);
    let kind = kind_for_message(message);
    if snapshot_mismatch(message) {
        return (
            hook_phase.unwrap_or(Phase::Test),
            FailureKind::Exception,
            Some("snapshot_mismatch_not_structured".to_owned()),
        );
    }
    if let Some(phase) = hook_phase {
        return (phase, kind, Some("hook_failure_not_structured".to_owned()));
    }
    if rest == "Test suite failed to run" {
        if is_build_message(message) {
            return (Phase::Build, kind, None);
        }
        if matches!(
            kind,
            FailureKind::MissingSymbol | FailureKind::DependencyResolutionError
        ) {
            return (Phase::Collect, kind, None);
        }
        if is_setup_or_environment_message(message) {
            return (Phase::Setup, kind, None);
        }
        return (Phase::Collect, kind, None);
    }
    match kind {
        FailureKind::MissingSymbol | FailureKind::DependencyResolutionError => {
            (Phase::Collect, kind, None)
        }
        _ => (Phase::Test, kind, None),
    }
}

fn kind_for_message(message: &str) -> FailureKind {
    if message.contains("Unhandled Rejection") || message.contains("unhandled rejection") {
        return FailureKind::UnhandledRejection;
    }
    if is_assertion_message(message) {
        return FailureKind::Assertion;
    }
    if is_build_message(message) {
        return FailureKind::SyntaxError;
    }
    if is_module_not_found(message) {
        return FailureKind::MissingSymbol;
    }
    if is_dependency_resolution(message) {
        return FailureKind::DependencyResolutionError;
    }
    match exception_type(message).as_deref() {
        Some("AssertionError") => FailureKind::Assertion,
        Some("TypeError") => FailureKind::TypeError,
        Some("SyntaxError") => FailureKind::SyntaxError,
        Some("ModuleNotFoundError") | Some("ImportError") => FailureKind::MissingSymbol,
        Some(_) | None => FailureKind::Exception,
    }
}

fn hook_phase(rest: &str) -> Option<Phase> {
    if rest.contains("beforeAll") || rest.contains("beforeEach") {
        Some(Phase::Setup)
    } else if rest.contains("afterAll") || rest.contains("afterEach") {
        Some(Phase::Teardown)
    } else {
        None
    }
}

fn location_role_for(message: &str) -> LocationRole {
    if matches!(
        kind_for_message(message),
        FailureKind::MissingSymbol | FailureKind::DependencyResolutionError
    ) {
        LocationRole::ImportSite
    } else {
        LocationRole::Primary
    }
}

fn test_name_from_fail_rest(rest: &str) -> String {
    rest.to_owned()
}

fn jest_test_name_pattern(test_name: &str) -> String {
    regex::escape(&test_name.replace(" \u{203a} ", " "))
}

fn strip_matcher_comment(message: &str) -> String {
    matcher_comment_regex()
        .replace(message, "")
        .trim_end()
        .to_owned()
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

fn missing_package(message: &str) -> Option<String> {
    package_regex()
        .captures(message)
        .map(|captures| captures["package"].to_owned())
}

fn resolver_for_message(message: &str) -> Resolver {
    if message.contains("pnpm") {
        Resolver::Pnpm
    } else if message.contains("yarn") {
        Resolver::Yarn
    } else if message.contains("npm") {
        Resolver::Npm
    } else {
        Resolver::Unknown
    }
}

fn is_module_not_found(message: &str) -> bool {
    message.contains("Cannot find module")
        || message.contains("Cannot find package")
        || message.contains("Module not found")
        || message.contains("Failed to resolve import")
        || message.contains("ERR_MODULE_NOT_FOUND")
}

fn is_dependency_resolution(message: &str) -> bool {
    message.contains("ERR_PNPM")
        || message.contains("npm ERR!")
        || yarn_diagnostic_regex().is_match(message)
        || message.contains("package manager")
}

fn yarn_diagnostic_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"\bYN\d{4}\b").unwrap())
}

fn snapshot_mismatch(message: &str) -> bool {
    message.to_ascii_lowercase().contains("snapshot")
}

fn is_assertion_message(message: &str) -> bool {
    message.contains("expect(")
        || message.contains("Expected:")
        || message.contains("Received:")
        || message.contains("toBe(")
}

fn is_build_message(message: &str) -> bool {
    message.contains("Jest encountered an unexpected token")
        || message.contains("Unexpected token")
        || message.contains("SyntaxError")
        || message.contains("transform")
}

fn is_setup_or_environment_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("setup file")
        || lower.contains("setupfiles")
        || lower.contains("test environment")
        || lower.contains("environment")
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

fn strip_ansi(text: &str) -> String {
    ansi_regex().replace_all(text, "").into_owned()
}

fn version_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    // EXTRACT-CANDIDATE: OnceLock<Regex> accessor wrappers are duplicated across adapters.
    REGEX.get_or_init(|| Regex::new(r"^jest(?:@|\s+)(?<version>\d+\.\d+\.\d+)\b").unwrap())
}

fn file_recap_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^(?<status>PASS|FAIL)\s+(?<path>\S.*)$").unwrap())
}

fn bullet_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^\s*● (?<rest>.+)$").unwrap())
}

fn stack_frame_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"^\s+at (?:.+ \()?(?<path>[^()]+):(?<line>\d+):(?<column>\d+)\)?$").unwrap()
    })
}

fn code_frame_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^\s*>?\s*(?<line>\d+) \|").unwrap())
}

fn test_suites_summary_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^Test Suites:\s+(?<body>.+)$").unwrap())
}

fn tests_summary_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^Tests:\s+(?<body>.+)$").unwrap())
}

fn snapshots_summary_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^Snapshots:\s+.+$").unwrap())
}

fn time_summary_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^Time:\s+.+$").unwrap())
}

fn no_test_files_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"^No tests? (?:found|files found), exiting with code (?<code>[01])$").unwrap()
    })
}

fn count_segment_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?<count>\d+) (?<name>failed|passed|skipped|todo|total)").unwrap()
    })
}

fn tests_total_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?<total>\d+) total").unwrap())
}

fn matcher_comment_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"\s+// .*$").unwrap())
}

fn exception_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"^(?<name>[A-Za-z_][A-Za-z0-9_]*(?:Error|Exception))\b").unwrap()
    })
}

fn missing_symbol_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?:Cannot find module|No module named) ['"](?<symbol>[^'"]+)['"]"#).unwrap()
    })
}

fn package_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?<package>@?[\w.-]+/[\w.-]+|@?[\w.-]+)").unwrap())
}

fn ansi_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]").unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASIC_ASSERTION: &[u8] =
        include_bytes!("../../tests/fixtures/jest/basic_assertion/run/log");
    const GREEN: &[u8] = include_bytes!("../../tests/fixtures/jest/green/run/log");

    #[test]
    fn parses_recaps_fail_block_and_counts() {
        let parsed = JestParse::from_log(BASIC_ASSERTION);
        assert_eq!(parsed.version, None);
        assert_eq!(parsed.file_recaps.len(), 1);
        assert_eq!(parsed.failure_blocks.len(), 1);
        assert_eq!(
            parsed.failure_blocks[0].message,
            "expect(received).toBe(expected)"
        );
        assert_eq!(parsed.failure_blocks[0].primary_location.line, Some(3));
        assert_eq!(parsed.failure_blocks[0].primary_location.column, Some(19));
        assert_eq!(parsed.final_counts.unwrap().total, 1);
        assert!(parsed.parse_warnings.is_empty());
    }

    #[test]
    fn green_run_parses_counts_without_failures() {
        let parsed = JestParse::from_log(GREEN);
        let sidecar = Sidecar {
            meta_schema_version: "1.2".to_owned(),
            schema_version: "1.6".to_owned(),
            failtrace_version: "0.1.0".to_owned(),
            capture_build: None,
            adapters: vec![],
            adapter_candidates: vec![],
            argv: vec![],
            command: String::new(),
            cwd: String::new(),
            started_at: String::new(),
            duration_ms: 0,
            exit_code: Some(0),
            signal: None,
            early_exit: None,
            truncated: false,
            truncation_reason: None,
            path_case_folding: crate::storage::PathCaseFolding::CaseSensitive,
            stream_merge: crate::storage::StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
        };
        assert!(parsed.looks_like_jest(&sidecar));
        assert!(parsed.failure_blocks.is_empty());
        let final_counts = parsed.final_counts.unwrap();
        assert_eq!(final_counts.passed, 2);
        assert_eq!(final_counts.total, 2);
    }

    #[test]
    fn count_only_log_is_rejected() {
        let parsed = JestParse::from_log(b"2 passed\n");
        let sidecar = Sidecar {
            meta_schema_version: "1.2".to_owned(),
            schema_version: "1.6".to_owned(),
            failtrace_version: "0.1.0".to_owned(),
            capture_build: None,
            adapters: vec![],
            adapter_candidates: vec![],
            argv: vec![],
            command: String::new(),
            cwd: String::new(),
            started_at: String::new(),
            duration_ms: 0,
            exit_code: Some(0),
            signal: None,
            early_exit: None,
            truncated: false,
            truncation_reason: None,
            path_case_folding: crate::storage::PathCaseFolding::CaseSensitive,
            stream_merge: crate::storage::StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
        };
        assert!(!parsed.looks_like_jest(&sidecar));
    }

    #[test]
    fn jest_version_policy_accepts_only_validated_major() {
        for version in ["30.0.0", "30.4.1"] {
            let version = JestVersion {
                version: version.to_owned(),
            };
            assert!(version.has_validated_output_family(), "{version:?}");
        }
        for version in ["29.7.0", "31.0.0"] {
            let version = JestVersion {
                version: version.to_owned(),
            };
            assert!(!version.has_validated_output_family(), "{version:?}");
        }
    }

    #[test]
    fn node_esm_module_not_found_is_missing_symbol() {
        assert_eq!(
            kind_for_message("Error [ERR_MODULE_NOT_FOUND]: Cannot find package 'foo'"),
            FailureKind::MissingSymbol
        );
        assert_eq!(
            kind_for_message("Error: Cannot find module 'bar'"),
            FailureKind::MissingSymbol
        );
    }

    #[test]
    fn yarn_diagnostic_codes_match_word_boundary() {
        assert_eq!(
            kind_for_message("Error: YN0001 something"),
            FailureKind::DependencyResolutionError
        );
        assert_eq!(
            kind_for_message("Error: DYNAMIC_IMPORT failed"),
            FailureKind::Exception
        );
        assert_eq!(
            kind_for_message("Error: ASYNC_CONFIG failed"),
            FailureKind::Exception
        );
    }

    #[test]
    fn jest_test_name_pattern_strips_reporter_separator() {
        assert_eq!(
            jest_test_name_pattern("addition \u{203a} adds two numbers"),
            "addition adds two numbers"
        );
        assert_eq!(
            jest_test_name_pattern("regex chars . * + ?"),
            r"regex chars \. \* \+ \?"
        );
    }
}
