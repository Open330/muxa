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
        guard let groupIndex = groups.firstIndex(where: { $0.id == groupID }),
              let tabIndex = groups[groupIndex].tabs.firstIndex(of: selection) else {
            return focusedSelection
        }

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
            let fallbackIndex = min(groupIndex, groups.count - 1)
            focusedGroupID = groups[fallbackIndex].id
        } else {
            focusedGroupID = groupID
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
    @discardableResult
    func splitRight(selection: MuxaSidebarSelection, from groupID: UUID) -> UUID {
        if groups.count >= 2,
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
        guard let groupIndex = groups.firstIndex(where: { $0.id == groupID }),
              let sourceIndex = groups[groupIndex].tabs.firstIndex(where: {
                  $0.tabIdentifier == tabIdentifier
              }),
              let targetIndex = groups[groupIndex].tabs.firstIndex(of: target),
              sourceIndex != targetIndex else { return }
        let moved = groups[groupIndex].tabs.remove(at: sourceIndex)
        let adjustedTarget = sourceIndex < targetIndex ? targetIndex - 1 : targetIndex
        groups[groupIndex].tabs.insert(moved, at: adjustedTarget)
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
        case .workBoard:
            "work-board"
        case .watch:
            "watch"
        case .ask:
            "ask"
        case .work(let identity):
            "work:\(identity.workspaceID):\(identity.workID)"
        case .agent(let id):
            "agent:\(id)"
        case .host(let id):
            "host:\(id)"
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
    let close: () -> Void
    let next: () -> Void
    let previous: () -> Void
    let splitRight: () -> Void
    let pin: () -> Void
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
