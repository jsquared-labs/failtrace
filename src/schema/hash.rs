use sha1::{Digest, Sha1};

use super::normalize::normalize_message;
use super::types::{Failure, FailureKind, Location, Phase};

#[derive(Debug, Clone, Copy)]
pub struct FailureIdParts<'a> {
    pub adapter_id: &'a str,
    pub phase: Phase,
    pub kind: FailureKind,
    pub runner_test_id: Option<&'a str>,
    pub primary_location: &'a Location,
    pub param_id: Option<&'a str>,
    pub message: &'a str,
    pub cwd: &'a str,
}

pub fn derive_failure_id(failure: &Failure, cwd: &str) -> String {
    failure_id(FailureIdParts {
        adapter_id: &failure.adapter_id,
        phase: failure.phase,
        kind: failure.kind,
        runner_test_id: failure.runner_test_id.as_deref(),
        primary_location: &failure.primary_location,
        param_id: failure.param_id.as_deref(),
        message: &failure.message,
        cwd,
    })
}

pub fn failure_id(parts: FailureIdParts<'_>) -> String {
    let normalized = normalize_message(parts.message, parts.cwd);
    failure_id_from_normalized(
        parts.adapter_id,
        parts.phase,
        parts.kind,
        parts.runner_test_id,
        parts.primary_location,
        parts.param_id,
        &normalized,
    )
}

fn failure_id_from_normalized(
    adapter_id: &str,
    phase: Phase,
    kind: FailureKind,
    runner_test_id: Option<&str>,
    primary_location: &Location,
    param_id: Option<&str>,
    normalized_message: &str,
) -> String {
    let test_id_or_loc = match runner_test_id {
        Some(runner_test_id) => runner_test_id.to_owned(),
        None => {
            let line = primary_location
                .line
                .map_or_else(|| "?".to_owned(), |line| line.to_string());
            format!("{}:{line}", primary_location.file)
        }
    };

    short_sha1(&format!(
        "{adapter_id}|{phase}|{kind}|{test_id_or_loc}|{}|{normalized_message}",
        param_id.unwrap_or("")
    ))
}

pub fn derive_test_group_id(
    adapter_id: &str,
    runner_test_id: &str,
    strip_params: impl FnOnce(&str) -> String,
) -> String {
    test_group_id(adapter_id, &strip_params(runner_test_id))
}

pub fn test_group_id(adapter_id: &str, stripped_test_id: &str) -> String {
    short_sha1(&format!("{adapter_id}|{stripped_test_id}"))
}

pub fn strip_params_pytest(runner_test_id: &str) -> String {
    runner_test_id.find('[').map_or_else(
        || runner_test_id.to_owned(),
        |index| runner_test_id[..index].to_owned(),
    )
}

fn short_sha1(payload: &str) -> String {
    let digest = Sha1::digest(payload.as_bytes());
    format!("{digest:x}")[..16].to_owned()
}
