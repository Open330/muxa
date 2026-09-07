import Foundation

/// What the daemon's `work_compose` request (and `muxa work compose --json`)
/// returns: a pipeline that already passed the CLI's validation, the model's
/// remarks outside the JSON block, and the raw answer for the log.
struct MuxaWorkComposeResult: Decodable, Equatable, Sendable {
    let pipeline: MuxaWorkOptions.Pipeline
    let notes: String
    let raw: String

    private enum CodingKeys: String, CodingKey {
        case pipeline, notes, raw
    }

    init(pipeline: MuxaWorkOptions.Pipeline, notes: String = "", raw: String = "") {
        self.pipeline = pipeline
        self.notes = notes
        self.raw = raw
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        pipeline = try values.decode(MuxaWorkOptions.Pipeline.self, forKey: .pipeline)
        notes = try values.decodeIfPresent(String.self, forKey: .notes) ?? ""
        raw = try values.decodeIfPresent(String.self, forKey: .raw) ?? ""
    }

    /// Decodes either the bare object (`muxa work compose --json` prints it)
    /// or a full daemon reply that carries it under `work_compose`.
    static func decode(_ data: Data) throws -> MuxaWorkComposeResult {
        let decoder = JSONDecoder()
        if let envelope = try? decoder.decode(MuxaWorkComposeEnvelope.self, from: data),
           let result = envelope.workCompose {
            return result
        }
        return try decoder.decode(MuxaWorkComposeResult.self, from: data)
    }

    /// The draft as the composer edits it, plus the name the model chose.
    var definition: MuxaPipelineDefinition { MuxaPipelineDefinition(pipeline) }
}

/// One entry of the daemon's `ask_providers` list, reduced to what the
/// composer's picker needs. The Ask settings own the full model.
struct MuxaWorkComposeProvider: Decodable, Equatable, Sendable, Identifiable {
    let id: String
    let title: String
    let selected: Bool

    private enum CodingKeys: String, CodingKey {
        case id, title, selected
    }

    init(id: String, title: String, selected: Bool = false) {
        self.id = id
        self.title = title
        self.selected = selected
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        id = try values.decode(String.self, forKey: .id)
        title = try values.decodeIfPresent(String.self, forKey: .title) ?? id
        selected = try values.decodeIfPresent(Bool.self, forKey: .selected) ?? false
    }
}

/// A one-turn credential for the chosen provider, the shape `ask_send`
/// already carries (`{"agent": …, "api_key": …}`).
struct MuxaWorkComposeCredential: Equatable, Sendable {
    let agent: String
    let apiKey: String

    var payload: [String: String] { ["agent": agent, "api_key": apiKey] }
}

private struct MuxaWorkComposeEnvelope: Decodable {
    let ok: Bool?
    let error: String?
    let workCompose: MuxaWorkComposeResult?
    let askProviders: [MuxaWorkComposeProvider]?

    enum CodingKeys: String, CodingKey {
        case ok, error
        case workCompose = "work_compose"
        case askProviders = "ask_providers"
    }
}

/// Sends `work_compose` over its own connection and queue. A model call can
/// take a minute, and the control transport (keys, resizes, refreshes) must
/// not wait behind it the way it would inside `MuxaIPCClient`'s serialized
/// exchange.
final class MuxaWorkComposeClient: Sendable {
    /// Long enough for a slow provider with one validation retry.
    static let requestTimeout: TimeInterval = 300

    let socketPath: String
    private let transport: SerializedIPCTransport

    init(socketPath: String) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.muxa.mac.ipc-compose") { path, payload in
            try UnixSocket.request(path: path, payload: payload, timeout: Self.requestTimeout)
        }
    }

    /// Test seam: exchanges go through `request` instead of the socket.
    init(socketPath: String, request: @escaping MuxaIPCRequestHandler) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.muxa.mac.ipc-compose-test", handler: request)
    }

    /// The `current` payload: the same JSON `MuxaPipelineDefinition.jsonString()`
    /// produces, plus the `name` the daemon's pipeline shape requires.
    static func currentPayload(_ definition: MuxaPipelineDefinition, name: String?) throws -> [String: Any] {
        let data = Data(try definition.jsonString().utf8)
        var object = try JSONSerialization.jsonObject(with: data) as? [String: Any] ?? [:]
        let trimmed = name?.trimmingCharacters(in: .whitespaces) ?? ""
        object["name"] = trimmed.isEmpty ? "draft" : trimmed
        return object
    }

    /// The wire request, exactly as the daemon documents it. `agent`,
    /// `current`, and `credential` are sent as JSON `null` when absent.
    static func requestObject(
        description: String,
        agent: String?,
        current: MuxaPipelineDefinition?,
        name: String?,
        credential: MuxaWorkComposeCredential?
    ) throws -> [String: Any] {
        var request: [String: Any] = [
            "protocol": MuxaIPCClient.protocolVersion,
            "kind": "work_compose",
            "description": description.trimmingCharacters(in: .whitespacesAndNewlines),
            "agent": NSNull(),
            "current": NSNull(),
            "credential": NSNull(),
        ]
        if let agent = agent?.trimmingCharacters(in: .whitespaces), !agent.isEmpty {
            request["agent"] = agent
        }
        if let current {
            request["current"] = try currentPayload(current, name: name)
        }
        if let credential, !credential.apiKey.isEmpty {
            request["credential"] = credential.payload
        }
        return request
    }

    func composeWork(
        description: String,
        agent: String?,
        current: MuxaPipelineDefinition?,
        name: String?,
        credential: MuxaWorkComposeCredential?
    ) async throws -> MuxaWorkComposeResult {
        let request = try Self.requestObject(
            description: description,
            agent: agent,
            current: current,
            name: name,
            credential: credential
        )
        let response = try await call(request)
        guard let result = response.workCompose else {
            throw MuxaIPCError.missingField("work_compose")
        }
        return result
    }

    /// The daemon's provider list (`ask_providers`), for the composer's picker.
    func providers() async throws -> [MuxaWorkComposeProvider] {
        let response = try await call([
            "protocol": MuxaIPCClient.protocolVersion,
            "kind": "ask_providers",
        ])
        guard let providers = response.askProviders else {
            throw MuxaIPCError.missingField("ask_providers")
        }
        return providers
    }

    private func call(_ object: [String: Any]) async throws -> MuxaWorkComposeEnvelope {
        let payload = try JSONSerialization.data(withJSONObject: object)
        let data = try await transport.request(path: socketPath, payload: payload, timeout: Self.requestTimeout)
        let response = try JSONDecoder().decode(MuxaWorkComposeEnvelope.self, from: data)
        if response.ok == false {
            throw MuxaIPCError.server(response.error ?? "muxad rejected the request")
        }
        return response
    }
}

extension MuxaIPCClient {
    static let workComposeCapability = "work_compose_v1"
    /// The capability behind the daemon's provider list. The Ask settings
    /// read the same string; this constant is named apart so the two files
    /// never declare the same member.
    static let workComposeProviderListCapability = "ask_providers_v1"

    /// A compose client bound to this daemon's socket.
    nonisolated func makeWorkComposeClient() -> MuxaWorkComposeClient {
        MuxaWorkComposeClient(socketPath: socketPath)
    }

    /// Drafts (or refines, when `current` is given) a pipeline through the
    /// daemon's `work_compose`. `agent` nil uses the ask store's provider.
    func composeWork(
        description: String,
        agent: String?,
        current: MuxaPipelineDefinition?,
        name: String?,
        credential: MuxaWorkComposeCredential?
    ) async throws -> MuxaWorkComposeResult {
        guard supports(Self.workComposeCapability) else {
            throw MuxaIPCError.server(
                "muxad does not support drafting pipelines; update muxa and restart muxad"
            )
        }
        return try await makeWorkComposeClient().composeWork(
            description: description,
            agent: agent,
            current: current,
            name: name,
            credential: credential
        )
    }

    /// Provider choices for the composer: nil when the daemon predates
    /// `ask_providers_v1`, so callers fall back to the built-in pair.
    func workComposeProviders() async throws -> [MuxaWorkComposeProvider]? {
        guard supports(Self.workComposeProviderListCapability) else { return nil }
        return try await makeWorkComposeClient().providers()
    }
}
