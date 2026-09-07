import Foundation
import SwiftUI

/// Rebuilds a fully parsed Markdown `AttributedString` so that SwiftUI `Text`
/// still shows block structure.
///
/// `AttributedString(markdown:)` with `.full` syntax records paragraphs,
/// headings, lists, tables, and code blocks only as presentation intents.
/// `Text` ignores those intents and renders every block glued to the previous
/// one, which turns an agent's multi-paragraph response into a single run of
/// words. This keeps the inline styling (emphasis, code, links) that `Text`
/// does understand and reinserts the block boundaries as literal characters,
/// so the result stays a single `Text` that can still honor `lineLimit`.
enum MuxaMarkdownText {
    static func attributedString(markdown source: String) -> AttributedString {
        let options = AttributedString.MarkdownParsingOptions(
            interpretedSyntax: .full,
            failurePolicy: .returnPartiallyParsedIfPossible
        )
        guard let parsed = try? AttributedString(markdown: source, options: options) else {
            return AttributedString(source)
        }
        return flattenBlocks(parsed)
    }

    /// The plain characters `Text` will show for `source`, with block
    /// boundaries restored. Used by tests and by compact previews.
    static func plainText(markdown source: String) -> String {
        String(attributedString(markdown: source).characters)
    }

    private struct BlockPosition: Equatable {
        var blockID: Int?
        var rowID: Int?
    }

    static func flattenBlocks(_ parsed: AttributedString) -> AttributedString {
        var result = AttributedString()
        var previous = BlockPosition()
        var hasEmittedBlock = false

        for run in parsed.runs {
            var fragment = AttributedString(parsed[run.range])
            fragment.presentationIntent = nil
            // Components are ordered innermost first.
            let components = run.presentationIntent?.components ?? []

            var position = BlockPosition()
            var isHeader = false
            var isCode = false
            var isRule = false
            var listDepth = 0
            var listPrefix: String?
            for (index, component) in components.enumerated() {
                switch component.kind {
                case .paragraph, .codeBlock, .header, .thematicBreak, .tableCell:
                    if position.blockID == nil { position.blockID = component.identity }
                case .tableRow, .tableHeaderRow:
                    position.rowID = component.identity
                case .orderedList, .unorderedList:
                    listDepth += 1
                case .listItem(let ordinal):
                    if listPrefix == nil {
                        let parent = components.dropFirst(index + 1).first
                        if case .unorderedList? = parent?.kind {
                            listPrefix = "• "
                        } else {
                            listPrefix = "\(ordinal). "
                        }
                    }
                default:
                    break
                }
                switch component.kind {
                case .header, .tableHeaderRow: isHeader = true
                case .codeBlock: isCode = true
                case .thematicBreak: isRule = true
                default: break
                }
            }

            let startsNewBlock = position.blockID != previous.blockID
            if startsNewBlock {
                if hasEmittedBlock {
                    let continuesTableRow = position.rowID != nil && position.rowID == previous.rowID
                    result.append(AttributedString(continuesTableRow ? "  |  " : "\n"))
                }
                if let listPrefix {
                    let indent = String(repeating: "    ", count: max(0, listDepth - 1))
                    result.append(AttributedString(indent + listPrefix))
                }
            }

            if isRule {
                fragment = AttributedString("———")
            } else if isCode {
                var text = String(fragment.characters)
                while text.hasSuffix("\n") { text.removeLast() }
                fragment = AttributedString(text)
                fragment.inlinePresentationIntent = .code
            } else if isHeader {
                fragment.inlinePresentationIntent = .stronglyEmphasized
            }

            result.append(fragment)
            previous = position
            hasEmittedBlock = true
        }
        return result
    }

    /// One-line preview text for list rows: block boundaries collapse to
    /// spaces and Markdown markers never leak through as literal `#`/`**`.
    static func previewText(markdown source: String) -> String {
        plainText(markdown: source)
            .split(whereSeparator: \.isWhitespace)
            .joined(separator: " ")
    }
}
