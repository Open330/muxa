import SwiftUI

/// What Tend does when an agent hits its usage limit: one plain choice
/// instead of a rule editor. Each choice is a pair of automation rules the
/// daemon already understands, so the CLI, `config.toml` and the Automations
/// pane all see the same thing.
enum TendLimitPolicy: Hashable, Sendable, CaseIterable, Identifiable {
    /// Relaunch the agent on an `aas` account with quota left and carry on.
    case switchAccount
    /// Wait for the window to reset, then type `continue`.
    case waitForReset
    /// Do nothing on its own: the capped agent needs the person.
    case tellMe

    var id: Self { self }

    /// The rule the switch choice owns.
    static let swapRuleName = "swap-on-limit"
    /// The starter tendd installs (disabled) for the wait choice.
    static let resumeRuleName = "resume-after-limit"

    var title: String {
        switch self {
        case .switchAccount: String(localized: "Switch to another account and continue")
        case .waitForReset: String(localized: "Wait for the reset, then continue")
        case .tellMe: String(localized: "Just tell me")
        }
    }

    var detail: String {
        switch self {
        case .switchAccount:
            String(localized: "Tend relaunches the same conversation on the aas account with the most quota left and carries the cut-off turn on. Needs aas and a second account.")
        case .waitForReset:
            String(localized: "Tend types continue two minutes after the limit resets (or after 20 minutes when no reset time is known).")
        case .tellMe:
            String(localized: "Capped agents count as needing you and send a notification.")
        }
    }

    var systemImage: String {
        switch self {
        case .switchAccount: "arrow.triangle.2.circlepath"
        case .waitForReset: "clock.arrow.circlepath"
        case .tellMe: "bell"
        }
    }

    /// What the live rules amount to. A custom rule on `rate_limited` counts
    /// by what it does; a switch rule wins over a resume rule, as it fires
    /// first (on the cap, not at the reset).
    static func current(enabled: Bool, rules: [TendAutomationRule]) -> TendLimitPolicy {
        guard enabled else { return .tellMe }
        let live = rules.filter { $0.enabled && $0.on == .rateLimited }
        if live.contains(where: { $0.action == .swapAccount }) { return .switchAccount }
        if live.contains(where: { $0.action == .sendPrompt }) { return .waitForReset }
        return .tellMe
    }

    /// The rule edits that make `self` the policy, against the rules there
    /// are now. Rules the user wrote by hand on `rate_limited` are switched
    /// off when they would contradict the choice, never removed.
    func plan(for rules: [TendAutomationRule]) -> TendLimitPolicyPlan {
        var plan = TendLimitPolicyPlan()
        let capRules = rules.filter { $0.on == .rateLimited }
        switch self {
        case .switchAccount:
            if let swap = rules.first(where: { $0.name == Self.swapRuleName }) {
                if !swap.enabled { plan.enable.append(swap.name) }
            } else {
                plan.create = Self.swapRule
            }
            plan.disable = capRules
                .filter { $0.enabled && $0.name != Self.swapRuleName && $0.action == .sendPrompt }
                .map(\.name)
        case .waitForReset:
            if let resume = rules.first(where: { $0.name == Self.resumeRuleName }) {
                if !resume.enabled { plan.enable.append(resume.name) }
            } else {
                plan.create = TendAutomationRule.sessionLimitRecommendation
            }
            plan.disable = capRules
                .filter { $0.enabled && $0.action == .swapAccount }
                .map(\.name)
        case .tellMe:
            plan.disable = capRules
                .filter { $0.enabled && ($0.action == .swapAccount || $0.action == .sendPrompt) }
                .map(\.name)
        }
        plan.enablesEngine = self != .tellMe
        return plan
    }

    /// The switch rule as the app writes it: on the cap, any Claude Code or
    /// Codex agent, the best-ranked account, the `[swap]` follow-up.
    static var swapRule: TendAutomationRule {
        TendAutomationRule(
            name: swapRuleName,
            on: .rateLimited,
            agent: ["claude_code", "codex"],
            action: .swapAccount,
            maxPerHour: 3,
            cooldown: "5m"
        )
    }
}

/// The edits a policy choice needs.
struct TendLimitPolicyPlan: Equatable, Sendable {
    var create: TendAutomationRule?
    var enable: [String] = []
    var disable: [String] = []
    var enablesEngine = false
}

extension AutomationStore {
    /// The limit policy the loaded rules amount to; nil until loaded, or on
    /// a daemon without automations.
    var limitPolicy: TendLimitPolicy? {
        guard hasLoaded, isSupported else { return nil }
        return TendLimitPolicy.current(enabled: masterEnabled, rules: snapshot.rules)
    }

    /// Make `policy` the limit policy.
    @discardableResult
    func apply(_ policy: TendLimitPolicy, model: AppModel) async -> Bool {
        let plan = policy.plan(for: snapshot.rules)
        if let rule = plan.create, !(await save(rule, model: model)) { return false }
        for name in plan.enable {
            guard let rule = snapshot.rules.first(where: { $0.name == name }) else { continue }
            await setEnabled(rule, enabled: true, model: model)
        }
        for name in plan.disable {
            guard let rule = snapshot.rules.first(where: { $0.name == name }) else { continue }
            await setEnabled(rule, enabled: false, model: model)
        }
        if plan.enablesEngine, !masterEnabled {
            await setMasterEnabled(true, model: model)
        }
        return actionError == nil
    }
}

// MARK: - View

/// The three choices, with the current one checked. Used in the rate-limit
/// popover, the usage popover and Settings › Automations.
struct TendLimitPolicyPicker: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AutomationStore = .shared
    var compact = false

    var body: some View {
        VStack(alignment: .leading, spacing: compact ? 4 : 8) {
            Text("When an agent hits its usage limit")
                .font(compact ? TendType.detail.weight(.semibold) : .headline)
            if !store.hasLoaded {
                ProgressView().controlSize(.small)
            } else if !store.isSupported {
                Text("This tendd cannot run automations; update tend to choose.")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
            } else {
                ForEach(TendLimitPolicy.allCases) { policy in
                    row(policy)
                }
                Text("Applies to agents on this Mac. Fleet hosts follow their own tendd's rules, so a capped agent there still asks for you.")
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if store.snapshot.isPaused() {
                    Label("Automations are paused, so nothing happens on its own until they resume.", systemImage: "pause.circle")
                        .font(TendType.meta)
                        .foregroundStyle(.orange)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if let error = store.actionError {
                    Text(error)
                        .font(TendType.meta)
                        .foregroundStyle(.red)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
        .task {
            if !store.hasLoaded { await store.reload(model: model) }
        }
    }

    private func row(_ policy: TendLimitPolicy) -> some View {
        let selected = store.limitPolicy == policy
        return Button {
            guard !selected else { return }
            Task { await store.apply(policy, model: model) }
        } label: {
            HStack(alignment: .top, spacing: 8) {
                Image(systemName: selected ? "largecircle.fill.circle" : "circle")
                    .foregroundStyle(selected ? Color.accentColor : Color.secondary)
                    .frame(width: 14)
                VStack(alignment: .leading, spacing: 2) {
                    Label(policy.title, systemImage: policy.systemImage)
                        .font(TendType.detail.weight(selected ? .semibold : .regular))
                    if !compact || selected {
                        Text(policy.detail)
                            .font(TendType.meta)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                Spacer(minLength: 0)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(store.isMutating)
    }
}
