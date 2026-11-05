import Foundation

/// The one definition of what needs the person, computed once per change by
/// the attention center. Every badge and count reads from it, so the Inbox
/// badge, the Dock, the menu bar and Command Center never disagree:
///
/// - **needs you** — agents whose state needs the person (blocked, errored,
///   or capped with no limit policy acting), requests waiting on a human
///   decision, and replies not yet read. The Inbox badge and the menu bar.
/// - **unread** — agent panes with a turn or a wait the person has not seen.
///   The Explore badge.
/// - **Dock** — both, each item counted once.
///
/// Commands still waiting on an agent's reply are shown in the Inbox but
/// never counted: nothing about them is the person's to do.
struct MuxaAttentionSummary: Equatable, Sendable {
    /// Agents whose state needs the person; capped ones only when no limit
    /// policy acts on them.
    var agents = 0
    /// Capped agents, whether or not a policy acts.
    var limited = 0
    /// Capped agents left to the person.
    var uncoveredLimited = 0
    /// Peers' requests waiting on the person's decision.
    var decisions = 0
    /// Replies to the person's commands not yet read — a blocked or failed
    /// one included, until it is read.
    var unreadReplies = 0
    /// Agent panes with something new.
    var unreadPanes = 0
    /// Distinct items for the Dock: needs-you plus unread, an agent that is
    /// both counted once.
    var dock = 0

    var needsYou: Int { agents + decisions + unreadReplies }

    /// Every input reduced to counts. `unread` keys are the pane keys the
    /// attention center uses; an agent needing the person in an unread pane
    /// counts once on the Dock.
    static func make(
        agents: [MuxaHostedAgent],
        messages: [MuxaOperatorMessage],
        unread: Set<String>,
        paneKey: (MuxaHostedAgent) -> String
    ) -> MuxaAttentionSummary {
        var summary = MuxaAttentionSummary()
        var dock = unread
        for hosted in agents {
            let status = hosted.agent.status
            if status.isLimited {
                summary.limited += 1
                if status.needsAttention { summary.uncoveredLimited += 1 }
            }
            guard status.needsAttention else { continue }
            summary.agents += 1
            dock.insert(paneKey(hosted))
        }
        for message in messages {
            if message.isHumanRequest && message.needsHumanDecision {
                summary.decisions += 1
                dock.insert("message:\(message.id)")
            } else if message.hasUnreadReply {
                summary.unreadReplies += 1
                dock.insert("message:\(message.id)")
            }
        }
        summary.unreadPanes = unread.count
        summary.dock = dock.count
        return summary
    }

    /// One line for the menu bar and Command Center, or nil when nothing
    /// needs the person.
    var needsYouLine: String? {
        var parts: [String] = []
        if agents > 0 {
            parts.append(agents == 1
                ? String(localized: "1 agent needs you")
                : String(localized: "\(agents) agents need you"))
        }
        if decisions > 0 {
            parts.append(decisions == 1
                ? String(localized: "1 decision")
                : String(localized: "\(decisions) decisions"))
        }
        if unreadReplies > 0 {
            parts.append(unreadReplies == 1
                ? String(localized: "1 unread reply")
                : String(localized: "\(unreadReplies) unread replies"))
        }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }
}
