import Foundation
import AppKit
import SwiftUI
import Testing
@testable import Muxa

private func paletteItem(_ id: String, title: String = "demo", enabled: Bool = true) -> MuxaPaletteItem {
    MuxaPaletteItem(
        stableID: MuxaPaletteID(components: ["session", id]),
        title: title, subtitle: "host-a /tmp/tmux.sock", systemImage: "terminal",
        action: .navigate(.shell(id)), disabledReason: enabled ? nil : "Unavailable"
    )
}

@Test func paletteSearchSupportsFuzzyWordsAndDiacritics() {
    #expect(MuxaPaletteSearch.score(query: "cafe HST", text: "Café on host-a") != nil)
    #expect(MuxaPaletteSearch.score(query: "host demo", text: "demo · host-a") != nil)
    #expect(MuxaPaletteSearch.score(query: "개발", text: "개발 세션") != nil)
    #expect(MuxaPaletteSearch.score(query: "missing", text: "demo") == nil)
    #expect(MuxaPaletteSearch.score(query: "aa", text: "a") == nil)
    #expect(MuxaPaletteSearch.score(query: "", text: "") == 0)
    #expect(MuxaPaletteSearch.score(query: "a", text: "") == nil)
    #expect((MuxaPaletteSearch.score(query: "two.sock", text: "editor Window /tmp/two.sock") ?? 0)
        > (MuxaPaletteSearch.score(query: "two.sock", text: "editor Window /tmp/one.sock") ?? 0))
}

@Test func paletteRanksRecentAndPreservesSameNamedDestinations() {
    let first = paletteItem("first")
    let second = paletteItem("second")
    let results = MuxaPaletteSearch.results([first, second, first], query: "", recent: [.shell("second")])
    #expect(results == [second, first])
    #expect(MuxaPaletteSearch.results([first, second], query: "nomatch", recent: []).isEmpty)
    #expect(MuxaPaletteID(components: ["a:b", "c"]) != MuxaPaletteID(components: ["a", "b:c"]))
    #expect(MuxaPaletteID(components: [nil]) != MuxaPaletteID(components: [""]))
}

@Test func paletteExactPaneTitleOutranksRecentContextAndFuzzyMatches() {
    let exact = MuxaPaletteItem(
        stableID: MuxaPaletteID(components: ["pane", "local", "default", "%7209"]),
        title: "CAL-7209", subtitle: "Pane · local · default", systemImage: "terminal",
        action: .navigate(.pane(MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%7209")))
    )
    let context = MuxaPaletteItem(
        stableID: MuxaPaletteID(components: ["shell", "recent"]),
        title: "Other task", subtitle: "/work/CAL-7209", systemImage: "terminal", action: .navigate(.shell("recent"))
    )
    let prefix = paletteItem("prefix", title: "CAL-7209 review")
    let substring = paletteItem("substring", title: "Review CAL-7209")
    let fuzzy = paletteItem("fuzzy", title: "CAL-712039")
    let candidates = [context, fuzzy, substring, prefix, exact]
    for query in ["CAL-7209", "cal-7209", "  CAL-7209  "] {
        let results = MuxaPaletteSearch.results(candidates, query: query, recent: [.shell("recent")])
        #expect(results.map(\.id) == [exact, prefix, substring, context, fuzzy].map(\.id))
        #expect(MuxaPaletteSelection.action(results.first?.id, in: results) == exact.action)
    }
    let results = MuxaPaletteSearch.results(candidates, query: "CAL-7209", recent: [])
    #expect(MuxaPaletteSelection.updated(context.id, previousQuery: "CAL-720", query: "CAL-7209", in: results) == exact.id)
    #expect(MuxaPaletteSelection.updated(context.id, previousQuery: "CAL-7209", query: "CAL-7209", in: results) == context.id)
}

@Test func paletteCAL7209SelectsExactPaneInsteadOfItsWindowOrSibling() {
    let pane = MuxaPaletteItem(
        stableID: MuxaPaletteID(components: ["pane", "rtzr", "default", "%78"]),
        title: "cal-7209", subtitle: "/home/june/workspace-agent/cal-7209", systemImage: "terminal",
        action: .navigate(.pane(MuxaWatchPaneIdentity(hostAlias: "rtzr", socket: "default", paneID: "%78")))
    )
    let window = MuxaPaletteItem(
        stableID: MuxaPaletteID(components: ["window", "rtzr", "default", "@39"]),
        title: "CAL-7209", subtitle: "rtzr · default", systemImage: "macwindow",
        action: .navigate(.fleetWindow(MuxaWatchWindowIdentity(hostAlias: "rtzr", socket: "default", sessionID: "$5", windowID: "@39")))
    )
    let sibling = paletteItem("sibling", title: "✳ CAL-7209 callabo-resolve finalize 예외 경로")
    let results = MuxaPaletteSearch.results([window, sibling, pane], query: "CAL-7209", recent: [])
    #expect(results.map(\.id) == [pane, window, sibling].map(\.id))
}

@Test func paletteSelectionSkipsDisabledAndWraps() {
    let first = paletteItem("first")
    let disabled = paletteItem("disabled", enabled: false)
    let last = paletteItem("last")
    let items = [first, disabled, last]
    #expect(MuxaPaletteSelection.retained(nil, in: items) == first.id)
    #expect(MuxaPaletteSelection.retained(last.id, in: items) == last.id)
    #expect(MuxaPaletteSelection.retained(disabled.id, in: items) == first.id)
    #expect(MuxaPaletteSelection.moved(first.id, by: 1, in: items) == last.id)
    #expect(MuxaPaletteSelection.moved(last.id, by: 1, in: items) == first.id)
    #expect(MuxaPaletteSelection.moved(first.id, by: -1, in: items) == last.id)
    #expect(MuxaPaletteSelection.moved(nil, by: -1, in: items) == last.id)
    #expect(MuxaPaletteSelection.action(disabled.id, in: items) == nil)
    #expect(MuxaPaletteSelection.action(last.id, in: items) == last.action)
    #expect(MuxaPaletteSelection.action(last.id, in: [first]) == nil)
    #expect(MuxaPaletteSelection.retained(first.id, in: [disabled]) == nil)
    #expect(MuxaPaletteSelection.moved(first.id, by: 1, in: []) == nil)
}

@Test func paletteKeysRespectInputMethodComposition() {
    let keys: [(String, MuxaPaletteKeyDecision)] = [
        ("moveUp:", .previous), ("insertBacktab:", .previous),
        ("moveDown:", .next), ("insertTab:", .next),
        ("insertNewline:", .submit), ("insertNewlineIgnoringFieldEditor:", .submit),
        ("cancelOperation:", .cancel),
    ]
    for (selector, decision) in keys {
        #expect(MuxaPaletteKeyDecision.resolve(selector: selector, hasMarkedText: false) == decision)
        #expect(MuxaPaletteKeyDecision.resolve(selector: selector, hasMarkedText: true) == .passThrough)
        #expect(MuxaPaletteKeyDecision.resolve(selector: selector, hasMarkedText: false, beganWithMarkedText: true) == .passThrough)
    }
    #expect(MuxaPaletteKeyDecision.resolve(selector: "moveLeft:", hasMarkedText: false) == .passThrough)
}

@Test func paletteDestinationsKeepHostSocketSessionAndWindowIdentity() throws {
    let host = try JSONDecoder().decode(MuxaFleetHost.self, from: Data(
        #"{"alias":"local","local":true,"mode":"control","state":"online"}"#.utf8
    ))
    let sessions = ["/tmp/one.sock", "/tmp/two.sock"].map { socket in
        MuxaWatchSession(
            hostAlias: "local", socket: socket, sessionID: "$1", name: "demo",
            windows: [MuxaWatchWindow(
                hostAlias: "local", socket: socket, sessionID: "$1", windowID: "@1",
                name: "editor", index: "0", panes: []
            )]
        )
    }
    let items = MuxaPaletteItems.navigation(
        watchSections: [MuxaWatchHost(host: host, sessions: sessions)],
        workGroups: [], sessions: [], agents: [], localSocket: "/tmp/muxa.sock"
    )
    #expect(items.count == 4)
    #expect(Set(items.map(\.id)).count == 4)
    let results = MuxaPaletteSearch.results(items, query: "editor two.sock", recent: [])
    #expect(results.count == 2)
    #expect(results.first?.action == .navigate(.fleetWindow(sessions[1].windows[0].identity)))
    #expect(MuxaPaletteSearch.results(items, query: "demo", recent: []).count == 4)
}

@Test func paletteModeShortcutsRequireExactCommandModifiers() {
    #expect(MuxaPaletteMode.shortcut(characters: "p", modifiers: .command) == .navigation)
    #expect(MuxaPaletteMode.shortcut(characters: "P", modifiers: [.command, .shift]) == .commands)
    #expect(MuxaPaletteMode.shortcut(characters: "p", modifiers: [.command, .capsLock]) == .navigation)
    #expect(MuxaPaletteMode.shortcut(characters: "p", modifiers: []) == nil)
    #expect(MuxaPaletteMode.shortcut(characters: "p", modifiers: [.command, .option]) == nil)
    #expect(MuxaPaletteMode.shortcut(characters: "q", modifiers: .command) == nil)
}

@Test(arguments: [false, true]) @MainActor
func paletteViewSelectsNewBestMatchAndRetainsManualSelectionOnRefresh(refresh: Bool) async throws {
    func snapshot(title: String) throws -> MuxaExecutionSnapshot {
        let panes: [[String: String]] = ["%1": "CAL-7200", "%2": title].sorted { $0.key < $1.key }.map { id, title in
            ["pane_id": id, "title": title, "session_id": "$1", "session": "demo",
             "window_id": "@1", "window_name": "CAL-7209", "window_index": "0", "pane_index": id,
             "current_command": "codex", "current_path": "/tmp/CAL-7209", "socket": "default"]
        }
        let host = try JSONDecoder().decode(MuxaFleetHost.self, from: JSONSerialization.data(withJSONObject: [
            "alias": "local", "local": true, "mode": "control", "state": "online",
            "remote": ["agents": [], "panes": panes],
        ]))
        return MuxaExecutionSnapshot(hosts: [host])
    }
    let model = AppModel()
    model.ingestExecutionSnapshotForTesting(try snapshot(title: "CAL-7209"))
    var chosen: MuxaPaletteAction?
    let previous = MuxaSidebarSelection.pane(MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%1"))
    let hosting = NSHostingView(rootView: CommandPaletteView(
        model: model, mode: .navigation, recent: [previous], onChoose: { chosen = $0 }, onCancel: {}
    ))
    let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 680, height: 430), styleMask: [.titled], backing: .buffered, defer: false)
    window.isReleasedWhenClosed = false
    window.contentView = hosting
    window.makeKeyAndOrderFront(nil)
    defer { window.close() }
    try await Task.sleep(for: .milliseconds(150))
    func field(in view: NSView) -> NSTextField? {
        if let field = view as? MuxaPaletteTextField { return field }
        return view.subviews.lazy.compactMap { field(in: $0) }.first
    }
    let editor = try #require(field(in: hosting)?.currentEditor() as? NSTextView)
    editor.insertText("CAL", replacementRange: NSRange(location: NSNotFound, length: 0))
    try await Task.sleep(for: .milliseconds(40))
    editor.insertText("-7209", replacementRange: NSRange(location: NSNotFound, length: 0))
    try await Task.sleep(for: .milliseconds(40))
    if refresh {
        let tab = try #require(NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
            windowNumber: window.windowNumber, context: nil, characters: "\t", charactersIgnoringModifiers: "\t", isARepeat: false, keyCode: 48))
        window.sendEvent(tab)
        model.ingestExecutionSnapshotForTesting(try snapshot(title: "cal-7209"))
        try await Task.sleep(for: .milliseconds(40))
    }
    let enter = try #require(NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
        windowNumber: window.windowNumber, context: nil, characters: "\r", charactersIgnoringModifiers: "\r", isARepeat: false, keyCode: 36))
    window.sendEvent(enter)
    let expected: MuxaSidebarSelection = refresh
        ? .fleetWindow(MuxaWatchWindowIdentity(hostAlias: "local", socket: "default", sessionID: "$1", windowID: "@1"))
        : .pane(MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%2"))
    #expect(chosen == .navigate(expected))
}

@Test @MainActor func paletteNativeSearchAcceptsFocusTypingAndModeShortcuts() async throws {
    var query = ""
    var selectedMode: MuxaPaletteMode?
    var decisions: [MuxaPaletteKeyDecision] = []
    let hosting = NSHostingView(rootView: MuxaPaletteSearchField(
        text: Binding(get: { query }, set: { query = $0 }), placeholder: "Search",
        onKey: { decisions.append($0) }, onModeChange: { selectedMode = $0 }
    ))
    let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 680, height: 70), styleMask: [.titled], backing: .buffered, defer: false)
    let parent = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 900, height: 500), styleMask: [.titled], backing: .buffered, defer: false)
    window.isReleasedWhenClosed = false
    parent.isReleasedWhenClosed = false
    window.contentView = hosting
    parent.makeKeyAndOrderFront(nil)
    parent.beginSheet(window, completionHandler: { _ in })
    defer { parent.endSheet(window); window.close(); parent.close() }
    try await Task.sleep(for: .milliseconds(350))
    func searchField(in view: NSView) -> NSTextField? {
        if let field = view as? NSTextField { return field }
        return view.subviews.lazy.compactMap { searchField(in: $0) }.first
    }
    let field = try #require(searchField(in: hosting))
    #expect(field.isEditable)
    #expect(field.isSelectable)
    #expect(field.acceptsFirstResponder)
    let editor = try #require(field.currentEditor() as? NSTextView)
    #expect(window.firstResponder === editor)
    editor.insertText("session-search", replacementRange: NSRange(location: NSNotFound, length: 0))
    #expect(editor.string == "session-search")
    #expect(query == "session-search")
    for (characters, keyCode, modifiers, expected): (String, UInt16, NSEvent.ModifierFlags, MuxaPaletteKeyDecision) in [
        ("\t", 48, [], .next), ("\u{19}", 48, .shift, .previous),
        ("\r", 36, [], .submit), ("\u{1b}", 53, [], .cancel),
    ] {
        let event = try #require(NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: modifiers, timestamp: 0,
            windowNumber: window.windowNumber, context: nil, characters: characters,
            charactersIgnoringModifiers: characters, isARepeat: false, keyCode: keyCode
        ))
        window.sendEvent(event)
        #expect(decisions.last == expected)
        #expect(window.firstResponder === editor)
    }
    for (modifiers, expected): (NSEvent.ModifierFlags, MuxaPaletteMode) in [(.command, .navigation), ([.command, .shift], .commands)] {
        let event = try #require(NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: modifiers, timestamp: 0,
            windowNumber: window.windowNumber, context: nil, characters: "p",
            charactersIgnoringModifiers: "p", isARepeat: false, keyCode: 35
        ))
        #expect(window.performKeyEquivalent(with: event))
        #expect(selectedMode == expected)
    }
}
