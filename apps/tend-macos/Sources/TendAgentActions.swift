import SwiftUI

// MARK: - Naming

extension TendHostedAgent {
    /// The one name every surface gives this agent: its `@alias`, else the
    /// provider's title for the conversation, else the pane title, else
    /// the agent kind.
    var displayTitle: String {
        pane?.agentAlias.map { "@\($0)" }
            ?? agent.aiTitle?.nonEmpty
            ?? pane?.title.nonEmpty
            ?? agent.kind.replacingOccurrences(of: "_", with: " ")
    }
}

// MARK: - Actions

/// What can be done to one agent, from wherever it is shown.
///
/// State implies action: a capped agent's first action is to switch it to
/// an account with quota, a blocked agent's is to answer it. Every surface
/// that lists an agent (Explore, the pane editor, agent detail, Command
/// Center, the rate-limit popover, the palette) asks this type, so each
/// offers the same actions under the same names.
struct TendAgentActionTarget: Sendable {
    let host: TendFleetHostIdentity
    let agent: TendAgent
    let pane: TendPaneInfo?

    init(host: TendFleetHostIdentity, agent: TendAgent, pane: TendPaneInfo?) {
        self.host = host
        self.agent = agent
        self.pane = pane
    }

    init(_ hosted: TendHostedAgent) {
        self.init(host: hosted.host, agent: hosted.agent, pane: hosted.pane)
    }

    init?(_ pane: TendWatchPane) {
        guard let agent = pane.agent else { return nil }
        self.init(host: pane.host, agent: agent, pane: pane.pane)
    }

    var status: TendAgentStatus { agent.status }

    /// `aas` relaunches Claude Code and Codex; nothing else can be swapped.
    var canSwitchAccount: Bool {
        TendAgentActionTarget.switchableKinds.contains(agent.kind)
    }

    static let switchableKinds: Set<String> = ["claude_code", "codex"]

    var title: String {
        TendHostedAgent(host: host, agent: agent, pane: pane).displayTitle
    }

    var hostArgument: String? { host.local ? nil : host.alias }

    var swapSheetTarget: TendSwapSheetTarget {
        TendSwapSheetTarget(
            host: hostArgument,
            scope: .agents([agent.agentSessionID]),
            label: title,
            agentKind: agent.kind,
            interruptsWork: status == .working
        )
    }
}

enum TendAgentActionKind: Hashable, Sendable {
    /// Capped: move to another account and carry on (the primary action).
    case switchAccountAndResume
    /// Not capped: move to another account anyway.
    case moveToAccount
    /// Type `continue` into the agent: unblocks a stalled or errored turn.
    case sendContinue
}

struct TendAgentAction: Identifiable, Hashable, Sendable {
    let kind: TendAgentActionKind
    let isPrimary: Bool
    /// A swap for this agent is already running; the action shows it
    /// instead of starting a second one.
    let isInProgress: Bool

    var id: TendAgentActionKind { kind }

    var title: String {
        switch kind {
        case .switchAccountAndResume:
            isInProgress ? String(localized: "Switching Account…") : String(localized: "Switch Account and Resume…")
        case .moveToAccount:
            String(localized: "Move to Another Account…")
        case .sendContinue:
            String(localized: "Send “continue”")
        }
    }

    var systemImage: String {
        switch kind {
        case .switchAccountAndResume, .moveToAccount: "arrow.triangle.2.circlepath"
        case .sendContinue: "arrow.uturn.forward"
        }
    }

    var help: String {
        switch kind {
        case .switchAccountAndResume:
            String(localized: "Relaunch this conversation on an aas account with quota left and carry the interrupted turn on")
        case .moveToAccount:
            String(localized: "Relaunch this conversation on another aas account")
        case .sendContinue:
            String(localized: "Type continue into the agent and press Return")
        }
    }
}

enum TendAgentActions {
    /// The actions `target`'s state calls for, primary first.
    @MainActor
    static func actions(for target: TendAgentActionTarget, swaps: TendSwapCenter?) -> [TendAgentAction] {
        let swapping = swaps?.runningSwap(for: target.agent.agentSessionID) != nil
        var actions: [TendAgentAction] = []
        switch target.status {
        case .limited:
            if target.canSwitchAccount {
                actions.append(.init(kind: .switchAccountAndResume, isPrimary: true, isInProgress: swapping))
            }
        case .needsInput, .error:
            if target.pane != nil {
                actions.append(.init(kind: .sendContinue, isPrimary: false, isInProgress: false))
            }
            if target.canSwitchAccount {
                actions.append(.init(kind: .moveToAccount, isPrimary: false, isInProgress: swapping))
            }
        // An agent that asked is at its prompt: the answer, not "continue".
        case .asked, .working, .idle, .done, .other:
            if target.canSwitchAccount {
                actions.append(.init(kind: .moveToAccount, isPrimary: false, isInProgress: swapping))
            }
        }
        return actions
    }

    @MainActor
    static func perform(_ action: TendAgentAction, on target: TendAgentActionTarget, model: AppModel) {
        guard !action.isInProgress else { return }
        switch action.kind {
        case .switchAccountAndResume, .moveToAccount:
            model.swaps.present(target.swapSheetTarget)
        case .sendContinue:
            guard let pane = target.pane else { return }
            Task {
                do {
                    // Prompt text, not UI chrome: typed into the agent.
                    try await model.client.sendFleetPrompt(host: target.host, pane: pane, text: "continue")
                    model.reportAgentActionDone(String(localized: "Sent “continue” to \(target.title)"))
                } catch {
                    model.reportAgentActionError(error.localizedDescription)
                }
            }
        }
    }
}

extension AppModel {
    /// Surfaces an agent action that failed after its control was clicked.
    /// The status bar shows the message until the next action.
    func reportAgentActionError(_ message: String) {
        agentActionNotice = nil
        agentActionError = message
        clearAgentActionMessage(after: .seconds(20)) { $0.agentActionError == message ? \.agentActionError : nil }
    }

    /// Confirms an action in the status bar for a few seconds.
    func reportAgentActionDone(_ message: String) {
        agentActionError = nil
        agentActionNotice = message
        clearAgentActionMessage(after: .seconds(4)) { $0.agentActionNotice == message ? \.agentActionNotice : nil }
    }

    private func clearAgentActionMessage(
        after delay: Duration,
        _ field: @escaping @MainActor (AppModel) -> ReferenceWritableKeyPath<AppModel, String?>?
    ) {
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: delay)
            guard let self, let path = field(self) else { return }
            self[keyPath: path] = nil
        }
    }
}

// MARK: - Views

/// The agent's actions as menu items: for context menus and "…" menus.
struct TendAgentActionMenuItems: View {
    @ObservedObject var model: AppModel
    let target: TendAgentActionTarget

    var body: some View {
        let actions = TendAgentActions.actions(for: target, swaps: model.swaps)
        if !actions.isEmpty {
            ForEach(actions) { action in
                Button {
                    TendAgentActions.perform(action, on: target, model: model)
                } label: {
                    Label(action.title, systemImage: action.systemImage)
                }
                .disabled(action.isInProgress)
                .help(action.help)
            }
        }
    }
}

/// The primary action as a button, for a row or header. Renders nothing
/// when the agent's state calls for no action.
struct TendAgentPrimaryActionButton: View {
    @ObservedObject var model: AppModel
    @ObservedObject private var swaps: TendSwapCenter
    let target: TendAgentActionTarget
    var compact = false

    init(model: AppModel, target: TendAgentActionTarget, compact: Bool = false) {
        self.model = model
        swaps = model.swaps
        self.target = target
        self.compact = compact
    }

    var body: some View {
        if let action = TendAgentActions.actions(for: target, swaps: swaps).first(where: \.isPrimary) {
            Button {
                TendAgentActions.perform(action, on: target, model: model)
            } label: {
                if action.isInProgress {
                    HStack(spacing: 4) {
                        ProgressView().controlSize(.mini)
                        if !compact { Text(action.title) }
                    }
                } else if compact {
                    Image(systemName: action.systemImage)
                } else {
                    Label(action.title, systemImage: action.systemImage)
                }
            }
            .buttonStyle(.tendSecondary)
            .disabled(action.isInProgress)
            .help(action.help)
        }
    }
}

private extension String {
    var nonEmpty: String? { isEmpty ? nil : self }
}

/// The primary action as a button plus every action in a menu: for an
/// agent's header (pane editor, agent detail).
struct TendAgentActionBar: View {
    @ObservedObject var model: AppModel
    @ObservedObject private var swaps: TendSwapCenter
    let target: TendAgentActionTarget

    init(model: AppModel, target: TendAgentActionTarget) {
        self.model = model
        swaps = model.swaps
        self.target = target
    }

    var body: some View {
        let actions = TendAgentActions.actions(for: target, swaps: swaps)
        HStack(spacing: 6) {
            TendAgentPrimaryActionButton(model: model, target: target)
            if actions.contains(where: { !$0.isPrimary }) {
                Menu {
                    TendAgentActionMenuItems(model: model, target: target)
                } label: {
                    Label("Agent Actions", systemImage: "ellipsis.circle")
                        .labelStyle(.iconOnly)
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
                .help("Agent actions")
            }
        }
    }
}

/// "Switch All…" for a set of capped agents: one sheet per host, since a
/// swap only ever moves one host's agents. With several hosts it is a menu.
struct TendSwitchLimitedButton: View {
    @ObservedObject var model: AppModel
    let limited: [TendHostedAgent]

    private var byHost: [(host: TendFleetHostIdentity, agents: [TendHostedAgent])] {
        let switchable = limited.filter { TendAgentActionTarget.switchableKinds.contains($0.agent.kind) }
        let grouped = Dictionary(grouping: switchable, by: \.host.alias)
        return grouped.values
            .compactMap { agents in agents.first.map { (host: $0.host, agents: agents) } }
            .sorted { $0.host.local != $1.host.local ? $0.host.local : $0.host.alias < $1.host.alias }
    }

    var body: some View {
        let groups = byHost
        if groups.count == 1, let group = groups.first {
            Button {
                present(group.host, group.agents)
            } label: {
                Label(group.agents.count == 1 ? "Switch Account…" : "Switch All…", systemImage: "arrow.triangle.2.circlepath")
            }
            .buttonStyle(.tendSecondary)
            .help("Relaunch the capped agents on aas accounts with quota left and carry their turns on")
        } else if groups.count > 1 {
            Menu {
                ForEach(groups, id: \.host.alias) { group in
                    Button("On \(group.host.alias) (\(group.agents.count))") {
                        present(group.host, group.agents)
                    }
                }
            } label: {
                Label("Switch All…", systemImage: "arrow.triangle.2.circlepath")
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .help("Relaunch the capped agents on aas accounts with quota left and carry their turns on")
        }
    }

    private func present(_ host: TendFleetHostIdentity, _ agents: [TendHostedAgent]) {
        model.swaps.present(Self.sheetTarget(host: host, agents: agents))
    }

    /// Hosts with switchable capped agents: a swap moves one host's agents,
    /// so with more than one, Command Center's per-host menu is the place.
    static func hostCount(for limited: [TendHostedAgent]) -> Int {
        Set(limited.filter { TendAgentActionTarget.switchableKinds.contains($0.agent.kind) }.map(\.host.alias)).count
    }

    /// Open the switch sheet when one host has capped agents, else Command
    /// Center, which offers one switch per host. False when none can be
    /// switched.
    @MainActor
    @discardableResult
    static func presentSwitch(for limited: [TendHostedAgent], model: AppModel) -> Bool {
        switch hostCount(for: limited) {
        case 0: return false
        case 1:
            guard let target = sheetTarget(for: limited) else { return false }
            model.swaps.present(target)
        default:
            model.select(.workBoard)
        }
        return true
    }

    /// The sheet for the host with the most switchable capped agents (the
    /// local host on a tie); nil when none can be switched.
    static func sheetTarget(for limited: [TendHostedAgent]) -> TendSwapSheetTarget? {
        let switchable = limited.filter { TendAgentActionTarget.switchableKinds.contains($0.agent.kind) }
        let groups = Dictionary(grouping: switchable, by: \.host.alias).values
        guard let busiest = groups.max(by: { lhs, rhs in
            lhs.count != rhs.count ? lhs.count < rhs.count : !(lhs.first?.host.local ?? false)
        }), let host = busiest.first?.host else { return nil }
        return sheetTarget(host: host, agents: busiest)
    }

    static func sheetTarget(host: TendFleetHostIdentity, agents: [TendHostedAgent]) -> TendSwapSheetTarget {
        let kinds = Set(agents.map(\.agent.kind))
        let label = agents.count == 1
            ? agents[0].displayTitle
            : String(localized: "\(agents.count) limited agents")
        return TendSwapSheetTarget(
            host: host.local ? nil : host.alias,
            scope: .agents(agents.map(\.agent.agentSessionID)),
            label: label,
            agentKind: kinds.count == 1 ? kinds.first : nil,
            interruptsWork: false
        )
    }
}
