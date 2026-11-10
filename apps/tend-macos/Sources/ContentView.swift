import GhosttyTerminal
import SwiftUI
import UniformTypeIdentifiers

struct ContentView: View {
    @EnvironmentObject private var model: AppModel
    @StateObject private var tabs = TendWorkbenchTabs()
    @State private var paletteMode: TendPaletteMode?
    @State private var pendingPaletteAction: TendPaletteAction?
    @State private var sidebarFocusRequest = UUID()
    @FocusState private var focusedEditorGroup: UUID?
    @State private var chrome = TitleBarMetrics.fallback
    @State private var showingShortcuts = false
    @AppStorage("tend.sidebar.visible") private var sidebarVisible = true

    /// Just the activity rail and its edge.
    private var collapsedSidebarWidth: CGFloat { TendTheme.activityBarWidth + 1 }

    /// How far the first tab strip moves in to clear the traffic lights:
    /// all the way with the side bar hidden, the part the rail does not
    /// cover while the lists are folded away, nothing otherwise.
    private var firstStripInset: CGFloat {
        if !sidebarVisible { return chrome.leadingInset }
        if model.isSidebarListCollapsed { return max(0, chrome.leadingInset - collapsedSidebarWidth) }
        return 0
    }

    var body: some View {
        VStack(spacing: 0) {
            HSplitView {
                if sidebarVisible {
                    TendSidebar(
                        model: model,
                        chrome: chrome,
                        openPinnedSession: { id in
                            model.selectWatchSession(id)
                            tabs.openPinned(.fleetSession(id))
                        },
                        openPinnedPane: { id in
                            model.selectWatchPane(id)
                            tabs.openPinned(.pane(id))
                        },
                        openFile: openFile,
                        closeShell: dismissExitedShell,
                        focusRequest: sidebarFocusRequest
                    )
                        // Each split pane is hosted on its own, so each opts out
                        // of the hidden title bar's inset: its top row is the
                        // title bar (see WorkbenchWindowChrome.swift).
                        .ignoresSafeArea(.container, edges: .top)
                        .frame(
                            minWidth: model.isSidebarListCollapsed ? collapsedSidebarWidth : 260,
                            idealWidth: model.isSidebarListCollapsed ? collapsedSidebarWidth : 300,
                            maxWidth: model.isSidebarListCollapsed ? collapsedSidebarWidth : 380
                        )
                }
                editorRegion
                    .frame(minWidth: 560)
            }
            // WS-C usage: the usage popover jumps to a capped agent's pane.
            WorkbenchStatusBar(model: model, openPane: { id in
                model.selectWatchPane(id)
                tabs.openPinned(.pane(id))
            })
        }
        .ignoresSafeArea(.container, edges: .top)
        .background(TitleBarMetricsReader(metrics: $chrome).allowsHitTesting(false))
        .frame(minWidth: 920, minHeight: 580)
        .alert(
            "Replace the running tendd?",
            isPresented: $model.isConfirmingDaemonReplacement
        ) {
            Button("Cancel", role: .cancel) {}
            Button("Replace Daemon", role: .destructive) {
                model.replaceRunningDaemon()
            }
        } message: {
            Text(
                "This stops the tendd currently using the socket, disables older background services that could reclaim it, and starts the version bundled with Tend. Native PTY sessions owned by the old daemon will end; tmux sessions will not be terminated."
            )
        }
        .sheet(item: $paletteMode, onDismiss: performPendingPaletteAction) { mode in
            CommandPaletteView(
                model: model,
                mode: mode,
                recent: tabs.groups.sorted { $0.id == tabs.focusedGroupID && $1.id != tabs.focusedGroupID }
                    .flatMap { $0.history.reversed() },
                onChoose: { action in
                    pendingPaletteAction = action
                    paletteMode = nil
                },
                onCancel: { paletteMode = nil }
            )
        }
        .sheet(isPresented: $showingShortcuts) {
            TendShortcutsSheet { showingShortcuts = false }
        }
        .sheet(isPresented: $model.isPresentingFleetDispatch) {
            FleetDispatchView(model: model)
        }
        .sheet(isPresented: $model.isPresentingWorkStart) {
            WorkStartView(model: model, isPresented: $model.isPresentingWorkStart)
        }
        .sheet(isPresented: $model.isPresentingHostRegistration) {
            HostRegistrationView(model: model)
        }
        // WS-F: snapshot
        .sheet(item: $model.sessionSnapshotSheet) { sheet in
            SessionSnapshotSheetHost(model: model, sheet: sheet)
        }
        // While the template manager is open, it presents the editor on
        // itself (a window shows one sheet at a time).
        .sheet(item: Binding(
            get: { model.isPresentingTemplates ? nil : model.pipelineEditorTarget },
            set: { model.pipelineEditorTarget = $0 }
        )) { target in
            PipelineEditorView(target: target, model: model)
        }

        .tendNewAgentPresentation(model: model, openEditor: openEditor) // WS-B: new agent
        .tendSwapSheet(model: model)
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
        .onChange(of: focusedEditorGroup) { groupID in
            guard let groupID else { return }
            tabs.focus(groupID)
            model.activateEditor(tabs.focusedSelection)
        }
        .background(WorkbenchWindowPresenter())
        // WS-A: unread tracking and notification-click routing.
        .modifier(TendAttentionTracking(model: model, tabs: tabs, attention: model.attention, open: openAgentPane))
        .focusedSceneValue(\.tendEditorCommands, editorCommands)
        .background(TendWorkbenchCommandBridge(actions: editorCommands))
        // The shortcut that works from any app opens the agents palette.
        .background(TendHotKeyReceiver(paletteOpen: paletteMode != nil) { paletteMode = .agents })
    }

    /// The window's own actions, at the trailing end of the last editor
    /// group's tab strip (the title bar row). The rarely used ones sit in a
    /// "…" menu so the strip stays one quiet row.
    private var windowActions: some View {
        HStack(spacing: 1) {
            Button {
                paletteMode = .navigation
            } label: {
                Label("Go to Anything", systemImage: "magnifyingglass")
            }
            .help("Go to Anything (⌘P)")

            Button {
                model.presentWorkStart()
            } label: {
                Label("Start Work", systemImage: "play.square.stack")
            }
            .help("Start Work")
            .disabled(!model.isConnected || model.isStartingWork)

            Button {
                model.createShell()
            } label: {
                Label("New shell", systemImage: "plus")
            }
            .help("New shell")
            .disabled(!model.isConnected || model.isCreatingSession)

            // WS-B: new agent
            Button {
                model.presentNewAgent()
            } label: {
                Label("New Agent…", systemImage: "sparkles.rectangle.stack")
            }
            .help("New Agent… (⌥⇧⌘T)")
            .disabled(!model.isConnected || model.isStartingAgent)

            Menu {
                Button("Open File…") { chooseFile() }
                Button("Show Files") { showFiles() }
                Divider()
                Button("Commands…") { paletteMode = .commands }
                Button("Refresh") { Task { await model.refresh() } }
                    .disabled(!model.isConnected)
                Divider()
                Button("Terminate Session", role: .destructive) {
                    model.terminateSelectedSession()
                }
                .disabled(
                    !model.isConnected
                        || model.selectedSessionID == nil
                        || model.isTerminatingSession
                )
            } label: {
                Image(systemName: "ellipsis")
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
            .frame(width: 22, height: 22)
            .help("More Actions")
        }
        .buttonStyle(.tendIcon)
    }

    @ViewBuilder
    private var editorRegion: some View {
        HSplitView {
            ForEach(tabs.groups) { group in
                VStack(spacing: 0) {
                    WorkspaceTabBar(
                        model: model,
                        tabs: tabs,
                        groupID: group.id,
                        height: chrome.rowHeight,
                        accessory: group.id == tabs.groups.last?.id ? AnyView(windowActions) : nil,
                        // With the side bar hidden the first strip reaches
                        // the window's leading edge and must clear the
                        // traffic lights.
                        leadingInset: group.id == tabs.groups.first?.id ? firstStripInset : 0
                    )
                    // Editors paint backgrounds that ignore the safe area;
                    // the strip is the title bar and must stay on top of
                    // them, for drawing and for clicks.
                    .zIndex(1)
                    detail(for: group.active, groupID: group.id)
                        // Buttons that don't pick a style of their own get
                        // the workbench's flat one instead of AppKit's bezel.
                        .buttonStyle(.tendSecondary)
                        .frame(
                            maxWidth: .infinity,
                            maxHeight: .infinity,
                            alignment: .topLeading
                        )
                        .overlay {
                            EditorDropOverlay(
                                groupID: group.id,
                                tabs: tabs,
                                session: TendTabDragSession.shared,
                                didDrop: { selection, _ in model.activateEditor(selection) }
                            )
                        }
                }
                .frame(
                    minWidth: tabs.groups.count > 1 ? 420 : nil,
                    maxWidth: .infinity,
                    maxHeight: .infinity,
                    alignment: .topLeading
                )
                .ignoresSafeArea(.container, edges: .top)
                .focusable()
                .tendFocusEffectDisabled()
                .focused($focusedEditorGroup, equals: group.id)
                .background(TendEditorGroupFocusBridge {
                    guard tabs.focusedGroupID != group.id else { return }
                    tabs.focus(group.id)
                    focusedEditorGroup = group.id
                    model.activateEditor(group.active)
                })
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }

    @ViewBuilder
    private func detail(
        for selection: TendSidebarSelection?,
        groupID: UUID
    ) -> some View {
        switch selection {
        case .file(let location):
            ArtifactPreviewView(location: location, model: model, isFocused: groupID == tabs.focusedGroupID, openFile: openFile)
                .id(location)
        case .workBoard:
            FleetHomeView(model: model)
        case .watch:
            NativeWatchView(model: model)
        case .inbox:
            TendOperatorInboxView(model: model)
        case .ask:
            TendAskView(model: model)
        case .work(let identity):
            if let work = model.workGroups.first(where: { $0.identity == identity }) {
                WorkDetailView(work: work, model: model)
                    .id(work.identity)
            } else {
                TendEmptyDetail(model: model)
            }
        case .agent(let id):
            if let participant = model.hostedAgents.first(where: { $0.id == id }) {
                FleetAgentDetailView(participant: participant, model: model)
                    .id(participant.id)
            } else {
                TendEmptyDetail(model: model)
            }
        case .host(let id):
            if let host = model.fleetHosts.first(where: { $0.id == id }) {
                FleetHostDetailView(host: host, model: model)
                    .id(host.id)
            } else {
                TendEmptyDetail(model: model)
            }
        case .fleetSession(let id):
            if let session = model.executionSnapshot.watchSession(id: id) {
                FleetSessionDetailView(session: session, model: model)
                    .id(session.id)
            } else {
                TendEmptyDetail(model: model)
            }
        case .fleetWindow(let id):
            if let window = model.executionSnapshot.watchWindow(id: id) {
                FleetWindowDetailView(window: window, model: model)
                    .id(window.id)
            } else {
                TendEmptyDetail(model: model)
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
                TendEmptyDetail(model: model)
            }
        case .pane(let id):
            if let pane = model.executionSnapshot.watchPane(id: id) {
                FleetPaneModuleView(pane: pane, model: model)
                    .id(pane.id)
            } else {
                TendEmptyDetail(model: model)
            }
        case nil:
            TendEmptyDetail(model: model)
        }
    }

    private func closeExitedShell(id: String, groupID: UUID) {
        let selection = TendSidebarSelection.shell(id)
        if tabs.focusedGroupID == groupID {
            model.activateEditor(tabs.close(selection, groupID: groupID))
        }
        Task { await model.refresh() }
    }

    /// The Shells sidebar's close button on an exited row: drop every editor
    /// tab that still shows the dead session, then refresh like the terminal's
    /// own exit handling does. tendd keeps the record for a while, so the
    /// sidebar hides the row itself.
    private func dismissExitedShell(id: String) {
        let selection = TendSidebarSelection.shell(id)
        model.activateEditor(tabs.closeEverywhere(selection))
        Task { await model.refresh() }
    }

    private var editorCommands: TendEditorCommandActions {
        // WS-A: nil while nothing is unread, which disables the menu item.
        let attention = model.attention
        let markAllRead: (() -> Void)? = attention.hasUnread ? { attention.markAllRead() } : nil
        return TendEditorCommandActions(
            // An empty workbench has no close-tab action; the window-owned
            // keymap consumes ⌘W so it cannot close the whole window.
            close: tabs.focusedSelection == nil ? nil : {
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
            },
            quickOpen: { paletteMode = .navigation },
            commandPalette: { paletteMode = .commands },
            activateAt: { index in
                if let selection = tabs.activateAt(index) { model.activateEditor(selection) }
            },
            activateLast: {
                if let selection = tabs.activateLast() { model.activateEditor(selection) }
            },
            focusRelativeGroup: { offset in
                model.activateEditor(tabs.focusRelativeGroup(offset))
                focusedEditorGroup = tabs.focusedGroupID
            },
            openWorkCommandCenter: { openEditor(.workBoard) },
            openAsk: { openEditor(.ask) },
            openInbox: { openEditor(.inbox) },
            selectSidebar: { model.show($0) },
            focusSidebar: {
                // Home folds the lists away; bring the current one out so
                // there is a filter to focus, then ask once it is built.
                if model.isSidebarListCollapsed {
                    model.show(model.sidebarMode)
                    sidebarVisible = true
                    DispatchQueue.main.async { sidebarFocusRequest = UUID() }
                    return
                }
                guard !sidebarVisible else {
                    sidebarFocusRequest = UUID()
                    return
                }
                // A side bar that is only now appearing is built with the
                // request already set, and `onChange` ignores a starting
                // value: ask again once it is on screen.
                sidebarVisible = true
                DispatchQueue.main.async { sidebarFocusRequest = UUID() }
            },
            jumpToAgent: { paletteMode = .agents },
            nextAttention: jumpToNextAttention,
            toggleSidebar: { sidebarVisible.toggle() },
            reopenClosed: {
                if let selection = tabs.reopenClosed(isAvailable: model.isSelectionAvailable) {
                    model.activateEditor(selection)
                }
            },
            showShortcuts: { showingShortcuts = true },
            markAllRead: markAllRead, // WS-A
            // WS-D: the focused pane or Work editor switches to Changes.
            showChanges: { NotificationCenter.default.post(name: .tendShowChanges, object: tabs.focusedSelection) },
            findInFile: fileFindAction,
            openFile: chooseFile,
            showFiles: showFiles,
            isEnabled: paletteMode == nil && !showingShortcuts && !model.isPresentingWorkStart
                && !model.isPresentingHostRegistration && model.pipelineEditorTarget == nil
                && !model.isConfirmingDaemonReplacement
        )
    }

    private var fileFindAction: (() -> Void)? {
        guard case .file = tabs.focusedSelection else { return nil }
        return { NotificationCenter.default.post(name: .tendFindInFile, object: nil) }
    }

    private func openFile(_ location: TendFileLocation, _ pinned: Bool) {
        if pinned { tabs.openPinned(.file(location)) } else { tabs.openPreview(.file(location)) }
        if pinned { focusedEditorGroup = tabs.focusedGroupID }
        model.activateEditor(.file(location))
    }

    private func showFiles() {
        if let directory = model.activeFileDirectory { model.fileRoot = directory }
        sidebarVisible = true
        model.show(.files)
    }

    private func chooseFile() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = true
        panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let url = panel.url else { return }
        var directory: ObjCBool = false
        FileManager.default.fileExists(atPath: url.path, isDirectory: &directory)
        let location = TendFileLocation(hostAlias: "local", path: url.path)
        sidebarVisible = true
        model.show(.files)
        model.fileRoot = directory.boolValue ? location : location.parent
        if !directory.boolValue { openFile(location, true) }
    }

    private func openEditor(_ selection: TendSidebarSelection) {
        guard model.isSelectionAvailable(selection) else { return }
        tabs.openPinned(selection)
        model.select(selection)
        model.activateEditor(selection)
    }

    /// ⇧⌘J: opens the next pane whose agent is waiting on the operator,
    /// cycling across every host in ⌘J order from the pane being looked at.
    private func jumpToNextAttention() {
        let candidates = TendAttention.cycle(in: model.executionSnapshot.watchHosts)
        let current: TendWatchPaneIdentity? = if case .pane(let id) = tabs.focusedSelection { id } else { nil }
        guard let next = TendAttention.next(after: current, in: candidates) else {
            NSSound.beep()
            return
        }
        openAgentPane(next)
    }

    /// Pins a pane in the focused group: ⇧⌘J and a notification click.
    private func openAgentPane(_ pane: TendWatchPaneIdentity) {
        model.selectWatchPane(pane)
        tabs.openPinned(.pane(pane))
        model.activateEditor(.pane(pane))
    }

    private func performPendingPaletteAction() {
        guard let action = pendingPaletteAction else { return }
        pendingPaletteAction = nil
        switch action {
        case .navigate(let selection):
            openEditor(selection)
        case .command(let command):
            guard command.disabledReason(model: model) == nil else { return }
            switch command {
            case .startWork: model.presentWorkStart()
            case .workCommandCenter: openEditor(.workBoard)
            case .liveWatch: openEditor(.watch)
            case .ask: openEditor(.ask)
            case .newShell: model.createShell()
            case .showWork: model.showHome()
            case .showWatch: model.show(.watch)
            case .showInbox: model.show(.inbox)
            case .showShells: model.show(.shells)
            case .refresh: Task { await model.refresh() }
            case .closeEditor: editorCommands.close?()
            case .previousEditor: editorCommands.previous?()
            case .nextEditor: editorCommands.next?()
            case .splitEditor: editorCommands.splitRight?()
            case .pinEditor: editorCommands.pin?()
            case .focusSidebar: editorCommands.focusSidebar?()
            case .jumpToAgent:
                // Reopen as the agent palette once this one has closed.
                DispatchQueue.main.async { paletteMode = .agents }
            case .nextAttention: jumpToNextAttention()
            case .toggleSidebar: sidebarVisible.toggle()
            case .reopenEditor: editorCommands.reopenClosed?()
            case .showShortcuts: showingShortcuts = true
            case .markAllRead: model.attention.markAllRead() // WS-A
            case .showChanges: editorCommands.showChanges?() // WS-D
            // WS-B: new agent
            case .newAgent: model.presentNewAgent()
            case .newDefaultAgent: Task { await model.quickStartAgent() }
            // WS-F: snapshot
            case .saveSnapshot: model.presentSessionSnapshots(.save)
            case .restoreSnapshot: model.presentSessionSnapshots(.restore)
            case .switchLimitedAgents:
                TendSwitchLimitedButton.presentSwitch(
                    for: model.hostedAgents.filter { $0.agent.status.isLimited }, model: model
                )
            case .fleetDispatch: model.isPresentingFleetDispatch = true
            case .showFiles: editorCommands.showFiles?()
            }
        }
    }
}

/// SwiftUI can restore a menu-bar application's Window scene offscreen after
/// its content state has already loaded. Reveal the canonical workbench at the
/// point where its content is actually attached to an NSWindow.
func agentStateLabel(_ state: String) -> String {
    switch state {
    case "waiting_input": String(localized: "Waiting for input")
    case "waiting_choice": String(localized: "Waiting for choice")
    case "working": String(localized: "Working")
    case "starting": String(localized: "Starting")
    case "idle": String(localized: "Idle")
    case "error", "failed": String(localized: "Error")
    case "blocked": String(localized: "Blocked")
    case "done": String(localized: "Done")
    case "pending": String(localized: "Pending")
    case "stopped": String(localized: "Stopped")
    default: state.replacingOccurrences(of: "_", with: " ").capitalized
    }
}

/// Display wording for a fleet host `state` as tendd reports it.
func fleetHostStateLabel(_ state: String) -> String {
    switch state {
    case "online": String(localized: "Online")
    case "offline": String(localized: "Offline")
    case "connecting": String(localized: "Connecting")
    case "degraded": String(localized: "Degraded")
    case "version_skew": String(localized: "Version skew")
    case "auth_failed": String(localized: "Authentication failed")
    case "disabled": String(localized: "Disabled")
    default: state.replacingOccurrences(of: "_", with: " ").capitalized
    }
}

/// Display wording for a fleet host access `mode` (`observe` or `control`).
func fleetHostModeLabel(_ mode: String) -> String {
    switch mode {
    case "observe": String(localized: "Observe")
    case "control": String(localized: "Control")
    default: mode.capitalized
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
    @StateObject private var pane: TerminalPaneModel
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.openWindow) private var openWindow
    private let sessionID: String
    private let showsToolbar: Bool
    private let onExit: () -> Void

    init(
        client: TendIPCClient,
        sessionID: String,
        replayInitialHistory: Bool,
        showsToolbar: Bool = true,
        onExit: @escaping () -> Void = {}
    ) {
        self.sessionID = sessionID
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
            TendSurfacePalette.terminal(for: colorScheme)
                .ignoresSafeArea()

            TerminalSurfaceView(context: pane.terminalState)
                .background(TendSurfacePalette.terminal(for: colorScheme))
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .clipped()

            if showsToolbar {
                VStack {
                    HStack(spacing: 8) {
                        Spacer()
                        Button {
                            openWindow(value: TendModuleRoute.shell(sessionID))
                        } label: {
                            Image(systemName: "macwindow.on.rectangle")
                        }
                        .buttonStyle(.tendIcon)
                        .help("Open this Shell in a separate window")
                    }
                    .padding(8)
                    Spacer()
                }
            }

            if pane.outputWasTruncated {
                Label("Earlier output was truncated by tendd's retained buffer", systemImage: "exclamationmark.triangle")
                    .font(TendType.detail)
                    .padding(7)
                    .background(.ultraThinMaterial, in: Capsule())
                    .padding(8)
            } else if let error = pane.errorMessage {
                Label(error, systemImage: "bolt.horizontal.circle")
                    .font(TendType.detail)
                    .padding(7)
                    .background(.ultraThinMaterial, in: Capsule())
                    .padding(8)
            } else if pane.exited {
                Label {
                    if let status = pane.exitStatus {
                        Text("Session ended (status \(status))")
                    } else {
                        Text("Session ended")
                    }
                } icon: {
                    Image(systemName: pane.exitStatus == 0 ? "checkmark.circle" : "stop.circle")
                }
                .font(TendType.detail)
                .padding(.horizontal, 10)
                .padding(.vertical, 7)
                .background(.regularMaterial, in: Capsule())
                .padding(8)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .clipped()
        .onAppear { pane.start() }
        .task(id: sessionID) {
            // The embedded Live Pane is inserted after the attach request
            // completes. Give AppKit one run-loop turn to put the native
            // Ghostty surface in a window, then deterministically move the
            // first responder to it so the first keystroke is not lost to
            // the inspector or sidebar that initiated the attach.
            await Task.yield()
            try? await Task.sleep(for: .milliseconds(120))
            guard !Task.isCancelled else { return }
            pane.focus()
        }
        .onDisappear { pane.stop() }
        .onChange(of: pane.exited) { exited in
            if exited { onExit() }
        }
    }
}

enum TendSurfacePalette {
    static func editor(for colorScheme: ColorScheme) -> Color {
        workspace(for: colorScheme)
    }

    static func sidebar(for colorScheme: ColorScheme) -> Color {
        TendTheme.sideBar(colorScheme)
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
        TendTheme.editor(colorScheme)
    }
}
