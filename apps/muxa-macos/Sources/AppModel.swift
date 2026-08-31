import Foundation

enum MuxaSidebarSelection: Codable, Hashable, Sendable {
    case workBoard
    case watch
    case ask
    case work(MuxaWorkIdentity)
    case agent(String)
    case host(String)
    case shell(String)
    case pane(MuxaWatchPaneIdentity)
}

enum MuxaSidebarMode: String, CaseIterable, Identifiable {
    case work
    case watch
    case hosts
    case shells

    var id: Self { self }

    var title: String {
        switch self {
        case .work: "Work"
        case .watch: "Explore"
        case .hosts: "Hosts"
        case .shells: "Shells"
        }
    }

    var systemImage: String {
        switch self {
        case .work: "square.stack.3d.up"
        case .watch: "sidebar.left"
        case .hosts: "network"
        case .shells: "terminal"
        }
    }
}

struct MuxaHostRegistrationRequest: Sendable {
    let alias: String
    let ssh: String
    let mode: String
    let connect: String
    let muxaPath: String
    let remoteSocket: String?
    let overwrite: Bool
}

@MainActor
final class AppModel: ObservableObject {
    enum ConnectionState: Equatable {
        case connecting
        case connected
        case upgradeRequired(String)
        case failed(String)
    }

    @Published private(set) var sessions: [MuxaSession] = []
    @Published private(set) var pipelineRuns: [MuxaPipelineRun] = []
    @Published private(set) var executionSnapshot = MuxaExecutionSnapshot.empty
    @Published private(set) var workspaceRevision: UInt64 = 0
    @Published var sidebarSelection: MuxaSidebarSelection?
    @Published var sidebarMode: MuxaSidebarMode = .work
    @Published var watchSelection: MuxaWatchPaneIdentity?
    @Published private(set) var connectionState: ConnectionState = .connecting
    @Published private(set) var isCreatingSession = false
    @Published private(set) var isTerminatingSession = false
    @Published private(set) var isAttachingPane = false
    @Published private(set) var attachError: String?
    @Published var isPresentingWorkStart = false
    @Published private(set) var isStartingWork = false
    @Published private(set) var workStartStatus: String?
    @Published private(set) var workStartError: String?
    @Published var isConfirmingDaemonReplacement = false
    @Published private(set) var askEntries: [MuxaAskEntry] = []
    @Published private(set) var askAgent = "claude"
    @Published private(set) var isSendingAsk = false
    @Published private(set) var askError: String?
    @Published var isPresentingHostRegistration = false
    @Published private(set) var isRegisteringHost = false
    @Published private(set) var hostRegistrationError: String?

    let client: MuxaIPCClient
    private let daemon = DaemonManager()
    private var refreshTask: Task<Void, Never>?
    private var connectionGeneration: UInt64 = 0
    private var refreshInFlight = false
    private var shellNumber = 1

    var isConnected: Bool {
        connectionState == .connected
    }

    var needsWorkConfiguration: Bool {
        guard let workStartError else { return false }
        return workStartError.contains("no [[route]]")
            || workStartError.contains("no pipeline")
            || workStartError.contains("unknown pipeline")
    }

    var workGroups: [MuxaWorkGroup] {
        executionSnapshot.workGroups(pipelineRuns: pipelineRuns)
    }

    var agents: [MuxaAgent] { executionSnapshot.agents }

    var fleetHosts: [MuxaFleetHost] { executionSnapshot.hosts }

    var hostedAgents: [MuxaHostedAgent] {
        executionSnapshot.hostedAgents
            .filter { $0.agent.state != "stopped" }
            .sorted { left, right in
                let leftPriority = Self.agentPriority(left.agent.state)
                let rightPriority = Self.agentPriority(right.agent.state)
                if leftPriority != rightPriority { return leftPriority < rightPriority }
                if left.host.local != right.host.local { return left.host.local }
                if left.host.alias != right.host.alias { return left.host.alias < right.host.alias }
                return left.agent.agentSessionID < right.agent.agentSessionID
            }
    }

    var selectedSessionID: String? {
        guard case let .shell(id) = sidebarSelection else { return nil }
        return id
    }

    init(client: MuxaIPCClient = MuxaIPCClient()) {
        self.client = client
    }

    nonisolated static func isRunningTests(
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) -> Bool {
        environment["XCTestConfigurationFilePath"] != nil
    }

    func start() {
        guard !Self.isRunningTests() else { return }
        guard refreshTask == nil else { return }
        beginConnection(replacingExistingDaemon: false)
    }

    func replaceRunningDaemon() {
        isConfirmingDaemonReplacement = false
        beginConnection(replacingExistingDaemon: true)
    }

    func retryConnection() {
        beginConnection(replacingExistingDaemon: false)
    }

    private func beginConnection(replacingExistingDaemon: Bool) {
        refreshTask?.cancel()
        connectionGeneration &+= 1
        let generation = connectionGeneration
        connectionState = .connecting
        refreshTask = Task { [weak self] in
            guard let self else { return }
            defer {
                if connectionGeneration == generation {
                    refreshTask = nil
                }
            }
            do {
                if replacingExistingDaemon {
                    try await daemon.replaceRunningDaemon(client: client)
                } else {
                    try await daemon.ensureRunning(client: client)
                }
                guard connectionGeneration == generation else { return }
                connectionState = .connected
                await refresh(ifGeneration: generation)
                while !Task.isCancelled {
                    try await Task.sleep(for: .seconds(2))
                    await refresh(ifGeneration: generation)
                }
            } catch is CancellationError {
                return
            } catch let error as IncompatibleMuxadError {
                guard connectionGeneration == generation else { return }
                MuxaLog.app.error(
                    "incompatible daemon: \(error.localizedDescription, privacy: .public)"
                )
                connectionState = .upgradeRequired(error.localizedDescription)
            } catch {
                guard connectionGeneration == generation else { return }
                MuxaLog.app.error(
                    "connection failed: \(error.localizedDescription, privacy: .public)"
                )
                connectionState = .failed(error.localizedDescription)
            }
        }
    }

    func refresh() async {
        await refresh(ifGeneration: nil)
    }

    private func refresh(ifGeneration generation: UInt64?) async {
        guard !refreshInFlight else { return }
        refreshInFlight = true
        defer { refreshInFlight = false }
        do {
            try await client.hello()
            async let sessionsRequest = client.listSessions()
            async let pipelineRequest = client.listPipelineRuns()
            async let executionRequest = client.executionSnapshot()
            async let askEntriesRequest = try? client.listAskEntries()
            async let askAgentRequest = try? client.selectedAskAgent()
            let (listedSessions, listedRuns, listedExecution, listedAskEntries, selectedAskAgent) = try await (
                sessionsRequest,
                pipelineRequest,
                executionRequest,
                askEntriesRequest,
                askAgentRequest
            )
            let updated = listedSessions
                .sorted { lhs, rhs in
                    if lhs.exited != rhs.exited { return !lhs.exited }
                    return (lhs.displayName ?? lhs.id) < (rhs.displayName ?? rhs.id)
                }
            if let generation, connectionGeneration != generation { return }
            sessions = updated
            pipelineRuns = listedRuns.sorted {
                if $0.identity.workspaceID != $1.identity.workspaceID {
                    return $0.identity.workspaceID < $1.identity.workspaceID
                }
                return $0.identity.workID < $1.identity.workID
            }
            executionSnapshot = listedExecution
            if let listedAskEntries { askEntries = listedAskEntries }
            if let selectedAskAgent { askAgent = selectedAskAgent }
            reconcileWatchSelection()
            workspaceRevision &+= 1
            reconcileSelection()
            connectionState = .connected
        } catch is CancellationError {
            return
        } catch {
            if let generation, connectionGeneration != generation { return }
            MuxaLog.app.error(
                "refresh failed: \(error.localizedDescription, privacy: .public)"
            )
            if let error = error as? MuxaIPCError {
                switch error {
                case .server, .incompatibleProtocol:
                    connectionState = .upgradeRequired(error.localizedDescription)
                default:
                    connectionState = .failed(error.localizedDescription)
                }
            } else {
                connectionState = .failed(error.localizedDescription)
            }
        }
    }

    func createShell() {
        guard isConnected, !isCreatingSession else { return }
        isCreatingSession = true
        Task {
            defer { isCreatingSession = false }
            do {
                let environment = ProcessInfo.processInfo.environment
                let shell = environment["SHELL"].flatMap { $0.isEmpty ? nil : $0 } ?? "/bin/zsh"
                let cwd = FileManager.default.homeDirectoryForCurrentUser.path
                var terminalEnvironment = environment
                terminalEnvironment["TERM"] = "xterm-256color"
                terminalEnvironment["COLORTERM"] = "truecolor"
                terminalEnvironment["TERM_PROGRAM"] = "Muxa"
                terminalEnvironment["TERM_PROGRAM_VERSION"] = Bundle.main
                    .object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
                    ?? "development"
                terminalEnvironment["SHELL"] = shell
                if terminalEnvironment["LANG"]?.isEmpty != false {
                    terminalEnvironment["LANG"] = "en_US.UTF-8"
                }
                if terminalEnvironment["LC_ALL"]?.isEmpty != false,
                   terminalEnvironment["LC_CTYPE"]?.isEmpty != false {
                    terminalEnvironment["LC_CTYPE"] = "en_US.UTF-8"
                }
                let session = try await client.spawnShell(
                    command: shell,
                    cwd: cwd,
                    name: "Muxa Shell \(shellNumber)",
                    environment: terminalEnvironment
                )
                shellNumber += 1
                await refresh()
                select(.shell(session.id))
            } catch {
                MuxaLog.app.error(
                    "session creation failed: \(error.localizedDescription, privacy: .public)"
                )
                connectionState = .failed(error.localizedDescription)
            }
        }
    }

    func presentWorkStart() {
        workStartError = nil
        workStartStatus = nil
        isPresentingWorkStart = true
    }

    func startWork(_ request: MuxaWorkStartRequest) async -> Bool {
        guard isConnected, !isStartingWork else { return false }
        isStartingWork = true
        workStartError = nil
        workStartStatus = "Submitting Work to muxad…"
        defer { isStartingWork = false }
        do {
            var operation = try await client.startWork(request)
            workStartStatus = operation.message
            while operation.state == .running {
                try await Task.sleep(for: .seconds(1))
                operation = try await client.workOperation(id: operation.operationID)
                workStartStatus = operation.message
            }
            guard operation.state == .succeeded else {
                workStartError = operation.message
                return false
            }
            await refresh()
            if operation.result?.dryRun != true,
               let result = operation.result {
                let identity = MuxaWorkIdentity(
                    workspaceID: result.workspace,
                    workID: result.work
                )
                if workGroups.contains(where: { $0.identity == identity }) {
                    select(.work(identity))
                } else {
                    select(.workBoard)
                }
            } else {
                select(.workBoard)
            }
            return true
        } catch is CancellationError {
            workStartError = "The app stopped waiting, but muxad may still be running this Work operation."
            return false
        } catch {
            MuxaLog.app.error(
                "work start failed: \(error.localizedDescription, privacy: .public)"
            )
            workStartError = error.localizedDescription
            return false
        }
    }

    /// Open the canonical interactive `muxa work init` wizard inside a native
    /// Shell tab. This gives an unconfigured installation a recovery path in
    /// the app instead of leaving the user with a CLI-only error message.
    func configureWork(cwd: String?) async -> Bool {
        guard isConnected, !isCreatingSession else { return false }
        isCreatingSession = true
        workStartError = nil
        workStartStatus = "Opening the Work pipeline setup wizard…"
        defer { isCreatingSession = false }
        do {
            let bundled = Bundle.main.bundleURL
                .appendingPathComponent("Contents", isDirectory: true)
                .appendingPathComponent("Helpers", isDirectory: true)
                .appendingPathComponent("muxa")
            let hasBundledCLI = FileManager.default.isExecutableFile(atPath: bundled.path)
            let command = hasBundledCLI ? bundled.path : "/usr/bin/env"
            var arguments = ["work", "init"]
            if !hasBundledCLI { arguments.insert("muxa", at: 0) }

            var environment = ProcessInfo.processInfo.environment
            environment["MUXA_SOCKET"] = client.socketPath
            environment["TERM"] = "xterm-256color"
            environment["COLORTERM"] = "truecolor"
            environment["TERM_PROGRAM"] = "Muxa"
            let requestedDirectory = cwd?.trimmingCharacters(in: .whitespacesAndNewlines)
            let workingDirectory = requestedDirectory.flatMap { value in
                value.isEmpty ? nil : value
            } ?? FileManager.default.homeDirectoryForCurrentUser.path
            let session = try await client.spawnShell(
                command: command,
                arguments: arguments,
                cwd: workingDirectory,
                name: "Configure Muxa Work",
                environment: environment
            )
            await refresh()
            select(.shell(session.id))
            workStartStatus = nil
            return true
        } catch {
            MuxaLog.app.error(
                "Work configuration launch failed: \(error.localizedDescription, privacy: .public)"
            )
            workStartStatus = nil
            workStartError = error.localizedDescription
            return false
        }
    }

    func prompt(work: MuxaWorkGroup, text: String) async throws -> Int {
        let prompt = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !prompt.isEmpty else { return 0 }
        var sent = 0
        var failures: [String] = []
        for participant in work.participants {
            guard let pane = participant.pane else { continue }
            do {
                try await client.sendFleetPrompt(
                    host: participant.host,
                    pane: pane,
                    text: prompt
                )
                sent += 1
            } catch {
                failures.append("\(participant.host.alias)/\(pane.paneID): \(error.localizedDescription)")
            }
        }
        if sent == 0, !failures.isEmpty {
            throw MuxaIPCError.server(failures.joined(separator: "\n"))
        }
        if !failures.isEmpty {
            MuxaLog.app.warning("Work prompt partial failure: \(failures.joined(separator: "; "), privacy: .public)")
        }
        return sent
    }

    func sendAsk(prompt: String, agent: String) async -> Bool {
        let prompt = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !prompt.isEmpty, !isSendingAsk else { return false }
        isSendingAsk = true
        askError = nil
        defer { isSendingAsk = false }
        do {
            if askAgent != agent {
                askAgent = try await client.selectAskAgent(agent)
            }
            let entry = try await client.sendAsk(prompt)
            askEntries.removeAll { $0.id == entry.id }
            askEntries.insert(entry, at: 0)
            return true
        } catch {
            askError = error.localizedDescription
            return false
        }
    }

    func resetAskConversation() async {
        askError = nil
        do {
            try await client.resetAskConversation()
        } catch {
            askError = error.localizedDescription
        }
    }

    func presentHostRegistration() {
        hostRegistrationError = nil
        isPresentingHostRegistration = true
    }

    func registerHost(_ request: MuxaHostRegistrationRequest) async -> Bool {
        guard !isRegisteringHost else { return false }
        let alias = request.alias.trimmingCharacters(in: .whitespacesAndNewlines)
        let ssh = request.ssh.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !alias.isEmpty, !ssh.isEmpty else {
            hostRegistrationError = "Host alias and SSH target are required."
            return false
        }
        isRegisteringHost = true
        hostRegistrationError = nil
        defer { isRegisteringHost = false }
        do {
            var arguments = ["host", "add", alias, ssh, "--mode", request.mode, "--connect", request.connect]
            let muxaPath = request.muxaPath.trimmingCharacters(in: .whitespacesAndNewlines)
            arguments += ["--muxa-path", muxaPath.isEmpty ? "muxa" : muxaPath]
            if let socket = request.remoteSocket?.trimmingCharacters(in: .whitespacesAndNewlines),
               !socket.isEmpty {
                arguments += ["--remote-socket", socket]
            }
            if request.overwrite { arguments.append("--overwrite") }
            _ = try await Self.runBundledMuxa(arguments: arguments, socketPath: client.socketPath)
            isPresentingHostRegistration = false
            try? await Task.sleep(for: .milliseconds(500))
            beginConnection(replacingExistingDaemon: false)
            return true
        } catch {
            hostRegistrationError = error.localizedDescription
            return false
        }
    }

    nonisolated private static func runBundledMuxa(
        arguments: [String],
        socketPath: String
    ) async throws -> String {
        let bundled = Bundle.main.bundleURL
            .appendingPathComponent("Contents", isDirectory: true)
            .appendingPathComponent("Helpers", isDirectory: true)
            .appendingPathComponent("muxa")
        let hasBundledCLI = FileManager.default.isExecutableFile(atPath: bundled.path)
        return try await Task.detached {
            let process = Process()
            process.executableURL = hasBundledCLI ? bundled : URL(fileURLWithPath: "/usr/bin/env")
            process.arguments = hasBundledCLI ? arguments : ["muxa"] + arguments
            var environment = ProcessInfo.processInfo.environment
            environment["MUXA_SOCKET"] = socketPath
            process.environment = environment
            let output = Pipe()
            let errors = Pipe()
            process.standardOutput = output
            process.standardError = errors
            try process.run()
            process.waitUntilExit()
            let standardOutput = output.fileHandleForReading.readDataToEndOfFile()
            let standardError = errors.fileHandleForReading.readDataToEndOfFile()
            let message = String(data: standardOutput, encoding: .utf8) ?? ""
            let errorMessage = String(data: standardError, encoding: .utf8) ?? ""
            guard process.terminationStatus == 0 else {
                let reason = errorMessage.isEmpty ? message : errorMessage
                throw MuxaIPCError.server(reason.isEmpty ? "Host registration failed" : reason)
            }
            return message
        }.value
    }

    @discardableResult
    func attach(pane: MuxaWatchPane, selectShell: Bool = true) async -> MuxaSession? {
        guard isConnected, !isAttachingPane else { return nil }
        isAttachingPane = true
        attachError = nil
        defer { isAttachingPane = false }
        do {
            let bundled = Bundle.main.bundleURL
                .appendingPathComponent("Contents", isDirectory: true)
                .appendingPathComponent("Helpers", isDirectory: true)
                .appendingPathComponent("muxa")
            let hasBundledCLI = FileManager.default.isExecutableFile(atPath: bundled.path)
            let command = hasBundledCLI ? bundled.path : "/usr/bin/env"
            var arguments = [
                "fleet",
                "attach",
                pane.host.alias,
                try Self.exactPaneAddressJSON(pane.pane),
            ]
            if !hasBundledCLI { arguments.insert("muxa", at: 0) }

            var environment = ProcessInfo.processInfo.environment
            environment["MUXA_SOCKET"] = client.socketPath
            environment["TERM"] = "xterm-256color"
            environment["COLORTERM"] = "truecolor"
            environment["TERM_PROGRAM"] = "Muxa"
            let localDirectory = pane.host.local
                && FileManager.default.fileExists(atPath: pane.pane.currentPath)
                ? pane.pane.currentPath
                : FileManager.default.homeDirectoryForCurrentUser.path
            let session = try await client.spawnShell(
                command: command,
                arguments: arguments,
                cwd: localDirectory,
                name: "\(pane.host.alias) · \(pane.pane.windowName.isEmpty ? pane.pane.paneID : pane.pane.windowName)",
                environment: environment
            )
            await refresh()
            if selectShell {
                select(.shell(session.id))
            }
            return session
        } catch {
            MuxaLog.app.error("pane attach failed: \(error.localizedDescription, privacy: .public)")
            attachError = error.localizedDescription
            return nil
        }
    }

    nonisolated static func exactPaneAddressJSON(_ pane: MuxaPaneInfo) throws -> String {
        let object: [String: Any] = [
            "window": [
                "session": [
                    "host": pane.hostKind,
                    "socket": pane.endpointSocket,
                    "session_id": pane.stableSessionID,
                ],
                "window_id": pane.stableWindowID,
            ],
            "pane_id": pane.paneID,
        ]
        let data = try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
        guard let value = String(data: data, encoding: .utf8) else {
            throw MuxaIPCError.server("Could not encode the exact Fleet pane address")
        }
        return value
    }

    func show(_ mode: MuxaSidebarMode) {
        // Like VS Code's Activity Bar, this changes the visible view
        // container without replacing whichever editor tab is active.
        sidebarMode = mode
    }

    func selectWatchPane(_ id: MuxaWatchPaneIdentity) {
        watchSelection = id
        select(.pane(id))
    }

    func select(_ selection: MuxaSidebarSelection) {
        switch selection {
        case .workBoard: sidebarMode = .work
        case .watch: sidebarMode = .watch
        case .ask: sidebarMode = .watch
        case .work: sidebarMode = .work
        case .agent: sidebarMode = .watch
        case .host: sidebarMode = .hosts
        case .shell: sidebarMode = .shells
        case .pane: sidebarMode = .watch
        }
        sidebarSelection = selection
    }

    /// Focus an already-open editor without coupling it to an Activity Bar
    /// container change.
    func activateEditor(_ selection: MuxaSidebarSelection?) {
        sidebarSelection = selection
        // Fleet pane editors and the global Watch/Ask tools share the
        // execution navigator. Returning to an already-open pane tab must
        // restore its Explorer highlight as well as the editor content.
        switch selection {
        case .pane(let id):
            watchSelection = id
            sidebarMode = .watch
        case .watch, .ask:
            sidebarMode = .watch
        default:
            break
        }
    }

    func terminateSelectedSession() {
        guard isConnected, let selectedSessionID, !isTerminatingSession else { return }
        isTerminatingSession = true
        Task {
            defer { isTerminatingSession = false }
            do {
                try await client.terminateSession(id: selectedSessionID)
                await refresh()
            } catch {
                MuxaLog.app.error(
                    "session termination failed: \(error.localizedDescription, privacy: .public)"
                )
                connectionState = .failed(error.localizedDescription)
            }
        }
    }

    private func reconcileSelection() {
        if let sidebarSelection, isSelectionAvailable(sidebarSelection) { return }

        sidebarMode = .work
        sidebarSelection = .workBoard
    }

    private func reconcileWatchSelection() {
        let panes = executionSnapshot.watchHosts
            .flatMap(\.sessions)
            .flatMap(\.windows)
            .flatMap(\.panes)
        if let watchSelection, panes.contains(where: { $0.id == watchSelection }) {
            return
        }
        watchSelection = panes.first(where: { pane in
            pane.agent.map {
                ["waiting_input", "waiting_choice", "blocked", "error", "failed"]
                    .contains($0.state)
            } ?? false
        })?.id ?? panes.first?.id
    }

    func isSelectionAvailable(_ selection: MuxaSidebarSelection) -> Bool {
        switch selection {
        case .workBoard, .watch, .ask:
            true
        case .work(let key):
            workGroups.contains { $0.identity == key }
        case .agent(let id):
            hostedAgents.contains { $0.id == id }
        case .host(let id):
            fleetHosts.contains { $0.id == id }
        case .shell(let id):
            sessions.contains { $0.id == id && !$0.exited }
        case .pane(let id):
            executionSnapshot.watchPane(id: id) != nil
        }
    }

    private static func agentPriority(_ state: String) -> Int {
        switch state {
        case "waiting_input", "waiting_choice", "error", "failed", "blocked": 0
        case "working", "starting": 1
        case "idle": 2
        default: 3
        }
    }
}
