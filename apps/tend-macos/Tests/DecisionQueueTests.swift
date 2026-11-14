import Foundation
import Testing
@testable import Tend

private let local = TendFleetHostIdentity(alias: "mac", local: true, state: "online", mode: "local")
private let remote = TendFleetHostIdentity(alias: "mini", local: false, state: "online", mode: "control")

private let queueJSON = """
{"items":[
  {"kind":"human_request","request_id":"req_1","action":"decision","body":"Merge #12 now?",
   "decision":{"needed":"Merge #12 or hold for the perf run?","options":["Merge","Hold"],
     "recommendation":"Merge: the perf run is flaky and unrelated.",
     "evidence":[{"kind":"commit","sha":"1a2b3c4d","repo":"/src/tend"},
                 {"kind":"pull_request","url":"https://github.com/Open330/tend/pull/12"},
                 {"kind":"hologram","beam":1},
                 {"kind":"test","command":"cargo test","passed":true,"tail":"ok"}]},
   "from_session_id":"s-impl","from_pane":"%4","from_alias":"impl","work_id":"storage",
   "created_at":"2026-11-12T01:00:00.123456789Z"},
  {"kind":"agent_waiting","session_id":"s-2","agent_kind":"codex","pane":"%5","state":"waiting_input",
   "message":"Proceed?","since":"2026-11-12T01:01:00Z"},
  {"kind":"something_new","x":1},
  {"kind":"review","id":"rv_1","target":{"kind":"session","provider":"claude","session_id":"s-3"},
   "generation":1,"summary":"Check the retry path","anchor":"commit:abcdef12",
   "requested_at":"2026-11-12T01:02:00Z","requested_by":{"kind":"agent","label":"impl"},
   "decision":{"evidence":[{"kind":"diff","range":"main..feat/x"}]}}
],"truncated":false,"unavailable":["review items: locked"]}
"""

private func hello(_ capabilities: [String]) throws -> Data {
    try JSONSerialization.data(withJSONObject: [
        "ok": true, "min_protocol": 1, "max_protocol": 6,
        "capabilities": ["session_bytes_v1", "session_attachment_identity_v1", "work_control_v1"] + capabilities,
    ])
}

private final class Requests: @unchecked Sendable {
    private let lock = NSLock()
    private var objects: [[String: Any]] = []
    func add(_ object: [String: Any]) { lock.lock(); objects.append(object); lock.unlock() }
    var all: [[String: Any]] { lock.lock(); defer { lock.unlock() }; return objects }
}

private func message(host: TendFleetHostIdentity, id: String, created: String, status: String = "queued") throws -> TendOperatorMessage {
    let json = """
    {"id":"\(id)","kind":"question","body":"Which schema?","expects_reply":true,"work_mode":"read_only",
     "status":"\(status)","created_at":"\(created)",
     "decision":{"options":["v1","v2"]},
     "from":{"agent_kind":"codex","agent_session_id":"s-\(id)","pane":"%9","room":{"host":"tmux","window_id":"@1"},"alias":"db"},
     "to":{"agent_kind":"unknown","agent_session_id":"__tend_console__","pane":"console","room":{"host":"tmux","window_id":"@1"},"console":true}}
    """
    let request = try JSONDecoder().decode(TendCollaborationRequest.self, from: Data(json.utf8))
    return TendOperatorMessage(host: host, routePane: .operatorRoute, request: request)
}

@Suite struct DecisionQueueTests {
    @Test func decisionQueueDecodesEveryKindAndSkipsWhatItCannotRead() throws {
        let queue = try JSONDecoder().decode(TendPendingQueue.self, from: Data(queueJSON.utf8))
        #expect(queue.items.count == 3)
        #expect(queue.unavailable == ["review items: locked"])
        guard case .humanRequest(let human) = queue.items[0] else { Issue.record("not a human request"); return }
        #expect(human.decision?.options == ["Merge", "Hold"])
        // The unknown evidence kind is dropped; the others stay in order.
        #expect(human.decision?.evidence == [
            .commit(sha: "1a2b3c4d", repo: "/src/tend"),
            .pullRequest(url: "https://github.com/Open330/tend/pull/12"),
            .test(command: "cargo test", passed: true, tail: "ok"),
        ])
        guard case .review(let review) = queue.items[2] else { Issue.record("not a review"); return }
        // Typed evidence first, then the anchor read as evidence.
        #expect(review.allEvidence == [.diff(range: "main..feat/x", repo: nil), .commit(sha: "abcdef12", repo: nil)])
    }

    @Test func decisionQueueIsReadAsTheConsoleOnlyWhenTheDaemonHasIt() async throws {
        let requests = Requests()
        let client = TendIPCClient(socketPath: "/tmp/tend-decisions.sock") { _, payload in
            let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
            requests.add(object)
            if object["kind"] as? String == "hello" { return try hello(["decision_queue_v1", "review_items_v1"]) }
            let queue = try JSONSerialization.jsonObject(with: Data(queueJSON.utf8))
            return try JSONSerialization.data(withJSONObject: ["ok": true, "decision_queue": queue])
        }
        try await client.hello()
        let queue = try #require(try await client.decisionQueue())
        #expect(queue.items.count == 3)
        let sent = try #require(requests.all.last)
        #expect(sent["kind"] as? String == "decision_queue")
        #expect((sent["origin"] as? [String: Any])?["console"] as? Bool == true)

        let older = TendIPCClient(socketPath: "/tmp/tend-decisions-old.sock") { _, payload in
            let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
            #expect(object["kind"] as? String == "hello", "an older daemon is never asked for the queue")
            return try hello(["review_items_v1"])
        }
        try await older.hello()
        #expect(try await older.decisionQueue() == nil)
    }

    @Test func homeListsTheQueueThenOtherHostsQuestionsOnce() throws {
        let queue = try JSONDecoder().decode(TendPendingQueue.self, from: Data(queueJSON.utf8))
        let messages = [
            try message(host: local, id: "req_1", created: "2026-11-12T01:00:00Z"),
            try message(host: remote, id: "req_r", created: "2026-11-12T00:00:00Z"),
            try message(host: remote, id: "req_done", created: "2026-11-12T00:00:00Z", status: "completed"),
        ]
        let decisions = FleetDecisions.merge(queue: queue, reviews: [], messages: messages, localHost: local)
        // The waiting agent is already a Work card; the local request is the
        // queue's copy; the remote one follows; the answered one is gone.
        #expect(decisions.map(\.id) == ["human:mac:req_1", "review:rv_1", "human:mini:req_r"])
        #expect(decisions[0].detail == "Merge #12 now?", "the queue's copy wins")

        // A partial queue (its human-request source unavailable) is not
        // proof of absence: this Mac's mailbox copy still shows.
        let partial = TendPendingQueue(items: [], unavailable: ["human requests: locked"])
        let local2 = try message(host: local, id: "req_2", created: "2026-11-12T01:00:00Z")
        #expect(FleetDecisions.merge(queue: partial, reviews: [], messages: [local2], localHost: local).map(\.id) == ["human:mac:req_2"])
        // A complete queue that no longer holds it means it was answered;
        // the mailbox's older copy does not bring it back.
        #expect(FleetDecisions.merge(queue: TendPendingQueue(), reviews: [], messages: [local2], localHost: local).isEmpty)
        // An entry this app could not decode makes the read partial too.
        #expect(!queue.isComplete && queue.undecoded == 1)
        #expect(decisions[0].title == "Merge #12 or hold for the perf run?")
        #expect(decisions[0].detail == "Merge #12 now?")
        #expect(decisions[0].asker == "@impl")
        #expect(decisions[2].options == ["v1", "v2"])
        #expect(decisions[2].host.alias == "mini")

        // Without the queue: this Mac's reviews, then every host's questions.
        let review = try #require({ if case .review(let item) = queue.items[2] { item } else { nil } }())
        let fallback = FleetDecisions.merge(queue: nil, reviews: [review], messages: messages, localHost: local)
        #expect(fallback.map(\.id) == ["review:rv_1", "human:mini:req_r", "human:mac:req_1"])
    }

    @Test func evidenceOpensChangesOnlyForAReadableLocalCheckout() throws {
        let queue = try JSONDecoder().decode(TendPendingQueue.self, from: Data(queueJSON.utf8))
        var decisions = FleetDecisions.merge(queue: queue, reviews: [], messages: [], localHost: local)
        let human = decisions[0]
        #expect(FleetEvidenceAction.of(.commit(sha: "1a2b3c4d", repo: "/src/tend"), decision: human)
            == .changes(FleetRevisionTarget(repo: "/src/tend", revision: try #require(ChangesRevision(commit: "1a2b3c4d")))))
        #expect(FleetEvidenceAction.of(.pullRequest(url: "https://github.com/o/r/pull/1"), decision: human)
            == .url(try #require(URL(string: "https://github.com/o/r/pull/1"))))
        if case .unavailable = FleetEvidenceAction.of(.pullRequest(url: "file:///etc/passwd"), decision: human) {} else {
            Issue.record("only web addresses open in the browser")
        }
        // No checkout recorded: the asking agent's working directory, once known.
        var review = decisions[1]
        if case .unavailable = FleetEvidenceAction.of(.diff(range: "main..feat/x", repo: nil), decision: review) {} else {
            Issue.record("a range with no checkout cannot open")
        }
        review.askerCwd = "/src/tend-wt"
        #expect(FleetEvidenceAction.of(.diff(range: "main..feat/x", repo: nil), decision: review)
            == .changes(FleetRevisionTarget(repo: "/src/tend-wt", revision: try #require(ChangesRevision(range: "main..feat/x")))))
        // Anything that reads as a git option never reaches git.
        if case .unavailable = FleetEvidenceAction.of(.diff(range: "--output=/tmp/x", repo: "/src/tend"), decision: review) {} else {
            Issue.record("an option-like range must not open")
        }
        // Another host's checkout is not on this Mac's disk.
        decisions = FleetDecisions.merge(queue: nil, reviews: [], messages: [try message(host: remote, id: "r", created: "2026-11-12T00:00:00Z")], localHost: local)
        if case .unavailable = FleetEvidenceAction.of(.commit(sha: "1a2b3c4d", repo: "/src/tend"), decision: decisions[0]) {} else {
            Issue.record("a remote host's commit cannot open here")
        }
        #expect(TendEvidence.pullRequestLabel("https://github.com/Open330/tend/pull/12") == "Open330/tend#12")
    }

    @Test func askersAreFoundOnTheSameHostOnly() throws {
        let queue = try JSONDecoder().decode(TendPendingQueue.self, from: Data(queueJSON.utf8))
        let decisions = FleetDecisions.merge(queue: queue, reviews: [], messages: [], localHost: local)
        let agent = try JSONDecoder().decode(TendAgent.self, from: Data("""
        {"kind":"claude","agent_session_id":"s-impl","cwd":"/src/tend","state":"idle"}
        """.utf8))
        let attached = FleetDecisions.attachAskers(decisions, agents: [
            TendHostedAgent(host: remote, agent: agent, pane: nil),
            TendHostedAgent(host: local, agent: agent, pane: nil),
        ])
        #expect(attached[0].askerCwd == "/src/tend")
        #expect(attached[0].askerAgentID == "mac:\(agent.id)")
        #expect(attached[0].repo == "tend")
    }

    @Test(.enabled(if: GitRunner.shared != nil))
    func recordedRevisionsResolveToTheirTwoEnds() async throws {
        let git = try #require(GitRunner.shared)
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("tend-revision-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let dir = root.path
        func run(_ args: String...) async -> String {
            let output = await git.run(
                ["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"] + args,
                in: dir, limit: 1 << 20, timeout: 10
            )
            #expect(output.status == 0, "git \(args.joined(separator: " ")): \(output.stderr)")
            return output.stdoutText.trimmingCharacters(in: .whitespacesAndNewlines)
        }
        _ = await run("init", "-q", "-b", "main")
        try "one\n".write(toFile: "\(dir)/a.txt", atomically: true, encoding: .utf8)
        _ = await run("add", ".")
        _ = await run("commit", "-q", "-m", "first")
        let first = await run("rev-parse", "HEAD")
        _ = await run("checkout", "-q", "-b", "feat")
        try "one\ntwo\n".write(toFile: "\(dir)/a.txt", atomically: true, encoding: .utf8)
        try "new\n".write(toFile: "\(dir)/b.txt", atomically: true, encoding: .utf8)
        _ = await run("add", ".")
        _ = await run("commit", "-q", "-m", "second")
        let second = await run("rev-parse", "HEAD")
        // A working-tree edit never shows in a recorded revision.
        try "dirty\n".write(toFile: "\(dir)/a.txt", atomically: true, encoding: .utf8)

        let loader = ChangesLoader(git: git)
        let commit = try await loader.resolve(try #require(ChangesRevision(commit: String(second.prefix(10)))), root: dir).get()
        #expect(commit.sha == first)
        #expect(commit.head == second)
        let files = try await loader.branchFiles(root: dir, base: commit).get()
        #expect(files.map { "\($0.path) +\($0.added ?? -1) -\($0.deleted ?? -1)" }.sorted() == ["a.txt +1 -0", "b.txt +1 -0"])
        let diff = try await loader.diff(root: dir, file: try #require(files.first { $0.path == "a.txt" }), base: commit).get()
        #expect(diff.file.hunks.flatMap(\.lines).contains { $0.kind == .added && $0.text == "two" })
        #expect(!diff.file.hunks.flatMap(\.lines).contains { $0.text == "dirty" })

        // The root commit compares with the empty tree.
        let rootCommit = try await loader.resolve(try #require(ChangesRevision(commit: first)), root: dir).get()
        #expect(rootCommit.head == first)
        #expect(try await loader.branchFiles(root: dir, base: rootCommit).get().map(\.path) == ["a.txt"])

        let range = try await loader.resolve(try #require(ChangesRevision(range: "main..feat")), root: dir).get()
        #expect(range.sha == first && range.head == second)
        let symmetric = try await loader.resolve(try #require(ChangesRevision(range: "main...feat")), root: dir).get()
        #expect(symmetric.sha == first && symmetric.head == second)
        if case .success = await loader.resolve(try #require(ChangesRevision(commit: "deadbeef")), root: dir) {
            Issue.record("an unknown commit must not resolve")
        }
        if case .success = await loader.resolve(try #require(ChangesRevision(range: "main")), root: dir) {
            Issue.record("one revision alone compares with the working tree and must not open")
        }
        #expect(ChangesRevision(range: "--output=x") == nil)
        #expect(ChangesRevision(commit: "xyz") == nil)
    }

    /// A partial clone's missing blobs stay missing: Changes never fetches
    /// from the promisor remote, whatever git version honours which switch.
    @Test(.enabled(if: GitRunner.shared != nil))
    func aPartialCloneIsNeverFetchedFrom() async throws {
        let git = try #require(GitRunner.shared)
        let base = FileManager.default.temporaryDirectory.appendingPathComponent("tend-promisor-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: base, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: base) }
        let source = base.appendingPathComponent("src").path
        let clone = base.appendingPathComponent("clone").path
        try FileManager.default.createDirectory(atPath: source, withIntermediateDirectories: true)
        // Plain git, outside the app's settings, to build the fixture.
        func plain(_ args: [String], in dir: String) async -> GitOutput {
            let output = await BoundedProcess.run(
                executable: git.executable,
                arguments: ["-C", dir, "-c", "user.name=t", "-c", "user.email=t@example.com",
                            "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"] + args,
                limit: 1 << 20, timeout: 20
            )
            #expect(output.status == 0, "git \(args.joined(separator: " ")): \(output.stderr)")
            return output
        }
        _ = await plain(["init", "-q", "-b", "main"], in: source)
        try "one\n".write(toFile: "\(source)/a.txt", atomically: true, encoding: .utf8)
        _ = await plain(["add", "."], in: source)
        _ = await plain(["commit", "-q", "-m", "first"], in: source)
        try "one\ntwo\n".write(toFile: "\(source)/a.txt", atomically: true, encoding: .utf8)
        _ = await plain(["commit", "-q", "-am", "second"], in: source)
        _ = await plain(["config", "uploadpack.allowfilter", "true"], in: source)
        _ = await plain(["clone", "-q", "--no-checkout", "--filter=blob:none", "file://\(source)", clone], in: base.path)
        let head = await plain(["rev-parse", "HEAD"], in: clone).stdoutText.trimmingCharacters(in: .whitespacesAndNewlines)
        func missing() async -> Int {
            let output = await plain(["rev-list", "--objects", "--missing=print", "HEAD"], in: clone)
            return output.stdoutText.split(separator: "\n").filter { $0.hasPrefix("?") }.count
        }
        let before = await missing()
        #expect(before > 0, "the fixture must start with promised blobs")

        let loader = ChangesLoader(git: git)
        let revision = try #require(ChangesRevision(commit: head))
        let resolved = try await loader.resolve(revision, root: clone).get()
        let files = try await loader.branchFiles(root: clone, base: resolved).get()
        if let file = files.first {
            if case .success(let loaded) = await loader.diff(root: clone, file: file, base: resolved) {
                #expect(loaded.file.hunks.isEmpty, "a promised blob must not be fetched to show its diff")
            }
        }
        #expect(await missing() == before, "Changes fetched from the promisor remote")
    }

    @MainActor
    @Test func oneAnswerIsOpenAtATimeAndReopeningKeepsItsText() {
        let feed = FleetDecisionFeed()
        let first = feed.draft(for: "a")
        let second = feed.draft(for: "b")
        first.wrappedValue = "half an answer"
        second.wrappedValue = ""
        #expect(first.wrappedValue == nil, "only one composer is open")
        #expect(second.wrappedValue == "")
        first.wrappedValue = ""
        #expect(first.wrappedValue == "half an answer", "reopening keeps the text")
        #expect(second.wrappedValue == nil)
        first.wrappedValue = "Merge now"
        #expect(first.wrappedValue == "Merge now", "an option replaces it")
        first.wrappedValue = nil
        first.wrappedValue = ""
        #expect(first.wrappedValue == "", "Cancel discards it")
    }

    @MainActor
    @Test func aRecordedRevisionTakesNoReviewCommentsOrViewedMarks() throws {
        let revision = try #require(ChangesRevision(commit: "abcdef12"))
        let model = ChangesModel(source: ChangesSource(hostAlias: "local", isLocal: true, path: "/tmp"), revision: revision)
        #expect(model.compare == .revision)
        model.setCompare(.uncommitted)
        #expect(model.compare == .revision)
        #expect(model.draftKey == nil)
    }

    @Test func tendsOwnChecksComeFirstAndAreNotRepeatedAsEvidence() throws {
        let json = """
        {"kind":"review","id":"rv_9","target":{"kind":"work","workspace":"tend","work":"ENG-2"},
         "generation":1,"summary":"Done","requested_at":"2026-11-12T01:02:00Z",
         "requested_by":{"kind":"agent","label":"impl"},
         "verification":{"passed":true,"ran_at":"2026-11-12T01:01:00Z","checks":[
           {"command":"cargo test -p tend","name":"unit","passed":true,"exit_code":0,"tail":"ok"},
           {"command":"node --test","passed":true,"exit_code":0,"tail":"","future":1}]},
         "decision":{"evidence":[
           {"kind":"test","command":"cargo test -p tend","passed":true,"verified":true},
           {"kind":"test","command":"make lint","passed":true}]}}
        """
        let item = try JSONDecoder().decode(TendReviewItem.self, from: Data(json.utf8))
        let verification = try #require(item.verification)
        #expect(verification.passed)
        #expect(verification.checks.map(\.label) == ["unit", "node --test"])
        #expect(verification.checks.allSatisfy { $0.ending == nil })
        let decision = FleetDecision(review: item, host: local)
        #expect(decision.verification == verification)
        // The agent's own test stays; tend's copy of its check does not.
        #expect(decision.evidence == [.test(command: "make lint", passed: true, tail: nil)])
    }

    @Test func anAgentsOwnClaimAboutTheSameCommandStays() throws {
        let json = """
        {"kind":"review","id":"rv_8","target":{"kind":"work","workspace":"tend","work":"ENG-2"},
         "generation":1,"summary":"Done","requested_by":{"kind":"agent","label":"impl"},
         "verification":{"passed":true,"checks":[{"command":"cargo test","passed":true,"tail":""}]},
         "decision":{"evidence":[
           {"kind":"test","command":"cargo test","passed":true,"verified":true},
           {"kind":"test","command":"cargo test","passed":false,"tail":"agent saw a failure"}]}}
        """
        let item = try JSONDecoder().decode(TendReviewItem.self, from: Data(json.utf8))
        #expect(item.allEvidence == [.test(command: "cargo test", passed: false, tail: "agent saw a failure")])
    }

    @Test func aMalformedVerificationLeavesTheItemVisible() throws {
        let json = """
        {"kind":"review","id":"rv_7","target":{"kind":"session","provider":"claude","session_id":"s"},
         "generation":1,"summary":"Done","requested_by":{"kind":"agent"},
         "verification":"future-or-malformed-record",
         "decision":{"evidence":[{"kind":"test","command":"x","passed":true,"verified":true}]}}
        """
        let item = try JSONDecoder().decode(TendReviewItem.self, from: Data(json.utf8))
        #expect(item.verification == nil)
        // Without checks to show, tend's copy stays in the evidence.
        #expect(item.allEvidence == [.test(command: "x", passed: true, tail: nil)])
    }

    @Test func aFailedCheckSaysHowItEnded() throws {
        let json = """
        {"passed":true,"checks":[{"command":"a","passed":false,"exit_code":2,"tail":"boom"},
                                 {"command":"b","passed":false,"timed_out":true,"tail":""},
                                 {"command":"c","passed":false,"tail":""},{"bogus":true}]}
        """
        let verification = try JSONDecoder().decode(TendVerification.self, from: Data(json.utf8))
        // A record that says it passed while a check failed reads as failed.
        #expect(!verification.passed)
        #expect(verification.checks.map(\.ending) == ["exit 2", "timed out", "killed"])
    }

    @Test func aReviewWithoutChecksShowsNone() throws {
        let queue = try JSONDecoder().decode(TendPendingQueue.self, from: Data(queueJSON.utf8))
        guard case .review(let review) = queue.items[2] else { Issue.record("not a review"); return }
        #expect(review.verification == nil)
        #expect(FleetDecision(review: review, host: local).verification == nil)
    }
}
