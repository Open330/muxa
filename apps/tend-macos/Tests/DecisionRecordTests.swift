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
    /// Older records already in the store, oldest first.
    var preloaded: [(key: String, at: String)] = []
    private(set) var pagesRead = 0

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
        case "review_list":
            // Newest first: what was recorded here, then the preloaded ones.
            let made: [[String: Any]] = recorded.enumerated().reversed().map { index, request in
                let review = request["review"] as? [String: Any] ?? [:]
                return Self.item("rv_\(index)", key: review["idempotency_key"] as? String ?? "", at: "2026-11-12T23:00:00Z",
                                 disposition: request["disposition"] as? String ?? "accepted")
            }
            let older = preloaded.enumerated().reversed().map { index, old in
                Self.item("old_\(index)", key: old.key, at: old.at, disposition: "accepted")
            }
            let all = made + older
            let offset = object["offset"] as? Int ?? 0
            let limit = object["limit"] as? Int ?? 50
            let page = Array(all.dropFirst(offset).prefix(limit))
            pagesRead += 1
            return try JSONSerialization.data(withJSONObject: [
                "ok": true, "review_items": page, "review_has_more": offset + page.count < all.count,
            ])
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

    static func item(_ id: String, key: String, at: String, disposition: String) -> [String: Any] {
        [
            "id": id, "target": ["kind": "session", "session_id": "s"], "generation": 1, "summary": "x",
            "idempotency_key": key, "requested_at": at, "requested_by": ["kind": "user"],
            "disposition": ["kind": disposition, "by": ["kind": "user"], "at": at],
        ]
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
        let reviews: [TendReviewItem] = history.items.compactMap { if case .review(let review) = $0 { review } else { nil } }
        #expect(FleetDecisionRecord.reviewedKeys(in: reviews) == ["home:abc"])
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
        let work = try finished("a", at: "2026-11-08T01:00:00Z")
        await feed.load(client, works: [work])
        #expect(feed.reviewedKeys == [])
        await feed.recordLooksGood(work, client: client, works: [work])
        #expect(feed.reviewedKeys == [FleetBoard.reviewKey(work)])
        #expect(daemon.records.first?["disposition"] as? String == "accepted")

        daemon.refuseRecords = true
        let other = try finished("b", at: "2026-11-08T01:00:00Z")
        await feed.recordLooksGood(other, client: client, works: [work, other])
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

    /// A result reviewed long ago, behind many newer records, stays hidden:
    /// the read pages back to the board's oldest finished result.
    @MainActor
    @Test func anOldReviewBehindManyNewerOnesStillHides() async throws {
        let daemon = RecordingDaemon()
        let work = try finished("a", at: "2026-11-08T01:00:00Z")
        daemon.preloaded = [(FleetBoard.reviewKey(work), "2026-11-08T02:00:00Z")]
            + (0..<1_200).map { ("home:other\($0)", "2026-11-10T00:00:00Z") }
        let client = daemon.client()
        try await client.hello()
        let feed = FleetDecisionFeed()
        await feed.load(client, works: [work])
        #expect(feed.reviewedKeys?.contains(FleetBoard.reviewKey(work)) == true)
        #expect(daemon.pagesRead == 3)
    }

    /// More newer records than one read takes: nothing is unhidden, and the
    /// next read goes on where this one stopped until it finds the review.
    @MainActor
    @Test func aReadThatRunsOutOfPagesGoesOnNextTime() async throws {
        let daemon = RecordingDaemon()
        let work = try finished("a", at: "2026-11-08T01:00:00Z")
        daemon.preloaded = [(FleetBoard.reviewKey(work), "2026-11-08T02:00:00Z")]
            + (0..<5_001).map { ("home:other\($0)", "2026-11-10T00:00:00Z") }
        let client = daemon.client()
        try await client.hello()
        let feed = FleetDecisionFeed()
        await feed.loadReviewed(client, works: [work], force: true)
        #expect(!feed.reviewedIsComplete)
        #expect(daemon.pagesRead == FleetDecisionRecord.maxReviewPages)
        await feed.loadReviewed(client, works: [work], force: true)
        #expect(feed.reviewedIsComplete)
        #expect(feed.reviewedKeys?.contains(FleetBoard.reviewKey(work)) == true)
        #expect(feed.reviewedKeys?.contains("home:other0") == true, "what the first read found is kept")
    }

    @MainActor
    @Test func aMigrationTheDaemonDidNotRecordIsNotDone() async throws {
        let daemon = RecordingDaemon()
        let client = daemon.client()
        try await client.hello()
        let work = try finished("a", at: "2026-11-08T01:00:00Z")
        var marks = FleetReviewMarks()
        marks.markReviewed(work)
        daemon.refuseRecords = true
        #expect(await FleetDecisionFeed().migrate(marks, works: [work], client: client) == false)
    }

    @MainActor
    @Test func anOlderDaemonKeepsTheLocalMarks() async throws {
        let daemon = RecordingDaemon()
        daemon.capabilities.removeAll { $0 == "review_record_v1" }
        let client = daemon.client()
        try await client.hello()
        let feed = FleetDecisionFeed()
        await feed.load(client, works: [try finished("a", at: "2026-11-08T01:00:00Z")])
        #expect(feed.reviewedKeys == nil)
        #expect(try await client.recordDecision(
            target: .session(provider: "c", sessionID: "s"), key: "k", summary: "s", disposition: .accepted, note: nil
        ) == false)
        #expect(daemon.records.isEmpty)
    }
}
