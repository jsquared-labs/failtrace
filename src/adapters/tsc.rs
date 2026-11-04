use std::{collections::BTreeMap, path::Path, sync::OnceLock};

use regex::Regex;
use serde_json::json;

use crate::{
    adapters::{
        AdapterError, AdapterImpl, AnalyzeOptions,
        shared::{
            AnalysisDocumentParts, UserCodeEcosystem, build_analysis_document, is_user_code_path,
            parse_status_from_warnings, sidecar_command_tokens, sidecar_selected_adapter,
            truncate_for_budget,
        },
    },
    schema::{
        ByteRange, Command, CommandIntent, Counts, Details, Diagnostic, DiagnosticSeverity,
        Evidence, Excerpt, ExcerptKind, ExcerptSource, Failure, FailureKind, Location,
        LocationRole, OutputDocument, ParseWarning, Phase, Ranking, RankingStrategy, Span,
        derive_failure_id, validate,
    },
    storage::Sidecar,
};

const ADAPTER_ID: &str = "tsc";

// v1.6 backlog (tsc-specific):
//   - tsc_cross_file_chain with a real two-project fixture
//   - tsc_intra_diagnostic_chain via parsed relatedInformation
//     (companion attachment is currently empty — see TscParse::from_log)
//   - structured sidecar.runner_version for TypeScript version-family validation
//
// v1.6 backlog, cross-adapter (affects all adapters; tsc is just the one
// that exposed them):
//   - Location.in_user_code: Option<bool> field — docs/schema.md says the
//     denylist for tsc is `node_modules`, but Location/Span carry no
//     in_user_code field today (only Frame does). All sibling adapters
//     emit Locations under their respective denylist paths (site-packages,
//     node_modules, target, etc.) without being able to set the flag.
//   - Multi-adapter run_command + SelectionReason::CompositeCommandComponent
//     emission — the schema documents the `tsc && jest` composite shape at
//     docs/schema.md, but src/cli.rs::run_command is a single-adapter
//     if/else cascade. Nothing implements composite-command
//     selection; tsc is the first adapter where it's a routine pattern.
//   - Sidecar.argv: Vec<String> structured field — sidecar.command is a
//     joined string today. The global-diagnostic location fallback
//     parses it with split_whitespace, which is incorrect for paths with
//     shell-quoted spaces. Adding structured argv to the sidecar is the
//     proper fix and would benefit all adapters that introspect argv.

pub fn analyze_log(
    log: &[u8],
    sidecar: &Sidecar,
    options: AnalyzeOptions,
) -> Result<OutputDocument, AdapterError> {
    TscAdapter::analyze_log(log, sidecar, options)
}

struct TscAdapter;

impl AdapterImpl for TscAdapter {
    fn analyze_log(
        log: &[u8],
        sidecar: &Sidecar,
        options: AnalyzeOptions,
    ) -> Result<OutputDocument, AdapterError> {
        if !sidecar_selected_adapter(sidecar, ADAPTER_ID) {
            return Err(AdapterError::InvalidSidecar {
                reason: "sidecar does not select tsc adapter".to_owned(),
            });
        }
        let command = sidecar_command_tokens(sidecar);
        if crate::cli::tsc_command_args(&command).is_some_and(|args| {
            args.iter()
                .any(|arg| arg == "--watch" || arg == "-w" || arg.starts_with('@'))
        }) {
            return Err(AdapterError::UnrecognizedFormat {
                reason: "tsc watch mode and response files are unsupported".to_owned(),
            });
        }

        let parsed = TscParse::from_log(log, sidecar);
        if !parsed.looks_like_tsc(sidecar) {
            return Err(AdapterError::UnrecognizedFormat {
                reason: "no tsc diagnostic or summary signature found".to_owned(),
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
#[allow(dead_code)] // Placeholder for v1.5 sidecar.runner_version validation.
struct TscVersion;

#[allow(dead_code)]
impl TscVersion {
    // EXTRACT-CANDIDATE: adapter version family validation is shared policy with runner-specific validated majors.
    // v1.4 sidecars do not carry structured runner_version. TypeScript
    // version-family validation is deferred to the v1.5 sidecar.runner_version slice.
    fn has_validated_output_family(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TscParse {
    failures: Vec<TscFailureBlock>,
    parse_warnings: Vec<ParseWarning>,
    has_tsc_signature: bool,
    reported_error_count: Option<u64>,
    extracted_error_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TscFailureBlock {
    kind: FailureKind,
    primary_location: Location,
    message: String,
    diagnostic_code: Option<String>,
    details: Details,
    raw_log_lines: [u64; 2],
    byte_range: ByteRange,
    excerpt_text: String,
    ansi_stripped: bool,
    companion_diagnostics: Vec<Diagnostic>,
    parse_warnings: Vec<ParseWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TscDiagnostic {
    severity: TscSeverity,
    code: Option<String>,
    message: String,
    location: Option<Location>,
    line_number: u64,
    byte_range: ByteRange,
    raw_text: String,
    ansi_stripped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TscSeverity {
    Error,
    Warning,
    Suggestion,
    Message,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogLine {
    number: u64,
    start: u64,
    end: u64,
    raw: String,
    stripped: String,
}

impl TscParse {
    fn from_log(log: &[u8], sidecar: &Sidecar) -> Self {
        let lines = split_lines(log);
        let diagnostics = parse_diagnostics(&lines, sidecar);
        let has_tsc_signature = lines.iter().any(|line| {
            tsc_signature_regex().is_match(&line.stripped)
                || tsc_found_errors_regex().is_match(&line.stripped)
        });
        let reported_error_counts = lines
            .iter()
            .filter_map(|line| {
                tsc_found_errors_regex()
                    .captures(&line.stripped)
                    .and_then(|captures| captures["count"].parse().ok())
            })
            .collect::<Vec<_>>();
        let reported_error_count = (!reported_error_counts.is_empty()).then(|| {
            reported_error_counts
                .iter()
                .copied()
                .fold(0_u64, u64::saturating_add)
        });
        let mut parse_warnings = Vec::new();
        let mut failures = Vec::new();

        for diagnostic in diagnostics.iter() {
            if diagnostic.severity != TscSeverity::Error {
                // Locked severity rule: only `error` becomes a failures[] entry.
                // Standalone non-errors (warning/suggestion/message) are dropped.
                // Companion attachment via adjacency inference is forbidden — tsc
                // chains diagnostics structurally (relatedInformation in --pretty
                // mode), and parsing that is v1.5 work (tsc_cross_file_chain +
                // tsc_intra_diagnostic_chain). Until that exists, companions stay
                // empty rather than risk false-positive attachment.
                continue;
            }
            let block = failure_from_diagnostic(diagnostic, vec![]);
            push_unique_warnings(&mut parse_warnings, &block.parse_warnings);
            failures.push(block);
        }
        let extracted_error_count = failures.len() as u64;

        // G3-locked: when sidecar pins tsc and exit was non-zero but no diagnostic
        // parsed (empty log, signal kill, or runner output we couldn't recognize),
        // emit exactly one Unknown failure so typecheck_failed never has empty
        // failures[]. Does NOT require has_tsc_signature — the sidecar pin is the
        // authoritative "this was tsc" signal; the log-signature heuristic is only
        // a fallback for cases where the sidecar didn't pin (currently unreachable
        // because analysis dispatches by sidecar pin).
        let sidecar_pins_tsc = sidecar
            .adapters
            .iter()
            .any(|adapter| adapter.id == ADAPTER_ID && adapter.selected);
        if failures.is_empty() && sidecar.exit_code != Some(0) && sidecar_pins_tsc {
            let block = unknown_failure_block(&lines, sidecar);
            push_unique_warnings(&mut parse_warnings, &block.parse_warnings);
            failures.push(block);
        }

        Self {
            failures,
            parse_warnings,
            has_tsc_signature,
            reported_error_count,
            extracted_error_count,
        }
    }

    fn looks_like_tsc(&self, sidecar: &Sidecar) -> bool {
        // EXTRACT-CANDIDATE: runner recognition gates now share a multi-signal shape with adapter-specific signatures.
        // Sidecar pin is the admission signal. Two paths produce a pinned sidecar:
        // (1) `failtrace run` writes the pin after dispatching to the tsc
        // detector, so the pin is failtrace-authored and reliable; (2) `failtrace
        // analyze <run-dir>` reads a user-supplied meta.json, so the pin is
        // user-supplied — a hand-edited sidecar can attribute arbitrary log
        // bytes to tsc. failtrace is a developer tool, not a trust boundary,
        // so the analyze-time forgeability is acceptable: misclassified output
        // produces a malformed-looking document, not a security failure.
        // Sibling adapters (pytest/vitest/jest/cargo) keep additional log-
        // signature gates as defense in depth; tsc deliberately doesn't,
        // because empty / signature-less logs are a real tsc case and a
        // signature gate would reject them.
        sidecar
            .adapters
            .iter()
            .any(|adapter| adapter.id == ADAPTER_ID && adapter.selected)
    }
}

fn lower(
    parsed: TscParse,
    log: &[u8],
    sidecar: &Sidecar,
    options: AnalyzeOptions,
) -> OutputDocument {
    let all_failures = parsed
        .failures
        .iter()
        .map(|block| failure_from_block(block, &sidecar.cwd))
        .collect::<Vec<_>>();
    let counts = counts(&all_failures);
    let budgeted_failures = truncate_for_budget(all_failures, options);
    let suggested_next_commands = suggested_next_commands(sidecar, &budgeted_failures.failures);

    let mut parse_warnings = parsed.parse_warnings;
    let failure_list_untrusted = parsed
        .reported_error_count
        .is_some_and(|reported| reported > parsed.extracted_error_count);
    if failure_list_untrusted && !parse_warnings.contains(&ParseWarning::RunnerSummaryMismatch) {
        parse_warnings.push(ParseWarning::RunnerSummaryMismatch);
    }
    if sidecar.truncated && !parse_warnings.contains(&ParseWarning::TruncatedTraceback) {
        parse_warnings.push(ParseWarning::TruncatedTraceback);
    }
    let interrupted = sidecar.signal.is_some();
    if interrupted && !parse_warnings.contains(&ParseWarning::MissingTerminalEvent) {
        parse_warnings.push(ParseWarning::MissingTerminalEvent);
    }
    // EXTRACT-CANDIDATE OMITTED: this site does NOT use
    // shared::summary_parse_result because tsc derives
    // parse_status from diagnostic recoverability and global-location fallbacks
    // rather than from test-run summary completeness. Sibling adapters key the
    // decision on summary-line presence; that signal does not exist for tsc.
    let parse_status = parse_status_from_warnings(&parse_warnings);

    let runner_outcome = {
        // EXTRACT-CANDIDATE OMITTED: this site does NOT use
        // shared::summary_runner_outcome because tsc has no
        // "tests ran + summary present" branch — the sibling decision table
        // collapses to a 2-row gate (passed vs typecheck_failed) because tsc
        // never runs tests; runner_outcome=failed and timed_out / no_tests /
        // harness_crashed paths do not apply.
        if sidecar.exit_code == Some(0) && budgeted_failures.failures_total == 0 {
            crate::schema::RunnerOutcome::Passed
        } else {
            crate::schema::RunnerOutcome::TypecheckFailed
        }
    };
    let ranking = Ranking {
        strategy: if budgeted_failures.failures.is_empty() {
            RankingStrategy::None
        } else {
            RankingStrategy::CompileBeforeRuntime
        },
        is_stable_across_runs: true,
    };

    build_analysis_document(AnalysisDocumentParts {
        sidecar,
        log,
        budgeted_failures,
        runner_outcome,
        early_exit: None,
        parse_status,
        parse_warnings,
        failures_complete: !sidecar.truncated && !failure_list_untrusted && !interrupted,
        ranking,
        counts,
        suggested_next_commands,
        extensions: BTreeMap::new(),
        options,
    })
}

fn failure_from_block(block: &TscFailureBlock, cwd: &str) -> Failure {
    let mut failure = Failure {
        failure_id: String::new(),
        adapter_id: ADAPTER_ID.to_owned(),
        phase: Phase::Typecheck,
        kind: block.kind,
        runner_package_id: None,
        runner_test_id: None,
        test_segments: vec![],
        test_name: None,
        param_id: None,
        test_group_id: None,
        primary_location: block.primary_location.clone(),
        extra_locations: vec![],
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
        companion_diagnostics: block.companion_diagnostics.clone(),
        extensions: tsc_extensions(),
    };
    failure.failure_id = derive_failure_id(&failure, cwd);
    failure
}

fn counts(failures: &[Failure]) -> Counts {
    Counts {
        tests_run: 0,
        tests_passed: 0,
        tests_failed: 0,
        tests_errored: 0,
        tests_skipped: 0,
        tests_not_run: None,
        // For tsc, files_failed means unique primary diagnostic files, not test files.
        files_failed: Some(
            failures
                .iter()
                .map(|failure| failure.primary_location.file.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len() as u64,
        )
        .filter(|count| *count > 0),
    }
}

fn suggested_next_commands(sidecar: &Sidecar, failures: &[Failure]) -> Vec<Command> {
    failures
        .iter()
        .map(|failure| Command {
            argv: vec![
                "failtrace".to_owned(),
                "explain".to_owned(),
                failure.failure_id.clone(),
            ],
            command: format!("failtrace explain {}", failure.failure_id),
            cwd: sidecar.cwd.clone(),
            intent: CommandIntent::ExplainDiagnostic,
            interactive: false,
            mutates_workspace: false,
            uses_network: false,
            requires_approval: false,
        })
        .collect()
}

fn tsc_extensions() -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([(
        "tsc".to_owned(),
        json!({
            "column_unit": "utf16_code_unit",
            "phase_detail": "typecheck",
        }),
    )])
}

fn parse_diagnostics(lines: &[LogLine], sidecar: &Sidecar) -> Vec<TscDiagnostic> {
    lines
        .iter()
        .filter_map(|line| parse_diagnostic_line(line, sidecar))
        .collect()
}

fn parse_diagnostic_line(line: &LogLine, sidecar: &Sidecar) -> Option<TscDiagnostic> {
    let stripped = line.stripped.trim_end();
    if let Some(captures) = listed_diagnostic_regex().captures(stripped) {
        return Some(diagnostic_from_captures(
            line,
            &captures,
            Some(location_from_captures(&captures, sidecar.path_case_folding)),
        ));
    }
    if let Some(captures) = pretty_diagnostic_regex().captures(stripped) {
        return Some(diagnostic_from_captures(
            line,
            &captures,
            Some(location_from_captures(&captures, sidecar.path_case_folding)),
        ));
    }
    if let Some(captures) = global_diagnostic_regex().captures(stripped) {
        return Some(diagnostic_from_captures(
            line,
            &captures,
            Some(global_fallback_location(sidecar)),
        ));
    }
    None
}

fn diagnostic_from_captures(
    line: &LogLine,
    captures: &regex::Captures<'_>,
    location: Option<Location>,
) -> TscDiagnostic {
    let severity = tsc_severity(&captures["severity"]);
    let code = captures.name("code").map(|m| format!("TS{}", m.as_str()));
    let mut message = captures["message"].to_owned();
    if let Some(code) = code.as_deref() {
        message = format!("{code}: {message}");
    }
    TscDiagnostic {
        severity,
        code,
        message,
        location,
        line_number: line.number,
        byte_range: ByteRange {
            start: line.start,
            end: line.end,
        },
        raw_text: format!("{}\n", line.raw),
        ansi_stripped: line.raw != line.stripped,
    }
}

fn location_from_captures(
    captures: &regex::Captures<'_>,
    path_case_folding: crate::storage::PathCaseFolding,
) -> Location {
    Location {
        file: captures["file"].to_owned(),
        line: captures["line"].parse().ok(),
        column: captures["column"].parse().ok(),
        end_line: captures["line"].parse().ok(),
        end_column: captures["column"]
            .parse::<u64>()
            .ok()
            .and_then(|column| column.checked_add(1)),
        role: LocationRole::Primary,
        in_user_code: is_user_code_path(
            &captures["file"],
            UserCodeEcosystem::JavaScript,
            path_case_folding,
        ),
    }
}

fn global_fallback_location(sidecar: &Sidecar) -> Location {
    Location {
        file: global_fallback_file(&sidecar.command),
        line: None,
        column: None,
        end_line: None,
        end_column: None,
        role: LocationRole::Primary,
        in_user_code: true,
    }
}

fn global_fallback_file(command: &str) -> String {
    let all_tokens = command.split_whitespace().collect::<Vec<_>>();
    let tsc_index = all_tokens.iter().position(|token| {
        Path::new(token).file_name().and_then(|name| name.to_str()) == Some("tsc")
    });
    let tokens = tsc_index.map_or(all_tokens.as_slice(), |index| &all_tokens[index + 1..]);
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "-p" | "--project" => {
                if let Some(path) = tokens.get(i + 1) {
                    return (*path).to_owned();
                }
                i += 1;
            }
            token if token.starts_with("--project=") => {
                return token.trim_start_matches("--project=").to_owned();
            }
            _ => i += 1,
        }
    }

    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "--build" | "-b" => {
                if let Some(path) = tokens.get(i + 1)
                    && !path.starts_with('-')
                {
                    return (*path).to_owned();
                }
                i += 1;
            }
            token if token.starts_with("--build=") => {
                return token.trim_start_matches("--build=").to_owned();
            }
            _ => i += 1,
        }
    }

    tokens
        .iter()
        .find(|token| !token.starts_with('-') && token.ends_with(".json"))
        .map_or_else(|| "tsconfig.json".to_owned(), |token| (*token).to_owned())
}

fn failure_from_diagnostic(
    diagnostic: &TscDiagnostic,
    companion_diagnostics: Vec<Diagnostic>,
) -> TscFailureBlock {
    let code = diagnostic.code.clone();
    let kind = tsc_kind(code.as_deref());
    let mut parse_warnings = Vec::new();
    if diagnostic
        .location
        .as_ref()
        .is_none_or(|location| location.line.is_none())
    {
        parse_warnings.push(ParseWarning::MissingLineNumber);
    }
    if kind == FailureKind::Unknown
        && !parse_warnings.contains(&ParseWarning::UnstructuredDiagnostic)
    {
        parse_warnings.push(ParseWarning::UnstructuredDiagnostic);
    }
    let primary_location = diagnostic.location.clone().unwrap_or_else(|| Location {
        file: "tsconfig.json".to_owned(),
        line: None,
        column: None,
        end_line: None,
        end_column: None,
        role: LocationRole::Primary,
        in_user_code: true,
    });
    let details = details_for_kind(
        kind,
        &diagnostic.message,
        code.as_deref(),
        &primary_location,
    );

    TscFailureBlock {
        kind,
        primary_location,
        message: diagnostic.message.clone(),
        diagnostic_code: code,
        details,
        raw_log_lines: [diagnostic.line_number, diagnostic.line_number],
        byte_range: diagnostic.byte_range,
        excerpt_text: diagnostic.raw_text.clone(),
        ansi_stripped: diagnostic.ansi_stripped,
        companion_diagnostics,
        parse_warnings,
    }
}

// Companion synthesis was removed alongside the adjacency-inference companion
// attachment in TscParse::from_log. Real companion population requires explicit
// chain parsing (tsc relatedInformation) which is v1.5 work — see
// tsc_intra_diagnostic_chain and tsc_cross_file_chain in the backlog comment at
// the top of this file.

fn unknown_failure_block(lines: &[LogLine], sidecar: &Sidecar) -> TscFailureBlock {
    // G3-locked: this must always return a usable block, including for empty logs,
    // so that typecheck_failed → failures[] is never empty when sidecar pins tsc.
    let mut excerpt = excerpt_text(lines);
    // String::truncate panics if the byte index falls inside a UTF-8 codepoint.
    // Walk down to the nearest char boundary (≤1024 bytes) before truncating.
    if excerpt.len() > 1024 {
        let mut cutoff = 1024;
        while cutoff > 0 && !excerpt.is_char_boundary(cutoff) {
            cutoff -= 1;
        }
        excerpt.truncate(cutoff);
    }
    let raw_log_lines = match (lines.first(), lines.last()) {
        (Some(first), Some(last)) => [first.number, last.number],
        _ => [0, 0],
    };
    let byte_range = match (lines.first(), lines.last()) {
        (Some(first), Some(last)) => ByteRange {
            start: first.start,
            end: last.end,
        },
        _ => ByteRange { start: 0, end: 0 },
    };
    TscFailureBlock {
        kind: FailureKind::Unknown,
        primary_location: global_fallback_location(sidecar),
        message: excerpt.trim().to_owned(),
        diagnostic_code: None,
        details: Details::Unknown {},
        raw_log_lines,
        byte_range,
        excerpt_text: excerpt,
        ansi_stripped: lines.iter().any(|line| line.raw != line.stripped),
        companion_diagnostics: vec![],
        parse_warnings: vec![
            ParseWarning::MissingLineNumber,
            ParseWarning::UnstructuredDiagnostic,
        ],
    }
}

fn details_for_kind(
    kind: FailureKind,
    message: &str,
    code: Option<&str>,
    location: &Location,
) -> Details {
    match kind {
        FailureKind::TypeError => Details::TypeError {
            spans: vec![span_from_location(location)],
            explanation_command: code.map(|code| format!("tsc diagnostic {code}")),
        },
        FailureKind::SyntaxError => Details::SyntaxError {
            spans: vec![span_from_location(location)],
        },
        FailureKind::MissingSymbol => Details::MissingSymbol {
            symbol: missing_symbol(message).unwrap_or_else(|| message.to_owned()),
            import_chain: vec![],
        },
        _ => Details::Unknown {},
    }
}

fn span_from_location(location: &Location) -> Span {
    Span {
        file: location.file.clone(),
        line: location.line,
        column: location.column,
        end_line: location.end_line,
        end_column: location.end_column,
        role: location.role,
        note: None,
    }
}

fn missing_symbol(message: &str) -> Option<String> {
    quoted_symbol(message).or_else(|| {
        message
            .strip_prefix("TS2307: Cannot find module '")
            .and_then(|rest| rest.split('\'').next())
            .map(str::to_owned)
    })
}

fn quoted_symbol(message: &str) -> Option<String> {
    message
        .split('\'')
        .nth(1)
        .map(str::to_owned)
        .filter(|symbol| !symbol.is_empty())
}

// EXTRACT-CANDIDATE OMITTED: parallelism broken because phase/kind dispatch
// degenerates for tsc — phase is always `typecheck`, and kind is a closed
// TS-code allowlist rather than the sibling adapters' per-runner classifier
// policy. There is no shared logic to extract here.
fn tsc_kind(code: Option<&str>) -> FailureKind {
    match code {
        Some("TS2304") | Some("TS2552") | Some("TS2307") | Some("TS2688") => {
            FailureKind::MissingSymbol
        }
        Some("TS2322") | Some("TS2345") | Some("TS2741") | Some("TS2769") => FailureKind::TypeError,
        Some("TS1005") | Some("TS1128") | Some("TS1136") => FailureKind::SyntaxError,
        _ => FailureKind::Unknown,
    }
}

fn tsc_severity(value: &str) -> TscSeverity {
    match value {
        "warning" => TscSeverity::Warning,
        "suggestion" => TscSeverity::Suggestion,
        "message" => TscSeverity::Message,
        _ => TscSeverity::Error,
    }
}

// fn diagnostic_severity(...) was removed alongside companion_from_diagnostic.
// The locked severity mapping (error→error, warning→warning, suggestion→info,
// message→hint) is documented at docs/schema.md under the tsc per-adapter
// section. Re-introduce this helper when v1.5 chain parsing populates
// companions.

fn push_unique_warnings(target: &mut Vec<ParseWarning>, incoming: &[ParseWarning]) {
    for warning in incoming {
        if !target.contains(warning) {
            target.push(*warning);
        }
    }
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

fn excerpt_text(lines: &[LogLine]) -> String {
    let mut text = lines
        .iter()
        .map(|line| line.raw.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn strip_ansi(text: &str) -> String {
    ansi_regex().replace_all(text, "").into_owned()
}

fn listed_diagnostic_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    // EXTRACT-CANDIDATE: OnceLock<Regex> accessor wrappers are duplicated across adapters.
    REGEX.get_or_init(|| {
        Regex::new(
            r"^(?<file>\S.+)\((?<line>\d+),(?<column>\d+)\): (?<severity>error|warning|suggestion|message) TS(?<code>\d+): (?<message>.+)$",
        )
        .unwrap()
    })
}

fn pretty_diagnostic_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"^(?<file>\S.+):(?<line>\d+):(?<column>\d+) - (?<severity>error|warning|suggestion|message) TS(?<code>\d+): (?<message>.+)$",
        )
        .unwrap()
    })
}

fn global_diagnostic_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"^(?<severity>error|warning|suggestion|message) TS(?<code>\d+): (?<message>.+)$",
        )
        .unwrap()
    })
}

fn tsc_signature_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"\b(?:error|warning|suggestion|message) TS\d+:").unwrap())
}

fn tsc_found_errors_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^Found (?<count>\d+) errors?\b").unwrap())
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
    use crate::schema::{Adapter, AdapterKind, ParseStatus, SelectionReason};

    fn sidecar(command: &str, exit_code: i32) -> Sidecar {
        Sidecar {
            meta_schema_version: "1.2".to_owned(),
            schema_version: "1.6".to_owned(),
            failtrace_version: "0.1.0".to_owned(),
            capture_build: None,
            adapters: vec![Adapter {
                id: ADAPTER_ID.to_owned(),
                version: "tsc@0.1.0".to_owned(),
                kind: AdapterKind::Typechecker,
                selected: true,
                selection_reason: SelectionReason::ArgvNameMatch,
            }],
            adapter_candidates: vec![],
            argv: command.split_whitespace().map(str::to_owned).collect(),
            command: command.to_owned(),
            cwd: "/repo".to_owned(),
            started_at: "2026-05-15T12:00:00Z".to_owned(),
            duration_ms: 10,
            exit_code: Some(exit_code),
            signal: None,
            early_exit: None,
            truncated: false,
            truncation_reason: None,
            path_case_folding: crate::storage::PathCaseFolding::CaseSensitive,
            stream_merge: crate::storage::StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
        }
    }

    #[test]
    fn global_fallback_prefers_project_flag() {
        assert_eq!(
            global_fallback_file("tsc --project configs/tsconfig.app.json"),
            "configs/tsconfig.app.json"
        );
        assert_eq!(
            global_fallback_file("tsc -p tsconfig.dev.json"),
            "tsconfig.dev.json"
        );
    }

    #[test]
    fn global_fallback_uses_build_path_then_json_token_then_literal_default() {
        assert_eq!(
            global_fallback_file("tsc --build packages/app"),
            "packages/app"
        );
        assert_eq!(global_fallback_file("tsc -b refs/app"), "refs/app");
        assert_eq!(
            global_fallback_file("npx -p typescript@5 tsc --project tsconfig.app.json"),
            "tsconfig.app.json"
        );
        assert_eq!(global_fallback_file("tsc custom.json"), "custom.json");
        assert_eq!(global_fallback_file("tsc --noEmit"), "tsconfig.json");
    }

    #[test]
    fn unknown_nonzero_tsc_output_marks_reported_error_unaccounted() {
        let log = b"Found 1 error in a compiler output shape this adapter cannot parse.\n";
        let document =
            analyze_log(log, &sidecar("tsc --noEmit", 2), AnalyzeOptions::default()).unwrap();

        assert_eq!(
            document.runner_outcome,
            crate::schema::RunnerOutcome::TypecheckFailed
        );
        assert_eq!(document.parse_status, ParseStatus::Partial);
        assert_eq!(document.failures.len(), 1);
        assert_eq!(document.failures[0].kind, FailureKind::Unknown);
        assert_eq!(document.failures[0].diagnostic_code, None);
        assert!(
            document
                .parse_warnings
                .contains(&ParseWarning::UnstructuredDiagnostic)
        );
        assert!(
            document
                .parse_warnings
                .contains(&ParseWarning::RunnerSummaryMismatch)
        );
        assert!(!document.failures_complete);
    }

    #[test]
    fn standalone_non_error_diagnostic_is_dropped_when_exit_clean() {
        let log = b"src/index.ts(1,1): warning TS9999: synthetic warning\n";
        let document =
            analyze_log(log, &sidecar("tsc --noEmit", 0), AnalyzeOptions::default()).unwrap();

        assert_eq!(
            document.runner_outcome,
            crate::schema::RunnerOutcome::Passed
        );
        assert!(document.failures.is_empty());
    }

    #[test]
    fn standalone_non_errors_are_dropped_not_attached_as_companions() {
        // Adjacency-based companion attachment is forbidden.
        // Non-error diagnostics (warning, suggestion, message) emitted between
        // error diagnostics must be dropped from failures[] entirely, NOT
        // attached as companions of the preceding error. Structurally-chained
        // companions (tsc relatedInformation) are v1.5 work.
        let log = b"src/index.ts(1,7): error TS2322: Type 'string' is not assignable to type 'number'.\nsrc/index.ts(1,1): warning TS9999: synthetic warning\nsrc/index.ts(1,1): suggestion TS9998: synthetic suggestion\nsrc/index.ts(1,1): message TS9997: synthetic message\n";
        let document =
            analyze_log(log, &sidecar("tsc --noEmit", 2), AnalyzeOptions::default()).unwrap();

        // Exactly one failure (the error); no failures for the non-errors.
        assert_eq!(document.failures.len(), 1);
        // No adjacency-based companions attached.
        assert!(document.failures[0].companion_diagnostics.is_empty());
    }

    #[test]
    fn unknown_diagnostic_code_is_complete_with_advisory_warning() {
        let log = b"src/index.ts(1,7): error TS9999: Something new happened.\n";
        let document =
            analyze_log(log, &sidecar("tsc --noEmit", 2), AnalyzeOptions::default()).unwrap();

        assert_eq!(document.failures[0].kind, FailureKind::Unknown);
        assert_eq!(document.parse_status, ParseStatus::Complete);
        assert!(
            document
                .parse_warnings
                .contains(&ParseWarning::UnstructuredDiagnostic)
        );
    }
}
