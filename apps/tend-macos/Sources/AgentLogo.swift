import AppKit
import SwiftUI

// MARK: - The products' own marks

/// A product's official mark, shipped in `Assets.xcassets/AgentLogos`.
/// `Resources/AgentLogos-NOTICE.md` says where each file came from. The
/// marks are their owners' trademarks and only ever name the product next
/// to (or in place of) its name; they are never redrawn, only resized, and
/// the `-mono` variants are the same outlines filled with one colour.
enum TendAgentLogoAsset: String, CaseIterable, Sendable {
    case claude
    /// OpenAI's blossom, which Codex wears too.
    case openai
    case gemini
    case antigravity
    case opencode
    /// The Anthropic symbol, for the Anthropic API engine.
    case anthropic

    /// Brand colours, adapting to light and dark where the brand ships both.
    var colorName: String { "AgentLogos/\(rawValue)" }
    /// One colour, rendered as a template: for the menu bar's white-on-colour
    /// faces and anywhere the brand colour would clash.
    var monoName: String { "AgentLogos/\(rawValue)-mono" }

    /// How much of a square frame the mark fills. Every file is cropped to
    /// its mark; this evens out optical weight so a solid block (opencode)
    /// does not shout next to a thin starburst (Claude).
    var scale: CGFloat {
        switch self {
        case .claude: 0.98
        case .openai: 0.92
        case .gemini: 1.04
        case .antigravity: 1.0
        case .opencode: 0.86
        case .anthropic: 1.0
        }
    }

    /// The mark for an agent kind or program in any spelling tend sees
    /// (`claude_code`, `claude`, `gemini_cli`, `agy`, …), or nil.
    init?(kind: String) {
        guard let program = TendAgentProgram(configValue: kind) else { return nil }
        self = program.logo
    }
}

extension TendAgentProgram {
    var logo: TendAgentLogoAsset {
        switch self {
        case .claude: .claude
        case .codex: .openai
        case .gemini: .gemini
        case .agy: .antigravity
        case .opencode: .opencode
        }
    }
}

extension AskProviderEngine {
    /// The vendor's mark; nil for Apple, whose own SF Symbol is its logo.
    var logo: TendAgentLogoAsset? {
        switch self {
        case .claude: .claude
        case .codex, .openai: .openai
        case .gemini: .gemini
        case .anthropic: .anthropic
        case .apple: nil
        }
    }
}

// MARK: - One view for an agent's face

/// An agent's or provider's face: the product's logo, or an SF Symbol for
/// a kind this build has no logo for. Decorative — always placed next to
/// text that names the product, so VoiceOver reads the name, not the image.
struct TendAgentLogo: View {
    enum Style {
        /// The brand's colours. Callers add no tint behind it.
        case color
        /// One colour from the foreground style, for coloured faces.
        case mono
    }

    let asset: TendAgentLogoAsset?
    let fallbackSymbol: String
    var size: CGFloat = 16
    var style: Style = .color

    init(asset: TendAgentLogoAsset?, fallbackSymbol: String, size: CGFloat = 16, style: Style = .color) {
        self.asset = asset
        self.fallbackSymbol = fallbackSymbol
        self.size = size
        self.style = style
    }

    /// The face for an agent kind as the daemon spells it.
    init(kind: String, size: CGFloat = 16, style: Style = .color) {
        self.init(
            asset: TendAgentLogoAsset(kind: kind),
            fallbackSymbol: TendAgentMark.known(for: kind)?.symbol ?? "terminal",
            size: size,
            style: style
        )
    }

    init(mark: TendAgentMark, size: CGFloat = 16, style: Style = .color) {
        self.init(asset: mark.logo, fallbackSymbol: mark.symbol, size: size, style: style)
    }

    var body: some View {
        Group {
            if let asset {
                Image(style == .mono ? asset.monoName : asset.colorName)
                    .resizable()
                    .interpolation(.high)
                    .aspectRatio(contentMode: .fit)
                    .frame(width: size * asset.scale, height: size * asset.scale)
            } else {
                Image(systemName: fallbackSymbol)
                    .font(.system(size: size * 0.8, weight: .medium))
            }
        }
        .frame(width: size, height: size)
        // Sit on the text's baseline in a baseline-aligned stack: the mark's
        // centre lands on the middle of a cap height of about its own size.
        .alignmentGuide(.firstTextBaseline) { $0[VerticalAlignment.center] + size * 0.34 }
        .alignmentGuide(.lastTextBaseline) { $0[VerticalAlignment.center] + size * 0.34 }
        .accessibilityHidden(true)
    }
}

/// The logos of the agent kinds behind a unit of work, for a row that
/// does not name them in text: a few overlapping marks, spoken as names.
struct TendAgentLogoStack: View {
    let kinds: [String]
    var size: CGFloat = 12

    private var shown: [String] { Array(kinds.prefix(3)) }

    var body: some View {
        HStack(spacing: 2) {
            ForEach(shown, id: \.self) { kind in
                TendAgentLogo(kind: kind, size: size)
            }
        }
        .help(names)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(Text(verbatim: names))
    }

    private var names: String {
        ListFormatter.localizedString(byJoining: kinds.map(TendAgentMark.productName(forKind:)))
    }
}

/// A pipeline agent's program as a small capsule: logo and name. A program
/// with a logo sits on a neutral capsule — the logo carries the colour — and
/// one without keeps its tint.
struct TendAgentProgramChip: View {
    let program: String

    var body: some View {
        let logo = TendAgentLogoAsset(kind: program)
        let tint = logo == nil ? agentProgramTint(program) : Color.primary
        HStack(spacing: 3) {
            if logo != nil {
                TendAgentLogo(kind: program, size: 11)
            }
            Text(program)
        }
        .font(TendType.meta.weight(.medium))
        .foregroundStyle(tint)
        .padding(.horizontal, 5)
        .padding(.vertical, 1)
        .background((logo == nil ? tint : Color.secondary).opacity(0.12), in: Capsule())
    }
}

// MARK: - AppKit

extension TendAgentLogoAsset {
    /// The mono mark filled with `color`, centred in `rect` at the same
    /// optical size the SwiftUI view uses.
    func drawMono(in rect: NSRect, color: NSColor) {
        guard let image = NSImage(named: monoName) else { return }
        let side = min(rect.width, rect.height) * scale
        let fitted = image.size.width > 0 && image.size.height > 0
            ? min(side / image.size.width, side / image.size.height) : 1
        let size = NSSize(width: image.size.width * fitted, height: image.size.height * fitted)
        let bounds = NSRect(x: rect.midX - size.width / 2, y: rect.midY - size.height / 2, width: size.width, height: size.height)
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current?.cgContext.beginTransparencyLayer(auxiliaryInfo: nil)
        image.draw(in: bounds, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
        color.setFill()
        bounds.fill(using: .sourceAtop)
        NSGraphicsContext.current?.cgContext.endTransparencyLayer()
        NSGraphicsContext.restoreGraphicsState()
    }
}
