import Foundation

/// A small, escaped Markdown renderer for artifact previews. Raw HTML is shown
/// as text. Only the document viewer's explicit link handler can open a URL.
enum ArtifactMarkdown {
    static func escaped(_ value: String) -> String {
        value.replacingOccurrences(of: "&", with: "&amp;")
            .replacingOccurrences(of: "<", with: "&lt;")
            .replacingOccurrences(of: ">", with: "&gt;")
            .replacingOccurrences(of: "\"", with: "&quot;")
            .replacingOccurrences(of: "'", with: "&#39;")
    }
    static func inline(_ value: String) -> String {
        let pattern = #"`([^`]+)`|!\[([^\]]*)\]\(([^\s)]+)\)|\[([^\]]+)\]\(([^\s)]+)\)|\*\*([^*]+)\*\*|\*([^*]+)\*"#
        guard let expression = try? NSRegularExpression(pattern: pattern) else { return escaped(value) }
        let string = value as NSString
        var result = "", offset = 0
        for match in expression.matches(in: value, range: NSRange(location: 0, length: string.length)) {
            result += escaped(string.substring(with: NSRange(location: offset, length: match.range.location - offset)))
            func part(_ index: Int) -> String? {
                let range = match.range(at: index)
                return range.location == NSNotFound ? nil : string.substring(with: range)
            }
            if let code = part(1) { result += "<code>\(escaped(code))</code>" }
            else if let label = part(2), let url = part(3) {
                result += "<a href=\"\(escaped(url))\"><img alt=\"\(escaped(label))\" src=\"\(escaped(url))\"></a>"
            } else if let label = part(4), let url = part(5) {
                result += "<a href=\"\(escaped(url))\">\(escaped(label))</a>"
            } else if let bold = part(6) { result += "<strong>\(escaped(bold))</strong>" }
            else if let italic = part(7) { result += "<em>\(escaped(italic))</em>" }
            offset = match.range.location + match.range.length
        }
        result += escaped(string.substring(from: offset))
        return result
    }
    static func html(_ markdown: String) -> String {
        let lines = markdown.components(separatedBy: .newlines)
        var output = "", paragraph: [String] = [], code: [String] = []
        var fence: String?, list: String?
        func flushParagraph() {
            if !paragraph.isEmpty { output += "<p>\(paragraph.map(inline).joined(separator: "<br>"))</p>"; paragraph = [] }
        }
        func closeList() { if let tag = list { output += "</\(tag)>" }; list = nil }
        var index = 0
        while index < lines.count {
            let line = lines[index], trimmed = line.trimmingCharacters(in: .whitespaces)
            index += 1
            if let marker = fence {
                if trimmed.hasPrefix(marker) {
                    output += "<pre><code>\(escaped(code.joined(separator: "\n")))</code></pre>"
                    code = []; fence = nil
                } else { code.append(line) }
                continue
            }
            if trimmed.hasPrefix("```") || trimmed.hasPrefix("~~~") {
                flushParagraph(); closeList(); fence = String(trimmed.prefix(3)); continue
            }
            if trimmed.isEmpty { flushParagraph(); closeList(); continue }
            if line.contains("|"), index < lines.count, isTableSeparator(lines[index]) {
                flushParagraph(); closeList()
                output += "<table><thead><tr>" + cells(line).map { "<th>\(inline($0))</th>" }.joined() + "</tr></thead><tbody>"
                index += 1
                while index < lines.count, lines[index].contains("|"), !lines[index].trimmingCharacters(in: .whitespaces).isEmpty {
                    output += "<tr>" + cells(lines[index]).map { "<td>\(inline($0))</td>" }.joined() + "</tr>"
                    index += 1
                }
                output += "</tbody></table>"; continue
            }
            let level = trimmed.prefix(while: { $0 == "#" }).count
            if (1...6).contains(level), trimmed.dropFirst(level).first == " " {
                flushParagraph(); closeList()
                output += "<h\(level)>\(inline(String(trimmed.dropFirst(level + 1))))</h\(level)>"; continue
            }
            if ["---", "***", "___"].contains(trimmed) { flushParagraph(); closeList(); output += "<hr>"; continue }
            if trimmed.hasPrefix("> ") { flushParagraph(); closeList(); output += "<blockquote>\(inline(String(trimmed.dropFirst(2))))</blockquote>"; continue }
            let ordered = trimmed.range(of: #"^\d+\.\s"#, options: .regularExpression)
            let unordered = trimmed.hasPrefix("- ") || trimmed.hasPrefix("* ") || trimmed.hasPrefix("+ ")
            if unordered || ordered != nil {
                flushParagraph()
                let tag = unordered ? "ul" : "ol"
                if list != tag { closeList(); output += "<\(tag)>"; list = tag }
                let value = unordered ? String(trimmed.dropFirst(2)) : String(trimmed[ordered!.upperBound...])
                output += "<li>\(inline(value))</li>"; continue
            }
            closeList(); paragraph.append(line)
        }
        flushParagraph(); closeList()
        if fence != nil { output += "<pre><code>\(escaped(code.joined(separator: "\n")))</code></pre>" }
        return """
        <!doctype html><html><head><meta charset="utf-8"><meta name="color-scheme" content="light dark">
        <style>
        :root { color-scheme: light dark; } body { font: 14px/1.7 -apple-system, sans-serif; margin: 28px auto; padding: 0 28px 48px; max-width: 900px; overflow-wrap: anywhere; }
        h1,h2,h3 { line-height: 1.3; margin-top: 1.5em; } h1,h2 { border-bottom: 1px solid #8884; padding-bottom: .3em; }
        a { color: #3986e8; } pre,code { font-family: ui-monospace, Menlo, monospace; font-size: .9em; background: #8882; border-radius: 4px; }
        code { padding: 2px 4px; } pre { padding: 16px; overflow: auto; } pre code { background: none; padding: 0; }
        img { max-width: 100%; } table { border-collapse: collapse; width: 100%; } td,th { border: 1px solid #8885; padding: 7px 12px; text-align: left; }
        th { background: #8882; } blockquote { border-left: 3px solid #8888; padding-left: 16px; margin-left: 0; opacity: .8; } hr { border: 0; border-top: 1px solid #8884; }
        </style></head><body>\(output)</body></html>
        """
    }
    private static func cells(_ line: String) -> [String] {
        var value = line.trimmingCharacters(in: .whitespaces)
        if value.hasPrefix("|") { value.removeFirst() }
        if value.hasSuffix("|") { value.removeLast() }
        return value.components(separatedBy: "|").map { $0.trimmingCharacters(in: .whitespaces) }
    }
    private static func isTableSeparator(_ line: String) -> Bool {
        let values = cells(line)
        return !values.isEmpty && values.allSatisfy { $0.range(of: #"^:?-{3,}:?$"#, options: .regularExpression) != nil }
    }
}
