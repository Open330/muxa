import Foundation
import Testing
@testable import Muxa

private func indexedPane(_ id: String, socket: String) -> [String: Any] {
    ["pane_id": id, "session_id": "$1", "session": "test", "window_id": "@1",
     "window_name": "test", "window_index": "0", "pane_index": id.dropFirst().description,
     "current_command": "zsh", "title": "", "current_path": "/tmp", "socket": socket]
}

private func indexedAgent(_ id: String, pane: String?, socket: String? = nil) -> [String: Any] {
    var result: [String: Any] = ["kind": "codex", "agent_session_id": id, "state": "working"]
    if let pane { result["pane"] = pane }
    if let socket { result["tmux_socket"] = socket }
    return result
}

private func indexedHost(_ alias: String, panes: [[String: Any]], agents: [[String: Any]]) throws -> MuxaFleetHost {
    try JSONDecoder().decode(MuxaFleetHost.self, from: JSONSerialization.data(withJSONObject: [
        "alias": alias, "local": alias == "local", "mode": "control", "state": "online",
        "remote": ["panes": panes, "agents": agents],
    ]))
}

@Test
func paneIndexKeepsSocketAmbiguityAndHostIsolation() throws {
    let first = try indexedHost("local", panes: [indexedPane("%1", socket: "a"), indexedPane("%1", socket: "b"), indexedPane("%2", socket: "a")], agents: [
        indexedAgent("exact", pane: "%1", socket: "b"),
        indexedAgent("ambiguous", pane: "%1"),
        indexedAgent("unique", pane: "%2"),
        indexedAgent("fallback", pane: "%2", socket: "unknown"),
        indexedAgent("missing", pane: "%999"),
        indexedAgent("paneless", pane: nil),
    ])
    let second = try indexedHost("remote", panes: [indexedPane("%1", socket: "other")], agents: [indexedAgent("remote-agent", pane: "%1")])
    let snapshot = MuxaExecutionSnapshot(hosts: [first, second])
    let byID = Dictionary(uniqueKeysWithValues: snapshot.hostedAgents.map { ($0.agent.agentSessionID, $0) })
    #expect(byID["exact"]?.pane?.socket == "b")
    #expect(byID["ambiguous"]?.pane == nil)
    #expect(byID["unique"]?.pane?.paneID == "%2")
    #expect(byID["fallback"]?.pane?.paneID == "%2")
    #expect(byID["missing"]?.pane == nil)
    #expect(byID["paneless"]?.pane == nil)
    #expect(byID["remote-agent"]?.pane?.socket == "other")
}

@Test
func largeFleetPaneIndexMatchesReferenceScan() throws {
    let count = 3000
    let host = try indexedHost("local", panes: (0..<count).map { indexedPane("%\($0)", socket: "default") }, agents: (0..<count).map { indexedAgent("agent-\($0)", pane: "%\($0)", socket: "default") })
    let panes = try #require(host.remote?.panes)
    let agents = try #require(host.remote?.agents)
    let beganReference = ContinuousClock.now
    let reference = agents.map { agent in
        panes.first { $0.paneID == agent.pane && $0.socket == agent.tmuxSocket }
    }
    let referenceDuration = beganReference.duration(to: .now)
    let beganIndex = ContinuousClock.now
    let snapshot = MuxaExecutionSnapshot(hosts: [host])
    let indexDuration = beganIndex.duration(to: .now)
    #expect(snapshot.hostedAgents.map(\.pane) == reference)
    #expect(snapshot.watchHosts.first?.sessions.first?.windows.first?.panes.count == count)
    print("3,000 agents/panes: reference matching \(referenceDuration); complete indexed snapshot \(indexDuration)")
}
