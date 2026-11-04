use std::collections::{BTreeMap, HashSet};

use super::types::{
    Details, Diagnostic, EarlyExitReason, EmissionMode, Failure, FailureKind, OutputDocument,
    ParseStatus, Phase, RetryOutcome, RunnerOutcome,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub invariant: u8,
    pub message: String,
}

pub fn validate(document: &OutputDocument) -> Vec<Violation> {
    let mut violations = Vec::new();
    check_1(document, &mut violations);
    check_2(document, &mut violations);
    check_3(document, &mut violations);
    check_4(document, &mut violations);
    check_5(document, &mut violations);
    check_6(document, &mut violations);
    check_7(document, &mut violations);
    check_8(document, &mut violations);
    check_9(document, &mut violations);
    check_10(document, &mut violations);
    check_11(document, &mut violations);
    check_12(document, &mut violations);
    check_13(document, &mut violations);
    check_14(document, &mut violations);
    check_15(document, &mut violations);
    violations
}

fn push(violations: &mut Vec<Violation>, invariant: u8, message: impl Into<String>) {
    violations.push(Violation {
        invariant,
        message: message.into(),
    });
}

fn check_1(document: &OutputDocument, violations: &mut Vec<Violation>) {
    if !document.truncated && document.truncation_reason.is_some() {
        push(
            violations,
            1,
            "truncated is false but truncation_reason is not null",
        );
    }
}

fn check_2(document: &OutputDocument, violations: &mut Vec<Violation>) {
    if document.emission.failures_emitted != document.failures.len() as u64 {
        push(
            violations,
            2,
            "emission.failures_emitted does not equal len(failures)",
        );
    }
    if document.emission.failures_emitted > document.emission.failures_total {
        push(
            violations,
            2,
            "emission.failures_emitted exceeds emission.failures_total",
        );
    }
}

fn check_3(document: &OutputDocument, violations: &mut Vec<Violation>) {
    if document.emission.mode == EmissionMode::Full {
        if document.emission.max_failures.is_some() {
            push(
                violations,
                3,
                "emission.mode is full but emission.max_failures is not null",
            );
        }
        if document.emission.failures_emitted != document.emission.failures_total {
            push(
                violations,
                3,
                "emission.mode is full but failures_emitted differs from failures_total",
            );
        }
    }
}

fn check_4(document: &OutputDocument, violations: &mut Vec<Violation>) {
    if document.emission.mode == EmissionMode::Budgeted {
        match document.emission.max_failures {
            Some(max_failures) if document.emission.failures_emitted <= max_failures => {}
            Some(_) => push(
                violations,
                4,
                "emission.mode is budgeted but failures_emitted exceeds max_failures",
            ),
            None => push(
                violations,
                4,
                "emission.mode is budgeted but emission.max_failures is null",
            ),
        }
    }
}

fn check_5(document: &OutputDocument, violations: &mut Vec<Violation>) {
    if document.runner_outcome == RunnerOutcome::TimedOut {
        let allowed = matches!(
            document
                .early_exit
                .as_ref()
                .map(|early_exit| early_exit.reason),
            Some(EarlyExitReason::WatchdogSilence)
                | Some(EarlyExitReason::WatchdogWallclock)
                | Some(EarlyExitReason::RunnerInternalTimeout)
                | Some(EarlyExitReason::PerTestTimeout)
        );
        if !allowed {
            push(
                violations,
                5,
                "runner_outcome is timed_out but early_exit.reason is not a timeout reason",
            );
        }
    }
}

fn check_6(document: &OutputDocument, violations: &mut Vec<Violation>) {
    if document.runner_outcome == RunnerOutcome::Passed
        && (document.emission.failures_total != 0
            || document.counts.tests_failed != 0
            || document.counts.tests_errored != 0)
    {
        push(
            violations,
            6,
            "runner_outcome is passed but failures or failing/error counts are non-zero",
        );
    }
}

fn check_7(document: &OutputDocument, violations: &mut Vec<Violation>) {
    let failures = document
        .failures
        .iter()
        .map(|failure| (failure.failure_id.as_str(), failure))
        .collect::<BTreeMap<_, _>>();

    for failure in &document.failures {
        if failure.cascade_root.is_some() != failure.cascade_reason.is_some() {
            push(
                violations,
                7,
                format!(
                    "failure {} has cascade_root/cascade_reason nullness mismatch",
                    failure.failure_id
                ),
            );
        }

        if let Some(root_id) = failure.cascade_root.as_deref() {
            if root_id == failure.failure_id {
                push(
                    violations,
                    7,
                    format!(
                        "failure {} names itself as cascade_root",
                        failure.failure_id
                    ),
                );
                continue;
            }
            let Some(root) = failures.get(root_id) else {
                continue;
            };
            if root.cascade_root.is_some() {
                push(
                    violations,
                    7,
                    format!(
                        "failure {} references cascade_root {} that is itself dependent",
                        failure.failure_id, root_id
                    ),
                );
            }
            if !root
                .dependent_failure_ids
                .iter()
                .any(|id| id == &failure.failure_id)
            {
                push(
                    violations,
                    7,
                    format!(
                        "failure {} cascade_root {} does not reciprocate dependent_failure_ids",
                        failure.failure_id, root_id
                    ),
                );
            }
        }

        for dependent_id in &failure.dependent_failure_ids {
            let Some(dependent) = failures.get(dependent_id.as_str()) else {
                continue;
            };
            if dependent.cascade_root.as_deref() != Some(failure.failure_id.as_str()) {
                push(
                    violations,
                    7,
                    format!(
                        "failure {} dependent_failure_id {} does not reciprocate cascade_root",
                        failure.failure_id, dependent_id
                    ),
                );
            }
        }
    }
}

fn check_8(document: &OutputDocument, violations: &mut Vec<Violation>) {
    if matches!(
        document.parse_status,
        ParseStatus::UnparsedOutput | ParseStatus::UnsupportedCommand
    ) && !document.failures.is_empty()
    {
        push(
            violations,
            8,
            "parse_status is unparsed/unsupported but failures is not empty",
        );
    }
}

fn check_9(document: &OutputDocument, violations: &mut Vec<Violation>) {
    for failure in &document.failures {
        check_details_match(
            9,
            &failure.failure_id,
            failure.kind,
            &failure.details,
            violations,
        );
        for diagnostic in &failure.companion_diagnostics {
            check_details_match(
                9,
                &diagnostic.diagnostic_id,
                diagnostic.kind,
                &diagnostic.details,
                violations,
            );
        }
    }
}

fn check_details_match(
    invariant: u8,
    id: &str,
    kind: super::types::FailureKind,
    details: &Details,
    violations: &mut Vec<Violation>,
) {
    if details.kind() != kind {
        push(
            violations,
            invariant,
            format!("details.type does not match kind for {id}"),
        );
    }
}

fn check_10(document: &OutputDocument, violations: &mut Vec<Violation>) {
    let ids = document
        .failures
        .iter()
        .map(|failure| failure.failure_id.as_str())
        .collect::<HashSet<_>>();

    for failure in &document.failures {
        if let Some(cascade_root) = failure.cascade_root.as_deref()
            && !ids.contains(cascade_root)
        {
            push(
                violations,
                10,
                format!(
                    "failure {} references missing cascade_root {}",
                    failure.failure_id, cascade_root
                ),
            );
        }

        for dependent_id in &failure.dependent_failure_ids {
            if !ids.contains(dependent_id.as_str()) {
                push(
                    violations,
                    10,
                    format!(
                        "failure {} references missing dependent_failure_id {}",
                        failure.failure_id, dependent_id
                    ),
                );
            }
        }
    }
}

fn check_11(document: &OutputDocument, violations: &mut Vec<Violation>) {
    if document.emission.failures_emitted != document.emission.failures_total {
        return;
    }

    let mut outcomes_by_test: BTreeMap<&String, RetryOutcome> = BTreeMap::new();
    for failure in &document.failures {
        if let Some(runner_test_id) = failure.runner_test_id.as_ref() {
            let outcome = final_outcome(failure);
            if let Some(previous) = outcomes_by_test.insert(runner_test_id, outcome)
                && previous != outcome
            {
                push(
                    violations,
                    11,
                    format!(
                        "runner_test_id {} has multiple final outcomes ({previous:?} and {outcome:?})",
                        runner_test_id
                    ),
                );
            }
        }
    }

    let tests_failed = outcomes_by_test
        .values()
        .filter(|outcome| **outcome == RetryOutcome::Failed)
        .count() as u64;
    let tests_errored = outcomes_by_test
        .values()
        .filter(|outcome| **outcome == RetryOutcome::Errored)
        .count() as u64;

    if document.counts.tests_failed != tests_failed {
        push(
            violations,
            11,
            format!(
                "counts.tests_failed is {}, expected {} from failures",
                document.counts.tests_failed, tests_failed
            ),
        );
    }
    if document.counts.tests_errored != tests_errored {
        push(
            violations,
            11,
            format!(
                "counts.tests_errored is {}, expected {} from failures",
                document.counts.tests_errored, tests_errored
            ),
        );
    }
}

/// Derive a failure's final outcome per invariant 11.
///
/// Returns the `outcome` of the last `retries[]` entry when present. Otherwise
/// derives from `phase` and `kind`: `failed` only when `phase == Test` AND
/// `kind ∈ {Assertion, Snapshot}`; `errored` otherwise. `Build`, `Collect`,
/// `Setup`, and `Teardown` phases always resolve to `errored` regardless of
/// `kind` — the test infrastructure failed, not an expectation. Callers are
/// responsible for ignoring failures with `runner_test_id == null`.
pub fn final_outcome(failure: &Failure) -> RetryOutcome {
    if let Some(last) = failure.retries.last() {
        return last.outcome;
    }
    match failure.phase {
        Phase::Build | Phase::Collect | Phase::Setup | Phase::Teardown | Phase::Typecheck => {
            RetryOutcome::Errored
        }
        Phase::Test => match failure.kind {
            FailureKind::Assertion | FailureKind::Snapshot => RetryOutcome::Failed,
            _ => RetryOutcome::Errored,
        },
    }
}

fn check_12(document: &OutputDocument, violations: &mut Vec<Violation>) {
    let unique_test_ids = document
        .failures
        .iter()
        .filter_map(|failure| failure.runner_test_id.as_deref())
        .collect::<HashSet<_>>()
        .len() as u64;
    if document.counts.tests_run < unique_test_ids {
        push(
            violations,
            12,
            "tests_run is less than unique non-null runner_test_id count in failures",
        );
    }

    let classified = document.counts.tests_passed
        + document.counts.tests_failed
        + document.counts.tests_errored
        + document.counts.tests_skipped;
    if classified > document.counts.tests_run {
        push(
            violations,
            12,
            "tests_passed + tests_failed + tests_errored + tests_skipped exceeds tests_run",
        );
    }
}

fn check_13(document: &OutputDocument, violations: &mut Vec<Violation>) {
    if !matches!(
        document.parse_status,
        ParseStatus::UnparsedOutput | ParseStatus::UnsupportedCommand
    ) {
        let selected_count = document
            .adapters
            .iter()
            .filter(|adapter| adapter.selected)
            .count();
        if selected_count != 1 {
            push(
                violations,
                13,
                format!(
                    "parse_status is {:?} but selected adapter count is {selected_count}, not 1",
                    document.parse_status
                ),
            );
        }
    }
}

fn check_14(document: &OutputDocument, violations: &mut Vec<Violation>) {
    for failure in &document.failures {
        for diagnostic in &failure.companion_diagnostics {
            check_diagnostic_depth(diagnostic, violations);
        }
    }
}

fn check_15(document: &OutputDocument, violations: &mut Vec<Violation>) {
    if document.runner_outcome == RunnerOutcome::Passed
        && (document.exit_code != Some(0)
            || document.signal.is_some()
            || document.early_exit.is_some())
    {
        push(
            violations,
            15,
            "runner_outcome is passed but process exit state is not clean",
        );
    }
}

fn check_diagnostic_depth(diagnostic: &Diagnostic, violations: &mut Vec<Violation>) {
    if !diagnostic.companion_diagnostics.is_empty() {
        push(
            violations,
            14,
            format!(
                "companion diagnostic {} has nested companion_diagnostics",
                diagnostic.diagnostic_id
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::types::{DiagnosticSeverity, Location, LocationRole, Retry, RetryOutcome};

    fn skeleton_failure(phase: Phase, kind: FailureKind, retries: Vec<Retry>) -> Failure {
        Failure {
            failure_id: "test".to_owned(),
            adapter_id: "test".to_owned(),
            phase,
            kind,
            runner_package_id: None,
            runner_test_id: Some("test".to_owned()),
            test_segments: vec![],
            test_name: Some("test".to_owned()),
            param_id: None,
            test_group_id: None,
            primary_location: Location {
                file: "test".to_owned(),
                line: None,
                column: None,
                end_line: None,
                end_column: None,
                role: LocationRole::Primary,
                in_user_code: true,
            },
            extra_locations: vec![],
            message: "test".to_owned(),
            diagnostic_code: None,
            diagnostic_severity: DiagnosticSeverity::Error,
            details: Details::Unknown {},
            raw_log_lines: [1, 1],
            evidence: None,
            cascade_root: None,
            cascade_reason: None,
            dependent_failure_ids: vec![],
            retries,
            companion_diagnostics: vec![],
            extensions: Default::default(),
        }
    }

    fn retry(outcome: RetryOutcome) -> Retry {
        Retry {
            attempt: 1,
            outcome,
            duration_ms: 0,
        }
    }

    #[test]
    fn final_outcome_teardown_exception_with_empty_retries_is_errored() {
        let failure = skeleton_failure(Phase::Teardown, FailureKind::Exception, vec![]);
        assert_eq!(final_outcome(&failure), RetryOutcome::Errored);
    }

    #[test]
    fn final_outcome_test_assertion_with_empty_retries_is_failed() {
        let failure = skeleton_failure(Phase::Test, FailureKind::Assertion, vec![]);
        assert_eq!(final_outcome(&failure), RetryOutcome::Failed);
    }

    #[test]
    fn final_outcome_test_snapshot_with_empty_retries_is_failed() {
        let failure = skeleton_failure(Phase::Test, FailureKind::Snapshot, vec![]);
        assert_eq!(final_outcome(&failure), RetryOutcome::Failed);
    }

    #[test]
    fn final_outcome_test_exception_with_empty_retries_is_errored() {
        let failure = skeleton_failure(Phase::Test, FailureKind::Exception, vec![]);
        assert_eq!(final_outcome(&failure), RetryOutcome::Errored);
    }

    #[test]
    fn final_outcome_test_unknown_with_empty_retries_is_errored() {
        let failure = skeleton_failure(Phase::Test, FailureKind::Unknown, vec![]);
        assert_eq!(final_outcome(&failure), RetryOutcome::Errored);
    }

    #[test]
    fn final_outcome_setup_assertion_is_errored_because_phase_forces() {
        let failure = skeleton_failure(Phase::Setup, FailureKind::Assertion, vec![]);
        assert_eq!(final_outcome(&failure), RetryOutcome::Errored);
    }

    #[test]
    fn final_outcome_build_phase_is_errored() {
        let failure = skeleton_failure(Phase::Build, FailureKind::SyntaxError, vec![]);
        assert_eq!(final_outcome(&failure), RetryOutcome::Errored);
    }

    #[test]
    fn final_outcome_collect_phase_is_errored() {
        let failure = skeleton_failure(Phase::Collect, FailureKind::MissingSymbol, vec![]);
        assert_eq!(final_outcome(&failure), RetryOutcome::Errored);
    }

    #[test]
    fn final_outcome_with_retries_uses_last_entry_regardless_of_phase_or_kind() {
        let failure = skeleton_failure(
            Phase::Test,
            FailureKind::Assertion,
            vec![retry(RetryOutcome::Errored)],
        );
        assert_eq!(final_outcome(&failure), RetryOutcome::Errored);
    }

    #[test]
    fn final_outcome_with_multiple_retries_last_wins() {
        let failure = skeleton_failure(
            Phase::Test,
            FailureKind::Exception,
            vec![retry(RetryOutcome::Failed), retry(RetryOutcome::Errored)],
        );
        assert_eq!(final_outcome(&failure), RetryOutcome::Errored);
    }
}
