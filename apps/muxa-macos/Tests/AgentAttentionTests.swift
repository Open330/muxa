import Foundation
import Testing
import UserNotifications
@testable import Muxa

@MainActor
private final class RecordingPoster: MuxaNotificationPosting {
    var posted: [MuxaAgentNotification] = []
    var removed: [String] = []
    var badges: [Int] = []

    func post(_ notification: MuxaAgentNotification) { posted.append(notification) }
    func removeDelivered(identifiers: [String]) { removed.append(contentsOf: identifiers) }
    func setDockBadge(_ count: Int) { badges.append(count) }
}

private struct AgentFixture {
    var pane = "%1"
    var session = "s-1"
    var state = "working"
    var enteredAt = "2026-09-23T10:00:00Z"
    var promptAt: String? = "2026-09-23T09:59:00Z"
    var alias: String? = "coder"
}

private let paneA = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%1")
private let paneB = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%2")

private func snapshot(
    _ agents: [AgentFixture],
    host: String = "local",
    local: Bool = true,
    state: String = "online"
) throws -> MuxaExecutionSnapshot {
    let panes: [[String: Any]] = agents.map { agent in
        var pane: [String: Any] = [
            "pane_id": agent.pane, "title": "t", "session_id": "$1", "session": "demo",
            "window_id": "@1", "window_name": "work", "window_index": "0",
            "pane_index": agent.pane.dropFirst().description,
            "current_command": "claude", "current_path": "/tmp", "socket": "default",
        ]
        if let alias = agent.alias { pane["agent_alias"] = alias }
        return pane
    }
    let wireAgents: [[String: Any]] = agents.map { agent in
        var wire: [String: Any] = [
            "kind": "claude_code", "agent_session_id": agent.session, "pane": agent.pane,
            "state": agent.state, "state_entered_at": agent.enteredAt,
            "last_response": "All   tests\npass.",
        ]
        if let prompt = agent.promptAt { wire["last_prompt_at"] = prompt }
        return wire
    }
    let host = try JSONDecoder().decode(MuxaFleetHost.self, from: JSONSerialization.data(withJSONObject: [
        "alias": host, "local": local, "mode": "control", "state": state,
        "remote": ["agents": wireAgents, "panes": panes],
    ]))
    return MuxaExecutionSnapshot(hosts: [host])
}

@MainActor
private final class Harness {
    let defaults: UserDefaults
    let poster = RecordingPoster()
    var clock = Date(timeIntervalSince1970: 1_800_000_000)
    var settings = MuxaNotificationSettings()
    private(set) var center: MuxaAgentAttentionCenter!

    init(defaults: UserDefaults? = nil) {
        self.defaults = defaults ?? UserDefaults(suiteName: "muxa.attention.tests.\(UUID().uuidString)")!
        center = makeCenter()
    }

    func makeCenter() -> MuxaAgentAttentionCenter {
        MuxaAgentAttentionCenter(
            defaults: defaults, poster: poster,
            settings: { [unowned self] in self.settings }, now: { [unowned self] in self.clock }
        )
    }

    func relaunch() { center = makeCenter() }

    func ingest(_ agents: [AgentFixture]) throws {
        try center.ingest(snapshot(agents))
    }

    func advance(_ seconds: TimeInterval) { clock += seconds }
}

@Test @MainActor func firstSightIsReadAndAFinishedTurnBecomesUnread() throws {
    let h = Harness()
    try h.ingest([AgentFixture(state: "idle")])
    #expect(!h.center.isUnread(paneA))
    try h.ingest([AgentFixture(state: "working", enteredAt: "2026-09-23T10:01:00Z")])
    #expect(!h.center.isUnread(paneA))
    try h.ingest([AgentFixture(state: "idle", enteredAt: "2026-09-23T10:05:00Z")])
    #expect(h.center.isUnread(paneA))
    #expect(h.center.dockCount == 1)
    #expect(h.poster.badges.last == 1)
}

@Test @MainActor func anAgentThatNeverRanATurnIsNotFinished() throws {
    let h = Harness()
    try h.ingest([AgentFixture(state: "starting", promptAt: nil)])
    try h.ingest([AgentFixture(state: "idle", enteredAt: "2026-09-23T10:01:00Z", promptAt: nil)])
    #expect(!h.center.isUnread(paneA))
}

@Test @MainActor func aVisiblePaneStaysReadOnlyWhileMuxaIsFrontmost() throws {
    let h = Harness()
    try h.ingest([AgentFixture(), AgentFixture(pane: "%2", session: "s-2")])
    h.center.setVisible(panes: [paneA, paneB], appActive: true)
    try h.ingest([
        AgentFixture(state: "idle", enteredAt: "2026-09-23T10:05:00Z"),
        AgentFixture(pane: "%2", session: "s-2"),
    ])
    #expect(!h.center.isUnread(paneA))
    #expect(h.poster.posted.isEmpty)

    h.center.setVisible(panes: [paneA, paneB], appActive: false)
    try h.ingest([
        AgentFixture(state: "idle", enteredAt: "2026-09-23T10:05:00Z"),
        AgentFixture(pane: "%2", session: "s-2", state: "idle", enteredAt: "2026-09-23T10:06:00Z"),
    ])
    #expect(h.center.isUnread(paneB))
    #expect(h.poster.posted.map(\.pane) == [paneB])
}

@Test @MainActor func openingAPaneClearsItAndANewWaitMakesItUnreadAgain() throws {
    let h = Harness()
    try h.ingest([AgentFixture()])
    try h.ingest([AgentFixture(state: "waiting_input", enteredAt: "2026-09-23T10:02:00Z")])
    #expect(h.center.isUnread(paneA))
    h.center.setVisible(panes: [paneA], appActive: true)
    #expect(!h.center.isUnread(paneA))
    #expect(h.poster.removed.contains(MuxaAgentAttentionCenter.key(paneA)))
    // Still waiting, so the Dock keeps counting it although it is read.
    #expect(h.center.dockCount == 1)
    h.center.setVisible(panes: [], appActive: true)
    try h.ingest([AgentFixture(state: "working", enteredAt: "2026-09-23T10:03:00Z")])
    try h.ingest([AgentFixture(state: "waiting_choice", enteredAt: "2026-09-23T10:04:00Z")])
    #expect(h.center.isUnread(paneA))
}

@Test @MainActor func unreadComparesStampsNotClocks() throws {
    // A fleet host whose clock runs a day behind still turns unread.
    let h = Harness()
    try h.ingest([AgentFixture(enteredAt: "2026-09-22T10:00:00Z")])
    try h.ingest([AgentFixture(state: "idle", enteredAt: "2026-09-22T10:05:00Z")])
    #expect(h.center.isUnread(paneA))
}

@Test @MainActor func seenStampsSurviveARelaunchAndStaleOnesArePruned() throws {
    let h = Harness()
    try h.ingest([AgentFixture(), AgentFixture(pane: "%2", session: "s-2")])
    h.relaunch()
    // Finished while Muxa was closed: unread on the first snapshot, no banner.
    try h.ingest([AgentFixture(state: "idle", enteredAt: "2026-09-23T10:05:00Z")])
    #expect(h.center.isUnread(paneA))
    #expect(h.poster.posted.isEmpty)

    h.advance(MuxaAgentAttentionCenter.pruneAfter + 60)
    try h.ingest([AgentFixture(state: "idle", enteredAt: "2026-09-23T10:05:00Z")])
    h.relaunch()
    try h.ingest([
        AgentFixture(state: "idle", enteredAt: "2026-09-23T10:05:00Z"),
        AgentFixture(pane: "%2", session: "s-2", state: "idle", enteredAt: "2026-09-23T10:06:00Z"),
    ])
    #expect(h.center.isUnread(paneA))
    // %2 was pruned, so it is first sight again rather than unread.
    #expect(!h.center.isUnread(paneB))
}

@Test @MainActor func markAllReadClearsEveryPane() throws {
    let h = Harness()
    try h.ingest([AgentFixture(), AgentFixture(pane: "%2", session: "s-2")])
    try h.ingest([
        AgentFixture(state: "idle", enteredAt: "2026-09-23T10:05:00Z"),
        AgentFixture(pane: "%2", session: "s-2", state: "error", enteredAt: "2026-09-23T10:05:00Z"),
    ])
    #expect(h.center.unreadPanes == [paneA, paneB])
    #expect(h.center.dockCount == 2)
    h.center.markAllRead()
    #expect(h.center.unreadPanes.isEmpty)
    #expect(!h.center.hasUnread)
    #expect(h.center.dockCount == 1)
}

@Test func transitionRulesMatchMuxad() {
    #expect(MuxaNotificationRules.event(from: "working", to: "idle") == .finished)
    #expect(MuxaNotificationRules.event(from: "working", to: "stopped") == .finished)
    // A new agent passes starting → idle on its way up; that is not a turn.
    #expect(MuxaNotificationRules.event(from: "starting", to: "idle") == nil)
    #expect(MuxaNotificationRules.event(from: "starting", to: "stopped") == nil)
    #expect(MuxaNotificationRules.event(from: "idle", to: "stopped") == nil)
    #expect(MuxaNotificationRules.event(from: "working", to: "waiting_input") == .attention("waiting_input"))
    #expect(MuxaNotificationRules.event(from: "waiting_input", to: "error") == .attention("error"))
    #expect(MuxaNotificationRules.event(from: "waiting_input", to: "waiting_input") == nil)
    #expect(MuxaNotificationRules.event(from: "waiting_input", to: "working") == nil)
}

@Test func debounceDropsOnlyARepeatOfTheSameState() {
    let start = Date(timeIntervalSince1970: 0)
    #expect(MuxaNotificationRules.shouldFire(last: nil, state: "error", now: start))
    let last = (state: "waiting_input", at: start)
    #expect(!MuxaNotificationRules.shouldFire(last: last, state: "waiting_input", now: start + 29))
    #expect(MuxaNotificationRules.shouldFire(last: last, state: "waiting_input", now: start + 30))
    #expect(MuxaNotificationRules.shouldFire(last: last, state: "error", now: start + 1))
}

@Test @MainActor func notificationsFollowTransitionsSettingsAndDebounce() throws {
    let h = Harness()
    try h.ingest([AgentFixture()])
    #expect(h.poster.posted.isEmpty)
    try h.ingest([AgentFixture(state: "waiting_input", enteredAt: "2026-09-23T10:01:00Z")])
    let first = try #require(h.poster.posted.last)
    #expect(first.title.contains("@coder"))
    #expect(first.subtitle == "local · demo › work")
    #expect(first.pane == paneA)
    #expect(first.identifier == MuxaAgentAttentionCenter.key(paneA))
    #expect(first.sound)

    // Flapping back into the same state within 30 s is dropped.
    try h.ingest([AgentFixture(state: "working", enteredAt: "2026-09-23T10:01:10Z")])
    #expect(h.poster.posted.count == 1)
    h.advance(10)
    try h.ingest([AgentFixture(state: "waiting_input", enteredAt: "2026-09-23T10:01:20Z")])
    #expect(h.poster.posted.count == 1)

    h.settings.notifyAttention = false
    h.settings.sound = false
    try h.ingest([AgentFixture(state: "working", enteredAt: "2026-09-23T10:02:00Z")])
    h.advance(60)
    try h.ingest([AgentFixture(state: "error", enteredAt: "2026-09-23T10:03:00Z")])
    #expect(h.poster.posted.count == 1)
    try h.ingest([AgentFixture(state: "working", enteredAt: "2026-09-23T10:04:00Z")])
    try h.ingest([AgentFixture(state: "idle", enteredAt: "2026-09-23T10:05:00Z")])
    let finished = try #require(h.poster.posted.last)
    #expect(h.poster.posted.count == 2)
    #expect(finished.body == "All tests pass.")
    #expect(!finished.sound)

    h.settings.notifyFinished = false
    try h.ingest([AgentFixture(state: "working", enteredAt: "2026-09-23T10:06:00Z")])
    try h.ingest([AgentFixture(state: "idle", enteredAt: "2026-09-23T10:07:00Z")])
    #expect(h.poster.posted.count == 2)
}

@Test func notificationBodiesAreOneGlanceLong() {
    #expect(MuxaNotificationRules.body(nil) == nil)
    #expect(MuxaNotificationRules.body("  \n ") == nil)
    #expect(MuxaNotificationRules.body("a\n\n b\tc") == "a b c")
    let long = MuxaNotificationRules.body(String(repeating: "x", count: 400))
    #expect(long?.count == 160)
    #expect(long?.hasSuffix("…") == true)
}

@Test func unreadRulesIgnoreTurnsInProgress() throws {
    let agent = try #require(snapshot([AgentFixture(state: "working")]).agents.first)
    let seen = MuxaSeenRecord(agentSessionID: "other", stamp: "x", lastObservedAt: .now)
    #expect(!MuxaUnreadRules.isUnread(agent, seen: seen))
    #expect(!MuxaUnreadRules.isUnread(agent, seen: nil))
}

@Test func jumpToAgentRanksAttentionThenUnreadThenWorkingThenIdle() throws {
    let snap = try snapshot([
        AgentFixture(pane: "%1", session: "a", state: "idle", alias: "idle"),
        AgentFixture(pane: "%2", session: "b", state: "working", alias: "working"),
        AgentFixture(pane: "%3", session: "c", state: "idle", alias: "unread"),
        AgentFixture(pane: "%4", session: "d", state: "waiting_input", alias: "waiting"),
    ])
    let unread: Set = [MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%3")]
    let items = MuxaPaletteItems.agents(watchHosts: snap.watchHosts, unread: unread)
    #expect(items.map(\.title) == ["@waiting", "@unread", "@working", "@idle"])
    #expect(items[1].systemImage == "circle.fill")
}

@Test func onlyAnAgentWaitingForInputOffersReply() {
    #expect(MuxaNotificationActions.category(for: .attention("waiting_input")) == MuxaNotificationActions.inputCategory)
    for state in ["waiting_choice", "error", "failed", "blocked"] {
        #expect(MuxaNotificationActions.category(for: .attention(state)) == MuxaNotificationActions.agentCategory)
    }
    #expect(MuxaNotificationActions.category(for: .finished) == MuxaNotificationActions.agentCategory)
}

@Test func notificationResponsesRouteToOpenMarkReadOrReply() {
    typealias Actions = MuxaNotificationActions
    #expect(Actions.defaultAction == UNNotificationDefaultActionIdentifier)
    #expect(Actions.route(action: UNNotificationDefaultActionIdentifier, userText: nil) == .open)
    #expect(Actions.route(action: Actions.open, userText: nil) == .open)
    #expect(Actions.route(action: Actions.markRead, userText: nil) == .markRead)
    #expect(Actions.route(action: Actions.reply, userText: "  run the tests\n") == .reply("run the tests"))
    #expect(Actions.route(action: Actions.reply, userText: " \n ") == .ignore)
    #expect(Actions.route(action: Actions.reply, userText: nil) == .ignore)
    #expect(Actions.route(action: UNNotificationDismissActionIdentifier, userText: nil) == .ignore)
}

@Test @MainActor func postedNotificationsCarryTheirCategory() throws {
    let h = Harness()
    try h.ingest([AgentFixture()])
    try h.ingest([AgentFixture(state: "waiting_input", enteredAt: "2026-09-23T10:01:00Z")])
    #expect(h.poster.posted.last?.category == MuxaNotificationActions.inputCategory)
    try h.ingest([AgentFixture(state: "waiting_choice", enteredAt: "2026-09-23T10:02:00Z")])
    #expect(h.poster.posted.last?.category == MuxaNotificationActions.agentCategory)
}

@Test @MainActor func markReadFromANotificationClearsThePane() async throws {
    let h = Harness()
    try h.ingest([AgentFixture()])
    try h.ingest([AgentFixture(state: "waiting_input", enteredAt: "2026-09-23T10:01:00Z")])
    #expect(h.center.isUnread(paneA))
    #expect(h.center.dockCount == 1)
    await h.center.markRead(paneA)
    #expect(!h.center.isUnread(paneA))
    #expect(h.poster.removed.contains(MuxaAgentAttentionCenter.key(paneA)))
}

@Test @MainActor func aReplyIsSentToItsPaneAndMarksItRead() async throws {
    let h = Harness()
    var sent: [(String, String, String)] = []
    h.center.sendPrompt = { host, pane, text in sent.append((host.alias, pane.paneID, text)) }
    try h.ingest([AgentFixture()])
    try h.ingest([AgentFixture(state: "waiting_input", enteredAt: "2026-09-23T10:01:00Z")])
    let posted = h.poster.posted.count
    await h.center.reply("yes, go ahead", to: paneA)
    #expect(sent.count == 1)
    #expect(sent.first?.0 == "local")
    #expect(sent.first?.1 == "%1")
    #expect(sent.first?.2 == "yes, go ahead")
    #expect(!h.center.isUnread(paneA))
    #expect(h.poster.posted.count == posted)
}

@Test @MainActor func aReplyThatCannotBeDeliveredComesBackAsANotification() async throws {
    struct Refused: LocalizedError { var errorDescription: String? { "Prompt was rejected" } }
    let h = Harness()
    h.center.actionPaneWait = 0
    h.center.sendPrompt = { _, _, _ in throw Refused() }
    try h.ingest([AgentFixture()])
    try h.ingest([AgentFixture(state: "waiting_input", enteredAt: "2026-09-23T10:01:00Z")])

    await h.center.reply("try again", to: paneA)
    let failed = try #require(h.poster.posted.last)
    #expect(failed.subtitle == "Prompt was rejected")
    #expect(failed.body == "try again")
    #expect(failed.pane == paneA)
    #expect(failed.category == nil)
    #expect(h.center.isUnread(paneA))

    await h.center.reply("hello", to: paneB)
    #expect(h.poster.posted.last?.subtitle == "This pane is no longer available")
    #expect(h.poster.posted.last?.body == "hello")

    var sent = 0
    h.center.sendPrompt = { _, _, _ in sent += 1 }
    try h.center.ingest(snapshot([AgentFixture()], host: "gpu", local: false, state: "offline"))
    let remote = MuxaWatchPaneIdentity(hostAlias: "gpu", socket: "default", paneID: "%1")
    await h.center.reply("status?", to: remote)
    #expect(sent == 0)
    #expect(h.poster.posted.last?.subtitle == "Not connected: gpu")
}
