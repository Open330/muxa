import SwiftUI

/// The modules this build ships, whether each is switched on, and what they
/// contribute.
///
/// Enablement is per module and remembered in preferences. A module that is
/// switched off contributes nothing and is not probed, so an operator who
/// does not use one never pays for it.
@MainActor
final class MuxaModuleRegistry: ObservableObject {
    static let shared = MuxaModuleRegistry()

    /// Every module the app knows, in the order the Modules pane lists them.
    let modules: [any MuxaModule]

    /// Bumped after a probe so views re-read `availability`, which lives on
    /// the modules rather than here.
    @Published private(set) var probeGeneration = 0

    private let defaults: UserDefaults

    init(modules: [any MuxaModule]? = nil, defaults: UserDefaults = .standard) {
        self.defaults = defaults
        self.modules = modules ?? [AasModule(), AirModule()]
    }

    /// Preference key for a module's switch.
    static func enabledKey(_ id: String) -> String { "muxa.modules.\(id).enabled" }

    func isEnabled(_ id: String) -> Bool {
        // Off until the operator turns it on: a module reaches outside muxa,
        // and that is a decision to make rather than a default to discover.
        defaults.bool(forKey: Self.enabledKey(id))
    }

    func setEnabled(_ id: String, _ enabled: Bool) {
        defaults.set(enabled, forKey: Self.enabledKey(id))
        objectWillChange.send()
        guard enabled, let module = module(id: id) else { return }
        Task { await probe(module) }
    }

    func module(id: String) -> (any MuxaModule)? {
        modules.first { $0.id == id }
    }

    /// The concrete module of a given type, whether or not it is enabled —
    /// for the few call sites that need more than the protocol offers.
    func module<Module: MuxaModule>(_ type: Module.Type) -> Module? {
        modules.compactMap { $0 as? Module }.first
    }

    /// Enabled modules only. What every contribution point iterates.
    var enabledModules: [any MuxaModule] {
        modules.filter { isEnabled($0.id) }
    }

    func probeEnabled() async {
        for module in enabledModules {
            await probe(module)
        }
    }

    func probe(_ module: any MuxaModule) async {
        await module.probe()
        probeGeneration &+= 1
    }

    /// Everything the enabled modules offer for this object. A module whose
    /// tool is missing contributes nothing unless it says its actions work
    /// without one — a converter does not need the editor installed.
    func actions(for context: MuxaModuleContext, model: AppModel) -> [MuxaModuleAction] {
        enabledModules
            .filter { $0.availability.isAvailable || $0.worksWithoutTool }
            .flatMap { $0.actions(for: context, model: model) }
    }
}

/// Runs a module's command-line tool. Modules shell out the way muxa itself
/// shells out to tmux and git: an argv, no shell, a bounded wait, and the
/// login shell's PATH so a GUI launch finds what a terminal would.
enum MuxaModuleProcess {
    struct Output: Sendable {
        let status: Int32
        let stdout: String
        let stderr: String

        var succeeded: Bool { status == 0 }
    }

    enum Failure: LocalizedError {
        case notFound(String)
        case launch(String)
        case timedOut(String, TimeInterval)

        var errorDescription: String? {
            switch self {
            case .notFound(let name):
                String(localized: "\(name) was not found on your PATH")
            case .launch(let message):
                message
            case .timedOut(let name, let seconds):
                String(localized: "\(name) did not answer within \(Int(seconds)) seconds")
            }
        }
    }

    /// Resolves `name` the way the Welcome checklist does, so a module and
    /// the setup list never disagree about whether a tool is installed.
    static func resolve(_ name: String) async -> String? {
        if name.hasPrefix("/") {
            return FileManager.default.isExecutableFile(atPath: name) ? name : nil
        }
        return InstalledTools.resolve(name, in: await InstalledTools.searchDirectories())
    }

    static func run(
        _ name: String,
        _ arguments: [String],
        timeout: TimeInterval = 20,
        environment extra: [String: String] = [:]
    ) async throws -> Output {
        guard let path = await resolve(name) else { throw Failure.notFound(name) }
        return try await withCheckedThrowingContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                let process = Process()
                process.executableURL = URL(fileURLWithPath: path)
                process.arguments = arguments
                var environment = ProcessInfo.processInfo.environment
                environment["NO_COLOR"] = "1"
                for (key, value) in extra { environment[key] = value }
                process.environment = environment
                let out = Pipe()
                let err = Pipe()
                process.standardOutput = out
                process.standardError = err
                process.standardInput = FileHandle.nullDevice
                do {
                    try process.run()
                } catch {
                    continuation.resume(throwing: Failure.launch(error.localizedDescription))
                    return
                }
                let deadline = DispatchWorkItem {
                    if process.isRunning { process.terminate() }
                }
                DispatchQueue.global(qos: .utility)
                    .asyncAfter(deadline: .now() + timeout, execute: deadline)
                let outData = out.fileHandleForReading.readDataToEndOfFile()
                let errData = err.fileHandleForReading.readDataToEndOfFile()
                process.waitUntilExit()
                let timedOut = deadline.isCancelled == false && process.terminationReason == .uncaughtSignal
                deadline.cancel()
                if timedOut {
                    continuation.resume(throwing: Failure.timedOut(name, timeout))
                    return
                }
                continuation.resume(
                    returning: Output(
                        status: process.terminationStatus,
                        stdout: String(decoding: outData, as: UTF8.self),
                        stderr: String(decoding: errData, as: UTF8.self)
                    )
                )
            }
        }
    }
}
