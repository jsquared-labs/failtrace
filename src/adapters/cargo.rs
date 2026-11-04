use std::{collections::BTreeMap, sync::OnceLock};

use regex::Regex;

use crate::{
    adapters::{
        AdapterError, AdapterImpl, AnalyzeOptions,
        shared::{
            AnalysisDocumentParts, FailureListCoverage, SummaryParseSignals,
            SummaryRunnerOutcomeSignals, UserCodeEcosystem, build_analysis_document,
            distinct_failure_files, failure_outcome_counts, is_user_code_path,
            sidecar_selected_adapter, summary_parse_result, summary_runner_outcome,
            truncate_for_budget, zero_test_counts,
        },
    },
    schema::{
        ByteRange, Command, CommandIntent, Counts, Details, DiagnosticSeverity, Evidence, Excerpt,
        ExcerptKind, ExcerptSource, Failure, FailureKind, Frame, Location, LocationRole,
        OutputDocument, ParseStatus, ParseWarning, Phase, Ranking, RankingStrategy, Resolver,
        RunnerOutcome, Span, derive_failure_id, validate,
    },
    storage::{PathCaseFolding, Sidecar},
};

const ADAPTER_ID: &str = "cargo-test";

pub fn analyze_log(
    log: &[u8],
    sidecar: &Sidecar,
    options: AnalyzeOptions,
) -> Result<OutputDocument, AdapterError> {
    CargoAdapter::analyze_log(log, sidecar, options)
}

struct CargoAdapter;

impl AdapterImpl for CargoAdapter {
    fn analyze_log(
        log: &[u8],
        sidecar: &Sidecar,
        options: AnalyzeOptions,
    ) -> Result<OutputDocument, AdapterError> {
        if !sidecar_selected_adapter(sidecar, ADAPTER_ID) {
            return Err(AdapterError::InvalidSidecar {
                reason: "sidecar does not select cargo-test adapter".to_owned(),
            });
        }

        if command_uses_json_messages(&sidecar.command) {
            let document = json_message_rejection(log, sidecar);
            let violations = validate(&document);
            if !violations.is_empty() {
                return Err(AdapterError::InvariantViolation(violations));
            }
            return Ok(document);
        }

        let parsed = CargoParse::from_log_with_path_case_folding(log, sidecar.path_case_folding);
        if !parsed.looks_like_cargo_test(sidecar) {
            return Err(AdapterError::UnrecognizedFormat {
                reason: "no cargo test build, rustc diagnostic, libtest harness, or summary found"
                    .to_owned(),
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
struct CargoParse {
    build_failures: Vec<CargoFailureBlock>,
    reported_rustc_diagnostic_count: u64,
    reported_compile_error_lower_bound: u64,
    extracted_rustc_diagnostic_count: u64,
    reported_failed_target_count: u64,
    test_failures: Vec<CargoFailureBlock>,
    summaries: Vec<TestSummary>,
    rerun_pass_tails: Vec<RerunPassTail>,
    has_cargo_signature: bool,
    has_rustc_header_before_harness: bool,
    has_harness_start: bool,
    harness_start_count: u64,
    parse_warnings: Vec<ParseWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RerunPassTail {
    line_number: u64,
    tail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Placeholder for v1.5 sidecar.runner_version validation.
struct CargoVersion;

#[allow(dead_code)]
impl CargoVersion {
    // EXTRACT-CANDIDATE: adapter version family validation is shared policy with runner-specific validated majors.
    // v1.4 sidecars do not carry structured runner_version. Cargo version-family validation
    // is intentionally a no-op until the v1.5 sidecar.runner_version schema slice.
    fn has_validated_output_family(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoFailureBlock {
    phase: Phase,
    kind: FailureKind,
    runner_test_id: Option<String>,
    test_name: Option<String>,
    primary_location: Location,
    extra_locations: Vec<Location>,
    message: String,
    diagnostic_code: Option<String>,
    details: Details,
    raw_log_lines: [u64; 2],
    byte_range: ByteRange,
    excerpt_text: String,
    ansi_stripped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct TestSummary {
    passed: u64,
    failed: u64,
    ignored: u64,
    measured: u64,
    filtered_out: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogLine {
    number: u64,
    start: u64,
    end: u64,
    raw: String,
    stripped: String,
}

impl CargoParse {
    #[cfg(test)]
    fn from_log(log: &[u8]) -> Self {
        Self::from_log_with_path_case_folding(log, PathCaseFolding::CaseSensitive)
    }

    fn from_log_with_path_case_folding(log: &[u8], path_case_folding: PathCaseFolding) -> Self {
        let lines = split_lines(log);
        let first_harness = first_harness_start(&lines);
        let build_limit = first_harness.unwrap_or(lines.len());
        let build_lines = &lines[..build_limit];
        let test_lines = first_harness.map_or(&[][..], |index| &lines[index..]);

        let reported_rustc_diagnostic_count = build_lines
            .iter()
            .filter(|line| is_rustc_diagnostic_header(&line.stripped))
            .count() as u64;
        let reported_compile_error_lower_bound = build_lines
            .iter()
            .filter_map(|line| {
                cargo_compile_error_count_regex()
                    .captures(&line.stripped)
                    .and_then(|captures| captures["count"].parse().ok())
            })
            .max()
            .unwrap_or(0);
        let mut build_failures = parse_rustc_failures(build_lines, path_case_folding);
        let extracted_rustc_diagnostic_count = build_failures.len() as u64;
        if build_failures.is_empty() {
            if let Some(block) = parse_dependency_resolution_failure(build_lines) {
                build_failures.push(block);
            } else if let Some(block) = parse_build_script_failure(build_lines, path_case_folding) {
                build_failures.push(block);
            } else if let Some(block) = parse_unknown_build_failure(build_lines) {
                build_failures.push(block);
            }
        }

        let test_failures = parse_test_failures(test_lines, path_case_folding);
        let summaries = parse_test_summaries(&lines);
        let failed_target_terminal_count = lines
            .iter()
            .filter(|line| failed_target_terminal_regex().is_match(&line.stripped))
            .count() as u64;
        let aggregate_failed_target_count = lines
            .iter()
            .filter_map(|line| {
                aggregate_failed_targets_regex()
                    .captures(&line.stripped)
                    .and_then(|captures| captures["count"].parse().ok())
            })
            .max()
            .unwrap_or(0);
        let reported_failed_target_count =
            failed_target_terminal_count.max(aggregate_failed_target_count);
        let rerun_pass_tails = parse_rerun_pass_tails(&lines);
        let has_cargo_signature = lines.iter().any(|line| {
            cargo_status_regex().is_match(&line.stripped)
                || line.stripped.starts_with("error: test failed")
                || line.stripped.starts_with("error: doctest failed")
                || line.stripped.starts_with("error: could not compile")
                || line
                    .stripped
                    .starts_with("error: failed to select a version")
                || line
                    .stripped
                    .starts_with("error: failed to run custom build command")
        });
        let has_rustc_header_before_harness = build_lines
            .iter()
            .any(|line| rustc_header_regex().is_match(&line.stripped));
        let harness_start_count = lines
            .iter()
            .filter(|line| running_tests_regex().is_match(&line.stripped))
            .count() as u64;
        let has_harness_start = harness_start_count > 0;

        Self {
            build_failures,
            reported_rustc_diagnostic_count,
            reported_compile_error_lower_bound,
            extracted_rustc_diagnostic_count,
            reported_failed_target_count,
            test_failures,
            summaries,
            rerun_pass_tails,
            has_cargo_signature,
            has_rustc_header_before_harness,
            has_harness_start,
            harness_start_count,
            parse_warnings: vec![],
        }
    }

    fn looks_like_cargo_test(&self, sidecar: &Sidecar) -> bool {
        // EXTRACT-CANDIDATE: runner recognition gates now share a multi-signal shape with adapter-specific signatures.
        // Bare rustc diagnostics are admissible only because the sidecar argv selected cargo-test.
        let sidecar_pins_cargo = sidecar
            .adapters
            .iter()
            .any(|adapter| adapter.id == ADAPTER_ID && adapter.selected);
        self.has_cargo_signature
            || self.has_harness_start
            || !self.build_failures.is_empty()
            || !self.test_failures.is_empty()
            || (sidecar_pins_cargo && self.has_rustc_header_before_harness)
    }
}

fn lower(
    parsed: CargoParse,
    log: &[u8],
    sidecar: &Sidecar,
    options: AnalyzeOptions,
) -> OutputDocument {
    let all_failures = reconcile_failures(&parsed, &sidecar.cwd);
    let runner_outcome = runner_outcome(&all_failures, &parsed, sidecar);
    let counts = counts(&all_failures, &parsed, runner_outcome);
    let harness_crashed = runner_outcome == RunnerOutcome::HarnessCrashed;
    let missing_harness_summary = parsed.harness_start_count > parsed.summaries.len() as u64;
    let runner_summary_mismatch = !parsed.summaries.is_empty()
        && parsed.summaries.iter().all(|summary| summary.failed == 0)
        && (sidecar.exit_code != Some(0) || sidecar.signal.is_some());
    let failure_list_coverage = vec![
        FailureListCoverage {
            reported_units: parsed.summaries.iter().map(|summary| summary.failed).sum(),
            accounted_units: parsed.test_failures.len() as u64,
        },
        FailureListCoverage {
            reported_units: parsed.reported_rustc_diagnostic_count,
            accounted_units: parsed.extracted_rustc_diagnostic_count,
        },
        FailureListCoverage {
            reported_units: parsed.reported_compile_error_lower_bound,
            accounted_units: parsed.extracted_rustc_diagnostic_count,
        },
        FailureListCoverage {
            reported_units: parsed.reported_failed_target_count,
            accounted_units: parsed
                .summaries
                .iter()
                .filter(|summary| summary.failed > 0)
                .count() as u64,
        },
    ];
    let mut parse_warnings = parsed.parse_warnings;
    if runner_summary_mismatch && !parse_warnings.contains(&ParseWarning::RunnerSummaryMismatch) {
        parse_warnings.push(ParseWarning::RunnerSummaryMismatch);
    }
    let parse = summary_parse_result(SummaryParseSignals {
        parse_warnings,
        contextual_degrading_warnings: vec![],
        sidecar_truncated: sidecar.truncated,
        // Every `running N tests` starts a distinct libtest/doctest harness and
        // must have its own `test result:` summary. With --no-fail-fast, an
        // earlier target can complete and fail normally before a later target
        // aborts; whole-run outcome alone cannot reveal that missing tail.
        missing_summary: missing_harness_summary
            || (harness_crashed && parsed.summaries.is_empty()),
        incomplete_failure_list: harness_crashed
            || missing_harness_summary
            || runner_summary_mismatch,
        failure_list_coverage,
    });
    let budgeted_failures = truncate_for_budget(all_failures, options);

    let suggested_next_commands = suggested_next_commands(
        &budgeted_failures.failures,
        &sidecar.cwd,
        &parsed.rerun_pass_tails,
    );
    let ranking = ranking(&budgeted_failures.failures, sidecar);

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
        extensions: BTreeMap::new(),
        options,
    })
}

fn reconcile_failures(parsed: &CargoParse, cwd: &str) -> Vec<Failure> {
    parsed
        .build_failures
        .iter()
        .chain(parsed.test_failures.iter())
        .map(|block| failure_from_block(block, cwd))
        .collect()
}

fn failure_from_block(block: &CargoFailureBlock, cwd: &str) -> Failure {
    let mut failure = Failure {
        failure_id: String::new(),
        adapter_id: ADAPTER_ID.to_owned(),
        phase: block.phase,
        kind: block.kind,
        runner_package_id: None,
        runner_test_id: block.runner_test_id.clone(),
        test_segments: vec![],
        test_name: block.test_name.clone(),
        param_id: None,
        test_group_id: None,
        primary_location: block.primary_location.clone(),
        extra_locations: block.extra_locations.clone(),
        message: block.message.clone(),
        diagnostic_code: block.diagnostic_code.clone(),
        diagnostic_severity: DiagnosticSeverity::Error,
        details: block.details.clone(),
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
        extensions: BTreeMap::new(),
    };
    failure.failure_id = derive_failure_id(&failure, cwd);
    failure
}

fn runner_outcome(failures: &[Failure], parsed: &CargoParse, sidecar: &Sidecar) -> RunnerOutcome {
    // EXTRACT-CANDIDATE: this 7-row runner outcome decision table recurs across test-runner adapters.
    let has_tests = parsed.summaries.iter().any(|summary| summary.total() > 0);
    summary_runner_outcome(SummaryRunnerOutcomeSignals {
        has_pre_test_failure: failures.iter().any(is_pre_test_failure),
        has_failures: !failures.is_empty(),
        signal_received: sidecar.signal.is_some(),
        no_tests: sidecar.exit_code == Some(0) && !has_tests,
        passed: sidecar.exit_code == Some(0) && has_tests,
        failed: sidecar.exit_code.is_some()
            && parsed.summaries.iter().any(|summary| summary.failed > 0),
        summary_reports_failures: sidecar.exit_code.is_some_and(|code| code != 0)
            && parsed.summaries.iter().any(|summary| summary.failed > 0),
    })
}

fn ranking(failures: &[Failure], sidecar: &Sidecar) -> Ranking {
    Ranking {
        strategy: if failures.is_empty() {
            RankingStrategy::None
        } else if failures.iter().any(is_pre_test_failure) {
            RankingStrategy::CompileBeforeRuntime
        } else {
            RankingStrategy::FirstEmitted
        },
        is_stable_across_runs: failures.iter().any(is_pre_test_failure)
            || (!failures.is_empty() && has_single_thread_argv(&sidecar.command)),
    }
}

fn counts(failures: &[Failure], parsed: &CargoParse, runner_outcome: RunnerOutcome) -> Counts {
    if failures.iter().any(is_pre_test_failure) {
        return zero_test_counts();
    }

    let summary_total = parsed
        .summaries
        .iter()
        .fold(TestSummary::default(), |mut acc, item| {
            acc.passed += item.passed;
            acc.failed += item.failed;
            acc.ignored += item.ignored;
            acc.measured += item.measured;
            acc.filtered_out += item.filtered_out;
            acc
        });
    let outcome_counts = failure_outcome_counts(failures);
    let tests_run = if parsed.summaries.is_empty() {
        outcome_counts.failed + outcome_counts.errored
    } else {
        summary_total.passed + summary_total.failed + summary_total.ignored + summary_total.measured
    };
    let tests_passed = if runner_outcome == RunnerOutcome::HarnessCrashed {
        0
    } else {
        summary_total.passed
    };

    Counts {
        tests_run,
        tests_passed,
        tests_failed: outcome_counts.failed,
        tests_errored: outcome_counts.errored,
        tests_skipped: summary_total.ignored,
        tests_not_run: Some(summary_total.filtered_out).filter(|count| *count > 0),
        files_failed: distinct_failure_files(failures),
    }
}

impl TestSummary {
    fn total(self) -> u64 {
        self.passed + self.failed + self.ignored + self.measured
    }
}

fn is_pre_test_failure(failure: &Failure) -> bool {
    failure.phase == Phase::Build || failure.phase == Phase::Collect
}

fn has_single_thread_argv(command: &str) -> bool {
    let after_dash: Vec<&str> = command
        .split_whitespace()
        .skip_while(|token| *token != "--")
        .skip(1)
        .collect();
    if after_dash.contains(&"--test-threads=1") {
        return true;
    }
    after_dash
        .windows(2)
        .any(|w| w[0] == "--test-threads" && w[1] == "1")
}

fn suggested_next_commands(
    failures: &[Failure],
    cwd: &str,
    rerun_pass_tails: &[RerunPassTail],
) -> Vec<Command> {
    failures
        .iter()
        .map(|failure| {
            if let Some(code) = failure.diagnostic_code.as_deref() {
                let argv = vec!["rustc".to_owned(), "--explain".to_owned(), code.to_owned()];
                return Command {
                    command: format!("rustc --explain {code}"),
                    argv,
                    cwd: cwd.to_owned(),
                    intent: CommandIntent::ExplainDiagnostic,
                    interactive: false,
                    mutates_workspace: false,
                    uses_network: false,
                    requires_approval: false,
                };
            }
            if let Some(runner_test_id) = failure.runner_test_id.as_deref() {
                let rerun_tail =
                    rerun_tail_for_failure(failure, rerun_pass_tails).unwrap_or_else(|| {
                        if runner_test_id.contains(" - ") && runner_test_id.contains("(line ") {
                            "--doc".to_owned()
                        } else {
                            "--lib".to_owned()
                        }
                    });
                let mut argv = vec!["cargo".to_owned(), "test".to_owned()];
                argv.extend(rerun_tail.split_whitespace().map(str::to_owned));
                argv.push(runner_test_id.to_owned());
                return Command {
                    command: format!("cargo test {} {}", rerun_tail, shell_quote(runner_test_id)),
                    argv,
                    cwd: cwd.to_owned(),
                    intent: CommandIntent::RerunIsolated,
                    interactive: false,
                    mutates_workspace: false,
                    uses_network: false,
                    requires_approval: false,
                };
            }
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
        })
        .collect()
}

fn rerun_tail_for_failure(failure: &Failure, rerun_pass_tails: &[RerunPassTail]) -> Option<String> {
    // Cargo emits `error: test failed, to rerun pass `<tail>`` AFTER each failing
    // test binary's summary. Match each failure to the rerun-pass directive that
    // appears at or after the end of the failure's log range. Fall back to the
    // last tail seen if no positional match (single-failure case).
    let end = failure.raw_log_lines[1];
    rerun_pass_tails
        .iter()
        .find(|t| t.line_number >= end)
        .or_else(|| rerun_pass_tails.last())
        .map(|t| t.tail.clone())
}

fn parse_rerun_pass_tails(lines: &[LogLine]) -> Vec<RerunPassTail> {
    lines
        .iter()
        .filter_map(|line| {
            rerun_pass_regex()
                .captures(&line.stripped)
                .map(|captures| RerunPassTail {
                    line_number: line.number,
                    tail: captures["tail"].to_owned(),
                })
        })
        .collect()
}

fn json_message_rejection(log: &[u8], sidecar: &Sidecar) -> OutputDocument {
    build_analysis_document(AnalysisDocumentParts {
        sidecar,
        log,
        budgeted_failures: truncate_for_budget(Vec::new(), AnalyzeOptions::default()),
        runner_outcome: if sidecar.exit_code == Some(0) {
            RunnerOutcome::Passed
        } else {
            RunnerOutcome::HarnessCrashed
        },
        early_exit: None,
        parse_status: ParseStatus::UnparsedOutput,
        parse_warnings: vec![],
        failures_complete: false,
        ranking: Ranking {
            strategy: RankingStrategy::None,
            is_stable_across_runs: true,
        },
        counts: zero_test_counts(),
        suggested_next_commands: vec![Command {
            argv: vec!["cargo".to_owned(), "test".to_owned()],
            command: "cargo test".to_owned(),
            cwd: sidecar.cwd.clone(),
            intent: CommandIntent::RerunVerbose,
            interactive: false,
            mutates_workspace: false,
            uses_network: false,
            requires_approval: false,
        }],
        extensions: BTreeMap::new(),
        options: AnalyzeOptions::default(),
    })
}

fn command_uses_json_messages(command: &str) -> bool {
    // Walk argv tokens. Stop at `--`: anything after is passed to the test
    // binary and is NOT a cargo reporter flag, so `cargo test -- --message-format=json`
    // must NOT trigger JSON-reporter rejection.
    let mut tokens = command.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "--" {
            return false;
        }
        if token == "--message-format=json"
            || token.starts_with("--message-format=json,")
            || token.starts_with("--message-format=json-")
        {
            return true;
        }
        if token == "--message-format"
            && tokens.next().is_some_and(|next| {
                next == "json" || next.starts_with("json-") || next.starts_with("json,")
            })
        {
            return true;
        }
    }
    false
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

fn first_harness_start(lines: &[LogLine]) -> Option<usize> {
    lines
        .iter()
        .position(|line| running_tests_regex().is_match(&line.stripped))
}

fn parse_rustc_failures(
    lines: &[LogLine],
    path_case_folding: PathCaseFolding,
) -> Vec<CargoFailureBlock> {
    let mut failures = Vec::new();
    let mut index = 0;
    while let Some(start) = next_rustc_header(lines, index) {
        let end = next_rustc_block_end(lines, start + 1);
        if let Some(block) = parse_rustc_block(&lines[start..end], path_case_folding) {
            failures.push(block);
        }
        index = end;
    }
    failures
}

fn next_rustc_header(lines: &[LogLine], from: usize) -> Option<usize> {
    lines[from..]
        .iter()
        .position(|line| rustc_header_regex().is_match(&line.stripped))
        .map(|offset| from + offset)
}

fn next_rustc_block_end(lines: &[LogLine], from: usize) -> usize {
    for (index, line) in lines.iter().enumerate().skip(from) {
        if rustc_header_regex().is_match(&line.stripped)
            || line
                .stripped
                .starts_with("For more information about this error")
            || line.stripped.starts_with("error: could not compile")
            || line.stripped.starts_with("warning: build failed")
        {
            return index;
        }
    }
    lines.len()
}

fn parse_rustc_block(
    block_lines: &[LogLine],
    path_case_folding: PathCaseFolding,
) -> Option<CargoFailureBlock> {
    let first = block_lines.first()?;
    let captures = rustc_header_regex().captures(&first.stripped)?;
    let code = captures.name("code").map(|m| m.as_str().to_owned());
    let header_message = captures["message"].to_owned();
    let message = code.as_ref().map_or_else(
        || header_message.clone(),
        |code| format!("{code}: {header_message}"),
    );
    let spans = parse_rustc_spans(block_lines);
    let primary_span = spans
        .iter()
        .find(|span| span.role == LocationRole::Primary)
        .or_else(|| spans.first())?;
    let primary_location = location_from_span(primary_span, path_case_folding);
    let extra_locations = spans
        .iter()
        .filter(|span| span.role != LocationRole::Primary)
        .map(|span| location_from_span(span, path_case_folding))
        .collect::<Vec<_>>();
    let kind = rustc_kind(code.as_deref(), &message);
    let details = match kind {
        FailureKind::TypeError => Details::TypeError {
            spans: spans.clone(),
            explanation_command: code.as_ref().map(|code| format!("rustc --explain {code}")),
        },
        FailureKind::SyntaxError => Details::SyntaxError {
            spans: spans.clone(),
        },
        FailureKind::MissingSymbol => Details::MissingSymbol {
            symbol: missing_symbol_from_rustc(&message).unwrap_or_else(|| message.clone()),
            import_chain: vec![],
        },
        FailureKind::LinkerError => Details::LinkerError {
            unresolved_symbols: vec![],
        },
        _ => Details::Unknown {},
    };
    let last = block_lines.last()?;
    Some(CargoFailureBlock {
        phase: Phase::Build,
        kind,
        runner_test_id: None,
        test_name: None,
        primary_location,
        extra_locations,
        message,
        diagnostic_code: code,
        details,
        raw_log_lines: [first.number, last.number],
        byte_range: ByteRange {
            start: first.start,
            end: last.end,
        },
        excerpt_text: excerpt_text(block_lines),
        ansi_stripped: block_lines.iter().any(|line| line.raw != line.stripped),
    })
}

fn parse_rustc_spans(block_lines: &[LogLine]) -> Vec<Span> {
    let mut current_file = None::<String>;
    let mut current_line = None::<u64>;
    let mut spans = Vec::new();

    for line in block_lines {
        if let Some(captures) = rustc_location_regex().captures(&line.stripped) {
            current_file = Some(captures["file"].to_owned());
            current_line = captures["line"].parse().ok();
            continue;
        }
        if let Some(captures) = rustc_source_line_regex().captures(&line.stripped) {
            current_line = captures["line"].parse().ok();
            continue;
        }
        let Some(captures) = rustc_marker_regex().captures(&line.stripped) else {
            continue;
        };
        let Some(file) = current_file.clone() else {
            continue;
        };
        let Some(line_number) = current_line else {
            continue;
        };
        let marker = captures["marker"].to_owned();
        let prefix = captures["prefix"].to_owned();
        let column = prefix.chars().count() as u64 + 1;
        let width = marker.chars().count() as u64;
        let note = captures
            .name("note")
            .map(|m| m.as_str().trim().to_owned())
            .filter(|note| !note.is_empty());
        spans.push(Span {
            file,
            line: Some(line_number),
            column: Some(column),
            end_line: Some(line_number),
            end_column: Some(column + width),
            role: if marker.contains('^') {
                LocationRole::Primary
            } else {
                LocationRole::Related
            },
            note,
        });
    }
    spans
}

fn location_from_span(span: &Span, path_case_folding: PathCaseFolding) -> Location {
    Location {
        file: span.file.clone(),
        line: span.line,
        column: span.column,
        end_line: span.end_line,
        end_column: span.end_column,
        role: span.role,
        in_user_code: is_user_code_path(&span.file, UserCodeEcosystem::Rust, path_case_folding),
    }
}

// EXTRACT-CANDIDATE: phase/failure-kind dispatch has the same adapter-local policy shape across Rust test runners.
fn rustc_kind(code: Option<&str>, message: &str) -> FailureKind {
    if message.contains("linking with") || message.contains("undefined reference") {
        return FailureKind::LinkerError;
    }
    match code {
        Some("E0432") | Some("E0433") | Some("E0425") | Some("E0463") => FailureKind::MissingSymbol,
        Some("E0277") | Some("E0308") | Some("E0502") | Some("E0599") => FailureKind::TypeError,
        Some(_) if message.contains("expected") && message.contains("found") => {
            FailureKind::TypeError
        }
        Some(_) => FailureKind::TypeError,
        None if message.contains("expected") => FailureKind::SyntaxError,
        None => FailureKind::Unknown,
    }
}

fn missing_symbol_from_rustc(message: &str) -> Option<String> {
    message
        .split('`')
        .nth(1)
        .map(str::to_owned)
        .filter(|symbol| !symbol.is_empty())
}

fn parse_dependency_resolution_failure(lines: &[LogLine]) -> Option<CargoFailureBlock> {
    let start = lines.iter().position(|line| {
        line.stripped
            .starts_with("error: failed to select a version")
    })?;
    let block_lines = &lines[start..];
    let first = block_lines.first()?;
    let last = block_lines.last()?;
    let package = dependency_package_regex()
        .captures(&first.stripped)
        .map(|captures| captures["package"].to_owned());
    Some(CargoFailureBlock {
        phase: Phase::Build,
        kind: FailureKind::DependencyResolutionError,
        runner_test_id: None,
        test_name: None,
        primary_location: Location {
            file: "Cargo.toml".to_owned(),
            line: None,
            column: None,
            end_line: None,
            end_column: None,
            role: LocationRole::ImportSite,
            in_user_code: true,
        },
        extra_locations: vec![],
        message: first.stripped.trim_start_matches("error: ").to_owned(),
        diagnostic_code: None,
        details: Details::DependencyResolutionError {
            package,
            resolver: Resolver::Cargo,
        },
        raw_log_lines: [first.number, last.number],
        byte_range: ByteRange {
            start: first.start,
            end: last.end,
        },
        excerpt_text: excerpt_text(block_lines),
        ansi_stripped: block_lines.iter().any(|line| line.raw != line.stripped),
    })
}

fn parse_build_script_failure(
    lines: &[LogLine],
    path_case_folding: PathCaseFolding,
) -> Option<CargoFailureBlock> {
    let start = lines.iter().position(|line| {
        line.stripped
            .starts_with("error: failed to run custom build command")
    })?;
    let block_lines = &lines[start..];
    let first = block_lines.first()?;
    let last = block_lines.last()?;
    // Build scripts can fail without panicking — `cc` link failures, `pkg-config`
    // missing, subprocess exit nonzero, etc. Only classify as `kind: Panic` when
    // an actual panic line is present; otherwise return None so the dispatcher
    // falls through to `parse_unknown_build_failure` and routes to `kind: Unknown`
    // per the locked decision 1a (non-rustc/non-resolver/non-linker/non-panic
    // residual cargo build errors → kind: unknown + evidence note).
    let panic_index = block_lines
        .iter()
        .position(|line| panic_location_regex().is_match(&line.stripped))?;
    let primary_location = panic_location_regex()
        .captures(&block_lines[panic_index].stripped)
        .map(|captures| location_from_panic_captures(&captures, path_case_folding))
        .unwrap_or_else(|| Location {
            file: "build.rs".to_owned(),
            line: None,
            column: None,
            end_line: None,
            end_column: None,
            role: LocationRole::PanicSite,
            in_user_code: true,
        });
    let panic_message = block_lines
        .get(panic_index + 1)
        .map(|line| line.stripped.trim().to_owned())
        .filter(|line| !line.is_empty() && !line.starts_with("note:"))
        .unwrap_or_else(|| "build script panicked".to_owned());
    Some(CargoFailureBlock {
        phase: Phase::Build,
        kind: FailureKind::Panic,
        runner_test_id: None,
        test_name: None,
        primary_location: primary_location.clone(),
        extra_locations: vec![],
        message: format!("build script panic: {panic_message}"),
        diagnostic_code: None,
        details: Details::Panic {
            panic_message,
            stack: vec![frame_from_location(&primary_location)],
            goroutine_id: None,
        },
        raw_log_lines: [first.number, last.number],
        byte_range: ByteRange {
            start: first.start,
            end: last.end,
        },
        excerpt_text: excerpt_text(block_lines),
        ansi_stripped: block_lines.iter().any(|line| line.raw != line.stripped),
    })
}

fn parse_unknown_build_failure(lines: &[LogLine]) -> Option<CargoFailureBlock> {
    let start = lines
        .iter()
        .position(|line| line.stripped.starts_with("error:"))?;
    let block_lines = &lines[start..];
    let first = block_lines.first()?;
    let last = block_lines.last()?;
    Some(CargoFailureBlock {
        phase: Phase::Build,
        kind: FailureKind::Unknown,
        runner_test_id: None,
        test_name: None,
        primary_location: Location {
            file: "Cargo.toml".to_owned(),
            line: None,
            column: None,
            end_line: None,
            end_column: None,
            role: LocationRole::Primary,
            in_user_code: true,
        },
        extra_locations: vec![],
        message: first.stripped.trim_start_matches("error: ").to_owned(),
        diagnostic_code: None,
        details: Details::Unknown {},
        raw_log_lines: [first.number, last.number],
        byte_range: ByteRange {
            start: first.start,
            end: last.end,
        },
        excerpt_text: excerpt_text(block_lines),
        ansi_stripped: block_lines.iter().any(|line| line.raw != line.stripped),
    })
}

fn parse_test_failures(
    lines: &[LogLine],
    path_case_folding: PathCaseFolding,
) -> Vec<CargoFailureBlock> {
    let mut failures = Vec::new();
    let mut index = 0;
    while let Some(start) = next_failure_block(lines, index) {
        let end = next_failure_block_end(lines, start + 1);
        if let Some(block) = parse_test_block(&lines[start..end], path_case_folding) {
            failures.push(block);
        }
        index = end;
    }
    failures
}

fn next_failure_block(lines: &[LogLine], from: usize) -> Option<usize> {
    lines[from..]
        .iter()
        .position(|line| libtest_block_header_regex().is_match(&line.stripped))
        .map(|offset| from + offset)
}

fn next_failure_block_end(lines: &[LogLine], from: usize) -> usize {
    for (index, line) in lines.iter().enumerate().skip(from) {
        if libtest_block_header_regex().is_match(&line.stripped)
            || (line.stripped == "failures:" && index > from)
            || test_result_regex().is_match(&line.stripped)
        {
            return index;
        }
    }
    lines.len()
}

fn parse_test_block(
    block_lines: &[LogLine],
    path_case_folding: PathCaseFolding,
) -> Option<CargoFailureBlock> {
    let first = block_lines.first()?;
    let captures = libtest_block_header_regex().captures(&first.stripped)?;
    let runner_test_id = captures["name"].to_owned();
    let panic_line = block_lines
        .iter()
        .find(|line| panic_location_regex().is_match(&line.stripped));
    let primary_location = panic_line
        .and_then(|line| panic_location_regex().captures(&line.stripped))
        .map(|captures| location_from_panic_captures(&captures, path_case_folding))
        .unwrap_or_else(|| Location {
            file: runner_test_id.clone(),
            line: None,
            column: None,
            end_line: None,
            end_column: None,
            role: LocationRole::Primary,
            in_user_code: true,
        });
    let panic_message = panic_line
        .and_then(|line| line_after(block_lines, line.number + 1))
        .or_else(|| first_non_empty_after_header(block_lines))
        .unwrap_or_else(|| "test failed".to_owned());
    let kind = classify_test_failure(&panic_message, block_lines);
    let phase = Phase::Test;
    let details = match kind {
        FailureKind::Assertion => Details::Assertion {
            expected: assertion_value(block_lines, "right:")
                .or_else(|| expected_substring(block_lines)),
            actual: assertion_value(block_lines, "left:")
                .or_else(|| panic_message_value(block_lines)),
            diff: None,
        },
        FailureKind::Panic => Details::Panic {
            panic_message: panic_message.clone(),
            stack: vec![frame_from_location(&primary_location)],
            goroutine_id: None,
        },
        FailureKind::Unknown => Details::Unknown {},
        _ => Details::Exception {
            exception_type: Some("panic".to_owned()),
            stack: vec![frame_from_location(&primary_location)],
        },
    };
    let last = block_lines.last()?;
    Some(CargoFailureBlock {
        phase,
        kind,
        runner_test_id: Some(runner_test_id.clone()),
        test_name: Some(runner_test_id),
        primary_location,
        extra_locations: vec![],
        message: panic_message,
        diagnostic_code: None,
        details,
        raw_log_lines: [first.number, last.number],
        byte_range: ByteRange {
            start: first.start,
            end: last.end,
        },
        excerpt_text: excerpt_text(block_lines),
        ansi_stripped: block_lines.iter().any(|line| line.raw != line.stripped),
    })
}

fn classify_test_failure(message: &str, block_lines: &[LogLine]) -> FailureKind {
    let lower = message.to_ascii_lowercase();
    if lower.contains("assertion")
        || block_lines.iter().any(|line| {
            line.stripped
                .contains("panic did not contain expected string")
        })
    {
        FailureKind::Assertion
    } else if lower.contains("panicked") {
        FailureKind::Panic
    } else if block_lines
        .iter()
        .any(|line| rustc_header_regex().is_match(&line.stripped))
    {
        // Doctest compile errors (rustc diagnostics inside the `Doc-tests`
        // block) reach this classifier with `message` being the first rustc
        // line. Route to Unknown per locked decision 1a; v1.5 schema work can
        // promote these to TypeError/SyntaxError/MissingSymbol once we extract
        // rustc spans from inside test segments.
        FailureKind::Unknown
    } else if !message.is_empty() {
        FailureKind::Panic
    } else {
        FailureKind::Exception
    }
}

fn parse_test_summaries(lines: &[LogLine]) -> Vec<TestSummary> {
    lines
        .iter()
        .filter_map(|line| {
            test_result_regex()
                .captures(&line.stripped)
                .map(|captures| TestSummary {
                    passed: captures["passed"].parse().unwrap_or(0),
                    failed: captures["failed"].parse().unwrap_or(0),
                    ignored: captures["ignored"].parse().unwrap_or(0),
                    measured: captures["measured"].parse().unwrap_or(0),
                    filtered_out: captures["filtered"].parse().unwrap_or(0),
                })
        })
        .collect()
}

fn location_from_panic_captures(
    captures: &regex::Captures<'_>,
    path_case_folding: PathCaseFolding,
) -> Location {
    Location {
        file: captures["file"].to_owned(),
        line: captures["line"].parse().ok(),
        column: captures["column"].parse().ok(),
        end_line: None,
        end_column: None,
        role: LocationRole::PanicSite,
        in_user_code: is_user_code_path(
            &captures["file"],
            UserCodeEcosystem::Rust,
            path_case_folding,
        ),
    }
}

fn line_after(block_lines: &[LogLine], number: u64) -> Option<String> {
    block_lines
        .iter()
        .find(|line| line.number == number)
        .map(|line| line.stripped.trim().to_owned())
        .filter(|line| !line.is_empty() && !line.starts_with("note:"))
}

fn first_non_empty_after_header(block_lines: &[LogLine]) -> Option<String> {
    block_lines
        .iter()
        .skip(1)
        .map(|line| line.stripped.trim())
        .find(|line| !line.is_empty() && !line.starts_with("thread "))
        .map(str::to_owned)
}

fn assertion_value(block_lines: &[LogLine], prefix: &str) -> Option<String> {
    block_lines
        .iter()
        .map(|line| line.stripped.trim())
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
        .map(str::to_owned)
}

fn panic_message_value(block_lines: &[LogLine]) -> Option<String> {
    block_lines
        .iter()
        .map(|line| line.stripped.trim())
        .find_map(|line| line.strip_prefix("panic message:").map(str::trim))
        .map(str::to_owned)
}

fn expected_substring(block_lines: &[LogLine]) -> Option<String> {
    block_lines
        .iter()
        .map(|line| line.stripped.trim())
        .find_map(|line| line.strip_prefix("expected substring:").map(str::trim))
        .map(str::to_owned)
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

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_+-./:=@".contains(&byte))
    {
        return arg.to_owned();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn cargo_status_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    // EXTRACT-CANDIDATE: OnceLock<Regex> accessor wrappers are duplicated across adapters.
    REGEX.get_or_init(|| {
        Regex::new(r"^\s+(?:Compiling|Checking|Finished|Running|Doc-tests)\b").unwrap()
    })
}

fn running_tests_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^running \d+ tests?$").unwrap())
}

fn rustc_header_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^error(?:\[(?<code>E\d{4})\])?: (?<message>.+)$").unwrap())
}

fn is_rustc_diagnostic_header(line: &str) -> bool {
    rustc_header_regex().is_match(line)
        && !line.starts_with("error: could not compile")
        && !line.starts_with("error: test failed")
        && !line.starts_with("error: doctest failed")
        && !line.starts_with("error: failed to select a version")
        && !line.starts_with("error: failed to run custom build command")
}

fn cargo_compile_error_count_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"^error: could not compile .+ due to (?<count>\d+) previous errors?$").unwrap()
    })
}

fn rustc_location_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^\s*-->\s+(?<file>.+):(?<line>\d+):(?<column>\d+)$").unwrap())
}

fn rustc_source_line_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^\s*(?<line>\d+)\s+\|").unwrap())
}

fn rustc_marker_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"^\s*\|\s(?<prefix>\s*(?:-+\s+)?)(?<marker>[\^-]+)(?:\s+(?<note>.+))?$")
            .unwrap()
    })
}

fn dependency_package_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"requirement `(?<package>[^ =`]+)").unwrap())
}

fn libtest_block_header_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^---- (?<name>.+) stdout ----$").unwrap())
}

fn panic_location_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"^\s*thread '[^']+'(?: \(\d+\))? panicked at (?<file>.+):(?<line>\d+):(?<column>\d+):$",
        )
        .unwrap()
    })
}

fn test_result_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"^test result: \w+\. (?<passed>\d+) passed; (?<failed>\d+) failed; (?<ignored>\d+) ignored; (?<measured>\d+) measured; (?<filtered>\d+) filtered out; finished in .+$",
        )
        .unwrap()
    })
}

fn rerun_pass_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"to rerun pass `(?<tail>[^`]+)`").unwrap())
}

fn failed_target_terminal_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^error: (?:test|doctest|bench) failed(?:,|$)").unwrap())
}

fn aggregate_failed_targets_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^error: (?<count>\d+) targets? failed(?:[,:.]|$)").unwrap())
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
    use crate::schema::Adapter as SchemaAdapter;

    const RUNTIME_ASSERTION: &[u8] =
        include_bytes!("../../tests/fixtures/cargo/runtime_assertion/run/log");
    const GREEN: &[u8] = include_bytes!("../../tests/fixtures/cargo/green_passing/run/log");
    const BORROW_CHECKER: &[u8] =
        include_bytes!("../../tests/fixtures/cargo/borrow_checker_e0502/run/log");

    fn make_sidecar(command: &str, exit_code: Option<i32>, signal: Option<&str>) -> Sidecar {
        Sidecar {
            meta_schema_version: "1.2".to_owned(),
            schema_version: "1.6".to_owned(),
            failtrace_version: "0.1.0".to_owned(),
            capture_build: None,
            adapters: vec![SchemaAdapter {
                id: ADAPTER_ID.to_owned(),
                version: format!("{ADAPTER_ID}@0.1.0"),
                kind: crate::schema::AdapterKind::Runner,
                selected: true,
                selection_reason: crate::schema::SelectionReason::ArgvNameMatch,
            }],
            adapter_candidates: vec![],
            argv: command.split_whitespace().map(str::to_owned).collect(),
            command: command.to_owned(),
            cwd: String::new(),
            started_at: String::new(),
            duration_ms: 0,
            exit_code,
            signal: signal.map(str::to_owned),
            early_exit: None,
            truncated: false,
            truncation_reason: None,
            path_case_folding: crate::storage::PathCaseFolding::CaseSensitive,
            stream_merge: crate::storage::StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
        }
    }

    #[test]
    fn parses_runtime_assertion_block_and_summary() {
        let parsed = CargoParse::from_log(RUNTIME_ASSERTION);
        assert!(parsed.has_harness_start);
        assert!(!parsed.test_failures.is_empty());
        assert_eq!(parsed.test_failures[0].kind, FailureKind::Assertion);
        assert!(parsed.summaries.iter().any(|s| s.failed > 0));
        assert!(parsed.parse_warnings.is_empty());
    }

    #[test]
    fn green_run_parses_summary_without_failures() {
        let parsed = CargoParse::from_log(GREEN);
        let sidecar = make_sidecar("cargo test", Some(0), None);
        assert!(parsed.looks_like_cargo_test(&sidecar));
        assert!(parsed.test_failures.is_empty());
        assert!(parsed.build_failures.is_empty());
        assert!(parsed.summaries.iter().any(|s| s.total() > 0));
    }

    #[test]
    fn count_only_log_is_rejected_at_admission() {
        let parsed = CargoParse::from_log(b"test result: ok. 2 passed; 0 failed; 0 ignored\n");
        let sidecar = make_sidecar("cargo test", Some(0), None);
        assert!(!parsed.looks_like_cargo_test(&sidecar));
    }

    #[test]
    fn sidecar_pinned_admission_allows_bare_rustc_error() {
        let parsed = CargoParse::from_log(BORROW_CHECKER);
        let pinned = make_sidecar("cargo test", Some(101), None);
        assert!(parsed.looks_like_cargo_test(&pinned));
        let mut unpinned = pinned.clone();
        unpinned.adapters.clear();
        assert!(parsed.has_rustc_header_before_harness);
        assert!(!parsed.looks_like_cargo_test(&unpinned) || parsed.has_cargo_signature);
    }

    #[test]
    fn harness_crashed_without_summary_warns_and_marks_incomplete() {
        let log: &[u8] = b"\
running 1 test
test it_panics ... FAILED

failures:

---- it_panics stdout ----
thread 'it_panics' panicked at src/lib.rs:5:9:
boom
";
        let sidecar = make_sidecar("cargo test", None, Some("SIGKILL"));
        let parsed = CargoParse::from_log(log);
        let document = lower(parsed, log, &sidecar, AnalyzeOptions::default());
        assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
        assert!(
            document
                .parse_warnings
                .contains(&ParseWarning::MissingSummaryLine)
        );
        assert!(!document.failures_complete);
    }

    #[test]
    fn cargo_version_policy_is_deferred_no_op() {
        // v1.4: cargo emits no stable runner-version banner. The stub returns true
        // unconditionally; validated-family enforcement is blocked on the v1.5
        // structured sidecar.runner_version slice.
        assert!(CargoVersion.has_validated_output_family());
    }

    #[test]
    fn rerun_tail_sources_from_log_not_sidecar_command() {
        // Regression: rerun_tail used to read sidecar.command for the `to rerun
        // pass` directive, but that text is only emitted in the raw log. The fix
        // parses log lines and matches positionally against failure raw_log_lines.
        let log = b"\
running 1 test
test tests::it_fails ... FAILED

failures:

---- tests::it_fails stdout ----
thread 'tests::it_fails' panicked at crates/beta/src/lib.rs:9:9:
assertion failed: super::beta()

failures:
    tests::it_fails

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

error: test failed, to rerun pass `-p beta --lib`
";
        let sidecar = make_sidecar("cargo test --workspace", Some(101), None);
        let parsed = CargoParse::from_log(log);
        assert_eq!(parsed.rerun_pass_tails.len(), 1);
        assert_eq!(parsed.rerun_pass_tails[0].tail, "-p beta --lib");
        let document = lower(parsed, log, &sidecar, AnalyzeOptions::default());
        let suggested = &document.suggested_next_commands[0];
        assert_eq!(
            suggested.command,
            "cargo test -p beta --lib tests::it_fails"
        );
        assert_eq!(
            suggested.argv,
            vec!["cargo", "test", "-p", "beta", "--lib", "tests::it_fails"]
        );
    }

    #[test]
    fn build_script_non_panic_failure_falls_through_to_unknown() {
        // Regression: parse_build_script_failure used to set kind:Panic and
        // fabricate "build script panicked" even when no panic line was present.
        // Real cc/pkg-config/bindgen failures must NOT be classified as panics.
        let log = b"\
   Compiling foo v0.1.0 (/tmp/foo)
error: failed to run custom build command for `foo v0.1.0 (/tmp/foo)`

Caused by:
  process didn't exit successfully: `/tmp/foo/target/debug/build/foo-abc/build-script-build` (exit status: 1)
  --- stdout
  cargo:rerun-if-changed=build.rs

  --- stderr
  pkg-config: command not found
";
        let parsed = CargoParse::from_log(log);
        assert_eq!(parsed.build_failures.len(), 1);
        assert_eq!(parsed.build_failures[0].kind, FailureKind::Unknown);
        assert_eq!(parsed.build_failures[0].phase, Phase::Build);
    }

    #[test]
    fn harness_crashed_after_prior_summary_reports_missing_later_summary() {
        // Regression: workspace runs that complete one binary's summary then get
        // signal-killed during the next binary leave parsed.summaries non-empty.
        // Each `running N tests` starts a separate harness, so the missing second
        // summary must remain explicit even though the first harness completed.
        let log = b"\
running 1 test
test alpha::ok ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

running 2 tests
";
        let sidecar = make_sidecar("cargo test --workspace", None, Some("SIGKILL"));
        let parsed = CargoParse::from_log(log);
        assert!(!parsed.summaries.is_empty());
        let document = lower(parsed, log, &sidecar, AnalyzeOptions::default());
        assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
        assert!(!document.failures_complete);
        assert!(
            document
                .parse_warnings
                .contains(&ParseWarning::MissingSummaryLine),
            "the started second harness has no terminal summary"
        );
    }

    #[test]
    fn doctest_compile_error_classifies_as_unknown_not_panic() {
        // Regression: classify_test_failure used to fall through to Panic for any
        // non-empty message that didn't say "assertion" or "panicked". A rustc
        // compile error inside a doctest block reached that fallthrough and was
        // mis-classified. The fix routes rustc-shaped messages to Unknown.
        let log = b"\
running 1 test
test src/lib.rs - foo (line 5) ... FAILED

failures:

---- src/lib.rs - foo (line 5) stdout ----
error[E0277]: the trait bound `i32: PartialEq<&str>` is not satisfied
 --> src/lib.rs:7:9
  |
7 |     assert_eq!(foo(), \"bar\");
  |     ^^^^^^^^^^^^^^^^^^^^^^^^

failures:
    src/lib.rs - foo (line 5)

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

error: doctest failed, to rerun pass `--doc`
";
        let parsed = CargoParse::from_log(log);
        assert_eq!(parsed.test_failures.len(), 1);
        assert_eq!(parsed.test_failures[0].kind, FailureKind::Unknown);
        assert_ne!(
            parsed.test_failures[0].kind,
            FailureKind::Panic,
            "doctest compile errors must not be classified as runtime panics"
        );
    }

    #[test]
    fn json_message_format_respects_double_dash_boundary() {
        // Regression: command_uses_json_messages was a substring match and
        // falsely rejected `cargo test -- --message-format=json` even though
        // post-`--` flags are arguments to the test binary, not cargo flags.
        assert!(command_uses_json_messages(
            "cargo test --message-format=json"
        ));
        assert!(command_uses_json_messages(
            "cargo test --message-format json"
        ));
        assert!(command_uses_json_messages(
            "cargo test --message-format=json-render-diagnostics"
        ));
        assert!(
            !command_uses_json_messages("cargo test -- --message-format=json"),
            "post-`--` --message-format is test-binary arg, not cargo reporter flag"
        );
        assert!(!command_uses_json_messages(
            "cargo test --message-format=human"
        ));
    }

    #[test]
    fn has_single_thread_argv_accepts_space_and_equals_forms() {
        // Regression: has_single_thread_argv only matched `=` form. Libtest
        // accepts the space-separated form too; pessimistically marking ranking
        // unstable for users who type `-- --test-threads 1` was wrong.
        assert!(has_single_thread_argv("cargo test -- --test-threads=1"));
        assert!(has_single_thread_argv("cargo test -- --test-threads 1"));
        assert!(!has_single_thread_argv("cargo test -- --test-threads=2"));
        assert!(!has_single_thread_argv("cargo test -- --test-threads 4"));
        assert!(
            !has_single_thread_argv("cargo test --test-threads=1"),
            "pre-`--` --test-threads is not a libtest arg; cargo would reject"
        );
    }
}
