// Detail editors for a Work, agent, host, session and window.
import GhosttyTerminal
import SwiftUI
import UniformTypeIdentifiers

struct WorkDetailView: View {
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
                        .font(MuxaType.detail.weight(.semibold))
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
                            .font(MuxaType.detail.monospaced())
                            .foregroundStyle(.secondary)
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
                .font(MuxaType.detail)
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
                        .font(MuxaType.detail)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer()
                Label(participant.agent.status.label(), systemImage: "circle.fill")
                    .labelStyle(.titleAndIcon)
                    .font(MuxaType.detail.weight(.medium))
                    .foregroundStyle(participant.agent.status.color)
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
                        .foregroundStyle(.secondary)
                }
            }
            .frame(maxHeight: 68, alignment: .topLeading)
            .clipped()

            Text(executionLabel)
            .font(MuxaType.meta.monospaced())
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .truncationMode(.middle)
            .help(executionLabel)

            Spacer(minLength: 0)

            Button(action: openAgent) {
                Label("Open agent details", systemImage: "arrow.right.circle")
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.plain)
            .font(MuxaType.detail.weight(.medium))
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
                        .font(MuxaType.detail)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer()
                Label(agentStateLabel(state?.status ?? "pending"), systemImage: "circle.fill")
                    .font(MuxaType.detail.weight(.medium))
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
                        .foregroundStyle(.secondary)
                }
            }
            .frame(maxHeight: 68, alignment: .topLeading)
            .clipped()
            Spacer(minLength: 0)
            if let error = state?.error, !error.isEmpty {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(MuxaType.detail)
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
                .font(MuxaType.meta.weight(.semibold))
                .foregroundStyle(.secondary)
            if let source, !source.isEmpty {
                MarkdownContent(source: source)
            } else {
                Text("Not available")
                    .foregroundStyle(.secondary)
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
        font: Font = MuxaType.detail
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

struct FleetAgentDetailView: View {
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
        participant.agent.status.needsAttention
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
                        .fill(participant.agent.status.color)
                        .frame(width: 12, height: 12)
                        .padding(.top, 9)
                    VStack(alignment: .leading, spacing: 4) {
                        Text(title)
                            .font(.system(size: 22, weight: .semibold))
                        // A cap is said once, by the badge, which also
                        // explains it; any other status by its label.
                        if participant.agent.status.isLimited {
                            MuxaRateLimitBadge(agent: participant.agent, target: MuxaAgentActionTarget(participant)) // WS-C usage
                        } else {
                            Text(participant.agent.status.label())
                                .foregroundStyle(participant.agent.status.color)
                        }
                        Text(participant.agent.agentSessionID)
                            .font(MuxaType.detail.monospaced())
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                    }
                    Spacer(minLength: 12)
                    VStack(alignment: .trailing, spacing: 6) {
                        MuxaAgentActionBar(model: model, target: MuxaAgentActionTarget(participant))
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
                        .foregroundStyle(.secondary)
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

struct FleetHostDetailView: View {
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
                        .font(MuxaType.detail.weight(.medium))
                        .foregroundStyle(.orange)
                    }
                    Image(systemName: "chevron.right")
                        .font(MuxaType.meta.weight(.semibold))
                        .foregroundStyle(.tertiary)
                }

                Text("\(session.windows.count) windows · \(panes.count) panes · \(agents.count) agents")
                    .font(MuxaType.detail.monospacedDigit())
                    .foregroundStyle(.secondary)

                VStack(alignment: .leading, spacing: 7) {
                    ForEach(Array(agents.prefix(2))) { pane in
                        FleetResourceSummaryRow(pane: pane)
                    }
                    if agents.isEmpty {
                        Text("No retained agent summary")
                            .font(MuxaType.detail)
                            .foregroundStyle(.secondary)
                    } else if agents.count > 2 {
                        Text("+\(agents.count - 2) more agents")
                            .font(MuxaType.meta)
                            .foregroundStyle(.secondary)
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

struct FleetSessionDetailView: View {
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

struct FleetWindowDetailView: View {
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
                            .font(MuxaType.detail.weight(.semibold))
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
            $0.agentStatus == .working
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
        if pane.agentStatus == .working { return 1 }
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
                        .fill(pane.agent.map { $0.status.color } ?? Color.secondary)
                        .frame(width: 8, height: 8)
                    Text(fleetPaneDisplayTitle(pane))
                        .font(.headline)
                        .lineLimit(1)
                    if let agent = pane.agent {
                        Text(agent.status.label())
                            .font(MuxaType.meta.weight(.semibold))
                            .foregroundStyle(agent.status.color)
                    }
                    Spacer(minLength: 4)
                    Text(pane.pane.paneID)
                        .font(MuxaType.meta.monospaced())
                        .foregroundStyle(.secondary)
                    Image(systemName: "chevron.right")
                        .font(MuxaType.meta.weight(.semibold))
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
                    .font(MuxaType.detail)
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
            .font(MuxaType.meta.monospaced())
            .foregroundStyle(.secondary)
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
                .font(MuxaType.meta.weight(.bold))
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
        .font(MuxaType.meta)
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
                        MarkdownContent(source: reply.body, lineLimit: 3, selectable: false, font: MuxaType.detail)
                            .foregroundStyle(.secondary)
                    } else {
                        Text("Waiting for \(message.request.to.label) to reply")
                            .font(MuxaType.detail)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 6)
                Image(systemName: "chevron.right")
                    .font(MuxaType.meta.weight(.semibold))
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
            let leftPriority = left.statusPriority
            let rightPriority = right.statusPriority
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
                                .font(MuxaType.meta.weight(.medium))
                                .foregroundStyle(Color.accentColor)
                                .lineLimit(1)
                        }
                    }
                    Spacer(minLength: 4)
                    Text("#\(window.index) · \(window.panes.count) panes")
                        .font(MuxaType.meta.monospacedDigit())
                        .foregroundStyle(.secondary)
                    Image(systemName: "chevron.right")
                        .font(MuxaType.meta.weight(.semibold))
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
                    .font(MuxaType.meta.weight(.bold))
                    .foregroundStyle(paneNeedsAttentionForSummary(focusPane) ? Color.orange : Color.secondary)
                    MarkdownContent(source: summary, lineLimit: 3, selectable: false, font: MuxaType.detail)
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
                    .font(MuxaType.meta)
                    .foregroundStyle(.secondary)
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
                .fill(pane.agent.map { $0.status.color } ?? Color.secondary)
                .frame(width: 7, height: 7)
                .padding(.top, 5)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(fleetPaneDisplayTitle(pane))
                        .font(.subheadline.weight(.medium))
                        .lineLimit(1)
                    if let agent = pane.agent {
                        Text(agent.status.label())
                            .font(MuxaType.meta.weight(.medium))
                            .foregroundStyle(agent.status.color)
                    }
                }
                MarkdownContent(
                    source: fleetPaneSummary(pane) ?? String(localized: "No summary reported"),
                    lineLimit: 2,
                    selectable: false,
                    font: MuxaType.detail
                )
                .foregroundStyle(.secondary)
            }
            Spacer(minLength: 4)
            if showsChevron {
                Image(systemName: "chevron.right")
                    .font(MuxaType.meta.weight(.semibold))
                    .foregroundStyle(.tertiary)
                    .padding(.top, 5)
            }
        }
    }
}

private func paneNeedsAttentionForSummary(_ pane: MuxaWatchPane) -> Bool {
    pane.needsAttention
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

func presentText(_ value: String?) -> String? {
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
                .font(MuxaType.detail)
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

struct MuxaEmptyDetail: View {
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
