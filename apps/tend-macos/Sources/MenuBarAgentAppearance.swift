import AppKit
import SwiftUI

/// Identity color persists across state changes. Status has its own symbol
/// and text, so the face never changes identity when a turn finishes.
struct TendMenuBarAgentAppearance: Equatable {
    let paletteIndex: Int
    let symbol: String
    let provider: String

    init(identity: String, kind: String) {
        // Swift's Hasher is process-randomized; use a deterministic byte hash.
        let hash = identity.utf8.reduce(UInt64(14695981039346656037)) { ($0 ^ UInt64($1)) &* 1099511628211 }
        paletteIndex = Int(hash % UInt64(Self.palette.count))
        let program = TendAgentProgram(configValue: kind)
        switch program {
        case .claude: symbol = TendGlyph.claude; provider = "Claude"
        case .codex: symbol = TendGlyph.codex; provider = "Codex"
        case .gemini: symbol = "sparkles"; provider = "Gemini"
        case .agy: symbol = "arrow.up"; provider = "Antigravity"
        case .opencode: symbol = "terminal"; provider = "OpenCode"
        case nil: symbol = "terminal"; provider = kind.replacingOccurrences(of: "_", with: " ")
        }
    }

    init(identity: String, symbol: String, provider: String) {
        paletteIndex = Self(identity: identity, kind: "").paletteIndex
        self.symbol = symbol
        self.provider = provider
    }

    /// Dark enough for white symbols and 11pt capsule text in either menu-bar
    /// appearance; the gradient adds depth without a bright white highlight.
    private static let palette: [(UInt32, UInt32)] = [
        (0x5F63DB, 0x3E419B), (0x2875AB, 0x18577F),
        (0x257E80, 0x17595B), (0x9156BB, 0x643782),
        (0xB54F78, 0x813351), (0xAA5C2B, 0x81451E),
        (0x477F48, 0x2C582E), (0x596FA0, 0x394A73),
    ]
    private static func color(_ rgb: UInt32) -> NSColor {
        NSColor(srgbRed: CGFloat((rgb >> 16) & 255) / 255, green: CGFloat((rgb >> 8) & 255) / 255, blue: CGFloat(rgb & 255) / 255, alpha: 1)
    }
    /// Status text needs darker ink on light glass, lighter ink on dark
    /// glass. Keep the familiar semantic hue without the low contrast of a
    /// saturated system green/orange at the menu's small text sizes.
    static func statusColor(_ status: TendAgentStatus, finished: Bool) -> Color {
        let colors: (UInt32, UInt32)
        switch status {
        case _ where finished: colors = (0x21643B, 0x8AE0AB)
        case .error: colors = (0xAA2739, 0xFF9EA8)
        case .needsInput: colors = (0x83500B, 0xFFD18C)
        case .limited: colors = (0x68408F, 0xD1ACF5)
        case .working: colors = (0x245F99, 0x9BCAFF)
        case .done: colors = (0x21643B, 0x8AE0AB)
        default: return .secondary
        }
        return Color(nsColor: NSColor(name: nil) { appearance in
            Self.color(appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua ? colors.1 : colors.0)
        })
    }
    var top: NSColor { Self.color(Self.palette[paletteIndex].0) }
    var bottom: NSColor { Self.color(Self.palette[paletteIndex].1) }
    var gradient: LinearGradient {
        LinearGradient(colors: [Color(nsColor: top), Color(nsColor: bottom)], startPoint: .topLeading, endPoint: .bottomTrailing)
    }
}

/// Shared between the menu-bar image, its list, hover and detail card.
struct TendMenuBarAvatar: View {
    let item: TendMenuBarAgent
    var size: CGFloat = 28
    var showsStatus = true

    var body: some View {
        TendGlyphView(value: item.appearance.symbol, size: size * 0.45)
            .frame(width: size, height: size)
            .background(item.appearance.gradient, in: Circle())
            .overlay(Circle().strokeBorder(.white.opacity(0.18), lineWidth: 0.5))
            .overlay(alignment: .bottomTrailing) {
                if showsStatus {
                    Image(systemName: item.menuStatusSymbol)
                        .font(.system(size: max(9, size * 0.34), weight: .bold))
                        .foregroundStyle(item.menuStatusColor)
                        .padding(1.5)
                        .background(.regularMaterial, in: Circle())
                        .offset(x: 2, y: 2)
                }
            }
            .accessibilityHidden(true)
    }
}

struct TendMenuBarPeek: View {
    let item: TendMenuBarAgent
    var body: some View {
        HStack(spacing: 10) {
            TendMenuBarAvatar(item: item, size: 26)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 7) {
                    Text(verbatim: item.name).font(TendType.detail.weight(.semibold))
                    Text(verbatim: item.menuStatusLabel).foregroundStyle(item.menuStatusColor).font(TendType.meta)
                }
                Text(verbatim: item.location).font(TendType.meta).foregroundStyle(.secondary).truncationMode(.middle)
            }
            .lineLimit(1)
        }
        .padding(.horizontal, 12).padding(.vertical, 10)
        .frame(maxWidth: 340, alignment: .leading)
        .fixedSize()
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
    }
}
