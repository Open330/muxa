import AppKit
import SwiftUI

// MARK: - The products' own marks

/// A product's official mark, shipped in `Assets.xcassets/AgentLogos`
/// exactly as its owner publishes it. `Resources/AgentLogos-NOTICE.md`
/// gives each file's source. The marks are their owners' trademarks, are
/// not covered by Tend's licence, and only ever name the product next to
/// (or in place of) its name. They are drawn only scaled: never recoloured,
/// redrawn, cropped, or set on a background.
enum TendAgentLogoAsset: String, CaseIterable, Sendable {
    case claude
    /// OpenAI's blossom, which Codex wears too.
    case openai
    case antigravity
    case opencode
    /// The Anthropic symbol, for the Anthropic API engine.
    case anthropic

    /// The asset name. Brands that publish light and dark versions
    /// (Anthropic, OpenCode) switch by appearance in the catalog.
    var assetName: String { "AgentLogos/\(rawValue)" }

    /// OpenAI publishes the blossom in black and in white only. The catalog
    /// holds the black file as a template, which is drawn in exactly black
    /// or exactly white — the two published versions — and never tinted.
    var isMonochrome: Bool { self == .openai }

    /// The published blossom in the published colour for an appearance.
    static func monochromeInk(dark: Bool) -> Color { dark ? .white : .black }

    /// How far the image overflows its square slot, so marks whose files
    /// carry different margins (the blossom and Antigravity sit inside wide
    /// transparent margins) line up at the same optical size. Only scales.
    var scale: CGFloat {
        switch self {
        case .claude: 0.98
        case .openai: 1.37
        case .antigravity: 1.35
        case .opencode: 1
        case .anthropic: 1
        }
    }

    /// The smallest square slot the mark may be drawn in. OpenCode's brand
    /// page sets a 16×20 minimum in 4×5 steps with two strokes (a quarter
    /// of the width each) of clear space on every side: 32×36 in all.
    /// Smaller slots show the product's name instead of the mark.
    var minimumSlot: CGFloat { self == .opencode ? 36 : 0 }

    func fits(_ slot: CGFloat) -> Bool { slot >= minimumSlot }

    /// OpenCode's mark size in a slot: the largest 4k×5k that leaves its
    /// clear space (2k each side), in whole points.
    static func openCodeMark(in slot: CGFloat) -> CGSize {
        let step = floor(slot / 9)
        return CGSize(width: 4 * step, height: 5 * step)
    }

    /// The mark for an agent kind or program in any spelling tend sees
    /// (`claude_code`, `claude`, `agy`, `antigravity`, …), or nil.
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
        case .anthropic: .anthropic
        case .apple: nil
        }
    }
}

// MARK: - One view for an agent's face

/// An agent's or provider's face: the product's logo as published, or an
/// SF Symbol for a kind this build has no logo for. Decorative — placed
/// next to text that names the product, so VoiceOver reads the name.
///
/// A mark with a minimum size (OpenCode) is left out of a slot too small
/// for it; pass `nameFallback` where no text names the product, and the
/// name is shown instead.
struct TendAgentLogo: View {
    let asset: TendAgentLogoAsset?
    let fallbackSymbol: String
    var size: CGFloat = 16
    var nameFallback: String? = nil
    @Environment(\.colorScheme) private var colorScheme

    init(asset: TendAgentLogoAsset?, fallbackSymbol: String, size: CGFloat = 16, nameFallback: String? = nil) {
        self.asset = asset
        self.fallbackSymbol = fallbackSymbol
        self.size = size
        self.nameFallback = nameFallback
    }

    /// The face for an agent kind as the daemon spells it.
    init(kind: String, size: CGFloat = 16, showsNameWhenTooSmall: Bool = false) {
        self.init(
            asset: TendAgentLogoAsset(kind: kind),
            fallbackSymbol: TendAgentMark.known(for: kind)?.symbol ?? "terminal",
            size: size,
            nameFallback: showsNameWhenTooSmall ? TendAgentMark.productName(forKind: kind) : nil
        )
    }

    init(mark: TendAgentMark, size: CGFloat = 16) {
        self.init(asset: mark.logo, fallbackSymbol: mark.symbol, size: size)
    }

    var body: some View {
        if let asset {
            if asset.fits(size) {
                mark(asset)
                    .frame(width: size, height: size)
                    .modifier(TendLogoBaseline(size: size))
                    .accessibilityHidden(true)
            } else if let nameFallback {
                Text(verbatim: nameFallback)
            }
        } else {
            Image(systemName: fallbackSymbol)
                .font(.system(size: size * 0.8, weight: .medium))
                .frame(width: size, height: size)
                .modifier(TendLogoBaseline(size: size))
                .accessibilityHidden(true)
        }
    }

    @ViewBuilder
    private func mark(_ asset: TendAgentLogoAsset) -> some View {
        if asset == .opencode {
            let mark = TendAgentLogoAsset.openCodeMark(in: size)
            Image(asset.assetName)
                .resizable()
                .interpolation(.high)
                .frame(width: mark.width, height: mark.height)
        } else if asset.isMonochrome {
            Image(asset.assetName)
                .renderingMode(.template)
                .resizable()
                .interpolation(.high)
                .aspectRatio(contentMode: .fit)
                .frame(width: size * asset.scale, height: size * asset.scale)
                // Set here so no caller's tint can reach it.
                .foregroundStyle(TendAgentLogoAsset.monochromeInk(dark: colorScheme == .dark))
        } else {
            Image(asset.assetName)
                .renderingMode(.original)
                .resizable()
                .interpolation(.high)
                .aspectRatio(contentMode: .fit)
                .frame(width: size * asset.scale, height: size * asset.scale)
        }
    }
}

/// Sits a square mark on the text's baseline in a baseline-aligned stack:
/// its centre lands on the middle of a cap height of about its own size.
private struct TendLogoBaseline: ViewModifier {
    let size: CGFloat
    func body(content: Content) -> some View {
        content
            .alignmentGuide(.firstTextBaseline) { $0[VerticalAlignment.center] + size * 0.34 }
            .alignmentGuide(.lastTextBaseline) { $0[VerticalAlignment.center] + size * 0.34 }
    }
}

/// The logos of the agent kinds behind a unit of work, for a row that
/// does not name them in text: a few marks, spoken as names. A mark too
/// small to draw (OpenCode) is shown as its name.
struct TendAgentLogoStack: View {
    let kinds: [String]
    var size: CGFloat = 12

    private var shown: [String] { Array(kinds.prefix(3)) }

    var body: some View {
        HStack(spacing: 3) {
            ForEach(shown, id: \.self) { kind in
                TendAgentLogo(kind: kind, size: size, showsNameWhenTooSmall: true)
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
/// whose logo is drawn sits on a neutral capsule — the logo carries the
/// colour — and one without keeps its tint.
struct TendAgentProgramChip: View {
    static let logoSize: CGFloat = 11
    let program: String

    var body: some View {
        let drawsLogo = TendAgentLogoAsset(kind: program)?.fits(Self.logoSize) == true
        let tint = drawsLogo ? Color.primary : agentProgramTint(program)
        HStack(spacing: 3) {
            if drawsLogo {
                TendAgentLogo(kind: program, size: Self.logoSize)
            }
            Text(program)
        }
        .font(TendType.meta.weight(.medium))
        .foregroundStyle(tint)
        .padding(.horizontal, 5)
        .padding(.vertical, 1)
        .background((drawsLogo ? Color.secondary : tint).opacity(0.12), in: Capsule())
    }
}

// MARK: - AppKit

extension TendAgentLogoAsset {
    /// Draws the published mark centred in `rect` at the optical size the
    /// SwiftUI view uses, for the menu-bar image. `dark` picks the variant
    /// the brand publishes for a dark ground. Returns false when the slot
    /// is below the mark's minimum size and nothing was drawn.
    @discardableResult
    func draw(in rect: NSRect, dark: Bool, alpha: CGFloat = 1) -> Bool {
        let slot = min(rect.width, rect.height)
        guard fits(slot), let image = NSImage(named: assetName) else { return false }
        let size: NSSize
        if self == .opencode {
            let mark = Self.openCodeMark(in: slot)
            size = NSSize(width: mark.width, height: mark.height)
        } else {
            let side = slot * scale
            let fitted = image.size.width > 0 && image.size.height > 0
                ? min(side / image.size.width, side / image.size.height) : 1
            size = NSSize(width: image.size.width * fitted, height: image.size.height * fitted)
        }
        let bounds = NSRect(x: rect.midX - size.width / 2, y: rect.midY - size.height / 2, width: size.width, height: size.height)
        let appearance = NSAppearance(named: dark ? .darkAqua : .aqua) ?? NSAppearance.currentDrawing()
        appearance.performAsCurrentDrawingAppearance {
            NSGraphicsContext.saveGraphicsState()
            if isMonochrome {
                NSGraphicsContext.current?.cgContext.beginTransparencyLayer(auxiliaryInfo: nil)
                image.draw(in: bounds, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
                (dark ? NSColor.white : NSColor.black).withAlphaComponent(alpha).setFill()
                bounds.fill(using: .sourceIn)
                NSGraphicsContext.current?.cgContext.endTransparencyLayer()
            } else {
                image.draw(in: bounds, from: .zero, operation: .sourceOver, fraction: alpha, respectFlipped: true, hints: nil)
            }
            NSGraphicsContext.restoreGraphicsState()
        }
        return true
    }
}
