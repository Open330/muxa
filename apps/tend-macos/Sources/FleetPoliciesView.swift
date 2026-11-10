// Fleet Home's one line of automatic handling, and the rules behind it.
import SwiftUI

/// The header control: what tendd handles on its own, in one line, with the
/// policies one click away.
struct FleetAutomationButton: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AutomationStore = .shared
    let addApproval: () -> Void
    @State private var showsPolicies = false

    var body: some View {
        Button {
            showsPolicies.toggle()
        } label: {
            Image(systemName: isActive ? "wand.and.stars" : "wand.and.rays.inverse")
                .foregroundStyle(isActive ? Color.accentColor : Color.secondary)
        }
        .buttonStyle(.tendGhost)
        .help(label)
        .accessibilityLabel(label)
        .popover(isPresented: $showsPolicies, arrowEdge: .bottom) {
            FleetAutomationPopover(model: model, store: store, addApproval: {
                showsPolicies = false
                addApproval()
            })
        }
        .task(id: model.isConnected) {
            if model.isConnected { await store.reload(model: model) }
        }
    }

    private var isActive: Bool {
        store.isSupported && store.snapshot.enabled && !store.snapshot.isPaused()
    }

    private var label: String {
        guard store.isSupported else { return String(localized: "Automatic: unavailable") }
        guard store.snapshot.enabled else { return String(localized: "Automatic: off") }
        if store.snapshot.isPaused() { return String(localized: "Automatic: paused") }
        return String(localized: "Automatic: on")
    }
}

private struct FleetAutomationPopover: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AutomationStore
    let addApproval: () -> Void
    @State private var removing: String?

    private var approvals: [TendAutomationRule] {
        store.snapshot.rules.filter(FleetApprovalRules.isApproval)
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                HStack {
                    Text("Automatic handling")
                        .font(.headline)
                    Spacer()
                    if store.isSupported {
                        Toggle("On", isOn: Binding(
                            get: { store.snapshot.enabled },
                            set: { enabled in Task { await store.setMasterEnabled(enabled, model: model) } }
                        ))
                        .toggleStyle(.switch)
                        .controlSize(.small)
                        .labelsHidden()
                        .disabled(store.isMutating)
                    }
                }
                if !store.isSupported {
                    Text("This tendd cannot run automations; update tend to use them.")
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                } else {
                    if !store.snapshot.enabled {
                        Text("Nothing below runs until automatic handling is on.")
                            .font(TendType.meta)
                            .foregroundStyle(.orange)
                    }
                    TendLimitPolicyPicker(model: model, store: store, compact: true)
                    Divider()
                    approvalsSection
                    Divider()
                    recentSection
                }
                HStack {
                    Spacer()
                    Button("All Rules…") {
                        TendSettingsOpener.select(.automations)
                        TendSettingsOpener.openLegacySettingsWindow()
                    }
                    .buttonStyle(.tendGhost)
                    .controlSize(.small)
                }
            }
            .padding(16)
        }
        .frame(width: 380)
        .frame(maxHeight: 560)
    }

    private var approvalsSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Approve on its own")
                .font(TendType.detail.weight(.semibold))
            Text("Only shell commands you name here, matched in full. Tend presses the one-time Yes, never \"don't ask again\", and only on agents on this Mac.")
                .font(TendType.meta)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if approvals.isEmpty {
                Text("No approval rules. Every permission request waits for you.")
                    .font(TendType.meta)
                    .foregroundStyle(.tertiary)
            }
            ForEach(approvals, id: \.name) { rule in
                approvalRow(rule)
            }
            Button {
                addApproval()
            } label: {
                Label("Add Approval Rule…", systemImage: "plus")
            }
            .buttonStyle(.tendSecondary)
            .controlSize(.small)
            .disabled(store.isMutating)
        }
    }

    private func approvalRow(_ rule: TendAutomationRule) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Toggle(rule.name, isOn: Binding(
                    get: { rule.enabled },
                    set: { enabled in Task { await store.setEnabled(rule, enabled: enabled, model: model) } }
                ))
                .toggleStyle(.checkbox)
                .font(TendType.detail.weight(.medium))
                Spacer(minLength: 4)
                if removing == rule.name {
                    Button("Remove", role: .destructive) {
                        removing = nil
                        Task { _ = await store.remove(rule, model: model) }
                    }
                    .controlSize(.small)
                    Button("Keep") { removing = nil }
                        .controlSize(.small)
                } else {
                    Button {
                        removing = rule.name
                    } label: {
                        Image(systemName: "trash")
                    }
                    .buttonStyle(.tendGhost)
                    .help("Remove this rule")
                }
            }
            Text(rule.screen ?? "")
                .font(.system(size: 11, design: .monospaced))
                .foregroundStyle(.secondary)
                .lineLimit(2)
                .textSelection(.enabled)
            if let scope = scopeText(rule) {
                Text(scope)
                    .font(TendType.meta)
                    .foregroundStyle(.tertiary)
            }
        }
        .disabled(store.isMutating)
    }

    private func scopeText(_ rule: TendAutomationRule) -> String? {
        var parts: [String] = []
        if !rule.agent.isEmpty { parts.append(rule.agent.map(TendAgentMark.title(for:)).joined(separator: ", ")) }
        if rule.work != nil { parts.append(String(localized: "one Work")) }
        if let fired = rule.firedLastHour, fired > 0 { parts.append(String(localized: "\(fired) in the last hour")) }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    private var recentSection: some View {
        let handled = FleetAutomationSummary.handled(store.log)
        return VStack(alignment: .leading, spacing: 6) {
            Text("Handled today")
                .font(TendType.detail.weight(.semibold))
            if handled.isEmpty {
                Text("Nothing yet.")
                    .font(TendType.meta)
                    .foregroundStyle(.tertiary)
            }
            ForEach(Array(handled.prefix(6).enumerated()), id: \.offset) { _, entry in
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text(entry.rule)
                        .font(TendType.meta.weight(.medium))
                    if let pane = entry.pane {
                        Text(pane)
                            .font(TendType.meta)
                            .foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 4)
                    if let at = entry.firedAt.flatMap(TendUsageTimestamp.parse) {
                        Text(at, format: .relative(presentation: .numeric, unitsStyle: .abbreviated))
                            .font(TendType.meta)
                            .foregroundStyle(.tertiary)
                            .monospacedDigit()
                    }
                }
            }
        }
    }
}

/// What the approval sheet starts from: the agent whose request is on
/// screen, when the person came from one.
struct FleetApprovalSeed: Identifiable {
    let id = UUID()
    var agent: TendHostedAgent?
    var work: TendWorkIdentity?
}

/// Writes one approval rule: the request text to match, which agents, and
/// optionally only one Work. Shows the request on screen when there is one,
/// and whether the text finds it.
struct FleetApprovalRuleSheet: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AutomationStore = .shared
    let seed: FleetApprovalSeed
    @Environment(\.dismiss) private var dismiss

    @State private var text = ""
    @State private var isPattern = false
    @State private var agents: Set<String> = Set(FleetApprovalRules.supportedKinds)
    @State private var onlyThisWork = false
    @State private var request: [String]?
    @State private var loadingRequest = false
    @State private var saving = false
    @State private var error: String?

    private var trimmed: String { text.trimmingCharacters(in: .whitespacesAndNewlines) }

    private var canSave: Bool {
        !trimmed.isEmpty && !agents.isEmpty && !saving
            && (!isPattern || FleetApprovalRules.isValidPattern(trimmed))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Always Approve This Command")
                .font(.title3.weight(.semibold))
            Text("When an agent on this Mac asks to run exactly this command, Tend presses the one-time Yes. Any other command, or anything Tend cannot read with certainty, still waits for you.")
                .font(TendType.detail)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            if seed.agent != nil {
                requestPreview
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("Command")
                    .font(TendType.detail.weight(.semibold))
                TextField(isPattern ? "Regular expression for the whole command, e.g. npm (test|run lint)" : "e.g. npm test", text: $text)
                    .textFieldStyle(.roundedBorder)
                    .font(.system(size: 12, design: .monospaced))
                Toggle("Regular expression", isOn: $isPattern)
                    .toggleStyle(.checkbox)
                    .font(TendType.meta)
                if isPattern && !trimmed.isEmpty && !FleetApprovalRules.isValidPattern(trimmed) {
                    Text("This is not a valid regular expression.")
                        .font(TendType.meta)
                        .foregroundStyle(.red)
                } else if let request, !trimmed.isEmpty {
                    let found = FleetApprovalRules.matches(trimmed, isPattern: isPattern, command: request)
                    Label(
                        found ? String(localized: "Matches the command on screen") : String(localized: "Does not match the command on screen"),
                        systemImage: found ? "checkmark.circle.fill" : "xmark.circle"
                    )
                    .font(TendType.meta)
                    .foregroundStyle(found ? Color.green : Color.orange)
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("Agents")
                    .font(TendType.detail.weight(.semibold))
                HStack(spacing: 14) {
                    ForEach(FleetApprovalRules.supportedKinds, id: \.self) { kind in
                        Toggle(TendAgentMark.title(for: kind), isOn: Binding(
                            get: { agents.contains(kind) },
                            set: { on in if on { agents.insert(kind) } else { agents.remove(kind) } }
                        ))
                        .toggleStyle(.checkbox)
                    }
                }
                if seed.work != nil {
                    Toggle("Only in this Work", isOn: $onlyThisWork)
                        .toggleStyle(.checkbox)
                }
            }

            if !store.snapshot.enabled {
                Label("Automatic handling is off. The rule is saved but does nothing until you turn it on.", systemImage: "exclamationmark.triangle")
                    .font(TendType.meta)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let error {
                Text(error)
                    .font(TendType.meta)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            }

            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Save Rule", action: save)
                    .buttonStyle(.tendPrimary)
                    .keyboardShortcut(.defaultAction)
                    .disabled(!canSave)
            }
        }
        .padding(20)
        .frame(width: 480)
        .task { await loadRequest() }
        .onAppear {
            if let kind = seed.agent?.agent.kind, FleetApprovalRules.supportedKinds.contains(kind) {
                agents = [kind]
            }
        }
    }

    private var requestPreview: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Command on screen")
                .font(TendType.detail.weight(.semibold))
            Group {
                if loadingRequest {
                    ProgressView().controlSize(.small)
                } else if let request {
                    Text(request.joined(separator: "\n"))
                        .font(.system(size: 11, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    Text("No command confirmation is on screen right now.")
                        .font(TendType.meta)
                        .foregroundStyle(.secondary)
                }
            }
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: 7, style: .continuous))
            Text("Copy the command into Command. Only a Codex or Claude Code shell command dialog can be approved; a Claude description line is not part of the command.")
                .font(TendType.meta)
                .foregroundStyle(.tertiary)
        }
    }

    private func loadRequest() async {
        guard let hosted = seed.agent, let pane = hosted.pane else { return }
        loadingRequest = true
        defer { loadingRequest = false }
        if let capture = try? await model.client.captureFleetPane(host: hosted.host, pane: pane),
           let screen = capture.screenText {
            request = FleetApprovalRules.commandPreview(from: screen)
        }
    }

    private func save() {
        guard canSave else { return }
        saving = true
        error = nil
        let rule = FleetApprovalRules.make(
            text: trimmed,
            isPattern: isPattern,
            agents: Array(agents),
            work: onlyThisWork ? seed.work : nil,
            existingNames: Set(store.snapshot.rules.map(\.name))
        )
        Task {
            defer { saving = false }
            if await store.save(rule, model: model) {
                dismiss()
            } else {
                error = store.actionError ?? String(localized: "The rule was not saved.")
            }
        }
    }
}
