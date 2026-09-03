import Foundation

struct MuxaWorkGroup: Identifiable, Sendable {
    let identity: MuxaWorkIdentity
    let pipelineRun: MuxaPipelineRun?
    let participants: [MuxaHostedAgent]

    var id: MuxaWorkIdentity { identity }
    var workspaceID: String { identity.workspaceID }
    var workID: String { identity.workID }

    var title: String {
        return workID
    }

    var pipelineLabel: String {
        pipelineRun?.pipeline ?? "Observed Work"
    }

    var cwd: String? {
        if let cwd = pipelineRun?.cwd, !cwd.isEmpty { return cwd }
        if let cwd = participants.lazy.compactMap({ $0.pane?.currentPath }).first(where: { !$0.isEmpty }) {
            return cwd
        }
        return participants.lazy.compactMap(\.agent.cwd).first(where: { !$0.isEmpty })
    }

    var hostAliases: [String] {
        Array(Set(participants.map(\.host.alias))).sorted()
    }

    var attentionCount: Int {
        let agentAttention = participants.lazy.filter {
            ["waiting_input", "waiting_choice", "blocked", "error", "failed"]
                .contains($0.agent.state)
        }.count
        let pipelineAttention = pipelineRun?.aliases.values.lazy.filter {
            $0.status == "blocked" || $0.status == "failed"
        }.count ?? 0
        return max(agentAttention, pipelineAttention)
    }

    var workingCount: Int {
        participants.lazy.filter {
            $0.agent.state == "working" || $0.agent.state == "starting"
        }.count
    }

    var completedCount: Int {
        pipelineRun?.aliases.values.lazy.filter { $0.status == "done" }.count ?? 0
    }

    var totalCount: Int {
        pipelineRun?.desired.count ?? participants.count
    }

    func desiredAgent(for participant: MuxaHostedAgent) -> MuxaDesiredAgent? {
        guard let pipelineRun else { return nil }
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

struct MuxaWatchSessionIdentity: Codable, Hashable, Sendable {
    let hostAlias: String
    let socket: String
    let sessionID: String
}

struct MuxaWatchWindowIdentity: Codable, Hashable, Sendable {
    let hostAlias: String
    let socket: String
    let sessionID: String
    let windowID: String
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

    var identity: MuxaWatchWindowIdentity {
        MuxaWatchWindowIdentity(
            hostAlias: hostAlias,
            socket: socket,
            sessionID: sessionID,
            windowID: windowID
        )
    }
}

struct MuxaWatchSession: Identifiable, Sendable {
    let hostAlias: String
    let socket: String
    let sessionID: String
    let name: String
    let windows: [MuxaWatchWindow]

    var id: String { "\(hostAlias):\(socket):\(sessionID)" }

    var identity: MuxaWatchSessionIdentity {
        MuxaWatchSessionIdentity(
            hostAlias: hostAlias,
            socket: socket,
            sessionID: sessionID
        )
    }
}

struct MuxaWatchHost: Identifiable, Sendable {
    let host: MuxaFleetHost
    let sessions: [MuxaWatchSession]

    var id: String { host.alias }
    var paneCount: Int { sessions.reduce(0) { $0 + $1.windows.reduce(0) { $0 + $1.panes.count } } }
}

struct MuxaOperatorMessage: Identifiable, Hashable, Sendable {
    let host: MuxaFleetHostIdentity
    /// Any exact pane on the host. The Fleet mailbox is console-scoped for
    /// sent requests, so this is a routing endpoint rather than the target.
    let routePane: MuxaPaneInfo
    let request: MuxaCollaborationRequest

    var id: String { "\(host.alias):\(request.id)" }
    var needsReply: Bool { request.expectsReply && request.reply == nil }
    var hasUnreadReply: Bool { request.reply != nil && request.replyReadAt == nil }
    var needsHumanDecision: Bool {
        let terminalStatus = request.reply?.status ?? request.status
        return ["blocked", "declined", "failed"].contains(terminalStatus)
    }

    /// A request whose own status already reached `blocked`, `declined`, or
    /// `failed` without a reply will not be answered by the agent, so the
    /// Inbox must not present it as "waiting". `needsReply` keeps counting it
    /// for the activity badge; this flag drives the Waiting scope, the
    /// Waiting metric, and the row wording.
    var isAwaitingAgentReply: Bool { needsReply && !needsHumanDecision }

    /// Most recent durable change on the conversation: the reply time when a
    /// reply exists, otherwise the time the command was sent. muxad emits
    /// ISO-8601 timestamps, so lexical order is chronological.
    var activityAt: String { request.reply?.at ?? request.createdAt }

    /// Needs Action is a work queue rather than a log: decisions the operator
    /// has not read yet come first, then the most recently changed
    /// conversation, with the stable id as the final tie-breaker.
    static func needsActionOrder(_ lhs: Self, _ rhs: Self) -> Bool {
        if lhs.hasUnreadReply != rhs.hasUnreadReply { return lhs.hasUnreadReply }
        if lhs.activityAt != rhs.activityAt { return lhs.activityAt > rhs.activityAt }
        return lhs.id < rhs.id
    }
}

/// Presentation for per-host operator-mailbox failures. The state itself is
/// the plain `[hostAlias: reason]` dictionary `AppModel.inboxHostFailures`, so
/// the Inbox editor and, later, the sidebar render the same wording.
enum MuxaInboxHostFailureText {
    /// One compact line, e.g. "2 hosts unreachable: jiun-mbp, rtzr".
    static func summary(_ failures: [String: String]) -> String? {
        guard !failures.isEmpty else { return nil }
        let hosts = failures.keys.sorted()
        let noun = hosts.count == 1 ? "host" : "hosts"
        return "\(hosts.count) \(noun) unreachable: \(hosts.joined(separator: ", "))"
    }

    /// One "alias: reason" line per host, sorted by alias.
    static func details(_ failures: [String: String]) -> [String] {
        failures.keys.sorted().map { "\($0): \(failures[$0] ?? "")" }
    }
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
                guard let pane = participant.pane,
                      !assigned.contains(participant.id) else { return false }
                if let stamped = pane.workIdentity {
                    return stamped == run.identity
                }
                guard participant.host.local else { return false }
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
        var observed: [MuxaWorkIdentity: [MuxaHostedAgent]] = [:]
        for participant in visible where !assigned.contains(participant.id) {
            guard let identity = participant.pane?.workIdentity else { continue }
            observed[identity, default: []].append(participant)
        }
        for (identity, participants) in observed {
            assigned.formUnion(participants.map(\.id))
            groups.append(
                MuxaWorkGroup(
                    identity: identity,
                    pipelineRun: nil,
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

}
