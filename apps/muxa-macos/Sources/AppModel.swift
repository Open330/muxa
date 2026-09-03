import Foundation

enum MuxaSidebarSelection: Codable, Hashable, Sendable {
    case workBoard
    case watch
    case inbox
    case ask
    case work(MuxaWorkIdentity)
    case agent(String)
    case host(String)
    case fleetSession(MuxaWatchSessionIdentity)
    case fleetWindow(MuxaWatchWindowIdentity)
    case shell(String)
    case pane(MuxaWatchPaneIdentity)
}

enum MuxaSidebarMode: String, CaseIterable, Identifiable {
    case work
    case watch
    case inbox
    case shells

    var id: Self { self }

    var title: String {
        switch self {
        case .work: "Work"
        case .watch: "Explore"
        case .inbox: "Inbox"
        case .shells: "Shells"
        }
    }

    var systemImage: String {
        switch self {
        case .work: "square.stack.3d.up"
        case .watch: "sidebar.left"
        case .inbox: "tray.full"
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

private struct MuxaInboxFetch: Sendable {
    let target: MuxaWatchPane
    let mailbox: MuxaCollaborationMailbox?
    let error: String?
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
    @Published private(set) var workGroups: [MuxaWorkGroup] = []
    @Published private(set) var hostedAgents: [MuxaHostedAgent] = []
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
    /// Pipeline the Start Work sheet should preselect when opened from a
    /// pipeline card; nil leaves the route default.
    @Published var workStartPreselectedPipeline: String?
    /// Routes, pipelines, skills, and presets from `muxa work options`.
    @Published private(set) var workOptions: MuxaWorkOptions?
    @Published private(set) var workOptionsError: String?
    @Published private(set) var isLoadingWorkOptions = false
    @Published private(set) var isApplyingWorkPreset = false
    @Published private(set) var isStartingWork = false
    /// The last dry-run result, shown in the sheet so the operator sees the
    /// exact agents and prompts before launching for real.
    @Published private(set) var workStartPlan: MuxaWorkStartResult?
    @Published private(set) var workStartStatus: String?
    @Published private(set) var workStartError: String?
    @Published var isConfirmingDaemonReplacement = false
    @Published private(set) var askEntries: [MuxaAskEntry] = []
    @Published private(set) var askConversations: [MuxaAskConversation] = []
    @Published private(set) var activeAskConversationID: String?
    @Published private(set) var askAgent = "claude"
    @Published private(set) var askEnabled: Bool?
    @Published private(set) var askConfigurationPendingReload = false
    @Published private(set) var isSendingAsk = false
    @Published private(set) var isEnablingAsk = false
    @Published private(set) var askError: String?
    @Published var isPresentingAskSettings = false
    @Published private(set) var askSettingsStatus: String?
    @Published private(set) var askSettingsError: String?
    @Published private(set) var operatorMessages: [MuxaOperatorMessage] = []
    @Published private(set) var mailboxRevisions: [String: UInt64] = [:]
    @Published private(set) var isRefreshingInbox = false
    /// Errors that are not tied to one host's mailbox read: opening a
    /// conversation whose agent ended, or a failed mark-read call.
    @Published private(set) var inboxError: String?
    /// Operator-mailbox reads that failed on their most recent attempt, keyed
    /// by host alias. A host leaves the map as soon as one of its reads
    /// succeeds or it is no longer registered; the messages it delivered
    /// earlier stay in `operatorMessages` the whole time. Kept apart from
    /// `inboxError` so one flaky SSH host cannot hide the rest of the Inbox.
    @Published private(set) var inboxHostFailures: [String: String] = [:]
    @Published var isPresentingHostRegistration = false
    @Published private(set) var isRegisteringHost = false
    @Published private(set) var hostRegistrationError: String?

    let client: MuxaIPCClient
    private let daemon = DaemonManager()
    private var refreshTask: Task<Void, Never>?
    private var fleetRefreshTask: Task<Void, Never>?
    private var inboxEventTask: Task<Void, Never>?
    private var askEventRefreshTask: Task<Void, Never>?
    private var pipelineEventRefreshTask: Task<Void, Never>?
    private var connectionGeneration: UInt64 = 0
    private var refreshInFlight = false
    private var fleetRefreshPending = false
    private var askRefreshPending = false
    private var pipelineRefreshPending = false
    private var lastFleetRefresh = Date.distantPast
    private var pendingInboxHosts = Set<String>()
    private var shellNumber = 1
    private var lastInboxRefresh = Date.distantPast

    var isConnected: Bool {
        connectionState == .connected
    }

    var needsWorkConfiguration: Bool {
        guard let workStartError else { return false }
        return workStartError.contains("no [[route]]")
            || workStartError.contains("no pipeline")
            || workStartError.contains("unknown pipeline")
    }

    var agents: [MuxaAgent] { executionSnapshot.agents }

    /// Compact Inbox wording for `inboxHostFailures`, shared with the sidebar.
    var inboxHostFailureSummary: String? {
        MuxaInboxHostFailureText.summary(inboxHostFailures)
    }

    var fleetHosts: [MuxaFleetHost] { executionSnapshot.hosts }

    var selectedSessionID: String? {
        guard case let .shell(id) = sidebarSelection else { return nil }
        return id
    }

    init(client: MuxaIPCClient = MuxaIPCClient()) {
        self.client = client
    }

    /// Test seam. The app ingests execution snapshots through `refresh`, which
    /// needs a live daemon; tests feed a decoded snapshot directly so inbox
    /// refreshes have hosts to read. Not used by production code.
    func ingestExecutionSnapshotForTesting(_ snapshot: MuxaExecutionSnapshot) {
        executionSnapshot = snapshot
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
        fleetRefreshTask?.cancel()
        inboxEventTask?.cancel()
        askEventRefreshTask?.cancel()
        pipelineEventRefreshTask?.cancel()
        fleetRefreshTask = nil
        inboxEventTask = nil
        askEventRefreshTask = nil
        pipelineEventRefreshTask = nil
        fleetRefreshPending = false
        askRefreshPending = false
        pipelineRefreshPending = false
        pendingInboxHosts.removeAll()
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
                // The Inbox badge and sidebar counts are derived from the
                // operator mailbox. Load it once after connecting so they are
                // correct before the Inbox editor is ever opened; later
                // changes arrive through per-host mailbox revision events.
                Task { [weak self] in await self?.refreshOperatorInbox(force: true) }
                Task { [weak self] in await self?.loadWorkOptions() }
                async let events: Void = runFleetSubscription(ifGeneration: generation)
                async let askEvents: Void = runAskSubscription(ifGeneration: generation)
                async let pipelineEvents: Void = runPipelineSubscription(ifGeneration: generation)
                async let reconciliation: Void = runReconciliation(ifGeneration: generation)
                _ = await (events, askEvents, pipelineEvents, reconciliation)
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
            async let askConversationsRequest = try? client.listAskConversations()
            async let askAgentRequest = try? client.selectedAskAgent()
            async let askStatusRequest = try? client.askStatus()
            let (
                listedSessions,
                listedRuns,
                listedExecution,
                listedAskEntries,
                listedAskConversations,
                selectedAskAgent,
                listedAskStatus
            ) = try await (
                sessionsRequest,
                pipelineRequest,
                executionRequest,
                askEntriesRequest,
                askConversationsRequest,
                askAgentRequest,
                askStatusRequest
            )
            let updated = listedSessions
                .sorted { lhs, rhs in
                    if lhs.exited != rhs.exited { return !lhs.exited }
                    return (lhs.displayName ?? lhs.id) < (rhs.displayName ?? rhs.id)
                }
            if let generation, connectionGeneration != generation { return }
            let sortedRuns = listedRuns.sorted {
                if $0.identity.workspaceID != $1.identity.workspaceID {
                    return $0.identity.workspaceID < $1.identity.workspaceID
                }
                return $0.identity.workID < $1.identity.workID
            }
            let sessionsChanged = sessions != updated
            let runsChanged = pipelineRuns != sortedRuns
            let executionChanged = !executionSnapshot.hasSameSource(as: listedExecution)
            if sessionsChanged { sessions = updated }
            if runsChanged { pipelineRuns = sortedRuns }
            if executionChanged { executionSnapshot = listedExecution }
            if runsChanged || executionChanged { rebuildWorkspaceProjections() }
            if let listedAskEntries, askEntries != listedAskEntries {
                askEntries = listedAskEntries
            }
            if let listedAskConversations {
                if askConversations != listedAskConversations.conversations {
                    askConversations = listedAskConversations.conversations
                }
                if activeAskConversationID != listedAskConversations.active?.id {
                    activeAskConversationID = listedAskConversations.active?.id
                }
            }
            if let selectedAskAgent, askAgent != selectedAskAgent { askAgent = selectedAskAgent }
            if let listedAskStatus {
                if askEnabled != listedAskStatus { askEnabled = listedAskStatus }
                if listedAskStatus { askConfigurationPendingReload = false }
            }
            if sidebarMode == .inbox,
               Date().timeIntervalSince(lastInboxRefresh) >= 60,
               !isRefreshingInbox {
                Task { [weak self] in await self?.refreshOperatorInbox() }
            }
            if sessionsChanged || runsChanged || executionChanged {
                reconcileWatchSelection()
                workspaceRevision &+= 1
                reconcileSelection()
            }
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

    /// Push is the primary freshness path. A compact Fleet event only wakes
    /// one coalesced snapshot fetch; bursts are capped at four publishes per
    /// second and the 15-second reconciliation loop repairs disconnect/lag
    /// gaps without returning to a high-frequency poll.
    private func runFleetSubscription(ifGeneration generation: UInt64) async {
        guard await client.supports(MuxaIPCClient.fleetSubscribeCapability) else { return }
        while !Task.isCancelled, connectionGeneration == generation {
            do {
                let updates = try await client.fleetUpdates()
                for try await update in updates {
                    guard !Task.isCancelled, connectionGeneration == generation else { return }
                    if update.mailboxRevision != nil {
                        if mailboxRevisions[update.host] != update.mailboxRevision {
                            mailboxRevisions[update.host] = update.mailboxRevision
                        }
                        scheduleInboxRefresh(host: update.host, ifGeneration: generation)
                    } else {
                        scheduleFleetRefresh(ifGeneration: generation)
                    }
                }
            } catch is CancellationError {
                return
            } catch {
                guard connectionGeneration == generation else { return }
                MuxaLog.app.warning(
                    "Fleet invalidation stream reconnecting: \(error.localizedDescription, privacy: .public)"
                )
            }
            try? await Task.sleep(for: .seconds(1))
        }
    }

    private func runReconciliation(ifGeneration generation: UInt64) async {
        while !Task.isCancelled, connectionGeneration == generation {
            do {
                try await Task.sleep(for: .seconds(15))
            } catch {
                return
            }
            await refresh(ifGeneration: generation)
        }
    }

    private func runAskSubscription(ifGeneration generation: UInt64) async {
        guard await client.supports(MuxaIPCClient.askSubscribeCapability) else { return }
        while !Task.isCancelled, connectionGeneration == generation {
            do {
                let updates = try await client.askUpdates()
                for try await _ in updates {
                    guard connectionGeneration == generation else { return }
                    scheduleAskRefresh(ifGeneration: generation)
                }
            } catch is CancellationError {
                return
            } catch {
                guard connectionGeneration == generation else { return }
                MuxaLog.app.warning(
                    "Ask invalidation stream reconnecting: \(error.localizedDescription, privacy: .public)"
                )
            }
            try? await Task.sleep(for: .seconds(1))
        }
    }

    private func runPipelineSubscription(ifGeneration generation: UInt64) async {
        guard await client.supports(MuxaIPCClient.pipelineSubscribeCapability) else { return }
        while !Task.isCancelled, connectionGeneration == generation {
            do {
                let updates = try await client.pipelineUpdates()
                for try await _ in updates {
                    guard connectionGeneration == generation else { return }
                    schedulePipelineRefresh(ifGeneration: generation)
                }
            } catch is CancellationError {
                return
            } catch {
                guard connectionGeneration == generation else { return }
                MuxaLog.app.warning(
                    "Pipeline invalidation stream reconnecting: \(error.localizedDescription, privacy: .public)"
                )
            }
            try? await Task.sleep(for: .seconds(1))
        }
    }

    private func scheduleAskRefresh(ifGeneration generation: UInt64) {
        askRefreshPending = true
        guard askEventRefreshTask == nil else { return }
        askEventRefreshTask = Task { [weak self] in
            guard let self else { return }
            defer { askEventRefreshTask = nil }
            while !Task.isCancelled,
                  connectionGeneration == generation,
                  askRefreshPending {
                askRefreshPending = false
                try? await Task.sleep(for: .milliseconds(75))
                await refreshAskState(ifGeneration: generation)
            }
        }
    }

    private func schedulePipelineRefresh(ifGeneration generation: UInt64) {
        pipelineRefreshPending = true
        guard pipelineEventRefreshTask == nil else { return }
        pipelineEventRefreshTask = Task { [weak self] in
            guard let self else { return }
            defer { pipelineEventRefreshTask = nil }
            while !Task.isCancelled,
                  connectionGeneration == generation,
                  pipelineRefreshPending {
                pipelineRefreshPending = false
                try? await Task.sleep(for: .milliseconds(75))
                await refreshPipelineState(ifGeneration: generation)
            }
        }
    }

    private func refreshAskState(ifGeneration generation: UInt64) async {
        do {
            async let entriesRequest = client.listAskEntries()
            async let conversationsRequest = client.listAskConversations()
            let (entries, conversations) = try await (entriesRequest, conversationsRequest)
            guard connectionGeneration == generation else { return }
            if askEntries != entries { askEntries = entries }
            if askConversations != conversations.conversations {
                askConversations = conversations.conversations
            }
            if activeAskConversationID != conversations.active?.id {
                activeAskConversationID = conversations.active?.id
            }
        } catch is CancellationError {
            return
        } catch {
            MuxaLog.app.warning(
                "Ask event refresh failed: \(error.localizedDescription, privacy: .public)"
            )
        }
    }

    private func refreshPipelineState(ifGeneration generation: UInt64) async {
        do {
            let listedRuns = try await client.listPipelineRuns().sorted {
                if $0.identity.workspaceID != $1.identity.workspaceID {
                    return $0.identity.workspaceID < $1.identity.workspaceID
                }
                return $0.identity.workID < $1.identity.workID
            }
            guard connectionGeneration == generation, pipelineRuns != listedRuns else { return }
            pipelineRuns = listedRuns
            rebuildWorkspaceProjections()
            workspaceRevision &+= 1
            reconcileSelection()
        } catch is CancellationError {
            return
        } catch {
            MuxaLog.app.warning(
                "Pipeline event refresh failed: \(error.localizedDescription, privacy: .public)"
            )
        }
    }

    private func scheduleFleetRefresh(ifGeneration generation: UInt64) {
        fleetRefreshPending = true
        guard fleetRefreshTask == nil else { return }
        fleetRefreshTask = Task { [weak self] in
            guard let self else { return }
            defer { fleetRefreshTask = nil }
            try? await Task.sleep(for: .milliseconds(75))
            while !Task.isCancelled,
                  connectionGeneration == generation,
                  fleetRefreshPending {
                fleetRefreshPending = false
                let remaining = 0.25 - Date().timeIntervalSince(lastFleetRefresh)
                if remaining > 0 {
                    try? await Task.sleep(for: .seconds(remaining))
                }
                await refreshFleetState(ifGeneration: generation)
            }
        }
    }

    private func scheduleInboxRefresh(host: String, ifGeneration generation: UInt64) {
        pendingInboxHosts.insert(host)
        guard inboxEventTask == nil else { return }
        inboxEventTask = Task { [weak self] in
            guard let self else { return }
            defer { inboxEventTask = nil }
            try? await Task.sleep(for: .milliseconds(125))
            while !Task.isCancelled,
                  connectionGeneration == generation,
                  !pendingInboxHosts.isEmpty {
                let hosts = pendingInboxHosts
                pendingInboxHosts.removeAll()
                if isRefreshingInbox {
                    pendingInboxHosts.formUnion(hosts)
                    try? await Task.sleep(for: .milliseconds(200))
                    continue
                }
                await refreshOperatorInbox(force: true, hostAliases: hosts)
            }
        }
    }

    private func refreshFleetState(ifGeneration generation: UInt64) async {
        guard connectionGeneration == generation else { return }
        guard !refreshInFlight else {
            fleetRefreshPending = true
            try? await Task.sleep(for: .milliseconds(100))
            return
        }
        refreshInFlight = true
        defer {
            refreshInFlight = false
            lastFleetRefresh = Date()
        }
        do {
            async let sessionsRequest = client.listSessions()
            async let executionRequest = client.executionSnapshot()
            let (listedSessions, listedExecution) = try await (sessionsRequest, executionRequest)
            guard connectionGeneration == generation else { return }
            let sortedSessions = listedSessions.sorted { lhs, rhs in
                if lhs.exited != rhs.exited { return !lhs.exited }
                return (lhs.displayName ?? lhs.id) < (rhs.displayName ?? rhs.id)
            }
            let sessionsChanged = sessions != sortedSessions
            let executionChanged = !executionSnapshot.hasSameSource(as: listedExecution)
            if sessionsChanged { sessions = sortedSessions }
            if executionChanged {
                executionSnapshot = listedExecution
                rebuildWorkspaceProjections()
            }
            if sessionsChanged || executionChanged {
                reconcileWatchSelection()
                workspaceRevision &+= 1
                reconcileSelection()
            }
            connectionState = .connected
        } catch is CancellationError {
            return
        } catch {
            MuxaLog.app.warning(
                "Fleet event refresh failed: \(error.localizedDescription, privacy: .public)"
            )
        }
    }

    private func rebuildWorkspaceProjections() {
        hostedAgents = executionSnapshot.hostedAgents
            .filter { $0.agent.state != "stopped" }
            .sorted { left, right in
                let leftPriority = Self.agentPriority(left.agent.state)
                let rightPriority = Self.agentPriority(right.agent.state)
                if leftPriority != rightPriority { return leftPriority < rightPriority }
                if left.host.local != right.host.local { return left.host.local }
                if left.host.alias != right.host.alias { return left.host.alias < right.host.alias }
                return left.agent.agentSessionID < right.agent.agentSessionID
            }
        workGroups = executionSnapshot.workGroups(pipelineRuns: pipelineRuns)
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

    func presentWorkStart(pipeline: String? = nil) {
        workStartError = nil
        workStartStatus = nil
        workStartPlan = nil
        workStartPreselectedPipeline = pipeline
        isPresentingWorkStart = true
        Task { [weak self] in await self?.loadWorkOptions() }
    }

    /// Reads routes, pipelines, message skills, and presets through the
    /// bundled CLI so the Start Work form and the Command Center can offer
    /// real choices. The config file is the source of truth, so this is a
    /// fresh read every time rather than daemon state.
    func loadWorkOptions() async {
        guard !isLoadingWorkOptions else { return }
        isLoadingWorkOptions = true
        defer { isLoadingWorkOptions = false }
        do {
            let output = try await Self.runBundledMuxa(
                arguments: ["work", "options", "--json"],
                socketPath: client.socketPath
            )
            let decoded = try MuxaWorkOptions.decode(Data(output.utf8))
            if workOptions != decoded { workOptions = decoded }
            workOptionsError = nil
        } catch {
            MuxaLog.app.warning(
                "work options unavailable: \(error.localizedDescription, privacy: .public)"
            )
            workOptionsError = error.localizedDescription
        }
    }

    /// Writes one of muxa's built-in pipeline presets into the config through
    /// the canonical CLI (`muxa work preset apply`). A catch-all route is
    /// added only when the config has no route yet, so an existing routing
    /// table is never reordered from the app.
    func applyWorkPreset(_ name: String) async -> Bool {
        guard !isApplyingWorkPreset else { return false }
        isApplyingWorkPreset = true
        defer { isApplyingWorkPreset = false }
        var arguments = ["work", "preset", "apply", name, "--json"]
        if workOptions?.routes.isEmpty ?? true {
            arguments += ["--route", ".*"]
        }
        do {
            _ = try await Self.runBundledMuxa(arguments: arguments, socketPath: client.socketPath)
            workOptionsError = nil
            workStartError = nil
            await loadWorkOptions()
            return true
        } catch {
            MuxaLog.app.error(
                "work preset apply failed: \(error.localizedDescription, privacy: .public)"
            )
            workOptionsError = error.localizedDescription
            return false
        }
    }

    func startWork(_ request: MuxaWorkStartRequest) async -> Bool {
        guard isConnected, !isStartingWork else { return false }
        isStartingWork = true
        workStartError = nil
        workStartPlan = nil
        workStartStatus = request.dryRun ? "Building the Work plan…" : "Submitting Work to muxad…"
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
            if request.dryRun || operation.result?.dryRun == true {
                // A plan is something to read, not a reason to close the
                // sheet: keep it open with the steps muxad would take.
                workStartPlan = operation.result
                workStartStatus = operation.message
                return false
            }
            if let result = operation.result {
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
                let snapshot = try await client.listAskConversations()
                askConversations = snapshot.conversations
                activeAskConversationID = snapshot.active?.id
            }
            let provider = MuxaAskProvider(rawValue: agent)
            let apiKey = provider.flatMap { MuxaProviderCredentialStore.key(for: $0) }
            let entry = try await client.sendAsk(prompt, agent: agent, apiKey: apiKey)
            askEntries.removeAll { $0.id == entry.id }
            askEntries.insert(entry, at: 0)
            if let snapshot = try? await client.listAskConversations() {
                askConversations = snapshot.conversations
                activeAskConversationID = snapshot.active?.id ?? entry.conversationID
            } else if let conversationID = entry.conversationID {
                activeAskConversationID = conversationID
            }
            return true
        } catch {
            askError = error.localizedDescription
            if error.localizedDescription.localizedCaseInsensitiveContains("ask is disabled") {
                askEnabled = false
            }
            return false
        }
    }

    func enableAsk() async {
        guard !isEnablingAsk else { return }
        isEnablingAsk = true
        askError = nil
        defer { isEnablingAsk = false }
        do {
            if !askConfigurationPendingReload {
                _ = try await Self.runBundledMuxa(
                    arguments: [
                        "init",
                        "--component", "ask",
                        "--yes",
                        "--start-daemon=false",
                    ],
                    socketPath: client.socketPath
                )
                askConfigurationPendingReload = true
            }
            askEnabled = false
            askSettingsStatus = "Global Ask is enabled in config. Reloading muxad applies the grant."
            if sessions.contains(where: { !$0.exited }) {
                isConfirmingDaemonReplacement = true
            } else {
                replaceRunningDaemon()
            }
        } catch {
            askError = error.localizedDescription
        }
    }

    func selectAskAgent(_ agent: String) async {
        askError = nil
        do {
            askAgent = try await client.selectAskAgent(agent)
            let snapshot = try await client.listAskConversations()
            askConversations = snapshot.conversations
            activeAskConversationID = snapshot.active?.id
        } catch {
            askError = error.localizedDescription
        }
    }

    func selectAskConversation(_ conversationID: String) async {
        guard activeAskConversationID != conversationID else { return }
        askError = nil
        do {
            let conversation = try await client.selectAskConversation(conversationID)
            askAgent = conversation.agent
            activeAskConversationID = conversation.id
            let snapshot = try await client.listAskConversations()
            askConversations = snapshot.conversations
        } catch {
            askError = error.localizedDescription
        }
    }

    func resetAskConversation() async {
        askError = nil
        do {
            if let conversation = try await client.resetAskConversation() {
                askConversations.removeAll { $0.id == conversation.id }
                askConversations.insert(conversation, at: 0)
                activeAskConversationID = conversation.id
            } else {
                activeAskConversationID = nil
            }
        } catch {
            askError = error.localizedDescription
        }
    }

    func presentAskSettings() {
        askSettingsError = nil
        askSettingsStatus = nil
        isPresentingAskSettings = true
    }

    func saveProviderKey(_ key: String, provider: MuxaAskProvider) -> Bool {
        askSettingsError = nil
        do {
            try MuxaProviderCredentialStore.save(key, for: provider)
            askSettingsStatus = "Saved \(provider.title) key in the login Keychain. It will be passed only to the next matching Ask process."
            return true
        } catch {
            askSettingsError = error.localizedDescription
            return false
        }
    }

    func removeProviderKey(_ provider: MuxaAskProvider) {
        askSettingsError = nil
        do {
            try MuxaProviderCredentialStore.remove(for: provider)
            askSettingsStatus = "Removed the \(provider.title) API key. Future Ask processes will use CLI sign-in or their inherited environment."
        } catch {
            askSettingsError = error.localizedDescription
        }
    }

    func requestDaemonRestartForProviderSettings() {
        isPresentingAskSettings = false
        isConfirmingDaemonReplacement = true
    }

    func openProviderCLI(_ provider: MuxaAskProvider) async {
        askSettingsError = nil
        guard let executable = MuxaExecutableResolver.executablePath(provider.executable) else {
            askSettingsError = "\(provider.title) CLI was not found in ~/.local/bin, Homebrew, or PATH."
            return
        }
        do {
            let arguments = provider == .codex ? ["login"] : []
            let session = try await client.spawnShell(
                command: executable,
                arguments: arguments,
                cwd: FileManager.default.homeDirectoryForCurrentUser.path,
                name: provider == .codex ? "Codex Login" : "Claude Code Login",
                environment: MuxaProviderCredentialStore.environment(
                    ProcessInfo.processInfo.environment,
                    for: provider
                )
            )
            await refresh()
            isPresentingAskSettings = false
            select(.shell(session.id))
        } catch {
            askSettingsError = error.localizedDescription
        }
    }

    func refreshOperatorInbox(force: Bool = false) async {
        await refreshOperatorInbox(force: force, hostAliases: nil)
    }

    /// Reads the console mailbox of every reachable host, or only of
    /// `hostAliases` when given. Hosts are independent: a host whose read
    /// fails keeps the messages it delivered earlier and is recorded in
    /// `inboxHostFailures` until a later read of that same host succeeds.
    /// Only a full refresh prunes hosts, and only hosts that are no longer
    /// registered at all; a registered host that is merely offline or timing
    /// out keeps its history so a transient failure never empties its part of
    /// the Inbox. Internal (not private) so the contract can be unit-tested.
    func refreshOperatorInbox(
        force: Bool,
        hostAliases: Set<String>?
    ) async {
        guard !isRefreshingInbox else { return }
        if !force, Date().timeIntervalSince(lastInboxRefresh) < 4 { return }
        isRefreshingInbox = true
        if hostAliases == nil { lastInboxRefresh = Date() }
        inboxError = nil
        defer { isRefreshingInbox = false }

        let allTargets = executionSnapshot.watchHosts.compactMap { host -> MuxaWatchPane? in
            host.sessions.flatMap(\.windows).flatMap(\.panes).first
        }
        let targets = hostAliases.map { aliases in
            allTargets.filter { aliases.contains($0.host.alias) }
        } ?? allTargets
        var messagesByHost = Dictionary(grouping: operatorMessages) { $0.host.alias }
        var failures = inboxHostFailures
        if hostAliases == nil {
            // An empty host list means the fleet snapshot itself is missing
            // (the local host is always registered), so keep everything
            // rather than treating that as "every host was unregistered".
            let registered = Set(executionSnapshot.hosts.map(\.alias))
            if !registered.isEmpty {
                messagesByHost = messagesByHost.filter { registered.contains($0.key) }
                failures = failures.filter { registered.contains($0.key) }
            }
        }

        let results = await withTaskGroup(of: MuxaInboxFetch.self) { group in
            for target in targets {
                group.addTask { [client] in
                    do {
                        let mailbox = try await client.collaborationMailbox(
                            host: target.host,
                            pane: target.pane
                        )
                        return MuxaInboxFetch(target: target, mailbox: mailbox, error: nil)
                    } catch {
                        return MuxaInboxFetch(
                            target: target,
                            mailbox: nil,
                            error: error.localizedDescription
                        )
                    }
                }
            }
            var fetched: [MuxaInboxFetch] = []
            for await result in group { fetched.append(result) }
            return fetched
        }

        for result in results {
            let target = result.target
            if let mailbox = result.mailbox {
                var seen = Set<String>()
                messagesByHost[target.host.alias] = mailbox.sent.compactMap { request in
                    guard seen.insert(request.id).inserted else { return nil }
                    return MuxaOperatorMessage(
                        host: target.host,
                        routePane: target.pane,
                        request: request
                    )
                }
                failures[target.host.alias] = nil
            } else if let error = result.error {
                // Leave messagesByHost[alias] untouched: the last successful
                // read stays visible while the host is unreachable.
                failures[target.host.alias] = error
            }
        }

        let updatedMessages = messagesByHost.values
            .flatMap { $0 }
            .sorted { lhs, rhs in
                if lhs.request.createdAt != rhs.request.createdAt {
                    return lhs.request.createdAt > rhs.request.createdAt
                }
                return lhs.id < rhs.id
            }
        if operatorMessages != updatedMessages { operatorMessages = updatedMessages }
        if inboxHostFailures != failures { inboxHostFailures = failures }
    }

    func openOperatorMessage(_ message: MuxaOperatorMessage) {
        guard let selection = Self.operatorSelection(
            for: message,
            in: executionSnapshot
        ) else {
            inboxError = "\(message.request.to.label) is no longer present on \(message.host.alias). The conversation is still available, but its live agent cannot be opened."
            return
        }
        inboxError = nil
        if case .pane(let pane) = selection {
            selectWatchPane(pane)
        } else {
            select(selection)
        }
    }

    /// Collaboration requests retain the agent's stable session identity even
    /// when tmux reuses or moves its pane. Prefer that identity, then use the
    /// room alias and pane address only as progressively weaker live fallbacks.
    /// If the agent really ended, keep the historical conversation actionable
    /// by opening its surviving window, session, or host instead of failing.
    static func operatorSelection(
        for message: MuxaOperatorMessage,
        in snapshot: MuxaExecutionSnapshot
    ) -> MuxaSidebarSelection? {
        let participant = message.request.to
        let agents = snapshot.hostedAgents.filter {
            $0.host.alias == message.host.alias && $0.agent.state != "stopped"
        }

        if let exactAgent = agents.first(where: {
            $0.agent.agentSessionID == participant.agentSessionID
        }) {
            return .agent(exactAgent.id)
        }

        let participantSocket = participant.socket ?? participant.room.socket
        func matchesRoom(_ pane: MuxaPaneInfo) -> Bool {
            pane.stableWindowID == participant.room.windowID
                && (participantSocket == nil || pane.endpointSocket == participantSocket)
        }

        if let alias = participant.alias, !alias.isEmpty {
            let aliasMatches = agents.filter { agent in
                guard let pane = agent.pane else { return false }
                return pane.agentAlias == alias && matchesRoom(pane)
            }
            if aliasMatches.count == 1, let aliasAgent = aliasMatches.first {
                return .agent(aliasAgent.id)
            }
        }

        let panes = snapshot.watchHosts
            .first { $0.host.alias == message.host.alias }?
            .sessions.flatMap(\.windows).flatMap(\.panes) ?? []
        if let exactPane = panes.first(where: { pane in
            pane.pane.paneID == participant.pane
                && (participantSocket == nil || pane.pane.endpointSocket == participantSocket)
        }) {
            return .pane(exactPane.id)
        }

        let roomPanes = panes.filter { matchesRoom($0.pane) }
        if roomPanes.count == 1, let roomPane = roomPanes.first {
            return .pane(roomPane.id)
        }

        let host = snapshot.watchHosts.first { $0.host.alias == message.host.alias }
        let windows = host?.sessions.flatMap(\.windows) ?? []
        let roomWindows = windows.filter { window in
            window.windowID == participant.room.windowID
                && (participantSocket == nil || window.socket == participantSocket)
        }
        if roomWindows.count == 1, let roomWindow = roomWindows.first {
            return .fleetWindow(roomWindow.identity)
        }

        if let sessionID = participant.sessionID, !sessionID.isEmpty {
            let sessions = host?.sessions.filter { session in
                session.sessionID == sessionID
                    && (participantSocket == nil || session.socket == participantSocket)
            } ?? []
            if sessions.count == 1, let session = sessions.first {
                return .fleetSession(session.identity)
            }
        }

        if host != nil {
            return .host(message.host.alias)
        }
        return nil
    }

    /// Marks a reply read through the durable collaboration get operation.
    /// muxad stamps `reply_read_at` on the returned request, so it replaces
    /// the message in place immediately (a refresh that is already in flight
    /// would otherwise make the "New Reply" badge linger). The follow-up
    /// refresh is limited to the message's own host instead of re-reading
    /// every mailbox in the fleet.
    func markOperatorMessageRead(_ message: MuxaOperatorMessage) async {
        do {
            let updated = try await client.collaborationRequest(
                host: message.host,
                pane: message.routePane,
                requestID: message.request.id
            )
            if let index = operatorMessages.firstIndex(where: { $0.id == message.id }) {
                operatorMessages[index] = MuxaOperatorMessage(
                    host: message.host,
                    routePane: message.routePane,
                    request: updated
                )
            }
            await refreshOperatorInbox(force: true, hostAliases: [message.host.alias])
        } catch {
            inboxError = error.localizedDescription
        }
    }

    func presentHostRegistration() {
        prepareHostRegistration()
        isPresentingHostRegistration = true
    }

    func prepareHostRegistration() {
        hostRegistrationError = nil
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
                throw MuxaIPCError.server(
                    reason.isEmpty ? "muxa \(arguments.prefix(2).joined(separator: " ")) failed" : reason
                )
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
                "--fit",
                pane.host.alias,
                try Self.exactPaneAddressJSON(pane.pane),
            ]
            if !hasBundledCLI { arguments.insert("muxa", at: 0) }

            var environment = ProcessInfo.processInfo.environment
            environment["MUXA_SOCKET"] = client.socketPath
            environment["TERM"] = "xterm-256color"
            environment["COLORTERM"] = "truecolor"
            environment["TERM_PROGRAM"] = "Muxa"
            // The app owns a fresh PTY. Inheriting a development terminal's
            // tmux markers would make `muxa fleet attach` switch that other
            // client instead of attaching inside this Live Pane.
            environment.removeValue(forKey: "TMUX")
            environment.removeValue(forKey: "TMUX_PANE")
            let localDirectory = pane.host.local
                && FileManager.default.fileExists(atPath: pane.pane.currentPath)
                ? pane.pane.currentPath
                : FileManager.default.homeDirectoryForCurrentUser.path
            let session = try await client.spawnShell(
                command: command,
                arguments: arguments,
                cwd: localDirectory,
                name: "\(pane.host.alias) · \(pane.pane.windowName.isEmpty ? pane.pane.paneID : pane.pane.windowName)",
                environment: environment,
                columns: 160,
                rows: 48
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
            throw MuxaIPCError.server("Could not encode the exact pane address")
        }
        return value
    }

    func show(_ mode: MuxaSidebarMode) {
        // Like VS Code's Activity Bar, this changes the visible view
        // container without replacing whichever editor tab is active.
        sidebarMode = mode
        if mode == .inbox, isConnected {
            Task { [weak self] in await self?.refreshOperatorInbox() }
        }
    }

    func selectWatchPane(_ id: MuxaWatchPaneIdentity) {
        watchSelection = id
        select(.pane(id))
    }

    func selectWatchSession(_ id: MuxaWatchSessionIdentity) {
        select(.fleetSession(id))
    }

    func selectWatchWindow(_ id: MuxaWatchWindowIdentity) {
        select(.fleetWindow(id))
    }

    func select(_ selection: MuxaSidebarSelection) {
        switch selection {
        case .workBoard: sidebarMode = .work
        case .watch: sidebarMode = .watch
        case .inbox: sidebarMode = .inbox
        case .ask: sidebarMode = .inbox
        case .work: sidebarMode = .work
        case .agent: sidebarMode = .inbox
        case .host: sidebarMode = .watch
        case .fleetSession: sidebarMode = .watch
        case .fleetWindow: sidebarMode = .watch
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
        case .watch, .fleetSession, .fleetWindow:
            sidebarMode = .watch
        case .inbox, .ask, .agent:
            sidebarMode = .inbox
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
        case .workBoard, .watch, .inbox, .ask:
            true
        case .work(let key):
            workGroups.contains { $0.identity == key }
        case .agent(let id):
            hostedAgents.contains { $0.id == id }
        case .host(let id):
            fleetHosts.contains { $0.id == id }
        case .fleetSession(let id):
            executionSnapshot.watchSession(id: id) != nil
        case .fleetWindow(let id):
            executionSnapshot.watchWindow(id: id) != nil
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
