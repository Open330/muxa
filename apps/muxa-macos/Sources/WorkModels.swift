import Foundation

struct MuxaWorkGroup: Identifiable, Sendable {
    let identity: MuxaWorkIdentity
    let pipelineRun: MuxaPipelineRun
    let participants: [MuxaHostedAgent]

    var id: MuxaWorkIdentity { identity }
    var workspaceID: String { identity.workspaceID }
    var workID: String { identity.workID }

    var title: String {
        return workID
    }

    var hostAliases: [String] {
        Array(Set(participants.map(\.host.alias))).sorted()
    }

    var attentionCount: Int {
        let agentAttention = participants.lazy.filter {
            ["waiting_input", "waiting_choice", "blocked", "error", "failed"]
                .contains($0.agent.state)
        }.count
        let pipelineAttention = pipelineRun.aliases.values.lazy.filter {
            $0.status == "blocked" || $0.status == "failed"
        }.count
        return max(agentAttention, pipelineAttention)
    }

    var workingCount: Int {
        participants.lazy.filter {
            $0.agent.state == "working" || $0.agent.state == "starting"
        }.count
    }

    var completedCount: Int {
        pipelineRun.aliases.values.lazy.filter { $0.status == "done" }.count
    }

    var totalCount: Int {
        pipelineRun.desired.count
    }

    func desiredAgent(for participant: MuxaHostedAgent) -> MuxaDesiredAgent? {
        if let alias = participant.pane?.agentAlias,
           let desired = pipelineRun.desired.first(where: { $0.alias == alias }) {
            return desired
        }
        guard let paneID = participant.pane?.paneID else { return nil }
        let alias = pipelineRun.aliases.values.first(where: { $0.pane == paneID })?.alias
        return alias.flatMap { matched in
            pipelineRun.desired.first(where: { $0.alias == matched })
        }
    }
}

struct MuxaWatchPaneIdentity: Codable, Hashable, Sendable {
    let hostAlias: String
    let socket: String
    let paneID: String
}

enum MuxaModuleRoute: Codable, Hashable, Sendable {
    case shell(String)
    case fleetPane(MuxaWatchPaneIdentity)
}

struct MuxaWatchPane: Identifiable, Sendable {
    let host: MuxaFleetHostIdentity
    let pane: MuxaPaneInfo
    let agent: MuxaAgent?

    var id: MuxaWatchPaneIdentity {
        MuxaWatchPaneIdentity(
            hostAlias: host.alias,
            socket: pane.endpointSocket,
            paneID: pane.paneID
        )
    }
}

struct MuxaWatchWindow: Identifiable, Sendable {
    let hostAlias: String
    let socket: String
    let sessionID: String
    let windowID: String
    let name: String
    let index: String
    let panes: [MuxaWatchPane]

    var id: String { "\(hostAlias):\(socket):\(sessionID):\(windowID)" }
}

struct MuxaWatchSession: Identifiable, Sendable {
    let hostAlias: String
    let socket: String
    let sessionID: String
    let name: String
    let windows: [MuxaWatchWindow]

    var id: String { "\(hostAlias):\(socket):\(sessionID)" }
}

struct MuxaWatchHost: Identifiable, Sendable {
    let host: MuxaFleetHost
    let sessions: [MuxaWatchSession]

    var id: String { host.alias }
    var paneCount: Int { sessions.reduce(0) { $0 + $1.windows.reduce(0) { $0 + $1.panes.count } } }
}

extension MuxaExecutionSnapshot {
    func workGroups(pipelineRuns: [MuxaPipelineRun]) -> [MuxaWorkGroup] {
        let visible = hostedAgents.filter { $0.agent.state != "stopped" }
        let localWindowEndpoints = hosts.reduce(into: [String: Set<String>]()) { endpoints, host in
            guard host.local, let panes = host.remote?.panes else { return }
            for pane in panes {
                endpoints[pane.windowID, default: []].insert(pane.endpointSocket)
            }
        }
        var assigned = Set<String>()
        var groups: [MuxaWorkGroup] = []

        for run in pipelineRuns {
            let participants = visible.filter { participant in
                guard participant.host.local,
                      let pane = participant.pane,
                      !assigned.contains(participant.id) else { return false }
                if let windowID = run.windowID {
                    guard pane.windowID == windowID,
                          localWindowEndpoints[windowID]?.count == 1 else { return false }
                    return true
                }
                return run.aliases.values.contains { $0.pane == pane.paneID }
            }
            assigned.formUnion(participants.map(\.id))
            groups.append(
                MuxaWorkGroup(
                    identity: run.identity,
                    pipelineRun: run,
                    participants: participants.sorted(by: Self.participantComesBefore)
                )
            )
        }

        return groups.sorted { left, right in
            if (left.attentionCount > 0) != (right.attentionCount > 0) {
                return left.attentionCount > 0
            }
            if (left.workingCount > 0) != (right.workingCount > 0) {
                return left.workingCount > 0
            }
            if left.workspaceID != right.workspaceID {
                return left.workspaceID.localizedStandardCompare(right.workspaceID) == .orderedAscending
            }
            return left.title.localizedStandardCompare(right.title) == .orderedAscending
        }
    }

    private static func participantComesBefore(
        _ left: MuxaHostedAgent,
        _ right: MuxaHostedAgent
    ) -> Bool {
        let leftPriority = participantPriority(left.agent.state)
        let rightPriority = participantPriority(right.agent.state)
        if leftPriority != rightPriority { return leftPriority < rightPriority }
        if left.host.local != right.host.local { return left.host.local }
        if left.host.alias != right.host.alias { return left.host.alias < right.host.alias }
        return left.agent.id < right.agent.id
    }

    private static func participantPriority(_ state: String) -> Int {
        switch state {
        case "waiting_input", "waiting_choice", "error", "failed", "blocked": 0
        case "working", "starting": 1
        case "idle": 2
        default: 3
        }
    }

    var watchHosts: [MuxaWatchHost] {
        hosts.map { host in
            let panes = host.remote?.panes ?? []
            let agents = host.remote?.agents ?? []
            let groupedSessions = Dictionary(grouping: panes) { pane in
                "\(pane.endpointSocket)\u{0}\(pane.stableSessionID)"
            }
            let sessions = groupedSessions.values.map { sessionPanes -> MuxaWatchSession in
                let first = sessionPanes[0]
                let groupedWindows = Dictionary(grouping: sessionPanes, by: \.stableWindowID)
                let windows = groupedWindows.values.map { windowPanes -> MuxaWatchWindow in
                    let firstWindow = windowPanes[0]
                    let nodes = windowPanes.map { pane in
                        MuxaWatchPane(
                            host: host.identity,
                            pane: pane,
                            agent: Self.watchAgent(for: pane, among: agents)
                        )
                    }.sorted { Self.paneIndex($0.pane.paneIndex) < Self.paneIndex($1.pane.paneIndex) }
                    return MuxaWatchWindow(
                        hostAlias: host.alias,
                        socket: firstWindow.endpointSocket,
                        sessionID: firstWindow.stableSessionID,
                        windowID: firstWindow.stableWindowID,
                        name: firstWindow.windowName,
                        index: firstWindow.windowIndex,
                        panes: nodes
                    )
                }.sorted { left, right in
                    let leftIndex = Self.paneIndex(left.index)
                    let rightIndex = Self.paneIndex(right.index)
                    return leftIndex == rightIndex
                        ? left.name.localizedStandardCompare(right.name) == .orderedAscending
                        : leftIndex < rightIndex
                }
                return MuxaWatchSession(
                    hostAlias: host.alias,
                    socket: first.endpointSocket,
                    sessionID: first.stableSessionID,
                    name: first.session,
                    windows: windows
                )
            }.sorted { left, right in
                left.name.localizedStandardCompare(right.name) == .orderedAscending
            }
            return MuxaWatchHost(host: host, sessions: sessions)
        }.sorted { left, right in
            if left.host.local != right.host.local { return left.host.local }
            return left.host.alias.localizedStandardCompare(right.host.alias) == .orderedAscending
        }
    }

    func watchPane(id: MuxaWatchPaneIdentity) -> MuxaWatchPane? {
        watchHosts
            .flatMap(\.sessions)
            .flatMap(\.windows)
            .flatMap(\.panes)
            .first { $0.id == id }
    }

    private static func watchAgent(for pane: MuxaPaneInfo, among agents: [MuxaAgent]) -> MuxaAgent? {
        let candidates = agents.filter { agent in
            guard agent.pane == pane.paneID else { return false }
            return agent.tmuxSocket == nil || agent.tmuxSocket == pane.socket
        }
        return candidates.count == 1 ? candidates[0] : nil
    }

    private static func paneIndex(_ value: String) -> Int {
        Int(value) ?? Int.max
    }
}
