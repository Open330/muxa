import SwiftUI
import os

/// What an agent's state means to the person watching it.
///
/// muxad reports a raw `state` string, and a rate-limited agent usually
/// reports `error` because its last turn stopped on a 429. Shown as is, that
/// reads as something to fix when the only thing to do is wait. Every surface
/// derives one status from the agent through this type, so each shows the
/// same word and color, counts the same agents as needing attention, and
/// sorts them the same way.
enum MuxaAgentStatus: Hashable, Sendable {
    /// Stopped on an error that is not a rate limit.
    case error
    /// Waiting on the person: a prompt, a choice, or a blocked tool call.
    /// Carries the raw state so the full label can say which.
    case needsInput(String)
    /// Under a provider rate limit; `cap.until` is when it lifts.
    case limited(MuxaRateLimitCap)
    case working
    case idle
    case done
    /// `pending`, `stopped`, or a state this build does not know.
    case other(String)

    init(agent: MuxaAgent, now: Date = .now) {
        let state = agent.state
        // A cap explains a turn that stopped: one that ended in an error (a
        // 429) or went idle. The daemon clears the cap fields only on the
        // agent's next start or turn end, so on a running agent, or one now
        // waiting for input, the cap is history and must not hide it.
        if Self.cappableStates.contains(state),
           var cap = MuxaRateLimitCap.current(for: agent, now: now) {
            cap.covered = MuxaLimitCoverage.covers(agent, now: now)
            self = .limited(cap)
            return
        }
        self.init(state: state)
    }

    /// A raw state with no agent around it (a Work participant's reported
    /// status); a rate limit cannot be seen from the string alone.
    init(state: String) {
        switch state {
        case "error", "failed": self = .error
        case "waiting_input", "waiting_choice", "blocked": self = .needsInput(state)
        case _ where Self.runningStates.contains(state): self = .working
        case "idle": self = .idle
        case "done": self = .done
        default: self = .other(state)
        }
    }

    private static let runningStates: Set<String> = ["working", "running", "starting"]
    private static let cappableStates: Set<String> = ["error", "failed", "idle"]

    var isLimited: Bool {
        if case .limited = self { return true }
        return false
    }

    /// Something the person should act on now. A rate limit is only when no
    /// limit policy will act on it (`MuxaLimitCoverage`): otherwise the
    /// agent switches account or resumes at the reset on its own.
    var needsAttention: Bool {
        switch self {
        case .error, .needsInput: true
        case .limited(let cap): !cap.covered
        default: false
        }
    }

    /// Where the status sorts in a list that puts what needs the person
    /// first: errors, then waiting agents, then limits, then the rest.
    var priority: Int {
        switch self {
        case .error: 0
        case .needsInput: 1
        case .limited: 2
        case .working: 3
        case .idle: 4
        case .done: 5
        case .other: 6
        }
    }

    func label(now: Date = .now) -> String {
        switch self {
        case .error: String(localized: "Error")
        case .needsInput(let state): agentStateLabel(state)
        case .limited(let cap):
            // One word for a cap everywhere: "Limited", with the reset time
            // when the source reported one.
            cap.until == nil ? String(localized: "Limited") : MuxaUsageFormat.capText(cap, now: now)
        case .working: String(localized: "Working")
        case .idle: String(localized: "Idle")
        case .done: String(localized: "Done")
        case .other(let state): agentStateLabel(state)
        }
    }

    /// The short form for a dense row, where the reset time would crowd out
    /// the title.
    var shortLabel: String {
        switch self {
        case .needsInput: String(localized: "Needs input")
        case .limited: String(localized: "Limited")
        default: label()
        }
    }

    var color: Color {
        switch self {
        case .error: .red
        case .needsInput: .orange
        case .limited: .purple
        case .working: .blue
        case .idle: .mint
        case .done: .green
        case .other(let state): agentStateColor(state)
        }
    }

    var symbol: String {
        switch self {
        case .error: "exclamationmark.octagon.fill"
        case .needsInput: "hand.raised.fill"
        case .limited: "hourglass"
        case .working: "circle.dotted"
        case .idle: "pause.circle"
        case .done: "checkmark.circle.fill"
        case .other: "circle"
        }
    }
}

extension MuxaAgent {
    var status: MuxaAgentStatus { MuxaAgentStatus(agent: self) }
}

extension MuxaWatchPane {
    /// The pane's agent status; nil for a plain shell.
    var agentStatus: MuxaAgentStatus? { agent?.status }

    var needsAttention: Bool { agentStatus?.needsAttention ?? false }

    /// For sorting panes: agents by status, plain shells last.
    var statusPriority: Int { agentStatus?.priority ?? 7 }
}

/// The dot and short label every agent row uses, so a status reads the same
/// in the Explore tree, the inspector, and the Command Center.
struct AgentStatusTag: View {
    let status: MuxaAgentStatus
    var showsLabel = true
    var detailed = false

    var body: some View {
        HStack(spacing: 5) {
            Circle()
                .fill(status.color)
                .frame(width: 7, height: 7)
            if showsLabel {
                Text(detailed ? status.label() : status.shortLabel)
                    .font(MuxaType.meta.weight(.medium))
                    .foregroundStyle(status.color)
                    .lineLimit(1)
            }
        }
        .fixedSize()
        .accessibilityElement(children: .combine)
        .accessibilityLabel(status.label())
    }
}

/// Which capped agents a limit policy takes care of, so they need not ask
/// for the person.
///
/// Automation rules run in the daemon that owns the pane, so the app's own
/// muxad covers only its own agents: a capped agent on a Fleet host always
/// counts as the person's. A rule covers the agent kinds and limit scopes
/// it names (all when it names none); a rule with any other filter covers
/// nothing, since the app cannot tell which panes it matches. A switch rule
/// covers only Claude Code and Codex, and nothing while the swap service
/// says it cannot take a job. A pause covers nothing until it ends.
enum MuxaLimitCoverage {
    struct Grant: Hashable, Sendable {
        /// Agent kinds; nil for every kind.
        var kinds: Set<String>?
        /// Limit scopes (`five_hour`, …); nil for every scope.
        var scopes: Set<String>?
        /// It acts by switching account, so needs the swap service.
        var switches: Bool
    }

    struct State: Equatable, Sendable {
        var grants: [Grant] = []
        var pausedUntil: Date?
        var swapAvailable = true
        /// `agent_session_id`s on the app's own host.
        var localAgents: Set<String> = []
    }

    private static let state = OSAllocatedUnfairLock(initialState: State())
    /// Posted when the rules, pause or swap availability change, so counts
    /// that depend on them are worked out again.
    static let didChange = Notification.Name("MuxaLimitCoverageDidChange")

    static func covers(_ agent: MuxaAgent, now: Date = .now) -> Bool {
        covers(agent, in: state.withLock { $0 }, now: now)
    }

    static func covers(_ agent: MuxaAgent, in state: State, now: Date = .now) -> Bool {
        guard state.localAgents.contains(agent.agentSessionID) else { return false }
        if let until = state.pausedUntil, until > now { return false }
        return state.grants.contains { grant in
            if grant.switches && !state.swapAvailable { return false }
            if let kinds = grant.kinds, !kinds.contains(agent.kind) { return false }
            if let scopes = grant.scopes {
                guard let scope = agent.rateLimitScope, scopes.contains(scope) else { return false }
            }
            return true
        }
    }

    /// What the local daemon's live rules cover.
    static func grants(enabled: Bool, rules: [MuxaAutomationRule]) -> [Grant] {
        guard enabled else { return [] }
        return rules.compactMap { rule in
            guard rule.enabled, rule.on == .rateLimited,
                  rule.action == .swapAccount || rule.action == .sendPrompt,
                  rule.workspace == nil, rule.work == nil, rule.pane == nil,
                  rule.host == nil, rule.notification == nil else { return nil }
            let named = rule.agent.isEmpty ? nil : Set(rule.agent)
            let switches = rule.action == .swapAccount
            var kinds = named
            if switches {
                kinds = (named ?? MuxaAgentActionTarget.switchableKinds)
                    .intersection(MuxaAgentActionTarget.switchableKinds)
                if kinds?.isEmpty == true { return nil }
            }
            return Grant(kinds: kinds, scopes: rule.scope.isEmpty ? nil : Set(rule.scope), switches: switches)
        }
    }

    static func setRules(_ grants: [Grant], pausedUntil: Date?) {
        update { $0.grants = grants; $0.pausedUntil = pausedUntil }
    }

    static func setSwapAvailable(_ available: Bool) {
        update { $0.swapAvailable = available }
    }

    /// The agents on the app's own host; set before each snapshot is
    /// counted, so it posts nothing.
    static func setLocalAgents(_ ids: Set<String>) {
        state.withLock { $0.localAgents = ids }
    }

    private static func update(_ change: @Sendable (inout State) -> Void) {
        let changed = state.withLock { current -> Bool in
            let before = current
            change(&current)
            return current != before
        }
        if changed { NotificationCenter.default.post(name: didChange, object: nil) }
    }
}
