import Foundation

private struct MuxaAgentStartEnvelope: Decodable {
    let ok: Bool?
    let error: String?
    let agentStart: MuxaAgentStartResult?

    enum CodingKeys: String, CodingKey {
        case ok, error
        case agentStart = "agent_start"
    }
}

/// `agent_start` on its own connection: muxad runs `muxa agent start` to
/// completion before answering, which can take seconds on a fleet host, and
/// that must not hold up the control connection's state polling.
final class MuxaAgentStartClient: Sendable {
    /// muxad bounds the child to 30 s; a fleet host adds transport time.
    static let requestTimeout: TimeInterval = 50

    let socketPath: String
    private let transport: SerializedIPCTransport

    init(socketPath: String) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.muxa.mac.ipc-agent-start") { path, payload in
            try UnixSocket.request(path: path, payload: payload, timeout: Self.requestTimeout)
        }
    }

    /// Test seam: exchanges go through `request` instead of the socket.
    init(socketPath: String, request: @escaping MuxaIPCRequestHandler) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.muxa.mac.ipc-agent-start-test", handler: request)
    }

    static func startRequest(_ request: MuxaAgentStartRequest) -> [String: Any] {
        [
            "protocol": MuxaIPCClient.protocolVersion,
            "kind": "agent_start",
            "request": request.ipcPayload(),
        ]
    }

    func start(_ request: MuxaAgentStartRequest) async throws -> MuxaAgentStartResult {
        let payload = try JSONSerialization.data(withJSONObject: Self.startRequest(request))
        let data = try await transport.request(path: socketPath, payload: payload, timeout: Self.requestTimeout)
        let response = try JSONDecoder().decode(MuxaAgentStartEnvelope.self, from: data)
        if response.ok == false {
            throw MuxaIPCError.server(response.error ?? "muxad rejected the request")
        }
        guard let result = response.agentStart else {
            throw MuxaIPCError.missingField("agent_start")
        }
        return result
    }
}

extension MuxaIPCClient {
    static let agentStartCapability = "agent_start_v1"

    nonisolated func makeAgentStartClient() -> MuxaAgentStartClient {
        MuxaAgentStartClient(socketPath: socketPath)
    }
}
