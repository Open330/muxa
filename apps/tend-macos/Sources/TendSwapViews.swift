import SwiftUI

// MARK: - Sheet host

/// Presents the swap sheet wherever a surface asks for one
/// (`model.swaps.present`). Applied once, at the workbench root.
struct TendSwapSheetHost: ViewModifier {
    @ObservedObject var model: AppModel
    @ObservedObject var swaps: TendSwapCenter

    init(model: AppModel) {
        self.model = model
        swaps = model.swaps
    }

    func body(content: Content) -> some View {
        content.sheet(item: $swaps.sheet) { target in
            TendSwapSheet(model: model, swaps: swaps, target: target)
        }
    }
}

extension View {
    func tendSwapSheet(model: AppModel) -> some View {
        modifier(TendSwapSheetHost(model: model))
    }
}

// MARK: - Sheet

/// Switch account and resume: pick the account, see exactly what will happen
/// to each agent, then start it as a daemon job the app follows.
struct TendSwapSheet: View {
    @Environment(\.dismiss) private var dismiss
    @ObservedObject var model: AppModel
    @ObservedObject var swaps: TendSwapCenter
    let target: TendSwapSheetTarget

    @State private var accountProviders: [TendProviderCandidates] = []
    @State private var isReadingAccounts = false
    @State private var accountsError: String?
    @State private var choice: AccountChoice = .auto
    @State private var typedAccount = ""
    @State private var followUp = ""
    @State private var sendFollowUp = true
    @State private var force = false
    @State private var plan: [TendSwapOutcome]?
    @State private var planError: String?
    @State private var isPlanning = false
    @State private var isStarting = false
    @State private var startError: String?

    enum AccountChoice: Hashable {
        case auto
        case spread
        case account(String)
        case typed
    }

    private var isLocal: Bool { model.isLocalHost(target.host) }
    private var isMany: Bool {
        if case .agents(let ids) = target.scope { return ids.count > 1 }
        return true
    }

    /// aas spells providers `claude` and `codex`; agents report
    /// `claude_code` and `codex`.
    private var provider: String? {
        switch target.agentKind {
        case "claude_code"?: "claude"
        case let kind?: kind
        case nil: nil
        }
    }

    private var offeredAccounts: [TendAccountCandidate] {
        accountProviders.flatMap(\.candidates).filter { (provider == nil || $0.provider == provider) && $0.eligible }
    }

    private var request: TendSwapRequest {
        var request = TendSwapRequest(scope: target.scope)
        switch choice {
        case .auto: break
        case .spread: request.spread = true
        case .account(let name): request.account = name
        case .typed: request.account = typedAccount
        }
        request.force = force
        request.noFollowUp = !sendFollowUp
        request.followUp = sendFollowUp ? followUp : nil
        return request
    }

    private var canStart: Bool {
        guard !isStarting, !isPlanning, planError == nil, let plan else { return false }
        // The service said it takes no job: Start would only be refused.
        if isLocal, let health = swaps.health, !health.isReady { return false }
        if case .typed = choice,
           typedAccount.trimmingCharacters(in: .whitespaces).isEmpty
            || typedAccount.contains(where: \.isWhitespace) {
            return false
        }
        return plan.contains { $0.status == .planned }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            Form {
                if isLocal, let health = model.swaps.health, !health.isReady {
                    Section {
                        TendSwapHealthRow(health: health)
                    }
                }
                accountSection
                followUpSection
                if target.interruptsWork || isMany {
                    Section {
                        Toggle("Also stop agents that are mid-turn", isOn: $force)
                        Text("Without this, an agent that is working is left alone. With it, its current turn is cut off and, when follow-ups are on, carried on after the relaunch.")
                            .font(TendType.detail)
                            .foregroundStyle(.secondary)
                    }
                }
                planSection
            }
            .formStyle(.grouped)
            Divider()
            footer
        }
        .frame(width: 560)
        .frame(minHeight: 520)
        .task {
            if target.interruptsWork { force = true }
            if isLocal {
                await model.swaps.refreshHealth()
                await readAccounts()
            }
        }
        .task(id: request) {
            // Debounce typing in the account and follow-up fields.
            try? await Task.sleep(for: .milliseconds(350))
            guard !Task.isCancelled else { return }
            startError = nil
            await loadPlan()
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Label("Switch Account and Resume", systemImage: "arrow.triangle.2.circlepath")
                .font(.headline)
            Text(subtitle)
                .font(TendType.detail)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(16)
    }

    private var subtitle: String {
        let host = target.host.map { String(localized: " on \($0)") } ?? ""
        return String(localized: "Stops \(target.label)\(host), relaunches the same conversation on another account, and carries the interrupted turn on. Nothing is lost: the conversation resumes where it stopped.")
    }

    @ViewBuilder
    private var accountSection: some View {
        Section("Move to") {
            Picker("Account", selection: $choice) {
                Text("Best account with quota left").tag(AccountChoice.auto)
                if isMany {
                    Text("Spread across accounts with quota").tag(AccountChoice.spread)
                }
                if !offeredAccounts.isEmpty {
                    Divider()
                    ForEach(offeredAccounts) { account in
                        Text(accountLabel(account)).tag(AccountChoice.account(account.name))
                    }
                }
                Divider()
                Text("Another account…").tag(AccountChoice.typed)
            }
            .pickerStyle(.menu)
            if case .typed = choice {
                TextField("Account", text: $typedAccount, prompt: Text(verbatim: "work@claude"))
                    .font(.callout.monospaced())
            }
            if isLocal, isReadingAccounts, offeredAccounts.isEmpty {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text("Reading accounts…").font(TendType.detail).foregroundStyle(.secondary)
                }
            } else if let accountsError {
                Text(accountsError).font(TendType.detail).foregroundStyle(.secondary)
            } else if !isLocal {
                Text("Accounts are read on \(target.host ?? ""). Leave this on Best account to let Tend there choose.")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
            }
        }
    }

    @MainActor
    private func readAccounts() async {
        isReadingAccounts = true
        defer { isReadingAccounts = false }
        do {
            accountProviders = try await model.client.swapAccounts()
            accountsError = accountProviders
                .filter { provider == nil || $0.provider == provider }
                .compactMap(\.reason).joined(separator: "\n")
            if accountsError?.isEmpty == true { accountsError = nil }
        } catch {
            accountsError = error.localizedDescription
        }
    }

    private func accountLabel(_ account: TendAccountCandidate) -> String {
        var parts = [account.name]
        if let remaining = account.remainingPct {
            parts.append(String(localized: "\(Int(remaining.rounded()))% left"))
        }
        if account.active { parts.append(String(localized: "default")) }
        return parts.joined(separator: " · ")
    }

    @ViewBuilder
    private var followUpSection: some View {
        Section("After the relaunch") {
            Toggle("Send a follow-up so the agent carries on", isOn: $sendFollowUp)
            if sendFollowUp {
                TextField("Follow-up", text: $followUp, prompt: Text("The [swap] follow-up from config.toml"))
                    .font(.callout.monospaced())
                Text("Typed only into a relaunch confirmed to be on the same conversation, and only for agents whose turn was cut off.")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
            }
        }
    }

    @ViewBuilder
    private var planSection: some View {
        Section {
            if let planError {
                Label(planError, systemImage: "exclamationmark.triangle.fill")
                    .font(TendType.detail)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            } else if let plan {
                if plan.isEmpty {
                    Text("No agent matches. Agents without a known account, or that are not Claude Code or Codex, cannot be switched.")
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                }
                ForEach(Array(plan.enumerated()), id: \.offset) { _, outcome in
                    TendSwapOutcomeRow(outcome: outcome, agent: agent(for: outcome))
                }
            } else {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text("Working out the plan…").font(TendType.detail).foregroundStyle(.secondary)
                }
            }
        } header: {
            HStack {
                Text("What will happen")
                Spacer()
                if isPlanning, plan != nil { ProgressView().controlSize(.mini) }
            }
        }
    }

    private var footer: some View {
        HStack {
            if let startError {
                Label(startError, systemImage: "exclamationmark.triangle.fill")
                    .font(TendType.detail)
                    .foregroundStyle(.red)
                    .lineLimit(3)
            }
            Spacer()
            Button("Cancel") { dismiss() }
                .keyboardShortcut(.cancelAction)
            Button {
                Task { await start() }
            } label: {
                if isStarting {
                    ProgressView().controlSize(.small)
                } else {
                    Text(startTitle)
                }
            }
            .keyboardShortcut(.defaultAction)
            .disabled(!canStart)
        }
        .padding(16)
    }

    private var startTitle: String {
        let count = plan?.filter { $0.status == .planned }.count ?? 0
        return count > 1
            ? String(localized: "Switch \(count) Agents")
            : String(localized: "Switch and Resume")
    }

    private func agent(for outcome: TendSwapOutcome) -> TendHostedAgent? {
        model.hostedAgents.first { hosted in
            hosted.agent.agentSessionID == outcome.agentSessionID
                && (target.host == nil ? hosted.host.local : hosted.host.alias == target.host)
        }
    }

    private func loadPlan() async {
        let asked = request
        isPlanning = true
        defer { isPlanning = false }
        let result: Result<[TendSwapOutcome], Error>
        do {
            result = .success(try await swaps.plan(asked, host: target.host))
        } catch {
            result = .failure(error)
        }
        // A plan for what the fields said before the last edit is not this
        // request's plan: the next one is already on its way.
        guard !Task.isCancelled, asked == request else { return }
        switch result {
        case .success(let outcomes):
            plan = outcomes
            planError = nil
        case .failure(let error):
            plan = nil
            planError = error.localizedDescription
        }
    }

    private func start() async {
        startError = nil
        isStarting = true
        defer { isStarting = false }
        do {
            try await swaps.start(request, host: target.host, title: activityTitle)
            dismiss()
        } catch {
            startError = error.localizedDescription
        }
    }

    private var activityTitle: String {
        switch choice {
        case .account(let name): "\(target.label) → \(name)"
        case .typed: "\(target.label) → \(typedAccount)"
        case .spread: String(localized: "\(target.label) → spread")
        case .auto: String(localized: "\(target.label) → best account")
        }
    }
}

// MARK: - Rows

/// One agent in a swap plan or job: who, from which account to which, and
/// what will (or did) happen to it.
struct TendSwapOutcomeRow: View {
    let outcome: TendSwapOutcome
    let agent: TendHostedAgent?

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: symbol)
                .foregroundStyle(tint)
                .frame(width: 16)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(name).font(TendType.body.weight(.medium)).lineLimit(1)
                    if let pane = outcome.pane {
                        Text(verbatim: pane).font(TendType.meta.monospaced()).foregroundStyle(.secondary)
                    }
                }
                if outcome.from != nil || outcome.to != nil {
                    HStack(spacing: 4) {
                        Text(verbatim: outcome.from ?? String(localized: "no account"))
                        Image(systemName: "arrow.right").font(.system(size: 9))
                        Text(verbatim: outcome.to ?? "—").fontWeight(.semibold)
                    }
                    .font(TendType.detail.monospaced())
                }
                if let detail {
                    Text(detail)
                        .font(TendType.meta)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 2)
    }

    private var name: String {
        if let agent { return agent.displayTitle }
        return outcome.agentSessionID.isEmpty ? (outcome.pane ?? "?") : String(outcome.agentSessionID.prefix(8))
    }

    private var detail: String? {
        var parts: [String] = []
        if !outcome.note.isEmpty { parts.append(outcome.note) }
        if outcome.status == .planned || outcome.status == .swapped {
            parts.append(outcome.sendsFollowUp
                ? String(localized: "Resumes and carries on")
                : String(localized: "Resumes; no follow-up"))
        }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    private var symbol: String {
        switch outcome.status {
        case .planned: "arrow.triangle.2.circlepath"
        case .swapped: "checkmark.circle.fill"
        case .skipped: "minus.circle"
        case .failed: "exclamationmark.triangle.fill"
        case .other: "circle"
        }
    }

    private var tint: Color {
        switch outcome.status {
        case .planned: .accentColor
        case .swapped: .green
        case .skipped, .other: .secondary
        case .failed: .red
        }
    }
}

// MARK: - Activity

/// Status-bar item while swaps run or need a look; the popover follows them.
struct TendSwapStatusItem: View {
    @ObservedObject var swaps: TendSwapCenter
    let openPane: (TendWatchPaneIdentity) -> Void
    @State private var isShowing = false

    var body: some View {
        if swaps.hasActivity {
            Button { isShowing.toggle() } label: {
                HStack(spacing: 4) {
                    if !swaps.running.isEmpty {
                        ProgressView().controlSize(.mini)
                        Text("Switching \(swaps.running.count)")
                    } else if swaps.needsLook {
                        Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                        Text("Swap needs a look")
                    } else {
                        Image(systemName: "arrow.triangle.2.circlepath")
                        Text("Swaps")
                    }
                }
            }
            .buttonStyle(.plain)
            .help("Account switches started from Tend or by automation")
            .popover(isPresented: $isShowing, arrowEdge: .top) {
                TendSwapActivityList(swaps: swaps)
                    .frame(width: 420)
                    .frame(maxHeight: 480)
            }
        }
    }
}

struct TendSwapActivityList: View {
    @ObservedObject var swaps: TendSwapCenter

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text("Account Switches").font(.headline)
                Spacer()
                if swaps.activities.contains(where: { !$0.isRunning }) {
                    Button("Clear Finished") { swaps.dismissEnded() }
                        .buttonStyle(.tendGhost)
                }
            }
            .padding(12)
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    ForEach(swaps.activities) { activity in
                        TendSwapActivityRow(swaps: swaps, activity: activity)
                        Divider()
                    }
                }
                .padding(12)
            }
        }
    }
}

struct TendSwapActivityRow: View {
    @ObservedObject var swaps: TendSwapCenter
    let activity: TendSwapActivity

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                stateIcon
                Text(activity.title).font(TendType.body.weight(.medium)).lineLimit(1)
                Spacer()
                if let host = activity.host {
                    Text(verbatim: host).font(TendType.meta).foregroundStyle(.secondary)
                }
                Text(activity.startedAt, style: .time).font(TendType.meta).foregroundStyle(.secondary)
            }
            Text(stateText).font(TendType.detail).foregroundStyle(.secondary)
            if let error = activity.error {
                Text(error)
                    .font(TendType.detail)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            } else if let statusError = activity.statusError {
                Text(activity.lostTrack
                    ? "Tend stopped checking: \(statusError). The switch may still be running on its host; `tend swap --status=\(activity.requestKey)` there shows it."
                    : "Couldn't read its progress: \(statusError). Trying again…")
                    .font(TendType.meta)
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let job = activity.job {
                ForEach(job.entries) { entry in
                    entryLine(entry)
                }
                if let fault = job.fault ?? job.serviceFault {
                    Text(fault).font(TendType.meta).foregroundStyle(.orange)
                }
            }
            HStack {
                if activity.isRunning, activity.job != nil, activity.job?.cancelRequested != true {
                    Button("Stop After Current") { Task { await swaps.cancel(activity) } }
                        .buttonStyle(.tendSecondary)
                        .help("Starts no further agent; the one being switched finishes")
                }
                if !activity.isRunning, (activity.job?.failedCount ?? 0) > 0 {
                    Button("Retry Failed") { Task { await swaps.retry(activity) } }
                        .buttonStyle(.tendSecondary)
                        .help("A new switch for the agents that failed: ones that failed before their relaunch are switched again, ones already relaunched are only checked")
                }
            }
        }
        .onAppear { if activity.needsLook { swaps.acknowledge(activity) } }
    }

    @ViewBuilder
    private var stateIcon: some View {
        if activity.isRunning {
            ProgressView().controlSize(.mini)
        } else if activity.needsLook {
            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
        } else {
            Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        }
    }

    private var stateText: String {
        guard let job = activity.job else {
            if activity.error != nil { return String(localized: "Did not start") }
            if activity.lostTrack { return String(localized: "Progress unknown") }
            return String(localized: "Starting…")
        }
        if activity.lostTrack { return String(localized: "Progress unknown") }
        if job.isRunning {
            return job.cancelRequested
                ? String(localized: "Stopping after the current agent…")
                : String(localized: "Switching \(job.swappedCount) of \(job.entries.count)…")
        }
        if job.failedCount > 0 {
            return String(localized: "\(job.swappedCount) switched, \(job.failedCount) failed")
        }
        return String(localized: "\(job.swappedCount) switched")
    }

    private func entryLine(_ entry: TendSwapJobEntry) -> some View {
        HStack(alignment: .top, spacing: 6) {
            Image(systemName: entrySymbol(entry.phase))
                .foregroundStyle(entryTint(entry.phase))
                .font(.system(size: 10))
                .frame(width: 12)
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 4) {
                    Text(verbatim: entry.pane ?? entry.outcome?.pane ?? "?").font(TendType.meta.monospaced())
                    if let from = entry.outcome?.from, let to = entry.outcome?.to {
                        Text(verbatim: "\(from) → \(to)").font(TendType.meta.monospaced())
                    }
                }
                if let text = entryDetail(entry) {
                    Text(text)
                        .font(TendType.meta)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }

    private func entryDetail(_ entry: TendSwapJobEntry) -> String? {
        var parts: [String] = []
        if let reason = entry.reason, entry.phase == .failed || entry.phase == .skipped {
            parts.append(reason)
        } else if let note = entry.outcome?.note, !note.isEmpty, entry.phase != .swapped {
            parts.append(note)
        }
        if let followUp = entry.followUp {
            switch (followUp.submission, followUp.evidence) {
            case ("delivered", "prompt_hook"?), ("delivered", "screen_activity"?):
                parts.append(String(localized: "Follow-up sent; the agent carried on"))
            case ("delivered", "none_seen"?):
                parts.append(String(localized: "Follow-up sent; no activity seen yet"))
            case ("delivered", _):
                parts.append(String(localized: "Follow-up sent"))
            case ("refused", _):
                parts.append(String(localized: "Follow-up refused"))
            default:
                parts.append(String(localized: "Follow-up unconfirmed"))
            }
        }
        if entry.uncertainEffects > 0 {
            parts.append(String(localized: "Some effects were not confirmed; check the pane"))
        }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    private func entrySymbol(_ phase: TendSwapEntryPhase) -> String {
        switch phase {
        case .pending: "circle.dotted"
        case .swapped, .done: "checkmark.circle.fill"
        case .skipped, .cancelled: "minus.circle"
        case .failed: "exclamationmark.triangle.fill"
        }
    }

    private func entryTint(_ phase: TendSwapEntryPhase) -> Color {
        switch phase {
        case .pending: .secondary
        case .swapped, .done: .green
        case .skipped, .cancelled: .secondary
        case .failed: .red
        }
    }
}

/// The swap service's state with its reason: in the swap sheet when it
/// cannot take a job, and always in Settings › Automations.
struct TendSwapHealthRow: View {
    let health: TendSwapHealth

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: health.isReady ? "checkmark.circle.fill" : "exclamationmark.triangle.fill")
                .foregroundStyle(health.isReady ? Color.green : Color.orange)
            VStack(alignment: .leading, spacing: 2) {
                Text(health.title)
                    .font(TendType.detail.weight(.medium))
                if let detail = health.detail, !detail.isEmpty {
                    Text(detail)
                        .font(TendType.meta)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if health.jobsRunning > 0 {
                    Text(health.jobsRunning == 1 ? "1 switch running" : "\(health.jobsRunning) switches running")
                        .font(TendType.meta)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }
}

/// Settings › Automations: the local swap service's health, checked when
/// shown, so "why didn't it switch?" has an answer in the app.
struct TendSwapHealthSection: View {
    @ObservedObject var swaps: TendSwapCenter

    var body: some View {
        Group {
            if let health = swaps.health {
                TendSwapHealthRow(health: health)
            } else if let error = swaps.healthError {
                Label(String(localized: "Couldn't check account switching: \(error)"), systemImage: "questionmark.circle")
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
            }
        }
        .task { await swaps.refreshHealth() }
    }
}
