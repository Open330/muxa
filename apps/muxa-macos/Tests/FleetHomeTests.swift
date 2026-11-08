import Foundation
import Testing
@testable import Muxa

private let host = MuxaFleetHostIdentity(alias: "mini", local: false, state: "online", mode: "control")

private func agent(_ fields: [String: Any], id: String, kind: String = "claude_code") throws -> MuxaAgent {
    var object: [String: Any] = ["kind": kind, "agent_session_id": id, "state": "working"]
    object.merge(fields) { _, new in new }
    return try JSONDecoder().decode(MuxaAgent.self, from: JSONSerialization.data(withJSONObject: object))
}

private func pane(_ id: String, alias: String) throws -> MuxaPaneInfo {
    let object: [String: Any] = [
        "pane_id": id, "session_id": "$1", "session": "work", "window_id": "@1", "window_name": "w",
        "window_index": "0", "pane_index": "0", "current_command": "claude", "title": "",
        "current_path": "/Users/me/src/muxa", "workspace_id": "ws", "work_id": "fleet-home",
        "agent_alias": alias,
    ]
    return try JSONDecoder().decode(MuxaPaneInfo.self, from: JSONSerialization.data(withJSONObject: object))
}

private func run(aliases: [String: String]) throws -> MuxaPipelineRun {
    let object: [String: Any] = [
        "identity": ["workspace_id": "ws", "work_id": "fleet-home"],
        "pipeline": "implement-review",
        "desired": [
            ["alias": "impl", "program": "claude", "role": "Implement"],
            ["alias": "review", "program": "codex", "role": "Review", "after": ["impl"]],
        ],
        "cwd": "/Users/me/src/muxa",
        "generation": 1,
        "aliases": aliases.mapValues { ["alias": "x", "status": $0, "generation": 1] },
    ]
    return try JSONDecoder().decode(MuxaPipelineRun.self, from: JSONSerialization.data(withJSONObject: object))
}

@Test func soloAgentsBecomeOneStageWorkInTheirLane() throws {
    let waiting = MuxaHostedAgent(host: host, agent: try agent([
        "state": "waiting_input", "ai_title": "Sandbox location", "last_notification": "Where should the sandbox live?",
        "state_entered_at": "2026-11-08T02:00:00Z",
    ], id: "a"), pane: nil)
    let working = MuxaHostedAgent(host: host, agent: try agent(["recap": "Writing tests\nmore"], id: "b"), pane: nil)
    let idle = MuxaHostedAgent(host: host, agent: try agent(["state": "idle", "last_prompt": "Fix the README"], id: "c"), pane: nil)
    let stopped = MuxaHostedAgent(host: host, agent: try agent(["state": "stopped"], id: "d"), pane: nil)

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
    let impl = MuxaHostedAgent(host: host, agent: try agent(["state": "waiting_choice"], id: "impl"), pane: try pane("%1", alias: "impl"))
    let group = MuxaWorkGroup(
        identity: MuxaWorkIdentity(workspaceID: "ws", workID: "fleet-home"),
        pipelineRun: try run(aliases: [:]),
        participants: [impl]
    )

    let works = FleetBoard.works(groups: [group], agents: [impl])

    #expect(works.count == 1)
    let work = try #require(works.first)
    #expect(work.lane == .needsYou)
    #expect(work.repo == "muxa")
    #expect(work.stages.map(\.name) == ["Implement", "Review"])
    #expect(work.stages.map(\.state) == [.needsYou, .waiting])
    #expect(work.stages[0].owner == "claude · mini")
    #expect(work.stages[1].owner == "codex")
    #expect(work.currentStageIndex == 0)
    #expect(work.focus?.id == impl.id)
}

@Test func pipelineStatusDecidesStagesWithoutAgents() throws {
    let group = MuxaWorkGroup(
        identity: MuxaWorkIdentity(workspaceID: "ws", workID: "fleet-home"),
        pipelineRun: try run(aliases: ["impl": "done", "review": "done"]),
        participants: []
    )
    let finished = FleetBoard.work(for: group, now: .now)
    #expect(finished.lane == .finished)

    let blocked = FleetBoard.work(
        for: MuxaWorkGroup(
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
    func hosted(_ id: String, _ state: String, _ at: String) throws -> MuxaHostedAgent {
        MuxaHostedAgent(host: host, agent: try agent(["state": state, "state_entered_at": at], id: id), pane: nil)
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
    #expect(FleetBoard.repoName("/Users/me/src/muxa/") == "muxa")
}

private func group(_ aliases: [String: String], _ participants: [MuxaHostedAgent]) throws -> MuxaWorkGroup {
    MuxaWorkGroup(
        identity: MuxaWorkIdentity(workspaceID: "ws", workID: "fleet-home"),
        pipelineRun: try run(aliases: aliases),
        participants: participants
    )
}

private func member(_ id: String, _ state: String, alias: String, pane paneID: String) throws -> MuxaHostedAgent {
    MuxaHostedAgent(host: host, agent: try agent(["state": state], id: id), pane: try pane(paneID, alias: alias))
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
    let covered = MuxaAgentStatus.limited(MuxaRateLimitCap(scope: .fiveHour, until: nil, covered: true))
    let uncovered = MuxaAgentStatus.limited(MuxaRateLimitCap(scope: .fiveHour, until: nil, covered: false))

    #expect(FleetBoard.stageState(aliasStatus: "failed", status: covered, agentState: "error") == .running)
    #expect(FleetBoard.stageState(aliasStatus: "failed", status: uncovered, agentState: "error") == .needsYou)
    #expect(FleetBoard.stageState(aliasStatus: "failed", status: .error, agentState: "error") == .needsYou)
    #expect(FleetBoard.stageState(aliasStatus: "blocked", status: .working, agentState: "working") == .needsYou)
    #expect(FleetBoard.stageState(aliasStatus: nil, status: covered, agentState: "error") == .running)
}

@Test func onlyAnAgentAtItsPromptTakesTypedText() throws {
    func solo(_ state: String, withPane: Bool = true) throws -> FleetWork {
        let hosted = MuxaHostedAgent(
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
        let hosted = MuxaHostedAgent(host: host, agent: try agent(["state": "idle", "state_entered_at": time], id: "a"), pane: nil)
        return FleetBoard.work(for: hosted, now: .now)
    }
    let first = try finished(at: "2026-11-08T01:00:00Z")
    var marks = FleetReviewMarks()
    marks.markReviewed(first)
    #expect(marks.hides(first))
    #expect(!marks.hides(try finished(at: "2026-11-08T02:00:00Z")))

    let working = FleetBoard.work(
        for: MuxaHostedAgent(host: host, agent: try agent(["state_entered_at": "2026-11-08T01:00:00Z"], id: "a"), pane: nil),
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
    let asking = MuxaHostedAgent(
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
    func idle(_ id: String, _ at: String, alias: String, pane paneID: String) throws -> MuxaHostedAgent {
        MuxaHostedAgent(host: host, agent: try agent(["state": "idle", "state_entered_at": at], id: id), pane: try pane(paneID, alias: alias))
    }
    func work(_ participants: [MuxaHostedAgent]) -> FleetWork {
        FleetBoard.work(for: MuxaWorkGroup(
            identity: MuxaWorkIdentity(workspaceID: "ws", workID: "w"),
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
    let rerun = try JSONDecoder().decode(MuxaPipelineRun.self, from: JSONSerialization.data(withJSONObject: [
        "identity": ["workspace_id": "ws", "work_id": "fleet-home"], "pipeline": "implement-review",
        "desired": [["alias": "impl", "program": "claude"]], "cwd": "/x", "generation": 2,
        "aliases": ["impl": ["alias": "impl", "status": "done", "generation": 2]],
    ] as [String: Any]))
    let secondRun = FleetBoard.work(for: MuxaWorkGroup(identity: MuxaWorkIdentity(workspaceID: "ws", workID: "fleet-home"), pipelineRun: rerun, participants: []), now: .now)
    #expect(secondRun.lane == .finished)
    #expect(!marks.hides(secondRun))
}

@Test func aBoardOfOnlyReviewedUnitsKeepsItsLanes() {
    #expect(FleetBoard.showsEmptyBoard(visible: 0, reviewed: 0))
    #expect(!FleetBoard.showsEmptyBoard(visible: 0, reviewed: 1))
    #expect(!FleetBoard.showsEmptyBoard(visible: 2, reviewed: 0))
}
