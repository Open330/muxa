import AppKit
import SwiftUI
import Testing
@testable import Muxa

private func menuBarSnapshot(_ agents: [(String, String, String)], host: String = "local", remoteState: String = "online", workspace: String? = nil, work: String? = nil, socket: String = "default", window: String = "@1") throws -> MuxaExecutionSnapshot {
    let panes: [[String: Any]] = agents.enumerated().map { index, entry in
        var pane: [String: Any] = ["pane_id": "%\(index + 1)", "title": entry.0, "agent_alias": entry.0,
         "session_id": "$1", "session": "demo", "window_id": window, "window_name": "work",
         "window_index": "0", "pane_index": "\(index)", "current_command": "codex", "current_path": "/tmp", "socket": socket]
        pane["workspace_id"] = workspace
        pane["work_id"] = work
        return pane
    }
    let wireAgents: [[String: Any]] = agents.enumerated().map { index, entry in
        ["kind": "codex", "agent_session_id": entry.0, "pane": "%\(index + 1)",
         "state": entry.1, "state_entered_at": entry.2, "last_prompt_at": "2026-11-06T09:00:00Z",
         "last_notification": "Please review the implementation.", "last_response": "Completed the work."]
    }
    let host = try JSONDecoder().decode(MuxaFleetHost.self, from: JSONSerialization.data(withJSONObject: [
        "alias": host, "local": host == "local", "mode": "control", "state": remoteState,
        "remote": ["agents": wireAgents, "panes": panes],
    ]))
    return MuxaExecutionSnapshot(hosts: [host])
}

@Test @MainActor func menuBarHidesAllAcknowledgedStatesAndReturnsOnNewActivity() throws {
    let center = MuxaAgentAttentionCenter(defaults: nil)
    var order = MuxaMenuBarAgentOrder()
    let working = try menuBarSnapshot([("coder", "working", "1"), ("reviewer", "working", "1")])
    center.ingest(working)
    #expect(order.update(working.hostedAgents, unread: center.unreadPanes).isEmpty)
    let waiting = try menuBarSnapshot([("coder", "waiting_input", "2"), ("reviewer", "idle", "2")])
    center.ingest(waiting)
    #expect(order.update(waiting.hostedAgents, unread: center.unreadPanes).map(\.id) == ["local:coder", "local:reviewer"])
    center.markAllRead()
    #expect(order.update(waiting.hostedAgents, unread: center.unreadPanes).isEmpty)
    center.ingest(waiting)
    #expect(order.update(waiting.hostedAgents, unread: center.unreadPanes).isEmpty)
    let next = try menuBarSnapshot([("coder", "waiting_input", "3"), ("reviewer", "idle", "2")])
    center.ingest(next)
    #expect(order.update(next.hostedAgents, unread: center.unreadPanes).map(\.id) == ["local:coder"])
    center.markRead(MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%1"), agentSessionID: "coder")
    #expect(order.update(next.hostedAgents, unread: center.unreadPanes).isEmpty)
}

@MainActor private func menuBarUnread(_ hosted: [MuxaHostedAgent]) -> Set<MuxaWatchPaneIdentity> {
    Set(hosted.compactMap { MuxaAgentAttentionCenter.paneIdentity(for: $0) })
}

@Test func menuBarAdaptsFromZeroToManyWithinItsWidthBudget() {
    for budget: CGFloat in [80, 120, 240, 320] {
        for total in 0...250 {
            let count = MuxaMenuBarGeometry.visibleAgentCount(total: total, maximumWidth: budget)
            #expect(count >= 0 && count <= total)
            let width = MuxaMenuBarGeometry(count: count).width + (count < total ? MuxaMenuBarGeometry.overflowWidth : 0)
            #expect(width <= budget)
            if count < total {
                #expect(MuxaMenuBarGeometry(count: count + 1).width + (count + 1 < total ? MuxaMenuBarGeometry.overflowWidth : 0) > budget)
            }
        }
    }
    for total in 0...12 {
        #expect(MuxaMenuBarGeometry.visibleAgentCount(total: total, maximumWidth: 320) == total)
    }
}

@Test func menuBarRespectsConfiguredLimitIncludingZeroWithoutLosingOverflow() {
    for limit in 0...8 {
        for total in 0...50 {
            let count = MuxaMenuBarGeometry.visibleAgentCount(total: total, maximumWidth: 320, maximumAgents: limit)
            #expect(count == min(total, limit))
            let overflow = total - count
            #expect(count + overflow == total)
            #expect(MuxaMenuBarGeometry(count: count).width + (overflow > 0 ? MuxaMenuBarGeometry.overflowWidth : 0) <= 320)
        }
    }
    #expect(MuxaMenuBarGeometry.visibleAgentCount(total: 50, maximumWidth: 80, maximumAgents: 8) == 0)
}

@Test func menuBarLimitDefaultsToThreeAndPersistsAndBoundsUserSelection() throws {
    let suite = "muxa-menubar-limit-\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    #expect(MuxaPreferences.menuBarAgentLimit(defaults) == 3)
    for (stored, expected) in [(0, 0), (1, 1), (5, 5), (8, 8), (-3, 0), (999, 8)] {
        defaults.set(stored, forKey: MuxaPreferences.menuBarAgentLimitKey)
        #expect(MuxaPreferences.menuBarAgentLimit(defaults) == expected)
    }
}

@Test @MainActor func menuBarSettingsShortcutFindsTheSettingsSceneCommand() throws {
    let root = NSMenu()
    let app = NSMenuItem(title: "Muxa", action: nil, keyEquivalent: "")
    let submenu = NSMenu()
    let other = NSMenuItem(title: "Other", action: nil, keyEquivalent: ",")
    other.keyEquivalentModifierMask = [.command, .option]
    submenu.addItem(other)
    let settings = NSMenuItem(title: "Settings…", action: nil, keyEquivalent: ",")
    settings.keyEquivalentModifierMask = .command
    submenu.addItem(settings)
    app.submenu = submenu
    root.addItem(app)
    let command = try #require(MuxaSettingsOpener.settingsCommand(in: root))
    #expect(command.0 === submenu)
    #expect(command.1 == 1)
    settings.isEnabled = false
    #expect(MuxaSettingsOpener.settingsCommand(in: root) == nil)
}

@Test @MainActor func menuBarGroupsWorksWithoutMergingHostsOrWorkspaces() throws {
    let first = try menuBarSnapshot([("one", "waiting_input", "1"), ("two", "idle", "1")], workspace: "project-a", work: "BUILD")
    let otherProject = try menuBarSnapshot([("three", "idle", "1")], workspace: "project-b", work: "BUILD")
    let remote = try menuBarSnapshot([("four", "idle", "1")], host: "remote", workspace: "project-a", work: "BUILD")
    let items = (first.hostedAgents + otherProject.hostedAgents + remote.hostedAgents).map { MuxaMenuBarAgent(hosted: $0, unread: true) }
    let works = MuxaMenuBarChip.activity(items, grouping: .work)
    #expect(works.count == 3)
    #expect(works[0].memberIDs == ["local:one", "local:two"])
    #expect(works[0].status.needsAttention)
    #expect(Set(works.map(\.id)).count == 3)
    guard case .group(let scope) = works[0].destination else { Issue.record("Expected Work destination"); return }
    #expect(scope.contains(items[0]))
    #expect(!scope.contains(items[2]))
    #expect(!scope.contains(items[3]))
    let state = MuxaMenuBarState()
    state.agents = items
    state.scope = scope
    #expect(state.visibleAgents.map(\.id) == ["local:one", "local:two"])
    let hosts = MuxaMenuBarChip.activity(items, grouping: .fleet)
    #expect(hosts.count == 2)
    #expect(hosts[0].memberIDs.count == 3)
    #expect(MuxaMenuBarChip.activity(items, grouping: .agent).count == 4)
}

@Test @MainActor func menuBarUnmanagedWorkUsesSocketAndWindowIdentityInsteadOfDisplayNames() throws {
    let first = try menuBarSnapshot([("one", "idle", "1")], socket: "a")
    let second = try menuBarSnapshot([("two", "idle", "1")], socket: "b")
    let third = try menuBarSnapshot([("three", "idle", "1")], socket: "a", window: "@2")
    let items = (first.hostedAgents + second.hostedAgents + third.hostedAgents).map { MuxaMenuBarAgent(hosted: $0, unread: true) }
    #expect(MuxaMenuBarChip.activity(items, grouping: .work).count == 3)
}

@Test @MainActor func menuBarAasUsesTheSharedChipGeometryAndHasNoAgentRecipient() {
    let chip = MuxaMenuBarChip.aas(AasAccountStore())
    #expect(chip.id == "extension:aas")
    #expect(chip.memberIDs.isEmpty)
    #expect(chip.appearance.symbol == "gauge.medium")
    guard case .aas = chip.destination else { Issue.record("Expected usage destination"); return }
    let view = NSHostingView(rootView: MuxaMenuBarChipPeek(item: chip))
    #expect(view.fittingSize.width <= 340)
    let geometry = MuxaMenuBarGeometry(count: 1)
    let width = MuxaMenuBarAgentRenderer.pillWidth([chip])
    let image = MuxaMenuBarAgentRenderer.image(chips: [chip], positions: geometry.layout(focus: 0, pillWidth: width), focus: 0,
        pillWidth: width, lead: geometry.hoverLead(pillWidth: width), markOpacity: 0, overflow: 0, selectedID: nil,
        liveIDs: [chip.id], dark: false, mark: "terminal")
    #expect(image.size.height == 24)
}

@Test @MainActor func menuBarOrdersObservedChangesWithoutComparingHostClocks() throws {
    var order = MuxaMenuBarAgentOrder()
    let local = try menuBarSnapshot([("local-agent", "waiting_input", "2099-01-01T00:00:00Z")])
    let remote = try menuBarSnapshot([("remote-agent", "working", "2001-01-01T00:00:00Z")], host: "remote")
    _ = order.update(local.hostedAgents + remote.hostedAgents, unread: menuBarUnread(local.hostedAgents + remote.hostedAgents))
    let changed = try menuBarSnapshot([("remote-agent", "waiting_choice", "2001-01-02T00:00:00Z")], host: "remote")
    let sorted = order.update(local.hostedAgents + changed.hostedAgents, unread: menuBarUnread(local.hostedAgents + changed.hostedAgents))
    #expect(sorted.map(\.id) == ["remote:remote-agent", "local:local-agent"])
    #expect(order.update(Array((local.hostedAgents + changed.hostedAgents).reversed()), unread: menuBarUnread(local.hostedAgents + changed.hostedAgents)).map(\.id) == sorted.map(\.id))
}

@Test func menuBarCapsuleRetainsAStationaryPointerAndFitsItsLeadingExtension() {
    for count in 1...4 {
        let geometry = MuxaMenuBarGeometry(count: count)
        for pill: CGFloat in [18, 36, 60, 101, 130] {
            let lead = geometry.hoverLead(pillWidth: pill)
            #expect(lead.truncatingRemainder(dividingBy: 4) == 0)
            for pixel in 0..<Int(geometry.stackWidth * 2) {
                let x = CGFloat(pixel) / 2
                let focus = geometry.index(at: x, focus: nil, pillWidth: pill)
                #expect(geometry.index(at: x, focus: focus, pillWidth: pill) == focus)
            }
            for focus in 0..<count {
                let xs = geometry.layout(focus: focus, pillWidth: pill)
                #expect((xs.first ?? 0) + lead >= 0)
                let end = focus == count - 1 ? xs[focus] + pill + 2 * MuxaMenuBarGeometry.inset : xs[count - 1] + MuxaMenuBarGeometry.slot
                #expect(end <= geometry.width)
                for index in 1..<count {
                    let expected = index == focus ? MuxaMenuBarGeometry.reveal
                        : index == focus + 1 ? pill + MuxaMenuBarGeometry.reveal - MuxaMenuBarGeometry.disc : MuxaMenuBarGeometry.step
                    #expect(xs[index] - xs[index - 1] == expected)
                }
                // The foreground capsule must win everywhere it is painted,
                // including over the space formerly occupied by another chip.
                let start = xs[focus] + MuxaMenuBarGeometry.inset
                for pixel in 0...Int(pill * 2) {
                    #expect(geometry.index(at: start + CGFloat(pixel) / 2, focus: focus, pillWidth: pill) == focus)
                }
            }
            #expect(geometry.index(at: geometry.markX + 12, focus: nil, pillWidth: pill) == nil)
        }
    }
}

@Test func menuBarCapsuleSweepsBothDirectionsWithoutSelectionOscillation() {
    let geometry = MuxaMenuBarGeometry(count: 3)
    for pill: CGFloat in [36, 60, 101] {
        let lead = geometry.hoverLead(pillWidth: pill)
        let points = Array(Int(-lead)...Int(geometry.width))
        for sweep in [points, Array(points.reversed())] {
            var focus: Int?
            var visited: Set<Int> = []
            for point in sweep {
                let x = CGFloat(point)
                let next = geometry.index(at: x, focus: focus, pillWidth: pill)
                if let next {
                    visited.insert(next)
                    #expect(geometry.index(at: x, focus: next, pillWidth: pill) == next)
                }
                focus = next
            }
            #expect(visited == [0, 1, 2])
        }
    }
}

@Test func menuBarButtonPaddingAndLeadKeepTheFoldedStackAtTheSameScreenPosition() {
    let geometry = MuxaMenuBarGeometry(count: 3)
    let base = geometry.width + MuxaMenuBarGeometry.overflowWidth
    let right: CGFloat = 1500
    for lead: CGFloat in [0, 12, 36, 80] {
        let image = base + lead
        let button = image + 2
        let origin = MuxaMenuBarGeometry.origin(buttonWidth: button, imageWidth: image, lead: lead)
        #expect(origin == lead + 1)
        #expect(right - button + origin == right - base - 1)
    }
    let empty = MuxaMenuBarGeometry(count: 0)
    #expect(empty.index(at: 0, focus: nil, pillWidth: 60) == nil)
    #expect(empty.hoverLead(pillWidth: 60) == 0)
}

@Test @MainActor func menuBarCapsulesUseOneMeasuredWidthAndLabelUnreadCompletion() throws {
    let snap = try menuBarSnapshot([("coder", "waiting_input", "1"), ("reviewer", "waiting_choice", "1"), ("builder", "error", "1")])
    let agents = snap.hostedAgents.map { MuxaMenuBarAgent(hosted: $0, unread: false) }
    #expect(MuxaMenuBarAgentRenderer.pillWidth(agents) == agents.map { MuxaMenuBarAgentRenderer.pillWidth([$0]) }.max())
    let finished = try menuBarSnapshot([("coder", "idle", "1")])
    let item = MuxaMenuBarAgent(hosted: finished.hostedAgents[0], unread: true)
    #expect(item.pillLabel == String(localized: "Finished"))
}

@Test @MainActor func menuBarDistinguishesAgentSessionsAndHosts() throws {
    var order = MuxaMenuBarAgentOrder()
    let local = try menuBarSnapshot([("same", "error", "1")])
    let remote = try menuBarSnapshot([("same", "blocked", "1")], host: "remote")
    #expect(Set(order.update(local.hostedAgents + remote.hostedAgents, unread: menuBarUnread(local.hostedAgents + remote.hostedAgents)).map(\.id)).count == 2)
    let replaced = try menuBarSnapshot([("replacement", "waiting_input", "1")])
    let items = order.update(replaced.hostedAgents, unread: menuBarUnread(replaced.hostedAgents))
    #expect(items.map(\.id) == ["local:replacement"])
    #expect(items.first?.pane == MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%1"))
}

@Test @MainActor func menuBarShowsDisconnectedHostWithoutAllowingReply() throws {
    let snap = try menuBarSnapshot([("coder", "waiting_input", "1")], host: "remote", remoteState: "offline")
    var order = MuxaMenuBarAgentOrder()
    let item = try #require(order.update(snap.hostedAgents, unread: menuBarUnread(snap.hostedAgents)).first)
    #expect(!item.connected)
    #expect(MuxaNotificationActions.replyBlocker(snap.watchPane(id: try #require(item.pane))) != nil)
}

@Test @MainActor func menuBarShowsUnseenLimitsThatNeedManualAccountSwitch() throws {
    var wire: [String: Any] = ["kind": "codex", "agent_session_id": "limited", "state": "error",
                               "rate_limited_until": "2099-01-01T00:00:00Z", "rate_limit_scope": "five_hour"]
    let agent = try JSONDecoder().decode(MuxaAgent.self, from: JSONSerialization.data(withJSONObject: wire))
    let host = MuxaFleetHostIdentity(alias: "local", local: true, state: "online", mode: "control")
    var order = MuxaMenuBarAgentOrder()
    let snap = try menuBarSnapshot([("limited", "waiting_input", "1")])
    let pane = snap.hostedAgents[0].pane
    let unread = menuBarUnread(snap.hostedAgents)
    #expect(agent.status.isLimited)
    #expect(agent.status.needsAttention)
    #expect(order.update([MuxaHostedAgent(host: host, agent: agent, pane: pane)], unread: unread).count == 1)
    #expect(!MuxaAgentStatus.limited(MuxaRateLimitCap(scope: .fiveHour, until: nil, covered: true)).needsAttention)
    wire["state"] = "waiting_input"
    let waiting = try JSONDecoder().decode(MuxaAgent.self, from: JSONSerialization.data(withJSONObject: wire))
    #expect(order.update([MuxaHostedAgent(host: host, agent: waiting, pane: pane)], unread: unread).count == 1)
}

@Test func menuBarIdentityColorsPersistAndProviderSymbolsAreAvailable() {
    let a = MuxaMenuBarAgentAppearance(identity: "host:session-1", kind: "codex")
    #expect(a == MuxaMenuBarAgentAppearance(identity: "host:session-1", kind: "codex"))
    let programs = ["codex", "claude_code", "gemini_cli", "antigravity", "opencode", "unknown"]
    let styles = programs.map { MuxaMenuBarAgentAppearance(identity: "host:session-1", kind: $0) }
    #expect(Set(styles.map(\.symbol)).count >= 5)
    for style in styles {
        // Claude and Codex draw muxa's own marks; every other face is an SF Symbol.
        if case .symbol(let name) = MuxaGlyph.kind(style.symbol) {
            #expect(NSImage(systemSymbolName: name, accessibilityDescription: nil) != nil)
        }
    }
    let palette = (0..<32).map { MuxaMenuBarAgentAppearance(identity: "agent-\($0)", kind: "codex") }
    #expect(Set(palette.map(\.paletteIndex)).count == 8)
}

@Test func menuBarIdentityPaletteKeepsSmallWhiteTextReadable() {
    func luminance(_ color: NSColor) -> Double {
        let rgb = color.usingColorSpace(.sRGB)!
        func linear(_ value: CGFloat) -> Double {
            let v = Double(value)
            return v <= 0.04045 ? v / 12.92 : pow((v + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * linear(rgb.redComponent) + 0.7152 * linear(rgb.greenComponent) + 0.0722 * linear(rgb.blueComponent)
    }
    for index in 0..<32 {
        let style = MuxaMenuBarAgentAppearance(identity: "agent-\(index)", kind: "codex")
        for color in [style.top, style.bottom] { #expect(1.05 / (luminance(color) + 0.05) >= 4.5) }
    }
}

@Test @MainActor func menuBarCompletionUsesCheckmarkAndKeepsItsIdentityWhenStatusChanges() throws {
    let working = try menuBarSnapshot([("coder", "working", "1")])
    let idle = try menuBarSnapshot([("coder", "idle", "2")])
    let before = MuxaMenuBarAgent(hosted: working.hostedAgents[0], unread: false)
    let after = MuxaMenuBarAgent(hosted: idle.hostedAgents[0], unread: true)
    #expect(before.appearance == after.appearance)
    #expect(after.menuStatusSymbol == "checkmark.circle.fill")
    #expect(after.menuStatusLabel == String(localized: "Finished"))
    #expect(after.location.contains("demo"))
}

@Test @MainActor func menuBarHoverKeepsLongAgentNamesAndLocationsWithinTheScreen() throws {
    let snap = try menuBarSnapshot([(String(repeating: "long-agent-name", count: 20), "idle", "1")])
    let item = MuxaMenuBarAgent(hosted: snap.hostedAgents[0], unread: true)
    let view = NSHostingView(rootView: MuxaMenuBarPeek(item: item))
    #expect(view.fittingSize.width <= 340)
    #expect(view.fittingSize.height < 100)
}

@Test func menuBarBadgeCentersAndVisibleSliversChooseTheirPaintedOwner() {
    for count in 1...3 {
        let geometry = MuxaMenuBarGeometry(count: count)
        for focus in [nil] + (0..<count).map(Optional.some) {
            for pill: CGFloat in [18, 50, 101] {
                let paint = MuxaMenuBarPaintGeometry(count: count, positions: geometry.layout(focus: focus, pillWidth: pill), focus: focus, pillWidth: pill, badged: Set(0..<count))
                for owner in 0..<count {
                    let badge = paint.badges[owner]!
                    #expect(paint.index(at: CGPoint(x: badge.midX, y: badge.midY)) == owner)
                }
            }
        }
    }
    let geometry = MuxaMenuBarGeometry(count: 3)
    let paint = MuxaMenuBarPaintGeometry(count: 3, positions: [0, 13, 26], focus: nil, pillWidth: 50, badged: [0, 1, 2])
    #expect(paint.index(at: CGPoint(x: 14, y: 12)) == 0)
    #expect(geometry.index(at: 14, focus: nil, pillWidth: 50) == 0)
    #expect(paint.index(at: CGPoint(x: 20, y: 18.5)) == 0)
    #expect(paint.index(at: CGPoint(x: 33, y: 18.5)) == 1)
    #expect(paint.index(at: CGPoint(x: 20, y: 12)) == 0) // left face is the front of the folded avatar stack
}

@Test func menuBarHitsTrackThePaintedIntermediateAnimationInsteadOfTheFinalLayout() {
    let paint = MuxaMenuBarPaintGeometry(count: 3, positions: [0, 13, 26], focus: 0, pillWidth: 18, badged: [])
    #expect(paint.index(at: CGPoint(x: 35, y: 12)) == 1) // front chip owns its clear separator
    #expect(paint.index(at: CGPoint(x: 37, y: 12)) == 2)
    #expect(paint.index(at: CGPoint(x: 60, y: 12)) == nil)
}

@MainActor private final class MenuReplyGate {
    var delivery: CheckedContinuation<Void, Error>?
    var waiter: CheckedContinuation<Void, Never>?
    func deliver(_ text: String) async throws {
        try await withCheckedThrowingContinuation { continuation in
            delivery = continuation
            waiter?.resume()
            waiter = nil
        }
    }
    func waitUntilStarted() async {
        if delivery != nil { return }
        await withCheckedContinuation { waiter = $0 }
    }
    func finish(_ error: Error? = nil) {
        if let error { delivery?.resume(throwing: error) } else { delivery?.resume() }
        delivery = nil
    }
}

@MainActor private final class MenuReplyPoster: MuxaNotificationPosting {
    var posted: [MuxaAgentNotification] = []
    func post(_ notification: MuxaAgentNotification) { posted.append(notification) }
    func removeDelivered(identifiers: [String]) {}
    func setDockBadge(_ count: Int) {}
}
private struct MenuReplyError: LocalizedError {
    var errorDescription: String? { "Injected offline failure" }
}

@Test @MainActor func menuBarDraftSurvivesAgentSwitchAndCloseWithAnInFlightFailure() async throws {
    let snap = try menuBarSnapshot([("coder", "waiting_input", "1"), ("reviewer", "waiting_input", "1")])
    let item = MuxaMenuBarAgent(hosted: snap.hostedAgents[0], unread: true)
    let other = MuxaMenuBarAgent(hosted: snap.hostedAgents[1], unread: true)
    let state = MuxaMenuBarState()
    state.agents = [item, other]
    state.selectedID = item.id
    let draft = state.draft(for: item)
    draft.text = "  Preserve this reply  "
    state.selectedID = other.id
    state.draft(for: other).text = "Another agent's draft"
    state.selectedID = item.id
    #expect(state.draft(for: item) === draft)
    #expect(draft.text == "  Preserve this reply  ")
    let poster = MenuReplyPoster()
    let attention = MuxaAgentAttentionCenter(defaults: nil, poster: poster)
    let pane = try #require(item.pane)
    let gate = MenuReplyGate()
    let task = Task {
        await draft.send(using: gate.deliver) { text, reason in
            attention.postReplyFailure(text, to: pane, reason: reason)
        }
    }
    await gate.waitUntilStarted()
    state.selectedID = nil // hosting controller can now be destroyed
    state.agents = [] // even an ended session's unsent text remains recoverable
    #expect(draft.sending)
    #expect(state.displayedAgents.contains { $0.id == item.id })
    var duplicates = 0
    await draft.send(using: { _ in duplicates += 1 }, onFailure: { _, _ in })
    #expect(duplicates == 0)
    gate.finish(MenuReplyError())
    await task.value
    #expect(!draft.sending)
    #expect(draft.text == "  Preserve this reply  ")
    #expect(draft.failure == "Injected offline failure")
    #expect(state.draft(for: item) === draft)
    #expect(state.replyFailure(for: item.id) == draft.failure)
    #expect(poster.posted.count == 1)
    #expect(poster.posted.first?.body == "Preserve this reply")
    #expect(poster.posted.first?.pane == pane)
}

@Test @MainActor func menuBarSuccessfulSendDoesNotEraseNewerText() async throws {
    let snap = try menuBarSnapshot([("coder", "waiting_input", "1")])
    let item = MuxaMenuBarAgent(hosted: snap.hostedAgents[0], unread: false)
    let state = MuxaMenuBarState()
    let draft = state.draft(for: item)
    draft.text = "First reply"
    let gate = MenuReplyGate()
    let task = Task { await draft.send(using: gate.deliver, onFailure: { _, _ in }) }
    await gate.waitUntilStarted()
    draft.text = "Newer reply"
    gate.finish()
    await task.value
    #expect(draft.text == "Newer reply")
    var delivered: String?
    await draft.send(using: { delivered = $0 }, onFailure: { _, _ in })
    #expect(delivered == "Newer reply")
    #expect(draft.text.isEmpty)
    #expect(!draft.needsRecovery)
    #expect(state.displayedAgents.isEmpty)
}

@Test @MainActor func menuBarSessionBoundReadDoesNotConsumeAReplacementAgent() throws {
    let center = MuxaAgentAttentionCenter(defaults: nil)
    let first = try menuBarSnapshot([("first", "waiting_input", "1")])
    center.ingest(first)
    let next = try menuBarSnapshot([("replacement", "idle", "2")])
    center.ingest(next)
    let pane = try #require(MuxaAgentAttentionCenter.paneIdentity(for: next.hostedAgents[0]))
    #expect(center.isUnread(pane))
    center.markRead(pane, agentSessionID: "first")
    #expect(center.isUnread(pane))
    center.markRead(pane, agentSessionID: "replacement")
    #expect(!center.isUnread(pane))
}

@Test @MainActor func menuBarFailedEndedDraftCanBeExplicitlyDiscardedAndPruned() async throws {
    let snap = try menuBarSnapshot([("coder", "waiting_input", "1")])
    let item = MuxaMenuBarAgent(hosted: snap.hostedAgents[0], unread: false)
    let state = MuxaMenuBarState()
    let draft = state.draft(for: item)
    draft.text = "Unsent reply"
    await draft.send(using: { _ in throw MenuReplyError() }, onFailure: { _, _ in })
    state.agents = []
    state.updateDraftAgents([], unread: [])
    #expect(state.displayedAgents.map(\.id) == [item.id])
    draft.discard()
    #expect(draft.text.isEmpty && draft.failure == nil)
    #expect(state.displayedAgents.isEmpty)
    state.updateDraftAgents([], unread: [])
    #expect(state.draft(for: item) !== draft)
}

@Test func menuBarPollingKeepsHoverSmoothWithoutSixtyIdleChecksPerSecond() {
    #expect(MuxaMenuBarPolling.interval(pointerInside: false, animating: false, popoverShown: false) == 1.0 / 15)
    #expect(MuxaMenuBarPolling.interval(pointerInside: true, animating: false, popoverShown: false) == 1.0 / 60)
    #expect(MuxaMenuBarPolling.interval(pointerInside: false, animating: true, popoverShown: false) == 1.0 / 60)
    #expect(MuxaMenuBarPolling.interval(pointerInside: true, animating: false, popoverShown: true) == 1.0 / 15)
    #expect(MuxaMenuBarPolling.interval(pointerInside: false, animating: false, popoverShown: false, waitingToFold: true) == 1.0 / 60)
}

@Test func menuBarUsagePollingPausesForHiddenChipsAndAvoidsRecentReadsAndFailureRetries() {
    let now = Date(timeIntervalSince1970: 1_800_000_000)
    let old = now.addingTimeInterval(-121)
    let recent = now.addingTimeInterval(-10)
    #expect(MuxaMenuBarPolling.refreshUsage(chipShown: true, lastAttempt: old, lastSuccess: old, now: now))
    #expect(!MuxaMenuBarPolling.refreshUsage(chipShown: false, lastAttempt: old, lastSuccess: old, now: now))
    #expect(!MuxaMenuBarPolling.refreshUsage(chipShown: true, lastAttempt: old, lastSuccess: recent, now: now))
    #expect(!MuxaMenuBarPolling.refreshUsage(chipShown: true, lastAttempt: recent, lastSuccess: old, now: now))
}

@Test @MainActor func menuBarRenderSnapshotTracksLiveGeometryAndRecipientChanges() throws {
    let snapshot = try menuBarSnapshot([("first", "idle", "1"), ("second", "idle", "1")])
    let items = snapshot.hostedAgents.map { MuxaMenuBarAgent(hosted: $0, unread: true) }
    func frame(_ item: MuxaMenuBarAgent, position: CGFloat = 0, live: Bool = true) -> MuxaMenuBarRenderState {
        let chip = MuxaMenuBarChip.agent(item)
        return MuxaMenuBarRenderState(chips: [chip], positions: [position], focus: nil, pillWidth: 18, lead: 0,
            markOpacity: 1, overflow: 0, selectedID: nil, liveIDs: live ? [chip.id] : [], dark: false, mark: "terminal")
    }
    let original = frame(items[0])
    #expect(original == frame(items[0]))
    #expect(original != frame(items[1]))
    #expect(original != frame(items[0], position: 3))
    #expect(original != frame(items[0], live: false))
    #expect(original != frame(MuxaMenuBarAgent(hosted: items[0].hosted, unread: false)))
}

@Test func menuBarHoverGraceKeepsItsDeadlineAndCancelsOnReturn() {
    let now = Date(timeIntervalSince1970: 100)
    var intent = MuxaMenuBarFoldIntent()
    intent.schedule(now: now, grace: MuxaMenuBarHoverPolicy.exitGrace)
    intent.schedule(now: now.addingTimeInterval(0.1), grace: MuxaMenuBarHoverPolicy.exitGrace)
    #expect(intent.deadline == now.addingTimeInterval(0.2))
    let early = intent.consume(now: now.addingTimeInterval(0.19))
    #expect(!early)
    let due = intent.consume(now: now.addingTimeInterval(0.2))
    #expect(due)
    intent.schedule(now: now, grace: MuxaMenuBarHoverPolicy.peekGrace)
    let peekEarly = intent.consume(now: now.addingTimeInterval(0.3))
    #expect(!peekEarly)
    intent.cancel()
    let cancelled = intent.consume(now: now.addingTimeInterval(1))
    #expect(!cancelled)
}

@Test func menuBarHoverMarginAndPeekRespectScreenCoordinates() {
    let item = CGRect(x: 100, y: 700, width: 100, height: 24)
    let peek = CGRect(x: 70, y: 630, width: 160, height: 65)
    #expect(MuxaMenuBarHoverPolicy.zone(point: CGPoint(x: 120, y: 710), item: item, peek: peek) == .item)
    #expect(MuxaMenuBarHoverPolicy.zone(point: CGPoint(x: 98, y: 695), item: item, peek: peek) == .margin)
    #expect(MuxaMenuBarHoverPolicy.zone(point: CGPoint(x: 120, y: 680), item: item, peek: peek) == .peek)
    #expect(MuxaMenuBarHoverPolicy.zone(point: CGPoint(x: 20, y: 680), item: item, peek: peek) == .outside)
    #expect(MuxaMenuBarHoverPolicy.foldDuration > MuxaMenuBarHoverPolicy.unfoldDuration)
}

@Test @MainActor func menuBarPixelKeyIgnoresHiddenLabelsButTracksVisiblePillAndScale() throws {
    let snap = try menuBarSnapshot([("coder", "idle", "1")])
    let chip = MuxaMenuBarChip.agent(MuxaMenuBarAgent(hosted: snap.hostedAgents[0], unread: true))
    let changed = MuxaMenuBarChip(id: chip.id, name: "new accessible name", location: "new host location",
        pillLabel: "new value", menuStatusLabel: "new status text", appearance: chip.appearance,
        status: chip.status, unread: chip.unread, memberIDs: chip.memberIDs, destination: chip.destination)
    func frame(_ chip: MuxaMenuBarChip, focus: Int? = nil, scale: CGFloat = 1) -> MuxaMenuBarRenderState {
        MuxaMenuBarRenderState(chips: [chip], positions: [0], focus: focus, pillWidth: 70,
            lead: 0, markOpacity: 1, overflow: 0, selectedID: nil, liveIDs: [chip.id], dark: false,
            mark: "terminal", backingScales: [scale])
    }
    #expect(frame(chip) == frame(changed))
    #expect(frame(chip, focus: 0) != frame(changed, focus: 0))
    #expect(frame(chip) != frame(chip, scale: 2))
}

@Test @MainActor func menuBarBitmapsFillTheirRetinaRepresentation() throws {
    let snap = try menuBarSnapshot([("coder", "waiting_input", "1")])
    let chip = MuxaMenuBarChip.agent(MuxaMenuBarAgent(hosted: snap.hostedAgents[0], unread: true))
    let image = MuxaMenuBarAgentRenderer.image(chips: [chip], positions: [0], focus: nil, pillWidth: 18,
        lead: 0, markOpacity: 1, overflow: 0, selectedID: nil, liveIDs: [chip.id], dark: false,
        mark: "terminal", scales: [1, 2])
    let reps = image.representations.compactMap { $0 as? NSBitmapImageRep }.sorted { $0.pixelsWide < $1.pixelsWide }
    #expect(reps.count == 2)
    for (rep, scale) in zip(reps, [1, 2]) {
        #expect(rep.pixelsWide == Int(image.size.width) * scale)
        #expect(rep.pixelsHigh == 24 * scale)
        #expect(rep.size == image.size)
        var maxX = 0, maxY = 0
        for y in 0..<rep.pixelsHigh {
            for x in 0..<rep.pixelsWide where (rep.colorAt(x: x, y: y)?.alphaComponent ?? 0) > 0.2 {
                maxX = max(maxX, x); maxY = max(maxY, y)
            }
        }
        #expect(maxX > Int(CGFloat(rep.pixelsWide) * 0.8))
        #expect(maxY > Int(CGFloat(rep.pixelsHigh) * 0.65))
    }
}


@Test @MainActor func menuBarGlyphCacheReusesBitmapsAndSeparatesDisplayScales() throws {
    let cache = MuxaMenuBarGlyphCache()
    let first = try #require(cache.glyph("terminal", size: NSSize(width: 12, height: 12), scales: [1]))
    let again = cache.glyph("terminal", size: NSSize(width: 12, height: 12), scales: [1])
    #expect(first === again)
    let retina = try #require(cache.glyph("terminal", size: NSSize(width: 12, height: 12), scales: [1, 2]))
    #expect(first !== retina)
    #expect(retina.representations.map(\.pixelsWide).sorted() == [12, 24])
    for width in 1...200 {
        _ = cache.glyph("terminal", size: NSSize(width: width, height: 12), scales: [1])
    }
    #expect(cache.count <= MuxaMenuBarGlyphCache.limit)
}

@Test @MainActor func menuBarCachedGlyphsPreserveVisibleRendering() throws {
    let snap = try menuBarSnapshot([("coder", "waiting_input", "1")])
    let chip = MuxaMenuBarChip.agent(MuxaMenuBarAgent(hosted: snap.hostedAgents[0], unread: true))
    func image(_ cache: MuxaMenuBarGlyphCache?) -> NSImage {
        MuxaMenuBarAgentRenderer.image(chips: [chip], positions: [0], focus: nil, pillWidth: 18,
            lead: 0, markOpacity: 1, overflow: 0, selectedID: nil, liveIDs: [chip.id], dark: false,
            mark: "terminal", scales: [1, 2], glyphCache: cache)
    }
    let cached = image(MuxaMenuBarGlyphCache()).representations.compactMap { $0 as? NSBitmapImageRep }
    let fresh = image(nil).representations.compactMap { $0 as? NSBitmapImageRep }
    for (a, b) in zip(cached, fresh) {
        #expect(a.pixelsWide == b.pixelsWide && a.pixelsHigh == b.pixelsHigh)
        var error: CGFloat = 0
        for y in 0..<a.pixelsHigh {
            for x in 0..<a.pixelsWide {
                let c = try #require(a.colorAt(x: x, y: y)?.usingColorSpace(.deviceRGB))
                let d = try #require(b.colorAt(x: x, y: y)?.usingColorSpace(.deviceRGB))
                error += abs(c.alphaComponent - d.alphaComponent)
                    + abs(c.redComponent * c.alphaComponent - d.redComponent * d.alphaComponent)
                    + abs(c.greenComponent * c.alphaComponent - d.greenComponent * d.alphaComponent)
                    + abs(c.blueComponent * c.alphaComponent - d.blueComponent * d.alphaComponent)
            }
        }
        // Intermediate rasterization may change antialiasing on a glyph edge;
        // the overall chip and badge must preserve the reference rendering.
        #expect(error / CGFloat(a.pixelsWide * a.pixelsHigh * 4) < 0.015)
    }
}

@Test @MainActor func homeProjectionRefreshesOnPolicyChangesWithoutALiveDaemon() async throws {
    let model = AppModel()
    model.ingestExecutionSnapshotForTesting(try menuBarSnapshot([("first", "working", "1")]))
    NotificationCenter.default.post(name: MuxaLimitCoverage.didChange, object: nil)
    for _ in 0..<200 where model.homeWorks.isEmpty { try await Task.sleep(for: .milliseconds(10)) }
    #expect(model.homeWorks.flatMap(\.agents).map(\.agent.agentSessionID) == ["first"])
    model.ingestExecutionSnapshotForTesting(try menuBarSnapshot([("second", "waiting_input", "2")]))
    NotificationCenter.default.post(name: MuxaLimitCoverage.didChange, object: nil)
    for _ in 0..<200 where model.homeWorks.first?.agents.first?.agent.agentSessionID != "second" {
        try await Task.sleep(for: .milliseconds(10))
    }
    #expect(model.homeWorks.first?.agents.first?.agent.agentSessionID == "second")
    #expect(model.homeWorks.first?.lane == .needsYou)
}

@Test func menuBarCardClicksToggleOnlyTheirOwnDestination() {
    let now = Date()
    #expect(MuxaMenuBarCardInteraction.action(requested: "b", current: "a", shown: true,
        closed: nil, closedAt: nil, now: now) == .open)
    #expect(MuxaMenuBarCardInteraction.action(requested: "a", current: "a", shown: true,
        closed: nil, closedAt: nil, now: now) == .close)
    for destination: String? in [nil, "a"] {
        #expect(MuxaMenuBarCardInteraction.action(requested: destination, current: nil, shown: false,
            closed: destination, closedAt: now.addingTimeInterval(-0.1), now: now) == .ignore)
        #expect(MuxaMenuBarCardInteraction.action(requested: "b", current: nil, shown: false,
            closed: destination, closedAt: now.addingTimeInterval(-0.1), now: now) == .open)
        #expect(MuxaMenuBarCardInteraction.action(requested: destination, current: nil, shown: false,
            closed: destination, closedAt: now.addingTimeInterval(-1), now: now) == .open)
    }
}

@Test @MainActor func menuBarPopoverOpensSwitchesDestinationsAndAcceptsFocus() async throws {
    let model = AppModel(client: MuxaIPCClient(socketPath: "/tmp/muxa-menubar-test-unused.sock"))
    let snapshot = try menuBarSnapshot([("first", "waiting_input", "1"), ("second", "waiting_input", "1")])
    model.ingestExecutionSnapshotForTesting(snapshot)
    model.attention.ingest(snapshot)
    let controller = MuxaMenuBarAgentController(model: model)
    defer { controller.stop() }
    // The menu-bar host registers a newly created status item asynchronously.
    try await Task.sleep(for: .milliseconds(250))
    controller.showOverview()
    let surface = try #require(controller.presentedCard)
    #expect(surface.isShown, Comment(rawValue: controller.presentationDiagnostics))
    let window = try #require(surface.contentViewController?.view.window)
    #expect(window.isKeyWindow)

    let chips = snapshot.hostedAgents.map { MuxaMenuBarChip.agent(MuxaMenuBarAgent(hosted: $0, unread: true)) }
    for chip in chips {
        controller.showChip(chip)
        #expect(controller.presentedCard === surface)
        #expect(surface.isShown)
        #expect(window.isKeyWindow)
    }
    let agents = snapshot.hostedAgents.map { MuxaMenuBarAgent(hosted: $0, unread: true) }
    let group = try #require(MuxaMenuBarChip.activity(agents, grouping: .work).first)
    controller.showChip(group)
    #expect(controller.presentedCard === surface)
    #expect(surface.isShown)
    // Group and overview both have nil agent selection: destination still differs.
    controller.showOverview()
    #expect(controller.presentedCard === surface)
    #expect(surface.isShown)
    controller.showOverview()
    #expect(!surface.isShown)
}
