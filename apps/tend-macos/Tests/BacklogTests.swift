import Foundation
import Testing
@testable import Tend

private let backlogJSON = """
[{"identity":{"workspace_id":"tend","work_id":"export"},
  "metadata":{"title":"CSV export","goal":"Export the review history as CSV\\nwith headers","stage":"queued","updated_at":"2026-11-12T01:00:00Z"},
  "external_items":[],"plan":{"pipeline":"implement-review","priority":1,"cwd":"/src/tend","external":"#79"},
  "created_at":"2026-11-12T01:00:00Z"},
 {"identity":{"workspace_id":"tend","work_id":"running"},
  "metadata":{"stage":"queued","updated_at":"2026-11-12T01:00:00Z"},"plan":{"prompt":"Do the thing"},
  "created_at":"2026-11-12T01:00:00Z"},
 {"identity":{"workspace_id":"tend","work_id":"later"},"metadata":{"stage":"done","updated_at":"2026-11-12T01:00:00Z"},
  "created_at":"2026-11-12T01:00:00Z"},
 {"broken":true}]
"""

private final class BacklogDaemon: @unchecked Sendable {
    private let lock = NSLock()
    private var requests: [[String: Any]] = []
    var capabilities = ["session_bytes_v1", "session_attachment_identity_v1", "work_control_v1", "work_backlog_v1"]

    var sent: [[String: Any]] { lock.lock(); defer { lock.unlock() }; return requests }

    func client() -> TendIPCClient {
        TendIPCClient(socketPath: "/tmp/tend-backlog-\(UUID().uuidString).sock") { [self] _, payload in
            let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
            lock.lock()
            requests.append(object)
            let capabilities = capabilities
            lock.unlock()
            switch object["kind"] as? String {
            case "hello":
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true, "min_protocol": 1, "max_protocol": 6, "capabilities": capabilities,
                ])
            case "work_backlog":
                let records = try JSONSerialization.jsonObject(with: Data(backlogJSON.utf8))
                return try JSONSerialization.data(withJSONObject: ["ok": true, "work_records": records])
            default:
                return try JSONSerialization.data(withJSONObject: ["ok": true])
            }
        }
    }
}

@Suite struct BacklogTests {
    @Test func theBacklogIsReadOnlyFromADaemonThatHasIt() async throws {
        let daemon = BacklogDaemon()
        let client = daemon.client()
        try await client.hello()
        let records = try #require(try await client.workBacklog())
        // The unreadable record is dropped; the rest keep the daemon's order.
        #expect(records.map(\.identity.workID) == ["export", "running", "later"])
        #expect(records[0].title == "CSV export")
        #expect(records[0].line == "Export the review history as CSV")
        #expect(records[0].plan?.priority == 1)
        #expect(records[1].title == "running" && records[1].line == "Do the thing")
        #expect(daemon.sent.last?["stage"] as? String == "queued")

        let older = BacklogDaemon()
        older.capabilities.removeAll { $0 == "work_backlog_v1" }
        let oldClient = older.client()
        try await oldClient.hello()
        #expect(try await oldClient.workBacklog() == nil)
        #expect(!older.sent.contains { $0["kind"] as? String == "work_backlog" })
    }

    @MainActor
    @Test func homeShowsQueuedWorkThatIsNotAlreadyOnTheBoard() async throws {
        let daemon = BacklogDaemon()
        let client = daemon.client()
        try await client.hello()
        let feed = FleetBacklogFeed()
        await feed.load(client)
        let host = TendFleetHostIdentity(alias: "mac", local: true, state: "online", mode: "local")
        let agent = try JSONDecoder().decode(TendAgent.self, from: Data(#"{"kind":"claude","agent_session_id":"a","state":"working"}"#.utf8))
        let running = FleetBoard.work(for: TendWorkGroup(
            identity: TendWorkIdentity(workspaceID: "tend", workID: "running"),
            pipelineRun: nil,
            participants: [TendHostedAgent(host: host, agent: agent, pane: nil)]
        ), now: .now)
        #expect(feed.shown(on: [running]).map(\.identity.workID) == ["export"])
    }
}
