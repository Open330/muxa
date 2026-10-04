import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// The editor tab being dragged, and where it would land, shared by every
/// group's tab strip and editor so each can draw the drop indicator VS Code
/// draws: an insertion bar between tabs, or the half of an editor a new group
/// would take.
///
/// SwiftUI's drag API reports neither the end of a cancelled drag nor a
/// drop's position continuously, so the session watches the mouse button
/// to know when a drag ended and the drop targets report positions.
@MainActor
final class MuxaTabDragSession: ObservableObject {
    static let shared = MuxaTabDragSession()

    struct Drag: Equatable {
        let selection: MuxaSidebarSelection
        let sourceGroupID: UUID
    }

    enum Target: Equatable {
        /// Before the tab at `index` in a group's strip.
        case strip(groupID: UUID, index: Int)
        /// The middle of a group's editor: join that group.
        case editor(groupID: UUID)
        /// An edge of a group's editor: open a group on that side.
        case split(groupID: UUID, side: MuxaWorkbenchTabs.SplitSide)
    }

    @Published private(set) var drag: Drag?
    @Published var target: Target?

    /// Marks the payload so a text drag from elsewhere is never taken for a
    /// tab.
    static let payloadPrefix = "muxa-tab:"

    private var watcher: Timer?
    private var generation: UInt64 = 0

    var isDragging: Bool { drag != nil }

    func begin(_ selection: MuxaSidebarSelection, from groupID: UUID) -> NSItemProvider {
        drag = Drag(selection: selection, sourceGroupID: groupID)
        target = nil
        watchForRelease()
        return NSItemProvider(object: "\(Self.payloadPrefix)\(selection.tabIdentifier)" as NSString)
    }

    func end() {
        watcher?.invalidate()
        watcher = nil
        drag = nil
        target = nil
    }

    /// Option held at drop time copies the tab instead of moving it, as in
    /// VS Code.
    static var copyRequested: Bool {
        NSEvent.modifierFlags.contains(.option)
    }

    /// A cancelled drag (Escape, or a drop outside any target) sends no
    /// event SwiftUI exposes; once the button is up and no drop handled the
    /// drag, it is over.
    private func watchForRelease() {
        watcher?.invalidate()
        generation &+= 1
        let current = generation
        watcher = Timer.scheduledTimer(withTimeInterval: 0.15, repeats: true) { [weak self] timer in
            guard NSEvent.pressedMouseButtons & 1 == 0 else { return }
            timer.invalidate()
            Task { @MainActor [weak self] in
                // Leave a drop that is handling the release time to run
                // first, and never end a drag that started since.
                try? await Task.sleep(for: .milliseconds(250))
                guard let self, self.generation == current else { return }
                self.end()
            }
        }
    }
}

/// Where a pointer at `x` in a strip of tabs would insert: before the first
/// tab whose middle lies right of it, or at the end.
func tabInsertionIndex(at x: CGFloat, frames: [CGRect]) -> Int {
    frames.firstIndex { x < $0.midX } ?? frames.count
}

/// The editor zone under a pointer: the outer quarter on either side splits,
/// the rest joins the group.
func editorDropZone(at x: CGFloat, width: CGFloat) -> MuxaWorkbenchTabs.SplitSide? {
    guard width > 0 else { return nil }
    let edge = min(width * 0.25, 220)
    if x < edge { return .leading }
    if x > width - edge { return .trailing }
    return nil
}

// MARK: - Tab strip

struct TabFramesKey: PreferenceKey {
    static let defaultValue: [MuxaSidebarSelection: CGRect] = [:]

    static func reduce(value: inout [MuxaSidebarSelection: CGRect], nextValue: () -> [MuxaSidebarSelection: CGRect]) {
        value.merge(nextValue()) { _, new in new }
    }
}

/// Drops on a group's tab strip: reorders within the group, moves across
/// groups. The strip's empty tail drops at the end.
struct TabStripDropDelegate: DropDelegate {
    let groupID: UUID
    let orderedFrames: [CGRect]
    let tabs: MuxaWorkbenchTabs
    let session: MuxaTabDragSession
    let didDrop: (MuxaSidebarSelection, UUID) -> Void

    func validateDrop(info: DropInfo) -> Bool {
        session.isDragging && info.hasItemsConforming(to: [.plainText])
    }

    func dropEntered(info: DropInfo) {
        update(info)
    }

    func dropUpdated(info: DropInfo) -> DropProposal? {
        update(info)
        return DropProposal(operation: MuxaTabDragSession.copyRequested ? .copy : .move)
    }

    func dropExited(info: DropInfo) {
        if case .strip(groupID, _)? = session.target {
            session.target = nil
        }
    }

    func performDrop(info: DropInfo) -> Bool {
        guard let drag = session.drag else { return false }
        let index = tabInsertionIndex(at: info.location.x, frames: orderedFrames)
        tabs.move(
            drag.selection,
            from: drag.sourceGroupID,
            to: groupID,
            at: index,
            copy: MuxaTabDragSession.copyRequested
        )
        session.end()
        didDrop(drag.selection, groupID)
        return true
    }

    private func update(_ info: DropInfo) {
        let target = MuxaTabDragSession.Target.strip(
            groupID: groupID,
            index: tabInsertionIndex(at: info.location.x, frames: orderedFrames)
        )
        if session.target != target { session.target = target }
    }
}

/// The accent bar VS Code draws where a dragged tab would be inserted.
struct TabInsertionIndicator: View {
    let x: CGFloat

    var body: some View {
        Rectangle()
            .fill(Color.accentColor)
            .frame(width: 2)
            .frame(maxHeight: .infinity)
            .offset(x: max(0, x - 1))
            .allowsHitTesting(false)
    }
}

// MARK: - Editor

/// Covers a group's editor while a tab is dragged, so a drop lands on the
/// workbench instead of the terminal underneath (Ghostty accepts text
/// drops), and shades the part of the editor the tab would take.
struct EditorDropOverlay: View {
    let groupID: UUID
    let tabs: MuxaWorkbenchTabs
    @ObservedObject var session: MuxaTabDragSession
    let didDrop: (MuxaSidebarSelection, UUID) -> Void

    var body: some View {
        if let drag = session.drag {
            GeometryReader { proxy in
                ZStack(alignment: .topLeading) {
                    highlight(in: proxy.size)
                    EditorDropTarget(
                        update: { point in update(point, width: proxy.size.width, drag: drag) },
                        perform: { point in perform(point, width: proxy.size.width, drag: drag) }
                    )
                }
            }
        }
    }

    @ViewBuilder
    private func highlight(in size: CGSize) -> some View {
        switch session.target {
        case .editor(groupID)?:
            zone.frame(width: size.width, height: size.height)
        case .split(groupID, .leading)?:
            zone.frame(width: size.width / 2, height: size.height)
        case .split(groupID, .trailing)?:
            zone.frame(width: size.width / 2, height: size.height)
                .offset(x: size.width / 2)
        default:
            EmptyView()
        }
    }

    private var zone: some View {
        Rectangle()
            .fill(Color.accentColor.opacity(0.14))
            .overlay(Rectangle().strokeBorder(Color.accentColor.opacity(0.55), lineWidth: 1.5))
            .allowsHitTesting(false)
            .animation(.easeOut(duration: 0.12), value: session.target)
    }

    private func resolve(_ point: CGPoint, width: CGFloat, drag: MuxaTabDragSession.Drag) -> MuxaTabDragSession.Target? {
        let copy = MuxaTabDragSession.copyRequested
        if let side = editorDropZone(at: point.x, width: width),
           tabs.canSplit(drag.selection, from: drag.sourceGroupID, beside: groupID, side: side, copy: copy) {
            return .split(groupID: groupID, side: side)
        }
        // Dropping a tab on its own group's editor changes nothing.
        guard drag.sourceGroupID != groupID || copy else { return nil }
        return .editor(groupID: groupID)
    }

    private func update(_ point: CGPoint?, width: CGFloat, drag: MuxaTabDragSession.Drag) -> NSDragOperation {
        guard let point else {
            if session.target?.groupID == groupID { session.target = nil }
            return []
        }
        let target = resolve(point, width: width, drag: drag)
        if session.target != target { session.target = target }
        guard target != nil else { return [] }
        return MuxaTabDragSession.copyRequested ? .copy : .move
    }

    private func perform(_ point: CGPoint, width: CGFloat, drag: MuxaTabDragSession.Drag) -> Bool {
        let copy = MuxaTabDragSession.copyRequested
        defer { session.end() }
        switch resolve(point, width: width, drag: drag) {
        case .split(_, let side)?:
            tabs.split(drag.selection, from: drag.sourceGroupID, beside: groupID, side: side, copy: copy)
        case .editor?:
            let end = tabs.group(id: groupID)?.tabs.count ?? 0
            tabs.move(drag.selection, from: drag.sourceGroupID, to: groupID, at: end, copy: copy)
        default:
            return false
        }
        didDrop(drag.selection, groupID)
        return true
    }
}

private extension MuxaTabDragSession.Target {
    var groupID: UUID {
        switch self {
        case .strip(let id, _), .editor(let id), .split(let id, _): id
        }
    }
}

/// An AppKit drop target. A SwiftUI drop target over an embedded terminal
/// loses to the terminal's own NSView, which AppKit finds first; an NSView
/// placed above it wins.
private struct EditorDropTarget: NSViewRepresentable {
    /// The pointer in top-left-origin coordinates, or nil when it left.
    let update: (CGPoint?) -> NSDragOperation
    let perform: (CGPoint) -> Bool

    func makeNSView(context: Context) -> TargetView {
        let view = TargetView()
        view.registerForDraggedTypes([.string])
        return view
    }

    func updateNSView(_ view: TargetView, context: Context) {
        view.update = update
        view.perform = perform
    }

    final class TargetView: NSView {
        var update: ((CGPoint?) -> NSDragOperation)?
        var perform: ((CGPoint) -> Bool)?

        override var isFlipped: Bool { true }

        private func point(_ info: NSDraggingInfo) -> CGPoint {
            convert(info.draggingLocation, from: nil)
        }

        override func draggingEntered(_ sender: NSDraggingInfo) -> NSDragOperation {
            update?(point(sender)) ?? []
        }

        override func draggingUpdated(_ sender: NSDraggingInfo) -> NSDragOperation {
            update?(point(sender)) ?? []
        }

        override func draggingExited(_ sender: NSDraggingInfo?) {
            _ = update?(nil)
        }

        override func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
            perform?(point(sender)) ?? false
        }
    }
}
