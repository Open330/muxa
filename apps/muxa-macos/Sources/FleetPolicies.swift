// Fleet Home's automatic handling: what the person lets muxad do on its own.
//
// Two kinds of policy show on the board. Usage limits keep the existing
// three-way choice (`MuxaLimitPolicy`). Approvals are `press_enter` rules
// the person writes, each naming the request it may confirm with a
// `screen` pattern, so nothing is approved that the person did not name.
import Foundation

enum FleetApprovalRules {
    /// Mirrors the starter `confirm-selected-yes` guards: a short settle
    /// before Enter, no jitter, and a per-pane rate the person would notice.
    static let wait = "5s"
    static let jitter = "0s"
    static let cooldown = "30s"
    static let maxPerHour = 10

    /// The agents whose confirmation dialogs muxad can read.
    static let supportedKinds: [String] = ["claude_code", "codex"]

    /// A rule the person wrote to approve requests they named.
    static func isApproval(_ rule: MuxaAutomationRule) -> Bool {
        rule.action == .pressEnter && !(rule.screen ?? "").trimmingCharacters(in: .whitespaces).isEmpty
    }

    /// An approval rule for the shell command `text` exactly, or every
    /// command the regex matches in full. `work` narrows it to one Work.
    static func make(
        text: String,
        isPattern: Bool,
        agents: [String],
        work: MuxaWorkIdentity?,
        existingNames: Set<String>
    ) -> MuxaAutomationRule {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return MuxaAutomationRule(
            name: name(for: trimmed, existing: existingNames),
            on: .waitingInput,
            enabled: true,
            agent: agents.sorted(),
            workspace: work?.workspaceID,
            work: work.map { "^\(escape($0.workID))$" },
            screen: isPattern ? trimmed : escape(trimmed),
            wait: wait,
            jitter: jitter,
            action: .pressEnter,
            maxPerHour: maxPerHour,
            cooldown: cooldown
        )
    }

    /// `approve-<words of the request>`, unique among the rules there are.
    static func name(for text: String, existing: Set<String>) -> String {
        let words = text.lowercased()
            .map { $0.isASCII && ($0.isLetter || $0.isNumber) ? $0 : "-" }
        let slug = String(words)
            .split(separator: "-", omittingEmptySubsequences: true)
            .joined(separator: "-")
            .prefix(32)
        let base = slug.isEmpty ? "approve" : "approve-\(slug)"
        guard existing.contains(base) else { return base }
        var index = 2
        while existing.contains("\(base)-\(index)") { index += 1 }
        return "\(base)-\(index)"
    }

    /// Escapes the characters Rust's `regex::escape` escapes, so a literal
    /// request matches itself and nothing else. Works on Unicode scalars,
    /// not grapheme clusters: a `.` followed by a combining mark is still a
    /// `.` to the daemon's regex and must be escaped.
    static func escape(_ text: String) -> String {
        let meta: Set<Unicode.Scalar> = ["\\", ".", "+", "*", "?", "(", ")", "|", "[", "]", "{", "}", "^", "$", "#", "&", "-", "~"]
        var escaped = String.UnicodeScalarView()
        for scalar in text.unicodeScalars {
            if meta.contains(scalar) { escaped.append("\\") }
            escaped.append(scalar)
        }
        return String(escaped)
    }

    /// The command a confirmation dialog on `screen` asks to run, read the
    /// way muxad reads it: exactly one Codex `Would you like to run the
    /// following command?` heading with its single `$ …` line, or one Claude
    /// `Bash command` heading with the lines before `Do you want to
    /// proceed?`. A preview: the capture here has no styling, so a Claude
    /// description still shows as a last line; muxad leaves it out.
    static func commandPreview(from screen: String) -> [String]? {
        var lines = screen.components(separatedBy: .newlines)
        while let last = lines.last, last.trimmingCharacters(in: .whitespaces).isEmpty { lines.removeLast() }
        lines = Array(lines.suffix(30))
        let cursors = lines.indices.filter { isCursor(lines[$0]) }
        guard cursors.count == 1, let selected = cursors.first else { return nil }
        let start = max(0, selected - 14)
        let headers = (start..<selected).filter { index in
            let text = content(lines[index]).lowercased()
            return text == "would you like to run the following command?" || text == "bash command"
        }
        guard headers.count == 1, let header = headers.first else { return nil }
        let body = (header + 1..<selected).map { content(lines[$0]) }.filter { !$0.isEmpty }
        if content(lines[header]).lowercased() == "bash command" {
            guard let question = body.firstIndex(where: { $0.lowercased() == "do you want to proceed?" }) else { return nil }
            let command = Array(body[..<question])
            return command.isEmpty ? nil : command
        }
        guard let first = body.first, first.hasPrefix("$ ") else { return nil }
        return [String(first.dropFirst(2))]
    }

    /// Whether the text (literal, or a regex) matches one of the preview's
    /// lines in full, as muxad matches the whole command.
    static func matches(_ text: String, isPattern: Bool, command: [String]) -> Bool {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return false }
        guard isPattern else { return command.contains(trimmed) }
        guard let regex = try? NSRegularExpression(pattern: "\\A(?:\(trimmed))\\z") else { return false }
        return command.contains { line in
            regex.firstMatch(in: line, range: NSRange(line.startIndex..., in: line)) != nil
        }
    }

    static func isValidPattern(_ text: String) -> Bool {
        (try? NSRegularExpression(pattern: text)) != nil
    }

    private static func isCursor(_ line: String) -> Bool {
        var rest = Substring(line).drop { $0 == " " || $0 == "\t" }
        if rest.first == "│" { rest = rest.dropFirst().drop { $0 == " " || $0 == "\t" } }
        guard let first = rest.first, ">›❯".contains(first) else { return false }
        let after = rest.dropFirst()
        return after.isEmpty || after.first == " " || after.first == "\t"
    }

    private static func content(_ line: String) -> String {
        line.trimmingCharacters(in: .whitespaces)
            .trimmingCharacters(in: CharacterSet(charactersIn: "│|"))
            .trimmingCharacters(in: .whitespaces)
    }
}

/// What automatic handling did recently, for the one line on the board.
enum FleetAutomationSummary {
    /// Firings that acted (not skips) in the last day, newest first.
    static func handled(_ log: [MuxaAutomationLogEntry], now: Date = .now) -> [MuxaAutomationLogEntry] {
        let since = now.addingTimeInterval(-24 * 3600)
        return log.filter { entry in
            guard entry.outcome == .fired || entry.outcome == .admitted,
                  let at = entry.firedAt.flatMap(MuxaUsageTimestamp.parse) else { return false }
            return at >= since
        }
    }
}
