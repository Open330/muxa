import SwiftUI

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
        // The daemon clears the cap fields only on the agent's next start,
        // so a cap on a running agent is history, not its status.
        if !Self.runningStates.contains(state),
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

    var isLimited: Bool {
        if case .limited = self { return true }
        return false
    }

    /// Something the person should act on now. A rate limit is not: it lifts
    /// on its own, and an automation can resume the agent.
    var needsAttention: Bool {
        switch self {
        case .error, .needsInput: true
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
