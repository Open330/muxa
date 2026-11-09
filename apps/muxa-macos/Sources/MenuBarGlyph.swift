import AppKit
import SwiftUI

/// What a menu-bar face shows: an SF Symbol name, one of muxa's own marks
/// (`muxa.claude`, `muxa.codex`), or a short text (`text:🚀`, `text:MB`)
/// a person chose for a fleet host. Drawn the same way in the menu-bar
/// image and in the SwiftUI lists.
enum MuxaGlyph {
    static let claude = "muxa.claude"
    static let codex = "muxa.codex"
    static let textPrefix = "text:"

    enum Kind: Equatable {
        case symbol(String)
        case text(String)
        case claude
        case codex
    }

    static func text(_ value: String) -> String { textPrefix + value }

    static func kind(_ value: String) -> Kind {
        switch value {
        case claude: return .claude
        case codex: return .codex
        default:
            if value.hasPrefix(textPrefix) { return .text(String(value.dropFirst(textPrefix.count))) }
            return .symbol(value)
        }
    }

    /// At most two characters (an emoji counts as one), so it fits a face.
    static func cleanedText(_ text: String) -> String {
        String(text.trimmingCharacters(in: .whitespacesAndNewlines).prefix(2))
    }

    /// Claude's spark: rays of uneven length around a centre, in a unit
    /// square. Not the trademark artwork; a mark that reads as Claude.
    static func sparkPath(in rect: CGRect) -> CGPath {
        let path = CGMutablePath()
        let lengths: [CGFloat] = [1.0, 0.72, 0.9, 0.66, 0.96, 0.7, 0.88, 0.64, 1.0, 0.74, 0.86, 0.68]
        let center = CGPoint(x: rect.midX, y: rect.midY)
        let radius = min(rect.width, rect.height) / 2
        let width = radius * 0.22
        for (index, length) in lengths.enumerated() {
            let angle = CGFloat(index) / CGFloat(lengths.count) * 2 * .pi
            let ray = CGRect(x: -width / 2, y: radius * 0.08, width: width, height: radius * length * 0.92)
            let transform = CGAffineTransform(translationX: center.x, y: center.y).rotated(by: angle)
            path.addPath(CGPath(roundedRect: ray, cornerWidth: width / 2, cornerHeight: width / 2, transform: nil), transform: transform)
        }
        return path
    }

    /// A terminal prompt, `>_`, as strokes in a unit square: a chevron and
    /// a short underline, centred so it never spills off a small face.
    static func promptPath(in rect: CGRect) -> CGPath {
        let side = min(rect.width, rect.height)
        let origin = CGPoint(x: rect.midX - side / 2, y: rect.midY - side / 2)
        func point(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: origin.x + x * side, y: origin.y + y * side) }
        let path = CGMutablePath()
        path.move(to: point(0.12, 0.22))
        path.addLine(to: point(0.46, 0.5))
        path.addLine(to: point(0.12, 0.78))
        path.move(to: point(0.54, 0.22))
        path.addLine(to: point(0.9, 0.22))
        return path.copy(strokingWithWidth: side * 0.15, lineCap: .round, lineJoin: .round, miterLimit: 4)
    }

    /// Draws the glyph centred in `rect` in the current AppKit context.
    static func draw(_ value: String, in rect: NSRect, color: NSColor) {
        switch kind(value) {
        case .claude:
            guard let context = NSGraphicsContext.current?.cgContext else { return }
            let side = min(rect.width, rect.height)
            let square = CGRect(x: rect.midX - side / 2, y: rect.midY - side / 2, width: side, height: side)
            context.saveGState()
            context.addPath(sparkPath(in: square))
            context.setFillColor(color.cgColor)
            context.fillPath()
            context.restoreGState()
        case .codex:
            guard let context = NSGraphicsContext.current?.cgContext else { return }
            let side = min(rect.width, rect.height) * 0.9
            let square = CGRect(x: rect.midX - side / 2, y: rect.midY - side / 2, width: side, height: side)
            context.saveGState()
            context.addPath(promptPath(in: square))
            context.setFillColor(color.cgColor)
            context.fillPath()
            context.restoreGState()
        case .text(let text):
            var size = rect.height * 0.95
            var attributes: [NSAttributedString.Key: Any] = [:]
            var measured = NSSize.zero
            // Shrink until it fits the face.
            repeat {
                attributes = [.font: NSFont.systemFont(ofSize: size, weight: .bold), .foregroundColor: color]
                measured = (text as NSString).size(withAttributes: attributes)
                size -= 0.5
            } while measured.width > rect.width && size > 5
            (text as NSString).draw(
                at: NSPoint(x: rect.midX - measured.width / 2, y: rect.midY - measured.height / 2),
                withAttributes: attributes
            )
        case .symbol(let name):
            guard let source = NSImage(systemSymbolName: name, accessibilityDescription: nil)
                    ?? NSImage(systemSymbolName: "questionmark", accessibilityDescription: nil) else { return }
            let configured = source.withSymbolConfiguration(.init(pointSize: 11, weight: .semibold)) ?? source
            let scale = min(rect.width / configured.size.width, rect.height / configured.size.height)
            let size = NSSize(width: configured.size.width * scale, height: configured.size.height * scale)
            let tinted = NSImage(size: size)
            tinted.lockFocus()
            let bounds = NSRect(origin: .zero, size: size)
            configured.draw(in: bounds, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
            color.setFill()
            bounds.fill(using: .sourceAtop)
            tinted.unlockFocus()
            tinted.draw(in: NSRect(x: rect.midX - size.width / 2, y: rect.midY - size.height / 2, width: size.width, height: size.height))
        }
    }
}

/// The glyph in SwiftUI, sized to a face.
struct MuxaGlyphView: View {
    let value: String
    var size: CGFloat
    var color: Color = .white

    var body: some View {
        switch MuxaGlyph.kind(value) {
        case .claude:
            MuxaSparkShape()
                .fill(color)
                .frame(width: size, height: size)
        case .codex:
            MuxaPromptShape()
                .fill(color)
                .frame(width: size * 0.9, height: size * 0.9)
        case .text(let text):
            Text(verbatim: text)
                .font(.system(size: size * 0.9, weight: .bold))
                .minimumScaleFactor(0.5)
                .lineLimit(1)
                .foregroundStyle(color)
        case .symbol(let name):
            Image(systemName: name)
                .font(.system(size: size, weight: .semibold))
                .foregroundStyle(color)
        }
    }
}

struct MuxaPromptShape: Shape {
    func path(in rect: CGRect) -> Path {
        // CoreGraphics' y grows upward in AppKit drawing; SwiftUI's grows
        // downward, so flip to keep the underline at the bottom.
        Path(MuxaGlyph.promptPath(in: rect)).applying(
            CGAffineTransform(translationX: 0, y: rect.height + 2 * rect.minY).scaledBy(x: 1, y: -1)
        )
    }
}

struct MuxaSparkShape: Shape {
    func path(in rect: CGRect) -> Path { Path(MuxaGlyph.sparkPath(in: rect)) }
}

/// The icon each fleet host shows in the menu bar when activity is grouped
/// by host: this Mac a laptop, other hosts a server, unless a person chose
/// a symbol or a short text in Settings › Menu Bar.
enum MuxaHostIcons {
    static let defaultsKey = "muxa.menuBar.hostIcons"

    /// Symbols offered in Settings, in the order shown.
    static let choices: [String] = [
        "server.rack", "laptopcomputer", "desktopcomputer", "macmini", "macpro.gen3",
        "display", "cpu", "cloud", "house", "building.2", "network", "globe",
        "bolt", "leaf", "flame", "star", "hare", "tortoise", "cube", "shippingbox",
    ]

    static func icon(for alias: String, local: Bool, defaults: UserDefaults = .standard) -> String {
        if let chosen = all(defaults: defaults)[alias], !chosen.isEmpty { return chosen }
        return local ? "laptopcomputer" : "server.rack"
    }

    static func all(defaults: UserDefaults = .standard) -> [String: String] {
        defaults.dictionary(forKey: defaultsKey) as? [String: String] ?? [:]
    }

    static func set(_ icon: String?, for alias: String, defaults: UserDefaults = .standard) {
        var icons = all(defaults: defaults)
        icons[alias] = icon
        defaults.set(icons, forKey: defaultsKey)
    }
}
