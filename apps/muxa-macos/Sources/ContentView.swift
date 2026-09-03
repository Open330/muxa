import GhosttyTerminal
import SwiftUI

struct ContentView: View {
    @EnvironmentObject private var model: AppModel
    @StateObject private var tabs = MuxaWorkbenchTabs()
    @State private var isCommandPalettePresented = false

    var body: some View {
        VStack(spacing: 0) {
            HSplitView {
                MuxaSidebar(
                    model: model,
                    openPinnedSession: { id in
                        model.selectWatchSession(id)
                        tabs.openPinned(.fleetSession(id))
                    },
                    openPinnedPane: { id in
                        model.selectWatchPane(id)
                        tabs.openPinned(.pane(id))
                    }
                )
                    .frame(minWidth: 260, idealWidth: 300, maxWidth: 380)
                editorRegion
                    .frame(minWidth: 560)
            }
            .toolbar {
                ToolbarItemGroup {
                    Button {
                        model.presentWorkStart()
                    } label: {
                        Label("Start Work", systemImage: "play.square.stack")
                    }
                    .disabled(!model.isConnected || model.isStartingWork)

                    Button {
                        model.createShell()
                    } label: {
                        Label("New shell", systemImage: "plus")
                    }
                    .disabled(!model.isConnected || model.isCreatingSession)

                    Button {
                        Task { await model.refresh() }
                    } label: {
                        Label("Refresh", systemImage: "arrow.clockwise")
                    }
                    .disabled(!model.isConnected)

                    Button {
                        isCommandPalettePresented = true
                    } label: {
                        Label("Commands", systemImage: "command")
                    }
                    .keyboardShortcut("p", modifiers: [.command, .shift])

                    Button(role: .destructive) {
                        model.terminateSelectedSession()
                    } label: {
                        Label("Terminate", systemImage: "stop.circle")
                    }
                    .disabled(
                        !model.isConnected
                            || model.selectedSessionID == nil
                            || model.isTerminatingSession
                    )
                }
            }
            Divider()
            WorkbenchStatusBar(model: model)
        }
        .frame(minWidth: 920, minHeight: 580)
        .alert(
            "Replace the running muxad?",
            isPresented: $model.isConfirmingDaemonReplacement
        ) {
            Button("Cancel", role: .cancel) {}
            Button("Replace Daemon", role: .destructive) {
                model.replaceRunningDaemon()
            }
        } message: {
            Text(
                "This stops the muxad currently using the socket, disables older background services that could reclaim it, and starts the version bundled with Muxa. Native PTY sessions owned by the old daemon will end; tmux sessions will not be terminated."
            )
        }
        .sheet(isPresented: $isCommandPalettePresented) {
            CommandPaletteView(model: model, isPresented: $isCommandPalettePresented)
        }
        .sheet(isPresented: $model.isPresentingWorkStart) {
            WorkStartView(model: model, isPresented: $model.isPresentingWorkStart)
        }
        .sheet(isPresented: $model.isPresentingHostRegistration) {
            HostRegistrationView(model: model)
        }
        .task {
            model.activateEditor(tabs.focusedSelection)
            model.start()
        }
        .onChange(of: model.sidebarSelection) { selection in
            guard let selection, tabs.focusedSelection != selection else { return }
            tabs.openPreview(selection)
        }
        .onChange(of: model.workspaceRevision) { _ in
            tabs.prune(where: model.isSelectionAvailable)
            if model.sidebarSelection != tabs.focusedSelection {
                model.activateEditor(tabs.focusedSelection)
            }
        }
        .background(WorkbenchWindowPresenter())
        .focusedSceneValue(\.muxaEditorCommands, editorCommands)
    }

    @ViewBuilder
    private var editorRegion: some View {
        HSplitView {
            ForEach(tabs.groups) { group in
                VStack(spacing: 0) {
                    WorkspaceTabBar(model: model, tabs: tabs, groupID: group.id)
                    Divider()
                    detail(for: group.active, groupID: group.id)
                        .frame(
                            maxWidth: .infinity,
                            maxHeight: .infinity,
                            alignment: .topLeading
                        )
                }
                .frame(
                    minWidth: tabs.groups.count > 1 ? 420 : nil,
                    maxWidth: .infinity,
                    maxHeight: .infinity,
                    alignment: .topLeading
                )
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }

    @ViewBuilder
    private func detail(
        for selection: MuxaSidebarSelection?,
        groupID: UUID
    ) -> some View {
        switch selection {
        case .workBoard:
            WorkCommandCenterView(model: model)
        case .watch:
            NativeWatchView(model: model)
        case .inbox:
            MuxaOperatorInboxView(model: model)
        case .ask:
            MuxaAskView(model: model)
        case .work(let identity):
            if let work = model.workGroups.first(where: { $0.identity == identity }) {
                WorkDetailView(work: work, model: model)
                    .id(work.identity)
            } else {
                MuxaEmptyDetail(model: model)
            }
        case .agent(let id):
            if let participant = model.hostedAgents.first(where: { $0.id == id }) {
                FleetAgentDetailView(participant: participant, client: model.client)
                    .id(participant.id)
            } else {
                MuxaEmptyDetail(model: model)
            }
        case .host(let id):
            if let host = model.fleetHosts.first(where: { $0.id == id }) {
                FleetHostDetailView(host: host, model: model)
                    .id(host.id)
            } else {
                MuxaEmptyDetail(model: model)
            }
        case .fleetSession(let id):
            if let session = model.executionSnapshot.watchSession(id: id) {
                FleetSessionDetailView(session: session, model: model)
                    .id(session.id)
            } else {
                MuxaEmptyDetail(model: model)
            }
        case .fleetWindow(let id):
            if let window = model.executionSnapshot.watchWindow(id: id) {
                FleetWindowDetailView(window: window, model: model)
                    .id(window.id)
            } else {
                MuxaEmptyDetail(model: model)
            }
        case .shell(let id):
            if let session = model.sessions.first(where: { $0.id == id }) {
                TerminalPane(
                    client: model.client,
                    sessionID: session.id,
                    replayInitialHistory: session.hasBeenAttached == true,
                    onExit: { closeExitedShell(id: session.id, groupID: groupID) }
                )
                .id(session.id)
            } else {
                MuxaEmptyDetail(model: model)
            }
        case .pane(let id):
            if let pane = model.executionSnapshot.watchPane(id: id) {
                FleetPaneModuleView(pane: pane, model: model)
                    .id(pane.id)
            } else {
                MuxaEmptyDetail(model: model)
            }
        case nil:
            MuxaEmptyDetail(model: model)
        }
    }

    private func closeExitedShell(id: String, groupID: UUID) {
        let selection = MuxaSidebarSelection.shell(id)
        if tabs.focusedGroupID == groupID {
            model.activateEditor(tabs.close(selection, groupID: groupID))
        }
        Task { await model.refresh() }
    }

    private var editorCommands: MuxaEditorCommandActions {
        MuxaEditorCommandActions(
            close: {
                model.activateEditor(tabs.closeFocused())
            },
            next: {
                if let selection = tabs.activateRelative(1) {
                    model.activateEditor(selection)
                }
            },
            previous: {
                if let selection = tabs.activateRelative(-1) {
                    model.activateEditor(selection)
                }
            },
            splitRight: {
                guard let selection = tabs.focusedSelection else { return }
                let target = tabs.splitRight(
                    selection: selection,
                    from: tabs.focusedGroupID
                )
                tabs.activate(selection, groupID: target)
                model.activateEditor(selection)
            },
            pin: {
                guard let selection = tabs.focusedSelection else { return }
                tabs.pin(selection, groupID: tabs.focusedGroupID)
            }
        )
    }
}

/// SwiftUI can restore a menu-bar application's Window scene offscreen after
/// its content state has already loaded. Reveal the canonical workbench at the
/// point where its content is actually attached to an NSWindow.
private struct WorkbenchWindowPresenter: NSViewRepresentable {
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

private struct WorkspaceTabBar: View {
    @ObservedObject var model: AppModel
    @ObservedObject var tabs: MuxaWorkbenchTabs
    let groupID: UUID
    @Environment(\.openWindow) private var openWindow

    private var group: MuxaWorkbenchTabs.Group? {
        tabs.group(id: groupID)
    }

    var body: some View {
        HStack(spacing: 0) {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 0) {
                    ForEach(group?.tabs ?? [], id: \.self) { selection in
                        EditorTab(
                            selection: selection,
                            title: tabLabel(for: selection),
                            systemImage: tabIcon(for: selection),
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
                        .draggable(selection.tabIdentifier)
                        .dropDestination(for: String.self) { identifiers, _ in
                            guard let identifier = identifiers.first else { return false }
                            tabs.move(
                                tabIdentifier: identifier,
                                before: selection,
                                groupID: groupID
                            )
                            return true
                        }
                    }
                }
            }

            Divider().frame(height: 22)

            if let active = group?.active {
                if group?.preview == active {
                    Button {
                        tabs.pin(active, groupID: groupID)
                    } label: {
                        Image(systemName: "pin")
                            .frame(width: 27, height: 28)
                    }
                    .buttonStyle(.plain)
                    .help("Pin preview tab")
                }

                Button {
                    splitRight(active)
                } label: {
                    Image(systemName: "rectangle.split.2x1")
                        .frame(width: 27, height: 28)
                }
                .buttonStyle(.plain)
                .help("Open to the Side")

                if let route = active.moduleRoute {
                    Button {
                        openWindow(value: route)
                    } label: {
                        Image(systemName: "macwindow.badge.plus")
                            .frame(width: 27, height: 28)
                    }
                    .buttonStyle(.plain)
                    .help("Open in New Window")
                }

                Button {
                    close(active)
                } label: {
                    Image(systemName: "xmark")
                        .frame(width: 27, height: 28)
                }
                .buttonStyle(.plain)
                .help("Close Editor")
            }
        }
        .font(.system(size: 12))
        .foregroundStyle(.secondary)
        .frame(height: 35)
        .background(Color(nsColor: .controlBackgroundColor))
        .overlay(alignment: .bottom) {
            Rectangle()
                .fill(Color(nsColor: .separatorColor).opacity(0.55))
                .frame(height: 1)
        }
    }

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

    private func tabLabel(for selection: MuxaSidebarSelection) -> String {
        switch selection {
        case .workBoard:
            "Work Command Center"
        case .watch:
            "Live Watch"
        case .inbox:
            "Inbox"
        case .ask:
            "Ask"
        case .work(let identity):
            model.workGroups.first { $0.identity == identity }?.title ?? identity.workID
        case .agent(let id):
            model.hostedAgents.first { $0.id == id }.map {
                $0.agent.aiTitle
                    ?? $0.pane?.agentAlias.map { "@\($0)" }
                    ?? $0.agent.kind.replacingOccurrences(of: "_", with: " ")
            } ?? "Agent"
        case .host(let id):
            model.fleetHosts.first { $0.id == id }?.alias ?? "Host"
        case .fleetSession(let id):
            model.executionSnapshot.watchSession(id: id).map {
                "\($0.hostAlias) · \($0.name.isEmpty ? $0.sessionID : $0.name)"
            } ?? "Session"
        case .fleetWindow(let id):
            model.executionSnapshot.watchWindow(id: id).map {
                $0.name.isEmpty ? $0.windowID : $0.name
            } ?? "Window"
        case .shell(let id):
            model.sessions.first { $0.id == id }.map { $0.displayName ?? $0.id }
                ?? "Shell"
        case .pane(let id):
            model.executionSnapshot.watchPane(id: id).map {
                "\($0.host.alias) · \($0.pane.windowName.isEmpty ? $0.pane.paneID : $0.pane.windowName)"
            } ?? "Pane"
        }
    }

    private func tabIcon(for selection: MuxaSidebarSelection) -> String {
        switch selection {
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
        ZStack(alignment: .trailing) {
            Button(action: activate) {
                HStack(spacing: 7) {
                    Image(systemName: systemImage)
                        .foregroundStyle(active ? Color.accentColor : Color.secondary)
                    Text(title)
                        .italic(preview)
                        .lineLimit(1)
                    Spacer(minLength: 4)
                }
                .padding(.leading, 10)
                .padding(.trailing, 38)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .simultaneousGesture(
                TapGesture(count: 2).onEnded {
                    activate()
                    pin()
                }
            )

            if active || hovering {
                Button(action: close) {
                    Image(systemName: "xmark")
                        .font(.system(size: 10, weight: .semibold))
                        .frame(width: 30, height: 34)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .background(
                    active
                        ? MuxaSurfacePalette.editor(for: colorScheme)
                        : Color(nsColor: .controlBackgroundColor)
                )
                .accessibilityLabel("Close \(title)")
                .help("Close \(title)")
                .zIndex(2)
            }
        }
        .frame(minWidth: 132, maxWidth: 230, minHeight: 35, maxHeight: 35)
        .foregroundStyle(active ? Color.primary : Color.secondary)
        .background(
            active
                ? MuxaSurfacePalette.editor(for: colorScheme)
                : Color(nsColor: .controlBackgroundColor)
        )
        .overlay(alignment: .top) {
            Rectangle()
                .fill(active ? Color.accentColor : Color.clear)
                .frame(height: 2)
        }
        .overlay(alignment: .trailing) {
            Rectangle()
                .fill(Color(nsColor: .separatorColor).opacity(0.55))
                .frame(width: 1)
        }
        .onHover { hovering = $0 }
        .help(preview ? "Preview — double-click to keep open" : title)
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

private struct CommandPaletteView: View {
    private enum PaletteCommand: String, CaseIterable, Identifiable {
        case startWork = "Start configured Work"
        case workCommandCenter = "Open Work Command Center"
        case liveWatch = "Open native Live Watch"
        case ask = "Open global Ask"
        case newShell = "New native shell"
        case showWork = "Show managed work"
        case showShells = "Show native shells"
        case refresh = "Refresh workspace"

        var id: Self { self }

        var systemImage: String {
            switch self {
            case .startWork: "play.square.stack"
            case .workCommandCenter: "rectangle.3.group"
            case .liveWatch: "waveform.path.ecg.rectangle"
            case .ask: "sparkles"
            case .newShell: "plus.rectangle.on.rectangle"
            case .showWork: "square.stack.3d.up"
            case .showShells: "terminal"
            case .refresh: "arrow.clockwise"
            }
        }
    }

    @ObservedObject var model: AppModel
    @Binding var isPresented: Bool
    @State private var query = ""
    @FocusState private var searchFocused: Bool

    private var commands: [PaletteCommand] {
        guard !query.isEmpty else { return PaletteCommand.allCases }
        return PaletteCommand.allCases.filter {
            $0.rawValue.localizedCaseInsensitiveContains(query)
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: "command")
                    .foregroundStyle(.secondary)
                TextField("Type a command", text: $query)
                    .textFieldStyle(.plain)
                    .focused($searchFocused)
                    .onSubmit {
                        if let first = commands.first { run(first) }
                    }
            }
            .font(.title3)
            .padding(14)

            Divider()

            List(commands) { command in
                Button {
                    run(command)
                } label: {
                    Label(command.rawValue, systemImage: command.systemImage)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .disabled(
                    (command == .newShell && (!model.isConnected || model.isCreatingSession))
                        || (command == .startWork && (!model.isConnected || model.isStartingWork))
                )
            }
            .listStyle(.inset)
        }
        .frame(width: 520, height: 360)
        .onAppear { searchFocused = true }
    }

    private func run(_ command: PaletteCommand) {
        switch command {
        case .startWork:
            model.presentWorkStart()
        case .workCommandCenter:
            model.select(.workBoard)
        case .liveWatch:
            model.select(.watch)
        case .ask:
            model.select(.ask)
        case .newShell:
            model.createShell()
        case .showWork:
            model.show(.work)
        case .showShells:
            model.show(.shells)
        case .refresh:
            Task { await model.refresh() }
        }
        isPresented = false
    }
}

private struct MuxaSidebar: View {
    private enum StatusScope: String, CaseIterable, Identifiable {
        case all = "All"
        case attention = "Attention"
        case active = "Active"

        var id: Self { self }

        var systemImage: String {
            switch self {
            case .all: "line.3.horizontal.decrease.circle"
            case .attention: "exclamationmark.circle"
            case .active: "bolt.circle"
            }
        }
    }

    private enum ExploreGrouping: String, CaseIterable, Identifiable {
        case host
        case status
        case none

        var id: Self { self }

        var title: String {
            switch self {
            case .host: "Host tree"
            case .status: "Status groups"
            case .none: "No groups"
            }
        }

        var systemImage: String {
            switch self {
            case .host: "server.rack"
            case .status: "circle.grid.2x2"
            case .none: "list.bullet"
            }
        }
    }

    private enum ExploreStatusBucket: String, CaseIterable, Identifiable {
        case attention
        case active
        case idle
        case shell

        var id: Self { self }

        var title: String {
            switch self {
            case .attention: "Needs attention"
            case .active: "Active"
            case .idle: "Idle agents"
            case .shell: "Shell panes"
            }
        }
    }

    private struct ExplorePaneGroup: Identifiable {
        let bucket: ExploreStatusBucket
        let panes: [MuxaWatchPane]

        var id: ExploreStatusBucket { bucket }
    }

    private enum ExploreSort: String, CaseIterable, Identifiable {
        case topology
        case recent
        case myPrompt
        case agentActivity

        var id: Self { self }

        var title: String {
            switch self {
            case .topology: "Topology"
            case .recent: "Latest activity"
            case .myPrompt: "My latest prompt"
            case .agentActivity: "Agent latest update"
            }
        }

        var compactTitle: String {
            switch self {
            case .topology: "Topology"
            case .recent: "Last activity"
            case .myPrompt: "My prompt"
            case .agentActivity: "Agent update"
            }
        }

        var systemImage: String {
            switch self {
            case .topology: "point.3.connected.trianglepath.dotted"
            case .recent: "clock.arrow.circlepath"
            case .myPrompt: "person.crop.circle.badge.clock"
            case .agentActivity: "sparkles"
            }
        }
    }

    @ObservedObject var model: AppModel
    let openPinnedSession: (MuxaWatchSessionIdentity) -> Void
    let openPinnedPane: (MuxaWatchPaneIdentity) -> Void
    @Environment(\.colorScheme) private var colorScheme
    @State private var filterText = ""
    @State private var statusScope: StatusScope = .all
    @AppStorage("muxa.explore.sort") private var exploreSort: ExploreSort = .topology
    @AppStorage("muxa.explore.grouping") private var exploreGrouping: ExploreGrouping = .host

    private var background: Color {
        MuxaSurfacePalette.sidebar(for: colorScheme)
    }

    var body: some View {
        ZStack {
            background.ignoresSafeArea()

            HStack(spacing: 0) {
                SidebarActivityRail(model: model)

                Divider()

                VStack(spacing: 0) {
                    HStack {
                        Label(model.sidebarMode.title, systemImage: model.sidebarMode.systemImage)
                            .font(.headline)
                        Spacer()
                        if model.sidebarMode == .watch {
                            Button {
                                model.select(.watch)
                            } label: {
                                Image(systemName: "rectangle.on.rectangle")
                            }
                            .buttonStyle(.borderless)
                            .help("Open Live Watch")
                            Button {
                                model.presentHostRegistration()
                            } label: {
                                Image(systemName: "plus")
                            }
                            .buttonStyle(.borderless)
                            .help("Register SSH Host")
                        }
                        Text(sidebarCountLabel)
                            .font(.caption.monospacedDigit())
                            .foregroundStyle(.secondary)
                    }
                    .padding(.horizontal, 12)
                    .padding(.top, 12)
                    .padding(.bottom, 7)

                    HStack(spacing: 6) {
                        Image(systemName: "magnifyingglass")
                            .foregroundStyle(.secondary)
                        TextField("Filter \(model.sidebarMode.title.lowercased())", text: $filterText)
                            .textFieldStyle(.plain)
                        Menu {
                            Picker("Status", selection: $statusScope) {
                                ForEach(StatusScope.allCases) { scope in
                                    Label(scope.rawValue, systemImage: scope.systemImage)
                                        .tag(scope)
                                }
                            }
                        } label: {
                            Image(systemName: statusScope.systemImage)
                                .foregroundStyle(statusScope == .all ? Color.secondary : Color.accentColor)
                        }
                        .menuStyle(.borderlessButton)
                        .fixedSize()
                        .help("Filter by status")
                    }
                    .padding(.horizontal, 9)
                    .padding(.vertical, 6)
                    .background(Color.primary.opacity(0.06), in: RoundedRectangle(cornerRadius: 7))
                    .padding(.horizontal, 10)
                    .padding(.bottom, 3)

                    if model.sidebarMode == .watch {
                        HStack(spacing: 6) {
                            Menu {
                                Picker("Group", selection: $exploreGrouping) {
                                    ForEach(ExploreGrouping.allCases) { grouping in
                                        Label(grouping.title, systemImage: grouping.systemImage)
                                            .tag(grouping)
                                    }
                                }
                            } label: {
                                Label(exploreGrouping.title, systemImage: exploreGrouping.systemImage)
                                    .lineLimit(1)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }
                            .menuStyle(.borderlessButton)
                            .help("Group Explore by \(exploreGrouping.title.lowercased())")

                            Menu {
                                Picker("Order", selection: $exploreSort) {
                                    ForEach(ExploreSort.allCases) { order in
                                        Label(order.title, systemImage: order.systemImage)
                                            .tag(order)
                                    }
                                }
                            } label: {
                                Label(exploreSort.compactTitle, systemImage: exploreSort.systemImage)
                                    .lineLimit(1)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }
                            .menuStyle(.borderlessButton)
                            .help("Order Explore by \(exploreSort.title.lowercased())")
                        }
                        .font(.caption)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 4)
                    }

                    List {
                        contextualRows
                    }
                    .listStyle(.sidebar)
                    .scrollContentBackground(.hidden)
                    .background(Color.clear)
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .onChange(of: model.sidebarMode) { _ in
            filterText = ""
            statusScope = .all
        }
    }

    private var sidebarCount: Int {
        switch model.sidebarMode {
        case .work: model.workGroups.count
        case .watch: model.executionSnapshot.watchHosts.reduce(0) { $0 + $1.paneCount }
        case .inbox: inboxBadgeCount
        case .shells: model.sessions.lazy.filter { !$0.exited }.count
        }
    }

    private var filteredSidebarCount: Int {
        switch model.sidebarMode {
        case .work: filteredWorkGroups.count
        case .watch: filteredWatchHosts.reduce(0) { $0 + $1.paneCount }
        case .inbox: inboxBadgeCount
        case .shells: filteredSessions.count
        }
    }

    private var sidebarCountLabel: String {
        filterText.isEmpty && statusScope == .all
            ? "\(sidebarCount)"
            : "\(filteredSidebarCount)/\(sidebarCount)"
    }

    private var filteredWorkGroups: [MuxaWorkGroup] {
        model.workGroups.filter {
            matchesFilter([$0.title, $0.workspaceID, $0.pipelineLabel])
                && matchesScope(attention: $0.attentionCount > 0, active: $0.workingCount > 0)
        }
    }

    private var filteredSessions: [MuxaSession] {
        model.sessions.filter {
            guard !$0.exited else { return false }
            return matchesFilter([$0.displayName, $0.id])
                && matchesScope(
                    attention: false,
                    active: true
                )
        }
    }

    private var attentionAgents: [MuxaHostedAgent] {
        model.hostedAgents.filter {
            ["waiting_input", "waiting_choice", "blocked", "error", "failed"]
                .contains($0.agent.state)
        }
    }

    private var filteredAttentionAgents: [MuxaHostedAgent] {
        attentionAgents.filter { participant in
            matchesFilter([
                participant.host.alias,
                participant.agent.aiTitle,
                participant.agent.agentSessionID,
                participant.agent.recap,
                participant.agent.lastResponse,
                participant.pane?.session,
                participant.pane?.windowName,
                participant.pane?.agentAlias,
            ]) && matchesScope(attention: true, active: false)
        }
    }

    private var inboxBadgeCount: Int {
        let commandAttention = model.operatorMessages.lazy.filter {
            $0.needsReply || $0.hasUnreadReply
        }.count
        let runningAsk = model.askEntries.lazy.filter { $0.status == "running" }.count
        return commandAttention + runningAsk + attentionAgents.count
    }

    private var filteredWatchHosts: [MuxaWatchHost] {
        let filtered: [MuxaWatchHost] = model.executionSnapshot.watchHosts.compactMap { hostGroup in
            let sessions = hostGroup.sessions.compactMap { session -> MuxaWatchSession? in
                let windows = session.windows.compactMap { window -> MuxaWatchWindow? in
                    let panes = window.panes.filter { pane in
                        let state = pane.agent?.state
                        return matchesFilter([
                            pane.host.alias,
                            pane.pane.session,
                            pane.pane.windowName,
                            pane.pane.paneID,
                            pane.pane.currentCommand,
                            pane.pane.currentPath,
                            pane.pane.agentAlias,
                            pane.agent?.aiTitle,
                            pane.agent?.agentSessionID,
                        ]) && matchesScope(
                            attention: state.map {
                                ["waiting_input", "waiting_choice", "blocked", "error", "failed"]
                                    .contains($0)
                            } ?? false,
                            active: state.map { ["working", "starting"].contains($0) } ?? false
                        )
                    }
                    guard !panes.isEmpty else { return nil }
                    return MuxaWatchWindow(
                        hostAlias: window.hostAlias,
                        socket: window.socket,
                        sessionID: window.sessionID,
                        windowID: window.windowID,
                        name: window.name,
                        index: window.index,
                        panes: panes
                    )
                }
                guard !windows.isEmpty else { return nil }
                return MuxaWatchSession(
                    hostAlias: session.hostAlias,
                    socket: session.socket,
                    sessionID: session.sessionID,
                    name: session.name,
                    windows: windows
                )
            }
            guard !sessions.isEmpty else { return nil }
            return MuxaWatchHost(host: hostGroup.host, sessions: sessions)
        }
        return sortedWatchHosts(filtered)
    }

    private var filteredWatchPanes: [MuxaWatchPane] {
        sortedPanes(
            filteredWatchHosts
                .flatMap(\.sessions)
                .flatMap(\.windows)
                .flatMap(\.panes)
        )
    }

    private var filteredStatusPaneGroups: [ExplorePaneGroup] {
        let grouped = Dictionary(grouping: filteredWatchPanes) { statusBucket(for: $0) }
        return ExploreStatusBucket.allCases.compactMap { bucket in
            guard let panes = grouped[bucket], !panes.isEmpty else { return nil }
            return ExplorePaneGroup(bucket: bucket, panes: panes)
        }
    }

    private func statusBucket(for pane: MuxaWatchPane) -> ExploreStatusBucket {
        guard let state = pane.agent?.state else { return .shell }
        if ["waiting_input", "waiting_choice", "blocked", "error", "failed"].contains(state) {
            return .attention
        }
        if ["working", "starting"].contains(state) { return .active }
        return .idle
    }

    private func sortedWatchHosts(_ hosts: [MuxaWatchHost]) -> [MuxaWatchHost] {
        let rebuilt = hosts.map { host in
            let sessions = host.sessions.map { session in
                let windows = session.windows.map { window in
                    MuxaWatchWindow(
                        hostAlias: window.hostAlias,
                        socket: window.socket,
                        sessionID: window.sessionID,
                        windowID: window.windowID,
                        name: window.name,
                        index: window.index,
                        panes: sortedPanes(window.panes)
                    )
                }.sorted { ordered($0, before: $1) }
                return MuxaWatchSession(
                    hostAlias: session.hostAlias,
                    socket: session.socket,
                    sessionID: session.sessionID,
                    name: session.name,
                    windows: windows
                )
            }.sorted { ordered($0, before: $1) }
            return MuxaWatchHost(host: host.host, sessions: sessions)
        }
        return rebuilt.sorted { ordered($0, before: $1) }
    }

    private func sortedPanes(_ panes: [MuxaWatchPane]) -> [MuxaWatchPane] {
        panes.sorted { left, right in
            let leftDate = activityDate(for: left)
            let rightDate = activityDate(for: right)
            if exploreSort != .topology, leftDate != rightDate { return leftDate > rightDate }
            if left.host.alias != right.host.alias {
                if left.host.local != right.host.local { return left.host.local }
                return left.host.alias.localizedStandardCompare(right.host.alias) == .orderedAscending
            }
            if left.pane.session != right.pane.session {
                return left.pane.session.localizedStandardCompare(right.pane.session) == .orderedAscending
            }
            let leftWindowIndex = Int(left.pane.windowIndex) ?? Int.max
            let rightWindowIndex = Int(right.pane.windowIndex) ?? Int.max
            if leftWindowIndex != rightWindowIndex { return leftWindowIndex < rightWindowIndex }
            let leftIndex = Int(left.pane.paneIndex) ?? Int.max
            let rightIndex = Int(right.pane.paneIndex) ?? Int.max
            if leftIndex != rightIndex { return leftIndex < rightIndex }
            return left.pane.paneID.localizedStandardCompare(right.pane.paneID) == .orderedAscending
        }
    }

    private func ordered(_ left: MuxaWatchWindow, before right: MuxaWatchWindow) -> Bool {
        let leftDate = latestDate(in: left.panes)
        let rightDate = latestDate(in: right.panes)
        if exploreSort != .topology, leftDate != rightDate { return leftDate > rightDate }
        let leftIndex = Int(left.index) ?? Int.max
        let rightIndex = Int(right.index) ?? Int.max
        if leftIndex != rightIndex { return leftIndex < rightIndex }
        return left.name.localizedStandardCompare(right.name) == .orderedAscending
    }

    private func ordered(_ left: MuxaWatchSession, before right: MuxaWatchSession) -> Bool {
        let leftDate = latestDate(in: left.windows.flatMap(\.panes))
        let rightDate = latestDate(in: right.windows.flatMap(\.panes))
        if exploreSort != .topology, leftDate != rightDate { return leftDate > rightDate }
        return left.name.localizedStandardCompare(right.name) == .orderedAscending
    }

    private func ordered(_ left: MuxaWatchHost, before right: MuxaWatchHost) -> Bool {
        let leftDate = latestDate(in: left.sessions.flatMap(\.windows).flatMap(\.panes))
        let rightDate = latestDate(in: right.sessions.flatMap(\.windows).flatMap(\.panes))
        if exploreSort != .topology, leftDate != rightDate { return leftDate > rightDate }
        if left.host.local != right.host.local { return left.host.local }
        return left.host.alias.localizedStandardCompare(right.host.alias) == .orderedAscending
    }

    private func latestDate(in panes: [MuxaWatchPane]) -> Date {
        panes.map(activityDate(for:)).max() ?? .distantPast
    }

    private func activityDate(for pane: MuxaWatchPane) -> Date {
        guard let agent = pane.agent else { return .distantPast }
        switch exploreSort {
        case .topology:
            return .distantPast
        case .recent:
            return max(parsedDate(agent.lastPromptAt), parsedDate(agent.lastActivityAt))
        case .myPrompt:
            return parsedDate(agent.lastPromptAt)
        case .agentActivity:
            return parsedDate(agent.lastActivityAt)
        }
    }

    private func parsedDate(_ value: String?) -> Date {
        guard let value else { return .distantPast }
        let fractional = Date.ISO8601FormatStyle(includingFractionalSeconds: true)
        if let date = try? fractional.parse(value) { return date }
        return (try? Date.ISO8601FormatStyle().parse(value)) ?? .distantPast
    }

    private func matchesFilter(_ values: [String?]) -> Bool {
        guard !filterText.isEmpty else { return true }
        return values.compactMap { $0 }.contains {
            $0.localizedCaseInsensitiveContains(filterText)
        }
    }

    private func matchesScope(attention: Bool, active: Bool) -> Bool {
        switch statusScope {
        case .all: true
        case .attention: attention
        case .active: active
        }
    }

    @ViewBuilder
    private var contextualRows: some View {
        switch model.sidebarMode {
        case .work:
            Section("Workspace") {
                Button {
                    model.select(.workBoard)
                } label: {
                    WorkBoardRow(workCount: model.workGroups.count, agentCount: model.hostedAgents.count)
                }
                .buttonStyle(.plain)
                .listRowBackground(
                    model.sidebarSelection == .workBoard
                        ? Color.accentColor.opacity(0.14) : Color.clear
                )
            }
            Section("Managed work") {
                if model.workGroups.isEmpty {
                    SidebarEmptyRow(title: "No managed work", systemImage: "square.stack.3d.up.slash")
                } else if filteredWorkGroups.isEmpty {
                    SidebarEmptyRow(title: "No matching work", systemImage: "line.3.horizontal.decrease.circle")
                } else {
                    ForEach(filteredWorkGroups) { work in
                        Button {
                            model.select(.work(work.identity))
                        } label: {
                            WorkRow(work: work)
                        }
                        .buttonStyle(.plain)
                        .listRowBackground(
                            model.sidebarSelection == .work(work.identity)
                                ? Color.accentColor.opacity(0.14) : Color.clear
                        )
                    }
                }
            }
        case .watch:
            watchContextualRows
        case .inbox:
            Section("Operator") {
                Button {
                    model.select(.inbox)
                } label: {
                    OperatorInboxRow(
                        commands: model.operatorMessages.count,
                        attention: inboxBadgeCount
                    )
                }
                .buttonStyle(.plain)
                Button {
                    model.select(.ask)
                } label: {
                    GlobalAskRow(
                        conversationCount: model.askConversations.lazy.filter {
                            $0.agent == model.askAgent
                        }.count,
                        agent: model.askAgent
                    )
                }
                .buttonStyle(.plain)
            }
            Section("Needs attention") {
                if attentionAgents.isEmpty {
                    SidebarEmptyRow(title: "Nothing needs attention", systemImage: "checkmark.circle")
                } else if filteredAttentionAgents.isEmpty {
                    SidebarEmptyRow(title: "No matching requests", systemImage: "line.3.horizontal.decrease.circle")
                } else {
                    ForEach(filteredAttentionAgents) { participant in
                        Button {
                            if let pane = participant.pane {
                                model.selectWatchPane(
                                    MuxaWatchPaneIdentity(
                                        hostAlias: participant.host.alias,
                                        socket: pane.endpointSocket,
                                        paneID: pane.paneID
                                    )
                                )
                            } else {
                                model.select(.agent(participant.id))
                            }
                        } label: {
                            InboxAgentRow(participant: participant)
                        }
                        .buttonStyle(.plain)
                    }
                }
            }
        case .shells:
            Section("Native shells") {
                if model.sessions.isEmpty {
                    SidebarEmptyRow(title: "No native shells", systemImage: "terminal")
                } else if filteredSessions.isEmpty {
                    SidebarEmptyRow(title: "No matching shells", systemImage: "line.3.horizontal.decrease.circle")
                } else {
                    ForEach(filteredSessions) { session in
                        Button {
                            model.select(.shell(session.id))
                        } label: {
                            SessionRow(session: session)
                        }
                        .buttonStyle(.plain)
                        .listRowBackground(
                            model.sidebarSelection == .shell(session.id)
                                ? Color.accentColor.opacity(0.14) : Color.clear
                        )
                    }
                }
            }
        }
    }

    @ViewBuilder
    private var watchContextualRows: some View {
        if model.executionSnapshot.watchHosts.allSatisfy({ $0.paneCount == 0 }) {
            Section("Execution topology") {
                SidebarEmptyRow(title: "No panes detected", systemImage: "terminal")
            }
        } else if filteredWatchPanes.isEmpty {
            Section("Execution topology") {
                SidebarEmptyRow(
                    title: "No matching panes",
                    systemImage: "line.3.horizontal.decrease.circle"
                )
            }
        } else {
            switch exploreGrouping {
            case .host:
                Section("Execution topology") {
                    ForEach(filteredWatchHosts) { host in
                        WatchHostTree(
                            group: host,
                            selection: watchTreeSelection,
                            selectHost: { model.select(.host($0)) },
                            selectSession: model.selectWatchSession,
                            openPinnedSession: openPinnedSession,
                            selectPane: model.selectWatchPane,
                            openPinnedPane: openPinnedPane,
                            forceExpanded: !filterText.isEmpty || statusScope != .all,
                            workLabel: watchWorkLabel
                        )
                        .listRowInsets(EdgeInsets(top: 2, leading: 4, bottom: 2, trailing: 4))
                        .listRowBackground(Color.clear)
                    }
                }
            case .status:
                ForEach(filteredStatusPaneGroups) { group in
                    Section("\(group.bucket.title) · \(group.panes.count)") {
                        ForEach(group.panes) { pane in
                            WatchFlatPaneRow(
                                pane: pane,
                                highlight: watchTreeSelection.highlight(
                                    for: .pane(pane.id),
                                    containsFollowedPane: model.watchSelection == pane.id
                                ),
                                selectPane: model.selectWatchPane,
                                openPinnedPane: openPinnedPane
                            )
                            .listRowInsets(EdgeInsets(top: 1, leading: 5, bottom: 1, trailing: 5))
                            .listRowBackground(Color.clear)
                        }
                    }
                }
            case .none:
                Section("All panes · \(filteredWatchPanes.count)") {
                    ForEach(filteredWatchPanes) { pane in
                        WatchFlatPaneRow(
                            pane: pane,
                            highlight: watchTreeSelection.highlight(
                                for: .pane(pane.id),
                                containsFollowedPane: model.watchSelection == pane.id
                            ),
                            selectPane: model.selectWatchPane,
                            openPinnedPane: openPinnedPane
                        )
                        .listRowInsets(EdgeInsets(top: 1, leading: 5, bottom: 1, trailing: 5))
                        .listRowBackground(Color.clear)
                    }
                }
            }
        }
    }

    private func watchWorkLabel(_ window: MuxaWatchWindow) -> String? {
        let stamped = Set(window.panes.compactMap(\.pane.workIdentity))
        if stamped.count == 1, let identity = stamped.first {
            return "\(identity.workspaceID) › \(identity.workID)"
        }
        guard window.hostAlias == model.fleetHosts.first(where: { $0.local })?.alias else {
            return nil
        }
        return model.pipelineRuns.first(where: { $0.windowID == window.windowID }).map {
            "\($0.identity.workspaceID) › \($0.identity.workID)"
        }
    }

    private var watchTreeSelection: WatchTreeSelection {
        WatchTreeSelection(editor: model.sidebarSelection, followedPane: model.watchSelection)
    }
}

private struct SidebarActivityRail: View {
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(spacing: 6) {
            ForEach(MuxaSidebarMode.allCases) { mode in
                Button {
                    model.show(mode)
                } label: {
                    Image(systemName: mode.systemImage)
                        .font(.system(size: 17, weight: .medium))
                        .frame(width: 34, height: 34)
                        .contentShape(Rectangle())
                        .background(
                            model.sidebarMode == mode ? Color.accentColor.opacity(0.18) : Color.clear,
                            in: RoundedRectangle(cornerRadius: 7, style: .continuous)
                        )
                        .overlay(alignment: .leading) {
                            if model.sidebarMode == mode {
                                Capsule()
                                    .fill(Color.accentColor)
                                    .frame(width: 3, height: 20)
                                    .offset(x: -5)
                            }
                        }
                        .overlay(alignment: .topTrailing) {
                            let count = attentionCount(for: mode)
                            if count > 0 {
                                Text(count > 99 ? "99+" : "\(count)")
                                    .font(.system(size: 8, weight: .bold, design: .rounded))
                                    .foregroundStyle(.white)
                                    .padding(.horizontal, count > 9 ? 4 : 3)
                                    .frame(minWidth: 14, minHeight: 14)
                                    .background(.orange, in: Capsule())
                                    .offset(x: 5, y: -4)
                            }
                        }
                }
                .buttonStyle(.plain)
                .foregroundStyle(model.sidebarMode == mode ? Color.accentColor : Color.secondary)
                .help(mode.title)
                .accessibilityLabel(mode.title)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 7)
        .padding(.top, 10)
        .frame(width: 49)
        .frame(maxHeight: .infinity)
    }

    private func attentionCount(for mode: MuxaSidebarMode) -> Int {
        switch mode {
        case .work:
            return model.workGroups.lazy.filter { $0.attentionCount > 0 }.count
        case .watch:
            return 0
        case .inbox:
            let agentAttention = model.hostedAgents.lazy.filter {
                ["waiting_input", "waiting_choice", "blocked", "error", "failed"]
                    .contains($0.agent.state)
            }.count
            let commandAttention = model.operatorMessages.lazy.filter {
                $0.needsReply || $0.hasUnreadReply
            }.count
            let askAttention = model.askEntries.lazy.filter { $0.status == "running" }.count
            return agentAttention + commandAttention + askAttention
        case .shells:
            return 0
        }
    }
}

private struct SidebarEmptyRow: View {
    let title: String
    let systemImage: String

    var body: some View {
        Label(title, systemImage: systemImage)
            .foregroundStyle(.secondary)
            .listRowBackground(Color.clear)
    }
}

private struct GlobalAskRow: View {
    let conversationCount: Int
    let agent: String

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "sparkles")
                .foregroundStyle(.tint)
                .frame(width: 18)
            VStack(alignment: .leading, spacing: 2) {
                Text("Global Ask")
                    .fontWeight(.medium)
                Text("@\(agent) · \(conversationCount) \(conversationCount == 1 ? "conversation" : "conversations")")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 4)
            Image(systemName: "chevron.right")
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.tertiary)
        }
        .padding(.horizontal, 4)
        .frame(maxWidth: .infinity, minHeight: 34, alignment: .leading)
        .contentShape(Rectangle())
    }
}

private struct OperatorInboxRow: View {
    let commands: Int
    let attention: Int

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: attention > 0 ? "tray.full.fill" : "tray.full")
                .foregroundStyle(attention > 0 ? Color.orange : Color.accentColor)
                .frame(width: 18)
            VStack(alignment: .leading, spacing: 2) {
                Text("Operator Inbox")
                    .fontWeight(.medium)
                Text(attention > 0 ? "\(attention) waiting or new" : "\(commands) sent commands")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 4)
            Image(systemName: "chevron.right")
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.tertiary)
        }
        .padding(.horizontal, 4)
        .frame(maxWidth: .infinity, minHeight: 38, alignment: .leading)
        .contentShape(Rectangle())
    }
}

private struct InboxAgentRow: View {
    let participant: MuxaHostedAgent

    private var title: String {
        participant.pane?.agentAlias.map { "@\($0)" }
            ?? participant.agent.aiTitle
            ?? participant.agent.kind.replacingOccurrences(of: "_", with: " ")
    }

    private var summary: String {
        presentText(participant.agent.recap)
            ?? presentText(participant.agent.lastNotification)
            ?? presentText(participant.agent.lastPrompt)
            ?? "Waiting for input"
    }

    var body: some View {
        HStack(alignment: .top, spacing: 9) {
            Circle()
                .fill(agentStateColor(participant.agent.state))
                .frame(width: 8, height: 8)
                .padding(.top, 5)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(title)
                        .fontWeight(.medium)
                        .lineLimit(1)
                    Spacer(minLength: 4)
                    Text(participant.host.alias)
                        .font(.caption2.monospaced())
                        .foregroundStyle(.tertiary)
                }
                Text(summary)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .multilineTextAlignment(.leading)
            }
        }
        .padding(.horizontal, 4)
        .padding(.vertical, 5)
        .frame(maxWidth: .infinity, minHeight: 42, alignment: .leading)
        .contentShape(Rectangle())
    }
}

private struct WorkbenchStatusBar: View {
    @ObservedObject var model: AppModel

    var body: some View {
        HStack(spacing: 10) {
            switch model.connectionState {
            case .connecting:
                ProgressView()
                    .controlSize(.mini)
                    .tint(.white)
                Text("Connecting to muxad…")
            case .connected:
                Label("muxad", systemImage: "circle.fill")
                    .symbolRenderingMode(.monochrome)
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

            Spacer(minLength: 12)

            Text("\(model.fleetHosts.count) hosts")
            Text("\(model.hostedAgents.count) agents")
            Text("\(model.sessions.lazy.filter { !$0.exited }.count) shells")
        }
        .font(.system(size: 10.5, weight: .medium))
        .foregroundStyle(.white)
        .padding(.horizontal, 9)
        .frame(maxWidth: .infinity, minHeight: 23, maxHeight: 23)
        .background(statusColor)
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
                    .buttonStyle(.borderedProminent)
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
        .font(.caption)
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder
    private func statusMessage(
        title: String,
        message: String,
        systemImage: String
    ) -> some View {
        Label(title, systemImage: systemImage)
            .foregroundStyle(.orange)
        Text(message)
            .font(.caption2)
            .foregroundStyle(.secondary)
            .lineLimit(3)
            .fixedSize(horizontal: false, vertical: true)
    }
}

private struct WorkBoardRow: View {
    let workCount: Int
    let agentCount: Int

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "rectangle.3.group.fill")
                .foregroundStyle(.tint)
                .frame(width: 18)
            VStack(alignment: .leading, spacing: 2) {
                Text("Work Command Center")
                Text("\(workCount) work · \(agentCount) agents")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            Spacer()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
        .padding(.vertical, 3)
    }
}

private struct NativeWatchRow: View {
    let hostCount: Int
    let paneCount: Int
    let attentionCount: Int

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "waveform.path.ecg.rectangle.fill")
                .foregroundStyle(attentionCount > 0 ? Color.orange : Color.accentColor)
                .frame(width: 18)
            VStack(alignment: .leading, spacing: 2) {
                Text("Live Watch")
                Text("\(hostCount) hosts · \(paneCount) panes")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if attentionCount > 0 {
                Text("\(attentionCount)")
                    .font(.caption2.bold().monospacedDigit())
                    .foregroundStyle(.white)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(.orange, in: Capsule())
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
        .padding(.vertical, 3)
    }
}

private struct SessionRow: View {
    let session: MuxaSession

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: session.exited ? "terminal.fill" : "terminal")
                .foregroundStyle(session.exited ? .secondary : .primary)
                .frame(width: 16)
            VStack(alignment: .leading, spacing: 2) {
                Text(session.displayName ?? session.id)
                    .lineLimit(1)
                HStack(spacing: 6) {
                    if let pid = session.pid { Text("pid \(pid)") }
                    if session.attachedClients > 0 { Text("\(session.attachedClients) attached") }
                    if session.exited { Text("exited \(session.exitStatus ?? 0)") }
                }
                .font(.caption2)
                .foregroundStyle(.secondary)
                .monospacedDigit()
            }
            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
        .padding(.vertical, 2)
    }
}

private struct WorkRow: View {
    let work: MuxaWorkGroup

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: work.attentionCount > 0 ? "exclamationmark.square.fill" : "square.stack.3d.up.fill")
                .foregroundStyle(work.attentionCount > 0 ? .orange : .accentColor)
                .frame(width: 16)
            VStack(alignment: .leading, spacing: 2) {
                Text(work.title)
                    .lineLimit(1)
                HStack(spacing: 5) {
                    Text(work.workspaceID)
                    if !work.hostAliases.isEmpty {
                        Text("· \(work.hostAliases.joined(separator: ", "))")
                    }
                }
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 4)
            Text(
                work.pipelineRun == nil
                    ? "\(work.participants.count) agents"
                    : "\(work.completedCount)/\(work.totalCount)"
            )
                .font(.caption2.monospacedDigit())
                .foregroundStyle(work.attentionCount > 0 ? .orange : .secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
        .padding(.vertical, 2)
    }
}

private struct FleetAgentRow: View {
    let participant: MuxaHostedAgent

    private var title: String {
        participant.agent.aiTitle
            ?? participant.pane?.agentAlias.map { "@\($0)" }
            ?? participant.agent.kind.replacingOccurrences(of: "_", with: " ")
    }

    private var executionLocation: String {
        guard let pane = participant.pane else { return "no pane binding" }
        let window = pane.windowName.isEmpty ? pane.stableWindowID : pane.windowName
        return "\(pane.session) › \(window) › \(pane.paneID)"
    }

    var body: some View {
        HStack(spacing: 10) {
            HostIdentityBadge(identity: participant.host, size: 26)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 5) {
                    Circle()
                        .fill(agentStateColor(participant.agent.state))
                        .frame(width: 7, height: 7)
                    Text(title)
                        .lineLimit(1)
                }
                Text("\(participant.host.alias) · \(participant.agent.agentSessionID)")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                Text(executionLocation)
                    .font(.caption2.monospaced())
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
        .padding(.vertical, 2)
        .help("Independent agent session: \(participant.agent.agentSessionID)")
    }
}

private struct WorkDetailView: View {
    let work: MuxaWorkGroup
    @ObservedObject var model: AppModel
    @Environment(\.colorScheme) private var colorScheme

    private let columns = [
        GridItem(.adaptive(minimum: 250, maximum: 390), spacing: 12, alignment: .top),
    ]

    private let metricColumns = [
        GridItem(.adaptive(minimum: 112, maximum: 180), spacing: 12),
    ]

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                VStack(alignment: .leading, spacing: 5) {
                    Text(work.workspaceID.uppercased())
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    Text(work.title)
                        .font(.largeTitle.weight(.semibold))
                    HStack(spacing: 8) {
                        Label(work.pipelineLabel, systemImage: "point.3.connected.trianglepath.dotted")
                        if let generation = work.pipelineRun?.generation {
                            Text("generation \(generation)")
                        } else {
                            Text("observed from tmux metadata")
                        }
                        if !work.hostAliases.isEmpty {
                            Text("·")
                            Label(work.hostAliases.joined(separator: ", "), systemImage: "network")
                        }
                    }
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    if let cwd = work.cwd {
                        Text(cwd)
                            .font(.caption.monospaced())
                            .foregroundStyle(.tertiary)
                            .textSelection(.enabled)
                    }
                }

                LazyVGrid(columns: metricColumns, alignment: .leading, spacing: 12) {
                    WorkMetric(
                        title: "Participants",
                        value: "\(work.participants.count)",
                        color: .accentColor
                    )
                    WorkMetric(
                        title: "Running",
                        value: "\(work.workingCount)",
                        color: .blue
                    )
                    WorkMetric(
                        title: "Needs attention",
                        value: "\(work.attentionCount)",
                        color: .orange
                    )
                    if work.pipelineRun != nil {
                        WorkMetric(
                            title: "Pipeline done",
                            value: "\(work.completedCount)/\(work.totalCount)",
                            color: .green
                        )
                    }
                }

                WorkPromptComposer(work: work, model: model)

                VStack(alignment: .leading, spacing: 10) {
                    Text("Collaborators")
                        .font(.title2.weight(.semibold))
                    LazyVGrid(columns: columns, alignment: .leading, spacing: 12) {
                        ForEach(work.participants) { participant in
                            WorkParticipantCard(
                                participant: participant,
                                desired: work.desiredAgent(for: participant),
                                openAgent: {
                                    if let pane = participant.pane {
                                        model.selectWatchPane(
                                            MuxaWatchPaneIdentity(
                                                hostAlias: participant.host.alias,
                                                socket: pane.endpointSocket,
                                                paneID: pane.paneID
                                            )
                                        )
                                    } else {
                                        model.select(.agent(participant.id))
                                    }
                                }
                            )
                        }
                        if let run = work.pipelineRun {
                            ForEach(unboundDesiredAgents(run: run), id: \.alias) { desired in
                                PipelinePlaceholderCard(
                                    desired: desired,
                                    state: run.aliases[desired.alias]
                                )
                            }
                        }
                    }
                }
            }
            .padding(28)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(MuxaSurfacePalette.workspace(for: colorScheme).ignoresSafeArea())
    }

    private func unboundDesiredAgents(run: MuxaPipelineRun) -> [MuxaDesiredAgent] {
        let bound = Set(work.participants.compactMap { work.desiredAgent(for: $0)?.alias })
        return run.desired.filter { !bound.contains($0.alias) }
    }
}

private struct WorkMetric: View {
    let title: String
    let value: String
    let color: Color

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(value)
                .font(.title2.weight(.semibold).monospacedDigit())
                .foregroundStyle(color)
            Text(title)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .frame(minWidth: 90, alignment: .leading)
    }
}

private struct WorkParticipantCard: View {
    let participant: MuxaHostedAgent
    let desired: MuxaDesiredAgent?
    let openAgent: () -> Void

    private var title: String {
        participant.pane?.agentAlias.map { "@\($0)" }
            ?? desired.map { "@\($0.alias)" }
            ?? participant.agent.kind.replacingOccurrences(of: "_", with: " ")
    }

    private var summary: String? {
        participant.agent.recap
            ?? participant.agent.aiTitle
            ?? participant.agent.lastNotification
            ?? participant.agent.lastPrompt
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(.headline)
                        .lineLimit(1)
                    Text("\(participant.host.alias) · \(desired?.role ?? participant.agent.kind)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer()
                Label(agentStateLabel(participant.agent.state), systemImage: "circle.fill")
                    .labelStyle(.titleAndIcon)
                    .font(.caption.weight(.medium))
                    .foregroundStyle(agentStateColor(participant.agent.state))
                    .lineLimit(1)
                    .fixedSize()
            }
            .frame(minHeight: 38, maxHeight: 42, alignment: .top)

            Group {
                if let summary, !summary.isEmpty {
                    MarkdownContent(source: summary, lineLimit: 4, selectable: false)
                } else {
                    Text("Waiting for work context")
                        .font(.subheadline)
                        .foregroundStyle(.tertiary)
                }
            }
            .frame(maxHeight: 68, alignment: .topLeading)
            .clipped()

            Text(executionLabel)
            .font(.caption2.monospaced())
            .foregroundStyle(.tertiary)
            .lineLimit(1)
            .truncationMode(.middle)
            .help(executionLabel)

            Spacer(minLength: 0)

            Button(action: openAgent) {
                Label("Open agent details", systemImage: "arrow.right.circle")
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.plain)
            .font(.caption.weight(.medium))
            .foregroundStyle(.tint)
        }
        .padding(14)
        .frame(maxWidth: .infinity, minHeight: 210, maxHeight: 210, alignment: .topLeading)
        .clipped()
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .stroke(.separator.opacity(0.55), lineWidth: 0.5)
        }
    }

    private var executionLabel: String {
        var parts: [String] = []
        if let pane = participant.pane {
            parts.append("\(pane.session) › \(pane.windowName.isEmpty ? pane.stableWindowID : pane.windowName) › \(pane.paneID)")
        }
        if let model = participant.agent.model { parts.append(model) }
        return parts.isEmpty ? "No execution binding" : parts.joined(separator: " · ")
    }
}

private struct PipelinePlaceholderCard: View {
    let desired: MuxaDesiredAgent
    let state: MuxaPipelineAliasState?

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text("@\(desired.alias)")
                        .font(.headline)
                        .lineLimit(1)
                    Text(desired.role ?? desired.program)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer()
                Label(agentStateLabel(state?.status ?? "pending"), systemImage: "circle.fill")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(agentStateColor(state?.status ?? "pending"))
                    .lineLimit(1)
                    .fixedSize()
            }
            .frame(minHeight: 38, maxHeight: 42, alignment: .top)
            Group {
                if let task = desired.task, !task.isEmpty {
                    MarkdownContent(source: task, lineLimit: 4, selectable: false)
                } else {
                    Text("No live execution is currently bound.")
                        .font(.subheadline)
                        .foregroundStyle(.tertiary)
                }
            }
            .frame(maxHeight: 68, alignment: .topLeading)
            .clipped()
            Spacer(minLength: 0)
            if let error = state?.error, !error.isEmpty {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .lineLimit(1)
                    .help(error)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, minHeight: 210, maxHeight: 210, alignment: .topLeading)
        .clipped()
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }
}

private struct MarkdownSection: View {
    let title: String
    let source: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(title.uppercased())
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.secondary)
            if let source, !source.isEmpty {
                MarkdownContent(source: source)
            } else {
                Text("Not available")
                    .foregroundStyle(.tertiary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

struct MarkdownContent: View {
    let source: String
    var lineLimit: Int?
    var selectable: Bool
    var font: Font

    init(
        source: String,
        lineLimit: Int? = nil,
        selectable: Bool = true,
        font: Font = .subheadline
    ) {
        self.source = source
        self.lineLimit = lineLimit
        self.selectable = selectable
        self.font = font
    }

    private var attributed: AttributedString {
        MuxaMarkdownText.attributedString(markdown: normalizedSource)
    }

    private var normalizedSource: String {
        source
            .replacingOccurrences(of: "\r\n", with: "\n")
            .replacingOccurrences(of: "\r", with: "\n")
    }

    var body: some View {
        selectableText
    }

    @ViewBuilder
    private var selectableText: some View {
        let content = Text(attributed)
            .font(font)
            .lineLimit(lineLimit)
            .frame(maxWidth: .infinity, alignment: .leading)
            .fixedSize(horizontal: false, vertical: true)
            .layoutPriority(1)
        if selectable {
            content.textSelection(.enabled)
        } else {
            content.textSelection(.disabled)
        }
    }
}

private struct FleetAgentDetailView: View {
    private enum DetailTab: String, CaseIterable, Identifiable {
        case summary = "Summary"
        case conversation = "Conversation"
        case shell = "Shell"

        var id: Self { self }
    }

    let participant: MuxaHostedAgent
    let client: MuxaIPCClient
    @Environment(\.colorScheme) private var colorScheme
    @State private var selectedTab: DetailTab = .summary

    private var summary: String? {
        participant.agent.recap
            ?? participant.agent.lastNotification
            ?? participant.agent.aiTitle
            ?? participant.agent.lastPrompt
    }

    private var title: String {
        participant.agent.aiTitle
            ?? participant.pane?.agentAlias.map { "@\($0)" }
            ?? participant.agent.kind.replacingOccurrences(of: "_", with: " ")
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                HStack(alignment: .top, spacing: 14) {
                    Circle()
                        .fill(agentStateColor(participant.agent.state))
                        .frame(width: 12, height: 12)
                        .padding(.top, 9)
                    VStack(alignment: .leading, spacing: 4) {
                        Text(title)
                            .font(.largeTitle.weight(.semibold))
                        Text(agentStateLabel(participant.agent.state))
                            .foregroundStyle(agentStateColor(participant.agent.state))
                        Text(participant.agent.agentSessionID)
                            .font(.caption.monospaced())
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                    }
                }

                Picker("Agent detail", selection: $selectedTab) {
                    ForEach(availableTabs) { tab in
                        Text(tab.rawValue).tag(tab)
                    }
                }
                .labelsHidden()
                .pickerStyle(.segmented)
                .frame(maxWidth: 460)

                tabContent
            }
            .padding(28)
            .frame(maxWidth: 900, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(MuxaSurfacePalette.workspace(for: colorScheme).ignoresSafeArea())
        .onChange(of: participant.id) { _ in
            selectedTab = .summary
        }
    }

    private var availableTabs: [DetailTab] {
        participant.pane == nil ? [.summary, .conversation] : DetailTab.allCases
    }

    @ViewBuilder
    private var tabContent: some View {
        switch selectedTab {
        case .summary:
            VStack(alignment: .leading, spacing: 18) {
                if let summary, !summary.isEmpty {
                    MarkdownContent(source: summary, font: .body)
                } else {
                    Text("No retained summary")
                        .foregroundStyle(.tertiary)
                }

                GroupBox("Execution location") {
                    Grid(alignment: .leading, horizontalSpacing: 18, verticalSpacing: 8) {
                        AgentFact(label: "Host", value: participant.host.alias)
                        AgentFact(label: "Backend", value: participant.pane?.hostKind ?? "—")
                        AgentFact(label: "Session", value: participant.agent.tmuxSession ?? participant.pane?.session ?? "—")
                        AgentFact(label: "Window", value: participant.pane.map { $0.windowName.isEmpty ? $0.windowID : "\($0.windowName) (\($0.windowID))" } ?? "—")
                        AgentFact(label: "Pane", value: participant.agent.pane ?? "—")
                        AgentFact(label: "Directory", value: participant.agent.cwd ?? participant.pane?.currentPath ?? "—")
                        AgentFact(label: "Runtime", value: participant.agent.kind)
                        AgentFact(label: "Model", value: participant.agent.model ?? "—")
                    }
                    .padding(.vertical, 6)
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)

        case .conversation:
            VStack(alignment: .leading, spacing: 20) {
                MarkdownSection(title: "Request", source: participant.agent.lastPrompt)
                MarkdownSection(title: "Response", source: participant.agent.lastResponse)
            }
            .frame(maxWidth: .infinity, alignment: .leading)

        case .shell:
            if let pane = participant.pane {
                PaneCaptureView(
                    client: client,
                    target: MuxaPaneTarget(host: participant.host, pane: pane)
                )
            } else {
                VStack(spacing: 9) {
                    Image(systemName: "terminal")
                        .font(.system(size: 30))
                        .foregroundStyle(.secondary)
                    Text("No shell binding")
                        .font(.headline)
                    Text("This agent session is not attached to a pane.")
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, minHeight: 220)
            }
        }
    }
}

private struct FleetHostDetailView: View {
    let host: MuxaFleetHost
    @ObservedObject var model: AppModel
    @Environment(\.colorScheme) private var colorScheme
    @State private var connectionExpanded = false

    private let metricColumns = [
        GridItem(.adaptive(minimum: 105, maximum: 170), spacing: 12),
    ]
    private let sessionColumns = [
        GridItem(.adaptive(minimum: 260, maximum: 430), spacing: 12, alignment: .top),
    ]

    private var liveAgents: [MuxaAgent] {
        (host.remote?.agents ?? []).filter { $0.state != "stopped" }
    }

    private var watchHost: MuxaWatchHost? {
        model.executionSnapshot.watchHosts.first { $0.host.alias == host.alias }
    }

    private var sessions: [MuxaWatchSession] {
        watchHost?.sessions ?? []
    }

    private var panes: [MuxaPaneInfo] {
        host.remote?.panes ?? []
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                HStack(alignment: .top, spacing: 14) {
                    HostIdentityBadge(host: host, size: 40)
                    VStack(alignment: .leading, spacing: 4) {
                        Text(host.alias)
                            .font(.largeTitle.weight(.semibold))
                        Text(host.local ? "Local host" : host.state.replacingOccurrences(of: "_", with: " ").capitalized)
                            .foregroundStyle(fleetHostColor(host.state))
                    }
                }

                LazyVGrid(columns: metricColumns, alignment: .leading, spacing: 12) {
                    HostMetric(title: "Sessions", value: sessions.count)
                    HostMetric(title: "Agents", value: liveAgents.count)
                    HostMetric(title: "Panes", value: panes.count)
                    if !host.local, let latency = host.latencyMS {
                        HostMetric(title: "Latency", value: latency, suffix: " ms")
                    }
                }

                VStack(alignment: .leading, spacing: 10) {
                    Text("Execution sessions")
                        .font(.title2.weight(.semibold))
                    Text("Each session is summarized by its windows and the latest retained agent context.")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)

                    if sessions.isEmpty {
                        VStack(spacing: 8) {
                            Image(systemName: "square.3.layers.3d")
                                .font(.system(size: 28))
                                .foregroundStyle(.secondary)
                            Text("No sessions")
                                .font(.headline)
                            Text("No execution sessions are visible on this host.")
                                .foregroundStyle(.secondary)
                        }
                        .frame(maxWidth: .infinity, minHeight: 180)
                    } else {
                        LazyVGrid(columns: sessionColumns, alignment: .leading, spacing: 12) {
                            ForEach(sessions) { session in
                                FleetSessionSummaryCard(session: session) {
                                    model.selectWatchSession(session.identity)
                                }
                            }
                        }
                    }
                }

                DisclosureGroup("Connection details", isExpanded: $connectionExpanded) {
                    Grid(alignment: .leading, horizontalSpacing: 18, verticalSpacing: 8) {
                        AgentFact(label: "Mode", value: host.mode)
                        AgentFact(label: "State", value: host.state)
                        AgentFact(label: "Scope", value: host.local ? "local" : "remote")
                        if let target = host.sshTarget, !host.local {
                            AgentFact(label: "SSH target", value: target)
                        }
                        if let error = host.error, !error.isEmpty {
                            AgentFact(label: "Error", value: error)
                        }
                    }
                    .padding(.top, 10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .padding(12)
                .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 10))
            }
            .padding(28)
            .frame(maxWidth: 1250, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(MuxaSurfacePalette.workspace(for: colorScheme).ignoresSafeArea())
    }
}

private struct FleetSessionSummaryCard: View {
    let session: MuxaWatchSession
    let open: () -> Void

    private var panes: [MuxaWatchPane] {
        session.windows.flatMap(\.panes)
    }

    private var agents: [MuxaWatchPane] {
        panes.filter { $0.agent != nil }
    }

    private var attentionCount: Int {
        agents.filter(paneNeedsAttentionForSummary).count
    }

    var body: some View {
        Button(action: open) {
            VStack(alignment: .leading, spacing: 10) {
                HStack(spacing: 8) {
                    Image(systemName: "square.3.layers.3d")
                        .foregroundStyle(.tint)
                    Text(session.name.isEmpty ? session.sessionID : session.name)
                        .font(.headline)
                        .lineLimit(1)
                    Spacer(minLength: 4)
                    if attentionCount > 0 {
                        Label("\(attentionCount)", systemImage: "exclamationmark.circle.fill")
                            .font(.caption.weight(.medium))
                            .foregroundStyle(.orange)
                    }
                    Image(systemName: "chevron.right")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.tertiary)
                }

                Text("\(session.windows.count) windows · \(panes.count) panes · \(agents.count) agents")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)

                VStack(alignment: .leading, spacing: 7) {
                    ForEach(Array(agents.prefix(2))) { pane in
                        FleetResourceSummaryRow(pane: pane)
                    }
                    if agents.isEmpty {
                        Text("No retained agent summary")
                            .font(.caption)
                            .foregroundStyle(.tertiary)
                    } else if agents.count > 2 {
                        Text("+\(agents.count - 2) more agents")
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    }
                }

                Spacer(minLength: 0)
            }
            .padding(14)
            .frame(maxWidth: .infinity, minHeight: 188, maxHeight: 188, alignment: .topLeading)
            .contentShape(Rectangle())
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
            .overlay {
                RoundedRectangle(cornerRadius: 12)
                    .stroke(Color(nsColor: .separatorColor).opacity(0.5), lineWidth: 0.5)
            }
        }
        .buttonStyle(.plain)
    }
}

private struct FleetSessionDetailView: View {
    let session: MuxaWatchSession
    @ObservedObject var model: AppModel
    @Environment(\.colorScheme) private var colorScheme
    @State private var showsDetails = false

    private let columns = [
        GridItem(.adaptive(minimum: 260, maximum: 470), spacing: 12, alignment: .top),
    ]

    private var panes: [MuxaWatchPane] {
        session.windows.flatMap(\.panes)
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                HStack(alignment: .top, spacing: 12) {
                    Image(systemName: "square.3.layers.3d")
                        .font(.system(size: 28))
                        .foregroundStyle(.tint)
                        .frame(width: 42, height: 42)
                        .background(Color.accentColor.opacity(0.1), in: RoundedRectangle(cornerRadius: 10))
                    VStack(alignment: .leading, spacing: 3) {
                        Text(session.name.isEmpty ? session.sessionID : session.name)
                            .font(.largeTitle.weight(.semibold))
                        Text("\(session.hostAlias) · \(session.windows.count) windows · \(panes.count) panes")
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 8)
                    Button {
                        showsDetails.toggle()
                    } label: {
                        Label("Details", systemImage: "info.circle")
                    }
                    .popover(isPresented: $showsDetails, arrowEdge: .bottom) {
                        Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 8) {
                            AgentFact(label: "Host", value: session.hostAlias)
                            AgentFact(label: "Socket", value: session.socket)
                            AgentFact(label: "Session", value: session.sessionID)
                        }
                        .padding(16)
                        .frame(minWidth: 360)
                    }
                }

                VStack(alignment: .leading, spacing: 5) {
                    Text("Windows")
                        .font(.title2.weight(.semibold))
                    Text("Open any agent row to inspect its summary, latest response, and Live Pane.")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }

                LazyVGrid(columns: columns, alignment: .leading, spacing: 12) {
                    ForEach(session.windows) { window in
                        FleetWindowSummaryCard(
                            window: window,
                            openWindow: { model.selectWatchWindow(window.identity) },
                            openPane: { model.selectWatchPane($0.id) }
                        )
                    }
                }
            }
            .padding(28)
            .frame(maxWidth: 1250, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(MuxaSurfacePalette.workspace(for: colorScheme).ignoresSafeArea())
    }
}

private struct FleetWindowDetailView: View {
    let window: MuxaWatchWindow
    @ObservedObject var model: AppModel
    @Environment(\.colorScheme) private var colorScheme
    @State private var showsDetails = false

    private let columns = [
        GridItem(.adaptive(minimum: 330, maximum: 570), spacing: 12, alignment: .top),
    ]

    private var agents: [MuxaWatchPane] {
        window.panes.sorted { left, right in
            let leftPriority = panePriority(left)
            let rightPriority = panePriority(right)
            if leftPriority != rightPriority { return leftPriority < rightPriority }
            return (left.agent?.lastActivityAt ?? "") > (right.agent?.lastActivityAt ?? "")
        }
    }

    private var workIdentity: MuxaWorkIdentity? {
        let identities = Set(window.panes.compactMap(\.pane.workIdentity))
        return identities.count == 1 ? identities.first : nil
    }

    private var relatedMessages: [MuxaOperatorMessage] {
        model.operatorMessages.filter { message in
            message.host.alias == window.hostAlias
                && (message.request.from.room.windowID == window.windowID
                    || message.request.to.room.windowID == window.windowID)
        }
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                header
                metrics

                VStack(alignment: .leading, spacing: 5) {
                    Text("Agent reports")
                        .font(.title2.weight(.semibold))
                    Text("Recap and latest response are kept separate; runtime and workload facts come directly from muxad.")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }

                LazyVGrid(columns: columns, alignment: .leading, spacing: 12) {
                    ForEach(agents) { pane in
                        WindowAgentReportCard(pane: pane) {
                            model.selectWatchPane(pane.id)
                        }
                    }
                }

                if !relatedMessages.isEmpty {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Collaboration in this window")
                            .font(.title2.weight(.semibold))
                        Text("\(relatedMessages.count) operator command\(relatedMessages.count == 1 ? "" : "s") and their durable replies")
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                        ForEach(relatedMessages.prefix(5)) { message in
                            WindowCollaborationRow(message: message) {
                                model.select(.inbox)
                            }
                        }
                    }
                }
            }
            .padding(28)
            .frame(maxWidth: 1250, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .topLeading)
        }
        .background(MuxaSurfacePalette.workspace(for: colorScheme).ignoresSafeArea())
        .task { await model.refreshOperatorInbox() }
    }

    private var header: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: "macwindow")
                .font(.system(size: 28))
                .foregroundStyle(.tint)
                .frame(width: 42, height: 42)
                .background(Color.accentColor.opacity(0.1), in: RoundedRectangle(cornerRadius: 10))
            VStack(alignment: .leading, spacing: 4) {
                Text(window.name.isEmpty ? window.windowID : window.name)
                    .font(.largeTitle.weight(.semibold))
                HStack(spacing: 7) {
                    Text("\(window.hostAlias) · \(window.panes.count) panes")
                        .foregroundStyle(.secondary)
                    if let workIdentity {
                        Text("\(workIdentity.workspaceID) / \(workIdentity.workID)")
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(Color.accentColor)
                            .padding(.horizontal, 7)
                            .padding(.vertical, 3)
                            .background(Color.accentColor.opacity(0.1), in: Capsule())
                    }
                }
            }
            Spacer(minLength: 8)
            Button {
                showsDetails.toggle()
            } label: {
                Label("Runtime Details", systemImage: "info.circle")
            }
            .popover(isPresented: $showsDetails, arrowEdge: .bottom) {
                Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 8) {
                    AgentFact(label: "Host", value: window.hostAlias)
                    AgentFact(label: "Socket", value: window.socket)
                    AgentFact(label: "Session", value: window.sessionID)
                    AgentFact(label: "Window", value: window.windowID)
                    AgentFact(label: "Index", value: window.index)
                }
                .padding(16)
                .frame(minWidth: 380)
            }
        }
    }

    private var metrics: some View {
        let attention = window.panes.lazy.filter(paneNeedsAttentionForSummary).count
        let working = window.panes.lazy.filter {
            $0.agent.map { ["working", "starting"].contains($0.state) } ?? false
        }.count
        let subagents = window.panes.lazy.compactMap(\.agent?.subagents).reduce(0) { $0 + $1.count }
        let processes = window.panes.lazy.compactMap(\.agent?.workload?.processCount).reduce(0, +)
        let metricColumns = [
            GridItem(.adaptive(minimum: 108, maximum: 170), spacing: 18),
        ]
        return LazyVGrid(columns: metricColumns, alignment: .leading, spacing: 12) {
            HostMetric(title: "Agents", value: window.panes.compactMap(\.agent).count)
            HostMetric(title: "Working", value: working)
            HostMetric(title: "Need attention", value: attention)
            HostMetric(title: "Subagents", value: subagents)
            HostMetric(title: "Child processes", value: processes)
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
    }

    private func panePriority(_ pane: MuxaWatchPane) -> Int {
        if paneNeedsAttentionForSummary(pane) { return 0 }
        if pane.agent.map({ ["working", "starting"].contains($0.state) }) == true { return 1 }
        return pane.agent == nil ? 3 : 2
    }
}

private struct WindowAgentReportCard: View {
    let pane: MuxaWatchPane
    let open: () -> Void

    private var summary: String? {
        presentText(pane.agent?.recap)
            ?? presentText(pane.agent?.lastResponse)
            ?? presentText(pane.agent?.lastNotification)
            ?? presentText(pane.agent?.lastPrompt)
    }

    private var separateResponse: String? {
        guard let response = presentText(pane.agent?.lastResponse), response != pane.agent?.recap else {
            return nil
        }
        return response
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 11) {
            Button(action: open) {
                HStack(spacing: 8) {
                    Circle()
                        .fill(pane.agent.map { agentStateColor($0.state) } ?? Color.secondary)
                        .frame(width: 8, height: 8)
                    Text(fleetPaneDisplayTitle(pane))
                        .font(.headline)
                        .lineLimit(1)
                    if let agent = pane.agent {
                        Text(agentStateLabel(agent.state))
                            .font(.caption2.weight(.semibold))
                            .foregroundStyle(agentStateColor(agent.state))
                    }
                    Spacer(minLength: 4)
                    Text(pane.pane.paneID)
                        .font(.caption2.monospaced())
                        .foregroundStyle(.tertiary)
                    Image(systemName: "chevron.right")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.tertiary)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if let summary {
                reportSection("Summary", source: summary, lineLimit: separateResponse == nil ? 9 : 6)
            } else {
                Text("No agent-authored task summary has been retained yet.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            if let separateResponse {
                reportSection("Latest response", source: separateResponse, lineLimit: 8)
            }

            if let agent = pane.agent {
                ViewThatFits(in: .horizontal) {
                    HStack(spacing: 10) { factChips(agent) }
                    VStack(alignment: .leading, spacing: 5) { factChips(agent) }
                }
                if let workload = agent.workload,
                   workload.processCount > 0 || !(agent.subagents ?? []).isEmpty {
                    Divider()
                    HStack(spacing: 10) {
                        Label("\(workload.processCount) processes", systemImage: "point.3.connected.trianglepath.dotted")
                        if workload.shellCount > 0 { Text("\(workload.shellCount) shells") }
                        if workload.helperCount > 0 { Text("\(workload.helperCount) helpers") }
                        if let count = agent.subagents?.count, count > 0 { Text("\(count) live subagents") }
                    }
                    .font(.caption)
                    .foregroundStyle(.secondary)
                }
            }

            HStack(spacing: 6) {
                Image(systemName: "terminal")
                Text(pane.pane.currentCommand)
                Text("·")
                Text(pane.pane.currentPath)
                    .lineLimit(1)
            }
            .font(.caption2.monospaced())
            .foregroundStyle(.tertiary)
        }
        .padding(14)
        .frame(maxWidth: .infinity, minHeight: 300, alignment: .topLeading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 11))
        .overlay {
            RoundedRectangle(cornerRadius: 11)
                .stroke(
                    paneNeedsAttentionForSummary(pane) ? Color.orange.opacity(0.5) : Color(nsColor: .separatorColor).opacity(0.45),
                    lineWidth: paneNeedsAttentionForSummary(pane) ? 1 : 0.5
                )
        }
    }

    private func reportSection(_ label: String, source: String, lineLimit: Int) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(label.uppercased())
                .font(.caption2.weight(.bold))
                .foregroundStyle(.secondary)
            MarkdownContent(source: source, lineLimit: lineLimit, selectable: false, font: .callout)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder
    private func factChips(_ agent: MuxaAgent) -> some View {
        if let activity = agent.lastActivityAt {
            detailChip("Activity", compactWindowTimestamp(activity), systemImage: "clock")
        }
        if let model = agent.model { detailChip("Model", model, systemImage: "cpu") }
        if let context = agent.contextUsedPercent {
            detailChip("Context", "\(Int(context.rounded()))%", systemImage: "gauge.with.dots.needle.33percent")
        }
        if let cost = agent.costUSD {
            detailChip("Cost", cost.formatted(.currency(code: "USD")), systemImage: "dollarsign.circle")
        }
    }

    private func detailChip(_ label: String, _ value: String, systemImage: String) -> some View {
        Label("\(label) \(value)", systemImage: systemImage)
            .font(.caption2)
            .foregroundStyle(.secondary)
            .lineLimit(1)
    }
}

private struct WindowCollaborationRow: View {
    let message: MuxaOperatorMessage
    let openInbox: () -> Void

    var body: some View {
        Button(action: openInbox) {
            HStack(alignment: .top, spacing: 9) {
                Image(systemName: message.request.reply == nil ? "clock" : "arrowshape.turn.up.left.fill")
                    .foregroundStyle(message.request.reply == nil ? Color.blue : Color.green)
                    .frame(width: 18)
                VStack(alignment: .leading, spacing: 3) {
                    Text(message.request.body)
                        .font(.subheadline.weight(.medium))
                        .lineLimit(2)
                    if let reply = message.request.reply {
                        MarkdownContent(source: reply.body, lineLimit: 3, selectable: false, font: .caption)
                            .foregroundStyle(.secondary)
                    } else {
                        Text("Waiting for \(message.request.to.label) to reply")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 6)
                Image(systemName: "chevron.right")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.tertiary)
            }
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 8))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

private struct FleetWindowSummaryCard: View {
    let window: MuxaWatchWindow
    let openWindow: () -> Void
    let openPane: (MuxaWatchPane) -> Void

    private var displayedPanes: [MuxaWatchPane] {
        Array(window.panes.prefix(2))
    }

    private var workLabel: String? {
        let identities = Set(window.panes.compactMap(\.pane.workIdentity))
        guard identities.count == 1, let identity = identities.first else { return nil }
        return "\(identity.workspaceID) / \(identity.workID)"
    }

    private var focusPane: MuxaWatchPane? {
        window.panes.sorted { left, right in
            let leftPriority = paneNeedsAttentionForSummary(left) ? 0 : left.agent?.state == "working" ? 1 : 2
            let rightPriority = paneNeedsAttentionForSummary(right) ? 0 : right.agent?.state == "working" ? 1 : 2
            if leftPriority != rightPriority { return leftPriority < rightPriority }
            return (left.agent?.lastActivityAt ?? "") > (right.agent?.lastActivityAt ?? "")
        }.first
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Button(action: openWindow) {
                HStack(spacing: 8) {
                    Image(systemName: "macwindow")
                        .foregroundStyle(.secondary)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(window.name.isEmpty ? window.windowID : window.name)
                            .font(.headline)
                            .lineLimit(1)
                        if let workLabel {
                            Text(workLabel)
                                .font(.caption2.weight(.medium))
                                .foregroundStyle(Color.accentColor)
                                .lineLimit(1)
                        }
                    }
                    Spacer(minLength: 4)
                    Text("#\(window.index) · \(window.panes.count) panes")
                        .font(.caption2.monospacedDigit())
                        .foregroundStyle(.tertiary)
                    Image(systemName: "chevron.right")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.tertiary)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if let focusPane, let summary = fleetPaneSummary(focusPane) {
                VStack(alignment: .leading, spacing: 3) {
                    Text(paneNeedsAttentionForSummary(focusPane) ? "NEEDS ATTENTION" : "CURRENT PICTURE")
                        .font(.caption2.weight(.bold))
                        .foregroundStyle(paneNeedsAttentionForSummary(focusPane) ? Color.orange : Color.secondary)
                    MarkdownContent(source: summary, lineLimit: 3, selectable: false, font: .caption)
                }
                .padding(8)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 7))
            }

            VStack(spacing: 6) {
                ForEach(displayedPanes) { pane in
                    Button {
                        openPane(pane)
                    } label: {
                        FleetResourceSummaryRow(pane: pane, showsChevron: true)
                            .padding(.horizontal, 9)
                            .padding(.vertical, 7)
                            .frame(maxWidth: .infinity, minHeight: 48, alignment: .leading)
                            .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 7))
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
            }
            if window.panes.count > displayedPanes.count {
                Text("+\(window.panes.count - displayedPanes.count) more panes")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
            Spacer(minLength: 0)
        }
        .padding(14)
        .frame(maxWidth: .infinity, minHeight: 276, maxHeight: 276, alignment: .topLeading)
        .clipped()
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .overlay {
            RoundedRectangle(cornerRadius: 12)
                .stroke(Color(nsColor: .separatorColor).opacity(0.5), lineWidth: 0.5)
        }
    }
}

private func compactWindowTimestamp(_ value: String) -> String {
    let normalized = value.replacingOccurrences(of: "T", with: " ")
    return String(normalized.prefix(16))
}

private struct FleetResourceSummaryRow: View {
    let pane: MuxaWatchPane
    var showsChevron = false

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Circle()
                .fill(pane.agent.map { agentStateColor($0.state) } ?? Color.secondary)
                .frame(width: 7, height: 7)
                .padding(.top, 5)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(fleetPaneDisplayTitle(pane))
                        .font(.subheadline.weight(.medium))
                        .lineLimit(1)
                    if let agent = pane.agent {
                        Text(agentStateLabel(agent.state))
                            .font(.caption2.weight(.medium))
                            .foregroundStyle(agentStateColor(agent.state))
                    }
                }
                MarkdownContent(
                    source: fleetPaneSummary(pane) ?? "No summary reported",
                    lineLimit: 2,
                    selectable: false,
                    font: .caption
                )
                .foregroundStyle(.secondary)
            }
            Spacer(minLength: 4)
            if showsChevron {
                Image(systemName: "chevron.right")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.tertiary)
                    .padding(.top, 5)
            }
        }
    }
}

private func paneNeedsAttentionForSummary(_ pane: MuxaWatchPane) -> Bool {
    pane.agent.map {
        ["waiting_input", "waiting_choice", "blocked", "error", "failed"].contains($0.state)
    } ?? false
}

private func fleetPaneDisplayTitle(_ pane: MuxaWatchPane) -> String {
    pane.pane.agentAlias.map { "@\($0)" }
        ?? presentText(pane.agent?.aiTitle)
        ?? presentText(pane.pane.title)
        ?? presentText(pane.pane.currentCommand)
        ?? pane.pane.paneID
}

private func fleetPaneSummary(_ pane: MuxaWatchPane) -> String? {
    presentText(pane.agent?.recap)
        ?? presentText(pane.agent?.lastResponse)
        ?? presentText(pane.agent?.lastNotification)
        ?? presentText(pane.agent?.lastPrompt)
        ?? presentText(pane.pane.currentPath)
}

private func presentText(_ value: String?) -> String? {
    guard let value, !value.isEmpty else { return nil }
    return value
}

private struct HostMetric: View {
    let title: String
    let value: UInt64
    var suffix = ""

    init(title: String, value: Int, suffix: String = "") {
        self.title = title
        self.value = UInt64(value)
        self.suffix = suffix
    }

    init(title: String, value: UInt64, suffix: String = "") {
        self.title = title
        self.value = value
        self.suffix = suffix
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text("\(value)\(suffix)")
                .font(.title2.weight(.semibold).monospacedDigit())
            Text(title)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .frame(minWidth: 90, alignment: .leading)
    }
}

private struct AgentFact: View {
    let label: String
    let value: String

    var body: some View {
        GridRow {
            Text(label)
                .foregroundStyle(.secondary)
                .frame(width: 74, alignment: .leading)
            Text(value)
                .textSelection(.enabled)
        }
        .font(.subheadline)
    }
}

private struct MuxaEmptyDetail: View {
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: "square.stack.3d.up")
                .font(.system(size: 42))
                .foregroundStyle(.secondary)
            Text("Muxa Workspace")
                .font(.title2)
            Text("Managed work, collaborating agents, and native shells appear here.")
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            HStack {
                Button("Start Work") { model.presentWorkStart() }
                    .buttonStyle(.borderedProminent)
                    .disabled(!model.isConnected || model.isStartingWork)
                Button("Open Live Watch") { model.select(.watch) }
                    .buttonStyle(.bordered)
                Button("New Shell") { model.createShell() }
                    .buttonStyle(.bordered)
                    .disabled(!model.isConnected || model.isCreatingSession)
            }
        }
        .padding(32)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

func agentStateLabel(_ state: String) -> String {
    switch state {
    case "waiting_input": "Waiting for input"
    case "waiting_choice": "Waiting for choice"
    case "working": "Working"
    case "starting": "Starting"
    case "idle": "Idle"
    case "error", "failed": "Error"
    case "blocked": "Blocked"
    case "done": "Done"
    case "pending": "Pending"
    case "stopped": "Stopped"
    default: state.replacingOccurrences(of: "_", with: " ").capitalized
    }
}

func agentStateColor(_ state: String) -> Color {
    switch state {
    case "working", "running", "starting": .blue
    case "waiting_input", "waiting_choice", "blocked": .orange
    case "error", "failed": .red
    case "done": .green
    case "idle": .mint
    default: .secondary
    }
}

func fleetHostColor(_ state: String) -> Color {
    switch state {
    case "online": .green
    case "connecting": .blue
    case "degraded", "version_skew": .orange
    case "auth_failed": .red
    case "offline", "disabled": .secondary
    default: .secondary
    }
}

struct TerminalPane: View {
    private enum DisplayMode: String, CaseIterable, Identifiable {
        case terminal = "Terminal"
        case raw = "Raw"

        var id: Self { self }
    }

    @StateObject private var pane: TerminalPaneModel
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.openWindow) private var openWindow
    @State private var displayMode: DisplayMode = .terminal
    private let sessionID: String
    private let allowsRaw: Bool
    private let showsToolbar: Bool
    private let onExit: () -> Void

    init(
        client: MuxaIPCClient,
        sessionID: String,
        replayInitialHistory: Bool,
        allowsRaw: Bool = true,
        showsToolbar: Bool = true,
        onExit: @escaping () -> Void = {}
    ) {
        self.sessionID = sessionID
        self.allowsRaw = allowsRaw
        self.showsToolbar = showsToolbar
        self.onExit = onExit
        _pane = StateObject(
            wrappedValue: TerminalPaneModel(
                client: client,
                sessionID: sessionID,
                replayInitialHistory: replayInitialHistory
            )
        )
    }

    var body: some View {
        ZStack(alignment: .top) {
            MuxaSurfacePalette.terminal(for: colorScheme)
                .ignoresSafeArea()

            TerminalSurfaceView(context: pane.terminalState)
                .background(MuxaSurfacePalette.terminal(for: colorScheme))
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .clipped()
                .opacity(displayMode == .terminal ? 1 : 0)
                .allowsHitTesting(displayMode == .terminal)

            if displayMode == .raw {
                ScrollView([.horizontal, .vertical]) {
                    Text(verbatim: pane.rawOutputText)
                        .font(.system(size: 12, weight: .regular, design: .monospaced))
                        .foregroundStyle(colorScheme == .dark ? Color(white: 0.92) : Color(white: 0.12))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .topLeading)
                        .padding(16)
                }
                .background(MuxaSurfacePalette.terminal(for: colorScheme))
            }

            if showsToolbar {
                VStack {
                    HStack(spacing: 8) {
                        if displayMode == .raw {
                            Text("\(pane.rawOutputByteCount) bytes retained · controls escaped")
                                .font(.caption2.monospacedDigit())
                                .foregroundStyle(.secondary)
                        }
                        Spacer()
                        if allowsRaw {
                            Picker("Shell display", selection: $displayMode) {
                                ForEach(DisplayMode.allCases) { mode in
                                    Text(mode.rawValue).tag(mode)
                                }
                            }
                            .labelsHidden()
                            .pickerStyle(.segmented)
                            .frame(width: 170)
                        }
                        Button {
                            openWindow(value: MuxaModuleRoute.shell(sessionID))
                        } label: {
                            Image(systemName: "macwindow.on.rectangle")
                        }
                        .buttonStyle(.borderless)
                        .help("Open this Shell in a separate window")
                    }
                    .padding(8)
                    Spacer()
                }
            }

            if pane.outputWasTruncated {
                Label("Earlier output was truncated by muxad's retained buffer", systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .padding(7)
                    .background(.ultraThinMaterial, in: Capsule())
                    .padding(8)
            } else if let error = pane.errorMessage {
                Label(error, systemImage: "bolt.horizontal.circle")
                    .font(.caption)
                    .padding(7)
                    .background(.ultraThinMaterial, in: Capsule())
                    .padding(8)
            } else if pane.exited {
                Label(
                    pane.exitStatus.map { "Session ended (status \($0))" } ?? "Session ended",
                    systemImage: pane.exitStatus == 0 ? "checkmark.circle" : "stop.circle"
                )
                .font(.caption)
                .padding(.horizontal, 10)
                .padding(.vertical, 7)
                .background(.regularMaterial, in: Capsule())
                .padding(8)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .clipped()
        .onAppear {
            pane.start()
            pane.setRawDisplayEnabled(displayMode == .raw)
        }
        .task(id: sessionID) {
            // The embedded Live Pane is inserted after the attach request
            // completes. Give AppKit one run-loop turn to put the native
            // Ghostty surface in a window, then deterministically move the
            // first responder to it so the first keystroke is not lost to
            // the inspector or sidebar that initiated the attach.
            await Task.yield()
            try? await Task.sleep(for: .milliseconds(120))
            guard !Task.isCancelled, displayMode == .terminal else { return }
            pane.focus()
        }
        .onDisappear {
            pane.setRawDisplayEnabled(false)
            pane.stop()
        }
        .onChange(of: displayMode) { mode in
            pane.setRawDisplayEnabled(mode == .raw)
        }
        .onChange(of: pane.exited) { exited in
            if exited { onExit() }
        }
    }
}

enum MuxaSurfacePalette {
    static func editor(for colorScheme: ColorScheme) -> Color {
        workspace(for: colorScheme)
    }

    static func sidebar(for colorScheme: ColorScheme) -> Color {
        switch colorScheme {
        case .dark:
            // Deliberately lighter and cooler than the terminal's #212121.
            Color(red: 0.16, green: 0.18, blue: 0.21)
        default:
            Color(red: 0.93, green: 0.94, blue: 0.96)
        }
    }

    static func terminal(for colorScheme: ColorScheme) -> Color {
        switch colorScheme {
        case .dark:
            // Matches GhosttyTerminal's default Afterglow background.
            Color(red: 0.13, green: 0.13, blue: 0.13)
        default:
            // Matches GhosttyTerminal's default Alabaster background.
            Color(red: 0.97, green: 0.97, blue: 0.97)
        }
    }

    static func workspace(for colorScheme: ColorScheme) -> Color {
        switch colorScheme {
        case .dark:
            Color(red: 0.11, green: 0.12, blue: 0.14)
        default:
            Color(red: 0.97, green: 0.97, blue: 0.98)
        }
    }
}
