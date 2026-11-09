// Home: check what needs you, inspect a unit of work, start more.
import SwiftUI

struct FleetHomeView: View {
    @ObservedObject var model: AppModel
    @Environment(\.colorScheme) private var colorScheme

    @State private var selectedID: String?
    @State private var showsHosts = false
    @State private var hostFilter: String?
    @State private var projectFilter: String?
    @State private var reviewed = FleetReviewMarks(defaults: .standard)
    @State private var showsReviewed = false
    /// Reply drafts per unit, kept while the inspector closes or the unit's
    /// focus changes, each bound to the question it was written for.
    @State private var drafts: [String: FleetReplyDraft] = [:]
    @State private var approvalSeed: FleetApprovalSeed?
    @State private var quickLook: FleetQuickLookTarget?
    @State private var showsReview = false
    @AppStorage("home.review.scope") private var reviewScope: FleetReviewScope = .day

    private var allWorks: [FleetWork] {
        FleetBoard.works(groups: model.workGroups, agents: model.hostedAgents)
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            board
        }
        .background(MuxaSurfacePalette.workspace(for: colorScheme).ignoresSafeArea())
        .sheet(item: $approvalSeed) { seed in
            FleetApprovalRuleSheet(model: model, seed: seed)
        }
        .sheet(isPresented: $showsReview) {
            FleetReviewView(
                model: model,
                works: { allWorks },
                marks: $reviewed,
                draft: draftBinding,
                submit: { submit(for: $0) },
                open: open,
                markReviewed: { work in
                    reviewed.markReviewed(work)
                    reviewed.save(to: .standard)
                },
                send: { text, target in try await send(text, to: target) },
                ask: { prompt in try await model.client.askOneShot(prompt) }
            )
        }
        .sheet(item: $quickLook) { target in
            let work = allWorks.first { work in
                (work.agents + work.helpers).contains { $0.id == target.agentID }
            }
            FleetQuickLookView(
                model: model,
                target: target,
                work: work,
                draft: draftBinding(work?.id ?? ""),
                submit: { if let work { submit(for: work.id) } },
                openFull: open
            )
        }
    }

    // MARK: - Header

    private var header: some View {
        HStack(spacing: 12) {
            Text("Home")
                .font(.system(size: 17, weight: .semibold))
            Spacer(minLength: 8)
            reviewButton
            filterMenu
            hostsButton
            FleetAutomationButton(model: model) { approvalSeed = FleetApprovalSeed() }
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

    /// Results the person asked for and has not reviewed, in the chosen
    /// range: the way to go through Finished quickly.
    private var reviewButton: some View {
        let count = FleetBoard.reviewQueue(allWorks, marks: reviewed, scope: reviewScope).count
        return Button {
            showsReview = true
        } label: {
            HStack(spacing: 5) {
                Image(systemName: "checklist")
                Text("Review")
                if count > 0 {
                    Text(verbatim: count > 99 ? "99+" : "\(count)")
                        .font(.system(size: 11, weight: .semibold))
                        .monospacedDigit()
                        .foregroundStyle(.white)
                        .lineLimit(1)
                        .fixedSize()
                        .padding(.horizontal, 5)
                        .background(Color.accentColor, in: Capsule())
                }
            }
        }
        .buttonStyle(.muxaSecondary)
        .fixedSize()
        .keyboardShortcut("r", modifiers: [.command, .shift])
        .help("Review the results of work you asked for (⇧⌘R)")
    }

    /// Narrows the board to one host and/or one project.
    private var filterMenu: some View {
        let all = allWorks
        let hosts = Array(Set(all.flatMap(\.hostAliases))).sorted()
        let projects = Array(Set(all.compactMap(\.repo))).sorted()
        let active = hostFilter != nil || projectFilter != nil
        return Menu {
            Picker("Host", selection: $hostFilter) {
                Text("All hosts").tag(String?.none)
                ForEach(hosts, id: \.self) { Text($0).tag(String?.some($0)) }
            }
            Picker("Project", selection: $projectFilter) {
                Text("All projects").tag(String?.none)
                ForEach(projects, id: \.self) { Text($0).tag(String?.some($0)) }
            }
            if active {
                Divider()
                Button("Clear Filters") {
                    hostFilter = nil
                    projectFilter = nil
                }
            }
        } label: {
            Label(filterLabel, systemImage: active ? "line.3.horizontal.decrease.circle.fill" : "line.3.horizontal.decrease.circle")
        }
        .menuStyle(.borderlessButton)
        .fixedSize()
        .foregroundStyle(active ? Color.accentColor : Color.primary)
        .help("Show one host or one project")
    }

    private var filterLabel: String {
        switch (hostFilter, projectFilter) {
        case (nil, nil): String(localized: "Filter")
        case (let host?, nil): host
        case (nil, let project?): project
        case (let host?, let project?): "\(project) · \(host)"
        }
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
        let all = FleetBoard.filter(allWorks, host: hostFilter, project: projectFilter)
        let works = all.filter { showsReviewed || !reviewed.hides($0) }
        let hiddenCount = all.filter { reviewed.hides($0) }.count
        let selected = all.first { $0.id == selectedID }
        return GeometryReader { geometry in
            // Beside the board while all three lanes keep a readable width;
            // over the board in a narrower window.
            let floats = geometry.size.width < 1040
            ZStack(alignment: .trailing) {
                HStack(spacing: 0) {
                    lanes(works, reviewedCount: hiddenCount)
                    if let selected, !floats {
                        Divider()
                        inspector(selected)
                            .frame(width: 400)
                            .transition(.move(edge: .trailing))
                    }
                }
                if let selected, floats {
                    inspector(selected)
                        .frame(width: min(400, geometry.size.width - 40))
                        .background(.background)
                        .overlay(alignment: .leading) { Divider() }
                        .shadow(color: .black.opacity(0.18), radius: 14, x: -4)
                        .transition(.move(edge: .trailing))
                }
            }
        }
        .animation(.easeOut(duration: 0.18), value: selected?.id)
        .onExitCommand { selectedID = nil }
        // Display filters hide units; they are not gone. Prune review marks
        // against the whole board.
        .onChange(of: allWorks.map(\.id)) { ids in
            // Only a live board says a unit is gone; a disconnect is not.
            guard model.isConnected, !ids.isEmpty else { return }
            var pruned = reviewed
            pruned.prune(keeping: Set(ids))
            if pruned != reviewed { reviewed = pruned; reviewed.save(to: .standard) }
        }
    }

    private func inspector(_ work: FleetWork) -> some View {
        FleetWorkInspector(
            model: model,
            work: work,
            isReviewed: reviewed.hides(work),
            close: { selectedID = nil },
            open: peek,
            openWork: { identity in model.select(.work(identity)) },
            draft: draftBinding(work.id),
            submit: { submit(for: work.id) },
            approveLikeThis: {
                var seed = FleetApprovalSeed(agent: work.focus)
                if case .work(let identity) = work.source { seed.work = identity }
                approvalSeed = seed
            },
            markReviewed: {
                reviewed.markReviewed(work)
                reviewed.save(to: .standard)
                selectedID = nil
            }
        )
        .id(work.id)
    }

    /// Sends a unit's draft. The board owns the send, so it outlives the
    /// inspector and a second submission waits for the first to finish.
    private func submit(for workID: String) {
        guard var draft = drafts[workID], let submission = draft.begin() else { return }
        drafts[workID] = draft
        Task {
            var failure: String?
            do {
                try await send(submission.text, to: submission.target)
            } catch {
                failure = error.localizedDescription
            }
            guard var current = drafts[workID] else { return }
            current.finish(submission, error: failure)
            drafts[workID] = current.isIdle ? nil : current
        }
    }

    /// Types `text` into the agent the draft was written for, after checking
    /// that the same session is still at its pane, at the same question.
    private func send(_ text: String, to target: FleetReplyTarget) async throws {
        guard let hosted = model.hostedAgents.first(where: { $0.id == target.agentID }),
              let paneID = MuxaAgentAttentionCenter.paneIdentity(for: hosted),
              let current = model.executionSnapshot.watchPane(id: paneID),
              target.matches(current.agent) else {
            throw MuxaIPCError.server(String(localized: "The agent moved on. Open its pane to see where it is."))
        }
        if let blocker = MuxaNotificationActions.replyBlocker(current) { throw MuxaIPCError.server(blocker) }
        try await model.client.sendFleetPrompt(host: current.host, pane: current.pane, text: text)
    }

    @ViewBuilder
    private func lanes(_ works: [FleetWork], reviewedCount: Int) -> some View {
        Group {
            if FleetBoard.showsEmptyBoard(visible: works.count, reviewed: reviewedCount) {
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
                            open: peek,
                            reviewedCount: lane == .finished ? reviewedCount : 0,
                            showsReviewed: $showsReviewed
                        )
                    }
                }
                .padding(16)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            }
        }
    }

    private func draftBinding(_ workID: String) -> Binding<FleetReplyDraft> {
        Binding(
            get: { drafts[workID] ?? FleetReplyDraft() },
            set: { drafts[workID] = $0.isIdle ? nil : $0 }
        )
    }

    /// Open on the board: a quick look first, the full view from there.
    private func peek(_ hosted: MuxaHostedAgent) {
        quickLook = FleetQuickLookTarget(agentID: hosted.id)
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
    var reviewedCount = 0
    @Binding var showsReviewed: Bool
    @State private var showsOlder = false
    @State private var showsProbable = false
    @State private var showsEmpty = false

    /// Where a card folds outside Your turn, each in exactly one row, so a
    /// row's count is what opening it shows: a possible helper first, then
    /// an empty session, then (in Finished) anything older than a day.
    private enum Fold { case probable, empty, older }

    private func fold(_ work: FleetWork) -> Fold? {
        guard lane != .needsYou else { return nil }
        if work.isProbableHelper { return .probable }
        if work.isEmptySession { return .empty }
        if lane == .finished, !FleetBoard.isRecent(work) { return .older }
        return nil
    }

    private var shown: [FleetWork] {
        works.filter { work in
            switch fold(work) {
            case .probable: showsProbable
            case .empty: showsEmpty
            case .older: showsOlder
            case nil: true
            }
        }
    }

    private var emptyCount: Int { works.filter { fold($0) == .empty }.count }
    private var probableCount: Int { works.filter { fold($0) == .probable }.count }
    private var olderCount: Int { works.filter { fold($0) == .older }.count }

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
                    let groups = FleetBoard.byProject(shown)
                    ForEach(groups) { group in
                        if groups.count > 1 {
                            FleetProjectHeader(project: group.project, count: group.works.count)
                        }
                        ForEach(group.works) { work in
                            FleetWorkCard(
                                work: work,
                                isSelected: work.id == selectedID,
                                select: { select(work.id) },
                                open: open
                            )
                        }
                    }
                    if emptyCount > 0 {
                        Button(showsEmpty
                            ? String(localized: "Hide empty sessions")
                            : String(localized: "\(emptyCount) empty sessions")) {
                            showsEmpty.toggle()
                        }
                        .buttonStyle(.muxaGhost)
                        .controlSize(.small)
                        .padding(.top, 2)
                        .help("Agents that were opened but never given a task")
                    }
                    if probableCount > 0 {
                        Button(showsProbable
                            ? String(localized: "Hide possible helpers")
                            : String(localized: "\(probableCount) possible helper sessions")) {
                            showsProbable.toggle()
                        }
                        .buttonStyle(.muxaGhost)
                        .controlSize(.small)
                        .padding(.top, 2)
                        .help("Sessions whose prompt hands over a muxa request; who asked is not confirmed")
                    }
                    if olderCount > 0 {
                        Button(showsOlder ? String(localized: "Hide older") : String(localized: "\(olderCount) older")) {
                            showsOlder.toggle()
                        }
                        .buttonStyle(.muxaGhost)
                        .controlSize(.small)
                        .padding(.top, 2)
                    }
                    if reviewedCount > 0 || (lane == .finished && showsReviewed) {
                        Button(showsReviewed ? String(localized: "Hide reviewed") : String(localized: "Show \(reviewedCount) reviewed")) {
                            showsReviewed.toggle()
                        }
                        .buttonStyle(.muxaGhost)
                        .controlSize(.small)
                        .padding(.top, 2)
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

/// A project's name over its units in a lane.
private struct FleetProjectHeader: View {
    let project: String?
    let count: Int

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: "folder")
                .font(.system(size: 10))
            Text(project ?? String(localized: "No project"))
                .font(.system(size: 11, weight: .semibold))
                .lineLimit(1)
            Text("\(count)")
                .font(.system(size: 11))
                .monospacedDigit()
                .foregroundStyle(.tertiary)
            Spacer(minLength: 0)
        }
        .foregroundStyle(.secondary)
        .padding(.horizontal, 4)
        .padding(.top, 6)
        .accessibilityElement(children: .combine)
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
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                if work.isOrphanHelper {
                    Text("Helper")
                        .font(.system(size: 10, weight: .semibold))
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(Color.secondary.opacity(0.15), in: Capsule())
                        .foregroundStyle(.secondary)
                        .help("Working on another agent's request; that agent is no longer running")
                }
                Text(work.title)
                    .font(.system(size: 13, weight: .semibold))
                    .lineLimit(2)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
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
                FleetHostBadges(hosts: work.hosts)
                if !work.helpers.isEmpty {
                    Label("\(work.helpers.count)", systemImage: "person.2")
                        .labelStyle(.titleAndIcon)
                        .help("\(work.helpers.count) helper sessions")
                }
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
                        Text("\(work.doneStageCount)/\(work.stages.count)")
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
    @ObservedObject var model: AppModel
    let work: FleetWork
    let isReviewed: Bool
    let close: () -> Void
    let open: (MuxaHostedAgent) -> Void
    let openWork: (MuxaWorkIdentity) -> Void
    @Binding var draft: FleetReplyDraft
    let submit: () -> Void
    let approveLikeThis: () -> Void
    let markReviewed: () -> Void

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
                            ForEach(work.agents) { hosted in agentRow(hosted) }
                        }
                    }
                    if !work.helpers.isEmpty {
                        section("Helpers") {
                            VStack(alignment: .leading, spacing: 8) {
                                Text("Sessions other agents started to help with this work.")
                                    .font(MuxaType.meta)
                                    .foregroundStyle(.secondary)
                                ForEach(work.helpers) { hosted in agentRow(hosted) }
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
            if FleetBoard.choosesInPane(work), let focus = work.focus, focus.pane != nil {
                FleetChoicePanel(model: model, hosted: focus)
                    .id(focus.id)
            }
            if FleetBoard.replyTarget(for: work) != nil || !draft.isIdle {
                FleetReplyField(work: work, draft: $draft, submit: submit)
            }
            HStack(spacing: 8) {
                if let focus = work.focus {
                    Button(paneButtonTitle) { open(focus) }
                        .buttonStyle(FleetBoard.reply(for: work) == nil ? .muxaPrimary : .muxaSecondary)
                }
                if case .work(let identity) = work.source {
                    Button("Work Details") { openWork(identity) }
                        .buttonStyle(.muxaSecondary)
                }
                if FleetBoard.choosesInPane(work), work.focus.map({ FleetApprovalRules.supportedKinds.contains($0.agent.kind) }) == true {
                    Button("Always Approve This Command…", action: approveLikeThis)
                        .buttonStyle(.muxaGhost)
                        .help("Write a rule so Muxa approves this command on its own")
                }
                if work.lane == .finished && !isReviewed {
                    Spacer(minLength: 0)
                    Button(action: markReviewed) {
                        Label("Looks Good", systemImage: "checkmark")
                    }
                    .buttonStyle(.muxaGhost)
                    .help("Set this aside until it changes again")
                }
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(tint.opacity(0.1), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 9, style: .continuous).strokeBorder(tint.opacity(0.45)))
    }

    private func agentRow(_ hosted: MuxaHostedAgent) -> some View {
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

    private var paneButtonTitle: String {
        if FleetBoard.choosesInPane(work) { return String(localized: "Choose in Pane") }
        if work.lane == .needsYou && FleetBoard.reply(for: work) == nil { return String(localized: "Answer in Pane") }
        return String(localized: "Open Pane")
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

/// Answer an agent's question, or send a finished unit back to work. The
/// draft is the board's, bound to the question it was written for; shared
/// by the inspector and the quick look.
struct FleetReplyField: View {
    let work: FleetWork
    @Binding var draft: FleetReplyDraft
    let submit: () -> Void

    private var currentTarget: FleetReplyTarget? { FleetBoard.replyTarget(for: work) }

    /// Text written for a question the unit has moved past.
    private var isStale: Bool {
        guard let target = draft.target, draft.hasText else { return false }
        return target != currentTarget
    }

    var body: some View {
        let kind = (isStale ? nil : draft.target?.kind) ?? currentTarget?.kind ?? .answer
        VStack(alignment: .leading, spacing: 6) {
            if isStale, let target = draft.target {
                VStack(alignment: .leading, spacing: 6) {
                    Label("Written for an earlier question from \(target.owner). It was not sent.", systemImage: "exclamationmark.triangle.fill")
                        .font(MuxaType.meta)
                        .foregroundStyle(.orange)
                        .fixedSize(horizontal: false, vertical: true)
                    HStack(spacing: 8) {
                        if let current = currentTarget {
                            Button(String(localized: "Use for \(current.owner)")) {
                                draft.retarget(current)
                            }
                            .buttonStyle(.muxaSecondary)
                            .controlSize(.small)
                        }
                        Button("Discard", role: .destructive) {
                            draft.discard()
                        }
                        .buttonStyle(.muxaGhost)
                        .controlSize(.small)
                    }
                    .disabled(draft.isSending)
                }
            }
            HStack(alignment: .bottom, spacing: 8) {
                TextField(
                    kind == .answer ? String(localized: "Your answer") : String(localized: "Next instruction"),
                    text: Binding(
                        get: { draft.text },
                        set: { draft.edit($0, current: currentTarget) }
                    ),
                    axis: .vertical
                )
                .textFieldStyle(.roundedBorder)
                .lineLimit(1...6)
                .disabled(draft.isSending)
                .onSubmit(send)
                Button(kind == .answer ? String(localized: "Answer") : String(localized: "Send Back"), action: send)
                    .buttonStyle(.muxaPrimary)
                    .disabled(draft.isSending || isStale || draft.target == nil || !draft.hasText)
            }
            if draft.isSending {
                ProgressView().controlSize(.small)
            } else if let failure = draft.failure {
                Label(failure, systemImage: "exclamationmark.circle.fill")
                    .font(MuxaType.meta)
                    .foregroundStyle(.red)
            } else if draft.sent {
                Label("Sent", systemImage: "checkmark.circle.fill")
                    .font(MuxaType.meta)
                    .foregroundStyle(.green)
            }
        }
    }

    private func send() {
        guard !isStale else { return }
        submit()
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


/// Each host a unit runs on: its icon (the one set for the menu bar) and
/// its name.
struct FleetHostBadges: View {
    let hosts: [MuxaFleetHostIdentity]

    var body: some View {
        HStack(spacing: 6) {
            ForEach(hosts, id: \.alias) { host in
                let icon = MuxaHostIcons.icon(for: host.alias, local: host.local)
                HStack(spacing: 3) {
                    MuxaGlyphView(value: icon, size: 7)
                        .frame(width: 14, height: 14)
                        .background(MuxaMenuBarAgentAppearance(identity: host.alias, symbol: icon, provider: "").gradient, in: Circle())
                    Text(verbatim: host.alias)
                        .lineLimit(1)
                }
                .help(host.local ? String(localized: "\(host.alias) (this Mac)") : host.alias)
            }
        }
    }
}
