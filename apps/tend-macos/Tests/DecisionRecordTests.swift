import Foundation
import Testing
@testable import Tend

private let host = TendFleetHostIdentity(alias: "mac", local: true, state: "online", mode: "local")

private func finished(_ id: String, at time: String) throws -> FleetWork {
    let agent = try JSONDecoder().decode(TendAgent.self, from: JSONSerialization.data(withJSONObject: [
        "kind": "claude_code", "agent_session_id": id, "state": "idle", "state_entered_at": time, "last_prompt": "Fix it",
    ]))
    return FleetBoard.work(for: TendHostedAgent(host: host, agent: agent, pane: nil), now: .now)
}

/// A daemon that answers hello, records reviews (or refuses them) and
/// keeps them for `decision_history`.
private final class RecordingDaemon: @unchecked Sendable {
    private let lock = NSLock()
    private var recorded: [[String: Any]] = []
    var capabilities = ["session_bytes_v1", "session_attachment_identity_v1", "work_control_v1",
                        "review_items_v1", "review_record_v1", "decision_queue_v1"]
    var refuseRecords = false

    var records: [[String: Any]] { lock.lock(); defer { lock.unlock() }; return recorded }

    func handle(_ payload: Data) throws -> Data {
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        lock.lock()
        defer { lock.unlock() }
        switch object["kind"] as? String {
        case "hello":
            return try JSONSerialization.data(withJSONObject: [
                "ok": true, "min_protocol": 1, "max_protocol": 6, "capabilities": capabilities,
            ])
        case "review_record":
            if refuseRecords { return try JSONSerialization.data(withJSONObject: ["ok": false, "error": "review store unavailable"]) }
            recorded.append(object)
            return try JSONSerialization.data(withJSONObject: ["ok": true])
        case "decision_queue":
            return try JSONSerialization.data(withJSONObject: ["ok": true, "decision_queue": ["items": []]])
        case "decision_history":
            let items: [[String: Any]] = recorded.enumerated().map { index, request in
                let review = request["review"] as? [String: Any] ?? [:]
                return [
                    "kind": "review", "id": "rv_\(index)", "target": review["target"] ?? [:], "generation": 1,
                    "summary": review["summary"] ?? "", "idempotency_key": review["idempotency_key"] ?? "",
                    "requested_at": "2026-11-12T01:00:00Z", "requested_by": ["kind": "user", "label": "Tend.app"],
                    "disposition": ["kind": request["disposition"] ?? "", "by": ["kind": "user"], "at": "2026-11-12T01:00:00Z"],
                ]
            }
            return try JSONSerialization.data(withJSONObject: ["ok": true, "decision_history": ["items": items]])
        default:
            return try JSONSerialization.data(withJSONObject: ["ok": true])
        }
    }

    func client() -> TendIPCClient {
        TendIPCClient(socketPath: "/tmp/tend-decision-record-\(UUID().uuidString).sock") { [self] _, payload in
            try handle(payload)
        }
    }
}

@Suite struct DecisionRecordTests {
    @Test func historyDecodesDispositionsAndAnswers() throws {
        let json = """
        {"items":[
          {"kind":"human_answer","request_id":"req_1","body":"Merge?","from_session_id":"s","from_pane":"%1",
           "created_at":"2026-11-12T01:00:00Z","status":"completed","answer":"Merge now","answered_at":"2026-11-12T02:00:00Z",
           "decision":{"options":["Merge now","Hold"]}},
          {"kind":"review","id":"rv_1","target":{"kind":"work","workspace":"ws","work":"w"},"generation":1,
           "summary":"Looks good","idempotency_key":"home:abc","requested_at":"2026-11-12T01:00:00Z",
           "requested_by":{"kind":"user","label":"Tend.app"},
           "disposition":{"kind":"accepted","by":{"kind":"user","label":"Tend.app"},"at":"2026-11-12T01:30:00Z"}},
          {"kind":"review","id":"rv_2","target":{"kind":"session","session_id":"s"},"generation":1,
           "summary":"Which port?","idempotency_key":"home-answer:def","requested_at":"2026-11-12T01:00:00Z",
           "requested_by":{"kind":"user"},
           "disposition":{"kind":"returned","by":{"kind":"user"},"at":"2026-11-12T01:10:00Z","note":"8080"}},
          {"kind":"future_thing"}
        ]}
        """
        let history = try JSONDecoder().decode(TendDecisionHistory.self, from: Data(json.utf8))
        #expect(history.items.count == 3 && history.undecoded == 1)
        guard case .humanAnswer(let answer) = history.items[0] else { Issue.record("not an answer"); return }
        #expect(answer.answer == "Merge now" && answer.request.requestID == "req_1")
        #expect(answer.request.decision?.options == ["Merge now", "Hold"])
        // Only Looks Good from Home hides a result; an answer never does.
        #expect(FleetDecisionRecord.reviewedKeys(in: history) == ["home:abc"])
        #expect(FleetDecisionRecord.kind(ofKey: "home-answer:def") == .answered)
        #expect(FleetDecisionRecord.kind(ofKey: "home-sendback:x") == .sentBack)
        #expect(FleetDecisionRecord.kind(ofKey: "cli:x") == nil)
    }

    @Test func recordedKeysDecideWhatHomeHides() throws {
        let work = try finished("a", at: "2026-11-08T01:00:00Z")
        var marks = FleetReviewMarks()
        marks.markReviewed(work)
        marks.recorded = []
        #expect(!marks.hides(work), "once tendd keeps the record, a local mark alone hides nothing")
        marks.recorded = [FleetBoard.reviewKey(work)]
        #expect(marks.hides(work))
        #expect(!marks.hides(try finished("a", at: "2026-11-08T02:00:00Z")), "a later revision is a new result")
    }

    @Test func recordTextFitsTheStore() {
        #expect(TendRecordText.bounded("a\u{1b}[31mb\u{7}\n\tc ", maxBytes: 100) == "a[31mb\n\tc")
        let long = String(repeating: "가", count: 600)
        let cut = TendRecordText.bounded(long, maxBytes: 1024)
        #expect(cut.utf8.count <= 1024 && cut.hasSuffix("…"))
    }

    @Test func anInstructionIsRecordedAsReturnedWithTheWords() async throws {
        let daemon = RecordingDaemon()
        let client = daemon.client()
        try await client.hello()
        await FleetDecisionRecord.recordInstruction(
            client: client, target: .session(provider: "claude", sessionID: "s"), reply: .answer,
            sessionID: "s", waitSince: "2026-11-12T01:00:00Z", question: "Which port?", text: "8080"
        )
        await FleetDecisionRecord.recordInstruction(
            client: client, target: .session(provider: "claude", sessionID: "s"), reply: .sendBack,
            sessionID: "s", waitSince: nil, question: nil, text: "Add a test"
        )
        let records = daemon.records
        #expect(records.count == 2)
        #expect(records[0]["disposition"] as? String == "returned")
        #expect(records[0]["note"] as? String == "8080")
        let review = try #require(records[0]["review"] as? [String: Any])
        #expect(review["summary"] as? String == "Which port?")
        #expect((review["idempotency_key"] as? String)?.hasPrefix("home-answer:") == true)
        #expect(((records[1]["review"] as? [String: Any])?["idempotency_key"] as? String)?.hasPrefix("home-sendback:") == true)
        // The same words for the same wait are one record.
        #expect(FleetDecisionRecord.instructionKey(.answer, sessionID: "s", waitSince: "t", text: "x")
            == FleetDecisionRecord.instructionKey(.answer, sessionID: "s", waitSince: "t", text: "x"))
    }

    @MainActor
    @Test func looksGoodIsRecordedAndHiddenOrShownAgainOnFailure() async throws {
        let daemon = RecordingDaemon()
        let client = daemon.client()
        try await client.hello()
        let feed = FleetDecisionFeed()
        await feed.load(client)
        #expect(feed.reviewedKeys == [])
        let work = try finished("a", at: "2026-11-08T01:00:00Z")
        await feed.recordLooksGood(work, client: client)
        #expect(feed.reviewedKeys == [FleetBoard.reviewKey(work)])
        #expect(daemon.records.first?["disposition"] as? String == "accepted")

        daemon.refuseRecords = true
        let other = try finished("b", at: "2026-11-08T01:00:00Z")
        await feed.recordLooksGood(other, client: client)
        #expect(feed.reviewedKeys?.contains(FleetBoard.reviewKey(other)) == false)
        #expect(feed.problem?.contains("review store unavailable") == true)
    }

    @MainActor
    @Test func oldMarksMoveToTendOnceAndOnlyForWhatTheyStillHide() async throws {
        let daemon = RecordingDaemon()
        let client = daemon.client()
        try await client.hello()
        let feed = FleetDecisionFeed()
        let shown = try finished("a", at: "2026-11-08T01:00:00Z")
        let moved = try finished("b", at: "2026-11-08T01:00:00Z")
        var marks = FleetReviewMarks()
        marks.markReviewed(shown)
        marks.markReviewed(moved)
        let movedOn = try finished("b", at: "2026-11-08T05:00:00Z")
        #expect(await feed.migrate(marks, works: [shown, movedOn], client: client))
        #expect(daemon.records.count == 1, "a mark on a revision that moved on hid nothing")
        #expect(feed.reviewedKeys == [FleetBoard.reviewKey(shown)])
    }

    @MainActor
    @Test func anOlderDaemonKeepsTheLocalMarks() async throws {
        let daemon = RecordingDaemon()
        daemon.capabilities.removeAll { $0 == "review_record_v1" }
        let client = daemon.client()
        try await client.hello()
        let feed = FleetDecisionFeed()
        await feed.load(client)
        #expect(feed.reviewedKeys == nil)
        #expect(try await client.recordDecision(
            target: .session(provider: "c", sessionID: "s"), key: "k", summary: "s", disposition: .accepted, note: nil
        ) == false)
        #expect(daemon.records.isEmpty)
    }
}
