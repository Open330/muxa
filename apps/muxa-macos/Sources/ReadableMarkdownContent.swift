import AppKit
import SwiftUI

struct ReadableMarkdownDocument: Equatable {
    struct ListItem: Equatable {
        let marker: String
        let text: String
        let depth: Int
    }

    enum Block: Equatable {
        case heading(level: Int, text: String)
        case paragraph(String)
        case list(ordered: Bool, items: [ListItem])
        case quote(String)
        case code(language: String?, source: String)
        case rule
    }

    let blocks: [Block]

    init(source: String) {
        let normalized = source
            .replacingOccurrences(of: "\r\n", with: "\n")
            .replacingOccurrences(of: "\r", with: "\n")
        let lines = normalized.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        blocks = Self.parse(lines)
    }

    private static func parse(_ lines: [String]) -> [Block] {
        var result: [Block] = []
        var index = 0

        while index < lines.count {
            let line = lines[index]
            let trimmed = line.trimmingCharacters(in: .whitespaces)

            if trimmed.isEmpty {
                index += 1
                continue
            }

            if trimmed.hasPrefix("```") {
                let language = String(trimmed.dropFirst(3)).trimmingCharacters(in: .whitespaces)
                index += 1
                var code: [String] = []
                while index < lines.count,
                      !lines[index].trimmingCharacters(in: .whitespaces).hasPrefix("```")
                {
                    code.append(lines[index])
                    index += 1
                }
                if index < lines.count { index += 1 }
                result.append(.code(language: language.isEmpty ? nil : language, source: code.joined(separator: "\n")))
                continue
            }

            if let heading = heading(in: trimmed) {
                result.append(.heading(level: heading.level, text: heading.text))
                index += 1
                continue
            }

            if isRule(trimmed) {
                result.append(.rule)
                index += 1
                continue
            }

            if trimmed.hasPrefix(">") {
                var quoteLines: [String] = []
                while index < lines.count {
                    let candidate = lines[index].trimmingCharacters(in: .whitespaces)
                    guard candidate.hasPrefix(">") else { break }
                    quoteLines.append(String(candidate.dropFirst()).trimmingCharacters(in: .whitespaces))
                    index += 1
                }
                result.append(.quote(quoteLines.joined(separator: "\n")))
                continue
            }

            if let firstItem = listItem(in: line) {
                var items = [firstItem.item]
                let ordered = firstItem.ordered
                index += 1
                while index < lines.count,
                      let item = listItem(in: lines[index]),
                      item.ordered == ordered
                {
                    items.append(item.item)
                    index += 1
                }
                result.append(.list(ordered: ordered, items: items))
                continue
            }

            var paragraph = [line]
            index += 1
            while index < lines.count {
                let candidate = lines[index]
                let candidateTrimmed = candidate.trimmingCharacters(in: .whitespaces)
                if candidateTrimmed.isEmpty || isBlockStart(candidate) { break }
                paragraph.append(candidate)
                index += 1
            }
            result.append(.paragraph(paragraph.joined(separator: "\n")))
        }

        return result
    }

    private static func isBlockStart(_ line: String) -> Bool {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        return trimmed.hasPrefix("```")
            || heading(in: trimmed) != nil
            || isRule(trimmed)
            || trimmed.hasPrefix(">")
            || listItem(in: line) != nil
    }

    private static func heading(in line: String) -> (level: Int, text: String)? {
        let level = line.prefix { $0 == "#" }.count
        guard (1 ... 6).contains(level), line.dropFirst(level).first == " " else { return nil }
        return (level, String(line.dropFirst(level + 1)))
    }

    private static func isRule(_ line: String) -> Bool {
        let compact = line.replacingOccurrences(of: " ", with: "")
        guard compact.count >= 3, let first = compact.first, "-*_".contains(first) else {
            return false
        }
        return compact.allSatisfy { $0 == first }
    }

    private static func listItem(in line: String) -> (ordered: Bool, item: ListItem)? {
        let indentation = line.prefix { $0 == " " || $0 == "\t" }.count
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        let depth = indentation / 2

        for marker in ["- ", "* ", "+ "] where trimmed.hasPrefix(marker) {
            return (
                false,
                ListItem(marker: "•", text: String(trimmed.dropFirst(marker.count)), depth: depth)
            )
        }

        guard let separator = trimmed.range(of: ". ") else { return nil }
        let number = String(trimmed[..<separator.lowerBound])
        guard !number.isEmpty, number.allSatisfy(\.isNumber) else { return nil }
        return (
            true,
            ListItem(marker: "\(number).", text: String(trimmed[separator.upperBound...]), depth: depth)
        )
    }
}

struct ReadableMarkdownContent: View {
    let source: String

    private var document: ReadableMarkdownDocument {
        ReadableMarkdownDocument(source: source)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            ForEach(Array(document.blocks.enumerated()), id: \.offset) { _, block in
                blockView(block)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder
    private func blockView(_ block: ReadableMarkdownDocument.Block) -> some View {
        switch block {
        case let .heading(level, text):
            MarkdownContent(source: text, font: headingFont(level))
                .padding(.top, level == 1 ? 3 : 1)
        case let .paragraph(text):
            MarkdownContent(source: text, font: .body)
        case let .list(_, items):
            VStack(alignment: .leading, spacing: 6) {
                ForEach(Array(items.enumerated()), id: \.offset) { _, item in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text(item.marker)
                            .font(.body.monospacedDigit())
                            .foregroundStyle(.secondary)
                            .frame(minWidth: 14, alignment: .trailing)
                        MarkdownContent(source: item.text, font: .body)
                    }
                    .padding(.leading, CGFloat(item.depth) * 16)
                }
            }
        case let .quote(text):
            HStack(alignment: .top, spacing: 10) {
                RoundedRectangle(cornerRadius: 2)
                    .fill(Color.accentColor.opacity(0.65))
                    .frame(width: 3)
                // Quotes from provider CLIs often contain their own headings
                // and lists. Rendering the whole quote as one AttributedString
                // flattens those blocks and can glue adjacent lines together.
                ReadableMarkdownContent(source: text)
                    .foregroundStyle(.secondary)
            }
            .padding(.vertical, 3)
        case let .code(language, source):
            ReadableCodeBlock(language: language, source: source)
        case .rule:
            Divider()
                .padding(.vertical, 2)
        }
    }

    private func headingFont(_ level: Int) -> Font {
        switch level {
        case 1: .title3.weight(.semibold)
        case 2: .headline
        default: .subheadline.weight(.semibold)
        }
    }
}

private struct ReadableCodeBlock: View {
    let language: String?
    let source: String

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 8) {
                Text(language?.uppercased() ?? "CODE")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.secondary)
                Spacer()
                Button {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(source, forType: .string)
                } label: {
                    Label("Copy", systemImage: "doc.on.doc")
                        .labelStyle(.titleAndIcon)
                }
                .buttonStyle(.plain)
                .font(.caption)
            }
            .padding(.horizontal, 10)
            .frame(height: 30)
            .background(Color.primary.opacity(0.04))

            ScrollView(.horizontal) {
                Text(source)
                    .font(.system(.callout, design: .monospaced))
                    .textSelection(.enabled)
                    .fixedSize(horizontal: true, vertical: true)
                    .padding(10)
            }
        }
        .background(Color.primary.opacity(0.055), in: RoundedRectangle(cornerRadius: 7))
        .overlay {
            RoundedRectangle(cornerRadius: 7)
                .stroke(Color.primary.opacity(0.09), lineWidth: 1)
        }
    }
}
