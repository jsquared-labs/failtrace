use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::{OffsetDateTime, format_description::FormatItem, macros::format_description};

use crate::schema::{
    Adapter, DocumentKind, EarlyExit, EmissionChannel, EmissionMode, OutputDocument,
    TruncationReason,
};

const RUN_DIR_TIMESTAMP_FORMAT: &[FormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]-[minute]-[second].[subsecond digits:3]Z");
const META_SCHEMA_VERSION: &str = "1.2";
const LOG_FILE: &str = "log";
const META_FILE: &str = "meta.json";
const OUTPUT_FILE: &str = "output.json";
const LATEST_FILE: &str = "latest";

pub type StorageResult<T> = Result<T, StorageError>;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("HOME is required when XDG_DATA_HOME and FAILTRACE_RUNS_DIR are unset")]
    HomeUnavailable,
    #[error("run directory has no parent: {0}")]
    MissingRunDirParent(PathBuf),
    #[error("could not create a unique run directory under {root} after {attempts} attempts")]
    UniqueRunDirExhausted { root: PathBuf, attempts: usize },
    #[error("failed to format run directory timestamp")]
    TimestampFormat(#[from] time::error::Format),
    #[error("random nonce generation failed: {0}")]
    Random(String),
    #[error("filesystem error at {path}: {source}")]
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStoreOptions {
    pub run_dir: Option<PathBuf>,
    pub here: bool,
    pub cwd: PathBuf,
    pub failtrace_runs_dir: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub home_dir: Option<PathBuf>,
    // When the resolution would fall through to `$HOME/.failtrace/runs` and
    // that directory can't be created (sandboxed runtimes that block writes
    // to HOME, e.g. Codex CLI workspace-write), fall back to
    // `$cwd/.failtrace/runs` so failtrace works without requiring --run-dir.
    // `from_env` enables this; tests disable it to keep precedence assertions
    // independent of filesystem state.
    pub probe_home_writability: bool,
}

impl RunStoreOptions {
    pub fn from_env(cwd: impl Into<PathBuf>, run_dir: Option<PathBuf>, here: bool) -> Self {
        Self {
            run_dir,
            here,
            cwd: cwd.into(),
            failtrace_runs_dir: env::var_os("FAILTRACE_RUNS_DIR").map(PathBuf::from),
            xdg_data_home: env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            home_dir: env::var_os("HOME").map(PathBuf::from),
            probe_home_writability: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunDirResolution {
    Exact(PathBuf),
    Generated { root: PathBuf },
}

pub fn resolve_run_dir(options: &RunStoreOptions) -> StorageResult<RunDirResolution> {
    if let Some(run_dir) = &options.run_dir {
        return Ok(RunDirResolution::Exact(absolute_path(
            run_dir,
            &options.cwd,
        )));
    }

    if options.here {
        return Ok(RunDirResolution::Generated {
            root: absolute_path(".failtrace/runs", &options.cwd),
        });
    }

    if let Some(root) = &options.failtrace_runs_dir {
        return Ok(RunDirResolution::Generated {
            root: absolute_path(root, &options.cwd),
        });
    }

    if let Some(xdg_data_home) = &options.xdg_data_home {
        return Ok(RunDirResolution::Generated {
            root: absolute_path(xdg_data_home.join("failtrace/runs"), &options.cwd),
        });
    }

    let Some(home_dir) = &options.home_dir else {
        return Err(StorageError::HomeUnavailable);
    };

    let home_root = absolute_path(home_dir.join(".failtrace/runs"), &options.cwd);

    if options.probe_home_writability && !home_root_is_writable(&home_root) {
        return Ok(RunDirResolution::Generated {
            root: absolute_path(".failtrace/runs", &options.cwd),
        });
    }

    Ok(RunDirResolution::Generated { root: home_root })
}

// Real write+delete probe — `create_dir_all` alone returns Ok for an
// existing-but-unwritable directory (it's a no-op on existing paths), so a
// later `create_run_dir` would fail without ever triggering the fallback.
// The probe creates a private temp file inside the runs root and removes it.
fn home_root_is_writable(root: &Path) -> bool {
    if fs::create_dir_all(root).is_err() {
        return false;
    }
    let Ok(nonce) = random_u32() else {
        return false;
    };
    let probe = root.join(format!(".probe-{}-{:08x}", std::process::id(), nonce));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let writable = opts.open(&probe).is_ok();
    let _ = fs::remove_file(&probe);
    writable
}

#[derive(Debug, Clone)]
pub struct RunStore {
    resolution: RunDirResolution,
}

impl RunStore {
    pub fn new(options: RunStoreOptions) -> StorageResult<Self> {
        Ok(Self {
            resolution: resolve_run_dir(&options)?,
        })
    }

    pub fn from_env(
        cwd: impl Into<PathBuf>,
        run_dir: Option<PathBuf>,
        here: bool,
    ) -> StorageResult<Self> {
        Self::new(RunStoreOptions::from_env(cwd, run_dir, here))
    }

    pub fn resolution(&self) -> &RunDirResolution {
        &self.resolution
    }

    pub fn create_run_dir(&self) -> StorageResult<RunDir> {
        match &self.resolution {
            RunDirResolution::Exact(path) => {
                create_dir_all_private(path)?;
                Ok(RunDir { path: path.clone() })
            }
            RunDirResolution::Generated { root } => {
                create_dir_all_private(root)?;
                for _ in 0..64 {
                    let path = root.join(generate_run_dir_name(OffsetDateTime::now_utc())?);
                    match fs::create_dir(&path) {
                        Ok(()) => {
                            set_dir_permissions(&path)?;
                            return Ok(RunDir { path });
                        }
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                        Err(source) => {
                            return Err(StorageError::Io { path, source });
                        }
                    }
                }

                Err(StorageError::UniqueRunDirExhausted {
                    root: root.clone(),
                    attempts: 64,
                })
            }
        }
    }

    pub fn latest_path(&self) -> StorageResult<PathBuf> {
        match &self.resolution {
            RunDirResolution::Generated { root } => Ok(root.join(LATEST_FILE)),
            RunDirResolution::Exact(path) => {
                let Some(parent) = path.parent() else {
                    return Err(StorageError::MissingRunDirParent(path.clone()));
                };
                Ok(parent.join(LATEST_FILE))
            }
        }
    }

    pub fn update_latest(&self, run_dir: &RunDir) -> StorageResult<()> {
        let latest_path = self.latest_path()?;
        match &self.resolution {
            RunDirResolution::Generated { root } => create_dir_all_private(root)?,
            RunDirResolution::Exact(_) => {
                if let Some(parent) = latest_path.parent() {
                    create_dir_all_plain(parent)?;
                }
            }
        }
        write_atomic(
            &latest_path,
            absolute_path(run_dir.path(), Path::new(""))
                .to_string_lossy()
                .as_bytes(),
        )
    }

    pub fn read_latest(&self) -> StorageResult<PathBuf> {
        let latest_path = self.latest_path()?;
        let bytes = fs::read(&latest_path).map_err(|source| StorageError::Io {
            path: latest_path.clone(),
            source,
        })?;
        Ok(PathBuf::from(String::from_utf8_lossy(&bytes).trim_end()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDir {
    path: PathBuf,
}

impl RunDir {
    pub fn from_existing(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn log_path(&self) -> PathBuf {
        self.path.join(LOG_FILE)
    }

    pub fn meta_path(&self) -> PathBuf {
        self.path.join(META_FILE)
    }

    pub fn output_path(&self) -> PathBuf {
        self.path.join(OUTPUT_FILE)
    }

    pub fn write_log(&self, bytes: &[u8]) -> StorageResult<String> {
        create_dir_all_private(&self.path)?;
        let path = self.log_path();
        write_atomic(&path, bytes)?;
        Ok(sha256_hex(bytes))
    }

    pub fn read_log(&self) -> StorageResult<Vec<u8>> {
        let path = self.log_path();
        fs::read(&path).map_err(|source| StorageError::Io { path, source })
    }

    pub fn write_sidecar(&self, sidecar: &Sidecar) -> StorageResult<()> {
        create_dir_all_private(&self.path)?;
        write_json_atomic(&self.meta_path(), sidecar)
    }

    pub fn read_sidecar(&self) -> StorageResult<Sidecar> {
        read_json(&self.meta_path())
    }

    pub fn write_output_document(&self, document: &OutputDocument) -> StorageResult<()> {
        create_dir_all_private(&self.path)?;
        write_json_atomic(&self.output_path(), document)
    }

    pub fn read_output_document(&self) -> StorageResult<OutputDocument> {
        read_json(&self.output_path())
    }

    pub fn file_output_document(
        &self,
        mut document: OutputDocument,
        raw_log_sha256: impl Into<String>,
    ) -> OutputDocument {
        document.document_kind = DocumentKind::RunResult;
        document.emission.channel = EmissionChannel::File;
        document.emission.mode = EmissionMode::Full;
        document.emission.max_failures = None;
        document.emission.failures_emitted = document.failures.len() as u64;
        document.emission.failures_total = document.failures.len() as u64;
        document.emission.full_output_path = self.output_path().to_string_lossy().into_owned();
        document.raw_log_path = self.log_path().to_string_lossy().into_owned();
        document.raw_log_sha256 = raw_log_sha256.into();
        document
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sidecar {
    pub meta_schema_version: String,
    pub schema_version: String,
    pub failtrace_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_build: Option<String>,
    pub adapters: Vec<Adapter>,
    #[serde(default)]
    pub adapter_candidates: Vec<Adapter>,
    #[serde(default)]
    pub argv: Vec<String>,
    pub command: String,
    pub cwd: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    #[serde(default)]
    pub early_exit: Option<EarlyExit>,
    pub truncated: bool,
    pub truncation_reason: Option<TruncationReason>,
    pub path_case_folding: PathCaseFolding,
    pub stream_merge: StreamMerge,
    pub raw_log_sha256: String,
}

impl Sidecar {
    pub fn from_output_document(
        document: &OutputDocument,
        path_case_folding: PathCaseFolding,
        stream_merge: StreamMerge,
    ) -> Self {
        Self {
            meta_schema_version: META_SCHEMA_VERSION.to_owned(),
            schema_version: document.schema_version.clone(),
            failtrace_version: document.failtrace_version.clone(),
            capture_build: None,
            adapters: document.adapters.clone(),
            adapter_candidates: document.adapters.clone(),
            argv: vec![],
            command: document.command.clone(),
            cwd: document.cwd.clone(),
            started_at: document.started_at.clone(),
            duration_ms: document.duration_ms,
            exit_code: document.exit_code,
            signal: document.signal.clone(),
            early_exit: document.early_exit.clone(),
            truncated: document.truncated,
            truncation_reason: document.truncation_reason,
            path_case_folding,
            stream_merge,
            raw_log_sha256: document.raw_log_sha256.clone(),
        }
    }

    pub fn apply_replay_fields_to_document(&self, mut document: OutputDocument) -> OutputDocument {
        document.adapters = self.adapters.clone();
        document.command = self.command.clone();
        document.cwd = self.cwd.clone();
        document.started_at = self.started_at.clone();
        document.duration_ms = self.duration_ms;
        document.exit_code = self.exit_code;
        document.signal = self.signal.clone();
        if self.early_exit.is_some() {
            document.early_exit = self.early_exit.clone();
        }
        document.truncated = self.truncated;
        document.truncation_reason = self.truncation_reason;
        document.raw_log_sha256 = self.raw_log_sha256.clone();
        document
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathCaseFolding {
    CaseSensitive,
    CaseInsensitive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamMerge {
    StdoutStderrInterleaved,
    StdoutThenStderr,
    StderrThenStdout,
    StdoutOnly,
    StderrOnly,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

pub fn generate_run_dir_name(now: OffsetDateTime) -> StorageResult<String> {
    Ok(format!(
        "{}-{:08x}",
        now.format(RUN_DIR_TIMESTAMP_FORMAT)?,
        random_u32()?
    ))
}

fn random_u32() -> StorageResult<u32> {
    let mut bytes = [0_u8; 4];
    getrandom::fill(&mut bytes).map_err(|error| StorageError::Random(error.to_string()))?;
    Ok(u32::from_be_bytes(bytes))
}

fn absolute_path(path: impl AsRef<Path>, cwd: &Path) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn create_dir_all_private(path: &Path) -> StorageResult<()> {
    create_dir_all_plain(path)?;
    set_dir_permissions(path)
}

fn create_dir_all_plain(path: &Path) -> StorageResult<()> {
    fs::create_dir_all(path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_json<T: DeserializeOwned>(path: &Path) -> StorageResult<T> {
    let bytes = fs::read(path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| StorageError::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> StorageResult<()> {
    let bytes = serde_json::to_vec(value).map_err(|source| StorageError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    write_atomic(path, &bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> StorageResult<()> {
    let Some(parent) = path.parent() else {
        return Err(StorageError::MissingRunDirParent(path.to_path_buf()));
    };
    create_dir_all_plain(parent)?;

    let tmp_path = parent.join(format!(
        ".{}.tmp-{}-{:08x}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("failtrace"),
        std::process::id(),
        random_u32()?
    ));

    write_new_file(&tmp_path, bytes)?;
    fs::rename(&tmp_path, path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    set_file_permissions(path)
}

fn write_new_file(path: &Path, bytes: &[u8]) -> StorageResult<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    io::Write::write_all(&mut file, bytes).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_all().map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn set_dir_permissions(path: &Path) -> StorageResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        StorageError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_dir_permissions(_path: &Path) -> StorageResult<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> StorageResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        StorageError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> StorageResult<()> {
    Ok(())
}
