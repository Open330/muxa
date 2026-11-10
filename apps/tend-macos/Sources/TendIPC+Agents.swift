import Foundation

private struct TendAgentStartEnvelope: Decodable {
    let ok: Bool?
    let error: String?
    let agentStart: TendAgentStartResult?

    enum CodingKeys: String, CodingKey {
        case ok, error
        case agentStart = "agent_start"
    }
}

/// `agent_start` on its own connection: tendd runs `tend agent start` to
/// completion before answering, which can take seconds on a fleet host, and
/// that must not hold up the control connection's state polling.
final class TendAgentStartClient: Sendable {
    /// tendd bounds the child to 30 s; a fleet host adds transport time.
    static let requestTimeout: TimeInterval = 50

    let socketPath: String
    private let transport: SerializedIPCTransport

    init(socketPath: String) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.tend.mac.ipc-agent-start") { path, payload in
            try UnixSocket.request(path: path, payload: payload, timeout: Self.requestTimeout)
        }
    }

    /// Test seam: exchanges go through `request` instead of the socket.
    init(socketPath: String, request: @escaping TendIPCRequestHandler) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.tend.mac.ipc-agent-start-test", handler: request)
    }

    static func startRequest(_ request: TendAgentStartRequest) -> [String: Any] {
        [
            "protocol": TendIPCClient.protocolVersion,
            "kind": "agent_start",
            "request": request.ipcPayload(),
        ]
    }

    func start(_ request: TendAgentStartRequest) async throws -> TendAgentStartResult {
        let payload = try JSONSerialization.data(withJSONObject: Self.startRequest(request))
        let data = try await transport.request(path: socketPath, payload: payload, timeout: Self.requestTimeout)
        let response = try JSONDecoder().decode(TendAgentStartEnvelope.self, from: data)
        if response.ok == false {
            throw TendIPCError.server(response.error ?? "tendd rejected the request")
        }
        guard let result = response.agentStart else {
            throw TendIPCError.missingField("agent_start")
        }
        return result
    }
}

extension TendIPCClient {
    static let agentStartCapability = "agent_start_v1"

    nonisolated func makeAgentStartClient() -> TendAgentStartClient {
        TendAgentStartClient(socketPath: socketPath)
    }
}
