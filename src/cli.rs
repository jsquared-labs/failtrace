use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    time::Instant,
};

use clap::{Parser, Subcommand, error::ErrorKind};
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use unicode_normalization::UnicodeNormalization;

use crate::{
    adapters::{
        AdapterError, AnalyzeOptions, cargo, gotest, jest, pytest, tsc, vitest, xcodebuild,
    },
    schema::{
        Adapter, AdapterKind, Command, Counts, DeterminismWarning, DeterminismWarningCode,
        DocumentKind, EmissionChannel, EmissionMode, OutputDocument, ParseStatus, ParseWarning,
        Ranking, RankingStrategy, RunnerOutcome, SelectionReason, validate,
    },
    storage::{
        PathCaseFolding, RunDir, RunStore, RunStoreOptions, Sidecar, StorageError, StreamMerge,
        sha256_hex,
    },
};

pub(crate) const SCHEMA_VERSION: &str = "1.6";
const PYTEST_ADAPTER_VERSION: &str = "pytest@0.1.0";
const VITEST_ADAPTER_VERSION: &str = "vitest@0.1.0";
const JEST_ADAPTER_VERSION: &str = "jest@0.1.0";
const CARGO_TEST_ADAPTER_VERSION: &str = "cargo-test@0.1.0";
const TSC_ADAPTER_VERSION: &str = "tsc@0.1.0";
const GOTEST_ADAPTER_VERSION: &str = "gotest@0.1.0";
const XCODEBUILD_TEST_ADAPTER_VERSION: &str = "xcodebuild-test@0.1.1";
const CODEX_SHIM_GUARD_ENV: &str = "FAILTRACE_CODEX_SHIM_ACTIVE";

const EX_USAGE: i32 = 64;
const EX_UNAVAILABLE: i32 = 65;
const EX_DATAERR: i32 = 66;
const EX_SOFTWARE: i32 = 70;
const EX_IOERR: i32 = 74;
const EX_CONFIG: i32 = 78;

#[derive(Clone, Debug)]
struct CandidatePlan {
    candidates: Vec<Adapter>,
    source: CandidateSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CandidateSource {
    Direct,
    AmbiguousComposite,
}

#[derive(Debug, Parser)]
#[command(
    name = "failtrace",
    version = env!("FAILTRACE_BUILD_VERSION"),
    about = "Deterministic test and typecheck failure reports for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Run a supported command, capture its output, and emit structured JSON.
    Run(RunArgs),
    /// Re-analyze an existing run directory without executing the command.
    Analyze(AnalyzeArgs),
    /// Print output.json from the latest run in the active runs root.
    Last,
    /// Explain one stored failure from a specific run or the latest run.
    Explain(ExplainArgs),
    /// List the adapters compiled into this binary and their capabilities.
    Adapters,
}

#[derive(Debug, Parser)]
struct RunArgs {
    /// Write artifacts to this exact run directory instead of generating one.
    #[arg(long, value_name = "PATH")]
    run_dir: Option<PathBuf>,
    /// Generate the run directory under ./.failtrace/runs/.
    #[arg(long)]
    here: bool,
    /// Maximum failures emitted on stdout; use 0 for none or 'all' for every failure.
    #[arg(long, default_value = "10", value_name = "N|all")]
    max_failures: MaxFailures,
    /// Supported command and arguments to execute.
    #[arg(
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 1..,
        value_name = "COMMAND"
    )]
    command: Vec<String>,
}

#[derive(Debug, Parser)]
struct AnalyzeArgs {
    /// Maximum failures emitted on stdout; use 0 for none or 'all' for every failure.
    #[arg(long, default_value = "10", value_name = "N|all")]
    max_failures: MaxFailures,
    /// Existing run directory containing log and meta.json.
    #[arg(value_name = "RUN_DIR")]
    run_dir: PathBuf,
}

#[derive(Debug, Parser)]
struct ExplainArgs {
    /// Read this run directory; defaults to the latest run in the active runs root.
    #[arg(long, value_name = "PATH")]
    run_dir: Option<PathBuf>,
    /// Stable 16-hex failure id stored in the selected run.
    #[arg(value_name = "FAILURE_ID")]
    failure_id: String,
}

#[derive(Debug, Clone, Copy)]
enum MaxFailures {
    All,
    Limit(u64),
}

impl std::str::FromStr for MaxFailures {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "all" {
            return Ok(Self::All);
        }
        let limit = value
            .parse::<u64>()
            .map_err(|_| "expected a non-negative integer or 'all'".to_owned())?;
        Ok(Self::Limit(limit))
    }
}

impl MaxFailures {
    fn budget(self) -> Option<u64> {
        match self {
            Self::All => None,
            Self::Limit(limit) => Some(limit),
        }
    }
}

pub fn main() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = match error.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
                _ => EX_USAGE,
            };
            let _ = error.print();
            return code;
        }
    };

    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("failtrace: {error}");
            error.exit_code()
        }
    }
}

fn run(cli: Cli) -> Result<i32, CliError> {
    match cli.command {
        CliCommand::Run(args) => run_command(args),
        CliCommand::Analyze(args) => analyze_command(args),
        CliCommand::Last => last_command(),
        CliCommand::Explain(args) => explain_command(args),
        CliCommand::Adapters => adapters_command(),
    }
}

fn run_command(args: RunArgs) -> Result<i32, CliError> {
    let cwd = std::env::current_dir().map_err(|source| CliError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    let candidate_plan = adapter_candidates_for_command(&args.command, &cwd);
    if candidate_plan.candidates.is_empty() {
        return Err(CliError::AdapterUnavailable(format!(
            "no compiled adapter for command '{}'",
            args.command[0]
        )));
    }
    let store = RunStore::from_env(cwd.clone(), args.run_dir.clone(), args.here)?;
    let run_dir = store.create_run_dir()?;

    let started_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|source| CliError::Internal(source.to_string()))?;
    let started = Instant::now();
    let status = capture_command(&args.command, &cwd, &run_dir)?;
    let duration_ms = millis_u64(started.elapsed().as_millis());
    let log = run_dir.read_log()?;
    let raw_log_sha256 = sha256_hex(&log);

    let sidecar = Sidecar {
        meta_schema_version: "1.2".to_owned(),
        schema_version: SCHEMA_VERSION.to_owned(),
        failtrace_version: env!("CARGO_PKG_VERSION").to_owned(),
        capture_build: Some(concat!("failtrace ", env!("FAILTRACE_BUILD_VERSION")).to_owned()),
        adapters: candidate_plan.candidates.clone(),
        adapter_candidates: candidate_plan.candidates,
        argv: args.command.clone(),
        command: shell_join(&args.command),
        cwd: normalize_cwd(&cwd),
        started_at,
        duration_ms,
        exit_code: status.code(),
        signal: exit_signal(&status),
        early_exit: None,
        truncated: false,
        truncation_reason: None,
        path_case_folding: path_case_folding(&cwd),
        stream_merge: StreamMerge::StdoutStderrInterleaved,
        raw_log_sha256: raw_log_sha256.clone(),
    };
    let analyzed = analyze_log_or_unrecognized(&log, &sidecar, None, candidate_plan.source)?;
    let file_document = run_dir.file_output_document(analyzed, raw_log_sha256);
    ensure_valid(&file_document)?;
    let mut stored_sidecar = sidecar.clone();
    stored_sidecar.adapters = file_document.adapters.clone();
    stored_sidecar.early_exit = file_document.early_exit.clone();
    run_dir.write_sidecar(&stored_sidecar)?;
    run_dir.write_output_document(&file_document)?;
    store.update_latest(&run_dir)?;

    eprintln!(
        "failtrace: run-dir={} output={}",
        run_dir.path().to_string_lossy(),
        run_dir.output_path().to_string_lossy()
    );
    let stdout_document = emitted_document(
        &log,
        &stored_sidecar,
        &run_dir,
        DocumentKind::RunResult,
        args.max_failures,
        &file_document,
        candidate_plan.source,
    )?;
    write_json_line(&stdout_document)?;
    Ok(status.code().unwrap_or(1))
}

fn analyze_command(args: AnalyzeArgs) -> Result<i32, CliError> {
    let run_dir = RunDir::from_existing(abs_path(args.run_dir)?);
    let log = run_dir.read_log()?;
    let sidecar = run_dir.read_sidecar()?;
    if !is_compatible_sidecar_schema(&sidecar.schema_version) {
        return Err(CliError::Sidecar(format!(
            "incompatible schema version: meta.json has {}, binary emits {SCHEMA_VERSION}",
            sidecar.schema_version
        )));
    }
    let actual_sha = sha256_hex(&log);
    if sidecar.raw_log_sha256 != actual_sha {
        return Err(CliError::Sidecar(
            "sidecar raw_log_sha256 does not match log".to_owned(),
        ));
    }

    let candidate_source = replay_candidate_source(&sidecar);
    let mut full_document = analyze_log_or_unrecognized(&log, &sidecar, None, candidate_source)?;
    full_document.document_kind = DocumentKind::AnalysisResult;
    full_document.raw_log_path = run_dir.log_path().to_string_lossy().into_owned();
    full_document.emission.full_output_path = run_dir.output_path().to_string_lossy().into_owned();
    ensure_valid(&full_document)?;

    eprintln!(
        "failtrace: run-dir={} output={}",
        run_dir.path().to_string_lossy(),
        run_dir.output_path().to_string_lossy()
    );
    let stdout_document = emitted_document(
        &log,
        &sidecar,
        &run_dir,
        DocumentKind::AnalysisResult,
        args.max_failures,
        &full_document,
        candidate_source,
    )?;
    write_json_line(&stdout_document)?;
    Ok(0)
}

fn is_compatible_sidecar_schema(schema_version: &str) -> bool {
    matches!(schema_version, SCHEMA_VERSION | "1.5" | "1.4")
}

fn replay_candidate_source(sidecar: &Sidecar) -> CandidateSource {
    // Composite-ambiguous shape is "non-empty adapter list with no selected
    // adapter". Gating on `adapter_candidates.is_empty()` would regress
    // backward-compatible replay for v1.4 sidecars, which do not carry the
    // `adapter_candidates` field (it defaults to empty on deserialization).
    // The selection state of `sidecar.adapters` itself is the load-bearing
    // signal.
    let any_selected = sidecar.adapters.iter().any(|adapter| adapter.selected);
    if !sidecar.adapters.is_empty() && !any_selected {
        CandidateSource::AmbiguousComposite
    } else {
        CandidateSource::Direct
    }
}

fn last_command() -> Result<i32, CliError> {
    let cwd = std::env::current_dir().map_err(|source| CliError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    let store = RunStore::new(RunStoreOptions::from_env(cwd, None, false))?;
    let latest = store.read_latest()?;
    let output_path = latest.join("output.json");
    let bytes = fs::read(&output_path).map_err(|source| CliError::Io {
        path: output_path,
        source,
    })?;
    io::stdout()
        .lock()
        .write_all(&bytes)
        .map_err(|source| CliError::Io {
            path: PathBuf::from("<stdout>"),
            source,
        })?;
    Ok(0)
}

fn explain_command(args: ExplainArgs) -> Result<i32, CliError> {
    let run_dir_path = match args.run_dir {
        Some(path) => abs_path(path)?,
        None => {
            let cwd = std::env::current_dir().map_err(|source| CliError::Io {
                path: PathBuf::from("."),
                source,
            })?;
            RunStore::new(RunStoreOptions::from_env(cwd, None, false))?.read_latest()?
        }
    };
    let run_dir = RunDir::from_existing(run_dir_path);
    let document = run_dir.read_output_document()?;
    let Some(failure) = document
        .failures
        .iter()
        .find(|failure| failure.failure_id == args.failure_id)
    else {
        return Err(CliError::FailureNotFound(args.failure_id));
    };

    let byte_range = failure
        .evidence
        .as_ref()
        .map(|evidence| evidence.byte_range)
        .unwrap_or(crate::schema::ByteRange { start: 0, end: 0 });
    let (rule_id, rule_description) = explanation_rule(&failure.adapter_id);
    let explanation = ExplanationResult {
        schema_version: SCHEMA_VERSION.to_owned(),
        document_kind: "explanation_result",
        failtrace_version: env!("CARGO_PKG_VERSION").to_owned(),
        failure_id: failure.failure_id.clone(),
        run_dir: run_dir.path().to_string_lossy().into_owned(),
        summary: format!("{} {}: {}", failure.phase, failure.kind, failure.message),
        why_classified: vec![
            format!("{rule_id}:matched"),
            format!("failure_kind:{:?}", failure.kind),
        ],
        matched_rules: vec![MatchedRule {
            rule_id: rule_id.to_owned(),
            description: rule_description.to_owned(),
            span: RuleSpan { byte_range },
        }],
        raw_log_path: document.raw_log_path.clone(),
        failure: failure.clone(),
        raw_log_excerpts: failure
            .evidence
            .as_ref()
            .map(|evidence| evidence.excerpts.clone())
            .unwrap_or_default(),
        suggested_next_commands: document.suggested_next_commands.clone(),
    };
    write_json_line(&explanation)?;
    Ok(0)
}

fn explanation_rule(adapter_id: &str) -> (&'static str, &'static str) {
    match adapter_id {
        "pytest" => (
            "pytest_minimum_failure_rule",
            "pytest failure or error evidence matched a reportable failure",
        ),
        "vitest" => (
            "vitest_minimum_failure_rule",
            "vitest failure recap matched a reportable failure",
        ),
        "jest" => (
            "jest_minimum_failure_rule",
            "jest failure recap matched a reportable failure",
        ),
        "cargo-test" => (
            "cargo_test_minimum_failure_rule",
            "cargo diagnostic or libtest evidence matched a reportable failure",
        ),
        "tsc" => (
            "tsc_minimum_failure_rule",
            "TypeScript diagnostic evidence matched a reportable failure",
        ),
        "gotest" => (
            "gotest_minimum_failure_rule",
            "go test JSON event evidence matched a reportable failure",
        ),
        "xcodebuild-test" => (
            "xcodebuild_test_minimum_failure_rule",
            "xcodebuild diagnostic or test evidence matched a reportable failure",
        ),
        _ => (
            "adapter_minimum_failure_rule",
            "the selected adapter matched a reportable failure",
        ),
    }
}

fn adapters_command() -> Result<i32, CliError> {
    write_json_line(&AdaptersResult {
        schema_version: SCHEMA_VERSION.to_owned(),
        document_kind: "adapters_result",
        failtrace_version: env!("CARGO_PKG_VERSION").to_owned(),
        adapters: vec![
            AdapterInfo {
                id: "pytest",
                version: PYTEST_ADAPTER_VERSION,
                kind: "runner",
                supported_commands: vec!["pytest"],
                minimum_failure_rule: "pytest_minimum_failure_rule",
                strip_params_rule: Some("strip_params_pytest"),
                capabilities: vec![
                    "collection_errors",
                    "assertions",
                    "parameterized_tests",
                    "teardown_errors",
                ],
            },
            AdapterInfo {
                id: "vitest",
                version: VITEST_ADAPTER_VERSION,
                kind: "runner",
                supported_commands: vec!["vitest"],
                minimum_failure_rule: "vitest_minimum_failure_rule",
                strip_params_rule: None,
                capabilities: vec!["assertions"],
            },
            AdapterInfo {
                id: "jest",
                version: JEST_ADAPTER_VERSION,
                kind: "runner",
                supported_commands: vec!["jest"],
                minimum_failure_rule: "jest_minimum_failure_rule",
                strip_params_rule: None,
                capabilities: vec!["assertions", "pre_test_failures"],
            },
            AdapterInfo {
                id: "cargo-test",
                version: CARGO_TEST_ADAPTER_VERSION,
                kind: "runner",
                supported_commands: vec!["cargo test", "cargo t"],
                minimum_failure_rule: "cargo_test_minimum_failure_rule",
                strip_params_rule: None,
                capabilities: vec![
                    "rustc_diagnostics",
                    "borrow_checker_errors",
                    "dependency_resolution",
                    "build_script_panics",
                    "libtest_failures",
                    "doctests",
                ],
            },
            AdapterInfo {
                id: "tsc",
                version: TSC_ADAPTER_VERSION,
                kind: "typechecker",
                supported_commands: vec![
                    "tsc",
                    "npx tsc",
                    "npm exec tsc",
                    "pnpm tsc",
                    "yarn tsc",
                    "bunx tsc",
                ],
                minimum_failure_rule: "tsc_minimum_failure_rule",
                strip_params_rule: None,
                capabilities: vec!["type_errors", "missing_symbols", "syntax_errors"],
            },
            AdapterInfo {
                id: "gotest",
                version: GOTEST_ADAPTER_VERSION,
                kind: "runner",
                supported_commands: vec!["go test -json"],
                minimum_failure_rule: "gotest_minimum_failure_rule",
                strip_params_rule: None,
                capabilities: vec![
                    "build_failures",
                    "assertions",
                    "subtests",
                    "panics",
                    "race_detector",
                    "timeouts",
                ],
            },
            AdapterInfo {
                id: "xcodebuild-test",
                version: XCODEBUILD_TEST_ADAPTER_VERSION,
                kind: "runner",
                supported_commands: vec![
                    "xcodebuild test",
                    "xcodebuild test-without-building",
                    "xcodebuild clean test",
                    "xcrun xcodebuild test",
                ],
                minimum_failure_rule: "xcodebuild_test_minimum_failure_rule",
                strip_params_rule: None,
                capabilities: vec![
                    "swift_compile_errors",
                    "xctest_assertions",
                    "swift_testing_expectations",
                ],
            },
        ],
    })?;
    Ok(0)
}

fn capture_command(
    argv: &[String],
    cwd: &Path,
    run_dir: &RunDir,
) -> Result<std::process::ExitStatus, CliError> {
    let log_path = run_dir.log_path();
    let log_file = open_log_file(&log_path)?;
    let stdout = log_file.try_clone().map_err(|source| CliError::Io {
        path: log_path.clone(),
        source,
    })?;

    let mut command = ProcessCommand::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .env(CODEX_SHIM_GUARD_ENV, "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(log_file));
    command.status().map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            // Distinguish PATH lookup from a literal-path execution: argv[0]
            // with a `/` was a path the user supplied, not a PATH lookup, so
            // the wording must not blame PATH. In both cases we include the
            // original IO source so the remaining ENOENT variants (PATH empty,
            // interpreter missing, etc.) are still legible.
            let argv0 = &argv[0];
            let message = if argv0.contains('/') {
                format!("'{argv0}' could not be executed: {source}")
            } else {
                format!("'{argv0}' not found in PATH: {source}")
            };
            CliError::AdapterUnavailable(message)
        } else {
            CliError::Io {
                path: PathBuf::from(&argv[0]),
                source,
            }
        }
    })
}

fn open_log_file(path: &Path) -> Result<fs::File, CliError> {
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|source| CliError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn analyze_log_or_unrecognized(
    log: &[u8],
    sidecar: &Sidecar,
    max_failures: Option<u64>,
    candidate_source: CandidateSource,
) -> Result<OutputDocument, CliError> {
    let candidates = if sidecar.adapter_candidates.is_empty() {
        sidecar.adapters.clone()
    } else {
        sidecar.adapter_candidates.clone()
    };
    if candidate_source == CandidateSource::AmbiguousComposite {
        return Ok(composite_runtime_deferred_document(
            log,
            sidecar,
            &candidates,
        ));
    }
    let sidecar = if sidecar
        .adapters
        .iter()
        .filter(|adapter| adapter.selected)
        .count()
        == 1
    {
        sidecar.clone()
    } else if !candidates.is_empty() {
        sidecar_with_selected_adapter(sidecar, &candidates, 0, SelectionReason::ArgvNameMatch)
    } else {
        return Ok(unsupported_command_document(log, sidecar));
    };
    let Some(adapter_id) = selected_adapter_id(&sidecar) else {
        return Err(CliError::Sidecar(
            "sidecar does not select exactly one adapter".to_owned(),
        ));
    };
    analyze_selected_adapter(log, &sidecar, adapter_id, max_failures)
}

fn analyze_selected_adapter(
    log: &[u8],
    sidecar: &Sidecar,
    adapter_id: &str,
    max_failures: Option<u64>,
) -> Result<OutputDocument, CliError> {
    let result = match adapter_id {
        "jest" => jest::analyze_log(log, sidecar, AnalyzeOptions { max_failures }),
        "cargo-test" => cargo::analyze_log(log, sidecar, AnalyzeOptions { max_failures }),
        "tsc" => tsc::analyze_log(log, sidecar, AnalyzeOptions { max_failures }),
        "gotest" => gotest::analyze_log(log, sidecar, AnalyzeOptions { max_failures }),
        "pytest" => pytest::analyze_log(log, sidecar, AnalyzeOptions { max_failures }),
        "vitest" => vitest::analyze_log(log, sidecar, AnalyzeOptions { max_failures }),
        "xcodebuild-test" => xcodebuild::analyze_log(log, sidecar, AnalyzeOptions { max_failures }),
        other => {
            return Err(CliError::AdapterUnavailable(format!(
                "no compiled adapter for sidecar adapter '{other}'"
            )));
        }
    };
    let mut document = match result {
        Ok(document) => Ok(document),
        Err(AdapterError::UnrecognizedFormat { .. }) => Ok(unrecognized_document(
            log,
            sidecar,
            ParseStatus::UnparsedOutput,
        )),
        Err(error) => Err(CliError::Adapter(error)),
    }?;
    if adapter_id == "xcodebuild-test"
        && let Some(captured) = sidecar.adapters.iter().find(|adapter| adapter.selected)
        && captured.version != XCODEBUILD_TEST_ADAPTER_VERSION
    {
        document.determinism_warnings.push(DeterminismWarning {
            code: DeterminismWarningCode::AdapterVersionDrift,
            adapter_id: adapter_id.to_owned(),
            meta_version: captured.version.clone(),
            current_version: XCODEBUILD_TEST_ADAPTER_VERSION.to_owned(),
        });
    }
    Ok(document)
}

fn sidecar_with_selected_adapter(
    sidecar: &Sidecar,
    candidates: &[Adapter],
    selected_index: usize,
    selected_reason: SelectionReason,
) -> Sidecar {
    let mut probe = sidecar.clone();
    probe.adapters = candidates
        .iter()
        .enumerate()
        .map(|(index, adapter)| {
            let mut adapter = adapter.clone();
            adapter.selected = index == selected_index;
            adapter.selection_reason = if adapter.selected {
                selected_reason
            } else {
                SelectionReason::NotSelected
            };
            adapter
        })
        .collect();
    probe
}

fn selected_adapter_id(sidecar: &Sidecar) -> Option<&str> {
    let mut selected = sidecar
        .adapters
        .iter()
        .filter(|adapter| adapter.selected)
        .map(|adapter| adapter.id.as_str());
    let first = selected.next()?;
    if selected.next().is_some() {
        None
    } else {
        Some(first)
    }
}

fn unrecognized_document(
    log: &[u8],
    sidecar: &Sidecar,
    parse_status: ParseStatus,
) -> OutputDocument {
    OutputDocument {
        // Always emit the binary's schema version. When `analyze` replays a
        // legacy sidecar, the produced document still carries the current
        // shape, so it must declare the current schema version.
        schema_version: SCHEMA_VERSION.to_owned(),
        document_kind: DocumentKind::AnalysisResult,
        failtrace_version: env!("CARGO_PKG_VERSION").to_owned(),
        emission: crate::schema::Emission {
            channel: EmissionChannel::File,
            mode: EmissionMode::Full,
            max_failures: None,
            failures_emitted: 0,
            failures_total: 0,
            full_output_path: "<run-dir>/output.json".to_owned(),
        },
        command: sidecar.command.clone(),
        cwd: sidecar.cwd.clone(),
        started_at: sidecar.started_at.clone(),
        duration_ms: sidecar.duration_ms,
        exit_code: sidecar.exit_code,
        signal: sidecar.signal.clone(),
        runner_outcome: if sidecar.exit_code == Some(0) {
            RunnerOutcome::Passed
        } else {
            RunnerOutcome::HarnessCrashed
        },
        early_exit: sidecar.early_exit.clone(),
        adapters: sidecar.adapters.clone(),
        parse_status,
        parse_warnings: vec![],
        determinism_warnings: determinism_warnings_for_sidecar(sidecar),
        truncated: sidecar.truncated,
        truncation_reason: sidecar.truncation_reason,
        failures_complete: false,
        failures: vec![],
        ranking: Ranking {
            strategy: RankingStrategy::None,
            is_stable_across_runs: true,
        },
        counts: Counts {
            tests_run: 0,
            tests_passed: 0,
            tests_failed: 0,
            tests_errored: 0,
            tests_skipped: 0,
            tests_not_run: None,
            files_failed: None,
        },
        raw_log_path: "<run-dir>/log".to_owned(),
        raw_log_sha256: if sidecar.raw_log_sha256.is_empty() {
            sha256_hex(log)
        } else {
            sidecar.raw_log_sha256.clone()
        },
        suggested_next_commands: vec![],
        extensions: BTreeMap::new(),
    }
}

fn determinism_warnings_for_sidecar(sidecar: &Sidecar) -> Vec<DeterminismWarning> {
    if sidecar.schema_version == SCHEMA_VERSION {
        return vec![];
    }
    vec![DeterminismWarning {
        code: DeterminismWarningCode::SchemaVersionDriftMinor,
        adapter_id: selected_determinism_adapter_id(sidecar),
        meta_version: sidecar.schema_version.clone(),
        current_version: SCHEMA_VERSION.to_owned(),
    }]
}

fn selected_determinism_adapter_id(sidecar: &Sidecar) -> String {
    sidecar
        .adapters
        .iter()
        .find(|adapter| adapter.selected)
        .or_else(|| sidecar.adapters.first())
        .map(|adapter| adapter.id.clone())
        .unwrap_or_else(|| "core".to_owned())
}

fn unsupported_command_document(log: &[u8], sidecar: &Sidecar) -> OutputDocument {
    unrecognized_document(log, sidecar, ParseStatus::UnsupportedCommand)
}

fn composite_runtime_deferred_document(
    log: &[u8],
    sidecar: &Sidecar,
    candidates: &[Adapter],
) -> OutputDocument {
    let mut deferred_sidecar = sidecar.clone();
    deferred_sidecar.adapters = candidates
        .iter()
        .cloned()
        .map(|mut adapter| {
            adapter.selected = false;
            adapter.selection_reason = SelectionReason::NotSelected;
            adapter
        })
        .collect();

    let mut document = unsupported_command_document(log, &deferred_sidecar);
    document
        .parse_warnings
        .push(ParseWarning::CompositeRuntimeDeferred);
    document
}

fn emitted_document(
    log: &[u8],
    sidecar: &Sidecar,
    run_dir: &RunDir,
    document_kind: DocumentKind,
    max_failures: MaxFailures,
    full_document: &OutputDocument,
    candidate_source: CandidateSource,
) -> Result<OutputDocument, CliError> {
    let mut document = match max_failures.budget() {
        Some(limit) if full_document.failures.len() as u64 > limit => {
            let document =
                analyze_log_or_unrecognized(log, sidecar, Some(limit), candidate_source)?;
            place_document_in_run_dir(document, run_dir, document_kind)
        }
        _ => full_document.clone(),
    };

    document.document_kind = document_kind;
    document.emission.channel = EmissionChannel::Stdout;
    match max_failures.budget() {
        None => {
            document.emission.mode = EmissionMode::Full;
            document.emission.max_failures = None;
        }
        Some(limit) => {
            document.emission.mode = EmissionMode::Budgeted;
            document.emission.max_failures = Some(limit);
        }
    }
    document.emission.failures_emitted = document.failures.len() as u64;
    document.emission.failures_total = full_document.emission.failures_total;
    ensure_valid(&document)?;
    Ok(document)
}

fn place_document_in_run_dir(
    mut document: OutputDocument,
    run_dir: &RunDir,
    document_kind: DocumentKind,
) -> OutputDocument {
    document.document_kind = document_kind;
    document.raw_log_path = run_dir.log_path().to_string_lossy().into_owned();
    document.emission.full_output_path = run_dir.output_path().to_string_lossy().into_owned();
    document
}

fn ensure_valid(document: &OutputDocument) -> Result<(), CliError> {
    let violations = validate(document);
    if violations.is_empty() {
        Ok(())
    } else {
        Err(CliError::Internal(format!(
            "schema invariant violation: {violations:?}"
        )))
    }
}

fn write_json_line<T: Serialize>(value: &T) -> Result<(), CliError> {
    let mut bytes = serde_json::to_vec(value).map_err(|source| CliError::Json {
        path: PathBuf::from("<stdout>"),
        source,
    })?;
    bytes.push(b'\n');
    let mut stdout = io::stdout().lock();
    stdout.write_all(&bytes).map_err(|source| CliError::Io {
        path: PathBuf::from("<stdout>"),
        source,
    })
}

fn pytest_adapter(selected: bool) -> Adapter {
    Adapter {
        id: "pytest".to_owned(),
        version: PYTEST_ADAPTER_VERSION.to_owned(),
        kind: AdapterKind::Runner,
        selected,
        selection_reason: if selected {
            SelectionReason::ArgvNameMatch
        } else {
            SelectionReason::NotSelected
        },
    }
}

fn vitest_adapter(selected: bool) -> Adapter {
    Adapter {
        id: "vitest".to_owned(),
        version: VITEST_ADAPTER_VERSION.to_owned(),
        kind: AdapterKind::Runner,
        selected,
        selection_reason: if selected {
            SelectionReason::ArgvNameMatch
        } else {
            SelectionReason::NotSelected
        },
    }
}

fn jest_adapter(selected: bool) -> Adapter {
    Adapter {
        id: "jest".to_owned(),
        version: JEST_ADAPTER_VERSION.to_owned(),
        kind: AdapterKind::Runner,
        selected,
        selection_reason: if selected {
            SelectionReason::ArgvNameMatch
        } else {
            SelectionReason::NotSelected
        },
    }
}

fn cargo_test_adapter(selected: bool) -> Adapter {
    Adapter {
        id: "cargo-test".to_owned(),
        version: CARGO_TEST_ADAPTER_VERSION.to_owned(),
        kind: AdapterKind::Runner,
        selected,
        selection_reason: if selected {
            SelectionReason::ArgvNameMatch
        } else {
            SelectionReason::NotSelected
        },
    }
}

fn tsc_adapter(selected: bool) -> Adapter {
    Adapter {
        id: "tsc".to_owned(),
        version: TSC_ADAPTER_VERSION.to_owned(),
        kind: AdapterKind::Typechecker,
        selected,
        selection_reason: if selected {
            SelectionReason::ArgvNameMatch
        } else {
            SelectionReason::NotSelected
        },
    }
}

fn gotest_adapter(selected: bool) -> Adapter {
    Adapter {
        id: "gotest".to_owned(),
        version: GOTEST_ADAPTER_VERSION.to_owned(),
        kind: AdapterKind::Runner,
        selected,
        selection_reason: if selected {
            SelectionReason::ArgvNameMatch
        } else {
            SelectionReason::NotSelected
        },
    }
}

fn xcodebuild_test_adapter(selected: bool) -> Adapter {
    Adapter {
        id: "xcodebuild-test".to_owned(),
        version: XCODEBUILD_TEST_ADAPTER_VERSION.to_owned(),
        kind: AdapterKind::Runner,
        selected,
        selection_reason: if selected {
            SelectionReason::ArgvNameMatch
        } else {
            SelectionReason::NotSelected
        },
    }
}

fn adapter_candidates_for_command(argv: &[String], cwd: &Path) -> CandidatePlan {
    if let Some(segments) = composite_command_segments(argv) {
        return plan_for_segments(&segments);
    }
    if let Some(segments) = package_test_script_segments(argv, cwd) {
        // Package test scripts can never be "transparent" the way a shell
        // `-c "<single command>"` can: `npm test`, `pnpm test`, and `yarn test`
        // implicitly invoke `pretest` / `posttest` lifecycle hooks before and
        // after the resolved `scripts.test` command. Even if `scripts.test`
        // expands to one recognized runner (e.g., `"test": "pytest"`), a
        // failing `pretest` hook would land its failure in the same captured
        // log and be mis-attributed to that runner's parser. Force ambiguous
        // composite for the entire `npm|pnpm|yarn test` surface — the lone
        // recognized candidate is recorded unselected, defer.
        return plan_package_script_segments(&segments);
    }
    let candidates = adapter_for_argv(argv).into_iter().collect();
    CandidatePlan {
        candidates,
        source: CandidateSource::Direct,
    }
}

fn plan_for_segments(segments: &[Vec<String>]) -> CandidatePlan {
    let mut adapters = segments
        .iter()
        .filter_map(|segment| adapter_for_argv(segment))
        .collect::<Vec<_>>();
    for adapter in &mut adapters {
        adapter.selected = false;
        adapter.selection_reason = SelectionReason::NotSelected;
    }
    CandidatePlan {
        candidates: adapters,
        source: CandidateSource::AmbiguousComposite,
    }
}

fn plan_package_script_segments(segments: &[Vec<String>]) -> CandidatePlan {
    // Package-manager `test` scripts carry implicit `pretest`/`posttest`
    // lifecycle hooks that the manifest does not surface as separate segments;
    // a lone recognized runner is therefore still ambiguous in the captured
    // log. Collect recognized candidates, mark all unselected, return
    // AmbiguousComposite regardless of segment count.
    let mut adapters = Vec::new();
    for segment in segments {
        if let Some(mut adapter) = adapter_for_argv(segment) {
            adapter.selected = false;
            adapter.selection_reason = SelectionReason::NotSelected;
            adapters.push(adapter);
        }
    }
    CandidatePlan {
        candidates: adapters,
        source: CandidateSource::AmbiguousComposite,
    }
}

fn adapter_for_argv(argv: &[String]) -> Option<Adapter> {
    if is_pytest_command(argv) {
        Some(pytest_adapter(true))
    } else if is_vitest_command(argv) {
        Some(vitest_adapter(true))
    } else if is_jest_command(argv) {
        Some(jest_adapter(true))
    } else if is_cargo_test_command(argv) {
        Some(cargo_test_adapter(true))
    } else if is_tsc_command(argv) {
        Some(tsc_adapter(true))
    } else if is_go_test_command(argv) {
        Some(gotest_adapter(true))
    } else if is_xcodebuild_test_command(argv) {
        Some(xcodebuild_test_adapter(true))
    } else {
        None
    }
}

/// Detects `xcodebuild test` / `xcodebuild test-without-building` /
/// `xcodebuild clean test`, including `xcrun xcodebuild test` and absolute paths.
/// Rejects `xcodebuild build`, `xcodebuild archive`, `xcodebuild build-for-testing`,
/// and `xcodebuild` with no recognized action verb. See
/// `docs/design/xcodebuild-test-adapter.md` §Detector.
fn is_xcodebuild_test_command(argv: &[String]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    let basename = Path::new(first)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(first);
    // Strip the `xcrun [<options>...]` prefix with an explicit flag table.
    // The previous heuristic (consume-any-token-after-`--`) would mis-consume
    // `xcodebuild` itself when the user passed boolean long flags like
    // `xcrun --verbose xcodebuild test`. xcrun's actual interface is small
    // enough that an explicit table is both correct and conservative.
    let argv_after_xcrun: &[String] = if basename == "xcrun" {
        // xcrun value-flags (consume the following token as the flag's value).
        // Per `man xcrun`: only `--sdk` and `--toolchain` take values.
        // `--run`/`-r` is deliberately absent — it's a boolean mode switch
        // ("Enable run mode"), no argument. Treating it as a value flag would
        // make `xcrun --run xcodebuild test` consume `xcodebuild` as its
        // value and false-reject the command.
        const XCRUN_VALUE_FLAGS: &[&str] = &["-sdk", "--sdk", "-toolchain", "--toolchain"];
        // xcrun options whose presence means "no execution will happen" —
        // short-circuit the claim. Per `man xcrun`:
        //   -f/--find: print resolved tool path, do not execute
        //   -h/--help: print help, exit
        //   --version: print version, exit
        //   --show-{sdk,platform}-*: print metadata, do not execute
        // Boolean modifiers that still execute (`-r`/`--run`, `-l`/`--log`,
        // `-n`/`--no-cache`, `-k`/`--kill-cache`, `-v`/`--verbose`) are NOT
        // in this list — they fall through to the catch-all boolean branch
        // below.
        // Full --show-* family per `man xcrun` — each prints metadata and
        // does not execute the wrapped tool. (`--show-platform-info` is not a
        // real xcrun flag and is deliberately absent.)
        const XCRUN_REJECT_OPTIONS: &[&str] = &[
            "-f",
            "--find",
            "-h",
            "--help",
            "--version",
            "--show-sdk-version",
            "--show-sdk-build-version",
            "--show-sdk-path",
            "--show-sdk-platform-path",
            "--show-sdk-platform-version",
            "--show-toolchain-path",
        ];
        let mut i = 1;
        while i < argv.len() {
            let arg = argv[i].as_str();
            if XCRUN_REJECT_OPTIONS.contains(&arg) {
                return false;
            }
            if XCRUN_VALUE_FLAGS.contains(&arg) {
                // Consume the flag + its value.
                i += 2;
                continue;
            }
            if arg.starts_with('-') {
                // Unknown xcrun flag — treat as boolean (consumes 0 tokens).
                // Conservative: rather than assume it consumes a value (the
                // old heuristic, which sometimes ate `xcodebuild`), assume
                // none. If xcrun gains a new value-flag we miss, the worst
                // outcome is the detector wrongly thinks xcodebuild is the
                // flag's value and rejects — same direction as the existing
                // failure mode for unknown shapes.
                i += 1;
                continue;
            }
            break;
        }
        // Clamp the slice start to argv.len() to handle malformed input
        // where a value-flag appears as the final token (e.g.
        // `xcrun --sdk` with no value following). The value-flag branch
        // unconditionally advances `i += 2`, which can leave `i > argv.len()`
        // and panic on a bare `&argv[i..]` slice. `argv.get(..).unwrap_or(&[])`
        // returns an empty slice in that case, falling through to the
        // "no xcodebuild seen" rejection rather than crashing failtrace
        argv.get(i..).unwrap_or(&[])
    } else {
        argv
    };
    let Some(cmd) = argv_after_xcrun.first() else {
        return false;
    };
    let cmd_basename = Path::new(cmd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(cmd);
    if cmd_basename != "xcodebuild" {
        return false;
    }
    // Scan for a recognized test action verb (`test` or `test-without-building`).
    // xcodebuild(1) defines `action ...` as "Specify one or more actions to
    // perform" — so `xcodebuild clean build test` and `xcodebuild build test`
    // are both valid multi-action invocations where a test phase executes.
    // The detector therefore must NOT early-reject on seeing
    // `build`/`archive`/`analyze`/`install` before `test`; the only correct
    // rejection is "no test action appears anywhere in the argv". See the
    // hook test battery for `clean build test`-style cases.
    //
    // xcodebuild value-flags consume the following token as the flag's value
    // — without this skip, `xcodebuild -derivedDataPath build test ...` would
    // mis-classify the path value `build` as an action verb. Boolean flags
    // (`-quiet`, `-verbose`, `-allowProvisioningUpdates`) and joined flags
    // (`-only-testing:ExampleAppTests/Foo`) consume zero tokens.
    const XCODEBUILD_VALUE_FLAGS: &[&str] = &[
        "-project",
        "-target",
        "-scheme",
        "-configuration",
        "-sdk",
        "-arch",
        "-destination",
        "-destination-timeout",
        "-derivedDataPath",
        "-resultBundlePath",
        "-resultBundleVersion",
        "-toolchain",
        "-archivePath",
        "-exportPath",
        "-exportOptionsPlist",
        "-workspace",
        "-xcconfig",
        "-testPlan",
        "-parallel-testing-worker-count",
        "-maximum-parallel-testing-workers",
        "-maximum-concurrent-test-device-destinations",
        "-maximum-concurrent-test-simulator-destinations",
    ];
    let rest = &argv_after_xcrun[1..];
    if rest.iter().any(|arg| arg == "-workspace") {
        return false;
    }
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].as_str();
        if XCODEBUILD_VALUE_FLAGS.contains(&arg) {
            // Consume the flag and its value; the value cannot be an action verb.
            i += 2;
            continue;
        }
        if matches!(arg, "test" | "test-without-building") {
            return true;
        }
        i += 1;
    }
    false
}

fn composite_command_segments(argv: &[String]) -> Option<Vec<Vec<String>>> {
    let shell = Path::new(argv.first()?)
        .file_name()
        .and_then(|name| name.to_str())?;
    if !matches!(shell, "bash" | "sh" | "zsh") || argv.get(1).map(String::as_str) != Some("-c") {
        return None;
    }
    let expression = argv.get(2)?;
    let segments = parse_top_level_and_chain(expression)?;
    (segments.len() > 1).then_some(segments)
}

fn package_test_script_segments(argv: &[String], cwd: &Path) -> Option<Vec<Vec<String>>> {
    let runner = Path::new(argv.first()?)
        .file_name()
        .and_then(|name| name.to_str())?;
    let is_test_command = match runner {
        "npm" | "pnpm" | "yarn" => argv.get(1).map(String::as_str) == Some("test"),
        _ => false,
    };
    if !is_test_command {
        return None;
    }
    let package_json = fs::read_to_string(cwd.join("package.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&package_json).ok()?;
    let script = value
        .get("scripts")
        .and_then(|scripts| scripts.get("test"))
        .and_then(|script| script.as_str())?;
    parse_top_level_and_chain(script)
}

fn parse_top_level_and_chain(expression: &str) -> Option<Vec<Vec<String>>> {
    let mut segments = Vec::new();
    for segment in split_top_level_and(expression)? {
        if has_forbidden_shell_metachar(segment) {
            segments.push(Vec::new());
            continue;
        }
        let Some(argv) = split_simple_shell_words(segment) else {
            segments.push(Vec::new());
            continue;
        };
        if argv.is_empty() {
            segments.push(Vec::new());
            continue;
        }
        segments.push(argv);
    }
    Some(segments)
}

fn split_top_level_and(expression: &str) -> Option<Vec<&str>> {
    let bytes = expression.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'\\' {
            i += 2;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = if quote == Some(byte) {
                None
            } else if quote.is_none() {
                Some(byte)
            } else {
                quote
            };
            i += 1;
            continue;
        }
        if quote.is_none() && i + 1 < bytes.len() && byte == b'&' && bytes[i + 1] == b'&' {
            parts.push(expression[start..i].trim());
            i += 2;
            start = i;
            continue;
        }
        i += 1;
    }
    if quote.is_some() {
        return None;
    }
    parts.push(expression[start..].trim());
    Some(parts)
}

fn has_forbidden_shell_metachar(segment: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut chars = segment.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            chars.next();
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = if quote == Some(ch) {
                None
            } else if quote.is_none() {
                Some(ch)
            } else {
                quote
            };
            continue;
        }
        if quote.is_none() {
            if matches!(ch, '|' | ';' | '&' | '<' | '>' | '`') {
                return true;
            }
            if ch == '$' && chars.peek() == Some(&'(') {
                return true;
            }
        }
    }
    false
}

fn split_simple_shell_words(segment: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = segment.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            current.push(chars.next()?);
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = if quote == Some(ch) {
                None
            } else if quote.is_none() {
                Some(ch)
            } else {
                quote
            };
            continue;
        }
        if quote.is_none() && ch.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

fn is_pytest_command(argv: &[String]) -> bool {
    detect_pytest(argv)
}

pub(crate) fn is_vitest_command(argv: &[String]) -> bool {
    detect_vitest(argv)
}

fn is_jest_command(argv: &[String]) -> bool {
    detect_jest(argv)
}

fn is_cargo_test_command(argv: &[String]) -> bool {
    detect_cargo_test(argv)
}

fn is_tsc_command(argv: &[String]) -> bool {
    detect_tsc(argv)
}

fn is_go_test_command(argv: &[String]) -> bool {
    detect_go_test(argv)
}

fn detect_pytest(argv: &[String]) -> bool {
    pytest::command_is_supported(argv)
}

fn skip_wrapper_flags(args: &[String], value_flags: &[&str]) -> usize {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            return i + 1;
        }
        if !arg.starts_with('-') {
            return i;
        }
        if arg.contains('=') {
            i += 1;
        } else if value_flags.contains(&arg.as_str()) {
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

fn boolean_flag_enabled(argv: &[String], long: &str, short: Option<&str>) -> bool {
    argv.iter().any(|arg| {
        arg == long
            || short.is_some_and(|short| arg == short)
            || arg
                .strip_prefix(long)
                .and_then(|value| value.strip_prefix('='))
                .is_some_and(|value| value != "false")
    })
}

fn detect_vitest(argv: &[String]) -> bool {
    if boolean_flag_enabled(argv, "--watch", Some("-w"))
        || boolean_flag_enabled(argv, "--ui", None)
        || boolean_flag_enabled(argv, "--standalone", None)
    {
        return false;
    }
    let Some(first) = argv.first() else {
        return false;
    };
    let Some(basename) = Path::new(first).file_name().and_then(|n| n.to_str()) else {
        return false;
    };

    if basename == "vitest" {
        return vitest_args_are_explicitly_one_shot(&argv[1..]);
    }
    if basename == "npx" {
        return detect_wrapped_vitest(&argv[1..], NPM_VALUE_FLAGS);
    }
    if basename == "npm" {
        return detect_npm_vitest(&argv[1..]);
    }
    if basename == "pnpm" {
        return detect_pnpm_vitest(&argv[1..]);
    }
    if basename == "yarn" {
        return detect_yarn_vitest(&argv[1..]);
    }
    if basename == "bunx" {
        return detect_wrapped_vitest(&argv[1..], BUN_VALUE_FLAGS);
    }
    if basename == "bun" {
        let offset = skip_wrapper_flags(&argv[1..], BUN_VALUE_FLAGS);
        let i = 1 + offset;
        if argv.get(i).map(String::as_str) != Some("x") {
            return false;
        }
        return detect_wrapped_vitest(&argv[i + 1..], BUN_VALUE_FLAGS);
    }
    false
}

fn detect_pnpm_vitest(args: &[String]) -> bool {
    let offset = skip_wrapper_flags(args, PNPM_VALUE_FLAGS);
    let Some(command) = args.get(offset).map(String::as_str) else {
        return false;
    };
    match command {
        "vitest" => vitest_args_are_explicitly_one_shot(&args[offset + 1..]),
        "exec" | "dlx" => detect_wrapped_vitest(&args[offset + 1..], PNPM_VALUE_FLAGS),
        _ => false,
    }
}

fn detect_npm_vitest(args: &[String]) -> bool {
    let offset = skip_wrapper_flags(args, NPM_VALUE_FLAGS);
    let Some(command) = args.get(offset).map(String::as_str) else {
        return false;
    };
    match command {
        "exec" | "x" => detect_wrapped_vitest(&args[offset + 1..], NPM_VALUE_FLAGS),
        _ => false,
    }
}

fn detect_yarn_vitest(args: &[String]) -> bool {
    let offset = skip_wrapper_flags(args, YARN_VALUE_FLAGS);
    let Some(command) = args.get(offset).map(String::as_str) else {
        return false;
    };
    match command {
        "vitest" => vitest_args_are_explicitly_one_shot(&args[offset + 1..]),
        "exec" => detect_wrapped_vitest(&args[offset + 1..], YARN_VALUE_FLAGS),
        _ => false,
    }
}

fn detect_wrapped_vitest(args: &[String], value_flags: &[&str]) -> bool {
    let offset = skip_wrapper_flags(args, value_flags);
    let Some(command) = args.get(offset) else {
        return false;
    };
    Path::new(command).file_name().and_then(|n| n.to_str()) == Some("vitest")
        && vitest_args_are_explicitly_one_shot(&args[offset + 1..])
}

fn vitest_args_are_explicitly_one_shot(args: &[String]) -> bool {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "watch" | "dev" | "--run=false" | "--no-run"))
    {
        return false;
    }
    args.first().is_some_and(|arg| arg == "run")
        || args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "--run" | "--run=true" | "--watch=false" | "--no-watch"
            )
        })
}

fn detect_jest(argv: &[String]) -> bool {
    if boolean_flag_enabled(argv, "--watch", None)
        || boolean_flag_enabled(argv, "--watchAll", None)
        || boolean_flag_enabled(argv, "--watch-all", None)
    {
        return false;
    }
    let Some(first) = argv.first() else {
        return false;
    };
    let Some(basename) = Path::new(first).file_name().and_then(|n| n.to_str()) else {
        return false;
    };

    if basename == "jest" {
        return true;
    }
    if basename == "npx" {
        return detect_wrapped_jest(&argv[1..], NPM_VALUE_FLAGS);
    }
    if basename == "npm" {
        return detect_npm_jest(&argv[1..]);
    }
    if basename == "pnpm" {
        return detect_pnpm_jest(&argv[1..]);
    }
    if basename == "yarn" {
        return detect_yarn_jest(&argv[1..]);
    }
    if basename == "bunx" {
        return detect_wrapped_jest(&argv[1..], BUN_VALUE_FLAGS);
    }
    if basename == "bun" {
        let offset = skip_wrapper_flags(&argv[1..], BUN_VALUE_FLAGS);
        let i = 1 + offset;
        if argv.get(i).map(String::as_str) != Some("x") {
            return false;
        }
        return detect_wrapped_jest(&argv[i + 1..], BUN_VALUE_FLAGS);
    }
    false
}

fn detect_pnpm_jest(args: &[String]) -> bool {
    let offset = skip_wrapper_flags(args, PNPM_VALUE_FLAGS);
    let Some(command) = args.get(offset).map(String::as_str) else {
        return false;
    };
    match command {
        "jest" => true,
        "exec" | "dlx" => detect_wrapped_jest(&args[offset + 1..], PNPM_VALUE_FLAGS),
        _ => false,
    }
}

fn detect_npm_jest(args: &[String]) -> bool {
    let offset = skip_wrapper_flags(args, NPM_VALUE_FLAGS);
    let Some(command) = args.get(offset).map(String::as_str) else {
        return false;
    };
    match command {
        "exec" | "x" => detect_wrapped_jest(&args[offset + 1..], NPM_VALUE_FLAGS),
        _ => false,
    }
}

fn detect_yarn_jest(args: &[String]) -> bool {
    let offset = skip_wrapper_flags(args, YARN_VALUE_FLAGS);
    let Some(command) = args.get(offset).map(String::as_str) else {
        return false;
    };
    match command {
        "jest" => true,
        "exec" => detect_wrapped_jest(&args[offset + 1..], YARN_VALUE_FLAGS),
        _ => false,
    }
}

fn detect_wrapped_jest(args: &[String], value_flags: &[&str]) -> bool {
    let offset = skip_wrapper_flags(args, value_flags);
    let Some(command) = args.get(offset) else {
        return false;
    };
    let Some(basename) = Path::new(command).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    basename == "jest" || basename.starts_with("jest@")
}

fn detect_cargo_test(argv: &[String]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    let Some(basename) = Path::new(first).file_name().and_then(|n| n.to_str()) else {
        return false;
    };

    if basename == "rustup" {
        if argv.get(1).map(String::as_str) != Some("run") || argv.len() < 4 {
            return false;
        }
        return detect_cargo_test(&argv[3..]);
    }

    if basename != "cargo" {
        return false;
    }

    let mut i = 1;
    if argv.get(i).is_some_and(|arg| arg.starts_with('+')) {
        i += 1;
    }
    while i < argv.len() {
        let arg = argv[i].as_str();
        if arg == "test" || arg == "t" {
            return true;
        }
        if arg == "--" {
            return false;
        }
        if !arg.starts_with('-') {
            return false;
        }
        if arg.contains('=') {
            i += 1;
        } else if CARGO_VALUE_FLAGS.contains(&arg) {
            i += 2;
        } else {
            i += 1;
        }
    }
    false
}

fn detect_tsc(argv: &[String]) -> bool {
    tsc_command_args(argv).is_some_and(|args| {
        !args
            .iter()
            .any(|arg| arg == "--watch" || arg == "-w" || arg.starts_with('@'))
    })
}

pub(crate) fn tsc_command_args(argv: &[String]) -> Option<&[String]> {
    let first = argv.first()?;
    let basename = Path::new(first).file_name().and_then(|n| n.to_str())?;

    if basename == "tsc" {
        return Some(&argv[1..]);
    }
    if basename == "npx" {
        return wrapped_tsc_args(&argv[1..], NPM_VALUE_FLAGS);
    }
    if basename == "npm" {
        let args = &argv[1..];
        let offset = skip_wrapper_flags(args, NPM_VALUE_FLAGS);
        return matches!(args.get(offset).map(String::as_str), Some("exec" | "x"))
            .then(|| wrapped_tsc_args(&args[offset + 1..], NPM_VALUE_FLAGS))
            .flatten();
    }
    if basename == "pnpm" {
        let args = &argv[1..];
        let offset = skip_wrapper_flags(args, PNPM_VALUE_FLAGS);
        return match args.get(offset).map(String::as_str) {
            Some("tsc") => Some(&args[offset + 1..]),
            Some("exec" | "dlx") => wrapped_tsc_args(&args[offset + 1..], PNPM_VALUE_FLAGS),
            _ => None,
        };
    }
    if basename == "yarn" {
        let args = &argv[1..];
        let offset = skip_wrapper_flags(args, YARN_VALUE_FLAGS);
        return match args.get(offset).map(String::as_str) {
            Some("tsc") => Some(&args[offset + 1..]),
            Some("exec") => wrapped_tsc_args(&args[offset + 1..], YARN_VALUE_FLAGS),
            _ => None,
        };
    }
    if basename == "bunx" {
        return wrapped_tsc_args(&argv[1..], BUN_VALUE_FLAGS);
    }
    if basename == "bun" {
        let offset = skip_wrapper_flags(&argv[1..], BUN_VALUE_FLAGS);
        let i = 1 + offset;
        if argv.get(i).map(String::as_str) != Some("x") {
            return None;
        }
        return wrapped_tsc_args(&argv[i + 1..], BUN_VALUE_FLAGS);
    }
    None
}

fn detect_go_test(argv: &[String]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    let Some(basename) = Path::new(first).file_name().and_then(|n| n.to_str()) else {
        return false;
    };

    if basename == "env" {
        let offset = skip_env_assignments(&argv[1..]);
        return detect_go_test(&argv[1 + offset..]);
    }

    if !is_go_binary_basename(basename) {
        return false;
    }

    argv.get(1).map(String::as_str) == Some("test")
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

fn skip_env_assignments(args: &[String]) -> usize {
    args.iter()
        .take_while(|arg| arg.contains('=') && !arg.starts_with('-'))
        .count()
}

fn wrapped_tsc_args<'a>(args: &'a [String], value_flags: &[&str]) -> Option<&'a [String]> {
    let offset = skip_wrapper_flags(args, value_flags);
    let command = args.get(offset)?;
    (Path::new(command).file_name().and_then(|n| n.to_str()) == Some("tsc"))
        .then(|| &args[offset + 1..])
}

const NPM_VALUE_FLAGS: &[&str] = &[
    "--package",
    "-p",
    "--cache",
    "--userconfig",
    "--registry",
    "--prefix",
];

const PNPM_VALUE_FLAGS: &[&str] = &[
    "--package",
    "-C",
    "--dir",
    "--workspace-root",
    "--store-dir",
    "--cache-dir",
    "--config",
    "--filter",
    "-F",
];

const YARN_VALUE_FLAGS: &[&str] = &[
    "--cwd",
    "--cache-folder",
    "--global-folder",
    "--modules-folder",
    "--use-yarnrc",
];

const BUN_VALUE_FLAGS: &[&str] = &["--package", "-p", "--cwd"];

const CARGO_VALUE_FLAGS: &[&str] = &["--config", "-Z", "--color", "-C"];

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
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

fn abs_path(path: PathBuf) -> Result<PathBuf, CliError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        let cwd = std::env::current_dir().map_err(|source| CliError::Io {
            path: PathBuf::from("."),
            source,
        })?;
        Ok(cwd.join(path))
    }
}

fn millis_u64(value: u128) -> u64 {
    value.try_into().unwrap_or(u64::MAX)
}

fn normalize_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy().nfc().collect()
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|signal| format!("SIG{signal}"))
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

fn path_case_folding(cwd: &Path) -> PathCaseFolding {
    if let Some(detected) = probe_path_case_folding(cwd) {
        return detected;
    }

    if directory_proves_case_sensitive(cwd) {
        return PathCaseFolding::CaseSensitive;
    }

    platform_default_case_folding()
}

fn probe_path_case_folding(cwd: &Path) -> Option<PathCaseFolding> {
    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce).ok()?;
    let token = u64::from_be_bytes(nonce);
    let stem = format!(".failtrace-case-probe-{}-{token:016x}", std::process::id());
    let lower = cwd.join(format!("{stem}-a"));
    let upper = cwd.join(format!("{stem}-A"));

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let file = options.open(&lower).ok()?;
    drop(file);
    let detected = if upper.exists() {
        PathCaseFolding::CaseInsensitive
    } else {
        PathCaseFolding::CaseSensitive
    };
    let _ = fs::remove_file(&lower);
    Some(detected)
}

fn directory_proves_case_sensitive(cwd: &Path) -> bool {
    let Ok(entries) = fs::read_dir(cwd) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return false;
        };
        let Some((index, character)) = name
            .char_indices()
            .find(|(_, character)| character.is_ascii_alphabetic())
        else {
            return false;
        };
        let replacement = if character.is_ascii_lowercase() {
            character.to_ascii_uppercase()
        } else {
            character.to_ascii_lowercase()
        };
        let mut alternate = name;
        alternate.replace_range(
            index..index + character.len_utf8(),
            &replacement.to_string(),
        );
        matches!(
            fs::symlink_metadata(cwd.join(alternate)),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        )
    })
}

#[cfg(target_os = "macos")]
fn platform_default_case_folding() -> PathCaseFolding {
    PathCaseFolding::CaseInsensitive
}

#[cfg(not(target_os = "macos"))]
fn platform_default_case_folding() -> PathCaseFolding {
    PathCaseFolding::CaseSensitive
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("{0}")]
    Adapter(#[from] AdapterError),
    #[error("{0}")]
    Storage(#[from] StorageError),
    #[error("adapter unavailable: {0}")]
    AdapterUnavailable(String),
    #[error("failure id not found: {0}")]
    FailureNotFound(String),
    #[error("sidecar incomplete or incompatible: {0}")]
    Sidecar(String),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("JSON error at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("internal software error: {0}")]
    Internal(String),
}

impl CliError {
    fn exit_code(&self) -> i32 {
        match self {
            Self::Adapter(AdapterError::InvalidSidecar { .. })
            | Self::FailureNotFound(_)
            | Self::Sidecar(_) => EX_DATAERR,
            Self::Adapter(AdapterError::UnrecognizedFormat { .. })
            | Self::AdapterUnavailable(_) => EX_UNAVAILABLE,
            Self::Adapter(AdapterError::InvariantViolation(_)) | Self::Internal(_) => EX_SOFTWARE,
            Self::Storage(StorageError::HomeUnavailable) => EX_CONFIG,
            Self::Storage(_) | Self::Io { .. } | Self::Json { .. } => EX_IOERR,
        }
    }
}

#[derive(Debug, Serialize)]
struct ExplanationResult {
    schema_version: String,
    document_kind: &'static str,
    failtrace_version: String,
    failure_id: String,
    run_dir: String,
    summary: String,
    why_classified: Vec<String>,
    matched_rules: Vec<MatchedRule>,
    raw_log_path: String,
    failure: crate::schema::Failure,
    raw_log_excerpts: Vec<crate::schema::Excerpt>,
    suggested_next_commands: Vec<Command>,
}

#[derive(Debug, Serialize)]
struct MatchedRule {
    rule_id: String,
    description: String,
    span: RuleSpan,
}

#[derive(Debug, Serialize)]
struct RuleSpan {
    byte_range: crate::schema::ByteRange,
}

#[derive(Debug, Serialize)]
struct AdaptersResult {
    schema_version: String,
    document_kind: &'static str,
    failtrace_version: String,
    adapters: Vec<AdapterInfo>,
}

#[derive(Debug, Serialize)]
struct AdapterInfo {
    id: &'static str,
    version: &'static str,
    kind: &'static str,
    supported_commands: Vec<&'static str>,
    minimum_failure_rule: &'static str,
    strip_params_rule: Option<&'static str>,
    capabilities: Vec<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::{
        explanation_rule, is_cargo_test_command, is_go_test_command, is_jest_command,
        is_pytest_command, is_tsc_command, is_vitest_command, is_xcodebuild_test_command,
        normalize_cwd, path_case_folding,
    };
    use crate::storage::PathCaseFolding;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cwd_is_nfc_normalized() {
        let decomposed = std::path::Path::new("/tmp/Cafe\u{301}");
        assert_eq!(normalize_cwd(decomposed), "/tmp/Caf\u{e9}");
    }

    #[test]
    fn case_folding_matches_the_cwd_filesystem() {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let root = std::env::temp_dir().join(format!(
            "failtrace-case-folding-test-{}-{:016x}",
            std::process::id(),
            u64::from_be_bytes(nonce)
        ));
        std::fs::create_dir(&root).unwrap();

        let lower = root.join("case-check-a");
        let upper = root.join("case-check-A");
        std::fs::write(&lower, b"").unwrap();
        let expected = if upper.exists() {
            PathCaseFolding::CaseInsensitive
        } else {
            PathCaseFolding::CaseSensitive
        };
        std::fs::remove_file(&lower).unwrap();

        assert_eq!(path_case_folding(&root), expected);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn explanation_rules_match_each_shipped_adapter() {
        for (adapter_id, expected_rule) in [
            ("pytest", "pytest_minimum_failure_rule"),
            ("vitest", "vitest_minimum_failure_rule"),
            ("jest", "jest_minimum_failure_rule"),
            ("cargo-test", "cargo_test_minimum_failure_rule"),
            ("tsc", "tsc_minimum_failure_rule"),
            ("gotest", "gotest_minimum_failure_rule"),
            ("xcodebuild-test", "xcodebuild_test_minimum_failure_rule"),
        ] {
            assert_eq!(explanation_rule(adapter_id).0, expected_rule);
        }
    }

    #[test]
    fn xcodebuild_detector_does_not_panic_on_trailing_value_flag() {
        // Malformed input: a value-flag at the end of argv with no value
        // following. The parser unconditionally advances by 2 past a value
        // flag, so `&argv[i..]` would panic without the slice clamp. Must
        // return false (no xcodebuild seen) cleanly instead of crashing.
        assert!(!is_xcodebuild_test_command(&argv(&["xcrun", "--sdk"])));
        assert!(!is_xcodebuild_test_command(&argv(&["xcrun", "-sdk"])));
        assert!(!is_xcodebuild_test_command(&argv(&[
            "xcrun",
            "--toolchain"
        ])));
        assert!(!is_xcodebuild_test_command(&argv(&["xcrun", "-toolchain"])));
        // Value flag followed by another flag (no value provided) — also
        // exercises the consume-past-end path.
        assert!(!is_xcodebuild_test_command(&argv(&[
            "xcrun", "--sdk", "--help",
        ])));
    }

    #[test]
    fn xcodebuild_detector_claims_multi_action_invocations() {
        // xcodebuild(1) `action ...` allows combinations. Multiple actions
        // including `test` must claim; rejecting early on build-before-test
        // would miss them.
        assert!(is_xcodebuild_test_command(&argv(&[
            "xcodebuild",
            "build",
            "test"
        ])));
        assert!(is_xcodebuild_test_command(&argv(&[
            "xcodebuild",
            "clean",
            "build",
            "test"
        ])));
        assert!(is_xcodebuild_test_command(&argv(&[
            "xcodebuild",
            "-exportArchive",
            "test",
            "-scheme",
            "Foo"
        ])));
        assert!(is_xcodebuild_test_command(&argv(&[
            "xcodebuild",
            "-derivedDataPath",
            "build",
            "test",
        ])));
    }

    #[test]
    fn xcodebuild_detector_rejects_workspace_projects_in_v0_1() {
        assert!(!is_xcodebuild_test_command(&argv(&[
            "xcodebuild",
            "test",
            "-workspace",
            "Example.xcworkspace",
            "-scheme",
            "Example",
        ])));
    }

    #[test]
    fn xcodebuild_detector_handles_xcrun_run_as_boolean() {
        // `--run`/`-r` is a boolean mode switch per man xcrun, NOT a value
        // flag.
        assert!(is_xcodebuild_test_command(&argv(&[
            "xcrun",
            "--run",
            "xcodebuild",
            "test"
        ])));
        assert!(is_xcodebuild_test_command(&argv(&[
            "xcrun",
            "-r",
            "xcodebuild",
            "test"
        ])));
        // -f/--find is reject (lookup only).
        assert!(!is_xcodebuild_test_command(&argv(&[
            "xcrun",
            "-f",
            "xcodebuild",
            "test"
        ])));
        assert!(!is_xcodebuild_test_command(&argv(&[
            "xcrun",
            "--find",
            "xcodebuild",
            "test"
        ])));
    }

    #[test]
    fn detects_direct_pytest() {
        assert!(is_pytest_command(&argv(&["pytest"])));
        assert!(is_pytest_command(&argv(&["pytest", "tests/"])));
        assert!(is_pytest_command(&argv(&["/usr/local/bin/pytest", "-x"])));
    }

    #[test]
    fn detects_cargo_test_forms() {
        assert!(is_cargo_test_command(&argv(&["cargo", "test"])));
        assert!(is_cargo_test_command(&argv(&["cargo", "t"])));
        assert!(is_cargo_test_command(&argv(&[
            "/usr/local/bin/cargo",
            "test"
        ])));
        assert!(is_cargo_test_command(&argv(&["cargo", "+stable", "test"])));
        assert!(is_cargo_test_command(&argv(&[
            "cargo",
            "--locked",
            "--offline",
            "--color",
            "never",
            "-Z",
            "unstable-options",
            "-C",
            "crates/app",
            "test",
            "--workspace",
            "--",
            "--test-threads=1",
        ])));
        assert!(is_cargo_test_command(&argv(&[
            "rustup", "run", "stable", "cargo", "test",
        ])));
        assert!(is_cargo_test_command(&argv(&[
            "cargo",
            "--config=term.color='never'",
            "-vv",
            "t",
        ])));
        assert!(is_cargo_test_command(&argv(&[
            "cargo",
            "test",
            "--message-format=json",
        ])));
    }

    #[test]
    fn rejects_out_of_scope_cargo_forms() {
        assert!(!is_cargo_test_command(&argv(&[])));
        assert!(!is_cargo_test_command(&argv(&["cargo"])));
        assert!(!is_cargo_test_command(&argv(&["cargo", "build"])));
        assert!(!is_cargo_test_command(&argv(&["cargo", "check"])));
        assert!(!is_cargo_test_command(&argv(&["cargo", "nextest", "run"])));
        assert!(!is_cargo_test_command(&argv(&["cargo-make", "test"])));
        assert!(!is_cargo_test_command(&argv(&["just", "test"])));
        assert!(!is_cargo_test_command(&argv(&["make", "test"])));
        assert!(!is_cargo_test_command(&argv(&[
            "rustup", "run", "stable", "cargo", "build",
        ])));
    }

    #[test]
    fn detects_go_test_forms() {
        assert!(is_go_test_command(&argv(&["go", "test"])));
        assert!(is_go_test_command(&argv(&["go", "test", "./..."])));
        assert!(is_go_test_command(&argv(&[
            "/opt/homebrew/bin/go",
            "test",
            "-json",
            "./...",
        ])));
        assert!(is_go_test_command(&argv(&[
            "go", "test", "-json", "-race", "./...",
        ])));
        assert!(is_go_test_command(&argv(&[
            "go", "test", "-run", "TestFoo", "-count=1",
        ])));
        assert!(is_go_test_command(&argv(&[
            "env",
            "CGO_ENABLED=0",
            "go",
            "test",
            "-json",
            "./...",
        ])));
        assert!(is_go_test_command(&argv(&[
            "env",
            "CGO_ENABLED=0",
            "GOOS=linux",
            "go",
            "test",
        ])));
        assert!(is_go_test_command(&argv(&[
            "go",
            "test",
            "-json",
            "-coverprofile=coverage.out",
            "./...",
        ])));
    }

    #[test]
    fn rejects_out_of_scope_go_forms() {
        assert!(!is_go_test_command(&argv(&[])));
        assert!(!is_go_test_command(&argv(&["go"])));
        assert!(!is_go_test_command(&argv(&["go", "build"])));
        assert!(!is_go_test_command(&argv(&["go", "vet", "./..."])));
        assert!(!is_go_test_command(&argv(&["go", "run", "main.go"])));
        assert!(!is_go_test_command(&argv(&["go", "env"])));
        assert!(!is_go_test_command(&argv(&["go", "generate", "./..."])));
        assert!(!is_go_test_command(&argv(&["go", "install", "./..."])));
        assert!(!is_go_test_command(&argv(&["gotest"])));
        assert!(!is_go_test_command(&argv(&["go-test"])));
        assert!(!is_go_test_command(&argv(&["goimports", "-w", "."])));
        assert!(!is_go_test_command(&argv(&["gofmt", "test"])));
        assert!(!is_go_test_command(&argv(&["gopher", "test"])));
        assert!(!is_go_test_command(&argv(&["gone", "test"])));
        assert!(!is_go_test_command(&argv(&["gotestsum", "./..."])));
    }

    #[test]
    fn detects_go_test_versioned_launchers() {
        assert!(is_go_test_command(&argv(&["go1.21", "test", "./..."])));
        assert!(is_go_test_command(&argv(&["go1.21.5", "test"])));
        assert!(is_go_test_command(&argv(&["go1.22rc1", "test"])));
        assert!(is_go_test_command(&argv(&["go1.22beta1", "test"])));
        assert!(is_go_test_command(&argv(&["gotip", "test", "./..."])));
        assert!(is_go_test_command(&argv(&["go.exe", "test", "./..."])));
    }

    #[test]
    fn detects_tsc_forms_and_rejects_watch_mode() {
        assert!(is_tsc_command(&argv(&["tsc"])));
        assert!(is_tsc_command(&argv(&["tsc", "--noEmit"])));
        assert!(is_tsc_command(&argv(&["tsc", "-p", "tsconfig.json"])));
        assert!(is_tsc_command(&argv(&[
            "tsc",
            "--project",
            "tsconfig.json",
        ])));
        assert!(is_tsc_command(&argv(&["tsc", "--build"])));
        assert!(is_tsc_command(&argv(&["tsc", "-b"])));
        assert!(is_tsc_command(&argv(&["npx", "tsc"])));
        assert!(is_tsc_command(&argv(&["pnpm", "tsc"])));
        assert!(is_tsc_command(&argv(&["yarn", "tsc"])));
        assert!(is_tsc_command(&argv(&["bunx", "tsc"])));
        assert!(is_tsc_command(&argv(&["bun", "x", "tsc"])));
        assert!(is_tsc_command(&argv(&["npm", "exec", "tsc"])));
        assert!(is_tsc_command(&argv(&["npm", "x", "tsc"])));
        assert!(is_tsc_command(&argv(&["pnpm", "exec", "tsc"])));
        assert!(is_tsc_command(&argv(&["pnpm", "dlx", "tsc"])));
        assert!(is_tsc_command(&argv(&[
            "npx",
            "--package",
            "typescript",
            "--package",
            "@types/node",
            "tsc",
            "--noEmit",
        ])));

        assert!(!is_tsc_command(&argv(&["tsc", "--watch"])));
        assert!(!is_tsc_command(&argv(&["tsc", "-w"])));
        assert!(!is_tsc_command(&argv(&["tsc", "--build", "--watch"])));
        assert!(!is_tsc_command(&argv(&["tsc", "--build", "-w"])));
        assert!(!is_tsc_command(&argv(&["tsc", "-b", "-w"])));
        assert!(!is_tsc_command(&argv(&["tsc", "-b", "--watch"])));
        assert!(!is_tsc_command(&argv(&["npx", "tsc", "--watch"])));
        assert!(!is_tsc_command(&argv(&["pnpm", "tsc", "-w"])));
        assert!(!is_tsc_command(&argv(&["yarn", "tsc", "--watch"])));
        assert!(!is_tsc_command(&argv(&["bunx", "tsc", "-w"])));
        assert!(!is_tsc_command(&argv(&["tsc", "@watch.args"])));
        assert!(!is_tsc_command(&argv(&["npx", "tsc", "@watch.args"])));

        assert!(!is_tsc_command(&argv(&["tsc-something"])));
        assert!(!is_tsc_command(&argv(&["tsc@latest"])));
        assert!(!is_tsc_command(&argv(&["bash", "-c", "tsc"])));
        assert!(!is_tsc_command(&argv(&["npm", "run", "typecheck"])));
        assert!(!is_tsc_command(&argv(&["tsc-helper"])));
    }

    #[test]
    fn detects_python_dash_m_pytest() {
        assert!(is_pytest_command(&argv(&["python", "-m", "pytest"])));
        assert!(is_pytest_command(&argv(&[
            "python3", "-m", "pytest", "tests/"
        ])));
        assert!(is_pytest_command(&argv(&["python3.11", "-m", "pytest"])));
        assert!(is_pytest_command(&argv(&[
            "/usr/bin/python3",
            "-m",
            "pytest"
        ])));
    }

    #[test]
    fn detects_uv_run_pytest() {
        assert!(is_pytest_command(&argv(&["uv", "run", "pytest"])));
        assert!(is_pytest_command(&argv(&["uv", "run", "pytest", "tests/"])));
        assert!(is_pytest_command(&argv(&[
            "uv",
            "run",
            "--no-sync",
            "pytest"
        ])));
        assert!(is_pytest_command(&argv(&[
            "uv", "run", "python", "-m", "pytest"
        ])));
    }

    #[test]
    fn detects_poetry_run_pytest() {
        assert!(is_pytest_command(&argv(&["poetry", "run", "pytest"])));
        assert!(is_pytest_command(&argv(&[
            "poetry", "run", "python", "-m", "pytest"
        ])));
    }

    #[test]
    fn detects_pipenv_run_pytest() {
        assert!(is_pytest_command(&argv(&["pipenv", "run", "pytest"])));
        assert!(is_pytest_command(&argv(&[
            "pipenv", "run", "--python", "3.11", "pytest"
        ])));
    }

    #[test]
    fn detects_python_with_options_before_dash_m() {
        assert!(is_pytest_command(&argv(&[
            "python", "-W", "ignore", "-m", "pytest"
        ])));
        assert!(is_pytest_command(&argv(&[
            "python3", "-X", "dev", "-m", "pytest", "tests/"
        ])));
        assert!(is_pytest_command(&argv(&[
            "python",
            "-W=ignore",
            "-m",
            "pytest"
        ])));
    }

    #[test]
    fn detects_uv_run_with_value_flags() {
        assert!(is_pytest_command(&argv(&[
            "uv", "run", "--python", "3.11", "pytest"
        ])));
        assert!(is_pytest_command(&argv(&[
            "uv", "run", "--with", "requests", "pytest"
        ])));
        assert!(is_pytest_command(&argv(&[
            "uv",
            "run",
            "--with=requests",
            "pytest"
        ])));
        assert!(is_pytest_command(&argv(&["uv", "run", "--", "pytest"])));
        assert!(is_pytest_command(&argv(&[
            "uv", "run", "--with", "requests", "--python", "3.11", "python", "-m", "pytest"
        ])));
    }

    #[test]
    fn detects_poetry_run_with_value_flags() {
        assert!(is_pytest_command(&argv(&[
            "poetry", "run", "--with", "dev", "pytest"
        ])));
    }

    #[test]
    fn detects_coverage_run_dash_m_pytest() {
        assert!(is_pytest_command(&argv(&[
            "coverage", "run", "-m", "pytest"
        ])));
        assert!(is_pytest_command(&argv(&[
            "coverage", "run", "-m", "pytest", "tests/"
        ])));
        assert!(is_pytest_command(&argv(&[
            "coverage",
            "run",
            "--source=src",
            "-m",
            "pytest"
        ])));
        assert!(is_pytest_command(&argv(&[
            "coverage", "run", "--source", "src", "-m", "pytest"
        ])));
        assert!(is_pytest_command(&argv(&[
            "coverage",
            "run",
            "--rcfile=.coveragerc",
            "-m",
            "pytest"
        ])));
    }

    #[test]
    fn rejects_uv_run_with_pytest_as_value_flag_argument() {
        // `--with pytest` makes pytest the VALUE of --with (a uv dep), not the wrapped command.
        // The actual wrapped command here is `python -c ...`, which is not pytest.
        assert!(!is_pytest_command(&argv(&[
            "uv", "run", "--with", "pytest", "python", "-c", "print(1)"
        ])));
        assert!(!is_pytest_command(&argv(&[
            "uv",
            "run",
            "--with=pytest",
            "python",
            "script.py"
        ])));
    }

    #[test]
    fn rejects_coverage_without_dash_m_pytest() {
        assert!(!is_pytest_command(&argv(&["coverage", "run"])));
        assert!(!is_pytest_command(&argv(&["coverage", "erase"])));
        assert!(!is_pytest_command(&argv(&["coverage", "run", "script.py"])));
        assert!(!is_pytest_command(&argv(&[
            "coverage", "run", "-m", "unittest"
        ])));
    }

    #[test]
    fn rejects_non_pytest_commands() {
        assert!(!is_pytest_command(&argv(&[])));
        assert!(!is_pytest_command(&argv(&["ls"])));
        assert!(!is_pytest_command(&argv(&["cargo", "test"])));
        assert!(!is_pytest_command(&argv(&["pytest-helper"])));
        assert!(!is_pytest_command(&argv(&["python", "script.py"])));
        assert!(!is_pytest_command(&argv(&["python", "-c", "print(1)"])));
        assert!(!is_pytest_command(&argv(&[
            "uv", "pip", "install", "pytest"
        ])));
        assert!(!is_pytest_command(&argv(&["poetry", "add", "pytest"])));
        assert!(!is_pytest_command(&argv(&["uv", "run"])));
    }

    #[test]
    fn rejects_pytest_interactive_loop_and_response_file_modes() {
        for args in [
            vec!["pytest", "-f"],
            vec!["pytest", "-sx"],
            vec!["pytest", "-sv"],
            vec!["pytest", "-fx"],
            vec!["pytest", "-fs"],
        ] {
            assert!(!is_pytest_command(&argv(&args)));
        }
        assert!(!is_pytest_command(&argv(&[
            "python",
            "-m",
            "pytest",
            "--looponfail",
        ])));
        assert!(!is_pytest_command(&argv(&[
            "uv",
            "run",
            "pytest",
            "--looponfail=true",
        ])));
        for args in [
            vec!["pytest", "--pdb"],
            vec!["pytest", "--trace"],
            vec!["pytest", "-s"],
            vec!["pytest", "--capture=no"],
            vec!["pytest", "--capture", "no"],
            vec!["pytest", "@interactive.args"],
        ] {
            assert!(!is_pytest_command(&argv(&args)));
        }

        for args in [
            vec!["python", "-s", "-m", "pytest"],
            vec!["pytest", "-ks"],
            vec!["pytest", "-ms"],
            vec!["pytest", "-rs"],
            vec!["pytest", "-osomething"],
            vec!["pytest", "-n2"],
        ] {
            assert!(is_pytest_command(&argv(&args)));
        }
    }

    #[test]
    fn detects_direct_vitest() {
        assert!(!is_vitest_command(&argv(&["vitest"])));
        assert!(!is_vitest_command(&argv(&["vitest", "watch"])));
        assert!(!is_vitest_command(&argv(&["vitest", "dev"])));
        assert!(is_vitest_command(&argv(&["vitest", "run"])));
        assert!(is_vitest_command(&argv(&["vitest", "--run"])));
        assert!(is_vitest_command(&argv(&["vitest", "--no-watch"])));
        assert!(is_vitest_command(&argv(&["/usr/local/bin/vitest", "run"])));
    }

    #[test]
    fn detects_js_wrapper_vitest() {
        assert!(is_vitest_command(&argv(&["npx", "vitest", "run"])));
        assert!(is_vitest_command(&argv(&["npm", "exec", "vitest", "run"])));
        assert!(is_vitest_command(&argv(&["npm", "x", "vitest", "run"])));
        assert!(is_vitest_command(&argv(&[
            "npm", "exec", "--", "vitest", "run"
        ])));
        assert!(is_vitest_command(&argv(&["pnpm", "vitest", "run"])));
        assert!(is_vitest_command(&argv(&["pnpm", "exec", "vitest", "run"])));
        assert!(is_vitest_command(&argv(&["pnpm", "dlx", "vitest", "run"])));
        assert!(is_vitest_command(&argv(&["yarn", "vitest", "run"])));
        assert!(is_vitest_command(&argv(&["yarn", "exec", "vitest", "run"])));
        assert!(is_vitest_command(&argv(&["bunx", "vitest", "run"])));
        assert!(is_vitest_command(&argv(&["bun", "x", "vitest", "run"])));
    }

    #[test]
    fn detects_js_wrapper_vitest_after_value_flags() {
        assert!(is_vitest_command(&argv(&[
            "npx",
            "--cache",
            ".npm-cache",
            "vitest",
            "run"
        ])));
        assert!(is_vitest_command(&argv(&[
            "npm", "--prefix", "web", "exec", "vitest", "run"
        ])));
        assert!(is_vitest_command(&argv(&[
            "npm",
            "exec",
            "--package",
            "vitest",
            "vitest",
            "run"
        ])));
        assert!(is_vitest_command(&argv(&[
            "pnpm", "--dir", "web", "exec", "vitest", "run"
        ])));
        assert!(is_vitest_command(&argv(&[
            "yarn", "--cwd", "web", "exec", "vitest", "run"
        ])));
        assert!(is_vitest_command(&argv(&[
            "bunx",
            "--package",
            "vitest",
            "vitest",
            "run"
        ])));
    }

    #[test]
    fn rejects_script_name_and_package_only_vitest() {
        assert!(!is_vitest_command(&argv(&[
            "npx",
            "--package",
            "vitest",
            "node"
        ])));
        assert!(!is_vitest_command(&argv(&[
            "pnpm",
            "--package",
            "vitest",
            "node"
        ])));
        assert!(!is_vitest_command(&argv(&["npm", "test"])));
        assert!(!is_vitest_command(&argv(&["npm", "run", "vitest"])));
        assert!(!is_vitest_command(&argv(&["pnpm", "test"])));
        assert!(!is_vitest_command(&argv(&["yarn", "test"])));
        assert!(!is_vitest_command(&argv(&["bun", "test"])));
    }

    #[test]
    fn rejects_vitest_watch_and_ui_modes() {
        assert!(!is_vitest_command(&argv(&["vitest", "--watch"])));
        assert!(!is_vitest_command(&argv(&["vitest", "-w"])));
        assert!(!is_vitest_command(&argv(&["npx", "vitest", "--ui"])));
        assert!(!is_vitest_command(&argv(&[
            "vitest",
            "run",
            "--standalone",
        ])));
        assert!(!is_vitest_command(&argv(
            &["vitest", "run", "--run=false",]
        )));
        assert!(!is_vitest_command(&argv(&["vitest", "run", "--no-run",])));
        assert!(!is_vitest_command(&argv(&[
            "pnpm",
            "exec",
            "vitest",
            "--watch=true",
        ])));
        assert!(is_vitest_command(&argv(&[
            "vitest",
            "run",
            "--watch=false"
        ])));
        assert!(is_vitest_command(&argv(&["vitest", "run", "--ui=false"])));
        assert!(is_vitest_command(&argv(&[
            "vitest",
            "run",
            "--standalone=false",
        ])));
    }

    #[test]
    fn detects_direct_jest() {
        assert!(is_jest_command(&argv(&["jest"])));
        assert!(is_jest_command(&argv(&["jest", "--ci"])));
        assert!(is_jest_command(&argv(&["/usr/local/bin/jest", "--ci"])));
    }

    #[test]
    fn detects_js_wrapper_jest() {
        assert!(is_jest_command(&argv(&["npx", "jest"])));
        assert!(is_jest_command(&argv(&["npm", "exec", "jest"])));
        assert!(is_jest_command(&argv(&["npm", "x", "jest"])));
        assert!(is_jest_command(&argv(&["npm", "exec", "--", "jest"])));
        assert!(is_jest_command(&argv(&["pnpm", "jest"])));
        assert!(is_jest_command(&argv(&["pnpm", "exec", "jest"])));
        assert!(is_jest_command(&argv(&["pnpm", "dlx", "jest"])));
        assert!(is_jest_command(&argv(&["yarn", "jest"])));
        assert!(is_jest_command(&argv(&["yarn", "exec", "jest"])));
        assert!(is_jest_command(&argv(&["bunx", "jest"])));
        assert!(is_jest_command(&argv(&["bun", "x", "jest"])));
    }

    #[test]
    fn detects_js_wrapper_jest_after_value_flags() {
        assert!(is_jest_command(&argv(&[
            "npx",
            "--cache",
            ".npm-cache",
            "jest"
        ])));
        assert!(is_jest_command(&argv(&[
            "npm", "--prefix", "web", "exec", "jest"
        ])));
        assert!(is_jest_command(&argv(&[
            "npm",
            "exec",
            "--package",
            "jest",
            "jest"
        ])));
        assert!(is_jest_command(&argv(&[
            "pnpm", "--dir", "web", "exec", "jest"
        ])));
        assert!(is_jest_command(&argv(&[
            "yarn", "--cwd", "web", "exec", "jest"
        ])));
        assert!(is_jest_command(&argv(&[
            "bunx",
            "--package",
            "jest",
            "jest"
        ])));
    }

    #[test]
    fn rejects_script_name_and_package_only_jest() {
        assert!(!is_jest_command(&argv(&[
            "npx",
            "--package",
            "jest",
            "node"
        ])));
        assert!(!is_jest_command(&argv(&[
            "pnpm",
            "--package",
            "jest",
            "node"
        ])));
        assert!(!is_jest_command(&argv(&["npm", "test"])));
        assert!(!is_jest_command(&argv(&["npm", "run", "jest"])));
        assert!(!is_jest_command(&argv(&["pnpm", "test"])));
        assert!(!is_jest_command(&argv(&["yarn", "test"])));
        assert!(!is_jest_command(&argv(&["bun", "test"])));
        assert!(!is_jest_command(&argv(&["react-scripts", "test"])));
        assert!(!is_jest_command(&argv(&["next", "test"])));
    }

    #[test]
    fn rejects_jest_watch_modes() {
        assert!(!is_jest_command(&argv(&["jest", "--watch"])));
        assert!(!is_jest_command(&argv(&["npx", "jest", "--watchAll"])));
        assert!(!is_jest_command(&argv(&["jest", "--watch-all"])));
        assert!(!is_jest_command(&argv(&[
            "pnpm",
            "exec",
            "jest",
            "--watchAll=true",
        ])));
        assert!(is_jest_command(&argv(&["jest", "--watch=false"])));
        assert!(is_jest_command(&argv(&["jest", "--watchAll=false"])));
        assert!(is_jest_command(&argv(&["jest", "--watch-all=false"])));
        assert!(is_jest_command(&argv(&["jest", "-w", "2"])));
    }

    #[test]
    fn detects_versioned_jest_package_spec() {
        assert!(is_jest_command(&argv(&["npx", "jest@30.4"])));
        assert!(is_jest_command(&argv(&["npx", "jest@latest"])));
        assert!(is_jest_command(&argv(&["npm", "exec", "jest@30.4"])));
        assert!(is_jest_command(&argv(&["pnpm", "dlx", "jest@29"])));
        assert!(is_jest_command(&argv(&["yarn", "exec", "jest@30.4.1"])));
        assert!(is_jest_command(&argv(&["bunx", "jest@30"])));
    }

    #[test]
    fn detects_bunx_jest_with_bun_runtime_flag() {
        assert!(is_jest_command(&argv(&["bunx", "--bun", "jest"])));
        assert!(is_jest_command(&argv(&["bun", "x", "--bun", "jest"])));
    }

    #[test]
    fn unrecognized_document_emits_current_schema_version_when_replaying_legacy_sidecar() {
        use super::{ParseStatus, unrecognized_document};
        use crate::schema::{Adapter, AdapterKind, SelectionReason};
        use crate::storage::{PathCaseFolding, Sidecar, StreamMerge};

        // A legacy v1.4 sidecar — analyze accepts it under
        // is_compatible_sidecar_schema. The emitted document is shaped per
        // the current binary (v1.6 fields), so it must declare v1.6 to stay
        // schema_version-consistent. Same property applies to the shared
        // build_analysis_document path; covered separately in shared.rs.
        let legacy_sidecar = Sidecar {
            meta_schema_version: "1.1".to_owned(),
            schema_version: "1.4".to_owned(),
            failtrace_version: "0.1.0".to_owned(),
            capture_build: None,
            adapters: vec![Adapter {
                id: "pytest".to_owned(),
                version: "pytest@0.1.0".to_owned(),
                kind: AdapterKind::Runner,
                selected: true,
                selection_reason: SelectionReason::ArgvNameMatch,
            }],
            adapter_candidates: vec![],
            argv: vec!["pytest".to_owned()],
            command: "pytest".to_owned(),
            cwd: String::new(),
            started_at: String::new(),
            duration_ms: 0,
            exit_code: Some(0),
            signal: None,
            early_exit: None,
            truncated: false,
            truncation_reason: None,
            path_case_folding: PathCaseFolding::CaseSensitive,
            stream_merge: StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
        };

        let document =
            unrecognized_document(b"garbage\n", &legacy_sidecar, ParseStatus::UnparsedOutput);
        assert_eq!(
            document.schema_version, "1.6",
            "analyze of a v1.4 sidecar must emit a v1.6-labeled document because output shape is v1.6",
        );
        assert_eq!(
            document.determinism_warnings[0].code,
            crate::schema::DeterminismWarningCode::SchemaVersionDriftMinor
        );
    }

    #[test]
    fn multi_candidate_composite_defers_runtime_attribution() {
        use super::{
            Adapter, AdapterKind, CandidateSource, SelectionReason, analyze_log_or_unrecognized,
        };
        use crate::schema::{ParseStatus, ParseWarning, validate};
        use crate::storage::{PathCaseFolding, Sidecar, StreamMerge};

        let candidates = vec![
            Adapter {
                id: "tsc".to_owned(),
                version: "tsc@0.1.0".to_owned(),
                kind: AdapterKind::Typechecker,
                selected: false,
                selection_reason: SelectionReason::NotSelected,
            },
            Adapter {
                id: "jest".to_owned(),
                version: "jest@0.1.0".to_owned(),
                kind: AdapterKind::Runner,
                selected: false,
                selection_reason: SelectionReason::NotSelected,
            },
        ];
        let sidecar = Sidecar {
            meta_schema_version: "1.2".to_owned(),
            schema_version: "1.6".to_owned(),
            failtrace_version: "0.1.0".to_owned(),
            capture_build: None,
            adapters: candidates.clone(),
            adapter_candidates: candidates,
            argv: vec!["bash".into(), "-c".into(), "tsc && jest".into()],
            command: "bash -c \"tsc && jest\"".to_owned(),
            cwd: String::new(),
            started_at: "2026-05-16T12:00:00Z".to_owned(),
            duration_ms: 10,
            exit_code: Some(1),
            signal: None,
            early_exit: None,
            truncated: false,
            truncation_reason: None,
            path_case_folding: PathCaseFolding::CaseSensitive,
            stream_merge: StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
        };

        let document = analyze_log_or_unrecognized(
            b"test failure\n",
            &sidecar,
            None,
            CandidateSource::AmbiguousComposite,
        )
        .unwrap();

        assert_eq!(validate(&document), Vec::new());
        assert_eq!(document.parse_status, ParseStatus::UnsupportedCommand);
        assert_eq!(
            document.parse_warnings,
            vec![ParseWarning::CompositeRuntimeDeferred]
        );
        assert!(document.failures.is_empty());
        assert!(document.adapters.iter().all(|adapter| !adapter.selected));
        assert!(
            document
                .adapters
                .iter()
                .all(|adapter| adapter.selection_reason == SelectionReason::NotSelected)
        );
    }

    #[test]
    fn bash_c_single_recognized_segment_remains_outside_v0_1() {
        use super::{CandidateSource, adapter_candidates_for_command};

        let cwd = std::env::current_dir().unwrap();
        let plan = adapter_candidates_for_command(&argv(&["bash", "-c", "pytest"]), &cwd);

        assert_eq!(plan.source, CandidateSource::Direct);
        assert!(plan.candidates.is_empty());
    }

    #[test]
    fn bash_c_recognized_plus_unrecognized_defers_with_composite_runtime_warning() {
        use super::{CandidateSource, SelectionReason, adapter_candidates_for_command};
        use crate::schema::{ParseStatus, ParseWarning, validate};
        use crate::storage::{PathCaseFolding, Sidecar, StreamMerge};

        let cwd = std::env::current_dir().unwrap();
        let command = argv(&[
            "bash",
            "-c",
            "pnpm install --silent && pnpm exec vitest run",
        ]);
        let plan = adapter_candidates_for_command(&command, &cwd);

        assert_eq!(plan.source, CandidateSource::AmbiguousComposite);
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].id, "vitest");
        assert!(!plan.candidates[0].selected);
        assert_eq!(
            plan.candidates[0].selection_reason,
            SelectionReason::NotSelected
        );

        let sidecar = Sidecar {
            meta_schema_version: "1.2".to_owned(),
            schema_version: "1.6".to_owned(),
            failtrace_version: "0.1.0".to_owned(),
            capture_build: None,
            adapters: plan.candidates.clone(),
            adapter_candidates: plan.candidates,
            argv: command,
            command: "bash -c \"pnpm install --silent && pnpm exec vitest run\"".to_owned(),
            cwd: String::new(),
            started_at: "2026-05-16T12:00:00Z".to_owned(),
            duration_ms: 10,
            exit_code: Some(1),
            signal: None,
            early_exit: None,
            truncated: false,
            truncation_reason: None,
            path_case_folding: PathCaseFolding::CaseSensitive,
            stream_merge: StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
        };

        let document = super::analyze_log_or_unrecognized(
            b"",
            &sidecar,
            None,
            CandidateSource::AmbiguousComposite,
        )
        .unwrap();

        assert_eq!(validate(&document), Vec::new());
        assert_eq!(document.parse_status, ParseStatus::UnsupportedCommand);
        assert_eq!(
            document.parse_warnings,
            vec![ParseWarning::CompositeRuntimeDeferred]
        );
        assert!(document.adapters.iter().all(|adapter| !adapter.selected));
        assert!(
            document
                .adapters
                .iter()
                .all(|adapter| adapter.selection_reason == SelectionReason::NotSelected)
        );
    }

    #[test]
    fn bash_c_no_recognized_segments_still_returns_adapter_unavailable() {
        use super::adapter_candidates_for_command;

        let cwd = std::env::current_dir().unwrap();
        let plan =
            adapter_candidates_for_command(&argv(&["bash", "-c", "echo hi && ls /tmp"]), &cwd);

        assert!(plan.candidates.is_empty());
    }

    #[test]
    fn replay_candidate_source_treats_v14_sidecar_with_all_unselected_adapters_as_ambiguous() {
        // Backward-compat anchor: legacy v1.4 sidecars do
        // not carry `adapter_candidates` (the field is new in v1.5, defaults
        // empty on deserialization). The composite-ambiguous predicate must
        // be driven by `sidecar.adapters` selection state alone — gating on
        // `adapter_candidates.is_empty()` would silently demote a replayed
        // composite sidecar to Direct and select the first adapter, which
        // contradicts the original capture's defer contract.
        use super::{
            Adapter, AdapterKind, CandidateSource, SelectionReason, replay_candidate_source,
        };
        use crate::storage::{PathCaseFolding, Sidecar, StreamMerge};

        let composite_v14_sidecar = Sidecar {
            meta_schema_version: "1.1".to_owned(),
            schema_version: "1.4".to_owned(),
            failtrace_version: "legacy-binary".to_owned(),
            capture_build: None,
            adapters: vec![
                Adapter {
                    id: "pytest".to_owned(),
                    version: "pytest@0.1.0".to_owned(),
                    kind: AdapterKind::Runner,
                    selected: false,
                    selection_reason: SelectionReason::NotSelected,
                },
                Adapter {
                    id: "jest".to_owned(),
                    version: "jest@0.1.0".to_owned(),
                    kind: AdapterKind::Runner,
                    selected: false,
                    selection_reason: SelectionReason::NotSelected,
                },
            ],
            adapter_candidates: vec![], // v1.4 default: field absent
            argv: vec![],
            command: "legacy composite".to_owned(),
            cwd: String::new(),
            started_at: "2026-05-14T12:00:00Z".to_owned(),
            duration_ms: 10,
            exit_code: Some(1),
            signal: None,
            early_exit: None,
            truncated: false,
            truncation_reason: None,
            path_case_folding: PathCaseFolding::CaseSensitive,
            stream_merge: StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
        };
        assert_eq!(
            replay_candidate_source(&composite_v14_sidecar),
            CandidateSource::AmbiguousComposite,
            "v1.4 sidecar with all-unselected adapters must replay as ambiguous"
        );

        // Direct shape: one adapter, selected. Source is Direct.
        let direct_sidecar = Sidecar {
            adapters: vec![Adapter {
                id: "pytest".to_owned(),
                version: "pytest@0.1.0".to_owned(),
                kind: AdapterKind::Runner,
                selected: true,
                selection_reason: SelectionReason::ArgvNameMatch,
            }],
            ..composite_v14_sidecar.clone()
        };
        assert_eq!(
            replay_candidate_source(&direct_sidecar),
            CandidateSource::Direct,
            "single-adapter selected sidecar replays as direct regardless of schema version"
        );
    }

    #[test]
    fn bun_test_does_not_resolve_package_test_script() {
        use super::package_test_script_segments;
        use std::{
            fs,
            time::{SystemTime, UNIX_EPOCH},
        };

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "failtrace-bun-test-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&dir).unwrap();
        fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"test":"tsc && jest"}}"#,
        )
        .unwrap();

        assert_eq!(
            package_test_script_segments(&argv(&["bun", "test"]), &dir),
            None
        );
        assert_eq!(
            package_test_script_segments(&argv(&["npm", "test"]), &dir).unwrap(),
            vec![argv(&["tsc"]), argv(&["jest"])]
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn npm_test_with_single_recognized_runner_in_scripts_test_stays_ambiguous() {
        // `npm test`, `pnpm test`, and `yarn test` invoke
        // implicit `pretest` / `posttest` lifecycle hooks before and after the
        // resolved `scripts.test`. Even if `scripts.test` expands to ONE
        // recognized runner, a failing `pretest` would land its failure in the
        // captured log and be mis-attributed to that runner's parser. So
        // package-script expansion always defers with AmbiguousComposite.
        use super::{CandidateSource, adapter_candidates_for_command};
        use std::{
            fs,
            time::{SystemTime, UNIX_EPOCH},
        };

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "failtrace-npm-single-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("package.json"), r#"{"scripts":{"test":"pytest"}}"#).unwrap();

        let plan = adapter_candidates_for_command(&argv(&["npm", "test"]), &dir);
        assert_eq!(
            plan.source,
            CandidateSource::AmbiguousComposite,
            "single-segment `scripts.test` must still defer because pretest/posttest can run before/after"
        );
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].id, "pytest");
        assert!(
            !plan.candidates[0].selected,
            "package-script candidate must be unselected to route to the defer path"
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn composite_selection_disambiguates_duplicate_adapter_ids_by_position() {
        use super::{Adapter, AdapterKind, SelectionReason, sidecar_with_selected_adapter};
        use crate::storage::{PathCaseFolding, Sidecar, StreamMerge};

        // Two candidates with the same id, as `bash -c "pytest && pytest -k slow"` produces.
        // Before this fix, sidecar_with_selected_adapter marked BOTH as selected because it
        // keyed on adapter.id equality, violating invariant 13 (exactly one selected adapter
        // whenever parse_status is complete or partial).
        let candidates = vec![
            Adapter {
                id: "pytest".to_owned(),
                version: "pytest@0.1.0".to_owned(),
                kind: AdapterKind::Runner,
                selected: false,
                selection_reason: SelectionReason::CompositeCommandComponent,
            },
            Adapter {
                id: "pytest".to_owned(),
                version: "pytest@0.1.0".to_owned(),
                kind: AdapterKind::Runner,
                selected: false,
                selection_reason: SelectionReason::CompositeCommandComponent,
            },
        ];
        let sidecar = Sidecar {
            meta_schema_version: "1.2".to_owned(),
            schema_version: "1.6".to_owned(),
            failtrace_version: "0.1.0".to_owned(),
            capture_build: None,
            adapters: candidates.clone(),
            adapter_candidates: candidates.clone(),
            argv: vec![
                "bash".into(),
                "-c".into(),
                "pytest && pytest -k slow".into(),
            ],
            command: "bash -c \"pytest && pytest -k slow\"".to_owned(),
            cwd: String::new(),
            started_at: String::new(),
            duration_ms: 0,
            exit_code: Some(0),
            signal: None,
            early_exit: None,
            truncated: false,
            truncation_reason: None,
            path_case_folding: PathCaseFolding::CaseSensitive,
            stream_merge: StreamMerge::StdoutStderrInterleaved,
            raw_log_sha256: String::new(),
        };

        for selected_index in 0..candidates.len() {
            let probe = sidecar_with_selected_adapter(
                &sidecar,
                &candidates,
                selected_index,
                SelectionReason::CompositeCommandComponent,
            );
            let selected_count = probe.adapters.iter().filter(|a| a.selected).count();
            assert_eq!(
                selected_count, 1,
                "exactly one adapter must be selected when index={selected_index}; got {selected_count}",
            );
            assert!(
                probe.adapters[selected_index].selected,
                "adapter at probed index {selected_index} must be the selected one",
            );
            assert_eq!(
                probe.adapters[selected_index].selection_reason,
                SelectionReason::CompositeCommandComponent,
            );
            for (other_index, adapter) in probe.adapters.iter().enumerate() {
                if other_index == selected_index {
                    continue;
                }
                assert!(!adapter.selected);
                assert_eq!(adapter.selection_reason, SelectionReason::NotSelected);
            }
        }
    }
}
