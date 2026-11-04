use std::{fs, path::Path};

use serde_json::Value;

const XCODEBUILD_FIXTURES: &[&str] = &[
    "build_failure",
    "green_passing",
    "linker_failure",
    "sim_launch_failure",
    "swift_semantic_build_failure",
    "swift_testing_expect_failure",
    "swift_testing_multi_expect",
];

#[test]
fn xcodebuild_test_phase_fixtures_mark_ranking_unstable() {
    // This audit reads committed fixture documents. Their document_kind value
    // is incidental to fixture origin/generation, so contract predicates must
    // not assert on document_kind.
    let mut checked = Vec::new();

    for scenario in XCODEBUILD_FIXTURES {
        let expected = read_xcodebuild_expected(scenario);
        let document = &expected["expected_subset"];
        let has_test_failure = document["failures"]
            .as_array()
            .unwrap_or_else(|| panic!("{scenario}: failures must be an array"))
            .iter()
            .any(|failure| failure["phase"] == "test");

        if has_test_failure {
            checked.push(*scenario);
            assert_eq!(
                document["ranking"]["is_stable_across_runs"], false,
                "{scenario}: any test-phase failure must make ranking unstable"
            );
        }
    }

    assert_eq!(
        checked,
        vec!["swift_testing_expect_failure", "swift_testing_multi_expect"],
        "audit must cover every committed xcodebuild test-phase failure fixture"
    );
}

#[test]
fn xcodebuild_swift_semantic_fixture_has_no_unknown_build_kinds() {
    let expected = read_xcodebuild_expected("swift_semantic_build_failure");
    let failures = expected["expected_subset"]["failures"]
        .as_array()
        .expect("swift_semantic_build_failure: failures must be an array");
    assert_eq!(failures.len(), 6);

    let type_errors = failures
        .iter()
        .filter(|failure| failure["phase"] == "build" && failure["kind"] == "type_error")
        .count();
    let syntax_errors = failures
        .iter()
        .filter(|failure| failure["phase"] == "build" && failure["kind"] == "syntax_error")
        .count();
    let unknown = failures
        .iter()
        .filter(|failure| failure["kind"] == "unknown")
        .count();

    assert_eq!(type_errors, 5);
    assert_eq!(syntax_errors, 1);
    assert_eq!(unknown, 0);
}

fn read_xcodebuild_expected(scenario: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/xcodebuild")
        .join(scenario)
        .join("expected.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
