import GhosttyTerminal
import SwiftUI

struct ContentView: View {
    @EnvironmentObject private var model: AppModel
    @StateObject private var tabs = MuxaWorkbenchTabs()
    @State private var paletteMode: MuxaPaletteMode?
    @State private var pendingPaletteAction: MuxaPaletteAction?
    @State private var sidebarFocusRequest = UUID()
    @FocusState private var focusedEditorGroup: UUID?
    @State private var chrome = TitleBarMetrics.fallback
    @State private var showingShortcuts = false
    @AppStorage("muxa.sidebar.visible") private var sidebarVisible = true

    var body: some View {
        VStack(spacing: 0) {
            HSplitView {
                if sidebarVisible {
                    MuxaSidebar(
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
                        closeShell: dismissExitedShell,
                        focusRequest: sidebarFocusRequest
                    )
                        // Each split pane is hosted on its own, so each opts out
                        // of the hidden title bar's inset: its top row is the
                        // title bar (see WorkbenchWindowChrome.swift).
                        .ignoresSafeArea(.container, edges: .top)
                        .frame(minWidth: 260, idealWidth: 300, maxWidth: 380)
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
            MuxaShortcutsSheet { showingShortcuts = false }
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
        .sheet(item: $model.pipelineEditorTarget) { target in
            PipelineEditorView(target: target, model: model)
        }
        .muxaNewAgentPresentation(model: model, openEditor: openEditor) // WS-B: new agent
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
        .modifier(MuxaAttentionTracking(model: model, tabs: tabs, attention: model.attention, open: openAgentPane))
        .focusedSceneValue(\.muxaEditorCommands, editorCommands)
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
        .buttonStyle(.muxaIcon)
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
                        leadingInset: !sidebarVisible && group.id == tabs.groups.first?.id
                            ? chrome.leadingInset : 0
                    )
                    // Editors paint backgrounds that ignore the safe area;
                    // the strip is the title bar and must stay on top of
                    // them, for drawing and for clicks.
                    .zIndex(1)
                    detail(for: group.active, groupID: group.id)
                        // Buttons that don't pick a style of their own get
                        // the workbench's flat one instead of AppKit's bezel.
                        .buttonStyle(.muxaSecondary)
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
                .ignoresSafeArea(.container, edges: .top)
                .focusable()
                .muxaFocusEffectDisabled()
                .focused($focusedEditorGroup, equals: group.id)
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
                FleetAgentDetailView(participant: participant, model: model)
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

    /// The Shells sidebar's close button on an exited row: drop every editor
    /// tab that still shows the dead session, then refresh like the terminal's
    /// own exit handling does. muxad keeps the record for a while, so the
    /// sidebar hides the row itself.
    private func dismissExitedShell(id: String) {
        let selection = MuxaSidebarSelection.shell(id)
        model.activateEditor(tabs.closeEverywhere(selection))
        Task { await model.refresh() }
    }

    private var editorCommands: MuxaEditorCommandActions {
        // WS-A: nil while nothing is unread, which disables the menu item.
        let attention = model.attention
        let markAllRead: (() -> Void)? = attention.hasUnread ? { attention.markAllRead() } : nil
        return MuxaEditorCommandActions(
            // nil once no tab is open, so ⌘W falls through to closing the
            // window.
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
            showChanges: { NotificationCenter.default.post(name: .muxaShowChanges, object: tabs.focusedSelection) },
            isEnabled: paletteMode == nil && !showingShortcuts && !model.isPresentingWorkStart
                && !model.isPresentingHostRegistration && model.pipelineEditorTarget == nil
                && !model.isConfirmingDaemonReplacement
        )
    }

    private func openEditor(_ selection: MuxaSidebarSelection) {
        guard model.isSelectionAvailable(selection) else { return }
        tabs.openPinned(selection)
        model.select(selection)
        model.activateEditor(selection)
    }

    /// ⇧⌘J: opens the next pane whose agent is waiting on the operator,
    /// cycling in topology order from the pane being looked at.
    private func jumpToNextAttention() {
        let candidates = model.executionSnapshot.watchHosts
            .flatMap(\.sessions).flatMap(\.windows).flatMap(\.panes)
            .filter(MuxaAttention.needsAttention)
            .map(\.id)
        let current: MuxaWatchPaneIdentity? = if case .pane(let id) = tabs.focusedSelection { id } else { nil }
        guard let next = MuxaAttention.next(after: current, in: candidates) else {
            NSSound.beep()
            return
        }
        openAgentPane(next)
    }

    /// Pins a pane in the focused group: ⇧⌘J and a notification click.
    private func openAgentPane(_ pane: MuxaWatchPaneIdentity) {
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
            case .showWork: model.show(.work)
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
            }
        }
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
    /// The title bar row's height: the strip is the window's title bar.
    let height: CGFloat
    /// The window's actions, carried by the last group's strip only.
    let accessory: AnyView?
    var leadingInset: CGFloat = 0
    @Environment(\.openWindow) private var openWindow

    private var group: MuxaWorkbenchTabs.Group? {
        tabs.group(id: groupID)
    }

    var body: some View {
        HStack(spacing: 0) {
            if leadingInset > 0 {
                WindowDragArea().frame(width: leadingInset)
            }
            GeometryReader { strip in
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
                stateColor: agentState(for: selection).map(agentStateColor),
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
        case .workBoard:
            String(localized: "Work Command Center")
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
    private func agentState(for selection: MuxaSidebarSelection) -> String? {
        switch selection {
        case .pane(let id): model.executionSnapshot.watchPane(id: id)?.agent?.state
        case .agent(let id): model.hostedAgents.first { $0.id == id }?.agent.state
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
                        .font(.system(size: 11))
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


private struct MuxaSidebar: View {
    private enum StatusScope: CaseIterable, Identifiable {
        case all
        case attention
        case active

        var id: Self { self }

        var title: String {
            switch self {
            case .all: String(localized: "All")
            case .attention: String(localized: "Attention")
            case .active: String(localized: "Active")
            }
        }

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
            case .host: String(localized: "Host tree")
            case .status: String(localized: "Status groups")
            case .none: String(localized: "No groups")
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
            case .attention: String(localized: "Needs attention")
            case .active: String(localized: "Active")
            case .idle: String(localized: "Idle agents")
            case .shell: String(localized: "Shell panes")
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
            case .topology: String(localized: "Topology")
            case .recent: String(localized: "Latest activity")
            case .myPrompt: String(localized: "My latest prompt")
            case .agentActivity: String(localized: "Agent latest update")
            }
        }

        var compactTitle: String {
            switch self {
            case .topology: String(localized: "Topology")
            case .recent: String(localized: "Last activity")
            case .myPrompt: String(localized: "My prompt")
            case .agentActivity: String(localized: "Agent update")
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
    let chrome: TitleBarMetrics
    let openPinnedSession: (MuxaWatchSessionIdentity) -> Void
    let openPinnedPane: (MuxaWatchPaneIdentity) -> Void
    let closeShell: (String) -> Void
    let focusRequest: UUID
    @FocusState private var filterFocused: Bool
    @Environment(\.colorScheme) private var colorScheme
    @State private var filterText = ""
    @State private var statusScope: StatusScope = .all
    /// Exited shells the user closed from the Shells list. muxad keeps an
    /// exited session listed for a while, so the sidebar hides these until
    /// the daemon drops them.
    @State private var dismissedShellIDs: Set<String> = []
    @State private var remoteShellError: String?
    @State private var isOpeningRemoteShell = false
    @AppStorage("muxa.explore.sort") private var exploreSort: ExploreSort = .topology
    @AppStorage("muxa.explore.grouping") private var exploreGrouping: ExploreGrouping = .host

    private var background: Color {
        MuxaSurfacePalette.sidebar(for: colorScheme)
    }

    var body: some View {
        ZStack {
            background

            VStack(spacing: 0) {
                // The title bar row: the traffic lights, then the view's
                // title and actions (see WorkbenchWindowChrome.swift).
                HStack(spacing: 0) {
                    Color.clear.frame(width: max(0, chrome.leadingInset - 14))
                    sectionTitle
                }
                .frame(height: chrome.rowHeight)
                .background(alignment: .bottom) {
                    ZStack(alignment: .bottom) {
                        WindowDragArea()
                        MuxaTheme.border(colorScheme).frame(height: 1)
                            .allowsHitTesting(false)
                    }
                }

                HStack(spacing: 0) {
                    SidebarActivityRail(model: model, attention: model.attention)

                    MuxaTheme.border(colorScheme).frame(width: 1)

                    VStack(spacing: 0) {
                        MuxaFilterField(
                            prompt: model.sidebarMode.filterPrompt,
                            text: $filterText,
                            focused: $filterFocused
                        ) {
                            Menu {
                                Picker("Status", selection: $statusScope) {
                                    ForEach(StatusScope.allCases) { scope in
                                        Label(scope.title, systemImage: scope.systemImage)
                                            .tag(scope)
                                    }
                                }
                            } label: {
                                Image(systemName: "line.3.horizontal.decrease")
                                    .foregroundStyle(statusScope == .all ? Color.secondary : Color.accentColor)
                            }
                            .menuStyle(.borderlessButton)
                            .menuIndicator(.hidden)
                            .fixedSize()
                            .frame(width: 20, height: 20)
                            .help("Filter by status")
                        }
                        .onChange(of: focusRequest) { _ in filterFocused = true }
                        .padding(.horizontal, 10)
                        .padding(.top, 8)
                        .padding(.bottom, 6)

                        List {
                            contextualRows
                        }
                        .environment(\.defaultMinListRowHeight, MuxaTheme.rowHeight)
                        .buttonStyle(.muxaSecondary)
                        .listStyle(.sidebar)
                        .scrollContentBackground(.hidden)
                        .background(Color.clear)
                    }
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .onChange(of: model.sidebarMode) { _ in
            filterText = ""
            statusScope = .all
            remoteShellError = nil
        }
        .onChange(of: model.sessions) { sessions in
            dismissedShellIDs = dismissedShellIDs.filter { id in
                sessions.contains { $0.id == id }
            }
        }
    }

    private var sectionTitle: some View {
        MuxaSectionTitle(title: model.sidebarMode.title) {
            Text(verbatim: sidebarCountLabel)
                .font(.system(size: 10, weight: .medium).monospacedDigit())
                .foregroundStyle(.secondary)
                .padding(.horizontal, 5)
                .frame(minHeight: 16)
                .background(Color.primary.opacity(0.07), in: Capsule())
                .padding(.trailing, 4)
            if model.sidebarMode == .watch {
                Button {
                    model.select(.watch)
                } label: {
                    Image(systemName: "rectangle.on.rectangle")
                }
                .buttonStyle(.muxaIcon)
                .help("Open Live Watch")
                Button {
                    model.presentHostRegistration()
                } label: {
                    Image(systemName: "plus")
                }
                .buttonStyle(.muxaIcon)
                .help("Register SSH Host")
            }
            if model.sidebarMode == .ask {
                Button {
                    Task { await model.resetAskConversation() }
                    model.select(.ask)
                } label: {
                    Image(systemName: "plus.bubble")
                }
                .buttonStyle(.muxaIcon)
                .disabled(!model.isConnected)
                .help("New Conversation")
            }
            if model.sidebarMode == .shells {
                Button {
                    model.createShell()
                } label: {
                    Image(systemName: "plus")
                }
                .buttonStyle(.muxaIcon)
                .disabled(!model.isConnected || model.isCreatingSession)
                .help("New native shell")
                Menu {
                    if model.remoteShellHosts.isEmpty {
                        Text("No online fleet hosts")
                    } else {
                        ForEach(model.remoteShellHosts) { host in
                            Button {
                                openRemoteShell(on: host)
                            } label: {
                                Label(host.alias, systemImage: "server.rack")
                            }
                        }
                    }
                } label: {
                    Image(systemName: "network")
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
                .frame(width: 22, height: 22)
                .disabled(!model.isConnected || isOpeningRemoteShell)
                .help("New shell on a fleet host")
                MuxaModuleMenu(
                    context: .app,
                    model: model,
                    registry: MuxaModuleRegistry.shared
                )
            }
            if model.sidebarMode == .watch {
                Menu {
                    Picker("Group", selection: $exploreGrouping) {
                        ForEach(ExploreGrouping.allCases) { grouping in
                            Label(grouping.title, systemImage: grouping.systemImage)
                                .tag(grouping)
                        }
                    }
                    Picker("Order", selection: $exploreSort) {
                        ForEach(ExploreSort.allCases) { order in
                            Label(order.title, systemImage: order.systemImage)
                                .tag(order)
                        }
                    }
                    // WS-F: snapshot
                    Divider()
                    SessionSnapshotMenuItems(model: model)
                } label: {
                    Image(systemName: "ellipsis")
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
                .frame(width: 22, height: 22)
                .help("Group by \(exploreGrouping.title.lowercased()), ordered by \(exploreSort.title.lowercased())")
            }
        }
    }

    private func openInLiveWatch(_ participant: MuxaHostedAgent) {
        model.openInLiveWatch(participant)
    }

    private func openRemoteShell(on host: MuxaFleetHost) {
        guard !isOpeningRemoteShell else { return }
        isOpeningRemoteShell = true
        remoteShellError = nil
        Task {
            defer { isOpeningRemoteShell = false }
            do {
                try await model.createShell(sshHost: host)
            } catch {
                remoteShellError = error.localizedDescription
            }
        }
    }

    private func dismissShell(_ session: MuxaSession) {
        dismissedShellIDs.insert(session.id)
        closeShell(session.id)
    }

    private var sidebarCount: Int {
        switch model.sidebarMode {
        case .work: model.workGroups.count
        case .watch: model.executionSnapshot.watchHosts.reduce(0) { $0 + $1.paneCount }
        case .inbox: inboxBadgeCount
        case .ask: askConversations.count
        case .shells: model.sessions.lazy.filter { !$0.exited }.count
        }
    }

    private var filteredSidebarCount: Int {
        switch model.sidebarMode {
        case .work: filteredWorkGroups.count
        case .watch: filteredWatchHosts.reduce(0) { $0 + $1.paneCount }
        case .inbox: inboxBadgeCount
        case .ask: filteredAskConversations.count
        case .shells: filteredSessions.count
        }
    }

    /// The selected provider's conversations, newest activity first — the
    /// same list the Ask editor's menu offers, as sidebar rows.
    private var askConversations: [MuxaAskConversation] {
        model.askConversations
            .filter { $0.agent == model.askAgent }
            .sorted { $0.updatedAt > $1.updatedAt }
    }

    /// Status scopes read the conversation's newest turn: Active while it is
    /// still being answered, Attention when it failed.
    private var filteredAskConversations: [MuxaAskConversation] {
        let latest = Dictionary(
            model.askEntries.compactMap { entry in entry.conversationID.map { ($0, entry) } },
            uniquingKeysWith: { first, second in first.askedAt >= second.askedAt ? first : second }
        )
        return askConversations.filter { conversation in
            let newest = latest[conversation.id]
            return matchesFilter([conversation.title])
                && matchesScope(
                    attention: newest?.status == "failed",
                    active: newest?.status == "running"
                )
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

    /// Exited shells still listed by muxad and not yet closed from the
    /// sidebar. They are neither "active" nor "attention", so a status
    /// scope hides them.
    private var visibleExitedSessions: [MuxaSession] {
        guard statusScope == .all else { return [] }
        return model.sessions.filter {
            $0.exited
                && !dismissedShellIDs.contains($0.id)
                && matchesFilter([$0.displayName, $0.id])
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
        return commandAttention + attentionAgents.count
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
            }
            Section("Needs attention") {
                if attentionAgents.isEmpty {
                    SidebarEmptyRow(title: "Nothing needs attention", systemImage: "checkmark.circle")
                } else if filteredAttentionAgents.isEmpty {
                    SidebarEmptyRow(title: "No matching requests", systemImage: "line.3.horizontal.decrease.circle")
                } else {
                    ForEach(filteredAttentionAgents) { participant in
                        // The row is the selection; the sidebar stays on
                        // Inbox and the detail explains the request. The
                        // trailing button keeps one-click access to the
                        // pane in Live Watch.
                        HStack(spacing: 2) {
                            Button {
                                model.select(.agent(participant.id))
                            } label: {
                                InboxAgentRow(participant: participant)
                            }
                            .buttonStyle(.plain)
                            if participant.pane != nil {
                                Button {
                                    openInLiveWatch(participant)
                                } label: {
                                    Image(systemName: "rectangle.on.rectangle")
                                        .foregroundStyle(.secondary)
                                }
                                .buttonStyle(.muxaIcon)
                                .controlSize(.small)
                                .help("Open in Live Watch")
                            }
                        }
                        .listRowBackground(
                            model.sidebarSelection == .agent(participant.id)
                                ? Color.accentColor.opacity(0.14) : Color.clear
                        )
                        .contextMenu {
                            Button("Open in Live Watch") {
                                openInLiveWatch(participant)
                            }
                            .disabled(participant.pane == nil)
                        }
                    }
                }
            }
        case .ask:
            Section("Global Ask") {
                Button {
                    model.select(.ask)
                } label: {
                    GlobalAskRow(conversationCount: askConversations.count, agent: model.askAgent)
                }
                .buttonStyle(.plain)
                .listRowBackground(
                    model.sidebarSelection == .ask
                        ? Color.accentColor.opacity(0.14) : Color.clear
                )
            }
            Section("Conversations") {
                if askConversations.isEmpty {
                    SidebarEmptyRow(title: "No conversations yet", systemImage: "bubble.left.and.bubble.right")
                } else if filteredAskConversations.isEmpty {
                    SidebarEmptyRow(title: "No matching conversations", systemImage: "line.3.horizontal.decrease.circle")
                } else {
                    ForEach(filteredAskConversations) { conversation in
                        Button {
                            Task { await model.selectAskConversation(conversation.id) }
                            model.select(.ask)
                        } label: {
                            AskConversationRow(
                                conversation: conversation,
                                isActive: conversation.id == model.activeAskConversationID
                            )
                        }
                        .buttonStyle(.plain)
                    }
                }
            }
        case .shells:
            Section("Native shells") {
                if let remoteShellError {
                    Label(remoteShellError, systemImage: "exclamationmark.triangle")
                        .font(.caption)
                        .foregroundStyle(.red)
                        .lineLimit(3)
                        .listRowBackground(Color.clear)
                }
                if model.sessions.allSatisfy(\.exited), visibleExitedSessions.isEmpty {
                    ShellsEmptyRow(
                        canCreate: model.isConnected && !model.isCreatingSession,
                        create: model.createShell
                    )
                } else if filteredSessions.isEmpty, visibleExitedSessions.isEmpty {
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
                    ForEach(visibleExitedSessions) { session in
                        // Not selectable: the model drops an exited shell
                        // from the available selections, so a click would
                        // only bounce back to the Work board.
                        HStack(spacing: 2) {
                            SessionRow(session: session)
                            Button {
                                dismissShell(session)
                            } label: {
                                Image(systemName: "xmark")
                                    .foregroundStyle(.secondary)
                            }
                            .buttonStyle(.muxaIcon)
                            .controlSize(.small)
                            .help("Remove exited shell")
                        }
                        .listRowBackground(Color.clear)
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
                    Section {
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
                    } header: {
                        Text(verbatim: "\(group.bucket.title) · \(group.panes.count)")
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
    @ObservedObject var attention: MuxaAgentAttentionCenter // WS-A

    var body: some View {
        VStack(spacing: 0) {
            ForEach(MuxaSidebarMode.allCases) { mode in
                ActivityRailItem(
                    mode: mode,
                    active: model.sidebarMode == mode,
                    badge: attentionCount(for: mode),
                    select: { model.show(mode) }
                )
            }
            Spacer(minLength: 0)
        }
        .padding(.top, 4)
        .frame(width: MuxaTheme.activityBarWidth)
        .frame(maxHeight: .infinity)
        .background(MuxaTheme.activityBar(colorScheme))
    }

    @Environment(\.colorScheme) private var colorScheme

    private func attentionCount(for mode: MuxaSidebarMode) -> Int {
        switch mode {
        case .work:
            return model.workGroups.lazy.filter { $0.attentionCount > 0 }.count
        case .watch:
            return attention.unreadPanes.count // WS-A
        case .inbox:
            let agentAttention = model.hostedAgents.lazy.filter {
                ["waiting_input", "waiting_choice", "blocked", "error", "failed"]
                    .contains($0.agent.state)
            }.count
            let commandAttention = model.operatorMessages.lazy.filter {
                $0.needsReply || $0.hasUnreadReply
            }.count
            return agentAttention + commandAttention
        case .ask:
            // A question still being answered is the one thing worth a dot.
            return model.askEntries.lazy.filter { $0.status == "running" }.count
        case .shells:
            return 0
        }
    }
}

/// One activity bar icon: dimmed at rest, full strength with a 2pt accent bar
/// on the leading edge when its view is showing, a count badge in the corner.
private struct ActivityRailItem: View {
    let mode: MuxaSidebarMode
    let active: Bool
    let badge: Int
    let select: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: select) {
            Image(systemName: mode.systemImage)
                .font(.system(size: 17, weight: .regular))
                .frame(width: MuxaTheme.activityBarWidth, height: 44)
                .contentShape(Rectangle())
                .overlay(alignment: .leading) {
                    Rectangle()
                        .fill(active ? Color.accentColor : Color.clear)
                        .frame(width: 2)
                }
                .overlay(alignment: .bottomTrailing) {
                    if badge > 0 {
                        Text(verbatim: badge > 99 ? "99+" : "\(badge)")
                            .font(.system(size: 9, weight: .semibold).monospacedDigit())
                            .foregroundStyle(.white)
                            .padding(.horizontal, badge > 9 ? 4 : 0)
                            .frame(minWidth: 15, minHeight: 15)
                            .background(Color.accentColor, in: Capsule())
                            .offset(x: -7, y: -8)
                    }
                }
        }
        .buttonStyle(.plain)
        .foregroundStyle(Color.primary.opacity(active || hovering ? 0.92 : 0.45))
        .onHover { hovering = $0 }
        .help(mode.title)
        .accessibilityLabel(mode.title)
    }
}

private struct SidebarEmptyRow: View {
    let title: LocalizedStringKey
    let systemImage: String

    var body: some View {
        Label(title, systemImage: systemImage)
            .foregroundStyle(.secondary)
            .listRowBackground(Color.clear)
    }
}

/// The Shells tab with nothing to list: a hint plus the same action as the
/// toolbar's "New shell", so the tab can create what it shows.
private struct ShellsEmptyRow: View {
    let canCreate: Bool
    let create: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("No native shells", systemImage: "terminal")
                .foregroundStyle(.secondary)
            Text("Open a terminal on this Mac, or on an online fleet host from the network menu above.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Button(action: create) {
                Label("New Shell", systemImage: "plus")
            }
            .controlSize(.small)
            .disabled(!canCreate)
        }
        .padding(.vertical, 4)
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
                Text("@\(agent) · \(conversationCount) conversations")
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

/// One Global Ask conversation in the Ask container's sidebar.
private struct AskConversationRow: View {
    let conversation: MuxaAskConversation
    let isActive: Bool

    private var updated: Date? {
        (try? Date(conversation.updatedAt, strategy: .iso8601))
            ?? ISO8601DateFormatter().date(from: conversation.updatedAt)
    }

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: isActive ? "bubble.left.and.bubble.right.fill" : "bubble.left.and.bubble.right")
                .foregroundStyle(isActive ? Color.accentColor : Color.secondary)
                .frame(width: 18)
            VStack(alignment: .leading, spacing: 2) {
                Text(conversation.title)
                    .fontWeight(isActive ? .semibold : .regular)
                    .lineLimit(1)
                if let updated {
                    Text(updated, style: .relative)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer(minLength: 4)
        }
        .padding(.horizontal, 4)
        .frame(maxWidth: .infinity, minHeight: 30, alignment: .leading)
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
        presentText(participant.agent.lastResponse)
            ?? presentText(participant.agent.recap)
            ?? presentText(participant.agent.lastNotification)
            ?? presentText(participant.agent.lastPrompt)
            ?? String(localized: "Waiting for input")
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
                if healthy {
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
        .font(.system(size: 11))
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
            configuration.icon.font(.system(size: 10))
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
        .font(.caption)
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
                Text(verbatim: "\(attentionCount)")
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
                    .foregroundStyle(session.exited ? .secondary : .primary)
                    .lineLimit(1)
                HStack(spacing: 6) {
                    if session.exited {
                        Text(session.shellStateText)
                    } else {
                        if let pid = session.pid { Text("pid \(pid)") }
                        if session.attachedClients > 0 { Text("\(session.attachedClients) attached") }
                    }
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
                        Text(verbatim: "· \(work.hostAliases.joined(separator: ", "))")
                    }
                }
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 4)
            Group {
                if work.pipelineRun == nil {
                    Text("\(work.participants.count) agents")
                } else {
                    Text(verbatim: "\(work.completedCount)/\(work.totalCount)")
                }
            }
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
        guard let pane = participant.pane else { return String(localized: "no pane binding") }
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
                Text(verbatim: "\(participant.host.alias) · \(participant.agent.agentSessionID)")
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
    @State private var tab: WorkDetailTab = .overview // WS-D

    private let columns = [
        GridItem(.adaptive(minimum: 250, maximum: 390), spacing: 12, alignment: .top),
    ]

    private let metricColumns = [
        GridItem(.adaptive(minimum: 112, maximum: 180), spacing: 12),
    ]

    // WS-D: Overview | Changes; Overview is the page below, unchanged.
    var body: some View {
        VStack(spacing: 0) {
            WorkDetailTabBar(work: work, tab: $tab)
            switch tab {
            case .overview: overview
            case .changes: ChangesWorkspaceView(work: work, model: model)
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: .muxaShowChanges)) { note in
            if ChangesShowRequest.matches(note.object, work: work) { tab = .changes }
        }
    }

    private var overview: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                VStack(alignment: .leading, spacing: 5) {
                    Text(work.workspaceID.uppercased())
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    Text(work.title)
                        .font(.system(size: 22, weight: .semibold))
                    HStack(spacing: 8) {
                        Label(work.pipelineLabel, systemImage: "point.3.connected.trianglepath.dotted")
                        if let generation = work.pipelineRun?.generation {
                            Text("generation \(generation)")
                        } else {
                            Text("observed from tmux metadata")
                        }
                        if !work.hostAliases.isEmpty {
                            Text(verbatim: "·")
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
                        .font(.system(size: 14, weight: .semibold))
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
    let title: LocalizedStringKey
    let value: String
    let color: Color

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(verbatim: value)
                .font(.system(size: 18, weight: .semibold).monospacedDigit())
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
        participant.agent.lastResponse
            ?? participant.agent.recap
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
                    Text(verbatim: "\(participant.host.alias) · \(desired?.role ?? participant.agent.kind)")
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
        .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous)
                .stroke(.separator.opacity(0.55), lineWidth: 0.5)
        }
    }

    private var executionLabel: String {
        var parts: [String] = []
        if let pane = participant.pane {
            parts.append("\(pane.session) › \(pane.windowName.isEmpty ? pane.stableWindowID : pane.windowName) › \(pane.paneID)")
        }
        if let model = participant.agent.model { parts.append(model) }
        return parts.isEmpty ? String(localized: "No execution binding") : parts.joined(separator: " · ")
    }
}

private struct PipelinePlaceholderCard: View {
    let desired: MuxaDesiredAgent
    let state: MuxaPipelineAliasState?

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text(verbatim: "@\(desired.alias)")
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
        .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
    }
}

private struct MarkdownSection: View {
    let title: LocalizedStringKey
    let source: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(title)
                .textCase(.uppercase)
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
    private enum DetailTab: CaseIterable, Identifiable {
        case summary
        case conversation
        case shell

        var id: Self { self }

        var title: LocalizedStringKey {
            switch self {
            case .summary: "Summary"
            case .conversation: "Conversation"
            case .shell: "Shell"
            }
        }
    }

    let participant: MuxaHostedAgent
    @ObservedObject var model: AppModel
    @Environment(\.colorScheme) private var colorScheme
    @State private var selectedTab: DetailTab = .summary

    private var client: MuxaIPCClient { model.client }

    private var summary: String? {
        participant.agent.lastResponse
            ?? participant.agent.recap
            ?? participant.agent.lastNotification
            ?? participant.agent.aiTitle
            ?? participant.agent.lastPrompt
    }

    /// Agents the Inbox lists under "Needs attention" get the request card
    /// on top; a working or idle agent opened from elsewhere keeps the
    /// plain summary.
    private var needsAttention: Bool {
        ["waiting_input", "waiting_choice", "blocked", "error", "failed"]
            .contains(participant.agent.state)
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
                            .font(.system(size: 22, weight: .semibold))
                        Text(agentStateLabel(participant.agent.state))
                            .foregroundStyle(agentStateColor(participant.agent.state))
                        MuxaRateLimitBadge(agent: participant.agent) // WS-C usage
                        Text(participant.agent.agentSessionID)
                            .font(.caption.monospaced())
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                    }
                    Spacer(minLength: 12)
                    VStack(alignment: .trailing, spacing: 6) {
                        if !needsAttention {
                            Button {
                                model.openInLiveWatch(participant)
                            } label: {
                                Label("Open in Live Watch", systemImage: "rectangle.on.rectangle")
                            }
                            .buttonStyle(.muxaSecondary)
                            .disabled(participant.pane == nil)
                            .help("Follow this agent's pane in Live Watch")
                        }
                        // Whatever the enabled modules offer for this agent;
                        // nothing at all when none is switched on.
                        MuxaModuleMenu(
                            context: .agent(participant),
                            model: model,
                            registry: MuxaModuleRegistry.shared
                        )
                    }
                    .padding(.top, 6)
                }

                if needsAttention {
                    InboxAgentRequestCard(
                        participant: participant,
                        requests: participant.openRequests(in: model.operatorMessages),
                        work: model.workGroup(for: participant),
                        openInLiveWatch: { model.openInLiveWatch(participant) }
                    )
                }

                MuxaSegmented(selection: $selectedTab, options: availableTabs) { Text($0.title) }
                    .accessibilityLabel("Agent detail")

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
                            .font(.system(size: 22, weight: .semibold))
                        Group {
                            if host.local {
                                Text("Local host")
                            } else {
                                Text(fleetHostStateLabel(host.state))
                            }
                        }
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
                        .font(.system(size: 14, weight: .semibold))
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
                        AgentFact(label: "Mode", value: fleetHostModeLabel(host.mode))
                        AgentFact(label: "State", value: fleetHostStateLabel(host.state))
                        AgentFact(label: "Scope", value: host.local ? String(localized: "local") : String(localized: "remote"))
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
                .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
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
                        Label {
                            Text(verbatim: "\(attentionCount)")
                        } icon: {
                            Image(systemName: "exclamationmark.circle.fill")
                        }
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
            .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous)
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
                        .background(Color.accentColor.opacity(0.1), in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
                    VStack(alignment: .leading, spacing: 3) {
                        Text(session.name.isEmpty ? session.sessionID : session.name)
                            .font(.system(size: 22, weight: .semibold))
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
                        .font(.system(size: 14, weight: .semibold))
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
                        .font(.system(size: 14, weight: .semibold))
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
                            .font(.system(size: 14, weight: .semibold))
                        Text("\(relatedMessages.count) operator commands and their durable replies")
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
                .background(Color.accentColor.opacity(0.1), in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
            VStack(alignment: .leading, spacing: 4) {
                Text(window.name.isEmpty ? window.windowID : window.name)
                    .font(.system(size: 22, weight: .semibold))
                HStack(spacing: 7) {
                    Text("\(window.hostAlias) · \(window.panes.count) panes")
                        .foregroundStyle(.secondary)
                    if let workIdentity {
                        Text(verbatim: "\(workIdentity.workspaceID) / \(workIdentity.workID)")
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
        .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
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
        presentText(pane.agent?.lastResponse)
            ?? presentText(pane.agent?.recap)
            ?? presentText(pane.agent?.lastNotification)
            ?? presentText(pane.agent?.lastPrompt)
    }

    private var separateResponse: String? {
        guard let response = presentText(pane.agent?.lastResponse), response != summary else {
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
                Text(verbatim: "·")
                Text(pane.pane.currentPath)
                    .lineLimit(1)
            }
            .font(.caption2.monospaced())
            .foregroundStyle(.tertiary)
        }
        .padding(14)
        .frame(maxWidth: .infinity, minHeight: 300, alignment: .topLeading)
        .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous)
                .stroke(
                    paneNeedsAttentionForSummary(pane) ? Color.orange.opacity(0.5) : Color(nsColor: .separatorColor).opacity(0.45),
                    lineWidth: paneNeedsAttentionForSummary(pane) ? 1 : 0.5
                )
        }
    }

    private func reportSection(_ label: LocalizedStringKey, source: String, lineLimit: Int) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(label)
                .textCase(.uppercase)
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
            MuxaContextMeter(percent: context) // WS-C usage
        }
        if let cost = agent.costUSD {
            detailChip("Cost", cost.formatted(.currency(code: "USD")), systemImage: "dollarsign.circle")
        }
    }

    private func detailChip(_ label: LocalizedStringKey, _ value: String, systemImage: String) -> some View {
        Label {
            HStack(spacing: 3) {
                Text(label)
                Text(value)
            }
        } icon: {
            Image(systemName: systemImage)
        }
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
                    Group {
                        if paneNeedsAttentionForSummary(focusPane) {
                            Text("Needs attention")
                        } else {
                            Text("Current picture")
                        }
                    }
                    .textCase(.uppercase)
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
        .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous)
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
                    source: fleetPaneSummary(pane) ?? String(localized: "No summary reported"),
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
    presentText(pane.agent?.lastResponse)
        ?? presentText(pane.agent?.recap)
        ?? presentText(pane.agent?.lastNotification)
        ?? presentText(pane.agent?.lastPrompt)
        ?? presentText(pane.pane.currentPath)
}

private func presentText(_ value: String?) -> String? {
    guard let value, !value.isEmpty else { return nil }
    return value
}

private struct HostMetric: View {
    let title: LocalizedStringKey
    let value: UInt64
    var suffix = ""

    init(title: LocalizedStringKey, value: Int, suffix: String = "") {
        self.title = title
        self.value = UInt64(value)
        self.suffix = suffix
    }

    init(title: LocalizedStringKey, value: UInt64, suffix: String = "") {
        self.title = title
        self.value = value
        self.suffix = suffix
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(verbatim: "\(value)\(suffix)")
                .font(.system(size: 18, weight: .semibold).monospacedDigit())
            Text(title)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .frame(minWidth: 90, alignment: .leading)
    }
}

private struct AgentFact: View {
    let label: LocalizedStringKey
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
                    .buttonStyle(.muxaPrimary)
                    .disabled(!model.isConnected || model.isStartingWork)
                Button("Open Live Watch") { model.select(.watch) }
                    .buttonStyle(.muxaSecondary)
                Button("New Shell") { model.createShell() }
                    .buttonStyle(.muxaSecondary)
                    .disabled(!model.isConnected || model.isCreatingSession)
            }
        }
        .padding(32)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

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

/// Display wording for a fleet host `state` as muxad reports it.
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
        client: MuxaIPCClient,
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
            MuxaSurfacePalette.terminal(for: colorScheme)
                .ignoresSafeArea()

            TerminalSurfaceView(context: pane.terminalState)
                .background(MuxaSurfacePalette.terminal(for: colorScheme))
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .clipped()

            if showsToolbar {
                VStack {
                    HStack(spacing: 8) {
                        Spacer()
                        Button {
                            openWindow(value: MuxaModuleRoute.shell(sessionID))
                        } label: {
                            Image(systemName: "macwindow.on.rectangle")
                        }
                        .buttonStyle(.muxaIcon)
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
                Label {
                    if let status = pane.exitStatus {
                        Text("Session ended (status \(status))")
                    } else {
                        Text("Session ended")
                    }
                } icon: {
                    Image(systemName: pane.exitStatus == 0 ? "checkmark.circle" : "stop.circle")
                }
                .font(.caption)
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

enum MuxaSurfacePalette {
    static func editor(for colorScheme: ColorScheme) -> Color {
        workspace(for: colorScheme)
    }

    static func sidebar(for colorScheme: ColorScheme) -> Color {
        MuxaTheme.sideBar(colorScheme)
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
        MuxaTheme.editor(colorScheme)
    }
}
