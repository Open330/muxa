import AppKit
import SwiftUI

enum MuxaPaletteMode: String, Identifiable {
    case navigation
    case commands

    var id: Self { self }

    static func shortcut(characters: String?, modifiers: NSEvent.ModifierFlags) -> Self? {
        guard characters?.lowercased() == "p" else { return nil }
        let flags = modifiers.intersection(.deviceIndependentFlagsMask).subtracting([.capsLock, .numericPad, .function])
        switch flags {
        case .command: return .navigation
        case [.command, .shift]: return .commands
        default: return nil
        }
    }
}

enum MuxaPaletteAction: Hashable {
    case navigate(MuxaSidebarSelection)
    case command(MuxaPaletteCommand)
}

enum MuxaPaletteCommand: String, CaseIterable, Identifiable {
    case startWork, workCommandCenter, liveWatch, ask, newShell
    case showWork, showWatch, showInbox, showShells, refresh
    case closeEditor, previousEditor, nextEditor, splitEditor, pinEditor, focusSidebar

    var id: Self { self }

    var title: String {
        switch self {
        case .startWork: String(localized: "Work: Start configured Work")
        case .workCommandCenter: String(localized: "Work: Open Command Center")
        case .liveWatch: String(localized: "Watch: Open native Live Watch")
        case .ask: String(localized: "Ask: Open global Ask")
        case .newShell: String(localized: "Shell: New native shell")
        case .showWork: String(localized: "View: Show Work")
        case .showWatch: String(localized: "View: Show Explore")
        case .showInbox: String(localized: "View: Show Inbox")
        case .showShells: String(localized: "View: Show Shells")
        case .refresh: String(localized: "Workspace: Refresh")
        case .closeEditor: String(localized: "Editor: Close active editor")
        case .previousEditor: String(localized: "Editor: Previous editor")
        case .nextEditor: String(localized: "Editor: Next editor")
        case .splitEditor: String(localized: "Editor: Split right")
        case .pinEditor: String(localized: "Editor: Keep open (pin)")
        case .focusSidebar: String(localized: "View: Focus sidebar")
        }
    }

    var systemImage: String {
        switch self {
        case .startWork: "play.square.stack"
        case .workCommandCenter, .showWork: "square.stack.3d.up"
        case .liveWatch: "waveform.path.ecg.rectangle"
        case .ask: "sparkles"
        case .newShell, .showShells: "terminal"
        case .showWatch, .focusSidebar: "sidebar.left"
        case .showInbox: "tray.full"
        case .refresh: "arrow.clockwise"
        case .closeEditor: "xmark"
        case .previousEditor: "arrow.left"
        case .nextEditor: "arrow.right"
        case .splitEditor: "rectangle.split.2x1"
        case .pinEditor: "pin"
        }
    }

    @MainActor
    func disabledReason(model: AppModel) -> String? {
        switch self {
        case .startWork, .newShell, .refresh:
            guard model.isConnected else { return String(localized: "Daemon is not connected") }
            if self == .startWork && model.isStartingWork {
                return String(localized: "Work is starting")
            }
            if self == .newShell && model.isCreatingSession {
                return String(localized: "A shell is being created")
            }
        case .closeEditor, .previousEditor, .nextEditor, .splitEditor, .pinEditor:
            guard let selection = model.sidebarSelection, model.isSelectionAvailable(selection) else {
                return String(localized: "No active editor")
            }
        default:
            break
        }
        return nil
    }
}

struct MuxaPaletteID: Hashable {
    let components: [String?]
}

struct MuxaPaletteItem: Identifiable, Equatable {
    let stableID: MuxaPaletteID
    let title: String
    let subtitle: String
    let systemImage: String
    let action: MuxaPaletteAction
    var disabledReason: String? = nil

    var id: MuxaPaletteID { stableID }
    var isEnabled: Bool { disabledReason == nil }
}

enum MuxaPaletteSearch {
    private static func matchTier(query: String, item: MuxaPaletteItem) -> Int {
        let locale = Locale(identifier: "en_US_POSIX")
        let query = query.trimmingCharacters(in: .whitespacesAndNewlines)
            .folding(options: [.caseInsensitive, .diacriticInsensitive], locale: locale)
        guard !query.isEmpty else { return 0 }
        let title = item.title.trimmingCharacters(in: .whitespacesAndNewlines)
            .folding(options: [.caseInsensitive, .diacriticInsensitive], locale: locale)
        if title == query {
            if case .navigate(.pane) = item.action { return 6 }
            return 5
        }
        if title.hasPrefix(query) { return 4 }
        if title.contains(query) { return 3 }
        let subtitle = item.subtitle.folding(options: [.caseInsensitive, .diacriticInsensitive], locale: locale)
        if subtitle.contains(query) { return 2 }
        return 1
    }

    static func score(query: String, text: String) -> Int? {
        let locale = Locale(identifier: "en_US_POSIX")
        let foldedText = Array(text.folding(options: [.caseInsensitive, .diacriticInsensitive], locale: locale))
        let words = query.folding(options: [.caseInsensitive, .diacriticInsensitive], locale: locale)
            .split(whereSeparator: { $0.isWhitespace })
        var total = 0
        for word in words {
            if String(foldedText).contains(word) {
                total += 100 + word.count * 18
                continue
            }
            var cursor = 0
            var previous: Int?
            for character in word {
                guard let index = foldedText[cursor...].firstIndex(of: character) else { return nil }
                total += 10
                if index == 0 || !foldedText[index - 1].isLetter && !foldedText[index - 1].isNumber {
                    total += 12
                }
                if let previous {
                    total += index == previous + 1 ? 8 : -min(index - previous - 1, 8)
                }
                previous = index
                cursor = index + 1
            }
        }
        return total
    }

    static func results(_ items: [MuxaPaletteItem], query: String, recent: [MuxaSidebarSelection]) -> [MuxaPaletteItem] {
        var seen = Set<MuxaPaletteID>()
        let unique = items.filter { seen.insert($0.stableID).inserted }
        return unique.enumerated().compactMap { offset, item -> (MuxaPaletteItem, Int, Int, Int, Int)? in
            guard let score = score(query: query, text: item.title + " " + item.subtitle) else { return nil }
            let rank: Int
            if case .navigate(let selection) = item.action {
                rank = recent.firstIndex(of: selection) ?? Int.max
            } else {
                rank = Int.max
            }
            return (item, matchTier(query: query, item: item), score, rank, offset)
        }.sorted {
            if $0.1 != $1.1 { return $0.1 > $1.1 }
            if $0.2 != $1.2 { return $0.2 > $1.2 }
            if $0.3 != $1.3 { return $0.3 < $1.3 }
            return $0.4 < $1.4
        }.map { $0.0 }
    }
}

enum MuxaPaletteSelection {
    static func updated(_ selected: MuxaPaletteID?, previousQuery: String, query: String, in items: [MuxaPaletteItem]) -> MuxaPaletteID? {
        retained(previousQuery == query ? selected : nil, in: items)
    }

    static func retained(_ selected: MuxaPaletteID?, in items: [MuxaPaletteItem]) -> MuxaPaletteID? {
        if let selected, items.contains(where: { $0.stableID == selected && $0.isEnabled }) { return selected }
        return items.first(where: \.isEnabled)?.stableID
    }

    static func moved(_ selected: MuxaPaletteID?, by delta: Int, in items: [MuxaPaletteItem]) -> MuxaPaletteID? {
        let enabled = items.filter(\.isEnabled)
        guard !enabled.isEmpty else { return nil }
        guard let index = enabled.firstIndex(where: { $0.stableID == selected }) else {
            return delta < 0 ? enabled.last?.stableID : enabled.first?.stableID
        }
        let step = delta % enabled.count
        return enabled[(index + step + enabled.count) % enabled.count].stableID
    }

    static func action(_ selected: MuxaPaletteID?, in items: [MuxaPaletteItem]) -> MuxaPaletteAction? {
        items.first(where: { $0.stableID == selected && $0.isEnabled })?.action
    }
}

enum MuxaPaletteKeyDecision: Equatable {
    case previous, next, submit, cancel, passThrough

    static func resolve(selector: String, hasMarkedText: Bool, beganWithMarkedText: Bool = false) -> Self {
        guard !hasMarkedText, !beganWithMarkedText else { return .passThrough }
        switch selector {
        case "moveUp:", "insertBacktab:": return .previous
        case "moveDown:", "insertTab:": return .next
        case "insertNewline:", "insertNewlineIgnoringFieldEditor:": return .submit
        case "cancelOperation:": return .cancel
        default: return .passThrough
        }
    }
}

enum MuxaPaletteItems {
    static func navigation(
        watchSections: [MuxaWatchHost], workGroups: [MuxaWorkGroup],
        sessions: [MuxaSession], agents: [MuxaHostedAgent], localSocket: String
    ) -> [MuxaPaletteItem] {
        var items: [MuxaPaletteItem] = []
        for section in watchSections {
            for session in section.sessions {
                let context = [session.hostAlias, session.socket, session.sessionID].joined(separator: " · ")
                items.append(MuxaPaletteItem(
                    stableID: MuxaPaletteID(components: ["session", session.hostAlias, session.socket, session.sessionID]),
                    title: session.name, subtitle: String(localized: "Session") + " · " + context,
                    systemImage: "rectangle.stack", action: .navigate(.fleetSession(session.identity))
                ))
                for window in session.windows {
                    let components = ["window", window.hostAlias, window.socket, window.sessionID, window.windowID]
                    let windowContext = [session.name, window.name, context, window.windowID].joined(separator: " · ")
                    items.append(MuxaPaletteItem(
                        stableID: MuxaPaletteID(components: components), title: window.name,
                        subtitle: String(localized: "Window") + " · " + windowContext, systemImage: "macwindow",
                        action: .navigate(.fleetWindow(window.identity))
                    ))
                    for pane in window.panes {
                        items.append(MuxaPaletteItem(
                            stableID: MuxaPaletteID(components: [
                                "pane", pane.host.alias, pane.pane.endpointSocket,
                                pane.pane.sessionID, pane.pane.windowID, pane.pane.paneID,
                            ]),
                            title: pane.pane.title.isEmpty ? pane.pane.currentCommand : pane.pane.title,
                            subtitle: [String(localized: "Pane"), windowContext, pane.pane.paneID, pane.pane.currentPath].joined(separator: " · "),
                            systemImage: "rectangle.split.2x1", action: .navigate(.pane(pane.id))
                        ))
                    }
                }
            }
        }
        for session in sessions where !session.exited {
            items.append(MuxaPaletteItem(
                stableID: MuxaPaletteID(components: ["shell", "local", localSocket, session.id]),
                title: session.displayName ?? session.id,
                subtitle: [String(localized: "Native shell"), "local", localSocket, session.id, session.cwd ?? ""].joined(separator: " · "),
                systemImage: "terminal", action: .navigate(.shell(session.id))
            ))
        }
        for work in workGroups {
            items.append(MuxaPaletteItem(
                stableID: MuxaPaletteID(components: ["work", work.identity.workspaceID, work.identity.workID]),
                title: work.title,
                subtitle: ([String(localized: "Work"), work.workspaceID, work.pipelineLabel, work.cwd ?? ""] + work.hostAliases).joined(separator: " · "),
                systemImage: "square.stack.3d.up", action: .navigate(.work(work.identity))
            ))
        }
        let agentIDs = Dictionary(grouping: agents, by: \.id).mapValues { group in
            Set(group.map { agentID($0) }).count
        }
        for agent in agents {
            items.append(MuxaPaletteItem(
                stableID: agentID(agent), title: agent.agent.aiTitle ?? agent.agent.kind,
                subtitle: [
                    String(localized: "Agent"), agent.host.alias, agent.agent.tmuxSocket ?? "", agent.agent.tmuxSession ?? "",
                    agent.agent.id, agent.agent.state, agent.agent.cwd ?? "",
                ].joined(separator: " · "),
                systemImage: "sparkles", action: .navigate(.agent(agent.id)),
                disabledReason: (agentIDs[agent.id] ?? 0) > 1 ? String(localized: "Ambiguous agent destination; open its pane instead") : nil
            ))
        }
        return items
    }

    private static func agentID(_ agent: MuxaHostedAgent) -> MuxaPaletteID {
        MuxaPaletteID(components: [
            "agent", agent.host.alias, agent.pane?.endpointSocket ?? agent.agent.tmuxSocket,
            agent.pane?.sessionID ?? agent.agent.tmuxSession, agent.pane?.windowID,
            agent.agent.id, agent.pane?.paneID ?? agent.agent.pane,
        ])
    }
}

struct CommandPaletteView: View {
    @ObservedObject var model: AppModel
    @State var mode: MuxaPaletteMode
    let recent: [MuxaSidebarSelection]
    let onChoose: (MuxaPaletteAction) -> Void
    let onCancel: () -> Void

    @State private var query = ""
    @State private var selectedID: MuxaPaletteID?
    @State private var didFinish = false

    private var items: [MuxaPaletteItem] {
        let candidates: [MuxaPaletteItem]
        switch mode {
        case .navigation:
            candidates = MuxaPaletteItems.navigation(
                watchSections: model.executionSnapshot.watchHosts, workGroups: model.workGroups,
                sessions: model.sessions, agents: model.hostedAgents, localSocket: model.client.socketPath
            )
        case .commands:
            candidates = MuxaPaletteCommand.allCases.map { command in
                MuxaPaletteItem(
                    stableID: MuxaPaletteID(components: ["command", command.rawValue]), title: command.title,
                    subtitle: command.disabledReason(model: model) ?? "", systemImage: command.systemImage,
                    action: .command(command), disabledReason: command.disabledReason(model: model)
                )
            }
        }
        return MuxaPaletteSearch.results(candidates, query: query, recent: recent)
    }

    var body: some View {
        let results = items
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Image(systemName: mode == .navigation ? "magnifyingglass" : "chevron.right")
                    .foregroundStyle(.secondary)
                MuxaPaletteSearchField(
                    text: Binding(get: { query }, set: { updateQuery($0) }),
                    placeholder: mode == .navigation
                        ? String(localized: "Search sessions, windows, panes, shells, work, agents")
                        : String(localized: "Type a command"),
                    onKey: handleKey,
                    onModeChange: { mode = $0 }
                )
                Text(mode == .navigation ? "⌘P" : "⇧⌘P")
                    .font(.caption.monospaced()).foregroundStyle(.secondary)
            }
            .padding(16)
            Divider()
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(spacing: 2) {
                        if results.isEmpty {
                            VStack(spacing: 8) {
                                Image(systemName: "magnifyingglass").font(.title2)
                                Text(query.isEmpty ? "No destinations available" : "No matching results")
                                Text(query.isEmpty ? "Sessions and work appear here when available." : "Try a name, host, socket, or a shorter query.")
                                    .font(.caption)
                            }
                            .foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity, minHeight: 230)
                        }
                        ForEach(results) { item in
                            row(item)
                        }
                    }
                    .padding(8)
                }
                .onChange(of: selectedID) { selected in
                    if let selected { proxy.scrollTo(selected) }
                }
                .onChange(of: results) { updated in
                    selectedID = MuxaPaletteSelection.retained(selectedID, in: updated)
                    if let selectedID { proxy.scrollTo(selectedID) }
                }
            }
            Divider()
            HStack(spacing: 14) {
                Text("↑↓ / ⇧⇥ ⇥  select")
                Text("↵  open")
                Text("esc  dismiss")
                Spacer()
                Text("\(results.count) results")
            }
            .font(.caption).foregroundStyle(.secondary).padding(12)
        }
        .frame(width: 680, height: 430)
        .background(.regularMaterial)
        .onAppear { selectedID = MuxaPaletteSelection.retained(selectedID, in: results) }
        .onChange(of: mode) { _ in
            query = ""
            selectedID = MuxaPaletteSelection.retained(nil, in: items)
        }
    }

    private func row(_ item: MuxaPaletteItem) -> some View {
        Button {
            selectedID = item.stableID
            choose(item.stableID)
        } label: {
            HStack(spacing: 10) {
                Image(systemName: item.systemImage).frame(width: 22)
                VStack(alignment: .leading, spacing: 3) {
                    Text(item.title).font(.body).lineLimit(1)
                    if !item.subtitle.isEmpty {
                        Text(item.subtitle).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                            .truncationMode(.middle)
                    }
                }
                Spacer(minLength: 0)
                if !item.isEnabled { Image(systemName: "lock").font(.caption) }
                if item.stableID == selectedID { Image(systemName: "return").font(.caption) }
            }
            .padding(.horizontal, 10).padding(.vertical, 9)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(item.stableID == selectedID ? Color.accentColor.opacity(0.18) : Color.clear)
            .clipShape(RoundedRectangle(cornerRadius: 6))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(!item.isEnabled)
        .opacity(item.isEnabled ? 1 : 0.5)
        .help(item.disabledReason ?? item.subtitle)
        .accessibilityValue(item.stableID == selectedID ? String(localized: "Selected") : "")
        .id(item.stableID)
    }

    private func updateQuery(_ value: String) {
        let previousQuery = query
        query = value
        selectedID = MuxaPaletteSelection.updated(selectedID, previousQuery: previousQuery, query: value, in: items)
    }

    private func handleKey(_ decision: MuxaPaletteKeyDecision) {
        guard !didFinish else { return }
        switch decision {
        case .previous: selectedID = MuxaPaletteSelection.moved(selectedID, by: -1, in: items)
        case .next: selectedID = MuxaPaletteSelection.moved(selectedID, by: 1, in: items)
        case .submit: choose(selectedID)
        case .cancel:
            didFinish = true
            onCancel()
        case .passThrough: break
        }
    }

    private func choose(_ selected: MuxaPaletteID?) {
        guard !didFinish, let action = MuxaPaletteSelection.action(selected, in: items) else { return }
        didFinish = true
        onChoose(action)
    }
}

struct MuxaPaletteSearchField: NSViewRepresentable {
    @Binding var text: String
    let placeholder: String
    let onKey: (MuxaPaletteKeyDecision) -> Void
    let onModeChange: (MuxaPaletteMode) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeNSView(context: Context) -> NSTextField {
        let field = MuxaPaletteTextField(frame: .zero)
        field.cell = MuxaPaletteTextFieldCell(textCell: "")
        field.isEditable = true
        field.isSelectable = true
        field.onModeChange = onModeChange
        field.isBezeled = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.font = .systemFont(ofSize: 15)
        field.delegate = context.coordinator
        field.placeholderString = placeholder
        field.stringValue = text
        field.setAccessibilityLabel(placeholder)
        field.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        return field
    }

    func updateNSView(_ field: NSTextField, context: Context) {
        context.coordinator.parent = self
        (field as? MuxaPaletteTextField)?.onModeChange = onModeChange
        field.placeholderString = placeholder
        field.setAccessibilityLabel(placeholder)
        if field.stringValue != text, (field.currentEditor() as? NSTextView)?.hasMarkedText() != true {
            field.stringValue = text
        }
    }

    final class Coordinator: NSObject, NSTextFieldDelegate {
        var parent: MuxaPaletteSearchField

        init(_ parent: MuxaPaletteSearchField) { self.parent = parent }

        func controlTextDidChange(_ notification: Notification) {
            guard let field = notification.object as? NSTextField else { return }
            parent.text = field.stringValue
        }

        func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
            let decision = MuxaPaletteKeyDecision.resolve(
                selector: NSStringFromSelector(commandSelector), hasMarkedText: textView.hasMarkedText(),
                beganWithMarkedText: (textView as? MuxaPaletteFieldEditor)?.beganWithMarkedText ?? false
            )
            guard decision != .passThrough else { return false }
            parent.onKey(decision)
            return true
        }
    }
}

final class MuxaPaletteTextField: NSTextField {
    var onModeChange: (MuxaPaletteMode) -> Void = { _ in } {
        didSet { (cell as? MuxaPaletteTextFieldCell)?.editor.onModeChange = onModeChange }
    }

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        if let mode = MuxaPaletteMode.shortcut(characters: event.charactersIgnoringModifiers, modifiers: event.modifierFlags) {
            onModeChange(mode)
            return true
        }
        return super.performKeyEquivalent(with: event)
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard window != nil else { return }
        DispatchQueue.main.async { [weak self] in
            guard let self, let window = self.window else { return }
            window.makeFirstResponder(self)
        }
    }
}

private final class MuxaPaletteTextFieldCell: NSTextFieldCell {
    let editor = MuxaPaletteFieldEditor(frame: .zero)

    override func fieldEditor(for controlView: NSView) -> NSTextView? {
        editor.isFieldEditor = true
        editor.isRichText = false
        editor.isAutomaticQuoteSubstitutionEnabled = false
        editor.isAutomaticDashSubstitutionEnabled = false
        editor.isAutomaticTextReplacementEnabled = false
        return editor
    }
}

private final class MuxaPaletteFieldEditor: NSTextView {
    private(set) var beganWithMarkedText = false
    var onModeChange: (MuxaPaletteMode) -> Void = { _ in }

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        if let mode = MuxaPaletteMode.shortcut(characters: event.charactersIgnoringModifiers, modifiers: event.modifierFlags) {
            onModeChange(mode)
            return true
        }
        return super.performKeyEquivalent(with: event)
    }

    override func keyDown(with event: NSEvent) {
        beganWithMarkedText = hasMarkedText()
        defer { beganWithMarkedText = false }
        super.keyDown(with: event)
    }
}
