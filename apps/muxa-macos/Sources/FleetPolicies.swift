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

    /// An approval rule for requests whose dialog shows `text`, or matches
    /// it as a regular expression. `work` narrows it to one Work.
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
    /// request matches itself and nothing else.
    static func escape(_ text: String) -> String {
        let meta: Set<Character> = ["\\", ".", "+", "*", "?", "(", ")", "|", "[", "]", "{", "}", "^", "$", "#", "&", "-", "~"]
        var escaped = ""
        for character in text {
            if meta.contains(character) { escaped.append("\\") }
            escaped.append(character)
        }
        return escaped
    }

    /// The request a confirmation dialog on `screen` asks about, read the
    /// way muxad reads it before Enter: the lines above the one selected
    /// row, up to the dialog's top edge and at most 14 lines, box sides
    /// removed. A preview for the rule sheet; muxad decides on its own copy.
    static func dialogText(from screen: String) -> String? {
        var lines = screen.components(separatedBy: .newlines)
        while let last = lines.last, last.trimmingCharacters(in: .whitespaces).isEmpty { lines.removeLast() }
        lines = Array(lines.suffix(30))
        let cursors = lines.indices.filter { isCursor(lines[$0]) }
        guard cursors.count == 1, let selected = cursors.first else { return nil }
        var start = max(0, selected - 14)
        for index in stride(from: selected - 1, through: start, by: -1) where isBoundary(lines[index]) {
            start = index + 1
            break
        }
        let text = lines[start..<selected]
            .map { $0.trimmingCharacters(in: .whitespaces).trimmingCharacters(in: CharacterSet(charactersIn: "│|")).trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
            .joined(separator: "\n")
        return text.isEmpty ? nil : text
    }

    /// Whether `pattern` (a regex, or literal text) finds the request.
    static func matches(_ text: String, isPattern: Bool, request: String) -> Bool {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return false }
        guard isPattern else { return request.contains(trimmed) }
        guard let regex = try? NSRegularExpression(pattern: trimmed, options: [.anchorsMatchLines]) else { return false }
        return regex.firstMatch(in: request, range: NSRange(request.startIndex..., in: request)) != nil
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

    private static func isBoundary(_ line: String) -> Bool {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        if trimmed.hasPrefix("╭") || trimmed.hasPrefix("┌") { return true }
        return trimmed.prefix(6).count == 6 && trimmed.prefix(6).allSatisfy { "─━═-".contains($0) }
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
