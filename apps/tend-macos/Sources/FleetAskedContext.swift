import SwiftUI

/// What an agent's question is about: the response its turn ended on, shown
/// under the question. The question is one sentence ("레이블 정리 PR을 만들어서
/// 배포까지 진행할까요?"); what it refers to, and what a yes changes, sits in
/// the paragraphs above it. The view opens scrolled to the end, where the
/// question is, so the context right above it reads first.
struct FleetAskedContext: View {
    let agent: TendAgent

    var body: some View {
        if let source = Self.context(for: agent) {
            VStack(alignment: .leading, spacing: 6) {
                Text("What it's asking about")
                    .font(.system(size: 11, weight: .bold))
                    .foregroundStyle(.secondary)
                ScrollViewReader { proxy in
                    ScrollView {
                        VStack(alignment: .leading, spacing: 0) {
                            Text(TendMarkdownText.attributedString(markdown: source))
                                .font(TendType.body)
                                .fixedSize(horizontal: false, vertical: true)
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading)
                            Color.clear.frame(height: 1).id(Self.endID)
                        }
                        .padding(10)
                    }
                    .frame(maxHeight: 260)
                    .background(Color.primary.opacity(0.04), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                    .onAppear { proxy.scrollTo(Self.endID, anchor: .bottom) }
                    .onChange(of: source) { _ in proxy.scrollTo(Self.endID, anchor: .bottom) }
                }
            }
        }
    }

    private static let endID = "end"

    /// The response behind an `.asked` status, or nil when there is nothing
    /// beyond the question itself to show.
    nonisolated static func context(for agent: TendAgent, now: Date = .now) -> String? {
        guard case .asked(let question) = TendAgentStatus(agent: agent, now: now),
              let response = agent.lastResponse else { return nil }
        let body = withoutLeadingQuote(response).trimmingCharacters(in: .whitespacesAndNewlines)
        guard !body.isEmpty else { return nil }
        // A response that is the question and nothing more adds nothing.
        let plain = body.replacingOccurrences(of: "**", with: "")
        if plain == question || plain.count <= question.count + 2 { return nil }
        return body
    }

    /// Drops a block quote or rule the response opens with (a restated
    /// prompt, a coaching note): it is about the person's message, not the
    /// question.
    nonisolated static func withoutLeadingQuote(_ response: String) -> String {
        var lines = response.components(separatedBy: "\n")[...]
        while let first = lines.first {
            let line = first.trimmingCharacters(in: .whitespaces)
            let isRule = !line.isEmpty && line.allSatisfy { "-*_".contains($0) }
            if line.isEmpty || line.hasPrefix(">") || isRule {
                lines = lines.dropFirst()
            } else {
                break
            }
        }
        return lines.joined(separator: "\n")
    }
}
