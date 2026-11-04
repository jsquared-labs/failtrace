use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub type Extensions = BTreeMap<String, serde_json::Value>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputDocument {
    pub schema_version: String,
    pub document_kind: DocumentKind,
    pub failtrace_version: String,
    pub emission: Emission,
    pub command: String,
    pub cwd: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub runner_outcome: RunnerOutcome,
    pub early_exit: Option<EarlyExit>,
    pub adapters: Vec<Adapter>,
    pub parse_status: ParseStatus,
    pub parse_warnings: Vec<ParseWarning>,
    pub determinism_warnings: Vec<DeterminismWarning>,
    pub truncated: bool,
    pub truncation_reason: Option<TruncationReason>,
    pub failures_complete: bool,
    pub failures: Vec<Failure>,
    pub ranking: Ranking,
    pub counts: Counts,
    pub raw_log_path: String,
    pub raw_log_sha256: String,
    pub suggested_next_commands: Vec<Command>,
    pub extensions: Extensions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    RunResult,
    AnalysisResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Emission {
    pub channel: EmissionChannel,
    pub mode: EmissionMode,
    pub max_failures: Option<u64>,
    pub failures_emitted: u64,
    pub failures_total: u64,
    pub full_output_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmissionChannel {
    Stdout,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmissionMode {
    Budgeted,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerOutcome {
    Passed,
    Failed,
    PreTestFailed,
    TypecheckFailed,
    HarnessCrashed,
    TimedOut,
    NoTests,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EarlyExit {
    pub reason: EarlyExitReason,
    pub killer: EarlyExitKiller,
    pub details: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EarlyExitReason {
    Maxfail,
    Bail,
    WatchdogSilence,
    WatchdogWallclock,
    RunnerInternalTimeout,
    PerTestTimeout,
    SignalReceived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EarlyExitKiller {
    Runner,
    Failtrace,
    Os,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adapter {
    pub id: String,
    pub version: String,
    pub kind: AdapterKind,
    pub selected: bool,
    pub selection_reason: SelectionReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    Runner,
    Compiler,
    Typechecker,
    Wrapper,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionReason {
    ArgvNameMatch,
    CompositeCommandComponent,
    LogSignatureMatch,
    NotSelected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseStatus {
    Complete,
    Partial,
    UnparsedOutput,
    UnsupportedCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseWarning {
    MissingSessionBanner,
    MissingSummaryLine,
    TruncatedTraceback,
    InterleavedOutput,
    UnstructuredDiagnostic,
    MalformedStructuredEvent,
    MissingTerminalEvent,
    RunnerSummaryMismatch,
    CompositeRuntimeDeferred,
    MissingLineNumber,
    MissingTestName,
    AnsiCodesPresentInMessage,
    RunnerVersionUnsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeterminismWarning {
    pub code: DeterminismWarningCode,
    pub adapter_id: String,
    pub meta_version: String,
    pub current_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeterminismWarningCode {
    AdapterVersionDrift,
    SchemaVersionDriftMinor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationReason {
    BufferExceeded,
    Timeout,
    Signal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub failure_id: String,
    pub adapter_id: String,
    pub phase: Phase,
    pub kind: FailureKind,
    pub runner_package_id: Option<String>,
    pub runner_test_id: Option<String>,
    pub test_segments: Vec<String>,
    pub test_name: Option<String>,
    pub param_id: Option<String>,
    pub test_group_id: Option<String>,
    pub primary_location: Location,
    pub extra_locations: Vec<Location>,
    pub message: String,
    pub diagnostic_code: Option<String>,
    pub diagnostic_severity: DiagnosticSeverity,
    pub details: Details,
    pub raw_log_lines: [u64; 2],
    pub evidence: Option<Evidence>,
    pub cascade_root: Option<String>,
    pub cascade_reason: Option<CascadeReason>,
    pub dependent_failure_ids: Vec<String>,
    pub retries: Vec<Retry>,
    pub companion_diagnostics: Vec<Diagnostic>,
    pub extensions: Extensions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Build,
    Collect,
    Setup,
    Test,
    Teardown,
    Typecheck,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Build => "build",
            Self::Collect => "collect",
            Self::Setup => "setup",
            Self::Test => "test",
            Self::Teardown => "teardown",
            Self::Typecheck => "typecheck",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Assertion,
    Exception,
    TypeError,
    SyntaxError,
    MissingSymbol,
    DependencyResolutionError,
    LinkerError,
    Panic,
    Signal,
    Timeout,
    Oom,
    Snapshot,
    UnhandledRejection,
    Race,
    Unknown,
}

impl std::fmt::Display for FailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Assertion => "assertion",
            Self::Exception => "exception",
            Self::TypeError => "type_error",
            Self::SyntaxError => "syntax_error",
            Self::MissingSymbol => "missing_symbol",
            Self::DependencyResolutionError => "dependency_resolution_error",
            Self::LinkerError => "linker_error",
            Self::Panic => "panic",
            Self::Signal => "signal",
            Self::Timeout => "timeout",
            Self::Oom => "oom",
            Self::Snapshot => "snapshot",
            Self::UnhandledRejection => "unhandled_rejection",
            Self::Race => "race",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub file: String,
    pub line: Option<u64>,
    pub column: Option<u64>,
    pub end_line: Option<u64>,
    pub end_column: Option<u64>,
    pub role: LocationRole,
    pub in_user_code: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationRole {
    Primary,
    Definition,
    ImportSite,
    PanicSite,
    TestSite,
    Related,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub file: String,
    pub line: Option<u64>,
    pub column: Option<u64>,
    pub end_line: Option<u64>,
    pub end_column: Option<u64>,
    pub role: LocationRole,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub byte_range: ByteRange,
    pub excerpts: Vec<Excerpt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Excerpt {
    pub source: ExcerptSource,
    pub kind: ExcerptKind,
    pub byte_range: ByteRange,
    pub text: String,
    pub ansi_stripped: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExcerptSource {
    RawLog,
    RunnerStructuredEvent,
    AdapterSynthesized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExcerptKind {
    FailureBlock,
    Stack,
    Diff,
    Summary,
    Process,
    Notes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub diagnostic_id: String,
    pub kind: FailureKind,
    pub diagnostic_severity: DiagnosticSeverity,
    pub message: String,
    pub primary_location: Option<Location>,
    pub details: Details,
    pub evidence: Option<Evidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub companion_diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Details {
    Assertion {
        expected: Option<String>,
        actual: Option<String>,
        diff: Option<Diff>,
    },
    Exception {
        exception_type: Option<String>,
        stack: Vec<Frame>,
    },
    TypeError {
        spans: Vec<Span>,
        explanation_command: Option<String>,
    },
    SyntaxError {
        spans: Vec<Span>,
    },
    MissingSymbol {
        symbol: String,
        import_chain: Vec<String>,
    },
    DependencyResolutionError {
        package: Option<String>,
        resolver: Resolver,
    },
    LinkerError {
        unresolved_symbols: Vec<String>,
    },
    Panic {
        panic_message: String,
        stack: Vec<Frame>,
        goroutine_id: Option<u64>,
    },
    Signal {
        signal_name: String,
        exit_code_raw: Option<i32>,
    },
    Timeout {
        source: TimeoutSource,
        limit_ms: Option<u64>,
        silence_ms: Option<u64>,
    },
    Oom {
        quoted_message: String,
        source: OomSource,
    },
    Snapshot {
        snapshot_file: Option<String>,
        snapshot_name: Option<String>,
        diff: Diff,
    },
    UnhandledRejection {
        rejection_value: String,
        stack: Vec<Frame>,
    },
    Race {
        participants: Vec<RaceParticipant>,
    },
    Unknown {},
}

impl Details {
    pub fn kind(&self) -> FailureKind {
        match self {
            Self::Assertion { .. } => FailureKind::Assertion,
            Self::Exception { .. } => FailureKind::Exception,
            Self::TypeError { .. } => FailureKind::TypeError,
            Self::SyntaxError { .. } => FailureKind::SyntaxError,
            Self::MissingSymbol { .. } => FailureKind::MissingSymbol,
            Self::DependencyResolutionError { .. } => FailureKind::DependencyResolutionError,
            Self::LinkerError { .. } => FailureKind::LinkerError,
            Self::Panic { .. } => FailureKind::Panic,
            Self::Signal { .. } => FailureKind::Signal,
            Self::Timeout { .. } => FailureKind::Timeout,
            Self::Oom { .. } => FailureKind::Oom,
            Self::Snapshot { .. } => FailureKind::Snapshot,
            Self::UnhandledRejection { .. } => FailureKind::UnhandledRejection,
            Self::Race { .. } => FailureKind::Race,
            Self::Unknown {} => FailureKind::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolver {
    Pip,
    Npm,
    Yarn,
    Pnpm,
    Cargo,
    GoMod,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutSource {
    Runner,
    FailtraceWatchdog,
    PerTest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OomSource {
    Process,
    LanguageRuntime,
    Kernel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub file: String,
    pub line: Option<u64>,
    pub function: Option<String>,
    pub in_user_code: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub format: DiffFormat,
    pub lines: Vec<DiffLine>,
    pub truncated: bool,
    pub omitted_line_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffFormat {
    Unified,
    ExpectedActual,
    Inline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub op: DiffOp,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffOp {
    Add,
    Remove,
    Context,
    Meta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaceParticipant {
    pub stack: Vec<Frame>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CascadeReason {
    PytestCollectionError,
    CargoBuildFailed,
    TscIntraDiagnosticChain,
    TscCrossFileChain,
    GoBinaryAborted,
    NpmScriptChain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retry {
    pub attempt: u64,
    pub outcome: RetryOutcome,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryOutcome {
    Passed,
    Failed,
    Errored,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ranking {
    pub strategy: RankingStrategy,
    pub is_stable_across_runs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RankingStrategy {
    CascadeRoot,
    CompileBeforeRuntime,
    FirstEmitted,
    RunnerPriority,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub tests_run: u64,
    pub tests_passed: u64,
    pub tests_failed: u64,
    pub tests_errored: u64,
    pub tests_skipped: u64,
    pub tests_not_run: Option<u64>,
    pub files_failed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    pub argv: Vec<String>,
    pub command: String,
    pub cwd: String,
    pub intent: CommandIntent,
    pub interactive: bool,
    pub mutates_workspace: bool,
    pub uses_network: bool,
    pub requires_approval: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandIntent {
    RerunIsolated,
    RerunVerbose,
    RerunWithDebugger,
    UpdateSnapshots,
    RegenerateLockfile,
    ExplainDiagnostic,
    InspectRawLog,
}
