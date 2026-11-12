import Foundation

// MARK: - Start one agent

/// State of the New Agent sheet and the ⌥⌘T quick start. Held by `AppModel`
/// as a single property so the flow lives in its own files; views observe
/// this object directly.
@MainActor
final class TendAgentLauncher: ObservableObject {
    @Published var isPresenting = false
    @Published var isStarting = false
    @Published var status: String?
    @Published var error: String?
    /// The focused editor when the sheet opened, so choosing a folder or a
    /// host in the sheet cannot change what "here" means.
    @Published var focus = TendAgentLaunchFocus.empty
    /// `config_launch_read`; nil until read or when tendd cannot serve it.
    @Published var launchSettings: TendLaunchSettings?
    /// Agent CLIs found on this Mac's login PATH; nil until probed.
    @Published var installedPrograms: Set<String>?
    /// An editor to open pinned once its pane appeared. ContentView owns the
    /// tabs, so it performs the open and clears this.
    @Published var pendingEditor: TendSidebarSelection?
    /// A first prompt typed elsewhere (Fleet Home's assign bar) that the
    /// sheet starts with; taken once when the sheet seeds itself.
    @Published var draftPrompt: String?

    let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    var rememberedDefaults: TendAgentLaunchDefaults {
        TendAgentLaunchDefaults.load(from: defaults)
    }

    var recentDirectories: [String] {
        TendRecentAgentDirectories.load(from: defaults)
    }
}

extension AppModel {
    /// Opens the New Agent sheet for the focused editor. `error` carries a
    /// failed quick start over, so the operator sees why and can adjust.
    func presentNewAgent(error: String? = nil, prompt: String? = nil) {
        agentLauncher.draftPrompt = prompt
        agentLauncher.error = error
        agentLauncher.status = nil
        agentLauncher.focus = agentLaunchFocus()
        agentLauncher.isPresenting = true
        Task { [weak self] in
            await self?.loadAgentLaunchSettings()
            await self?.loadInstalledAgentPrograms()
        }
    }

    /// Reads the launch defaults and per-provider options from tendd's
    /// config. A daemon without `config_launch_v1`, or no config file, just
    /// means built-in defaults.
    func loadAgentLaunchSettings() async {
        guard await client.supports(TendIPCClient.configLaunchCapability) else { return }
        do {
            agentLauncher.launchSettings = try await client.makeConfigClient().readLaunch().launch
        } catch {
            TendLog.app.debug(
                "launch settings unavailable: \(error.localizedDescription, privacy: .public)"
            )
        }
    }

    private func loadInstalledAgentPrograms() async {
        guard agentLauncher.installedPrograms == nil else { return }
        let found = await InstalledTools.detect(InstalledTools.agentPrograms)
        agentLauncher.installedPrograms = Set(found.map(\.name))
    }

    /// Hosts an agent can start on: this Mac and control-mode fleet hosts.
    var agentCapableHosts: [TendFleetHost] { workCapableHosts }

    /// Where "here" is for the focused editor: a pane's directory and the
    /// pane itself, or a shell's, Work's, or agent's directory.
    func agentLaunchFocus() -> TendAgentLaunchFocus {
        switch sidebarSelection {
        case .pane(let id):
            guard let pane = executionSnapshot.watchPane(id: id) else { return .empty }
            return .pane(
                pane.pane,
                hostAlias: id.hostAlias,
                isLocalHost: isLocalHost(id.hostAlias),
                agentCwd: pane.agent?.cwd
            )
        case .shell(let id):
            return .directory(sessions.first { $0.id == id }?.cwd)
        case .work(let identity):
            let group = workGroups.first { $0.identity == identity }
            let cwd = group?.pipelineRun?.cwd ?? group?.participants.lazy.compactMap(\.agent.cwd).first
            let host = group?.participants.first?.host
            return .directory(cwd, hostAlias: host.flatMap { $0.local ? nil : $0.alias })
        case .agent(let id):
            guard let hosted = hostedAgents.first(where: { $0.id == id }) else { return .empty }
            if let pane = hosted.pane {
                return .pane(
                    pane,
                    hostAlias: hosted.host.alias,
                    isLocalHost: hosted.host.local,
                    agentCwd: hosted.agent.cwd
                )
            }
            return .directory(hosted.agent.cwd, hostAlias: hosted.host.local ? nil : hosted.host.alias)
        default:
            return .empty
        }
    }

    /// tmux sessions on `host` (nil = this Mac), for the New window picker.
    func agentLaunchSessions(host: String?) -> [TendWatchSession] {
        executionSnapshot.watchHosts
            .first { host == nil ? $0.host.local : $0.host.alias == host }?
            .sessions
            .filter { session in
                session.windows.flatMap(\.panes).contains { $0.pane.hostKind == "tmux" }
            }
            .sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending } ?? []
    }

    /// The environment a native agent's PTY gets: the login shell's PATH
    /// (tendd's own is launchd's minimal one) and a shell tab's terminal
    /// identity.
    func nativeAgentEnvironment() async -> [String: String] {
        let base = ProcessInfo.processInfo.environment
        let shell = base["SHELL"].flatMap { $0.isEmpty ? nil : $0 } ?? "/bin/zsh"
        let appVersion = Bundle.main
            .object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
            ?? "development"
        var environment = TendNativeShellLaunch.terminalEnvironment(
            base: base.filter { TendAgentStartRequest.nativeEnvironmentKeys.contains($0.key) },
            shell: shell,
            appVersion: appVersion
        )
        environment["PATH"] = await InstalledTools.searchDirectories().joined(separator: ":")
        return environment.filter { TendAgentStartRequest.nativeEnvironmentKeys.contains($0.key) }
    }

    /// ⌥⌘T: the default agent in the focused editor's directory, no sheet.
    /// Anything it cannot decide on its own — no default agent, no
    /// directory, a host it may not start on — opens the sheet instead.
    func quickStartAgent() async {
        guard isConnected, !agentLauncher.isStarting else { return }
        if agentLauncher.launchSettings == nil { await loadAgentLaunchSettings() }
        let focus = agentLaunchFocus()
        let remembered = agentLauncher.rememberedDefaults
        let guide = agentLauncher.launchSettings?.legacyGuide
        let hostCapable = focus.hostAlias.map { alias in
            agentCapableHosts.contains { $0.alias == alias }
        } ?? true
        guard let program = remembered.program(guide: guide),
              let cwd = focus.cwd,
              hostCapable else {
            presentNewAgent()
            return
        }
        let sessions = agentLaunchSessions(host: focus.hostAlias)
        let availability = TendAgentPlacementAvailability(
            focus: focus,
            launchHost: focus.hostAlias,
            sessionCount: sessions.count
        )
        let placement = availability.resolve(
            remembered: remembered.placement,
            guidePlacement: guide?.placement
        )
        let request = await defaultAgentStartRequest(
            program: program,
            placement: placement,
            focus: focus,
            windowSession: focus.pane == nil
                ? sessions.first.map { (id: $0.sessionID, socket: $0.socket) }
                : nil,
            cwd: cwd
        )
        // A second ⌥⌘T pressed while this one awaited its settings finds the
        // first launch under way: let that one finish instead of opening the
        // sheet over it with an error.
        guard !agentLauncher.isStarting else { return }
        if let failure = await startAgent(request) {
            presentNewAgent(error: failure)
        }
    }

    /// A no-sheet launch request: the remembered provider options, the
    /// configured split direction, and a native PTY's environment. Shared by
    /// ⌥⌘T and the Welcome guide's first agent, which resolve the placement
    /// themselves.
    func defaultAgentStartRequest(
        program: TendAgentProgram,
        placement: TendAgentPlacementKind,
        focus: TendAgentLaunchFocus,
        windowSession: (id: String, socket: String)?,
        cwd: String
    ) async -> TendAgentStartRequest {
        TendAgentStartRequest.make(
            program: program,
            hostAlias: focus.hostAlias,
            placement: placement,
            focus: focus,
            windowSession: windowSession,
            cwd: cwd,
            options: agentLauncher.rememberedDefaults.options,
            guideDirection: agentLauncher.launchSettings?.legacyGuide.direction,
            environment: placement == .native ? await nativeAgentEnvironment() : [:]
        )
    }

    /// Starts one agent and, in the background, opens its editor pinned once
    /// the snapshot shows it. Returns the error message, or nil on success.
    @discardableResult
    func startAgent(_ request: TendAgentStartRequest) async -> String? {
        guard isConnected else { return String(localized: "Connect to tendd first.") }
        guard !agentLauncher.isStarting else { return String(localized: "An agent is starting") }
        // Panes that exist before the launch can never be the new agent's,
        // even when another tmux server reuses the same `%N` id.
        let existingPanes = Set(
            executionSnapshot.watchHosts
                .flatMap(\.sessions).flatMap(\.windows).flatMap(\.panes)
                .map(\.id)
        )
        agentLauncher.isStarting = true
        isStartingAgent = true
        agentLauncher.error = nil
        agentLauncher.status = String(localized: "Starting \(request.program.rawValue)…")
        defer {
            agentLauncher.isStarting = false
            isStartingAgent = false
        }
        do {
            let result: TendAgentStartResult
            if await client.supports(TendIPCClient.agentStartCapability) {
                result = try await client.makeAgentStartClient().start(request)
            } else if request.hostAlias == nil {
                result = try await Self.startAgentWithBundledCLI(
                    arguments: request.cliArguments(),
                    socketPath: client.socketPath,
                    environment: request.environment
                )
            } else {
                throw TendIPCError.server(String(
                    localized: "The tendd on this Mac is too old to start agents on fleet hosts; update Tend and restart tendd."
                ))
            }
            if request.hostAlias == nil {
                TendRecentAgentDirectories.record(request.cwd, in: agentLauncher.defaults)
            }
            agentLauncher.status = nil
            let hostAlias = request.hostAlias ?? localHostAlias
            Task { [weak self] in
                await self?.openStartedAgent(result, hostAlias: hostAlias, excluding: existingPanes)
            }
            return nil
        } catch {
            TendLog.app.error(
                "agent start failed: \(error.localizedDescription, privacy: .public)"
            )
            let message = Self.agentStartFailureMessage(
                error.localizedDescription,
                hostAlias: request.hostAlias
            )
            agentLauncher.status = nil
            agentLauncher.error = message
            return message
        }
    }

    /// An older tend on a fleet host refuses the argv with its `work`-only
    /// allowlist; say what to do about it instead of quoting that.
    nonisolated static func agentStartFailureMessage(_ message: String, hostAlias: String?) -> String {
        if let hostAlias, message.contains("only `tend work") {
            return String(localized: "Update tend on \(hostAlias) to start agents there from the app.")
        }
        return message
    }

    /// Refreshes until the new agent's pane (or native session) is listed,
    /// then hands it to ContentView to open pinned. Gives up quietly after
    /// about 15 s: the pane still appears on a later refresh.
    private func openStartedAgent(
        _ result: TendAgentStartResult,
        hostAlias: String,
        excluding existingPanes: Set<TendWatchPaneIdentity>
    ) async {
        for delay in TendPendingAgentTab.refreshDelays {
            try? await Task.sleep(for: .seconds(delay))
            await refresh()
            let panes = executionSnapshot.watchHosts
                .flatMap(\.sessions).flatMap(\.windows).flatMap(\.panes)
                .map(\.id)
            if let selection = TendPendingAgentTab.selection(
                for: result,
                hostAlias: hostAlias,
                panes: panes.filter { !existingPanes.contains($0) },
                sessionIDs: Set(sessions.map(\.id))
            ) {
                agentLauncher.pendingEditor = selection
                return
            }
        }
        TendLog.app.info(
            "started agent \(result.pane ?? result.session ?? "?", privacy: .public) did not appear within the wait"
        )
    }

    /// `tend agent start --json` run by the app itself, for a tendd without
    /// `agent_start_v1`. Same socket pinning as the other bundled-CLI paths.
    nonisolated private static func startAgentWithBundledCLI(
        arguments: [String],
        socketPath: String,
        environment extra: [String: String]
    ) async throws -> TendAgentStartResult {
        let executable = bundledTendCLI()
        var environment = TendProviderCredentialStore.augmentPath(ProcessInfo.processInfo.environment)
        environment["TEND_SOCKET"] = socketPath
        environment.merge(extra) { _, new in new }
        let resolvedEnvironment = environment
        let data = try await Task.detached { () throws -> Data in
            let result = try TendCLIProcess.run(
                executable: executable ?? URL(fileURLWithPath: "/usr/bin/env"),
                arguments: executable == nil ? ["tend"] + arguments : arguments,
                environment: resolvedEnvironment
            )
            let standardOutput = result.standardOutput
            let standardError = result.standardError
            guard result.terminationStatus == 0 else {
                let reason = String(decoding: standardError, as: UTF8.self)
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                    .components(separatedBy: .newlines).last ?? ""
                throw TendIPCError.server(
                    reason.isEmpty ? "tend agent start exited with \(result.terminationStatus)" : reason
                )
            }
            return standardOutput
        }.value
        return try JSONDecoder().decode(TendAgentStartResult.self, from: data)
    }
}
