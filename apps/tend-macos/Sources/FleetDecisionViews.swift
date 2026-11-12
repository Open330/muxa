// Home's decisions: the daemon's queue drawn as cards in Your turn and the
// review sheet. A card says what to decide, offers the asker's options and
// recommendation, links the evidence, and records the answer with tendd
// (a human-request reply, or a review disposition).
import AppKit
import SwiftUI

// MARK: - Feed

/// Reads this Mac's decision queue, or the review list from a daemon
/// without it, and records what the person decides. Other hosts' human
/// requests come from the console mailbox the app already reads.
@MainActor
final class FleetDecisionFeed: ObservableObject {
    @Published private(set) var queue: TendPendingQueue?
    @Published private(set) var reviews: [TendReviewItem] = []
    @Published private(set) var problem: String?
    /// Decisions being recorded, by id.
    @Published private(set) var busy: Set<String> = []
    /// The last error recording a decision, by id.
    @Published var failures: [String: String] = [:]
    /// The one decision whose answer is open, and the text typed so far.
    @Published private var composingID: String?
    @Published private var drafts: [String: String] = [:]
    private var loading = false

    static let refreshEvery: Duration = .seconds(5)

    func load(_ client: TendIPCClient) async {
        guard !loading else { return }
        loading = true
        defer { loading = false }
        do {
            if let queue = try await client.decisionQueue() {
                if self.queue != queue { self.queue = queue }
                if !reviews.isEmpty { reviews = [] }
                problem = queue.unavailable.isEmpty
                    ? nil
                    : String(localized: "Some decisions could not be read: \(queue.unavailable.joined(separator: "; "))")
            } else {
                queue = nil
                let page = try await client.reviewItems(offset: 0, limit: 200)
                if reviews != page.items { reviews = page.items }
                problem = nil
            }
        } catch {
            problem = String(localized: "Could not read decisions: \(error.localizedDescription)")
        }
    }

    /// What Home shows, oldest-blocked first, each with the asking agent's
    /// checkout when Home knows it.
    func decisions(_ model: AppModel) -> [FleetDecision] {
        guard let local = model.fleetHosts.first(where: \.local)?.identity else { return [] }
        let merged = FleetDecisions.merge(queue: queue, reviews: reviews, messages: model.operatorMessages, localHost: local)
        return FleetDecisions.attachAskers(merged, agents: model.hostedAgents)
    }

    /// The open answer for `id`: one composer at a time across Home and the
    /// Review sheet, so a keyboard Send can only reach the one on screen.
    func draft(for id: String) -> Binding<String?> {
        Binding(
            get: { [weak self] in
                guard let self, self.composingID == id else { return nil }
                return self.drafts[id] ?? ""
            },
            set: { [weak self] value in
                guard let self else { return }
                if let value {
                    // Reopening keeps what was typed before; an option
                    // (a non-empty value) replaces it on purpose.
                    let reopening = self.composingID != id && value.isEmpty
                    self.composingID = id
                    if !(reopening && self.drafts[id] != nil) { self.drafts[id] = value }
                } else if self.composingID == id {
                    self.composingID = nil
                    self.drafts[id] = nil
                }
            }
        )
    }

    /// Replies to a human request as the person; tendd records the answer.
    func answer(_ decision: FleetDecision, text: String, model: AppModel) async -> Bool {
        guard case .humanRequest(let requestID) = decision.source else { return false }
        guard let route = model.consoleRoute(hostAlias: decision.host.alias) else {
            failures[decision.id] = String(localized: "\(decision.host.alias) is not connected.")
            return false
        }
        return await record(decision, model: model) {
            try await model.client.replyCollaboration(
                host: route.host, pane: route.pane, requestID: requestID, status: "completed", body: text
            )
            await model.refreshOperatorInbox(force: false, hostAliases: [decision.host.alias])
        }
    }

    /// Accepts, returns or withdraws a review item on this Mac's daemon.
    func decide(_ decision: FleetDecision, _ disposition: TendReviewDisposition, note: String?, model: AppModel) async -> Bool {
        guard case .review(let item) = decision.source else { return false }
        return await record(decision, model: model) {
            try await model.client.decideReview(id: item.id, disposition: disposition, note: note)
        }
    }

    private func record(_ decision: FleetDecision, model: AppModel, _ body: () async throws -> Void) async -> Bool {
        busy.insert(decision.id)
        failures[decision.id] = nil
        defer { busy.remove(decision.id) }
        var succeeded = true
        do {
            try await body()
        } catch {
            failures[decision.id] = error.localizedDescription
            succeeded = false
        }
        // Either way, show where it stands now: decided here, or already
        // decided elsewhere.
        await load(model.client)
        return succeeded
    }
}

extension FleetDecisions {
    /// Fills in each decision's asking agent (its checkout and repository)
    /// from the agents Home sees on the same host.
    static func attachAskers(_ decisions: [FleetDecision], agents: [TendHostedAgent]) -> [FleetDecision] {
        decisions.map { decision in
            guard let session = decision.askerSessionID,
                  let agent = agents.first(where: { $0.host.alias == decision.host.alias && $0.agent.agentSessionID == session })
            else { return decision }
            var decision = decision
            decision.askerAgentID = agent.id
            decision.askerCwd = agent.agent.cwd
            return decision
        }
    }
}

extension AppModel {
    /// The host and a pane on it to reach its console mailbox through, the
    /// way the Inbox reads it.
    func consoleRoute(hostAlias: String) -> (host: TendFleetHostIdentity, pane: TendPaneInfo)? {
        if let watch = executionSnapshot.watchHosts.first(where: { $0.host.alias == hostAlias }) {
            let pane = watch.sessions.flatMap(\.windows).flatMap(\.panes).first?.pane ?? .operatorRoute
            return (watch.host.identity, pane)
        }
        guard let host = fleetHosts.first(where: { $0.alias == hostAlias }) else { return nil }
        return (host.identity, .operatorRoute)
    }
}

// MARK: - Evidence

/// A recorded revision to open in Changes.
struct FleetRevisionTarget: Identifiable, Hashable, Sendable {
    let repo: String
    let revision: ChangesRevision
    var id: String { "\(repo)\n\(revision.label)" }
}

/// What one click on a piece of evidence does.
enum FleetEvidenceAction: Equatable, Sendable {
    case changes(FleetRevisionTarget)
    case url(URL)
    case reveal(String)
    /// Shown, not openable from here, with the reason.
    case unavailable(String)

    /// Commits and ranges open in Changes on this Mac's checkout: the one
    /// the asker recorded, else the asking agent's working directory. A
    /// pull request opens in the browser; a path is revealed in Finder.
    static func of(_ evidence: TendEvidence, decision: FleetDecision) -> FleetEvidenceAction {
        func revision(_ revision: ChangesRevision?, repo: String?) -> FleetEvidenceAction {
            guard let revision else { return .unavailable(String(localized: "Not a revision Tend can read")) }
            guard revision.comparesCommits else { return .unavailable(ChangesRevision.workingTreeReason) }
            guard decision.host.local else {
                return .unavailable(String(localized: "On \(decision.host.alias); Changes reads this Mac's checkouts"))
            }
            guard let repo = (repo ?? decision.askerCwd)?.trimmingCharacters(in: .whitespaces), repo.hasPrefix("/") else {
                return .unavailable(String(localized: "No checkout recorded"))
            }
            return .changes(FleetRevisionTarget(repo: repo, revision: revision))
        }
        switch evidence {
        case .commit(let sha, let repo):
            return revision(ChangesRevision(commit: sha), repo: repo)
        case .diff(let range, let repo):
            return revision(ChangesRevision(range: range), repo: repo)
        case .pullRequest(let url):
            guard let url = URL(string: url), ["https", "http"].contains(url.scheme?.lowercased() ?? "") else {
                return .unavailable(String(localized: "Not a web address"))
            }
            return .url(url)
        case .path(let path):
            guard decision.host.local else {
                return .unavailable(String(localized: "On \(decision.host.alias); Changes reads this Mac's checkouts"))
            }
            guard path.hasPrefix("/") else { return .unavailable(String(localized: "Not an absolute path")) }
            return .reveal(path)
        case .test(let command, let passed, let tail):
            let verdict = switch passed {
            case true?: String(localized: "Passed")
            case false?: String(localized: "Failed")
            case nil: String(localized: "Result not recorded")
            }
            return .unavailable([verdict + ": " + command, tail].compactMap { $0 }.joined(separator: "\n\n"))
        }
    }
}

/// The evidence as small chips; a chip that opens something is a button.
struct FleetEvidenceChips: View {
    let decision: FleetDecision
    let openRevision: (FleetRevisionTarget) -> Void

    var body: some View {
        PipelineFlowLayout(spacing: 5) {
            ForEach(Array(decision.evidence.enumerated()), id: \.offset) { _, evidence in
                chip(evidence, action: FleetEvidenceAction.of(evidence, decision: decision))
            }
        }
    }

    private func tint(_ evidence: TendEvidence) -> Color {
        if case .test(_, let passed, _) = evidence {
            switch passed {
            case true?: return .green
            case false?: return .red
            case nil: return .secondary
            }
        }
        return .accentColor
    }

    @ViewBuilder
    private func chip(_ evidence: TendEvidence, action: FleetEvidenceAction) -> some View {
        let label = Label {
            Text(verbatim: evidence.label)
                .lineLimit(1)
                .truncationMode(.middle)
        } icon: {
            Image(systemName: evidence.systemImage)
        }
        .font(.system(size: 11, weight: .medium))
        .frame(maxWidth: 220)
        .fixedSize(horizontal: false, vertical: true)
        .padding(.horizontal, 7)
        .padding(.vertical, 2)
        switch action {
        case .unavailable(let reason):
            label
                .foregroundStyle(tint(evidence) == .accentColor ? Color.secondary : tint(evidence))
                .background(Color.secondary.opacity(0.1), in: Capsule())
                .help(reason)
        case .changes(let target):
            Button { openRevision(target) } label: {
                label.background(Color.accentColor.opacity(0.12), in: Capsule())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Color.accentColor)
            .help(String(localized: "Open \(target.revision.label) in Changes"))
        case .url(let url):
            Button { NSWorkspace.shared.open(url) } label: {
                label.background(Color.accentColor.opacity(0.12), in: Capsule())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Color.accentColor)
            .help(url.absoluteString)
        case .reveal(let path):
            Button { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)]) } label: {
                label.background(Color.accentColor.opacity(0.12), in: Capsule())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Color.accentColor)
            .help(path)
        }
    }
}

// MARK: - Card

/// One decision: what to decide, the asker's options and recommendation,
/// the evidence, and the answer. An option fills the answer for a look
/// before it goes; nothing is sent on a single stray click.
struct FleetDecisionCard: View {
    let decision: FleetDecision
    var busy = false
    var failure: String?
    let openRevision: (FleetRevisionTarget) -> Void
    /// Replies to a human request; true when it was recorded.
    let answer: (String) async -> Bool
    /// Disposes a review item; true when it was recorded.
    let decide: (TendReviewDisposition, String?) async -> Bool
    var openAsker: (() -> Void)?
    /// Opens with the answer field showing, for previews and tests.
    var initialAnswer: String?
    /// The open answer, shared by every card so only one composer (and one
    /// ⌘↩ Send) exists at a time; a card without one keeps its own.
    var draft: Binding<String?>?
    @State private var localDraft: String?

    private var composing: String? {
        get { draft?.wrappedValue ?? localDraft }
        nonmutating set {
            if let draft { draft.wrappedValue = newValue } else { localDraft = newValue }
        }
    }
    @Environment(\.colorScheme) private var colorScheme

    private var answerText: String { composing ?? initialAnswer ?? "" }
    private var isComposing: Bool { composing != nil || initialAnswer != nil }

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            header
            Text(verbatim: decision.title)
                .font(.system(size: 13, weight: .semibold))
                .lineLimit(4)
                .frame(maxWidth: .infinity, alignment: .leading)
                .textSelection(.enabled)
            if let detail = decision.detail {
                Text(verbatim: detail)
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                    .lineLimit(3)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            if let recommendation = decision.recommendation {
                Label {
                    Text(verbatim: recommendation)
                        .lineLimit(3)
                } icon: {
                    Image(systemName: "lightbulb")
                        .foregroundStyle(.orange)
                }
                .font(TendType.detail)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .help(String(localized: "What the asker recommends"))
            }
            if !decision.evidence.isEmpty {
                FleetEvidenceChips(decision: decision, openRevision: openRevision)
            }
            if isComposing {
                composer
            } else {
                actions
            }
            if let failure {
                Label(failure, systemImage: "exclamationmark.circle")
                    .font(TendType.meta)
                    .foregroundStyle(.red)
                    .lineLimit(3)
            }
        }
        .padding(12)
        .background(TendSurfacePalette.workspace(for: colorScheme), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .strokeBorder(Color.orange.opacity(0.45), lineWidth: 1)
        )
        .disabled(busy)
        .contextMenu {
            if let openAsker {
                Button("Open Agent", action: openAsker)
            }
            if case .review = decision.source {
                Button("Withdraw") { Task { _ = await decide(.withdrawn, nil) } }
            }
        }
    }

    private var header: some View {
        HStack(spacing: 6) {
            Image(systemName: decision.isHumanRequest ? "questionmark.bubble" : "checklist")
                .foregroundStyle(.orange)
            Text(decision.isHumanRequest ? String(localized: "Question") : String(localized: "Review"))
                .fontWeight(.semibold)
            if let asker = decision.asker {
                Text(verbatim: asker)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            if !decision.host.local {
                Text(verbatim: decision.host.alias)
                    .lineLimit(1)
            }
            Spacer(minLength: 4)
            if busy {
                ProgressView().controlSize(.mini)
            } else if let since = decision.since {
                Text(since, format: .relative(presentation: .numeric, unitsStyle: .abbreviated))
                    .monospacedDigit()
            }
        }
        .font(TendType.meta)
        .foregroundStyle(.secondary)
    }

    private var actions: some View {
        PipelineFlowLayout(spacing: 6) {
            ForEach(decision.options, id: \.self) { option in
                Button {
                    composing = option
                } label: {
                    Text(verbatim: option)
                        .lineLimit(2)
                        .frame(maxWidth: 240)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .buttonStyle(.tendSecondary)
                .help(String(localized: "Answer with this option; you see it before it is sent"))
            }
            switch decision.source {
            case .humanRequest:
                Button(decision.options.isEmpty ? String(localized: "Answer…") : String(localized: "Other…")) {
                    composing = ""
                }
                .buttonStyle(decision.options.isEmpty ? TendButtonStyle.tendSecondary : TendButtonStyle.tendGhost)
            case .review:
                Button("Accept") { Task { _ = await decide(.accepted, nil) } }
                    .buttonStyle(.tendSecondary)
                Button("Return…") { composing = "" }
                    .buttonStyle(.tendGhost)
            }
        }
    }

    private var composer: some View {
        let text = Binding(get: { answerText }, set: { composing = $0 })
        let empty = answerText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        return VStack(alignment: .leading, spacing: 6) {
            TextField(decision.isHumanRequest ? String(localized: "Your answer") : String(localized: "Note for the asker"), text: text, axis: .vertical)
                .textFieldStyle(.roundedBorder)
                .lineLimit(1...5)
            HStack(spacing: 6) {
                Spacer(minLength: 0)
                Button("Cancel") { composing = nil }
                    .buttonStyle(.tendGhost)
                switch decision.source {
                case .humanRequest:
                    Button("Send") { submit { await answer(answerText) } }
                        .buttonStyle(.tendPrimary)
                        .disabled(empty)
                        .keyboardShortcut(.return, modifiers: .command)
                case .review:
                    Button("Accept") { submit { await decide(.accepted, empty ? nil : answerText) } }
                        .buttonStyle(.tendSecondary)
                    Button("Return") { submit { await decide(.returned, answerText) } }
                        .buttonStyle(.tendPrimary)
                        .disabled(empty)
                }
            }
        }
    }

    private func submit(_ send: @escaping () async -> Bool) {
        Task {
            if await send() { composing = nil }
        }
    }
}
