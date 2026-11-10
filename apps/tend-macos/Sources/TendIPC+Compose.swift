import Foundation

/// What the daemon's `work_compose` request (and `tend work compose --json`)
/// returns: a pipeline that already passed the CLI's validation, the model's
/// remarks outside the JSON block, and the raw answer for the log.
struct TendWorkComposeResult: Decodable, Equatable, Sendable {
    let pipeline: TendWorkOptions.Pipeline
    let notes: String
    let raw: String

    private enum CodingKeys: String, CodingKey {
        case pipeline, notes, raw
    }

    init(pipeline: TendWorkOptions.Pipeline, notes: String = "", raw: String = "") {
        self.pipeline = pipeline
        self.notes = notes
        self.raw = raw
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        pipeline = try values.decode(TendWorkOptions.Pipeline.self, forKey: .pipeline)
        notes = try values.decodeIfPresent(String.self, forKey: .notes) ?? ""
        raw = try values.decodeIfPresent(String.self, forKey: .raw) ?? ""
    }

    /// Decodes either the bare object (`tend work compose --json` prints it)
    /// or a full daemon reply that carries it under `work_compose`.
    static func decode(_ data: Data) throws -> TendWorkComposeResult {
        let decoder = JSONDecoder()
        if let envelope = try? decoder.decode(TendWorkComposeEnvelope.self, from: data),
           let result = envelope.workCompose {
            return result
        }
        return try decoder.decode(TendWorkComposeResult.self, from: data)
    }

    /// The draft as the composer edits it, plus the name the model chose.
    var definition: TendPipelineDefinition { TendPipelineDefinition(pipeline) }
}

/// One entry of the daemon's `ask_providers` list, reduced to what the
/// composer's picker needs. The Ask settings own the full model.
struct TendWorkComposeProvider: Decodable, Equatable, Sendable, Identifiable {
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
struct TendWorkComposeCredential: Equatable, Sendable {
    let agent: String
    let apiKey: String

    var payload: [String: String] { ["agent": agent, "api_key": apiKey] }
}

private struct TendWorkComposeEnvelope: Decodable {
    let ok: Bool?
    let error: String?
    let workCompose: TendWorkComposeResult?
    let askProviders: [TendWorkComposeProvider]?
    let askOneShot: String?

    enum CodingKeys: String, CodingKey {
        case ok, error
        case workCompose = "work_compose"
        case askProviders = "ask_providers"
        case askOneShot = "ask_one_shot"
    }
}

/// Sends `work_compose` over its own connection and queue. A model call can
/// take a minute, and the control transport (keys, resizes, refreshes) must
/// not wait behind it the way it would inside `TendIPCClient`'s serialized
/// exchange.
final class TendWorkComposeClient: Sendable {
    /// Long enough for a slow provider with one validation retry.
    static let requestTimeout: TimeInterval = 300

    let socketPath: String
    private let transport: SerializedIPCTransport

    init(socketPath: String) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.tend.mac.ipc-compose") { path, payload in
            try UnixSocket.request(path: path, payload: payload, timeout: Self.requestTimeout)
        }
    }

    /// Test seam: exchanges go through `request` instead of the socket.
    init(socketPath: String, request: @escaping TendIPCRequestHandler) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.tend.mac.ipc-compose-test", handler: request)
    }

    /// The `current` payload: the same JSON `TendPipelineDefinition.jsonString()`
    /// produces, plus the `name` the daemon's pipeline shape requires.
    static func currentPayload(_ definition: TendPipelineDefinition, name: String?) throws -> [String: Any] {
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
        current: TendPipelineDefinition?,
        name: String?,
        credential: TendWorkComposeCredential?
    ) throws -> [String: Any] {
        var request: [String: Any] = [
            "protocol": TendIPCClient.protocolVersion,
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
        current: TendPipelineDefinition?,
        name: String?,
        credential: TendWorkComposeCredential?
    ) async throws -> TendWorkComposeResult {
        let request = try Self.requestObject(
            description: description,
            agent: agent,
            current: current,
            name: name,
            credential: credential
        )
        let response = try await call(request)
        guard let result = response.workCompose else {
            throw TendIPCError.missingField("work_compose")
        }
        return result
    }

    /// One read-only turn of a configured Ask provider, recorded nowhere
    /// (`ask_one_shot`). `agent` nil uses the ask store's selection.
    func askOneShot(_ prompt: String, agent: String?) async throws -> String {
        var request: [String: Any] = [
            "protocol": TendIPCClient.protocolVersion,
            "kind": "ask_one_shot",
            "prompt": prompt,
            "agent": NSNull(),
            "credential": NSNull(),
        ]
        if let agent, !agent.isEmpty { request["agent"] = agent }
        let response = try await call(request)
        guard let answer = response.askOneShot else {
            throw TendIPCError.missingField("ask_one_shot")
        }
        return answer
    }

    /// The daemon's provider list (`ask_providers`), for the composer's picker.
    func providers() async throws -> [TendWorkComposeProvider] {
        let response = try await call([
            "protocol": TendIPCClient.protocolVersion,
            "kind": "ask_providers",
        ])
        guard let providers = response.askProviders else {
            throw TendIPCError.missingField("ask_providers")
        }
        return providers
    }

    private func call(_ object: [String: Any]) async throws -> TendWorkComposeEnvelope {
        let payload = try JSONSerialization.data(withJSONObject: object)
        let data = try await transport.request(path: socketPath, payload: payload, timeout: Self.requestTimeout)
        let response = try JSONDecoder().decode(TendWorkComposeEnvelope.self, from: data)
        if response.ok == false {
            throw TendIPCError.server(response.error ?? "tendd rejected the request")
        }
        return response
    }
}

extension TendIPCClient {
    static let askOneShotCapability = "ask_one_shot_v1"

    /// Asks the configured Ask provider once, outside any conversation.
    func askOneShot(_ prompt: String, agent: String? = nil) async throws -> String {
        guard supports(Self.askOneShotCapability) else {
            throw TendIPCError.server("This tendd cannot answer one-off questions; update tend and restart tendd")
        }
        return try await makeWorkComposeClient().askOneShot(prompt, agent: agent)
    }

    static let workComposeCapability = "work_compose_v1"
    /// The capability behind the daemon's provider list. The Ask settings
    /// read the same string; this constant is named apart so the two files
    /// never declare the same member.
    static let workComposeProviderListCapability = "ask_providers_v1"

    /// A compose client bound to this daemon's socket.
    nonisolated func makeWorkComposeClient() -> TendWorkComposeClient {
        TendWorkComposeClient(socketPath: socketPath)
    }

    /// Drafts (or refines, when `current` is given) a pipeline through the
    /// daemon's `work_compose`. `agent` nil uses the ask store's provider.
    func composeWork(
        description: String,
        agent: String?,
        current: TendPipelineDefinition?,
        name: String?,
        credential: TendWorkComposeCredential?
    ) async throws -> TendWorkComposeResult {
        guard supports(Self.workComposeCapability) else {
            throw TendIPCError.server(
                "tendd does not support drafting pipelines; update tend and restart tendd"
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
    func workComposeProviders() async throws -> [TendWorkComposeProvider]? {
        guard supports(Self.workComposeProviderListCapability) else { return nil }
        return try await makeWorkComposeClient().providers()
    }
}
