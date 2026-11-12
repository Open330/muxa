import AppKit
import SwiftUI

/// How much of the run log the Automations tab shows before "Show All…".
enum AutomationRunLogPreview {
    static let limit = 10

    /// The newest `limit` entries, and how many more the full log holds.
    static func split<Entry>(_ entries: [Entry], limit: Int = limit) -> (shown: [Entry], hidden: Int) {
        let bounded = max(0, limit)
        return (Array(entries.prefix(bounded)), max(0, entries.count - bounded))
    }
}

/// Settings › Automations: the rules tendd fires on its own, the switches
/// that gate them, and what they have done.
struct AutomationSettingsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AutomationStore
    @ObservedObject var configStore: TendConfigStore
    @State private var editorTarget: AutomationRuleEditorTarget?
    @State private var removalTarget: TendAutomationRule?
    @State private var showsFullLog = false

    var body: some View {
        Form {
            if store.hasLoaded, !store.isSupported, model.isConnected {
                unsupportedSection
            } else {
                engineSection
                rulesSection
                logSection
            }
            statusSection
        }
        .formStyle(.grouped)
        .task(id: model.isConnected) {
            await store.reload(model: model)
            await configStore.load(model: model)
        }
        .sheet(item: $editorTarget) { target in
            AutomationRuleEditor(
                draft: target.draft,
                existingNames: store.existingNames,
                model: model,
                store: store
            )
        }
        .sheet(isPresented: testReportBinding) {
            if let report = store.testReport {
                AutomationTestSheet(report: report)
            }
        }
        .sheet(isPresented: $showsFullLog) {
            AutomationFullLogSheet(model: model, store: store)
        }
        .alert("Remove this rule?", isPresented: removalAlertBinding, presenting: removalTarget) { rule in
            Button("Cancel", role: .cancel) { removalTarget = nil }
            Button("Remove", role: .destructive) {
                let target = rule
                removalTarget = nil
                Task { await store.remove(target, model: model) }
            }
        } message: { rule in
            Text("\(rule.name) will stop firing and is removed from tend configuration.")
        }
    }

    private var removalAlertBinding: Binding<Bool> {
        Binding(get: { removalTarget != nil }, set: { if !$0 { removalTarget = nil } })
    }

    private var testReportBinding: Binding<Bool> {
        Binding(get: { store.testReport != nil }, set: { if !$0 { store.clearTestReport() } })
    }

    // MARK: Capability fallback

    private var unsupportedSection: some View {
        Section {
            Label("This tendd has no automation engine", systemImage: "wand.and.rays")
                .font(.headline)
            TendSettingsCaption("Update tend and restart tendd to write rules from here. Until then, rules can be written by hand in the configuration file and read with tend automation list.")
            TendSettingsCaption("The Advanced tab opens that file.")
        }
    }

    // MARK: Engine switch and pause

    private var isPaused: Bool {
        store.snapshot.pausedUntil != nil && store.snapshot.isPaused()
    }

    private var engineSection: some View {
        Section {
            HStack(spacing: 10) {
                Toggle("Automation engine", isOn: masterBinding)
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .disabled(!store.isSupported || !store.hasLoaded || store.isMutating)
                Text("Automation engine")
                    .font(.headline)
                    .accessibilityHidden(true)
                engineStatus
                Spacer(minLength: 8)
                if store.isLoading || store.isMutating || configStore.isSaving {
                    ProgressView().controlSize(.small)
                }
                Menu {
                    Button("Pause for 1 Hour") {
                        Task { await store.pause(until: Date().addingTimeInterval(3600), model: model) }
                    }
                    Button("Pause Until Tomorrow") {
                        Task { await store.pause(until: TendAutomationTime.tomorrowMorning(), model: model) }
                    }
                    Divider()
                    Button("Resume") {
                        Task { await store.pause(until: nil, model: model) }
                    }
                    .disabled(!isPaused)
                } label: {
                    Text("Pause")
                }
                .fixedSize()
                .disabled(store.isMutating || !store.isSupported)
                safetyInfo
            }
            TendSettingsCaption("Off stops every rule and is remembered in tend configuration. Pause stops them until a time you choose.")
        }
    }

    @ViewBuilder
    private var engineStatus: some View {
        if let until = store.snapshot.pausedUntil, store.snapshot.isPaused() {
            Label("Paused until \(until.formatted(date: .abbreviated, time: .shortened))", systemImage: "pause.circle.fill")
                .font(TendType.detail)
                .foregroundStyle(.orange)
                .lineLimit(1)
        } else {
            Label("Running", systemImage: "play.circle.fill")
                .font(TendType.detail)
                .foregroundStyle(.green)
        }
    }

    /// The engine's own switch: the daemon applies it live and writes it
    /// back to config.toml, so it needs no restart and no config editor.
    private var masterBinding: Binding<Bool> {
        Binding(
            get: { store.masterEnabled },
            set: { newValue in
                Task { await store.setMasterEnabled(newValue, model: model) }
            }
        )
    }

    // MARK: Safety

    /// What a rule may do, behind ⓘ: worth reading once and re-opening
    /// later, not a warning poster to scroll past on every visit. The icon
    /// turns orange only once a rule that types into a pane actually exists.
    private var safetyInfo: some View {
        TendSettingsInfoButton(title: "What bounds this", tint: hasTypingRule ? .orange : .secondary) {
            VStack(alignment: .leading, spacing: 3) {
                Text("A rule types into a live agent's pane, exactly as a person would.")
                    .foregroundStyle(.primary)
                    .padding(.bottom, 3)
                Text("• The master switch and the pause above stop every rule.")
                Text("• Each rule has its own switch.")
                Text("• Firings per hour and the cooldown cap how often one rule may act on one pane.")
                Text("• A ceiling of \(store.snapshot.globalMaxPerHour) firings an hour bounds every rule together.")
                Text("• The condition is re-checked at fire time; a recovered agent is left alone.")
                Text("• One rate-limit episode fires a rule once.")
                Text("• Every firing and every skip is written to the run log below.")
                Text("Test on a rule shows what it would do right now without firing anything.")
                    .padding(.top, 3)
            }
        }
    }

    /// `send_prompt` is the action that reaches the keyboard, so it is the
    /// one that earns the accent.
    private var hasTypingRule: Bool {
        store.rules.contains { $0.action == .sendPrompt }
    }

    // MARK: Rules

    private var rulesSection: some View {
        Section {
            if store.rules.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    Text("No rules yet")
                    TendSettingsCaption("Nothing fires until a rule is added and switched on. Choose a limit policy in Accounts to handle capped agents without writing one.")
                }
            } else {
                ForEach(store.rules) { rule in
                    AutomationRuleRow(
                        model: model,
                        store: store,
                        rule: rule,
                        onEdit: {
                            editorTarget = AutomationRuleEditorTarget(
                                TendAutomationRuleDraft.draft(editing: rule)
                            )
                        },
                        onRemove: { removalTarget = rule }
                    )
                }
            }
        } header: {
            HStack {
                Text("Rules")
                Spacer()
                Button {
                    editorTarget = AutomationRuleEditorTarget(TendAutomationRuleDraft())
                } label: {
                    Label("Add Rule…", systemImage: "plus")
                }
                .controlSize(.small)
                .disabled(!store.isSupported)
            }
        }
    }

    // MARK: Run log

    private var logSection: some View {
        let preview = AutomationRunLogPreview.split(store.log)
        return Section {
            if store.log.isEmpty {
                TendSettingsCaption("Nothing has fired yet. Every firing and every skipped firing lands here, newest first.")
            } else {
                ForEach(Array(preview.shown.enumerated()), id: \.offset) { _, entry in
                    AutomationLogRow(entry: entry, model: model)
                }
                if preview.hidden > 0 {
                    Button("Show All \(store.log.count) Entries…") { showsFullLog = true }
                        .buttonStyle(.link)
                }
            }
        } header: {
            HStack {
                Text("Run log")
                Spacer()
                Button {
                    Task { await store.reload(model: model) }
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                .controlSize(.small)
                .disabled(store.isLoading || !store.isSupported)
            }
        }
    }

    @ViewBuilder
    private var statusSection: some View {
        if store.loadError != nil || store.actionError != nil || configStore.saveError != nil {
            Section {
                if let error = store.loadError {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(TendType.detail)
                        .foregroundStyle(.orange)
                        .textSelection(.enabled)
                }
                if let error = store.actionError {
                    Label(error, systemImage: "xmark.octagon.fill")
                        .font(TendType.detail)
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                }
                if let error = configStore.saveError {
                    Label(error, systemImage: "xmark.octagon.fill")
                        .font(TendType.detail)
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                }
            }
        }
    }
}

/// Every entry of the run log, for when the tab's last ten are not enough.
private struct AutomationFullLogSheet: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AutomationStore
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section("Run log") {
                    ForEach(Array(store.log.enumerated()), id: \.offset) { _, entry in
                        AutomationLogRow(entry: entry, model: model)
                    }
                }
            }
            .formStyle(.grouped)
            Divider()
            HStack {
                Text("\(store.log.count) entries, newest first")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
            .padding(14)
        }
        .frame(minWidth: 620, idealWidth: 720, minHeight: 420, idealHeight: 620)
    }
}

/// Identity for the editor sheet; a draft alone is not `Identifiable`.
struct AutomationRuleEditorTarget: Identifiable {
    let id: String
    let draft: TendAutomationRuleDraft

    init(_ draft: TendAutomationRuleDraft) {
        self.draft = draft
        id = draft.originalName ?? "new:\(UUID().uuidString)"
    }
}

// MARK: - Rows

private struct AutomationRuleRow: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AutomationStore
    let rule: TendAutomationRule
    let onEdit: () -> Void
    let onRemove: () -> Void

    /// Built here rather than handed in: an inline setter stays main-actor
    /// isolated, which is what makes the `Binding` `Sendable` while it
    /// captures the store and the model.
    private var enabledBinding: Binding<Bool> {
        Binding(
            get: { rule.enabled },
            set: { enabled in
                Task { await store.setEnabled(rule, enabled: enabled, model: model) }
            }
        )
    }

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Toggle(isOn: enabledBinding) {
                VStack(alignment: .leading, spacing: 5) {
                    HStack(spacing: 8) {
                        Text(verbatim: rule.name)
                            .font(.headline)
                        AutomationBadge(text: automationEventTitle(rule.on), symbol: "bolt.horizontal")
                        AutomationBadge(text: automationActionTitle(rule.action), symbol: "arrow.right.circle")
                    }
                    Group {
                        AutomationTargetSummary(rule: rule)
                        if let condition = rule.askCondition {
                            Text(condition.observeOnly ? "Ask condition · Observe only (no action)" : "Ask condition · Fixed action on match")
                                .font(TendType.detail)
                            Text(verbatim: condition.prompt).font(TendType.detail).lineLimit(2)
                        }
                        AutomationGuardSummary(rule: rule)
                        AutomationActivitySummary(rule: rule)
                    }
                    .opacity(rule.enabled ? 1 : 0.6)
                }
            }
            .toggleStyle(.switch)
            .disabled(store.isMutating)

            Menu {
                Button("Edit…", action: onEdit)
                Button("Test") {
                    Task { await store.test(rule, model: model) }
                }
                .help("Shows what this rule would do right now. Fires nothing.")
                .disabled(store.isTesting)
                Divider()
                Button("Remove…", role: .destructive, action: onRemove)
            } label: {
                Image(systemName: "ellipsis.circle")
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
            .disabled(store.isMutating)
            .help("Edit, test or remove this rule")
            .accessibilityLabel(Text("Actions for \(rule.name)"))
        }
        .padding(.vertical, 2)
    }
}

/// The filters a rule matches on. An agent kind and a limit window are said
/// the way the rest of the app says them; the rest are configuration
/// identifiers with no face of their own, shown verbatim next to a
/// localized label.
private struct AutomationTargetSummary: View {
    let rule: TendAutomationRule

    var body: some View {
        if !hasFilter {
            Text("Every agent on every host")
                .font(TendType.detail)
                .foregroundStyle(.secondary)
        } else {
            HStack(spacing: 10) {
                if !rule.agent.isEmpty {
                    AutomationTokenChips(title: "Agent", tokens: rule.agent, style: .agent)
                }
                if showsScope {
                    AutomationTokenChips(title: "Limit window", tokens: rule.scope, style: .limitScope)
                }
                ForEach(Array(chipList.enumerated()), id: \.offset) { _, chip in
                    AutomationChip(title: chip.title, value: chip.value)
                }
            }
        }
    }

    private var showsScope: Bool {
        rule.on.supportsScopeFilter && !rule.scope.isEmpty
    }

    private var hasFilter: Bool {
        !rule.agent.isEmpty || showsScope || !chipList.isEmpty
    }

    /// The filters whose values are configuration identifiers with no face
    /// of their own — a path, a regex, a pane id.
    private var chipList: [(title: LocalizedStringKey, value: String)] {
        var chips: [(title: LocalizedStringKey, value: String)] = []
        if let value = nonEmpty(rule.workspace) { chips.append((title: "Workspace", value: value)) }
        if let value = nonEmpty(rule.work) { chips.append((title: "Work", value: value)) }
        if let value = nonEmpty(rule.pane) { chips.append((title: "Pane", value: value)) }
        if let value = nonEmpty(rule.host) { chips.append((title: "Host", value: value)) }
        return chips
    }

    private func nonEmpty(_ value: String?) -> String? {
        guard let value, !value.isEmpty else { return nil }
        return value
    }
}

private struct AutomationGuardSummary: View {
    let rule: TendAutomationRule

    var body: some View {
        HStack(spacing: 10) {
            if let wait = rule.wait, !wait.isEmpty {
                AutomationWaitChip(wait: wait)
            }
            if rule.on.supportsResetTiming, let fallback = rule.fallback, !fallback.isEmpty {
                AutomationChip(title: "Fallback", value: fallback)
            }
            AutomationChip(title: "Cooldown", value: rule.cooldown)
            Text("\(rule.maxPerHour) per hour")
                .font(TendType.meta)
                .foregroundStyle(.secondary)
        }
    }
}

/// What the rule has actually done — the daemon sends both counters, so the
/// row can say how close it is to its own cap.
private struct AutomationActivitySummary: View {
    let rule: TendAutomationRule

    var body: some View {
        if rule.firedLastHour != nil || rule.lastFiredAt != nil {
            HStack(spacing: 10) {
                if let fired = rule.firedLastHour {
                    AutomationChip(
                        title: "This hour",
                        value: "\(fired)/\(rule.maxPerHour)"
                    )
                }
                if let date = TendAutomationTime.parse(rule.lastFiredAt) {
                    AutomationChip(
                        title: "Last fired",
                        value: date.formatted(date: .abbreviated, time: .shortened)
                    )
                }
            }
        }
    }
}

/// A rule's `wait`, read rather than printed: the reset anchor becomes a
/// capsule saying what it anchors on, followed by the offset. The
/// `{{reset}}` token itself never reaches the screen.
private struct AutomationWaitChip: View {
    let wait: String

    var body: some View {
        HStack(spacing: 4) {
            Text("Wait")
                .font(TendType.meta)
                .foregroundStyle(.secondary)
            if TendAutomationWaitText.isAnchored(wait) {
                AutomationResetChip()
                if !offsetText.isEmpty {
                    Text(verbatim: offsetText)
                        .font(TendType.detail.monospaced())
                }
            } else {
                Text(verbatim: wait)
                    .font(TendType.detail.monospaced())
            }
        }
    }

    /// A signed duration is a value, not a sentence: the sign and the unit
    /// are the daemon's own grammar.
    private var offsetText: String {
        TendAutomationWaitText.anchorOffsetText(wait)
    }
}

/// The reset anchor, said. Small enough to sit inside a row of chips.
struct AutomationResetChip: View {
    var body: some View {
        Text("limit resets")
            .font(TendType.meta.weight(.medium))
            .padding(.horizontal, 5)
            .padding(.vertical, 1)
            .background(Color.accentColor.opacity(0.14), in: Capsule())
    }
}

/// A filter whose values have a face of their own: the label, then each
/// value as a tinted capsule. A value this build does not know keeps its
/// wire spelling, because the daemon may know agents this build does not.
private struct AutomationTokenChips: View {
    let title: LocalizedStringKey
    let tokens: [String]
    let style: AutomationTokenStyle

    var body: some View {
        HStack(spacing: 4) {
            Text(title)
                .font(TendType.meta)
                .foregroundStyle(.secondary)
            ForEach(tokens, id: \.self) { token in
                AutomationTokenLabel(token: token, style: style)
                    .font(TendType.meta.weight(.medium))
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(style.capsuleTint(for: token).opacity(0.12), in: Capsule())
            }
        }
    }
}

private struct AutomationChip: View {
    let title: LocalizedStringKey
    let value: String

    var body: some View {
        HStack(spacing: 4) {
            Text(title)
                .font(TendType.meta)
                .foregroundStyle(.secondary)
            Text(verbatim: value)
                .font(TendType.detail.monospaced())
        }
    }
}

private struct AutomationBadge: View {
    let text: String
    let symbol: String

    var body: some View {
        Label {
            Text(verbatim: text)
        } icon: {
            Image(systemName: symbol)
        }
        .font(TendType.meta.weight(.semibold))
        .padding(.horizontal, 6)
        .padding(.vertical, 2)
        .background(Color.accentColor.opacity(0.12), in: Capsule())
    }
}

private struct AutomationLogRow: View {
    let entry: TendAutomationLogEntry
    var model: AppModel? = nil

    private var symbol: String {
        switch entry.outcome {
        case .fired: "checkmark.circle.fill"
        case .failed: "exclamationmark.triangle.fill"
        case .skipped: "minus.circle"
        case .admitted: "arrow.triangle.2.circlepath"
        case .other: "circle"
        }
    }

    private var tint: Color {
        switch entry.outcome {
        case .fired: .green
        case .failed: .red
        case .admitted: .blue
        case .skipped, .other: .secondary
        }
    }

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: symbol)
                .foregroundStyle(tint)
                .padding(.top, 2)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 8) {
                    Text(verbatim: entry.rule).font(.callout.weight(.semibold))
                    if let action = entry.action {
                        Text(verbatim: automationActionTitle(action))
                            .font(TendType.detail)
                            .foregroundStyle(.secondary)
                    }
                    Text(automationOutcomeTitle(entry.outcome))
                        .font(TendType.detail)
                        .foregroundStyle(tint)
                }
                HStack(spacing: 10) {
                    if let pane = entry.pane, !pane.isEmpty {
                        AutomationChip(title: "Pane", value: pane)
                    }
                    if let agent = entry.agent, !agent.isEmpty {
                        AutomationTokenChips(title: "Agent", tokens: [agent], style: .agent)
                    }
                    // A swap the automation started runs as its own job:
                    // follow it where the app follows every account switch.
                    if entry.action == .swapAccount, let key = entry.operation, let model {
                        Button("Follow Switch") {
                            model.swaps.follow(
                                requestKey: key,
                                host: nil,
                                title: String(localized: "\(entry.rule) · \(entry.pane ?? "")")
                            )
                        }
                        .buttonStyle(.tendGhost)
                        .controlSize(.small)
                        .help("Show this switch's progress and result in the status bar's Account Switches")
                    }
                }
                detailLine
            }
            Spacer(minLength: 8)
            Group {
                if let date = entry.firedDate {
                    Text(date.formatted(date: .abbreviated, time: .shortened))
                } else if let raw = entry.firedAt {
                    Text(verbatim: raw)
                } else {
                    Text("Unknown time")
                }
            }
            .font(TendType.detail.monospacedDigit())
            .foregroundStyle(.secondary)
        }
        .padding(.vertical, 8)
    }

    /// A skip's detail is a reason the app can name; a firing's is the text
    /// that was sent, which is the operator's own and stays verbatim.
    @ViewBuilder
    private var detailLine: some View {
        if let reason = automationLogReason(outcome: entry.outcome, detail: entry.detail) {
            Text(verbatim: reason.title)
                .font(TendType.detail)
                .foregroundStyle(.secondary)
            if let remainder = reason.remainder {
                Text(verbatim: remainder)
                    .font(TendType.detail.monospaced())
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
        } else if let reason = entry.skipReason, !reason.isEmpty {
            Text(verbatim: automationSkipReasonTitle(reason))
                .font(TendType.detail)
                .foregroundStyle(.secondary)
        } else if let detail = entry.detail, !detail.isEmpty {
            Text(verbatim: detail)
                .font(TendType.detail)
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
        }
    }
}

// MARK: - Dry run

/// `automation_test`: what the rule would do against the live registry,
/// having fired nothing.
private struct AutomationTestSheet: View {
    @Environment(\.dismiss) private var dismiss
    let report: TendAutomationTestReport

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 8) {
                    Text(verbatim: report.rule).font(.title3.weight(.semibold))
                    Spacer()
                    if !report.engineEnabled {
                        AutomationBadge(text: String(localized: "Engine off"), symbol: "power")
                    } else if let until = TendAutomationTime.parse(report.pausedUntil), until > Date() {
                        AutomationBadge(text: String(localized: "Paused"), symbol: "pause.circle")
                    }
                    if !report.enabled {
                        AutomationBadge(text: String(localized: "Rule off"), symbol: "moon.zzz")
                    }
                }
                Text("Free deterministic check: no Ask turn, no action, and nothing recorded. Ask conditions require the separate billed test in the rule editor.")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
            }
            .padding(20)

            Divider()

            if report.candidates.isEmpty {
                VStack(spacing: 6) {
                    Image(systemName: "person.slash")
                        .font(.system(size: 26))
                        .foregroundStyle(.secondary)
                    Text("No agent matches this rule right now.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollView {
                    VStack(spacing: 0) {
                        ForEach(Array(report.candidates.enumerated()), id: \.offset) { index, candidate in
                            if index > 0 { Divider() }
                            AutomationTestRow(candidate: candidate)
                        }
                    }
                    .padding(.horizontal, 16)
                }
            }

            Divider()

            HStack {
                Text("\(report.firing.count) of \(report.candidates.count) agents would be acted on")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Done") { dismiss() }
                    .buttonStyle(.borderedProminent)
            }
            .padding(16)
        }
        .frame(width: 560, height: 460)
    }
}

private struct AutomationTestRow: View {
    let candidate: TendAutomationTestCandidate

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: candidate.wouldFire ? "bolt.circle.fill" : "minus.circle")
                .foregroundStyle(candidate.wouldFire ? Color.accentColor : Color.secondary)
                .padding(.top, 2)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                Text(verbatim: automationDecisionTitle(candidate.decision))
                    .font(.callout.weight(candidate.wouldFire ? .semibold : .regular))
                HStack(spacing: 10) {
                    if let pane = candidate.pane, !pane.isEmpty {
                        AutomationChip(title: "Pane", value: pane)
                    }
                    AutomationTokenChips(title: "Agent", tokens: [candidate.agent], style: .agent)
                    AutomationChip(title: "State", value: candidate.state)
                }
                if let detail = candidate.detail, !detail.isEmpty {
                    Text(verbatim: detail)
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
            }
            Spacer(minLength: 8)
            if let date = candidate.fireDate {
                Text(date.formatted(date: .omitted, time: .shortened))
                    .font(TendType.detail.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 8)
    }
}

// MARK: - Editor

/// Add Rule… / Edit…: every field the daemon reads, validated before it is
/// allowed to leave the sheet.
struct AutomationRuleEditor: View {
    @Environment(\.dismiss) private var dismiss
    @ObservedObject var model: AppModel
    @ObservedObject var store: AutomationStore
    let existingNames: Set<String>
    @State private var draft: TendAutomationRuleDraft
    @State private var wait: TendAutomationWaitDraft
    @State private var copiedTOML = false
    @ObservedObject private var providers = AskProviderStore.shared

    init(
        draft: TendAutomationRuleDraft,
        existingNames: Set<String>,
        model: AppModel,
        store: AutomationStore
    ) {
        _draft = State(initialValue: draft)
        _wait = State(
            initialValue: TendAutomationWaitDraft.read(draft.wait, event: draft.event)
        )
        self.existingNames = existingNames
        self.model = model
        self.store = store
    }

    private var issues: [TendAutomationRuleIssue] {
        var issues = draft.issues(existingNames: existingNames)
        if let condition = draft.askCondition {
            if !store.isAskSupported {
                issues.append(.invalidAskCondition(String(localized: "Update tendd for automation_ask_v1. This condition cannot be saved on an older daemon.")))
            }
            if !providers.providersFromDaemon || !providers.providers.contains(where: {
                $0.id == condition.provider && $0.kind == .api
                    && ["openai", "anthropic"].contains($0.engine)
            }) {
                issues.append(.invalidAskCondition(String(localized: "Choose an API Ask provider ID backed by OpenAI or Anthropic. Configure its API key in Ask providers; Claude/Codex CLI providers are not supported.")))
            }
        }
        return issues
    }

    /// The Timing controls and `draft.wait` move together: every change
    /// recomposes the value, so what the daemon is sent is only ever what
    /// the controls composed.
    private var waitBinding: Binding<TendAutomationWaitDraft> {
        Binding(
            get: { wait },
            set: { newValue in
                wait = newValue
                draft.wait = newValue.text
            }
        )
    }

    /// When the rule acts: which clock the wait is measured from, and how
    /// far off it. The operator never types `{{reset}}` — these controls
    /// compose it — and a value they cannot express is handed back as the
    /// text it was, so an odd spelling written by hand is never silently
    /// rewritten.
    ///
    /// Rows of the Timing section rather than a view of their own: a custom
    /// `View` would be one row, and these need the form's own alignment.
    @ViewBuilder
    private var waitControls: some View {
        if wait.anchor == .freeform {
            TextField("Wait", text: waitBinding.freeform)
                .font(.callout.monospaced())
            HStack {
                Text("This wait was written by hand, so tend keeps it exactly as it is.")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                Spacer(minLength: 8)
                if wait.freeformIsReadable {
                    Button("Use the Controls") {
                        wait = TendAutomationWaitDraft.read(wait.freeform, event: draft.event)
                        draft.wait = wait.text
                    }
                    .controlSize(.small)
                }
            }
        } else {
            if draft.event.supportsResetTiming {
                Picker("Wait", selection: waitBinding.anchor) {
                    Text("When the limit resets")
                        .tag(TendAutomationWaitDraft.Anchor.reset)
                    Text("A fixed delay after the event")
                        .tag(TendAutomationWaitDraft.Anchor.event)
                }
            }
            if wait.anchor == .reset {
                LabeledContent {
                    HStack(spacing: 8) {
                        TextField("Offset", text: waitBinding.offset, prompt: Text(verbatim: "2m"))
                            .labelsHidden()
                            .frame(width: 92)
                        Picker("Offset direction", selection: waitBinding.isBefore) {
                            Text("After the reset").tag(false)
                            Text("Before the reset").tag(true)
                        }
                        .labelsHidden()
                        .pickerStyle(.segmented)
                        .fixedSize()
                        // Before or after nothing means nothing, so the
                        // direction only opens once an offset is typed.
                        .disabled(wait.offset.trimmingCharacters(in: .whitespaces).isEmpty)
                    }
                } label: {
                    Text("Offset")
                }
            } else {
                TextField("Delay", text: waitBinding.offset, prompt: Text(verbatim: "5m"))
            }
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section("Rule") {
                    TextField("Name", text: $draft.name)
                    Text("Used in tend configuration and in the run log. Letters, digits, hyphens, underscores and dots.")
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                    Toggle("Enabled", isOn: $draft.enabled)
                }

                Section("Event") {
                    Picker("Fires on", selection: $draft.event) {
                        ForEach(TendAutomationEvent.pickable, id: \.self) { event in
                            Text(automationEventTitle(event)).tag(event)
                        }
                    }
                    Text(automationEventDetail(draft.event))
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                    if draft.event.requiresDuration {
                        TextField("Idle for", text: $draft.idleFor, prompt: Text(verbatim: "10m"))
                    }
                }

                Section("Filters") {
                    AutomationTokenPicker(
                        title: "Agents",
                        tokens: TendAutomationRuleDraft.agentKinds,
                        style: .agent,
                        selection: $draft.agents
                    )
                    if draft.event.supportsScopeFilter {
                        AutomationTokenPicker(
                            title: "Limit window",
                            tokens: TendAutomationRuleDraft.rateLimitScopes,
                            style: .limitScope,
                            selection: $draft.scopes
                        )
                    }
                    // No placeholders here: an empty filter matches
                    // everything, and a greyed example reads like a value
                    // that is already set. The caption below is the
                    // explanation.
                    TextField("Workspace", text: $draft.workspace)
                    TextField("Work id matches", text: $draft.work)
                    TextField("Pane", text: $draft.pane)
                    Picker("Host", selection: $draft.host) {
                        Text("Any").tag("")
                        ForEach(TendAutomationRuleDraft.hosts, id: \.self) { host in
                            Text(verbatim: host).tag(host)
                        }
                    }
                    Text("Leave a filter empty to match everything. All the filters you set must match.")
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                }

                Section("Timing") {
                    waitControls
                    if draft.event.supportsResetTiming {
                        TextField("Fallback", text: $draft.fallback, prompt: Text(verbatim: "20m"))
                    }
                    TextField("Jitter", text: $draft.jitter, prompt: Text(verbatim: "30s"))
                    AutomationTimingPreview(draft: draft)
                }

                Section("Ask condition (optional)") {
                    Toggle("Judge a natural-language condition", isOn: Binding(
                        get: { draft.askCondition != nil },
                        set: { draft.askCondition = $0 ? TendAutomationAskCondition() : nil }
                    ))
                    if draft.askCondition != nil {
                        AutomationAskConditionPanel(
                            condition: Binding(
                                get: { draft.askCondition ?? TendAutomationAskCondition() },
                                set: { draft.askCondition = $0 }
                            ),
                            rule: draft.rule,
                            canTest: issues.isEmpty,
                            model: model
                        )
                    }
                }

                Section("Fixed action") {
                    if draft.askCondition != nil {
                        Text("Ask only judges the condition; it cannot choose or run an action. Observe-only logs judgments without executing this fixed action.")
                            .font(TendType.detail).foregroundStyle(.secondary)
                    }
                    Picker("Does", selection: $draft.action) {
                        ForEach(TendAutomationAction.pickable, id: \.self) { action in
                            Text(automationActionTitle(action)).tag(action)
                        }
                    }
                    Text(automationActionDetail(draft.action))
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                    if draft.action.carriesAccount {
                        TextField("Account", text: $draft.account, prompt: Text("Best account with quota left"))
                            .font(.callout.monospaced())
                        Text("A configured account name or an aas account such as work@claude. Leave empty to let Tend choose an available account of the same provider.")
                            .font(TendType.detail)
                            .foregroundStyle(.secondary)
                        TextField("Follow-up", text: $draft.text, prompt: Text("The [swap] follow-up from config.toml"))
                            .font(.callout.monospaced())
                        Text("Typed after the conversation resumes on the new account, so the turn the limit cut off carries on.")
                            .font(TendType.detail)
                            .foregroundStyle(.secondary)
                    } else if draft.action.needsText {
                        TextField("Text", text: $draft.text, prompt: Text(verbatim: "continue"))
                            .font(.callout.monospaced())
                        Toggle("Press Enter after typing", isOn: $draft.submit)
                        Text("Without this the text is left on the agent's prompt for you to send.")
                            .font(TendType.detail)
                            .foregroundStyle(.secondary)
                    }
                    if draft.action.needsMessage {
                        TextField("Message", text: $draft.message)
                        Text("Recorded in the run log and posted by tendd. The agent is not touched.")
                            .font(TendType.detail)
                            .foregroundStyle(.secondary)
                    }
                }

                Section("Guards") {
                    Stepper(value: $draft.maxPerHour, in: 1...TendAutomationRule.maximumMaxPerHour) {
                        LabeledContent("Firings per hour") {
                            Text(verbatim: String(draft.maxPerHour)).monospacedDigit()
                        }
                    }
                    TextField("Cooldown", text: $draft.cooldown, prompt: Text(verbatim: "5m"))
                    Picker("Only if still", selection: $draft.onlyIfStill) {
                        Text("The event's own condition").tag(TendAutomationCondition?.none)
                        ForEach(TendAutomationCondition.pickable, id: \.self) { condition in
                            Text(automationConditionTitle(condition)).tag(TendAutomationCondition?.some(condition))
                        }
                    }
                    Text("Cooldown applies per pane and rule. Only if still is re-checked against the live registry when the rule fires, so an agent you resumed yourself is left alone.")
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                }

                if !issues.isEmpty {
                    Section {
                        ForEach(Array(issues.enumerated()), id: \.offset) { _, issue in
                            Label(issue.message, systemImage: "exclamationmark.circle")
                                .font(TendType.detail)
                                .foregroundStyle(.orange)
                        }
                    }
                }

                if let error = store.actionError {
                    Section {
                        Label(error, systemImage: "xmark.octagon.fill")
                            .font(TendType.detail)
                            .foregroundStyle(.red)
                            .textSelection(.enabled)
                        Text("If this tendd cannot write rules yet, copy the rule and paste it into tend configuration, then reload tendd.")
                            .font(TendType.detail)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .formStyle(.grouped)
            .textFieldStyle(.roundedBorder)

            Divider()

            HStack {
                Button {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(draft.rule.tomlSnippet, forType: .string)
                    copiedTOML = true
                } label: {
                    Label(copiedTOML ? "Copied" : "Copy as TOML", systemImage: "doc.on.doc")
                }
                .help("Copies this rule as a [[automation.rule]] block for tend configuration.")
                Spacer()
                if store.isMutating {
                    ProgressView().controlSize(.small)
                }
                Button("Cancel", role: .cancel) { dismiss() }
                Button("Save") {
                    Task {
                        if await store.save(draft.rule, model: model) { dismiss() }
                    }
                }
                .buttonStyle(.borderedProminent)
                .disabled(!issues.isEmpty || store.isMutating)
            }
            .padding(16)
        }
        .frame(width: 620, height: 660)
        .task { await providers.reload(model: model) }
        .onChange(of: draft) { _ in copiedTOML = false }
        .onChange(of: draft.event) { _ in
            // Only a cap carries a reset time, so an event that has none
            // cannot keep the reset anchor the operator had picked.
            guard !draft.event.supportsResetTiming, wait.anchor == .reset else { return }
            wait.anchor = .event
            draft.wait = wait.text
        }
    }
}

private struct AutomationAskConditionPanel: View {
    @Binding var condition: TendAutomationAskCondition
    let rule: TendAutomationRule
    let canTest: Bool
    @ObservedObject var model: AppModel
    @State private var pane = ""
    @State private var isTesting = false
    @State private var confirmsTest = false
    @State private var judgment: TendAutomationJudgment?
    @State private var testError: String?
    @State private var testID: UUID?

    var body: some View {
        TextField("API Ask provider ID", text: $condition.provider, prompt: Text(verbatim: "openai"))
        Text("Default: openai. Requires API provider configuration and a key available to tendd; this request does not forward app-only Keychain keys. Only OpenAI, Anthropic, or named API instances backed by them are supported—not Claude/Codex CLI providers.")
            .font(TendType.detail).foregroundStyle(.secondary)
        TextField("Natural-language condition", text: $condition.prompt, axis: .vertical)
            .lineLimit(3...8)
        Toggle("Observe only — log judgments, execute no action", isOn: $condition.observeOnly)
        Stepper("Timeout: \(condition.timeoutSecs) seconds", value: $condition.timeoutSecs, in: 5...120)
        Stepper("Maximum judgment calls per hour: \(condition.maxPerHour)", value: $condition.maxPerHour, in: 1...30)
        Text("Each judgment is one read-only, tool-free Ask turn. Only bounded recent screen text, current state, and work/workspace IDs are sent—not full goal lookup. This context leaves your machine for the selected API provider and may be billed. Judgment call limits are separate from action firing limits.")
            .font(TendType.detail).foregroundStyle(.secondary)
        TextField("Pane ID to try against", text: $pane, prompt: Text(verbatim: "%1"))
        Button(isTesting ? "Judging…" : "Try condition — one billed API turn…") {
            confirmsTest = true
        }
        .disabled(!canTest || isTesting || pane.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        .confirmationDialog("Send pane context for one billed Ask turn?", isPresented: $confirmsTest) {
            Button("Send context and judge (billed)") {
                let testedRule = rule
                let testedPane = pane.trimmingCharacters(in: .whitespacesAndNewlines)
                let requestID = UUID()
                testID = requestID
                isTesting = true
                judgment = nil
                testError = nil
                Task {
                    defer { isTesting = false }
                    do {
                        let result = try await model.client.automationJudgeTest(rule: testedRule, pane: testedPane)
                        if testID == requestID {
                            judgment = result
                        }
                    } catch {
                        if testID == requestID { testError = error.localizedDescription }
                    }
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Bounded recent screen/current state and work/workspace IDs leave this machine for the selected API provider. This uses one potentially billed, read-only, tool-free turn and executes no action, even with observe-only off.")
        }
        if let judgment {
            VStack(alignment: .leading, spacing: 5) {
                Text("Judgment: \(judgment.decision.rawValue)").font(.headline)
                Text(verbatim: judgment.reason)
                ForEach(Array(judgment.evidence.enumerated()), id: \.offset) { _, evidence in
                    Text(verbatim: "• " + evidence)
                }
                Text(verbatim: "Provider: \(judgment.provider) · Model: \(judgment.model ?? "—")")
                Text(verbatim: "Context hash: \(judgment.contextHash)")
            }
            .font(TendType.detail).textSelection(.enabled)
        }
        if let testError {
            Text(verbatim: testError).font(TendType.detail).foregroundStyle(.red).textSelection(.enabled)
        }
        Text("Unknown is not a match and must not execute an action.")
            .font(TendType.detail).foregroundStyle(.secondary)
            .onChange(of: rule) { _ in testID = nil; judgment = nil; testError = nil }
            .onChange(of: pane) { _ in testID = nil; judgment = nil; testError = nil }
            .onAppear { if pane.isEmpty { pane = rule.pane ?? "" } }
            .onDisappear { testID = nil }
    }
}

/// How a wire value is drawn wherever it is offered or displayed: an agent
/// kind carries its mark, a limit window is said in words.
enum AutomationTokenStyle {
    case agent
    case limitScope

    func title(for token: String) -> String {
        switch self {
        case .agent: TendAgentMark.title(for: token)
        case .limitScope: automationLimitScopeTitle(token)
        }
    }

    func tint(for token: String) -> Color {
        switch self {
        case .agent: TendAgentMark.known(for: token)?.tint ?? .secondary
        case .limitScope: .accentColor
        }
    }

    /// The capsule behind a token. A known agent's logo already carries its
    /// brand colour, so its capsule stays neutral.
    func capsuleTint(for token: String) -> Color {
        if self == .agent, TendAgentMark.known(for: token)?.logo != nil { return .secondary }
        return tint(for: token)
    }
}

/// One token with whatever face it has. A value this build does not know
/// gets no symbol — inventing one would say something untrue — and keeps
/// its wire spelling.
private struct AutomationTokenLabel: View {
    let token: String
    let style: AutomationTokenStyle

    var body: some View {
        HStack(spacing: 4) {
            if let mark {
                TendAgentLogo(mark: mark, size: 12)
                    .foregroundStyle(mark.tint)
            }
            Text(verbatim: style.title(for: token))
        }
    }

    private var mark: TendAgentMark? {
        style == .agent ? TendAgentMark.known(for: token) : nil
    }
}

/// A small multi-select over the values a filter matches on. Values a rule
/// already carries that this build does not list come last, so editing a
/// rule never silently drops them.
private struct AutomationTokenPicker: View {
    let title: LocalizedStringKey
    let tokens: [String]
    let style: AutomationTokenStyle
    @Binding var selection: Set<String>

    var body: some View {
        LabeledContent {
            VStack(alignment: .leading, spacing: 6) {
                LazyVGrid(
                    columns: [GridItem(.adaptive(minimum: 128), spacing: 6, alignment: .leading)],
                    alignment: .leading,
                    spacing: 6
                ) {
                    ForEach(offered, id: \.self) { token in
                        Toggle(isOn: binding(for: token)) {
                            AutomationTokenLabel(token: token, style: style)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                        .toggleStyle(.button)
                        .controlSize(.small)
                    }
                }
                if selection.isEmpty {
                    Text("Any").font(TendType.detail).foregroundStyle(.secondary)
                }
            }
        } label: {
            Text(title)
        }
    }

    private var offered: [String] {
        tokens + selection.subtracting(tokens).sorted()
    }

    private func binding(for token: String) -> Binding<Bool> {
        Binding(
            get: { selection.contains(token) },
            set: { isOn in
                if isOn {
                    selection.insert(token)
                } else {
                    selection.remove(token)
                }
            }
        )
    }
}

/// Plain language for the timing fields, so `reset+2m` is never the only
/// explanation of when a rule acts — and so an empty field says what the
/// daemon's default actually does.
private struct AutomationTimingPreview: View {
    let draft: TendAutomationRuleDraft

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            waitLine
            if draft.event.supportsResetTiming {
                fallbackLine
            }
            jitterLine
        }
        .font(TendType.detail)
        .foregroundStyle(.secondary)
    }

    @ViewBuilder
    private var waitLine: some View {
        switch draft.timing {
        case .afterReset(let offset) where offset == 0:
            Text("Fires as soon as the limit resets.")
        case .afterReset(let offset) where offset < 0:
            Text("Fires \(TendDurationText.spelled(-offset)) before the limit resets.")
        case .afterReset(let offset):
            Text("Fires \(TendDurationText.spelled(offset)) after the limit resets.")
        case .delay(let seconds) where seconds <= 0:
            Text("Fires as soon as the event matches.")
        case .delay(let seconds):
            Text("Fires \(TendDurationText.spelled(seconds)) after the event.")
        case nil:
            Text("Wait is not a duration tendd can read.")
                .foregroundStyle(.orange)
        }
    }

    @ViewBuilder
    private var fallbackLine: some View {
        if let seconds = draft.fallbackSeconds {
            if draft.fallbackIsDefault {
                Text("When the cap carries no reset time, tendd waits \(TendDurationText.spelled(seconds)) — its default.")
            } else {
                Text("When the cap carries no reset time, it waits \(TendDurationText.spelled(seconds)) instead.")
            }
        } else {
            Text("Fallback is not a duration tendd can read.")
                .foregroundStyle(.orange)
        }
    }

    @ViewBuilder
    private var jitterLine: some View {
        if let seconds = draft.jitterSeconds {
            if draft.jitterIsDefault {
                Text("Up to \(TendDurationText.spelled(seconds)) of random delay is added — tendd's default, so several agents do not resume at once.")
            } else {
                Text("Up to \(TendDurationText.spelled(seconds)) of random delay is added.")
            }
        } else {
            Text("Jitter is not a duration tendd can read.")
                .foregroundStyle(.orange)
        }
    }
}

/// Seconds as words, through the system formatter so Korean reads right.
enum TendDurationText {
    static func spelled(_ seconds: TimeInterval) -> String {
        let formatter = DateComponentsFormatter()
        formatter.unitsStyle = .full
        formatter.allowedUnits = [.day, .hour, .minute, .second]
        formatter.maximumUnitCount = 2
        return formatter.string(from: seconds) ?? ""
    }
}

// MARK: - Titles

/// Event, action and outcome names live outside the wire enums so
/// `TendIPC+Automation` stays a transport file. `other` shows the daemon's
/// own token rather than inventing a name for it.
func automationEventTitle(_ event: TendAutomationEvent) -> String {
    switch event {
    case .rateLimited: String(localized: "Rate limited")
    case .waitingInput: String(localized: "Waiting for input")
    case .idleFor: String(localized: "Idle for")
    case .error: String(localized: "Error")
    case .other(let raw): raw
    }
}

func automationEventDetail(_ event: TendAutomationEvent) -> String {
    switch event {
    case .rateLimited:
        String(localized: "The agent hit its session or weekly limit.")
    case .waitingInput:
        String(localized: "The agent is blocked waiting for a person.")
    case .idleFor:
        String(localized: "The agent has done nothing for the duration below.")
    case .error:
        String(localized: "The agent stopped on a failure that is not a limit.")
    case .other:
        String(localized: "An event this build of Tend does not know.")
    }
}

func automationActionTitle(_ action: TendAutomationAction) -> String {
    switch action {
    case .sendPrompt: String(localized: "Send prompt")
    case .notify: String(localized: "Notify")
    case .interrupt: String(localized: "Interrupt")
    case .pressEnter: String(localized: "Confirm selected Yes")
    case .swapAccount: String(localized: "Switch account and resume")
    case .other(let raw): raw
    }
}

func automationActionDetail(_ action: TendAutomationAction) -> String {
    switch action {
    case .sendPrompt:
        String(localized: "Types the text into the agent's pane and presses Enter.")
    case .notify:
        String(localized: "Records a notice in the run log. The agent is not touched.")
    case .pressEnter:
        String(localized: "Presses Enter only while waiting on a selected Yes or Proceed confirmation.")
    case .interrupt:
        String(localized: "Sends the agent an interrupt, as Escape would.")
    case .swapAccount:
        String(localized: "Stops the agent, relaunches the same conversation on another account, and types the follow-up so it carries on.")
    case .other:
        String(localized: "An action this build of Tend does not know.")
    }
}

func automationConditionTitle(_ condition: TendAutomationCondition) -> String {
    switch condition {
    case .rateLimited: String(localized: "Still rate limited")
    case .waitingInput: String(localized: "Still waiting for input")
    case .idle: String(localized: "Still idle")
    case .error: String(localized: "Still in error")
    case .any: String(localized: "Whatever the agent is doing")
    case .other(let raw): raw
    }
}

func automationOutcomeTitle(_ outcome: TendAutomationOutcome) -> String {
    switch outcome {
    case .fired: String(localized: "Fired")
    case .skipped: String(localized: "Skipped")
    case .failed: String(localized: "Failed")
    case .admitted: String(localized: "Swap started")
    case .other("judging"): String(localized: "Judging condition")
    case .other("judged"): String(localized: "Condition judged")
    case .other("judge_test"): String(localized: "Ask condition test")
    case .other(let raw): raw
    }
}

/// tendd's `SkipReason` tokens, said in words. An unknown token is shown as
/// it arrived rather than hidden.
func automationSkipReasonTitle(_ reason: String) -> String {
    automationKnownReasonTitle(reason) ?? reason
}

/// A skipped or failed run's detail when it leads with a reason token tendd
/// names — `action_failed`, or `action_failed: job 7 ended failed` — as the
/// reason in words plus whatever tendd added after it. Nil for anything
/// else: the text a firing sent, or a sentence tendd wrote itself.
func automationLogReason(
    outcome: TendAutomationOutcome,
    detail: String?
) -> (title: String, remainder: String?)? {
    guard outcome == .skipped || outcome == .failed,
          let detail = detail?.trimmingCharacters(in: .whitespacesAndNewlines),
          !detail.isEmpty else { return nil }
    let token: String
    var remainder: String?
    if let separator = detail.range(of: ": ") {
        token = String(detail[..<separator.lowerBound])
        let rest = detail[separator.upperBound...].trimmingCharacters(in: .whitespaces)
        remainder = rest.isEmpty ? nil : rest
    } else {
        token = detail
    }
    guard let title = automationKnownReasonTitle(token) else { return nil }
    return (title, remainder)
}

/// The words for a reason token this build knows; nil for any other text.
func automationKnownReasonTitle(_ reason: String) -> String? {
    switch reason {
    case "ask_required": String(localized: "Ask judgment required (not run by this free test)")
    case "engine_disabled": String(localized: "Automations are switched off")
    case "paused": String(localized: "Automations are paused")
    case "rule_disabled": String(localized: "The rule is switched off")
    case "event_mismatch": String(localized: "The agent is not in this rule's state")
    case "filter_mismatch": String(localized: "A filter does not match this agent")
    case "no_pane": String(localized: "The agent has no pane to act on")
    case "episode_already_handled": String(localized: "This rule already acted on this episode")
    case "cooldown": String(localized: "Still inside the cooldown")
    case "hourly_cap": String(localized: "The rule's hourly cap is reached")
    case "global_cap": String(localized: "The engine's hourly ceiling is reached")
    case "condition_cleared": String(localized: "The agent recovered before it fired")
    case "pane_gone": String(localized: "The pane is gone")
    case "action_failed": String(localized: "The action was refused")
    default: nil
    }
}

/// `automation_test`'s `decision`: `fire`, or one of the skip reasons.
func automationDecisionTitle(_ decision: String) -> String {
    decision == "fire" ? String(localized: "Would fire") : automationSkipReasonTitle(decision)
}
