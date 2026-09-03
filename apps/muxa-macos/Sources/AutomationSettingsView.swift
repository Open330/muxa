import AppKit
import SwiftUI

/// Settings › Automations: the rules muxad fires on its own, the switches
/// that gate them, and what they have done.
struct AutomationSettingsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AutomationStore
    @ObservedObject var configStore: MuxaConfigStore
    @State private var editorTarget: AutomationRuleEditorTarget?
    @State private var removalTarget: MuxaAutomationRule?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                settingsHeading(
                    "Automations",
                    detail: "muxad watches your agents and acts on rules you write — resuming a capped session, for one."
                )

                if store.hasLoaded, !store.isSupported, model.isConnected {
                    unsupportedCard
                } else {
                    switchesCard
                    safetyCard
                    rulesCard
                    logCard
                }

                statusLines
            }
            .padding(20)
        }
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
        .alert("Remove this rule?", isPresented: removalAlertBinding, presenting: removalTarget) { rule in
            Button("Cancel", role: .cancel) { removalTarget = nil }
            Button("Remove", role: .destructive) {
                let target = rule
                removalTarget = nil
                Task { await store.remove(target, model: model) }
            }
        } message: { rule in
            Text("\(rule.name) will stop firing and is removed from muxa configuration.")
        }
    }

    private var removalAlertBinding: Binding<Bool> {
        Binding(get: { removalTarget != nil }, set: { if !$0 { removalTarget = nil } })
    }

    private var testReportBinding: Binding<Bool> {
        Binding(get: { store.testReport != nil }, set: { if !$0 { store.clearTestReport() } })
    }

    // MARK: Capability fallback

    private var unsupportedCard: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("This muxad has no automation engine", systemImage: "wand.and.rays")
                .font(.headline)
            Text("Update muxa and restart muxad to write rules from here. Until then, rules can be written by hand in the configuration file and read with muxa automation list.")
                .font(.callout)
                .foregroundStyle(.secondary)
            HStack {
                Text("The Advanced tab opens that file.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                MuxaDaemonReloadButton(model: model)
                    .controlSize(.small)
            }
        }
        .padding(14)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    // MARK: Master switch and pause

    private var switchesCard: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Label("Automation engine", systemImage: "power")
                    .font(.headline)
                Spacer()
                if store.isLoading || store.isMutating || configStore.isSaving {
                    ProgressView().controlSize(.small)
                }
                Toggle("Enabled", isOn: masterBinding)
                    .labelsHidden()
                    .disabled(!store.isSupported || !store.hasLoaded || store.isMutating)
            }

            Text("Off stops every rule and is remembered in muxa configuration. Pause stops them until a time you choose.")
                .font(.caption)
                .foregroundStyle(.secondary)

            Divider()

            HStack(spacing: 10) {
                if let until = store.snapshot.pausedUntil, store.snapshot.isPaused() {
                    Label("Paused until \(until.formatted(date: .abbreviated, time: .shortened))", systemImage: "pause.circle.fill")
                        .font(.callout)
                        .foregroundStyle(.orange)
                    Spacer()
                    Button("Resume") {
                        Task { await store.pause(until: nil, model: model) }
                    }
                    .buttonStyle(.borderedProminent)
                } else {
                    Label("Running", systemImage: "play.circle.fill")
                        .font(.callout)
                        .foregroundStyle(.green)
                    Spacer()
                    Button("Pause for 1 Hour") {
                        Task { await store.pause(until: Date().addingTimeInterval(3600), model: model) }
                    }
                    Button("Pause Until Tomorrow") {
                        Task { await store.pause(until: MuxaAutomationTime.tomorrowMorning(), model: model) }
                    }
                }
            }
            .controlSize(.small)
            .disabled(store.isMutating || !store.isSupported)
        }
        .padding(14)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
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

    private var safetyCard: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("A rule types into a live agent", systemImage: "exclamationmark.triangle.fill")
                .font(.headline)
                .foregroundStyle(.orange)
            Text("Send prompt injects text and presses Enter in the agent's pane, exactly as you would. These guards bound it:")
                .font(.callout)
            VStack(alignment: .leading, spacing: 3) {
                Text("• The master switch and the pause above stop every rule.")
                Text("• Each rule has its own switch.")
                Text("• Firings per hour and the cooldown cap how often one rule may act on one pane.")
                Text("• A ceiling of \(store.snapshot.globalMaxPerHour) firings an hour bounds every rule together.")
                Text("• The condition is re-checked at fire time; a recovered agent is left alone.")
                Text("• One rate-limit episode fires a rule once.")
                Text("• Every firing and every skip is written to the run log below.")
            }
            .font(.caption)
            .foregroundStyle(.secondary)
            Text("Test on a rule shows what it would do right now without firing anything.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(14)
        .background(Color.orange.opacity(0.08), in: RoundedRectangle(cornerRadius: 12))
    }

    // MARK: Rules

    private var rulesCard: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Label("Rules", systemImage: "list.bullet.rectangle")
                    .font(.headline)
                Spacer()
                Button {
                    editorTarget = AutomationRuleEditorTarget(MuxaAutomationRuleDraft.sessionLimitDraft)
                } label: {
                    Label("Add the Session-Limit Rule", systemImage: "sparkles")
                }
                .disabled(store.existingNames.contains(MuxaAutomationRule.sessionLimitRecommendation.name))
                Button {
                    editorTarget = AutomationRuleEditorTarget(MuxaAutomationRuleDraft())
                } label: {
                    Label("Add Rule…", systemImage: "plus")
                }
                .buttonStyle(.borderedProminent)
            }
            .controlSize(.small)
            .disabled(!store.isSupported)

            if store.rules.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    Text("No rules yet").font(.callout.weight(.semibold))
                    Text("A fresh install ships none, so nothing fires until you add one. Add the session-limit rule to resume a capped agent when its window reopens.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.vertical, 10)
            } else {
                VStack(spacing: 8) {
                    ForEach(store.rules) { rule in
                        AutomationRuleRow(
                            model: model,
                            store: store,
                            rule: rule,
                            onEdit: {
                                editorTarget = AutomationRuleEditorTarget(
                                    MuxaAutomationRuleDraft.draft(editing: rule)
                                )
                            },
                            onRemove: { removalTarget = rule }
                        )
                    }
                }
            }
        }
        .padding(14)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    // MARK: Run log

    private var logCard: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Label("Run log", systemImage: "clock.arrow.circlepath")
                    .font(.headline)
                Spacer()
                Button {
                    Task { await store.reload(model: model) }
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                .controlSize(.small)
                .disabled(store.isLoading || !store.isSupported)
            }

            if store.log.isEmpty {
                Text("Nothing has fired yet. Every firing and every skipped firing lands here, newest first.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.vertical, 6)
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(store.log.enumerated()), id: \.offset) { index, entry in
                        if index > 0 { Divider() }
                        AutomationLogRow(entry: entry)
                    }
                }
            }
        }
        .padding(14)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    @ViewBuilder
    private var statusLines: some View {
        if let error = store.loadError {
            Label(error, systemImage: "exclamationmark.triangle.fill")
                .font(.caption)
                .foregroundStyle(.orange)
                .textSelection(.enabled)
        }
        if let error = store.actionError {
            Label(error, systemImage: "xmark.octagon.fill")
                .font(.caption)
                .foregroundStyle(.red)
                .textSelection(.enabled)
        }
        if let error = configStore.saveError {
            Label(error, systemImage: "xmark.octagon.fill")
                .font(.caption)
                .foregroundStyle(.red)
                .textSelection(.enabled)
        }
    }
}

/// Identity for the editor sheet; a draft alone is not `Identifiable`.
struct AutomationRuleEditorTarget: Identifiable {
    let id: String
    let draft: MuxaAutomationRuleDraft

    init(_ draft: MuxaAutomationRuleDraft) {
        self.draft = draft
        id = draft.originalName ?? "new:\(UUID().uuidString)"
    }
}

// MARK: - Rows

private struct AutomationRuleRow: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AutomationStore
    let rule: MuxaAutomationRule
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
        HStack(alignment: .top, spacing: 12) {
            Toggle("Enabled", isOn: enabledBinding)
                .labelsHidden()
                .disabled(store.isMutating)
                .padding(.top, 2)

            VStack(alignment: .leading, spacing: 5) {
                HStack(spacing: 8) {
                    Text(verbatim: rule.name)
                        .font(.headline)
                    AutomationBadge(text: automationEventTitle(rule.on), symbol: "bolt.horizontal")
                    AutomationBadge(text: automationActionTitle(rule.action), symbol: "arrow.right.circle")
                }
                AutomationTargetSummary(rule: rule)
                AutomationGuardSummary(rule: rule)
                AutomationActivitySummary(rule: rule)
            }

            Spacer(minLength: 8)

            VStack(alignment: .trailing, spacing: 6) {
                Button("Edit…", action: onEdit)
                Button("Test") {
                    Task { await store.test(rule, model: model) }
                }
                .help("Shows what this rule would do right now. Fires nothing.")
                .disabled(store.isTesting)
                Button("Remove", action: onRemove)
            }
            .controlSize(.small)
            .disabled(store.isMutating)
        }
        .padding(11)
        .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 10))
        .opacity(rule.enabled ? 1 : 0.55)
    }
}

/// The filters a rule matches on. Values are configuration identifiers, so
/// they are shown verbatim next to a localized label.
private struct AutomationTargetSummary: View {
    let rule: MuxaAutomationRule

    var body: some View {
        if chipList.isEmpty {
            Text("Every agent on every host")
                .font(.caption)
                .foregroundStyle(.secondary)
        } else {
            HStack(spacing: 10) {
                ForEach(Array(chipList.enumerated()), id: \.offset) { _, chip in
                    AutomationChip(title: chip.title, value: chip.value)
                }
            }
        }
    }

    private var chipList: [(title: LocalizedStringKey, value: String)] {
        var chips: [(title: LocalizedStringKey, value: String)] = []
        if !rule.agent.isEmpty {
            chips.append((title: "Agent", value: rule.agent.joined(separator: ", ")))
        }
        if rule.on.supportsScopeFilter, !rule.scope.isEmpty {
            chips.append((title: "Window", value: rule.scope.joined(separator: ", ")))
        }
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
    let rule: MuxaAutomationRule

    var body: some View {
        HStack(spacing: 10) {
            if let wait = rule.wait, !wait.isEmpty {
                AutomationChip(title: "Wait", value: wait)
            }
            if rule.on.supportsResetTiming, let fallback = rule.fallback, !fallback.isEmpty {
                AutomationChip(title: "Fallback", value: fallback)
            }
            AutomationChip(title: "Cooldown", value: rule.cooldown)
            Text("\(rule.maxPerHour) per hour")
                .font(.caption2)
                .foregroundStyle(.secondary)
        }
    }
}

/// What the rule has actually done — the daemon sends both counters, so the
/// row can say how close it is to its own cap.
private struct AutomationActivitySummary: View {
    let rule: MuxaAutomationRule

    var body: some View {
        if rule.firedLastHour != nil || rule.lastFiredAt != nil {
            HStack(spacing: 10) {
                if let fired = rule.firedLastHour {
                    AutomationChip(
                        title: "This hour",
                        value: "\(fired)/\(rule.maxPerHour)"
                    )
                }
                if let date = MuxaAutomationTime.parse(rule.lastFiredAt) {
                    AutomationChip(
                        title: "Last fired",
                        value: date.formatted(date: .abbreviated, time: .shortened)
                    )
                }
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
                .font(.caption2)
                .foregroundStyle(.secondary)
            Text(verbatim: value)
                .font(.caption.monospaced())
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
        .font(.caption2.weight(.semibold))
        .padding(.horizontal, 6)
        .padding(.vertical, 2)
        .background(Color.accentColor.opacity(0.12), in: Capsule())
    }
}

private struct AutomationLogRow: View {
    let entry: MuxaAutomationLogEntry

    private var symbol: String {
        switch entry.outcome {
        case .fired: "checkmark.circle.fill"
        case .failed: "exclamationmark.triangle.fill"
        case .skipped: "minus.circle"
        case .other: "circle"
        }
    }

    private var tint: Color {
        switch entry.outcome {
        case .fired: .green
        case .failed: .red
        case .skipped, .other: .secondary
        }
    }

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: symbol)
                .foregroundStyle(tint)
                .padding(.top, 2)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 8) {
                    Text(verbatim: entry.rule).font(.callout.weight(.semibold))
                    if let action = entry.action {
                        Text(verbatim: automationActionTitle(action))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    Text(automationOutcomeTitle(entry.outcome))
                        .font(.caption)
                        .foregroundStyle(tint)
                }
                HStack(spacing: 10) {
                    if let pane = entry.pane, !pane.isEmpty {
                        AutomationChip(title: "Pane", value: pane)
                    }
                    if let agent = entry.agent, !agent.isEmpty {
                        AutomationChip(title: "Agent", value: agent)
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
            .font(.caption.monospacedDigit())
            .foregroundStyle(.secondary)
        }
        .padding(.vertical, 8)
    }

    /// A skip's detail is a reason the app can name; a firing's is the text
    /// that was sent, which is the operator's own and stays verbatim.
    @ViewBuilder
    private var detailLine: some View {
        if let reason = entry.skipReason, !reason.isEmpty {
            Text(verbatim: automationSkipReasonTitle(reason))
                .font(.caption)
                .foregroundStyle(.secondary)
        } else if let detail = entry.detail, !detail.isEmpty {
            Text(verbatim: detail)
                .font(.caption)
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
    let report: MuxaAutomationTestReport

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 8) {
                    Text(verbatim: report.rule).font(.title3.weight(.semibold))
                    Spacer()
                    if !report.engineEnabled {
                        AutomationBadge(text: String(localized: "Engine off"), symbol: "power")
                    } else if let until = MuxaAutomationTime.parse(report.pausedUntil), until > Date() {
                        AutomationBadge(text: String(localized: "Paused"), symbol: "pause.circle")
                    }
                    if !report.enabled {
                        AutomationBadge(text: String(localized: "Rule off"), symbol: "moon.zzz")
                    }
                }
                Text("Nothing was fired and nothing was recorded.")
                    .font(.caption)
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
                    .font(.caption)
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
    let candidate: MuxaAutomationTestCandidate

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: candidate.wouldFire ? "bolt.circle.fill" : "minus.circle")
                .foregroundStyle(candidate.wouldFire ? Color.accentColor : Color.secondary)
                .padding(.top, 2)
            VStack(alignment: .leading, spacing: 3) {
                Text(verbatim: automationDecisionTitle(candidate.decision))
                    .font(.callout.weight(candidate.wouldFire ? .semibold : .regular))
                HStack(spacing: 10) {
                    if let pane = candidate.pane, !pane.isEmpty {
                        AutomationChip(title: "Pane", value: pane)
                    }
                    AutomationChip(title: "Agent", value: candidate.agent)
                    AutomationChip(title: "State", value: candidate.state)
                }
                if let detail = candidate.detail, !detail.isEmpty {
                    Text(verbatim: detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
            }
            Spacer(minLength: 8)
            if let date = candidate.fireDate {
                Text(date.formatted(date: .omitted, time: .shortened))
                    .font(.caption.monospacedDigit())
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
    @State private var draft: MuxaAutomationRuleDraft
    @State private var copiedTOML = false

    init(
        draft: MuxaAutomationRuleDraft,
        existingNames: Set<String>,
        model: AppModel,
        store: AutomationStore
    ) {
        _draft = State(initialValue: draft)
        self.existingNames = existingNames
        self.model = model
        self.store = store
    }

    private var issues: [MuxaAutomationRuleIssue] {
        draft.issues(existingNames: existingNames)
    }

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section("Rule") {
                    TextField("Name", text: $draft.name)
                    Text("Used in muxa configuration and in the run log. Letters, digits, hyphens, underscores and dots.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Toggle("Enabled", isOn: $draft.enabled)
                }

                Section("Event") {
                    Picker("Fires on", selection: $draft.event) {
                        ForEach(MuxaAutomationEvent.pickable, id: \.self) { event in
                            Text(automationEventTitle(event)).tag(event)
                        }
                    }
                    Text(automationEventDetail(draft.event))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    if draft.event.requiresDuration {
                        TextField("Idle for", text: $draft.idleFor, prompt: Text(verbatim: "10m"))
                    }
                }

                Section("Filters") {
                    AutomationTokenPicker(
                        title: "Agents",
                        tokens: MuxaAutomationRuleDraft.agentKinds,
                        selection: $draft.agents
                    )
                    if draft.event.supportsScopeFilter {
                        AutomationTokenPicker(
                            title: "Limit window",
                            tokens: MuxaAutomationRuleDraft.rateLimitScopes,
                            selection: $draft.scopes
                        )
                    }
                    TextField("Workspace", text: $draft.workspace)
                    TextField("Work id matches", text: $draft.work, prompt: Text(verbatim: "^CAL-"))
                    TextField("Pane", text: $draft.pane, prompt: Text(verbatim: "%42"))
                    Picker("Host", selection: $draft.host) {
                        Text("Any").tag("")
                        ForEach(MuxaAutomationRuleDraft.hosts, id: \.self) { host in
                            Text(verbatim: host).tag(host)
                        }
                    }
                    Text("Leave a filter empty to match everything. All the filters you set must match.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                Section("Timing") {
                    TextField(
                        "Wait",
                        text: $draft.wait,
                        prompt: Text(verbatim: draft.event.supportsResetTiming ? "reset+2m" : "5m")
                    )
                    if draft.event.supportsResetTiming {
                        TextField("Fallback", text: $draft.fallback, prompt: Text(verbatim: "20m"))
                    }
                    TextField("Jitter", text: $draft.jitter, prompt: Text(verbatim: "30s"))
                    AutomationTimingPreview(draft: draft)
                }

                Section("Action") {
                    Picker("Does", selection: $draft.action) {
                        ForEach(MuxaAutomationAction.pickable, id: \.self) { action in
                            Text(automationActionTitle(action)).tag(action)
                        }
                    }
                    Text(automationActionDetail(draft.action))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    if draft.action.needsText {
                        TextField("Text", text: $draft.text, prompt: Text(verbatim: "continue"))
                            .font(.callout.monospaced())
                        Toggle("Press Enter after typing", isOn: $draft.submit)
                        Text("Without this the text is left on the agent's prompt for you to send.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    if draft.action.needsMessage {
                        TextField("Message", text: $draft.message)
                        Text("Recorded in the run log and posted by muxad. The agent is not touched.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }

                Section("Guards") {
                    Stepper(value: $draft.maxPerHour, in: 1...MuxaAutomationRule.maximumMaxPerHour) {
                        LabeledContent("Firings per hour") {
                            Text(verbatim: String(draft.maxPerHour)).monospacedDigit()
                        }
                    }
                    TextField("Cooldown", text: $draft.cooldown, prompt: Text(verbatim: "5m"))
                    Picker("Only if still", selection: $draft.onlyIfStill) {
                        Text("The event's own condition").tag(MuxaAutomationCondition?.none)
                        ForEach(MuxaAutomationCondition.pickable, id: \.self) { condition in
                            Text(automationConditionTitle(condition)).tag(MuxaAutomationCondition?.some(condition))
                        }
                    }
                    Text("Cooldown applies per pane and rule. Only if still is re-checked against the live registry when the rule fires, so an agent you resumed yourself is left alone.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                if !issues.isEmpty {
                    Section {
                        ForEach(Array(issues.enumerated()), id: \.offset) { _, issue in
                            Label(issue.message, systemImage: "exclamationmark.circle")
                                .font(.caption)
                                .foregroundStyle(.orange)
                        }
                    }
                }

                if let error = store.actionError {
                    Section {
                        Label(error, systemImage: "xmark.octagon.fill")
                            .font(.caption)
                            .foregroundStyle(.red)
                            .textSelection(.enabled)
                        Text("If this muxad cannot write rules yet, copy the rule and paste it into muxa configuration, then reload muxad.")
                            .font(.caption)
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
                .help("Copies this rule as a [[automation.rule]] block for muxa configuration.")
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
        .onChange(of: draft) { _ in copiedTOML = false }
    }
}

/// A small multi-select over configuration identifiers (agent kinds, limit
/// windows). Values are shown verbatim because they are what goes on the wire.
private struct AutomationTokenPicker: View {
    let title: LocalizedStringKey
    let tokens: [String]
    @Binding var selection: Set<String>

    var body: some View {
        LabeledContent {
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 6) {
                    ForEach(tokens, id: \.self) { token in
                        Toggle(isOn: binding(for: token)) {
                            Text(verbatim: token)
                        }
                        .toggleStyle(.button)
                        .controlSize(.small)
                    }
                }
                ForEach(extras, id: \.self) { token in
                    Toggle(isOn: binding(for: token)) {
                        Text(verbatim: token)
                    }
                    .toggleStyle(.button)
                    .controlSize(.small)
                }
                if selection.isEmpty {
                    Text("Any").font(.caption).foregroundStyle(.secondary)
                }
            }
        } label: {
            Text(title)
        }
    }

    /// Values a rule already carries that this build does not list, so
    /// editing a rule never silently drops them.
    private var extras: [String] {
        selection.subtracting(tokens).sorted()
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
    let draft: MuxaAutomationRuleDraft

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            waitLine
            if draft.event.supportsResetTiming {
                fallbackLine
            }
            jitterLine
        }
        .font(.caption)
        .foregroundStyle(.secondary)
    }

    @ViewBuilder
    private var waitLine: some View {
        switch draft.timing {
        case .afterReset(let offset) where offset == 0:
            Text("Fires as soon as the limit resets.")
        case .afterReset(let offset) where offset < 0:
            Text("Fires \(MuxaDurationText.spelled(-offset)) before the limit resets.")
        case .afterReset(let offset):
            Text("Fires \(MuxaDurationText.spelled(offset)) after the limit resets.")
        case .delay(let seconds) where seconds <= 0:
            Text("Fires as soon as the event matches.")
        case .delay(let seconds):
            Text("Fires \(MuxaDurationText.spelled(seconds)) after the event.")
        case nil:
            Text("Wait is not a duration muxad can read.")
                .foregroundStyle(.orange)
        }
    }

    @ViewBuilder
    private var fallbackLine: some View {
        if let seconds = draft.fallbackSeconds {
            if draft.fallbackIsDefault {
                Text("When the cap carries no reset time, muxad waits \(MuxaDurationText.spelled(seconds)) — its default.")
            } else {
                Text("When the cap carries no reset time, it waits \(MuxaDurationText.spelled(seconds)) instead.")
            }
        } else {
            Text("Fallback is not a duration muxad can read.")
                .foregroundStyle(.orange)
        }
    }

    @ViewBuilder
    private var jitterLine: some View {
        if let seconds = draft.jitterSeconds {
            if draft.jitterIsDefault {
                Text("Up to \(MuxaDurationText.spelled(seconds)) of random delay is added — muxad's default, so several agents do not resume at once.")
            } else {
                Text("Up to \(MuxaDurationText.spelled(seconds)) of random delay is added.")
            }
        } else {
            Text("Jitter is not a duration muxad can read.")
                .foregroundStyle(.orange)
        }
    }
}

/// Seconds as words, through the system formatter so Korean reads right.
enum MuxaDurationText {
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
/// `MuxaIPC+Automation` stays a transport file. `other` shows the daemon's
/// own token rather than inventing a name for it.
func automationEventTitle(_ event: MuxaAutomationEvent) -> String {
    switch event {
    case .rateLimited: String(localized: "Rate limited")
    case .waitingInput: String(localized: "Waiting for input")
    case .idleFor: String(localized: "Idle for")
    case .error: String(localized: "Error")
    case .other(let raw): raw
    }
}

func automationEventDetail(_ event: MuxaAutomationEvent) -> String {
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
        String(localized: "An event this build of Muxa does not know.")
    }
}

func automationActionTitle(_ action: MuxaAutomationAction) -> String {
    switch action {
    case .sendPrompt: String(localized: "Send prompt")
    case .notify: String(localized: "Notify")
    case .interrupt: String(localized: "Interrupt")
    case .other(let raw): raw
    }
}

func automationActionDetail(_ action: MuxaAutomationAction) -> String {
    switch action {
    case .sendPrompt:
        String(localized: "Types the text into the agent's pane and presses Enter.")
    case .notify:
        String(localized: "Records a notice in the run log. The agent is not touched.")
    case .interrupt:
        String(localized: "Sends the agent an interrupt, as Escape would.")
    case .other:
        String(localized: "An action this build of Muxa does not know.")
    }
}

func automationConditionTitle(_ condition: MuxaAutomationCondition) -> String {
    switch condition {
    case .rateLimited: String(localized: "Still rate limited")
    case .waitingInput: String(localized: "Still waiting for input")
    case .idle: String(localized: "Still idle")
    case .error: String(localized: "Still in error")
    case .any: String(localized: "Whatever the agent is doing")
    case .other(let raw): raw
    }
}

func automationOutcomeTitle(_ outcome: MuxaAutomationOutcome) -> String {
    switch outcome {
    case .fired: String(localized: "Fired")
    case .skipped: String(localized: "Skipped")
    case .failed: String(localized: "Failed")
    case .other(let raw): raw
    }
}

/// muxad's `SkipReason` tokens, said in words. An unknown token is shown as
/// it arrived rather than hidden.
func automationSkipReasonTitle(_ reason: String) -> String {
    switch reason {
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
    default: reason
    }
}

/// `automation_test`'s `decision`: `fire`, or one of the skip reasons.
func automationDecisionTitle(_ decision: String) -> String {
    decision == "fire" ? String(localized: "Would fire") : automationSkipReasonTitle(decision)
}
