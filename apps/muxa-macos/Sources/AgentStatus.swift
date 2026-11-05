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
           let cap = MuxaRateLimitCap.current(for: agent, now: now) {
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
        case .limited: !MuxaLimitCoverage.isCovered
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

/// Whether a limit policy (an enabled `rate_limited` rule that switches
/// account or resumes) will act on capped agents. Read by every status, on
/// any thread, so it is a lock-guarded value rather than published state;
/// `AutomationStore` sets it whenever its rules change.
///
/// It starts covered: until the rules are read, a capped agent is not
/// worth a launch-time notification storm.
enum MuxaLimitCoverage {
    private static let state = OSAllocatedUnfairLock(initialState: true)
    /// Posted when `isCovered` changes, so counts that depend on it are
    /// worked out again.
    static let didChange = Notification.Name("MuxaLimitCoverageDidChange")

    static var isCovered: Bool {
        get { state.withLock { $0 } }
        set {
            let changed = state.withLock { current -> Bool in
                defer { current = newValue }
                return current != newValue
            }
            if changed { NotificationCenter.default.post(name: didChange, object: nil) }
        }
    }
}
