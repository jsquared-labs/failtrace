use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use failtrace::{schema::*, storage::*};
use serde_json::Value;

#[test]
fn run_dir_resolution_obeys_release_contract_precedence() {
    let cwd = Path::new("/workspace/project");
    let options = RunStoreOptions {
        run_dir: Some("exact-run".into()),
        here: true,
        cwd: cwd.into(),
        failtrace_runs_dir: Some("/env/runs".into()),
        xdg_data_home: Some("/xdg".into()),
        home_dir: Some("/home/user".into()),
        probe_home_writability: false,
    };
    assert_eq!(
        resolve_run_dir(&options).unwrap(),
        RunDirResolution::Exact(cwd.join("exact-run"))
    );

    let options = RunStoreOptions {
        run_dir: None,
        here: true,
        ..options
    };
    assert_eq!(
        resolve_run_dir(&options).unwrap(),
        RunDirResolution::Generated {
            root: cwd.join(".failtrace/runs")
        }
    );

    let options = RunStoreOptions {
        here: false,
        ..options
    };
    assert_eq!(
        resolve_run_dir(&options).unwrap(),
        RunDirResolution::Generated {
            root: "/env/runs".into()
        }
    );

    let options = RunStoreOptions {
        failtrace_runs_dir: None,
        ..options
    };
    assert_eq!(
        resolve_run_dir(&options).unwrap(),
        RunDirResolution::Generated {
            root: "/xdg/failtrace/runs".into()
        }
    );

    let options = RunStoreOptions {
        xdg_data_home: None,
        ..options
    };
    assert_eq!(
        resolve_run_dir(&options).unwrap(),
        RunDirResolution::Generated {
            root: "/home/user/.failtrace/runs".into()
        }
    );
}

#[test]
fn generated_run_dirs_use_schema_name_convention_and_private_permissions() {
    let temp = tempdir().unwrap();
    let store = RunStore::new(RunStoreOptions {
        run_dir: None,
        here: false,
        cwd: temp.path().into(),
        failtrace_runs_dir: Some(temp.path().join("runs")),
        xdg_data_home: None,
        home_dir: None,
        probe_home_writability: false,
    })
    .unwrap();

    let run_dir = store.create_run_dir().unwrap();
    let name = run_dir.path().file_name().unwrap().to_str().unwrap();

    assert_eq!(name.len(), "YYYY-MM-DDTHH-MM-SS.sssZ-12345678".len());
    assert_eq!(&name[4..5], "-");
    assert_eq!(&name[7..8], "-");
    assert_eq!(&name[10..11], "T");
    assert_eq!(&name[13..14], "-");
    assert_eq!(&name[16..17], "-");
    assert_eq!(&name[19..20], ".");
    assert_eq!(&name[23..25], "Z-");
    assert!(name[25..].chars().all(|ch| ch.is_ascii_hexdigit()));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(run_dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}

#[test]
fn sidecar_log_and_output_round_trip_byte_identically_modulo_document_kind_and_version() {
    let temp = tempdir().unwrap();
    let cwd = temp.path().join("repo");
    fs::create_dir(&cwd).unwrap();

    let store = RunStore::new(RunStoreOptions {
        run_dir: None,
        here: false,
        cwd: cwd.clone(),
        failtrace_runs_dir: Some(temp.path().join("runs")),
        xdg_data_home: None,
        home_dir: None,
        probe_home_writability: false,
    })
    .unwrap();
    let run_dir = store.create_run_dir().unwrap();

    let log = b"============================= test session starts =============================\nFAILED tests/test_math.py::test_add - assert 2 == 3\n";
    let raw_log_sha256 = run_dir.write_log(log).unwrap();
    let document = run_dir.file_output_document(valid_document(&cwd), raw_log_sha256.clone());
    let sidecar = Sidecar::from_output_document(
        &document,
        PathCaseFolding::CaseSensitive,
        StreamMerge::StdoutStderrInterleaved,
    );

    run_dir.write_sidecar(&sidecar).unwrap();
    run_dir.write_output_document(&document).unwrap();
    store.update_latest(&run_dir).unwrap();

    assert_eq!(run_dir.read_log().unwrap(), log);
    assert_eq!(sha256_hex(log), raw_log_sha256);
    assert_eq!(run_dir.read_sidecar().unwrap(), sidecar);
    assert_eq!(store.read_latest().unwrap(), run_dir.path());

    let stored = run_dir.read_output_document().unwrap();
    assert_eq!(stored, document);

    let replay_log = run_dir.read_log().unwrap();
    let replay_sidecar = run_dir.read_sidecar().unwrap();
    assert_eq!(sha256_hex(&replay_log), replay_sidecar.raw_log_sha256);

    let mut replayed = replay_sidecar.apply_replay_fields_to_document(stored.clone());
    replayed.document_kind = DocumentKind::AnalysisResult;
    replayed.failtrace_version = "0.1.1-test".to_owned();

    let original = serde_json::to_vec(&stored).unwrap();
    let replay = serde_json::to_vec(&replayed).unwrap();
    assert_json_bytes_equal_modulo_kind_and_version(&original, &replay);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            run_dir.log_path(),
            run_dir.meta_path(),
            run_dir.output_path(),
            store.latest_path().unwrap(),
        ] {
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}

#[test]
fn sidecar_preserves_early_exit_and_accepts_legacy_missing_field() {
    let temp = tempdir().unwrap();
    let mut document = valid_document(temp.path());
    document.early_exit = Some(EarlyExit {
        reason: EarlyExitReason::PerTestTimeout,
        killer: EarlyExitKiller::Runner,
        details: "test exceeded its configured timeout".to_owned(),
    });

    let sidecar = Sidecar::from_output_document(
        &document,
        PathCaseFolding::CaseSensitive,
        StreamMerge::StdoutStderrInterleaved,
    );
    assert_eq!(sidecar.early_exit, document.early_exit);

    let mut replay_target = valid_document(temp.path());
    replay_target.early_exit = None;
    let replayed = sidecar.apply_replay_fields_to_document(replay_target);
    assert_eq!(replayed.early_exit, document.early_exit);

    let mut legacy_json = serde_json::to_value(&sidecar).unwrap();
    legacy_json.as_object_mut().unwrap().remove("early_exit");
    let legacy_sidecar: Sidecar = serde_json::from_value(legacy_json).unwrap();
    assert_eq!(legacy_sidecar.early_exit, None);

    let mut computed = valid_document(temp.path());
    computed.early_exit = document.early_exit.clone();
    let legacy_replayed = legacy_sidecar.apply_replay_fields_to_document(computed.clone());
    assert_eq!(legacy_replayed.early_exit, computed.early_exit);

    let mut current_null_sidecar = legacy_sidecar;
    current_null_sidecar.schema_version = "1.6".to_owned();
    let current_replayed = current_null_sidecar.apply_replay_fields_to_document(computed.clone());
    assert_eq!(current_replayed.early_exit, computed.early_exit);
}

#[test]
fn home_default_falls_back_to_cwd_when_home_not_writable() {
    let temp = tempdir().unwrap();
    let cwd = temp.path().join("project");
    fs::create_dir(&cwd).unwrap();

    // Simulate an unwritable HOME by pointing `home_dir` at a regular file.
    // `fs::create_dir_all` on `<file>/.failtrace/runs` fails with NotADirectory,
    // which is the same shape an agent hits under Codex CLI workspace-write
    // (HOME exists but writes to it are denied).
    let blocker = temp.path().join("blocker-home");
    fs::write(&blocker, b"").unwrap();

    let options = RunStoreOptions {
        run_dir: None,
        here: false,
        cwd: cwd.clone(),
        failtrace_runs_dir: None,
        xdg_data_home: None,
        home_dir: Some(blocker),
        probe_home_writability: true,
    };

    assert_eq!(
        resolve_run_dir(&options).unwrap(),
        RunDirResolution::Generated {
            root: cwd.join(".failtrace/runs")
        }
    );
}

#[cfg(unix)]
#[test]
fn home_default_falls_back_when_home_root_exists_but_is_read_only() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempdir().unwrap();
    let cwd = temp.path().join("project");
    fs::create_dir(&cwd).unwrap();
    let home = temp.path().join("home");
    let runs_root = home.join(".failtrace/runs");
    fs::create_dir_all(&runs_root).unwrap();
    fs::set_permissions(&runs_root, fs::Permissions::from_mode(0o500)).unwrap();

    let options = RunStoreOptions {
        run_dir: None,
        here: false,
        cwd: cwd.clone(),
        failtrace_runs_dir: None,
        xdg_data_home: None,
        home_dir: Some(home.clone()),
        probe_home_writability: true,
    };

    let resolved = resolve_run_dir(&options).unwrap();

    // Restore permissions so TempDir drop can clean up regardless of assertion outcome.
    let _ = fs::set_permissions(&runs_root, fs::Permissions::from_mode(0o700));

    assert_eq!(
        resolved,
        RunDirResolution::Generated {
            root: cwd.join(".failtrace/runs")
        }
    );
}

#[cfg(unix)]
#[test]
fn sandbox_fallback_writes_and_reads_latest_consistently() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempdir().unwrap();
    let cwd = temp.path().join("project");
    fs::create_dir(&cwd).unwrap();
    let home = temp.path().join("home");
    let runs_root = home.join(".failtrace/runs");
    fs::create_dir_all(&runs_root).unwrap();
    fs::set_permissions(&runs_root, fs::Permissions::from_mode(0o500)).unwrap();

    let make_options = || RunStoreOptions {
        run_dir: None,
        here: false,
        cwd: cwd.clone(),
        failtrace_runs_dir: None,
        xdg_data_home: None,
        home_dir: Some(home.clone()),
        probe_home_writability: true,
    };

    let store = RunStore::new(make_options()).unwrap();
    let expected_root = cwd.join(".failtrace/runs");
    assert_eq!(
        store.resolution(),
        &RunDirResolution::Generated {
            root: expected_root.clone()
        }
    );

    let run_dir = store.create_run_dir().unwrap();
    store.update_latest(&run_dir).unwrap();

    // A separate RunStore instance must resolve to the same fallback root
    // and read the latest pointer we just wrote.
    let store2 = RunStore::new(make_options()).unwrap();
    let latest = store2.read_latest().unwrap();

    let _ = fs::set_permissions(&runs_root, fs::Permissions::from_mode(0o700));

    assert_eq!(latest, run_dir.path());
    assert!(run_dir.path().starts_with(&expected_root));
}

#[test]
fn home_default_keeps_home_root_when_probe_succeeds() {
    let temp = tempdir().unwrap();
    let cwd = temp.path().join("project");
    fs::create_dir(&cwd).unwrap();
    let writable_home = temp.path().join("writable-home");
    fs::create_dir(&writable_home).unwrap();

    let options = RunStoreOptions {
        run_dir: None,
        here: false,
        cwd: cwd.clone(),
        failtrace_runs_dir: None,
        xdg_data_home: None,
        home_dir: Some(writable_home.clone()),
        probe_home_writability: true,
    };

    assert_eq!(
        resolve_run_dir(&options).unwrap(),
        RunDirResolution::Generated {
            root: writable_home.join(".failtrace/runs")
        }
    );
}

fn assert_json_bytes_equal_modulo_kind_and_version(left: &[u8], right: &[u8]) {
    fn normalize(bytes: &[u8]) -> Vec<u8> {
        let mut value: Value = serde_json::from_slice(bytes).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("document_kind");
        object.remove("failtrace_version");
        serde_json::to_vec(&value).unwrap()
    }

    assert_eq!(normalize(left), normalize(right));
}

fn valid_document(cwd: &Path) -> OutputDocument {
    OutputDocument {
        schema_version: "1.4".to_owned(),
        document_kind: DocumentKind::RunResult,
        failtrace_version: "0.1.0".to_owned(),
        emission: Emission {
            channel: EmissionChannel::Stdout,
            mode: EmissionMode::Budgeted,
            max_failures: Some(10),
            failures_emitted: 0,
            failures_total: 0,
            full_output_path: String::new(),
        },
        command: "pytest tests/test_math.py::test_add".to_owned(),
        cwd: cwd.to_string_lossy().into_owned(),
        started_at: "2026-05-13T12:00:00Z".to_owned(),
        duration_ms: 42,
        exit_code: Some(1),
        signal: None,
        runner_outcome: RunnerOutcome::Failed,
        early_exit: None,
        adapters: vec![Adapter {
            id: "pytest".to_owned(),
            version: "pytest@0.1.0".to_owned(),
            kind: AdapterKind::Runner,
            selected: true,
            selection_reason: SelectionReason::ArgvNameMatch,
        }],
        parse_status: ParseStatus::Complete,
        parse_warnings: vec![],
        determinism_warnings: vec![],
        truncated: false,
        truncation_reason: None,
        failures_complete: true,
        failures: vec![Failure {
            failure_id: "06f0c0e485fd31cd".to_owned(),
            adapter_id: "pytest".to_owned(),
            phase: Phase::Test,
            kind: FailureKind::Assertion,
            runner_package_id: None,
            runner_test_id: Some("tests/test_math.py::test_add".to_owned()),
            test_segments: vec![],
            test_name: Some("test_add".to_owned()),
            param_id: None,
            test_group_id: None,
            primary_location: Location {
                file: "tests/test_math.py".to_owned(),
                line: Some(7),
                column: None,
                end_line: None,
                end_column: None,
                role: LocationRole::Primary,
                in_user_code: true,
            },
            extra_locations: vec![],
            message: "assert 2 == 3".to_owned(),
            diagnostic_code: None,
            diagnostic_severity: DiagnosticSeverity::Error,
            details: Details::Assertion {
                expected: Some("3".to_owned()),
                actual: Some("2".to_owned()),
                diff: None,
            },
            raw_log_lines: [1, 2],
            evidence: Some(Evidence {
                byte_range: ByteRange { start: 0, end: 128 },
                excerpts: vec![Excerpt {
                    source: ExcerptSource::RawLog,
                    kind: ExcerptKind::FailureBlock,
                    byte_range: ByteRange { start: 0, end: 128 },
                    text: "FAILED tests/test_math.py::test_add - assert 2 == 3".to_owned(),
                    ansi_stripped: false,
                    truncated: false,
                }],
            }),
            cascade_root: None,
            cascade_reason: None,
            dependent_failure_ids: vec![],
            retries: vec![],
            companion_diagnostics: vec![],
            extensions: BTreeMap::new(),
        }],
        ranking: Ranking {
            strategy: RankingStrategy::FirstEmitted,
            is_stable_across_runs: true,
        },
        counts: Counts {
            tests_run: 1,
            tests_passed: 0,
            tests_failed: 1,
            tests_errored: 0,
            tests_skipped: 0,
            tests_not_run: Some(0),
            files_failed: Some(1),
        },
        raw_log_path: String::new(),
        raw_log_sha256: String::new(),
        suggested_next_commands: vec![Command {
            argv: vec![
                "pytest".to_owned(),
                "tests/test_math.py::test_add".to_owned(),
                "-vv".to_owned(),
            ],
            command: "pytest tests/test_math.py::test_add -vv".to_owned(),
            cwd: cwd.to_string_lossy().into_owned(),
            intent: CommandIntent::RerunVerbose,
            interactive: false,
            mutates_workspace: false,
            uses_network: false,
            requires_approval: false,
        }],
        extensions: BTreeMap::new(),
    }
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn tempdir() -> Result<TempDir, std::io::Error> {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    for _ in 0..100 {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!(
            "failtrace-storage-test-{}-{nanos}-{id}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(TempDir { path }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not create unique tempdir",
    ))
}
