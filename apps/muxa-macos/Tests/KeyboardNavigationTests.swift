import AppKit
import Testing
@testable import Muxa

@Test @MainActor func keyboardTabActivationUsesZeroBasedVisualOrder() {
    let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
    tabs.openPinned(.ask)
    tabs.openPreview(.inbox)
    let groupID = tabs.focusedGroupID

    #expect(tabs.activateAt(0) == .workBoard)
    #expect(tabs.activateAt(1) == .ask)
    #expect(tabs.group(id: groupID)?.history.last == .ask)
    #expect(tabs.activateLast() == .inbox)
    #expect(tabs.group(id: groupID)?.preview == .inbox)

    let before = tabs.groups
    #expect(tabs.activateAt(-1) == nil)
    #expect(tabs.activateAt(3) == nil)
    #expect(tabs.activateAt(Int.max) == nil)
    #expect(tabs.groups == before)

    tabs.move(tabIdentifier: MuxaSidebarSelection.inbox.tabIdentifier, before: .workBoard, groupID: groupID)
    #expect(tabs.activateAt(0) == .inbox)
    #expect(tabs.activateLast() == .ask)
}

@Test @MainActor func keyboardTabActivationStaysInFocusedGroup() {
    let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
    tabs.openPinned(.ask)
    let firstID = tabs.focusedGroupID
    let secondID = tabs.splitRight(selection: .inbox, from: firstID)

    #expect(tabs.activateAt(0) == .inbox)
    #expect(tabs.activateAt(1) == nil)
    #expect(tabs.activateLast() == .inbox)
    #expect(tabs.focusedGroupID == secondID)
    #expect(tabs.group(id: firstID)?.active == .ask)
}

@Test @MainActor func keyboardNavigationHandlesEmptyWorkbench() {
    let tabs = MuxaWorkbenchTabs(initial: nil, persistenceKey: nil)
    let before = tabs.groups
    let groupID = tabs.focusedGroupID

    #expect(tabs.activateAt(0) == nil)
    #expect(tabs.activateLast() == nil)
    #expect(tabs.activateRelative(1) == nil)
    #expect(tabs.activateRelative(-1) == nil)
    #expect(tabs.focusRelativeGroup(1) == nil)
    #expect(tabs.focusRelativeGroup(-1) == nil)
    #expect(tabs.groups == before)
    #expect(tabs.focusedGroupID == groupID)
}

@Test @MainActor func keyboardRelativeEditorNavigationWrapsWithoutClosingSessions() {
    let tabs = MuxaWorkbenchTabs(initial: .shell("first"), persistenceKey: nil)
    tabs.openPinned(.shell("second"))
    tabs.openPinned(.shell("third"))

    #expect(tabs.activateRelative(1) == .shell("first"))
    #expect(tabs.activateRelative(-1) == .shell("third"))
    #expect(tabs.activateRelative(4) == .shell("first"))
    #expect(tabs.activateRelative(-4) == .shell("third"))
    #expect(tabs.activateRelative(0) == .shell("third"))
    #expect(tabs.groups[0].tabs == [.shell("first"), .shell("second"), .shell("third")])
}

@Test @MainActor func keyboardGroupNavigationWrapsAndPreservesEditorState() {
    let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
    tabs.openPreview(.ask)
    let firstID = tabs.focusedGroupID
    let secondID = tabs.splitRight(selection: .shell("local"), from: firstID)
    let before = tabs.groups

    #expect(tabs.focusRelativeGroup(1) == .ask)
    #expect(tabs.focusedGroupID == firstID)
    #expect(tabs.focusRelativeGroup(-1) == .shell("local"))
    #expect(tabs.focusedGroupID == secondID)
    #expect(tabs.focusRelativeGroup(0) == .shell("local"))
    #expect(tabs.focusRelativeGroup(Int.max) == .ask)
    #expect(tabs.focusRelativeGroup(Int.min) == .ask)
    #expect(tabs.groups == before)
}

@Test @MainActor func keyboardGroupNavigationCanFocusEmptyGroup() {
    let tabs = MuxaWorkbenchTabs(initial: nil, persistenceKey: nil)
    let emptyID = tabs.focusedGroupID
    let populatedID = tabs.splitRight(selection: .inbox, from: emptyID)

    #expect(tabs.focusRelativeGroup(-1) == nil)
    #expect(tabs.focusedGroupID == emptyID)
    #expect(tabs.activateLast() == nil)
    #expect(tabs.focusRelativeGroup(1) == .inbox)
    #expect(tabs.focusedGroupID == populatedID)
}

@Test @MainActor func keyboardNavigationSkipsPrunedGroups() {
    let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
    let firstID = tabs.focusedGroupID
    let removedID = tabs.splitRight(selection: .inbox, from: firstID)
    tabs.prune { $0 != .inbox }

    #expect(tabs.group(id: removedID) == nil)
    #expect(tabs.focusedGroupID == firstID)
    tabs.focus(removedID)
    #expect(tabs.focusRelativeGroup(1) == .workBoard)
    #expect(tabs.focusRelativeGroup(-1) == .workBoard)
    #expect(tabs.activateAt(0) == .workBoard)
    #expect(tabs.activateLast() == .workBoard)

    tabs.prune { _ in false }
    #expect(tabs.groups.count == 1)
    #expect(tabs.focusRelativeGroup(1) == nil)
    #expect(tabs.activateLast() == nil)
}

@Test func keyboardCommandActionsPreserveExistingInitializer() {
    var invoked: [String] = []
    let actions = MuxaEditorCommandActions(
        close: { invoked.append("close") },
        next: { invoked.append("next") },
        previous: { invoked.append("previous") },
        splitRight: { invoked.append("split") },
        pin: { invoked.append("pin") }
    )
    actions.next?()
    actions.previous?()
    actions.splitRight?()
    actions.pin?()
    #expect(invoked == ["next", "previous", "split", "pin"])
    #expect(actions.quickOpen == nil)
    #expect(actions.commandPalette == nil)
    #expect(actions.activateAt == nil)
    #expect(actions.activateLast == nil)
    #expect(actions.focusRelativeGroup == nil)
    #expect(actions.openWorkCommandCenter == nil)
    #expect(actions.openAsk == nil)
    #expect(actions.openInbox == nil)
    #expect(actions.selectSidebar == nil)
    #expect(actions.focusSidebar == nil)
    #expect(actions.isEnabled)
    #expect(MuxaEditorCommandActions().close == nil)
}

@Test @MainActor func closingBackgroundEditorsDoesNotStealGroupFocus() {
    let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
    let firstID = tabs.focusedGroupID
    let backgroundID = tabs.splitRight(selection: .ask, from: firstID)
    tabs.openPinned(.inbox)
    tabs.focus(firstID)
    #expect(tabs.close(.ask, groupID: backgroundID) == .workBoard)
    #expect(tabs.focusedGroupID == firstID)
    #expect(tabs.close(.inbox, groupID: backgroundID) == .workBoard)
    #expect(tabs.focusedGroupID == firstID)
    #expect(tabs.groups.count == 1)
}

@Test @MainActor func closingExitedShellEverywhereReturnsSurvivingEditor() {
    let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
    let firstID = tabs.focusedGroupID
    tabs.openPinned(.shell("exited"))
    tabs.splitRight(selection: .shell("exited"), from: firstID)
    #expect(tabs.closeEverywhere(.shell("exited")) == .workBoard)
    #expect(tabs.focusedGroupID == firstID)
    #expect(tabs.groups.count == 1)
    #expect(tabs.groups[0].tabs == [.workBoard])
    #expect(tabs.closeEverywhere(.workBoard) == nil)
    #expect(tabs.groups.count == 1)
}

@Test @MainActor func editorOperationsMaintainStateInvariants() {
    let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
    let destinations: [MuxaSidebarSelection] = [.workBoard, .ask, .inbox, .watch, .shell("one"), .shell("two")]
    var seed: UInt64 = 7209
    for _ in 0..<1200 {
        seed = seed &* 6364136223846793005 &+ 1442695040888963407
        let selection = destinations[Int(seed % UInt64(destinations.count))]
        switch (seed >> 32) % 9 {
        case 0: tabs.openPreview(selection)
        case 1: tabs.openPinned(selection)
        case 2: tabs.splitRight(selection: selection, from: tabs.focusedGroupID)
        case 3: tabs.closeFocused()
        case 4: tabs.closeEverywhere(selection)
        case 5: tabs.prune { $0 != selection }
        case 6: tabs.activateRelative(1)
        case 7: tabs.focusRelativeGroup(-1)
        default: tabs.closeOthers(keeping: selection, groupID: tabs.focusedGroupID)
        }
        #expect((1...2).contains(tabs.groups.count))
        #expect(tabs.groups.contains { $0.id == tabs.focusedGroupID })
        for group in tabs.groups {
            #expect(Set(group.tabs).count == group.tabs.count)
            #expect(Set(group.history).count == group.history.count)
            #expect(group.history.allSatisfy { group.tabs.contains($0) })
            #expect(group.active.map { group.tabs.contains($0) } ?? group.tabs.isEmpty)
            #expect(group.preview.map { group.tabs.contains($0) } ?? true)
        }
    }
}

/// The terminal's own keybindings are replaced so workbench shortcuts (⌘W,
/// ⌃Tab, ⌘1–9) reach the menu bar; libghostty drops the whole configuration
/// over a single invalid line, so it must load cleanly.
@Test @MainActor func terminalKeybindingsLoadAndLeaveWorkbenchShortcutsFree() {
    #expect(MuxaTerminalKeybindings.loadIssue() == nil)
    let bound = MuxaTerminalKeybindings.bindings.map { $0.split(separator: "=")[0] }
    for shortcut in ["super+w", "ctrl+tab", "ctrl+shift+tab", "super+1", "super+comma", "super+t"] {
        #expect(!bound.contains(Substring(shortcut)))
    }
    // Line editing survives the `clear`.
    for shortcut in ["super+left", "super+right", "super+backspace", "alt+left", "alt+right"] {
        #expect(bound.contains(Substring(shortcut)))
    }
}

@Test func nextAttentionCyclesThroughWaitingPanes() {
    let a = MuxaWatchPaneIdentity(hostAlias: "local", socket: "s", paneID: "%1")
    let b = MuxaWatchPaneIdentity(hostAlias: "local", socket: "s", paneID: "%2")
    let elsewhere = MuxaWatchPaneIdentity(hostAlias: "local", socket: "s", paneID: "%9")
    #expect(MuxaAttention.next(after: nil, in: [a, b]) == a)
    #expect(MuxaAttention.next(after: a, in: [a, b]) == b)
    #expect(MuxaAttention.next(after: b, in: [a, b]) == a)
    #expect(MuxaAttention.next(after: elsewhere, in: [a, b]) == a)
    #expect(MuxaAttention.next(after: a, in: []) == nil)
}

@Test @MainActor func reopenClosedRestoresMostRecentAvailableEditor() {
    let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
    tabs.openPinned(.ask)
    tabs.openPinned(.inbox)
    tabs.closeFocused()
    tabs.close(.ask, groupID: tabs.focusedGroupID)
    #expect(tabs.reopenClosed(isAvailable: { $0 != .ask }) == .inbox)
    #expect(tabs.focusedSelection == .inbox)
    #expect(tabs.reopenClosed(isAvailable: { _ in true }) == nil)
}

@Test func paletteShortcutsSwitchModesAndJumpToResults() {
    #expect(MuxaPaletteMode.shortcut(characters: "j", modifiers: .command) == .agents)
    #expect(MuxaPaletteMode.shortcut(characters: "p", modifiers: .command) == .navigation)
    #expect(MuxaPaletteMode.shortcut(characters: "p", modifiers: [.command, .shift]) == .commands)
    #expect(MuxaPaletteMode.shortcut(characters: "j", modifiers: [.command, .shift]) == nil)
    #expect(MuxaPaletteMode.resultIndex(characters: "1", modifiers: .command) == 0)
    #expect(MuxaPaletteMode.resultIndex(characters: "9", modifiers: .command) == 8)
    #expect(MuxaPaletteMode.resultIndex(characters: "0", modifiers: .command) == nil)
    #expect(MuxaPaletteMode.resultIndex(characters: "1", modifiers: [.command, .shift]) == nil)
}

@Test func everyPaletteCommandShortcutAppearsInTheReference() {
    let listed = MuxaShortcutCatalog.sections.flatMap(\.entries).map(\.keys)
    for command in MuxaPaletteCommand.allCases {
        guard let shortcut = command.shortcut else { continue }
        #expect(listed.contains { $0.contains(shortcut) }, "\(command) \(shortcut) missing from ⌘/")
    }
}

@Test @MainActor func draggedTabReordersWithinItsGroup() {
    let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
    tabs.openPinned(.ask)
    tabs.openPinned(.inbox)
    let groupID = tabs.focusedGroupID

    // workBoard, ask, inbox → drop workBoard at the end.
    tabs.move(.workBoard, from: groupID, to: groupID, at: 3)
    #expect(tabs.group(id: groupID)?.tabs == [.ask, .inbox, .workBoard])
    #expect(tabs.group(id: groupID)?.active == .workBoard)

    // Dropping a tab just before itself or after itself changes nothing.
    tabs.move(.inbox, from: groupID, to: groupID, at: 1)
    tabs.move(.inbox, from: groupID, to: groupID, at: 2)
    #expect(tabs.group(id: groupID)?.tabs == [.ask, .inbox, .workBoard])
}

@Test @MainActor func draggedTabMovesAcrossGroupsAndClosesAnEmptySource() {
    let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
    let firstID = tabs.focusedGroupID
    tabs.openPreview(.ask)
    let secondID = tabs.splitRight(selection: .inbox, from: firstID)

    // A preview tab placed by hand is kept open.
    tabs.move(.ask, from: firstID, to: secondID, at: 0)
    #expect(tabs.group(id: firstID)?.tabs == [.workBoard])
    #expect(tabs.group(id: secondID)?.tabs == [.ask, .inbox])
    #expect(tabs.group(id: secondID)?.preview == nil)
    #expect(tabs.focusedGroupID == secondID)

    // Option-drag copies.
    tabs.move(.inbox, from: secondID, to: firstID, at: 1, copy: true)
    #expect(tabs.group(id: firstID)?.tabs == [.workBoard, .inbox])
    #expect(tabs.group(id: secondID)?.tabs == [.ask, .inbox])

    // Moving the last tab out closes its group.
    tabs.move(.workBoard, from: firstID, to: secondID, at: 3)
    tabs.move(.inbox, from: firstID, to: secondID, at: 0)
    #expect(tabs.groups.map(\.id) == [secondID])
    #expect(tabs.group(id: secondID)?.tabs == [.inbox, .ask, .workBoard])
}

@Test @MainActor func draggedTabSplitsTowardAnEditorEdge() {
    let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
    let firstID = tabs.focusedGroupID
    tabs.openPinned(.ask)

    // A lone group with one tab has nothing to split off.
    let lone = MuxaWorkbenchTabs(persistenceKey: nil)
    #expect(!lone.canSplit(.workBoard, from: lone.focusedGroupID, beside: lone.focusedGroupID, side: .trailing))

    // Dropping on the leading edge opens a group on that side.
    #expect(tabs.canSplit(.ask, from: firstID, beside: firstID, side: .leading))
    tabs.split(.ask, from: firstID, beside: firstID, side: .leading)
    #expect(tabs.groups.count == 2)
    #expect(tabs.groups[0].tabs == [.ask])
    #expect(tabs.groups[1].id == firstID)
    #expect(tabs.focusedGroupID == tabs.groups[0].id)

    // At the group limit an edge with a neighbour moves the tab there, and
    // an outer edge has nowhere to go.
    let leftID = tabs.groups[0].id
    #expect(tabs.canSplit(.workBoard, from: firstID, beside: firstID, side: .leading) == true)
    #expect(!tabs.canSplit(.workBoard, from: firstID, beside: firstID, side: .trailing))
    tabs.split(.workBoard, from: firstID, beside: firstID, side: .leading)
    #expect(tabs.groups.map(\.id) == [leftID])
    #expect(tabs.group(id: leftID)?.tabs == [.ask, .workBoard])
}

@Test func tabDropGeometryPicksInsertionAndZones() {
    let frames = [CGRect(x: 0, y: 0, width: 100, height: 30), CGRect(x: 100, y: 0, width: 100, height: 30)]
    #expect(tabInsertionIndex(at: 10, frames: frames) == 0)
    #expect(tabInsertionIndex(at: 60, frames: frames) == 1)
    #expect(tabInsertionIndex(at: 190, frames: frames) == 2)
    #expect(tabInsertionIndex(at: 500, frames: frames) == 2)
    #expect(editorDropZone(at: 40, width: 800) == .leading)
    #expect(editorDropZone(at: 400, width: 800) == nil)
    #expect(editorDropZone(at: 790, width: 800) == .trailing)
}
