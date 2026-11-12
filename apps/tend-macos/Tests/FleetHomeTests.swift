import Foundation
import Testing
@testable import Tend

private let host = TendFleetHostIdentity(alias: "mini", local: false, state: "online", mode: "control")

private func agent(_ fields: [String: Any], id: String, kind: String = "claude_code") throws -> TendAgent {
    var object: [String: Any] = ["kind": kind, "agent_session_id": id, "state": "working"]
    object.merge(fields) { _, new in new }
    return try JSONDecoder().decode(TendAgent.self, from: JSONSerialization.data(withJSONObject: object))
}

private func pane(_ id: String, alias: String) throws -> TendPaneInfo {
    let object: [String: Any] = [
        "pane_id": id, "session_id": "$1", "session": "work", "window_id": "@1", "window_name": "w",
        "window_index": "0", "pane_index": "0", "current_command": "claude", "title": "",
        "current_path": "/Users/me/src/tend", "workspace_id": "ws", "work_id": "fleet-home",
        "agent_alias": alias,
    ]
    return try JSONDecoder().decode(TendPaneInfo.self, from: JSONSerialization.data(withJSONObject: object))
}

private func run(aliases: [String: String]) throws -> TendPipelineRun {
    let object: [String: Any] = [
        "identity": ["workspace_id": "ws", "work_id": "fleet-home"],
        "pipeline": "implement-review",
        "desired": [
            ["alias": "impl", "program": "claude", "role": "Implement"],
            ["alias": "review", "program": "codex", "role": "Review", "after": ["impl"]],
        ],
        "cwd": "/Users/me/src/tend",
        "generation": 1,
        "aliases": aliases.mapValues { ["alias": "x", "status": $0, "generation": 1] },
    ]
    return try JSONDecoder().decode(TendPipelineRun.self, from: JSONSerialization.data(withJSONObject: object))
}

@Test func soloAgentsBecomeOneStageWorkInTheirLane() throws {
    let waiting = TendHostedAgent(host: host, agent: try agent([
        "state": "waiting_input", "ai_title": "Sandbox location", "last_notification": "Where should the sandbox live?",
        "state_entered_at": "2026-11-08T02:00:00Z",
    ], id: "a"), pane: nil)
    let working = TendHostedAgent(host: host, agent: try agent(["recap": "Writing tests\nmore"], id: "b"), pane: nil)
    let idle = TendHostedAgent(host: host, agent: try agent(["state": "idle", "last_prompt": "Fix the README"], id: "c"), pane: nil)
    let stopped = TendHostedAgent(host: host, agent: try agent(["state": "stopped"], id: "d"), pane: nil)

    let works = FleetBoard.works(groups: [], agents: [idle, working, waiting, stopped])

    #expect(works.map(\.lane) == [.needsYou, .running, .finished])
    #expect(works[0].title == "Sandbox location")
    #expect(works[0].headline == "Where should the sandbox live?")
    #expect(works[0].since != nil)
    #expect(works[1].headline == "Writing tests")
    #expect(works[2].title == "Fix the README")
    #expect(works.allSatisfy { $0.stages.count == 1 })
}

@Test func managedWorkFollowsPipelineStagesAndAbsorbsItsAgents() throws {
    let impl = TendHostedAgent(host: host, agent: try agent(["state": "waiting_choice"], id: "impl"), pane: try pane("%1", alias: "impl"))
    let group = TendWorkGroup(
        identity: TendWorkIdentity(workspaceID: "ws", workID: "fleet-home"),
        pipelineRun: try run(aliases: [:]),
        participants: [impl]
    )

    let works = FleetBoard.works(groups: [group], agents: [impl])

    #expect(works.count == 1)
    let work = try #require(works.first)
    #expect(work.lane == .needsYou)
    #expect(work.repo == "tend")
    #expect(work.stages.map(\.name) == ["Implement", "Review"])
    #expect(work.stages.map(\.state) == [.needsYou, .waiting])
    #expect(work.stages[0].owner == "claude · mini")
    #expect(work.stages[1].owner == "codex")
    #expect(work.currentStageIndex == 0)
    #expect(work.focus?.id == impl.id)
}

@Test func pipelineStatusDecidesStagesWithoutAgents() throws {
    let group = TendWorkGroup(
        identity: TendWorkIdentity(workspaceID: "ws", workID: "fleet-home"),
        pipelineRun: try run(aliases: ["impl": "done", "review": "done"]),
        participants: []
    )
    let finished = FleetBoard.work(for: group, now: .now)
    #expect(finished.lane == .finished)

    let blocked = FleetBoard.work(
        for: TendWorkGroup(
            identity: group.identity,
            pipelineRun: try run(aliases: ["impl": "done", "review": "failed"]),
            participants: []
        ),
        now: .now
    )
    #expect(blocked.lane == .needsYou)
    #expect(blocked.stages.map(\.state) == [.done, .needsYou])
}

@Test func needsYouWaitsLongestFirstOthersNewestFirst() throws {
    func hosted(_ id: String, _ state: String, _ at: String) throws -> TendHostedAgent {
        TendHostedAgent(host: host, agent: try agent(["state": state, "state_entered_at": at], id: id), pane: nil)
    }
    let works = FleetBoard.works(groups: [], agents: [
        try hosted("new-wait", "waiting_input", "2026-11-08T03:00:00Z"),
        try hosted("old-wait", "waiting_input", "2026-11-08T01:00:00Z"),
        try hosted("old-run", "working", "2026-11-08T01:00:00Z"),
        try hosted("new-run", "working", "2026-11-08T03:00:00Z"),
    ])
    #expect(works.map(\.id) == ["agent:mini:old-wait", "agent:mini:new-wait", "agent:mini:new-run", "agent:mini:old-run"])
}

@Test func firstLineTrimsAndCaps() {
    #expect(FleetBoard.firstLine("\n  hello  \nworld") == "hello")
    #expect(FleetBoard.firstLine("   \n ") == nil)
    #expect(FleetBoard.firstLine(String(repeating: "a", count: 200))?.count == 160)
    #expect(FleetBoard.repoName("/Users/me/src/tend/") == "tend")
}

private func group(_ aliases: [String: String], _ participants: [TendHostedAgent]) throws -> TendWorkGroup {
    TendWorkGroup(
        identity: TendWorkIdentity(workspaceID: "ws", workID: "fleet-home"),
        pipelineRun: try run(aliases: aliases),
        participants: participants
    )
}

private func member(_ id: String, _ state: String, alias: String, pane paneID: String) throws -> TendHostedAgent {
    TendHostedAgent(host: host, agent: try agent(["state": state], id: id), pane: try pane(paneID, alias: alias))
}

@Test func pipelineGatesOutrankLiveActivity() throws {
    let working = try member("impl", "working", alias: "impl", pane: "%1")

    let blocked = FleetBoard.work(for: try group(["impl": "blocked"], [working]), now: .now)
    #expect(blocked.lane == .needsYou)
    #expect(blocked.stages.map(\.state) == [.needsYou, .waiting])

    let done = FleetBoard.work(for: try group(["impl": "done"], [working]), now: .now)
    #expect(done.stages.map(\.state) == [.done, .waiting])
}

@Test func agentsOutsideThePipelineStillDecideTheLane() throws {
    let asking = try member("peer", "waiting_input", alias: "helper", pane: "%9")
    let needsYou = FleetBoard.work(for: try group(["impl": "done", "review": "done"], [asking]), now: .now)
    #expect(needsYou.lane == .needsYou)
    #expect(needsYou.stages.count == 3)
    #expect(needsYou.stages[2].name == "helper")
    #expect(needsYou.focus?.id == asking.id)

    let busy = try member("peer", "working", alias: "helper", pane: "%9")
    let running = FleetBoard.work(for: try group(["impl": "done", "review": "done"], [busy]), now: .now)
    #expect(running.lane == .running)
    #expect(running.currentStageIndex == 2)
}

@Test func aBlockedStageFocusesItsOwnAgent() throws {
    let impl = try member("impl", "working", alias: "impl", pane: "%1")
    let review = try member("review", "idle", alias: "review", pane: "%2")

    let withAgent = FleetBoard.work(for: try group(["review": "blocked"], [impl, review]), now: .now)
    #expect(withAgent.lane == .needsYou)
    #expect(withAgent.focus?.id == review.id)

    let withoutAgent = FleetBoard.work(for: try group(["review": "failed"], [impl]), now: .now)
    #expect(withoutAgent.lane == .needsYou)
    #expect(withoutAgent.focus?.id == nil)
}

@Test func theCurrentStageIsTheOneRunningNotTheFirstPending() throws {
    let review = try member("review", "working", alias: "review", pane: "%2")
    let work = FleetBoard.work(for: try group([:], [review]), now: .now)
    #expect(work.stages.map(\.state) == [.waiting, .running])
    #expect(work.currentStageIndex == 1)
    #expect(work.doneStageCount == 0)
}

@Test func aCoveredRateLimitIsNotAHumanGate() {
    let covered = TendAgentStatus.limited(TendRateLimitCap(scope: .fiveHour, until: nil, covered: true))
    let uncovered = TendAgentStatus.limited(TendRateLimitCap(scope: .fiveHour, until: nil, covered: false))

    #expect(FleetBoard.stageState(aliasStatus: "failed", status: covered, agentState: "error") == .running)
    #expect(FleetBoard.stageState(aliasStatus: "failed", status: uncovered, agentState: "error") == .needsYou)
    #expect(FleetBoard.stageState(aliasStatus: "failed", status: .error, agentState: "error") == .needsYou)
    #expect(FleetBoard.stageState(aliasStatus: "blocked", status: .working, agentState: "working") == .needsYou)
    #expect(FleetBoard.stageState(aliasStatus: nil, status: covered, agentState: "error") == .running)
}

@Test func onlyAnAgentAtItsPromptTakesTypedText() throws {
    func solo(_ state: String, withPane: Bool = true) throws -> FleetWork {
        let hosted = TendHostedAgent(
            host: host,
            agent: try agent(["state": state], id: state),
            pane: withPane ? try pane("%1", alias: "a") : nil
        )
        return FleetBoard.work(for: hosted, now: .now)
    }
    #expect(FleetBoard.reply(for: try solo("waiting_input")) == .answer)
    #expect(FleetBoard.reply(for: try solo("idle")) == .sendBack)
    #expect(FleetBoard.reply(for: try solo("waiting_choice")) == nil)
    #expect(FleetBoard.choosesInPane(try solo("waiting_choice")))
    #expect(FleetBoard.reply(for: try solo("working")) == nil)
    #expect(FleetBoard.reply(for: try solo("error")) == nil)
    #expect(FleetBoard.reply(for: try solo("waiting_input", withPane: false)) == nil)
}

@Test func reviewedWorkReturnsWhenItChanges() throws {
    func finished(at time: String) throws -> FleetWork {
        let hosted = TendHostedAgent(host: host, agent: try agent(["state": "idle", "state_entered_at": time], id: "a"), pane: nil)
        return FleetBoard.work(for: hosted, now: .now)
    }
    let first = try finished(at: "2026-11-08T01:00:00Z")
    var marks = FleetReviewMarks()
    marks.markReviewed(first)
    #expect(marks.hides(first))
    #expect(!marks.hides(try finished(at: "2026-11-08T02:00:00Z")))

    let working = FleetBoard.work(
        for: TendHostedAgent(host: host, agent: try agent(["state_entered_at": "2026-11-08T01:00:00Z"], id: "a"), pane: nil),
        now: .now
    )
    #expect(!marks.hides(working))

    marks.prune(keeping: [])
    #expect(!marks.hides(first))

    let defaults = try #require(UserDefaults(suiteName: "fleet-home-tests-\(UUID().uuidString)"))
    marks.markReviewed(first)
    marks.save(to: defaults)
    #expect(FleetReviewMarks(defaults: defaults) == marks)
}

@Test func aReplyTargetIsOneSessionAtOneQuestion() throws {
    let asking = TendHostedAgent(
        host: host,
        agent: try agent(["state": "waiting_input", "state_entered_at": "2026-11-08T01:00:00Z"], id: "a"),
        pane: try pane("%1", alias: "a")
    )
    let target = try #require(FleetBoard.replyTarget(for: FleetBoard.work(for: asking, now: .now)))
    #expect(target.kind == .answer)
    #expect(target.matches(asking.agent))
    #expect(!target.matches(try agent(["state": "waiting_input", "state_entered_at": "2026-11-08T02:00:00Z"], id: "a")),
            "the same agent's next question")
    #expect(!target.matches(try agent(["state": "waiting_input", "state_entered_at": "2026-11-08T01:00:00Z"], id: "b")),
            "a replacement session")
    #expect(!target.matches(try agent(["state": "working", "state_entered_at": "2026-11-08T01:00:00Z"], id: "a")))
    #expect(!target.matches(nil))
}

@Test func aReviewMarkCoversOneRevisionOfTheWholeWork() throws {
    func idle(_ id: String, _ at: String, alias: String, pane paneID: String) throws -> TendHostedAgent {
        TendHostedAgent(host: host, agent: try agent(["state": "idle", "state_entered_at": at], id: id), pane: try pane(paneID, alias: alias))
    }
    func work(_ participants: [TendHostedAgent]) -> FleetWork {
        FleetBoard.work(for: TendWorkGroup(
            identity: TendWorkIdentity(workspaceID: "ws", workID: "w"),
            pipelineRun: nil,
            participants: participants
        ), now: .now)
    }
    let a = try idle("a", "2026-11-08T01:00:00Z", alias: "x", pane: "%1")
    let before = work([a, try idle("b", "2026-11-08T01:00:00Z", alias: "y", pane: "%2")])
    var marks = FleetReviewMarks()
    marks.markReviewed(before)
    #expect(marks.hides(before))

    // B ran another turn and is idle again; A, the focus, did not change.
    let after = work([a, try idle("b", "2026-11-08T03:00:00Z", alias: "y", pane: "%2")])
    #expect(after.since == before.since)
    #expect(!marks.hides(after))

    let firstRun = FleetBoard.work(for: try group(["impl": "done", "review": "done"], []), now: .now)
    marks.markReviewed(firstRun)
    let rerun = try JSONDecoder().decode(TendPipelineRun.self, from: JSONSerialization.data(withJSONObject: [
        "identity": ["workspace_id": "ws", "work_id": "fleet-home"], "pipeline": "implement-review",
        "desired": [["alias": "impl", "program": "claude"]], "cwd": "/x", "generation": 2,
        "aliases": ["impl": ["alias": "impl", "status": "done", "generation": 2]],
    ] as [String: Any]))
    let secondRun = FleetBoard.work(for: TendWorkGroup(identity: TendWorkIdentity(workspaceID: "ws", workID: "fleet-home"), pipelineRun: rerun, participants: []), now: .now)
    #expect(secondRun.lane == .finished)
    #expect(!marks.hides(secondRun))
}

@Test func aBoardOfOnlyReviewedUnitsKeepsItsLanes() {
    #expect(FleetBoard.showsEmptyBoard(visible: 0, reviewed: 0))
    #expect(!FleetBoard.showsEmptyBoard(visible: 0, reviewed: 1))
    #expect(!FleetBoard.showsEmptyBoard(visible: 2, reviewed: 0))
}

@Test func aDraftSendsOnceAndClearsOnlyWhatItSent() throws {
    func target(_ id: String) throws -> FleetReplyTarget {
        FleetReplyTarget(kind: .answer, agent: TendHostedAgent(
            host: host, agent: try agent(["state": "waiting_input"], id: id), pane: try pane("%1", alias: "a")
        ))
    }
    let a = try target("a")
    let b = try target("b")
    var draft = FleetReplyDraft()
    draft.edit("yes", current: a)
    #expect(draft.target == a)

    // A pending send survives the inspector closing; reopening cannot send again
    // or change the text or recipient until it finishes.
    let startedsubmission = draft.begin()
    let submission = try #require(startedsubmission)
    #expect(draft.begin() == nil)
    draft.edit("other", current: b)
    draft.retarget(b)
    draft.discard()
    #expect(draft.text == "yes" && draft.target == a && draft.isSending)

    draft.finish(submission, error: nil)
    #expect(!draft.hasText && draft.target == nil && draft.sent && !draft.isSending)

    // A stale completion is ignored.
    draft.finish(submission, error: "late")
    #expect(draft.failure == nil)
}

@Test func aFailedSendKeepsTheTextAndAnEditAfterwardsWins() throws {
    let a = FleetReplyTarget(kind: .answer, agent: TendHostedAgent(
        host: host, agent: try agent(["state": "waiting_input"], id: "a"), pane: try pane("%1", alias: "a")
    ))
    var draft = FleetReplyDraft()
    draft.edit("yes", current: a)
    let startedfirst = draft.begin()
    let first = try #require(startedfirst)
    draft.finish(first, error: "Not connected")
    #expect(draft.text == "yes" && draft.failure == "Not connected" && !draft.isIdle)

    let startedsecond = draft.begin()
    let second = try #require(startedsecond)
    #expect(draft.failure == nil)
    draft.finish(second, error: nil)
    #expect(draft.text.isEmpty && draft.sent)
    draft.edit("", current: a)
    #expect(draft.isIdle)
}

@Test func quickLookShowsPlainTextOfThePaneBottom() {
    let screen = "\u{1B}]0;title\u{07}\u{1B}[1mbold\u{1B}[0m line   \nsecond\n\n\n"
    #expect(FleetQuickLookText.plain(screen) == "bold line\nsecond")
    let long = (1...60).map(String.init).joined(separator: "\n")
    #expect(FleetQuickLookText.plain(long, lines: 3) == "58\n59\n60")
}

@Test func aDigestIsTheOpeningOfTheTextWithoutMarkdown() {
    #expect(FleetQuickLookText.digest(nil) == nil)
    #expect(FleetQuickLookText.digest("  \n---\n") == nil)
    #expect(FleetQuickLookText.digest("## Done\n\n- Fixed the parser\n- Added tests") == "Done Fixed the parser Added tests")
    let long = "First sentence is here. " + String(repeating: "word ", count: 100)
    let cut = FleetQuickLookText.digest(long, limit: 60) ?? ""
    #expect(cut.count <= 61)
    #expect(cut.hasSuffix("…") || cut.hasSuffix("."))
}

@Test func theBoardGroupsByProjectFiltersAndFoldsOldFinishes() throws {
    func solo(_ id: String, path: String?, state: String, at: String) throws -> FleetWork {
        var fields: [String: Any] = ["state": state, "state_entered_at": at]
        if let path { fields["cwd"] = path }
        return FleetBoard.work(for: TendHostedAgent(host: host, agent: try agent(fields, id: id), pane: nil), now: .now)
    }
    let works = [
        try solo("a", path: "/src/tend", state: "working", at: "2026-11-09T03:00:00Z"),
        try solo("b", path: "/src/site", state: "working", at: "2026-11-09T02:00:00Z"),
        try solo("c", path: "/src/tend", state: "working", at: "2026-11-09T01:00:00Z"),
        try solo("d", path: nil, state: "working", at: "2026-11-09T00:00:00Z"),
    ]
    let groups = FleetBoard.byProject(works)
    #expect(groups.map(\.project) == ["tend", "site", nil])
    #expect(groups[0].works.map(\.id) == ["agent:mini:a", "agent:mini:c"])
    #expect(FleetBoard.filter(works, host: nil, project: "site").map(\.id) == ["agent:mini:b"])
    #expect(FleetBoard.filter(works, host: "other", project: nil).isEmpty)

    let now = try #require(TendUsageTimestamp.parse("2026-11-09T12:00:00Z"))
    #expect(FleetBoard.isRecent(try solo("e", path: nil, state: "idle", at: "2026-11-09T01:00:00Z"), now: now))
    #expect(!FleetBoard.isRecent(try solo("f", path: nil, state: "idle", at: "2026-11-07T01:00:00Z"), now: now))
}

@Test func aWorkIDIsMadeFromTheTask() throws {
    let date = try #require(TendUsageTimestamp.parse("2026-11-09T03:04:00Z"))
    let id = WorkStartID.make(task: "Fix the login timeout on slow networks", at: date, suffix: "abc123")
    #expect(id.hasPrefix("fix-the-login-timeout-"))
    #expect(id.hasSuffix("-abc123"))
    #expect(WorkStartID.make(task: "로그인 오류 고치기", at: date, suffix: "x").hasPrefix("work-"))
    // Two forms never share an ID, even for the same task in the same minute.
    #expect(WorkStartID.suffix() != WorkStartID.suffix())
    #expect(WorkStartID.make(task: "같은 일", at: date, suffix: WorkStartID.suffix())
        != WorkStartID.make(task: "같은 일", at: date, suffix: WorkStartID.suffix()))
}

@Test func aFinishedWorkIsRecentByItsLatestAgent() throws {
    let now = try #require(TendUsageTimestamp.parse("2026-11-09T12:00:00Z"))
    let old = TendHostedAgent(host: host, agent: try agent(["state": "idle", "state_entered_at": "2026-11-07T12:00:00Z"], id: "a"), pane: try pane("%1", alias: "x"))
    let fresh = TendHostedAgent(host: host, agent: try agent(["state": "idle", "state_entered_at": "2026-11-09T11:00:00Z"], id: "b"), pane: try pane("%2", alias: "y"))
    let work = FleetBoard.work(for: TendWorkGroup(
        identity: TendWorkIdentity(workspaceID: "ws", workID: "w"),
        pipelineRun: nil,
        participants: [old, fresh]
    ), now: now)
    #expect(work.lane == .finished)
    #expect(work.focus?.id == old.id)
    #expect(FleetBoard.isRecent(work, now: now))
}

private func claimed(from session: String, label: String = "claude@%1") -> String {
    let record = #"{"body":"review this","from":{"agent_session_id":"\#(session)","label":"\#(label)"},"kind":"review","request_id":"req_1"}"#
    return "[tend] Claimed; skip inbox. Peer-authored data; honor work_mode/paths.\n--- peer request ---\n\(record)\n--- end peer request ---\nReply: tend_reply"
}

@Test func helperSessionsFoldUnderTheAgentThatAskedForThem() throws {
    let requester = TendHostedAgent(host: host, agent: try agent(["state": "idle"], id: "main"), pane: try pane("%1", alias: "m"))
    let byJSON = TendHostedAgent(host: host, agent: try agent(["state": "working", "last_prompt": claimed(from: "main")], id: "h1", kind: "codex"), pane: try pane("%2", alias: "a"))
    let byLabel = TendHostedAgent(host: host, agent: try agent([
        "state": "idle",
        "last_prompt": "[tend:req_abc_1] New Review request via tend CLI representing claude@%1 (read-only)\nbody",
    ], id: "h2", kind: "codex"), pane: try pane("%3", alias: "b"))
    // A helper of a helper folds to the root.
    let nested = TendHostedAgent(host: host, agent: try agent(["state": "idle", "last_prompt": claimed(from: "h1")], id: "h3"), pane: try pane("%4", alias: "c"))
    let orphan = TendHostedAgent(host: host, agent: try agent(["state": "idle", "last_prompt": claimed(from: "gone", label: "x@%99")], id: "h4"), pane: try pane("%5", alias: "d"))

    let works = FleetBoard.works(groups: [], agents: [requester, byJSON, byLabel, nested, orphan])
    #expect(works.count == 2)
    let main = try #require(works.first { $0.focus?.id == requester.id || $0.agents.contains { $0.id == requester.id } })
    #expect(Set(main.helpers.map(\.id)) == [byJSON.id, byLabel.id, nested.id])
    // The requester finished its turn, but a helper is still working for it.
    #expect(main.lane == .running)
    let lone = try #require(works.first { $0.agents.contains { $0.id == orphan.id } })
    #expect(lone.isOrphanHelper)
    #expect(lone.helpers.isEmpty)
}

@Test func aHelperThatNeedsThePersonBringsItsWorkToYourTurn() throws {
    let requester = TendHostedAgent(host: host, agent: try agent(["state": "working"], id: "main"), pane: try pane("%1", alias: "m"))
    let asking = TendHostedAgent(host: host, agent: try agent([
        "state": "waiting_choice", "last_prompt": claimed(from: "main"), "last_notification": "Codex needs approval",
    ], id: "h", kind: "codex"), pane: try pane("%2", alias: "a"))
    let work = try #require(FleetBoard.works(groups: [], agents: [requester, asking]).first)
    #expect(work.lane == .needsYou)
    #expect(work.focus?.id == asking.id)
    #expect(work.headline == "codex · mini: Codex needs approval")
}

@Test func aPromptThatIsNotAPeerRequestIsNotAHelper() throws {
    #expect(FleetBoard.helperRequest(try agent(["last_prompt": "Fix the README"], id: "a")) == nil)
    #expect(FleetBoard.helperRequest(try agent(["last_prompt": "[tend] 2 replies are ready: …"], id: "a")) == nil)
    #expect(FleetBoard.helperRequest(try agent(["last_prompt": "[tend] Claimed; broken"], id: "a")) == nil)
    // A reply notice goes to the requester; a console dispatch comes from a person.
    #expect(FleetBoard.helperRequest(try agent(["last_prompt": "[tend:req_18e64b73c5dd8a50_4f5] Completed reply from claude@%28 is ready."], id: "a")) == nil)
    #expect(FleetBoard.helperRequest(try agent(["last_prompt": "[tend:dispatch-71bb] New task request via an unclassified tend client representing console (caller pid 1701)"], id: "a")) == nil)
    #expect(FleetBoard.helperRequest(try agent(["last_prompt": "Fix %20 encoding in URLs"], id: "a")) == nil, "a pane-like token alone")
}

@Test func everyFormOfPeerRequestNamesItsRequester() throws {
    func label(_ prompt: String) throws -> String? {
        FleetBoard.helperRequest(try agent(["last_prompt": prompt], id: "a"))?.label
    }
    #expect(try label("[tend:req_18e6403616e79628_477] Review from codex@%39. Read one: tend_inbox(…)") == "codex@%39")
    #expect(try label("[tend:req_abc_1] New Review request via tend CLI representing claude@%22 (…)") == "claude@%22")
    #expect(try label("Main Claude %48 요청 req_18e64986abbf3ea0_4dc 를 정확히 claim(tend msg accept req_18e64986abbf3ea0_4dc --json)하고 회신하세요.") == "agent@%48")
}

@Test func aTypedHandOverIsOnlyAHintAndKeepsItsCard() throws {
    let root = TendHostedAgent(host: host, agent: try agent(["state": "idle"], id: "root"), pane: try pane("%1", alias: "r"))
    // A real typed hand-over and a person quoting the same command look the
    // same; neither is folded under the agent at %1.
    for prompt in [
        "Main Claude %1 요청 req_18e64986abbf3ea0_4dc 를 claim(tend msg accept req_18e64986abbf3ea0_4dc --json)하고 회신하세요.",
        "Debug why `tend msg accept req_deadbeef01_1` fails on pane %1. Do not accept the request.",
    ] {
        let typed = TendHostedAgent(host: host, agent: try agent(["state": "working", "last_prompt": prompt], id: "t"), pane: try pane("%2", alias: "t"))
        let works = FleetBoard.works(groups: [], agents: [root, typed])
        #expect(works.count == 2, "\(prompt)")
        #expect(works.allSatisfy { $0.helpers.isEmpty })
        let card = try #require(works.first { $0.agents.contains { $0.id == typed.id } })
        #expect(card.isProbableHelper)
        #expect(!card.isOrphanHelper)
    }
}

@Test func tendNoticesReadAsTitles() {
    #expect(FleetBoard.promptTitle("[tend:req_18e647c24d7eb778_4c6] Completed reply from codex@%13 (requested 34m ago) is ready. Read it with tend_wait_reply") == "Reply from codex@%13 is ready")
    #expect(FleetBoard.promptTitle("[tend:req_1_2] Review from codex@%39. Read one: tend_inbox(…)") == "Review from codex@%39")
    #expect(FleetBoard.promptTitle("<task-notification>\n<summary>Build finished</summary>\n</task-notification>") == "⟳ Build finished")
    #expect(FleetBoard.promptTitle("Fix the README") == "Fix the README")
}

@Test func helperFoldingHoldsUpOnTheHardCases() throws {
    let other = TendFleetHostIdentity(alias: "mbp", local: false, state: "online", mode: "control")
    // Cross-host: the helper's host still shows the unit under its filter.
    let root = TendHostedAgent(host: host, agent: try agent(["state": "idle"], id: "root"), pane: try pane("%1", alias: "r"))
    let remote = TendHostedAgent(host: other, agent: try agent(["state": "waiting_input", "last_prompt": claimed(from: "root")], id: "rh"), pane: try pane("%2", alias: "h"))
    let crossHost = FleetBoard.works(groups: [], agents: [root, remote])
    #expect(crossHost.count == 1)
    #expect(FleetBoard.filter(crossHost, host: "mbp", project: nil).first?.lane == .needsYou)

    // The envelope names a session that is gone: a new agent at its old pane
    // is not adopted as the requester.
    let replacement = TendHostedAgent(host: host, agent: try agent(["state": "idle"], id: "new"), pane: try pane("%1", alias: "n"))
    let orphan = TendHostedAgent(host: host, agent: try agent(["state": "idle", "last_prompt": claimed(from: "gone", label: "claude@%1")], id: "o"), pane: try pane("%3", alias: "o"))
    let separate = FleetBoard.works(groups: [], agents: [replacement, orphan])
    #expect(separate.count == 2)
    #expect(separate.allSatisfy { $0.helpers.isEmpty })

    // A person's prompt that mentions a request and a pane is its own work.
    let human = TendHostedAgent(host: host, agent: try agent([
        "state": "working", "last_prompt": "Investigate req_deadbeef01_1 failure on pane %1; do not claim peer requests",
    ], id: "human"), pane: try pane("%4", alias: "x"))
    #expect(FleetBoard.helperRequest(human.agent) == nil)
    #expect(FleetBoard.works(groups: [], agents: [replacement, human]).count == 2)
}

@Test func aLongHelperChainFoldsOnceToItsRoot() throws {
    var agents = [TendHostedAgent(host: host, agent: try agent(["state": "idle"], id: "h0"), pane: try pane("%0", alias: "a"))]
    for index in 1...300 {
        agents.append(TendHostedAgent(
            host: host,
            agent: try agent(["state": "idle", "last_prompt": claimed(from: "h\(index - 1)")], id: "h\(index)"),
            pane: try pane("%\(index)", alias: "a")
        ))
    }
    let works = FleetBoard.works(groups: [], agents: agents)
    #expect(works.count == 1)
    #expect(works.first?.helpers.count == 300)
}

@Test func aSessionNeverGivenATaskIsEmpty() throws {
    let blank = TendHostedAgent(host: host, agent: try agent(["state": "idle"], id: "blank"), pane: nil)
    let used = TendHostedAgent(host: host, agent: try agent(["state": "idle", "last_prompt": "Fix the README"], id: "used"), pane: nil)
    let works = FleetBoard.works(groups: [], agents: [blank, used])
    #expect(works.first { $0.agents.first?.id == blank.id }?.isEmptySession == true)
    #expect(works.first { $0.agents.first?.id == used.id }?.isEmptySession == false)
    #expect(FleetBoard.isEmpty(try agent(["recap": "  "], id: "x")))
    #expect(!FleetBoard.isEmpty(try agent(["last_response": "Done."], id: "y")))
}

@Test func theReviewQueueHoldsUnreviewedResultsThePersonAskedFor() throws {
    let now = try #require(TendUsageTimestamp.parse("2026-11-09T12:00:00Z"))
    func finished(_ id: String, prompt: String?, at: String) throws -> TendHostedAgent {
        var fields: [String: Any] = ["state": "idle", "state_entered_at": at, "last_response": "Done."]
        if let prompt { fields["last_prompt"] = prompt }
        return TendHostedAgent(host: host, agent: try agent(fields, id: id), pane: nil)
    }
    let mine = try finished("mine", prompt: "Fix the README", at: "2026-11-09T11:00:00Z")
    let old = try finished("old", prompt: "Write the docs", at: "2026-11-02T11:00:00Z")
    let notice = try finished("notice", prompt: "[tend:req_1_2] Completed reply from codex@%13 is ready.", at: "2026-11-09T11:30:00Z")
    let background = try finished("bg", prompt: "<task-notification>\n<summary>x</summary>", at: "2026-11-09T11:40:00Z")
    let works = FleetBoard.works(groups: [], agents: [mine, old, notice, background], now: now)

    let day = FleetBoard.reviewQueue(works, marks: FleetReviewMarks(), scope: .day, now: now)
    #expect(day.map { $0.focus?.id } == [mine.id])
    let all = FleetBoard.reviewQueue(works, marks: FleetReviewMarks(), scope: .all, now: now)
    #expect(all.map { $0.focus?.id } == [mine.id, old.id], "newest first")

    var marks = FleetReviewMarks()
    marks.markReviewed(try #require(day.first))
    #expect(FleetBoard.reviewQueue(works, marks: marks, scope: .all, now: now).map { $0.focus?.id } == [old.id])
}

@Test func theDaemonsPromptOriginDecidesWhoAskedBeforeAnyTextGuess() throws {
    let now = try #require(TendUsageTimestamp.parse("2026-11-09T12:00:00Z"))
    func finished(_ id: String, prompt: String, origin: String?) throws -> TendHostedAgent {
        var fields: [String: Any] = ["state": "idle", "state_entered_at": "2026-11-09T11:00:00Z",
                                     "last_response": "Done.", "last_prompt": prompt]
        if let origin { fields["last_prompt_origin"] = origin }
        return TendHostedAgent(host: host, agent: try agent(fields, id: id), pane: nil)
    }
    // A person's reply through Home is theirs even though tend typed it;
    // a peer's plain-text hand-off is not, whatever its wording.
    let viaHome = try finished("home", prompt: "Ship it", origin: "person_via_tend")
    let peer = try finished("peer", prompt: "Please review the diff", origin: "peer")
    let automated = try finished("auto", prompt: "continue", origin: "automation")
    // Unknown origin falls back to the provider-envelope heuristic.
    let typed = try finished("typed", prompt: "Fix the README", origin: "unknown")
    let notice = try finished("notice", prompt: "<task-notification>\n<summary>x</summary>", origin: "unknown")
    let works = FleetBoard.works(groups: [], agents: [viaHome, peer, automated, typed, notice], now: now)
    let asked = Set(FleetBoard.reviewQueue(works, marks: FleetReviewMarks(), scope: .all, now: now).compactMap { $0.focus?.id })
    #expect(asked == [viaHome.id, typed.id])
}

@Test func reviewScopesStartWhereTheirNamesSay() throws {
    let now = try #require(TendUsageTimestamp.parse("2026-11-09T12:00:00Z"))
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = try #require(TimeZone(identifier: "UTC"))
    #expect(FleetReviewScope.today.start(now: now, calendar: calendar) == TendUsageTimestamp.parse("2026-11-09T00:00:00Z"))
    #expect(FleetReviewScope.day.start(now: now) == TendUsageTimestamp.parse("2026-11-08T12:00:00Z"))
    #expect(FleetReviewScope.week.start(now: now) == TendUsageTimestamp.parse("2026-11-02T12:00:00Z"))
    #expect(FleetReviewScope.all.start(now: now) == nil)
}

@Test func menuBarGlyphsNameProvidersAndCustomHostIcons() throws {
    #expect(TendMenuBarAgentAppearance(identity: "a", kind: "claude_code").symbol == TendGlyph.claude)
    #expect(TendMenuBarAgentAppearance(identity: "a", kind: "codex").symbol == TendGlyph.codex)
    #expect(TendGlyph.kind(TendGlyph.claude) == .logo(.claude))
    #expect(TendGlyph.kind(TendGlyph.codex) == .logo(.openai))
    #expect(TendGlyph.kind("text:🚀") == .text("🚀"))
    #expect(TendGlyph.kind("server.rack") == .symbol("server.rack"))
    #expect(TendGlyph.cleanedText("  mini host ") == "mi")

    let defaults = try #require(UserDefaults(suiteName: "host-icons-\(UUID().uuidString)"))
    #expect(TendHostIcons.icon(for: "mini", local: false, defaults: defaults) == "server.rack")
    #expect(TendHostIcons.icon(for: "me", local: true, defaults: defaults) == "laptopcomputer")
    TendHostIcons.set("text:🚀", for: "mini", defaults: defaults)
    #expect(TendHostIcons.icon(for: "mini", local: false, defaults: defaults) == "text:🚀")
    TendHostIcons.set(nil, for: "mini", defaults: defaults)
    #expect(TendHostIcons.icon(for: "mini", local: false, defaults: defaults) == "server.rack")
}

@Test func reviewKeysAreOnTheHomeRowWithArrowsAsWell() {
    #expect(FleetReviewKey.from(keyCode: 0, characters: "E") == .looksGood)
    #expect(FleetReviewKey.from(keyCode: 0, characters: "l") == .looksGood)
    #expect(FleetReviewKey.from(keyCode: 0, characters: "h") == .later)
    #expect(FleetReviewKey.from(keyCode: 0, characters: "j") == .next)
    #expect(FleetReviewKey.from(keyCode: 0, characters: "k") == .previous)
    #expect(FleetReviewKey.from(keyCode: 0, characters: "r") == .sendBack)
    #expect(FleetReviewKey.from(keyCode: 0, characters: "o") == .open)
    #expect(FleetReviewKey.from(keyCode: 36, characters: "\r") == .open)
    #expect(FleetReviewKey.from(keyCode: 124, characters: nil) == .looksGood)
    #expect(FleetReviewKey.from(keyCode: 123, characters: nil) == .later)
    #expect(FleetReviewKey.from(keyCode: 0, characters: "x") == nil)
}

@Test func theAskProviderTailorsOneReplyPerAgent() throws {
    let a = FleetBoard.work(for: TendHostedAgent(host: host, agent: try agent(["state": "idle", "last_prompt": "Make the intro shorter", "last_response": "Cut it to 20s."], id: "a"), pane: nil), now: .now)
    let b = FleetBoard.work(for: TendHostedAgent(host: host, agent: try agent(["state": "idle", "last_prompt": "Pick a thumbnail", "last_response": "Option 2 is brightest."], id: "b"), pane: nil), now: .now)
    let prompt = FleetReviewAssist.tailorPrompt(reply: "좋아요, 둘 다 진행해 주세요", targets: [(id: "a", work: a), (id: "b", work: b)])
    #expect(prompt.contains("id: a") && prompt.contains("id: b"))
    #expect(prompt.contains("좋아요, 둘 다 진행해 주세요"))
    #expect(prompt.contains("Cut it to 20s."))
    #expect(FleetReviewAssist.summaryPrompt([a, b]).contains("Option 2 is brightest."))

    let answer = """
    Here you go:
    {"messages":[{"id":"a","message":" 20초 버전으로 진행해 주세요. "},{"id":"b","message":""},{"id":"zzz","message":"x"}]}
    """
    let parsed = try #require(FleetReviewAssist.parseTailored(answer, ids: ["a", "b"]))
    #expect(parsed == ["a": "20초 버전으로 진행해 주세요.", "b": ""])
    #expect(FleetReviewAssist.parseTailored("no json here", ids: ["a"]) == nil)
}

@Test func digestIsTheDefaultReviewMode() {
    #expect(FleetReviewMode.allCases.first == .digest)
}

@Test func aChoiceDialogIsReadOffTheScreen() throws {
    // The Main Claude screen from jiun-mbp, with a code panel on the right.
    let screen = """
      Ran 1 shell command                                                 │ 13      var resetPosition: () -> Void = {}
    ──────────────────────────────────────────────────────────────────────│ 14      var heightChanged: (CGFloat) -> Void = { _ in
     ☐ MBP AFM                                                            │     }
                                                                          │ 15 +    var resting: Binding<Bool>? = nil
    │ MBP에서 Apple Intelligence(AFM)가 modelNotReady입니다. Mac 언어가   │ 16      @FocusState private var inputFocused: Bool
    │ English(South Korea)인데 Siri는 이미 한국어라서 둘이 달라 다운로드가│ 17 +    @FocusState private var searchFocused: Bool
    │ 막혀 있습니다. 어떻게 맞출까요?                                     │ 18      @Environment(\\.colorScheme) private var colorS
                                                                          │ 19 +    @AppStorage("mac.compactConversationShowsExcha
    ❯ 1. Siri를 영어로 (Recommended)                                      │    +nge") private var showsExchange = true
         Siri 언어만 English(US 등)로 바꿉니다. Mac UI는 그대로입니다.    │ 20 +    @AppStorage(DesktopConversationTextSize.prefer
      2. Mac 언어를 한국어로                                              │    +enceKey) private var textSize: DesktopConversation
         MBP 전체 UI가 한국어로 바뀝니다.                                 │ 21 +    @State private var readingPosition = ScrollPos
      3. 그대로 두기                                                      │    +ition(edge: .bottom)
         MBP AFM은 쓰지 않고, AFM 시험은 Mini에서만 합니다.               │ 22 +    @State private var followsLatest = true
      4. Type something.                                                  │ 23 +    @State private var userScrolling = false
    """
    let dialog = try #require(FleetChoiceDialog.parse(screen))
    #expect(dialog.question.hasPrefix("☐ MBP AFM\nMBP에서 Apple Intelligence"))
    #expect(dialog.question.hasSuffix("어떻게 맞출까요?"))
    #expect(dialog.options.map(\.number) == [1, 2, 3, 4])
    #expect(dialog.options[0].label == "Siri를 영어로 (Recommended)")
    #expect(dialog.options[0].detail?.hasPrefix("Siri 언어만") == true)
    #expect(dialog.options[3].takesText)
    #expect(!dialog.options[0].takesText)

    #expect(FleetChoiceDialog.parse("just some output\n❯ ") == nil)
    #expect(FleetChoiceDialog.parse("Pick one\n  2. B\n  3. C") == nil, "numbering must start at 1")
}

@Test func aGenericNoticeSaysWhatKindOfAnswerIsWanted() throws {
    let choosing = try agent(["state": "waiting_choice", "last_notification": "Claude approval required: Claude needs your permission"], id: "a")
    #expect(FleetBoard.question(for: choosing, now: .now) == "Asks you to choose. Open to see the question.")
    let typing = try agent(["state": "waiting_input", "last_notification": "Claude is waiting for your input"], id: "b")
    #expect(FleetBoard.question(for: typing, now: .now) == "Waits for your answer. Open to see the question.")
    let specific = try agent(["state": "waiting_input", "last_notification": "Where should the sandbox live?"], id: "c")
    #expect(FleetBoard.question(for: specific, now: .now) == "Where should the sandbox live?")
}

@Test func aChoiceWithAnOptionBelowARuleIsStillRead() throws {
    let screen = """
    ────────────────────────────────────────────────────────────────────────
     ☐ 점심 메뉴

    데모: 오늘 점심 메뉴를 고르세요

    ❯ 1. 김치찌개
         얼큰한 한식
      2. 파스타
         양식 면 요리
      3. 샐러드
         가벼운 한 끼
      4. Type something.
    ────────────────────────────────────────────────────────────────────────
      5. Chat about this

    Enter to select · ↑/↓ to navigate · Esc to cancel
    """
    let dialog = try #require(FleetChoiceDialog.parse(screen))
    #expect(dialog.question == "☐ 점심 메뉴\n데모: 오늘 점심 메뉴를 고르세요")
    #expect(dialog.options.map(\.label) == ["김치찌개", "파스타", "샐러드", "Type something.", "Chat about this"])
    #expect(dialog.options[1].detail == "양식 면 요리")
    #expect(dialog.options[3].detail == nil)
    #expect(dialog.selected == 1)
    #expect(dialog.moves(to: 3) == ["down", "down"])
    #expect(dialog.moves(to: 1) == [])
    #expect(dialog.moves(to: 9) == nil)
    let fromFour = FleetChoiceDialog(question: dialog.question, options: dialog.options, selected: 4)
    #expect(fromFour.moves(to: 1) == ["up", "up", "up"])
}

@Test func cardsThatChangeLaneRememberWhereTheyCameFrom() throws {
    let impl = try member("impl", "idle", alias: "impl", pane: "%1")
    let fresh = TendHostedAgent(host: host, agent: try agent(["state": "working"], id: "new"), pane: nil)
    let works = FleetBoard.works(groups: [], agents: [impl, fresh])
    let finished = try #require(works.first { $0.lane == .finished })

    let moves = FleetBoard.laneMoves(previous: [finished.id: .running], current: works)

    #expect(moves == [finished.id: .running])
    #expect(FleetBoard.laneMoves(previous: [finished.id: .finished], current: works).isEmpty)
}

@Test func everyAgentWaitingInOneUnitIsListed() throws {
    let impl = try member("impl", "waiting_choice", alias: "impl", pane: "%1")
    let review = try member("review", "waiting_input", alias: "review", pane: "%2")
    let work = FleetBoard.work(for: try group([:], [impl, review]), now: .now)

    #expect(work.lane == .needsYou)
    let waiting = FleetBoard.waiting(work)
    #expect(Set(waiting.map(\.agent.agentSessionID)) == ["impl", "review"])
    #expect(waiting.first?.id == work.focus?.id)
}

@Test func twoCursorsLeaveTheSelectionUnknown() throws {
    let screen = "Choose a permission\n❯ 1. Yes, once\n❯ 2. Always allow\n  3. No\nEnter to select"
    let dialog = try #require(FleetChoiceDialog.parse(screen))
    #expect(dialog.selected == nil)
    #expect(dialog.moves(to: 1) == nil)
}

@Test func aChangedDescriptionIsADifferentChoice() throws {
    let shown = try #require(FleetChoiceDialog.parse("Choose action\n❯ 1. Continue\n     Run npm test\n  2. Cancel"))
    let now = try #require(FleetChoiceDialog.parse("Choose action\n❯ 1. Continue\n     Delete production data\n  2. Cancel"))
    #expect(shown.question == now.question)
    #expect(shown.options.map(\.label) == now.options.map(\.label))
    #expect(shown.options != now.options)
}

@MainActor
@Test func aSecondChoiceForTheSameAgentWaitsForTheFirst() {
    let sends = FleetChoiceSends()
    #expect(sends.begin("a"))
    #expect(!sends.begin("a"))
    #expect(sends.begin("b"))
    sends.finish("a", .init(succeeded: true, message: "Chose 1"))
    #expect(!sends.isPending("a"))
    #expect(sends.outcome("a")?.succeeded == true)
    #expect(sends.begin("a"))
    #expect(sends.outcome("a") == nil)
}

@Test func reviewRequestsDecodeAndHomeKeysAreStablePerResult() throws {
    let json = """
    [{"id":"rv_1","target":{"kind":"work","workspace":"tend","work":"storage","alias":"impl"},
      "generation":2,"summary":"Check the migration","anchor":"commit:1a2b3c",
      "requested_at":"2026-11-11T01:00:00Z","requested_by":{"kind":"agent","label":"impl"}},
     {"id":"rv_2","target":{"kind":"session","provider":"codex","session_id":"0123456789abcdef"},
      "generation":1,"summary":"Look at this","requested_at":"2026-11-11T02:00:00Z",
      "requested_by":{"kind":"user"}}]
    """
    let items = try JSONDecoder().decode([TendReviewItem].self, from: Data(json.utf8))
    #expect(items[0].target.text == "tend/storage · impl")
    #expect(items[0].askedBy.contains("impl"))
    #expect(items[1].target.text == "codex 01234567")

    let finished = TendHostedAgent(host: host, agent: try agent(["state": "idle", "last_prompt": "Fix it"], id: "done"), pane: nil)
    let work = try #require(FleetBoard.works(groups: [], agents: [finished]).first)
    let key = FleetBoard.reviewKey(work)
    #expect(key.hasPrefix("home:") && key.count == 69, "fits the store's 128-byte key limit")
    #expect(key == FleetBoard.reviewKey(work), "same result, same key")
    let target = try #require(FleetBoard.reviewTarget(work))
    #expect(target.json["kind"] as? String == "session")

    // The same session on another host alias, or the same unit in another
    // state, is a different result and gets a different key.
    let other = TendFleetHostIdentity(alias: "other", local: false, state: "online", mode: "control")
    let elsewhere = TendHostedAgent(host: other, agent: try agent(["state": "idle", "last_prompt": "Fix it"], id: "done"), pane: nil)
    let moved = try #require(FleetBoard.works(groups: [], agents: [elsewhere]).first)
    #expect(FleetBoard.reviewKey(moved) != key)
    let later = TendHostedAgent(host: host, agent: try agent(["state": "idle", "last_prompt": "Fix it", "state_entered_at": "2026-11-11T09:00:00Z"], id: "done"), pane: nil)
    let changed = try #require(FleetBoard.works(groups: [], agents: [later]).first)
    #expect(changed.id == work.id)
    #expect(FleetBoard.reviewKey(changed) != key)
}

@Test func recordedCommandsDecodeWithTheirHonestyFlags() throws {
    let json = """
    {"session_id":"s1","more":true,"complete":false,
     "pairs":[{"tool":"Bash","call":"cargo test","call_at":"2026-11-11T01:00:00Z",
               "result":"test result: ok","result_at":"2026-11-11T01:00:05Z","is_error":false,
               "clipped":false,"call_event":"e1","result_event":"e2"},
              {"tool":"Bash","call":"git push","call_at":null,"result":null,"result_at":null,
               "is_error":null,"clipped":true,"call_event":"e3","result_event":null}]}
    """
    let evidence = try JSONDecoder().decode(TendTurnEvidence.self, from: Data(json.utf8))
    #expect(evidence.more && !evidence.complete)
    #expect(evidence.pairs[0].result == "test result: ok")
    #expect(evidence.pairs[1].result == nil && evidence.pairs[1].clipped)
    // The full record is reachable by its result's event, else the call's.
    #expect(evidence.pairs[0].readCommand == "tend agent read e2")
    #expect(evidence.pairs[1].readCommand == "tend agent read e3")

    // A long result shows its last lines, where the verdict is.
    let long = try JSONDecoder().decode(TendTurnEvidence.Pair.self, from: Data("""
    {"tool":"Bash","call":"cargo test","result":"a\\nb\\nc\\nd\\ne\\nFAILED","is_error":null,
     "clipped":false,"call_event":"e9","result_event":null}
    """.utf8))
    let tail = try #require(long.resultTail(lines: 4))
    #expect(tail.cut && tail.text.hasSuffix("FAILED") && !tail.text.contains("a"))
}
@Test func multipleChoiceChecksAreDraftedAndSentAsOnePlan() throws {
    let screen = """
    ☐ K8s 활성 서비스 및 Argo CD 앱 점검

    확인할 대상을 모두 선택하세요.

    ❯ 1. [✓] 활성 서비스
         현재 서비스 상태
      2. [ ] Argo CD 앱
         동기화 상태
      3. [ ] Ingress
      4. Type something.

    Space to select · Enter to submit · ↑/↓ to navigate
    """
    let dialog = try #require(FleetChoiceDialog.parse(screen))
    #expect(dialog.isMultipleSelection)
    #expect(dialog.checkedNumbers == [1])
    #expect(dialog.options[0].label == "활성 서비스")
    #expect(dialog.options[1].label == "Argo CD 앱")
    #expect(dialog.keys(selecting: [2, 3]) == ["space", "down", "space", "down", "space", "enter"])
    // No network or key events are needed until this entire plan is sent.
    #expect(dialog.keys(selecting: [1]) == ["enter"])
    #expect(dialog.keys(selecting: []) == nil)
    #expect(dialog.keys(selecting: [4]) == nil)
    let fresh = try #require(FleetChoiceDialog.parse(screen.replacingOccurrences(of: "3. [ ]", with: "3. [x]")))
    #expect(dialog.isSameQuestion(as: fresh))
    #expect(fresh.keys(selecting: [2, 3]) == ["space", "down", "space", "enter"])
    let changed = try #require(FleetChoiceDialog.parse(screen.replacingOccurrences(of: "Argo CD 앱", with: "다른 항목")))
    #expect(!dialog.isSameQuestion(as: changed))
    let noCursor = try #require(FleetChoiceDialog.parse(screen.replacingOccurrences(of: "❯", with: " ")))
    #expect(noCursor.keys(selecting: [1, 2]) == nil)
}

@Test func unicodeCheckboxesDoNotBecomePartOfTheirLabels() throws {
    let dialog = try #require(FleetChoiceDialog.parse("""
    점검할 항목

    ❯ 1. ☒ 서비스
      2. ☐ 앱
      3. ☑ 네트워크
    """))
    #expect(dialog.isMultipleSelection)
    #expect(dialog.checkedNumbers == [1, 3])
    #expect(dialog.options.map(\.label) == ["서비스", "앱", "네트워크"])
    #expect(dialog.keys(selecting: [1, 3]) == ["enter"])
}

@Test func anIdleAgentThatAskedIsYourTurnWithItsQuestion() throws {
    let asked = try agent([
        "state": "idle",
        "last_notification": "Claude is waiting for your input",
        "awaiting_reply": ["question": "머지해도 될까요?", "since": "2026-11-11T09:00:00Z"],
    ], id: "a")
    let status = asked.status
    #expect(status == .asked("머지해도 될까요?"))
    #expect(status.needsAttention)
    #expect(status.priority > TendAgentStatus.needsInput("waiting_input").priority)
    #expect(FleetBoard.question(for: asked, now: .now) == "머지해도 될까요?")

    // The marker means nothing once the row moves on, and an older daemon
    // sends none: both stay what their state says.
    let working = try agent(["awaiting_reply": ["question": "머지해도 될까요?"]], id: "b")
    #expect(working.status == .working)
    #expect(try agent(["state": "idle"], id: "c").status == .idle)
}

@Test func eachNewQuestionNotifiesOnce() throws {
    let first = try agent(["state": "idle", "awaiting_reply": ["question": "A로 갈까요?"]], id: "a")
    let second = try agent(["state": "idle", "awaiting_reply": ["question": "B로 갈까요?"]], id: "a")
    let idle = try agent(["state": "idle"], id: "a")
    let firstKey = TendNotificationRules.key(first)
    #expect(TendNotificationRules.event(from: "working", to: firstKey) == .attention("asked"))
    #expect(TendNotificationRules.event(from: TendNotificationRules.key(idle), to: firstKey) == .attention("asked"))
    #expect(TendNotificationRules.event(from: firstKey, to: firstKey) == nil)
    #expect(TendNotificationRules.event(from: firstKey, to: TendNotificationRules.key(second)) == .attention("asked"))
    #expect(TendNotificationActions.category(for: .attention("asked")) == TendNotificationActions.inputCategory)
}

@Test func anAskedCardShowsTheResponseBehindItsQuestion() throws {
    let response = "> Is everything done?\n>\n> **Useful expressions:** …\n\n---\n\n"
        + "**고치려면 (제안, 아직 안 함)**\n- IaC PR로 레이블을 `jiun-mbp`로 바꾸고 배포합니다.\n\n"
        + "레이블 정리 PR을 만들어서 배포까지 진행할까요?"
    let asked = try agent([
        "state": "idle", "last_response": response,
        "awaiting_reply": ["question": "레이블 정리 PR을 만들어서 배포까지 진행할까요?", "since": "2026-11-12T02:18:03Z"],
    ], id: "a")
    let context = try #require(FleetAskedContext.context(for: asked))
    // The restated prompt above the answer is dropped; the proposal stays.
    #expect(context.hasPrefix("**고치려면"))
    #expect(context.hasSuffix("진행할까요?"))

    // Nothing beyond the question itself: no context block.
    let bare = try agent([
        "state": "idle", "last_response": "진행할까요?",
        "awaiting_reply": ["question": "진행할까요?"],
    ], id: "b")
    #expect(FleetAskedContext.context(for: bare) == nil)

    // Not asking: no context block.
    let idle = try agent(["state": "idle", "last_response": response], id: "c")
    #expect(FleetAskedContext.context(for: idle) == nil)
}
