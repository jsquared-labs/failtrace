//! `xcodebuild test` adapter.
//!
//! Parses the text output of `xcodebuild test` and `xcodebuild test-without-building`
//! invocations against `.xcodeproj` projects. Handles both legacy XCTest output
//! (`Test Suite '...' started`, `XCTAssert*`) and Swift Testing output
//! (`◇ Test ... started.`, `✘ Test ... recorded an issue ...`).
//!
//! Scope per `docs/design/xcodebuild-test-adapter.md`:
//! - Raw xcodebuild only — no `xcpretty`/`xcbeautify` wrapped streams.
//! - Text logs only — `.xcresult` bundle parsing deferred.
//! - Single-file Xcode projects — composite shapes (e.g. `bash -c "xcodebuild test"`)
//!   handled by the dispatcher's transparent-composite path.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use regex::Regex;

use crate::{
    adapters::{
        AdapterError, AdapterImpl, AnalyzeOptions,
        shared::{
            AnalysisDocumentParts, BudgetedFailures, build_analysis_document,
            parse_status_from_warnings, sidecar_selected_adapter, zero_test_counts,
        },
    },
    schema::{
        ByteRange, Command, CommandIntent, Counts, Details, DiagnosticSeverity, EarlyExit,
        EarlyExitKiller, EarlyExitReason, Evidence, Excerpt, ExcerptKind, ExcerptSource, Failure,
        FailureKind, Location, LocationRole, OutputDocument, ParseWarning, Phase, Ranking,
        RankingStrategy, Resolver, RunnerOutcome, TruncationReason, derive_failure_id, validate,
    },
    storage::Sidecar,
};

const ADAPTER_ID: &str = "xcodebuild-test";

/// Xcode major versions whose `xcodebuild test` output shape has been verified
/// against fixtures. Detected from log markers when available; absent banner is
/// not a warning (see design doc §VALIDATED versions).
#[allow(dead_code)] // surfaced via parse warnings once banner detection lands
const VALIDATED_XCODE_MAJOR_VERSIONS: &[u64] = &[16, 17, 27];

/// Swift Testing library versions whose output shape has been verified.
/// Observable in the log via `↳ Testing Library Version: <N>`.
#[allow(dead_code)] // surfaced via parse warnings once version-warning lands
const VALIDATED_SWIFT_TESTING_VERSIONS: &[u64] = &[1902, 2077];
const MAX_DESTINATION_SUGGESTIONS: usize = 5;

pub fn analyze_log(
    log: &[u8],
    sidecar: &Sidecar,
    options: AnalyzeOptions,
) -> Result<OutputDocument, AdapterError> {
    XcodebuildAdapter::analyze_log(log, sidecar, options)
}

struct XcodebuildAdapter;

impl AdapterImpl for XcodebuildAdapter {
    fn analyze_log(
        log: &[u8],
        sidecar: &Sidecar,
        options: AnalyzeOptions,
    ) -> Result<OutputDocument, AdapterError> {
        if !sidecar_selected_adapter(sidecar, ADAPTER_ID) {
            return Err(AdapterError::InvalidSidecar {
                reason: "sidecar does not select xcodebuild-test adapter".to_owned(),
            });
        }

        let parsed = ParsedRun::from_log(log);
        if !parsed.looks_like_xcodebuild() {
            return Err(AdapterError::UnrecognizedFormat {
                reason: "no xcodebuild test-phase markers found".to_owned(),
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

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Xcode 27 beta's zero-width-space line prefix. See [`line_text`].
const ZWSP: char = '\u{200B}';

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogLine {
    /// 1-indexed line number.
    number: u64,
    /// Byte offset of the line start in the raw log.
    start: u64,
    /// Byte offset just past the line's terminating newline (or EOF).
    end: u64,
    /// The matching view: line text with any trailing `\n` / `\r\n` stripped
    /// and any leading [`ZWSP`] run removed. Every regex, prefix check and
    /// captured message reads this — never the raw view — so failure_ids do
    /// not shift when Xcode decorates its recorder lines.
    text: String,
    /// How many leading [`ZWSP`] characters `text` dropped. Excerpt and
    /// evidence text must come from [`LogLine::raw_text`], which puts them
    /// back.
    zwsp_prefix: usize,
}

impl LogLine {
    /// The evidence view: exactly what the line's raw bytes decode to. An
    /// excerpt built from this round-trips against the `byte_range` it claims,
    /// which `text` cannot do once a ZWSP prefix has been stripped.
    fn raw_text(&self) -> Cow<'_, str> {
        if self.zwsp_prefix == 0 {
            return Cow::Borrowed(&self.text);
        }
        let mut raw = String::with_capacity(self.zwsp_prefix * ZWSP.len_utf8() + self.text.len());
        for _ in 0..self.zwsp_prefix {
            raw.push(ZWSP);
        }
        raw.push_str(&self.text);
        Cow::Owned(raw)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedFailure {
    phase: Phase,
    kind: FailureKind,
    runner_test_id: Option<String>,
    test_name: Option<String>,
    test_segments: Vec<String>,
    file: String,
    /// `None` for failures with no source line (e.g. linker errors that name no
    /// source location; xcodebuild's `ld: framework not found Foo` carries no
    /// file:line). The lowering emits these with `Location { line: None, .. }`.
    line: Option<u64>,
    column: Option<u64>,
    message: String,
    /// Range covers the entire failure block (single line for most failures;
    /// multi-line for `Undefined symbols for architecture` linker blocks).
    raw_log_lines: [u64; 2],
    byte_range: ByteRange,
    excerpt_text: String,
    /// Unresolved symbols extracted from `Undefined symbols ...` blocks.
    /// Empty for non-linker failures.
    unresolved_symbols: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DestinationResolution {
    bad_spec: DestinationSpec,
    available_destinations: Vec<AvailableDestination>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DestinationSpec {
    fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AvailableDestination {
    platform: String,
    id: Option<String>,
}

impl AvailableDestination {
    fn from_fields(fields: &BTreeMap<String, String>) -> Option<Self> {
        Some(Self {
            platform: fields.get("platform")?.to_owned(),
            id: fields.get("id").cloned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ParsedRun {
    failures: Vec<ParsedFailure>,
    /// Index of the first line that crossed into the test phase, if any.
    test_phase_start: Option<u64>,
    /// `** TEST SUCCEEDED **` (or `** TEST EXECUTE SUCCEEDED **` from
    /// test-without-building) observed.
    saw_test_succeeded: bool,
    /// `** TEST FAILED **` (or `** TEST EXECUTE FAILED **` from
    /// test-without-building) observed.
    saw_test_failed: bool,
    /// `Testing failed:` (top-level summary header) observed.
    saw_testing_failed_summary: bool,
    /// `◇ Test run started.` (Swift Testing harness present) observed.
    saw_swift_testing_harness: bool,
    /// `Test Suite '...' started` (XCTest envelope) observed.
    saw_xctest_envelope: bool,
    /// `Restarting after unexpected exit, crash, or test timeout; summary
    /// will include totals from previous launches.` observed. xcodebuild
    /// prints it when the test runner died and it relaunches the bundle. The
    /// promise in the message does not hold for the summaries this adapter
    /// reads: in real logs with many restarts, the final
    /// `Test run with N tests` covers only the distinct tests started after
    /// the last restart. Every launch before the last left no summary, so the
    /// counts cannot be complete.
    saw_test_runner_restart: bool,
    /// `Mach error` or `Failed to install or launch the test runner` observed
    /// before any test phase.
    saw_runtime_environment_failure: bool,
    /// Test totals parsed from summary lines. `build_counts` derives passed,
    /// failed and errored itself — failed from the unique `runner_test_id`s in
    /// `failures[]` (invariant 11), passed from the remainder — so only the
    /// run and skip totals are carried here.
    tests_run: u64,
    /// Failure-list coverage is tracked per harness because XCTest reports
    /// assertion incidents while Swift Testing reports issue incidents. These
    /// are not interchangeable with top-level unique failed-test counts.
    xctest_reported_failure_incidents: u64,
    xctest_outer_reported_failure_incidents: u64,
    xctest_outer_summary_count: u64,
    xctest_summary_scope_ambiguous: bool,
    xctest_extracted_failure_incidents: u64,
    swift_reported_issue_count: u64,
    swift_reported_test_issue_count: u64,
    swift_reported_issue_lower_bound: u64,
    swift_suite_scope_ambiguous: bool,
    swift_observed_issue_markers: u64,
    swift_extracted_issue_count: u64,
    terminal_reported_failed_test_ids: Vec<String>,
    tests_skipped: u64,
    /// Count of Swift Testing `➜ Test ... skipped` lines seen but not yet
    /// folded into `swift_testing_total_skipped`. Cleared at every
    /// `◇ Test run started.` (skips from a bundle that ended without a summary
    /// belong to no counted total) and again at every fold, so each bundle
    /// contributes exactly its own skips exactly once.
    swift_testing_bundle_skipped: u64,
    /// Swift Testing totals summed across every bundle's run summary. One
    /// xcodebuild invocation can run several bundles; `failures[]` spans all of
    /// them, so the counts must too.
    swift_testing_total_run: u64,
    swift_testing_total_skipped: u64,
    /// Every XCTest bundle execution in log order, opened at `Test Suite
    /// 'Name.xctest' started` and closed by the `Executed` line that follows
    /// `Test Suite 'Name.xctest' passed|failed`. One `xcodebuild test` can run
    /// several bundles (unit + UI tests), each inside its own `All tests`
    /// envelope; none is a superset of another, so they sum (taking the
    /// largest would drop the smaller bundle's tests). The same bundle can
    /// also run more than once
    /// (a second destination; xcodebuild relaunching a crashed bundle), so
    /// totals take the max per name across occurrences. The last entry may
    /// still be open when the log ends. See `xctest_totals`.
    xctest_bundles: Vec<XctestBundleRun>,
    /// Largest `Executed` line seen anywhere: the total when no bundle
    /// occurrence exists at all (older envelopes, `Selected tests` without a
    /// bundle line, a Swift Testing-only log whose envelope says 0).
    xctest_summary_max: (u64, u64),
    /// `Test Case` ids seen outside any bundle occurrence, and the skipped
    /// subset: the witness for the no-bundle fallback above.
    xctest_orphan_case_ids: BTreeSet<String>,
    xctest_orphan_skipped_case_ids: BTreeSet<String>,
    /// Swift Testing suite stack. Pushed on `◇ Suite "..." started.` / `◇ Suite X started.`,
    /// popped on the matching `✘/✔ Suite "..." passed/failed`. Test IDs emitted while the
    /// stack is non-empty get prefixed with `stack.join(".") + "."`.
    suite_stack: Vec<String>,
    destination_resolution: Option<DestinationResolution>,
}

/// One execution of an XCTest bundle: the lines between `Test Suite
/// 'Name.xctest' started` and the bundle's own `Executed` summary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct XctestBundleRun {
    name: String,
    /// Witness for this occurrence's total: every id seen on a `Test Case
    /// '-[Suite test]' started.` or `... passed|failed|skipped (...)` line
    /// inside it, and the subset whose terminal line said skipped. Ids rather
    /// than lines; started as well as terminal because interleaved stderr can
    /// clobber a terminal line's column 0 (observed: `unrecognised property
    /// keyTest Case '...' passed`).
    case_ids: BTreeSet<String>,
    skipped_case_ids: BTreeSet<String>,
    /// Largest class-level `Executed` line seen inside the occurrence. The
    /// summary of record for a bundle that never reached its end line (the
    /// runner crashed; xcodebuild relaunches the bundle as a new occurrence).
    inner_max: (u64, u64),
    /// The bundle's own `Executed` line, present once the bundle ended.
    total: Option<(u64, u64)>,
}

impl XctestBundleRun {
    fn recovered(&self) -> bool {
        self.total.is_some()
    }

    /// `(run, skipped)` for this occurrence: the bundle line, or the largest
    /// inner summary when the bundle never ended.
    fn summary(&self) -> (u64, u64) {
        self.total.unwrap_or(self.inner_max)
    }

    /// True when this occurrence's own witness disagrees with its own summary
    /// (or there is no summary to agree with). Scoped per occurrence so a
    /// bundle run twice with different skips (a configuration-dependent
    /// `XCTSkip` across two destinations) compares each run to its own line.
    fn disagrees(&self) -> bool {
        match self.total {
            None => true,
            Some((run, skipped)) => {
                self.case_ids.len() as u64 != run || self.skipped_case_ids.len() as u64 != skipped
            }
        }
    }
}

impl ParsedRun {
    /// XCTest (run, skipped) totals: per bundle name the largest occurrence,
    /// summed across names; or the largest summary line seen when the log
    /// has no bundle occurrence at all.
    fn xctest_totals(&self) -> (u64, u64) {
        if self.xctest_bundles.is_empty() {
            return self.xctest_summary_max;
        }
        let mut per_name: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
        for bundle in &self.xctest_bundles {
            let summary = bundle.summary();
            let entry = per_name.entry(bundle.name.as_str()).or_insert(summary);
            if summary.0 > entry.0 {
                *entry = summary;
            }
        }
        per_name
            .values()
            .fold((0, 0), |acc, (run, skipped)| (acc.0 + run, acc.1 + skipped))
    }

    /// True when the XCTest per-test lines and the published XCTest totals
    /// disagree. The counts come from the summary lines; the per-test lines
    /// are an independent witness present in every XCTest log. Any bundle
    /// occurrence that never ended counts as a disagreement: its summary of
    /// record is a class-level line that cannot cover the whole bundle. So
    /// does a bundle name whose occurrences together started more distinct
    /// tests than the occurrence `xctest_totals` selected for it: two
    /// occurrences that each agree with their own line can still hide tests
    /// from the published total when their populations differ. A Swift
    /// Testing-only log has no per-test lines and its
    /// bundle (if the envelope names one) ends with `Executed 0 tests`, so
    /// it agrees.
    fn xctest_summary_disagrees_with_cases(&self) -> bool {
        if self.xctest_bundles.is_empty() {
            let (run, skipped) = self.xctest_summary_max;
            return !self.xctest_orphan_case_ids.is_empty()
                && (self.xctest_orphan_case_ids.len() as u64 != run
                    || self.xctest_orphan_skipped_case_ids.len() as u64 != skipped);
        }
        !self.xctest_orphan_case_ids.is_empty()
            || self.xctest_bundles.iter().any(XctestBundleRun::disagrees)
            || self.xctest_bundle_populations_exceed_selected_totals()
    }

    /// For each bundle name, the distinct ids started across all of its
    /// occurrences against the run count of the occurrence the totals use.
    fn xctest_bundle_populations_exceed_selected_totals(&self) -> bool {
        let mut per_name: BTreeMap<&str, (u64, BTreeSet<&String>)> = BTreeMap::new();
        for bundle in &self.xctest_bundles {
            let entry = per_name
                .entry(bundle.name.as_str())
                .or_insert((0, BTreeSet::new()));
            entry.0 = entry.0.max(bundle.summary().0);
            entry.1.extend(bundle.case_ids.iter());
        }
        per_name
            .values()
            .any(|(selected_run, ids)| ids.len() as u64 > *selected_run)
    }

    /// The bundle occurrence that is still collecting lines, if any.
    fn open_xctest_bundle(&mut self) -> Option<&mut XctestBundleRun> {
        self.xctest_bundles
            .last_mut()
            .filter(|bundle| !bundle.recovered())
    }

    /// Distinct failing test ids among test-phase failures: invariant 11's
    /// `tests_failed`. One test recording several `#expect` issues yields
    /// several failure records but one failing test.
    fn unique_failed_test_ids(&self) -> u64 {
        self.failures
            .iter()
            .filter(|f| f.phase == Phase::Test)
            .filter_map(|f| f.runner_test_id.as_ref())
            .collect::<BTreeSet<_>>()
            .len() as u64
    }

    /// True when the evidence in the log contradicts the run/skip totals it
    /// published: an XCTest witness disagrees (see
    /// `xctest_summary_disagrees_with_cases`); xcodebuild relaunched the
    /// test runner (`saw_test_runner_restart`: every launch before the last
    /// left no summary, for either harness); or the parsed failures plus
    /// skips exceed the total (`build_counts` floors `tests_run` so the
    /// document is still emitted; this is what tells the consumer the floor
    /// engaged). The floor is a narrow guarantee: it fires only when failures
    /// plus skips exceed the surviving total, not whenever an earlier launch
    /// recorded failures. Swift Testing has no per-launch witness of its own
    /// (`◇ Test run started.` repeats only across relaunches in real crash
    /// logs, never within a healthy run), so the restart line is what
    /// catches a crashed Swift Testing launch.
    fn counts_disagree_with_evidence(&self) -> bool {
        self.xctest_summary_disagrees_with_cases()
            || self.saw_test_runner_restart
            || self.tests_run < self.unique_failed_test_ids() + self.tests_skipped
    }

    fn from_log(log: &[u8]) -> Self {
        let lines = split_lines(log);
        let mut run = ParsedRun {
            terminal_reported_failed_test_ids: parse_terminal_failed_test_ids(&lines),
            ..ParsedRun::default()
        };
        // Track which line indices have been consumed as part of a multi-line
        // linker block; skip them in the main scan to avoid double-counting.
        let mut consumed: BTreeSet<usize> = BTreeSet::new();
        let mut swift_run_epoch = 0_u64;
        let mut swift_suite_summaries_by_epoch = BTreeMap::<u64, u64>::new();
        let mut swift_suite_issue_lower_bound_by_epoch = BTreeMap::<u64, u64>::new();
        let mut swift_test_issue_count_by_epoch = BTreeMap::<u64, u64>::new();
        let mut swift_issue_marker_count_by_epoch = BTreeMap::<u64, u64>::new();
        let mut swift_run_issue_count_by_epoch = BTreeMap::<u64, u64>::new();
        let mut swift_overlapping_suite_epochs = BTreeSet::<u64>::new();
        let mut swift_epochs_with_run_issue_total = BTreeSet::<u64>::new();

        // Pre-fetch every regex once before the line loop. The `OnceLock`
        // `.get_or_init` is itself cheap after the first call, but clippy flags
        // the in-loop form as `regex_creation_in_loops`; hoisting also makes
        // the regex set discoverable in one place.
        let xctest_suite_start = XCTEST_SUITE_START_RE
            .get_or_init(|| Regex::new(r"^Test Suite '.+' started at ").unwrap());
        let xctest_suite_end = XCTEST_SUITE_END_RE.get_or_init(|| {
            Regex::new(r"^Test Suite '(?P<name>.+)' (?:passed|failed) at ").unwrap()
        });
        let swift_testing_run_start = SWIFT_TESTING_RUN_START_RE
            .get_or_init(|| Regex::new(r"^◇ Test run started\.").unwrap());
        let suite_start = SWIFT_TESTING_SUITE_START_RE.get_or_init(|| {
            Regex::new(r#"^◇ Suite (?:"(?P<quoted>.+?)"|(?P<bare>\S+)) started\."#).unwrap()
        });
        let suite_end = SWIFT_TESTING_SUITE_END_RE.get_or_init(|| {
            Regex::new(r#"^[✘✔━] Suite (?:"(?P<quoted>.+?)"|(?P<bare>\S+)) (?:passed|failed)"#)
                .unwrap()
        });
        let xctest_summary = XCTEST_SUMMARY_RE.get_or_init(|| {
            // Two observed shapes (skip clause only present when tests were
            // skipped): `Executed 20 tests, with 0 failures (0 unexpected)`
            // and `Executed 72 tests, with 2 tests skipped and 0 failures
            // (0 unexpected)`. Named groups because the optional skip clause
            // would otherwise shift positional indices only on skip lines.
            Regex::new(
                r"^\s*Executed (?P<executed>\d+) tests?, with (?:(?P<skipped>\d+) tests? skipped and )?(?P<failures>\d+) failures? \((?P<unexpected>\d+) unexpected\)",
            )
            .unwrap()
        });
        // XCTest per-test line, started or terminal: `Test Case '-[Suite
        // test]' started.` / `... passed (0.001 seconds).` / `failed` /
        // `skipped`. Closed vocabulary: no other terminal word has been seen
        // in real xcodebuild output. Counted independently of
        // the summary lines so the two can be cross-checked.
        let xctest_case_line = XCTEST_CASE_LINE_RE.get_or_init(|| {
            Regex::new(
                r"^Test Case '(?P<id>[^']+)' (?:started\.|(?P<outcome>passed|failed|skipped) \()",
            )
            .unwrap()
        });
        // XCTest bundle boundary: `Test Suite 'Name.xctest' started|passed|
        // failed at ...`. Class-level and `All tests` / `Selected tests`
        // lines are not matched; only the bundle's own `Executed` line (the
        // one after its end line) sums across a run.
        let xctest_bundle_event = XCTEST_BUNDLE_EVENT_RE.get_or_init(|| {
            Regex::new(
                r"^Test Suite '(?P<name>[^']+\.xctest)' (?P<event>started|passed|failed) at ",
            )
            .unwrap()
        });
        // Set when the previous line ended the open bundle: the `Executed`
        // line that follows is that bundle's total.
        let mut bundle_ended_on_previous_line = false;
        let swift_testing_run_summary = SWIFT_TESTING_RUN_SUMMARY_RE.get_or_init(|| {
            Regex::new(
                r"^[✘✔━] Test run with (?P<tests>\d+) tests? in \d+ suites? (?P<outcome>passed|failed)",
            )
            .unwrap()
        });
        let swift_testing_run_issues = SWIFT_TESTING_RUN_ISSUES_RE.get_or_init(|| {
            Regex::new(r" with (?P<issues>\d+) (?P<known>known )?issues?(?: \([^)]*\))?\.$")
                .unwrap()
        });
        let swift_testing_issue_summary = SWIFT_TESTING_ISSUE_SUMMARY_RE.get_or_init(|| {
            Regex::new(
                r"^✘ (?P<scope>Test|Suite) .+ failed .* with (?P<issues>\d+) issues?(?: \([^)]*\))?\.$",
            )
            .unwrap()
        });
        // Swift Testing per-test skip line, both shapes: `➜ Test foo() skipped.`
        // and `➜ Test foo() skipped: "reason"`. Test-name alternation mirrors
        // the issue-line regex: quoted display name OR identifier-with-parens.
        // Deliberately `Test`-only — a disabled suite emits BOTH a
        // `➜ Suite X skipped` line AND one `➜ Test ... skipped` line per member
        // (observed, library version 2077), so counting suite lines would
        // double-count. `➜` alone is not distinctive: xcodebuild build output
        // also uses it (`➜ Explicit dependency on target ...`).
        let swift_testing_skip = SWIFT_TESTING_SKIP_RE
            .get_or_init(|| Regex::new(r#"^➜ Test (?:"[^"]+"|.+?\([^)]*\)) skipped[.:]"#).unwrap());
        // Swift Testing emits three test-name shapes in the issue line:
        //   - Bare function: `Test foo() recorded an issue at ...`
        //   - Parameterized: `Test foo(_:) recorded an issue at ...` (arg-label form)
        //   - Display name:  `Test "My display name" recorded an issue at ...`
        // A `\(\)`-only regex would drop the latter two. The alternation
        // accepts either a quoted display-name run OR an identifier-with-parens
        // form (any content inside the parens, lazy).
        let swift_testing_issue = SWIFT_TESTING_ISSUE_RE.get_or_init(|| {
            Regex::new(
                r#"^✘ Test (?P<test>"[^"]+"|.+?\([^)]*\)) recorded an issue at (?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<msg>.+)$"#,
            )
            .unwrap()
        });
        let xctest_assertion = XCTEST_ASSERTION_RE.get_or_init(|| {
            Regex::new(
                r"^(?P<file>[^:]+):(?P<line>\d+): error: -\[(?P<test>[^\]]+)\] : (?P<msg>.+)$",
            )
            .unwrap()
        });
        // Compile-error shape applies to any language xcodebuild builds —
        // Swift, Objective-C/C/C++, module maps, header files, and the
        // synthetic `<unknown>:0:0:` form. Restricting by `.swift` extension
        // would silently drop diagnostics from mixed-language projects. The
        // `:col:` requirement still
        // distinguishes this from the XCTest assertion pattern above, which
        // uses `file:line:` with no column.
        let compile_error = COMPILE_ERROR_RE.get_or_init(|| {
            Regex::new(r"^(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): error: (?P<msg>.+)$").unwrap()
        });
        let ld_error = LD_ERROR_RE.get_or_init(|| Regex::new(r"^ld: (?P<msg>.+)$").unwrap());
        let undefined_symbols = UNDEFINED_SYMBOLS_RE.get_or_init(|| {
            Regex::new(r"^Undefined symbols for architecture (?P<arch>\S+):$").unwrap()
        });
        let symbol_reference = SYMBOL_REFERENCE_RE
            .get_or_init(|| Regex::new(r#"^\s+"(?P<symbol>[^"]+)", referenced from:"#).unwrap());

        for (idx, line) in lines.iter().enumerate() {
            if consumed.contains(&idx) {
                continue;
            }
            let bundle_ended_on_line_before = bundle_ended_on_previous_line;
            bundle_ended_on_previous_line = false;
            if line.text.starts_with("Test Suite '")
                && let Some(caps) = xctest_bundle_event.captures(&line.text)
            {
                let name = &caps["name"];
                if &caps["event"] == "started" {
                    // A bundle still open here never reached its end line
                    // (crashed; xcodebuild relaunched it). It stays in the
                    // list unrecovered and the new occurrence takes over.
                    run.xctest_bundles.push(XctestBundleRun {
                        name: name.to_owned(),
                        ..XctestBundleRun::default()
                    });
                } else if run
                    .open_xctest_bundle()
                    .is_some_and(|bundle| bundle.name == name)
                {
                    bundle_ended_on_previous_line = true;
                }
            }
            // Phase boundary detection.
            if run.test_phase_start.is_none()
                && (xctest_suite_start.is_match(&line.text)
                    || swift_testing_run_start.is_match(&line.text))
            {
                run.test_phase_start = Some(line.number);
            }
            if swift_testing_run_start.is_match(&line.text) {
                // xcodebuild restarts Swift Testing after crashes/timeouts. A
                // previous launch may never emit suite-end markers, so its
                // stack cannot carry into the next harness run.
                run.suite_stack.clear();
                swift_run_epoch = swift_run_epoch.saturating_add(1);
            }
            if is_outer_xctest_suite_end(&line.text, xctest_suite_end)
                && next_nonempty_line(&lines, idx + 1)
                    .is_none_or(|next| !xctest_summary.is_match(&next.text))
            {
                run.xctest_summary_scope_ambiguous = true;
            }
            if (line.text.starts_with("✘ Test ") || line.text.starts_with("✘ Suite "))
                && line.text.contains(" recorded an issue")
            {
                run.swift_observed_issue_markers += 1;
                let count = swift_issue_marker_count_by_epoch
                    .entry(swift_run_epoch)
                    .or_insert(0);
                *count = count.saturating_add(1);
            }
            if let Some(caps) = swift_testing_issue_summary.captures(&line.text) {
                let issues = caps["issues"].parse().unwrap_or(0);
                if &caps["scope"] == "Test" && !line.text.starts_with("✘ Test run with ") {
                    // The broad issue-summary shape also matches the top-level
                    // `Test run with ... issues` envelope. That total is
                    // reconciled separately below; only concrete per-test
                    // summaries are disjoint and therefore additive here.
                    run.swift_reported_test_issue_count =
                        run.swift_reported_test_issue_count.saturating_add(issues);
                    let count = swift_test_issue_count_by_epoch
                        .entry(swift_run_epoch)
                        .or_insert(0);
                    *count = count.saturating_add(issues);
                } else if &caps["scope"] == "Suite" {
                    // Suite summaries within one harness run are envelopes and
                    // may be nested, so only their maximum is a safe lower
                    // bound. Separate harness epochs are disjoint and their
                    // per-epoch lower bounds are additive.
                    let epoch_lower_bound = swift_suite_issue_lower_bound_by_epoch
                        .entry(swift_run_epoch)
                        .or_insert(0);
                    *epoch_lower_bound = (*epoch_lower_bound).max(issues);
                    let count = swift_suite_summaries_by_epoch
                        .entry(swift_run_epoch)
                        .or_insert(0);
                    *count = count.saturating_add(1);
                }
            }

            // Swift Testing suite-stack tracking. Push on suite start (both quoted
            // `"FooTests"` and unquoted bareword forms exist in real output);
            // pop on the matching suite pass/fail line.
            if let Some(caps) = suite_start.captures(&line.text) {
                let name = caps
                    .name("quoted")
                    .or_else(|| caps.name("bare"))
                    .map(|m| m.as_str().to_owned())
                    .unwrap_or_default();
                if !name.is_empty() {
                    if !run.suite_stack.is_empty() {
                        swift_overlapping_suite_epochs.insert(swift_run_epoch);
                    }
                    run.suite_stack.push(name);
                }
            }
            if let Some(caps) = suite_end.captures(&line.text) {
                let name = caps
                    .name("quoted")
                    .or_else(|| caps.name("bare"))
                    .map(|m| m.as_str().to_owned())
                    .unwrap_or_default();
                if run.suite_stack.last() != Some(&name) {
                    // Concurrent/interleaved or malformed suite envelopes make
                    // additive issue accounting ambiguous. Fail closed instead
                    // of popping the wrong scope and crediting later summaries
                    // to a fabricated hierarchy.
                    run.swift_suite_scope_ambiguous = true;
                    run.suite_stack.clear();
                } else {
                    run.suite_stack.pop();
                }
            }

            // Marker accounting. `xcodebuild test` emits `** TEST SUCCEEDED/
            // FAILED **`; `xcodebuild test-without-building` emits the
            // `** TEST EXECUTE SUCCEEDED/FAILED **` variants. (`** TEST BUILD
            // SUCCEEDED **` from build-for-testing is deliberately not
            // matched — builds are out of scope.)
            if line.text.starts_with("** TEST SUCCEEDED **")
                || line.text.starts_with("** TEST EXECUTE SUCCEEDED **")
            {
                run.saw_test_succeeded = true;
            }
            if line.text.starts_with("** TEST FAILED **")
                || line.text.starts_with("** TEST EXECUTE FAILED **")
            {
                run.saw_test_failed = true;
            }
            if line.text.starts_with("Testing failed:") {
                run.saw_testing_failed_summary = true;
            }
            if line.text.starts_with("◇ Test run started.") {
                run.saw_swift_testing_harness = true;
                run.swift_testing_bundle_skipped = 0;
            }
            if line.text.starts_with("Test Suite '") {
                run.saw_xctest_envelope = true;
            }
            if line
                .text
                .starts_with("Restarting after unexpected exit, crash, or test timeout")
            {
                run.saw_test_runner_restart = true;
            }
            if (line.text.contains("Mach error")
                || line
                    .text
                    .contains("Failed to install or launch the test runner"))
                && run.test_phase_start.is_none()
            {
                run.saw_runtime_environment_failure = true;
            }

            // XCTest summary: "Executed N tests, with [S tests skipped and]
            // M failures ..."
            if let Some(caps) = xctest_summary.captures(&line.text) {
                let failures: u64 = caps["failures"].parse().unwrap_or(0);
                run.xctest_reported_failure_incidents =
                    run.xctest_reported_failure_incidents.max(failures);
                if previous_nonempty_line(&lines, idx).is_some_and(|previous| {
                    is_outer_xctest_suite_end(&previous.text, xctest_suite_end)
                }) {
                    run.xctest_outer_summary_count += 1;
                    run.xctest_outer_reported_failure_incidents = run
                        .xctest_outer_reported_failure_incidents
                        .saturating_add(failures);
                }
                let executed: u64 = caps["executed"].parse().unwrap_or(0);
                let skipped: u64 = caps
                    .name("skipped")
                    .map(|m| m.as_str().parse().unwrap_or(0))
                    .unwrap_or(0);
                // xcodebuild emits one `Executed` line per suite level: each
                // test class, each `.xctest` bundle, and the `All tests` /
                // `Selected tests` envelope around one bundle. The bundle
                // line closes the open occurrence; class lines inside it are
                // kept as its fallback; the envelope duplicates the bundle
                // line and is ignored once bundles exist. Harness totals are
                // combined after the loop.
                if let Some(bundle) = run.open_xctest_bundle() {
                    if bundle_ended_on_line_before {
                        bundle.total = Some((executed, skipped));
                    } else if executed > bundle.inner_max.0 {
                        bundle.inner_max = (executed, skipped);
                    }
                }
                if executed > run.xctest_summary_max.0 {
                    run.xctest_summary_max = (executed, skipped);
                }
            }

            // XCTest per-test line, started or terminal, credited to the open
            // bundle occurrence. The started line already inserted the id, so
            // the terminal line usually allocates nothing.
            if line.text.starts_with("Test Case '")
                && let Some(caps) = xctest_case_line.captures(&line.text)
            {
                let id = &caps["id"];
                let skipped = caps
                    .name("outcome")
                    .is_some_and(|m| m.as_str() == "skipped");
                let (ids, skipped_ids) = match run.open_xctest_bundle() {
                    Some(bundle) => (&mut bundle.case_ids, &mut bundle.skipped_case_ids),
                    None => (
                        &mut run.xctest_orphan_case_ids,
                        &mut run.xctest_orphan_skipped_case_ids,
                    ),
                };
                if !ids.contains(id) {
                    ids.insert(id.to_owned());
                }
                if skipped {
                    skipped_ids.insert(id.to_owned());
                }
            }

            // Swift Testing per-test skip marker. Counted as encountered; the
            // bundle's run summary (which arrives last and includes skipped
            // tests in its `N tests` total) decides whether the count is used.
            if swift_testing_skip.is_match(&line.text) {
                run.swift_testing_bundle_skipped += 1;
            }

            // Swift Testing run summary: "✘ Test run with N tests in M suites failed"
            // or "✔ Test run with N tests in M suites passed". Unlike the
            // XCTest lines above, each of these summarizes a whole separate
            // bundle — none is a superset of another — so they sum. Pairing a
            // single winning summary with the whole run cannot work: `failures[]`
            // spans every bundle and invariant 11 derives `tests_failed` from
            // it, so a per-bundle total would be smaller than the failures it
            // has to account for. Equal-sized bundles made that concrete: the
            // strict `>` skipped the second summary outright and invariant 12
            // rejected the document.
            if let Some(caps) = swift_testing_run_summary.captures(&line.text) {
                let executed: u64 = caps["tests"].parse().unwrap_or(0);
                let outcome = &caps["outcome"];
                let issues = swift_testing_run_issues.captures(&line.text);
                let reported_issues = issues
                    .as_ref()
                    .filter(|suffix| outcome != "passed" || suffix.name("known").is_none())
                    .and_then(|suffix| suffix.name("issues"))
                    .and_then(|value| value.as_str().parse::<u64>().ok())
                    .unwrap_or(0);
                if issues.is_some() {
                    swift_epochs_with_run_issue_total.insert(swift_run_epoch);
                }
                run.swift_reported_issue_count = run
                    .swift_reported_issue_count
                    .saturating_add(reported_issues);
                let count = swift_run_issue_count_by_epoch
                    .entry(swift_run_epoch)
                    .or_insert(0);
                *count = count.saturating_add(reported_issues);
                run.swift_testing_total_run += executed;
                // The summary's `N tests` includes skipped tests (observed:
                // 1 passed + 5 skipped reports "6 tests ... passed"), and it is
                // emitted after every skip line — this bundle's per-test
                // counter is complete here, so the two totals stay paired.
                run.swift_testing_total_skipped += run.swift_testing_bundle_skipped;
                run.swift_testing_bundle_skipped = 0;
            }

            // Failure extraction — try each pattern in priority order.

            // 0. xcodebuild destination resolver failure. This happens before
            // the build or test phases and comes with a structured bad-spec +
            // available-destinations block.
            //
            // The anchor accepts three observed phrasings: `Unable to find a device matching ...` (the captured
            // shape), `Unable to find a destination matching ...` (variant),
            // and a leading-clause form `Could not configure request ...:
            // Unable to find a (device|destination) matching ...`. All three
            // wrap the same downstream block (bad-spec + Available destinations),
            // so once admitted the rest of the parser is shared.
            if run.test_phase_start.is_none() && destination_error_anchor().is_match(&line.text) {
                let mut block_end_line = line.number;
                let mut block_end_byte = line.end;
                let mut block_text = line.raw_text().into_owned();
                let mut bad_spec = None;
                let mut available_destinations = Vec::new();
                let mut in_available_destinations = false;
                let mut saw_available_header = false;
                let mut peek = idx + 1;
                while let Some(peek_line) = lines.get(peek) {
                    let trimmed = peek_line.text.trim_start();
                    // Symmetric early-exit with the linker block (xcodebuild.rs
                    // §"Undefined symbols for architecture"): once we've left
                    // the available-destinations block, stop consuming. Without
                    // this, an anchor-but-no-header log (truncation, future
                    // xcodebuild phrasing) would consume to EOF.
                    if saw_available_header
                        && in_available_destinations
                        && !trimmed.is_empty()
                        && !trimmed.starts_with('{')
                    {
                        break;
                    }
                    // Skip the brace parse on lines that can't carry a spec we
                    // still need: pre-anchor preamble after bad_spec is set, and
                    // anything outside the available-destinations block.
                    let destination_fields = if bad_spec.is_none() || in_available_destinations {
                        parse_braced_destination_fields(&peek_line.text)
                    } else {
                        None
                    };
                    if bad_spec.is_none()
                        && let Some(fields) = destination_fields.as_ref()
                    {
                        bad_spec = Some(DestinationSpec {
                            fields: fields.clone(),
                        });
                    }
                    if trimmed.starts_with("Available destinations for the ") {
                        saw_available_header = true;
                        in_available_destinations = true;
                    } else if in_available_destinations
                        && let Some(fields) = destination_fields
                        && let Some(destination) = AvailableDestination::from_fields(&fields)
                    {
                        available_destinations.push(destination);
                    }

                    block_text.push('\n');
                    block_text.push_str(&peek_line.raw_text());
                    consumed.insert(peek);
                    block_end_line = peek_line.number;
                    block_end_byte = peek_line.end;
                    peek += 1;
                }

                let raw_log_lines = [line.number, block_end_line];
                let byte_range = ByteRange {
                    start: line.start,
                    end: block_end_byte,
                };
                run.failures.push(ParsedFailure {
                    phase: Phase::Setup,
                    kind: FailureKind::DependencyResolutionError,
                    runner_test_id: None,
                    test_name: None,
                    test_segments: vec![],
                    file: "<xcodebuild>".to_owned(),
                    line: None,
                    column: None,
                    // The only failure whose message is a whole log line
                    // rather than a regex capture. It takes the matching view
                    // like every other message: messages feed failure_id, and
                    // a decorative ZWSP must not move one.
                    message: line.text.clone(),
                    raw_log_lines,
                    byte_range,
                    excerpt_text: block_text,
                    unresolved_symbols: vec![],
                });
                if let Some(bad_spec) = bad_spec {
                    run.destination_resolution = Some(DestinationResolution {
                        bad_spec,
                        available_destinations,
                    });
                }
                continue;
            }

            // 1. Swift Testing: ✘ Test foo() recorded an issue at file:line:col: <message>
            if let Some(caps) = swift_testing_issue.captures(&line.text) {
                let test = caps["test"].to_owned();
                let file = caps["file"].to_owned();
                let lineno: u64 = caps["line"].parse().unwrap_or(0);
                let col: u64 = caps["col"].parse().unwrap_or(0);
                let mut message = caps["msg"].to_owned();
                let mut raw_log_end_line = line.number;
                let mut byte_range_end = line.end;
                let mut excerpt_text = line.raw_text().into_owned();
                // Lookahead for contiguous ↳ continuation lines (user messages
                // under #expect). Message and evidence must extend together —
                // the message from the matching view, the excerpt from the raw.
                let mut peek = idx + 1;
                while let Some(next) = lines.get(peek)
                    && let Some(stripped) = next.text.strip_prefix("↳ ")
                    && !stripped.starts_with("Testing Library Version")
                {
                    message.push_str(": ");
                    message.push_str(stripped);
                    excerpt_text.push('\n');
                    excerpt_text.push_str(&next.raw_text());
                    consumed.insert(peek);
                    raw_log_end_line = next.number;
                    byte_range_end = next.end;
                    peek += 1;
                }
                let phase = Phase::Test;
                let kind = classify_swift_testing_kind(&message);
                let raw_log_lines = [line.number, raw_log_end_line];
                let byte_range = ByteRange {
                    start: line.start,
                    end: byte_range_end,
                };
                let test_id = normalize_swift_testing_test_id(&test);
                let qualified_test_id = if run.suite_stack.is_empty() {
                    test_id.clone()
                } else {
                    format!("{}.{}", run.suite_stack.join("."), test_id)
                };
                let mut segments = run.suite_stack.clone();
                segments.push(test_id.clone());
                run.failures.push(ParsedFailure {
                    phase,
                    kind,
                    runner_test_id: Some(qualified_test_id),
                    test_name: Some(test_id),
                    test_segments: segments,
                    file,
                    line: Some(lineno),
                    column: Some(col),
                    message,
                    raw_log_lines,
                    byte_range,
                    excerpt_text,
                    unresolved_symbols: vec![],
                });
                run.swift_extracted_issue_count += 1;
                continue;
            }

            // 2. Legacy XCTest assertion: <file>:<line>: error: -[Class testFoo] : XCTAssert*: <message>
            if let Some(caps) = xctest_assertion.captures(&line.text) {
                let file = caps["file"].to_owned();
                let lineno: u64 = caps["line"].parse().unwrap_or(0);
                let test = caps["test"].to_owned();
                let message = caps["msg"].to_owned();
                let test_id = normalize_xctest_test_id(&test);
                let segments: Vec<String> = test_id.split('.').map(|s| s.to_owned()).collect();
                run.failures.push(ParsedFailure {
                    phase: Phase::Test,
                    kind: FailureKind::Assertion,
                    runner_test_id: Some(test_id.clone()),
                    test_name: segments.last().cloned(),
                    test_segments: segments,
                    file,
                    line: Some(lineno),
                    column: None,
                    message,
                    raw_log_lines: [line.number, line.number],
                    byte_range: ByteRange {
                        start: line.start,
                        end: line.end,
                    },
                    excerpt_text: line.raw_text().into_owned(),
                    unresolved_symbols: vec![],
                });
                run.xctest_extracted_failure_incidents += 1;
                continue;
            }

            // 3. Swift compile error: <file>:<line>:<col>: error: <message>
            // Only when we have NOT yet entered the test phase (build errors).
            if run.test_phase_start.is_none() {
                if let Some(caps) = compile_error.captures(&line.text) {
                    let file = caps["file"].to_owned();
                    let lineno: u64 = caps["line"].parse().unwrap_or(0);
                    let col: u64 = caps["col"].parse().unwrap_or(0);
                    let message = caps["msg"].to_owned();
                    let kind = classify_compile_error_kind(&message);
                    run.failures.push(ParsedFailure {
                        phase: Phase::Build,
                        kind,
                        runner_test_id: None,
                        test_name: None,
                        test_segments: vec![],
                        file,
                        line: Some(lineno),
                        column: Some(col),
                        message,
                        raw_log_lines: [line.number, line.number],
                        byte_range: ByteRange {
                            start: line.start,
                            end: line.end,
                        },
                        excerpt_text: line.raw_text().into_owned(),
                        unresolved_symbols: vec![],
                    });
                    continue;
                }

                // 4. Linker error — single-line `ld: <message>` form.
                // e.g. `ld: framework not found Foo`. No source location.
                if let Some(caps) = ld_error.captures(&line.text) {
                    let message = caps["msg"].to_owned();
                    if message.trim_start().starts_with("warning:") {
                        continue;
                    }
                    run.failures.push(ParsedFailure {
                        phase: Phase::Build,
                        kind: FailureKind::LinkerError,
                        runner_test_id: None,
                        test_name: None,
                        test_segments: vec![],
                        file: "<linker>".to_owned(),
                        line: None,
                        column: None,
                        message,
                        raw_log_lines: [line.number, line.number],
                        byte_range: ByteRange {
                            start: line.start,
                            end: line.end,
                        },
                        excerpt_text: line.raw_text().into_owned(),
                        unresolved_symbols: vec![],
                    });
                    continue;
                }

                // 5. Linker error — multi-line `Undefined symbols for architecture` block.
                // Block shape:
                //   Undefined symbols for architecture arm64:
                //     "_FooFunction", referenced from:
                //         _BarFunction in BarLib(BarLib.o)
                //     "_BazFunction", referenced from:
                //         ...
                //   ld: symbol(s) not found for architecture arm64
                // We scan ahead until a non-indented line; the `ld: symbol(s) not found`
                // line is a sibling, not part of the block.
                if let Some(caps) = undefined_symbols.captures(&line.text) {
                    let arch = caps["arch"].to_owned();
                    let mut symbols: Vec<String> = vec![];
                    let mut block_end_line = line.number;
                    let mut block_end_byte = line.end;
                    // Collect every line of the block (header + indented
                    // referenced-from continuation lines) so the excerpt
                    // reflects the same byte range the failure record claims.
                    // A single-line excerpt would drop the symbol-name and
                    // referenced-from context.
                    let mut block_text = String::new();
                    block_text.push_str(&line.raw_text());
                    let mut peek = idx + 1;
                    while peek < lines.len() {
                        let peek_line = &lines[peek];
                        // Block ends when we hit a non-indented line. Treat any
                        // leading whitespace as continuation; some toolchains
                        // or pipes use tabs here.
                        if !peek_line
                            .text
                            .chars()
                            .next()
                            .is_some_and(char::is_whitespace)
                        {
                            break;
                        }
                        if let Some(symcaps) = symbol_reference.captures(&peek_line.text) {
                            symbols.push(symcaps["symbol"].to_owned());
                        }
                        block_text.push('\n');
                        block_text.push_str(&peek_line.raw_text());
                        consumed.insert(peek);
                        block_end_line = peek_line.number;
                        block_end_byte = peek_line.end;
                        peek += 1;
                    }
                    let message = if symbols.is_empty() {
                        format!("Undefined symbols for architecture {arch}")
                    } else if symbols.len() == 1 {
                        format!("Undefined symbol for architecture {arch}: {}", symbols[0])
                    } else {
                        format!(
                            "Undefined symbols for architecture {arch}: {} (+{} more)",
                            symbols[0],
                            symbols.len() - 1
                        )
                    };
                    run.failures.push(ParsedFailure {
                        phase: Phase::Build,
                        kind: FailureKind::LinkerError,
                        runner_test_id: None,
                        test_name: None,
                        test_segments: vec![],
                        file: "<linker>".to_owned(),
                        line: None,
                        column: None,
                        message,
                        raw_log_lines: [line.number, block_end_line],
                        byte_range: ByteRange {
                            start: line.start,
                            end: block_end_byte,
                        },
                        excerpt_text: block_text,
                        unresolved_symbols: symbols,
                    });
                    continue;
                }
            }
        }

        if run.xctest_outer_summary_count > 0 {
            run.xctest_reported_failure_incidents = run
                .xctest_reported_failure_incidents
                .max(run.xctest_outer_reported_failure_incidents);
        }
        let swift_issue_epochs = swift_run_issue_count_by_epoch
            .keys()
            .chain(swift_test_issue_count_by_epoch.keys())
            .chain(swift_suite_issue_lower_bound_by_epoch.keys())
            .chain(swift_issue_marker_count_by_epoch.keys())
            .copied()
            .collect::<BTreeSet<_>>();
        run.swift_reported_issue_lower_bound = swift_issue_epochs
            .iter()
            .map(|epoch| {
                [
                    swift_run_issue_count_by_epoch
                        .get(epoch)
                        .copied()
                        .unwrap_or(0),
                    swift_test_issue_count_by_epoch
                        .get(epoch)
                        .copied()
                        .unwrap_or(0),
                    swift_suite_issue_lower_bound_by_epoch
                        .get(epoch)
                        .copied()
                        .unwrap_or(0),
                    swift_issue_marker_count_by_epoch
                        .get(epoch)
                        .copied()
                        .unwrap_or(0),
                ]
                .into_iter()
                .max()
                .unwrap_or(0)
            })
            .fold(0_u64, u64::saturating_add);
        let has_ambiguous_swift_epoch =
            swift_suite_summaries_by_epoch
                .iter()
                .any(|(epoch, summary_count)| {
                    *summary_count > 1 && !swift_epochs_with_run_issue_total.contains(epoch)
                })
                || swift_overlapping_suite_epochs
                    .iter()
                    .any(|epoch| !swift_epochs_with_run_issue_total.contains(epoch));
        if has_ambiguous_swift_epoch {
            // Swift Testing does not expose parent IDs on suite events, and
            // parallel siblings can look exactly like temporal nesting. In the
            // absence of a run-level issue total, multiple suite envelopes
            // cannot prove an exhaustive count.
            run.swift_suite_scope_ambiguous = true;
        }

        // Dedupe BUILD-phase failures by (file, line, message) — xcodebuild
        // often reports the same compile error multiple times (one per failed
        // target). Test-phase failures must NOT be deduped this way: two
        // distinct tests calling the same assertion helper would land at the
        // same file:line and collapse into one, hiding a real failure. Synthetic
        // `<linker>` failures are also kept distinct: universal builds can emit
        // identical `ld:` text per architecture, and the duplicate is signal.
        let (mut build_failures, test_failures): (Vec<_>, Vec<_>) =
            std::mem::take(&mut run.failures)
                .into_iter()
                .partition(|f| f.phase == Phase::Build);
        build_failures.sort_by(|a, b| {
            (&a.file, a.line, &a.message, a.raw_log_lines[0]).cmp(&(
                &b.file,
                b.line,
                &b.message,
                b.raw_log_lines[0],
            ))
        });
        let mut deduped_build_failures: Vec<ParsedFailure> = Vec::new();
        for failure in build_failures {
            let duplicate_compile_failure = failure.file != "<linker>"
                && deduped_build_failures.last().is_some_and(|previous| {
                    previous.file == failure.file
                        && previous.line == failure.line
                        && previous.message == failure.message
                });
            if !duplicate_compile_failure {
                deduped_build_failures.push(failure);
            }
        }
        run.failures = deduped_build_failures
            .into_iter()
            .chain(test_failures)
            .collect();

        // Publish the run/skip totals: XCTest plus Swift Testing. The two
        // harnesses report disjoint populations: the XCTest envelope of a
        // Swift Testing bundle says `Executed 0 tests` (green_passing
        // fixture, 152 Swift Testing tests), so the sum double-counts
        // nothing there. No log with both harnesses executing tests has been
        // captured; taking the larger total would drop the smaller harness
        // in one.
        let (xctest_run, xctest_skipped) = run.xctest_totals();
        run.tests_run = xctest_run + run.swift_testing_total_run;
        run.tests_skipped = xctest_skipped + run.swift_testing_total_skipped;

        run
    }

    /// Whether the log shows enough xcodebuild structure to be ours.
    fn looks_like_xcodebuild(&self) -> bool {
        // Build-only failures can lack `** TEST ... **` / XCTest / Swift
        // Testing markers. Admitting `!failures.is_empty()` is safe because
        // analyze_log first requires a sidecar that selected this adapter from
        // xcodebuild argv. If dispatcher selection changes, revisit this gate.
        // `saw_runtime_environment_failure` (Mach error / Failed to install or
        // launch the test runner) admits truncated sim-launch failures where
        // the trailing `Testing failed:` summary was cut off. Without this,
        // the intended HarnessCrashed + MissingTerminalEvent path never fires
        // for that truncated shape.
        !self.failures.is_empty()
            || self.saw_test_succeeded
            || self.saw_test_failed
            || self.saw_xctest_envelope
            || self.saw_swift_testing_harness
            || self.saw_testing_failed_summary
            || self.saw_runtime_environment_failure
    }
}

// Regex caches — initialized once per process.
static SWIFT_TESTING_ISSUE_RE: OnceLock<Regex> = OnceLock::new();
static COMPILE_ERROR_RE: OnceLock<Regex> = OnceLock::new();
static XCTEST_ASSERTION_RE: OnceLock<Regex> = OnceLock::new();
static XCTEST_SUITE_START_RE: OnceLock<Regex> = OnceLock::new();
static XCTEST_SUITE_END_RE: OnceLock<Regex> = OnceLock::new();
static SWIFT_TESTING_RUN_START_RE: OnceLock<Regex> = OnceLock::new();
static XCTEST_SUMMARY_RE: OnceLock<Regex> = OnceLock::new();
static XCTEST_CASE_LINE_RE: OnceLock<Regex> = OnceLock::new();
static XCTEST_BUNDLE_EVENT_RE: OnceLock<Regex> = OnceLock::new();
static SWIFT_TESTING_RUN_SUMMARY_RE: OnceLock<Regex> = OnceLock::new();
static SWIFT_TESTING_RUN_ISSUES_RE: OnceLock<Regex> = OnceLock::new();
static SWIFT_TESTING_ISSUE_SUMMARY_RE: OnceLock<Regex> = OnceLock::new();
static SWIFT_TESTING_SKIP_RE: OnceLock<Regex> = OnceLock::new();
static SWIFT_TESTING_SUITE_START_RE: OnceLock<Regex> = OnceLock::new();
static SWIFT_TESTING_SUITE_END_RE: OnceLock<Regex> = OnceLock::new();
static LD_ERROR_RE: OnceLock<Regex> = OnceLock::new();
static UNDEFINED_SYMBOLS_RE: OnceLock<Regex> = OnceLock::new();
static SYMBOL_REFERENCE_RE: OnceLock<Regex> = OnceLock::new();
static SWIFT_PRIVATE_ACCESS_RE: OnceLock<Regex> = OnceLock::new();
static SWIFT_ILLEGAL_TRY_RE: OnceLock<Regex> = OnceLock::new();
static SWIFT_MISSING_TRY_RE: OnceLock<Regex> = OnceLock::new();
static DESTINATION_ERROR_ANCHOR_RE: OnceLock<Regex> = OnceLock::new();

fn destination_error_anchor() -> &'static Regex {
    // Matches the xcodebuild destination-resolution error anchor across
    // observed phrasings:
    //   - `xcodebuild: error: Unable to find a device matching ...`
    //   - `xcodebuild: error: Unable to find a destination matching ...`
    //   - `xcodebuild: error: Could not configure request ...: Unable to find a
    //     (device|destination) matching ...`
    // The `xcodebuild: error:` prefix is required so we don't admit
    // user-printed text that happens to contain the inner phrase.
    DESTINATION_ERROR_ANCHOR_RE.get_or_init(|| {
        Regex::new(r"^xcodebuild: error: .*\bUnable to find a (?:device|destination) matching\b")
            .unwrap()
    })
}

fn classify_compile_error_kind(message: &str) -> FailureKind {
    let lower = message.to_lowercase();
    // Deliberately tightens on `uses a private type`; widen when internal/public escalation variants are observed.
    if swift_private_access_re().is_match(&lower) {
        return FailureKind::TypeError;
    }
    if swift_illegal_try_re().is_match(&lower) {
        return FailureKind::SyntaxError;
    }
    if swift_missing_try_re().is_match(&lower) {
        return FailureKind::TypeError;
    }
    // Missing-symbol family. Heuristics cover Swift, Clang (Objective-C / C /
    // C++), and module-map diagnostics:
    //   - Swift 5+: `cannot find 'X' in scope`
    //   - Older Swift: `use of unresolved identifier`
    //   - Clang: `use of undeclared identifier`, `undeclared selector` (Obj-C)
    //   - Swift/Clang: `is not a member` (member access on wrong type)
    //   - Swift module imports: `no such module`
    //   - Clang #include: `file not found` for header / module
    let missing_symbol = (lower.starts_with("cannot find ") && lower.contains(" in scope"))
        || lower.starts_with("use of unresolved identifier")
        || lower.starts_with("use of undeclared identifier")
        || lower.contains("undeclared selector")
        || lower.contains("is not a member")
        || lower.contains("no such module")
        || lower.contains("file not found");
    if missing_symbol {
        return FailureKind::MissingSymbol;
    }
    if lower.contains("linker command failed") || lower.contains("undefined symbol") {
        return FailureKind::LinkerError;
    }
    if lower.starts_with("expected ")
        || lower.contains("consecutive declarations")
        || lower.contains("expected expression")
    {
        return FailureKind::SyntaxError;
    }
    if lower.contains("cannot convert") || lower.contains("cannot assign") {
        return FailureKind::TypeError;
    }
    FailureKind::Unknown
}

fn swift_private_access_re() -> &'static Regex {
    SWIFT_PRIVATE_ACCESS_RE.get_or_init(|| {
        Regex::new(r"^\w+ must be declared private because its \w+ uses a private type\b").unwrap()
    })
}

fn swift_illegal_try_re() -> &'static Regex {
    SWIFT_ILLEGAL_TRY_RE
        .get_or_init(|| Regex::new(r"^'try[!?]?' (?:cannot|must) appear\b").unwrap())
}

fn swift_missing_try_re() -> &'static Regex {
    SWIFT_MISSING_TRY_RE.get_or_init(|| {
        Regex::new(r"\bcan throw but expression is not marked with 'try'(?:\W|$)").unwrap()
    })
}

fn classify_swift_testing_kind(message: &str) -> FailureKind {
    let lower = message.to_lowercase();
    // `#require` failures surface as either "Required value was nil" or
    // wrap a thrown error via `Issue.error(...)` — both lower to Exception.
    // Everything else, including `Expectation failed` and the default, lowers
    // to Assertion (the dominant case).
    if lower.contains("required value was nil") || lower.contains("issue.error(") {
        FailureKind::Exception
    } else {
        FailureKind::Assertion
    }
}

/// Normalize Swift Testing test identifiers into a stable form for hashing.
/// Three input shapes appear in the wild:
///   - Bare function: `foo()` → `foo`
///   - Parameterized: `foo(_:)` → `foo(_:)` (preserve arg-label signature for
///     test ID stability — parameterized tests with different signatures must
///     remain distinguishable).
///   - Display name:  `"My test name"` → `My test name` (strip enclosing quotes).
fn normalize_swift_testing_test_id(symbol: &str) -> String {
    if symbol.len() >= 2 && symbol.starts_with('"') && symbol.ends_with('"') {
        return symbol[1..symbol.len() - 1].to_owned();
    }
    symbol.trim_end_matches("()").to_owned()
}

/// Normalize legacy XCTest `-[Module.Class testFoo]` test identifiers into the
/// dotted form `Module.Class.testFoo`.
fn normalize_xctest_test_id(bracket: &str) -> String {
    // Input is the contents of `-[...]` — e.g. `ExampleAppTests.MyClass testFoo`.
    let parts: Vec<&str> = bracket.splitn(2, ' ').collect();
    if parts.len() == 2 {
        format!("{}.{}", parts[0], parts[1])
    } else {
        bracket.to_owned()
    }
}

fn previous_nonempty_line(lines: &[LogLine], index: usize) -> Option<&LogLine> {
    lines[..index]
        .iter()
        .rev()
        .find(|line| !line.text.trim().is_empty())
}

fn next_nonempty_line(lines: &[LogLine], index: usize) -> Option<&LogLine> {
    lines[index..]
        .iter()
        .find(|line| !line.text.trim().is_empty())
}

fn is_outer_xctest_suite_end(line: &str, suite_end: &Regex) -> bool {
    suite_end
        .captures(line)
        .is_some_and(|captures| matches!(&captures["name"], "All tests" | "Selected tests"))
}

fn parse_terminal_failed_test_ids(lines: &[LogLine]) -> Vec<String> {
    let mut reported = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.text.trim() != "Failing tests:" {
            continue;
        }
        for entry in &lines[index + 1..] {
            let trimmed = entry.text.trim();
            if trimmed.is_empty() || trimmed.starts_with("** TEST") {
                break;
            }
            if !entry.text.starts_with(char::is_whitespace) {
                break;
            }
            reported.push(trimmed.trim_end_matches("()").to_owned());
        }
    }
    reported
}

fn terminal_test_ids_are_accounted(reported: &[String], extracted: &[&str]) -> bool {
    let mut used = vec![false; extracted.len()];
    for reported_id in reported {
        let Some(index) = extracted.iter().enumerate().position(|(index, candidate)| {
            !used[index]
                && (*candidate == reported_id
                    || candidate
                        .strip_suffix(reported_id)
                        .is_some_and(|prefix| prefix.ends_with('.')))
        }) else {
            return false;
        };
        used[index] = true;
    }
    true
}

fn parse_braced_destination_fields(line: &str) -> Option<BTreeMap<String, String>> {
    let trimmed = line.trim();
    let body = trimmed.strip_prefix('{')?.strip_suffix('}')?.trim();
    let fields = parse_kv_fields(body, ':');
    (!fields.is_empty()).then_some(fields)
}

/// Parse a delimited body (already brace-stripped if applicable) into a
/// key/value map. Shared by the braced log form (`{ platform:iOS, id:UUID }`,
/// separator `:`) and the argv-value form (`platform=iOS,id=UUID`,
/// separator `=`). Fields that don't contain the separator are skipped.
fn parse_kv_fields(body: &str, sep: char) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    for field in split_destination_fields(body) {
        let Some((key, value)) = field.split_once(sep) else {
            continue;
        };
        fields.insert(key.trim().to_owned(), value.trim().to_owned());
    }
    fields
}

fn split_destination_fields(body: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut start = 0usize;
    let mut bracket_depth = 0usize;
    for (idx, ch) in body.char_indices() {
        match ch {
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            ',' if bracket_depth == 0 => {
                fields.push(body[start..idx].trim());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    fields.push(body[start..].trim());
    fields
}

// ---------------------------------------------------------------------------
// Lowering to OutputDocument
// ---------------------------------------------------------------------------

fn lower(
    parsed: ParsedRun,
    log: &[u8],
    sidecar: &Sidecar,
    options: AnalyzeOptions,
) -> OutputDocument {
    let extracted_test_ids = parsed
        .failures
        .iter()
        .filter(|failure| failure.phase == Phase::Test)
        .filter_map(|failure| failure.runner_test_id.as_deref())
        .collect::<Vec<_>>();
    let terminal_failure_shortfall = !terminal_test_ids_are_accounted(
        &parsed.terminal_reported_failed_test_ids,
        &extracted_test_ids,
    );
    let failure_list_untrusted = parsed.xctest_reported_failure_incidents
        > parsed.xctest_extracted_failure_incidents
        || parsed.xctest_summary_scope_ambiguous
        || parsed.swift_suite_scope_ambiguous
        || parsed.swift_reported_issue_count > parsed.swift_extracted_issue_count
        || parsed.swift_reported_test_issue_count > parsed.swift_extracted_issue_count
        || parsed.swift_reported_issue_lower_bound > parsed.swift_extracted_issue_count
        || parsed.swift_observed_issue_markers > parsed.swift_extracted_issue_count
        || terminal_failure_shortfall
        || (parsed.saw_test_failed
            && parsed.test_phase_start.is_some()
            && !parsed
                .failures
                .iter()
                .any(|failure| failure.phase == Phase::Test));
    let mut parse_warnings = compute_parse_warnings(&parsed, sidecar);
    if failure_list_untrusted && !parse_warnings.contains(&ParseWarning::RunnerSummaryMismatch) {
        parse_warnings.push(ParseWarning::RunnerSummaryMismatch);
    }
    let parse_status = parse_status_from_warnings(&parse_warnings);
    let missing_terminal_event = parse_warnings.contains(&ParseWarning::MissingTerminalEvent);

    let runner_outcome = determine_runner_outcome(&parsed, sidecar);
    let counts = build_counts(&parsed);
    let failures_total = parsed.failures.len() as u64;
    let is_stable_across_runs = parsed.failures.iter().all(|f| f.phase != Phase::Test);
    let suggested_next_commands = suggested_destination_commands(&parsed, sidecar);
    let (failures, budgeted) = apply_failure_budget(parsed.failures, options.max_failures);
    let failures: Vec<Failure> = failures
        .into_iter()
        .map(|pf| parsed_failure_to_failure(pf, &sidecar.cwd))
        .collect();

    // Ranking stability: build-phase failures come from a deterministic
    // Swift/Clang compile (file:line ordered, stable across runs). Test-phase
    // failures execute concurrently — xcodebuild parallelizes across
    // destinations by default, and Swift Testing's harness runs tests
    // concurrently unless explicitly serialized (`-disable-concurrent-testing`,
    // `.serialized` on `@Suite`). When any test-phase failure is present, the
    // emission order reflects observed-log order, which is non-deterministic
    // across runs. This matches cargo's is_pre_test_failure-only-is-stable
    // pattern.
    let ranking = Ranking {
        strategy: if failures.is_empty() {
            RankingStrategy::None
        } else {
            RankingStrategy::CompileBeforeRuntime
        },
        is_stable_across_runs,
    };

    let parts = AnalysisDocumentParts {
        sidecar,
        log,
        budgeted_failures: BudgetedFailures {
            failures,
            failures_total,
            budgeted,
        },
        runner_outcome,
        early_exit: early_exit(sidecar),
        parse_status,
        parse_warnings,
        failures_complete: !sidecar.truncated && !failure_list_untrusted && !missing_terminal_event,
        ranking,
        counts,
        suggested_next_commands,
        extensions: Default::default(),
        options,
    };
    build_analysis_document(parts)
}

fn compute_parse_warnings(parsed: &ParsedRun, sidecar: &Sidecar) -> Vec<ParseWarning> {
    let mut warnings = Vec::new();
    if sidecar.truncated {
        warnings.push(ParseWarning::MissingTerminalEvent);
    } else if parsed.saw_runtime_environment_failure {
        // Simulator boot / launch failure happened before any test phase.
        // No dedicated ParseWarning variant exists yet; the closest existing
        // signal is `MissingTerminalEvent` — Swift Testing never started.
        warnings.push(ParseWarning::MissingTerminalEvent);
    } else if parsed.test_phase_start.is_some()
        && !parsed.saw_test_succeeded
        && !parsed.saw_test_failed
    {
        // Test phase started but neither `** TEST SUCCEEDED **` nor
        // `** TEST FAILED **` was seen — log was truncated or the harness
        // crashed mid-stream. Flag the gap so consumers know runner_outcome
        // was inferred from parsed failure data, not the runner's own
        // verdict.
        warnings.push(ParseWarning::MissingTerminalEvent);
    }
    // Count cross-check. `counts` is published from the summary lines; the
    // per-test lines, the run-start markers and `failures[]` are independent
    // witnesses. When they disagree the counts cannot be trusted, which is
    // what `partial` means under the v1.6 taxonomy (a complete failure list
    // with a degraded `counts` field is a distinct state). The motivating
    // case: a summary regex that rejected the `with S tests skipped and` form
    // fell back to the largest per-suite line and reported 13 of 160 tests as
    // `complete`. Skipped when the log is already known to be cut short: a
    // truncated or crashed stream disagrees trivially and
    // `MissingTerminalEvent` already degrades it. An explicit restart is
    // independent evidence of lost count coverage, even without a terminal.
    if parsed.saw_test_runner_restart
        || (warnings.is_empty() && parsed.counts_disagree_with_evidence())
    {
        warnings.push(ParseWarning::RunnerSummaryMismatch);
    }
    warnings
}

fn determine_runner_outcome(parsed: &ParsedRun, sidecar: &Sidecar) -> RunnerOutcome {
    if parsed.saw_test_succeeded && sidecar.exit_code == Some(0) {
        return RunnerOutcome::Passed;
    }
    if parsed.test_phase_start.is_none() && parsed.failures.iter().any(|f| f.phase != Phase::Test) {
        return RunnerOutcome::PreTestFailed;
    }
    // External interruption (signal kill, failtrace watchdog truncation) is
    // structurally different from a test failure even if some failures were
    // parsed mid-stream: the process didn't terminate naturally, so we cannot
    // trust that the test phase ran to completion. Pre-test build failures
    // above still win because they are actionable evidence before any test
    // phase was reached, matching gotest's build-failure priority.
    if sidecar.signal.is_some() || sidecar.truncated {
        return RunnerOutcome::HarnessCrashed;
    }
    // Build diagnostics remain higher priority when they co-occur with a
    // pre-test simulator/launch failure: they are concrete source/build
    // evidence and match the build-failure-first ordering used for external
    // interruption above.
    if parsed.saw_runtime_environment_failure && parsed.test_phase_start.is_none() {
        return RunnerOutcome::HarnessCrashed;
    }
    if parsed.saw_test_failed {
        return RunnerOutcome::Failed;
    }
    // Even without the terminal `** TEST FAILED **` marker, if we have parsed
    // test-phase failures the run had real failures — don't misclassify as
    // HarnessCrashed just because the trailing marker is absent (truncated or
    // crashed-after-failure log, etc.). The `MissingTerminalEvent` parse
    // warning surfaces the gap; see compute_parse_warnings.
    if parsed.failures.iter().any(|f| f.phase == Phase::Test) {
        return RunnerOutcome::Failed;
    }
    if sidecar.exit_code == Some(0) {
        RunnerOutcome::Passed
    } else {
        RunnerOutcome::HarnessCrashed
    }
}

fn suggested_destination_commands(parsed: &ParsedRun, sidecar: &Sidecar) -> Vec<Command> {
    let Some(destination_resolution) = parsed.destination_resolution.as_ref() else {
        return vec![];
    };
    let Some(replacement) =
        matching_destination_arg(&sidecar.argv, &destination_resolution.bad_spec)
    else {
        return vec![];
    };
    destination_resolution
        .available_destinations
        .iter()
        .filter(|destination| {
            destination.platform
                == destination_resolution
                    .bad_spec
                    .platform()
                    .unwrap_or_default()
        })
        .filter_map(|destination| {
            let id = destination.id.as_deref()?;
            if !looks_like_uuid(id) {
                return None;
            }
            let mut argv = sidecar.argv.clone();
            let new_value = format!("platform={},id={id}", destination.platform);
            match replacement {
                DestinationArg::Joined { idx } => {
                    argv[idx] = format!("-destination={new_value}");
                }
                DestinationArg::Separate { idx } => {
                    argv[idx] = new_value;
                }
            }
            Some(Command {
                command: argv
                    .iter()
                    .map(|arg| shell_quote(arg))
                    .collect::<Vec<_>>()
                    .join(" "),
                argv,
                cwd: sidecar.cwd.clone(),
                intent: CommandIntent::RerunIsolated,
                interactive: false,
                mutates_workspace: false,
                uses_network: false,
                requires_approval: false,
            })
        })
        .take(MAX_DESTINATION_SUGGESTIONS)
        .collect()
}

/// Tokenization of a `-destination` argv argument. `Joined` covers the
/// `-destination=value` form (single token); `Separate` covers `-destination
/// value` (two tokens). The caller rewrites the value differently in each
/// case, so returning a typed form keeps the helper's knowledge intact
/// rather than forcing the caller to re-inspect the argv slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestinationArg {
    /// `-destination=value` (single token at `idx`).
    Joined { idx: usize },
    /// `-destination value` (two tokens; `idx` points at the value token).
    Separate { idx: usize },
}

fn matching_destination_arg(argv: &[String], bad_spec: &DestinationSpec) -> Option<DestinationArg> {
    let mut matches = Vec::new();
    let mut index = 0usize;
    while index < argv.len() {
        let candidate = if argv[index] == "-destination" {
            index += 1;
            argv.get(index)
                .map(|value| (DestinationArg::Separate { idx: index }, value.as_str()))
        } else {
            argv[index]
                .strip_prefix("-destination=")
                .map(|value| (DestinationArg::Joined { idx: index }, value))
        };
        if let Some((arg, value)) = candidate
            && destination_value_matches_bad_spec(value, bad_spec)
        {
            matches.push(arg);
        }
        index += 1;
    }
    if matches.len() == 1 {
        Some(matches[0])
    } else {
        None
    }
}

fn destination_value_matches_bad_spec(value: &str, bad_spec: &DestinationSpec) -> bool {
    let value_spec = DestinationSpec {
        fields: parse_destination_value_fields(value),
    };
    if value_spec.platform() != bad_spec.platform() {
        return false;
    }
    if let Some(bad_name) = bad_spec.name()
        && let Some(value_name) = value_spec.name()
        && value_name != bad_name
    {
        return false;
    }
    if let Some(value_os) = value_spec.os()
        && let Some(bad_os) = bad_spec.os()
        && value_os != bad_os
    {
        return false;
    }
    if let Some(value_id) = value_spec.id()
        && let Some(bad_id) = bad_spec.id()
        && value_id != bad_id
    {
        return false;
    }
    value_spec.name().is_some() || value_spec.os().is_some() || value_spec.id().is_some()
}

fn parse_destination_value_fields(value: &str) -> BTreeMap<String, String> {
    parse_kv_fields(value, '=')
}

impl DestinationSpec {
    fn platform(&self) -> Option<&str> {
        self.fields.get("platform").map(String::as_str)
    }

    fn name(&self) -> Option<&str> {
        self.fields.get("name").map(String::as_str)
    }

    fn os(&self) -> Option<&str> {
        self.fields.get("OS").map(String::as_str)
    }

    fn id(&self) -> Option<&str> {
        self.fields.get("id").map(String::as_str)
    }
}

fn looks_like_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (idx, byte) in bytes.iter().enumerate() {
        let is_dash = matches!(idx, 8 | 13 | 18 | 23);
        if is_dash {
            if *byte != b'-' {
                return false;
            }
        } else if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    true
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

fn build_counts(parsed: &ParsedRun) -> Counts {
    if parsed.tests_run == 0 && parsed.failures.is_empty() {
        return zero_test_counts();
    }
    // Invariant 11 recomputes tests_failed from unique runner_test_id values
    // in emitted failures, so count distinct test ids, not failure records.
    let tests_failed = parsed.unique_failed_test_ids();
    // Keep tests_run consistent if it was undercounted (build-only failures
    // can leave tests_run at zero, which is fine). The floor includes the
    // skip total: a bundle that crashed before its end line can contribute
    // parsed failures while its summary of record covers fewer tests, and
    // invariant 12 (`passed + failed + errored + skipped <= run`) would
    // otherwise reject the document instead of emitting it as `partial`.
    // `counts_disagree_with_evidence` reports the floor engaging.
    let tests_skipped = parsed.tests_skipped;
    let tests_run = parsed.tests_run.max(tests_failed + tests_skipped);
    let tests_passed = tests_run.saturating_sub(tests_failed + tests_skipped);
    Counts {
        tests_run,
        tests_passed,
        tests_failed,
        tests_errored: 0,
        tests_skipped,
        tests_not_run: None,
        files_failed: None,
    }
}

fn early_exit(sidecar: &Sidecar) -> Option<EarlyExit> {
    sidecar
        .signal
        .as_ref()
        .map(|signal| EarlyExit {
            reason: EarlyExitReason::SignalReceived,
            killer: EarlyExitKiller::Os,
            details: signal.clone(),
        })
        .or_else(|| {
            if !sidecar.truncated {
                return None;
            }
            let reason = match sidecar.truncation_reason {
                Some(TruncationReason::Timeout) => EarlyExitReason::WatchdogWallclock,
                Some(TruncationReason::Signal) => EarlyExitReason::SignalReceived,
                Some(TruncationReason::BufferExceeded) | None => EarlyExitReason::WatchdogSilence,
            };
            Some(EarlyExit {
                reason,
                killer: EarlyExitKiller::Failtrace,
                details: sidecar
                    .truncation_reason
                    .map(|reason| format!("failtrace truncated capture: {reason:?}"))
                    .unwrap_or_else(|| "failtrace truncated capture".to_owned()),
            })
        })
}

fn apply_failure_budget(
    failures: Vec<ParsedFailure>,
    max: Option<u64>,
) -> (Vec<ParsedFailure>, bool) {
    if let Some(limit) = max {
        let limit = limit as usize;
        if failures.len() > limit {
            return (failures.into_iter().take(limit).collect(), true);
        }
    }
    (failures, false)
}

fn parsed_failure_to_failure(pf: ParsedFailure, cwd: &str) -> Failure {
    let primary_location = Location {
        file: pf.file.clone(),
        line: pf.line,
        column: pf.column,
        end_line: None,
        end_column: None,
        role: LocationRole::Primary,
        // Synthetic tool locations are not user code.
        in_user_code: !matches!(pf.file.as_str(), "<linker>" | "<xcodebuild>"),
    };

    let (excerpt_text, excerpt_truncated) = clip_excerpt(&pf.excerpt_text);
    let evidence = Evidence {
        byte_range: pf.byte_range,
        excerpts: vec![Excerpt {
            source: ExcerptSource::RawLog,
            kind: ExcerptKind::FailureBlock,
            byte_range: pf.byte_range,
            text: excerpt_text,
            ansi_stripped: false,
            truncated: excerpt_truncated,
        }],
    };

    let details = details_for_kind(pf.kind, &pf.message, &pf.unresolved_symbols);

    let mut failure = Failure {
        failure_id: String::new(),
        adapter_id: ADAPTER_ID.to_owned(),
        phase: pf.phase,
        kind: pf.kind,
        runner_package_id: None,
        runner_test_id: pf.runner_test_id.clone(),
        test_segments: pf.test_segments,
        test_name: pf.test_name,
        param_id: None,
        test_group_id: None,
        primary_location,
        extra_locations: vec![],
        message: pf.message,
        diagnostic_code: None,
        diagnostic_severity: DiagnosticSeverity::Error,
        details,
        raw_log_lines: pf.raw_log_lines,
        evidence: Some(evidence),
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

fn details_for_kind(kind: FailureKind, message: &str, unresolved_symbols: &[String]) -> Details {
    match kind {
        FailureKind::Assertion => Details::Assertion {
            expected: extract_expected(message),
            actual: extract_actual(message),
            diff: None,
        },
        FailureKind::SyntaxError => Details::SyntaxError { spans: vec![] },
        FailureKind::MissingSymbol => Details::MissingSymbol {
            symbol: extract_missing_symbol(message).unwrap_or_default(),
            import_chain: vec![],
        },
        FailureKind::DependencyResolutionError => Details::DependencyResolutionError {
            package: None,
            resolver: Resolver::Unknown,
        },
        FailureKind::TypeError => Details::TypeError {
            spans: vec![],
            explanation_command: None,
        },
        FailureKind::LinkerError => Details::LinkerError {
            unresolved_symbols: unresolved_symbols.to_vec(),
        },
        FailureKind::Exception => Details::Exception {
            exception_type: None,
            stack: vec![],
        },
        _ => Details::Unknown {},
    }
}

/// For Swift Testing `Expectation failed: (a → 1) == (b → 2)`, extract the LHS
/// rendering as `actual` and the RHS as `expected`. Returns None when the message
/// is not in the recognized shape.
fn extract_actual(message: &str) -> Option<String> {
    extract_expectation_values(message).map(|(actual, _)| actual)
}

fn extract_expected(message: &str) -> Option<String> {
    extract_expectation_values(message).map(|(_, expected)| expected)
}

fn extract_expectation_values(message: &str) -> Option<(String, String)> {
    let prefix = "Expectation failed: ";
    let mut pos = message.find(prefix)? + prefix.len();
    let (actual, lhs_end) = parse_named_expectation_value(message, pos)?;
    pos = lhs_end;
    let separator = " == ";
    if !message.get(pos..)?.starts_with(separator) {
        return None;
    }
    pos += separator.len();
    let (expected, _) = parse_named_expectation_value(message, pos)?;
    Some((actual, expected))
}

fn parse_named_expectation_value(message: &str, start: usize) -> Option<(String, usize)> {
    let mut pos = start;
    if !message.get(pos..)?.starts_with('(') {
        return None;
    }
    pos += '('.len_utf8();
    let arrow = " → ";
    let arrow_offset = message.get(pos..)?.find(arrow)?;
    let value_start = pos + arrow_offset + arrow.len();
    let mut depth = 0usize;
    for (offset, ch) in message.get(value_start..)?.char_indices() {
        let absolute = value_start + offset;
        match ch {
            '(' => depth += 1,
            ')' if depth == 0 => {
                let value = message[value_start..absolute].trim().to_owned();
                return Some((value, absolute + ch.len_utf8()));
            }
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// For Swift's `cannot find 'Foo' in scope`, extract `Foo`.
fn extract_missing_symbol(message: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"cannot find '([^']+)' in scope").unwrap());
    re.captures(message).map(|c| c[1].to_owned())
}

fn clip_excerpt(text: &str) -> (String, bool) {
    const MAX: usize = 2048;
    if text.len() <= MAX {
        return (text.to_owned(), false);
    }
    // Round the cut point DOWN to the nearest UTF-8 char boundary so we never
    // panic on a multi-byte split. Swift Testing's output is unicode-heavy
    // (◇/✘/✔/↳), so a naive `text[..MAX]` would panic on plausible input.
    let end = text
        .char_indices()
        .take_while(|(i, _)| *i < MAX)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let mut s = text[..end].to_owned();
    s.push_str(" …");
    (s, true)
}

// ---------------------------------------------------------------------------
// Line splitting (byte-faithful)
// ---------------------------------------------------------------------------

fn split_lines(log: &[u8]) -> Vec<LogLine> {
    let mut out = Vec::new();
    let mut start: u64 = 0;
    let mut number: u64 = 1;
    for (i, &b) in log.iter().enumerate() {
        if b == b'\n' {
            let end = (i + 1) as u64;
            let raw_end = i; // exclusive
            let raw = &log[start as usize..raw_end];
            let raw = if raw.last() == Some(&b'\r') {
                &raw[..raw.len() - 1]
            } else {
                raw
            };
            let (text, zwsp_prefix) = line_text(raw);
            out.push(LogLine {
                number,
                start,
                end,
                text,
                zwsp_prefix,
            });
            start = end;
            number += 1;
        }
    }
    if (start as usize) < log.len() {
        let (text, zwsp_prefix) = line_text(&log[start as usize..]);
        out.push(LogLine {
            number,
            start,
            end: log.len() as u64,
            text,
            zwsp_prefix,
        });
    }
    out
}

/// Decode a raw log line for matching, stripping any leading U+200B
/// zero-width spaces and reporting how many came off. Xcode 27 beta prefixes
/// Swift Testing recorder lines (`◇`/`✔`/`✘`/`➜`) with varying runs of them
/// (observed up to four), which would silently defeat every `^`-anchored
/// recorder regex. Byte offsets in `LogLine.start`/`end` still span the raw
/// line including the prefix, and [`LogLine::raw_text`] rebuilds the raw
/// decode from the returned count for excerpt and evidence text.
fn line_text(raw: &[u8]) -> (String, usize) {
    let text = String::from_utf8_lossy(raw);
    let trimmed = text.trim_start_matches(ZWSP);
    if trimmed.len() == text.len() {
        return (text.into_owned(), 0);
    }
    let zwsp_prefix = (text.len() - trimmed.len()) / ZWSP.len_utf8();
    (trimmed.to_owned(), zwsp_prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_error_anchor_matches_observed_phrasings() {
        // The captured fixture's anchor — the original drove the design.
        assert!(destination_error_anchor().is_match(
            "xcodebuild: error: Unable to find a device matching the provided destination specifier:"
        ));
        // xcodebuild also emits the `destination matching` phrasing.
        assert!(destination_error_anchor().is_match(
            "xcodebuild: error: Unable to find a destination matching the provided destination specifier:"
        ));
        // And the leading-clause form.
        assert!(destination_error_anchor().is_match(
            "xcodebuild: error: Could not configure request for the test plan: Unable to find a device matching the provided destination specifier:"
        ));
        // Discrimination: user-printed text containing the inner phrase
        // (e.g. a Swift test name or assertion message) must NOT admit.
        assert!(
            !destination_error_anchor()
                .is_match("Test 'foo' recorded an issue: Unable to find a device matching ...")
        );
        // Discrimination: other xcodebuild errors that share the prefix must
        // NOT admit.
        assert!(!destination_error_anchor().is_match(
            "xcodebuild: error: Scheme Foo is not currently configured for the test action."
        ));
    }

    fn selected_sidecar() -> Sidecar {
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
                version: "xcodebuild-test@0.1.0".to_owned(),
                kind: AdapterKind::Runner,
                selected: true,
                selection_reason: SelectionReason::ArgvNameMatch,
            }],
            command: "xcodebuild test".to_owned(),
            cwd: "/tmp/project".to_owned(),
            started_at: "2026-05-23T00:00:00Z".to_owned(),
            duration_ms: 1,
            exit_code: Some(65),
            signal: None,
            early_exit: None,
            truncated: false,
            truncation_reason: None,
            path_case_folding: PathCaseFolding::CaseSensitive,
            stream_merge: StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
            adapter_candidates: vec![],
            argv: vec!["xcodebuild".to_owned(), "test".to_owned()],
        }
    }

    #[test]
    fn classify_compile_error_kind_missing_symbol() {
        assert_eq!(
            classify_compile_error_kind("cannot find 'Foo' in scope"),
            FailureKind::MissingSymbol
        );
    }

    #[test]
    fn classify_compile_error_kind_syntax() {
        assert_eq!(
            classify_compile_error_kind("expected '}' in struct"),
            FailureKind::SyntaxError
        );
    }

    #[test]
    fn classify_compile_error_kind_type() {
        assert_eq!(
            classify_compile_error_kind("cannot convert value of type 'Int' to 'String'"),
            FailureKind::TypeError
        );
    }

    #[test]
    fn classify_compile_error_kind_swift_private_access_family() {
        assert_eq!(
            classify_compile_error_kind(
                "property must be declared private because its type uses a private type"
            ),
            FailureKind::TypeError
        );
        assert_eq!(
            classify_compile_error_kind(
                "PROPERTY MUST BE DECLARED PRIVATE BECAUSE ITS TYPE USES A PRIVATE TYPE"
            ),
            FailureKind::TypeError
        );
        assert_eq!(
            classify_compile_error_kind(
                "method must be declared private because its parameter uses a private type"
            ),
            FailureKind::TypeError
        );
        assert_eq!(
            classify_compile_error_kind(
                "subscript must be declared private because its index uses a private type"
            ),
            FailureKind::TypeError
        );
        assert_eq!(
            classify_compile_error_kind(
                "initializer must be declared private because its parameter uses a private type"
            ),
            FailureKind::TypeError
        );
        assert_eq!(
            classify_compile_error_kind(
                "function must be declared private because its result uses a private type; consider adding 'private'"
            ),
            FailureKind::TypeError
        );
    }

    #[test]
    fn classify_compile_error_kind_swift_illegal_try_family() {
        assert_eq!(
            classify_compile_error_kind(
                "'try' cannot appear to the right of a non-assignment operator"
            ),
            FailureKind::SyntaxError
        );
        assert_eq!(
            classify_compile_error_kind(
                "'TRY' CANNOT APPEAR TO THE RIGHT OF A NON-ASSIGNMENT OPERATOR"
            ),
            FailureKind::SyntaxError
        );
        assert_eq!(
            classify_compile_error_kind("'try' must appear immediately before the expression"),
            FailureKind::SyntaxError
        );
        assert_eq!(
            classify_compile_error_kind(
                "'try!' cannot appear to the right of a non-assignment operator"
            ),
            FailureKind::SyntaxError
        );
        assert_eq!(
            classify_compile_error_kind(
                "'try?' cannot appear to the right of a non-assignment operator"
            ),
            FailureKind::SyntaxError
        );
    }

    #[test]
    fn classify_compile_error_kind_swift_missing_try_family() {
        assert_eq!(
            classify_compile_error_kind(
                "operator can throw but expression is not marked with 'try'"
            ),
            FailureKind::TypeError
        );
        assert_eq!(
            classify_compile_error_kind(
                "OPERATOR CAN THROW BUT EXPRESSION IS NOT MARKED WITH 'TRY'"
            ),
            FailureKind::TypeError
        );
    }

    #[test]
    fn classify_compile_error_kind_objc_undeclared_identifier() {
        assert_eq!(
            classify_compile_error_kind("use of undeclared identifier 'fooFunction'"),
            FailureKind::MissingSymbol
        );
    }

    #[test]
    fn classify_compile_error_kind_clang_file_not_found() {
        assert_eq!(
            classify_compile_error_kind("'Foundation/Foundation.h' file not found"),
            FailureKind::MissingSymbol
        );
    }

    #[test]
    fn parse_extracts_objc_compile_error() {
        // Objective-C compile errors share xcodebuild's `<file>:<line>:<col>:
        // error: ...` shape. Restricting the file pattern to `.swift` would
        // silently drop these.
        let log = b"/path/to/MyClass.m:42:10: error: use of undeclared identifier 'fooFunction'\n** TEST FAILED **\n";
        let parsed = ParsedRun::from_log(log);
        assert_eq!(parsed.failures.len(), 1);
        let f = &parsed.failures[0];
        assert_eq!(f.phase, Phase::Build);
        assert_eq!(f.kind, FailureKind::MissingSymbol);
        assert_eq!(f.file, "/path/to/MyClass.m");
        assert_eq!(f.line, Some(42));
        assert_eq!(f.column, Some(10));
    }

    #[test]
    fn parse_extracts_modulemap_compile_error() {
        let log = b"/path/to/module.modulemap:5:8: error: redefinition of module 'Foo'\n** TEST FAILED **\n";
        let parsed = ParsedRun::from_log(log);
        assert_eq!(parsed.failures.len(), 1);
        assert_eq!(parsed.failures[0].file, "/path/to/module.modulemap");
        assert_eq!(parsed.failures[0].phase, Phase::Build);
    }

    #[test]
    fn truncated_build_failure_is_admitted_and_lowers_to_incomplete_pre_test_failed() {
        let log = b"/tmp/project/Foo.swift:42:10: error: cannot find 'MissingType' in scope\n";
        let mut sidecar = selected_sidecar();
        sidecar.exit_code = None;
        sidecar.signal = Some("SIGTERM".to_owned());
        sidecar.truncated = true;
        sidecar.truncation_reason = Some(crate::schema::TruncationReason::Timeout);

        let document = analyze_log(log, &sidecar, AnalyzeOptions::default())
            .expect("parsed build failures should make the selected adapter admissible");

        assert_eq!(document.runner_outcome, RunnerOutcome::PreTestFailed);
        assert_eq!(document.parse_status, crate::schema::ParseStatus::Partial);
        assert_eq!(
            document.parse_warnings,
            vec![ParseWarning::MissingTerminalEvent]
        );
        assert!(!document.failures_complete);
        assert_eq!(document.failures.len(), 1);
        assert_eq!(document.failures[0].phase, Phase::Build);
    }

    #[test]
    fn xctest_summary_undercount_degrades_to_partial_with_summary_mismatch() {
        // The per-test lines say four tests ran (one skipped); the summary
        // says two. This is the shape the v0.1.0 binary produced on a
        // 160-test suite with two XCTSkips: its summary regex rejected the
        // `with S tests skipped and` form, max-wins fell back to a per-suite
        // line, and `complete` vouched for 13/160. Counts stay with the
        // summary (trust the runner); the disagreement must be observable.
        let log = b"Test Suite 'All tests' started at 2026-10-24 10:15:08.670.\n\
            Test Suite 'ExampleAppTests.xctest' started at 2026-10-24 10:15:08.670.\n\
            Test Suite 'FooTests' started at 2026-10-24 10:15:08.670.\n\
            Test Case '-[ExampleAppTests.FooTests testA]' started.\n\
            Test Case '-[ExampleAppTests.FooTests testA]' passed (0.001 seconds).\n\
            Test Case '-[ExampleAppTests.FooTests testB]' started.\n\
            Test Case '-[ExampleAppTests.FooTests testB]' passed (0.001 seconds).\n\
            Test Case '-[ExampleAppTests.FooTests testC]' started.\n\
            Test Case '-[ExampleAppTests.FooTests testC]' passed (0.001 seconds).\n\
            Test Case '-[ExampleAppTests.FooTests testD]' started.\n\
            /tmp/project/FooTests.swift:14: -[ExampleAppTests.FooTests testD] : Test skipped - SNAPSHOT_DIR not set\n\
            Test Case '-[ExampleAppTests.FooTests testD]' skipped (0.012 seconds).\n\
            Test Suite 'FooTests' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 2 tests, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'ExampleAppTests.xctest' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 2 tests, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 2 tests, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            ** TEST SUCCEEDED **\n";
        let mut sidecar = selected_sidecar();
        sidecar.exit_code = Some(0);

        let document = analyze_log(log, &sidecar, AnalyzeOptions::default())
            .expect("XCTest envelope makes the selected adapter admissible");

        assert_eq!(document.runner_outcome, RunnerOutcome::Passed);
        assert_eq!(document.parse_status, crate::schema::ParseStatus::Partial);
        assert_eq!(
            document.parse_warnings,
            vec![ParseWarning::RunnerSummaryMismatch]
        );
        assert!(document.failures_complete);
        assert_eq!(document.counts.tests_run, 2);
        assert_eq!(document.counts.tests_skipped, 0);
    }

    #[test]
    fn xctest_summary_agreeing_with_cases_including_skips_stays_complete() {
        let log = b"Test Suite 'All tests' started at 2026-10-24 10:15:08.670.\n\
            Test Suite 'FooTests' started at 2026-10-24 10:15:08.670.\n\
            Test Case '-[ExampleAppTests.FooTests testA]' started.\n\
            Test Case '-[ExampleAppTests.FooTests testA]' passed (0.001 seconds).\n\
            Test Case '-[ExampleAppTests.FooTests testB]' started.\n\
            Test Case '-[ExampleAppTests.FooTests testB]' passed (0.001 seconds).\n\
            Test Case '-[ExampleAppTests.FooTests testC]' started.\n\
            /tmp/project/FooTests.swift:14: -[ExampleAppTests.FooTests testC] : Test skipped - reason\n\
            Test Case '-[ExampleAppTests.FooTests testC]' skipped (0.001 seconds).\n\
            Test Case '-[ExampleAppTests.FooTests testD]' started.\n\
            /tmp/project/FooTests.swift:20: -[ExampleAppTests.FooTests testD] : Test skipped - reason\n\
            Test Case '-[ExampleAppTests.FooTests testD]' skipped (0.001 seconds).\n\
            Test Suite 'FooTests' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 4 tests, with 2 tests skipped and 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 4 tests, with 2 tests skipped and 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            ** TEST SUCCEEDED **\n";
        let mut sidecar = selected_sidecar();
        sidecar.exit_code = Some(0);

        let document = analyze_log(log, &sidecar, AnalyzeOptions::default()).unwrap();

        assert_eq!(document.parse_status, crate::schema::ParseStatus::Complete);
        assert_eq!(document.parse_warnings, vec![]);
        assert_eq!(document.counts.tests_run, 4);
        assert_eq!(document.counts.tests_passed, 2);
        assert_eq!(document.counts.tests_skipped, 2);
    }

    #[test]
    fn xctest_unrecovered_bundle_keeps_its_largest_summary_and_degrades() {
        // Bundle A completes with one skip; bundle B parses two failures and a class-level `Executed 2`
        // but never reaches its end line. Summing only recovered bundles gave
        // run 1 / skipped 1, the failures forced run to 2, and invariant 12
        // (passed + failed + skipped <= run) rejected the document outright.
        // The unrecovered bundle's largest inner summary stands in for its
        // total, the mismatch marks the counts, and the document is emitted.
        let log = b"Test Suite 'All tests' started at 2026-10-24 10:15:08.670.\n\
            Test Suite 'ATests.xctest' started at 2026-10-24 10:15:08.670.\n\
            Test Suite 'FooTests' started at 2026-10-24 10:15:08.670.\n\
            Test Case '-[ATests.FooTests testA]' started.\n\
            /tmp/project/FooTests.swift:14: -[ATests.FooTests testA] : Test skipped - reason\n\
            Test Case '-[ATests.FooTests testA]' skipped (0.001 seconds).\n\
            Test Suite 'FooTests' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 1 test, with 1 test skipped and 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'ATests.xctest' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 1 test, with 1 test skipped and 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 1 test, with 1 test skipped and 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' started at 2026-10-24 10:15:11.000.\n\
            Test Suite 'BUITests.xctest' started at 2026-10-24 10:15:11.000.\n\
            Test Suite 'UITests' started at 2026-10-24 10:15:11.000.\n\
            Test Case '-[BUITests.UITests testX]' started.\n\
            /tmp/project/UITests.swift:10: error: -[BUITests.UITests testX] : XCTAssertTrue failed\n\
            Test Case '-[BUITests.UITests testX]' failed (0.100 seconds).\n\
            Test Case '-[BUITests.UITests testY]' started.\n\
            /tmp/project/UITests.swift:20: error: -[BUITests.UITests testY] : XCTAssertEqual failed\n\
            Test Case '-[BUITests.UITests testY]' failed (0.100 seconds).\n\
            Test Suite 'UITests' failed at 2026-10-24 10:15:12.000.\n\
            \t Executed 2 tests, with 2 failures (2 unexpected) in 0.200 (0.201) seconds\n\
            ** TEST FAILED **\n";
        let sidecar = selected_sidecar();

        let document = analyze_log(log, &sidecar, AnalyzeOptions::default())
            .expect("an unrecovered bundle must degrade the document, not reject it");

        assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
        assert_eq!(document.parse_status, crate::schema::ParseStatus::Partial);
        assert_eq!(
            document.parse_warnings,
            vec![ParseWarning::RunnerSummaryMismatch]
        );
        assert_eq!(document.counts.tests_run, 3);
        assert_eq!(document.counts.tests_failed, 2);
        assert_eq!(document.counts.tests_skipped, 1);
        assert_eq!(document.counts.tests_passed, 0);
        assert_eq!(document.failures.len(), 2);
    }

    #[test]
    fn xctest_repeated_bundle_with_swapped_skips_stays_complete() {
        // The same two tests on two destinations, each skipping a different one (configuration-
        // dependent XCTSkip). A witness unioned across both occurrences saw
        // two skipped ids against a total of one. Scoping the witness to its
        // own occurrence keeps the healthy run `complete`.
        let log = b"Test Suite 'All tests' started at 2026-10-24 10:15:08.670.\n\
            Test Suite 'ExampleAppTests.xctest' started at 2026-10-24 10:15:08.670.\n\
            Test Case '-[ExampleAppTests.FooTests testA]' started.\n\
            /tmp/project/FooTests.swift:14: -[ExampleAppTests.FooTests testA] : Test skipped - not on this platform\n\
            Test Case '-[ExampleAppTests.FooTests testA]' skipped (0.001 seconds).\n\
            Test Case '-[ExampleAppTests.FooTests testB]' started.\n\
            Test Case '-[ExampleAppTests.FooTests testB]' passed (0.001 seconds).\n\
            Test Suite 'ExampleAppTests.xctest' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 2 tests, with 1 test skipped and 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 2 tests, with 1 test skipped and 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' started at 2026-10-24 10:15:09.670.\n\
            Test Suite 'ExampleAppTests.xctest' started at 2026-10-24 10:15:09.670.\n\
            Test Case '-[ExampleAppTests.FooTests testA]' started.\n\
            Test Case '-[ExampleAppTests.FooTests testA]' passed (0.001 seconds).\n\
            Test Case '-[ExampleAppTests.FooTests testB]' started.\n\
            /tmp/project/FooTests.swift:20: -[ExampleAppTests.FooTests testB] : Test skipped - not on this platform\n\
            Test Case '-[ExampleAppTests.FooTests testB]' skipped (0.001 seconds).\n\
            Test Suite 'ExampleAppTests.xctest' passed at 2026-10-24 10:15:09.700.\n\
            \t Executed 2 tests, with 1 test skipped and 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:09.700.\n\
            \t Executed 2 tests, with 1 test skipped and 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            ** TEST SUCCEEDED **\n";
        let mut sidecar = selected_sidecar();
        sidecar.exit_code = Some(0);

        let document = analyze_log(log, &sidecar, AnalyzeOptions::default()).unwrap();

        assert_eq!(document.parse_status, crate::schema::ParseStatus::Complete);
        assert_eq!(document.parse_warnings, vec![]);
        assert_eq!(document.counts.tests_run, 2);
        assert_eq!(document.counts.tests_skipped, 1);
    }

    #[test]
    fn mixed_harness_totals_sum_across_xctest_and_swift_testing() {
        // Max-wins between the harnesses would publish the larger one alone, and moving the comparison after the
        // loop changed which side won a tie. The populations are disjoint
        // (a Swift Testing bundle's XCTest envelope says `Executed 0 tests`),
        // so they sum. Synthetic: no mixed-harness log has been captured.
        let log = "◇ Test run started.\n\
            ◇ Test foo() started.\n\
            ✔ Test foo() passed after 0.001 seconds.\n\
            ◇ Test bar() started.\n\
            ➜ Test bar() skipped.\n\
            ◇ Test baz() started.\n\
            ✔ Test baz() passed after 0.001 seconds.\n\
            ✔ Test run with 3 tests in 1 suite passed after 0.002 seconds.\n\
            Test Suite 'All tests' started at 2026-10-24 10:15:08.670.\n\
            Test Suite 'UnitTests.xctest' started at 2026-10-24 10:15:08.670.\n\
            Test Case '-[UnitTests.LegacyTests testOne]' started.\n\
            Test Case '-[UnitTests.LegacyTests testOne]' passed (0.001 seconds).\n\
            Test Case '-[UnitTests.LegacyTests testTwo]' started.\n\
            Test Case '-[UnitTests.LegacyTests testTwo]' passed (0.001 seconds).\n\
            Test Suite 'UnitTests.xctest' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 2 tests, with 0 failures (0 unexpected) in 0.002 (0.003) seconds\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 2 tests, with 0 failures (0 unexpected) in 0.002 (0.003) seconds\n\
            ** TEST SUCCEEDED **\n";
        let mut sidecar = selected_sidecar();
        sidecar.exit_code = Some(0);

        let document = analyze_log(log.as_bytes(), &sidecar, AnalyzeOptions::default()).unwrap();

        assert_eq!(document.parse_warnings, vec![]);
        assert_eq!(document.counts.tests_run, 5);
        assert_eq!(document.counts.tests_skipped, 1);
        assert_eq!(document.counts.tests_passed, 4);
    }

    #[test]
    fn failures_and_skips_exceeding_the_swift_testing_total_degrade_not_reject() {
        // Seen in a real run: the first Swift Testing bundle recorded failures
        // and crashed before its `Test run with N tests` summary; only the
        // second bundle's total was counted. Two failed ids plus one skip
        // exceeded a run total of 2 and invariant 12 would reject the
        // document. The floor emits it
        // and the floor engaging marks the counts untrustworthy. (No restart
        // line in this synthetic log; the real run also carried one, which
        // `saw_test_runner_restart` catches on its own.)
        let log = "◇ Test run started.\n\
            ◇ Test alpha() started.\n\
            ✘ Test alpha() recorded an issue at Tests.swift:10:5: Expectation failed: (a → 1) == (b → 2)\n\
            ✘ Test alpha() failed after 0.100 seconds with 1 issue.\n\
            ◇ Test beta() started.\n\
            ✘ Test beta() recorded an issue at Tests.swift:20:5: Expectation failed: (c → 3) == (d → 4)\n\
            ◇ Test run started.\n\
            ◇ Test gamma() started.\n\
            ✔ Test gamma() passed after 0.001 seconds.\n\
            ➜ Test delta() skipped: \"not today\"\n\
            ✘ Test run with 2 tests in 1 suite failed after 0.002 seconds with 0 issues.\n\
            ** TEST FAILED **\n";
        let sidecar = selected_sidecar();

        let document = analyze_log(log.as_bytes(), &sidecar, AnalyzeOptions::default())
            .expect("an unsummarized Swift Testing run must degrade, not reject");

        assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
        assert_eq!(document.parse_status, crate::schema::ParseStatus::Partial);
        assert_eq!(
            document.parse_warnings,
            vec![ParseWarning::RunnerSummaryMismatch]
        );
        assert_eq!(document.counts.tests_failed, 2);
        assert_eq!(document.counts.tests_skipped, 1);
        assert_eq!(document.counts.tests_run, 3);
        assert_eq!(document.counts.tests_passed, 0);
    }

    #[test]
    fn swift_testing_run_passed_with_known_issue_counts_its_tests() {
        // A run (and suite) that passed with a known issue is prefixed `━`,
        // not `✔`. A summary regex that only accepts `✔` would count 0 tests.
        // A failing sibling suite follows so the `━ Suite` end line must
        // also pop the suite stack: without that pop the failure id would
        // carry the known-issue suite's prefix as well.
        let log = "◇ Test run started.\n\
            ◇ Suite PlannerTests started.\n\
            ◇ Test foo() started.\n\
            ✔ Test foo() passed after 0.001 seconds.\n\
            ━ Suite PlannerTests passed after 14.614 seconds with 1 known issue.\n\
            ◇ Suite OtherTests started.\n\
            ◇ Test bar() started.\n\
            ✘ Test bar() recorded an issue at Other.swift:9:5: Expectation failed: (x → 1) == (y → 2)\n\
            ✘ Test bar() failed after 0.001 seconds with 1 issue.\n\
            ✘ Suite OtherTests failed after 0.001 seconds with 1 issue.\n\
            ━ Test run with 7 tests in 2 suites failed after 14.615 seconds with 1 issue.\n\
            ** TEST EXECUTE FAILED **\n";
        let sidecar = selected_sidecar();

        let document = analyze_log(log.as_bytes(), &sidecar, AnalyzeOptions::default()).unwrap();

        assert_eq!(document.runner_outcome, RunnerOutcome::Failed);
        assert_eq!(document.parse_warnings, vec![]);
        assert_eq!(document.counts.tests_run, 7);
        assert_eq!(document.counts.tests_failed, 1);
        let id = document.failures[0]
            .runner_test_id
            .as_deref()
            .expect("Swift Testing failure carries a test id");
        assert!(id.ends_with("OtherTests.bar"), "{id}");
        assert!(!id.contains("PlannerTests"), "{id}");
    }

    #[test]
    fn xctest_bundle_occurrences_with_disjoint_tests_do_not_pass_as_complete() {
        // Two occurrences of one bundle, each agreeing
        // with its own `Executed 1 test` line but running a different test
        // (Apple distributes a target's classes across destinations). Max per
        // name publishes 1; the two distinct ids must not pass as `complete`.
        let log = b"Test Suite 'All tests' started at 2026-10-24 10:15:08.670.\n\
            Test Suite 'Tests.xctest' started at 2026-10-24 10:15:08.670.\n\
            Test Case '-[Tests.FooTests testA]' started.\n\
            Test Case '-[Tests.FooTests testA]' passed (0.001 seconds).\n\
            Test Suite 'Tests.xctest' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 1 test, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 1 test, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' started at 2026-10-24 10:15:09.670.\n\
            Test Suite 'Tests.xctest' started at 2026-10-24 10:15:09.670.\n\
            Test Case '-[Tests.FooTests testB]' started.\n\
            Test Case '-[Tests.FooTests testB]' passed (0.001 seconds).\n\
            Test Suite 'Tests.xctest' passed at 2026-10-24 10:15:09.700.\n\
            \t Executed 1 test, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:09.700.\n\
            \t Executed 1 test, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            ** TEST SUCCEEDED **\n";
        let mut sidecar = selected_sidecar();
        sidecar.exit_code = Some(0);

        let document = analyze_log(log, &sidecar, AnalyzeOptions::default()).unwrap();

        assert_eq!(document.runner_outcome, RunnerOutcome::Passed);
        assert_eq!(document.parse_status, crate::schema::ParseStatus::Partial);
        assert_eq!(
            document.parse_warnings,
            vec![ParseWarning::RunnerSummaryMismatch]
        );
        assert_eq!(document.counts.tests_run, 1);
    }

    #[test]
    fn test_runner_restart_degrades_the_counts() {
        // xcodebuild's relaunch line promises the summary will include
        // previous launches; the summaries this adapter reads do not (in real
        // logs the final summary covers only the last launch). No per-test witness and no failures here,
        // so the restart line is the only evidence that `1` is short.
        let log = "◇ Test run started.\n\
            ◇ Test alpha() started.\n\
            ✔ Test alpha() passed after 0.001 seconds.\n\
            ◇ Test beta() started.\n\
            Restarting after unexpected exit, crash, or test timeout; summary will include totals from previous launches.\n\
            ◇ Test run started.\n\
            ◇ Test gamma() started.\n\
            ✔ Test gamma() passed after 0.001 seconds.\n\
            ✔ Test run with 1 test in 1 suite passed after 0.001 seconds.\n\
            ** TEST FAILED **\n";
        let sidecar = selected_sidecar();

        let document = analyze_log(log.as_bytes(), &sidecar, AnalyzeOptions::default()).unwrap();

        assert_eq!(document.parse_status, crate::schema::ParseStatus::Partial);
        assert_eq!(
            document.parse_warnings,
            vec![ParseWarning::RunnerSummaryMismatch]
        );
        assert_eq!(document.counts.tests_run, 1);
    }

    #[test]
    fn xctest_case_repeated_for_a_second_destination_counts_once() {
        // Two `-destination` arguments run every bundle once per destination:
        // each id gets two terminal lines and the bundle gets two summary
        // lines. Ids dedupe the witness and max-per-bundle-name dedupes the
        // total, so a healthy run is not flagged and not double-counted.
        let log = b"Test Suite 'All tests' started at 2026-10-24 10:15:08.670.\n\
            Test Suite 'ExampleAppTests.xctest' started at 2026-10-24 10:15:08.670.\n\
            Test Case '-[ExampleAppTests.FooTests testA]' started.\n\
            Test Case '-[ExampleAppTests.FooTests testA]' passed (0.001 seconds).\n\
            Test Suite 'ExampleAppTests.xctest' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 1 test, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 1 test, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' started at 2026-10-24 10:15:09.670.\n\
            Test Suite 'ExampleAppTests.xctest' started at 2026-10-24 10:15:09.670.\n\
            Test Case '-[ExampleAppTests.FooTests testA]' started.\n\
            Test Case '-[ExampleAppTests.FooTests testA]' passed (0.001 seconds).\n\
            Test Suite 'ExampleAppTests.xctest' passed at 2026-10-24 10:15:09.700.\n\
            \t Executed 1 test, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:09.700.\n\
            \t Executed 1 test, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            ** TEST SUCCEEDED **\n";
        let mut sidecar = selected_sidecar();
        sidecar.exit_code = Some(0);

        let document = analyze_log(log, &sidecar, AnalyzeOptions::default()).unwrap();

        assert_eq!(document.parse_warnings, vec![]);
        assert_eq!(document.counts.tests_run, 1);
    }

    #[test]
    fn xctest_bundles_sum_and_keep_a_skip_in_the_second_bundle() {
        // Unit + UI test bundles, each with its own `All tests` envelope.
        // Max-wins would publish the larger bundle alone and drop the other
        // bundle's tests from every count. Bundle lines sum; the skip in the smaller bundle survives.
        let log = b"Test Suite 'All tests' started at 2026-10-24 10:15:08.670.\n\
            Test Suite 'ExampleAppTests.xctest' started at 2026-10-24 10:15:08.670.\n\
            Test Suite 'FooTests' started at 2026-10-24 10:15:08.670.\n\
            Test Case '-[ExampleAppTests.FooTests testA]' started.\n\
            Test Case '-[ExampleAppTests.FooTests testA]' passed (0.001 seconds).\n\
            Test Case '-[ExampleAppTests.FooTests testB]' started.\n\
            Test Case '-[ExampleAppTests.FooTests testB]' passed (0.001 seconds).\n\
            Test Case '-[ExampleAppTests.FooTests testC]' started.\n\
            Test Case '-[ExampleAppTests.FooTests testC]' passed (0.001 seconds).\n\
            Test Suite 'FooTests' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 3 tests, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'ExampleAppTests.xctest' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 3 tests, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 3 tests, with 0 failures (0 unexpected) in 0.003 (0.004) seconds\n\
            Test Suite 'All tests' started at 2026-10-24 10:15:11.000.\n\
            Test Suite 'ExampleAppUITests.xctest' started at 2026-10-24 10:15:11.000.\n\
            Test Suite 'UITests' started at 2026-10-24 10:15:11.000.\n\
            Test Case '-[ExampleAppUITests.UITests testLaunch]' started.\n\
            Test Case '-[ExampleAppUITests.UITests testLaunch]' passed (5.001 seconds).\n\
            Test Case '-[ExampleAppUITests.UITests testSnapshots]' started.\n\
            /tmp/project/UITests.swift:30: -[ExampleAppUITests.UITests testSnapshots] : Test skipped - no snapshot dir\n\
            Test Case '-[ExampleAppUITests.UITests testSnapshots]' skipped (0.002 seconds).\n\
            Test Suite 'UITests' passed at 2026-10-24 10:15:16.100.\n\
            \t Executed 2 tests, with 1 test skipped and 0 failures (0 unexpected) in 5.003 (5.004) seconds\n\
            Test Suite 'ExampleAppUITests.xctest' passed at 2026-10-24 10:15:16.100.\n\
            \t Executed 2 tests, with 1 test skipped and 0 failures (0 unexpected) in 5.003 (5.004) seconds\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:16.100.\n\
            \t Executed 2 tests, with 1 test skipped and 0 failures (0 unexpected) in 5.003 (5.005) seconds\n\
            ** TEST SUCCEEDED **\n";
        let mut sidecar = selected_sidecar();
        sidecar.exit_code = Some(0);

        let document = analyze_log(log, &sidecar, AnalyzeOptions::default()).unwrap();

        assert_eq!(document.parse_status, crate::schema::ParseStatus::Complete);
        assert_eq!(document.parse_warnings, vec![]);
        assert_eq!(document.counts.tests_run, 5);
        assert_eq!(document.counts.tests_passed, 4);
        assert_eq!(document.counts.tests_skipped, 1);
    }

    #[test]
    fn xctest_case_whose_terminal_line_was_clobbered_still_counts_as_started() {
        // Interleaved stderr glued to the front of a terminal line (observed
        // in a HomeReference run: `unrecognised property keyTest Case '...'
        // passed`). The summary counted the test; the started line is the
        // witness, so the healthy run stays `complete`.
        let log = b"Test Suite 'All tests' started at 2026-10-24 10:15:08.670.\n\
            Test Suite 'HomeReferenceTests.xctest' started at 2026-10-24 10:15:08.670.\n\
            Test Case '-[HomeReferenceTests.LabelTests testVision]' started.\n\
            unrecognised property keyTest Case '-[HomeReferenceTests.LabelTests testVision]' passed (1.879 seconds).\n\
            Test Case '-[HomeReferenceTests.LabelTests testPrinted]' started.\n\
            Test Case '-[HomeReferenceTests.LabelTests testPrinted]' passed (0.765 seconds).\n\
            Test Suite 'HomeReferenceTests.xctest' passed at 2026-10-24 10:15:11.000.\n\
            \t Executed 2 tests, with 0 failures (0 unexpected) in 2.644 (2.650) seconds\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:11.000.\n\
            \t Executed 2 tests, with 0 failures (0 unexpected) in 2.644 (2.651) seconds\n\
            ** TEST SUCCEEDED **\n";
        let mut sidecar = selected_sidecar();
        sidecar.exit_code = Some(0);

        let document = analyze_log(log, &sidecar, AnalyzeOptions::default()).unwrap();

        assert_eq!(document.parse_warnings, vec![]);
        assert_eq!(document.counts.tests_run, 2);
    }

    #[test]
    fn swift_testing_only_log_does_not_trip_the_xctest_cross_check() {
        // Swift Testing logs carry an XCTest envelope that reports
        // `Executed 0 tests` and no `Test Case` lines at all. With no per-test
        // witness the cross-check must stay silent and the Swift Testing
        // total must still win.
        let log = "◇ Test run started.\n\
            ◇ Test foo() started.\n\
            ✔ Test foo() passed after 0.001 seconds.\n\
            ✔ Test run with 1 test in 1 suite passed after 0.001 seconds.\n\
            Test Suite 'All tests' started at 2026-10-24 10:15:08.670.\n\
            Test Suite 'All tests' passed at 2026-10-24 10:15:08.700.\n\
            \t Executed 0 tests, with 0 failures (0 unexpected) in 0.000 (0.001) seconds\n\
            ** TEST SUCCEEDED **\n";
        let mut sidecar = selected_sidecar();
        sidecar.exit_code = Some(0);

        let document = analyze_log(log.as_bytes(), &sidecar, AnalyzeOptions::default()).unwrap();

        assert_eq!(document.parse_warnings, vec![]);
        assert_eq!(document.counts.tests_run, 1);
    }

    #[test]
    fn test_without_building_execute_markers_count_as_terminal_events() {
        // `xcodebuild test-without-building` emits `** TEST EXECUTE
        // SUCCEEDED/FAILED **` instead of `** TEST SUCCEEDED/FAILED **`.
        // Matching only the plain markers would leave every
        // test-without-building run partial + missing_terminal_event.
        let log = b"Testing started\n\
            Test Suite 'Selected tests' started at 2026-07-03 12:11:05.683.\n\
            Test Suite 'Selected tests' passed at 2026-07-03 12:11:05.685.\n\
            \t Executed 0 tests, with 0 failures (0 unexpected) in 0.000 (0.002) seconds\n\
            ** TEST EXECUTE SUCCEEDED **\n";
        let parsed = ParsedRun::from_log(log);
        assert!(parsed.saw_test_succeeded);
        assert!(!parsed.saw_test_failed);

        let mut sidecar = selected_sidecar();
        sidecar.exit_code = Some(0);
        assert_eq!(
            determine_runner_outcome(&parsed, &sidecar),
            RunnerOutcome::Passed
        );
        assert_eq!(compute_parse_warnings(&parsed, &sidecar), vec![]);

        let failed_log = b"Testing started\n\
            Test Suite 'Selected tests' started at 2026-07-03 12:11:05.683.\n\
            ** TEST EXECUTE FAILED **\n";
        let parsed = ParsedRun::from_log(failed_log);
        assert!(parsed.saw_test_failed);
        assert!(!parsed.saw_test_succeeded);
        assert_eq!(
            determine_runner_outcome(&parsed, &selected_sidecar()),
            RunnerOutcome::Failed
        );
    }

    #[test]
    fn truncated_log_with_parsed_failures_lowers_to_failed_not_harness_crashed() {
        // When test-phase failures are successfully parsed but the terminal `** TEST FAILED **` marker is missing
        // (truncated/interrupted log), the previous determine_runner_outcome
        // fell through to the exit-code branch and reported HarnessCrashed
        // for non-zero exits. That misclassified real test failures as
        // infrastructure crashes. Treat parsed Phase::Test failures as
        // dispositive evidence the run failed.
        use crate::storage::{PathCaseFolding, Sidecar, StreamMerge};
        let log = b"\xe2\x97\x87 Test run started.\n\
            \xe2\x97\x87 Suite \"AlphaTests\" started.\n\
            \xe2\x97\x87 Test alphaOne() started.\n\
            \xe2\x9c\x98 Test alphaOne() recorded an issue at AlphaTests.swift:10:5: Expectation failed: 1 == 2\n\
            \xe2\x9c\x98 Test alphaOne() failed after 0.001 seconds with 1 issue.\n";
        let parsed = ParsedRun::from_log(log);
        assert!(
            !parsed.saw_test_failed,
            "test fixture must lack the ** TEST FAILED ** marker"
        );
        assert_eq!(parsed.failures.len(), 1);
        assert_eq!(parsed.failures[0].phase, Phase::Test);

        let sidecar = Sidecar {
            meta_schema_version: "1.2".to_owned(),
            schema_version: "1.6".to_owned(),
            failtrace_version: "0.1.0".to_owned(),
            capture_build: None,
            adapters: vec![],
            command: String::new(),
            cwd: String::new(),
            started_at: String::new(),
            duration_ms: 0,
            exit_code: Some(65),
            signal: None,
            early_exit: None,
            truncated: false,
            truncation_reason: None,
            path_case_folding: PathCaseFolding::CaseSensitive,
            stream_merge: StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
            adapter_candidates: vec![],
            argv: vec![],
        };
        let outcome = determine_runner_outcome(&parsed, &sidecar);
        assert_eq!(
            outcome,
            RunnerOutcome::Failed,
            "parsed test failures must trump missing terminal marker"
        );

        let warnings = compute_parse_warnings(&parsed, &sidecar);
        assert!(
            warnings.contains(&ParseWarning::MissingTerminalEvent),
            "must flag MissingTerminalEvent so inferred-Failed is observable: got {warnings:?}"
        );
    }

    #[test]
    fn signal_kill_dominates_parsed_failures_with_harness_crashed() {
        // The inverse of the above: when sidecar.signal is set (failtrace
        // watchdog SIGTERM, OS killed the process, etc.) the run was
        // externally interrupted. Even if we managed to parse some failures
        // mid-stream, the test phase did not complete naturally — report
        // HarnessCrashed so consumers know the verdict is unreliable.
        use crate::storage::{PathCaseFolding, Sidecar, StreamMerge};
        let log = b"\xe2\x97\x87 Test run started.\n\
            \xe2\x9c\x98 Test alphaOne() recorded an issue at AlphaTests.swift:10:5: Expectation failed: 1 == 2\n\
            \xe2\x9c\x98 Test alphaOne() failed after 0.001 seconds with 1 issue.\n";
        let parsed = ParsedRun::from_log(log);
        let sidecar = Sidecar {
            meta_schema_version: "1.2".to_owned(),
            schema_version: "1.6".to_owned(),
            failtrace_version: "0.1.0".to_owned(),
            capture_build: None,
            adapters: vec![],
            command: String::new(),
            cwd: String::new(),
            started_at: String::new(),
            duration_ms: 0,
            exit_code: None,
            signal: Some("SIGTERM".to_owned()),
            early_exit: None,
            truncated: true,
            truncation_reason: Some(crate::schema::TruncationReason::Timeout),
            path_case_folding: PathCaseFolding::CaseSensitive,
            stream_merge: StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
            adapter_candidates: vec![],
            argv: vec![],
        };
        assert_eq!(
            determine_runner_outcome(&parsed, &sidecar),
            RunnerOutcome::HarnessCrashed
        );
    }

    #[test]
    fn truncated_runtime_env_failure_admits_via_admission_gate() {
        // A sim-launch failure log truncated BEFORE the trailing
        // `Testing failed:` summary header has only the
        // saw_runtime_environment_failure marker set. The admission gate must
        // accept that marker; otherwise analyze_log returns
        // UnrecognizedFormat and bypasses the intended HarnessCrashed +
        // MissingTerminalEvent lowering at the determine_runner_outcome /
        // compute_parse_warnings sites.
        let log = b"2026-05-23 14:42:43.899 xcodebuild[76501:944513] [MT] IDELaunchReport: Failed to install or launch the test runner.\n2026-05-23 14:42:43.900 xcodebuild[76501:944513] Mach error -308 - (ipc/mig) server died\n";
        let parsed = ParsedRun::from_log(log);
        assert!(
            parsed.saw_runtime_environment_failure,
            "fixture must trigger saw_runtime_environment_failure"
        );
        assert!(
            !parsed.saw_testing_failed_summary,
            "fixture must NOT have the Testing failed: summary (truncated before)"
        );
        assert!(
            parsed.looks_like_xcodebuild(),
            "admission gate must admit a runtime-env-failure-only log"
        );
    }

    #[test]
    fn parse_pops_swift_testing_suite_stack_between_suites() {
        // Locks suite-stack popping reliability. The Rust raw-string syntax
        // `r#"..."#` makes the trailing `"#` the closing delimiter, not a
        // literal `"` followed by `#` — so the suite-end regex actually
        // requires NO trailing `"` after `passed|failed`. Verified
        // empirically here: a fixture with two distinct suites must
        // produce per-suite test IDs (`SuiteA.testA`, `SuiteB.testB`),
        // not the suite-stack-leak shape (`SuiteA.SuiteB.testB`).
        let log = b"\xe2\x97\x87 Test run started.\n\
            \xe2\x97\x87 Suite \"AlphaTests\" started.\n\
            \xe2\x97\x87 Test alphaOne() started.\n\
            \xe2\x9c\x98 Test alphaOne() recorded an issue at AlphaTests.swift:10:5: Expectation failed: 1 == 2\n\
            \xe2\x9c\x98 Test alphaOne() failed after 0.001 seconds with 1 issue.\n\
            \xe2\x9c\x98 Suite \"AlphaTests\" failed after 0.002 seconds with 1 issue.\n\
            \xe2\x97\x87 Suite BetaTests started.\n\
            \xe2\x97\x87 Test betaOne() started.\n\
            \xe2\x9c\x98 Test betaOne() recorded an issue at BetaTests.swift:20:5: Expectation failed: a == b\n\
            \xe2\x9c\x98 Test betaOne() failed after 0.001 seconds with 1 issue.\n\
            \xe2\x9c\x98 Suite BetaTests failed after 0.002 seconds with 1 issue.\n\
            \xe2\x9c\x98 Test run with 2 tests in 2 suites failed after 0.004 seconds with 2 issues.\n\
            ** TEST FAILED **\n";
        let parsed = ParsedRun::from_log(log);
        assert_eq!(parsed.failures.len(), 2);
        let ids: Vec<&str> = parsed
            .failures
            .iter()
            .filter_map(|f| f.runner_test_id.as_deref())
            .collect();
        // Each test must carry ONLY its own suite as prefix. Suite-stack
        // leak would produce `AlphaTests.BetaTests.betaOne` for the second.
        assert!(
            ids.contains(&"AlphaTests.alphaOne"),
            "expected AlphaTests.alphaOne in {ids:?}"
        );
        assert!(
            ids.contains(&"BetaTests.betaOne"),
            "expected BetaTests.betaOne in {ids:?}"
        );
        // Mixed quoted and unquoted suite-name forms must both push AND
        // pop correctly. The fixture above uses quoted for AlphaTests and
        // unquoted for BetaTests deliberately.
    }

    #[test]
    fn parse_extracts_unknown_pseudo_file_compile_error() {
        // xcodebuild sometimes emits `<unknown>:0:0:` for diagnostics with no
        // source location (e.g. SDK-level errors). Must still be captured.
        let log = b"<unknown>:0:0: error: SDK 'iphonesimulator26.5' is not installed\n** TEST FAILED **\n";
        let parsed = ParsedRun::from_log(log);
        assert_eq!(parsed.failures.len(), 1);
        assert_eq!(parsed.failures[0].file, "<unknown>");
        assert_eq!(parsed.failures[0].line, Some(0));
    }

    #[test]
    fn build_failures_sort_by_location_not_observed_log_order() {
        let log = b"/tmp/project/Zeta.swift:20:5: error: cannot find 'Zed' in scope\n\
            /tmp/project/Alpha.swift:10:5: error: cannot find 'Aye' in scope\n\
            ** TEST FAILED **\n";
        let parsed = ParsedRun::from_log(log);
        assert_eq!(parsed.failures.len(), 2);
        assert_eq!(parsed.failures[0].file, "/tmp/project/Alpha.swift");
        assert_eq!(parsed.failures[1].file, "/tmp/project/Zeta.swift");
    }

    #[test]
    fn ranking_stability_uses_full_failure_set_before_budgeting() {
        let log = b"/tmp/project/Build.swift:20:5: error: cannot find 'Missing' in scope\n\
            \xe2\x97\x87 Test run started.\n\
            \xe2\x9c\x98 Test flakyOne() recorded an issue at FlakyTests.swift:10:5: Expectation failed: 1 == 2\n\
            ** TEST FAILED **\n";
        let document = analyze_log(
            log,
            &selected_sidecar(),
            AnalyzeOptions {
                max_failures: Some(1),
            },
        )
        .expect("mixed build/test log should analyze");
        assert_eq!(document.emission.failures_emitted, 1);
        assert_eq!(document.emission.failures_total, 2);
        assert_eq!(document.failures[0].phase, Phase::Build);
        assert!(
            !document.ranking.is_stable_across_runs,
            "budgeted emission hid the test failure, but run-level ranking must stay unstable"
        );
    }

    #[test]
    fn classify_swift_testing_kind_expectation() {
        assert_eq!(
            classify_swift_testing_kind("Expectation failed: (a → 1) == (b → 2)"),
            FailureKind::Assertion
        );
    }

    #[test]
    fn normalize_xctest_test_id_basic() {
        assert_eq!(
            normalize_xctest_test_id("ExampleAppTests.MyClass testFoo"),
            "ExampleAppTests.MyClass.testFoo"
        );
    }

    #[test]
    fn normalize_swift_testing_drops_parens() {
        assert_eq!(
            normalize_swift_testing_test_id("intentionalExpectFailure"),
            "intentionalExpectFailure"
        );
    }

    #[test]
    fn extract_actual_and_expected() {
        let msg = "Expectation failed: (actual → 42) == (expected → 99)";
        assert_eq!(extract_actual(msg), Some("42".to_owned()));
        assert_eq!(extract_expected(msg), Some("99".to_owned()));
    }

    #[test]
    fn extract_actual_and_expected_with_parenthesized_values() {
        let msg = "Expectation failed: (a → bar(42)) == (b → qux(99))";
        assert_eq!(extract_actual(msg), Some("bar(42)".to_owned()));
        assert_eq!(extract_expected(msg), Some("qux(99)".to_owned()));
    }

    #[test]
    fn extract_missing_symbol_basic() {
        assert_eq!(
            extract_missing_symbol("cannot find 'ExampleCatalog' in scope"),
            Some("ExampleCatalog".to_owned())
        );
    }

    #[test]
    fn parse_extracts_swift_testing_display_name_issue() {
        // `@Test("My display name") func ...` emits the issue line with a
        // quoted display name where `()` would normally be. A regex that
        // requires `\(\)` would drop this shape and leave failures
        // unreported.
        let log = b"\xe2\x97\x87 Test run started.\n\
            \xe2\x9c\x98 Test \"My display name\" recorded an issue at FooTests.swift:10:5: Expectation failed: 1 == 2\n\
            \xe2\x9c\x98 Test \"My display name\" failed after 0.001 seconds with 1 issue.\n\
            ** TEST FAILED **\n";
        let parsed = ParsedRun::from_log(log);
        assert_eq!(parsed.failures.len(), 1);
        let f = &parsed.failures[0];
        assert_eq!(f.kind, FailureKind::Assertion);
        assert_eq!(f.runner_test_id.as_deref(), Some("My display name"));
        assert_eq!(f.test_segments, vec!["My display name"]);
        assert_eq!(f.file, "FooTests.swift");
        assert_eq!(f.line, Some(10));
    }

    #[test]
    fn swift_testing_display_name_with_dot_is_one_segment() {
        let log = b"\xe2\x97\x87 Test run started.\n\
            \xe2\x9c\x98 Test \"User logs in v2.0\" recorded an issue at FooTests.swift:10:5: Expectation failed: 1 == 2\n\
            ** TEST FAILED **\n";
        let parsed = ParsedRun::from_log(log);
        assert_eq!(parsed.failures.len(), 1);
        let f = &parsed.failures[0];
        assert_eq!(f.runner_test_id.as_deref(), Some("User logs in v2.0"));
        assert_eq!(f.test_segments, vec!["User logs in v2.0"]);
    }

    #[test]
    fn swift_testing_continuations_extend_message_and_evidence() {
        let log = b"\xe2\x97\x87 Test run started.\n\
            \xe2\x9c\x98 Test foo() recorded an issue at FooTests.swift:10:5: Expectation failed: 1 == 2\n\
            \xe2\x86\xb3 first user message line\n\
            \xe2\x86\xb3 second user message line\n\
            ** TEST FAILED **\n";
        let parsed = ParsedRun::from_log(log);
        assert_eq!(parsed.failures.len(), 1);
        let f = &parsed.failures[0];
        assert!(f.message.contains("first user message line"));
        assert!(f.message.contains("second user message line"));
        assert_eq!(f.raw_log_lines, [2, 4]);
        assert!(f.excerpt_text.contains("first user message line"));
        assert!(f.excerpt_text.contains("second user message line"));
        assert!(f.byte_range.end > f.byte_range.start);
    }

    #[test]
    fn parse_extracts_swift_testing_parameterized_issue() {
        // Parameterized tests emit `foo(_:)` with arg-label parens content;
        // a regex requiring bare `()` would drop this shape.
        let log = b"\xe2\x97\x87 Test run started.\n\
            \xe2\x9c\x98 Test foo(_:) recorded an issue at FooTests.swift:42:5: Expectation failed: a == b\n\
            \xe2\x9c\x98 Test foo(_:) failed after 0.001 seconds with 1 issue.\n\
            ** TEST FAILED **\n";
        let parsed = ParsedRun::from_log(log);
        assert_eq!(parsed.failures.len(), 1);
        let f = &parsed.failures[0];
        assert_eq!(f.kind, FailureKind::Assertion);
        // Parameterized signature is preserved — `foo()` and `foo(_:)` are
        // distinct overloads and must hash to distinct failure_ids.
        assert_eq!(f.runner_test_id.as_deref(), Some("foo(_:)"));
        assert_eq!(f.file, "FooTests.swift");
        assert_eq!(f.line, Some(42));
    }

    #[test]
    fn normalize_swift_testing_strips_display_name_quotes() {
        assert_eq!(
            normalize_swift_testing_test_id("\"My display name\""),
            "My display name"
        );
    }

    #[test]
    fn normalize_swift_testing_preserves_parameterized_signature() {
        assert_eq!(normalize_swift_testing_test_id("foo(_:)"), "foo(_:)");
        assert_eq!(
            normalize_swift_testing_test_id("foo(input:expected:)"),
            "foo(input:expected:)"
        );
    }

    #[test]
    fn ranking_is_stable_only_when_no_test_phase_failures() {
        // xcodebuild executes tests concurrently
        // across destinations and (by default in Swift Testing) within a
        // single destination. Test-phase failures therefore arrive in
        // non-deterministic order across runs. Build-phase failures come
        // from a deterministic compile and ARE stable. Matches cargo's
        // is_pre_test_failure-only-is-stable pattern.

        // Pure-build failure → stable.
        let log = b"/path/MyClass.m:42:10: error: use of undeclared identifier 'foo'\n** TEST FAILED **\n";
        let parsed = ParsedRun::from_log(log);
        assert!(parsed.failures.iter().all(|f| f.phase == Phase::Build));
        let stable = parsed.failures.iter().all(|f| f.phase != Phase::Test);
        assert!(stable, "build-only failures must be stable");

        // Test-phase failure → NOT stable, even with a single test.
        let log = b"\xe2\x97\x87 Test run started.\n\
            \xe2\x9c\x98 Test foo() recorded an issue at FooTests.swift:10:5: Expectation failed: 1 == 2\n\
            ** TEST FAILED **\n";
        let parsed = ParsedRun::from_log(log);
        assert!(parsed.failures.iter().any(|f| f.phase == Phase::Test));
        let stable = parsed.failures.iter().all(|f| f.phase != Phase::Test);
        assert!(
            !stable,
            "any test-phase failure must mark ranking as non-stable"
        );
    }

    #[test]
    fn parse_linker_excerpt_includes_full_block() {
        // The `excerpt_text` for `Undefined symbols for architecture` blocks
        // must cover the full block, matching `raw_log_lines` /
        // `byte_range`, so agents reading the excerpt get the symbol-name and
        // referenced-from context rather than only the header line.
        // NOTE: do NOT use `\` line-continuation here — Rust strips leading
        // whitespace from the continued line, which would un-indent the
        // `"  "symbol", referenced from:"` lines and cause the block scanner
        // to terminate at the first continuation.
        let log = b"Undefined symbols for architecture arm64:\n  \"_FooFunction\", referenced from:\n      _BarFunction in BarLib(BarLib.o)\n  \"_BazFunction\", referenced from:\n      _QuxFunction in QuxLib(QuxLib.o)\nld: symbol(s) not found for architecture arm64\n";
        let parsed = ParsedRun::from_log(log);
        // First failure is the block; second is the trailing `ld: ...` line.
        assert!(!parsed.failures.is_empty());
        let block_failure = parsed
            .failures
            .iter()
            .find(|f| f.message.starts_with("Undefined"))
            .expect("expected Undefined-symbols block failure");
        assert!(
            block_failure.excerpt_text.contains("_FooFunction"),
            "excerpt must include the referenced-symbol lines: {:?}",
            block_failure.excerpt_text
        );
        assert!(
            block_failure.excerpt_text.contains("_BazFunction"),
            "excerpt must include all referenced-symbol lines, not just the first: {:?}",
            block_failure.excerpt_text
        );
        // Excerpt must NOT include the sibling `ld:` line (that's its own
        // failure record).
        assert!(
            !block_failure.excerpt_text.contains("ld:"),
            "excerpt must not include the trailing ld: line: {:?}",
            block_failure.excerpt_text
        );
    }

    #[test]
    fn linker_warning_lines_are_not_failures() {
        let log = b"ld: warning: directory not found for option '-L/foo'\n";
        let parsed = ParsedRun::from_log(log);
        assert!(parsed.failures.is_empty());
    }

    #[test]
    fn undefined_symbols_block_allows_tab_indented_continuations() {
        let log = b"Undefined symbols for architecture arm64:\n\t\"_FooFunction\", referenced from:\n\t    _BarFunction in BarLib(BarLib.o)\nld: symbol(s) not found for architecture arm64\n";
        let parsed = ParsedRun::from_log(log);
        let block_failure = parsed
            .failures
            .iter()
            .find(|f| f.message.starts_with("Undefined"))
            .expect("expected Undefined-symbols block failure");
        assert_eq!(block_failure.unresolved_symbols, vec!["_FooFunction"]);
        assert_eq!(block_failure.raw_log_lines, [1, 3]);
    }

    #[test]
    fn duplicate_synthetic_linker_lines_are_preserved() {
        let log = b"ld: framework not found Foo\nld: framework not found Foo\n";
        let parsed = ParsedRun::from_log(log);
        assert_eq!(parsed.failures.len(), 2);
        assert!(
            parsed
                .failures
                .iter()
                .all(|f| f.kind == FailureKind::LinkerError)
        );
    }

    #[test]
    fn clipped_excerpt_sets_truncated_true() {
        let mut text = String::from("Undefined symbols for architecture arm64:");
        for i in 0..160 {
            text.push_str(&format!("\n  \"_Symbol{i}\", referenced from:"));
        }
        let pf = ParsedFailure {
            phase: Phase::Build,
            kind: FailureKind::LinkerError,
            runner_test_id: None,
            test_name: None,
            test_segments: vec![],
            file: "<linker>".to_owned(),
            line: None,
            column: None,
            message: "Undefined symbols for architecture arm64".to_owned(),
            raw_log_lines: [1, 161],
            byte_range: ByteRange {
                start: 0,
                end: text.len() as u64,
            },
            excerpt_text: text,
            unresolved_symbols: vec![],
        };
        let failure = parsed_failure_to_failure(pf, "/tmp/project");
        let excerpt = &failure.evidence.as_ref().unwrap().excerpts[0];
        assert!(excerpt.truncated);
        assert!(excerpt.text.ends_with(" …"));
    }

    #[test]
    fn mixed_xctest_summary_and_swift_testing_counts_match_invariant_11() {
        let log = b"\xe2\x97\x87 Test run started.\n\
            Executed 2 tests, with 2 failures (2 unexpected) in 0.001 seconds\n\
            \xe2\x9c\x98 Test swiftFailure() recorded an issue at FooTests.swift:10:5: Expectation failed: 1 == 2\n\
            ** TEST FAILED **\n";
        let document = analyze_log(log, &selected_sidecar(), AnalyzeOptions::default())
            .expect("mixed harness log should analyze");
        assert_eq!(document.counts.tests_run, 2);
        assert_eq!(document.counts.tests_failed, 1);
        assert_eq!(validate(&document), Vec::new());
    }

    #[test]
    fn multi_suite_summary_with_skips_takes_envelope_totals() {
        // Per-suite summaries followed by the bundle envelope. The 39- and
        // 72-test lines carry the `with N tests skipped and` clause, which the
        // original regex rejected — leaving tests_run at the largest matching
        // per-suite line (20) and tests_skipped at 0.
        let log = b"Test Suite 'ExampleAppTests.xctest' started at 2026-08-20 21:03:01.000.\n\
            \t Executed 5 tests, with 0 failures (0 unexpected) in 0.139 (0.140) seconds\n\
            \t Executed 8 tests, with 0 failures (0 unexpected) in 0.012 (0.014) seconds\n\
            \t Executed 39 tests, with 2 tests skipped and 0 failures (0 unexpected) in 0.335 (0.343) seconds\n\
            \t Executed 20 tests, with 0 failures (0 unexpected) in 0.123 (0.126) seconds\n\
            \t Executed 72 tests, with 2 tests skipped and 0 failures (0 unexpected) in 0.608 (0.624) seconds\n\
            ** TEST SUCCEEDED **\n";
        let document = analyze_log(log, &selected_sidecar(), AnalyzeOptions::default())
            .expect("multi-suite log should analyze");
        assert_eq!(document.counts.tests_run, 72);
        assert_eq!(document.counts.tests_passed, 70);
        assert_eq!(document.counts.tests_failed, 0);
        assert_eq!(document.counts.tests_skipped, 2);
        assert_eq!(validate(&document), Vec::new());
    }

    #[test]
    fn swift_testing_skips_counted_from_test_lines_not_suite_lines() {
        // Shapes from the swift_testing_skipped fixture (library version 2077):
        // bare-with-reason, bare-no-reason, display name, and suite-level skip.
        // The disabled suite emits BOTH its own `➜ Suite` line AND one
        // `➜ Test` line per member — only the Test lines may count, and the
        // run summary's `6 tests` total includes all skipped tests.
        let log = "◇ Test run started.\n\
            ➜ Suite DisabledSuite skipped: \"entire suite off\"\n\
            ◇ Suite SkipCapTests started.\n\
            ➜ Test disabledTwo() skipped.\n\
            ➜ Test \"Display named skip\" skipped: \"flaky\"\n\
            ➜ Test one() skipped: \"entire suite off\"\n\
            ◇ Test addsNumbers() started.\n\
            ➜ Test disabledOne() skipped: \"not ready yet\"\n\
            ➜ Test two() skipped: \"entire suite off\"\n\
            ✔ Test addsNumbers() passed after 0.001 seconds.\n\
            ✔ Suite SkipCapTests passed after 0.001 seconds.\n\
            ✔ Test run with 6 tests in 2 suites passed after 0.001 seconds.\n\
            ** TEST SUCCEEDED **\n"
            .as_bytes();
        let document = analyze_log(log, &selected_sidecar(), AnalyzeOptions::default())
            .expect("skip log should analyze");
        assert_eq!(document.counts.tests_run, 6);
        assert_eq!(document.counts.tests_passed, 1);
        assert_eq!(document.counts.tests_failed, 0);
        assert_eq!(document.counts.tests_skipped, 5);
        assert_eq!(validate(&document), Vec::new());
    }

    #[test]
    fn swift_testing_failed_run_with_skips_derives_passed_from_remainder() {
        let log = "◇ Test run started.\n\
            ➜ Test flaky() skipped: \"quarantined\"\n\
            ✘ Test broken() recorded an issue at FooTests.swift:10:5: Expectation failed: 1 == 2\n\
            ✘ Test run with 3 tests in 1 suite failed after 0.002 seconds with 1 issue.\n\
            ** TEST FAILED **\n"
            .as_bytes();
        let document = analyze_log(log, &selected_sidecar(), AnalyzeOptions::default())
            .expect("failed run with skips should analyze");
        assert_eq!(document.counts.tests_run, 3);
        assert_eq!(document.counts.tests_passed, 1);
        assert_eq!(document.counts.tests_failed, 1);
        assert_eq!(document.counts.tests_skipped, 1);
        assert_eq!(validate(&document), Vec::new());
    }

    #[test]
    fn swift_testing_all_skipped_run_reports_zero_passed() {
        // Boundary for `passed = executed − skipped` and invariant 12 at
        // equality: every test skipped, run still summarized as passed.
        let log = "◇ Test run started.\n\
            ➜ Test one() skipped.\n\
            ➜ Test two() skipped: \"off\"\n\
            ✔ Test run with 2 tests in 1 suite passed after 0.001 seconds.\n\
            ** TEST SUCCEEDED **\n"
            .as_bytes();
        let document = analyze_log(log, &selected_sidecar(), AnalyzeOptions::default())
            .expect("all-skipped log should analyze");
        assert_eq!(document.counts.tests_run, 2);
        assert_eq!(document.counts.tests_passed, 0);
        assert_eq!(document.counts.tests_skipped, 2);
        assert_eq!(validate(&document), Vec::new());
    }

    #[test]
    fn swift_testing_skips_do_not_leak_across_bundles() {
        // One xcodebuild invocation, two Swift Testing bundles. Each bundle's
        // summary contributes its own `N tests` total and its own skips, and
        // the two totals sum: 2 + 3 tests run, 2 + 0 skipped. What must not
        // happen is the first bundle's 2 skips being paired with the second
        // bundle's 3-test total.
        let log = "◇ Test run started.\n\
            ➜ Test one() skipped: \"off\"\n\
            ➜ Test two() skipped: \"off\"\n\
            ✔ Test run with 2 tests in 1 suite passed after 0.001 seconds.\n\
            ◇ Test run started.\n\
            ✔ Test alpha() passed after 0.001 seconds.\n\
            ✔ Test beta() passed after 0.001 seconds.\n\
            ✔ Test gamma() passed after 0.001 seconds.\n\
            ✔ Test run with 3 tests in 1 suite passed after 0.002 seconds.\n\
            ** TEST SUCCEEDED **\n"
            .as_bytes();
        let document = analyze_log(log, &selected_sidecar(), AnalyzeOptions::default())
            .expect("two-bundle log should analyze");
        assert_eq!(document.counts.tests_run, 5);
        assert_eq!(document.counts.tests_passed, 3);
        assert_eq!(document.counts.tests_failed, 0);
        assert_eq!(document.counts.tests_skipped, 2);
        assert_eq!(validate(&document), Vec::new());
    }

    #[test]
    fn swift_testing_equal_sized_bundles_aggregate() {
        // Two bundles of the same size — the shape that forces aggregation
        // over pairing. Under a max-wins guard the second summary was skipped
        // entirely (its `2 tests` is not greater than the first's), leaving
        // tests_run at 2 while the second bundle's failure still landed in
        // failures[]. Invariant 11 then derived tests_failed=1 against a
        // 2-test run already claiming 2 skips, and invariant 12 rejected the
        // document — analysis failed outright rather than returning bad
        // counts. The `validate` assertion below is the crux of this test.
        let log = "◇ Test run started.\n\
            ➜ Test one() skipped: \"off\"\n\
            ➜ Test two() skipped: \"off\"\n\
            ✔ Test run with 2 tests in 1 suite passed after 0.001 seconds.\n\
            ◇ Test run started.\n\
            ✘ Test foo() recorded an issue at File.swift:1:1: Expectation failed: 1 == 2\n\
            ✘ Test run with 2 tests in 1 suite failed after 0.002 seconds with 1 issue.\n\
            ** TEST FAILED **\n"
            .as_bytes();
        let document = analyze_log(log, &selected_sidecar(), AnalyzeOptions::default())
            .expect("equal-sized two-bundle log should analyze");
        assert_eq!(validate(&document), Vec::new());
        assert_eq!(document.counts.tests_run, 4);
        assert_eq!(document.counts.tests_passed, 1);
        assert_eq!(document.counts.tests_failed, 1);
        assert_eq!(document.counts.tests_skipped, 2);
    }

    #[test]
    fn zero_width_space_prefixes_do_not_defeat_recorder_anchors() {
        // Xcode 27 beta prefixes recorder lines with varying runs of U+200B
        // (observed up to four in the swift_testing_skipped fixture capture).
        // Without stripping, every ^-anchored recorder regex silently fails.
        let log = "◇ Test run started.\n\
            \u{200B}\u{200B}\u{200B}➜ Test disabledTwo() skipped.\n\
            \u{200B}✘ Test broken() recorded an issue at FooTests.swift:10:5: Expectation failed: 1 == 2\n\
            \u{200B}\u{200B}✘ Test run with 3 tests in 1 suite failed after 0.002 seconds with 1 issue.\n\
            ** TEST FAILED **\n"
            .as_bytes();
        let document = analyze_log(log, &selected_sidecar(), AnalyzeOptions::default())
            .expect("zwsp-prefixed log should analyze");
        assert_eq!(document.counts.tests_run, 3);
        assert_eq!(document.counts.tests_failed, 1);
        assert_eq!(document.counts.tests_skipped, 1);
        assert_eq!(document.failures.len(), 1);
        assert_eq!(validate(&document), Vec::new());

        // Matching strips the prefix; evidence must not. The excerpt has to
        // round-trip from the byte range it claims — that range spans the raw
        // bytes, ZWSPs included. Only the line terminator is trimmed off,
        // because `byte_range.end` sits just past it while excerpt text never
        // carries it.
        let failure = &document.failures[0];
        let evidence = failure.evidence.as_ref().expect("issue line has evidence");
        let excerpt = &evidence.excerpts[0];
        assert_eq!(excerpt.source, ExcerptSource::RawLog);
        assert!(!excerpt.truncated);
        let claimed = &log[excerpt.byte_range.start as usize..excerpt.byte_range.end as usize];
        let claimed = String::from_utf8_lossy(claimed);
        assert_eq!(excerpt.text, claimed.trim_end_matches(['\r', '\n']));
        assert!(excerpt.text.starts_with(ZWSP));
        // ...while the message that derives the failure_id stays stripped.
        assert!(!failure.message.contains(ZWSP));
    }

    #[test]
    fn xcodebuild_populates_early_exit_from_signal_and_truncation() {
        let log = b"\xe2\x97\x87 Test run started.\n\
            \xe2\x9c\x98 Test foo() recorded an issue at FooTests.swift:10:5: Expectation failed: 1 == 2\n";
        let mut sidecar = selected_sidecar();
        sidecar.exit_code = None;
        sidecar.signal = Some("SIGTERM".to_owned());
        sidecar.truncated = true;
        sidecar.truncation_reason = Some(TruncationReason::Timeout);
        let document = analyze_log(log, &sidecar, AnalyzeOptions::default())
            .expect("interrupted xcodebuild log should analyze");
        assert_eq!(document.runner_outcome, RunnerOutcome::HarnessCrashed);
        let early_exit = document.early_exit.expect("early_exit should be populated");
        assert_eq!(early_exit.reason, EarlyExitReason::SignalReceived);
        assert_eq!(early_exit.killer, EarlyExitKiller::Os);
        assert_eq!(early_exit.details, "SIGTERM");
    }
}
