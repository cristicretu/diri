import Foundation

/// The two tests that hang on GitHub runners are skipped there, but the reason
/// one of them hangs is still unknown — and it cannot be found on a dev Mac,
/// where both pass every time. So the skip has an escape hatch: set
/// `DIRIJOR_RUN_HANGING_TESTS=1` and they run on CI too, next to
/// `scripts/sample-hung-tests.sh`, which prints where the run actually stopped.
///
/// Deliberately opt-in and off by default: the point is a stack trace from one
/// manual run, not a suite that hangs again on every pull request.
enum HangingTestGate {
    static var isEnabled: Bool {
        let env = ProcessInfo.processInfo.environment
        return env["CI"] == nil || env["DIRIJOR_RUN_HANGING_TESTS"] == "1"
    }
}
