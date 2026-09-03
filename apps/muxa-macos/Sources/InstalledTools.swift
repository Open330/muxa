import Foundation

/// One command-line tool found on the user's PATH.
struct InstalledTool: Identifiable, Sendable, Equatable {
    let name: String
    let path: String
    /// First line of `<tool> --version`, trimmed; nil when the probe failed
    /// or timed out.
    let version: String?

    var id: String { name }
}

/// Finds agent CLIs (and tmux/muxa) the way a terminal would: GUI apps
/// inherit a minimal PATH, so the login shell's PATH is consulted first and
/// the usual per-user install folders are appended as a fallback.
enum InstalledTools {
    static let agentPrograms = ["claude", "codex", "gemini", "agy", "opencode"]
    static let supportPrograms = ["tmux", "muxa"]

    /// Directories appended after the login-shell PATH; cheap insurance for
    /// shells whose rc files do not export PATH for `-lc`.
    static var fallbackDirectories: [String] {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return [
            "\(home)/.cargo/bin",
            "\(home)/.local/bin",
            "\(home)/.npm-global/bin",
            "\(home)/.bun/bin",
            "\(home)/.volta/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
        ]
    }

    /// Resolves each name and probes its version off the main actor.
    static func detect(_ names: [String]) async -> [InstalledTool] {
        let directories = await searchDirectories()
        return await withTaskGroup(of: (Int, InstalledTool?).self) { group in
            for (index, name) in names.enumerated() {
                group.addTask {
                    guard let path = resolve(name, in: directories) else { return (index, nil) }
                    let version = await probeVersion(named: name, at: path)
                    return (index, InstalledTool(name: name, path: path, version: version))
                }
            }
            var found: [(Int, InstalledTool)] = []
            for await (index, tool) in group {
                if let tool { found.append((index, tool)) }
            }
            return found.sorted { $0.0 < $1.0 }.map(\.1)
        }
    }

    /// The login shell's PATH entries followed by the fallback folders,
    /// de-duplicated in order.
    static func searchDirectories() async -> [String] {
        let shell = ProcessInfo.processInfo.environment["SHELL"].flatMap { $0.isEmpty ? nil : $0 } ?? "/bin/zsh"
        let fromShell = await runCapturing(shell, ["-lc", "printf %s \"$PATH\""], timeout: 3) ?? ""
        let fromProcess = ProcessInfo.processInfo.environment["PATH"] ?? ""
        return mergedDirectories(
            pathStrings: [fromShell, fromProcess],
            fallback: fallbackDirectories
        )
    }

    /// Pure helper (unit-tested): splits PATH strings and appends fallbacks
    /// without duplicates or empty entries.
    static func mergedDirectories(pathStrings: [String], fallback: [String]) -> [String] {
        var seen = Set<String>()
        var result: [String] = []
        for entry in pathStrings.flatMap({ $0.split(separator: ":").map(String.init) }) + fallback {
            let trimmed = entry.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !trimmed.isEmpty, seen.insert(trimmed).inserted else { continue }
            result.append(trimmed)
        }
        return result
    }

    /// First executable named `name` in `directories`.
    static func resolve(_ name: String, in directories: [String]) -> String? {
        for directory in directories {
            let candidate = (directory as NSString).appendingPathComponent(name)
            if FileManager.default.isExecutableFile(atPath: candidate) {
                return candidate
            }
        }
        return nil
    }

    /// Pure helper (unit-tested): the first non-empty line of a
    /// `--version` output, trimmed.
    static func versionLine(from output: String) -> String? {
        output
            .split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .first { !$0.isEmpty }
    }

    /// Version flags to try, in order. Most tools take `--version`; tmux only
    /// understands `-V` and prints its usage to stderr for anything else.
    static func versionArguments(for name: String) -> [[String]] {
        switch name {
        case "tmux": [["-V"], ["--version"]]
        default: [["--version"], ["-V"], ["version"]]
        }
    }

    private static func probeVersion(named name: String, at path: String) async -> String? {
        for arguments in versionArguments(for: name) {
            if let output = await runCapturing(path, arguments, timeout: 3),
               let line = versionLine(from: output)
            {
                return line
            }
        }
        return nil
    }

    /// Runs a process with a timeout and returns its stdout; nil on failure
    /// or timeout. Never called on the main actor's thread.
    private static func runCapturing(_ executable: String, _ arguments: [String], timeout: TimeInterval) async -> String? {
        await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .utility).async {
                let process = Process()
                process.executableURL = URL(fileURLWithPath: executable)
                process.arguments = arguments
                let pipe = Pipe()
                process.standardOutput = pipe
                process.standardError = FileHandle.nullDevice
                process.standardInput = FileHandle.nullDevice
                var environment = ProcessInfo.processInfo.environment
                environment["NO_COLOR"] = "1"
                process.environment = environment
                do {
                    try process.run()
                } catch {
                    continuation.resume(returning: nil)
                    return
                }
                let deadline = DispatchWorkItem {
                    if process.isRunning { process.terminate() }
                }
                DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + timeout, execute: deadline)
                let data = pipe.fileHandleForReading.readDataToEndOfFile()
                process.waitUntilExit()
                deadline.cancel()
                guard process.terminationStatus == 0 || !data.isEmpty else {
                    continuation.resume(returning: nil)
                    return
                }
                continuation.resume(returning: String(decoding: data, as: UTF8.self))
            }
        }
    }
}
