# xcodebuild test

Use on macOS for raw `xcodebuild test` and `test-without-building` actions against `.xcodeproj` projects.

- Supported forms include `xcrun xcodebuild test`, `xcodebuild clean test`, and multi-action invocations that actually include a test action.
- The stream may contain the XCTest envelope and Swift Testing's harness. Both XCTest assertions and Swift Testing `#expect` / `#require` failures are captured.
- The first XCTest suite-start or Swift Testing run-start marker begins the test phase. Earlier failures are build phase.
- Build coverage includes Swift, Objective-C/Clang, module-map, and linker diagnostics. All-build failure ordering is stable.
- Test-phase ordering is unstable because destinations and Swift Testing may execute concurrently. Preserve display names, parameterized signatures, and suite hierarchy by planning against `failure_id`.
- Destination-resolution errors lower to a setup `dependency_resolution_error`. Concrete simulator-id suggestions require exactly one matching destination argument and available simulator UUIDs.
- Runtime install/launch failures lower to `harness_crashed` with `missing_terminal_event` and no structured failure. Truncation or signals dominate only when no build failure was parsed; parsed build failures remain `pre_test_failed`.
- `.xcworkspace`, formatter wrappers, fastlane scan, `.xcresult` parsing, and SwiftPM `swift test` are outside v0.1. Validated Xcode/Swift Testing version enforcement is deferred.
- `-only-testing:` and `-skip-testing:` flags pass through unchanged.
