import AppKit
import SwiftUI

/// What a menu-bar face shows: an SF Symbol name, a product's logo
/// (`tend.claude`, `tend.codex`, `tend.antigravity`, …), or a short text
/// (`text:🚀`, `text:MB`) a person chose for a fleet host. Drawn the same
/// way in the menu-bar image and in the SwiftUI lists. A logo is drawn as
/// its owner publishes it, on no background (`drawsOwnFace`); every other
/// glyph is white on the face's identity colour.
enum TendGlyph {
    static let claude = "tend.claude"
    static let codex = "tend.codex"
    static let antigravity = "tend.antigravity"
    static let opencode = "tend.opencode"
    static let textPrefix = "text:"

    enum Kind: Equatable {
        case symbol(String)
        case text(String)
        case logo(TendAgentLogoAsset)
    }

    static func text(_ value: String) -> String { textPrefix + value }

    /// The glyph value for a program's logo.
    static func logo(for program: TendAgentProgram) -> String {
        switch program {
        case .claude: claude
        case .codex: codex
        case .agy: antigravity
        case .opencode: opencode
        }
    }

    static func kind(_ value: String) -> Kind {
        switch value {
        case claude: return .logo(.claude)
        case codex: return .logo(.openai)
        case antigravity: return .logo(.antigravity)
        case opencode: return .logo(.opencode)
        default:
            if value.hasPrefix(textPrefix) { return .text(String(value.dropFirst(textPrefix.count))) }
            return .symbol(value)
        }
    }

    /// At most two characters (an emoji counts as one), so it fits a face.
    static func cleanedText(_ text: String) -> String {
        String(text.trimmingCharacters(in: .whitespacesAndNewlines).prefix(2))
    }

    /// True when the glyph is a logo that fits `slot`: it is then the whole
    /// face, with no identity-colour disc behind it. A logo too small for
    /// its brand's minimum (OpenCode in the menu bar) is not drawn; the face
    /// keeps its disc and shows a terminal symbol instead.
    static func drawsOwnFace(_ value: String, slot: CGFloat) -> Bool {
        if case .logo(let logo) = kind(value) { return logo.fits(slot) }
        return false
    }

    /// What a logo that does not fit its slot shows instead.
    static let tooSmallSymbol = "terminal"

    /// Draws the glyph centred in `rect` in the current AppKit context.
    /// `color` inks symbols and text; a logo keeps its own colours and only
    /// takes the alpha. `dark` picks a logo's variant for a dark ground.
    static func draw(_ value: String, in rect: NSRect, color: NSColor, dark: Bool = false) {
        switch kind(value) {
        case .logo(let logo):
            if !logo.draw(in: rect, dark: dark, alpha: color.alphaComponent) {
                draw(tooSmallSymbol, in: rect, color: color, dark: dark)
            }
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
struct TendGlyphView: View {
    let value: String
    var size: CGFloat
    var color: Color = .white

    var body: some View {
        switch TendGlyph.kind(value) {
        case .logo(let logo):
            if logo.fits(size) {
                TendAgentLogo(asset: logo, fallbackSymbol: TendGlyph.tooSmallSymbol, size: size)
            } else {
                Image(systemName: TendGlyph.tooSmallSymbol)
                    .font(.system(size: size, weight: .semibold))
                    .foregroundStyle(color)
            }
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

/// A round menu-bar face: a logo on its own, anything else white on the
/// identity gradient. Shared by the menu-bar lists, the chips, host badges
/// and Settings, so every face follows the same rule.
struct TendGlyphFace: View {
    let value: String
    let gradient: LinearGradient
    var size: CGFloat
    /// The symbol or text size on a disc.
    var glyphSize: CGFloat

    /// A logo fills most of the face's circle, as the symbol and disc did.
    private var logoSize: CGFloat { (size * 0.78).rounded() }

    var body: some View {
        if TendGlyph.drawsOwnFace(value, slot: logoSize) {
            TendGlyphView(value: value, size: logoSize)
                .frame(width: size, height: size)
        } else {
            TendGlyphView(value: value, size: glyphSize)
                .frame(width: size, height: size)
                .background(gradient, in: Circle())
        }
    }
}

/// The icon each fleet host shows in the menu bar when activity is grouped
/// by host: this Mac a laptop, other hosts a server, unless a person chose
/// a symbol or a short text in Settings › Menu Bar.
enum TendHostIcons {
    static let defaultsKey = "tend.menuBar.hostIcons"

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
