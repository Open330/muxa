import SwiftUI

/// Why an agent sits in the Inbox: the state it is stuck in, the operator
/// requests it still owes, its latest prompt and reply, and where it runs.
/// Shown at the top of the agent detail so a "Needs attention" selection
/// explains itself without leaving the Inbox.
struct InboxAgentRequestCard: View {
    let participant: MuxaHostedAgent
    let requests: [MuxaOperatorMessage]
    let work: MuxaWorkGroup?
    let openInLiveWatch: () -> Void

    private var state: String { participant.agent.state }

    private var stateSymbol: String {
        switch state {
        case "waiting_input", "waiting_choice": "questionmark.bubble"
        case "blocked": "hand.raised"
        case "error", "failed": "exclamationmark.triangle"
        case "working", "starting": "bolt"
        case "done": "checkmark.circle"
        default: "circle"
        }
    }

    /// The most specific text about what the agent is waiting on: its last
    /// notification (the input request itself), then its recap, then the
    /// reply it stopped at.
    private var waitingText: String? {
        nonEmpty(participant.agent.lastNotification)
            ?? nonEmpty(participant.agent.recap)
            ?? nonEmpty(participant.agent.lastResponse)
    }

    private var paneLocation: String? {
        guard let pane = participant.pane else { return participant.agent.pane }
        let window = pane.windowName.isEmpty ? pane.stableWindowID : pane.windowName
        return "\(pane.session) › \(window) · \(pane.paneID)"
    }

    private var workLabel: String? {
        if let work {
            return "\(work.workspaceID) › \(work.workID)"
        }
        guard let identity = participant.pane?.workIdentity else { return nil }
        return "\(identity.workspaceID) › \(identity.workID)"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Label(agentStateLabel(state), systemImage: stateSymbol)
                    .font(.headline)
                    .foregroundStyle(agentStateColor(state))
                if let since = participant.agent.stateEnteredAt {
                    Text("since \(compactTimestamp(since))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer(minLength: 8)
                Button(action: openInLiveWatch) {
                    Label("Open in Live Watch", systemImage: "rectangle.on.rectangle")
                }
                .buttonStyle(.borderedProminent)
                .disabled(participant.pane == nil)
                .help(
                    participant.pane == nil
                        ? "This agent is not attached to a pane"
                        : "Follow this agent's pane in Live Watch"
                )
            }

            if let waitingText {
                MarkdownContent(source: waitingText, lineLimit: 8, font: .body)
            } else {
                Text("No request text was retained for this agent.")
                    .font(.subheadline)
                    .foregroundStyle(.tertiary)
            }

            if !requests.isEmpty {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Open requests")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    ForEach(requests) { message in
                        InboxAgentRequestRow(message: message)
                    }
                }
            }

            if let prompt = nonEmpty(participant.agent.lastPrompt) {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Latest prompt")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    MarkdownContent(source: prompt, lineLimit: 4)
                }
            }

            Divider()

            Grid(alignment: .leading, horizontalSpacing: 18, verticalSpacing: 6) {
                InboxAgentFact(label: "Host", value: participant.host.alias)
                if let paneLocation {
                    InboxAgentFact(label: "Pane", value: paneLocation)
                }
                if let workLabel {
                    InboxAgentFact(label: "Work", value: workLabel)
                }
                if let cwd = nonEmpty(participant.agent.cwd ?? participant.pane?.currentPath) {
                    InboxAgentFact(label: "Directory", value: cwd)
                }
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
    }
}

/// One operator request the agent still owes, or whose reply the operator
/// has not read yet.
private struct InboxAgentRequestRow: View {
    let message: MuxaOperatorMessage

    private var request: MuxaCollaborationRequest { message.request }

    private var statusText: String {
        if let reply = request.reply { return reply.status.capitalized }
        return request.status.capitalized
    }

    private var statusColor: Color {
        let status = request.reply?.status ?? request.status
        switch status {
        case "completed": return .green
        case "blocked", "declined", "failed": return .red
        case "claimed": return .blue
        default: return .secondary
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 7) {
                Text(request.kind.capitalized)
                    .font(.caption.weight(.semibold))
                Text(statusText)
                    .font(.caption2.weight(.medium))
                    .foregroundStyle(statusColor)
                if message.hasUnreadReply {
                    Label("New reply", systemImage: "arrowshape.turn.up.left.fill")
                        .font(.caption2)
                        .foregroundStyle(.orange)
                }
                Spacer(minLength: 4)
                Text(compactTimestamp(message.activityAt))
                    .font(.caption2.monospaced())
                    .foregroundStyle(.tertiary)
            }
            MarkdownContent(source: request.body, lineLimit: 3)
            if let reply = request.reply, !reply.body.isEmpty {
                MarkdownContent(source: reply.body, lineLimit: 3)
                    .padding(.leading, 10)
                    .overlay(alignment: .leading) {
                        RoundedRectangle(cornerRadius: 1)
                            .fill(statusColor.opacity(0.5))
                            .frame(width: 2)
                    }
            }
        }
        .padding(9)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: 8))
    }
}

private struct InboxAgentFact: View {
    let label: LocalizedStringKey
    let value: String

    var body: some View {
        GridRow {
            Text(label)
                .foregroundStyle(.secondary)
                .frame(width: 74, alignment: .leading)
            Text(value)
                .textSelection(.enabled)
                .lineLimit(2)
        }
        .font(.subheadline)
    }
}

private func nonEmpty(_ value: String?) -> String? {
    guard let value, !value.isEmpty else { return nil }
    return value
}

/// "2026-09-03T10:15:42Z" → "2026-09-03 10:15".
private func compactTimestamp(_ value: String) -> String {
    String(value.replacingOccurrences(of: "T", with: " ").prefix(16))
}
