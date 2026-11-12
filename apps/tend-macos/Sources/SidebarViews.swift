// The side bar: activity rail, each mode's list, and their rows.
import GhosttyTerminal
import SwiftUI
import UniformTypeIdentifiers

struct TendSidebar: View {
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
        case limited
        case active
        case idle
        case shell

        var id: Self { self }

        var title: String {
            switch self {
            case .attention: String(localized: "Needs attention")
            case .limited: String(localized: "Rate limited")
            case .active: String(localized: "Active")
            case .idle: String(localized: "Idle agents")
            case .shell: String(localized: "Shell panes")
            }
        }
    }

    private struct ExplorePaneGroup: Identifiable {
        let bucket: ExploreStatusBucket
        let panes: [TendWatchPane]

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
    let openPinnedSession: (TendWatchSessionIdentity) -> Void
    let openPinnedPane: (TendWatchPaneIdentity) -> Void
    let openFile: (TendFileLocation, Bool) -> Void
    let closeShell: (String) -> Void
    let focusRequest: UUID
    @FocusState private var filterFocused: Bool
    @Environment(\.colorScheme) private var colorScheme
    @State private var filterText = ""
    @State private var statusScope: StatusScope = .all
    /// Exited shells the user closed from the Shells list. tendd keeps an
    /// exited session listed for a while, so the sidebar hides these until
    /// the daemon drops them.
    @State private var dismissedShellIDs: Set<String> = []
    @State private var remoteShellError: String?
    @State private var isOpeningRemoteShell = false
    @AppStorage("tend.explore.sort") private var exploreSort: ExploreSort = .topology
    @AppStorage("tend.explore.grouping") private var exploreGrouping: ExploreGrouping = .host

    private var background: Color {
        TendSurfacePalette.sidebar(for: colorScheme)
    }

    var body: some View {
        ZStack {
            background

            VStack(spacing: 0) {
                // The title bar row: the traffic lights, then the view's
                // title and actions (see WorkbenchWindowChrome.swift).
                HStack(spacing: 0) {
                    if model.isSidebarListCollapsed {
                        // Only the rail is showing; the traffic lights sit
                        // over it and the tab strip clears the rest.
                        Spacer(minLength: 0)
                    } else {
                        Color.clear.frame(width: max(0, chrome.leadingInset - 14))
                        sectionTitle
                    }
                }
                .frame(height: chrome.rowHeight)
                .background(alignment: .bottom) {
                    ZStack(alignment: .bottom) {
                        WindowDragArea()
                        TendTheme.border(colorScheme).frame(height: 1)
                            .allowsHitTesting(false)
                    }
                }

                HStack(spacing: 0) {
                    SidebarActivityRail(model: model, attention: model.attention)

                    TendTheme.border(colorScheme).frame(width: 1)

                    if model.isSidebarListCollapsed {
                        EmptyView()
                    } else if model.sidebarMode == .files {
                        ArtifactFileSidebar(model: model, openFile: openFile)
                    } else {
                    VStack(spacing: 0) {
                        TendFilterField(
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
                        .environment(\.defaultMinListRowHeight, TendTheme.rowHeight)
                        .buttonStyle(.tendSecondary)
                        .listStyle(.sidebar)
                        .scrollContentBackground(.hidden)
                        .background(Color.clear)
                    }
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
        TendSectionTitle(title: model.sidebarMode.title) {
            if model.sidebarMode != .files {
            Text(verbatim: sidebarCountLabel)
                .font(.system(size: 11, weight: .medium).monospacedDigit())
                .foregroundStyle(.secondary)
                .padding(.horizontal, 5)
                .frame(minHeight: 16)
                .background(Color.primary.opacity(0.07), in: Capsule())
                .padding(.trailing, 4)
            }
            if model.sidebarMode == .watch {
                Button {
                    model.select(.watch)
                } label: {
                    Image(systemName: "rectangle.on.rectangle")
                }
                .buttonStyle(.tendIcon)
                .help("Open Live Watch")
                Button {
                    model.presentHostRegistration()
                } label: {
                    Image(systemName: "plus")
                }
                .buttonStyle(.tendIcon)
                .help("Register SSH Host")
            }
            if model.sidebarMode == .ask {
                Button {
                    Task { await model.resetAskConversation() }
                    model.select(.ask)
                } label: {
                    Image(systemName: "plus.bubble")
                }
                .buttonStyle(.tendIcon)
                .disabled(!model.isConnected)
                .help("New Conversation")
            }
            if model.sidebarMode == .shells {
                Button {
                    model.createShell()
                } label: {
                    Image(systemName: "plus")
                }
                .buttonStyle(.tendIcon)
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
                TendModuleMenu(
                    context: .app,
                    model: model,
                    registry: TendModuleRegistry.shared
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

    private func openInLiveWatch(_ participant: TendHostedAgent) {
        model.openInLiveWatch(participant)
    }

    private func openRemoteShell(on host: TendFleetHost) {
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

    private func dismissShell(_ session: TendSession) {
        dismissedShellIDs.insert(session.id)
        closeShell(session.id)
    }

    private var sidebarCount: Int {
        switch model.sidebarMode {
        case .work: model.workGroups.count
        case .watch: model.executionSnapshot.watchHosts.reduce(0) { $0 + $1.paneCount }
        case .inbox: inboxBadgeCount
        case .ask: askConversations.count
        case .files: 0
        case .shells: model.sessions.lazy.filter { !$0.exited }.count
        }
    }

    private var filteredSidebarCount: Int {
        switch model.sidebarMode {
        case .work: filteredWorkGroups.count
        case .watch: filteredWatchHosts.reduce(0) { $0 + $1.paneCount }
        case .inbox: inboxBadgeCount
        case .ask: filteredAskConversations.count
        case .files: 0
        case .shells: filteredSessions.count
        }
    }

    /// The selected provider's conversations, newest activity first — the
    /// same list the Ask editor's menu offers, as sidebar rows.
    private var askConversations: [TendAskConversation] {
        model.askConversations
            .filter { $0.agent == model.askAgent }
            .sorted { $0.updatedAt > $1.updatedAt }
    }

    /// Status scopes read the conversation's newest turn: Active while it is
    /// still being answered, Attention when it failed.
    private var filteredAskConversations: [TendAskConversation] {
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

    private var filteredWorkGroups: [TendWorkGroup] {
        model.workGroups.filter {
            matchesFilter([$0.title, $0.workspaceID, $0.pipelineLabel])
                && matchesScope(attention: $0.attentionCount > 0, active: $0.workingCount > 0)
        }
    }

    private var filteredSessions: [TendSession] {
        model.sessions.filter {
            guard !$0.exited else { return false }
            return matchesFilter([$0.displayName, $0.id])
                && matchesScope(
                    attention: false,
                    active: true
                )
        }
    }

    /// Exited shells still listed by tendd and not yet closed from the
    /// sidebar. They are neither "active" nor "attention", so a status
    /// scope hides them.
    private var visibleExitedSessions: [TendSession] {
        guard statusScope == .all else { return [] }
        return model.sessions.filter {
            $0.exited
                && !dismissedShellIDs.contains($0.id)
                && matchesFilter([$0.displayName, $0.id])
        }
    }

    private var attentionAgents: [TendHostedAgent] {
        model.hostedAgents.filter { $0.agent.status.needsAttention }
    }

    private var filteredAttentionAgents: [TendHostedAgent] {
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
        model.attention.summary.needsYou
    }

    private var filteredWatchHosts: [TendWatchHost] {
        let filtered: [TendWatchHost] = model.executionSnapshot.watchHosts.compactMap { hostGroup in
            let sessions = hostGroup.sessions.compactMap { session -> TendWatchSession? in
                let windows = session.windows.compactMap { window -> TendWatchWindow? in
                    let panes = window.panes.filter { pane in
                        matchesFilter([
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
                            attention: pane.needsAttention,
                            active: pane.agentStatus == .working
                        )
                    }
                    guard !panes.isEmpty else { return nil }
                    return TendWatchWindow(
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
                return TendWatchSession(
                    hostAlias: session.hostAlias,
                    socket: session.socket,
                    sessionID: session.sessionID,
                    name: session.name,
                    windows: windows
                )
            }
            guard !sessions.isEmpty else { return nil }
            return TendWatchHost(host: hostGroup.host, sessions: sessions)
        }
        return sortedWatchHosts(filtered)
    }

    private var filteredWatchPanes: [TendWatchPane] {
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

    private func statusBucket(for pane: TendWatchPane) -> ExploreStatusBucket {
        switch pane.agentStatus {
        case nil: .shell
        case .error?, .needsInput?: .attention
        case .limited?: .limited
        case .working?: .active
        default: .idle
        }
    }

    private func sortedWatchHosts(_ hosts: [TendWatchHost]) -> [TendWatchHost] {
        let rebuilt = hosts.map { host in
            let sessions = host.sessions.map { session in
                let windows = session.windows.map { window in
                    TendWatchWindow(
                        hostAlias: window.hostAlias,
                        socket: window.socket,
                        sessionID: window.sessionID,
                        windowID: window.windowID,
                        name: window.name,
                        index: window.index,
                        panes: sortedPanes(window.panes)
                    )
                }.sorted { ordered($0, before: $1) }
                return TendWatchSession(
                    hostAlias: session.hostAlias,
                    socket: session.socket,
                    sessionID: session.sessionID,
                    name: session.name,
                    windows: windows
                )
            }.sorted { ordered($0, before: $1) }
            return TendWatchHost(host: host.host, sessions: sessions)
        }
        return rebuilt.sorted { ordered($0, before: $1) }
    }

    private func sortedPanes(_ panes: [TendWatchPane]) -> [TendWatchPane] {
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

    private func ordered(_ left: TendWatchWindow, before right: TendWatchWindow) -> Bool {
        let leftDate = latestDate(in: left.panes)
        let rightDate = latestDate(in: right.panes)
        if exploreSort != .topology, leftDate != rightDate { return leftDate > rightDate }
        let leftIndex = Int(left.index) ?? Int.max
        let rightIndex = Int(right.index) ?? Int.max
        if leftIndex != rightIndex { return leftIndex < rightIndex }
        return left.name.localizedStandardCompare(right.name) == .orderedAscending
    }

    private func ordered(_ left: TendWatchSession, before right: TendWatchSession) -> Bool {
        let leftDate = latestDate(in: left.windows.flatMap(\.panes))
        let rightDate = latestDate(in: right.windows.flatMap(\.panes))
        if exploreSort != .topology, leftDate != rightDate { return leftDate > rightDate }
        return left.name.localizedStandardCompare(right.name) == .orderedAscending
    }

    private func ordered(_ left: TendWatchHost, before right: TendWatchHost) -> Bool {
        let leftDate = latestDate(in: left.sessions.flatMap(\.windows).flatMap(\.panes))
        let rightDate = latestDate(in: right.sessions.flatMap(\.windows).flatMap(\.panes))
        if exploreSort != .topology, leftDate != rightDate { return leftDate > rightDate }
        if left.host.local != right.host.local { return left.host.local }
        return left.host.alias.localizedStandardCompare(right.host.alias) == .orderedAscending
    }

    private func latestDate(in panes: [TendWatchPane]) -> Date {
        panes.map(activityDate(for:)).max() ?? .distantPast
    }

    private func activityDate(for pane: TendWatchPane) -> Date {
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
        case .files: EmptyView()
        case .work:
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
                    InboxEmptyState(model: model)
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
                                .buttonStyle(.tendIcon)
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
                        .font(TendType.detail)
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
                            .buttonStyle(.tendIcon)
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
                ExploreEmptyState(model: model)
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
                if statusScope == .all, !pinnedAttentionPanes.isEmpty {
                    pinnedAttentionSection
                }
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

    /// Agents that need the reader, errors first. The topology tree
    /// scatters them across hosts and sessions, so the host view repeats
    /// them in one place above it. Rate-limited agents join them only when
    /// no limit policy will resume them.
    private var pinnedAttentionPanes: [TendWatchPane] {
        filteredWatchPanes
            .filter(\.needsAttention)
            .sorted { $0.statusPriority < $1.statusPriority }
    }

    private static let pinnedAttentionLimit = 5

    @ViewBuilder
    private var pinnedAttentionSection: some View {
        let panes = pinnedAttentionPanes
        Section {
            ForEach(panes.prefix(Self.pinnedAttentionLimit)) { pane in
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
            if panes.count > Self.pinnedAttentionLimit {
                Button {
                    exploreGrouping = .status
                } label: {
                    Text("Show all \(panes.count)")
                        .font(TendType.detail)
                        .foregroundStyle(Color.accentColor)
                }
                .buttonStyle(.plain)
                .padding(.leading, 12)
                .listRowBackground(Color.clear)
            }
        } header: {
            Text(verbatim: "\(String(localized: "Needs attention")) · \(panes.count)")
        }
    }

    private func watchWorkLabel(_ window: TendWatchWindow) -> String? {
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
    @ObservedObject var attention: TendAgentAttentionCenter // WS-A

    var body: some View {
        VStack(spacing: 0) {
            // Fleet Home sits above the lists: one click from anywhere, and
            // its badge counts what is waiting for the person.
            ActivityRailItem(
                systemImage: "house",
                title: String(localized: "Home"),
                active: model.isSidebarListCollapsed,
                badge: model.homeWorks.lazy
                    .filter { $0.lane == .needsYou }.count,
                select: { model.showHome() }
            )
            TendTheme.border(colorScheme)
                .frame(width: TendTheme.activityBarWidth - 18, height: 1)
                .padding(.vertical, 4)
            ForEach(TendSidebarMode.railModes) { mode in
                ActivityRailItem(
                    systemImage: mode.systemImage,
                    title: mode.title,
                    active: model.sidebarMode == mode && !model.isSidebarListCollapsed,
                    badge: attentionCount(for: mode),
                    select: { model.show(mode) }
                )
            }
            Spacer(minLength: 0)
        }
        .padding(.top, 4)
        .frame(width: TendTheme.activityBarWidth)
        .frame(maxHeight: .infinity)
        .background(TendTheme.activityBar(colorScheme))
    }

    @Environment(\.colorScheme) private var colorScheme

    private func attentionCount(for mode: TendSidebarMode) -> Int {
        switch mode {
        case .work:
            return model.workGroups.lazy.filter { $0.attentionCount > 0 }.count
        case .watch:
            return attention.summary.unreadPanes // WS-A
        case .inbox:
            return attention.summary.needsYou
        case .ask:
            // A question still being answered is the one thing worth a dot.
            return model.askEntries.lazy.filter { $0.status == "running" }.count
        case .shells, .files:
            return 0
        }
    }
}

/// One activity bar icon: dimmed at rest, full strength with a 2pt accent bar
/// on the leading edge when its view is showing, a count badge in the corner.
private struct ActivityRailItem: View {
    let systemImage: String
    let title: String
    let active: Bool
    let badge: Int
    let select: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: select) {
            Image(systemName: systemImage)
                .font(.system(size: 17, weight: .regular))
                .frame(width: TendTheme.activityBarWidth, height: 44)
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
        .help(title)
        .accessibilityLabel(title)
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
                .font(TendType.detail)
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
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 4)
            Image(systemName: "chevron.right")
                .font(TendType.meta.weight(.semibold))
                .foregroundStyle(.tertiary)
        }
        .padding(.horizontal, 4)
        .frame(maxWidth: .infinity, minHeight: 34, alignment: .leading)
        .contentShape(Rectangle())
    }
}

/// One Global Ask conversation in the Ask container's sidebar.
private struct AskConversationRow: View {
    let conversation: TendAskConversation
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
                        .font(TendType.meta)
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
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 4)
            Image(systemName: "chevron.right")
                .font(TendType.meta.weight(.semibold))
                .foregroundStyle(.tertiary)
        }
        .padding(.horizontal, 4)
        .frame(maxWidth: .infinity, minHeight: 38, alignment: .leading)
        .contentShape(Rectangle())
    }
}

private struct InboxAgentRow: View {
    let participant: TendHostedAgent

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
                .fill(participant.agent.status.color)
                .frame(width: 8, height: 8)
                .padding(.top, 5)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    TendAgentLogo(kind: participant.agent.kind, size: 13)
                    Text(title)
                        .fontWeight(.medium)
                        .lineLimit(1)
                    Spacer(minLength: 4)
                    Text(participant.host.alias)
                        .font(TendType.meta.monospaced())
                        .foregroundStyle(.secondary)
                }
                Text(summary)
                    .font(TendType.meta)
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
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if attentionCount > 0 {
                Text(verbatim: "\(attentionCount)")
                    .font(TendType.meta.bold().monospacedDigit())
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
    let session: TendSession

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
                .font(TendType.meta)
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
    let work: TendWorkGroup

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
                    .font(TendType.meta)
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
            .font(TendType.meta.monospacedDigit())
            .foregroundStyle(work.attentionCount > 0 ? .orange : .secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
        .padding(.vertical, 2)
    }
}

private struct FleetAgentRow: View {
    let participant: TendHostedAgent

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
                        .fill(participant.agent.status.color)
                        .frame(width: 7, height: 7)
                    TendAgentLogo(kind: participant.agent.kind, size: 13)
                    Text(title)
                        .lineLimit(1)
                }
                Text(verbatim: "\(participant.host.alias) · \(participant.agent.agentSessionID)")
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                Text(executionLocation)
                    .font(TendType.meta.monospaced())
                    .foregroundStyle(.secondary)
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
