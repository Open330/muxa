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
                FleetHostDetailView(host: host)
                    .id(host.id)
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
        case .shell(let id):
            model.sessions.first { $0.id == id }.map { $0.displayName ?? $0.id }
                ?? "Shell"
        case .pane(let id):
            model.executionSnapshot.watchPane(id: id).map {
                "\($0.host.alias) · \($0.pane.windowName.isEmpty ? $0.pane.paneID : $0.pane.windowName)"
            } ?? "Fleet pane"
        }
    }

    private func tabIcon(for selection: MuxaSidebarSelection) -> String {
        switch selection {
        case .workBoard: "rectangle.3.group"
        case .watch: "waveform.path.ecg.rectangle"
        case .ask: "sparkles"
        case .work: "square.stack.3d.up"
        case .agent: "person.crop.circle"
        case .host: "network"
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
        HStack(spacing: 7) {
            Image(systemName: systemImage)
                .foregroundStyle(active ? Color.accentColor : Color.secondary)
            Text(title)
                .italic(preview)
                .lineLimit(1)
            Spacer(minLength: 4)
            Button(action: close) {
                Image(systemName: "xmark")
                    .font(.system(size: 9, weight: .semibold))
                    .frame(width: 18, height: 20)
            }
            .buttonStyle(.plain)
            .opacity(active || hovering ? 1 : 0)
            .accessibilityLabel("Close \(title)")
        }
        .padding(.leading, 10)
        .padding(.trailing, 5)
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
        .contentShape(Rectangle())
        .onTapGesture(perform: activate)
        .simultaneousGesture(
            TapGesture(count: 2).onEnded {
                activate()
                pin()
            }
        )
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
        case showHosts = "Show hosts"
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
            case .showHosts: "network"
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
        case .showHosts:
            model.show(.hosts)
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

    @ObservedObject var model: AppModel
    let openPinnedPane: (MuxaWatchPaneIdentity) -> Void
    @Environment(\.colorScheme) private var colorScheme
    @State private var filterText = ""
    @State private var statusScope: StatusScope = .all

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
                                model.select(.ask)
                            } label: {
                                Image(systemName: "sparkles")
                            }
                            .buttonStyle(.borderless)
                            .help("Open global Ask")
                            Button {
                                model.select(.watch)
                            } label: {
                                Image(systemName: "rectangle.on.rectangle")
                            }
                            .buttonStyle(.borderless)
                            .help("Open Live Watch")
                        }
                        if model.sidebarMode == .hosts {
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

                    List(selection: $model.sidebarSelection) {
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
        case .hosts: model.fleetHosts.count
        case .shells: model.sessions.lazy.filter { !$0.exited }.count
        }
    }

    private var filteredSidebarCount: Int {
        switch model.sidebarMode {
        case .work: filteredWorkGroups.count
        case .watch: filteredWatchHosts.reduce(0) { $0 + $1.paneCount }
        case .hosts: filteredHosts.count
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
            matchesFilter([$0.title, $0.workspaceID, $0.pipelineRun.pipeline])
                && matchesScope(attention: $0.attentionCount > 0, active: $0.workingCount > 0)
        }
    }

    private var filteredHosts: [MuxaFleetHost] {
        model.fleetHosts.filter {
            matchesFilter([$0.alias, $0.state, $0.mode, $0.error])
                && matchesScope(
                    attention: !["online", "connecting"].contains($0.state),
                    active: ["online", "connecting"].contains($0.state)
                )
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

    private var filteredWatchHosts: [MuxaWatchHost] {
        model.executionSnapshot.watchHosts.compactMap { hostGroup in
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
                WorkBoardRow(workCount: model.workGroups.count, agentCount: model.hostedAgents.count)
                    .tag(MuxaSidebarSelection.workBoard)
            }
            Section("Managed work") {
                if model.workGroups.isEmpty {
                    SidebarEmptyRow(title: "No managed work", systemImage: "square.stack.3d.up.slash")
                } else if filteredWorkGroups.isEmpty {
                    SidebarEmptyRow(title: "No matching work", systemImage: "line.3.horizontal.decrease.circle")
                } else {
                    ForEach(filteredWorkGroups) { work in
                        WorkRow(work: work)
                            .tag(MuxaSidebarSelection.work(work.identity))
                    }
                }
            }
        case .watch:
            Section("Execution topology") {
                if model.executionSnapshot.watchHosts.allSatisfy({ $0.paneCount == 0 }) {
                    SidebarEmptyRow(title: "No panes detected", systemImage: "terminal")
                } else if filteredWatchHosts.isEmpty {
                    SidebarEmptyRow(title: "No matching panes", systemImage: "line.3.horizontal.decrease.circle")
                } else {
                    ForEach(filteredWatchHosts) { host in
                        WatchHostTree(
                            group: host,
                            selectedPaneID: model.watchSelection,
                            selectPane: model.selectWatchPane,
                            openPinnedPane: openPinnedPane,
                            forceExpanded: !filterText.isEmpty || statusScope != .all,
                            workLabel: watchWorkLabel
                        )
                        .listRowInsets(EdgeInsets(top: 2, leading: 4, bottom: 2, trailing: 4))
                        .listRowBackground(Color.clear)
                    }
                }
            }
        case .hosts:
            Section("Fleet") {
                if model.fleetHosts.isEmpty {
                    SidebarEmptyRow(title: "No hosts detected", systemImage: "network.slash")
                } else if filteredHosts.isEmpty {
                    SidebarEmptyRow(title: "No matching hosts", systemImage: "line.3.horizontal.decrease.circle")
                } else {
                    ForEach(filteredHosts) { host in
                        FleetHostRow(host: host)
                            .tag(MuxaSidebarSelection.host(host.id))
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
                        SessionRow(session: session)
                            .tag(MuxaSidebarSelection.shell(session.id))
                    }
                }
            }
        }
    }

    private func watchWorkLabel(_ window: MuxaWatchWindow) -> String? {
        guard window.hostAlias == model.fleetHosts.first(where: { $0.local })?.alias else {
            return nil
        }
        return model.pipelineRuns.first(where: { $0.windowID == window.windowID }).map {
            "\($0.identity.workspaceID) › \($0.identity.workID)"
        }
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
            model.workGroups.lazy.filter { $0.attentionCount > 0 }.count
        case .watch:
            model.hostedAgents.lazy.filter {
                ["waiting_input", "waiting_choice", "blocked", "error", "failed"]
                    .contains($0.agent.state)
            }.count
        case .hosts:
            model.fleetHosts.lazy.filter { !["online", "connecting"].contains($0.state) }.count
        case .shells:
            0
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
            Text("\(work.completedCount)/\(work.totalCount)")
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
        .help("Independent fleet agent session: \(participant.agent.agentSessionID)")
    }
}

private struct FleetHostRow: View {
    let host: MuxaFleetHost

    var body: some View {
        HStack(spacing: 10) {
            HostIdentityBadge(host: host, size: 28)
            VStack(alignment: .leading, spacing: 2) {
                Text(host.alias)
                    .lineLimit(1)
                HStack(spacing: 5) {
                    Text(host.local ? "local" : host.state.replacingOccurrences(of: "_", with: " "))
                    if let remote = host.remote {
                        Text("· \(remote.agents.filter { $0.state != "stopped" }.count) agents")
                    }
                    if let latency = host.latencyMS, !host.local {
                        Text("· \(latency) ms")
                    }
                }
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
        .padding(.vertical, 2)
        .help(host.error ?? "\(host.mode) access")
    }
}

private struct WorkDetailView: View {
    let work: MuxaWorkGroup
    @ObservedObject var model: AppModel
    @Environment(\.colorScheme) private var colorScheme

    private let columns = [
        GridItem(.adaptive(minimum: 250, maximum: 380), spacing: 12),
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
                        Label(work.pipelineRun.pipeline, systemImage: "point.3.connected.trianglepath.dotted")
                        Text("generation \(work.pipelineRun.generation)")
                        if !work.hostAliases.isEmpty {
                            Text("·")
                            Label(work.hostAliases.joined(separator: ", "), systemImage: "network")
                        }
                    }
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    Text(work.pipelineRun.cwd)
                        .font(.caption.monospaced())
                        .foregroundStyle(.tertiary)
                        .textSelection(.enabled)
                }

                HStack(spacing: 16) {
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
                    WorkMetric(
                        title: "Pipeline done",
                        value: "\(work.completedCount)/\(work.totalCount)",
                        color: .green
                    )
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
                        ForEach(unboundDesiredAgents(run: work.pipelineRun), id: \.alias) { desired in
                            PipelinePlaceholderCard(
                                desired: desired,
                                state: work.pipelineRun.aliases[desired.alias]
                            )
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
                    Text("\(participant.host.alias) · \(desired?.role ?? participant.agent.kind)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Label(agentStateLabel(participant.agent.state), systemImage: "circle.fill")
                    .labelStyle(.titleAndIcon)
                    .font(.caption.weight(.medium))
                    .foregroundStyle(agentStateColor(participant.agent.state))
            }

            if let summary, !summary.isEmpty {
                MarkdownContent(source: summary, lineLimit: 4)
            } else {
                Text("Waiting for work context")
                    .font(.subheadline)
                    .foregroundStyle(.tertiary)
            }

            HStack(spacing: 8) {
                if let pane = participant.pane {
                    Text("\(pane.session) › \(pane.windowName.isEmpty ? pane.stableWindowID : pane.windowName) › \(pane.paneID)")
                }
                if let model = participant.agent.model { Text(model) }
            }
            .font(.caption2.monospaced())
            .foregroundStyle(.tertiary)

            Button(action: openAgent) {
                Label("Open agent details", systemImage: "arrow.right.circle")
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.plain)
            .font(.caption.weight(.medium))
            .foregroundStyle(.tint)
        }
        .padding(14)
        .frame(maxWidth: .infinity, minHeight: 140, alignment: .topLeading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .stroke(.separator.opacity(0.55), lineWidth: 0.5)
        }
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
                    Text(desired.role ?? desired.program)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Label(agentStateLabel(state?.status ?? "pending"), systemImage: "circle.fill")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(agentStateColor(state?.status ?? "pending"))
            }
            if let task = desired.task, !task.isEmpty {
                MarkdownContent(source: task, lineLimit: 5)
            } else {
                Text("No live execution is currently bound.")
                    .font(.subheadline)
                    .foregroundStyle(.tertiary)
            }
            if let error = state?.error, !error.isEmpty {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, minHeight: 140, alignment: .topLeading)
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

    init(source: String, lineLimit: Int? = nil) {
        self.source = source
        self.lineLimit = lineLimit
    }

    private var attributed: AttributedString {
        (try? AttributedString(
            markdown: source,
            options: AttributedString.MarkdownParsingOptions(
                interpretedSyntax: .full,
                failurePolicy: .returnPartiallyParsedIfPossible
            )
        )) ?? AttributedString(source)
    }

    var body: some View {
        Text(attributed)
            .font(.subheadline)
            .lineLimit(lineLimit)
            .textSelection(.enabled)
            .frame(maxWidth: .infinity, alignment: .leading)
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
                    MarkdownContent(source: summary)
                        .font(.body)
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
    @Environment(\.colorScheme) private var colorScheme

    private var liveAgents: [MuxaAgent] {
        (host.remote?.agents ?? []).filter { $0.state != "stopped" }
    }

    private var panes: [MuxaPaneInfo] {
        host.remote?.panes ?? []
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                HStack(alignment: .top, spacing: 14) {
                    Circle()
                        .fill(fleetHostColor(host.state))
                        .frame(width: 12, height: 12)
                        .padding(.top, 9)
                    VStack(alignment: .leading, spacing: 4) {
                        Text(host.alias)
                            .font(.largeTitle.weight(.semibold))
                        Text(host.local ? "Local host" : host.state.replacingOccurrences(of: "_", with: " ").capitalized)
                            .foregroundStyle(fleetHostColor(host.state))
                    }
                }

                HStack(spacing: 16) {
                    HostMetric(title: "Agents", value: liveAgents.count)
                    HostMetric(title: "Panes", value: panes.count)
                    if !host.local, let latency = host.latencyMS {
                        HostMetric(title: "Latency", value: latency, suffix: " ms")
                    }
                }

                GroupBox("Connection") {
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
                    .padding(.vertical, 6)
                    .frame(maxWidth: .infinity, alignment: .leading)
                }

                if !liveAgents.isEmpty {
                    VStack(alignment: .leading, spacing: 9) {
                        Text("Agent sessions")
                            .font(.title2.weight(.semibold))
                        ForEach(liveAgents) { agent in
                            HStack(spacing: 10) {
                                Circle()
                                    .fill(agentStateColor(agent.state))
                                    .frame(width: 8, height: 8)
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(agent.aiTitle ?? agent.kind.replacingOccurrences(of: "_", with: " "))
                                    Text(agent.agentSessionID)
                                        .font(.caption.monospaced())
                                        .foregroundStyle(.secondary)
                                }
                                Spacer()
                                Text(agentStateLabel(agent.state))
                                    .font(.caption)
                                    .foregroundStyle(agentStateColor(agent.state))
                            }
                            .padding(.vertical, 4)
                        }
                    }
                }
            }
            .padding(28)
            .frame(maxWidth: 900, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(MuxaSurfacePalette.workspace(for: colorScheme).ignoresSafeArea())
    }
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
