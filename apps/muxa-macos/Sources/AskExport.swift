import AppKit
import Foundation
import UniformTypeIdentifiers

/// How a Global Ask exchange leaves the app: as Markdown, for notes and
/// issues, or as a prompt, for handing the thread to another agent.
///
/// The formatters are pure — the provider's display title comes in as a
/// closure because the store that knows it is main-actor state — so the exact
/// text is unit-tested. Nothing here is localized: the output is a document
/// for a reader outside the app, and for a prompt that reader is a model.
enum AskExport {
    /// One exchange, or a whole conversation when `title` is given.
    ///
    /// A conversation leads with its title and a line saying where it came
    /// from; a single turn is just the two messages, ready to paste under
    /// whatever heading the person already has.
    static func markdown(
        _ entries: [MuxaAskEntry],
        title: String? = nil,
        providerTitle: (String) -> String
    ) -> String {
        var sections: [String] = []
        if let title = title?.trimmingCharacters(in: .whitespacesAndNewlines), !title.isEmpty {
            sections.append("# \(title)")
            if let first = entries.first {
                let source = ["Global Ask", providerTitle(first.agent), day(of: first.askedAt)]
                    .compactMap { $0 }
                    .joined(separator: " · ")
                sections.append("_\(source)_")
            }
        }
        for entry in entries {
            sections.append("## You\n\n\(markdownBody(trimmed(entry.prompt)))")
            sections.append("## \(providerTitle(entry.agent))\n\n\(markdownBody(reply(of: entry)))")
        }
        return sections.joined(separator: "\n\n") + "\n"
    }

    /// The thread as context for another agent. It ends on a blank line: what
    /// the person wants done with it is theirs to type.
    static func prompt(_ entries: [MuxaAskEntry], providerTitle: (String) -> String) -> String {
        guard let first = entries.first else { return "" }
        let who = providerTitle(first.agent)
        var lines = [
            "Here is a conversation I had with \(who) in muxa Global Ask. "
                + "Use it as context for what follows.",
            "",
            "<conversation>",
        ]
        for entry in entries {
            lines.append("<user>")
            lines.append(promptBody(trimmed(entry.prompt)))
            lines.append("</user>")
            lines.append("<assistant name=\"\(attribute(providerTitle(entry.agent)))\">")
            lines.append(promptBody(reply(of: entry)))
            lines.append("</assistant>")
        }
        lines.append("</conversation>")
        lines.append("")
        return lines.joined(separator: "\n") + "\n"
    }

    /// `<title>.md`, with the characters a file name cannot hold removed.
    static func fileName(title: String?) -> String {
        let cleaned = (title ?? "")
            .components(separatedBy: CharacterSet(charactersIn: "/\\:?%*|\"<>\n\r\t"))
            .joined(separator: " ")
            .split(separator: " ")
            .joined(separator: " ")
            .drop { $0 == "." || $0 == " " }
        let stem = cleaned.isEmpty ? "Global Ask" : String(cleaned.prefix(80))
        return "\(stem).md"
    }

    /// A message placed under the export's own `##` headings: its headings
    /// move two levels down so they read as part of the message, and a code
    /// fence it left open — an answer cut off mid-block — is closed, so it
    /// cannot swallow the headings that follow.
    static func markdownBody(_ text: String) -> String {
        var fence: Substring?
        var lines: [String] = []
        for line in text.split(separator: "\n", omittingEmptySubsequences: false) {
            if track(&fence, past: line) {
                lines.append(String(line))
                continue
            }
            if fence == nil, line.hasPrefix("#") {
                let level = line.prefix { $0 == "#" }.count
                let rest = line.dropFirst(level)
                if level <= 6, rest.isEmpty || rest.first == " " {
                    lines.append(String(repeating: "#", count: min(level + 2, 6)) + rest)
                    continue
                }
            }
            lines.append(String(line))
        }
        if let open = fence {
            lines.append(String(open))
        }
        return lines.joined(separator: "\n")
    }

    /// A message placed between the prompt's own tags. Only the closing tags
    /// of the wrapper are escaped — an answer that explains this very format
    /// would otherwise end its block early — and a code fence it left open is
    /// closed, so the tags after it are not read as code. The rest, code
    /// included, reaches the next model as written.
    static func promptBody(_ text: String) -> String {
        let escaped = text.replacingOccurrences(
            of: "</(?=(conversation|user|assistant)>)",
            with: "&lt;/",
            options: [.regularExpression, .caseInsensitive]
        )
        var fence: Substring?
        for line in escaped.split(separator: "\n", omittingEmptySubsequences: false) {
            track(&fence, past: line)
        }
        return fence.map { "\(escaped)\n\($0)" } ?? escaped
    }

    /// Moves `open` — the code fence a message has open so far — past
    /// `line`, and says whether `line` was a fence at all: it opens one when
    /// none is open, and closes `open` when it is the same kind, at least as
    /// long, with nothing after it.
    @discardableResult
    private static func track(_ open: inout Substring?, past line: Substring) -> Bool {
        let stripped = line.drop { $0 == " " }
        let marker = stripped.prefix { $0 == "`" || $0 == "~" }
        guard marker.count >= 3, Set(marker).count == 1 else { return false }
        if let current = open {
            if marker.first == current.first, marker.count >= current.count,
               stripped.dropFirst(marker.count).allSatisfy(\.isWhitespace) {
                open = nil
            }
        } else {
            open = marker
        }
        return true
    }

    private static func attribute(_ value: String) -> String {
        value
            .replacingOccurrences(of: "&", with: "&amp;")
            .replacingOccurrences(of: "\"", with: "&quot;")
            .replacingOccurrences(of: "<", with: "&lt;")
    }

    /// What stands in the answer's place: the answer, or why there is none.
    private static func reply(of entry: MuxaAskEntry) -> String {
        let answer = trimmed(entry.answer)
        if !answer.isEmpty { return answer }
        if let error = entry.error.map(trimmed), !error.isEmpty { return "(failed: \(error))" }
        return entry.status == "running" ? "(no answer yet)" : "(no answer)"
    }

    /// `2026-09-20` from muxad's ISO-8601 timestamp. Read off the string, not
    /// parsed, so the export does not depend on the reader's time zone.
    private static func day(of timestamp: String) -> String? {
        let day = timestamp.prefix(10)
        let shape = day.enumerated().allSatisfy { index, character in
            index == 4 || index == 7 ? character == "-" : character.isNumber
        }
        return day.count == 10 && shape ? String(day) : nil
    }

    private static func trimmed(_ text: String) -> String {
        text.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

extension AskExport {
    @MainActor
    static func copy(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }

    /// Asks where to put the Markdown and writes it. Returns the file, nil
    /// when the person cancels, and throws what the write threw.
    @MainActor
    static func save(_ markdown: String, suggestedName: String) throws -> URL? {
        let panel = NSSavePanel()
        panel.title = String(localized: "Export Global Ask conversation")
        panel.prompt = String(localized: "Export")
        panel.nameFieldStringValue = suggestedName
        panel.allowedContentTypes = [UTType(filenameExtension: "md") ?? .plainText]
        guard panel.runModal() == .OK, let url = panel.url else { return nil }
        try Data(markdown.utf8).write(to: url, options: .atomic)
        return url
    }
}
