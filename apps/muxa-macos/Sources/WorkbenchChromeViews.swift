// The workbench frame: editor tabs and the status bar.
import GhosttyTerminal
import SwiftUI
import UniformTypeIdentifiers

struct WorkbenchWindowPresenter: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let view = NSView(frame: .zero)
        revealWindow(for: view, remainingAttempts: 30)
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {}

    private func revealWindow(for view: NSView, remainingAttempts: Int) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { [weak view] in
            guard let view else { return }
            if let window = view.window {
                window.identifier = NSUserInterfaceItemIdentifier("muxa.main-workbench")
                window.isRestorable = false
                window.makeKeyAndOrderFront(nil)
                NSApp.activate(ignoringOtherApps: true)
            } else if remainingAttempts > 0 {
                revealWindow(for: view, remainingAttempts: remainingAttempts - 1)
            }
        }
    }
}

struct WorkspaceTabBar: View {
    @ObservedObject var model: AppModel
    @ObservedObject var tabs: MuxaWorkbenchTabs
    let groupID: UUID
    /// The title bar row's height: the strip is the window's title bar.
    let height: CGFloat
    /// The window's actions, carried by the last group's strip only.
    let accessory: AnyView?
    var leadingInset: CGFloat = 0
    @Environment(\.openWindow) private var openWindow
    @ObservedObject private var dragSession = MuxaTabDragSession.shared
    /// Each tab's frame in the strip, for placing a dragged tab.
    @State private var tabFrames: [MuxaSidebarSelection: CGRect] = [:]

    private var group: MuxaWorkbenchTabs.Group? {
        tabs.group(id: groupID)
    }

    private var stripSpace: String { "tab-strip-\(groupID)" }

    /// Aligned with the group's tabs; nil for one not laid out yet.
    private var orderedTabFrames: [CGRect?] {
        (group?.tabs ?? []).map { tabFrames[$0] }
    }

    /// Where the insertion bar sits while a dragged tab hovers this strip.
    private var insertionX: CGFloat? {
        guard case .strip(groupID, let index)? = dragSession.target else { return nil }
        let frames = orderedTabFrames
        if frames.indices.contains(index), let frame = frames[index] { return frame.minX }
        return frames.compactMap { $0 }.last?.maxX ?? 0
    }

    var body: some View {
        HStack(spacing: 0) {
            if leadingInset > 0 {
                WindowDragArea().frame(width: leadingInset)
            }
            GeometryReader { strip in
                ScrollViewReader { proxy in
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 0) {
                        tabItems
                        // The strip is also the window's title bar: the space
                        // after the last tab moves the window. It lives inside
                        // the scroll view, which would otherwise take the click.
                        WindowDragArea()
                            .frame(minWidth: 24, maxWidth: .infinity, maxHeight: .infinity)
                    }
                    .frame(minWidth: strip.size.width, maxHeight: .infinity, alignment: .leading)
                    .coordinateSpace(name: stripSpace)
                    .onPreferenceChange(TabFramesKey.self) { tabFrames = $0 }
                    .overlay(alignment: .leading) {
                        if let insertionX {
                            TabInsertionIndicator(x: insertionX)
                        }
                    }
                    .onDrop(
                        of: [MuxaTabDragSession.tabType],
                        delegate: TabStripDropDelegate(
                            groupID: groupID,
                            orderedFrames: orderedTabFrames,
                            tabs: tabs,
                            session: dragSession,
                            didDrop: { selection, _ in model.activateEditor(selection) }
                        )
                    )
                }
                .onChange(of: group?.active) { active in
                    if let active { proxy.scrollTo(active, anchor: .trailing) }
                }
                .onAppear { if let active = group?.active { proxy.scrollTo(active, anchor: .trailing) } }
                }
            }

            if let active = group?.active {
                HStack(spacing: 1) {
                    if group?.preview == active {
                        Button {
                            tabs.pin(active, groupID: groupID)
                        } label: {
                            Image(systemName: "pin")
                        }
                        .help("Pin preview tab")
                    }

                    Button {
                        splitRight(active)
                    } label: {
                        Image(systemName: "rectangle.split.2x1")
                    }
                    .help("Open to the Side")

                    if let route = active.moduleRoute {
                        Button {
                            openWindow(value: route)
                        } label: {
                            Image(systemName: "macwindow.badge.plus")
                        }
                        .help("Open in New Window")
                    }
                }
                .buttonStyle(.muxaIcon)
                .padding(.leading, 6)
            }

            if let accessory {
                MuxaTheme.border(colorScheme)
                    .frame(width: 1, height: 16)
                    .padding(.horizontal, 6)
                accessory
            }
        }
        .padding(.trailing, 6)
        .font(.system(size: 12))
        .foregroundStyle(.secondary)
        .frame(height: height)
        // The strip's bottom border sits under the tabs, so the active tab,
        // filled with the editor's color, reads as joined to the editor.
        .background(alignment: .bottom) {
            ZStack(alignment: .bottom) {
                MuxaTheme.tabStrip(colorScheme)
                WindowDragArea()
                MuxaTheme.border(colorScheme).frame(height: 1)
                    .allowsHitTesting(false)
            }
        }
    }

    @ViewBuilder
    private var tabItems: some View {
        ForEach(group?.tabs ?? [], id: \.self) { selection in
            EditorTab(
                selection: selection,
                title: Self.title(for: selection, model: model),
                systemImage: tabIcon(for: selection),
                stateColor: agentStatus(for: selection)?.color,
                unreadPane: unreadPane(for: selection), // WS-A
                active: group?.active == selection,
                preview: group?.preview == selection,
                activate: { activate(selection) },
                pin: { tabs.pin(selection, groupID: groupID) },
                close: { close(selection) },
                closeOthers: {
                    tabs.closeOthers(keeping: selection, groupID: groupID)
                    model.activateEditor(selection)
                },
                splitRight: { splitRight(selection) },
                moveToWindow: selection.moduleRoute.map { route in
                    { openWindow(value: route) }
                }
            )
            .id(selection)
            .background {
                GeometryReader { proxy in
                    Color.clear.preference(
                        key: TabFramesKey.self,
                        value: [selection: proxy.frame(in: .named(stripSpace))]
                    )
                }
            }
            // The tab being dragged dims in place, as in VS Code.
            .opacity(dragSession.drag?.selection == selection && dragSession.drag?.sourceGroupID == groupID ? 0.45 : 1)
            .onDrag { dragSession.begin(selection, from: groupID, in: tabs) }
        }
    }

    @Environment(\.colorScheme) private var colorScheme

    private func activate(_ selection: MuxaSidebarSelection) {
        tabs.activate(selection, groupID: groupID)
        model.activateEditor(selection)
    }

    private func close(_ selection: MuxaSidebarSelection) {
        let next = tabs.close(selection, groupID: groupID)
        model.activateEditor(next)
    }

    private func splitRight(_ selection: MuxaSidebarSelection) {
        let target = tabs.splitRight(selection: selection, from: groupID)
        tabs.activate(selection, groupID: target)
        model.activateEditor(selection)
    }

    static func title(for selection: MuxaSidebarSelection, model: AppModel) -> String {
        switch selection {
        case .file(let location):
            location.name
        case .workBoard:
            String(localized: "Home")
        case .watch:
            String(localized: "Live Watch")
        case .inbox:
            String(localized: "Inbox")
        case .ask:
            String(localized: "Ask")
        case .work(let identity):
            model.workGroups.first { $0.identity == identity }?.title ?? identity.workID
        case .agent(let id):
            model.hostedAgents.first { $0.id == id }.map {
                $0.agent.aiTitle
                    ?? $0.pane?.agentAlias.map { "@\($0)" }
                    ?? $0.agent.kind.replacingOccurrences(of: "_", with: " ")
            } ?? String(localized: "Agent")
        case .host(let id):
            model.fleetHosts.first { $0.id == id }?.alias ?? String(localized: "Host")
        case .fleetSession(let id):
            model.executionSnapshot.watchSession(id: id).map {
                "\($0.hostAlias) · \($0.name.isEmpty ? $0.sessionID : $0.name)"
            } ?? String(localized: "Session")
        case .fleetWindow(let id):
            model.executionSnapshot.watchWindow(id: id).map {
                $0.name.isEmpty ? $0.windowID : $0.name
            } ?? String(localized: "Window")
        case .shell(let id):
            model.sessions.first { $0.id == id }.map { $0.displayName ?? $0.id }
                ?? String(localized: "Shell")
        case .pane(let id):
            model.executionSnapshot.watchPane(id: id).map {
                "\($0.host.alias) · \($0.pane.windowName.isEmpty ? $0.pane.paneID : $0.pane.windowName)"
            } ?? String(localized: "Pane")
        }
    }

    /// The live state of the agent a tab shows, for the dot on its icon
    /// (Orca marks agent tabs the same way).
    private func agentStatus(for selection: MuxaSidebarSelection) -> MuxaAgentStatus? {
        switch selection {
        case .pane(let id): model.executionSnapshot.watchPane(id: id)?.agent?.status
        case .agent(let id): model.hostedAgents.first { $0.id == id }?.agent.status
        default: nil
        }
    }

    // WS-A: the pane whose unread dot a tab carries.
    private func unreadPane(for selection: MuxaSidebarSelection) -> MuxaWatchPaneIdentity? {
        switch selection {
        case .pane(let id): id
        case .agent(let id): model.hostedAgents.first { $0.id == id }.flatMap(MuxaAgentAttentionCenter.paneIdentity)
        default: nil
        }
    }

    private func tabIcon(for selection: MuxaSidebarSelection) -> String {
        switch selection {
        case .file: "doc"
        case .workBoard: "rectangle.3.group"
        case .watch: "waveform.path.ecg.rectangle"
        case .inbox: "tray.full"
        case .ask: "sparkles"
        case .work: "square.stack.3d.up"
        case .agent: "person.crop.circle"
        case .host: "network"
        case .fleetSession: "square.3.layers.3d"
        case .fleetWindow: "macwindow"
        case .shell: "terminal"
        case .pane: "terminal.fill"
        }
    }
}

private struct EditorTab: View {
    let selection: MuxaSidebarSelection
    let title: String
    let systemImage: String
    var stateColor: Color? = nil
    var unreadPane: MuxaWatchPaneIdentity? = nil // WS-A
    let active: Bool
    let preview: Bool
    let activate: () -> Void
    let pin: () -> Void
    let close: () -> Void
    let closeOthers: () -> Void
    let splitRight: () -> Void
    let moveToWindow: (() -> Void)?
    @State private var hovering = false

    var body: some View {
        HStack(spacing: 4) {
            Button(action: activate) {
                HStack(spacing: 6) {
                    Image(systemName: systemImage)
                        .font(.system(size: 12))
                        .foregroundStyle(active ? Color.accentColor : Color.secondary)
                        .overlay(alignment: .bottomTrailing) {
                            if let stateColor {
                                Circle()
                                    .fill(stateColor)
                                    .frame(width: 6, height: 6)
                                    .overlay(Circle().stroke(MuxaTheme.tabStrip(colorScheme), lineWidth: 1))
                                    .offset(x: 3, y: 2)
                            }
                        }
                    Text(title)
                        .italic(preview)
                        .lineLimit(1)
                }
                .padding(.leading, 12)
                .frame(maxHeight: .infinity, alignment: .leading)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .simultaneousGesture(
                TapGesture(count: 2).onEnded {
                    activate()
                    pin()
                }
            )

            Spacer(minLength: 0)

            // The close button's slot is always reserved so tabs never
            // change width as the pointer moves across them.
            Button(action: close) {
                Image(systemName: "xmark")
                    .font(.system(size: 9, weight: .bold))
            }
            .buttonStyle(.muxaIcon(size: 18))
            .opacity(active || hovering ? 1 : 0)
            // WS-A: an unseen agent shows a dot in the close slot, like an
            // editor's unsaved-changes dot, until the tab is hovered.
            .overlay {
                if let unreadPane, !active, !hovering {
                    MuxaUnreadDot(pane: unreadPane, size: 7).allowsHitTesting(false)
                }
            }
            .accessibilityLabel("Close \(title)")
            .help("Close \(title)")
            .padding(.trailing, 6)
        }
        .frame(minWidth: 120, maxWidth: 220, maxHeight: .infinity)
        .foregroundStyle(active ? Color.primary : Color.secondary)
        // A view, not a ShapeStyle: a style background would spread into the
        // title bar's safe area above the tab.
        .background {
            Rectangle().fill(
                active
                    ? MuxaTheme.editor(colorScheme)
                    : hovering ? MuxaTheme.hover(colorScheme) : Color.clear
            )
        }
        .overlay(alignment: .top) {
            Rectangle()
                .fill(active ? Color.accentColor : Color.clear)
                .frame(height: 1.5)
        }
        .overlay(alignment: .trailing) {
            MuxaTheme.border(colorScheme).frame(width: 1)
        }
        .onHover { hovering = $0 }
        .help(preview ? Text("Preview — double-click to keep open") : Text(title))
        .contextMenu {
            if preview { Button("Keep Open", action: pin) }
            Button("Close", action: close)
            Button("Close Others", action: closeOthers)
            Divider()
            Button("Open to the Side", action: splitRight)
            if let moveToWindow {
                Button("Open in New Window", action: moveToWindow)
            }
        }
    }

    @Environment(\.colorScheme) private var colorScheme
}

struct WorkbenchStatusBar: View {
    @ObservedObject var model: AppModel
    let openPane: (MuxaWatchPaneIdentity) -> Void

    @Environment(\.colorScheme) private var colorScheme

    /// Healthy, the bar is neutral like the rest of the chrome and only the
    /// leading chip carries the accent; a problem turns the whole bar its
    /// color so it can't be missed.
    private var healthy: Bool {
        if case .connected = model.connectionState { return true }
        return false
    }

    var body: some View {
        HStack(spacing: 0) {
            connectionItems
                .padding(.horizontal, 9)
                .frame(maxHeight: .infinity)
                .background(healthy ? Color.accentColor : Color.clear)
                .foregroundStyle(.white)

            Spacer(minLength: 12)

            HStack(spacing: 14) {
                // WS-C usage: hidden while unhealthy, when the figures are stale.
                if let error = model.agentActionError {
                    Button {
                        model.agentActionError = nil
                    } label: {
                        Label(error, systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.orange)
                    }
                    .buttonStyle(.plain)
                    .help("Click to dismiss")
                } else if let notice = model.agentActionNotice {
                    Label(notice, systemImage: "checkmark.circle.fill")
                        .foregroundStyle(.green)
                        .lineLimit(1)
                }
                if healthy {
                    MuxaSwapStatusItem(swaps: model.swaps, openPane: openPane)
                    MuxaUsageStatusItems(agents: model.hostedAgents, openPane: openPane)
                }
                Label("\(model.fleetHosts.count) hosts", systemImage: "server.rack")
                Label("\(model.hostedAgents.count) agents", systemImage: "person.2")
                Label("\(model.sessions.lazy.filter { !$0.exited }.count) shells", systemImage: "terminal")
            }
            .labelStyle(StatusBarLabelStyle())
            .padding(.trailing, 10)
            .foregroundStyle(healthy ? Color.secondary : Color.white)
        }
        .font(.system(size: 12))
        .lineLimit(1)
        .frame(maxWidth: .infinity, minHeight: MuxaTheme.statusBarHeight, maxHeight: MuxaTheme.statusBarHeight)
        .background(healthy ? MuxaTheme.sideBar(colorScheme) : statusColor)
        .overlay(alignment: .top) {
            MuxaTheme.border(colorScheme).frame(height: 1)
        }
    }

    @ViewBuilder
    private var connectionItems: some View {
        HStack(spacing: 8) {
            switch model.connectionState {
            case .connecting:
                ProgressView()
                    .controlSize(.mini)
                    .tint(.white)
                Text("Connecting to muxad…")
            case .connected:
                Label("muxad", systemImage: "bolt.horizontal.fill")
                    .labelStyle(StatusBarLabelStyle())
            case .upgradeRequired(let message):
                Label("muxad upgrade required", systemImage: "arrow.triangle.2.circlepath")
                Text(message).lineLimit(1)
                Button("Use Bundled muxad") {
                    model.isConfirmingDaemonReplacement = true
                }
                .buttonStyle(.plain)
                .underline()
            case .failed(let message):
                Label("Disconnected", systemImage: "exclamationmark.triangle.fill")
                Text(message).lineLimit(1)
                Button("Retry", action: model.retryConnection)
                    .buttonStyle(.plain)
                    .underline()
            }
        }
    }

    private var statusColor: Color {
        switch model.connectionState {
        case .connected: Color.accentColor
        case .connecting: Color(nsColor: .systemGray)
        case .upgradeRequired: Color.orange
        case .failed: Color.red
        }
    }
}

private struct StatusBarLabelStyle: LabelStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: 4) {
            configuration.icon.font(.system(size: 11))
            configuration.title
        }
    }
}

private struct SidebarConnectionStatus: View {
    let state: AppModel.ConnectionState
    let retry: () -> Void
    let useBundledDaemon: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            switch state {
            case .connecting:
                Label {
                    Text("Connecting to muxad…")
                } icon: {
                    ProgressView().controlSize(.small)
                }
            case .connected:
                Label("muxad connected", systemImage: "circle.fill")
                    .symbolRenderingMode(.palette)
                    .foregroundStyle(.green, .secondary)
            case .upgradeRequired(let message):
                statusMessage(
                    title: "muxad upgrade required",
                    message: message,
                    systemImage: "arrow.triangle.2.circlepath.circle.fill"
                )
                Button("Use Bundled muxad", action: useBundledDaemon)
                    .buttonStyle(.muxaPrimary)
                    .controlSize(.small)
            case .failed(let message):
                statusMessage(
                    title: "Connection failed",
                    message: message,
                    systemImage: "exclamationmark.triangle.fill"
                )
                Button("Retry", action: retry)
                    .controlSize(.small)
            }
        }
        .font(MuxaType.detail)
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder
    private func statusMessage(
        title: LocalizedStringKey,
        message: String,
        systemImage: String
    ) -> some View {
        Label(title, systemImage: systemImage)
            .foregroundStyle(.orange)
        Text(message)
            .font(MuxaType.meta)
            .foregroundStyle(.secondary)
            .lineLimit(3)
            .fixedSize(horizontal: false, vertical: true)
    }
}
