// Command Center: the fleet at a glance and what needs the person now.
import AppKit
import SwiftUI

struct WorkCommandCenterView: View {
    @ObservedObject var model: AppModel
    /// Fleet Home's Templates page: only the pipeline library and routes.
    var pipelinesOnly = false
    @Environment(\.colorScheme) private var colorScheme

    private let columns = [GridItem(.adaptive(minimum: 260, maximum: 420), spacing: 12)]

    /// Every agent that needs the reader, on every host, with or without a
    /// pane or a Work, errors first: the agents the Dock badge counts as
    /// needing attention (it also counts unread ones).
    private var attentionAgents: [TendHostedAgent] {
        model.hostedAgents
            // Capped agents have their own row (with Switch All).
            .filter { $0.agent.status.needsAttention && !$0.agent.status.isLimited }
            .sorted { $0.agent.status.priority < $1.agent.status.priority }
    }

    private var limitedAgents: [TendHostedAgent] {
        model.hostedAgents.filter { $0.agent.status.isLimited }
    }

    /// An agent's pane editor when it has a live pane, its agent editor
    /// otherwise.
    private func open(_ hosted: TendHostedAgent) {
        if let pane = TendAgentAttentionCenter.paneIdentity(for: hosted),
           model.executionSnapshot.watchPane(id: pane) != nil {
            model.selectWatchPane(pane)
        } else {
            model.select(.agent(hosted.id))
        }
    }

    private var workingCount: Int {
        model.hostedAgents.lazy.filter { $0.agent.status == .working }.count
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                if !pipelinesOnly {
                ViewThatFits(in: .horizontal) {
                    HStack(alignment: .top, spacing: 18) {
                        commandCenterTitle
                            .frame(minWidth: 360, alignment: .leading)
                        Spacer(minLength: 0)
                        commandCenterActions
                    }
                    VStack(alignment: .leading, spacing: 14) {
                        commandCenterTitle
                        commandCenterActions
                    }
                }

                CommandCenterStatusLine(
                    attention: model.attention,
                    working: workingCount,
                    work: model.workGroups.count,
                    hosts: model.fleetHosts.count,
                    openInbox: { model.select(.inbox) },
                    openWork: { model.showHome() },
                    openHosts: { model.show(.watch) }
                )

                let humanRequests = model.operatorMessages.filter { $0.isHumanRequest && $0.needsHumanDecision }
                if !humanRequests.isEmpty {
                    VStack(alignment: .leading, spacing: 8) {
                        Label("Your decision needed", systemImage: "person.crop.circle.badge.exclamationmark")
                            .font(.headline)
                        ForEach(humanRequests) { message in
                            Button {
                                model.pendingOperatorMessageID = message.id
                                model.select(.inbox)
                            } label: {
                                HStack {
                                    Text(verbatim: "\(message.host.alias) · \(message.counterpart.label)").fontWeight(.medium)
                                    Text(message.request.body).lineLimit(1)
                                    Spacer()
                                    Image(systemName: "arrow.right")
                                }
                            }.buttonStyle(.tendSecondary)
                        }
                    }
                }

                CommandCenterAttentionList(
                    attention: attentionAgents,
                    limited: limitedAgents,
                    open: open
                )

                VStack(alignment: .leading, spacing: 10) {
                    Text("Active Work")
                        .font(.system(size: 14, weight: .semibold))
                    if model.workGroups.isEmpty {
                        HStack(spacing: 10) {
                            Image(systemName: "square.stack.3d.up.slash")
                                .foregroundStyle(.secondary)
                            VStack(alignment: .leading, spacing: 2) {
                                Text("No managed Work yet")
                                    .font(TendType.body.weight(.medium))
                                Text("Start a configured pipeline here. Tend will keep Work identity separate from its tmux window and agents.")
                                    .font(TendType.detail)
                                    .foregroundStyle(.secondary)
                            }
                            Spacer(minLength: 8)
                            Button("Start your first Work") { model.presentWorkStart() }
                                .buttonStyle(.tendSecondary)
                        }
                        .padding(14)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous))
                    } else {
                        LazyVGrid(columns: columns, alignment: .leading, spacing: 12) {
                            ForEach(model.workGroups) { work in
                                WorkCommandCard(
                                    work: work,
                                    open: { model.select(.work(work.identity)) },
                                    moduleModel: model
                                )
                            }
                        }
                    }
                }

                VStack(alignment: .leading, spacing: 10) {
                    Text("Hosts")
                        .font(.system(size: 14, weight: .semibold))
                    ScrollView(.horizontal, showsIndicators: false) {
                        HStack(spacing: 10) {
                            ForEach(model.fleetHosts) { host in
                                Button {
                                    model.select(.host(host.id))
                                } label: {
                                    HStack(spacing: 9) {
                                        HostIdentityBadge(host: host, size: 30)
                                        VStack(alignment: .leading, spacing: 1) {
                                            Text(host.alias).fontWeight(.medium)
                                            Text("\(host.remote?.agents.filter { $0.state != "stopped" }.count ?? 0) agents · \(fleetHostStateLabel(host.state))")
                                                .font(TendType.meta)
                                                .foregroundStyle(.secondary)
                                        }
                                    }
                                    .padding(.horizontal, 11)
                                    .padding(.vertical, 8)
                                    .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous))
                                }
                                .buttonStyle(.plain)
                            }
                        }
                    }
                }

                }

                pipelinesSection
            }
            .padding(28)
            .frame(maxWidth: 1250, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .top)
        }
        .background(TendSurfacePalette.workspace(for: colorScheme).ignoresSafeArea())
        .task { await model.loadAllWorkOptions() }
        .onChange(of: model.workCapableHosts.map(\.alias)) { _ in
            // The host list arrives with the first fleet snapshot, usually
            // after this view appeared; read the newly known hosts then.
            Task { await model.loadAllWorkOptions() }
        }
        .sheet(item: $pipelineComposer.target) { target in
            PipelineComposerView(
                target: target,
                model: model,
                shellFallback: pipelineComposer.shellFallback
            )
        }
    }

    private let pipelineColumns = [
        GridItem(.adaptive(minimum: 300, maximum: 460), spacing: 12, alignment: .top),
    ]

    /// Host whose routes the Routes editor shows; "" is the local host.
    /// Pipelines are one library kept in sync across hosts; routes carry
    /// host-specific folders, so they stay per host.
    @State private var routesHost = ""
    /// The preset gallery is a one-time setup step; it waits behind a button
    /// so it does not take over the Command Center every day.
    @State private var showsPresetGallery = false
    @State private var syncingPipelines = Set<String>()
    @State private var syncFailures: [String: String] = [:]
    @ObservedObject private var pipelineComposer = PipelineComposerPresenter.shared

    private var routesHostAlias: String? { routesHost.isEmpty ? nil : routesHost }
    private var pipelinesHostAlias: String? { nil }

    /// `tend work init` opens a local Shell tab, so only the local host gets it.
    private var describeWithAgentAction: (() -> Void)? {
        guard pipelinesHostAlias == nil else { return nil }
        return { Task { await model.configureWork(cwd: nil) } }
    }

    /// The pipeline library drawn as launchable presets. This is where a GUI
    /// earns its place over the CLI: the stage picture is visible before a
    /// single agent exists, every control host shows whether it carries the
    /// same definition, and an empty config offers tend's built-in presets
    /// instead of an error.
    @ViewBuilder
    private var pipelinesSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text("Pipelines")
                    .font(.system(size: 14, weight: .semibold))
                if let path = model.workOptions?.configPath {
                    Text(path)
                        .font(TendType.meta.monospaced())
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .help(path)
                }
                Spacer()
                if model.isLoadingWorkOptions {
                    ProgressView().controlSize(.small)
                }
                if libraryNeedsSync {
                    Button {
                        syncAll()
                    } label: {
                        Label("Sync All to Hosts", systemImage: "arrow.triangle.2.circlepath")
                    }
                    .buttonStyle(.tendGhost)
                    .disabled(!syncingPipelines.isEmpty)
                    .help("Write every library pipeline to the hosts where it is missing or differs")
                }
                Button {
                    model.presentPipelineComposer(host: pipelinesHostAlias)
                } label: {
                    Label("Describe…", systemImage: "sparkles")
                }
                .buttonStyle(.tendGhost)
                .disabled(model.workOptions == nil || !model.isConnected)
                .help("Describe a pipeline in plain language and let the Ask provider draft it")
                Button {
                    model.presentPipelineEditor(host: nil, pipeline: nil)
                } label: {
                    Label("New Pipeline…", systemImage: "plus")
                }
                .buttonStyle(.tendGhost)
                .disabled(model.workOptions == nil)
                Button {
                    Task { await model.loadAllWorkOptions() }
                } label: {
                    Label("Reload", systemImage: "arrow.clockwise")
                }
                .buttonStyle(.tendGhost)
                .help("Re-read pipelines and routes from every host")
            }

            if model.workCapableHosts.count > 1, !model.supportsHostWorkCommands {
                Label(
                    "Host sync needs the updated tendd on this Mac (Settings › Advanced › Reload tendd).",
                    systemImage: "exclamationmark.triangle"
                )
                .font(TendType.detail)
                .foregroundStyle(.orange)
            }

            if let options = model.workOptions(for: pipelinesHostAlias) {
                if options.pipelines.isEmpty, !showsPresetGallery {
                    HStack(spacing: 10) {
                        Text("No pipelines yet. Tend ships presets for one, two, and three agents.")
                            .font(TendType.detail)
                            .foregroundStyle(.secondary)
                        Spacer(minLength: 8)
                        Button("Browse Presets") { showsPresetGallery = true }
                            .buttonStyle(.tendGhost)
                    }
                    .padding(14)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous))
                } else if options.pipelines.isEmpty {
                    WorkPresetGallery(
                        options: options,
                        host: pipelinesHostAlias,
                        model: model,
                        onInstalled: { installed in
                            model.presentWorkStart(pipeline: installed, host: pipelinesHostAlias)
                        },
                        onDescribe: describeWithAgentAction
                    )
                    .padding(16)
                    .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous))
                } else {
                    LazyVGrid(columns: pipelineColumns, alignment: .leading, spacing: 12) {
                        ForEach(options.pipelines) { pipeline in
                            WorkPipelineCard(
                                pipeline: pipeline,
                                routes: options.routes.filter { $0.pipeline == pipeline.name },
                                start: { model.presentWorkStart(pipeline: pipeline.name) },
                                edit: { model.presentPipelineEditor(host: nil, pipeline: pipeline) },
                                hostStates: model.pipelineHostStates(for: pipeline),
                                sync: { sync(pipeline) },
                                syncing: syncingPipelines.contains(pipeline.name),
                                moduleContext: (model: model, host: pipelinesHostAlias)
                            )
                        }
                    }
                    remoteOnlyPipelinesRow
                    if !syncFailures.isEmpty {
                        ForEach(syncFailures.keys.sorted(), id: \.self) { key in
                            Label {
                                Text(verbatim: "\(key): \(syncFailures[key] ?? "")")
                            } icon: {
                                Image(systemName: "exclamationmark.triangle.fill")
                            }
                                .font(TendType.detail)
                                .foregroundStyle(.orange)
                                .textSelection(.enabled)
                        }
                    }
                    routesSection
                    if let error = model.workOptionsError {
                        Label(error, systemImage: "exclamationmark.triangle.fill")
                            .font(TendType.detail)
                            .foregroundStyle(.orange)
                            .textSelection(.enabled)
                    }
                }
            } else if let error = model.workOptionsError(for: pipelinesHostAlias) {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(TendType.detail)
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
                    .padding(14)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous))
            } else {
                Text("Reading pipelines from the tend config…")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .padding(14)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous))
            }
        }
    }

    private var libraryNeedsSync: Bool {
        (model.workOptions?.pipelines ?? []).contains { pipeline in
            model.pipelineHostStates(for: pipeline).contains(where: \.needsSync)
        }
    }

    private func sync(_ pipeline: TendWorkOptions.Pipeline) {
        guard !syncingPipelines.contains(pipeline.name) else { return }
        syncingPipelines.insert(pipeline.name)
        Task {
            let failures = await model.syncPipeline(pipeline)
            syncFailures = syncFailures.filter { !$0.key.hasSuffix("/\(pipeline.name)") }
            for (host, error) in failures { syncFailures["\(host)/\(pipeline.name)"] = error }
            syncingPipelines.remove(pipeline.name)
        }
    }

    private func syncAll() {
        let names = (model.workOptions?.pipelines ?? []).map(\.name)
        syncingPipelines.formUnion(names)
        Task {
            syncFailures = await model.syncAllPipelines()
            syncingPipelines.subtract(names)
        }
    }

    /// Pipelines that only exist on some host: pull one into the library
    /// (this Mac's config) so it can be synced everywhere.
    @ViewBuilder
    private var remoteOnlyPipelinesRow: some View {
        let remoteOnly = model.remoteOnlyPipelines
        if !remoteOnly.isEmpty {
            VStack(alignment: .leading, spacing: 6) {
                Text("Only on other hosts")
                    .font(.headline)
                ForEach(Array(remoteOnly.enumerated()), id: \.offset) { _, entry in
                    HStack(spacing: 10) {
                        Text(entry.pipeline.name)
                            .font(.callout.weight(.medium))
                        Text("on host \(entry.host)")
                            .font(TendType.detail)
                            .foregroundStyle(.secondary)
                        PipelineStagesView(agents: entry.pipeline.agents, compact: true)
                            .frame(maxWidth: 420)
                        Spacer(minLength: 4)
                        Button {
                            Task {
                                _ = await model.savePipeline(
                                    TendPipelineDefinition(entry.pipeline),
                                    named: entry.pipeline.name,
                                    host: nil
                                )
                            }
                        } label: {
                            Label("Add to Library", systemImage: "square.and.arrow.down")
                        }
                        .controlSize(.small)
                        .disabled(model.isSavingPipeline)
                        .help("Copy this pipeline into this Mac's config so it can be synced to every host")
                    }
                }
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous))
        }
    }

    /// Routes are host-specific (they carry folders and workspaces on that
    /// host), so the editor keeps its own host switcher.
    @ViewBuilder
    private var routesSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            if model.workCapableHosts.count > 1 {
                Picker("Routes on", selection: $routesHost) {
                    ForEach(model.workCapableHosts) { candidate in
                        Group {
                            if candidate.local {
                                Text("\(candidate.alias) (this Mac)")
                            } else {
                                Text(candidate.alias)
                            }
                        }
                        .tag(candidate.local ? "" : candidate.alias)
                    }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .frame(maxWidth: 560, alignment: .leading)
            }
            if let options = model.workOptions(for: routesHostAlias) {
                WorkRoutesEditor(options: options, host: routesHostAlias, model: model)
            } else if let error = model.workOptionsError(for: routesHostAlias) {
                Label {
                    Text(verbatim: "\(routesHost): \(error)")
                } icon: {
                    Image(systemName: "exclamationmark.triangle.fill")
                }
                    .font(TendType.detail)
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
            } else {
                Text("Reading routes on \(routesHost)…")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var commandCenterTitle: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text("Work Command Center")
                .font(.system(size: 22, weight: .semibold))
                .fixedSize(horizontal: false, vertical: true)
            Text("Start outcomes, coordinate collaborators, and inspect their execution without returning to tmux.")
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var commandCenterActions: some View {
        HStack(spacing: 10) {
            Button("Fleet Dispatch") { model.isPresentingFleetDispatch = true }
                .disabled(!model.isConnected)
            Button {
                model.select(.watch)
            } label: {
                Label("Live Watch", systemImage: "waveform.path.ecg.rectangle")
            }
            .buttonStyle(.tendSecondary)
            Button {
                model.presentWorkStart()
            } label: {
                Label("Start Work", systemImage: "play.fill")
            }
            .buttonStyle(.tendPrimary)
            .disabled(!model.isConnected || model.isStartingWork)
        }
    }
}

/// The Command Center's counts as one sentence-like line. A zero count
/// says nothing worth a card, so only Needs attention and Hosts always
/// show: attention because "none" is the answer the reader came for.
/// The fleet at a glance. "Need attention" is the Inbox badge's count and
/// opens the Inbox; every count that can open its list is a button.
private struct CommandCenterStatusLine: View {
    @ObservedObject var attention: TendAgentAttentionCenter
    let working: Int
    let work: Int
    let hosts: Int
    let openInbox: () -> Void
    let openWork: () -> Void
    let openHosts: () -> Void

    private var needsYou: Int { attention.summary.needsYou }
    private var limited: Int { attention.summary.limited }

    var body: some View {
        ViewThatFits(in: .horizontal) {
            HStack(spacing: 18) { items }
            VStack(alignment: .leading, spacing: 8) { items }
        }
    }

    @ViewBuilder
    private var items: some View {
        if needsYou > 0 {
            Button(action: openInbox) {
                stat(needsYou, "Need attention", color: .orange, symbol: "exclamationmark.circle.fill")
            }
            .buttonStyle(.plain)
            .help(attention.summary.needsYouLine ?? "")
        } else {
            Label("Nothing needs you", systemImage: "checkmark.circle.fill")
                .font(TendType.body.weight(.medium))
                .foregroundStyle(.green)
        }
        if working > 0 {
            stat(working, "Working", color: .blue, symbol: "circle.dotted")
        }
        if limited > 0 {
            stat(limited, "Limited", color: .purple, symbol: "hourglass")
        }
        if work > 0 {
            Button(action: openWork) {
                stat(work, "Managed Work", color: .accentColor, symbol: "square.stack.3d.up")
            }
            .buttonStyle(.plain)
        }
        Button(action: openHosts) {
            stat(hosts, "Hosts", color: .secondary, symbol: "server.rack")
        }
        .buttonStyle(.plain)
    }

    private func stat(_ value: Int, _ title: LocalizedStringKey, color: Color, symbol: String) -> some View {
        HStack(spacing: 6) {
            Image(systemName: symbol)
                .foregroundStyle(color)
            Text(verbatim: "\(value)")
                .font(.system(size: 17, weight: .semibold).monospacedDigit())
            Text(title)
                .font(TendType.detail)
                .foregroundStyle(.secondary)
        }
        .fixedSize()
    }
}

/// What the Command Center asks the reader to look at first: agents that
/// need them, worst first, each with what it last said. Rate-limited agents
/// follow on one line each, with when they resume.
private struct CommandCenterAttentionList: View {
    @EnvironmentObject private var model: AppModel
    let attention: [TendHostedAgent]
    let limited: [TendHostedAgent]
    let open: (TendHostedAgent) -> Void

    private static let limit = 6

    var body: some View {
        if !attention.isEmpty || !limited.isEmpty {
            VStack(alignment: .leading, spacing: 10) {
                Text("Look at now")
                    .font(.system(size: 14, weight: .semibold))
                VStack(spacing: 0) {
                    ForEach(Array(attention.prefix(Self.limit).enumerated()), id: \.element.id) { index, hosted in
                        if index > 0 { Divider().padding(.leading, 36) }
                        attentionRow(hosted)
                    }
                    if attention.count > Self.limit {
                        Divider()
                        Button {
                            model.show(.watch)
                        } label: {
                            HStack(spacing: 4) {
                                Text("\(attention.count - Self.limit) more in Explore › Needs attention")
                                Image(systemName: "arrow.right").font(.system(size: 9, weight: .semibold))
                            }
                            .font(TendType.detail)
                            .foregroundStyle(Color.accentColor)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.horizontal, 14)
                            .padding(.vertical, 9)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                    }
                    if !limited.isEmpty {
                        if !attention.isEmpty { Divider() }
                        limitedRow
                    }
                }
                .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous))
                .overlay {
                    RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous)
                        .stroke(Color(nsColor: .separatorColor).opacity(0.4), lineWidth: 0.5)
                }
            }
        }
    }

    private func attentionRow(_ hosted: TendHostedAgent) -> some View {
        Button { open(hosted) } label: {
            HStack(alignment: .top, spacing: 10) {
                HostIdentityBadge(identity: hosted.host, size: 26)
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 6) {
                        TendAgentLogo(kind: hosted.agent.kind, size: 14)
                        Text(commandCenterAgentTitle(hosted))
                            .font(TendType.body.weight(.semibold))
                            .lineLimit(1)
                        AgentStatusTag(status: hosted.agent.status, detailed: true)
                        Spacer(minLength: 6)
                        Text(verbatim: commandCenterAgentLocation(hosted))
                            .font(TendType.meta)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                    if let activity = agentActivity(hosted.agent) {
                        Text(activity)
                            .font(TendType.detail)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                            .multilineTextAlignment(.leading)
                    }
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private var limitedRow: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "hourglass")
                .foregroundStyle(.purple)
                .frame(width: 26)
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 8) {
                    Text(verbatim: "\(String(localized: "Limited")) · \(limited.count)")
                        .font(TendType.detail.weight(.semibold))
                        .foregroundStyle(.purple)
                    Spacer(minLength: 0)
                    TendSwitchLimitedButton(model: model, limited: limited)
                }
                LimitedAgentLinks(agents: limited, open: open)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// Rate-limited agents as `@name → 15:45 host` links, wrapping as needed.
private struct LimitedAgentLinks: View {
    @EnvironmentObject private var model: AppModel
    let agents: [TendHostedAgent]
    let open: (TendHostedAgent) -> Void

    var body: some View {
        LazyVGrid(columns: [GridItem(.adaptive(minimum: 180), spacing: 10, alignment: .leading)], alignment: .leading, spacing: 4) {
            ForEach(agents) { hosted in
                Button { open(hosted) } label: {
                    HStack(spacing: 4) {
                        Text(commandCenterAgentTitle(hosted))
                            .font(TendType.detail.weight(.medium))
                        if case .limited(let cap) = hosted.agent.status, let until = cap.until {
                            Text(verbatim: "→ \(TendUsageFormat.clock(until))")
                                .font(TendType.meta.monospacedDigit())
                                .foregroundStyle(.secondary)
                        }
                        Text(verbatim: hosted.host.alias)
                            .font(TendType.meta)
                            .foregroundStyle(.secondary)
                    }
                    .lineLimit(1)
                }
                .buttonStyle(.plain)
                .help(Text(verbatim: commandCenterAgentLocation(hosted)))
                .contextMenu {
                    Button("Open") { open(hosted) }
                    Divider()
                    TendAgentActionMenuItems(model: model, target: TendAgentActionTarget(hosted))
                }
            }
        }
    }
}

private func commandCenterAgentTitle(_ hosted: TendHostedAgent) -> String {
    hosted.displayTitle
}

private func commandCenterAgentLocation(_ hosted: TendHostedAgent) -> String {
    guard let pane = hosted.pane else { return hosted.host.alias }
    return "\(hosted.host.alias) · \(pane.session) › \(pane.windowName)"
}

private struct WorkCommandCard: View {
    let work: TendWorkGroup
    let open: () -> Void
    /// What the enabled modules offer for this Work; nil when there is no
    /// model to give them.
    var moduleModel: AppModel?

    private var statusText: Text {
        if work.attentionCount > 0 {
            Text("Attention")
        } else if work.workingCount > 0 {
            Text("Running")
        } else {
            Text("Ready")
        }
    }

    var body: some View {
        Button(action: open) {
            VStack(alignment: .leading, spacing: 12) {
                HStack {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(work.workspaceID.uppercased())
                            .font(TendType.meta.weight(.semibold))
                            .foregroundStyle(.secondary)
                        Text(work.title)
                            .font(.headline)
                    }
                    Spacer()
                    Label {
                        statusText
                    } icon: {
                        Image(systemName: "circle.fill")
                    }
                    .font(TendType.detail.weight(.medium))
                    .foregroundStyle(work.attentionCount > 0 ? .orange : work.workingCount > 0 ? .blue : .green)
                    if let moduleModel {
                        TendModuleMenu(
                            context: .work(work),
                            model: moduleModel,
                            registry: TendModuleRegistry.shared,
                            label: "More"
                        )
                    }
                }
                Text(work.pipelineLabel)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                HStack(spacing: 14) {
                    Label {
                        Text(verbatim: "\(work.participants.count)")
                    } icon: {
                        Image(systemName: "person.2")
                    }
                    if work.pipelineRun != nil {
                        Label {
                            Text(verbatim: "\(work.completedCount)/\(work.totalCount)")
                        } icon: {
                            Image(systemName: "checkmark.circle")
                        }
                    }
                    if !work.hostAliases.isEmpty {
                        Label(work.hostAliases.joined(separator: ", "), systemImage: "network")
                    }
                }
                .font(TendType.detail)
                .foregroundStyle(.secondary)
            }
            .padding(15)
            .frame(maxWidth: .infinity, minHeight: 140, alignment: .topLeading)
            .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous)
                    .stroke(
                        work.attentionCount > 0
                            ? Color.orange.opacity(0.5)
                            : Color(nsColor: .separatorColor).opacity(0.5)
                    )
            }
        }
        .buttonStyle(.plain)
    }
}

private extension String {
    var nonEmpty: String? { isEmpty ? nil : self }
}
