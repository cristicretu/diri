import Foundation

/// Resolves the user's real interactive-login environment so agents spawned by
/// a launchd-bare daemon (minimal PATH) can still be found.
///
/// The app is launched via `open`, so the daemon it starts inherits only
/// `/usr/bin:/bin:/usr/sbin:/sbin`. Tools like `claude` (~/.local/bin), `codex`
/// (nvm), and Homebrew binaries live on the PATH the user configures in their
/// shell rc — which we recover by asking their login shell.
public enum LoginEnvironment {
    /// The PATH captured from the user's login+interactive shell, cached for the
    /// daemon's lifetime. Falls back to a sensible default if capture fails.
    public static let path: String = capturePath()

    /// The user's real login shell (e.g. /opt/homebrew/bin/fish), read from the
    /// user database via getpwuid. This is authoritative even under launchd,
    /// where the SHELL env var is often just /bin/zsh regardless of the user's
    /// actual configured shell.
    public static let loginShell: String = resolveLoginShell()

    private static let fallback =
        "\(NSHomeDirectory())/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

    private static func resolveLoginShell() -> String {
        if let pw = getpwuid(getuid()), let shell = pw.pointee.pw_shell {
            let path = String(cString: shell)
            if !path.isEmpty, FileManager.default.isExecutableFile(atPath: path) {
                return path
            }
        }
        return ProcessInfo.processInfo.environment["SHELL"] ?? "/bin/zsh"
    }

    private static func capturePath() -> String {
        let shell = loginShell
        // `printenv PATH` prints the real colon-separated env var regardless of
        // shell (fish stores $PATH space-separated, so echo would be wrong).
        // `-i -l` sources both interactive (.zshrc / config.fish) and login files.
        let process = Process()
        process.executableURL = URL(fileURLWithPath: shell)
        process.arguments = ["-i", "-l", "-c", "printenv PATH"]
        let out = Pipe()
        process.standardOutput = out
        process.standardError = FileHandle.nullDevice
        // No TTY: some interactive rc files wait on input forever. A hung
        // capture used to brick daemon init (Daemon → BrowserPool → PATH) so
        // the socket never came up — kill and fall back instead.
        let captureTimeoutSeconds: TimeInterval = 5
        do {
            try process.run()
            let watchdog = DispatchWorkItem {
                if process.isRunning { process.terminate() }
            }
            DispatchQueue.global().asyncAfter(
                deadline: .now() + captureTimeoutSeconds, execute: watchdog)
            let data = out.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            watchdog.cancel()
            // Interactive shells may print a greeting; take the last line that
            // looks like a PATH (contains a "/" and a ":").
            let lines = String(decoding: data, as: UTF8.self)
                .split(separator: "\n")
                .map { $0.trimmingCharacters(in: .whitespaces) }
            if let path = lines.last(where: { $0.contains("/") }), !path.isEmpty {
                return path.contains(":") ? path : "\(path):\(fallback)"
            }
        } catch {}
        return fallback
    }

    /// Absolute path of `binary` searched across the login PATH, or nil.
    public static func resolve(_ binary: String) -> String? {
        for dir in path.split(separator: ":") {
            let candidate = "\(dir)/\(binary)"
            if FileManager.default.isExecutableFile(atPath: candidate) {
                return candidate
            }
        }
        return nil
    }
}
