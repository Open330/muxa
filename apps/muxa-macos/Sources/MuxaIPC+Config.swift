import Foundation

/// The daemon's `config.toml`, as `config_read` / `config_write` return it.
///
/// `text` is the whole document. `exists` is false for a daemon running on
/// defaults with no file on disk yet; the first write creates it.
struct MuxaDaemonConfigDocument: Decodable, Hashable, Sendable {
    let path: String
    let text: String
    let exists: Bool

    init(path: String, text: String, exists: Bool) {
        self.path = path
        self.text = text
        self.exists = exists
    }

    enum CodingKeys: String, CodingKey {
        case path, text, exists
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        path = try values.decodeIfPresent(String.self, forKey: .path) ?? ""
        text = try values.decodeIfPresent(String.self, forKey: .text) ?? ""
        exists = try values.decodeIfPresent(Bool.self, forKey: .exists) ?? !path.isEmpty
    }

    var url: URL? {
        path.isEmpty ? nil : URL(fileURLWithPath: path)
    }
}

/// The daemon refused a write because the file changed under the editor.
///
/// It answers a stale `expected_text` with both its message and the
/// document as it now stands, so the app can re-apply on top of the current
/// file instead of asking the operator to start over.
struct MuxaConfigConflict: LocalizedError, Sendable {
    let message: String
    let current: MuxaDaemonConfigDocument

    var errorDescription: String? { message }
}

private struct MuxaConfigEnvelope: Decodable {
    let ok: Bool?
    let error: String?
    let config: MuxaDaemonConfigDocument?
}

struct MuxaLaunchProvider: Codable, Hashable, Sendable, Identifiable {
    var program: String
    var options: [String]?
    var effectiveOptions: [String]
    var id: String { program }
    enum CodingKeys: String, CodingKey {
        case program, options
        case effectiveOptions = "effective_options"
    }
}

struct MuxaLaunchPipelineAgent: Codable, Hashable, Sendable, Identifiable {
    var pipeline: String
    var index: Int
    var name: String
    var program: String
    var options: [String]?
    var effectiveOptions: [String]
    var id: String { "\(pipeline.utf8.count):\(pipeline):\(index)" }
    enum CodingKeys: String, CodingKey {
        case pipeline, index, name, program, options
        case effectiveOptions = "effective_options"
    }
}

struct MuxaLaunchSettings: Codable, Hashable, Sendable {
    struct LegacyGuide: Codable, Hashable, Sendable {
        var program: String?
        var options: [String]
    }
    var providers: [MuxaLaunchProvider]
    var pipelines: [MuxaLaunchPipelineAgent]
    var legacyGuide: LegacyGuide
    enum CodingKeys: String, CodingKey {
        case providers, pipelines
        case legacyGuide = "legacy_guide"
    }

    func effectiveOptions(program: String, override: [String]?) -> [String] {
        if let override { return override }
        if let options = providers.first(where: { $0.program == program })?.options { return options }
        return legacyGuide.program == program ? legacyGuide.options : []
    }

    func edits(against baseline: Self) -> [[String: Any]] {
        var edits: [[String: Any]] = []
        for provider in providers where provider.options != baseline.providers.first(where: { $0.id == provider.id })?.options {
            edits.append(["target": "provider", "program": provider.program, "options": provider.options as Any? ?? NSNull()])
        }
        for agent in pipelines where agent.options != baseline.pipelines.first(where: { $0.id == agent.id })?.options {
            edits.append(["target": "pipeline", "pipeline": agent.pipeline, "index": agent.index, "options": agent.options as Any? ?? NSNull()])
        }
        return edits
    }
}

struct MuxaLaunchDocument: Decodable, Sendable {
    let config: MuxaDaemonConfigDocument
    let launch: MuxaLaunchSettings
}

private struct MuxaLaunchEnvelope: Decodable {
    let ok: Bool?
    let error: String?
    let config: MuxaDaemonConfigDocument?
    let launch: MuxaLaunchSettings?
}

/// The `config_read` / `config_write` pair, on its own connection for the
/// same reason `MuxaAutomationClient` has one.
final class MuxaConfigClient: Sendable {
    /// A write parses and validates the whole document before replacing it.
    static let requestTimeout: TimeInterval = 10

    let socketPath: String
    private let transport: SerializedIPCTransport

    init(socketPath: String) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.muxa.mac.ipc-config") { path, payload in
            try UnixSocket.request(path: path, payload: payload, timeout: Self.requestTimeout)
        }
    }

    /// Test seam: exchanges go through `request` instead of the socket.
    init(socketPath: String, request: @escaping MuxaIPCRequestHandler) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.muxa.mac.ipc-config-test", handler: request)
    }

    static func readRequest() -> [String: Any] {
        ["protocol": MuxaIPCClient.protocolVersion, "kind": "config_read"]
    }

    /// `expected_text` is the document the editor was opened on. Sending it
    /// as JSON `null` is the explicit "overwrite whatever is there"; leaving
    /// the key out is not a thing this client does, so a concurrent edit is
    /// always detected unless the caller deliberately forces the write.
    static func writeRequest(text: String, expectedText: String?) -> [String: Any] {
        [
            "protocol": MuxaIPCClient.protocolVersion,
            "kind": "config_write",
            "text": text,
            "expected_text": expectedText ?? NSNull(),
        ]
    }

    func read() async throws -> MuxaDaemonConfigDocument {
        try await document(Self.readRequest())
    }

    func write(text: String, expectedText: String?) async throws -> MuxaDaemonConfigDocument {
        try await document(Self.writeRequest(text: text, expectedText: expectedText))
    }

    func readLaunch() async throws -> MuxaLaunchDocument {
        try await launchDocument(["protocol": MuxaIPCClient.protocolVersion, "kind": "config_launch_read"])
    }

    func writeLaunch(expectedText: String, settings: MuxaLaunchSettings, baseline: MuxaLaunchSettings) async throws -> MuxaLaunchDocument {
        try await launchDocument([
            "protocol": MuxaIPCClient.protocolVersion, "kind": "config_launch_write",
            "expected_text": expectedText, "edits": settings.edits(against: baseline),
        ])
    }

    private func launchDocument(_ object: [String: Any]) async throws -> MuxaLaunchDocument {
        let payload = try JSONSerialization.data(withJSONObject: object)
        let data = try await transport.request(path: socketPath, payload: payload, timeout: Self.requestTimeout)
        let response = try JSONDecoder().decode(MuxaLaunchEnvelope.self, from: data)
        if response.ok == false {
            let message = response.error ?? "muxad rejected the request"
            if let current = response.config { throw MuxaConfigConflict(message: message, current: current) }
            throw MuxaIPCError.server(message)
        }
        guard let config = response.config, let launch = response.launch else {
            throw MuxaIPCError.missingField("config / launch")
        }
        return MuxaLaunchDocument(config: config, launch: launch)
    }

    private func document(_ object: [String: Any]) async throws -> MuxaDaemonConfigDocument {
        let payload = try JSONSerialization.data(withJSONObject: object)
        let data = try await transport.request(
            path: socketPath,
            payload: payload,
            timeout: Self.requestTimeout
        )
        let response = try JSONDecoder().decode(MuxaConfigEnvelope.self, from: data)
        if response.ok == false {
            let message = response.error ?? "muxad rejected the request"
            // A refusal that still carries a document is the concurrent-edit
            // case: that document is what is on disk now. A parse or
            // validation refusal changes nothing and carries none.
            if let current = response.config {
                throw MuxaConfigConflict(message: message, current: current)
            }
            throw MuxaIPCError.server(message)
        }
        guard let config = response.config else {
            throw MuxaIPCError.missingField("config")
        }
        return config
    }
}

extension MuxaIPCClient {
    static let configEditCapability = "config_edit_v1"
    static let configLaunchCapability = "config_launch_v1"

    nonisolated func makeConfigClient() -> MuxaConfigClient {
        MuxaConfigClient(socketPath: socketPath)
    }

    private func requireConfigEdit() throws -> MuxaConfigClient {
        guard supports(Self.configEditCapability) else {
            throw MuxaIPCError.server(
                "muxad does not support editing muxa configuration; update muxa and restart muxad"
            )
        }
        return makeConfigClient()
    }

    func readDaemonConfig() async throws -> MuxaDaemonConfigDocument {
        try await requireConfigEdit().read()
    }

    func writeDaemonConfig(
        text: String,
        expectedText: String?
    ) async throws -> MuxaDaemonConfigDocument {
        try await requireConfigEdit().write(text: text, expectedText: expectedText)
    }
}
