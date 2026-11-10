// The choice an agent is waiting on, answered from Home: the question and
// options read off its screen, an option sent by its number, or the
// person's own words through a "Type something" option.
import SwiftUI

/// What an agent waiting on the person takes, decided by what is on its
/// screen rather than by its reported state alone: a numbered choice gets
/// option buttons; otherwise `fallback` (the answer field) is shown.
struct FleetChoicePanel<Fallback: View>: View {
    @ObservedObject var model: AppModel
    let hosted: MuxaHostedAgent
    @ViewBuilder var fallback: () -> Fallback

    @State private var dialog: FleetChoiceDialog?
    /// Who and where the dialog was read from: the pane and the agent's
    /// session at that wait. Input goes only to that pane while it still
    /// holds them.
    @State private var seen: Seen?
    @State private var loading = false
    @State private var ownWords = ""
    /// The question the own words were written for; set when typing starts.
    @State private var ownWordsFor: Fingerprint?
    @State private var error: String?
    /// Sends outlive the panel: closing and reopening it shows the one in
    /// flight and cannot start a second.
    @ObservedObject private var sends = FleetChoiceSends.shared

    private var sending: Bool { sends.isPending(hosted.id) }

    /// The dialog on screen, as the person sees it, from that pane and wait.
    struct Fingerprint: Equatable {
        let seen: Seen
        let question: String
        let options: [FleetChoiceDialog.Option]
    }

    private var fingerprint: Fingerprint? {
        guard let dialog, let seen else { return nil }
        return Fingerprint(seen: seen, question: dialog.question, options: dialog.options)
    }

    /// Own words written for a question that is no longer the one shown.
    private var ownWordsAreStale: Bool {
        !ownWords.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && ownWordsFor != fingerprint
    }

    init(model: AppModel, hosted: MuxaHostedAgent, @ViewBuilder fallback: @escaping () -> Fallback) {
        self.model = model
        self.hosted = hosted
        self.fallback = fallback
    }

    /// Which body shows depends on what was read, never on a read being in
    /// flight: swapping bodies mid-read cancelled the read, and the
    /// cancellation showed as an error and started the read over.
    var body: some View {
        Group {
            if dialog != nil || hosted.agent.state == "waiting_choice" {
                choiceBody
            } else {
                // Nothing to pick on screen: the agent takes typed words.
                fallback()
            }
        }
        .task(id: hosted.agent.stateEnteredAt) { await load() }
    }

    private var choiceBody: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("Choice on screen")
                    .font(.system(size: 11, weight: .bold))
                    .foregroundStyle(.orange)
                Spacer()
                if loading { ProgressView().controlSize(.small) }
                Button {
                    Task { await load() }
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.muxaGhost)
                .help("Read the screen again")
                .disabled(loading || sending)
            }
            if let dialog {
                Text(dialog.question)
                    .font(.system(size: 13, weight: .semibold))
                    .fixedSize(horizontal: false, vertical: true)
                    .textSelection(.enabled)
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(dialog.options.filter { !$0.takesText }) { option in
                        Button {
                            choose(option, words: nil)
                        } label: {
                            HStack(alignment: .firstTextBaseline, spacing: 8) {
                                Text("\(option.number)")
                                    .font(.system(size: 11, weight: .bold).monospacedDigit())
                                    .frame(width: 18, height: 18)
                                    .background(Color.accentColor.opacity(0.15), in: Circle())
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(option.label)
                                        .font(MuxaType.body.weight(.medium))
                                        .multilineTextAlignment(.leading)
                                    if let detail = option.detail {
                                        Text(detail)
                                            .font(MuxaType.meta)
                                            .foregroundStyle(.secondary)
                                            .multilineTextAlignment(.leading)
                                    }
                                }
                                Spacer(minLength: 0)
                            }
                            .padding(8)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .background(Color.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 7, style: .continuous))
                        .disabled(sending)
                    }
                }
                if let typed = dialog.options.first(where: \.takesText) {
                    if ownWordsAreStale {
                        VStack(alignment: .leading, spacing: 6) {
                            Label("Written for an earlier question. It was not sent.", systemImage: "exclamationmark.triangle.fill")
                                .font(MuxaType.meta)
                                .foregroundStyle(.orange)
                            HStack(spacing: 8) {
                                Button("Use for This Question") { ownWordsFor = fingerprint }
                                    .buttonStyle(.muxaSecondary)
                                    .controlSize(.small)
                                Button("Discard", role: .destructive) {
                                    ownWords = ""
                                    ownWordsFor = nil
                                }
                                .buttonStyle(.muxaGhost)
                                .controlSize(.small)
                            }
                            .disabled(sending)
                        }
                    }
                    HStack(alignment: .bottom, spacing: 8) {
                        TextField("Your own answer", text: Binding(
                            get: { ownWords },
                            set: { text in
                                ownWords = text
                                if text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                                    ownWordsFor = nil
                                } else if ownWordsFor == nil {
                                    ownWordsFor = fingerprint
                                }
                            }
                        ), axis: .vertical)
                            .textFieldStyle(.roundedBorder)
                            .lineLimit(1...5)
                            .disabled(sending)
                        Button("Answer") { choose(typed, words: ownWords) }
                            .buttonStyle(.muxaPrimary)
                            .disabled(sending || ownWordsAreStale || ownWords.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    }
                }
            } else if !loading {
                Text("No numbered choice is on screen. Open the pane to see what it is asking.")
                    .font(MuxaType.meta)
                    .foregroundStyle(.secondary)
            }
            if sending {
                ProgressView().controlSize(.small)
            } else if let error {
                Label(error, systemImage: "exclamationmark.circle.fill")
                    .font(MuxaType.meta)
                    .foregroundStyle(.red)
            } else if let outcome = sends.outcome(hosted.id) {
                Label(outcome.message, systemImage: outcome.succeeded ? "checkmark.circle.fill" : "exclamationmark.circle.fill")
                    .font(MuxaType.meta)
                    .foregroundStyle(outcome.succeeded ? Color.green : Color.red)
            }
        }
    }

    struct Seen: Equatable {
        let pane: MuxaWatchPaneIdentity
        let target: FleetReplyTarget
    }

    private func capture(host: MuxaFleetHostIdentity, pane: MuxaPaneInfo) async throws -> FleetChoiceDialog? {
        let capture = try await model.client.captureFleetPane(host: host, pane: pane)
        return capture.screenText.flatMap(FleetChoiceDialog.parse)
    }

    /// The pane the dialog was read from, as it is now; throws unless it
    /// still holds the same agent session at the same wait.
    private func live(_ seen: Seen) throws -> MuxaWatchPane {
        guard let current = model.executionSnapshot.watchPane(id: seen.pane),
              seen.target.matches(current.agent),
              ["waiting_choice", "waiting_input"].contains(current.agent?.state ?? "") else {
            throw MuxaIPCError.server(String(localized: "The agent moved on. Open its pane to see where it is."))
        }
        return current
    }

    private func load() async {
        loading = true
        defer { loading = false }
        do {
            guard let pane = hosted.pane, let paneID = MuxaAgentAttentionCenter.paneIdentity(for: hosted) else {
                dialog = nil
                seen = nil
                return
            }
            // Taken before the read, so a wait that ends during it shows up
            // as a mismatch when choosing.
            let target = FleetReplyTarget(kind: .answer, agent: hosted)
            let read = try await capture(host: hosted.host, pane: pane)
            guard !Task.isCancelled else { return }
            dialog = read
            seen = read == nil ? nil : Seen(pane: paneID, target: target)
            error = nil
        } catch {
            // A read cut short (the agent moved on, the view went away) is
            // not something the person has to act on.
            if Self.isCancellation(error) || Task.isCancelled { return }
            self.error = error.localizedDescription
        }
    }

    static func isCancellation(_ error: Error) -> Bool {
        if error is CancellationError { return true }
        if let url = error as? URLError, url.code == .cancelled { return true }
        return (error as NSError).domain == NSCocoaErrorDomain && (error as NSError).code == NSUserCancelledError
    }

    /// Moves the dialog's cursor to the option with the arrow keys and
    /// presses Enter. Before any key, the pane the dialog was read from must
    /// still hold the same agent session at the same wait, and a fresh read
    /// must show the same question and the same options (labels and
    /// descriptions; only the cursor may differ); the pane is checked again
    /// after that read, right before input. For "Type something", the
    /// person's words are typed on that row and submitted. Then the screen
    /// is read once more: a question still there means it did not go through.
    private func choose(_ option: FleetChoiceDialog.Option, words: String?) {
        guard let shown = dialog, let seen, let fingerprint else { return }
        // Own words go only to the question they were written for.
        if words != nil, ownWordsFor != fingerprint { return }
        // One choice per agent at a time, wherever it was started from.
        let agentID = hosted.id
        guard sends.begin(agentID) else { return }
        error = nil
        Task {
            do {
                var target = try live(seen)
                let current = try await capture(host: target.host, pane: target.pane)
                guard let current, current.question == shown.question, current.options == shown.options else {
                    dialog = current
                    throw MuxaIPCError.server(String(localized: "The question on screen changed. Check it again before choosing."))
                }
                guard let moves = current.moves(to: option.number) else {
                    throw MuxaIPCError.server(String(localized: "Muxa could not tell which option is selected. Open the pane to choose."))
                }
                target = try live(seen)
                if let words = words?.trimmingCharacters(in: .whitespacesAndNewlines), !words.isEmpty {
                    if !moves.isEmpty {
                        try await model.client.sendFleetKeys(host: target.host, pane: target.pane, keys: moves)
                        try await Task.sleep(nanoseconds: 200_000_000)
                        target = try live(seen)
                    }
                    try await model.client.sendFleetPrompt(host: target.host, pane: target.pane, text: words, submit: true)
                    ownWords = ""
                } else {
                    try await model.client.sendFleetKeys(host: target.host, pane: target.pane, keys: moves + ["enter"])
                }
                try await Task.sleep(nanoseconds: 900_000_000)
                let after = try? await capture(host: target.host, pane: target.pane)
                if let after, after.question == shown.question, after.options == shown.options {
                    dialog = after
                    throw MuxaIPCError.server(String(localized: "The choice did not go through; the question is still on screen. Open the pane to answer."))
                }
                dialog = after
                ownWordsFor = nil
                sends.finish(agentID, .init(succeeded: true, message: String(localized: "Chose \(option.number). \(option.label)")))
            } catch {
                sends.finish(agentID, Self.isCancellation(error) ? nil : .init(succeeded: false, message: error.localizedDescription))
            }
        }
    }
}

/// Another agent in the same unit that waits on the person: who it is, what
/// it asks, and its own answer, so one unit with several asking agents does
/// not hide all but the first.
struct FleetWaitingAgent<Answer: View>: View {
    @ObservedObject var model: AppModel
    let hosted: MuxaHostedAgent
    let open: (MuxaHostedAgent) -> Void
    @ViewBuilder var answer: () -> Answer

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 6) {
                AgentStatusTag(status: hosted.agent.status, showsLabel: false)
                Text(FleetBoard.owner(hosted))
                    .font(MuxaType.body.weight(.medium))
                Spacer(minLength: 4)
                Button("Open") { open(hosted) }
                    .buttonStyle(.muxaSecondary)
                    .controlSize(.small)
            }
            Text(FleetBoard.question(for: hosted.agent, now: .now))
                .font(MuxaType.body)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
            if hosted.pane != nil, ["waiting_input", "waiting_choice"].contains(hosted.agent.state) {
                FleetChoicePanel(model: model, hosted: hosted, fallback: answer)
                    .id(hosted.id)
            }
        }
        .padding(10)
        .background(Color.orange.opacity(0.07), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
    }
}

/// Choices being sent, by agent, kept outside any one panel: a panel closed
/// and reopened mid-send shows the send in flight, and no second choice
/// starts for that agent until the first one ends.
@MainActor
final class FleetChoiceSends: ObservableObject {
    static let shared = FleetChoiceSends()

    struct Outcome: Equatable {
        let succeeded: Bool
        let message: String
    }

    @Published private var pending: Set<String> = []
    @Published private var outcomes: [String: Outcome] = [:]

    func isPending(_ agentID: String) -> Bool { pending.contains(agentID) }
    func outcome(_ agentID: String) -> Outcome? { outcomes[agentID] }

    /// Starts a send for the agent; false when one is already in flight.
    func begin(_ agentID: String) -> Bool {
        guard !pending.contains(agentID) else { return false }
        pending.insert(agentID)
        outcomes[agentID] = nil
        return true
    }

    func finish(_ agentID: String, _ outcome: Outcome?) {
        pending.remove(agentID)
        outcomes[agentID] = outcome
    }
}
