// The choice an agent is waiting on, answered from Home: the question and
// options read off its screen, an option sent by its number, or the
// person's own words through a "Type something" option.
import SwiftUI

struct FleetChoicePanel: View {
    @ObservedObject var model: AppModel
    let hosted: MuxaHostedAgent

    @State private var dialog: FleetChoiceDialog?
    @State private var loading = false
    @State private var sending = false
    @State private var ownWords = ""
    @State private var status: String?
    @State private var error: String?

    var body: some View {
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
        .task(id: hosted.agent.stateEnteredAt) { await load() }
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
            dialog = try await capture()
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }

    /// Sends the option's number, after reading the screen again: the same
    /// agent session, still waiting, with the same question and the same
    /// option at that number. Then the person's words, when they gave any.
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
                try await model.client.sendFleetPrompt(host: hosted.host, pane: pane, text: "\(option.number)", submit: false)
                if let words = words?.trimmingCharacters(in: .whitespacesAndNewlines), !words.isEmpty {
                    try await Task.sleep(nanoseconds: 400_000_000)
                    try await model.client.sendFleetPrompt(host: hosted.host, pane: pane, text: words, submit: true)
                    ownWords = ""
                }
                status = String(localized: "Sent \(option.number). \(option.label)")
                try? await Task.sleep(nanoseconds: 800_000_000)
                dialog = try? await capture()
            } catch {
                self.error = error.localizedDescription
            }
        }
    }
}
