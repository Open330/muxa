// Fleet Home's one line of automatic handling, and the rules behind it.
import SwiftUI

/// The header control: what muxad handles on its own, in one line, with the
/// policies one click away.
struct FleetAutomationButton: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AutomationStore = .shared
    let addApproval: () -> Void
    @State private var showsPolicies = false

    private var handledCount: Int { FleetAutomationSummary.handled(store.log).count }

    var body: some View {
        Button {
            showsPolicies.toggle()
        } label: {
            HStack(spacing: 6) {
                Image(systemName: isActive ? "wand.and.stars" : "wand.and.rays.inverse")
                    .foregroundStyle(isActive ? Color.accentColor : Color.secondary)
                Text(label)
                    .monospacedDigit()
            }
        }
        .buttonStyle(.muxaGhost)
        .help("What Muxa handles on its own")
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
        return handledCount > 0
            ? String(localized: "Handled \(handledCount) today")
            : String(localized: "Automatic: on")
    }
}

private struct FleetAutomationPopover: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AutomationStore
    let addApproval: () -> Void
    @State private var removing: String?

    private var approvals: [MuxaAutomationRule] {
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
                    Text("This muxad cannot run automations; update muxa to use them.")
                        .font(MuxaType.detail)
                        .foregroundStyle(.secondary)
                } else {
                    if !store.snapshot.enabled {
                        Text("Nothing below runs until automatic handling is on.")
                            .font(MuxaType.meta)
                            .foregroundStyle(.orange)
                    }
                    MuxaLimitPolicyPicker(model: model, store: store, compact: true)
                    Divider()
                    approvalsSection
                    Divider()
                    recentSection
                }
                HStack {
                    Spacer()
                    Button("All Rules…") {
                        MuxaSettingsOpener.select(.automations)
                        MuxaSettingsOpener.openLegacySettingsWindow()
                    }
                    .buttonStyle(.muxaGhost)
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
                .font(MuxaType.detail.weight(.semibold))
            Text("Only requests you name here. Muxa presses the one-time Yes, never \"don't ask again\", and only on agents on this Mac.")
                .font(MuxaType.meta)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if approvals.isEmpty {
                Text("No approval rules. Every permission request waits for you.")
                    .font(MuxaType.meta)
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
            .buttonStyle(.muxaSecondary)
            .controlSize(.small)
            .disabled(store.isMutating)
        }
    }

    private func approvalRow(_ rule: MuxaAutomationRule) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Toggle(rule.name, isOn: Binding(
                    get: { rule.enabled },
                    set: { enabled in Task { await store.setEnabled(rule, enabled: enabled, model: model) } }
                ))
                .toggleStyle(.checkbox)
                .font(MuxaType.detail.weight(.medium))
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
                    .buttonStyle(.muxaGhost)
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
                    .font(MuxaType.meta)
                    .foregroundStyle(.tertiary)
            }
        }
        .disabled(store.isMutating)
    }

    private func scopeText(_ rule: MuxaAutomationRule) -> String? {
        var parts: [String] = []
        if !rule.agent.isEmpty { parts.append(rule.agent.map(MuxaAgentMark.title(for:)).joined(separator: ", ")) }
        if rule.work != nil { parts.append(String(localized: "one Work")) }
        if let fired = rule.firedLastHour, fired > 0 { parts.append(String(localized: "\(fired) in the last hour")) }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    private var recentSection: some View {
        let handled = FleetAutomationSummary.handled(store.log)
        return VStack(alignment: .leading, spacing: 6) {
            Text("Handled today")
                .font(MuxaType.detail.weight(.semibold))
            if handled.isEmpty {
                Text("Nothing yet.")
                    .font(MuxaType.meta)
                    .foregroundStyle(.tertiary)
            }
            ForEach(Array(handled.prefix(6).enumerated()), id: \.offset) { _, entry in
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text(entry.rule)
                        .font(MuxaType.meta.weight(.medium))
                    if let pane = entry.pane {
                        Text(pane)
                            .font(MuxaType.meta)
                            .foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 4)
                    if let at = entry.firedAt.flatMap(MuxaUsageTimestamp.parse) {
                        Text(at, format: .relative(presentation: .numeric, unitsStyle: .abbreviated))
                            .font(MuxaType.meta)
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
    var agent: MuxaHostedAgent?
    var work: MuxaWorkIdentity?
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
    @State private var request: String?
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
            Text("Approve Requests Like This")
                .font(.title3.weight(.semibold))
            Text("When an agent on this Mac asks for permission and the request contains this text, Muxa presses the one-time Yes. Anything else still waits for you.")
                .font(MuxaType.detail)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            if seed.agent != nil {
                requestPreview
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("Request text")
                    .font(MuxaType.detail.weight(.semibold))
                TextField(isPattern ? "Regular expression, e.g. ^npm (test|run lint)$" : "e.g. npm test", text: $text)
                    .textFieldStyle(.roundedBorder)
                    .font(.system(size: 12, design: .monospaced))
                Toggle("Regular expression", isOn: $isPattern)
                    .toggleStyle(.checkbox)
                    .font(MuxaType.meta)
                if isPattern && !trimmed.isEmpty && !FleetApprovalRules.isValidPattern(trimmed) {
                    Text("This is not a valid regular expression.")
                        .font(MuxaType.meta)
                        .foregroundStyle(.red)
                } else if let request, !trimmed.isEmpty {
                    let found = FleetApprovalRules.matches(trimmed, isPattern: isPattern, request: request)
                    Label(
                        found ? String(localized: "Finds the request on screen") : String(localized: "Does not find the request on screen"),
                        systemImage: found ? "checkmark.circle.fill" : "xmark.circle"
                    )
                    .font(MuxaType.meta)
                    .foregroundStyle(found ? Color.green : Color.orange)
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("Agents")
                    .font(MuxaType.detail.weight(.semibold))
                HStack(spacing: 14) {
                    ForEach(FleetApprovalRules.supportedKinds, id: \.self) { kind in
                        Toggle(MuxaAgentMark.title(for: kind), isOn: Binding(
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
                    .font(MuxaType.meta)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let error {
                Text(error)
                    .font(MuxaType.meta)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            }

            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Save Rule", action: save)
                    .buttonStyle(.muxaPrimary)
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
            Text("Request on screen")
                .font(MuxaType.detail.weight(.semibold))
            Group {
                if loadingRequest {
                    ProgressView().controlSize(.small)
                } else if let request {
                    Text(request)
                        .font(.system(size: 11, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    Text("No confirmation is on screen right now.")
                        .font(MuxaType.meta)
                        .foregroundStyle(.secondary)
                }
            }
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: 7, style: .continuous))
            Text("Copy the part that should always be allowed, such as the command, into Request text.")
                .font(MuxaType.meta)
                .foregroundStyle(.tertiary)
        }
    }

    private func loadRequest() async {
        guard let hosted = seed.agent, let pane = hosted.pane else { return }
        loadingRequest = true
        defer { loadingRequest = false }
        if let capture = try? await model.client.captureFleetPane(host: hosted.host, pane: pane),
           let screen = capture.screenText {
            request = FleetApprovalRules.dialogText(from: screen)
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
