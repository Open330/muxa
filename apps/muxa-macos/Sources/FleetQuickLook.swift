// Fleet Home's quick look: a short look at one agent without leaving the
// board. Its state, the question or latest line, the bottom of its pane, and
// the answer field; Open Pane goes to the full view only when wanted.
import SwiftUI

struct FleetQuickLookTarget: Identifiable, Equatable {
    let agentID: String
    var id: String { agentID }
}

enum FleetQuickLookText {
    /// Removes terminal control sequences (CSI, OSC, other escapes) and the
    /// blank padding below the content, keeping the last `lines` lines.
    static func plain(_ screen: String, lines: Int = 40) -> String {
        var text = screen
        let patterns = [
            "\u{1B}\\][^\u{07}\u{1B}]*(?:\u{07}|\u{1B}\\\\)", // OSC … BEL / ST
            "\u{1B}\\[[0-?]*[ -/]*[@-~]", // CSI
            "\u{1B}[@-Z\\\\-_]", // other two-byte escapes
        ]
        for pattern in patterns {
            text = text.replacingOccurrences(of: pattern, with: "", options: .regularExpression)
        }
        var rows = text.components(separatedBy: .newlines).map { row in
            String(row.reversed().drop(while: { $0 == " " }).reversed())
        }
        while let last = rows.last, last.isEmpty { rows.removeLast() }
        return rows.suffix(lines).joined(separator: "\n")
    }
}

struct FleetQuickLookView: View {
    @ObservedObject var model: AppModel
    let target: FleetQuickLookTarget
    /// The unit the agent belongs to, as the board has it now.
    let work: FleetWork?
    @Binding var draft: FleetReplyDraft
    let submit: () -> Void
    let openFull: (MuxaHostedAgent) -> Void
    @Environment(\.dismiss) private var dismiss

    @State private var screen: String?
    @State private var captureError: String?

    private var hosted: MuxaHostedAgent? {
        model.hostedAgents.first { $0.id == target.agentID }
    }

    /// The answer field belongs here only when the unit's reply goes to this
    /// agent, or a draft for it is still open.
    private var showsReply: Bool {
        guard let work else { return false }
        if let reply = FleetBoard.replyTarget(for: work) { return reply.agentID == target.agentID }
        return draft.target?.agentID == target.agentID && !draft.isIdle
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            header
            if let hosted {
                summary(hosted)
                preview(hosted)
                if showsReply, let work {
                    FleetReplyField(work: work, draft: $draft, submit: submit)
                }
            } else {
                Text("This agent is no longer running.")
                    .foregroundStyle(.secondary)
            }
            HStack {
                Spacer()
                Button("Close") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                if let hosted {
                    Button(hosted.pane == nil ? "Open Agent" : "Open Pane") {
                        dismiss()
                        openFull(hosted)
                    }
                    .buttonStyle(.muxaPrimary)
                    .keyboardShortcut(.return, modifiers: .command)
                    .help("Go to the full view (⌘↩)")
                }
            }
        }
        .padding(20)
        .frame(width: 620)
        .frame(minHeight: 360)
        .task(id: target.agentID) { await follow() }
    }

    private var header: some View {
        HStack(alignment: .top, spacing: 10) {
            VStack(alignment: .leading, spacing: 4) {
                Text(work?.title ?? hosted.map(FleetBoard.owner) ?? "")
                    .font(.system(size: 16, weight: .semibold))
                    .lineLimit(2)
                if let hosted {
                    HStack(spacing: 6) {
                        AgentStatusTag(status: hosted.agent.status)
                        Text(FleetBoard.owner(hosted))
                            .foregroundStyle(.secondary)
                        if let repo = work?.repo {
                            Text("· \(repo)")
                                .foregroundStyle(.secondary)
                        }
                    }
                    .font(MuxaType.meta)
                }
            }
            Spacer(minLength: 0)
        }
    }

    private func summary(_ hosted: MuxaHostedAgent) -> some View {
        let status = hosted.agent.status
        let line = status.needsAttention
            ? FleetBoard.question(for: hosted.agent, now: .now)
            : FleetBoard.statusLine(for: hosted.agent, now: .now)
        return Text(line)
            .font(.system(size: 13.5, weight: status.needsAttention ? .semibold : .regular))
            .fixedSize(horizontal: false, vertical: true)
            .textSelection(.enabled)
    }

    @ViewBuilder
    private func preview(_ hosted: MuxaHostedAgent) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(hosted.pane == nil ? "Last response" : "Pane")
                    .font(MuxaType.meta.weight(.semibold))
                    .foregroundStyle(.secondary)
                Spacer()
                if hosted.pane != nil {
                    Text("Live")
                        .font(MuxaType.meta)
                        .foregroundStyle(.tertiary)
                }
            }
            ScrollViewReader { proxy in
                ScrollView {
                    Text(previewText(hosted))
                        .font(.system(size: 11.5, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(10)
                        .id("end")
                }
                .frame(height: 240)
                .background(Color.black.opacity(0.82), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                .foregroundStyle(Color.white.opacity(0.9))
                .onChange(of: screen) { _ in proxy.scrollTo("end", anchor: .bottom) }
                .onAppear { proxy.scrollTo("end", anchor: .bottom) }
            }
            if let captureError {
                Text(captureError)
                    .font(MuxaType.meta)
                    .foregroundStyle(.red)
            }
        }
    }

    private func previewText(_ hosted: MuxaHostedAgent) -> String {
        if hosted.pane == nil {
            return hosted.agent.lastResponse ?? hosted.agent.recap ?? String(localized: "No output yet.")
        }
        return screen ?? String(localized: "Loading…")
    }

    /// Keeps the preview current while the quick look is open.
    private func follow() async {
        while !Task.isCancelled {
            if let hosted, let pane = hosted.pane {
                do {
                    let capture = try await model.client.captureFleetPane(host: hosted.host, pane: pane)
                    screen = capture.screenText.map { FleetQuickLookText.plain($0) } ?? ""
                    captureError = nil
                } catch {
                    captureError = error.localizedDescription
                }
            }
            try? await Task.sleep(nanoseconds: 2_000_000_000)
        }
    }
}
