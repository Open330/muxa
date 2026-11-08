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
