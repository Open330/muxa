import SwiftUI

/// VS Code-style editor state, deliberately separate from the sidebar view
/// container. A sidebar click opens a preview in the focused editor group;
/// tab activation does not change which Activity Bar container is visible.
@MainActor
final class MuxaWorkbenchTabs: ObservableObject {
    struct Group: Codable, Identifiable, Equatable {
        let id: UUID
        var tabs: [MuxaSidebarSelection]
        var active: MuxaSidebarSelection?
        var preview: MuxaSidebarSelection?
        var history: [MuxaSidebarSelection]
    }

    @Published private(set) var groups: [Group] {
        didSet { persist() }
    }
    @Published private(set) var focusedGroupID: UUID {
        didSet { persist() }
    }
    /// Editors closed this session, most recent last, for ⇧⌘T. Not persisted:
    /// a reopen after relaunch would bring back tabs the operator already
    /// chose to close.
    private(set) var recentlyClosed: [MuxaSidebarSelection] = []
    private let persistenceKey: String?
    private let defaults: UserDefaults

    init(
        initial: MuxaSidebarSelection? = .workBoard,
        persistenceKey: String? = "muxa.workbench.tabs.v1",
        defaults: UserDefaults = .standard
    ) {
        self.persistenceKey = persistenceKey
        self.defaults = defaults
        if let persistenceKey,
           let data = defaults.data(forKey: persistenceKey),
           let snapshot = try? JSONDecoder().decode(Snapshot.self, from: data),
           !snapshot.groups.isEmpty,
           snapshot.groups.contains(where: { $0.id == snapshot.focusedGroupID }) {
            groups = snapshot.groups
            focusedGroupID = snapshot.focusedGroupID
            return
        }
        let id = UUID()
        let tabs = initial.map { [$0] } ?? []
        groups = [
            Group(
                id: id,
                tabs: tabs,
                active: initial,
                preview: nil,
                history: tabs
            ),
        ]
        focusedGroupID = id
    }

    var focusedSelection: MuxaSidebarSelection? {
        group(id: focusedGroupID)?.active
    }

    func group(id: UUID) -> Group? {
        groups.first { $0.id == id }
    }

    func openPreview(_ selection: MuxaSidebarSelection) {
        open(selection, preview: true, groupID: focusedGroupID)
    }

    func openPinned(_ selection: MuxaSidebarSelection, groupID: UUID? = nil) {
        open(selection, preview: false, groupID: groupID ?? focusedGroupID)
    }

    func activate(_ selection: MuxaSidebarSelection, groupID: UUID) {
        guard let index = groups.firstIndex(where: { $0.id == groupID }),
              groups[index].tabs.contains(selection) else { return }
        groups[index].active = selection
        touch(selection, in: &groups[index])
        focusedGroupID = groupID
    }

    func focus(_ groupID: UUID) {
        guard groups.contains(where: { $0.id == groupID }) else { return }
        focusedGroupID = groupID
    }

    @discardableResult
    func activateAt(_ index: Int) -> MuxaSidebarSelection? {
        guard let group = group(id: focusedGroupID),
              group.tabs.indices.contains(index) else { return nil }
        let selection = group.tabs[index]
        activate(selection, groupID: group.id)
        return selection
    }

    @discardableResult
    func activateLast() -> MuxaSidebarSelection? {
        guard let group = group(id: focusedGroupID) else { return nil }
        return activateAt(group.tabs.count - 1)
    }

    @discardableResult
    func focusRelativeGroup(_ offset: Int) -> MuxaSidebarSelection? {
        guard !groups.isEmpty,
              let current = groups.firstIndex(where: { $0.id == focusedGroupID }) else { return nil }
        let count = groups.count
        let nextIndex = (current + offset % count + count) % count
        focus(groups[nextIndex].id)
        return focusedSelection
    }

    @discardableResult
    func activateRelative(_ offset: Int) -> MuxaSidebarSelection? {
        guard let group = group(id: focusedGroupID), !group.tabs.isEmpty else { return nil }
        let current = group.active.flatMap { group.tabs.firstIndex(of: $0) } ?? 0
        let count = group.tabs.count
        let nextIndex = (current + offset % count + count) % count
        let next = group.tabs[nextIndex]
        activate(next, groupID: group.id)
        return next
    }

    @discardableResult
    func closeFocused() -> MuxaSidebarSelection? {
        guard let focused = focusedSelection else { return nil }
        return close(focused, groupID: focusedGroupID)
    }

    func pin(_ selection: MuxaSidebarSelection, groupID: UUID) {
        guard let index = groups.firstIndex(where: { $0.id == groupID }) else { return }
        if groups[index].preview == selection {
            groups[index].preview = nil
        }
    }

    @discardableResult
    func close(_ selection: MuxaSidebarSelection, groupID: UUID) -> MuxaSidebarSelection? {
        guard let group = group(id: groupID), group.tabs.contains(selection) else {
            return focusedSelection
        }
        recentlyClosed.removeAll { $0 == selection }
        recentlyClosed.append(selection)
        if recentlyClosed.count > 20 { recentlyClosed.removeFirst() }
        detach(selection, from: groupID)
        return focusedSelection
    }

    /// Takes a tab out of a group without recording it as closed: the
    /// shared half of closing a tab and dragging it to another group. A
    /// group left empty goes away unless it is the last one.
    private func detach(_ selection: MuxaSidebarSelection, from groupID: UUID) {
        guard let groupIndex = groups.firstIndex(where: { $0.id == groupID }),
              let tabIndex = groups[groupIndex].tabs.firstIndex(of: selection) else { return }

        groups[groupIndex].tabs.remove(at: tabIndex)
        groups[groupIndex].history.removeAll { $0 == selection }
        if groups[groupIndex].preview == selection {
            groups[groupIndex].preview = nil
        }
        if groups[groupIndex].active == selection {
            groups[groupIndex].active = groups[groupIndex].history.last(where: {
                groups[groupIndex].tabs.contains($0)
            }) ?? groups[groupIndex].tabs[safe: min(tabIndex, groups[groupIndex].tabs.count - 1)]
        }

        if groups[groupIndex].tabs.isEmpty, groups.count > 1 {
            groups.remove(at: groupIndex)
            if focusedGroupID == groupID {
                let fallbackIndex = min(groupIndex, groups.count - 1)
                focusedGroupID = groups[fallbackIndex].id
            }
        }
    }

    /// ⇧⌘T: reopens the most recently closed editor that still exists and is
    /// not already open in the focused group, pinned there.
    @discardableResult
    func reopenClosed(
        isAvailable: (MuxaSidebarSelection) -> Bool
    ) -> MuxaSidebarSelection? {
        let openTabs = group(id: focusedGroupID)?.tabs ?? []
        while let selection = recentlyClosed.popLast() {
            guard isAvailable(selection), !openTabs.contains(selection) else { continue }
            openPinned(selection)
            return selection
        }
        return nil
    }

    @discardableResult
    func closeEverywhere(_ selection: MuxaSidebarSelection) -> MuxaSidebarSelection? {
        for group in groups where group.tabs.contains(selection) {
            close(selection, groupID: group.id)
        }
        return focusedSelection
    }

    func closeOthers(keeping selection: MuxaSidebarSelection, groupID: UUID) {
        guard let index = groups.firstIndex(where: { $0.id == groupID }),
              groups[index].tabs.contains(selection) else { return }
        groups[index].tabs = [selection]
        groups[index].active = selection
        groups[index].preview = nil
        groups[index].history = [selection]
        focusedGroupID = groupID
    }

    /// Muxa currently keeps at most two editor groups so each terminal remains
    /// usable at the app's supported minimum window width.
    static let maxGroups = 2

    @discardableResult
    func splitRight(selection: MuxaSidebarSelection, from groupID: UUID) -> UUID {
        if groups.count >= Self.maxGroups,
           let other = groups.first(where: { $0.id != groupID }) {
            open(selection, preview: false, groupID: other.id)
            return other.id
        }
        let newID = UUID()
        let newGroup = Group(
            id: newID,
            tabs: [selection],
            active: selection,
            preview: nil,
            history: [selection]
        )
        let insertion = groups.firstIndex(where: { $0.id == groupID }).map { $0 + 1 }
            ?? groups.endIndex
        groups.insert(newGroup, at: insertion)
        focusedGroupID = newID
        return newID
    }

    func move(tabIdentifier: String, before target: MuxaSidebarSelection, groupID: UUID) {
        guard let group = group(id: groupID),
              let moved = group.tabs.first(where: { $0.tabIdentifier == tabIdentifier }),
              let targetIndex = group.tabs.firstIndex(of: target) else { return }
        move(moved, from: groupID, to: groupID, at: targetIndex)
    }

    /// Which side of a group a dragged tab opens a new group on.
    enum SplitSide: Equatable {
        case leading
        case trailing
    }

    /// Drops a dragged tab into a group's strip before the tab at `index`
    /// (`tabs.count` for the end), the way VS Code does: within a group it
    /// reorders, across groups it moves (or, with `copy`, opens a second
    /// copy). The moved tab becomes the target's active, kept-open tab, and a
    /// source group left empty closes.
    func move(
        _ selection: MuxaSidebarSelection,
        from sourceID: UUID,
        to targetID: UUID,
        at index: Int,
        copy: Bool = false
    ) {
        guard group(id: sourceID)?.tabs.contains(selection) == true,
              group(id: targetID) != nil else { return }
        if sourceID != targetID, !copy {
            detach(selection, from: sourceID)
        }
        insert(selection, into: targetID, at: index)
    }

    /// Drops a dragged tab on the leading or trailing edge of a group's
    /// editor. With room for another group it opens one on that side;
    /// otherwise the tab joins the neighbouring group on that side.
    func split(
        _ selection: MuxaSidebarSelection,
        from sourceID: UUID,
        beside targetID: UUID,
        side: SplitSide,
        copy: Bool = false
    ) {
        guard canSplit(selection, from: sourceID, beside: targetID, side: side, copy: copy),
              let targetIndex = groups.firstIndex(where: { $0.id == targetID }) else { return }
        let neighbourIndex = side == .leading ? targetIndex - 1 : targetIndex + 1
        // A group's only tab dropped on its own edge: the group would close
        // under it, so the neighbour is the only place to go.
        let targetCloses = sourceID == targetID && !copy && group(id: sourceID)?.tabs == [selection]
        if groups.indices.contains(neighbourIndex),
           targetCloses || projectedGroupCount(selection, from: sourceID, copy: copy) >= Self.maxGroups {
            let neighbour = groups[neighbourIndex]
            move(selection, from: sourceID, to: neighbour.id, at: neighbour.tabs.count, copy: copy)
            return
        }
        if !copy { detach(selection, from: sourceID) }
        // Detaching may have closed the source group and shifted indexes.
        guard let anchor = groups.firstIndex(where: { $0.id == targetID }) else { return }
        let newGroup = Group(
            id: UUID(),
            tabs: [selection],
            active: selection,
            preview: nil,
            history: [selection]
        )
        groups.insert(newGroup, at: side == .leading ? anchor : anchor + 1)
        focusedGroupID = newGroup.id
    }

    /// Whether dropping on `side` of `targetID` does anything: a lone tab
    /// cannot split off its own group, and at the group limit a side with no
    /// neighbouring group has nowhere to go.
    func canSplit(
        _ selection: MuxaSidebarSelection,
        from sourceID: UUID,
        beside targetID: UUID,
        side: SplitSide,
        copy: Bool = false
    ) -> Bool {
        guard let source = group(id: sourceID), source.tabs.contains(selection),
              let targetIndex = groups.firstIndex(where: { $0.id == targetID }) else { return false }
        let neighbourIndex = side == .leading ? targetIndex - 1 : targetIndex + 1
        let hasNeighbour = groups.indices.contains(neighbourIndex)
        if sourceID == targetID, source.tabs.count == 1, !copy {
            // Splitting would leave this group empty: only a move into the
            // neighbour is meaningful.
            return hasNeighbour
        }
        if projectedGroupCount(selection, from: sourceID, copy: copy) < Self.maxGroups { return true }
        guard hasNeighbour else { return false }
        // Moving into the neighbour is a no-op when the neighbour is the source.
        return groups[neighbourIndex].id != sourceID || copy
    }

    /// The group count once the dragged tab leaves its source.
    private func projectedGroupCount(_ selection: MuxaSidebarSelection, from sourceID: UUID, copy: Bool) -> Int {
        guard !copy, let source = group(id: sourceID),
              source.tabs == [selection], groups.count > 1 else { return groups.count }
        return groups.count - 1
    }

    private func insert(_ selection: MuxaSidebarSelection, into groupID: UUID, at index: Int) {
        guard let groupIndex = groups.firstIndex(where: { $0.id == groupID }) else { return }
        var position = min(max(index, 0), groups[groupIndex].tabs.count)
        if let existing = groups[groupIndex].tabs.firstIndex(of: selection) {
            groups[groupIndex].tabs.remove(at: existing)
            if existing < position { position -= 1 }
        }
        groups[groupIndex].tabs.insert(selection, at: position)
        // A tab the operator placed by hand is not a preview any more.
        if groups[groupIndex].preview == selection {
            groups[groupIndex].preview = nil
        }
        groups[groupIndex].active = selection
        touch(selection, in: &groups[groupIndex])
        focusedGroupID = groupID
    }

    func prune(where isAvailable: (MuxaSidebarSelection) -> Bool) {
        for index in groups.indices.reversed() {
            groups[index].tabs.removeAll { !isAvailable($0) }
            groups[index].history.removeAll { !isAvailable($0) }
            if let preview = groups[index].preview, !isAvailable(preview) {
                groups[index].preview = nil
            }
            if let active = groups[index].active, !isAvailable(active) {
                groups[index].active = groups[index].history.last
                    ?? groups[index].tabs.first
            }
            if groups[index].tabs.isEmpty, groups.count > 1 {
                let removedID = groups[index].id
                groups.remove(at: index)
                if focusedGroupID == removedID {
                    focusedGroupID = groups[0].id
                }
            }
        }
    }

    private func open(
        _ selection: MuxaSidebarSelection,
        preview: Bool,
        groupID: UUID
    ) {
        guard let index = groups.firstIndex(where: { $0.id == groupID }) else { return }
        if groups[index].tabs.contains(selection) {
            groups[index].active = selection
            if !preview, groups[index].preview == selection {
                groups[index].preview = nil
            }
            touch(selection, in: &groups[index])
            focusedGroupID = groupID
            return
        }

        if preview,
           let previous = groups[index].preview,
           let previewIndex = groups[index].tabs.firstIndex(of: previous) {
            groups[index].tabs[previewIndex] = selection
            groups[index].history.removeAll { $0 == previous }
            groups[index].preview = selection
        } else {
            groups[index].tabs.append(selection)
            if preview { groups[index].preview = selection }
        }
        groups[index].active = selection
        touch(selection, in: &groups[index])
        focusedGroupID = groupID
    }

    private func touch(_ selection: MuxaSidebarSelection, in group: inout Group) {
        group.history.removeAll { $0 == selection }
        group.history.append(selection)
    }

    private func persist() {
        guard let persistenceKey else { return }
        let snapshot = Snapshot(groups: groups, focusedGroupID: focusedGroupID)
        guard let data = try? JSONEncoder().encode(snapshot) else { return }
        defaults.set(data, forKey: persistenceKey)
    }

    private struct Snapshot: Codable {
        let groups: [Group]
        let focusedGroupID: UUID
    }
}

extension MuxaSidebarSelection {
    var tabIdentifier: String {
        switch self {
        case .file(let location):
            "file:\(location.id)"
        case .workBoard:
            "work-board"
        case .watch:
            "watch"
        case .inbox:
            "inbox"
        case .ask:
            "ask"
        case .work(let identity):
            "work:\(identity.workspaceID):\(identity.workID)"
        case .agent(let id):
            "agent:\(id)"
        case .host(let id):
            "host:\(id)"
        case .fleetSession(let id):
            "fleet-session:\(id.hostAlias):\(id.socket):\(id.sessionID)"
        case .fleetWindow(let id):
            "fleet-window:\(id.hostAlias):\(id.socket):\(id.sessionID):\(id.windowID)"
        case .shell(let id):
            "shell:\(id)"
        case .pane(let identity):
            "pane:\(identity.hostAlias):\(identity.socket):\(identity.paneID)"
        }
    }

    var moduleRoute: MuxaModuleRoute? {
        switch self {
        case .shell(let id): .shell(id)
        case .pane(let id): .fleetPane(id)
        default: nil
        }
    }
}

private extension Array {
    subscript(safe index: Int) -> Element? {
        indices.contains(index) ? self[index] : nil
    }
}

struct MuxaEditorCommandActions {
    var close: (() -> Void)? = nil
    var next: (() -> Void)? = nil
    var previous: (() -> Void)? = nil
    var splitRight: (() -> Void)? = nil
    var pin: (() -> Void)? = nil
    var quickOpen: (() -> Void)? = nil
    var commandPalette: (() -> Void)? = nil
    var activateAt: ((Int) -> Void)? = nil
    var activateLast: (() -> Void)? = nil
    var focusRelativeGroup: ((Int) -> Void)? = nil
    var openWorkCommandCenter: (() -> Void)? = nil
    var openAsk: (() -> Void)? = nil
    var openInbox: (() -> Void)? = nil
    var selectSidebar: ((MuxaSidebarMode) -> Void)? = nil
    var focusSidebar: (() -> Void)? = nil
    var jumpToAgent: (() -> Void)? = nil
    var nextAttention: (() -> Void)? = nil
    var toggleSidebar: (() -> Void)? = nil
    var reopenClosed: (() -> Void)? = nil
    var showShortcuts: (() -> Void)? = nil
    var markAllRead: (() -> Void)? = nil // WS-A
    var showChanges: (() -> Void)? = nil // WS-D
    var findInFile: (() -> Void)? = nil
    var openFile: (() -> Void)? = nil
    var showFiles: (() -> Void)? = nil
    var isEnabled: Bool = true
}

private struct MuxaEditorCommandActionsKey: FocusedValueKey {
    typealias Value = MuxaEditorCommandActions
}

extension FocusedValues {
    var muxaEditorCommands: MuxaEditorCommandActions? {
        get { self[MuxaEditorCommandActionsKey.self] }
        set { self[MuxaEditorCommandActionsKey.self] = newValue }
    }
}
