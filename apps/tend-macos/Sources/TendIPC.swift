import Darwin
import Dispatch
import Foundation

enum TendIPCError: LocalizedError {
    case invalidSocketPath(String)
    case posix(operation: String, code: Int32)
    case responseTooLarge
    case emptyResponse
    case server(String)
    case incompatibleProtocol(minimum: UInt32?, maximum: UInt32?)
    case missingField(String)
    case invalidBase64
    /// A failure that reached tendd, so the request must not be replayed.
    indirect case afterRequestReached(TendIPCError)

    /// True while the request provably never reached tendd: the connect or
    /// the write failed on a socket tendd had already dropped.
    ///
    /// Deliberately excludes read failures — a timeout (EAGAIN) or a reset
    /// partway through a reply proves nothing about whether tendd applied the
    /// request, and replaying a mutation would double-apply it. Those are
    /// reported, not retried; `AppModel` re-polls instead, which is safe
    /// because a snapshot refresh mutates nothing.
    var isReconnectable: Bool {
        switch self {
        case .posix(let operation, let code):
            (operation == "connect" || operation == "write" || operation == "socket")
                && [EPIPE, ECONNRESET, ECONNREFUSED, ENOENT].contains(code)
        default:
            false
        }
    }

    /// The same error, marked as "tendd may have seen this", so a caller
    /// that retries connection failures leaves it alone.
    var notReconnectable: TendIPCError {
        if case .afterRequestReached = self { return self }
        return .afterRequestReached(self)
    }

    var errorDescription: String? {
        switch self {
        case .invalidSocketPath(let path):
            "The tendd socket path is too long: \(path)"
        case .posix(let operation, let code):
            // A dropped connection almost always means tendd restarted (its
            // own binary watch re-execs it, and an app update replaces it),
            // so say that rather than handing over an errno. A read that hits
            // SO_RCVTIMEO reports EAGAIN, which is that same restart seen from
            // a connection tendd accepted and then stopped answering on as it
            // shut down; left unclassified it reached the connection banner as
            // "read failed: Resource temporarily unavailable".
            switch code {
            case EPIPE, ECONNRESET, ECONNREFUSED, ENOENT, EAGAIN, ETIMEDOUT:
                "tendd is not answering — it may be restarting. Try again in a moment."
            default:
                "\(operation) failed: \(String(cString: strerror(code)))"
            }
        case .responseTooLarge:
            "tendd returned an oversized IPC response"
        case .emptyResponse:
            // End of file before a response: tendd closed the connection,
            // which is what its shutdown now does to an idle or read-only
            // handler instead of holding the client for the drain timeout.
            "tendd closed the connection without responding — it may be restarting. Try again in a moment."
        case .server(let message):
            message
        case .incompatibleProtocol(let minimum, let maximum):
            "Incompatible tendd protocol (server supports \(minimum.map(String.init) ?? "?")…\(maximum.map(String.init) ?? "?"), app requires \(TendIPCClient.protocolVersion))"
        case .missingField(let name):
            "tendd response is missing \(name)"
        case .afterRequestReached(let underlying):
            underlying.errorDescription
        case .invalidBase64:
            "tendd returned invalid byte-safe terminal data"
        }
    }
}

struct TendSession: Codable, Hashable, Identifiable, Sendable {
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

struct TendSessionOutput: Decodable, Sendable {
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

struct TendWorkIdentity: Codable, Hashable, Identifiable, Sendable {
    let workspaceID: String
    let workID: String

    var id: String { "\(workspaceID)/\(workID)" }

    enum CodingKeys: String, CodingKey {
        case workspaceID = "workspace_id"
        case workID = "work_id"
    }
}

struct TendWorkStartRequest: Hashable, Sendable {
    let work: String
    let workspace: String?
    let pipeline: String?
    let cwd: String?
    let external: String?
    let skill: String?
    let body: String?
    let context: String?
    let dryRun: Bool
    /// Fleet host alias to run `tend work up` on; nil is the local host.
    let host: String?

    init(
        work: String,
        workspace: String?,
        pipeline: String?,
        cwd: String?,
        external: String?,
        skill: String?,
        body: String?,
        context: String?,
        dryRun: Bool,
        host: String? = nil
    ) {
        self.work = work
        self.workspace = workspace
        self.pipeline = pipeline
        self.cwd = cwd
        self.external = external
        self.skill = skill
        self.body = body
        self.context = context
        self.dryRun = dryRun
        self.host = host
    }
}

/// Output of one allowlisted `tend work …` invocation run by tendd on the
/// local host or on a fleet host (`work_command`).
struct TendWorkCommandOutput: Decodable, Equatable, Sendable {
    let exitCode: Int32
    let stdout: String
    let stderr: String

    enum CodingKeys: String, CodingKey {
        case stdout, stderr
        case exitCode = "exit_code"
    }
}

/// One step of `tend work up`'s reconciliation plan: `launch` a missing
/// pane, `reprompt` or `keep` a live one, `waiting` on an `after` edge, or
/// `attention` when a person has to act first.
struct TendWorkPlanStep: Decodable, Equatable, Sendable, Identifiable {
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

struct TendWorkPlan: Decodable, Equatable, Sendable {
    let steps: [TendWorkPlanStep]
}

struct TendWorkStartResult: Decodable, Equatable, Sendable {
    let work: String
    let workspace: String
    let pipeline: String?
    let cwd: String?
    let dryRun: Bool?
    let layout: String?
    let plan: TendWorkPlan?

    enum CodingKeys: String, CodingKey {
        case work, workspace, pipeline, cwd, layout, plan
        case dryRun = "dry_run"
    }
}

struct TendWorkOperation: Decodable, Sendable {
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
    let result: TendWorkStartResult?

    enum CodingKeys: String, CodingKey {
        case state, work, workspace, message, result
        case operationID = "operation_id"
    }
}

struct TendPipelineAliasState: Decodable, Hashable, Sendable {
    let alias: String
    let status: String
    let generation: UInt64
    let pane: String?
    let error: String?
}

struct TendDesiredAgent: Decodable, Hashable, Sendable {
    let alias: String
    let program: String
    let role: String?
    let task: String?
    let after: [String]?
}

struct TendPipelineRun: Decodable, Hashable, Identifiable, Sendable {
    let identity: TendWorkIdentity
    let pipeline: String
    let desired: [TendDesiredAgent]
    let cwd: String
    let generation: UInt64
    let windowID: String?
    let aliases: [String: TendPipelineAliasState]

    var id: TendWorkIdentity { identity }

    enum CodingKeys: String, CodingKey {
        case identity, pipeline, desired, cwd, generation, aliases
        case windowID = "window_id"
    }
}

struct TendAgent: Decodable, Hashable, Identifiable, Sendable {
    let kind: String
    let agentSessionID: String
    let pane: String?
    let tmuxSocket: String?
    let tmuxSession: String?
    let cwd: String?
    let state: String
    let lastPrompt: String?
    /// Who caused the latest prompt when tend delivered it (`prompt_origin_v1`):
    /// person_via_tend, peer, automation, tend, or unknown (no tend delivery
    /// matched). nil from an older daemon.
    let lastPromptOrigin: String?
    let lastPromptAt: String?
    let lastResponse: String?
    let recap: String?
    let aiTitle: String?
    let lastNotification: String?
    let model: String?
    let contextUsedPercent: Double?
    let costUSD: Double?
    // WS-C usage: rate-limit fields from Claude's statusline heartbeat and
    // Codex's rollout. Timestamps and enums stay strings, like `startedAt`;
    // `TendUsage` parses them. All optional so older daemons still decode.
    let rateLimit5hPercent: Double?
    let rateLimit5hResetsAt: String?
    let rateLimit7dPercent: Double?
    let rateLimit7dResetsAt: String?
    let rateLimitedUntil: String?
    let rateLimitScope: String?
    let rateLimitSource: String?
    // WS-C usage end
    let startedAt: String?
    let lastActivityAt: String?
    let stateEnteredAt: String?
    let workload: TendAgentWorkload?
    let subagents: [TendAgentSubagent]?
    /// The question an idle agent's turn ended on (`awaiting_reply_v1`),
    /// inferred by tendd from its last response; nil from an older daemon
    /// and on rows that are not idle.
    let awaitingReply: TendAwaitingReply?

    var id: String { agentSessionID }

    enum CodingKeys: String, CodingKey {
        case kind, pane, cwd, state, recap, model
        case agentSessionID = "agent_session_id"
        case tmuxSocket = "tmux_socket"
        case tmuxSession = "tmux_session"
        case lastPrompt = "last_prompt"
        case lastPromptOrigin = "last_prompt_origin"
        case lastPromptAt = "last_prompt_at"
        case lastResponse = "last_response"
        case aiTitle = "ai_title"
        case lastNotification = "last_notification"
        case contextUsedPercent = "context_used_pct"
        case costUSD = "cost_usd"
        // WS-C usage
        case rateLimit5hPercent = "rate_limit_5h_pct"
        case rateLimit5hResetsAt = "rate_limit_5h_resets_at"
        case rateLimit7dPercent = "rate_limit_7d_pct"
        case rateLimit7dResetsAt = "rate_limit_7d_resets_at"
        case rateLimitedUntil = "rate_limited_until"
        case rateLimitScope = "rate_limit_scope"
        case rateLimitSource = "rate_limit_source"
        // WS-C usage end
        case startedAt = "started_at"
        case lastActivityAt = "last_activity_at"
        case stateEnteredAt = "state_entered_at"
        case workload, subagents
        case awaitingReply = "awaiting_reply"
    }
}

/// What an idle agent asked the person at the end of its turn.
struct TendAwaitingReply: Decodable, Hashable, Sendable {
    let question: String
    let since: String?
}

struct TendAgentWorkload: Decodable, Hashable, Sendable {
    let processCount: Int
    let shellCount: Int
    let subagentCount: Int
    let helperCount: Int
    let preview: [TendAgentProcess]

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
        preview = try values.decodeIfPresent([TendAgentProcess].self, forKey: .preview) ?? []
    }
}

struct TendAgentProcess: Decodable, Hashable, Identifiable, Sendable {
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

struct TendAgentSubagent: Decodable, Hashable, Identifiable, Sendable {
    let kind: String
    let description: String?
    let startedAt: String

    var id: String { "\(kind):\(startedAt):\(description ?? "")" }

    enum CodingKeys: String, CodingKey {
        case kind, description
        case startedAt = "started_at"
    }
}

struct TendPaneInfo: Decodable, Hashable, Sendable {
    /// A console routing envelope, never a terminal target. Operator-only
    /// mailbox reads/replies work even after a host's last pane has closed.
    static let operatorRoute = TendPaneInfo(
        paneID: "console", sessionID: "console", session: "", windowID: "console",
        windowName: "", windowIndex: "0", paneIndex: "0", currentCommand: "",
        title: "", currentPath: "", socket: "default", workspaceID: nil,
        workID: nil, agentRole: nil, agentAlias: nil
    )

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

    var workIdentity: TendWorkIdentity? {
        guard let workspaceID, !workspaceID.isEmpty,
              let workID, !workID.isEmpty else { return nil }
        return TendWorkIdentity(workspaceID: workspaceID, workID: workID)
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

struct TendExecutionSnapshot: Sendable {
    let hosts: [TendFleetHost]
    let agents: [TendAgent]
    let panes: [TendPaneInfo]
    let hostedAgents: [TendHostedAgent]
    let watchHosts: [TendWatchHost]

    private let panesByAgentSessionID: [String: [TendPaneInfo]]
    private let agentsByPaneID: [String: [TendAgent]]
    private let watchPanesByID: [TendWatchPaneIdentity: TendWatchPane]
    private let watchSessionsByID: [TendWatchSessionIdentity: TendWatchSession]
    private let watchWindowsByID: [TendWatchWindowIdentity: TendWatchWindow]
    private let watchWindowsByPaneID: [TendWatchPaneIdentity: TendWatchWindow]

    static let empty = TendExecutionSnapshot(hosts: [])

    init(hosts: [TendFleetHost]) {
        self.hosts = hosts
        agents = hosts.flatMap { $0.remote?.agents ?? [] }
        panes = hosts.flatMap { $0.remote?.panes ?? [] }

        var hosted: [TendHostedAgent] = []
        var panesByAgent: [String: [TendPaneInfo]] = [:]
        var agentsByPane: [String: [TendAgent]] = [:]
        var builtWatchHosts: [TendWatchHost] = []
        var paneIndex: [TendWatchPaneIdentity: TendWatchPane] = [:]
        var sessionIndex: [TendWatchSessionIdentity: TendWatchSession] = [:]
        var windowIndex: [TendWatchWindowIdentity: TendWatchWindow] = [:]
        var windowByPaneIndex: [TendWatchPaneIdentity: TendWatchWindow] = [:]

        for host in hosts {
            let hostPanes = host.remote?.panes ?? []
            let hostAgents = host.remote?.agents ?? []
            // Build once per host: matching every agent by scanning every
            // pane turns each Fleet refresh into agents × panes work.
            let hostPanesByID = Dictionary(grouping: hostPanes, by: \.paneID)
            let hostAgentsByPane = Dictionary(grouping: hostAgents.compactMap { agent in
                agent.pane.map { ($0, agent) }
            }, by: { $0.0 }).mapValues { $0.map(\.1) }

            for agent in hostAgents {
                let pane = agent.pane.flatMap { Self.pane(for: agent, among: hostPanesByID[$0] ?? []) }
                hosted.append(TendHostedAgent(host: host.identity, agent: agent, pane: pane))
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
            let sessions = groupedSessions.values.map { sessionPanes -> TendWatchSession in
                let first = sessionPanes[0]
                let groupedWindows = Dictionary(grouping: sessionPanes, by: \.stableWindowID)
                let windows = groupedWindows.values.map { windowPanes -> TendWatchWindow in
                    let firstWindow = windowPanes[0]
                    let nodes = windowPanes.map { pane -> TendWatchPane in
                        let candidates = (hostAgentsByPane[pane.paneID] ?? []).filter {
                            $0.tmuxSocket == nil || $0.tmuxSocket == pane.socket
                        }
                        return TendWatchPane(
                            host: host.identity,
                            pane: pane,
                            agent: candidates.count == 1 ? candidates[0] : nil
                        )
                    }.sorted {
                        Self.numericIndex($0.pane.paneIndex) < Self.numericIndex($1.pane.paneIndex)
                    }
                    return TendWatchWindow(
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
                return TendWatchSession(
                    hostAlias: host.alias,
                    socket: first.endpointSocket,
                    sessionID: first.stableSessionID,
                    name: first.session,
                    windows: windows
                )
            }.sorted {
                $0.name.localizedStandardCompare($1.name) == .orderedAscending
            }
            builtWatchHosts.append(TendWatchHost(host: host, sessions: sessions))
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

    func pane(for agent: TendAgent) -> TendPaneInfo? {
        let matches = panesByAgentSessionID[agent.agentSessionID] ?? []
        return matches.count == 1 ? matches[0] : nil
    }

    private static func pane(for agent: TendAgent, among panes: [TendPaneInfo]) -> TendPaneInfo? {
        guard let paneID = agent.pane else { return nil }
        if let socket = agent.tmuxSocket,
           let exact = panes.first(where: { $0.paneID == paneID && $0.socket == socket }) {
            return exact
        }
        // Caller already scoped this list to the agent's pane ID.
        return panes.count == 1 ? panes[0] : nil
    }

    func agent(for pane: TendPaneInfo) -> TendAgent? {
        let exact = (agentsByPaneID[pane.paneID] ?? []).filter {
            $0.pane == pane.paneID && ($0.tmuxSocket == nil || $0.tmuxSocket == pane.socket)
        }
        return exact.count == 1 ? exact[0] : nil
    }

    func watchPane(id: TendWatchPaneIdentity) -> TendWatchPane? {
        watchPanesByID[id]
    }

    func watchSession(id: TendWatchSessionIdentity) -> TendWatchSession? {
        watchSessionsByID[id]
    }

    func watchWindow(containing id: TendWatchPaneIdentity) -> TendWatchWindow? {
        watchWindowsByPaneID[id]
    }

    func watchWindow(id: TendWatchWindowIdentity) -> TendWatchWindow? {
        watchWindowsByID[id]
    }

    func hasSameSource(as other: TendExecutionSnapshot) -> Bool {
        hosts == other.hosts
    }

    private static func numericIndex(_ value: String) -> Int {
        Int(value) ?? Int.max
    }
}

struct TendFleetHostIdentity: Hashable, Sendable {
    let alias: String
    let local: Bool
    let state: String
    let mode: String
}

struct TendHostedAgent: Identifiable, Sendable {
    let host: TendFleetHostIdentity
    let agent: TendAgent
    let pane: TendPaneInfo?

    var id: String { "\(host.alias):\(agent.id)" }
}

struct TendFleetSnapshot: Decodable, Sendable {
    let hosts: [TendFleetHost]
}

struct TendFleetUpdate: Decodable, Sendable {
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

struct TendFleetHost: Decodable, Hashable, Identifiable, Sendable {
    let alias: String
    let local: Bool
    let sshTarget: String?
    let mode: String
    let state: String
    let latencyMS: UInt64?
    let error: String?
    let tendVersion: String?
    let daemonGeneration: UInt64?
    let labels: [String: String]?
    let annotations: [String: String]?
    let remote: TendRemoteSnapshot?

    var nodeID: String? = nil

    var id: String { alias }
    var identity: TendFleetHostIdentity {
        TendFleetHostIdentity(alias: alias, local: local, state: state, mode: mode)
    }

    enum CodingKeys: String, CodingKey {
        case alias, local, mode, state, error, labels, annotations, remote
        case nodeID = "node_id"
        case sshTarget = "ssh_target"
        case latencyMS = "latency_ms"
        case tendVersion = "tend_version"
        case daemonGeneration = "daemon_generation"
    }
}

struct TendRemoteSnapshot: Decodable, Hashable, Sendable {
    let agents: [TendAgent]
    let panes: [TendPaneInfo]
}

struct TendPaneCapture: Sendable {
    let screenText: String?
    let rawBytes: Data?
}

struct TendAskEntry: Decodable, Hashable, Identifiable, Sendable {
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

struct TendAskConversation: Decodable, Hashable, Identifiable, Sendable {
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

struct TendAskConversationSnapshot: Sendable {
    let conversations: [TendAskConversation]
    let active: TendAskConversation?
}

/// One optional `[ask.providers.<id>]` key in an `ask_provider_configure`
/// request: omitted from the request (`keep`), sent as JSON `null` so the
/// daemon clears it, or set to a value.
enum TendAskProviderFieldUpdate: Equatable, Sendable {
    case keep
    case clear
    case set(String)

    /// `nil` or blank clears; anything else sets the trimmed value.
    init(_ value: String?) {
        let trimmed = value?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        self = trimmed.isEmpty ? .clear : .set(trimmed)
    }

    fileprivate func apply(to request: inout [String: Any], key: String) {
        switch self {
        case .keep:
            break
        case .clear:
            request[key] = NSNull()
        case .set(let value):
            request[key] = value
        }
    }
}

struct TendCollaborationRoom: Decodable, Hashable, Sendable {
    let host: String
    let socket: String?
    let windowID: String

    enum CodingKeys: String, CodingKey {
        case host, socket
        case windowID = "window_id"
    }
}

struct TendCollaborationParticipant: Decodable, Hashable, Sendable {
    let agentKind: String
    let agentSessionID: String
    let pane: String
    let socket: String?
    let room: TendCollaborationRoom
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

struct TendCollaborationReply: Decodable, Hashable, Sendable {
    let status: String
    let body: String
    let at: String
}

struct TendPeerInterruption: Decodable, Hashable, Sendable {
    let requestID: String
    let reason: String
    let active: Bool
    let observedAt: String
    let resetAt: String?
    let actionRequestID: String?
    let decision: TendCollaborationReply?
    enum CodingKeys: String, CodingKey {
        case reason, active, decision
        case requestID = "request_id"
        case observedAt = "observed_at"
        case resetAt = "reset_at"
        case actionRequestID = "action_request_id"
    }
}

struct TendCollaborationRequest: Decodable, Identifiable, Hashable, Sendable {
    var interruption: TendPeerInterruption? = nil
    var peerInterrupted: Bool {
        interruption?.active == true && interruption?.requestID == id && ["queued", "claimed"].contains(status)
    }
    var humanAction: String? = nil
    /// The asker's decision context (`collaboration_human_decision_v1`).
    var decision: TendDecision? = nil
    var needsHumanResponse: Bool {
        to.console == true && expectsReply && kind != "notice" && ["queued", "claimed"].contains(status)
    }
    let id: String
    let from: TendCollaborationParticipant
    let to: TendCollaborationParticipant
    let kind: String
    let body: String
    let expectsReply: Bool
    let workMode: String
    let status: String
    let createdAt: String
    let reply: TendCollaborationReply?
    let threadID: String?
    let parentRequestID: String?
    let workspaceID: String?
    let workID: String?
    let runID: String?
    let claimedAt: String?
    let replyReadAt: String?

    enum CodingKeys: String, CodingKey {
        case id, from, to, kind, body, status, reply
        case interruption, decision
        case humanAction = "human_action"
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

struct TendCollaborationMailbox: Sendable {
    let incoming: [TendCollaborationRequest]
    let sent: [TendCollaborationRequest]
    let readToken: String?
    let unchanged: Bool

    init(incoming: [TendCollaborationRequest], sent: [TendCollaborationRequest], readToken: String? = nil, unchanged: Bool = false) {
        self.incoming = incoming
        self.sent = sent
        self.readToken = readToken
        self.unchanged = unchanged
    }
}

private struct TendFleetCommandResult: Decodable, Sendable {
    let accepted: Bool
    let message: String?
    let capture: String?
    let captureRawBase64: String?
    let collaborationRequest: TendCollaborationRequest?
    let collaborationIncoming: [TendCollaborationRequest]?
    let collaborationSent: [TendCollaborationRequest]?
    let collaborationReadToken: String?
    let collaborationUnchanged: Bool?
    let worktrees: [TendWorktreeRow]?
    let worktreesTotal: Int?
    let worktreesNode: String?
    let worktreeChanges: [TendWorktreeChangeOutcome]?
    let servers: TendServerInventory?

    enum CodingKeys: String, CodingKey {
        case accepted, message, capture, worktrees, servers
        case worktreesTotal = "worktrees_total"
        case worktreesNode = "worktrees_node"
        case worktreeChanges = "worktree_changes"
        case captureRawBase64 = "capture_raw_base64"
        case collaborationRequest = "collaboration_request"
        case collaborationIncoming = "collaboration_incoming"
        case collaborationSent = "collaboration_sent"
        case collaborationReadToken = "collaboration_read_token"
        case collaborationUnchanged = "collaboration_unchanged"
    }
}

private struct TendIPCResponse: Decodable, Sendable {
    let ok: Bool
    let error: String?
    let minProtocol: UInt32?
    let maxProtocol: UInt32?
    let capabilities: [String]?
    /// `hello`'s per-process daemon identity (`daemon_boot_id_v1`); absent
    /// from an older tendd.
    let bootID: String?
    /// `hello`'s self-reexec count, present with the `restart` capability.
    let generation: UInt64?
    let agents: [TendAgent]?
    let sessions: [TendSession]?
    let session: TendSession?
    let output: TendSessionOutput?
    let capture: String?
    let fleet: TendFleetSnapshot?
    let fleetResult: TendFleetCommandResult?
    let pipelineRuns: [TendPipelineRun]?
    let workOperation: TendWorkOperation?
    let askEntries: [TendAskEntry]?
    let askEntry: TendAskEntry?
    let askConversations: [TendAskConversation]?
    let askConversation: TendAskConversation?
    let askAgent: String?
    let askEnabled: Bool?
    let askProviders: [TendAskProvider]?
    let workCommand: TendWorkCommandOutput?
    let turnEvidence: TendTurnEvidence?
    let reviewItems: [TendReviewItem]?
    let reviewHasMore: Bool?
    let reviewItem: TendReviewItem?
    let decisionQueue: TendLenient<TendPendingQueue>?
    let decisionHistory: TendLenient<TendDecisionHistory>?
    let workRecords: [TendLenient<TendWorkRecord>]?
    /// `swap_health`'s answer. Swap job requests also answer in `swap`,
    /// with another shape: that reads as nil rather than failing the reply.
    let swap: TendLenient<TendSwapHealth>?
    let swapAccounts: [TendProviderCandidates]?

    enum CodingKeys: String, CodingKey {
        case ok, error, capabilities, agents, sessions, session, output, capture, fleet, swap, generation
        case minProtocol = "min_protocol"
        case maxProtocol = "max_protocol"
        case bootID = "boot_id"
        case fleetResult = "fleet_result"
        case pipelineRuns = "pipeline_runs"
        case workCommand = "work_command"
        case turnEvidence = "turn_evidence"
        case reviewItems = "review_items"
        case reviewHasMore = "review_has_more"
        case reviewItem = "review_item"
        case decisionQueue = "decision_queue"
        case decisionHistory = "decision_history"
        case workRecords = "work_records"
        case workOperation = "work_operation"
        case askEntries = "ask_entries"
        case askEntry = "ask_entry"
        case askConversations = "ask_conversations"
        case askConversation = "ask_conversation"
        case askAgent = "ask_agent"
        case askEnabled = "ask_enabled"
        case askProviders = "ask_providers"
        case swapAccounts = "swap_accounts"
    }
}

/// A field decoded when it has the expected shape and nil otherwise.
struct TendLenient<Value: Decodable & Sendable>: Decodable, Sendable {
    let value: Value?

    init(from decoder: Decoder) throws {
        value = try? Value(from: decoder)
    }
}

private struct TendRevisionUpdate: Decodable, Sendable {
    let revision: UInt64
}

typealias TendIPCRequestHandler = @Sendable (String, Data) throws -> Data
typealias TendIPCTimedRequestHandler = @Sendable (
    String,
    Data,
    TimeInterval
) throws -> Data

/// Owns exactly one continuation across cancellation/queue-start races.
private final class QueuedIPCRequest: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Data, Error>?
    private var cancelled = false
    private var started = false

    func install(_ value: CheckedContinuation<Data, Error>) -> Bool {
        lock.lock()
        if cancelled {
            lock.unlock()
            value.resume(throwing: CancellationError())
            return false
        }
        continuation = value
        lock.unlock()
        return true
    }

    func begin() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard !cancelled else { return false }
        started = true
        return true
    }

    func cancel() {
        lock.lock()
        cancelled = true
        // A command already sent cannot be undone by cancelling its waiter.
        let pending = started ? nil : continuation
        if !started { continuation = nil }
        lock.unlock()
        pending?.resume(throwing: CancellationError())
    }

    func finish(_ result: Result<Data, Error>) {
        lock.lock()
        let pending = continuation
        continuation = nil
        lock.unlock()
        pending?.resume(with: result)
    }
}

/// Runs the blocking Unix-socket exchange on one serial queue.
///
/// Actor methods are reentrant while awaiting detached work. The previous
/// implementation therefore allowed every key, resize, refresh, and polling
/// request to create another blocked worker when tendd was slow or restarting.
/// Keeping exactly one blocking exchange active bounds thread and descriptor
/// use while preserving wire order.
final class SerializedIPCTransport: @unchecked Sendable {
    private let queue: DispatchQueue
    private let handler: TendIPCTimedRequestHandler

    init(
        label: String = "dev.tend.mac.ipc-transport",
        handler: @escaping TendIPCRequestHandler
    ) {
        queue = DispatchQueue(label: label, qos: .userInitiated)
        self.handler = { path, payload, _ in try handler(path, payload) }
    }

    init(label: String, timedHandler: @escaping TendIPCTimedRequestHandler) {
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
        let pending = QueuedIPCRequest()
        let response = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                guard pending.install(continuation) else { return }
                queue.async {
                    guard pending.begin() else { return }
                    pending.finish(Result { try handler(path, payload, timeout) })
                }
            }
        } onCancel: {
            pending.cancel()
        }
        // Preserve acknowledgements for work already started: attachment
        // cleanup must know whether the attach actually took effect.
        return response
    }
}

/// One lazily built value, shared by every caller.
///
/// The feature clients below each own a `SerializedIPCTransport`; building a
/// fresh one per call would give every request its own queue and lose the
/// one-at-a-time wire order that transport exists for.
final class IPCLazyClient<Client: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var client: Client?

    func value(_ make: () -> Client) -> Client {
        lock.lock()
        defer { lock.unlock() }
        if let client { return client }
        let made = make()
        client = made
        return made
    }
}

/// What a successful `hello` says about which daemon answered.
struct TendDaemonHello: Equatable, Sendable {
    /// Random per-process id; nil from a tendd older than
    /// `daemon_boot_id_v1`, which the app then tells apart by socket owner pid.
    var bootID: String?
    /// Self-reexec count; nil from a daemon without the `restart` capability.
    var generation: UInt64?

    init(bootID: String?, generation: UInt64?) {
        self.bootID = bootID.flatMap { $0.isEmpty ? nil : $0 }
        self.generation = generation
    }

    fileprivate init(_ response: TendIPCResponse) {
        self.init(bootID: response.bootID, generation: response.generation)
    }
}

actor TendIPCClient {
    static let protocolVersion: UInt32 = 6
    static let byteSafeCapability = "session_bytes_v1"
    static let attachmentIdentityCapability = "session_attachment_identity_v1"
    static let workControlCapability = "work_control_v1"
    static let workCommandCapability = "work_command_v1"
    static let askCredentialCapability = "ask_one_turn_credential_v1"
    static let askConversationCapability = "ask_conversations_v1"
    static let askProvidersCapability = "ask_providers_v1"
    static let fleetSubscribeCapability = "fleet_subscribe"
    static let sessionWaitCapability = "session_wait_v1"
    static let askSubscribeCapability = "ask_subscribe"
    static let pipelineSubscribeCapability = "pipeline_subscribe"
    static let daemonBootIDCapability = "daemon_boot_id_v1"

    let socketPath: String
    private(set) var capabilities: Set<String> = []
    /// The boot id from the last successful `hello`; nil before one, or
    /// when the daemon predates `daemon_boot_id_v1`.
    private(set) var daemonBootID: String?
    private let transport: SerializedIPCTransport
    // Slow command execution and background snapshots must never queue in
    // front of terminal input, resize, attachment, or prompt delivery.
    private let snapshotTransport: SerializedIPCTransport
    private let stateTransport: SerializedIPCTransport
    private var workTransports: [String: SerializedIPCTransport] = [:]
    private let timedRequestHandler: TendIPCTimedRequestHandler
    private var terminalReadTransports: [String: SerializedIPCTransport] = [:]
    private let fleetReadTransports: [SerializedIPCTransport]
    private let attachmentClientID: String
    // Feature clients, built once on first use. `socketPath` is immutable, so
    // the cache never goes stale: pointing the app at another socket builds a
    // new `TendIPCClient`, and with it empty caches.
    let automationClientCache = IPCLazyClient<TendAutomationClient>()
    let configClientCache = IPCLazyClient<TendConfigClient>()
    let workComposeClientCache = IPCLazyClient<TendWorkComposeClient>()
    let agentStartClientCache = IPCLazyClient<TendAgentStartClient>()

    init(socketPath: String = TendIPCClient.defaultSocketPath()) {
        self.socketPath = socketPath
        timedRequestHandler = UnixSocket.request
        snapshotTransport = SerializedIPCTransport(label: "dev.tend.mac.ipc-snapshot", timedHandler: UnixSocket.request)
        stateTransport = SerializedIPCTransport(label: "dev.tend.mac.ipc-state", timedHandler: UnixSocket.request)
        transport = SerializedIPCTransport(
            label: "dev.tend.mac.ipc-control",
            timedHandler: UnixSocket.request
        )
        fleetReadTransports = (0..<4).map { index in
            SerializedIPCTransport(
                label: "dev.tend.mac.ipc-fleet-read-\(index)",
                timedHandler: UnixSocket.request
            )
        }
        attachmentClientID = "tend-macos:\(getuid())"
    }

    init(socketPath: String, request: @escaping TendIPCRequestHandler) {
        self.socketPath = socketPath
        timedRequestHandler = { path, payload, _ in try request(path, payload) }
        snapshotTransport = SerializedIPCTransport(label: "dev.tend.mac.ipc-snapshot-test", handler: request)
        stateTransport = SerializedIPCTransport(label: "dev.tend.mac.ipc-state-test", handler: request)
        transport = SerializedIPCTransport(label: "dev.tend.mac.ipc-control-test", handler: request)
        fleetReadTransports = (0..<4).map { index in
            SerializedIPCTransport(
                label: "dev.tend.mac.ipc-fleet-read-test-\(index)",
                handler: request
            )
        }
        attachmentClientID = "tend-macos-test:\(getpid())"
    }

    static func defaultSocketPath(environment: [String: String] = ProcessInfo.processInfo.environment) -> String {
        if let configured = environment["TEND_SOCKET"], !configured.isEmpty {
            return configured
        }
        return "/tmp/tend-\(getuid()).sock"
    }

    @discardableResult
    func hello() async throws -> TendDaemonHello {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "hello",
            "client": "tend-macos",
        ])
        guard let minimum = response.minProtocol,
              let maximum = response.maxProtocol,
              (minimum...maximum).contains(Self.protocolVersion) else {
            throw TendIPCError.incompatibleProtocol(
                minimum: response.minProtocol,
                maximum: response.maxProtocol
            )
        }
        capabilities = Set(response.capabilities ?? [])
        let identity = TendDaemonHello(response)
        daemonBootID = identity.bootID
        try requireNativeSessionCapabilities()
        guard capabilities.contains(Self.workControlCapability) else {
            throw TendIPCError.server(
                "tendd does not support native Work control; update tend or choose Use Bundled tendd"
            )
        }
        return identity
    }

    func listSessions() async throws -> [TendSession] {
        try requireNativeSessionCapabilities()
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "list_sessions",
        ])
        return response.sessions ?? []
    }

    func listPipelineRuns() async throws -> [TendPipelineRun] {
        guard capabilities.contains("pipeline_runs_v1") else { return [] }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "pipeline_runs",
        ])
        return response.pipelineRuns ?? []
    }

    func startWork(_ request: TendWorkStartRequest) async throws -> TendWorkOperation {
        guard capabilities.contains(Self.workControlCapability) else {
            throw TendIPCError.server(
                "tendd does not support native Work control; update tend and restart tendd"
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
        Self.put(request.host, key: "host", in: &workRequest)
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "work_up",
            "request": workRequest,
        ])
        guard let operation = response.workOperation else {
            throw TendIPCError.missingField("work_operation")
        }
        return operation
    }

    /// Runs one allowlisted `tend work …` subcommand (options, preset,
    /// pipeline, route) through tendd, locally or on a fleet host. Requires
    /// `work_command_v1`; callers fall back to the bundled CLI for the local
    /// host when an older daemon lacks it.
    func workCommand(
        host: String?,
        arguments: [String],
        stdin: String? = nil
    ) async throws -> TendWorkCommandOutput {
        guard capabilities.contains(Self.workCommandCapability) else {
            throw TendIPCError.server(
                "tendd does not support Work commands on hosts; update tend and restart tendd"
            )
        }
        var request: [String: Any] = [
            "protocol": Self.protocolVersion,
            "kind": "work_command",
            "args": arguments,
        ]
        if let host, !host.isEmpty { request["host"] = host }
        if let stdin { request["stdin"] = stdin }
        let response = try await call(request, using: workTransport(for: host ?? "local"), timeout: 40)
        guard let output = response.workCommand else {
            throw TendIPCError.missingField("work_command")
        }
        return output
    }

    /// How this daemon's swap service is (`swap_health_v1`); nil from a
    /// daemon without swap jobs or without the health check.
    func swapHealth() async throws -> TendSwapHealth? {
        guard capabilities.contains(Self.swapHealthCapability) else { return nil }
        let response = try await call(["protocol": Self.protocolVersion, "kind": "swap_health"])
        guard let health = response.swap?.value else { throw TendIPCError.missingField("swap") }
        return health
    }

    func swapAccounts() async throws -> [TendProviderCandidates] {
        guard capabilities.contains("swap_accounts_v1") else {
            throw TendIPCError.missingField("swap_accounts_v1 (update tendd)")
        }
        let response = try await call(["protocol": Self.protocolVersion, "kind": "swap_accounts"], timeout: 30)
        guard let accounts = response.swapAccounts else { throw TendIPCError.missingField("swap_accounts") }
        return accounts
    }

    static let swapHealthCapability = "swap_health_v1"

    func workOperation(id: String) async throws -> TendWorkOperation {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "work_up_status",
            "operation_id": id,
        ])
        guard let operation = response.workOperation else {
            throw TendIPCError.missingField("work_operation")
        }
        return operation
    }

    func executionSnapshot() async throws -> TendExecutionSnapshot {
        if capabilities.contains("fleet_v1") {
            let response = try await call([
                "protocol": Self.protocolVersion,
                "kind": "fleet_snapshot",
            ])
            if let fleet = response.fleet {
                return TendExecutionSnapshot(hosts: fleet.hosts)
            }
        }

        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "snapshot",
        ])
        let local = TendFleetHost(
            alias: "local",
            local: true,
            sshTarget: "local://",
            mode: "control",
            state: "online",
            latencyMS: nil,
            error: nil,
            tendVersion: nil,
            daemonGeneration: nil,
            labels: nil,
            annotations: nil,
            remote: TendRemoteSnapshot(agents: response.agents ?? [], panes: [])
        )
        return TendExecutionSnapshot(hosts: [local])
    }

    func captureFleetPane(host: TendFleetHostIdentity, pane: TendPaneInfo) async throws -> TendPaneCapture {
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
                throw TendIPCError.missingField("fleet_result")
            }
            guard result.accepted else {
                throw TendIPCError.server(result.message ?? "Pane capture was rejected")
            }
            let rawBytes = result.captureRawBase64.flatMap { Data(base64Encoded: $0) }
            if result.captureRawBase64 != nil, rawBytes == nil {
                throw TendIPCError.invalidBase64
            }
            return TendPaneCapture(screenText: result.capture, rawBytes: rawBytes)
        }

        guard host.local else {
            throw TendIPCError.server("Remote pane capture requires tendd multi-host support (fleet_v1)")
        }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "capture",
            "pane": pane.paneID,
        ])
        return TendPaneCapture(
            screenText: response.capture,
            rawBytes: response.capture.map { Data($0.utf8) }
        )
    }

    /// Presses navigation keys in a pane (`fleet_keys_v1`): "up", "down",
    /// "enter", "escape", "tab", and "space" for checkbox choices.
    func sendFleetKeys(host: TendFleetHostIdentity, pane: TendPaneInfo, keys: [String]) async throws {
        guard capabilities.contains("fleet_keys_v1") else {
            throw TendIPCError.server("Choosing from Home needs an updated tendd; update tend and restart tendd")
        }
        if keys.contains("space"), !capabilities.contains("fleet_multiselect_v1") {
            throw TendIPCError.server(String(localized: "Multiple selections need an updated tendd; update tend and restart tendd"))
        }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "fleet_command",
            "host": host.alias,
            "operation": [
                "kind": "send_keys",
                "pane": Self.paneKey(pane),
                "keys": keys,
            ],
        ])
        guard let result = response.fleetResult else {
            throw TendIPCError.missingField("fleet_result")
        }
        guard result.accepted else {
            throw TendIPCError.server(result.message ?? "Keys were rejected")
        }
    }

    /// One page of open review items on this Mac's daemon
    /// (`review_items_v1`), oldest first so the longest-waiting request is
    /// always reachable, and whether more follow. A review item is someone's
    /// explicit request that a result be looked at; tend never creates one
    /// on its own.
    func reviewItems(offset: Int = 0, limit: Int = 50) async throws -> (items: [TendReviewItem], hasMore: Bool) {
        guard capabilities.contains("review_items_v1") else { return ([], false) }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "review_list",
            "open_only": true,
            "oldest_first": true,
            "limit": limit,
            "offset": offset,
        ])
        return (response.reviewItems ?? [], response.reviewHasMore ?? false)
    }

    /// One page of every review item, decided ones included, newest first
    /// (`review_items_v1`): what Home reads to know which results tendd
    /// recorded as reviewed.
    func reviewPage(offset: Int, limit: Int) async throws -> (items: [TendReviewItem], hasMore: Bool) {
        guard capabilities.contains("review_items_v1") else { return ([], false) }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "review_list",
            "open_only": false,
            "oldest_first": false,
            "limit": limit,
            "offset": offset,
        ], timeout: 10)
        return (response.reviewItems ?? [], response.reviewHasMore ?? false)
    }

    /// Everything pending on the person on this Mac's daemon, in its order
    /// (`decision_queue_v1`), read as the operator console so human
    /// requests are included. nil from a daemon without the queue: the
    /// caller falls back to the review list and the console mailbox.
    func decisionQueue(limit: Int = 200) async throws -> TendPendingQueue? {
        guard capabilities.contains(tendDecisionQueueCapability) else { return nil }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "decision_queue",
            "limit": limit,
            "origin": ["pane": "", "console": true],
        ], timeout: 10)
        guard let queue = response.decisionQueue?.value else {
            throw TendIPCError.server(String(localized: "tendd sent a decision queue this app cannot read"))
        }
        return queue
    }

    /// `work_backlog` at `stage`; callers check `work_backlog_v1` first.
    /// A record this app cannot read is dropped, not fatal.
    func workRecords(stage: String) async throws -> [TendWorkRecord] {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "work_backlog",
            "stage": stage,
        ], timeout: 10)
        return (response.workRecords ?? []).compactMap(\.value)
    }

    /// Accept, return or withdraw a review item, as the person.
    func decideReview(id: String, disposition: TendReviewDisposition, note: String?) async throws {
        var request: [String: Any] = [
            "protocol": Self.protocolVersion,
            "kind": "review_dispose",
            "id": id,
            "disposition": disposition.rawValue,
            "by": ["kind": "user", "label": "Tend.app"],
        ]
        if let note, !note.isEmpty { request["note"] = note }
        _ = try await call(request)
    }

    /// Records that the person reviewed a result, asked and accepted in one
    /// step (`review_record_v1`), so it shows in `tend review list --all` on
    /// this Mac. Idempotent per `key`. False when the daemon cannot record.
    @discardableResult
    func recordReviewed(target: TendReviewTargetRef, key: String, summary: String) async throws -> Bool {
        try await recordDecision(target: target, key: key, summary: summary, disposition: .accepted, note: nil)
    }

    /// Records a decision the person made from Home, claimed and disposed
    /// in one step (`review_record_v1`): Looks Good is `accepted`; Send Back
    /// and an answer typed to a waiting agent are `returned` with what they
    /// said as the note. Idempotent per `key`. False when the daemon cannot
    /// record. Text is cleaned and cut to the store's limits.
    @discardableResult
    func recordDecision(
        target: TendReviewTargetRef,
        key: String,
        summary: String,
        disposition: TendReviewDisposition,
        note: String?
    ) async throws -> Bool {
        guard capabilities.contains("review_record_v1") else { return false }
        let summary = TendRecordText.bounded(summary, maxBytes: 1024)
        var request: [String: Any] = [
            "protocol": Self.protocolVersion,
            "kind": "review_record",
            "review": [
                "target": target.json,
                "summary": summary.isEmpty ? String(localized: "Reviewed in Home") : summary,
                "idempotency_key": key,
                "requested_by": ["kind": "user", "label": "Tend.app"],
            ],
            "disposition": disposition.rawValue,
        ]
        if let note = note.map({ TendRecordText.bounded($0, maxBytes: 1024) }), !note.isEmpty {
            request["note"] = note
        }
        _ = try await call(request)
        return true
    }

    /// Past decisions on this Mac's daemon, newest first
    /// (`decision_queue_v1`), read as the operator console so answered
    /// human requests are included. nil from a daemon without it.
    func decisionHistory(limit: Int = 500) async throws -> TendDecisionHistory? {
        guard capabilities.contains(tendDecisionQueueCapability) else { return nil }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "decision_history",
            "limit": limit,
            "origin": ["pane": "", "console": true],
        ], timeout: 10)
        guard let history = response.decisionHistory?.value else {
            throw TendIPCError.server(String(localized: "tendd sent a decision history this app cannot read"))
        }
        return history
    }

    /// The calls a local agent session made since `since` and their
    /// results, from this Mac's own records (`turn_evidence_v1`). nil from
    /// a daemon without it. Never asked for a remote host's session: a
    /// session id means nothing on another host's records.
    func turnEvidence(sessionID: String, since: String) async throws -> TendTurnEvidence? {
        guard capabilities.contains("turn_evidence_v1") else { return nil }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "turn_evidence",
            "session_id": sessionID,
            "since": since,
        ], timeout: 20)
        return response.turnEvidence
    }

    /// The host's receipted worktrees and what would stop a cleanup from
    /// taking each. Read-only. The host bounds the scan to 20 seconds, so
    /// this waits longer than an ordinary call, on its own transport.
    func fleetWorktrees(host: TendFleetHostIdentity) async throws -> TendWorktreeInventory {
        guard capabilities.contains("fleet_worktrees_v1") else {
            throw TendIPCError.server(String(localized: "Listing worktrees needs an updated tendd; update tend and restart tendd"))
        }
        let result = try await fleetCommand(
            host: host,
            operation: ["kind": "worktree_inventory"],
            requestTransport: workTransport(for: "worktrees-\(host.alias)"),
            timeout: 35
        )
        let rows = result.worktrees ?? []
        return TendWorktreeInventory(
            rows: rows,
            total: max(result.worktreesTotal ?? rows.count, rows.count),
            node: result.worktreesNode
        )
    }

    /// TCP listeners on `host` associated with tend. Read-only. The host
    /// bounds the scan to 20 seconds, so this waits longer, on its own
    /// transport.
    func fleetServers(host: TendFleetHostIdentity) async throws -> TendServerInventory {
        guard capabilities.contains("fleet_servers_v1") else {
            throw TendIPCError.server(String(localized: "Listing servers needs an updated tendd; update tend and restart tendd"))
        }
        let result = try await fleetCommand(
            host: host,
            operation: ["kind": "server_inventory"],
            requestTransport: workTransport(for: "servers-\(host.alias)"),
            timeout: 35
        )
        guard let servers = result.servers else {
            throw TendIPCError.missingField("servers")
        }
        return servers
    }

    /// Sets aside, restores or deletes worktrees on `host`, each named with
    /// the generation, branch and commit it was listed with; the host checks
    /// them again and refuses any that changed. The host bounds a change to
    /// a minute, so this waits longer than that.
    func fleetChangeWorktrees(
        host: TendFleetHostIdentity,
        action: TendWorktreeAction,
        items: [TendWorktreeChangeItem],
        node: String?
    ) async throws -> [TendWorktreeChangeOutcome] {
        guard capabilities.contains("fleet_worktree_change_v1") else {
            throw TendIPCError.server(String(localized: "Changing worktrees needs an updated tendd; update tend and restart tendd"))
        }
        var change: [String: Any] = [
            "op_id": UUID().uuidString.lowercased(),
            "action": action.rawValue,
            "items": items.map(\.json),
        ]
        if let node { change["expected_node"] = node }
        let result = try await fleetCommand(
            host: host,
            operation: ["kind": "worktree_change", "change": change],
            requestTransport: workTransport(for: "worktrees-\(host.alias)"),
            timeout: 85
        )
        return result.worktreeChanges ?? []
    }

    /// Every send from the app follows a person's action, so it is declared
    /// as theirs; the daemon stamps the prompt the agent reports with it.
    /// Older daemons ignore the field.
    func sendFleetPrompt(
        host: TendFleetHostIdentity,
        pane: TendPaneInfo,
        text: String,
        submit: Bool = true
    ) async throws {
        guard capabilities.contains("fleet_v1") else {
            throw TendIPCError.server("Prompt control requires tendd multi-host support (fleet_v1)")
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
                "origin": "person_via_tend",
            ],
        ])
        guard let result = response.fleetResult else {
            throw TendIPCError.missingField("fleet_result")
        }
        guard result.accepted else {
            throw TendIPCError.server(result.message ?? "Prompt was rejected")
        }
    }

    func listAskEntries() async throws -> [TendAskEntry] {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_list",
        ])
        return response.askEntries ?? []
    }

    func listAskConversations() async throws -> TendAskConversationSnapshot {
        guard capabilities.contains(Self.askConversationCapability) else {
            return TendAskConversationSnapshot(conversations: [], active: nil)
        }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_conversation_list",
        ])
        return TendAskConversationSnapshot(
            conversations: response.askConversations ?? [],
            active: response.askConversation
        )
    }

    func selectAskConversation(_ conversationID: String) async throws -> TendAskConversation {
        guard capabilities.contains(Self.askConversationCapability) else {
            throw TendIPCError.server(
                "The running tendd does not support resumable Ask conversations; update tend or choose Use Bundled tendd"
            )
        }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_conversation_select",
            "conversation_id": conversationID,
        ])
        guard let conversation = response.askConversation else {
            throw TendIPCError.missingField("ask_conversation")
        }
        return conversation
    }

    func askStatus() async throws -> Bool {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_status",
        ])
        guard let enabled = response.askEnabled else {
            throw TendIPCError.missingField("ask_enabled")
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

    /// The daemon's Ask providers (`ask_providers`, capability
    /// `ask_providers_v1`). Callers fall back to `TendAskProvider.builtIn`
    /// when the capability is absent; see `AskProviderStore.reload`.
    func listAskProviders() async throws -> [TendAskProvider] {
        guard capabilities.contains(Self.askProvidersCapability) else {
            throw TendIPCError.server(
                "The running tendd does not list Ask providers; update tend or choose Use Bundled tendd"
            )
        }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_providers",
        ])
        guard let providers = response.askProviders else {
            throw TendIPCError.missingField("ask_providers")
        }
        return providers
    }

    /// Writes `title` / `model` / `api_key_env` / `executable` under
    /// `[ask.providers.<id>]` in the daemon's config and returns the
    /// refreshed provider list. `.keep` leaves the key out of the request so
    /// the daemon does not touch it, `.clear` sends null so it is removed.
    func configureAskProvider(
        _ providerID: String,
        title: TendAskProviderFieldUpdate = .keep,
        model: TendAskProviderFieldUpdate = .keep,
        apiKeyEnv: TendAskProviderFieldUpdate = .keep,
        executable: TendAskProviderFieldUpdate = .keep
    ) async throws -> [TendAskProvider] {
        guard capabilities.contains(Self.askProvidersCapability) else {
            throw TendIPCError.server(
                "The running tendd cannot configure Ask providers; update tend or choose Use Bundled tendd"
            )
        }
        var request: [String: Any] = [
            "protocol": Self.protocolVersion,
            "kind": "ask_provider_configure",
            "provider": providerID,
        ]
        title.apply(to: &request, key: "title")
        model.apply(to: &request, key: "model")
        apiKeyEnv.apply(to: &request, key: "api_key_env")
        executable.apply(to: &request, key: "executable")
        let response = try await call(request, timeout: 10)
        guard let providers = response.askProviders else {
            throw TendIPCError.missingField("ask_providers")
        }
        return providers
    }

    /// Writes a new `[ask.providers.<id>]` entry (`ask_provider_add`) and
    /// returns the refreshed list. `id` is the config key, the Ask agent
    /// name and the Keychain account, so several instances of one engine
    /// stay independent. Optional fields are sent only when non-empty; the
    /// daemon refuses an id that is not a TOML bare key, an unknown engine,
    /// and an id that is already configured.
    func addAskProvider(
        id providerID: String,
        engine: String,
        title: String? = nil,
        model: String? = nil,
        apiKeyEnv: String? = nil,
        executable: String? = nil
    ) async throws -> [TendAskProvider] {
        guard capabilities.contains(Self.askProvidersCapability) else {
            throw TendIPCError.server(
                "The running tendd cannot add Ask providers; update tend or choose Use Bundled tendd"
            )
        }
        var request: [String: Any] = [
            "protocol": Self.protocolVersion,
            "kind": "ask_provider_add",
            "id": providerID,
            "engine": engine,
        ]
        for (key, value) in [
            ("title", title), ("model", model),
            ("api_key_env", apiKeyEnv), ("executable", executable),
        ] {
            if case .set(let text) = TendAskProviderFieldUpdate(value) {
                request[key] = text
            }
        }
        let response = try await call(request, timeout: 10)
        guard let providers = response.askProviders else {
            throw TendIPCError.missingField("ask_providers")
        }
        return providers
    }

    /// Deletes `[ask.providers.<id>]` (`ask_provider_remove`) and returns the
    /// refreshed list. The daemon refuses a built-in that has no config entry
    /// and re-points `[ask] agent` when it named the removed instance.
    func removeAskProvider(_ providerID: String) async throws -> [TendAskProvider] {
        guard capabilities.contains(Self.askProvidersCapability) else {
            throw TendIPCError.server(
                "The running tendd cannot remove Ask providers; update tend or choose Use Bundled tendd"
            )
        }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_provider_remove",
            "id": providerID,
        ], timeout: 10)
        guard let providers = response.askProviders else {
            throw TendIPCError.missingField("ask_providers")
        }
        return providers
    }

    func sendAsk(
        _ prompt: String,
        agent: String? = nil,
        apiKey: String? = nil
    ) async throws -> TendAskEntry {
        var request: [String: Any] = [
            "protocol": Self.protocolVersion,
            "kind": "ask_send",
            "prompt": prompt,
        ]
        if let agent, let apiKey, !apiKey.isEmpty {
            guard capabilities.contains(Self.askCredentialCapability) else {
                throw TendIPCError.server(
                    "The running tendd is too old for Keychain API keys; choose Use Bundled tendd and retry"
                )
            }
            request["credential"] = ["agent": agent, "api_key": apiKey]
        }
        let response = try await call(request)
        guard let entry = response.askEntry else {
            throw TendIPCError.missingField("ask_entry")
        }
        return entry
    }

    func resetAskConversation() async throws -> TendAskConversation? {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_reset",
        ])
        return response.askConversation
    }

    func sendCollaboration(
        host: TendFleetHostIdentity,
        pane: TendPaneInfo,
        kind: String,
        body: String,
        workMode: String
    ) async throws -> TendCollaborationRequest {
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
                ],
            ]
        )
        guard let request = result.collaborationRequest else {
            throw TendIPCError.missingField("fleet_result.collaboration_request")
        }
        return request
    }

    func collaborationMailbox(
        host: TendFleetHostIdentity,
        pane: TendPaneInfo,
        operatorOnly: Bool = false,
        ifUnchanged: String? = nil
    ) async throws -> TendCollaborationMailbox {
        var operation: [String: Any] = [
            "kind": "collaboration_mailbox",
            "operator_only": operatorOnly,
            "pane": Self.paneKey(pane),
        ]
        if let ifUnchanged { operation["if_unchanged"] = ifUnchanged }
        let result = try await fleetCommand(
            host: host,
            operation: operation,
            requestTransport: fleetReadTransport(for: host.alias)
        )
        let unchanged = result.collaborationUnchanged == true
        if unchanged {
            guard let ifUnchanged, result.collaborationReadToken == ifUnchanged,
                  (result.collaborationIncoming ?? []).isEmpty,
                  (result.collaborationSent ?? []).isEmpty else {
                throw TendIPCError.missingField("valid conditional mailbox token")
            }
        }
        return TendCollaborationMailbox(
            incoming: result.collaborationIncoming ?? [],
            sent: result.collaborationSent ?? [],
            readToken: result.collaborationReadToken,
            unchanged: unchanged
        )
    }

    func collaborationRequest(
        host: TendFleetHostIdentity,
        pane: TendPaneInfo,
        requestID: String
    ) async throws -> TendCollaborationRequest {
        let result = try await fleetCommand(
            host: host,
            operation: [
                "kind": "collaboration_get",
                "pane": Self.paneKey(pane),
                "request_id": requestID,
            ]
        )
        guard let request = result.collaborationRequest else {
            throw TendIPCError.missingField("fleet_result.collaboration_request")
        }
        return request
    }

    func claimCollaboration(
        host: TendFleetHostIdentity,
        pane: TendPaneInfo
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
        host: TendFleetHostIdentity,
        pane: TendPaneInfo,
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
    ) async throws -> TendSession {
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
            throw TendIPCError.missingField("session")
        }
        return session
    }

    func readSession(
        id: String,
        offset: UInt64,
        waitForChanges: Bool = false
    ) async throws -> TendSessionOutput {
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
            throw TendIPCError.missingField("output")
        }
        guard output.bytes != nil else {
            throw TendIPCError.invalidBase64
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

    /// `onReady` receives the stream connection's own `hello` once the
    /// subscription is acknowledged — on every (re)connect, so a quiet daemon
    /// still reports which process now serves the socket.
    func fleetUpdates(
        onReady: (@Sendable (TendDaemonHello) -> Void)? = nil
    ) throws -> AsyncThrowingStream<TendFleetUpdate, Error> {
        guard capabilities.contains(Self.fleetSubscribeCapability) else {
            throw TendIPCError.server(
                "tendd does not support host invalidation subscriptions; update tend and restart tendd"
            )
        }
        let hello = try JSONSerialization.data(withJSONObject: [
            "protocol": Self.protocolVersion,
            "kind": "hello",
            "client": "tend-macos-fleet-stream",
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
                onReady: onReady,
                onLine: { data in
                    continuation.yield(try JSONDecoder().decode(TendFleetUpdate.self, from: data))
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
            throw TendIPCError.server(
                "tendd does not support byte-safe terminal sessions; update tend and restart tendd"
            )
        }
        guard capabilities.contains(Self.attachmentIdentityCapability) else {
            throw TendIPCError.server(
                "tendd does not support crash-safe terminal attachments; update tend and restart tendd"
            )
        }
    }

    private func revisionUpdates(
        kind: String,
        capability: String
    ) throws -> AsyncThrowingStream<UInt64, Error> {
        guard capabilities.contains(capability) else {
            throw TendIPCError.server("tendd does not support \(kind); update tend and restart tendd")
        }
        let hello = try JSONSerialization.data(withJSONObject: [
            "protocol": Self.protocolVersion,
            "kind": "hello",
            "client": "tend-macos-\(kind)",
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
                    let update = try JSONDecoder().decode(TendRevisionUpdate.self, from: data)
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
        host: TendFleetHostIdentity,
        operation: [String: Any],
        requestTransport: SerializedIPCTransport? = nil,
        timeout: TimeInterval = 3
    ) async throws -> TendFleetCommandResult {
        guard capabilities.contains("fleet_v1") else {
            throw TendIPCError.server("This operation requires tendd multi-host support (fleet_v1)")
        }
        let response = try await call(
            [
                "protocol": Self.protocolVersion,
                "kind": "fleet_command",
                "host": host.alias,
                "operation": operation,
            ],
            using: requestTransport,
            timeout: timeout
        )
        guard let result = response.fleetResult else {
            throw TendIPCError.missingField("fleet_result")
        }
        guard result.accepted else {
            throw TendIPCError.server(result.message ?? "Host operation was rejected")
        }
        return result
    }

    private func fleetReadTransport(for host: String) -> SerializedIPCTransport {
        let hash = host.utf8.reduce(0) { (partial: Int, byte: UInt8) in
            (partial &* 31) &+ Int(byte)
        }
        return fleetReadTransports[abs(hash % fleetReadTransports.count)]
    }

    private func workTransport(for host: String) -> SerializedIPCTransport {
        if let existing = workTransports[host] { return existing }
        let created = SerializedIPCTransport(label: "dev.tend.mac.ipc-work-\(host)", timedHandler: timedRequestHandler)
        workTransports[host] = created
        return created
    }

    private func defaultTransport(for object: [String: Any]) -> SerializedIPCTransport {
        switch object["kind"] as? String {
        case "fleet_snapshot", "snapshot": return snapshotTransport
        case "list_sessions", "pipeline_runs", "ask_list", "ask_conversations", "ask_status":
            return stateTransport
        case "ask_agent" where object["agent"] == nil: return stateTransport
        default: return transport
        }
    }

    private func terminalReadTransport(for sessionID: String) -> SerializedIPCTransport {
        if let existing = terminalReadTransports[sessionID] { return existing }
        let hash = sessionID.utf8.reduce(0) { (partial: Int, byte: UInt8) in
            (partial &* 31) &+ Int(byte)
        }
        let created = SerializedIPCTransport(
            label: "dev.tend.mac.ipc-terminal-read-\(String(hash, radix: 16))",
            timedHandler: timedRequestHandler
        )
        terminalReadTransports[sessionID] = created
        return created
    }

    private func call(
        _ object: [String: Any],
        using requestTransport: SerializedIPCTransport? = nil,
        timeout: TimeInterval = 3
    ) async throws -> TendIPCResponse {
        let request = try JSONSerialization.data(withJSONObject: object)
        let data = try await (requestTransport ?? defaultTransport(for: object)).request(
            path: socketPath,
            payload: request,
            timeout: timeout
        )
        let response = try JSONDecoder().decode(TendIPCResponse.self, from: data)
        if !response.ok {
            throw TendIPCError.server(response.error ?? "tendd rejected the request")
        }
        return response
    }

    private static func paneKey(_ pane: TendPaneInfo) -> [String: Any] {
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

    /// One request, one connection. A failure to connect or to write means
    /// the request never reached tendd — which is what a restart looks like
    /// from here, and tendd restarts routinely (its binary watch re-execs
    /// it, an app update replaces it). Those two are retried once after a
    /// short pause. A failure while reading is never retried: the daemon may
    /// already have applied the request.
    static func request(
        path: String,
        payload: Data,
        timeout: TimeInterval = 3
    ) throws -> Data {
        var attempt = 0
        while true {
            do {
                return try attemptRequest(path: path, payload: payload, timeout: timeout)
            } catch let error as TendIPCError {
                guard attempt == 0, error.isReconnectable else { throw error }
                attempt += 1
                Thread.sleep(forTimeInterval: 0.25)
            }
        }
    }

    private static func attemptRequest(
        path: String,
        payload: Data,
        timeout: TimeInterval
    ) throws -> Data {
        let descriptor = try connect(path: path, timeout: timeout)
        defer { Darwin.close(descriptor) }
        try writeLine(descriptor: descriptor, payload: payload)
        var buffered = Data()
        do {
            return try readLine(descriptor: descriptor, buffered: &buffered)
        } catch let error as TendIPCError {
            // Mark a read failure so the retry above leaves it alone.
            throw error.notReconnectable
        }
    }

    static func subscribe(
        path: String,
        hello: Data,
        request: Data,
        onReady: (@Sendable (TendDaemonHello) -> Void)? = nil,
        onLine: @escaping @Sendable (Data) throws -> Void,
        onFinish: @escaping @Sendable (Error?) -> Void
    ) -> UnixSocketSubscription {
        UnixSocketSubscription(
            path: path,
            hello: hello,
            request: request,
            onReady: onReady,
            onLine: onLine,
            onFinish: onFinish
        )
    }

    fileprivate static func connect(path: String, timeout: TimeInterval) throws -> Int32 {
        let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else {
            throw TendIPCError.posix(operation: "socket", code: errno)
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
            throw TendIPCError.invalidSocketPath(path)
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
            throw TendIPCError.posix(operation: "connect", code: code)
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
                    throw TendIPCError.posix(operation: "write", code: errno)
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
                throw TendIPCError.posix(operation: "read", code: errno)
            }
            if count == 0 { break }
            buffered.append(contentsOf: buffer.prefix(count))
            if buffered.count > maximumResponseBytes {
                throw TendIPCError.responseTooLarge
            }
        }

        guard !buffered.isEmpty else { throw TendIPCError.emptyResponse }
        let trailing = buffered
        buffered.removeAll(keepingCapacity: true)
        return trailing
    }
}

final class UnixSocketSubscription: @unchecked Sendable {
    private let path: String
    private let hello: Data
    private let request: Data
    private let onReady: (@Sendable (TendDaemonHello) -> Void)?
    private let onLine: @Sendable (Data) throws -> Void
    private let onFinish: @Sendable (Error?) -> Void
    private let queue = DispatchQueue(label: "dev.tend.mac.ipc-fleet-stream", qos: .utility)
    private let lock = NSLock()
    private var descriptor: Int32 = -1
    private var cancelled = false

    init(
        path: String,
        hello: Data,
        request: Data,
        onReady: (@Sendable (TendDaemonHello) -> Void)? = nil,
        onLine: @escaping @Sendable (Data) throws -> Void,
        onFinish: @escaping @Sendable (Error?) -> Void
    ) {
        self.path = path
        self.hello = hello
        self.request = request
        self.onReady = onReady
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
            let helloAck = try validateAck(UnixSocket.readLine(descriptor: socket, buffered: &buffered))
            try UnixSocket.writeLine(descriptor: socket, payload: request)
            try validateAck(UnixSocket.readLine(descriptor: socket, buffered: &buffered))
            onReady?(TendDaemonHello(helloAck))

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

    @discardableResult
    private func validateAck(_ data: Data) throws -> TendIPCResponse {
        let response = try JSONDecoder().decode(TendIPCResponse.self, from: data)
        guard response.ok else {
            throw TendIPCError.server(response.error ?? "tendd rejected the subscription")
        }
        return response
    }
}

/// Public candidate metadata from the daemon's account provider.
struct TendAccountCandidate: Decodable, Sendable, Identifiable {
    let name: String
    let provider: String
    let eligible: Bool
    let active: Bool
    let reason: String
    let remainingPct: Double?
    var id: String { "\(provider)/\(name)" }
    enum CodingKeys: String, CodingKey {
        case name, provider, eligible, active, reason
        case remainingPct = "remainingPct"
    }
}

struct TendProviderCandidates: Decodable, Sendable {
    let provider: String
    let source: String
    let reason: String?
    let candidates: [TendAccountCandidate]
}
