import Foundation
import SwiftUI

struct TendExecutionPaths: Codable, Equatable, Sendable {
    var root = "~/workspace-tend"
    var repo = "repos/{repo}"
    var run = "runs/{workspace}/{work}/{attempt}"
    var artifacts = "artifacts/{workspace}/{work}/{attempt}"
}

struct TendPathOverrides: Codable, Equatable, Sendable {
    var root: String?
    var repo: String?
    var run: String?
    var artifacts: String?
}

struct TendWorkspacePolicy: Codable, Equatable, Sendable {
    var repo = ""
    var url = ""
    var pipeline = ""
    var selector = ""
    var paths = TendPathOverrides()
    var nodes: [String: TendPathOverrides] = [:]
}

struct TendOrchestrationSettings: Codable, Equatable, Sendable {
    var enabled = false
    var coordinator: String?
    var paths = TendExecutionPaths()
    var workspaces: [String: TendWorkspacePolicy] = [:]
}

struct TendDispatchRequest: Codable, Equatable, Sendable {
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

struct TendDispatchPlan: Codable, Equatable, Sendable {
    var request: TendDispatchRequest
    var nodeID: String
    var host: String
    var repo: String
    var url: String
    var pipeline: String
    var paths: TendExecutionPaths
    var reason: String
    enum CodingKeys: String, CodingKey {
        case nodeID = "node_id"
        case request, host, repo, url, pipeline, paths, reason
    }
}

struct TendDispatchReport: Equatable, Sendable {
    var state: String
    var plan: TendDispatchPlan?
    var error: String?
    var artifacts: String?
    var aliases: [String: String]
    static func decode(_ data: Data) throws -> Self {
        guard let value = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              let state = value["state"] as? String else { throw TendIPCError.missingField("dispatch state") }
        let plan = try value["plan"].map { try JSONDecoder().decode(TendDispatchPlan.self, from: JSONSerialization.data(withJSONObject: $0)) }
        let worker = value["result"] as? [String: Any]
        let details = worker?["result"] as? [String: Any]
        let aliases = details?["aliases"] as? [String: [String: Any]] ?? [:]
        return Self(state: state, plan: plan, error: value["error"] as? String ?? worker?["error"] as? String,
                    artifacts: details?["artifacts"] as? String,
                    aliases: aliases.compactMapValues { $0["status"] as? String })
    }
}

struct TendOrchestrationDocument: Decodable, Sendable {
    let config: TendDaemonConfigDocument
    let orchestration: TendOrchestrationSettings

    /// Accept unrelated config edits without accepting a concurrent policy change.
    func refreshedAfterLabelEdit(_ latest: Self) throws -> Self {
        guard config.path == latest.config.path, orchestration == latest.orchestration else {
            throw TendConfigConflict(message: "config.toml changed; reload before saving Fleet policy", current: latest.config)
        }
        return latest
    }
}

/// Slow dispatch uses a dedicated connection; Work launch must not block app state updates.
final class TendDispatchClient: Sendable {
    let socketPath: String
    private let transport: SerializedIPCTransport
    init(socketPath: String, request: @escaping TendIPCTimedRequestHandler = { path, payload, timeout in
        try UnixSocket.request(path: path, payload: payload, timeout: timeout)
    }) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.tend.mac.dispatch", timedHandler: request)
    }
    func exchange(_ object: [String: Any], timeout: TimeInterval = 10) async throws -> Data {
        let payload = try JSONSerialization.data(withJSONObject: object)
        let data = try await transport.request(path: socketPath, payload: payload, timeout: timeout)
        let envelope = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        if envelope?["ok"] as? Bool == false {
            let message = envelope?["error"] as? String ?? "Fleet request failed"
            if let config = envelope?["config"] {
                throw TendConfigConflict(message: message, current: try JSONDecoder().decode(TendDaemonConfigDocument.self, from: JSONSerialization.data(withJSONObject: config)))
            }
            throw TendIPCError.server(message)
        }
        return data
    }
    func command(_ args: [String], input: Data? = nil, timeout: TimeInterval = 40) async throws -> Data {
        var request: [String: Any] = ["protocol": TendIPCClient.protocolVersion, "kind": "work_command", "args": ["work"] + args]
        if let input { request["stdin"] = String(decoding: input, as: UTF8.self) }
        let data = try await exchange(request, timeout: timeout)
        struct Envelope: Decodable { let work_command: TendWorkCommandOutput }
        let output = try JSONDecoder().decode(Envelope.self, from: data).work_command
        guard output.exitCode == 0 else { throw TendIPCError.server(output.stderr) }
        return Data(output.stdout.utf8)
    }
    func options() async throws -> TendOrchestrationSettings {
        try await JSONDecoder().decode(TendOrchestrationSettings.self, from: command(["dispatch-options"]))
    }
    func dispatch(_ request: TendDispatchRequest, preview: Bool) async throws -> Data {
        try await command(preview ? ["dispatch", "--plan"] : ["dispatch"], input: JSONEncoder().encode(request), timeout: preview ? 40 : 650)
    }
    func status(_ id: String) async throws -> TendDispatchReport {
        guard UUID(uuidString: id) != nil else { throw TendIPCError.server("Enter a dispatch UUID") }
        return try await TendDispatchReport.decode(command(["dispatch-status", id]))
    }
    func readSettings() async throws -> TendOrchestrationDocument {
        try await JSONDecoder().decode(TendOrchestrationDocument.self, from: exchange(["protocol": TendIPCClient.protocolVersion, "kind": "config_orchestration_read"]))
    }
    func saveSettings(_ settings: TendOrchestrationSettings, expected: String) async throws -> TendOrchestrationDocument {
        let object = try JSONSerialization.jsonObject(with: JSONEncoder().encode(settings))
        return try await JSONDecoder().decode(TendOrchestrationDocument.self, from: exchange([
            "protocol": TendIPCClient.protocolVersion, "kind": "config_orchestration_write", "settings": object, "expected_text": expected,
        ]))
    }
}

struct TendDispatchReceipt: Codable, Equatable, Identifiable {
    let socket: String
    let coordinator: String
    let request: TendDispatchRequest
    var id: String { request.dispatchID }
}

/// A receipt is committed before any launch. Closing a sheet or an app is not cancellation.
@MainActor
final class TendDispatchStore: ObservableObject {
    static let shared = TendDispatchStore()
    @Published var request = TendDispatchRequest()
    @Published var submitted = false
    @Published var busy = false
    @Published var preview: TendDispatchPlan?
    @Published var report: TendDispatchReport?
    @Published var error: String?
    @Published private(set) var receipts: [TendDispatchReceipt] = []
    private let url: URL
    private var readable = true
    init(url: URL? = nil) {
        self.url = url ?? FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("tend/app-dispatches/receipts.json")
        if FileManager.default.fileExists(atPath: self.url.path) {
            do { receipts = try JSONDecoder().decode([TendDispatchReceipt].self, from: Data(contentsOf: self.url)) }
            catch { self.readable = false; self.error = "Cannot read saved dispatches: \(error.localizedDescription)" }
        }
    }
    func reserve(socket: String, coordinator: String) throws {
        guard readable else { throw TendIPCError.server("Saved dispatch journal is unreadable; preserve it before recovery") }
        let receipt = TendDispatchReceipt(socket: socket, coordinator: coordinator, request: request)
        if let prior = receipts.first(where: { $0.id == receipt.id }) {
            guard prior == receipt else { throw TendIPCError.server("Saved dispatch belongs to another request or coordinator") }
        } else {
            let updated = receipts + [receipt]
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
            try JSONEncoder().encode(updated).write(to: url, options: .atomic)
            try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
            receipts = updated
        }
        submitted = true
    }
    func select(_ receipt: TendDispatchReceipt) {
        request = receipt.request; submitted = true; preview = nil; report = nil; error = nil
    }
    func newDraft() {
        request = TendDispatchRequest(); submitted = false; preview = nil; report = nil; error = nil
    }
}

enum TendAskSubscriptionPolicy {
    static func usesSnapshots(_ error: Error) -> Bool {
        if case TendIPCError.afterRequestReached(let underlying) = error { return usesSnapshots(underlying) }
        guard case TendIPCError.server(let message) = error else { return false }
        return message.hasPrefix("shared Ask streaming requires a direct coordinator connection")
    }
}
