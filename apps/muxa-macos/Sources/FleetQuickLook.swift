// Fleet Home's quick look: a short look at one agent without leaving the
// board. A summary of where it is (the question or what it is doing, what
// it was asked, its latest result, its own recap), the answer field, and
// the raw pane one click away; Open Pane goes to the full view.
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

    /// The opening of a longer text, for a summary line: Markdown markers
    /// and blank lines removed, cut near `limit` characters at the end of a
    /// sentence or word.
    static func digest(_ text: String?, limit: Int = 320) -> String? {
        guard let text else { return nil }
        let lines = text.components(separatedBy: .newlines)
            .map { line -> String in
                var line = line.trimmingCharacters(in: .whitespaces)
                while let first = line.first, "#>*-`".contains(first) {
                    line = String(line.dropFirst()).trimmingCharacters(in: .whitespaces)
                }
                return line
            }
            .filter { !$0.isEmpty && !$0.allSatisfy { "-=*_`|".contains($0) } }
        let joined = lines.joined(separator: " ")
        guard !joined.isEmpty else { return nil }
        guard joined.count > limit else { return joined }
        let head = String(joined.prefix(limit))
        if let stop = head.lastIndex(where: { ".!?。".contains($0) }), head.distance(from: head.startIndex, to: stop) > limit / 2 {
            return String(head[...stop])
        }
        if let space = head.lastIndex(of: " ") { return String(head[..<space]) + "…" }
        return head + "…"
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
    @State private var showsPane = false

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
                if hosted.agent.state == "waiting_choice", hosted.pane != nil {
                    FleetChoicePanel(model: model, hosted: hosted)
                }
                summary(hosted)
                if showsReply, let work {
                    FleetReplyField(work: work, draft: $draft, submit: submit)
                }
            } else {
                Text("This agent is no longer running.")
                    .foregroundStyle(.secondary)
            }
            if let hosted, hosted.pane != nil {
                DisclosureGroup("Show pane", isExpanded: $showsPane) {
                    preview(hosted)
                        .padding(.top, 6)
                }
                .font(MuxaType.detail)
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
        .frame(width: 560)
        .task(id: showsPane) { if showsPane { await follow() } }
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
        let agent = hosted.agent
        let status = agent.status
        let now = status.needsAttention
            ? FleetBoard.question(for: agent, now: .now)
            : FleetBoard.statusLine(for: agent, now: .now)
        let since = MuxaUsageTimestamp.parse(agent.stateEnteredAt ?? agent.lastActivityAt)
        return VStack(alignment: .leading, spacing: 12) {
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    Text(status.needsAttention ? "Needs you" : "Now")
                        .font(.system(size: 11, weight: .bold))
                        .foregroundStyle(status.needsAttention ? Color.orange : Color.secondary)
                    if let since {
                        Text(since, format: .relative(presentation: .numeric, unitsStyle: .abbreviated))
                            .font(MuxaType.meta)
                            .foregroundStyle(.tertiary)
                            .monospacedDigit()
                    }
                }
                Text(now)
                    .font(.system(size: 13.5, weight: status.needsAttention ? .semibold : .regular))
                    .fixedSize(horizontal: false, vertical: true)
                    .textSelection(.enabled)
            }
            if let work, work.stages.count > 1 {
                summaryRow("Stages", work.stages.map { "\($0.name) \(stageMark($0.state))" }.joined(separator: " → "))
            }
            if let asked = FleetQuickLookText.digest(agent.lastPrompt, limit: 200) {
                summaryRow("Asked", asked)
            }
            if let result = FleetQuickLookText.digest(agent.lastResponse), result != now {
                summaryRow("Latest result", result)
            }
            if let recap = FleetQuickLookText.digest(agent.recap), recap != now {
                summaryRow("Session recap", recap)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
    }

    private func summaryRow(_ title: LocalizedStringKey, _ text: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(title)
                .font(.system(size: 11, weight: .bold))
                .foregroundStyle(.secondary)
            Text(text)
                .font(MuxaType.body)
                .foregroundStyle(.primary.opacity(0.85))
                .lineLimit(4)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
        }
    }

    private func stageMark(_ state: FleetWorkStage.State) -> String {
        switch state {
        case .done: "✓"
        case .running: "●"
        case .needsYou: "!"
        case .waiting: "○"
        }
    }

    @ViewBuilder
    private func preview(_ hosted: MuxaHostedAgent) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text("Bottom of the pane")
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
