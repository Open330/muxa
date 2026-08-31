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

struct MuxaWorkStartResult: Decodable, Sendable {
    let work: String
    let workspace: String
    let pipeline: String?
    let cwd: String?
    let dryRun: Bool?

    enum CodingKeys: String, CodingKey {
        case work, workspace, pipeline, cwd
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

struct MuxaPipelineRun: Decodable, Identifiable, Sendable {
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

struct MuxaAgent: Decodable, Identifiable, Sendable {
    let kind: String
    let agentSessionID: String
    let pane: String?
    let tmuxSocket: String?
    let tmuxSession: String?
    let cwd: String?
    let state: String
    let lastPrompt: String?
    let lastResponse: String?
    let recap: String?
    let aiTitle: String?
    let lastNotification: String?
    let model: String?
    let contextUsedPercent: Double?
    let costUSD: Double?

    var id: String { agentSessionID }

    enum CodingKeys: String, CodingKey {
        case kind, pane, cwd, state, recap, model
        case agentSessionID = "agent_session_id"
        case tmuxSocket = "tmux_socket"
        case tmuxSession = "tmux_session"
        case lastPrompt = "last_prompt"
        case lastResponse = "last_response"
        case aiTitle = "ai_title"
        case lastNotification = "last_notification"
        case contextUsedPercent = "context_used_pct"
        case costUSD = "cost_usd"
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
        case agentRole = "agent_role"
        case agentAlias = "agent_alias"
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

    static let empty = MuxaExecutionSnapshot(hosts: [])

    var agents: [MuxaAgent] {
        hosts.flatMap { $0.remote?.agents ?? [] }
    }

    var panes: [MuxaPaneInfo] {
        hosts.flatMap { $0.remote?.panes ?? [] }
    }

    var hostedAgents: [MuxaHostedAgent] {
        hosts.flatMap { host -> [MuxaHostedAgent] in
            guard let remote = host.remote else { return [] }
            return remote.agents.map { agent in
                MuxaHostedAgent(
                    host: host.identity,
                    agent: agent,
                    pane: Self.pane(for: agent, among: remote.panes)
                )
            }
        }
    }

    func pane(for agent: MuxaAgent) -> MuxaPaneInfo? {
        let matches = hosts.compactMap { host -> MuxaPaneInfo? in
            guard let panes = host.remote?.panes else { return nil }
            return Self.pane(for: agent, among: panes)
        }
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
        let exact = agents.filter {
            $0.pane == pane.paneID && ($0.tmuxSocket == nil || $0.tmuxSocket == pane.socket)
        }
        return exact.count == 1 ? exact[0] : nil
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

struct MuxaFleetHost: Decodable, Identifiable, Sendable {
    let alias: String
    let local: Bool
    let sshTarget: String?
    let mode: String
    let state: String
    let latencyMS: UInt64?
    let error: String?
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
    }
}

struct MuxaRemoteSnapshot: Decodable, Sendable {
    let agents: [MuxaAgent]
    let panes: [MuxaPaneInfo]
}

struct MuxaPaneCapture: Sendable {
    let screenText: String?
    let rawBytes: Data?
}

struct MuxaAskEntry: Decodable, Identifiable, Sendable {
    let id: String
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
        case agentSessionID = "agent_session_id"
        case askedAt = "asked_at"
        case answeredAt = "answered_at"
        case costUSD = "cost_usd"
    }
}

struct MuxaCollaborationRoom: Decodable, Sendable {
    let host: String
    let socket: String?
    let windowID: String

    enum CodingKeys: String, CodingKey {
        case host, socket
        case windowID = "window_id"
    }
}

struct MuxaCollaborationParticipant: Decodable, Sendable {
    let agentKind: String
    let agentSessionID: String
    let pane: String
    let socket: String?
    let room: MuxaCollaborationRoom
    let alias: String?
    let roles: [String]?
    let console: Bool?

    enum CodingKeys: String, CodingKey {
        case pane, socket, room, alias, roles, console
        case agentKind = "agent_kind"
        case agentSessionID = "agent_session_id"
    }

    var label: String {
        if console == true { return "operator" }
        if let alias, !alias.isEmpty { return "@\(alias)" }
        return "\(agentKind)@\(pane)"
    }
}

struct MuxaCollaborationReply: Decodable, Sendable {
    let status: String
    let body: String
    let at: String
}

struct MuxaCollaborationRequest: Decodable, Identifiable, Sendable {
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

    enum CodingKeys: String, CodingKey {
        case id, from, to, kind, body, status, reply
        case expectsReply = "expects_reply"
        case workMode = "work_mode"
        case createdAt = "created_at"
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
    let askAgent: String?

    enum CodingKeys: String, CodingKey {
        case ok, error, capabilities, agents, sessions, session, output, capture, fleet
        case minProtocol = "min_protocol"
        case maxProtocol = "max_protocol"
        case fleetResult = "fleet_result"
        case pipelineRuns = "pipeline_runs"
        case workOperation = "work_operation"
        case askEntries = "ask_entries"
        case askEntry = "ask_entry"
        case askAgent = "ask_agent"
    }
}

typealias MuxaIPCRequestHandler = @Sendable (String, Data) throws -> Data

/// Runs the blocking Unix-socket exchange on one serial queue.
///
/// Actor methods are reentrant while awaiting detached work. The previous
/// implementation therefore allowed every key, resize, refresh, and polling
/// request to create another blocked worker when muxad was slow or restarting.
/// Keeping exactly one blocking exchange active bounds thread and descriptor
/// use while preserving wire order.
final class SerializedIPCTransport: @unchecked Sendable {
    private let queue = DispatchQueue(
        label: "dev.muxa.mac.ipc-transport",
        qos: .userInitiated
    )
    private let handler: MuxaIPCRequestHandler

    init(handler: @escaping MuxaIPCRequestHandler) {
        self.handler = handler
    }

    func request(path: String, payload: Data) async throws -> Data {
        try Task.checkCancellation()
        let handler = self.handler
        return try await withCheckedThrowingContinuation { continuation in
            queue.async {
                do {
                    continuation.resume(returning: try handler(path, payload))
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

    let socketPath: String
    private(set) var capabilities: Set<String> = []
    private let transport: SerializedIPCTransport
    private let attachmentClientID: String

    init(socketPath: String = MuxaIPCClient.defaultSocketPath()) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(handler: UnixSocket.request)
        attachmentClientID = "muxa-macos:\(getuid())"
    }

    init(socketPath: String, request: @escaping MuxaIPCRequestHandler) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(handler: request)
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
            labels: nil,
            annotations: nil,
            remote: MuxaRemoteSnapshot(agents: response.agents ?? [], panes: [])
        )
        return MuxaExecutionSnapshot(hosts: [local])
    }

    func captureFleetPane(host: MuxaFleetHostIdentity, pane: MuxaPaneInfo) async throws -> MuxaPaneCapture {
        if capabilities.contains("fleet_v1") {
            let response = try await call([
                "protocol": Self.protocolVersion,
                "kind": "fleet_command",
                "host": host.alias,
                "operation": [
                    "kind": "capture",
                    "pane": Self.paneKey(pane),
                ],
            ])
            guard let result = response.fleetResult else {
                throw MuxaIPCError.missingField("fleet_result")
            }
            guard result.accepted else {
                throw MuxaIPCError.server(result.message ?? "Fleet capture was rejected")
            }
            let rawBytes = result.captureRawBase64.flatMap { Data(base64Encoded: $0) }
            if result.captureRawBase64 != nil, rawBytes == nil {
                throw MuxaIPCError.invalidBase64
            }
            return MuxaPaneCapture(screenText: result.capture, rawBytes: rawBytes)
        }

        guard host.local else {
            throw MuxaIPCError.server("Remote pane capture requires muxad Fleet support")
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
            throw MuxaIPCError.server("Prompt control requires muxad Fleet support")
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
            throw MuxaIPCError.server(result.message ?? "Fleet prompt was rejected")
        }
    }

    func listAskEntries() async throws -> [MuxaAskEntry] {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_list",
        ])
        return response.askEntries ?? []
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

    func sendAsk(_ prompt: String) async throws -> MuxaAskEntry {
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_send",
            "prompt": prompt,
        ])
        guard let entry = response.askEntry else {
            throw MuxaIPCError.missingField("ask_entry")
        }
        return entry
    }

    func resetAskConversation() async throws {
        _ = try await call([
            "protocol": Self.protocolVersion,
            "kind": "ask_reset",
        ])
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
            ]
        )
        return MuxaCollaborationMailbox(
            incoming: result.collaborationIncoming ?? [],
            sent: result.collaborationSent ?? []
        )
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

    func readSession(id: String, offset: UInt64) async throws -> MuxaSessionOutput {
        try requireNativeSessionCapabilities()
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "read_session",
            "session_id": id,
            "offset": offset,
        ])
        guard let output = response.output else {
            throw MuxaIPCError.missingField("output")
        }
        guard output.bytes != nil else {
            throw MuxaIPCError.invalidBase64
        }
        return output
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

    private func fleetCommand(
        host: MuxaFleetHostIdentity,
        operation: [String: Any]
    ) async throws -> MuxaFleetCommandResult {
        guard capabilities.contains("fleet_v1") else {
            throw MuxaIPCError.server("This operation requires muxad Fleet support")
        }
        let response = try await call([
            "protocol": Self.protocolVersion,
            "kind": "fleet_command",
            "host": host.alias,
            "operation": operation,
        ])
        guard let result = response.fleetResult else {
            throw MuxaIPCError.missingField("fleet_result")
        }
        guard result.accepted else {
            throw MuxaIPCError.server(result.message ?? "Fleet operation was rejected")
        }
        return result
    }

    private func call(_ object: [String: Any]) async throws -> MuxaIPCResponse {
        let request = try JSONSerialization.data(withJSONObject: object)
        let data = try await transport.request(path: socketPath, payload: request)
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

    static func request(path: String, payload: Data) throws -> Data {
        let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else {
            throw MuxaIPCError.posix(operation: "socket", code: errno)
        }
        defer { Darwin.close(descriptor) }

        var noSignal: Int32 = 1
        let noSignalSize = socklen_t(MemoryLayout<Int32>.size)
        _ = withUnsafePointer(to: &noSignal) {
            setsockopt(descriptor, SOL_SOCKET, SO_NOSIGPIPE, $0, noSignalSize)
        }

        var timeout = timeval(tv_sec: 3, tv_usec: 0)
        let timeoutSize = socklen_t(MemoryLayout<timeval>.size)
        _ = withUnsafePointer(to: &timeout) {
            setsockopt(descriptor, SOL_SOCKET, SO_RCVTIMEO, $0, timeoutSize)
        }
        _ = withUnsafePointer(to: &timeout) {
            setsockopt(descriptor, SOL_SOCKET, SO_SNDTIMEO, $0, timeoutSize)
        }

        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(path.utf8CString)
        let capacity = MemoryLayout.size(ofValue: address.sun_path)
        guard pathBytes.count <= capacity else {
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
            throw MuxaIPCError.posix(operation: "connect", code: errno)
        }

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

        var response = Data()
        var buffer = [UInt8](repeating: 0, count: 16 * 1024)
        while true {
            let count = Darwin.read(descriptor, &buffer, buffer.count)
            if count < 0, errno == EINTR { continue }
            if count < 0 {
                throw MuxaIPCError.posix(operation: "read", code: errno)
            }
            if count == 0 { break }
            response.append(contentsOf: buffer.prefix(count))
            if response.count > maximumResponseBytes {
                throw MuxaIPCError.responseTooLarge
            }
            if let newline = response.firstIndex(of: 0x0A) {
                return response.prefix(upTo: newline)
            }
        }

        guard !response.isEmpty else { throw MuxaIPCError.emptyResponse }
        return response
    }
}
