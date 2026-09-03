import Darwin
import Dispatch
import Foundation

enum MuxaIPCError: LocalizedError {
    case invalidSocketPath(String)
    case posix(operation: String, code: Int32)
    case responseTooLarge
    case emptyResponse
    case server(String)
    case incompatibleProtocol(minimum: UInt32?, maximum: UInt32?)
    case missingField(String)
    case invalidBase64

    var errorDescription: String? {
        switch self {
        case .invalidSocketPath(let path):
            "The muxad socket path is too long: \(path)"
        case .posix(let operation, let code):
            "\(operation) failed: \(String(cString: strerror(code)))"
        case .responseTooLarge:
            "muxad returned an oversized IPC response"
        case .emptyResponse:
            "muxad closed the IPC connection without a response"
        case .server(let message):
            message
        case .incompatibleProtocol(let minimum, let maximum):
            "Incompatible muxad protocol (server supports \(minimum.map(String.init) ?? "?")…\(maximum.map(String.init) ?? "?"), app requires \(MuxaIPCClient.protocolVersion))"
        case .missingField(let name):
            "muxad response is missing \(name)"
        case .invalidBase64:
            "muxad returned invalid byte-safe terminal data"
        }
    }
}

struct MuxaSession: Codable, Hashable, Identifiable, Sendable {
    enum Backend: String, Codable, Sendable {
        case tmux
        case zellij
        case pty
    }

    let id: String
    let backend: Backend
    let displayName: String?
    let cwd: String?
    let attachedClients: Int
    let hasBeenAttached: Bool?
    let exited: Bool
    let exitStatus: Int32?
    let pid: UInt32?

    enum CodingKeys: String, CodingKey {
        case id, backend, cwd, exited, pid
        case displayName = "display_name"
        case attachedClients = "attached_clients"
        case hasBeenAttached = "has_been_attached"
        case exitStatus = "exit_status"
    }
}

struct MuxaSessionOutput: Decodable, Sendable {
    let sessionID: String
    let offset: UInt64
    let nextOffset: UInt64
    let data: String
    let dataBase64: String?
    let truncated: Bool
    let exited: Bool
    let exitStatus: Int32?

    enum CodingKeys: String, CodingKey {
        case offset, data, truncated, exited
        case sessionID = "session_id"
        case nextOffset = "next_offset"
        case dataBase64 = "data_base64"
        case exitStatus = "exit_status"
    }

    var bytes: Data? {
        if let dataBase64 {
            return Data(base64Encoded: dataBase64)
        }
        return data.data(using: .utf8)
    }
}

struct MuxaWorkIdentity: Codable, Hashable, Identifiable, Sendable {
    let workspaceID: String
    let workID: String

    var id: String { "\(workspaceID)/\(workID)" }

    enum CodingKeys: String, CodingKey {
        case workspaceID = "workspace_id"
        case workID = "work_id"
    }
}

struct MuxaWorkStartRequest: Hashable, Sendable {
    let work: String
    let workspace: String?
    let pipeline: String?
    let cwd: String?
    let external: String?
    let skill: String?
    let body: String?
    let context: String?
    let dryRun: Bool
}

/// One step of `muxa work up`'s reconciliation plan: `launch` a missing
/// pane, `reprompt` or `keep` a live one, `waiting` on an `after` edge, or
/// `attention` when a person has to act first.
struct MuxaWorkPlanStep: Decodable, Equatable, Sendable, Identifiable {
    let action: String
    let alias: String
    let program: String?
    let role: String?
    let task: String?
    let prompt: String?
    let pane: String?
    let state: String?
    let waitingOn: [String]

    var id: String { "\(action):\(alias)" }

    enum CodingKeys: String, CodingKey {
        case action, alias, program, role, task, prompt, pane, state
        case waitingOn = "waiting_on"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        action = try values.decodeIfPresent(String.self, forKey: .action) ?? "launch"
        alias = try values.decodeIfPresent(String.self, forKey: .alias) ?? ""
        program = try values.decodeIfPresent(String.self, forKey: .program)
        role = try values.decodeIfPresent(String.self, forKey: .role)
        task = try values.decodeIfPresent(String.self, forKey: .task)
        prompt = try values.decodeIfPresent(String.self, forKey: .prompt)
        pane = try values.decodeIfPresent(String.self, forKey: .pane)
        state = try values.decodeIfPresent(String.self, forKey: .state)
        waitingOn = try values.decodeIfPresent([String].self, forKey: .waitingOn) ?? []
    }
}

struct MuxaWorkPlan: Decodable, Equatable, Sendable {
    let steps: [MuxaWorkPlanStep]
}

struct MuxaWorkStartResult: Decodable, Equatable, Sendable {
    let work: String
    let workspace: String
    let pipeline: String?
    let cwd: String?
    let dryRun: Bool?
    let layout: String?
    let plan: MuxaWorkPlan?

    enum CodingKeys: String, CodingKey {
        case work, workspace, pipeline, cwd, layout, plan
        case dryRun = "dry_run"
    }
}

struct MuxaWorkOperation: Decodable, Sendable {
    enum State: String, Decodable, Sendable {
        case running
        case succeeded
        case failed
    }

    let operationID: String
    let state: State
    let work: String
    let workspace: String?
    let message: String
    let result: MuxaWorkStartResult?

    enum CodingKeys: String, CodingKey {
        case state, work, workspace, message, result
        case operationID = "operation_id"
    }
}

struct MuxaPipelineAliasState: Decodable, Hashable, Sendable {
    let alias: String
    let status: String
    let generation: UInt64
    let pane: String?
    let error: String?
}

struct MuxaDesiredAgent: Decodable, Hashable, Sendable {
    let alias: String
    let program: String
    let role: String?
    let task: String?
    let after: [String]?
}

struct MuxaPipelineRun: Decodable, Hashable, Identifiable, Sendable {
    let identity: MuxaWorkIdentity
    let pipeline: String
    let desired: [MuxaDesiredAgent]
    let cwd: String
    let generation: UInt64
    let windowID: String?
    let aliases: [String: MuxaPipelineAliasState]

    var id: MuxaWorkIdentity { identity }

    enum CodingKeys: String, CodingKey {
        case identity, pipeline, desired, cwd, generation, aliases
        case windowID = "window_id"
    }
}

struct MuxaAgent: Decodable, Hashable, Identifiable, Sendable {
    let kind: String
    let agentSessionID: String
    let pane: String?
    let tmuxSocket: String?
    let tmuxSession: String?
    let cwd: String?
    let state: String
    let lastPrompt: String?
    let lastPromptAt: String?
    let lastResponse: String?
    let recap: String?
    let aiTitle: String?
    let lastNotification: String?
    let model: String?
    let contextUsedPercent: Double?
    let costUSD: Double?
    let startedAt: String?
    let lastActivityAt: String?
    let stateEnteredAt: String?
    let workload: MuxaAgentWorkload?
    let subagents: [MuxaAgentSubagent]?

    var id: String { agentSessionID }

    enum CodingKeys: String, CodingKey {
        case kind, pane, cwd, state, recap, model
        case agentSessionID = "agent_session_id"
        case tmuxSocket = "tmux_socket"
        case tmuxSession = "tmux_session"
        case lastPrompt = "last_prompt"
        case lastPromptAt = "last_prompt_at"
        case lastResponse = "last_response"
        case aiTitle = "ai_title"
        case lastNotification = "last_notification"
        case contextUsedPercent = "context_used_pct"
        case costUSD = "cost_usd"
        case startedAt = "started_at"
        case lastActivityAt = "last_activity_at"
        case stateEnteredAt = "state_entered_at"
        case workload, subagents
    }
}

struct MuxaAgentWorkload: Decodable, Hashable, Sendable {
    let processCount: Int
    let shellCount: Int
    let subagentCount: Int
    let helperCount: Int
    let preview: [MuxaAgentProcess]

    enum CodingKeys: String, CodingKey {
        case preview
        case processCount = "process_count"
        case shellCount = "shell_count"
        case subagentCount = "subagent_count"
        case helperCount = "helper_count"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        processCount = try values.decodeIfPresent(Int.self, forKey: .processCount) ?? 0
        shellCount = try values.decodeIfPresent(Int.self, forKey: .shellCount) ?? 0
        subagentCount = try values.decodeIfPresent(Int.self, forKey: .subagentCount) ?? 0
        helperCount = try values.decodeIfPresent(Int.self, forKey: .helperCount) ?? 0
        preview = try values.decodeIfPresent([MuxaAgentProcess].self, forKey: .preview) ?? []
    }
}

struct MuxaAgentProcess: Decodable, Hashable, Identifiable, Sendable {
    let pid: UInt32
    let parentPID: UInt32
    let depth: UInt8
    let kind: String
    let command: String

    var id: UInt32 { pid }

    enum CodingKeys: String, CodingKey {
        case pid, depth, kind, command
        case parentPID = "parent_pid"
    }
}

struct MuxaAgentSubagent: Decodable, Hashable, Identifiable, Sendable {
    let kind: String
    let description: String?
    let startedAt: String

    var id: String { "\(kind):\(startedAt):\(description ?? "")" }

    enum CodingKeys: String, CodingKey {
        case kind, description
        case startedAt = "started_at"
    }
}

struct MuxaPaneInfo: Decodable, Hashable, Sendable {
    let paneID: String
    let sessionID: String
    let session: String
    let windowID: String
    let windowName: String
    let windowIndex: String
    let paneIndex: String
    let currentCommand: String
    let title: String
    let currentPath: String
    let socket: String?
    let workspaceID: String?
    let workID: String?
    let agentRole: String?
    let agentAlias: String?

    enum CodingKeys: String, CodingKey {
        case session, title, socket
        case paneID = "pane_id"
        case sessionID = "session_id"
        case windowID = "window_id"
        case windowName = "window_name"
        case windowIndex = "window_index"
        case paneIndex = "pane_index"
        case currentCommand = "current_command"
        case currentPath = "current_path"
        case workspaceID = "workspace_id"
        case workID = "work_id"
        case agentRole = "agent_role"
        case agentAlias = "agent_alias"
    }

    var workIdentity: MuxaWorkIdentity? {
        guard let workspaceID, !workspaceID.isEmpty,
              let workID, !workID.isEmpty else { return nil }
        return MuxaWorkIdentity(workspaceID: workspaceID, workID: workID)
    }

    var hostKind: String {
        if paneID.hasPrefix("rmux:") { return "rmux" }
        if paneID.hasPrefix("herdr:") { return "herdr" }
        if paneID.hasPrefix("zellij:") { return "zellij" }
        if paneID.hasPrefix("cmux:") { return "cmux" }
        return "tmux"
    }

    var stableSessionID: String { sessionID.isEmpty ? session : sessionID }
    var stableWindowID: String { windowID.isEmpty ? windowIndex : windowID }

    var endpointSocket: String {
        if let socket, !socket.isEmpty { return socket }
        switch hostKind {
        case "cmux": return "cmux"
        case "herdr": return "herdr"
        case "zellij": return "zellij"
        default: return "default"
        }
    }
}

struct MuxaExecutionSnapshot: Sendable {
    let hosts: [MuxaFleetHost]
    let agents: [MuxaAgent]
    let panes: [MuxaPaneInfo]
    let hostedAgents: [MuxaHostedAgent]
    let watchHosts: [MuxaWatchHost]

    private let panesByAgentSessionID: [String: [MuxaPaneInfo]]
    private let agentsByPaneID: [String: [MuxaAgent]]
    private let watchPanesByID: [MuxaWatchPaneIdentity: MuxaWatchPane]
    private let watchSessionsByID: [MuxaWatchSessionIdentity: MuxaWatchSession]
    private let watchWindowsByID: [MuxaWatchWindowIdentity: MuxaWatchWindow]
    private let watchWindowsByPaneID: [MuxaWatchPaneIdentity: MuxaWatchWindow]

    static let empty = MuxaExecutionSnapshot(hosts: [])

    init(hosts: [MuxaFleetHost]) {
        self.hosts = hosts
        agents = hosts.flatMap { $0.remote?.agents ?? [] }
        panes = hosts.flatMap { $0.remote?.panes ?? [] }

        var hosted: [MuxaHostedAgent] = []
        var panesByAgent: [String: [MuxaPaneInfo]] = [:]
        var agentsByPane: [String: [MuxaAgent]] = [:]
        var builtWatchHosts: [MuxaWatchHost] = []
        var paneIndex: [MuxaWatchPaneIdentity: MuxaWatchPane] = [:]
        var sessionIndex: [MuxaWatchSessionIdentity: MuxaWatchSession] = [:]
        var windowIndex: [MuxaWatchWindowIdentity: MuxaWatchWindow] = [:]
        var windowByPaneIndex: [MuxaWatchPaneIdentity: MuxaWatchWindow] = [:]

        for host in hosts {
            let hostPanes = host.remote?.panes ?? []
            let hostAgents = host.remote?.agents ?? []
            let hostAgentsByPane = Dictionary(grouping: hostAgents.compactMap { agent in
                agent.pane.map { ($0, agent) }
            }, by: { $0.0 }).mapValues { $0.map(\.1) }

            for agent in hostAgents {
                let pane = Self.pane(for: agent, among: hostPanes)
                hosted.append(MuxaHostedAgent(host: host.identity, agent: agent, pane: pane))
                if let pane {
                    panesByAgent[agent.agentSessionID, default: []].append(pane)
                }
                if let paneID = agent.pane {
                    agentsByPane[paneID, default: []].append(agent)
                }
            }

            let groupedSessions = Dictionary(grouping: hostPanes) { pane in
                "\(pane.endpointSocket)\u{0}\(pane.stableSessionID)"
            }
            let sessions = groupedSessions.values.map { sessionPanes -> MuxaWatchSession in
                let first = sessionPanes[0]
                let groupedWindows = Dictionary(grouping: sessionPanes, by: \.stableWindowID)
                let windows = groupedWindows.values.map { windowPanes -> MuxaWatchWindow in
                    let firstWindow = windowPanes[0]
                    let nodes = windowPanes.map { pane -> MuxaWatchPane in
                        let candidates = (hostAgentsByPane[pane.paneID] ?? []).filter {
                            $0.tmuxSocket == nil || $0.tmuxSocket == pane.socket
                        }
                        return MuxaWatchPane(
                            host: host.identity,
                            pane: pane,
                            agent: candidates.count == 1 ? candidates[0] : nil
                        )
                    }.sorted {
                        Self.numericIndex($0.pane.paneIndex) < Self.numericIndex($1.pane.paneIndex)
                    }
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
                    let leftIndex = Self.numericIndex(left.index)
                    let rightIndex = Self.numericIndex(right.index)
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
            }.sorted {
                $0.name.localizedStandardCompare($1.name) == .orderedAscending
            }
            builtWatchHosts.append(MuxaWatchHost(host: host, sessions: sessions))
        }

        builtWatchHosts.sort { left, right in
            if left.host.local != right.host.local { return left.host.local }
            return left.host.alias.localizedStandardCompare(right.host.alias) == .orderedAscending
        }
        for host in builtWatchHosts {
            for session in host.sessions {
                sessionIndex[session.identity] = session
                for window in session.windows {
                    windowIndex[window.identity] = window
                    for pane in window.panes {
                        paneIndex[pane.id] = pane
                        windowByPaneIndex[pane.id] = window
                    }
                }
            }
        }

        hostedAgents = hosted
        watchHosts = builtWatchHosts
        panesByAgentSessionID = panesByAgent
        agentsByPaneID = agentsByPane
        watchPanesByID = paneIndex
        watchSessionsByID = sessionIndex
        watchWindowsByID = windowIndex
        watchWindowsByPaneID = windowByPaneIndex
    }

    func pane(for agent: MuxaAgent) -> MuxaPaneInfo? {
        let matches = panesByAgentSessionID[agent.agentSessionID] ?? []
        return matches.count == 1 ? matches[0] : nil
    }

    private static func pane(for agent: MuxaAgent, among panes: [MuxaPaneInfo]) -> MuxaPaneInfo? {
        guard let paneID = agent.pane else { return nil }
        if let socket = agent.tmuxSocket,
           let exact = panes.first(where: { $0.paneID == paneID && $0.socket == socket }) {
            return exact
        }
        let candidates = panes.filter { $0.paneID == paneID }
        return candidates.count == 1 ? candidates[0] : nil
    }

    func agent(for pane: MuxaPaneInfo) -> MuxaAgent? {
        let exact = (agentsByPaneID[pane.paneID] ?? []).filter {
            $0.pane == pane.paneID && ($0.tmuxSocket == nil || $0.tmuxSocket == pane.socket)
        }
        return exact.count == 1 ? exact[0] : nil
    }

    func watchPane(id: MuxaWatchPaneIdentity) -> MuxaWatchPane? {
        watchPanesByID[id]
    }

    func watchSession(id: MuxaWatchSessionIdentity) -> MuxaWatchSession? {
        watchSessionsByID[id]
    }

    func watchWindow(containing id: MuxaWatchPaneIdentity) -> MuxaWatchWindow? {
        watchWindowsByPaneID[id]
    }

    func watchWindow(id: MuxaWatchWindowIdentity) -> MuxaWatchWindow? {
        watchWindowsByID[id]
    }

    func hasSameSource(as other: MuxaExecutionSnapshot) -> Bool {
        hosts == other.hosts
    }

    private static func numericIndex(_ value: String) -> Int {
        Int(value) ?? Int.max
    }
}

struct MuxaFleetHostIdentity: Hashable, Sendable {
    let alias: String
    let local: Bool
    let state: String
    let mode: String
}

struct MuxaHostedAgent: Identifiable, Sendable {
    let host: MuxaFleetHostIdentity
    let agent: MuxaAgent
    let pane: MuxaPaneInfo?

    var id: String { "\(host.alias):\(agent.id)" }
}

struct MuxaFleetSnapshot: Decodable, Sendable {
    let hosts: [MuxaFleetHost]
}

struct MuxaFleetUpdate: Decodable, Sendable {
    let host: String
    let state: String
    let revision: UInt64?
    let resync: Bool?
    let mailboxRevision: UInt64?

    enum CodingKeys: String, CodingKey {
        case host, state, revision, resync
        case mailboxRevision = "mailbox_revision"
    }
}

struct MuxaFleetHost: Decodable, Hashable, Identifiable, Sendable {
    let alias: String
    let local: Bool
    let sshTarget: String?
    let mode: String
    let state: String
    let latencyMS: UInt64?
    let error: String?
    let muxaVersion: String?
    let daemonGeneration: UInt64?
    let labels: [String: String]?
    let annotations: [String: String]?
    let remote: MuxaRemoteSnapshot?

    var id: String { alias }
    var identity: MuxaFleetHostIdentity {
        MuxaFleetHostIdentity(alias: alias, local: local, state: state, mode: mode)
    }

    enum CodingKeys: String, CodingKey {
        case alias, local, mode, state, error, labels, annotations, remote
        case sshTarget = "ssh_target"
        case latencyMS = "latency_ms"
        case muxaVersion = "muxa_version"
        case daemonGeneration = "daemon_generation"
    }
}

struct MuxaRemoteSnapshot: Decodable, Hashable, Sendable {
    let agents: [MuxaAgent]
    let panes: [MuxaPaneInfo]
}

struct MuxaPaneCapture: Sendable {
    let screenText: String?
    let rawBytes: Data?
}

struct MuxaAskEntry: Decodable, Hashable, Identifiable, Sendable {
    let id: String
    let conversationID: String?
    let prompt: String
    let answer: String
    let status: String
    let agent: String
    let agentSessionID: String?
    let cwd: String
    let askedAt: String
    let answeredAt: String?
    let costUSD: Double?
    let error: String?

    enum CodingKeys: String, CodingKey {
        case id, prompt, answer, status, agent, cwd, error
        case conversationID = "conversation_id"
        case agentSessionID = "agent_session_id"
        case askedAt = "asked_at"
        case answeredAt = "answered_at"
        case costUSD = "cost_usd"
    }
}

struct MuxaAskConversation: Decodable, Hashable, Identifiable, Sendable {
    let id: String
    let title: String
    let agent: String
    let agentSessionID: String?
    let createdAt: String
    let updatedAt: String

    enum CodingKeys: String, CodingKey {
        case id, title, agent
        case agentSessionID = "agent_session_id"
        case createdAt = "created_at"
        case updatedAt = "updated_at"
    }
}

struct MuxaAskConversationSnapshot: Sendable {
    let conversations: [MuxaAskConversation]
    let active: MuxaAskConversation?
}

struct MuxaCollaborationRoom: Decodable, Hashable, Sendable {
    let host: String
    let socket: String?
    let windowID: String

    enum CodingKeys: String, CodingKey {
        case host, socket
        case windowID = "window_id"
    }
}

struct MuxaCollaborationParticipant: Decodable, Hashable, Sendable {
    let agentKind: String
    let agentSessionID: String
    let pane: String
    let socket: String?
    let room: MuxaCollaborationRoom
    let alias: String?
    let roles: [String]?
    let console: Bool?
    let sessionID: String?
    let sessionName: String?
    let windowName: String?

    enum CodingKeys: String, CodingKey {
        case pane, socket, room, alias, roles, console
        case agentKind = "agent_kind"
        case agentSessionID = "agent_session_id"
        case sessionID = "tmux_session_id"
        case sessionName = "tmux_session_name"
        case windowName = "window_name"
    }

    var label: String {
        if console == true { return "operator" }
        if let alias, !alias.isEmpty { return "@\(alias)" }
        return "\(agentKind)@\(pane)"
    }
}

struct MuxaCollaborationReply: Decodable, Hashable, Sendable {
    let status: String
    let body: String
    let at: String
}

struct MuxaCollaborationRequest: Decodable, Identifiable, Hashable, Sendable {
    let id: String
    let from: MuxaCollaborationParticipant
    let to: MuxaCollaborationParticipant
    let kind: String
    let body: String
    let expectsReply: Bool
    let workMode: String
    let status: String
    let createdAt: String
    let reply: MuxaCollaborationReply?
    let threadID: String?
    let parentRequestID: String?
    let workspaceID: String?
    let workID: String?
    let runID: String?
    let claimedAt: String?
    let replyReadAt: String?

    enum CodingKeys: String, CodingKey {
        case id, from, to, kind, body, status, reply
        case expectsReply = "expects_reply"
        case workMode = "work_mode"
        case createdAt = "created_at"
        case threadID = "thread_id"
        case parentRequestID = "parent_request_id"
        case workspaceID = "workspace_id"
        case workID = "work_id"
        case runID = "run_id"
        case claimedAt = "claimed_at"
        case replyReadAt = "reply_read_at"
    }
}

struct MuxaCollaborationMailbox: Sendable {
    let incoming: [MuxaCollaborationRequest]
    let sent: [MuxaCollaborationRequest]
}

private struct MuxaFleetCommandResult: Decodable, Sendable {
    let accepted: Bool
    let message: String?
    let capture: String?
    let captureRawBase64: String?
    let collaborationRequest: MuxaCollaborationRequest?
    let collaborationIncoming: [MuxaCollaborationRequest]?
    let collaborationSent: [MuxaCollaborationRequest]?

    enum CodingKeys: String, CodingKey {
        case accepted, message, capture
        case captureRawBase64 = "capture_raw_base64"
        case collaborationRequest = "collaboration_request"
        case collaborationIncoming = "collaboration_incoming"
        case collaborationSent = "collaboration_sent"
    }
}

private struct MuxaIPCResponse: Decodable, Sendable {
    let ok: Bool
    let error: String?
    let minProtocol: UInt32?
    let maxProtocol: UInt32?
    let capabilities: [String]?
    let agents: [MuxaAgent]?
    let sessions: [MuxaSession]?
    let session: MuxaSession?
    let output: MuxaSessionOutput?
    let capture: String?
    let fleet: MuxaFleetSnapshot?
    let fleetResult: MuxaFleetCommandResult?
    let pipelineRuns: [MuxaPipelineRun]?
    let workOperation: MuxaWorkOperation?
    let askEntries: [MuxaAskEntry]?
    let askEntry: MuxaAskEntry?
    let askConversations: [MuxaAskConversation]?
    let askConversation: MuxaAskConversation?
    let askAgent: String?
    let askEnabled: Bool?

    enum CodingKeys: String, CodingKey {
        case ok, error, capabilities, agents, sessions, session, output, capture, fleet
        case minProtocol = "min_protocol"
        case maxProtocol = "max_protocol"
        case fleetResult = "fleet_result"
        case pipelineRuns = "pipeline_runs"
        case workOperation = "work_operation"
        case askEntries = "ask_entries"
        case askEntry = "ask_entry"
        case askConversations = "ask_conversations"
        case askConversation = "ask_conversation"
        case askAgent = "ask_agent"
        case askEnabled = "ask_enabled"
    }
}

private struct MuxaRevisionUpdate: Decodable, Sendable {
    let revision: UInt64
}

typealias MuxaIPCRequestHandler = @Sendable (String, Data) throws -> Data
fileprivate typealias MuxaIPCTimedRequestHandler = @Sendable (
    String,
    Data,
    TimeInterval
) throws -> Data

/// Runs the blocking Unix-socket exchange on one serial queue.
///
/// Actor methods are reentrant while awaiting detached work. The previous
/// implementation therefore allowed every key, resize, refresh, and polling
/// request to create another blocked worker when muxad was slow or restarting.
/// Keeping exactly one blocking exchange active bounds thread and descriptor
/// use while preserving wire order.
final class SerializedIPCTransport: @unchecked Sendable {
    private let queue: DispatchQueue
    private let handler: MuxaIPCTimedRequestHandler

    init(
        label: String = "dev.muxa.mac.ipc-transport",
        handler: @escaping MuxaIPCRequestHandler
    ) {
        queue = DispatchQueue(label: label, qos: .userInitiated)
        self.handler = { path, payload, _ in try handler(path, payload) }
    }

    fileprivate init(label: String, timedHandler: @escaping MuxaIPCTimedRequestHandler) {
        queue = DispatchQueue(label: label, qos: .userInitiated)
        handler = timedHandler
    }

    func request(
        path: String,
        payload: Data,
        timeout: TimeInterval = 3
    ) async throws -> Data {
        try Task.checkCancellation()
        let handler = self.handler
        return try await withCheckedThrowingContinuation { continuation in
            queue.async {
                do {
                    continuation.resume(returning: try handler(path, payload, timeout))
                } catch {
                    continuation.resume(throwing: error)
                }
            }
        }
    }
}

actor MuxaIPCClient {
    static let protocolVersion: UInt32 = 6
    static let byteSafeCapability = "session_bytes_v1"
    static let attachmentIdentityCapability = "session_attachment_identity_v1"
    static let workControlCapability = "work_control_v1"
    static let askCredentialCapability = "ask_one_turn_credential_v1"
    static let askConversationCapability = "ask_conversations_v1"
    static let fleetSubscribeCapability = "fleet_subscribe"
    static let sessionWaitCapability = "session_wait_v1"
    static let askSubscribeCapability = "ask_subscribe"
    static let pipelineSubscribeCapability = "pipeline_subscribe"

    let socketPath: String
    private(set) var capabilities: Set<String> = []
    private let transport: SerializedIPCTransport
    private let timedRequestHandler: MuxaIPCTimedRequestHandler
    private var terminalReadTransports: [String: SerializedIPCTransport] = [:]
    private let fleetReadTransports: [SerializedIPCTransport]
    private let attachmentClientID: String

    init(socketPath: String = MuxaIPCClient.defaultSocketPath()) {
        self.socketPath = socketPath
        timedRequestHandler = UnixSocket.request
        transport = SerializedIPCTransport(
            label: "dev.muxa.mac.ipc-control",
            timedHandler: UnixSocket.request
        )
        fleetReadTransports = (0..<4).map { index in
            SerializedIPCTransport(
                label: "dev.muxa.mac.ipc-fleet-read-\(index)",
                timedHandler: UnixSocket.request
            )
        }
        attachmentClientID = "muxa-macos:\(getuid())"
    }

    init(socketPath: String, request: @escaping MuxaIPCRequestHandler) {
        self.socketPath = socketPath
        timedRequestHandler = { path, payload, _ in try request(path, payload) }
        transport = SerializedIPCTransport(label: "dev.muxa.mac.ipc-control-test", handler: request)
        fleetReadTransports = (0..<4).map { index in
            SerializedIPCTransport(
                label: "dev.muxa.mac.ipc-fleet-read-test-\(index)",
                handler: request
            )
        }
        attachmentClientID = "muxa-macos-test:\(getpid())"
    }

    static func defaultSocketPath(environment: [String: String] = ProcessInfo.processInfo.environment) -> String {
        if let configured = environment["MUXA_SOCKET"], !configured.isEmpty {
            return configured
        }
        return "/tmp/muxa-\(getuid()).sock"
    }

    func hello() async throws {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "hello",
            "client": "muxa-macos",
        ])
        guard let minimum = response.minProtocol,
              let maximum = response.maxProtocol,
              (minimum...maximum).contains(Self.protocolVersion) else {
            throw MuxaIPCError.incompatibleProtocol(
                minimum: response.minProtocol,
                maximum: response.maxProtocol
            )
        }
        capabilities = Set(response.capabilities ?? [])
        try requireNativeSessionCapabilities()
        guard capabilities.contains(Self.workControlCapability) else {
            throw MuxaIPCError.server(
                "muxad does not support native Work control; update muxa or choose Use Bundled muxad"
            )
        }
    }

    func listSessions() async throws -> [MuxaSession] {
        try requireNativeSessionCapabilities()
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "list_sessions",
        ])
        return response.sessions ?? []
    }

    func listPipelineRuns() async throws -> [MuxaPipelineRun] {
        guard capabilities.contains("pipeline_runs_v1") else { return [] }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "pipeline_runs",
        ])
        return response.pipelineRuns ?? []
    }

    func startWork(_ request: MuxaWorkStartRequest) async throws -> MuxaWorkOperation {
        guard capabilities.contains(Self.workControlCapability) else {
            throw MuxaIPCError.server(
                "muxad does not support native Work control; update muxa and restart muxad"
            )
        }
        var workRequest: [String: Any] = [
            "work": request.work,
            "no_ticket": request.external?.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty != false,
            "dry_run": request.dryRun,
        ]
        Self.put(request.workspace, key: "workspace", in: &workRequest)
        Self.put(request.pipeline, key: "pipeline", in: &workRequest)
        Self.put(request.cwd, key: "cwd", in: &workRequest)
        Self.put(request.external, key: "external", in: &workRequest)
        Self.put(request.skill, key: "skill", in: &workRequest)
        Self.put(request.body, key: "body", in: &workRequest)
        Self.put(request.context, key: "context", in: &workRequest)
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "work_up",
            "request": workRequest,
        ])
        guard let operation = response.workOperation else {
            throw MuxaIPCError.missingField("work_operation")
        }
        return operation
    }

    func workOperation(id: String) async throws -> MuxaWorkOperation {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "work_up_status",
            "operation_id": id,
        ])
        guard let operation = response.workOperation else {
            throw MuxaIPCError.missingField("work_operation")
        }
        return operation
    }

    func executionSnapshot() async throws -> MuxaExecutionSnapshot {
        if capabilities.contains("fleet_v1") {
            let response = try await call([
                "protocol": Self.protocolVersion,
                "kind": "fleet_snapshot",
            ])
            if let fleet = response.fleet {
                return MuxaExecutionSnapshot(hosts: fleet.hosts)
            }
        }

        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "snapshot",
        ])
        let local = MuxaFleetHost(
            alias: "local",
            local: true,
            sshTarget: "local://",
            mode: "control",
            state: "online",
            latencyMS: nil,
            error: nil,
            muxaVersion: nil,
            daemonGeneration: nil,
            labels: nil,
            annotations: nil,
            remote: MuxaRemoteSnapshot(agents: response.agents ?? [], panes: [])
        )
        return MuxaExecutionSnapshot(hosts: [local])
    }

    func captureFleetPane(host: MuxaFleetHostIdentity, pane: MuxaPaneInfo) async throws -> MuxaPaneCapture {
        if capabilities.contains("fleet_v1") {
            let response = try await call(
                [
                    "protocol": Self.protocolVersion,
                    "kind": "fleet_command",
                    "host": host.alias,
                    "operation": [
                        "kind": "capture",
                        "pane": Self.paneKey(pane),
                    ],
                ],
                using: fleetReadTransport(for: host.alias)
            )
            guard let result = response.fleetResult else {
                throw MuxaIPCError.missingField("fleet_result")
            }
            guard result.accepted else {
                throw MuxaIPCError.server(result.message ?? "Pane capture was rejected")
            }
            let rawBytes = result.captureRawBase64.flatMap { Data(base64Encoded: $0) }
            if result.captureRawBase64 != nil, rawBytes == nil {
                throw MuxaIPCError.invalidBase64
            }
            return MuxaPaneCapture(screenText: result.capture, rawBytes: rawBytes)
        }

        guard host.local else {
            throw MuxaIPCError.server("Remote pane capture requires muxad multi-host support (fleet_v1)")
        }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "capture",
            "pane": pane.paneID,
        ])
        return MuxaPaneCapture(
            screenText: response.capture,
            rawBytes: response.capture.map { Data($0.utf8) }
        )
    }

    func sendFleetPrompt(
        host: MuxaFleetHostIdentity,
        pane: MuxaPaneInfo,
        text: String,
        submit: Bool = true
    ) async throws {
        guard capabilities.contains("fleet_v1") else {
            throw MuxaIPCError.server("Prompt control requires muxad multi-host support (fleet_v1)")
        }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "fleet_command",
            "host": host.alias,
            "operation": [
                "kind": "send_prompt",
                "pane": Self.paneKey(pane),
                "text": text,
                "submit": submit,
            ],
        ])
        guard let result = response.fleetResult else {
            throw MuxaIPCError.missingField("fleet_result")
        }
        guard result.accepted else {
            throw MuxaIPCError.server(result.message ?? "Prompt was rejected")
        }
    }

    func listAskEntries() async throws -> [MuxaAskEntry] {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_list",
        ])
        return response.askEntries ?? []
    }

    func listAskConversations() async throws -> MuxaAskConversationSnapshot {
        guard capabilities.contains(Self.askConversationCapability) else {
            return MuxaAskConversationSnapshot(conversations: [], active: nil)
        }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_conversation_list",
        ])
        return MuxaAskConversationSnapshot(
            conversations: response.askConversations ?? [],
            active: response.askConversation
        )
    }

    func selectAskConversation(_ conversationID: String) async throws -> MuxaAskConversation {
        guard capabilities.contains(Self.askConversationCapability) else {
            throw MuxaIPCError.server(
                "The running muxad does not support resumable Ask conversations; update muxa or choose Use Bundled muxad"
            )
        }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_conversation_select",
            "conversation_id": conversationID,
        ])
        guard let conversation = response.askConversation else {
            throw MuxaIPCError.missingField("ask_conversation")
        }
        return conversation
    }

    func askStatus() async throws -> Bool {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_status",
        ])
        guard let enabled = response.askEnabled else {
            throw MuxaIPCError.missingField("ask_enabled")
        }
        return enabled
    }

    func selectedAskAgent() async throws -> String {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_agent",
        ])
        return response.askAgent ?? "claude"
    }

    func selectAskAgent(_ agent: String) async throws -> String {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_agent",
            "agent": agent,
        ])
        return response.askAgent ?? agent
    }

    func sendAsk(
        _ prompt: String,
        agent: String? = nil,
        apiKey: String? = nil
    ) async throws -> MuxaAskEntry {
        var request: [String: Any] = [
            "protocol": Self.protocolVersion,
            "kind": "ask_send",
            "prompt": prompt,
        ]
        if let agent, let apiKey, !apiKey.isEmpty {
            guard capabilities.contains(Self.askCredentialCapability) else {
                throw MuxaIPCError.server(
                    "The running muxad is too old for Keychain API keys; choose Use Bundled muxad and retry"
                )
            }
            request["credential"] = ["agent": agent, "api_key": apiKey]
        }
        let response = try await call(request)
        guard let entry = response.askEntry else {
            throw MuxaIPCError.missingField("ask_entry")
        }
        return entry
    }

    func resetAskConversation() async throws -> MuxaAskConversation? {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_reset",
        ])
        return response.askConversation
    }

    func sendCollaboration(
        host: MuxaFleetHostIdentity,
        pane: MuxaPaneInfo,
        kind: String,
        body: String,
        workMode: String
    ) async throws -> MuxaCollaborationRequest {
        let expectsReply = kind != "notice"
        let result = try await fleetCommand(
            host: host,
            operation: [
                "kind": "collaboration_send",
                "pane": Self.paneKey(pane),
                "request": [
                    "kind": kind,
                    "body": body,
                    "expects_reply": expectsReply,
                    "work_mode": workMode,
                    "paths": [],
                    "air_artifacts": [],
                ],
            ]
        )
        guard let request = result.collaborationRequest else {
            throw MuxaIPCError.missingField("fleet_result.collaboration_request")
        }
        return request
    }

    func collaborationMailbox(
        host: MuxaFleetHostIdentity,
        pane: MuxaPaneInfo
    ) async throws -> MuxaCollaborationMailbox {
        let result = try await fleetCommand(
            host: host,
            operation: [
                "kind": "collaboration_mailbox",
                "pane": Self.paneKey(pane),
            ],
            requestTransport: fleetReadTransport(for: host.alias)
        )
        return MuxaCollaborationMailbox(
            incoming: result.collaborationIncoming ?? [],
            sent: result.collaborationSent ?? []
        )
    }

    func collaborationRequest(
        host: MuxaFleetHostIdentity,
        pane: MuxaPaneInfo,
        requestID: String
    ) async throws -> MuxaCollaborationRequest {
        let result = try await fleetCommand(
            host: host,
            operation: [
                "kind": "collaboration_get",
                "pane": Self.paneKey(pane),
                "request_id": requestID,
            ]
        )
        guard let request = result.collaborationRequest else {
            throw MuxaIPCError.missingField("fleet_result.collaboration_request")
        }
        return request
    }

    func claimCollaboration(
        host: MuxaFleetHostIdentity,
        pane: MuxaPaneInfo
    ) async throws {
        _ = try await fleetCommand(
            host: host,
            operation: [
                "kind": "collaboration_claim",
                "pane": Self.paneKey(pane),
            ]
        )
    }

    func replyCollaboration(
        host: MuxaFleetHostIdentity,
        pane: MuxaPaneInfo,
        requestID: String,
        status: String,
        body: String
    ) async throws {
        _ = try await fleetCommand(
            host: host,
            operation: [
                "kind": "collaboration_reply",
                "pane": Self.paneKey(pane),
                "request_id": requestID,
                "status": status,
                "body": body,
            ]
        )
    }

    func spawnShell(
        command: String,
        arguments: [String] = [],
        cwd: String,
        name: String,
        environment: [String: String],
        columns: UInt16 = 80,
        rows: UInt16 = 24
    ) async throws -> MuxaSession {
        try requireNativeSessionCapabilities()
        let environmentPairs = environment
            .sorted { $0.key < $1.key }
            .map { [$0.key, $0.value] }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "spawn_session",
            "command": command,
            "args": arguments,
            "env": environmentPairs,
            "cwd": cwd,
            "name": name,
            "cols": columns,
            "rows": rows,
        ])
        guard let session = response.session else {
            throw MuxaIPCError.missingField("session")
        }
        return session
    }

    func readSession(
        id: String,
        offset: UInt64,
        waitForChanges: Bool = false
    ) async throws -> MuxaSessionOutput {
        try requireNativeSessionCapabilities()
        let eventDriven = waitForChanges && capabilities.contains(Self.sessionWaitCapability)
        let response = try await call(
            [
                "protocol": Self.protocolVersion,
                "kind": eventDriven ? "read_session_wait" : "read_session",
                "session_id": id,
                "offset": offset,
                "timeout_ms": 15_000,
            ],
            using: terminalReadTransport(for: id),
            timeout: eventDriven ? 18 : 3
        )
        guard let output = response.output else {
            throw MuxaIPCError.missingField("output")
        }
        guard output.bytes != nil else {
            throw MuxaIPCError.invalidBase64
        }
        // Compatibility for a daemon predating session_wait_v1. Keep the old
        // bounded cadence inside the client so render models never grow their
        // own polling loops again.
        if waitForChanges, !eventDriven, output.bytes?.isEmpty == true, !output.exited {
            try await Task.sleep(for: .milliseconds(45))
        }
        if output.exited {
            terminalReadTransports.removeValue(forKey: id)
        }
        return output
    }

    func fleetUpdates() throws -> AsyncThrowingStream<MuxaFleetUpdate, Error> {
        guard capabilities.contains(Self.fleetSubscribeCapability) else {
            throw MuxaIPCError.server(
                "muxad does not support host invalidation subscriptions; update muxa and restart muxad"
            )
        }
        let hello = try JSONSerialization.data(withJSONObject: [
            "protocol": Self.protocolVersion,
            "kind": "hello",
            "client": "muxa-macos-fleet-stream",
        ])
        let subscribe = try JSONSerialization.data(withJSONObject: [
            "protocol": Self.protocolVersion,
            "kind": "fleet_subscribe",
        ])
        let path = socketPath
        return AsyncThrowingStream { continuation in
            let subscription = UnixSocket.subscribe(
                path: path,
                hello: hello,
                request: subscribe,
                onLine: { data in
                    continuation.yield(try JSONDecoder().decode(MuxaFleetUpdate.self, from: data))
                },
                onFinish: { error in
                    if let error {
                        continuation.finish(throwing: error)
                    } else {
                        continuation.finish()
                    }
                }
            )
            continuation.onTermination = { @Sendable [weak subscription] _ in
                subscription?.cancel()
            }
            subscription.start()
        }
    }

    func askUpdates() throws -> AsyncThrowingStream<UInt64, Error> {
        try revisionUpdates(kind: "ask_subscribe", capability: Self.askSubscribeCapability)
    }

    func supports(_ capability: String) -> Bool {
        capabilities.contains(capability)
    }

    func pipelineUpdates() throws -> AsyncThrowingStream<UInt64, Error> {
        try revisionUpdates(
            kind: "pipeline_subscribe",
            capability: Self.pipelineSubscribeCapability
        )
    }

    func writeSession(id: String, bytes: Data) async throws {
        try requireNativeSessionCapabilities()
        _ = try await call([
            "protocol": Self.protocolVersion,
            "kind": "write_session_bytes",
            "session_id": id,
            "data_base64": bytes.base64EncodedString(),
        ])
    }

    func resizeSession(id: String, columns: UInt16, rows: UInt16) async throws {
        try requireNativeSessionCapabilities()
        _ = try await call([
            "protocol": Self.protocolVersion,
            "kind": "resize_session",
            "session_id": id,
            "cols": columns,
            "rows": rows,
        ])
    }

    func setAttached(id: String, clientID: String? = nil, attached: Bool) async throws {
        try requireNativeSessionCapabilities()
        _ = try await call([
            "protocol": Self.protocolVersion,
            "kind": "set_session_attached",
            "session_id": id,
            "client_id": clientID ?? attachmentClientID,
            "attached": attached,
        ])
    }

    func terminateSession(id: String) async throws {
        try requireNativeSessionCapabilities()
        _ = try await call([
            "protocol": Self.protocolVersion,
            "kind": "terminate_session",
            "session_id": id,
        ])
        terminalReadTransports.removeValue(forKey: id)
    }

    private func requireNativeSessionCapabilities() throws {
        guard capabilities.contains(Self.byteSafeCapability) else {
            throw MuxaIPCError.server(
                "muxad does not support byte-safe terminal sessions; update muxa and restart muxad"
            )
        }
        guard capabilities.contains(Self.attachmentIdentityCapability) else {
            throw MuxaIPCError.server(
                "muxad does not support crash-safe terminal attachments; update muxa and restart muxad"
            )
        }
    }

    private func revisionUpdates(
        kind: String,
        capability: String
    ) throws -> AsyncThrowingStream<UInt64, Error> {
        guard capabilities.contains(capability) else {
            throw MuxaIPCError.server("muxad does not support \(kind); update muxa and restart muxad")
        }
        let hello = try JSONSerialization.data(withJSONObject: [
            "protocol": Self.protocolVersion,
            "kind": "hello",
            "client": "muxa-macos-\(kind)",
        ])
        let subscribe = try JSONSerialization.data(withJSONObject: [
            "protocol": Self.protocolVersion,
            "kind": kind,
        ])
        let path = socketPath
        return AsyncThrowingStream { continuation in
            let subscription = UnixSocket.subscribe(
                path: path,
                hello: hello,
                request: subscribe,
                onLine: { data in
                    let update = try JSONDecoder().decode(MuxaRevisionUpdate.self, from: data)
                    continuation.yield(update.revision)
                },
                onFinish: { error in
                    if let error {
                        continuation.finish(throwing: error)
                    } else {
                        continuation.finish()
                    }
                }
            )
            continuation.onTermination = { @Sendable [weak subscription] _ in
                subscription?.cancel()
            }
            subscription.start()
        }
    }

    private func fleetCommand(
        host: MuxaFleetHostIdentity,
        operation: [String: Any],
        requestTransport: SerializedIPCTransport? = nil
    ) async throws -> MuxaFleetCommandResult {
        guard capabilities.contains("fleet_v1") else {
            throw MuxaIPCError.server("This operation requires muxad multi-host support (fleet_v1)")
        }
        let response = try await call(
            [
                "protocol": Self.protocolVersion,
                "kind": "fleet_command",
                "host": host.alias,
                "operation": operation,
            ],
            using: requestTransport
        )
        guard let result = response.fleetResult else {
            throw MuxaIPCError.missingField("fleet_result")
        }
        guard result.accepted else {
            throw MuxaIPCError.server(result.message ?? "Host operation was rejected")
        }
        return result
    }

    private func fleetReadTransport(for host: String) -> SerializedIPCTransport {
        let hash = host.utf8.reduce(0) { (partial: Int, byte: UInt8) in
            (partial &* 31) &+ Int(byte)
        }
        return fleetReadTransports[abs(hash % fleetReadTransports.count)]
    }

    private func terminalReadTransport(for sessionID: String) -> SerializedIPCTransport {
        if let existing = terminalReadTransports[sessionID] { return existing }
        let hash = sessionID.utf8.reduce(0) { (partial: Int, byte: UInt8) in
            (partial &* 31) &+ Int(byte)
        }
        let created = SerializedIPCTransport(
            label: "dev.muxa.mac.ipc-terminal-read-\(String(hash, radix: 16))",
            timedHandler: timedRequestHandler
        )
        terminalReadTransports[sessionID] = created
        return created
    }

    private func call(
        _ object: [String: Any],
        using requestTransport: SerializedIPCTransport? = nil,
        timeout: TimeInterval = 3
    ) async throws -> MuxaIPCResponse {
        let request = try JSONSerialization.data(withJSONObject: object)
        let data = try await (requestTransport ?? transport).request(
            path: socketPath,
            payload: request,
            timeout: timeout
        )
        let response = try JSONDecoder().decode(MuxaIPCResponse.self, from: data)
        if !response.ok {
            throw MuxaIPCError.server(response.error ?? "muxad rejected the request")
        }
        return response
    }

    private static func paneKey(_ pane: MuxaPaneInfo) -> [String: Any] {
        [
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
    }

    private static func put(_ value: String?, key: String, in object: inout [String: Any]) {
        guard let value = value?.trimmingCharacters(in: .whitespacesAndNewlines),
              !value.isEmpty else { return }
        object[key] = value
    }
}

enum UnixSocket {
    private static let maximumResponseBytes = 8 * 1024 * 1024

    static func request(
        path: String,
        payload: Data,
        timeout: TimeInterval = 3
    ) throws -> Data {
        let descriptor = try connect(path: path, timeout: timeout)
        defer { Darwin.close(descriptor) }
        try writeLine(descriptor: descriptor, payload: payload)
        var buffered = Data()
        return try readLine(descriptor: descriptor, buffered: &buffered)
    }

    static func subscribe(
        path: String,
        hello: Data,
        request: Data,
        onLine: @escaping @Sendable (Data) throws -> Void,
        onFinish: @escaping @Sendable (Error?) -> Void
    ) -> UnixSocketSubscription {
        UnixSocketSubscription(
            path: path,
            hello: hello,
            request: request,
            onLine: onLine,
            onFinish: onFinish
        )
    }

    fileprivate static func connect(path: String, timeout: TimeInterval) throws -> Int32 {
        let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else {
            throw MuxaIPCError.posix(operation: "socket", code: errno)
        }

        var noSignal: Int32 = 1
        let noSignalSize = socklen_t(MemoryLayout<Int32>.size)
        _ = withUnsafePointer(to: &noSignal) {
            setsockopt(descriptor, SOL_SOCKET, SO_NOSIGPIPE, $0, noSignalSize)
        }

        let boundedTimeout = max(0.1, timeout)
        let seconds = floor(boundedTimeout)
        var socketTimeout = timeval(
            tv_sec: Int(seconds),
            tv_usec: Int32((boundedTimeout - seconds) * 1_000_000)
        )
        let timeoutSize = socklen_t(MemoryLayout<timeval>.size)
        _ = withUnsafePointer(to: &socketTimeout) {
            setsockopt(descriptor, SOL_SOCKET, SO_RCVTIMEO, $0, timeoutSize)
        }
        _ = withUnsafePointer(to: &socketTimeout) {
            setsockopt(descriptor, SOL_SOCKET, SO_SNDTIMEO, $0, timeoutSize)
        }

        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(path.utf8CString)
        let capacity = MemoryLayout.size(ofValue: address.sun_path)
        guard pathBytes.count <= capacity else {
            Darwin.close(descriptor)
            throw MuxaIPCError.invalidSocketPath(path)
        }
        path.withCString { source in
            withUnsafeMutablePointer(to: &address.sun_path) { destination in
                let destination = UnsafeMutableRawPointer(destination)
                    .assumingMemoryBound(to: CChar.self)
                strlcpy(destination, source, capacity)
            }
        }

        let addressLength = socklen_t(MemoryLayout<sockaddr_un>.size)
        address.sun_len = UInt8(addressLength)
        let connected = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(descriptor, $0, addressLength)
            }
        }
        guard connected == 0 else {
            let code = errno
            Darwin.close(descriptor)
            throw MuxaIPCError.posix(operation: "connect", code: code)
        }
        return descriptor
    }

    fileprivate static func writeLine(descriptor: Int32, payload: Data) throws {
        var framed = payload
        framed.append(0x0A)
        try framed.withUnsafeBytes { rawBuffer in
            guard let base = rawBuffer.baseAddress else { return }
            var written = 0
            while written < rawBuffer.count {
                let count = Darwin.write(
                    descriptor,
                    base.advanced(by: written),
                    rawBuffer.count - written
                )
                if count < 0, errno == EINTR { continue }
                guard count > 0 else {
                    throw MuxaIPCError.posix(operation: "write", code: errno)
                }
                written += count
            }
        }
    }

    fileprivate static func readLine(descriptor: Int32, buffered: inout Data) throws -> Data {
        var buffer = [UInt8](repeating: 0, count: 16 * 1024)
        while true {
            if let newline = buffered.firstIndex(of: 0x0A) {
                let line = Data(buffered.prefix(upTo: newline))
                buffered.removeSubrange(...newline)
                return line
            }
            let count = Darwin.read(descriptor, &buffer, buffer.count)
            if count < 0, errno == EINTR { continue }
            if count < 0 {
                throw MuxaIPCError.posix(operation: "read", code: errno)
            }
            if count == 0 { break }
            buffered.append(contentsOf: buffer.prefix(count))
            if buffered.count > maximumResponseBytes {
                throw MuxaIPCError.responseTooLarge
            }
        }

        guard !buffered.isEmpty else { throw MuxaIPCError.emptyResponse }
        let trailing = buffered
        buffered.removeAll(keepingCapacity: true)
        return trailing
    }
}

final class UnixSocketSubscription: @unchecked Sendable {
    private let path: String
    private let hello: Data
    private let request: Data
    private let onLine: @Sendable (Data) throws -> Void
    private let onFinish: @Sendable (Error?) -> Void
    private let queue = DispatchQueue(label: "dev.muxa.mac.ipc-fleet-stream", qos: .utility)
    private let lock = NSLock()
    private var descriptor: Int32 = -1
    private var cancelled = false

    init(
        path: String,
        hello: Data,
        request: Data,
        onLine: @escaping @Sendable (Data) throws -> Void,
        onFinish: @escaping @Sendable (Error?) -> Void
    ) {
        self.path = path
        self.hello = hello
        self.request = request
        self.onLine = onLine
        self.onFinish = onFinish
    }

    func start() {
        queue.async { [self] in run() }
    }

    func cancel() {
        lock.lock()
        cancelled = true
        let activeDescriptor = descriptor
        lock.unlock()
        if activeDescriptor >= 0 {
            Darwin.shutdown(activeDescriptor, SHUT_RDWR)
        }
    }

    private func run() {
        do {
            let socket = try UnixSocket.connect(path: path, timeout: 20)
            lock.lock()
            descriptor = socket
            let shouldCancel = cancelled
            lock.unlock()
            defer {
                Darwin.close(socket)
                lock.lock()
                descriptor = -1
                lock.unlock()
            }
            if shouldCancel {
                Darwin.shutdown(socket, SHUT_RDWR)
                onFinish(nil)
                return
            }

            var buffered = Data()
            try UnixSocket.writeLine(descriptor: socket, payload: hello)
            try validateAck(UnixSocket.readLine(descriptor: socket, buffered: &buffered))
            try UnixSocket.writeLine(descriptor: socket, payload: request)
            try validateAck(UnixSocket.readLine(descriptor: socket, buffered: &buffered))

            while !isCancelled {
                let line = try UnixSocket.readLine(descriptor: socket, buffered: &buffered)
                if line.isEmpty { continue }
                try onLine(line)
            }
            onFinish(nil)
        } catch {
            onFinish(isCancelled ? nil : error)
        }
    }

    private var isCancelled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return cancelled
    }

    private func validateAck(_ data: Data) throws {
        let response = try JSONDecoder().decode(MuxaIPCResponse.self, from: data)
        guard response.ok else {
            throw MuxaIPCError.server(response.error ?? "muxad rejected the subscription")
        }
    }
}
