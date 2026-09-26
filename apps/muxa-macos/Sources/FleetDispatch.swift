import Foundation
import SwiftUI

struct MuxaExecutionPaths: Codable, Equatable, Sendable {
    var root = "~/workspace-muxa"
    var repo = "repos/{repo}"
    var run = "runs/{workspace}/{work}/{attempt}"
    var artifacts = "artifacts/{workspace}/{work}/{attempt}"
}

struct MuxaPathOverrides: Codable, Equatable, Sendable {
    var root: String?
    var repo: String?
    var run: String?
    var artifacts: String?
}

struct MuxaWorkspacePolicy: Codable, Equatable, Sendable {
    var repo = ""
    var url = ""
    var pipeline = ""
    var selector = ""
    var paths = MuxaPathOverrides()
    var nodes: [String: MuxaPathOverrides] = [:]
}

struct MuxaOrchestrationSettings: Codable, Equatable, Sendable {
    var enabled = false
    var coordinator: String?
    var paths = MuxaExecutionPaths()
    var workspaces: [String: MuxaWorkspacePolicy] = [:]
}

struct MuxaDispatchRequest: Codable, Equatable, Sendable {
    var dispatchID = UUID().uuidString.lowercased()
    var workspace = ""
    var work = ""
    var commit = ""
    var body = ""
    var selector: String?
    var host: String?
    enum CodingKeys: String, CodingKey {
        case dispatchID = "dispatch_id"
        case workspace, work, commit, body, selector, host
    }
    var isValid: Bool {
        !workspace.isEmpty && !work.isEmpty && !body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && [40, 64].contains(commit.utf8.count)
            && commit.utf8.allSatisfy { (48...57).contains($0) || (65...70).contains($0) || (97...102).contains($0) }
    }
}

struct MuxaDispatchPlan: Codable, Equatable, Sendable {
    var request: MuxaDispatchRequest
    var nodeID: String
    var host: String
    var repo: String
    var url: String
    var pipeline: String
    var paths: MuxaExecutionPaths
    var reason: String
    enum CodingKeys: String, CodingKey {
        case nodeID = "node_id"
        case request, host, repo, url, pipeline, paths, reason
    }
}

struct MuxaDispatchReport: Equatable, Sendable {
    var state: String
    var plan: MuxaDispatchPlan?
    var error: String?
    var artifacts: String?
    var aliases: [String: String]
    static func decode(_ data: Data) throws -> Self {
        guard let value = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              let state = value["state"] as? String else { throw MuxaIPCError.missingField("dispatch state") }
        let plan = try value["plan"].map { try JSONDecoder().decode(MuxaDispatchPlan.self, from: JSONSerialization.data(withJSONObject: $0)) }
        let worker = value["result"] as? [String: Any]
        let details = worker?["result"] as? [String: Any]
        let aliases = details?["aliases"] as? [String: [String: Any]] ?? [:]
        return Self(state: state, plan: plan, error: value["error"] as? String ?? worker?["error"] as? String,
                    artifacts: details?["artifacts"] as? String,
                    aliases: aliases.compactMapValues { $0["status"] as? String })
    }
}

struct MuxaOrchestrationDocument: Decodable, Sendable {
    let config: MuxaDaemonConfigDocument
    let orchestration: MuxaOrchestrationSettings

    /// Accept unrelated config edits without accepting a concurrent policy change.
    func refreshedAfterLabelEdit(_ latest: Self) throws -> Self {
        guard config.path == latest.config.path, orchestration == latest.orchestration else {
            throw MuxaConfigConflict(message: "config.toml changed; reload before saving Fleet policy", current: latest.config)
        }
        return latest
    }
}

/// Slow dispatch uses a dedicated connection; Work launch must not block app state updates.
final class MuxaDispatchClient: Sendable {
    let socketPath: String
    private let transport: SerializedIPCTransport
    init(socketPath: String, request: @escaping MuxaIPCTimedRequestHandler = { path, payload, timeout in
        try UnixSocket.request(path: path, payload: payload, timeout: timeout)
    }) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.muxa.mac.dispatch", timedHandler: request)
    }
    func exchange(_ object: [String: Any], timeout: TimeInterval = 10) async throws -> Data {
        let payload = try JSONSerialization.data(withJSONObject: object)
        let data = try await transport.request(path: socketPath, payload: payload, timeout: timeout)
        let envelope = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        if envelope?["ok"] as? Bool == false {
            let message = envelope?["error"] as? String ?? "Fleet request failed"
            if let config = envelope?["config"] {
                throw MuxaConfigConflict(message: message, current: try JSONDecoder().decode(MuxaDaemonConfigDocument.self, from: JSONSerialization.data(withJSONObject: config)))
            }
            throw MuxaIPCError.server(message)
        }
        return data
    }
    func command(_ args: [String], input: Data? = nil, timeout: TimeInterval = 40) async throws -> Data {
        var request: [String: Any] = ["protocol": MuxaIPCClient.protocolVersion, "kind": "work_command", "args": ["work"] + args]
        if let input { request["stdin"] = String(decoding: input, as: UTF8.self) }
        let data = try await exchange(request, timeout: timeout)
        struct Envelope: Decodable { let work_command: MuxaWorkCommandOutput }
        let output = try JSONDecoder().decode(Envelope.self, from: data).work_command
        guard output.exitCode == 0 else { throw MuxaIPCError.server(output.stderr) }
        return Data(output.stdout.utf8)
    }
    func options() async throws -> MuxaOrchestrationSettings {
        try await JSONDecoder().decode(MuxaOrchestrationSettings.self, from: command(["dispatch-options"]))
    }
    func dispatch(_ request: MuxaDispatchRequest, preview: Bool) async throws -> Data {
        try await command(preview ? ["dispatch", "--plan"] : ["dispatch"], input: JSONEncoder().encode(request), timeout: preview ? 40 : 650)
    }
    func status(_ id: String) async throws -> MuxaDispatchReport {
        guard UUID(uuidString: id) != nil else { throw MuxaIPCError.server("Enter a dispatch UUID") }
        return try await MuxaDispatchReport.decode(command(["dispatch-status", id]))
    }
    func readSettings() async throws -> MuxaOrchestrationDocument {
        try await JSONDecoder().decode(MuxaOrchestrationDocument.self, from: exchange(["protocol": MuxaIPCClient.protocolVersion, "kind": "config_orchestration_read"]))
    }
    func saveSettings(_ settings: MuxaOrchestrationSettings, expected: String) async throws -> MuxaOrchestrationDocument {
        let object = try JSONSerialization.jsonObject(with: JSONEncoder().encode(settings))
        return try await JSONDecoder().decode(MuxaOrchestrationDocument.self, from: exchange([
            "protocol": MuxaIPCClient.protocolVersion, "kind": "config_orchestration_write", "settings": object, "expected_text": expected,
        ]))
    }
}

struct MuxaDispatchReceipt: Codable, Equatable, Identifiable {
    let socket: String
    let coordinator: String
    let request: MuxaDispatchRequest
    var id: String { request.dispatchID }
}

/// A receipt is committed before any launch. Closing a sheet or an app is not cancellation.
@MainActor
final class MuxaDispatchStore: ObservableObject {
    static let shared = MuxaDispatchStore()
    @Published var request = MuxaDispatchRequest()
    @Published var submitted = false
    @Published var busy = false
    @Published var preview: MuxaDispatchPlan?
    @Published var report: MuxaDispatchReport?
    @Published var error: String?
    @Published private(set) var receipts: [MuxaDispatchReceipt] = []
    private let url: URL
    private var readable = true
    init(url: URL? = nil) {
        self.url = url ?? FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("muxa/app-dispatches/receipts.json")
        if FileManager.default.fileExists(atPath: self.url.path) {
            do { receipts = try JSONDecoder().decode([MuxaDispatchReceipt].self, from: Data(contentsOf: self.url)) }
            catch { self.readable = false; self.error = "Cannot read saved dispatches: \(error.localizedDescription)" }
        }
    }
    func reserve(socket: String, coordinator: String) throws {
        guard readable else { throw MuxaIPCError.server("Saved dispatch journal is unreadable; preserve it before recovery") }
        let receipt = MuxaDispatchReceipt(socket: socket, coordinator: coordinator, request: request)
        if let prior = receipts.first(where: { $0.id == receipt.id }) {
            guard prior == receipt else { throw MuxaIPCError.server("Saved dispatch belongs to another request or coordinator") }
        } else {
            let updated = receipts + [receipt]
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
            try JSONEncoder().encode(updated).write(to: url, options: .atomic)
            try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
            receipts = updated
        }
        submitted = true
    }
    func select(_ receipt: MuxaDispatchReceipt) {
        request = receipt.request; submitted = true; preview = nil; report = nil; error = nil
    }
    func newDraft() {
        request = MuxaDispatchRequest(); submitted = false; preview = nil; report = nil; error = nil
    }
}

enum MuxaAskSubscriptionPolicy {
    static func usesSnapshots(_ error: Error) -> Bool {
        if case MuxaIPCError.afterRequestReached(let underlying) = error { return usesSnapshots(underlying) }
        guard case MuxaIPCError.server(let message) = error else { return false }
        return message.hasPrefix("shared Ask streaming requires a direct coordinator connection")
    }
}
