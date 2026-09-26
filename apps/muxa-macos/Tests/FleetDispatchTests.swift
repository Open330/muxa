import Foundation
import Testing
@testable import Muxa

@Test func dispatchWireUsesStableIDAndSeparatePlanCommand() async throws {
    var request = MuxaDispatchRequest()
    request.workspace = "muxa"; request.work = "test"; request.commit = String(repeating: "a", count: 40); request.body = "verify"
    let expected = request
    let client = MuxaDispatchClient(socketPath: "/test") { _, data, timeout in
        let message = try #require(JSONSerialization.jsonObject(with: data) as? [String: Any])
        #expect(message["kind"] as? String == "work_command")
        let args = try #require(message["args"] as? [String])
        #expect(args == ["work", "dispatch", "--plan"] || args == ["work", "dispatch"])
        #expect(timeout == (args.contains("--plan") ? 40 : 650))
        let input = try #require(message["stdin"] as? String)
        #expect(try JSONDecoder().decode(MuxaDispatchRequest.self, from: Data(input.utf8)) == expected)
        return Data(#"{"ok":true,"work_command":{"stdout":"{}","stderr":"","exit_code":0}}"#.utf8)
    }
    _ = try await client.dispatch(request, preview: false) // No preview prerequisite.
    _ = try await client.dispatch(request, preview: true)
    #expect(request.isValid)
    request.commit = "main"
    #expect(!request.isValid)
}

@Test @MainActor func dispatchReceiptSurvivesReopenAndRejectsChangedOwnerOrPayload() throws {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: dir) }
    let url = dir.appendingPathComponent("receipts.json")
    let store = MuxaDispatchStore(url: url)
    store.request.body = "original"
    try store.reserve(socket: "/socket", coordinator: "mac")
    let reopened = MuxaDispatchStore(url: url)
    let receipt = try #require(reopened.receipts.first)
    reopened.select(receipt)
    try reopened.reserve(socket: "/socket", coordinator: "mac")
    #expect(reopened.request.dispatchID == store.request.dispatchID)
    #expect(throws: (any Error).self) { try reopened.reserve(socket: "/socket", coordinator: "other") }
    reopened.request.body = "changed"
    #expect(throws: (any Error).self) { try reopened.reserve(socket: "/socket", coordinator: "mac") }
    try Data("{broken".utf8).write(to: url)
    let damaged = MuxaDispatchStore(url: url)
    #expect(throws: (any Error).self) { try damaged.reserve(socket: "/socket", coordinator: "mac") }
    #expect(try String(contentsOf: url, encoding: .utf8) == "{broken")
}

@Test func dispatchStatusKeepsUnknownAndReportsWorkerArtifacts() throws {
    let unknown = try MuxaDispatchReport.decode(Data(#"{"state":"unknown","error":"disconnect"}"#.utf8))
    #expect(unknown.state == "unknown")
    #expect(unknown.plan == nil)
    let complete = try MuxaDispatchReport.decode(Data(#"{"state":"completed","result":{"result":{"artifacts":"/worker/results","aliases":{"impl":{"status":"done"}}}}}"#.utf8))
    #expect(complete.artifacts == "/worker/results")
    #expect(complete.aliases["impl"] == "done")
}

@Test func remoteAskSubscriptionStopsRetryingOnlyForKnownUnsupportedRouting() {
    #expect(MuxaAskSubscriptionPolicy.usesSnapshots(MuxaIPCError.server("shared Ask streaming requires a direct coordinator connection; use ask_list for snapshots")))
    #expect(MuxaAskSubscriptionPolicy.usesSnapshots(MuxaIPCError.afterRequestReached(.server("shared Ask streaming requires a direct coordinator connection"))))
    #expect(!MuxaAskSubscriptionPolicy.usesSnapshots(MuxaIPCError.server("temporary failure")))
}

@Test func orchestrationSettingsPreserveInheritedPathsAndConcurrentWriteContract() async throws {
    let settings = MuxaOrchestrationSettings()
    let encoded = try JSONEncoder().encode(settings)
    let client = MuxaDispatchClient(socketPath: "/config") { _, data, timeout in
        let message = try #require(JSONSerialization.jsonObject(with: data) as? [String: Any])
        #expect(timeout == 10)
        #expect(message["expected_text"] as? String == "# original")
        #expect(message["kind"] as? String == "config_orchestration_write")
        return try JSONSerialization.data(withJSONObject: ["ok":true,"config":["path":"/config","text":"new","exists":true],"orchestration":JSONSerialization.jsonObject(with: encoded)])
    }
    let result = try await client.saveSettings(settings, expected: "# original")
    #expect(result.orchestration == settings)
    let overrides = try JSONDecoder().decode(MuxaPathOverrides.self, from: Data("{}".utf8))
    #expect(overrides.root == nil)
}

@Test func fleetWorkerIdentityIsDecodedIndependentlyOfAlias() throws {
    let host = try JSONDecoder().decode(MuxaFleetHost.self, from: Data(#"{"alias":"entry-alias","local":false,"mode":"control","state":"online","node_id":"stable-node"}"#.utf8))
    #expect(host.nodeID == "stable-node")
    #expect(host.alias == "entry-alias")
}

@Test func labelRefreshAdvancesBaselineButRejectsConcurrentPolicyEdits() throws {
    func document(_ text: String, _ settings: MuxaOrchestrationSettings) throws -> MuxaOrchestrationDocument {
        let settings = try JSONSerialization.jsonObject(with: JSONEncoder().encode(settings))
        return try JSONDecoder().decode(MuxaOrchestrationDocument.self, from: JSONSerialization.data(withJSONObject: [
            "config": ["path": "/config", "text": text, "exists": true], "orchestration": settings,
        ]))
    }
    let original = try document("original", MuxaOrchestrationSettings())
    let labeled = try document("new labels", original.orchestration)
    #expect(try original.refreshedAfterLabelEdit(labeled).config.text == "new labels")
    var concurrent = original.orchestration
    concurrent.paths.root = "~/changed-by-another-editor"
    #expect(throws: MuxaConfigConflict.self) {
        try original.refreshedAfterLabelEdit(document("changed policy", concurrent))
    }
}
