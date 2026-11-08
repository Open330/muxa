import Combine
import Foundation

/// Owned by the menu-bar controller, not by a transient card's SwiftUI graph.
/// Text and failure survive closing, switching cards and a slow RPC finishing.
@MainActor
final class MuxaMenuBarReplyDraft: ObservableObject {
    var agent: MuxaMenuBarAgent
    @Published var text = ""
    @Published private(set) var sending = false
    @Published private(set) var failure: String?

    init(agent: MuxaMenuBarAgent) { self.agent = agent }
    var needsRecovery: Bool { !text.isEmpty || sending || failure != nil }

    func discard() {
        guard !sending else { return }
        text = ""
        failure = nil
    }

    func send(
        using deliver: @MainActor (String) async throws -> Void,
        onFailure: @MainActor (String, String) -> Void
    ) async {
        let original = text
        let outgoing = original.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !sending, !outgoing.isEmpty else { return }
        sending = true
        failure = nil
        defer { sending = false }
        do {
            try await deliver(outgoing)
            // Do not erase a newer draft if it changed while the RPC ran.
            if text == original { text = "" }
        } catch {
            let reason = error.localizedDescription
            failure = reason
            onFailure(outgoing, reason)
        }
    }
}

@MainActor
final class MuxaMenuBarState: ObservableObject {
    @Published var agents: [MuxaMenuBarAgent] = []
    @Published var selectedID: String?
    @Published var scope: MuxaMenuBarScope?
    @Published var showingUsage = false
    private var drafts: [String: MuxaMenuBarReplyDraft] = [:]
    private var subscriptions: [String: AnyCancellable] = [:]

    func draft(for agent: MuxaMenuBarAgent) -> MuxaMenuBarReplyDraft {
        if let draft = drafts[agent.id] { draft.agent = agent; return draft }
        let draft = MuxaMenuBarReplyDraft(agent: agent)
        drafts[agent.id] = draft
        subscriptions[agent.id] = draft.objectWillChange.sink { [weak self] _ in self?.objectWillChange.send() }
        return draft
    }

    func updateDraftAgents(_ hosted: [MuxaHostedAgent], unread: Set<MuxaWatchPaneIdentity>) {
        let live = Dictionary(hosted.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        let obsolete = drafts.filter { live[$0.key] == nil && !$0.value.needsRecovery }.map(\.key)
        for id in obsolete { drafts.removeValue(forKey: id); subscriptions.removeValue(forKey: id) }
        for draft in drafts.values {
            if let current = live[draft.agent.id] {
                let pane = MuxaAgentAttentionCenter.paneIdentity(for: current)
                draft.agent = MuxaMenuBarAgent(hosted: current, unread: pane.map(unread.contains) ?? false)
            }
        }
    }

    func replyFailure(for id: String) -> String? { drafts[id]?.failure }

    /// Keep drafts discoverable even if their agent finishes or disappears.
    /// Session-checked delivery stays disabled for an ended/replaced agent.
    var displayedAgents: [MuxaMenuBarAgent] {
        let present = Set(agents.map(\.id))
        return agents + drafts.values.filter { $0.needsRecovery && !present.contains($0.agent.id) }
            .map(\.agent).sorted { $0.id < $1.id }
    }

    var visibleAgents: [MuxaMenuBarAgent] {
        guard let scope else { return displayedAgents }
        return displayedAgents.filter(scope.contains)
    }
}
