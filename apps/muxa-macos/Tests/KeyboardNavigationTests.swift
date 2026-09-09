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
