// A choice dialog on an agent's screen, read so Home can show the question
// and let the person pick an option without opening the pane.
import Foundation

struct FleetChoiceDialog: Equatable, Sendable {
    struct Option: Equatable, Sendable, Identifiable {
        let number: Int
        let label: String
        /// The indented lines under the option, joined.
        let detail: String?
        var id: Int { number }

        /// Claude's "Type something." / "Other": picking it opens a field
        /// for the person's own words.
        var takesText: Bool {
            let lower = label.lowercased()
            return lower.hasPrefix("type something") || lower == "other" || lower.hasPrefix("type here")
        }
    }

    let question: String
    let options: [Option]
    /// The option the dialog's cursor is on.
    var selected: Int?

    /// Keys that move the cursor from where it is to `number`: arrows only,
    /// so nothing is typed into the dialog. Nil when the cursor is unknown.
    func moves(to number: Int) -> [String]? {
        guard let selected, options.contains(where: { $0.number == number }) else { return nil }
        let steps = number - selected
        return Array(repeating: steps > 0 ? "down" : "up", count: abs(steps))
    }

    /// The question and options of the newest numbered choice on `screen`:
    /// a block of `N. label` rows (one of them selected with ❯ › or >),
    /// numbered 1, 2, 3… with any indented detail lines between them, and
    /// the text above the first row up to a rule or a blank-line gap.
    /// Nil unless that shape is there.
    static func parse(_ screen: String) -> FleetChoiceDialog? {
        let lines = FleetQuickLookText.plain(screen, lines: 60).components(separatedBy: "\n")
            .map(leftColumn)
            .map { line in line.trimmingCharacters(in: CharacterSet(charactersIn: "│ ")).isEmpty ? "" : line }
        // The last option row, then walk up to option 1.
        guard let lastRow = lines.indices.last(where: { optionRow(lines[$0]) != nil }) else { return nil }
        var rows: [(index: Int, number: Int, label: String)] = []
        var selected: Int?
        var index = lastRow
        while index >= 0 {
            if let row = optionRow(lines[index]) {
                rows.insert((index, row.number, row.label), at: 0)
                if isSelected(lines[index]) { selected = row.number }
                if row.number == 1 { break }
            } else if isRule(lines[index]) {
                // Claude sets "Chat about this" apart below a rule.
            } else if !isDetail(lines[index]) {
                return nil
            }
            index -= 1
        }
        guard let first = rows.first, first.number == 1,
              rows.map(\.number) == Array(1...rows.count) else { return nil }
        var options: [Option] = []
        for (offset, row) in rows.enumerated() {
            let end = offset + 1 < rows.count ? rows[offset + 1].index : min(lines.count, row.index + 4)
            let detail = lines[(row.index + 1)..<end]
                .filter { isDetail($0) && !isRule($0) }
                .map { $0.trimmingCharacters(in: CharacterSet(charactersIn: "│ ")) }
                .filter { !$0.isEmpty }
                .joined(separator: " ")
            options.append(Option(number: row.number, label: row.label, detail: detail.isEmpty ? nil : detail))
        }
        // The question: text above option 1, up to a rule, a ☐ title (kept)
        // or two blank lines.
        var question: [String] = []
        var blanks = 0
        var cursor = first.index - 1
        while cursor >= 0, question.count < 12 {
            let line = lines[cursor].trimmingCharacters(in: CharacterSet(charactersIn: "│ "))
            if line.hasPrefix("─") || line.hasPrefix("╭") || line.hasPrefix("┌") { break }
            if line.isEmpty {
                blanks += 1
                if blanks >= 2 { break }
                cursor -= 1
                continue
            }
            blanks = 0
            question.insert(line, at: 0)
            if line.hasPrefix("☐") || line.hasPrefix("☒") { break }
            cursor -= 1
        }
        let text = question.joined(separator: "\n").trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return nil }
        return FleetChoiceDialog(question: text, options: options, selected: selected)
    }

    private static func optionRow(_ line: String) -> (number: Int, label: String)? {
        let trimmed = line.trimmingCharacters(in: CharacterSet(charactersIn: "│ "))
        var rest = Substring(trimmed)
        if let first = rest.first, "❯›>".contains(first) { rest = rest.dropFirst().drop(while: { $0 == " " }) }
        let digits = rest.prefix(while: \.isNumber)
        guard !digits.isEmpty, digits.count <= 2, let number = Int(digits) else { return nil }
        rest = rest.dropFirst(digits.count)
        guard rest.first == "." || rest.first == ")" else { return nil }
        let label = rest.dropFirst().trimmingCharacters(in: .whitespaces)
        return label.isEmpty ? nil : (number, label)
    }

    /// An option's description: indented text, or a blank line between rows.
    private static func isDetail(_ line: String) -> Bool {
        if line.isEmpty { return true }
        let inner = line.drop(while: { $0 == "│" })
        return inner.hasPrefix("   ") && optionRow(line) == nil && !inner.trimmingCharacters(in: .whitespaces).hasPrefix("─")
    }

    private static func isSelected(_ line: String) -> Bool {
        let trimmed = line.trimmingCharacters(in: CharacterSet(charactersIn: "│ "))
        return trimmed.first.map { "❯›>".contains($0) } ?? false
    }

    private static func isRule(_ line: String) -> Bool {
        let trimmed = line.trimmingCharacters(in: CharacterSet(charactersIn: "│ "))
        return trimmed.count >= 6 && trimmed.allSatisfy { "─━".contains($0) }
    }

    /// The dialog's own column: a TUI that splits the pane draws another
    /// panel after a `│` past a run of spaces; drop that part.
    private static func leftColumn(_ line: String) -> String {
        let body = line.hasPrefix("│") ? String(line.dropFirst()) : line
        guard let split = body.range(of: #"\s{2,}│"#, options: .regularExpression) else { return line }
        return (line.hasPrefix("│") ? "│" : "") + body[..<split.lowerBound]
    }
}
