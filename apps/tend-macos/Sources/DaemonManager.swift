import Darwin
import Foundation

struct IncompatibleTenddError: LocalizedError {
    let socketPath: String
    let detail: String

    var errorDescription: String? {
        String(localized: "An older tendd is already serving \(socketPath). \(detail)")
    }
}

struct DaemonSocketOwner: Equatable, Sendable {
    static let legacyLaunchAgentLabels = ["dev.open330.tendd"]

    struct LsofRecord: Equatable, Sendable {
        var pid: Int32?
        var command: String?
        var uid: uid_t?
    }

    let pid: Int32
    let executablePath: String

    static func find(socketPath: String) async throws -> DaemonSocketOwner {
        try await Task.detached(priority: .userInitiated) {
            let attributes = try FileManager.default.attributesOfItem(atPath: socketPath)
            let socketOwner = (attributes[.ownerAccountID] as? NSNumber)?.uint32Value
            guard socketOwner == getuid() else {
                throw TendIPCError.server("refusing to replace a socket not owned by this user")
            }

            let process = Process()
            let output = Pipe()
            process.executableURL = URL(fileURLWithPath: "/usr/sbin/lsof")
            process.arguments = ["-nP", "-Fpcu", "-a", "-U", socketPath]
            process.standardOutput = output
            process.standardError = FileHandle.nullDevice
            try process.run()
            process.waitUntilExit()

            let data = output.fileHandleForReading.readDataToEndOfFile()
            let records = parseLsof(String(decoding: data, as: UTF8.self))
            let candidates = records.compactMap { record -> Int32? in
                guard record.command == "tendd", record.uid == getuid() else { return nil }
                return record.pid
            }
            guard candidates.count == 1, let pid = candidates.first else {
                throw TendIPCError.server(
                    "could not identify exactly one owner-only tendd for \(socketPath)"
                )
            }

            var pathBuffer = [CChar](repeating: 0, count: 4096)
            let pathLength = proc_pidpath(pid, &pathBuffer, UInt32(pathBuffer.count))
            guard pathLength > 0 else {
                throw TendIPCError.posix(operation: "proc_pidpath", code: errno)
            }
            let executablePath = String(
                decoding: pathBuffer.prefix(while: { $0 != 0 }).map { UInt8(bitPattern: $0) },
                as: UTF8.self
            )
            guard URL(fileURLWithPath: executablePath).lastPathComponent == "tendd" else {
                throw TendIPCError.server("the socket owner is not a tendd executable")
            }
            return DaemonSocketOwner(pid: pid, executablePath: executablePath)
        }.value
    }

    static func parseLsof(_ output: String) -> [LsofRecord] {
        var records: [LsofRecord] = []
        var current: LsofRecord?
        for line in output.split(separator: "\n") {
            guard let field = line.first else { continue }
            let value = String(line.dropFirst())
            if field == "p" {
                if let current { records.append(current) }
                current = LsofRecord(pid: Int32(value), command: nil, uid: nil)
            } else if field == "c" {
                current?.command = value
            } else if field == "u" {
                current?.uid = uid_t(value)
            }
        }
        if let current { records.append(current) }
        return records
    }

    var homebrewExecutable: String? {
        guard let candidate = Self.homebrewExecutablePath(for: executablePath) else {
            return nil
        }
        return FileManager.default.isExecutableFile(atPath: candidate) ? candidate : nil
    }

    static func homebrewExecutablePath(for daemonPath: String) -> String? {
        if daemonPath.hasPrefix("/opt/homebrew/") {
            return "/opt/homebrew/bin/brew"
        } else if daemonPath.hasPrefix("/usr/local/") {
            return "/usr/local/bin/brew"
        } else {
            return nil
        }
    }

    func stop() async throws {
        // A launchd-managed cargo install is just as persistent as a Homebrew
        // one. Unload known legacy labels before stopping either executable,
        // otherwise KeepAlive can win the socket race against the bundled
        // daemon and leave the app in a restart loop.
        try await Self.bootOutLegacyLaunchAgents()
        if let brew = homebrewExecutable {
            try await Self.stopHomebrewService(brew: brew)
        }
        // Signal the pid even after a Homebrew stop: `brew services stop`
        // exits 0 with only a "not started" warning when this tendd was run
        // by hand (`tendd --socket …` from a shell) rather than as the
        // service, and leaves the process holding the socket.
        if Darwin.kill(pid, SIGTERM) != 0, errno != ESRCH {
            // ESRCH means the service stop or the bootout above already
            // reaped the daemon, which is the state this call is asking for.
            throw TendIPCError.posix(operation: "kill", code: errno)
        }

        for _ in 0..<100 {
            if Darwin.kill(pid, 0) != 0, errno == ESRCH { return }
            try await Task.sleep(for: .milliseconds(50))
        }
        throw TendIPCError.server("the existing tendd did not stop within five seconds")
    }

    private static func stopHomebrewService(brew: String) async throws {
        try await Task.detached(priority: .userInitiated) {
            let process = Process()
            let errors = Pipe()
            process.executableURL = URL(fileURLWithPath: brew)
            process.arguments = ["services", "stop", "open330/tap/tend"]
            process.standardOutput = FileHandle.nullDevice
            process.standardError = errors
            try process.run()
            process.waitUntilExit()
            guard process.terminationStatus == 0 else {
                let data = errors.fileHandleForReading.readDataToEndOfFile()
                let detail = String(decoding: data, as: UTF8.self)
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                throw TendIPCError.server(
                    detail.isEmpty ? "Homebrew could not stop the tend service" : detail
                )
            }
        }.value
    }

    /// Unload the legacy `LaunchAgent` for the rest of this login session, so
    /// its `KeepAlive` cannot resurrect the old daemon while the app brings up
    /// the bundled one.
    ///
    /// Deliberately `bootout` and never `launchctl disable`: `disable` writes a
    /// persistent override into
    /// `/var/db/com.apple.xpc.launchd/disabled.<uid>.plist` that survives the
    /// reboot and outranks the plist's own `RunAtLoad`, so one "replace the
    /// running daemon" would leave tendd silently absent at every later login
    /// — and `tend init` re-bootstraps a label launchd still refuses to run.
    /// `bootout` is scoped to the current session, which is the only window
    /// this race is fought in.
    private static func bootOutLegacyLaunchAgents() async throws {
        try await Task.detached(priority: .userInitiated) {
            let domain = "gui/\(getuid())"
            for label in legacyLaunchAgentLabels {
                let bootout = Process()
                bootout.executableURL = URL(fileURLWithPath: "/bin/launchctl")
                bootout.arguments = ["bootout", "\(domain)/\(label)"]
                bootout.standardOutput = FileHandle.nullDevice
                bootout.standardError = FileHandle.nullDevice
                try bootout.run()
                bootout.waitUntilExit()
                // A non-zero exit means the label was not loaded to begin
                // with, which is already the state this call wants.
            }
        }.value
    }
}

@MainActor
final class DaemonManager {
    private var launchedProcess: Process?

    func ensureRunning(client: TendIPCClient) async throws {
        do {
            try await client.hello()
            return
        } catch let error as TendIPCError {
            switch error {
            case .server, .incompatibleProtocol:
                throw IncompatibleTenddError(
                    socketPath: client.socketPath,
                    detail: error.localizedDescription
                )
            case .posix(_, let code) where code == ENOENT || code == ECONNREFUSED || code == ENOTSOCK:
                break
            default:
                throw error
            }
        }

        try await launchBundledDaemon(client: client)
    }

    func replaceRunningDaemon(client: TendIPCClient) async throws {
        let socketPath = client.socketPath
        let owner = try await DaemonSocketOwner.find(socketPath: socketPath)
        try await owner.stop()
        try await launchBundledDaemon(client: client)
    }

    private func launchBundledDaemon(client: TendIPCClient) async throws {
        let process = Process()
        if let bundled = bundledDaemonURL() {
            TendLog.daemon.info("starting bundled tendd")
            process.executableURL = bundled
            process.arguments = ["--socket", client.socketPath]
        } else {
            TendLog.daemon.info("starting tendd from PATH")
            process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
            process.arguments = ["tendd", "--socket", client.socketPath]
        }
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        // A GUI launch does not inherit the user's interactive-shell PATH.
        // Credentials are passed only to an individual Ask child over the
        // owner-only IPC socket, never installed daemon-wide.
        process.environment = TendProviderCredentialStore.augmentPath(
            ProcessInfo.processInfo.environment
        )
        try process.run()
        launchedProcess = process

        var lastError: Error?
        for attempt in 0..<30 {
            do {
                try await client.hello()
                TendLog.daemon.info("tendd is ready")
                return
            } catch {
                lastError = error
                if !process.isRunning, attempt > 2 { break }
                try await Task.sleep(for: .milliseconds(100))
            }
        }
        TendLog.daemon.error(
            "tendd did not become ready: \((lastError?.localizedDescription ?? "unknown error"), privacy: .public)"
        )
        throw lastError ?? TendIPCError.server("tendd did not become ready")
    }

    private func bundledDaemonURL() -> URL? {
        let url = Bundle.main.bundleURL
            .appendingPathComponent("Contents", isDirectory: true)
            .appendingPathComponent("Helpers", isDirectory: true)
            .appendingPathComponent("tendd")
        return FileManager.default.isExecutableFile(atPath: url.path) ? url : nil
    }
}
