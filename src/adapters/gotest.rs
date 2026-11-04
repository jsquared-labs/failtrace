use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::OnceLock,
};

use regex::Regex;
use serde::Deserialize;
use serde_json::json;

use crate::{
    adapters::{
        AdapterError, AdapterImpl, AnalyzeOptions,
        shared::{
            AnalysisDocumentParts, UserCodeEcosystem, build_analysis_document, is_user_code_path,
            parse_status_from_warnings, sidecar_selected_adapter, truncate_for_budget,
            zero_test_counts,
        },
    },
    schema::{
        ByteRange, Command, CommandIntent, Counts, Details, DiagnosticSeverity, EarlyExit,
        EarlyExitKiller, EarlyExitReason, Evidence, Excerpt, ExcerptKind, ExcerptSource, Failure,
        FailureKind, Frame, Location, LocationRole, OutputDocument, ParseStatus, ParseWarning,
        Phase, RaceParticipant, Ranking, RankingStrategy, Span, TimeoutSource, derive_failure_id,
        final_outcome, validate,
    },
    storage::{PathCaseFolding, Sidecar},
};

const ADAPTER_ID: &str = "gotest";

pub fn analyze_log(
    log: &[u8],
    sidecar: &Sidecar,
    options: AnalyzeOptions,
) -> Result<OutputDocument, AdapterError> {
    GoTestAdapter::analyze_log(log, sidecar, options)
}

struct GoTestAdapter;

impl AdapterImpl for GoTestAdapter {
    fn analyze_log(
        log: &[u8],
        sidecar: &Sidecar,
        options: AnalyzeOptions,
    ) -> Result<OutputDocument, AdapterError> {
        if !sidecar_selected_adapter(sidecar, ADAPTER_ID) {
            return Err(AdapterError::InvalidSidecar {
                reason: "sidecar does not select gotest adapter".to_owned(),
            });
        }

        if !command_has_json_flag(&sidecar.command) {
            let document = json_required_rejection(log, sidecar);
            let violations = validate(&document);
            if !violations.is_empty() {
                return Err(AdapterError::InvariantViolation(violations));
            }
            return Ok(document);
        }

        let parsed = GoParse::from_log(log);
        if !parsed.looks_like_gotest(sidecar) {
            return Err(AdapterError::UnrecognizedFormat {
                reason: "no go test -json event stream found".to_owned(),
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

#[derive(Debug, Clone, PartialEq)]
struct GoParse {
    packages: BTreeMap<String, PackageState>,
    builds: BTreeMap<String, BuildState>,
    malformed_event_lines: u64,
    unknown_actions: BTreeMap<String, u64>,
    event_count: u64,
    has_pause_or_cont: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct PackageState {
    name: String,
    outputs: Vec<OutputChunk>,
    terminal: Option<Terminal>,
    fail_terminal_count: u64,
    tests: BTreeMap<String, TestState>,
    observed_runs: BTreeSet<String>,
    no_test_files: bool,
    has_pause_or_cont: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct TestState {
    name: String,
    outputs: Vec<OutputChunk>,
    terminal: Option<Terminal>,
    fail_terminal_count: u64,
    saw_run: bool,
    has_pause_or_cont: bool,
    first_line: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct BuildState {
    import_path: String,
    outputs: Vec<OutputChunk>,
    failed: Option<EventSite>,
    fail_terminal_count: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct OutputChunk {
    text: String,
    site: EventSite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EventSite {
    line: u64,
    byte_range: ByteRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Terminal {
    action: String,
    site: EventSite,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GoEvent {
    action: Option<String>,
    package: Option<String>,
    test: Option<String>,
    output: Option<String>,
    import_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoFailureBlock {
    phase: Phase,
    kind: FailureKind,
    runner_package_id: Option<String>,
    runner_test_id: Option<String>,
    test_segments: Vec<String>,
    test_name: Option<String>,
    primary_location: Location,
    message: String,
    diagnostic_code: Option<String>,
    details: Details,
    raw_log_lines: [u64; 2],
    byte_range: ByteRange,
    excerpt_text: String,
    ambiguous_subtest_name: bool,
}

impl GoParse {
    fn from_log(log: &[u8]) -> Self {
        let lines = split_lines(log);
        let mut parsed = Self {
            packages: BTreeMap::new(),
            builds: BTreeMap::new(),
            malformed_event_lines: 0,
            unknown_actions: BTreeMap::new(),
            event_count: 0,
            has_pause_or_cont: false,
        };

        for line in lines {
            if line.bytes.is_empty() {
                continue;
            }
            let event = match serde_json::Deserializer::from_slice(line.bytes)
                .into_iter::<GoEvent>()
                .next()
            {
                Some(Ok(event)) => event,
                _ => {
                    parsed.malformed_event_lines += 1;
                    continue;
                }
            };
            parsed.event_count += 1;
            parsed.fold_event(
                event,
                EventSite {
                    line: line.number,
                    byte_range: ByteRange {
                        start: line.start,
                        end: line.end,
                    },
                },
            );
        }

        parsed
    }

    fn fold_event(&mut self, event: GoEvent, site: EventSite) {
        let Some(action) = event.action.as_deref() else {
            self.malformed_event_lines += 1;
            return;
        };

        match action {
            "build-output" => {
                let Some(import_path) = event.import_path else {
                    return;
                };
                let state = self
                    .builds
                    .entry(import_path.clone())
                    .or_insert_with(|| BuildState {
                        import_path,
                        outputs: vec![],
                        failed: None,
                        fail_terminal_count: 0,
                    });
                if let Some(output) = event.output {
                    state.outputs.push(OutputChunk { text: output, site });
                }
            }
            "build-fail" => {
                let Some(import_path) = event.import_path else {
                    return;
                };
                let state = self
                    .builds
                    .entry(import_path.clone())
                    .or_insert_with(|| BuildState {
                        import_path,
                        outputs: vec![],
                        failed: None,
                        fail_terminal_count: 0,
                    });
                state.fail_terminal_count += 1;
                state.failed = Some(site);
            }
            "start" | "run" | "output" | "pass" | "fail" | "skip" | "pause" | "cont" | "bench" => {
                let Some(package) = event.package else {
                    return;
                };
                if matches!(action, "pause" | "cont") {
                    self.has_pause_or_cont = true;
                }
                let state = self
                    .packages
                    .entry(package.clone())
                    .or_insert_with(|| PackageState {
                        name: package.clone(),
                        outputs: vec![],
                        terminal: None,
                        fail_terminal_count: 0,
                        tests: BTreeMap::new(),
                        observed_runs: BTreeSet::new(),
                        no_test_files: false,
                        has_pause_or_cont: false,
                    });
                if matches!(action, "pause" | "cont") {
                    state.has_pause_or_cont = true;
                }
                if let Some(test) = event.test {
                    let test_state = state
                        .tests
                        .entry(test.clone())
                        .or_insert_with(|| TestState {
                            name: test.clone(),
                            outputs: vec![],
                            terminal: None,
                            fail_terminal_count: 0,
                            saw_run: false,
                            has_pause_or_cont: false,
                            first_line: site.line,
                        });
                    if action == "run" {
                        test_state.saw_run = true;
                        state.observed_runs.insert(test.clone());
                    }
                    if matches!(action, "pause" | "cont") {
                        test_state.has_pause_or_cont = true;
                    }
                    if let Some(output) = event.output {
                        test_state.outputs.push(OutputChunk { text: output, site });
                    }
                    if matches!(action, "pass" | "fail" | "skip" | "bench") {
                        if action == "fail" {
                            test_state.fail_terminal_count += 1;
                        }
                        test_state.terminal = Some(Terminal {
                            action: action.to_owned(),
                            site,
                        });
                    }
                } else {
                    if let Some(output) = event.output {
                        if output.contains("[no test files]") {
                            state.no_test_files = true;
                        }
                        state.outputs.push(OutputChunk { text: output, site });
                    }
                    if matches!(action, "pass" | "fail" | "skip") {
                        if action == "fail" {
                            state.fail_terminal_count += 1;
                        }
                        state.terminal = Some(Terminal {
                            action: action.to_owned(),
                            site,
                        });
                    }
                }
            }
            other => {
                *self.unknown_actions.entry(other.to_owned()).or_insert(0) += 1;
                if let (Some(package), Some(output)) = (event.package, event.output) {
                    let state =
                        self.packages
                            .entry(package.clone())
                            .or_insert_with(|| PackageState {
                                name: package.clone(),
                                outputs: vec![],
                                terminal: None,
                                fail_terminal_count: 0,
                                tests: BTreeMap::new(),
                                observed_runs: BTreeSet::new(),
                                no_test_files: false,
                                has_pause_or_cont: false,
                            });
                    if let Some(test) = event.test {
                        state
                            .tests
                            .entry(test.clone())
                            .or_insert_with(|| TestState {
                                name: test,
                                outputs: vec![],
                                terminal: None,
                                fail_terminal_count: 0,
                                saw_run: false,
                                has_pause_or_cont: false,
                                first_line: site.line,
                            })
                            .outputs
                            .push(OutputChunk { text: output, site });
                    } else {
                        state.outputs.push(OutputChunk { text: output, site });
                    }
                }
            }
        }
    }

    fn looks_like_gotest(&self, sidecar: &Sidecar) -> bool {
        // EXTRACT-CANDIDATE: runner recognition gates now share a sidecar-pin
        // plus adapter-specific signature shape. For gotest, any decoded
        // event stream from a selected gotest sidecar is sufficient.
        sidecar_selected_adapter(sidecar, ADAPTER_ID) && self.event_count > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Placeholder for v1.5 sidecar.runner_version validation.
struct GoTestVersion;

#[allow(dead_code)]
impl GoTestVersion {
    // EXTRACT-CANDIDATE: adapter version family validation is shared policy with runner-specific validated majors.
    // v1.4 sidecars do not carry structured runner_version. Go test JSON
    // version-family validation is deferred to the v1.5 sidecar.runner_version slice.
    fn has_validated_output_family(&self) -> bool {
        true
    }
}

fn lower(
    parsed: GoParse,
    log: &[u8],
    sidecar: &Sidecar,
    options: AnalyzeOptions,
) -> OutputDocument {
    let all_blocks = reconcile_failures(&parsed, sidecar.path_case_folding);
    let all_failures = all_blocks
        .iter()
        .map(|block| failure_from_block(block, &sidecar.cwd))
        .collect::<Vec<_>>();
    let early_exit = early_exit(&parsed, sidecar);
    let runner_outcome = runner_outcome(&parsed, &all_failures, sidecar, early_exit.as_ref());
    let mut parse_warnings = parse_warnings(&parsed);
    let failure_terminal_shortfall = has_unaccounted_failure_terminal(&parsed, &all_blocks);
    if failure_terminal_shortfall && !parse_warnings.contains(&ParseWarning::RunnerSummaryMismatch)
    {
        parse_warnings.push(ParseWarning::RunnerSummaryMismatch);
    }
    let parse_status = parse_status_from_warnings(&parse_warnings);
    let failures_complete = !sidecar.truncated
        && !failure_terminal_shortfall
        && !parse_warnings.contains(&ParseWarning::MissingTerminalEvent)
        && !parse_warnings.contains(&ParseWarning::MalformedStructuredEvent);
    let counts = counts(&parsed, &all_failures);
    let budgeted_failures = truncate_for_budget(all_failures, options);

    build_analysis_document(AnalysisDocumentParts {
        sidecar,
        log,
        budgeted_failures,
        runner_outcome,
        early_exit,
        // EXTRACT-CANDIDATE OMITTED: this site does NOT use
        // shared::summary_parse_result because gotest derives
        // parse fidelity from an event graph and silently-accounted malformed
        // JSONL lines, not from sibling summary-line completeness.
        parse_status,
        parse_warnings,
        failures_complete,
        ranking: ranking(&parsed, &all_blocks),
        counts,
        suggested_next_commands: suggested_next_commands(&all_blocks, &sidecar.cwd),
        extensions: document_extensions(&parsed),
        options,
    })
}

fn has_unaccounted_failure_terminal(parsed: &GoParse, blocks: &[GoFailureBlock]) -> bool {
    let block_counts = FailureBlockCounts::from_blocks(blocks);
    let build_shortfall = parsed.builds.values().any(|build| {
        if build.fail_terminal_count > 1 {
            return true;
        }
        let accounted = block_counts
            .builds
            .get(&build.import_path)
            .copied()
            .unwrap_or(0);
        build.fail_terminal_count > accounted
    });
    if build_shortfall {
        return true;
    }

    for package in parsed.packages.values() {
        let cascade_parents = failed_descendant_parents(package);
        for test in package.tests.values() {
            let cascade_parent = is_cascade_parent(test, &cascade_parents);
            if cascade_parent && test.fail_terminal_count > 1 {
                // One parent fail terminal can be explained by a failed
                // descendant. Repeated terminals mean collapsed epochs or an
                // additional parent failure that the single child block
                // cannot account for.
                return true;
            }
            if test.fail_terminal_count > 0 && !cascade_parent {
                let expected_test_id = runner_test_id(&package.name, &test.name);
                let accounted = block_counts
                    .tests
                    .get(&expected_test_id)
                    .copied()
                    .unwrap_or(0);
                if test.fail_terminal_count > accounted {
                    return true;
                }
            }
        }

        if package.fail_terminal_count > 1 {
            return true;
        }
        let accounted = block_counts
            .packages
            .get(&package.name)
            .copied()
            .unwrap_or(0);
        if package.fail_terminal_count > accounted {
            return true;
        }
    }

    false
}

#[derive(Debug, Default)]
struct FailureBlockCounts {
    builds: BTreeMap<String, u64>,
    tests: BTreeMap<String, u64>,
    packages: BTreeMap<String, u64>,
}

impl FailureBlockCounts {
    fn from_blocks(blocks: &[GoFailureBlock]) -> Self {
        let mut counts = Self::default();
        for block in blocks {
            if let Some(test_id) = &block.runner_test_id {
                *counts.tests.entry(test_id.clone()).or_default() += 1;
            }
            let Some(package_id) = &block.runner_package_id else {
                continue;
            };
            if block.phase == Phase::Build {
                *counts.builds.entry(package_id.clone()).or_default() += 1;
                let package_name = package_id
                    .split_once(' ')
                    .map_or(package_id.as_str(), |(name, _)| name);
                *counts.packages.entry(package_name.to_owned()).or_default() += 1;
            } else {
                *counts.packages.entry(package_id.clone()).or_default() += 1;
            }
        }
        counts
    }
}

fn reconcile_failures(parsed: &GoParse, path_case_folding: PathCaseFolding) -> Vec<GoFailureBlock> {
    let mut blocks = Vec::new();
    for build in parsed.builds.values() {
        if build.failed.is_some() {
            blocks.extend(build_failures(build, path_case_folding));
        }
    }

    for package in parsed.packages.values() {
        let cascade_parents = failed_descendant_parents(package);
        for test in package.tests.values() {
            if is_cascade_parent(test, &cascade_parents) {
                continue;
            }
            if let Some(block) = race_failure(package, test, path_case_folding) {
                blocks.push(block);
                continue;
            }
            if let Some(block) = timeout_failure(package, test, path_case_folding) {
                blocks.push(block);
                continue;
            }
            if let Some(block) = panic_failure(package, test, path_case_folding) {
                blocks.push(block);
                continue;
            }
            if let Some(block) = assertion_failure(package, test, path_case_folding) {
                blocks.push(block);
            }
        }
        if let Some(block) = init_panic_failure(package, path_case_folding) {
            blocks.push(block);
        }
    }
    blocks
}

fn build_failures(build: &BuildState, path_case_folding: PathCaseFolding) -> Vec<GoFailureBlock> {
    build
        .outputs
        .iter()
        .filter_map(|output| {
            let captures = build_diagnostic_regex().captures(output.text.trim_end())?;
            let file = captures["file"].to_owned();
            let line = captures["line"].parse().ok();
            let column = captures["column"].parse().ok();
            let message = captures["message"].to_owned();
            let kind = go_build_kind(&message);
            let location = Location {
                file,
                line,
                column,
                end_line: line,
                end_column: column.and_then(|column| column.checked_add(1)),
                role: LocationRole::Primary,
                in_user_code: is_user_code_path(
                    &captures["file"],
                    UserCodeEcosystem::Go,
                    path_case_folding,
                ),
            };
            Some(GoFailureBlock {
                phase: Phase::Build,
                kind,
                runner_package_id: Some(build.import_path.clone()),
                runner_test_id: None,
                test_segments: vec![],
                test_name: None,
                primary_location: location.clone(),
                message: message.clone(),
                diagnostic_code: None,
                details: details_for_kind(kind, &message, &location, vec![], None),
                raw_log_lines: [output.site.line, output.site.line],
                byte_range: output.site.byte_range,
                excerpt_text: output.text.clone(),
                ambiguous_subtest_name: false,
            })
        })
        .collect()
}

fn assertion_failure(
    package: &PackageState,
    test: &TestState,
    path_case_folding: PathCaseFolding,
) -> Option<GoFailureBlock> {
    if test
        .terminal
        .as_ref()
        .map(|terminal| terminal.action.as_str())
        != Some("fail")
    {
        return None;
    }
    let diagnostic = first_assertion_diagnostic(&test.outputs)?;
    let diagnostic_file = diagnostic.file.clone();
    let location = Location {
        file: diagnostic_file.clone(),
        line: Some(diagnostic.line),
        column: None,
        end_line: Some(diagnostic.line),
        end_column: None,
        role: LocationRole::Primary,
        in_user_code: is_user_code_path(&diagnostic_file, UserCodeEcosystem::Go, path_case_folding),
    };
    Some(GoFailureBlock {
        phase: Phase::Test,
        kind: FailureKind::Assertion,
        runner_package_id: Some(package.name.clone()),
        runner_test_id: Some(runner_test_id(&package.name, &test.name)),
        test_segments: test.name.split('/').map(str::to_owned).collect(),
        test_name: Some(test.name.clone()),
        primary_location: location,
        message: diagnostic.message.clone(),
        diagnostic_code: None,
        details: Details::Assertion {
            expected: None,
            actual: None,
            diff: None,
        },
        raw_log_lines: [diagnostic.site.line, diagnostic.site.line],
        byte_range: diagnostic.site.byte_range,
        excerpt_text: diagnostic.raw_text,
        ambiguous_subtest_name: subtest_name_ambiguous(package, &test.name),
    })
}

fn panic_failure(
    package: &PackageState,
    test: &TestState,
    path_case_folding: PathCaseFolding,
) -> Option<GoFailureBlock> {
    if test
        .terminal
        .as_ref()
        .map(|terminal| terminal.action.as_str())
        != Some("fail")
    {
        return None;
    }
    let output = joined_output(&test.outputs);
    if !output.contains("panic: ") || output.contains("panic: test timed out") {
        return None;
    }
    let panic_line = first_line_containing(&test.outputs, "panic: ")?;
    let panic_message = panic_line
        .text
        .trim()
        .strip_prefix("panic: ")
        .unwrap_or_else(|| panic_line.text.trim())
        .to_owned();
    let stack = stack_frames(&output, path_case_folding);
    let location = primary_location_from_frames(&stack).unwrap_or_else(fallback_location);
    Some(GoFailureBlock {
        phase: Phase::Test,
        kind: FailureKind::Panic,
        runner_package_id: Some(package.name.clone()),
        runner_test_id: Some(runner_test_id(&package.name, &test.name)),
        test_segments: test.name.split('/').map(str::to_owned).collect(),
        test_name: Some(test.name.clone()),
        primary_location: location,
        message: panic_message.clone(),
        diagnostic_code: None,
        details: Details::Panic {
            panic_message,
            stack,
            goroutine_id: first_goroutine_id(&output),
        },
        raw_log_lines: output_line_range(&test.outputs),
        byte_range: output_byte_range(&test.outputs),
        excerpt_text: output,
        ambiguous_subtest_name: subtest_name_ambiguous(package, &test.name),
    })
}

fn race_failure(
    package: &PackageState,
    test: &TestState,
    path_case_folding: PathCaseFolding,
) -> Option<GoFailureBlock> {
    let output = joined_output(&test.outputs);
    if !output.contains("WARNING: DATA RACE") {
        return None;
    }
    let stack = stack_frames(&output, path_case_folding);
    let location = primary_location_from_frames(&stack).unwrap_or_else(fallback_location);
    Some(GoFailureBlock {
        phase: Phase::Test,
        kind: FailureKind::Race,
        runner_package_id: Some(package.name.clone()),
        runner_test_id: Some(runner_test_id(&package.name, &test.name)),
        test_segments: test.name.split('/').map(str::to_owned).collect(),
        test_name: Some(test.name.clone()),
        primary_location: location,
        message: "data race detected".to_owned(),
        diagnostic_code: None,
        details: Details::Race {
            participants: vec![RaceParticipant { stack }],
        },
        raw_log_lines: output_line_range(&test.outputs),
        byte_range: output_byte_range(&test.outputs),
        excerpt_text: output,
        ambiguous_subtest_name: subtest_name_ambiguous(package, &test.name),
    })
}

fn timeout_failure(
    package: &PackageState,
    test: &TestState,
    path_case_folding: PathCaseFolding,
) -> Option<GoFailureBlock> {
    let output = joined_output(&test.outputs);
    let captures = timeout_regex().captures(&output)?;
    let stack = stack_frames(&output, path_case_folding);
    let location = primary_location_from_frames(&stack).unwrap_or_else(fallback_location);
    let limit_ms = duration_ms(&captures["duration"]);
    Some(GoFailureBlock {
        phase: Phase::Test,
        kind: FailureKind::Timeout,
        runner_package_id: Some(package.name.clone()),
        runner_test_id: Some(runner_test_id(&package.name, &test.name)),
        test_segments: test.name.split('/').map(str::to_owned).collect(),
        test_name: Some(test.name.clone()),
        primary_location: location,
        message: format!("test timed out after {}", &captures["duration"]),
        diagnostic_code: None,
        details: Details::Timeout {
            source: TimeoutSource::PerTest,
            limit_ms,
            silence_ms: None,
        },
        raw_log_lines: output_line_range(&test.outputs),
        byte_range: output_byte_range(&test.outputs),
        excerpt_text: output,
        ambiguous_subtest_name: subtest_name_ambiguous(package, &test.name),
    })
}

fn init_panic_failure(
    package: &PackageState,
    path_case_folding: PathCaseFolding,
) -> Option<GoFailureBlock> {
    if package
        .terminal
        .as_ref()
        .map(|terminal| terminal.action.as_str())
        != Some("fail")
    {
        return None;
    }
    if package.tests.values().any(|test| test.saw_run) {
        return None;
    }
    let output = joined_output(&package.outputs);
    if !output.contains("panic: ") {
        return None;
    }
    let panic_line = first_line_containing(&package.outputs, "panic: ")?;
    let panic_message = panic_line
        .text
        .trim()
        .strip_prefix("panic: ")
        .unwrap_or_else(|| panic_line.text.trim())
        .to_owned();
    let stack = stack_frames(&output, path_case_folding);
    let location = primary_location_from_frames(&stack).unwrap_or_else(fallback_location);
    Some(GoFailureBlock {
        phase: Phase::Setup,
        kind: FailureKind::Panic,
        runner_package_id: Some(package.name.clone()),
        runner_test_id: None,
        test_segments: vec![],
        test_name: None,
        primary_location: location,
        message: panic_message.clone(),
        diagnostic_code: None,
        details: Details::Panic {
            panic_message,
            stack,
            goroutine_id: first_goroutine_id(&output),
        },
        raw_log_lines: output_line_range(&package.outputs),
        byte_range: output_byte_range(&package.outputs),
        excerpt_text: output,
        ambiguous_subtest_name: false,
    })
}

fn failure_from_block(block: &GoFailureBlock, cwd: &str) -> Failure {
    let mut extensions = BTreeMap::new();
    if block.ambiguous_subtest_name {
        extensions.insert("go".to_owned(), json!({ "subtest_name_ambiguous": true }));
    }
    let mut failure = Failure {
        failure_id: String::new(),
        adapter_id: ADAPTER_ID.to_owned(),
        phase: block.phase,
        kind: block.kind,
        runner_package_id: block.runner_package_id.clone(),
        runner_test_id: block.runner_test_id.clone(),
        test_segments: block.test_segments.clone(),
        test_name: block.test_name.clone(),
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
                source: ExcerptSource::RunnerStructuredEvent,
                kind: ExcerptKind::FailureBlock,
                byte_range: block.byte_range,
                text: block.excerpt_text.clone(),
                ansi_stripped: false,
                truncated: false,
            }],
        }),
        cascade_root: None,
        cascade_reason: None,
        dependent_failure_ids: vec![],
        retries: vec![],
        companion_diagnostics: vec![],
        extensions,
    };
    failure.failure_id = derive_failure_id(&failure, cwd);
    failure
}

fn runner_outcome(
    parsed: &GoParse,
    failures: &[Failure],
    sidecar: &Sidecar,
    early_exit: Option<&EarlyExit>,
) -> crate::schema::RunnerOutcome {
    // EXTRACT-CANDIDATE OMITTED: this site does NOT use
    // shared::summary_runner_outcome because gotest derives
    // runner_outcome from package/test event terminals plus build-fail events,
    // not a final summary line or one fixed runner footer.
    if sidecar.signal.is_some() {
        return crate::schema::RunnerOutcome::HarnessCrashed;
    }
    if early_exit.is_some_and(|early_exit| {
        matches!(
            early_exit.reason,
            EarlyExitReason::WatchdogSilence
                | EarlyExitReason::WatchdogWallclock
                | EarlyExitReason::RunnerInternalTimeout
                | EarlyExitReason::PerTestTimeout
        )
    }) {
        return crate::schema::RunnerOutcome::TimedOut;
    }
    if parsed.builds.values().any(|build| build.failed.is_some()) {
        return crate::schema::RunnerOutcome::PreTestFailed;
    }
    if sidecar.truncated {
        return crate::schema::RunnerOutcome::HarnessCrashed;
    }
    if parsed.packages.values().all(|package| {
        package
            .terminal
            .as_ref()
            .map(|terminal| terminal.action.as_str())
            == Some("skip")
            && package.no_test_files
    }) {
        return crate::schema::RunnerOutcome::NoTests;
    }
    if !failures.is_empty()
        || parsed
            .packages
            .values()
            .any(|package| package.terminal.as_ref().map(|t| t.action.as_str()) == Some("fail"))
    {
        return crate::schema::RunnerOutcome::Failed;
    }
    if parsed
        .packages
        .values()
        .all(|package| package.terminal.as_ref().map(|t| t.action.as_str()) == Some("pass"))
    {
        crate::schema::RunnerOutcome::Passed
    } else {
        crate::schema::RunnerOutcome::HarnessCrashed
    }
}

fn early_exit(parsed: &GoParse, sidecar: &Sidecar) -> Option<EarlyExit> {
    if let Some(signal) = &sidecar.signal {
        return Some(EarlyExit {
            reason: EarlyExitReason::SignalReceived,
            killer: EarlyExitKiller::Os,
            details: signal.clone(),
        });
    }
    let timed_out = parsed
        .packages
        .values()
        .flat_map(|package| package.tests.values())
        .any(|test| joined_output(&test.outputs).contains("panic: test timed out"));
    timed_out.then(|| EarlyExit {
        reason: EarlyExitReason::PerTestTimeout,
        killer: EarlyExitKiller::Runner,
        details: "go test reported a per-test timeout".to_owned(),
    })
}

fn parse_warnings(parsed: &GoParse) -> Vec<ParseWarning> {
    let mut warnings = Vec::new();
    if parsed.malformed_event_lines > 0 {
        warnings.push(ParseWarning::MalformedStructuredEvent);
    }
    let missing_terminal = parsed.packages.values().any(|package| {
        package.terminal.is_none()
            || package
                .tests
                .values()
                .any(|test| test.saw_run && test.terminal.is_none())
    });
    if missing_terminal {
        warnings.push(ParseWarning::MissingTerminalEvent);
    }
    warnings
}

fn ranking(parsed: &GoParse, blocks: &[GoFailureBlock]) -> Ranking {
    Ranking {
        strategy: if blocks.is_empty() {
            RankingStrategy::None
        } else if blocks.iter().any(|block| block.phase == Phase::Build) {
            RankingStrategy::CompileBeforeRuntime
        } else {
            RankingStrategy::FirstEmitted
        },
        is_stable_across_runs: parsed.packages.len() <= 1 && !parsed.has_pause_or_cont,
    }
}

fn counts(parsed: &GoParse, failures: &[Failure]) -> Counts {
    // EXTRACT-CANDIDATE OMITTED: this site does NOT use the shared
    // summary-runner count primitives because gotest counts are
    // derived from event terminals and synthesized timeout/race entries, not
    // a single summary line with passed/failed/skipped totals.
    let tests_run = parsed
        .packages
        .values()
        .flat_map(|package| package.tests.values())
        .filter(|test| test.saw_run)
        .count() as u64;
    let terminal_passed = parsed
        .packages
        .values()
        .flat_map(|package| package.tests.values())
        .filter(|test| test.terminal.as_ref().map(|t| t.action.as_str()) == Some("pass"))
        .count() as u64;
    let tests_skipped = parsed
        .packages
        .values()
        .flat_map(|package| package.tests.values())
        .filter(|test| test.terminal.as_ref().map(|t| t.action.as_str()) == Some("skip"))
        .count() as u64;
    let tests_failed = failures
        .iter()
        .filter(|failure| failure.runner_test_id.is_some())
        .filter(|failure| final_outcome(failure) == crate::schema::RetryOutcome::Failed)
        .count() as u64;
    let tests_errored = failures
        .iter()
        .filter(|failure| failure.runner_test_id.is_some())
        .filter(|failure| final_outcome(failure) == crate::schema::RetryOutcome::Errored)
        .count() as u64;
    Counts {
        tests_run,
        tests_passed: terminal_passed,
        tests_failed,
        tests_errored,
        tests_skipped,
        tests_not_run: None,
        files_failed: Some(
            failures
                .iter()
                .map(|failure| failure.primary_location.file.as_str())
                .collect::<BTreeSet<_>>()
                .len() as u64,
        )
        .filter(|count| *count > 0),
    }
}

fn suggested_next_commands(blocks: &[GoFailureBlock], cwd: &str) -> Vec<Command> {
    blocks
        .iter()
        .filter_map(|block| {
            let runner_test_id = block.runner_test_id.as_ref()?;
            let (package, test) = runner_test_id.split_once("::")?;
            let argv = vec![
                "go".to_owned(),
                "test".to_owned(),
                "-json".to_owned(),
                package.to_owned(),
                "-run".to_owned(),
                format!("^{}$", regex::escape(test)),
            ];
            Some(Command {
                command: argv
                    .iter()
                    .map(|arg| shell_quote(arg))
                    .collect::<Vec<_>>()
                    .join(" "),
                argv,
                cwd: cwd.to_owned(),
                intent: CommandIntent::RerunIsolated,
                interactive: false,
                mutates_workspace: false,
                uses_network: false,
                requires_approval: false,
            })
        })
        .collect()
}

fn document_extensions(parsed: &GoParse) -> BTreeMap<String, serde_json::Value> {
    if parsed.malformed_event_lines == 0 && parsed.unknown_actions.is_empty() {
        return BTreeMap::new();
    }
    BTreeMap::from([(
        "go".to_owned(),
        json!({
            "malformed_event_lines": parsed.malformed_event_lines,
            "unknown_actions": parsed.unknown_actions,
        }),
    )])
}

fn failed_descendant_parents(package: &PackageState) -> BTreeSet<String> {
    let mut parents = BTreeSet::new();
    for test in package.tests.values().filter(|test| {
        test.terminal
            .as_ref()
            .map(|terminal| terminal.action.as_str())
            == Some("fail")
    }) {
        for (index, _) in test.name.match_indices('/') {
            parents.insert(test.name[..index].to_owned());
        }
    }
    parents
}

fn is_cascade_parent(test: &TestState, failed_descendant_parents: &BTreeSet<String>) -> bool {
    if test
        .terminal
        .as_ref()
        .map(|terminal| terminal.action.as_str())
        != Some("fail")
    {
        return false;
    }
    failed_descendant_parents.contains(&test.name) && !has_independent_failure_evidence(test)
}

fn has_independent_failure_evidence(test: &TestState) -> bool {
    let output = joined_output(&test.outputs);
    first_assertion_diagnostic(&test.outputs).is_some()
        || output.contains("WARNING: DATA RACE")
        || output.contains("panic: ")
        || output.contains("fatal error: ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AssertionDiagnostic {
    file: String,
    line: u64,
    message: String,
    raw_text: String,
    site: EventSite,
}

fn first_assertion_diagnostic(outputs: &[OutputChunk]) -> Option<AssertionDiagnostic> {
    for output in outputs {
        for line in output.text.lines() {
            let Some(captures) = assertion_line_regex().captures(line) else {
                continue;
            };
            let line_number = captures["line"].parse().ok()?;
            return Some(AssertionDiagnostic {
                file: captures["file"].to_owned(),
                line: line_number,
                message: captures["message"].to_owned(),
                raw_text: output.text.clone(),
                site: output.site,
            });
        }
    }
    None
}

fn go_build_kind(message: &str) -> FailureKind {
    if message.contains("undefined:") {
        FailureKind::MissingSymbol
    } else if message.contains("syntax error") {
        FailureKind::SyntaxError
    } else if message.contains("cannot use")
        || message.contains("invalid operation")
        || message.contains("cannot assign")
        || message.contains("too many")
        || message.contains("not enough")
    {
        FailureKind::TypeError
    } else {
        FailureKind::Unknown
    }
}

fn details_for_kind(
    kind: FailureKind,
    message: &str,
    location: &Location,
    stack: Vec<Frame>,
    panic_message: Option<String>,
) -> Details {
    match kind {
        FailureKind::TypeError => Details::TypeError {
            spans: vec![span_from_location(location)],
            explanation_command: None,
        },
        FailureKind::SyntaxError => Details::SyntaxError {
            spans: vec![span_from_location(location)],
        },
        FailureKind::MissingSymbol => Details::MissingSymbol {
            symbol: message
                .split("undefined:")
                .nth(1)
                .map(str::trim)
                .unwrap_or(message)
                .to_owned(),
            import_chain: vec![],
        },
        FailureKind::Panic => Details::Panic {
            panic_message: panic_message.unwrap_or_else(|| message.to_owned()),
            stack,
            goroutine_id: None,
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

fn subtest_name_ambiguous(package: &PackageState, test_name: &str) -> bool {
    if !test_name.contains('/') {
        return false;
    }
    let parts = test_name.split('/').collect::<Vec<_>>();
    if parts.len() <= 2 {
        return false;
    }
    for end in 2..parts.len() {
        let prefix = parts[..end].join("/");
        if !package.observed_runs.contains(&prefix) {
            return true;
        }
    }
    false
}

fn runner_test_id(package: &str, test: &str) -> String {
    format!("{package}::{test}")
}

fn joined_output(outputs: &[OutputChunk]) -> String {
    outputs
        .iter()
        .map(|output| output.text.as_str())
        .collect::<String>()
}

fn output_line_range(outputs: &[OutputChunk]) -> [u64; 2] {
    match (outputs.first(), outputs.last()) {
        (Some(first), Some(last)) => [first.site.line, last.site.line],
        _ => [0, 0],
    }
}

fn output_byte_range(outputs: &[OutputChunk]) -> ByteRange {
    match (outputs.first(), outputs.last()) {
        (Some(first), Some(last)) => ByteRange {
            start: first.site.byte_range.start,
            end: last.site.byte_range.end,
        },
        _ => ByteRange { start: 0, end: 0 },
    }
}

fn first_line_containing<'a>(outputs: &'a [OutputChunk], needle: &str) -> Option<&'a OutputChunk> {
    outputs.iter().find(|output| output.text.contains(needle))
}

fn stack_frames(output: &str, path_case_folding: PathCaseFolding) -> Vec<Frame> {
    let lines = output.lines().collect::<Vec<_>>();
    let mut frames = Vec::new();
    let mut index = 0;
    while index + 1 < lines.len() {
        let function = lines[index].trim();
        let path_line = lines[index + 1].trim();
        if let Some(captures) = stack_file_regex().captures(path_line) {
            let raw_file = captures["file"].to_owned();
            let file = portable_go_file(&raw_file);
            let line = captures["line"].parse().ok();
            frames.push(Frame {
                in_user_code: is_user_code_path(
                    &raw_file,
                    UserCodeEcosystem::Go,
                    path_case_folding,
                ),
                file,
                line,
                function: Some(function.to_owned()).filter(|value| !value.is_empty()),
            });
            index += 2;
        } else {
            index += 1;
        }
    }
    frames
}

fn primary_location_from_frames(frames: &[Frame]) -> Option<Location> {
    frames
        .iter()
        .find(|frame| frame.in_user_code)
        .map(|frame| Location {
            file: frame.file.clone(),
            line: frame.line,
            column: None,
            end_line: frame.line,
            end_column: None,
            role: LocationRole::Primary,
            in_user_code: frame.in_user_code,
        })
}

fn portable_go_file(file: &str) -> String {
    Path::new(file)
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| file.to_owned(), str::to_owned)
}

fn first_goroutine_id(output: &str) -> Option<u64> {
    output.lines().find_map(|line| {
        goroutine_regex()
            .captures(line)
            .and_then(|captures| captures["id"].parse().ok())
    })
}

fn fallback_location() -> Location {
    Location {
        file: "<go-test-json>".to_owned(),
        line: None,
        column: None,
        end_line: None,
        end_column: None,
        role: LocationRole::Primary,
        in_user_code: true,
    }
}

fn duration_ms(value: &str) -> Option<u64> {
    let number = value
        .trim_end_matches("ms")
        .trim_end_matches('s')
        .parse::<f64>()
        .ok()?;
    if value.ends_with("ms") {
        Some(number.round() as u64)
    } else if value.ends_with('s') {
        Some((number * 1000.0).round() as u64)
    } else {
        None
    }
}

fn json_required_rejection(log: &[u8], sidecar: &Sidecar) -> OutputDocument {
    let suggested = suggest_json_rerun_argv(&sidecar.command);
    build_analysis_document(AnalysisDocumentParts {
        sidecar,
        log,
        budgeted_failures: truncate_for_budget(Vec::new(), AnalyzeOptions::default()),
        runner_outcome: if sidecar.exit_code == Some(0) {
            crate::schema::RunnerOutcome::Passed
        } else {
            crate::schema::RunnerOutcome::HarnessCrashed
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
            command: suggested
                .iter()
                .map(|arg| shell_quote_assignment_word(arg))
                .collect::<Vec<_>>()
                .join(" "),
            argv: suggested,
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

fn command_has_json_flag(command: &str) -> bool {
    let tokens = split_command(command);
    let Some(test_index) = go_test_token_index(&tokens) else {
        return false;
    };
    matches!(json_flag_state(&tokens, test_index), JsonFlagState::Enabled)
}

fn suggest_json_rerun_argv(command: &str) -> Vec<String> {
    let mut tokens = split_command(command);
    let Some(test_index) = go_test_token_index(&tokens) else {
        return tokens;
    };

    let prefix_end = test_index.saturating_sub(1);
    let last_goflags_token_index = tokens[..prefix_end]
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, token)| token.starts_with("GOFLAGS=").then_some(idx));
    if let Some(idx) = last_goflags_token_index
        && let Some(value) = tokens[idx].strip_prefix("GOFLAGS=")
    {
        let retained: Vec<&str> = value
            .split_whitespace()
            .filter(|part| json_flag_value(part).is_none())
            .collect();
        tokens[idx] = format!("GOFLAGS={}", retained.join(" "));
    }

    let runner_args_start = test_index + 1;
    let runner_args_end = runner_args_end_index(&tokens, runner_args_start);
    let retained_runner_args: Vec<String> = tokens[runner_args_start..runner_args_end]
        .iter()
        .filter(|token| json_flag_value(token).is_none())
        .cloned()
        .collect();
    tokens.splice(runner_args_start..runner_args_end, retained_runner_args);
    tokens.insert(runner_args_start, "-json".to_owned());
    tokens
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonFlagValue {
    Enabled,
    Disabled,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JsonFlagState {
    Unmentioned,
    Enabled,
    Disabled,
    // Sticky: once Invalid, later mentions cannot override. Go's flag parser fatals on the
    // first unparseable boolean, so subsequent valid tokens never get a chance to run.
    Invalid,
}

fn json_flag_value(token: &str) -> Option<JsonFlagValue> {
    if matches!(token, "-json" | "--json") {
        return Some(JsonFlagValue::Enabled);
    }
    let value = token
        .strip_prefix("-json=")
        .or_else(|| token.strip_prefix("--json="))?;
    // Go boolean flag values follow strconv.ParseBool's exact 12-spelling grammar. Any value
    // outside that set — including empty `-json=` — is a fatal parse error in Go's flag
    // parser and produces no JSON output. Sticky in json_flag_state.
    match value {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(JsonFlagValue::Enabled),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(JsonFlagValue::Disabled),
        _ => Some(JsonFlagValue::Invalid),
    }
}

// Compute the effective JSON-output state for a `go test` command. Inline GOFLAGS is scanned
// first, then pre-boundary runner args (argv wins over env defaults). Within each scope, later
// mentions overwrite earlier ones. Shell semantics make repeated `VAR=value` prefixes
// last-wins, so only the LAST `GOFLAGS=` assignment in the prefix contributes — earlier ones
// are shadowed. Tokens after `--` or `-args` are test-binary args and never contribute.
fn json_flag_state(tokens: &[String], test_index: usize) -> JsonFlagState {
    let mut state = JsonFlagState::Unmentioned;

    let prefix_end = test_index.saturating_sub(1);
    let last_goflags =
        tokens[..prefix_end]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(token_index, token)| {
                token
                    .strip_prefix("GOFLAGS=")
                    .map(|value| (token_index, value))
            });
    if let Some((_token_index, value)) = last_goflags {
        for part in value.split_whitespace() {
            if matches!(state, JsonFlagState::Invalid) {
                break;
            }
            state = match json_flag_value(part) {
                Some(JsonFlagValue::Enabled) => JsonFlagState::Enabled,
                Some(JsonFlagValue::Disabled) => JsonFlagState::Disabled,
                Some(JsonFlagValue::Invalid) => JsonFlagState::Invalid,
                None => continue,
            };
        }
    }

    let runner_args_start = test_index + 1;
    let runner_args_end = runner_args_end_index(tokens, runner_args_start);
    for token in &tokens[runner_args_start..runner_args_end] {
        if matches!(state, JsonFlagState::Invalid) {
            break;
        }
        state = match json_flag_value(token) {
            Some(JsonFlagValue::Enabled) => JsonFlagState::Enabled,
            Some(JsonFlagValue::Disabled) => JsonFlagState::Disabled,
            Some(JsonFlagValue::Invalid) => JsonFlagState::Invalid,
            None => continue,
        };
    }

    state
}

fn runner_args_end_index(tokens: &[String], runner_args_start: usize) -> usize {
    tokens[runner_args_start..]
        .iter()
        .position(|token| token == "--" || token == "-args")
        .map_or(tokens.len(), |offset| runner_args_start + offset)
}

fn go_test_token_index(tokens: &[String]) -> Option<usize> {
    let mut i = 0;
    if tokens.get(i).map(String::as_str) == Some("env") {
        i += 1;
        while let Some(token) = tokens.get(i) {
            if token.contains('=') && !token.starts_with('-') {
                i += 1;
            } else {
                break;
            }
        }
    }
    while i + 1 < tokens.len() {
        if Path::new(&tokens[i])
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_go_binary_basename)
            && tokens[i + 1] == "test"
        {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

fn is_go_binary_basename(basename: &str) -> bool {
    let name = basename.strip_suffix(".exe").unwrap_or(basename);
    if name == "go" || name == "gotip" {
        return true;
    }
    let Some(version) = name.strip_prefix("go") else {
        return false;
    };
    is_go_version_suffix(version)
}

fn is_go_version_suffix(version: &str) -> bool {
    let Some(mut rest) = version.strip_prefix("1.") else {
        return false;
    };
    let minor_len = rest.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if minor_len == 0 {
        return false;
    }
    rest = &rest[minor_len..];
    if rest.is_empty() {
        return true;
    }
    if let Some(patch) = rest.strip_prefix('.') {
        return !patch.is_empty() && patch.chars().all(|ch| ch.is_ascii_digit());
    }
    for suffix in ["rc", "beta"] {
        if let Some(n) = rest.strip_prefix(suffix) {
            return !n.is_empty() && n.chars().all(|ch| ch.is_ascii_digit());
        }
    }
    false
}

fn split_command(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            '\\' if in_single && chars.peek() == Some(&'\'') => {
                let _ = chars.next();
                current.push('\'');
            }
            '\\' if in_double => {
                if let Some(next @ ('"' | '\\' | '$' | '`')) = chars.peek().copied() {
                    let _ = chars.next();
                    current.push(next);
                } else {
                    current.push(ch);
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_+-./:=@^$".contains(&byte))
    {
        return arg.to_owned();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn shell_quote_assignment_word(arg: &str) -> String {
    let Some((name, value)) = arg.split_once('=') else {
        return shell_quote(arg);
    };
    if !is_posix_env_name(name) || value.is_empty() {
        return shell_quote(arg);
    }
    let quoted_value = shell_quote(value);
    if quoted_value == value {
        arg.to_owned()
    } else {
        format!("{name}={quoted_value}")
    }
}

fn is_posix_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

#[derive(Debug, Clone, Copy)]
struct LogLine<'a> {
    number: u64,
    start: u64,
    end: u64,
    bytes: &'a [u8],
}

fn split_lines(log: &[u8]) -> Vec<LogLine<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in log.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(LogLine {
                number: lines.len() as u64 + 1,
                start: start as u64,
                end: (index + 1) as u64,
                bytes: &log[start..index + 1],
            });
            start = index + 1;
        }
    }
    if start < log.len() {
        lines.push(LogLine {
            number: lines.len() as u64 + 1,
            start: start as u64,
            end: log.len() as u64,
            bytes: &log[start..],
        });
    }
    lines
}

fn build_diagnostic_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    // EXTRACT-CANDIDATE: OnceLock<Regex> accessor wrappers are duplicated across adapters.
    REGEX.get_or_init(|| {
        Regex::new(r"^\./(?<file>.+\.go):(?<line>\d+):(?<column>\d+): (?<message>.+)$").unwrap()
    })
}

fn assertion_line_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"^\s{4}(?<file>\S+\.go):(?<line>\d+): (?<message>.+)$").unwrap()
    })
}

fn stack_file_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^(?<file>.+\.go):(?<line>\d+)(?: .*)?$").unwrap())
}

fn goroutine_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^goroutine (?<id>\d+) ").unwrap())
}

fn timeout_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"panic: test timed out after (?<duration>[0-9.]+(?:ms|s))").unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    mod detector {
        use super::*;

        #[test]
        fn detects_existing_json_flag() {
            assert!(command_has_json_flag("go test -json ./..."));
            assert!(command_has_json_flag("go test --json ./..."));
            assert!(command_has_json_flag("go test -json=true ./..."));
            assert!(command_has_json_flag("env GOFLAGS=-json go test ./..."));
            assert!(command_has_json_flag(
                "GOFLAGS='-count=1 -json' go test ./..."
            ));
            assert!(!command_has_json_flag("go test ./..."));
            assert!(!command_has_json_flag("go test -json=false ./..."));
            assert!(!command_has_json_flag("go test --json=false ./..."));
            assert!(!command_has_json_flag(
                "env GOFLAGS=-json=false go test ./..."
            ));
            assert!(!command_has_json_flag("go test ./pkg -- -json"));
            assert!(!command_has_json_flag("go test ./pkg -args -json"));
        }

        #[test]
        fn detects_json_flag_inside_double_quoted_goflags() {
            assert!(command_has_json_flag("GOFLAGS=\"-json\" go test ./..."));
            assert!(command_has_json_flag(
                "GOFLAGS=\"-count=1 -json\" go test ./..."
            ));
            assert!(!command_has_json_flag(
                "GOFLAGS=\"-count=1 -json=false\" go test ./..."
            ));
        }

        #[test]
        fn detector_stops_at_args_boundary() {
            assert!(command_has_json_flag(
                "go test -json ./pkg -args -json=false"
            ));
            assert!(!command_has_json_flag("go test ./pkg -args -json"));
        }

        #[test]
        fn invalid_json_values_disable_runner_json() {
            assert!(!command_has_json_flag("go test -json=maybe ./..."));
            assert!(!command_has_json_flag("go test -json=yes ./..."));
            assert!(!command_has_json_flag("go test -json= ./..."));
            assert!(!command_has_json_flag("go test --json=garbage ./..."));
            assert!(!command_has_json_flag(
                "env GOFLAGS=-json=garbage go test ./..."
            ));
        }

        #[test]
        fn invalid_json_state_is_sticky_against_later_valid_tokens() {
            assert!(!command_has_json_flag(
                "go test -json=maybe -json=true ./..."
            ));
            assert!(!command_has_json_flag(
                "go test -json=maybe -json=false ./..."
            ));
            assert!(!command_has_json_flag(
                "go test -json=maybe -json=maybe ./..."
            ));
            assert!(!command_has_json_flag(
                "env GOFLAGS=-json=maybe go test -json ./..."
            ));
            assert!(!command_has_json_flag(
                "env GOFLAGS=-json=maybe go test -json=false ./..."
            ));
            assert!(!command_has_json_flag(
                "GOFLAGS='-json=maybe -json=true' go test ./..."
            ));
        }

        #[test]
        fn json_flag_value_matches_go_parsebool_grammar() {
            for falsy in ["0", "f", "F", "false", "FALSE", "False"] {
                let command = format!("go test -json={falsy} ./...");
                assert!(
                    !command_has_json_flag(&command),
                    "expected `{command}` to read as JSON-disabled"
                );
            }
            for truthy in ["1", "t", "T", "true", "TRUE", "True"] {
                let command = format!("go test -json={truthy} ./...");
                assert!(
                    command_has_json_flag(&command),
                    "expected `{command}` to read as JSON-enabled"
                );
            }
        }

        #[test]
        fn repeated_goflags_last_assignment_wins() {
            assert!(!command_has_json_flag(
                "GOFLAGS=-json GOFLAGS=-count=1 go test ./..."
            ));
            assert!(command_has_json_flag(
                "GOFLAGS=-count=1 GOFLAGS=-json go test ./..."
            ));
            assert!(!command_has_json_flag(
                "GOFLAGS=-json GOFLAGS=-json=false go test ./..."
            ));
            assert!(command_has_json_flag(
                "GOFLAGS=-json=false GOFLAGS=-json go test ./..."
            ));
            assert!(!command_has_json_flag(
                "env GOFLAGS=-json GOFLAGS=-count=1 go test ./..."
            ));
        }

        #[test]
        fn effective_json_state_respects_argv_and_goflags_ordering() {
            assert!(command_has_json_flag(
                "go test -json=false -json=true ./..."
            ));
            assert!(!command_has_json_flag("go test -json -json=false ./..."));
            assert!(command_has_json_flag(
                "env GOFLAGS=-json=false go test -json ./..."
            ));
            assert!(!command_has_json_flag(
                "env GOFLAGS=-json go test -json=false ./..."
            ));
        }
    }

    mod suggester {
        use super::*;

        #[test]
        fn inserts_json_after_go_test() {
            assert_eq!(
                suggest_json_rerun_argv("env GOFLAGS=-count=1 go test ./... -run TestThing"),
                [
                    "env",
                    "GOFLAGS=-count=1",
                    "go",
                    "test",
                    "-json",
                    "./...",
                    "-run",
                    "TestThing"
                ]
            );
        }

        #[test]
        fn drops_existing_json_mentions_and_inserts_one_runner_flag() {
            assert_eq!(
                suggest_json_rerun_argv("go test -json=true ./..."),
                ["go", "test", "-json", "./..."]
            );
            assert_eq!(
                suggest_json_rerun_argv("go test -json=false --json=garbage -json ./..."),
                ["go", "test", "-json", "./..."]
            );
            assert_eq!(
                suggest_json_rerun_argv("env GOFLAGS=-json=false go test -json=maybe ./..."),
                ["env", "GOFLAGS=", "go", "test", "-json", "./..."]
            );
        }

        #[test]
        fn preserves_test_binary_args_after_boundaries() {
            assert_eq!(
                suggest_json_rerun_argv("go test ./pkg -- -json"),
                ["go", "test", "-json", "./pkg", "--", "-json"]
            );
            assert_eq!(
                suggest_json_rerun_argv("go test ./pkg -args -json=false"),
                ["go", "test", "-json", "./pkg", "-args", "-json=false"]
            );
        }

        #[test]
        fn strips_json_parts_from_only_the_last_goflags_slot() {
            assert_eq!(
                suggest_json_rerun_argv("GOFLAGS=-json=maybe GOFLAGS=-json=false go test ./..."),
                [
                    "GOFLAGS=-json=maybe",
                    "GOFLAGS=",
                    "go",
                    "test",
                    "-json",
                    "./..."
                ]
            );
            assert_eq!(
                suggest_json_rerun_argv("GOFLAGS=-json=maybe GOFLAGS=-count=1 go test ./..."),
                [
                    "GOFLAGS=-json=maybe",
                    "GOFLAGS=-count=1",
                    "go",
                    "test",
                    "-json",
                    "./..."
                ]
            );
        }

        #[test]
        fn strips_all_json_forms_from_multipart_goflags() {
            assert_eq!(
                suggest_json_rerun_argv(
                    "GOFLAGS='-json=false -count=1 -json=maybe -race -json=true' go test ./..."
                ),
                ["GOFLAGS=-count=1 -race", "go", "test", "-json", "./..."]
            );
        }

        #[test]
        fn robust_for_enabled_commands_without_returning_as_is() {
            assert_eq!(
                suggest_json_rerun_argv("env GOFLAGS=-json go test ./..."),
                ["env", "GOFLAGS=", "go", "test", "-json", "./..."]
            );
            assert_eq!(
                suggest_json_rerun_argv("go test -json=false -json=true ./..."),
                ["go", "test", "-json", "./..."]
            );
        }
    }

    mod tokenizer_renderer {
        use super::*;

        #[test]
        fn tokenizer_strips_double_quotes_and_preserves_supported_escapes() {
            assert_eq!(
                split_command("GOFLAGS=\"-count=1 -json\" go test ./..."),
                ["GOFLAGS=-count=1 -json", "go", "test", "./..."]
            );
            assert_eq!(
                split_command("go test -run \"Test\\\"Name\" ./pkg"),
                ["go", "test", "-run", "Test\"Name", "./pkg"]
            );
        }

        #[test]
        fn assignment_aware_shell_quote_cases() {
            assert_eq!(
                shell_quote_assignment_word("GOFLAGS=-count=1"),
                "GOFLAGS=-count=1"
            );
            assert_eq!(
                shell_quote_assignment_word("GOFLAGS=-count=1 -race"),
                "GOFLAGS='-count=1 -race'"
            );
            assert_eq!(shell_quote_assignment_word("GOFLAGS="), "GOFLAGS=");
            assert_eq!(
                shell_quote_assignment_word("--flag=value with spaces"),
                "'--flag=value with spaces'"
            );
            assert_eq!(
                shell_quote_assignment_word("A_B2=value with spaces"),
                "A_B2='value with spaces'"
            );
            assert_eq!(
                shell_quote_assignment_word("1BAD=value with spaces"),
                "'1BAD=value with spaces'"
            );
        }

        #[test]
        fn renders_assignment_preserving_suggested_command() {
            let argv =
                suggest_json_rerun_argv("GOFLAGS=\"-count=1 -race -json=false\" go test ./...");
            assert_eq!(
                argv,
                ["GOFLAGS=-count=1 -race", "go", "test", "-json", "./..."]
            );
            let command = argv
                .iter()
                .map(|arg| shell_quote_assignment_word(arg))
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(command, "GOFLAGS='-count=1 -race' go test -json ./...");
        }

        #[test]
        fn renders_empty_last_goflags_without_unshadowing_earlier_assignment() {
            let argv =
                suggest_json_rerun_argv("GOFLAGS=-json=maybe GOFLAGS=-json=false go test ./...");
            assert_eq!(
                argv,
                [
                    "GOFLAGS=-json=maybe",
                    "GOFLAGS=",
                    "go",
                    "test",
                    "-json",
                    "./..."
                ]
            );
            let command = argv
                .iter()
                .map(|arg| shell_quote_assignment_word(arg))
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(command, "GOFLAGS=-json=maybe GOFLAGS= go test -json ./...");
        }

        #[test]
        fn finds_versioned_go_test_launchers() {
            assert_eq!(
                go_test_token_index(&split_command("go1.21 test ./...")),
                Some(1)
            );
            assert_eq!(
                go_test_token_index(&split_command("go1.21.5 test")),
                Some(1)
            );
            assert_eq!(
                go_test_token_index(&split_command("go1.22rc1 test")),
                Some(1)
            );
            assert_eq!(
                go_test_token_index(&split_command("go1.22beta1 test")),
                Some(1)
            );
            assert_eq!(
                go_test_token_index(&split_command("gotip test ./...")),
                Some(1)
            );
            assert_eq!(
                go_test_token_index(&split_command("go.exe test ./...")),
                Some(1)
            );
            assert_eq!(go_test_token_index(&split_command("gofmt test")), None);
            assert_eq!(go_test_token_index(&split_command("gopher test")), None);
        }
    }
}
