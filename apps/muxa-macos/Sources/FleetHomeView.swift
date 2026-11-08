// Fleet Home: check what needs you, inspect a unit of work, start more.
import SwiftUI

struct FleetHomeView: View {
    @ObservedObject var model: AppModel
    @Environment(\.colorScheme) private var colorScheme

    private enum Page: Hashable {
        case board
        case templates
    }

    @State private var page: Page = .board
    @State private var selectedID: String?
    @State private var showsHosts = false

    private var works: [FleetWork] {
        FleetBoard.works(groups: model.workGroups, agents: model.hostedAgents)
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            switch page {
            case .board: board
            case .templates: WorkCommandCenterView(model: model, pipelinesOnly: true)
            }
        }
        .background(MuxaSurfacePalette.workspace(for: colorScheme).ignoresSafeArea())
    }

    // MARK: - Header

    private var header: some View {
        HStack(spacing: 12) {
            Text("Fleet Home")
                .font(.system(size: 17, weight: .semibold))
            Picker("Page", selection: $page) {
                Text("Board").tag(Page.board)
                Text("Templates").tag(Page.templates)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            Spacer(minLength: 8)
            hostsButton
            Button {
                model.presentNewAgent()
            } label: {
                Label("New Agent", systemImage: "plus")
            }
            .buttonStyle(.muxaSecondary)
            .disabled(!model.isConnected)
            Button {
                model.presentWorkStart()
            } label: {
                Label("Start Work", systemImage: "play.fill")
            }
            .buttonStyle(.muxaPrimary)
            .disabled(!model.isConnected || model.isStartingWork)
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
    }

    private var hostsButton: some View {
        let hosts = model.fleetHosts
        let online = hosts.filter { $0.state == "online" }.count
        return Button {
            showsHosts.toggle()
        } label: {
            HStack(spacing: 6) {
                Circle()
                    .fill(online == hosts.count ? Color.green : Color.orange)
                    .frame(width: 7, height: 7)
                Text("Hosts \(online)/\(hosts.count)")
                    .monospacedDigit()
            }
        }
        .buttonStyle(.muxaGhost)
        .help("Host connections")
        .popover(isPresented: $showsHosts, arrowEdge: .bottom) {
            FleetHostsPopover(hosts: hosts) { host in
                showsHosts = false
                model.select(.host(host.id))
            }
        }
    }

    // MARK: - Board

    private var board: some View {
        let works = works
        let selected = works.first { $0.id == selectedID }
        return HStack(spacing: 0) {
            if works.isEmpty {
                FleetEmptyBoard(
                    isConnected: model.isConnected,
                    newAgent: { model.presentNewAgent() },
                    startWork: { model.presentWorkStart() }
                )
            } else {
                HStack(alignment: .top, spacing: 12) {
                    ForEach(FleetLane.allCases) { lane in
                        FleetLaneColumn(
                            lane: lane,
                            works: works.filter { $0.lane == lane },
                            selectedID: selectedID,
                            select: { id in selectedID = selectedID == id ? nil : id },
                            open: open
                        )
                    }
                }
                .padding(16)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            }
            if let selected {
                Divider()
                FleetWorkInspector(
                    work: selected,
                    close: { selectedID = nil },
                    open: open,
                    openWork: { identity in model.select(.work(identity)) }
                )
                .frame(width: 400)
                .transition(.move(edge: .trailing))
            }
        }
        .animation(.easeOut(duration: 0.18), value: selected?.id)
        .onExitCommand { selectedID = nil }
    }

    /// An agent's pane when it has a live one, its agent editor otherwise.
    private func open(_ hosted: MuxaHostedAgent) {
        if let pane = MuxaAgentAttentionCenter.paneIdentity(for: hosted),
           model.executionSnapshot.watchPane(id: pane) != nil {
            model.selectWatchPane(pane)
        } else {
            model.select(.agent(hosted.id))
        }
    }
}

// MARK: - Lanes and cards

private struct FleetLaneColumn: View {
    let lane: FleetLane
    let works: [FleetWork]
    let selectedID: String?
    let select: (String) -> Void
    let open: (MuxaHostedAgent) -> Void

    private var title: String {
        switch lane {
        case .needsYou: String(localized: "Your turn")
        case .running: String(localized: "In progress")
        case .finished: String(localized: "Finished")
        }
    }

    private var subtitle: String {
        switch lane {
        case .needsYou: String(localized: "Decide or review")
        case .running: String(localized: "Agents are on it")
        case .finished: String(localized: "Ready for the next instruction")
        }
    }

    private var empty: String {
        switch lane {
        case .needsYou: String(localized: "Nothing needs you right now.")
        case .running: String(localized: "No agent is working.")
        case .finished: String(localized: "Nothing finished yet.")
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(title)
                    .font(.system(size: 13, weight: .bold))
                    .foregroundStyle(lane == .needsYou && !works.isEmpty ? Color.orange : Color.primary)
                Text("\(works.count)")
                    .font(.system(size: 12, weight: .semibold))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                Spacer(minLength: 4)
                Text(subtitle)
                    .font(MuxaType.meta)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
            }
            .padding(.horizontal, 12)
            .padding(.top, 10)
            .padding(.bottom, 8)
            ScrollView {
                LazyVStack(spacing: 8) {
                    if works.isEmpty {
                        Text(empty)
                            .font(MuxaType.detail)
                            .foregroundStyle(.tertiary)
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 24)
                    }
                    ForEach(works) { work in
                        FleetWorkCard(
                            work: work,
                            isSelected: work.id == selectedID,
                            select: { select(work.id) },
                            open: open
                        )
                    }
                }
                .padding(.horizontal, 8)
                .padding(.bottom, 10)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
    }
}

private struct FleetWorkCard: View {
    let work: FleetWork
    let isSelected: Bool
    let select: () -> Void
    let open: (MuxaHostedAgent) -> Void
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(work.title)
                .font(.system(size: 13, weight: .semibold))
                .lineLimit(2)
                .frame(maxWidth: .infinity, alignment: .leading)
            if work.lane != .finished || work.stages.count > 1 {
                FleetStageBar(work: work)
            }
            Text(work.headline)
                .font(MuxaType.body)
                .foregroundStyle(work.lane == .needsYou ? Color.primary : Color.secondary)
                .lineLimit(work.lane == .needsYou ? 3 : 2)
                .frame(maxWidth: .infinity, alignment: .leading)
            if work.handledLimit {
                Label("Rate limited · the limit policy will resume it", systemImage: "arrow.triangle.2.circlepath")
                    .font(MuxaType.meta)
                    .foregroundStyle(.secondary)
            }
            if work.lane == .needsYou, let focus = work.focus {
                Button("Open") { open(focus) }
                    .buttonStyle(.muxaPrimary)
                    .controlSize(.small)
            }
            HStack(spacing: 6) {
                if let repo = work.repo {
                    Text(repo)
                }
                Text(work.hostAliases.joined(separator: ", "))
                    .lineLimit(1)
                Spacer(minLength: 4)
                if let since = work.since {
                    Text(since, format: .relative(presentation: .numeric, unitsStyle: .abbreviated))
                        .monospacedDigit()
                }
            }
            .font(MuxaType.meta)
            .foregroundStyle(.tertiary)
        }
        .padding(12)
        .background(MuxaSurfacePalette.workspace(for: colorScheme), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .strokeBorder(isSelected ? Color.accentColor : MuxaTheme.border(colorScheme), lineWidth: isSelected ? 1.5 : 1)
        )
        .contentShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
        .onTapGesture(perform: select)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isButton)
        .accessibilityAction(named: Text("Inspect"), select)
    }
}

/// One segment per stage, with the current stage named under it.
private struct FleetStageBar: View {
    let work: FleetWork

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 3) {
                ForEach(Array(work.stages.enumerated()), id: \.offset) { _, stage in
                    Capsule()
                        .fill(stage.state.color)
                        .frame(height: 4)
                }
            }
            if let index = work.currentStageIndex {
                HStack {
                    Text(work.stages[index].name)
                        .fontWeight(.medium)
                        .foregroundStyle(.secondary)
                    Spacer()
                    if work.stages.count > 1 {
                        Text("\(index + 1)/\(work.stages.count)")
                            .monospacedDigit()
                    }
                }
                .font(MuxaType.meta)
                .foregroundStyle(.tertiary)
            }
        }
    }
}

extension FleetWorkStage.State {
    var color: Color {
        switch self {
        case .done: .green
        case .running: .blue
        case .needsYou: .orange
        case .waiting: Color.secondary.opacity(0.25)
        }
    }

    var label: String {
        switch self {
        case .done: String(localized: "Done")
        case .running: String(localized: "Working")
        case .needsYou: String(localized: "Your turn")
        case .waiting: String(localized: "Waiting")
        }
    }
}

// MARK: - Inspector

private struct FleetWorkInspector: View {
    let work: FleetWork
    let close: () -> Void
    let open: (MuxaHostedAgent) -> Void
    let openWork: (MuxaWorkIdentity) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .top, spacing: 8) {
                VStack(alignment: .leading, spacing: 4) {
                    Text(work.title)
                        .font(.system(size: 16, weight: .semibold))
                        .fixedSize(horizontal: false, vertical: true)
                    Text([work.repo, work.hostAliases.joined(separator: ", ")].compactMap { $0 }.joined(separator: " · "))
                        .font(MuxaType.meta)
                        .foregroundStyle(.secondary)
                }
                Spacer(minLength: 4)
                Button(action: close) {
                    Image(systemName: "xmark")
                }
                .buttonStyle(.muxaGhost)
                .help("Close")
                .keyboardShortcut(.cancelAction)
            }
            .padding(16)
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    now
                    section("Stages") {
                        VStack(alignment: .leading, spacing: 0) {
                            ForEach(Array(work.stages.enumerated()), id: \.offset) { index, stage in
                                FleetStageRow(stage: stage, isLast: index == work.stages.count - 1)
                            }
                        }
                    }
                    section("Agents") {
                        VStack(alignment: .leading, spacing: 8) {
                            ForEach(work.agents) { hosted in
                                HStack(spacing: 8) {
                                    AgentStatusTag(status: hosted.agent.status, showsLabel: false)
                                    VStack(alignment: .leading, spacing: 1) {
                                        Text(FleetBoard.owner(hosted))
                                            .font(MuxaType.body.weight(.medium))
                                        Text(hosted.agent.status.label())
                                            .font(MuxaType.meta)
                                            .foregroundStyle(.secondary)
                                    }
                                    Spacer(minLength: 4)
                                    Button("Open") { open(hosted) }
                                        .buttonStyle(.muxaSecondary)
                                        .controlSize(.small)
                                }
                            }
                        }
                    }
                }
                .padding(16)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    @ViewBuilder
    private var now: some View {
        let tint: Color = switch work.lane {
        case .needsYou: .orange
        case .running: .blue
        case .finished: .green
        }
        VStack(alignment: .leading, spacing: 10) {
            Text(laneTitle)
                .font(.system(size: 11, weight: .bold))
                .foregroundStyle(tint)
            Text(work.headline)
                .font(.system(size: 13.5, weight: work.lane == .needsYou ? .semibold : .regular))
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
            if work.handledLimit {
                Label("Rate limited · the limit policy will resume it", systemImage: "arrow.triangle.2.circlepath")
                    .font(MuxaType.meta)
                    .foregroundStyle(.secondary)
            }
            HStack(spacing: 8) {
                if let focus = work.focus {
                    Button(work.lane == .needsYou ? "Answer in Pane" : "Open Pane") { open(focus) }
                        .buttonStyle(.muxaPrimary)
                }
                if case .work(let identity) = work.source {
                    Button("Work Details") { openWork(identity) }
                        .buttonStyle(.muxaSecondary)
                }
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(tint.opacity(0.1), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 9, style: .continuous).strokeBorder(tint.opacity(0.45)))
    }

    private var laneTitle: String {
        switch work.lane {
        case .needsYou: String(localized: "Needs your decision")
        case .running: String(localized: "Now")
        case .finished: String(localized: "Last result")
        }
    }

    private func section<Content: View>(_ title: LocalizedStringKey, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title)
                .font(.system(size: 11, weight: .bold))
                .foregroundStyle(.secondary)
                .textCase(.uppercase)
            content()
        }
    }
}

private struct FleetStageRow: View {
    let stage: FleetWorkStage
    let isLast: Bool

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            VStack(spacing: 0) {
                Circle()
                    .strokeBorder(stage.state == .waiting ? Color.secondary.opacity(0.4) : stage.state.color, lineWidth: 2)
                    .background(Circle().fill(stage.state == .done ? stage.state.color : Color.clear))
                    .frame(width: 12, height: 12)
                    .padding(.top, 3)
                if !isLast {
                    Rectangle()
                        .fill(Color.secondary.opacity(0.25))
                        .frame(width: 2)
                        .frame(maxHeight: .infinity)
                }
            }
            .frame(width: 12)
            VStack(alignment: .leading, spacing: 1) {
                Text(stage.name).fontWeight(.semibold)
                Text(stage.owner)
                    .font(MuxaType.meta)
                    .foregroundStyle(.secondary)
            }
            .padding(.bottom, isLast ? 0 : 10)
            Spacer(minLength: 4)
            Text(stage.state.label)
                .font(MuxaType.meta)
                .foregroundStyle(.secondary)
        }
        .fixedSize(horizontal: false, vertical: true)
    }
}

// MARK: - Hosts and empty board

private struct FleetHostsPopover: View {
    let hosts: [MuxaFleetHost]
    let select: (MuxaFleetHost) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Hosts")
                .font(.headline)
            if hosts.isEmpty {
                Text("No hosts yet.")
                    .foregroundStyle(.secondary)
            }
            ForEach(hosts) { host in
                Button {
                    select(host)
                } label: {
                    HStack(spacing: 8) {
                        Circle()
                            .fill(fleetHostColor(host.state))
                            .frame(width: 7, height: 7)
                        Text(host.alias).fontWeight(.medium)
                        Spacer(minLength: 12)
                        Text(fleetHostStateLabel(host.state))
                            .foregroundStyle(.secondary)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
        .padding(14)
        .frame(width: 260)
    }
}

private struct FleetEmptyBoard: View {
    let isConnected: Bool
    let newAgent: () -> Void
    let startWork: () -> Void

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: "rectangle.3.group")
                .font(.system(size: 38))
                .foregroundStyle(.secondary)
            Text("No work yet")
                .font(.title3.weight(.semibold))
            Text("Start a Work from a template, or start one agent. Each shows up here as a unit of work.")
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 380)
            HStack {
                Button("Start Work", action: startWork)
                    .buttonStyle(.muxaPrimary)
                Button("New Agent", action: newAgent)
                    .buttonStyle(.muxaSecondary)
            }
            .disabled(!isConnected)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(32)
    }
}
