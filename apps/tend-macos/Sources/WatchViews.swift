// Explore: the live pane tree, its rows, and the pane inspector.
import AppKit
import SwiftUI

struct WatchLivePanePanel: View {
    let pane: TendWatchPane
    @ObservedObject var model: AppModel
    let attachedSessionID: String?
    let startAttach: () -> Void
    let stopAttach: () -> Void
    let sessionExited: () -> Void
    @Environment(\.colorScheme) private var colorScheme
    @State private var prompt = ""
    @State private var sending = false
    @State private var feedback: PromptFeedback?

    private var attachedSession: TendSession? {
        attachedSessionID.flatMap { id in model.sessions.first(where: { $0.id == id }) }
    }

    private var canAttach: Bool {
        pane.host.local || pane.host.mode == "control"
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 9) {
                Label("Live Pane", systemImage: "terminal")
                    .font(TendType.detail.weight(.semibold))
                    .fixedSize()
                Text(verbatim: "\(pane.host.alias) · \(pane.pane.session) › \(pane.pane.windowName) › \(pane.pane.paneID)")
                    .font(TendType.meta.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                Text(attachedSessionID == nil ? "Read-only" : "Fitted · Interactive")
                    .font(TendType.meta.weight(.medium))
                    .foregroundStyle(attachedSessionID == nil ? Color.secondary : Color.green)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(Color.primary.opacity(0.07), in: Capsule())
                    .help(
                        attachedSessionID == nil
                            ? "The preview preserves the pane's current tmux dimensions"
                            : "This pane is temporarily zoomed and follows the Live Pane size; tmux settings are restored on detach"
                    )
                Spacer(minLength: 4)
                if model.isAttachingPane {
                    ProgressView().controlSize(.mini)
                }
                if attachedSessionID == nil {
                    Button {
                        startAttach()
                    } label: {
                        Label("Click to Type", systemImage: "keyboard")
                    }
                    .buttonStyle(.tendPrimary)
                    .controlSize(.small)
                    .disabled(model.isAttachingPane || !canAttach)
                    .help(canAttach ? "Attach this pane here and enable keyboard input" : "This host is available in observe mode only")
                } else {
                    Button {
                        stopAttach()
                    } label: {
                        Label("Stop", systemImage: "stop.fill")
                    }
                    .buttonStyle(.tendGhost)
                }
            }
            .padding(.horizontal, 10)
            .frame(height: 36)
            .background(TendSurfacePalette.sidebar(for: colorScheme))

            Divider()

            if let session = attachedSession, !session.exited {
                TerminalPane(
                    client: model.client,
                    sessionID: session.id,
                    replayInitialHistory: session.hasBeenAttached == true,
                    showsToolbar: false,
                    onExit: sessionExited
                )
                .id(session.id)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .clipped()
            } else {
                PaneCaptureView(
                    client: model.client,
                    target: TendPaneTarget(host: pane.host, pane: pane.pane),
                    showsHeader: false
                )
                .id(pane.id)
                .help(canAttach ? "Use Click to Type to enter interactive mode" : "Read-only pane preview")
                .task(id: attachedSessionID) {
                    if attachedSessionID != nil, attachedSession?.exited == true {
                        sessionExited()
                    }
                }

                Divider()

                PanePromptComposer(
                    host: pane.host,
                    pane: pane.pane,
                    client: model.client,
                    prompt: $prompt,
                    sending: $sending,
                    feedback: $feedback
                )
                .padding(10)
                .background(TendSurfacePalette.sidebar(for: colorScheme))
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .clipped()
        .overlay {
            Rectangle()
                .stroke(Color(nsColor: .separatorColor).opacity(0.7), lineWidth: 0.5)
        }
    }
}

/// How strongly one Explore tree row is highlighted. Only the row that matches
/// the active editor (`sidebarSelection`) is `selected`; the path down to the
/// followed pane (`watchSelection`) is a lighter `followed` marker.
enum WatchTreeHighlight: Equatable {
    case selected
    case followed
    case idle
}

/// The two selections the Explore tree reflects, and the rule for combining
/// them so that a row never looks selected because of a stale pane choice.
struct WatchTreeSelection: Equatable {
    let editor: TendSidebarSelection?
    let followedPane: TendWatchPaneIdentity?

    /// The followed pane path is only meaningful while the active editor
    /// follows that pane: the Live Watch tool or the pane's own editor.
    var showsFollowedPath: Bool {
        switch editor {
        case .watch: true
        case .pane(let id): id == followedPane
        default: false
        }
    }

    func highlight(
        for row: TendSidebarSelection,
        containsFollowedPane: Bool
    ) -> WatchTreeHighlight {
        if editor == row { return .selected }
        if showsFollowedPath, containsFollowedPane { return .followed }
        return .idle
    }
}

/// Explore row fill: a strong accent for the active editor row, a lighter
/// tint for the followed pane path, otherwise `idle`.
private func watchHighlightFill(
    _ highlight: WatchTreeHighlight,
    selected: Double = 0.18,
    idle: Color = .clear
) -> Color {
    switch highlight {
    case .selected: Color.accentColor.opacity(selected)
    case .followed: Color.accentColor.opacity(0.06)
    case .idle: idle
    }
}

/// Thin leading accent bar that marks the followed pane without reading as a
/// selection. It never participates in hit testing.
private struct WatchFollowedMarker: View {
    let highlight: WatchTreeHighlight

    var body: some View {
        if highlight == .followed {
            Capsule()
                .fill(Color.accentColor.opacity(0.65))
                .frame(width: 3)
                .padding(.vertical, 9)
                .allowsHitTesting(false)
        }
    }
}

struct WatchHostTree: View {
    let group: TendWatchHost
    let selection: WatchTreeSelection
    let selectHost: (String) -> Void
    let selectSession: (TendWatchSessionIdentity) -> Void
    let openPinnedSession: (TendWatchSessionIdentity) -> Void
    let selectPane: (TendWatchPaneIdentity) -> Void
    let openPinnedPane: (TendWatchPaneIdentity) -> Void
    let forceExpanded: Bool
    let workLabel: (TendWatchWindow) -> String?
    @State private var manualExpansion: Bool?

    private var containsSelection: Bool {
        selection.followedPane.map { selected in
            group.sessions.contains { session in
                session.windows.contains { window in
                    window.panes.contains { $0.id == selected }
                }
            }
        } ?? false
    }

    private var expanded: Bool {
        forceExpanded || (manualExpansion ?? containsSelection)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                Button { manualExpansion = !expanded } label: {
                    hierarchyChevron(expanded)
                        .frame(width: 16, height: TendTheme.rowHeight)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)

                Button { selectHost(group.host.alias) } label: {
                    HStack(spacing: 6) {
                        HostIdentityBadge(host: group.host, size: 16)
                        Text(group.host.alias)
                            .font(.callout.weight(.semibold))
                            .lineLimit(1)
                        Spacer(minLength: 4)
                        Text(verbatim: "\(group.paneCount)")
                            .font(TendType.meta.monospacedDigit())
                            .foregroundStyle(.secondary)
                        Circle()
                            .fill(fleetHostColor(group.host.state))
                            .frame(width: 6, height: 6)
                    }
                    .padding(.horizontal, 7)
                    .frame(maxWidth: .infinity, minHeight: TendTheme.rowHeight, alignment: .leading)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .simultaneousGesture(
                    TapGesture(count: 2).onEnded { manualExpansion = !expanded }
                )
            }
            .background(
                watchHighlightFill(
                    selection.highlight(
                        for: .host(group.host.alias),
                        containsFollowedPane: containsSelection
                    ),
                    selected: 0.14,
                    idle: expanded ? Color.primary.opacity(0.035) : Color.clear
                )
            )

            if expanded {
                ForEach(group.sessions) { session in
                    WatchSessionTree(
                        session: session,
                        selection: selection,
                        selectSession: selectSession,
                        openPinnedSession: openPinnedSession,
                        selectPane: selectPane,
                        openPinnedPane: openPinnedPane,
                        forceExpanded: forceExpanded,
                        workLabel: workLabel
                    )
                }
            }
        }
    }
}

private struct WatchSessionTree: View {
    let session: TendWatchSession
    let selection: WatchTreeSelection
    let selectSession: (TendWatchSessionIdentity) -> Void
    let openPinnedSession: (TendWatchSessionIdentity) -> Void
    let selectPane: (TendWatchPaneIdentity) -> Void
    let openPinnedPane: (TendWatchPaneIdentity) -> Void
    let forceExpanded: Bool
    let workLabel: (TendWatchWindow) -> String?
    @State private var manualExpansion: Bool?

    private var selectedPath: Bool {
        selection.followedPane.map { selected in
            session.windows.contains { window in
                window.panes.contains { $0.id == selected }
            }
        } ?? false
    }

    private var highlight: WatchTreeHighlight {
        selection.highlight(
            for: .fleetSession(session.identity),
            containsFollowedPane: selectedPath
        )
    }

    private var expanded: Bool {
        forceExpanded || (manualExpansion ?? (singleWindow != nil || selectedPath))
    }

    private var singleWindow: TendWatchWindow? {
        session.windows.count == 1 ? session.windows.first : nil
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                explorerIndent(depth: 1)
                Button { manualExpansion = !expanded } label: {
                    hierarchyChevron(expanded)
                        .frame(width: 16, height: TendTheme.rowHeight)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                Button {
                    selectSession(session.identity)
                    if !expanded { manualExpansion = true }
                } label: {
                    HStack(spacing: 6) {
                        Image(systemName: singleWindow == nil ? "square.3.layers.3d" : "rectangle.stack")
                            .foregroundStyle(singleWindow == nil ? Color.secondary : Color.accentColor)
                            .frame(width: 17)
                        Text(session.name.isEmpty ? session.sessionID : session.name)
                            .font(.callout)
                            .lineLimit(1)
                        if let singleWindow {
                            Image(systemName: "chevron.right")
                                .font(.system(size: 7, weight: .bold))
                                .foregroundStyle(.tertiary)
                            Text(workLabel(singleWindow) ?? (singleWindow.name.isEmpty ? singleWindow.windowID : singleWindow.name))
                                .font(.callout)
                                .foregroundStyle(workLabel(singleWindow) == nil ? Color.secondary : Color.accentColor)
                                .lineLimit(1)
                        }
                        Spacer(minLength: 3)
                        Text(verbatim: singleWindow.map { "\($0.panes.count)" } ?? "\(session.windows.count)")
                            .font(TendType.meta.monospacedDigit())
                            .foregroundStyle(.secondary)
                    }
                    .padding(.trailing, 7)
                    .frame(maxWidth: .infinity, minHeight: TendTheme.rowHeight, alignment: .leading)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .simultaneousGesture(
                    TapGesture(count: 2).onEnded {
                        openPinnedSession(session.identity)
                    }
                )
            }
            .background(watchHighlightFill(highlight))

            if expanded {
                if let singleWindow {
                    ForEach(singleWindow.panes) { pane in
                        WatchPaneRow(
                            pane: pane,
                            highlight: selection.highlight(
                                for: .pane(pane.id),
                                containsFollowedPane: selection.followedPane == pane.id
                            ),
                            depth: 2,
                            selectPane: selectPane,
                            openPinnedPane: openPinnedPane
                        )
                    }
                } else {
                    ForEach(session.windows) { window in
                        WatchWindowTree(
                            window: window,
                            selection: selection,
                            selectPane: selectPane,
                            openPinnedPane: openPinnedPane,
                            forceExpanded: forceExpanded,
                            logicalWork: workLabel(window)
                        )
                    }
                }
            }
        }
    }
}

private struct WatchWindowTree: View {
    let window: TendWatchWindow
    let selection: WatchTreeSelection
    let selectPane: (TendWatchPaneIdentity) -> Void
    let openPinnedPane: (TendWatchPaneIdentity) -> Void
    let forceExpanded: Bool
    let logicalWork: String?
    @State private var manualExpansion: Bool?

    private var containsSelection: Bool {
        selection.followedPane.map { selected in window.panes.contains { $0.id == selected } } ?? false
    }

    private var expanded: Bool {
        forceExpanded || (manualExpansion ?? containsSelection)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                explorerIndent(depth: 2)
                Button { manualExpansion = !expanded } label: {
                    hierarchyChevron(expanded)
                        .frame(width: 16, height: TendTheme.rowHeight)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                Button {
                    if let pane = window.panes.first { selectPane(pane.id) }
                    if !expanded { manualExpansion = true }
                } label: {
                    HStack(spacing: 6) {
                        Image(systemName: logicalWork == nil ? "macwindow" : "square.stack.3d.up")
                            .foregroundStyle(logicalWork == nil ? Color.secondary : Color.accentColor)
                            .frame(width: 17)
                        Text(logicalWork ?? (window.name.isEmpty ? window.windowID : window.name))
                            .font(.callout)
                            .fontWeight(logicalWork == nil ? .regular : .medium)
                            .lineLimit(1)
                        Spacer(minLength: 3)
                        Text(verbatim: "#\(window.index) · \(window.panes.count)")
                            .font(TendType.meta.monospacedDigit())
                            .foregroundStyle(.secondary)
                    }
                    .padding(.trailing, 7)
                    .frame(maxWidth: .infinity, minHeight: TendTheme.rowHeight, alignment: .leading)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .simultaneousGesture(
                    TapGesture(count: 2).onEnded {
                        if let pane = window.panes.first { openPinnedPane(pane.id) }
                    }
                )
            }
            .background(
                watchHighlightFill(
                    selection.highlight(
                        for: .fleetWindow(window.identity),
                        containsFollowedPane: containsSelection
                    )
                )
            )

            if expanded {
                ForEach(window.panes) { pane in
                    WatchPaneRow(
                        pane: pane,
                        highlight: selection.highlight(
                            for: .pane(pane.id),
                            containsFollowedPane: selection.followedPane == pane.id
                        ),
                        depth: 3,
                        selectPane: selectPane,
                        openPinnedPane: openPinnedPane
                    )
                }
            }
        }
    }
}

private struct WatchPaneRow: View {
    @EnvironmentObject private var model: AppModel
    let pane: TendWatchPane
    let highlight: WatchTreeHighlight
    let depth: Int
    let selectPane: (TendWatchPaneIdentity) -> Void
    let openPinnedPane: (TendWatchPaneIdentity) -> Void

    private var title: String {
        pane.pane.agentAlias.map { "@\($0)" }
            ?? pane.agent?.aiTitle
            ?? pane.pane.title.nonEmpty
            ?? pane.pane.currentCommand.nonEmpty
            ?? pane.pane.paneID
    }

    var body: some View {
        Button { selectPane(pane.id) } label: {
            HStack(spacing: 0) {
                explorerIndent(depth: depth)
                Color.clear.frame(width: 16)
                HStack(spacing: 6) {
                    Image(systemName: pane.agent == nil ? "terminal" : "person.crop.circle")
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                        .frame(width: 17)
                    Text(title)
                        .font(.callout)
                        .lineLimit(1)
                    TendUnreadDot(pane: pane.id) // WS-A
                    Spacer(minLength: 3)
                    WatchPaneRowTrailing(pane: pane)
                }
                .padding(.trailing, 8)
            }
            .frame(maxWidth: .infinity, minHeight: TendTheme.rowHeight, alignment: .leading)
            .background(watchHighlightFill(highlight))
            .overlay(alignment: .leading) { WatchFollowedMarker(highlight: highlight) }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(watchPaneHelp(pane))
        .simultaneousGesture(
            TapGesture(count: 2).onEnded { openPinnedPane(pane.id) }
        )
        .contextMenu {
            Button("Open") { selectPane(pane.id) }
            Button("Open Pinned") { openPinnedPane(pane.id) }
            if let target = TendAgentActionTarget(pane) {
                Divider()
                TendAgentActionMenuItems(model: model, target: target)
            }
        }
    }
}

/// The trailing edge of a pane row: an agent's status, said once. A status
/// that asks something of the reader (an error, a question, a rate limit)
/// gets its word; working and idle agents, the common case, keep only the
/// dot so the column stays quiet. A plain shell shows its command.
private struct WatchPaneRowTrailing: View {
    let pane: TendWatchPane

    var body: some View {
        if let status = pane.agentStatus {
            AgentStatusTag(status: status, showsLabel: status.priority <= 3)
        } else if let command = pane.pane.currentCommand.nonEmpty {
            Text(command)
                .font(TendType.meta.monospaced())
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
    }
}

/// The tmux address and latest activity, which a row leaves to the tooltip.
private func watchPaneHelp(_ pane: TendWatchPane) -> String {
    var lines = ["\(pane.host.alias) · \(pane.pane.session) › \(pane.pane.windowName) › \(pane.pane.paneID)"]
    if let status = pane.agentStatus {
        lines.append(status.label())
    } else if let command = pane.pane.currentCommand.nonEmpty {
        lines.append(command)
    }
    if let activity = watchPaneActivity(pane) {
        lines.append(String(activity.prefix(200)))
    }
    return lines.joined(separator: "\n")
}

/// One line on what the pane's agent last did, for rows with room for it.
func watchPaneActivity(_ pane: TendWatchPane) -> String? {
    pane.agent.flatMap(agentActivity)
}

/// One line on what an agent last did.
func agentActivity(_ agent: TendAgent) -> String? {
    let text = agent.recap?.nonEmpty
        ?? agent.lastNotification?.nonEmpty
        ?? agent.lastResponse?.nonEmpty
    return text.map { firstReadableLine($0) }
}

/// The first line of `text` with Markdown emphasis and list markers removed,
/// for a one-line preview.
func firstReadableLine(_ text: String) -> String {
    let line = text.split(whereSeparator: \.isNewline)
        .lazy
        .map { $0.trimmingCharacters(in: .whitespaces) }
        .first { !$0.isEmpty } ?? ""
    var cleaned = line.replacingOccurrences(of: "**", with: "")
        .replacingOccurrences(of: "__", with: "")
        .replacingOccurrences(of: "`", with: "")
    for marker in ["- ", "* ", "• ", "# ", "## ", "### "] where cleaned.hasPrefix(marker) {
        cleaned.removeFirst(marker.count)
    }
    return cleaned
}

/// A globally sortable Explore row. Unlike the topology tree it carries its
/// host and tmux path in the row, so removing host grouping does not remove the
/// context needed to identify the pane.
struct WatchFlatPaneRow: View {
    @EnvironmentObject private var model: AppModel
    let pane: TendWatchPane
    let highlight: WatchTreeHighlight
    let selectPane: (TendWatchPaneIdentity) -> Void
    let openPinnedPane: (TendWatchPaneIdentity) -> Void

    private var title: String {
        pane.pane.agentAlias.map { "@\($0)" }
            ?? pane.agent?.aiTitle
            ?? pane.pane.title.nonEmpty
            ?? pane.pane.currentCommand.nonEmpty
            ?? pane.pane.paneID
    }

    private var location: String {
        let window = pane.pane.windowName.nonEmpty ?? pane.pane.stableWindowID
        return "\(pane.host.alias) · \(pane.pane.session) › \(window)"
    }

    var body: some View {
        Button { selectPane(pane.id) } label: {
            HStack(alignment: .top, spacing: 8) {
                HostIdentityBadge(identity: pane.host, size: 22)
                    .padding(.top, 1)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(title)
                            .font(.callout.weight(.medium))
                            .lineLimit(1)
                        TendUnreadDot(pane: pane.id) // WS-A
                        Spacer(minLength: 3)
                        WatchPaneRowTrailing(pane: pane)
                    }
                    if let activity = watchPaneActivity(pane) {
                        Text(activity)
                            .font(TendType.detail)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                    Text(location)
                        .font(TendType.meta)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            .padding(.horizontal, 7)
            .padding(.vertical, 7)
            .frame(maxWidth: .infinity, minHeight: 48, alignment: .leading)
            .background(
                watchHighlightFill(highlight),
                in: RoundedRectangle(cornerRadius: 6)
            )
            .overlay(alignment: .leading) { WatchFollowedMarker(highlight: highlight) }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(watchPaneHelp(pane))
        .simultaneousGesture(
            TapGesture(count: 2).onEnded { openPinnedPane(pane.id) }
        )
        .contextMenu {
            Button("Open") { selectPane(pane.id) }
            Button("Open Pinned") { openPinnedPane(pane.id) }
            if let target = TendAgentActionTarget(pane) {
                Divider()
                TendAgentActionMenuItems(model: model, target: target)
            }
        }
    }
}

private func explorerIndent(depth: Int) -> some View {
    HStack(spacing: TendTheme.treeIndent - 1) {
        ForEach(0..<depth, id: \.self) { _ in
            Rectangle()
                .fill(Color(nsColor: .separatorColor).opacity(0.42))
                .frame(width: 1)
        }
    }
    .frame(width: CGFloat(depth) * TendTheme.treeIndent, height: TendTheme.rowHeight, alignment: .trailing)
}

private func hierarchyChevron(_ expanded: Bool) -> some View {
    Image(systemName: "chevron.right")
        .font(TendType.meta.weight(.semibold))
        .foregroundStyle(.secondary)
        .rotationEffect(.degrees(expanded ? 90 : 0))
        .frame(width: 12, height: 20)
}

struct FleetPaneInspector: View {
    let pane: TendWatchPane
    @ObservedObject var model: AppModel
    let compact: Bool
    let openInShell: () -> Void
    @State private var showsMetadata = false
    @State private var agentsExpanded = true
    /// Overview sections the reader opened past their preview length.
    @State private var expandedSections = Set<String>()

    private var window: TendWatchWindow? {
        model.executionSnapshot.watchWindow(containing: pane.id)
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: compact ? 12 : 16) {
                ViewThatFits(in: .horizontal) {
                    HStack(alignment: .top, spacing: 12) {
                        inspectorIdentity
                            .frame(minWidth: 250, alignment: .leading)
                        Spacer(minLength: 8)
                        inspectorActions
                            .fixedSize()
                    }
                    VStack(alignment: .leading, spacing: 10) {
                        inspectorIdentity
                        compactInspectorActions
                    }
                }

                selectedPaneOverview

                if let window, window.panes.contains(where: { $0.id != pane.id }) {
                    windowOverview(window)
                }

                if let error = model.attachError {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(TendType.detail)
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                }

            }
            .padding(compact ? 12 : 18)
            .frame(maxWidth: 1100, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .topLeading)
        }
        .onChange(of: pane.id) { _ in expandedSections = [] }
    }

    @ViewBuilder
    private func windowOverview(_ window: TendWatchWindow) -> some View {
        DisclosureGroup(isExpanded: $agentsExpanded) {
            VStack(spacing: 1) {
                // The selected pane is the subject of this whole inspector;
                // listing it again repeated its response word for word.
                ForEach(window.panes.filter { $0.id != pane.id }) { item in
                    Button {
                        model.selectWatchPane(item.id)
                    } label: {
                        HStack(alignment: .top, spacing: 9) {
                            Circle()
                                .fill(item.agent.map { $0.status.color } ?? Color.secondary)
                                .frame(width: 7, height: 7)
                                .padding(.top, 5)
                            VStack(alignment: .leading, spacing: 2) {
                                HStack(spacing: 6) {
                                    Text(overviewTitle(item))
                                        .font(.subheadline.weight(item.id == pane.id ? .semibold : .regular))
                                    if let agent = item.agent {
                                        Text(agent.status.label())
                                            .font(TendType.meta.weight(.medium))
                                            .foregroundStyle(agent.status.color)
                                    }
                                    Spacer(minLength: 4)
                                }
                                Text(overviewSummary(item) ?? String(localized: "No task summary has been reported for this pane yet."))
                                    .font(TendType.detail)
                                    .foregroundStyle(.secondary)
                                    .lineLimit(2)
                                    .multilineTextAlignment(.leading)
                            }
                        }
                        .padding(.horizontal, 10)
                        .padding(.vertical, 8)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(
                            item.id == pane.id ? Color.accentColor.opacity(0.1) : Color.primary.opacity(0.025),
                            in: RoundedRectangle(cornerRadius: 7)
                        )
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
            }
            .padding(.top, 9)
        } label: {
            HStack(spacing: 8) {
                Label("Others in this window", systemImage: "person.2")
                    .font(.headline)
                if let identity = windowWorkIdentity(window) {
                    Text(verbatim: "\(identity.workspaceID) / \(identity.workID)")
                        .font(TendType.detail.weight(.medium))
                        .foregroundStyle(Color.accentColor)
                        .padding(.horizontal, 7)
                        .padding(.vertical, 3)
                        .background(Color.accentColor.opacity(0.1), in: Capsule())
                }
                Spacer(minLength: 6)
                Text("\(window.panes.count) panes · \(window.panes.compactMap(\.agent).count) agents")
                    .font(TendType.detail.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
        }
        .padding(12)
        .background(Color.primary.opacity(0.025), in: RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous)
                .stroke(Color(nsColor: .separatorColor).opacity(0.4), lineWidth: 0.5)
        }
    }

    @ViewBuilder
    private var selectedPaneOverview: some View {
        if let agent = pane.agent {
            VStack(alignment: .leading, spacing: 10) {
                HStack(spacing: 8) {
                    Label("Current context", systemImage: "text.alignleft")
                        .font(.headline.weight(.semibold))
                    agentStatus(agent)
                }
                .font(TendType.detail)

                if let summary = agent.recap?.nonEmpty,
                   let response = agent.lastResponse?.nonEmpty,
                   response != summary {
                    ViewThatFits(in: .horizontal) {
                        HStack(alignment: .top, spacing: 12) {
                            overviewSection(
                                String(localized: "Summary"),
                                systemImage: "list.bullet.rectangle",
                                summary,
                                lineLimit: compact ? 3 : 4
                            )
                            .frame(maxWidth: .infinity, alignment: .topLeading)
                            overviewSection(
                                String(localized: "Latest response"),
                                systemImage: "text.bubble",
                                response,
                                lineLimit: compact ? 4 : 6
                            )
                            .frame(maxWidth: .infinity, alignment: .topLeading)
                        }
                        VStack(alignment: .leading, spacing: 10) {
                            overviewSection(
                                String(localized: "Summary"),
                                systemImage: "list.bullet.rectangle",
                                summary,
                                lineLimit: compact ? 3 : 4
                            )
                            overviewSection(
                                String(localized: "Latest response"),
                                systemImage: "text.bubble",
                                response,
                                lineLimit: compact ? 4 : 6
                            )
                        }
                    }
                } else if let summary = agent.recap?.nonEmpty {
                    overviewSection(
                        String(localized: "Summary"),
                        systemImage: "list.bullet.rectangle",
                        summary,
                        lineLimit: compact ? 3 : 5
                    )
                } else if let response = agent.lastResponse?.nonEmpty {
                    overviewSection(
                        String(localized: "Latest response"),
                        systemImage: "text.bubble",
                        response,
                        lineLimit: compact ? 4 : 6
                    )
                }

                if let activity = latestActivity(agent) {
                    overviewSection(
                        activity.title,
                        systemImage: activity.systemImage,
                        activity.source,
                        lineLimit: compact ? 2 : 3
                    )
                }
            }
        } else {
            Label(
                "No agent session is currently associated with this pane.",
                systemImage: "person.crop.circle.badge.questionmark"
            )
            .font(TendType.detail)
            .foregroundStyle(.secondary)
        }
    }

    private func overviewSection(
        _ title: String,
        systemImage: String,
        _ source: String,
        lineLimit: Int
    ) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 6) {
                Label(title, systemImage: systemImage)
                    .font(TendType.detail.weight(.semibold))
                    .foregroundStyle(.secondary)
                Spacer(minLength: 4)
                Button {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(source, forType: .string)
                } label: {
                    Image(systemName: "doc.on.doc")
                        .frame(width: 24, height: 24)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Copy \(title.lowercased())")
            }
            MarkdownContent(
                source: source,
                lineLimit: expandedSections.contains(title) ? nil : lineLimit,
                selectable: expandedSections.contains(title),
                font: TendType.body
            )
            if source.count > lineLimit * 70 || source.filter(\.isNewline).count >= lineLimit {
                Button(expandedSections.contains(title) ? "Show less" : "Show more") {
                    if expandedSections.contains(title) {
                        expandedSections.remove(title)
                    } else {
                        expandedSections.insert(title)
                    }
                }
                .buttonStyle(.plain)
                .font(TendType.detail.weight(.medium))
                .foregroundStyle(Color.accentColor)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 8))
    }

    private func latestActivity(_ agent: TendAgent) -> (title: String, systemImage: String, source: String)? {
        if let notice = agent.lastNotification?.nonEmpty,
           notice != agent.recap,
           notice != agent.lastResponse {
            return (String(localized: "Latest notice"), "bell", notice)
        }
        if agent.recap == nil,
           agent.lastResponse == nil,
           let prompt = agent.lastPrompt?.nonEmpty {
            return (String(localized: "Latest activity"), "clock.arrow.circlepath", humanReadablePrompt(prompt))
        }
        return nil
    }

    private func metadataRow(_ label: LocalizedStringKey, _ value: String) -> some View {
        GridRow {
            Text(label)
                .foregroundStyle(.secondary)
            Text(value)
                .fontDesign(.monospaced)
        }
    }

    private func windowWorkIdentity(_ window: TendWatchWindow) -> TendWorkIdentity? {
        let identities = Set(window.panes.compactMap(\.pane.workIdentity))
        return identities.count == 1 ? identities.first : nil
    }

    private func overviewTitle(_ item: TendWatchPane) -> String {
        item.pane.agentAlias.map { "@\($0)" }
            ?? item.agent?.aiTitle?.nonEmpty
            ?? item.pane.title.nonEmpty
            ?? item.pane.currentCommand.nonEmpty
            ?? item.pane.paneID
    }

    private func overviewSummary(_ item: TendWatchPane) -> String? {
        guard let agent = item.agent else { return item.pane.currentPath.nonEmpty }
        return agent.lastResponse?.nonEmpty
            ?? agent.recap?.nonEmpty
            ?? agent.lastNotification?.nonEmpty
            ?? agent.lastPrompt?.nonEmpty.map(humanReadablePrompt)
    }

    private func humanReadablePrompt(_ prompt: String) -> String {
        if prompt.hasPrefix("[tend] Claimed;") {
            return String(localized: "A collaboration request is waiting for this agent.")
        }
        if prompt.hasPrefix("[tend:req_") {
            if prompt.contains("Completed reply") {
                return String(localized: "A collaborator reply is ready for this agent.")
            }
            if prompt.contains("New ") && prompt.contains(" request") {
                return String(localized: "A collaboration request is waiting for this agent.")
            }
            return String(localized: "Recent Tend collaboration activity.")
        }
        if prompt.hasPrefix("<task-notification>") {
            return String(localized: "A background task reported an update.")
        }
        return prompt
    }

    private var inspectorIdentity: some View {
        HStack(alignment: .top, spacing: 10) {
            HostIdentityBadge(identity: pane.host, size: 36)
            VStack(alignment: .leading, spacing: 2) {
                Text(pane.agent?.aiTitle ?? pane.pane.agentAlias.map { "@\($0)" } ?? pane.pane.title.nonEmpty ?? pane.pane.currentCommand)
                    .font(.system(size: 14, weight: .semibold))
                    .lineLimit(2)
                HStack(spacing: 7) {
                    Text(pane.host.alias)
                    if let agent = pane.agent {
                        Text(verbatim: "·")
                        HStack(spacing: 4) {
                            TendAgentLogo(kind: agent.kind, size: 13)
                            Text(verbatim: TendAgentMark.productName(forKind: agent.kind))
                        }
                        Text(verbatim: "·")
                        // The breadcrumb bar above carries the reset time
                        // and the auto-resume action for a rate limit.
                        AgentStatusTag(status: agent.status, detailed: !agent.status.isLimited)
                    }
                    if let identity = pane.pane.workIdentity {
                        Text(verbatim: "·")
                        Text(verbatim: "\(identity.workspaceID) / \(identity.workID)")
                            .foregroundStyle(Color.accentColor)
                    }
                }
                .font(TendType.detail.weight(.medium))
                .foregroundStyle(.secondary)
                .lineLimit(1)
            }
        }
    }

    private var inspectorActions: some View {
        HStack(spacing: 8) {
            metadataButton(label: "Details")
            Button {
                openInShell()
            } label: {
                Label("Open in Shell", systemImage: "rectangle.on.rectangle")
            }
        }
    }

    private var compactInspectorActions: some View {
        HStack(spacing: 8) {
            metadataButton(label: "Info")
            Button {
                openInShell()
            } label: {
                Label("Shell", systemImage: "rectangle.on.rectangle")
            }
        }
    }

    private func metadataButton(label: LocalizedStringKey) -> some View {
        Button {
            showsMetadata.toggle()
        } label: {
            Label(label, systemImage: "info.circle")
        }
        .popover(isPresented: $showsMetadata, arrowEdge: .bottom) {
            VStack(alignment: .leading, spacing: 12) {
                Text("Execution location")
                    .font(.headline)
                Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 8) {
                    metadataRow("Host", pane.host.alias)
                    metadataRow("Session", pane.pane.session.nonEmpty ?? pane.pane.stableSessionID)
                    metadataRow("Window", pane.pane.windowName.nonEmpty ?? pane.pane.stableWindowID)
                    metadataRow("Pane", "\(pane.pane.paneID) · \(pane.pane.currentCommand)")
                    if let path = pane.pane.currentPath.nonEmpty {
                        metadataRow("Directory", path)
                    }
                }
                .font(TendType.detail)
                .textSelection(.enabled)
            }
            .padding(16)
            .frame(minWidth: 420, maxWidth: 560, alignment: .leading)
        }
    }

    @ViewBuilder
    private func agentStatus(_ agent: TendAgent) -> some View {
        // Status sits in the identity line and usage in the breadcrumb bar;
        // repeating them here made each read three times.
        if let modelName = agent.model {
            Text(modelName)
                .foregroundStyle(.secondary)
        }
    }
}

private extension String {
    var nonEmpty: String? { isEmpty ? nil : self }
}
