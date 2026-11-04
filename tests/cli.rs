#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

#[test]
fn version_identifies_product_and_build_without_runtime_git() {
    let output = Command::new(env!("CARGO_BIN_EXE_failtrace"))
        .arg("--version")
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let version = String::from_utf8(output.stdout).unwrap();
    let pattern = regex::Regex::new(
        r"\Afailtrace 0\.1\.0 \(product=failtrace-rust revision=([0-9a-f]{40}|[0-9a-f]{64}|unknown) state=(clean|dirty|unknown)\)\n\z",
    )
    .unwrap();
    assert!(pattern.is_match(&version), "{version:?}");
}

#[test]
fn capture_build_is_optional_informational_and_never_rewritten_by_analysis() {
    let temp = tempdir();
    let runs_root = temp.path().join("runs");
    let fixture = fixture_root("basic_assertion");
    let pytest = write_fake_pytest(temp.path(), &fixture.join("run/log"), 1);
    let run = failtrace(
        temp.path(),
        &runs_root,
        temp.path(),
        [
            "run".to_owned(),
            "--".to_owned(),
            pytest.to_string_lossy().into_owned(),
        ],
    );
    assert_eq!(run.status.code(), Some(1), "{}", stderr(&run));
    let run_dir = parse_run_dir(&stderr(&run));
    let meta_path = run_dir.join("meta.json");
    let mut meta: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    let version = Command::new(env!("CARGO_BIN_EXE_failtrace"))
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(
        meta["capture_build"],
        String::from_utf8(version.stdout).unwrap().trim_end()
    );
    assert_eq!(meta["meta_schema_version"], "1.2");
    let stored_output = fs::read(run_dir.join("output.json")).unwrap();
    let document: Value = serde_json::from_slice(&stored_output).unwrap();
    assert_eq!(document["failtrace_version"], "0.1.0");
    assert_eq!(document["schema_version"], "1.6");
    assert!(document.get("capture_build").is_none());

    let mut expected_stdout = None;
    for identity in [
        meta.get("capture_build").cloned(),
        None,
        Some(Value::Null),
        Some(Value::String("another capturing build".into())),
    ] {
        meta.as_object_mut().unwrap().remove("capture_build");
        if let Some(identity) = identity {
            meta["capture_build"] = identity;
        }
        let bytes = serde_json::to_vec(&meta).unwrap();
        fs::write(&meta_path, &bytes).unwrap();
        let sidecar: failtrace::storage::Sidecar = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            sidecar.capture_build.as_deref(),
            meta["capture_build"].as_str()
        );
        let analyze = failtrace(
            temp.path(),
            &runs_root,
            temp.path(),
            [
                "analyze".to_owned(),
                run_dir.to_string_lossy().into_owned(),
                "--max-failures".to_owned(),
                "all".to_owned(),
            ],
        );
        assert_eq!(analyze.status.code(), Some(0), "{}", stderr(&analyze));
        if let Some(expected) = &expected_stdout {
            assert_eq!(&analyze.stdout, expected);
        } else {
            expected_stdout = Some(analyze.stdout);
        }
        assert_eq!(fs::read(&meta_path).unwrap(), bytes);
        assert_eq!(
            fs::read(run_dir.join("output.json")).unwrap(),
            stored_output
        );
    }
}

#[test]
fn cli_help_describes_commands_arguments_and_failure_budget() {
    let help = Command::new(env!("CARGO_BIN_EXE_failtrace"))
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(0));
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("Run a supported command, capture its output"));
    assert!(help.contains("Re-analyze an existing run directory"));

    let run_help = Command::new(env!("CARGO_BIN_EXE_failtrace"))
        .args(["run", "--help"])
        .output()
        .unwrap();
    assert_eq!(run_help.status.code(), Some(0));
    let run_help = String::from_utf8(run_help.stdout).unwrap();
    assert!(run_help.contains("--max-failures <N|all>"));
    assert!(run_help.contains("use 0 for none or 'all'"));
    assert!(run_help.contains("<COMMAND>..."));

    let explain_help = Command::new(env!("CARGO_BIN_EXE_failtrace"))
        .args(["explain", "--help"])
        .output()
        .unwrap();
    assert_eq!(explain_help.status.code(), Some(0));
    let explain_help = String::from_utf8(explain_help.stdout).unwrap();
    assert!(explain_help.contains("defaults to the latest run"));

    let invalid_budget = Command::new(env!("CARGO_BIN_EXE_failtrace"))
        .args(["run", "--max-failures", "nope", "--", "pytest"])
        .output()
        .unwrap();
    assert_eq!(invalid_budget.status.code(), Some(64));
    assert!(stderr(&invalid_budget).contains("non-negative integer or 'all'"));
}

#[test]
fn cli_vertical_slice_run_analyze_last_explain_and_adapters() {
    let temp = tempdir();
    let runs_root = temp.path().join("runs");
    let fixture = fixture_root("basic_assertion");
    let fixture_log = fs::read(fixture.join("run/log")).unwrap();
    let pytest = write_fake_pytest(temp.path(), &fixture.join("run/log"), 1);

    let run = failtrace(
        temp.path(),
        &runs_root,
        fixture.join("source"),
        ["run", "--max-failures", "all", "--"]
            .into_iter()
            .map(str::to_owned)
            .chain([pytest.to_string_lossy().into_owned()]),
    );
    assert_eq!(run.status.code(), Some(1), "stderr: {}", stderr(&run));
    assert!(
        stderr(&run).starts_with("failtrace: run-dir="),
        "{}",
        stderr(&run)
    );

    let run_json: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(run_json["document_kind"], "run_result");
    assert_eq!(run_json["emission"]["channel"], "stdout");
    assert_eq!(run_json["emission"]["mode"], "full");

    let run_dir = parse_run_dir(&stderr(&run));
    assert_eq!(fs::read(run_dir.join("log")).unwrap(), fixture_log);

    let output_bytes = fs::read(run_dir.join("output.json")).unwrap();
    let output_json: Value = serde_json::from_slice(&output_bytes).unwrap();
    assert_eq!(output_json["document_kind"], "run_result");
    assert_eq!(output_json["emission"]["channel"], "file");
    assert_eq!(output_json["emission"]["mode"], "full");
    assert_eq!(output_json["failures"].as_array().unwrap().len(), 1);

    let meta_json: Value =
        serde_json::from_slice(&fs::read(run_dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta_json["stream_merge"], "stdout_stderr_interleaved");
    assert_eq!(meta_json["adapters"][0]["id"], "pytest");
    assert_eq!(meta_json["adapters"][0]["selected"], true);

    let analyze = failtrace(
        temp.path(),
        &runs_root,
        temp.path(),
        [
            "analyze".to_owned(),
            "--max-failures".to_owned(),
            "all".to_owned(),
            run_dir.to_string_lossy().into_owned(),
        ],
    );
    assert_eq!(analyze.status.code(), Some(0), "{}", stderr(&analyze));
    let analyze_json: Value = serde_json::from_slice(&analyze.stdout).unwrap();
    assert_eq!(analyze_json["document_kind"], "analysis_result");
    assert_eq!(analyze_json["emission"]["channel"], "stdout");
    assert_json_equal_modulo_command_mode(&output_bytes, &analyze.stdout);

    let last = failtrace(temp.path(), &runs_root, temp.path(), ["last".to_owned()]);
    assert_eq!(last.status.code(), Some(0), "{}", stderr(&last));
    assert_eq!(last.stdout, output_bytes);

    let failure_id = output_json["failures"][0]["failure_id"].as_str().unwrap();
    let explain = failtrace(
        temp.path(),
        &runs_root,
        temp.path(),
        [
            "explain".to_owned(),
            "--run-dir".to_owned(),
            run_dir.to_string_lossy().into_owned(),
            failure_id.to_owned(),
        ],
    );
    assert_eq!(explain.status.code(), Some(0), "{}", stderr(&explain));
    let explain_json: Value = serde_json::from_slice(&explain.stdout).unwrap();
    assert_eq!(explain_json["document_kind"], "explanation_result");
    assert_eq!(explain_json["failure_id"], failure_id);
    assert_eq!(explain_json["failure"]["failure_id"], failure_id);
    assert!(
        !explain_json["raw_log_excerpts"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let missing = failtrace(
        temp.path(),
        &runs_root,
        temp.path(),
        [
            "explain".to_owned(),
            "--run-dir".to_owned(),
            run_dir.to_string_lossy().into_owned(),
            "0000000000000000".to_owned(),
        ],
    );
    assert_eq!(missing.status.code(), Some(66));
    assert!(stderr(&missing).contains("failure id not found"));

    let adapters = failtrace(
        temp.path(),
        &runs_root,
        temp.path(),
        ["adapters".to_owned()],
    );
    assert_eq!(adapters.status.code(), Some(0), "{}", stderr(&adapters));
    let adapters_json: Value = serde_json::from_slice(&adapters.stdout).unwrap();
    assert_eq!(adapters_json["document_kind"], "adapters_result");
    assert_eq!(adapters_json["adapters"][0]["id"], "pytest");
    assert!(
        adapters_json["adapters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|adapter| adapter["id"] == "vitest")
    );
    assert!(
        adapters_json["adapters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|adapter| adapter["id"] == "gotest" && adapter["kind"] == "runner")
    );
}

#[test]
fn explain_uses_the_selected_adapter_rule_for_tsc() {
    let temp = tempdir();
    let runs_root = temp.path().join("runs");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tsc/basic_type_mismatch");
    let tsc = write_fake_runner(temp.path(), "tsc", &fixture.join("run/log"), 2);

    let run = failtrace(
        temp.path(),
        &runs_root,
        fixture.join("source"),
        ["run", "--max-failures", "all", "--"]
            .into_iter()
            .map(str::to_owned)
            .chain([tsc.to_string_lossy().into_owned()]),
    );
    assert_eq!(run.status.code(), Some(2), "{}", stderr(&run));
    let output: Value = serde_json::from_slice(&run.stdout).unwrap();
    let failure_id = output["failures"][0]["failure_id"].as_str().unwrap();
    let run_dir = parse_run_dir(&stderr(&run));

    let explain = failtrace(
        temp.path(),
        &runs_root,
        temp.path(),
        [
            "explain".to_owned(),
            "--run-dir".to_owned(),
            run_dir.to_string_lossy().into_owned(),
            failure_id.to_owned(),
        ],
    );
    assert_eq!(explain.status.code(), Some(0), "{}", stderr(&explain));
    let explanation: Value = serde_json::from_slice(&explain.stdout).unwrap();
    assert_eq!(
        explanation["matched_rules"][0]["rule_id"],
        "tsc_minimum_failure_rule"
    );
    assert_eq!(
        explanation["why_classified"][0],
        "tsc_minimum_failure_rule:matched"
    );
}

#[test]
fn run_stdout_budget_does_not_truncate_output_json() {
    let temp = tempdir();
    let runs_root = temp.path().join("runs");
    let fixture = fixture_root("parameterized");
    let pytest = write_fake_pytest(temp.path(), &fixture.join("run/log"), 1);

    let run = failtrace(
        temp.path(),
        &runs_root,
        fixture.join("source"),
        ["run", "--max-failures", "2", "--"]
            .into_iter()
            .map(str::to_owned)
            .chain([pytest.to_string_lossy().into_owned()]),
    );
    assert_eq!(run.status.code(), Some(1), "{}", stderr(&run));
    let stdout_json: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(stdout_json["emission"]["channel"], "stdout");
    assert_eq!(stdout_json["emission"]["mode"], "budgeted");
    assert_eq!(stdout_json["emission"]["max_failures"], 2);
    assert_eq!(stdout_json["emission"]["failures_emitted"], 2);
    assert_eq!(stdout_json["emission"]["failures_total"], 3);
    assert_eq!(stdout_json["failures"].as_array().unwrap().len(), 2);

    let run_dir = parse_run_dir(&stderr(&run));
    let output_json: Value =
        serde_json::from_slice(&fs::read(run_dir.join("output.json")).unwrap()).unwrap();
    assert_eq!(output_json["emission"]["channel"], "file");
    assert_eq!(output_json["emission"]["mode"], "full");
    assert_eq!(output_json["emission"]["failures_emitted"], 3);
    assert_eq!(output_json["failures"].as_array().unwrap().len(), 3);
}

#[test]
fn analyze_old_minor_sidecars_emits_schema_drift_warning() {
    for old_schema in ["1.5", "1.4"] {
        let temp = tempdir();
        let runs_root = temp.path().join("runs");
        let fixture = fixture_root("basic_assertion");
        let pytest = write_fake_pytest(temp.path(), &fixture.join("run/log"), 1);

        let run = failtrace(
            temp.path(),
            &runs_root,
            fixture.join("source"),
            ["run", "--max-failures", "all", "--"]
                .into_iter()
                .map(str::to_owned)
                .chain([pytest.to_string_lossy().into_owned()]),
        );
        assert_eq!(run.status.code(), Some(1), "stderr: {}", stderr(&run));

        let run_dir = parse_run_dir(&stderr(&run));
        let meta_path = run_dir.join("meta.json");
        let mut meta_json: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        meta_json["schema_version"] = Value::String(old_schema.to_owned());
        fs::write(&meta_path, serde_json::to_vec_pretty(&meta_json).unwrap()).unwrap();

        let analyze = failtrace(
            temp.path(),
            &runs_root,
            temp.path(),
            [
                "analyze".to_owned(),
                "--max-failures".to_owned(),
                "all".to_owned(),
                run_dir.to_string_lossy().into_owned(),
            ],
        );
        assert_eq!(analyze.status.code(), Some(0), "{}", stderr(&analyze));
        let analyze_json: Value = serde_json::from_slice(&analyze.stdout).unwrap();
        assert_eq!(analyze_json["schema_version"], "1.6");
        assert_eq!(
            analyze_json["determinism_warnings"][0]["code"],
            "schema_version_drift_minor"
        );
        assert_eq!(
            analyze_json["determinism_warnings"][0]["adapter_id"],
            "pytest"
        );
        assert_eq!(
            analyze_json["determinism_warnings"][0]["meta_version"],
            old_schema
        );
        assert_eq!(
            analyze_json["determinism_warnings"][0]["current_version"],
            "1.6"
        );
    }
}

#[test]
fn last_prints_stored_output_json_byte_for_byte_even_for_old_schema() {
    let temp = tempdir();
    let runs_root = temp.path().join("runs");
    let fixture = fixture_root("basic_assertion");
    let pytest = write_fake_pytest(temp.path(), &fixture.join("run/log"), 1);

    let run = failtrace(
        temp.path(),
        &runs_root,
        fixture.join("source"),
        ["run", "--max-failures", "all", "--"]
            .into_iter()
            .map(str::to_owned)
            .chain([pytest.to_string_lossy().into_owned()]),
    );
    assert_eq!(run.status.code(), Some(1), "stderr: {}", stderr(&run));

    let run_dir = parse_run_dir(&stderr(&run));
    let output_path = run_dir.join("output.json");
    let mut stored: Value = serde_json::from_slice(&fs::read(&output_path).unwrap()).unwrap();
    stored["schema_version"] = Value::String("1.5".to_owned());
    let stored_bytes = serde_json::to_vec_pretty(&stored).unwrap();
    fs::write(&output_path, &stored_bytes).unwrap();

    let last = failtrace(temp.path(), &runs_root, temp.path(), ["last".to_owned()]);
    assert_eq!(last.status.code(), Some(0), "{}", stderr(&last));
    assert_eq!(last.stdout, stored_bytes);
}

#[test]
fn xcodebuild_adapter_version_capture_and_read_only_drift_replay() {
    let temp = tempdir();
    let runs_root = temp.path().join("runs");
    let log_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/xcodebuild/green_passing/run/log");
    let runner = write_fake_runner(temp.path(), "xcodebuild", &log_path, 0);
    let capture = failtrace(
        temp.path(),
        &runs_root,
        temp.path(),
        [
            "run".to_owned(),
            "--".to_owned(),
            runner.to_string_lossy().into_owned(),
            "test".to_owned(),
        ],
    );
    assert_eq!(capture.status.code(), Some(0), "{}", stderr(&capture));
    let captured: Value = serde_json::from_slice(&capture.stdout).unwrap();
    assert_eq!(captured["adapters"][0]["version"], "xcodebuild-test@0.1.1");
    assert_eq!(captured["failtrace_version"], "0.1.0");
    assert_eq!(captured["schema_version"], "1.6");
    let run_dir = parse_run_dir(&stderr(&capture));
    let meta_path = run_dir.join("meta.json");
    let mut meta: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    assert_eq!(meta["adapters"][0]["version"], "xcodebuild-test@0.1.1");
    for field in ["adapters", "adapter_candidates"] {
        for adapter in meta[field].as_array_mut().unwrap() {
            adapter["version"] = "xcodebuild-test@0.1.0".into();
        }
    }
    fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
    let original_meta = fs::read(&meta_path).unwrap();
    let original_output = fs::read(run_dir.join("output.json")).unwrap();
    let original_log = fs::read(run_dir.join("log")).unwrap();
    let replay = failtrace(
        temp.path(),
        &runs_root,
        temp.path(),
        [
            "analyze".to_owned(),
            run_dir.to_string_lossy().into_owned(),
            "--max-failures".to_owned(),
            "all".to_owned(),
        ],
    );
    assert_eq!(replay.status.code(), Some(0), "{}", stderr(&replay));
    let analyzed: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(
        analyzed["determinism_warnings"],
        serde_json::json!([{
            "code": "adapter_version_drift", "adapter_id": "xcodebuild-test",
            "meta_version": "xcodebuild-test@0.1.0", "current_version": "xcodebuild-test@0.1.1"
        }])
    );
    assert_eq!(fs::read(&meta_path).unwrap(), original_meta);
    assert_eq!(
        fs::read(run_dir.join("output.json")).unwrap(),
        original_output
    );
    assert_eq!(fs::read(run_dir.join("log")).unwrap(), original_log);
}

#[test]
fn run_captures_stdout_and_stderr_into_log() {
    let temp = tempdir();
    let runs_root = temp.path().join("runs");
    let pytest = temp.path().join("pytest");
    fs::write(
        &pytest,
        "#!/bin/sh\nprintf 'stdout one\\n'\nprintf 'stderr one\\n' >&2\nprintf 'stdout two\\n'\nprintf 'stderr two\\n' >&2\n",
    )
    .unwrap();
    fs::set_permissions(&pytest, fs::Permissions::from_mode(0o755)).unwrap();

    let run = failtrace(
        temp.path(),
        &runs_root,
        temp.path(),
        ["run", "--max-failures", "all", "--"]
            .into_iter()
            .map(str::to_owned)
            .chain([pytest.to_string_lossy().into_owned()]),
    );
    assert_eq!(run.status.code(), Some(0), "{}", stderr(&run));
    let stdout_json: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(stdout_json["parse_status"], "unparsed_output");

    let run_dir = parse_run_dir(&stderr(&run));
    assert_eq!(
        fs::read(run_dir.join("log")).unwrap(),
        b"stdout one\nstderr one\nstdout two\nstderr two\n"
    );
}

#[test]
fn run_marks_the_child_environment_as_already_wrapped_for_codex_shims() {
    let temp = tempdir();
    let runs_root = temp.path().join("runs");
    let marker = temp.path().join("shim-guard");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cargo/green_passing/run/log");
    let cargo = temp.path().join("cargo");
    fs::write(
        &cargo,
        format!(
            "#!/bin/sh\nprintf '%s' \"$FAILTRACE_CODEX_SHIM_ACTIVE\" > '{}'\ncat '{}'\n",
            marker.to_string_lossy(),
            fixture.to_string_lossy(),
        ),
    )
    .unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();

    let path = std::env::join_paths(std::iter::once(temp.path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap()
    .to_string_lossy()
    .into_owned();
    let run = failtrace_with_env(
        temp.path(),
        &runs_root,
        temp.path(),
        ["run", "--", "cargo", "test"]
            .into_iter()
            .map(str::to_owned),
        &[("PATH", path.as_str())],
    );

    assert_eq!(run.status.code(), Some(0), "{}", stderr(&run));
    assert_eq!(fs::read_to_string(marker).unwrap(), "1");
    let output: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(output["runner_outcome"], "passed");
}

#[test]
fn run_reports_adapter_unavailable_when_runner_not_on_path() {
    // Bare-name argv[0]: should report `not found in PATH`, exit 65
    // (EX_UNAVAILABLE). PATH is cleared so the test is deterministic
    // regardless of whether pytest is installed on the host.
    let temp = tempdir();
    let runs_root = temp.path().join("runs");

    let run = failtrace_with_env(
        temp.path(),
        &runs_root,
        temp.path(),
        ["run", "--", "pytest"].into_iter().map(str::to_owned),
        &[("PATH", "")],
    );

    assert_eq!(run.status.code(), Some(65), "stderr: {}", stderr(&run));
    let err = stderr(&run);
    assert!(
        err.contains("not found in PATH"),
        "expected 'not found in PATH' in stderr, got: {err}"
    );
    assert!(
        err.contains("pytest"),
        "expected binary name in stderr, got: {err}"
    );
    // The generic "I/O error at" wording must not appear in this message.
    assert!(
        !err.contains("I/O error at"),
        "should not surface the raw I/O error wording, got: {err}"
    );
}

#[test]
fn run_reports_adapter_unavailable_when_runner_path_does_not_exist() {
    // Path-with-slash argv[0]: must not blame PATH, since the user supplied a
    // literal path. Should still exit 65 with a legible error containing the
    // path and the underlying IO source.
    let temp = tempdir();
    let runs_root = temp.path().join("runs");
    let nonexistent = temp.path().join("definitely/not/here/pytest");
    let nonexistent_str = nonexistent.to_string_lossy().into_owned();

    let run = failtrace(
        temp.path(),
        &runs_root,
        temp.path(),
        ["run", "--"]
            .into_iter()
            .map(str::to_owned)
            .chain([nonexistent_str.clone()]),
    );

    assert_eq!(run.status.code(), Some(65), "stderr: {}", stderr(&run));
    let err = stderr(&run);
    assert!(
        err.contains("could not be executed"),
        "expected 'could not be executed' in stderr, got: {err}"
    );
    assert!(
        err.contains(&nonexistent_str),
        "expected path in stderr, got: {err}"
    );
    assert!(
        !err.contains("not found in PATH"),
        "must not blame PATH for a literal path, got: {err}"
    );
    assert!(
        !err.contains("I/O error at"),
        "should not surface the raw I/O error wording, got: {err}"
    );
}

fn failtrace<I>(temp: &Path, runs_root: &Path, cwd: impl AsRef<Path>, args: I) -> Output
where
    I: IntoIterator<Item = String>,
{
    failtrace_with_env(temp, runs_root, cwd, args, &[])
}

fn failtrace_with_env<I>(
    temp: &Path,
    runs_root: &Path,
    cwd: impl AsRef<Path>,
    args: I,
    env_overrides: &[(&str, &str)],
) -> Output
where
    I: IntoIterator<Item = String>,
{
    let home = temp.join("home");
    fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_failtrace"));
    command
        .args(args)
        .current_dir(cwd)
        .env("FAILTRACE_RUNS_DIR", runs_root)
        .env("HOME", home)
        .env_remove("FAILTRACE_CODEX_SHIM_ACTIVE")
        .env_remove("XDG_DATA_HOME");
    for (key, value) in env_overrides {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn fixture_root(scenario: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pytest")
        .join(scenario)
}

fn write_fake_pytest(temp: &Path, log_path: &Path, code: i32) -> PathBuf {
    write_fake_runner(temp, "pytest", log_path, code)
}

fn write_fake_runner(temp: &Path, name: &str, log_path: &Path, code: i32) -> PathBuf {
    let path = temp.join(name);
    fs::write(
        &path,
        format!(
            "#!/bin/sh\ncat '{}'\nexit {code}\n",
            log_path.to_string_lossy()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn parse_run_dir(stderr: &str) -> PathBuf {
    let line = stderr
        .lines()
        .find(|line| line.starts_with("failtrace: run-dir="))
        .unwrap();
    let after = line.strip_prefix("failtrace: run-dir=").unwrap();
    let (run_dir, _) = after.split_once(" output=").unwrap();
    PathBuf::from(run_dir)
}

fn assert_json_equal_modulo_command_mode(left: &[u8], right: &[u8]) {
    fn normalize(bytes: &[u8]) -> Value {
        let mut value: Value = serde_json::from_slice(bytes).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("document_kind");
        object.remove("failtrace_version");
        object
            .get_mut("emission")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("channel");
        value
    }

    assert_eq!(normalize(left), normalize(right));
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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

fn tempdir() -> TempDir {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    for _ in 0..100 {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!(
            "failtrace-cli-test-{}-{nanos}-{id}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return TempDir { path },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("failed to create tempdir: {error}"),
        }
    }

    panic!("could not create unique tempdir");
}
