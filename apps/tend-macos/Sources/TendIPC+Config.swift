import Foundation

/// The daemon's `config.toml`, as `config_read` / `config_write` return it.
///
/// `text` is the whole document. `exists` is false for a daemon running on
/// defaults with no file on disk yet; the first write creates it.
struct TendDaemonConfigDocument: Decodable, Hashable, Sendable {
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
struct TendConfigConflict: LocalizedError, Sendable {
    let message: String
    let current: TendDaemonConfigDocument

    var errorDescription: String? { message }
}

private struct TendConfigEnvelope: Decodable {
    let ok: Bool?
    let error: String?
    let config: TendDaemonConfigDocument?
}

struct TendLaunchProvider: Codable, Hashable, Sendable, Identifiable {
    var program: String
    var options: [String]?
    var effectiveOptions: [String]
    var id: String { program }
    enum CodingKeys: String, CodingKey {
        case program, options
        case effectiveOptions = "effective_options"
    }
}

struct TendLaunchPipelineAgent: Codable, Hashable, Sendable, Identifiable {
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

struct TendLaunchSettings: Codable, Hashable, Sendable {
    struct LegacyGuide: Codable, Hashable, Sendable {
        var program: String?
        var options: [String]
        // WS-B: new agent — `[mcp.guide]` placement/direction; absent from
        // an older tendd.
        var placement: String? = nil
        var direction: String? = nil
    }
    var providers: [TendLaunchProvider]
    var pipelines: [TendLaunchPipelineAgent]
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

    /// Launch arguments as a person reads them: space-separated, with an
    /// empty or space-bearing argument quoted so its boundaries stay
    /// visible, and "None" for an empty list.
    static func displayText(_ arguments: [String]) -> String {
        guard !arguments.isEmpty else { return String(localized: "None") }
        return arguments.map { argument in
            let needsQuotes = argument.isEmpty
                || argument.contains(where: { $0.isWhitespace || $0 == "\"" || $0 == "\\" })
            guard needsQuotes else { return argument }
            let escaped = argument
                .replacingOccurrences(of: "\\", with: "\\\\")
                .replacingOccurrences(of: "\"", with: "\\\"")
            return "\"\(escaped)\""
        }.joined(separator: " ")
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

struct TendLaunchDocument: Decodable, Sendable {
    let config: TendDaemonConfigDocument
    let launch: TendLaunchSettings
}

private struct TendLaunchEnvelope: Decodable {
    let ok: Bool?
    let error: String?
    let config: TendDaemonConfigDocument?
    let launch: TendLaunchSettings?
}

/// The `config_read` / `config_write` pair, on its own connection for the
/// same reason `TendAutomationClient` has one.
final class TendConfigClient: Sendable {
    /// A write parses and validates the whole document before replacing it.
    static let requestTimeout: TimeInterval = 10

    let socketPath: String
    private let transport: SerializedIPCTransport

    init(socketPath: String) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.tend.mac.ipc-config") { path, payload in
            try UnixSocket.request(path: path, payload: payload, timeout: Self.requestTimeout)
        }
    }

    /// Test seam: exchanges go through `request` instead of the socket.
    init(socketPath: String, request: @escaping TendIPCRequestHandler) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.tend.mac.ipc-config-test", handler: request)
    }

    static func readRequest() -> [String: Any] {
        ["protocol": TendIPCClient.protocolVersion, "kind": "config_read"]
    }

    /// `expected_text` is the document the editor was opened on. Sending it
    /// as JSON `null` is the explicit "overwrite whatever is there"; leaving
    /// the key out is not a thing this client does, so a concurrent edit is
    /// always detected unless the caller deliberately forces the write.
    static func writeRequest(text: String, expectedText: String?) -> [String: Any] {
        [
            "protocol": TendIPCClient.protocolVersion,
            "kind": "config_write",
            "text": text,
            "expected_text": expectedText ?? NSNull(),
        ]
    }

    func read() async throws -> TendDaemonConfigDocument {
        try await document(Self.readRequest())
    }

    func write(text: String, expectedText: String?) async throws -> TendDaemonConfigDocument {
        try await document(Self.writeRequest(text: text, expectedText: expectedText))
    }

    func readLaunch() async throws -> TendLaunchDocument {
        try await launchDocument(["protocol": TendIPCClient.protocolVersion, "kind": "config_launch_read"])
    }

    func writeLaunch(expectedText: String, settings: TendLaunchSettings, baseline: TendLaunchSettings) async throws -> TendLaunchDocument {
        try await launchDocument([
            "protocol": TendIPCClient.protocolVersion, "kind": "config_launch_write",
            "expected_text": expectedText, "edits": settings.edits(against: baseline),
        ])
    }

    private func launchDocument(_ object: [String: Any]) async throws -> TendLaunchDocument {
        let payload = try JSONSerialization.data(withJSONObject: object)
        let data = try await transport.request(path: socketPath, payload: payload, timeout: Self.requestTimeout)
        let response = try JSONDecoder().decode(TendLaunchEnvelope.self, from: data)
        if response.ok == false {
            let message = response.error ?? "tendd rejected the request"
            if let current = response.config { throw TendConfigConflict(message: message, current: current) }
            throw TendIPCError.server(message)
        }
        guard let config = response.config, let launch = response.launch else {
            throw TendIPCError.missingField("config / launch")
        }
        return TendLaunchDocument(config: config, launch: launch)
    }

    private func document(_ object: [String: Any]) async throws -> TendDaemonConfigDocument {
        let payload = try JSONSerialization.data(withJSONObject: object)
        let data = try await transport.request(
            path: socketPath,
            payload: payload,
            timeout: Self.requestTimeout
        )
        let response = try JSONDecoder().decode(TendConfigEnvelope.self, from: data)
        if response.ok == false {
            let message = response.error ?? "tendd rejected the request"
            // A refusal that still carries a document is the concurrent-edit
            // case: that document is what is on disk now. A parse or
            // validation refusal changes nothing and carries none.
            if let current = response.config {
                throw TendConfigConflict(message: message, current: current)
            }
            throw TendIPCError.server(message)
        }
        guard let config = response.config else {
            throw TendIPCError.missingField("config")
        }
        return config
    }
}

extension TendIPCClient {
    static let configEditCapability = "config_edit_v1"
    static let configLaunchCapability = "config_launch_v1"

    nonisolated func makeConfigClient() -> TendConfigClient {
        let socketPath = socketPath
        return configClientCache.value { TendConfigClient(socketPath: socketPath) }
    }

    private func requireConfigEdit() throws -> TendConfigClient {
        guard supports(Self.configEditCapability) else {
            throw TendIPCError.server(
                "tendd does not support editing tend configuration; update tend and restart tendd"
            )
        }
        return makeConfigClient()
    }

    func readDaemonConfig() async throws -> TendDaemonConfigDocument {
        try await requireConfigEdit().read()
    }

    func writeDaemonConfig(
        text: String,
        expectedText: String?
    ) async throws -> TendDaemonConfigDocument {
        try await requireConfigEdit().write(text: text, expectedText: expectedText)
    }
}
