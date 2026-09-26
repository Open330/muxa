import AppKit
import SwiftUI

/// AppKit owns disclosure hit testing, selection, arrow keys and type-to-select.
struct ArtifactOutline: NSViewRepresentable {
    let model: AppModel
    let root: MuxaFileLocation
    let showHidden: Bool
    let revision: Int
    let openFile: (MuxaFileLocation, Bool) -> Void
    func makeCoordinator() -> Coordinator { Coordinator(self) }
    func makeNSView(context: Context) -> NSScrollView {
        let outline = ArtifactOutlineView()
        outline.submit = { context.coordinator.preview(pin: true) }
        let column = NSTableColumn(identifier: .init("files"))
        outline.addTableColumn(column); outline.outlineTableColumn = column
        outline.headerView = nil; outline.rowHeight = 25; outline.indentationPerLevel = 14
        outline.style = .sourceList; outline.floatsGroupRows = false
        outline.columnAutoresizingStyle = .lastColumnOnlyAutoresizingStyle
        outline.delegate = context.coordinator; outline.dataSource = context.coordinator
        outline.target = context.coordinator; outline.action = #selector(Coordinator.clicked)
        outline.doubleAction = #selector(Coordinator.doubleClicked)
        let menu = NSMenu()
        for (title, action) in [("Keep Open", #selector(Coordinator.keepOpen)), ("Copy Path", #selector(Coordinator.copyPath)), ("Open Folder", #selector(Coordinator.openFolder))] {
            let item = NSMenuItem(title: title, action: action, keyEquivalent: "")
            item.target = context.coordinator; menu.addItem(item)
        }
        outline.menu = menu
        outline.setAccessibilityLabel("File explorer")
        let scroll = NSScrollView(); scroll.documentView = outline
        scroll.hasVerticalScroller = true; scroll.drawsBackground = false
        context.coordinator.outline = outline
        context.coordinator.startPolling()
        return scroll
    }
    static func dismantleNSView(_ view: NSScrollView, coordinator: Coordinator) { coordinator.stop() }
    func updateNSView(_ view: NSScrollView, context: Context) {
        let coordinator = context.coordinator
        let changed = coordinator.owner.root != root || coordinator.owner.revision != revision || coordinator.owner.showHidden != showHidden
        if coordinator.owner.root != root { coordinator.expanded.removeAll(); coordinator.selectedLocation = nil }
        coordinator.owner = self
        if changed || !coordinator.started { coordinator.reload() }
        let active: MuxaFileLocation? = if case .file(let location) = model.sidebarSelection { location } else { nil }
        if active != coordinator.lastRevealed {
            coordinator.lastRevealed = active
            coordinator.revealTask?.cancel()
            if let active { coordinator.revealTask = Task { await coordinator.reveal(active) } }
        }
    }
    final class ArtifactOutlineView: NSOutlineView {
        var submit: (() -> Void)?
        override func keyDown(with event: NSEvent) {
            if event.keyCode == 36 { submit?() } else { super.keyDown(with: event) }
        }
    }
    @MainActor final class Node: NSObject {
        let location: MuxaFileLocation
        let directory: Bool
        let title: String
        let isStatus: Bool
        lazy var placeholder = Node(location, directory: false, title: String(localized: "Loading…"))
        var children: [Node]?
        var loading = false
        var entries: [MuxaFileEntry]?
        var truncated = false
        var failed = false
        var task: Task<Void, Never>?
        init(_ location: MuxaFileLocation, directory: Bool, title: String? = nil) {
            self.location = location; self.directory = directory; self.title = title ?? location.name; self.isStatus = title != nil
        }
    }
    @MainActor final class Coordinator: NSObject, NSOutlineViewDataSource, NSOutlineViewDelegate {
        var owner: ArtifactOutline
        weak var outline: NSOutlineView?
        var rootNode: Node?
        var expanded: Set<MuxaFileLocation> = []
        var selectedLocation: MuxaFileLocation?
        var restoring = false
        var started = false
        var generation = 0
        var lastRevealed: MuxaFileLocation?
        var revealTask: Task<Void, Never>?
        var polling: Task<Void, Never>?
        var refreshOffset = 0
        func stop() {
            polling?.cancel(); polling = nil
            revealTask?.cancel(); revealTask = nil
            cancel(rootNode)
            generation += 1
        }
        private func cancel(_ node: Node?) {
            node?.task?.cancel()
            for child in node?.children ?? [] { cancel(child) }
        }
        func startPolling() {
            polling = Task { [weak self] in
                while !Task.isCancelled {
                    let seconds = self?.owner.root.hostAlias == "local" ? 2 : 10
                    do { try await Task.sleep(for: .seconds(seconds)) } catch { return }
                    await self?.refreshVisible()
                }
            }
        }
        private func refreshVisible() async {
            guard let rootNode, let outline, outline.window?.isVisible == true else { return }
            let current = generation
            var nodes = [rootNode]
            for row in 0..<outline.numberOfRows {
                if let node = outline.item(atRow: row) as? Node, node.directory, outline.isItemExpanded(node) { nodes.append(node) }
            }
            let folders = Array(nodes.dropFirst())
            let start = folders.isEmpty ? 0 : refreshOffset % folders.count
            let batch = [rootNode] + (0..<min(31, folders.count)).map { folders[(start + $0) % folders.count] }
            refreshOffset = start + min(31, folders.count)
            for node in batch {
                guard !Task.isCancelled, current == generation else { return }
                if node !== rootNode, outline.row(forItem: node) < 0 { continue }
                load(node, refresh: true)
                await node.task?.value
            }
        }
        func reveal(_ location: MuxaFileLocation) async {
            guard let rootNode, Self.contains(location, in: owner.root) else { return }
            let current = generation
            var node = rootNode
            while !Task.isCancelled, current == generation {
                load(node)
                await node.task?.value
                guard !Task.isCancelled, current == generation else { return }
                guard let next = node.children?.first(where: { !$0.isStatus && ($0.location == location || ($0.directory && Self.contains(location, in: $0.location))) }) else { return }
                if next.location == location, let outline {
                    let row = outline.row(forItem: next)
                    guard row >= 0 else { return }
                    restoring = true
                    selectedLocation = location
                    outline.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
                    outline.scrollRowToVisible(row)
                    restoring = false
                    return
                }
                outline?.expandItem(next)
                node = next
            }
        }
        static func contains(_ location: MuxaFileLocation, in root: MuxaFileLocation) -> Bool {
            root.contains(location)
        }
        init(_ owner: ArtifactOutline) { self.owner = owner }
        func reload() {
            cancel(rootNode)
            lastRevealed = nil
            started = true; generation += 1
            rootNode = Node(owner.root, directory: true)
            restoring = true
            outline?.reloadData()
            restoring = false
            if let rootNode { load(rootNode) }
        }
        func load(_ node: Node, refresh: Bool = false) {
            guard !node.loading, refresh || node.children == nil else { return }
            node.loading = true
            let generation = generation
            node.task = Task {
                defer { node.loading = false }
                do {
                    let listing = try await owner.model.fileReader(for: node.location).list(node.location.path)
                    guard !Task.isCancelled, generation == self.generation else { return }
                    if node === rootNode, listing.path != owner.root.path || owner.model.fileRoot == nil {
                        owner.model.fileRoot = .init(hostAlias: node.location.hostAlias, path: listing.path)
                    }
                    guard node.entries != listing.entries || node.truncated != listing.truncated || node.failed else { return }
                    node.entries = listing.entries; node.truncated = listing.truncated; node.failed = false
                    let existing = Dictionary((node.children ?? []).filter { !$0.isStatus }.map { ($0.location, $0) }, uniquingKeysWith: { first, _ in first })
                    node.children = listing.entries.filter { owner.showHidden || !$0.name.hasPrefix(".") }.map {
                        let location = MuxaFileLocation(hostAlias: node.location.hostAlias, path: listing.path).appending($0.name)
                        if let previous = existing[location], previous.directory == $0.directory { return previous }
                        return Node(location, directory: $0.directory)
                    }
                    if node.children?.isEmpty == true { node.children = [Node(node.location, directory: false, title: String(localized: "No files in this view"))] }
                    if listing.truncated { node.children?.append(Node(node.location, directory: false, title: "Showing first 1,000 entries")) }
                    owner.model.rememberBrowsedFiles(listing, on: node.location.hostAlias)
                } catch {
                    guard !Task.isCancelled, generation == self.generation else { return }
                    node.failed = true
                    node.children = [Node(node.location, directory: false, title: error.localizedDescription)]
                }
                restoring = true
                if node === rootNode { outline?.reloadData() }
                else if let outline, outline.row(forItem: node) >= 0 { outline.reloadItem(node, reloadChildren: true) }
                for child in node.children ?? [] {
                    if child.directory && expanded.contains(child.location) { outline?.expandItem(child) }
                    if !child.isStatus, child.location == selectedLocation, let outline {
                        let row = outline.row(forItem: child)
                        if row >= 0 { outline.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false) }
                    }
                }
                restoring = false
            }
        }
        func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
            let node = item as? Node ?? rootNode
            return node?.children?.count ?? (node == nil ? 0 : 1)
        }
        func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
            let node = (item as? Node ?? rootNode)!
            return node.children?[index] ?? node.placeholder
        }
        func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool { (item as? Node)?.directory == true }
        func outlineView(_ outlineView: NSOutlineView, shouldExpandItem item: Any) -> Bool {
            if let node = item as? Node { expanded.insert(node.location); load(node) }; return true
        }
        func outlineView(_ outlineView: NSOutlineView, shouldCollapseItem item: Any) -> Bool {
            if !restoring, let node = item as? Node { expanded.remove(node.location) }
            return true
        }
        func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
            guard let node = item as? Node else { return nil }
            let cell = NSTableCellView()
            let text = NSTextField(labelWithString: node.title)
            text.font = .systemFont(ofSize: 12); text.lineBreakMode = .byTruncatingMiddle
            let icon = NSImageView(image: NSImage(systemSymbolName: node.directory ? "folder.fill" : ArtifactPreviewKind(path: node.title).icon, accessibilityDescription: nil) ?? NSImage())
            icon.contentTintColor = node.directory ? .systemBlue : .secondaryLabelColor
            text.translatesAutoresizingMaskIntoConstraints = false; icon.translatesAutoresizingMaskIntoConstraints = false
            cell.addSubview(text); cell.addSubview(icon); cell.textField = text; cell.imageView = icon
            NSLayoutConstraint.activate([
                icon.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 2), icon.widthAnchor.constraint(equalToConstant: 16), icon.heightAnchor.constraint(equalToConstant: 16), icon.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
                text.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 6), text.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -4), text.centerYAnchor.constraint(equalTo: cell.centerYAnchor)
            ])
            cell.toolTip = node.location.path
            return cell
        }
        func outlineView(_ outlineView: NSOutlineView, shouldSelectItem item: Any) -> Bool { (item as? Node)?.isStatus != true }
        private var contextNode: Node? {
            guard let outline else { return nil }
            return outline.item(atRow: outline.clickedRow >= 0 ? outline.clickedRow : outline.selectedRow) as? Node
        }
        @objc func keepOpen() { if let node = contextNode, !node.directory, !node.isStatus { owner.openFile(node.location, true) } }
        @objc func copyPath() {
            guard let node = contextNode, !node.isStatus else { return }
            NSPasteboard.general.clearContents(); NSPasteboard.general.setString(node.location.path, forType: .string)
        }
        @objc func openFolder() { if let node = contextNode, node.directory { owner.model.fileRoot = node.location } }
        func outlineViewSelectionDidChange(_ notification: Notification) {
            guard !restoring else { return }
            if let outline, let node = outline.item(atRow: outline.selectedRow) as? Node { selectedLocation = node.location }
            preview(pin: false)
        }
        @objc func clicked() {
            guard let outline, outline.clickedRow >= 0, let node = outline.item(atRow: outline.clickedRow) as? Node else { return }
            if !node.directory && !node.isStatus { owner.openFile(node.location, false) }
        }
        @objc func doubleClicked() {
            guard let outline, outline.clickedRow >= 0, let node = outline.item(atRow: outline.clickedRow) as? Node else { return }
            if node.directory {
                if outline.isItemExpanded(node) { outline.collapseItem(node) } else { outline.expandItem(node) }
            } else if !node.isStatus { owner.openFile(node.location, true) }
        }
        func preview(pin: Bool) {
            guard let outline, outline.selectedRow >= 0, let node = outline.item(atRow: outline.selectedRow) as? Node, !node.directory, !node.isStatus else { return }
            owner.openFile(node.location, pin)
        }
    }
}

struct ArtifactFolderPicker: View {
    @ObservedObject var model: AppModel
    let root: MuxaFileLocation
    let select: (MuxaFileLocation) -> Void
    @State private var path = ""
    @State private var suggestions: [String] = []
    @State private var error: String?
    @State private var loading = false
    @State private var selected = 0
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Go to Folder…").font(.headline)
            ArtifactPathField(text: $path, submit: { navigate() }, complete: complete, move: { delta in
                guard !suggestions.isEmpty else { return }
                selected = min(max(0, selected + delta), suggestions.count - 1)
            }).frame(height: 26)
            Text(root.hostAlias == "local" ? "This Mac · ~ / Documents / …" : root.hostAlias).font(.caption).foregroundStyle(.secondary)
            if let error { Text(error).font(.caption).foregroundStyle(.red) }
            ForEach(Array(suggestions.prefix(8).enumerated()), id: \.element) { index, value in
                Button { path = value + "/" } label: {
                    HStack { Image(systemName: "folder"); Text((value as NSString).lastPathComponent); Spacer(); Image(systemName: "arrow.turn.down.right").font(.caption) }
                        .padding(6).contentShape(Rectangle()).background(index == selected ? Color.accentColor.opacity(0.15) : Color.clear).cornerRadius(4)
                }.buttonStyle(.plain)
            }
            HStack {
                Text("↑ ↓ Select · Tab Complete · Return Open").font(.caption2).foregroundStyle(.secondary)
                Spacer()
                if loading { ProgressView().controlSize(.small) }
                Button("Open") { navigate() }.disabled(loading)
            }
        }.padding(14).onAppear { path = root.path.hasSuffix("/") ? root.path : root.path + "/" }
        .task(id: path) {
            suggestions = []; selected = 0
            do {
                try await Task.sleep(for: .milliseconds(180))
                let query = Self.completionQuery(path, relativeTo: root.path)
                let listing = try await model.fileReader(for: root).list(query.directory)
                try Task.checkCancellation()
                suggestions = listing.entries.filter { $0.directory && (query.prefix.isEmpty || $0.name.range(of: query.prefix, options: [.anchored, .caseInsensitive, .diacriticInsensitive]) != nil) }.prefix(8).map {
                    (listing.path as NSString).appendingPathComponent($0.name)
                }
                selected = 0
            } catch is CancellationError {} catch { if !Task.isCancelled { suggestions = [] } }
        }
    }
    static func completionQuery(_ input: String, relativeTo root: String) -> (directory: String, prefix: String) {
        let value = input.hasPrefix("/") || input.hasPrefix("~") ? input : (root as NSString).appendingPathComponent(input)
        if value.hasSuffix("/") || value == "~" { return (value, "") }
        return ((value as NSString).deletingLastPathComponent, (value as NSString).lastPathComponent)
    }
    private func complete() { if suggestions.indices.contains(selected) { path = suggestions[selected] + "/" } }
    private func navigate() {
        guard !loading else { return }
        loading = true; error = nil
        let value = path.hasPrefix("/") || path.hasPrefix("~") ? path : (root.path as NSString).appendingPathComponent(path)
        Task {
            do {
                let listing = try await model.fileReader(for: root).list(value)
                select(.init(hostAlias: root.hostAlias, path: listing.path))
            } catch { self.error = error.localizedDescription }
            loading = false
        }
    }
}

private struct ArtifactPathField: NSViewRepresentable {
    @Binding var text: String
    let submit: () -> Void
    let complete: () -> Void
    let move: (Int) -> Void
    func makeCoordinator() -> Coordinator { Coordinator(self) }
    func makeNSView(context: Context) -> NSTextField {
        let field = NSTextField(); field.delegate = context.coordinator
        field.placeholderString = "Folder path"; field.setAccessibilityLabel("Folder path")
        field.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        DispatchQueue.main.async { field.window?.makeFirstResponder(field); field.selectText(nil) }
        return field
    }

    func updateNSView(_ field: NSTextField, context: Context) {
        context.coordinator.owner = self
        if field.stringValue != text { field.stringValue = text }
    }
    final class Coordinator: NSObject, NSTextFieldDelegate {
        var owner: ArtifactPathField
        init(_ owner: ArtifactPathField) { self.owner = owner }
        func controlTextDidChange(_ notification: Notification) {
            if let field = notification.object as? NSTextField { owner.text = field.stringValue }
        }
        func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
            guard !textView.hasMarkedText() else { return false }
            switch NSStringFromSelector(selector) {
            case "insertNewline:": owner.submit()
            case "insertTab:": owner.complete()
            case "moveDown:": owner.move(1)
            case "moveUp:": owner.move(-1)
            default: return false
            }
            return true
        }
    }
}
