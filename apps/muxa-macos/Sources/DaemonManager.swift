import Darwin
import Foundation

struct IncompatibleMuxadError: LocalizedError {
    let socketPath: String
    let detail: String

    var errorDescription: String? {
        String(localized: "An older muxad is already serving \(socketPath). \(detail)")
    }
}

struct DaemonSocketOwner: Equatable, Sendable {
    static let legacyLaunchAgentLabels = ["dev.open330.muxad"]

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
                throw MuxaIPCError.server("refusing to replace a socket not owned by this user")
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
                guard record.command == "muxad", record.uid == getuid() else { return nil }
                return record.pid
            }
            guard candidates.count == 1, let pid = candidates.first else {
                throw MuxaIPCError.server(
                    "could not identify exactly one owner-only muxad for \(socketPath)"
                )
            }

            var pathBuffer = [CChar](repeating: 0, count: 4096)
            let pathLength = proc_pidpath(pid, &pathBuffer, UInt32(pathBuffer.count))
            guard pathLength > 0 else {
                throw MuxaIPCError.posix(operation: "proc_pidpath", code: errno)
            }
            let executablePath = String(
                decoding: pathBuffer.prefix(while: { $0 != 0 }).map { UInt8(bitPattern: $0) },
                as: UTF8.self
            )
            guard URL(fileURLWithPath: executablePath).lastPathComponent == "muxad" else {
                throw MuxaIPCError.server("the socket owner is not a muxad executable")
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
        // one. Disable known legacy labels before stopping either executable,
        // otherwise KeepAlive can win the socket race against the bundled
        // daemon and leave the app in a restart loop.
        try await Self.disableLegacyLaunchAgents()
        if let brew = homebrewExecutable {
            try await Self.stopHomebrewService(brew: brew)
        } else {
            guard Darwin.kill(pid, SIGTERM) == 0 else {
                throw MuxaIPCError.posix(operation: "kill", code: errno)
            }
        }

        for _ in 0..<100 {
            if Darwin.kill(pid, 0) != 0, errno == ESRCH { return }
            try await Task.sleep(for: .milliseconds(50))
        }
        throw MuxaIPCError.server("the existing muxad did not stop within five seconds")
    }

    private static func stopHomebrewService(brew: String) async throws {
        try await Task.detached(priority: .userInitiated) {
            let process = Process()
            let errors = Pipe()
            process.executableURL = URL(fileURLWithPath: brew)
            process.arguments = ["services", "stop", "open330/tap/muxa"]
            process.standardOutput = FileHandle.nullDevice
            process.standardError = errors
            try process.run()
            process.waitUntilExit()
            guard process.terminationStatus == 0 else {
                let data = errors.fileHandleForReading.readDataToEndOfFile()
                let detail = String(decoding: data, as: UTF8.self)
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                throw MuxaIPCError.server(
                    detail.isEmpty ? "Homebrew could not stop the muxa service" : detail
                )
            }
        }.value
    }

    private static func disableLegacyLaunchAgents() async throws {
        try await Task.detached(priority: .userInitiated) {
            let domain = "gui/\(getuid())"
            for label in legacyLaunchAgentLabels {
                let disable = Process()
                disable.executableURL = URL(fileURLWithPath: "/bin/launchctl")
                disable.arguments = ["disable", "\(domain)/\(label)"]
                disable.standardOutput = FileHandle.nullDevice
                disable.standardError = FileHandle.nullDevice
                try disable.run()
                disable.waitUntilExit()
                guard disable.terminationStatus == 0 else {
                    throw MuxaIPCError.server("could not disable legacy service \(label)")
                }

                let bootout = Process()
                bootout.executableURL = URL(fileURLWithPath: "/bin/launchctl")
                bootout.arguments = ["bootout", "\(domain)/\(label)"]
                bootout.standardOutput = FileHandle.nullDevice
                bootout.standardError = FileHandle.nullDevice
                try bootout.run()
                bootout.waitUntilExit()
                // bootout returns non-zero when the label was already unloaded;
                // the preceding persistent disable is the authoritative action.
            }
        }.value
    }
}

@MainActor
final class DaemonManager {
    private var launchedProcess: Process?

    func ensureRunning(client: MuxaIPCClient) async throws {
        do {
            try await client.hello()
            return
        } catch let error as MuxaIPCError {
            switch error {
            case .server, .incompatibleProtocol:
                throw IncompatibleMuxadError(
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

    func replaceRunningDaemon(client: MuxaIPCClient) async throws {
        let socketPath = client.socketPath
        let owner = try await DaemonSocketOwner.find(socketPath: socketPath)
        try await owner.stop()
        try await launchBundledDaemon(client: client)
    }

    private func launchBundledDaemon(client: MuxaIPCClient) async throws {
        let process = Process()
        if let bundled = bundledDaemonURL() {
            MuxaLog.daemon.info("starting bundled muxad")
            process.executableURL = bundled
            process.arguments = ["--socket", client.socketPath]
        } else {
            MuxaLog.daemon.info("starting muxad from PATH")
            process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
            process.arguments = ["muxad", "--socket", client.socketPath]
        }
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        // A GUI launch does not inherit the user's interactive-shell PATH.
        // Credentials are passed only to an individual Ask child over the
        // owner-only IPC socket, never installed daemon-wide.
        process.environment = MuxaProviderCredentialStore.augmentPath(
            ProcessInfo.processInfo.environment
        )
        try process.run()
        launchedProcess = process

        var lastError: Error?
        for attempt in 0..<30 {
            do {
                try await client.hello()
                MuxaLog.daemon.info("muxad is ready")
                return
            } catch {
                lastError = error
                if !process.isRunning, attempt > 2 { break }
                try await Task.sleep(for: .milliseconds(100))
            }
        }
        MuxaLog.daemon.error(
            "muxad did not become ready: \((lastError?.localizedDescription ?? "unknown error"), privacy: .public)"
        )
        throw lastError ?? MuxaIPCError.server("muxad did not become ready")
    }

    private func bundledDaemonURL() -> URL? {
        let url = Bundle.main.bundleURL
            .appendingPathComponent("Contents", isDirectory: true)
            .appendingPathComponent("Helpers", isDirectory: true)
            .appendingPathComponent("muxad")
        return FileManager.default.isExecutableFile(atPath: url.path) ? url : nil
    }
}
