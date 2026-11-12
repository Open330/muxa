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
    @State private var migrating = false
    /// Reply drafts per unit, kept while the inspector closes or the unit's
    /// focus changes, each bound to the question it was written for.
    @State private var drafts: [String: FleetReplyDraft] = [:]
    @State private var approvalSeed: FleetApprovalSeed?
    @State private var quickLook: FleetQuickLookTarget?
    @State private var showsReview = false
    @State private var showsLeftovers = false
    /// The daemon's decision queue, read while Home is on screen.
    @StateObject private var decisionFeed = FleetDecisionFeed()
    /// A commit or range from a decision's evidence, open in Changes.
    @State private var revisionTarget: FleetRevisionTarget?
    @AppStorage("home.review.scope") private var reviewScope: FleetReviewScope = .day
    @AppStorage(TendPreferences.homeAnimatesMovesKey) private var animatesMoves = true
    @Namespace private var cardSpace
    /// Each unit's lane at the last update, to tell which cards moved.
    @State private var lastLanes: [String: FleetLane] = [:]
    /// Cards that just changed lane, with the lane they left; cleared a few
    /// seconds later.
    @State private var movedFrom: [String: FleetLane] = [:]

    private var allWorks: [FleetWork] {
        model.homeWorks
    }

    /// What hides a finished result: tendd's record of Looks Good when the
    /// daemon keeps one, else this Mac's own marks (an older tendd).
    private var marks: FleetReviewMarks {
        var marks = reviewed
        marks.recorded = decisionFeed.reviewedKeys
        return marks
    }

    /// Looks Good: recorded by tendd, which then hides the result. Against
    /// a tendd that cannot record, a local mark hides it as before.
    private func markReviewed(_ work: FleetWork) {
        if decisionFeed.reviewedKeys != nil {
            let feed = decisionFeed
            let client = model.client
            Task { await feed.recordLooksGood(work, client: client) }
            return
        }
        reviewed.markReviewed(work)
        reviewed.save(to: .standard)
        guard let target = FleetBoard.reviewTarget(work) else { return }
        let key = FleetBoard.reviewKey(work)
        let summary = FleetDecisionRecord.summary(work)
        let client = model.client
        Task {
            do {
                try await client.recordReviewed(target: target, key: key, summary: summary)
            } catch {
                TendLog.app.warning("recording a review failed: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    /// Hands this Mac's old review marks to tendd once it keeps the record,
    /// then forgets them. A failure leaves them for the next try.
    private func migrateMarksIfNeeded() {
        guard decisionFeed.reviewedKeys != nil, model.isConnected, !allWorks.isEmpty,
              !UserDefaults.standard.bool(forKey: FleetReviewMarks.migratedKey) else { return }
        guard !reviewed.marks.isEmpty else {
            UserDefaults.standard.set(true, forKey: FleetReviewMarks.migratedKey)
            return
        }
        guard !migrating else { return }
        migrating = true
        let feed = decisionFeed
        let client = model.client
        let marks = reviewed
        let works = allWorks
        Task {
            defer { migrating = false }
            guard await feed.migrate(marks, works: works, client: client) else { return }
            reviewed.clear()
            reviewed.save(to: .standard)
            UserDefaults.standard.set(true, forKey: FleetReviewMarks.migratedKey)
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            board
        }
        .background(TendSurfacePalette.workspace(for: colorScheme).ignoresSafeArea())
        .sheet(item: $approvalSeed) { seed in
            FleetApprovalRuleSheet(model: model, seed: seed)
        }
        .sheet(item: $revisionTarget) { target in
            ChangesRevisionView(repo: target.repo, revision: target.revision)
        }
        .task {
            // Decisions made elsewhere (the CLI, an agent, another window)
            // leave the queue while Home stays open.
            while !Task.isCancelled {
                await decisionFeed.load(model.client)
                try? await Task.sleep(for: FleetDecisionFeed.refreshEvery)
            }
        }
        .sheet(isPresented: $showsReview) {
            FleetReviewView(
                model: model,
                feed: decisionFeed,
                decisions: { decisions },
                decisionCard: { decisionCard($0, openRevision: $1) },
                works: { allWorks },
                marks: Binding(get: { marks }, set: { _ in }),
                draft: draftBinding,
                submit: { submit(for: $0) },
                open: open,
                markReviewed: { markReviewed($0) },
                send: { text, target in try await send(text, to: target) },
                ask: { prompt in try await model.client.askOneShot(prompt) }
            )
        }
        .sheet(isPresented: $showsLeftovers) {
            FleetLeftoversView(model: model)
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
            .buttonStyle(.tendPrimary)
            .disabled(!model.isConnected || model.isStartingWork)
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
    }

    /// Results the person asked for and has not reviewed, in the chosen
    /// range: the way to go through Finished quickly.
    private var reviewButton: some View {
        let count = FleetBoard.reviewQueue(allWorks, marks: marks, scope: reviewScope).count
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
        .buttonStyle(.tendSecondary)
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
        .buttonStyle(.tendGhost)
        .help("Host connections")
        .popover(isPresented: $showsHosts, arrowEdge: .bottom) {
            FleetHostsPopover(hosts: hosts, select: { host in
                showsHosts = false
                model.select(.host(host.id))
            }, showLeftovers: {
                showsHosts = false
                showsLeftovers = true
            })
        }
    }

    // MARK: - Board

    /// The decisions Home shows under the same host and project filters as
    /// the cards.
    private var decisions: [FleetDecision] {
        decisionFeed.decisions(model).filter { decision in
            (hostFilter == nil || decision.host.alias == hostFilter)
                && (projectFilter == nil || decision.repo == projectFilter)
        }
    }

    /// A decision card wired to the feed. `openRevision` is the sheet the
    /// card's host view presents Changes in.
    func decisionCard(_ decision: FleetDecision, openRevision: @escaping (FleetRevisionTarget) -> Void) -> FleetDecisionCard {
        let feed = decisionFeed
        let model = model
        let asker = decision.askerAgentID.flatMap { id in model.hostedAgents.first { $0.id == id } }
        return FleetDecisionCard(
            decision: decision,
            busy: feed.busy.contains(decision.id),
            failure: feed.failures[decision.id],
            openRevision: openRevision,
            answer: { text in await feed.answer(decision, text: text, model: model) },
            decide: { disposition, note in await feed.decide(decision, disposition, note: note, model: model) },
            openAsker: asker.map { hosted in { open(hosted) } },
            draft: feed.draft(for: decision.id)
        )
    }

    private var board: some View {
        let all = FleetBoard.filter(allWorks, host: hostFilter, project: projectFilter)
        let decisions = decisions
        let marks = marks
        let works = all.filter { showsReviewed || !marks.hides($0) }
        let hiddenCount = all.filter { marks.hides($0) }.count
        let selected = all.first { $0.id == selectedID }
        return GeometryReader { geometry in
            // Beside the board while all three lanes keep a readable width;
            // over the board in a narrower window.
            let floats = geometry.size.width < 1040
            ZStack(alignment: .trailing) {
                HStack(spacing: 0) {
                    lanes(works, decisions: decisions, reviewedCount: hiddenCount)
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
        .onAppear { lastLanes = Dictionary(allWorks.map { ($0.id, $0.lane) }, uniquingKeysWith: { first, _ in first }) }
        .onChange(of: allWorks.map { "\($0.id)|\($0.lane.rawValue)" }) { _ in noteMoves() }
        .onChange(of: allWorks.map(\.id)) { ids in
            // Only a live board says a unit is gone; a disconnect is not.
            guard model.isConnected, !ids.isEmpty else { return }
            var pruned = reviewed
            pruned.prune(keeping: Set(ids))
            if pruned != reviewed { reviewed = pruned; reviewed.save(to: .standard) }
            migrateMarksIfNeeded()
        }
        .onChange(of: decisionFeed.reviewedKeys != nil) { _ in migrateMarksIfNeeded() }
    }

    /// Marks the cards that changed lane so they can show where they came
    /// from, and forgets the mark after a few seconds.
    private func noteMoves() {
        let works = allWorks
        let moves = FleetBoard.laneMoves(previous: lastLanes, current: works)
        lastLanes = Dictionary(works.map { ($0.id, $0.lane) }, uniquingKeysWith: { first, _ in first })
        guard animatesMoves, !moves.isEmpty else { return }
        movedFrom.merge(moves) { _, new in new }
        Task { @MainActor in
            try? await Task.sleep(nanoseconds: 4_000_000_000)
            withAnimation(.easeOut(duration: 0.4)) {
                for (id, lane) in moves where movedFrom[id] == lane { movedFrom[id] = nil }
            }
        }
    }

    /// A draft for one of the unit's other waiting agents.
    private static func agentDraftKey(_ work: FleetWork, _ hosted: TendHostedAgent) -> String {
        "\(work.id)#\(hosted.id)"
    }

    private func inspector(_ work: FleetWork) -> some View {
        FleetWorkInspector(
            model: model,
            work: work,
            isReviewed: marks.hides(work),
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
                markReviewed(work)
                selectedID = nil
            },
            agentDraft: { draftBinding(Self.agentDraftKey(work, $0)) },
            submitAgent: { submit(for: Self.agentDraftKey(work, $0)) }
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
              let paneID = TendAgentAttentionCenter.paneIdentity(for: hosted),
              let current = model.executionSnapshot.watchPane(id: paneID),
              target.matches(current.agent) else {
            throw TendIPCError.server(String(localized: "The agent moved on. Open its pane to see where it is."))
        }
        if let blocker = TendNotificationActions.replyBlocker(current) { throw TendIPCError.server(blocker) }
        try await model.client.sendFleetPrompt(host: current.host, pane: current.pane, text: text)
        // The words went to the agent; tendd records them as the decision.
        let question = current.agent?.awaitingReply?.question
            ?? allWorks.first { ($0.agents + $0.helpers).contains { $0.id == hosted.id } }?.headline
        await FleetDecisionRecord.recordInstruction(
            client: model.client,
            target: model.homeReviewTarget(for: hosted),
            reply: target.kind,
            sessionID: target.sessionID,
            waitSince: target.stateEnteredAt,
            question: question,
            text: text
        )
    }

    @ViewBuilder
    private func lanes(_ works: [FleetWork], decisions: [FleetDecision], reviewedCount: Int) -> some View {
        Group {
            // A decision read that failed keeps the lanes, so its warning
            // shows instead of an ordinary empty Home.
            let problem = decisionFeed.problem == nil ? 0 : 1
            if FleetBoard.showsEmptyBoard(visible: works.count + decisions.count + problem, reviewed: reviewedCount) {
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
                            decisions: lane == .needsYou ? decisions : [],
                            decisionCard: { decisionCard($0) { revisionTarget = $0 } },
                            decisionProblem: lane == .needsYou ? decisionFeed.problem : nil,
                            selectedID: selectedID,
                            select: { id in selectedID = selectedID == id ? nil : id },
                            open: peek,
                            reviewedCount: lane == .finished ? reviewedCount : 0,
                            showsReviewed: $showsReviewed,
                            cardSpace: animatesMoves ? cardSpace : nil,
                            movedFrom: movedFrom
                        )
                    }
                }
                .animation(
                    animatesMoves ? .spring(response: 0.5, dampingFraction: 0.86) : nil,
                    value: works.map { "\($0.id)|\($0.lane.rawValue)" }
                )
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
    private func peek(_ hosted: TendHostedAgent) {
        quickLook = FleetQuickLookTarget(agentID: hosted.id)
    }

    /// An agent's pane when it has a live one, its agent editor otherwise.
    private func open(_ hosted: TendHostedAgent) {
        if let pane = TendAgentAttentionCenter.paneIdentity(for: hosted),
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
    /// The daemon's decisions, in Your turn only, above the Work cards.
    var decisions: [FleetDecision] = []
    var decisionCard: ((FleetDecision) -> FleetDecisionCard)?
    /// Why the decisions may be incomplete (a source tendd could not read).
    var decisionProblem: String?
    let selectedID: String?
    let select: (String) -> Void
    let open: (TendHostedAgent) -> Void
    var reviewedCount = 0
    @Binding var showsReviewed: Bool
    /// Set when cards should glide between lanes.
    var cardSpace: Namespace.ID?
    var movedFrom: [String: FleetLane] = [:]
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

    private var title: String { lane.title }

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
                    .foregroundStyle(lane == .needsYou && works.count + decisions.count > 0 ? Color.orange : Color.primary)
                Text("\(works.count + decisions.count)")
                    .font(.system(size: 12, weight: .semibold))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                Spacer(minLength: 4)
                Text(subtitle)
                    .font(TendType.meta)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
            }
            .padding(.horizontal, 12)
            .padding(.top, 10)
            .padding(.bottom, 8)
            ScrollView {
                LazyVStack(spacing: 8) {
                    if let decisionProblem {
                        Label(decisionProblem, systemImage: "exclamationmark.circle")
                            .font(TendType.meta)
                            .foregroundStyle(.orange)
                            .lineLimit(3)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.horizontal, 4)
                    }
                    if let decisionCard {
                        ForEach(decisions) { decision in
                            decisionCard(decision)
                        }
                    }
                    if works.isEmpty && decisions.isEmpty {
                        Text(empty)
                            .font(TendType.detail)
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
                                movedFrom: movedFrom[work.id]
                            )
                            .modifier(FleetCardMotion(id: work.id, space: cardSpace))
                        }
                    }
                    if emptyCount > 0 {
                        Button(showsEmpty
                            ? String(localized: "Hide empty sessions")
                            : String(localized: "\(emptyCount) empty sessions")) {
                            showsEmpty.toggle()
                        }
                        .buttonStyle(.tendGhost)
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
                        .buttonStyle(.tendGhost)
                        .controlSize(.small)
                        .padding(.top, 2)
                        .help("Sessions whose prompt hands over a tend request; who asked is not confirmed")
                    }
                    if olderCount > 0 {
                        Button(showsOlder ? String(localized: "Hide older") : String(localized: "\(olderCount) older")) {
                            showsOlder.toggle()
                        }
                        .buttonStyle(.tendGhost)
                        .controlSize(.small)
                        .padding(.top, 2)
                    }
                    if reviewedCount > 0 || (lane == .finished && showsReviewed) {
                        Button(showsReviewed ? String(localized: "Hide reviewed") : String(localized: "Show \(reviewedCount) reviewed")) {
                            showsReviewed.toggle()
                        }
                        .buttonStyle(.tendGhost)
                        .controlSize(.small)
                        .padding(.top, 2)
                    }
                }
                .padding(.horizontal, 8)
                .padding(.bottom, 10)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
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
    var movedFrom: FleetLane?
    @Environment(\.colorScheme) private var colorScheme

    private var laneTint: Color {
        switch work.lane {
        case .needsYou: .orange
        case .running: .blue
        case .finished: .green
        }
    }

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
                if let movedFrom {
                    Label(String(localized: "From \(movedFrom.title)"), systemImage: "arrow.turn.down.right")
                        .font(.system(size: 10, weight: .semibold))
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(laneTint.opacity(0.18), in: Capsule())
                        .foregroundStyle(laneTint)
                        .fixedSize()
                        .transition(.opacity)
                }
            }
            if work.lane != .finished || work.stages.count > 1 {
                FleetStageBar(work: work)
            }
            Text(work.headline)
                .font(TendType.body)
                .foregroundStyle(work.lane == .needsYou ? Color.primary : Color.secondary)
                .lineLimit(work.lane == .needsYou ? 3 : 2)
                .frame(maxWidth: .infinity, alignment: .leading)
            if work.handledLimit {
                Label("Rate limited · the limit policy will resume it", systemImage: "arrow.triangle.2.circlepath")
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
            }
            if work.lane == .needsYou, FleetBoard.waiting(work).count > 1 {
                Label(String(localized: "\(FleetBoard.waiting(work).count) agents wait on you"), systemImage: "person.2.badge.gearshape")
                    .font(TendType.meta)
                    .foregroundStyle(.orange)
            }
            HStack(spacing: 6) {
                if !work.agentKinds.isEmpty {
                    TendAgentLogoStack(kinds: work.agentKinds, size: 12)
                }
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
            .font(TendType.meta)
            .foregroundStyle(.tertiary)
        }
        .padding(12)
        .background(TendSurfacePalette.workspace(for: colorScheme), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .strokeBorder(isSelected ? Color.accentColor : TendTheme.border(colorScheme), lineWidth: isSelected ? 1.5 : 1)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .strokeBorder(laneTint, lineWidth: 2)
                .opacity(movedFrom == nil ? 0 : 0.9)
                .allowsHitTesting(false)
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
                .font(TendType.meta)
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
    let open: (TendHostedAgent) -> Void
    let openWork: (TendWorkIdentity) -> Void
    @Binding var draft: FleetReplyDraft
    let submit: () -> Void
    let approveLikeThis: () -> Void
    let markReviewed: () -> Void
    /// Drafts and sends for the unit's other waiting agents, kept by the
    /// board like the unit's own.
    let agentDraft: (TendHostedAgent) -> Binding<FleetReplyDraft>
    let submitAgent: (TendHostedAgent) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .top, spacing: 8) {
                VStack(alignment: .leading, spacing: 4) {
                    Text(work.title)
                        .font(.system(size: 16, weight: .semibold))
                        .fixedSize(horizontal: false, vertical: true)
                    Text([work.repo, work.hostAliases.joined(separator: ", ")].compactMap { $0 }.joined(separator: " · "))
                        .font(TendType.meta)
                        .foregroundStyle(.secondary)
                }
                Spacer(minLength: 4)
                Button(action: close) {
                    Image(systemName: "xmark")
                }
                .buttonStyle(.tendGhost)
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
                                    .font(TendType.meta)
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
            if work.lane == .needsYou, let focus = work.focus {
                FleetAskedContext(agent: focus.agent)
            }
            if work.handledLimit {
                Label("Rate limited · the limit policy will resume it", systemImage: "arrow.triangle.2.circlepath")
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
            }
            if work.lane == .needsYou, let focus = work.focus, focus.pane != nil,
               ["waiting_input", "waiting_choice"].contains(focus.agent.state) {
                // The screen decides: option buttons for a choice, the answer
                // field for a question that takes words.
                FleetChoicePanel(model: model, hosted: focus) {
                    if FleetBoard.replyTarget(for: work) != nil || !draft.isIdle {
                        FleetReplyField(work: work, draft: $draft, submit: submit)
                    }
                }
                .id(focus.id)
            } else if FleetBoard.replyTarget(for: work) != nil || !draft.isIdle {
                FleetReplyField(work: work, draft: $draft, submit: submit)
            }
            let others = FleetBoard.waiting(work).filter { $0.id != work.focus?.id }
            if work.lane == .needsYou, !others.isEmpty {
                // One unit, several agents asking: each answers on its own.
                VStack(alignment: .leading, spacing: 10) {
                    Text("Also waiting on you")
                        .font(.system(size: 11, weight: .bold))
                        .foregroundStyle(.orange)
                    ForEach(others) { hosted in
                        FleetWaitingAgent(model: model, hosted: hosted, open: open) {
                            FleetReplyField(
                                work: work,
                                draft: agentDraft(hosted),
                                submit: { submitAgent(hosted) },
                                agent: hosted
                            )
                        }
                    }
                }
                .padding(.top, 4)
            }
            HStack(spacing: 8) {
                if let focus = work.focus {
                    Button(paneButtonTitle) { open(focus) }
                        .buttonStyle(FleetBoard.reply(for: work) == nil ? .tendPrimary : .tendSecondary)
                }
                if case .work(let identity) = work.source {
                    Button("Work Details") { openWork(identity) }
                        .buttonStyle(.tendSecondary)
                }
                if FleetBoard.choosesInPane(work), work.focus.map({ FleetApprovalRules.supportedKinds.contains($0.agent.kind) }) == true {
                    Button("Always Approve This Command…", action: approveLikeThis)
                        .buttonStyle(.tendGhost)
                        .help("Write a rule so Tend approves this command on its own")
                }
                if work.lane == .finished && !isReviewed {
                    Spacer(minLength: 0)
                    Button(action: markReviewed) {
                        Label("Looks Good", systemImage: "checkmark")
                    }
                    .buttonStyle(.tendGhost)
                    .help("Set this aside until it changes again")
                }
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(tint.opacity(0.1), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 9, style: .continuous).strokeBorder(tint.opacity(0.45)))
    }

    private func agentRow(_ hosted: TendHostedAgent) -> some View {
        HStack(spacing: 8) {
            AgentStatusTag(status: hosted.agent.status, showsLabel: false)
            TendAgentLogo(kind: hosted.agent.kind, size: 18)
            VStack(alignment: .leading, spacing: 1) {
                Text(FleetBoard.owner(hosted))
                    .font(TendType.body.weight(.medium))
                Text(hosted.agent.status.label())
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 4)
            Button("Open") { open(hosted) }
                .buttonStyle(.tendSecondary)
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
    /// The agent the field answers when it is not the unit's reply target:
    /// another agent in the unit waiting for words.
    var agent: TendHostedAgent?

    private var currentTarget: FleetReplyTarget? {
        guard let agent else { return FleetBoard.replyTarget(for: work) }
        let answers: Bool
        if case .asked = agent.agent.status { answers = true } else { answers = agent.agent.state == "waiting_input" }
        return answers ? FleetReplyTarget(kind: .answer, agent: agent) : nil
    }

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
                        .font(TendType.meta)
                        .foregroundStyle(.orange)
                        .fixedSize(horizontal: false, vertical: true)
                    HStack(spacing: 8) {
                        if let current = currentTarget {
                            Button(String(localized: "Use for \(current.owner)")) {
                                draft.retarget(current)
                            }
                            .buttonStyle(.tendSecondary)
                            .controlSize(.small)
                        }
                        Button("Discard", role: .destructive) {
                            draft.discard()
                        }
                        .buttonStyle(.tendGhost)
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
                    .buttonStyle(.tendPrimary)
                    .disabled(draft.isSending || isStale || draft.target == nil || !draft.hasText)
            }
            if draft.isSending {
                ProgressView().controlSize(.small)
            } else if let failure = draft.failure {
                Label(failure, systemImage: "exclamationmark.circle.fill")
                    .font(TendType.meta)
                    .foregroundStyle(.red)
            } else if draft.sent {
                Label("Sent", systemImage: "checkmark.circle.fill")
                    .font(TendType.meta)
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
                HStack(spacing: 4) {
                    if let kind = stage.kind {
                        TendAgentLogo(kind: kind, size: 12)
                    }
                    Text(stage.owner)
                }
                .font(TendType.meta)
                .foregroundStyle(.secondary)
            }
            .padding(.bottom, isLast ? 0 : 10)
            Spacer(minLength: 4)
            Text(stage.state.label)
                .font(TendType.meta)
                .foregroundStyle(.secondary)
        }
        .fixedSize(horizontal: false, vertical: true)
    }
}

// MARK: - Hosts and empty board

private struct FleetHostsPopover: View {
    let hosts: [TendFleetHost]
    let select: (TendFleetHost) -> Void
    var showLeftovers: () -> Void = {}

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
                            .lineLimit(1)
                        Spacer(minLength: 12)
                        Text(fleetHostStateLabel(host.state))
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .fixedSize(horizontal: true, vertical: false)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
            Divider()
            Button("Leftovers…", action: showLeftovers)
                .buttonStyle(.tendGhost)
                .help("Worktrees tend created on each host, and what keeps them")
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
                    .buttonStyle(.tendPrimary)
                Button("New Agent", action: newAgent)
                    .buttonStyle(.tendSecondary)
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
    let hosts: [TendFleetHostIdentity]

    var body: some View {
        HStack(spacing: 6) {
            ForEach(hosts, id: \.alias) { host in
                let icon = TendHostIcons.icon(for: host.alias, local: host.local)
                HStack(spacing: 3) {
                    TendGlyphFace(value: icon, gradient: TendMenuBarAgentAppearance(identity: host.alias, symbol: icon, provider: "").gradient, size: 14, glyphSize: 7)
                    Text(verbatim: host.alias)
                        .lineLimit(1)
                }
                .help(host.local ? String(localized: "\(host.alias) (this Mac)") : host.alias)
            }
        }
    }
}

/// Lets a card glide from its old lane to its new one: the same id in one
/// namespace across the lanes. Off when the setting is.
private struct FleetCardMotion: ViewModifier {
    let id: String
    let space: Namespace.ID?

    func body(content: Content) -> some View {
        if let space {
            content
                .matchedGeometryEffect(id: id, in: space)
                .transition(.opacity.combined(with: .scale(scale: 0.96)))
        } else {
            content
        }
    }
}
