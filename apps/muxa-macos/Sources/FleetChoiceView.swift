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
    @State private var loading = false
    @State private var sending = false
    @State private var ownWords = ""
    @State private var status: String?
    @State private var error: String?

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
                    HStack(alignment: .bottom, spacing: 8) {
                        TextField("Your own answer", text: $ownWords, axis: .vertical)
                            .textFieldStyle(.roundedBorder)
                            .lineLimit(1...5)
                            .disabled(sending)
                        Button("Answer") { choose(typed, words: ownWords) }
                            .buttonStyle(.muxaPrimary)
                            .disabled(sending || ownWords.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
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
            } else if let status {
                Label(status, systemImage: "checkmark.circle.fill")
                    .font(MuxaType.meta)
                    .foregroundStyle(.green)
            }
        }
    }

    private func capture() async throws -> FleetChoiceDialog? {
        guard let pane = hosted.pane else { return nil }
        let capture = try await model.client.captureFleetPane(host: hosted.host, pane: pane)
        return capture.screenText.flatMap(FleetChoiceDialog.parse)
    }

    private func load() async {
        loading = true
        defer { loading = false }
        do {
            let read = try await capture()
            guard !Task.isCancelled else { return }
            dialog = read
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
    /// presses Enter, after reading the screen again: the same agent
    /// session, still waiting, with the same question and the same option
    /// at that number. For "Type something", the person's words are typed
    /// on that row and submitted. Then the screen is read once more: a
    /// question still there means the choice did not go through.
    private func choose(_ option: FleetChoiceDialog.Option, words: String?) {
        guard let shown = dialog, let pane = hosted.pane else { return }
        sending = true
        error = nil
        status = nil
        Task {
            defer { sending = false }
            do {
                guard let live = model.hostedAgents.first(where: { $0.id == hosted.id }),
                      live.agent.agentSessionID == hosted.agent.agentSessionID,
                      live.agent.state == "waiting_choice" || live.agent.state == "waiting_input" else {
                    throw MuxaIPCError.server(String(localized: "The agent moved on. Open its pane to see where it is."))
                }
                let current = try await capture()
                guard let current, current.question == shown.question,
                      current.options.first(where: { $0.number == option.number })?.label == option.label else {
                    dialog = current
                    throw MuxaIPCError.server(String(localized: "The question on screen changed. Check it again before choosing."))
                }
                guard let moves = current.moves(to: option.number) else {
                    throw MuxaIPCError.server(String(localized: "Muxa could not tell which option is selected. Open the pane to choose."))
                }
                if let words = words?.trimmingCharacters(in: .whitespacesAndNewlines), !words.isEmpty {
                    if !moves.isEmpty {
                        try await model.client.sendFleetKeys(host: hosted.host, pane: pane, keys: moves)
                        try await Task.sleep(nanoseconds: 200_000_000)
                    }
                    try await model.client.sendFleetPrompt(host: hosted.host, pane: pane, text: words, submit: true)
                    ownWords = ""
                } else {
                    try await model.client.sendFleetKeys(host: hosted.host, pane: pane, keys: moves + ["enter"])
                }
                try await Task.sleep(nanoseconds: 900_000_000)
                let after = try? await capture()
                if let after, after.question == shown.question {
                    dialog = after
                    throw MuxaIPCError.server(String(localized: "The choice did not go through; the question is still on screen. Open the pane to answer."))
                }
                dialog = after
                status = String(localized: "Chose \(option.number). \(option.label)")
            } catch {
                if Self.isCancellation(error) { return }
                self.error = error.localizedDescription
            }
        }
    }
}
