import Darwin
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
        capturePath(
            shell: loginShell,
            arguments: ["-i", "-l", "-c", "printenv PATH"],
            timeout: 5)
    }

    static func capturePath(
        shell: String,
        arguments: [String],
        timeout: TimeInterval
    ) -> String {
        // `printenv PATH` prints the real colon-separated env var regardless of
        // shell (fish stores $PATH space-separated, so echo would be wrong).
        // `-i -l` sources both interactive (.zshrc / config.fish) and login files.
        let process = Process()
        process.executableURL = URL(fileURLWithPath: shell)
        process.arguments = arguments
        // A regular file is deliberate. A background process from an rc file
        // can inherit stdout after its shell exits; a Pipe would then wait for
        // that unrelated descendant to close its copy before reporting EOF.
        // Reading a regular file stops at its current length instead.
        guard let out = anonymousCaptureFile() else { return fallback }
        process.standardOutput = out
        process.standardError = FileHandle.nullDevice
        // No TTY: some interactive rc files wait on input forever. A hung
        // capture used to brick daemon init (Daemon → BrowserPool → PATH) so
        // the socket never came up — kill the process group and fall back.
        // Wait for exit first (bounded), then read at most 1 MiB from the
        // regular capture file. Descendants that inherit stdout therefore
        // cannot extend the deadline after the shell itself exits.
        do {
            try process.run()
            let pid = process.processIdentifier
            // Best effort: Foundation may already have exec'd before this
            // parent-side call. The regular-file capture keeps the deadline
            // hard even if grouping loses that race.
            _ = setpgid(pid, pid)

            let exited = DispatchGroup()
            exited.enter()
            DispatchQueue.global().async {
                process.waitUntilExit()
                exited.leave()
            }
            if exited.wait(timeout: .now() + timeout) == .timedOut {
                kill(-pid, SIGKILL)
                kill(pid, SIGKILL)
                process.waitUntilExit()
                return fallback
            }

            out.seek(toFileOffset: 0)
            let data = out.readData(ofLength: 1 << 20)
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

    /// Owner-only, already-unlinked regular file used to capture shell output.
    /// The descriptor remains valid for the parent and any spawned child, but
    /// no pathname survives a crash or a background descendant.
    private static func anonymousCaptureFile() -> FileHandle? {
        var template = Array(
            (FileManager.default.temporaryDirectory.path + "/dirijor-path.XXXXXX").utf8CString)
        let fd = mkstemp(&template)
        guard fd >= 0 else { return nil }
        _ = unlink(template)
        return FileHandle(fileDescriptor: fd, closeOnDealloc: true)
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
