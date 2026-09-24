import AppKit
import SwiftUI

/// The workbench's design tokens. The workbench is laid out like a
/// professional editor (activity bar, side bar, editor groups, status bar),
/// and these values follow the density and neutral surfaces of VS Code's
/// "Modern" themes rather than stock AppKit controls, so every region reads as
/// one tool instead of a stack of system panels.
enum MuxaTheme {
    // MARK: Metrics

    /// One Explore/tree row. VS Code lists are 22px; a touch taller keeps the
    /// Korean glyphs from crowding.
    static let rowHeight: CGFloat = 24
    static let treeIndent: CGFloat = 12
    static let sectionHeaderHeight: CGFloat = 32
    static let breadcrumbHeight: CGFloat = 28
    static let statusBarHeight: CGFloat = 22
    static let activityBarWidth: CGFloat = 46
    static let controlRadius: CGFloat = 4

    /// Cards and grouped content inside an editor: small corners and a
    /// hairline edge rather than a floating material slab.
    static let panelRadius: CGFloat = 6

    /// The card fill, following the appearance on its own so views that
    /// don't read `colorScheme` can use it as a plain `ShapeStyle`.
    static let panelFill = Color(nsColor: NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor(srgbRed: 0x25 / 255, green: 0x25 / 255, blue: 0x26 / 255, alpha: 1)
            : NSColor(srgbRed: 0xF8 / 255, green: 0xF8 / 255, blue: 0xF8 / 255, alpha: 1)
    })

    // MARK: Surfaces

    static func activityBar(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? rgb(0x181818) : rgb(0xF8F8F8)
    }

    static func sideBar(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? rgb(0x181818) : rgb(0xF8F8F8)
    }

    static func editor(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? rgb(0x1F1F1F) : rgb(0xFFFFFF)
    }

    /// Tab strip and inactive tabs: the side bar's tone, so the active tab
    /// visibly joins the editor below it.
    static func tabStrip(_ scheme: ColorScheme) -> Color {
        sideBar(scheme)
    }

    /// Panels inside an editor (inspector cards, the live-pane header).
    static func panel(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? rgb(0x181818) : rgb(0xF8F8F8)
    }

    static func border(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? rgb(0x2B2B2B) : rgb(0xE5E5E5)
    }

    static func inputBackground(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? rgb(0x313131) : rgb(0xFFFFFF)
    }

    static func inputBorder(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? rgb(0x3C3C3C) : rgb(0xCECECE)
    }

    static func hover(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? Color.white.opacity(0.07) : Color.black.opacity(0.05)
    }

    static func pressed(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? Color.white.opacity(0.12) : Color.black.opacity(0.09)
    }

    /// The active row in a list that has focus: the accent, muted.
    static func selection(_ scheme: ColorScheme) -> Color {
        Color.accentColor.opacity(scheme == .dark ? 0.30 : 0.16)
    }

    static func segmentTrack(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? Color.white.opacity(0.06) : Color.black.opacity(0.055)
    }

    static func segmentChip(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? rgb(0x3A3A3A) : rgb(0xFFFFFF)
    }

    private static func rgb(_ hex: UInt32) -> Color {
        Color(
            red: Double((hex >> 16) & 0xFF) / 255,
            green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255
        )
    }
}

extension View {
    /// An editor group takes focus to route ⌘-shortcuts, not to be edited;
    /// the system's focus ring around the whole group only adds noise (the
    /// active tab already shows which group has focus).
    @ViewBuilder
    func muxaFocusEffectDisabled() -> some View {
        if #available(macOS 14.0, *) {
            focusEffectDisabled()
        } else {
            self
        }
    }
}

// MARK: - Buttons

/// A square icon button for action bars (side bar titles, tab strip, editor
/// headers): no chrome at rest, a soft fill on hover like VS Code's actions.
struct MuxaIconButtonStyle: ButtonStyle {
    var size: CGFloat = 22

    func makeBody(configuration: Configuration) -> some View {
        MuxaIconButtonBody(configuration: configuration, size: size)
    }
}

private struct MuxaIconButtonBody: View {
    let configuration: ButtonStyle.Configuration
    let size: CGFloat
    @Environment(\.isEnabled) private var isEnabled
    @Environment(\.colorScheme) private var colorScheme
    @State private var hovering = false

    var body: some View {
        configuration.label
            .labelStyle(.iconOnly)
            .font(.system(size: 13))
            .frame(width: size, height: size)
            .contentShape(Rectangle())
            .background(
                RoundedRectangle(cornerRadius: MuxaTheme.controlRadius + 1, style: .continuous)
                    .fill(fill)
            )
            .foregroundStyle(isEnabled ? Color.primary.opacity(0.78) : Color.secondary.opacity(0.45))
            .onHover { hovering = $0 && isEnabled }
    }

    private var fill: Color {
        guard isEnabled else { return .clear }
        if configuration.isPressed { return MuxaTheme.pressed(colorScheme) }
        return hovering ? MuxaTheme.hover(colorScheme) : .clear
    }
}

extension ButtonStyle where Self == MuxaIconButtonStyle {
    static var muxaIcon: MuxaIconButtonStyle { MuxaIconButtonStyle() }
    static func muxaIcon(size: CGFloat) -> MuxaIconButtonStyle { MuxaIconButtonStyle(size: size) }
}

/// The workbench's text buttons, sized by `controlSize` (22pt small, 26pt
/// regular):
/// - `primary`: the accent fill, one per surface, for the action that moves
///   work forward (Send, Start, Click to Type).
/// - `secondary`: a hairline outline that fills on hover — the everyday
///   action (Details, Open in Shell).
/// - `ghost`: no chrome until hovered, for actions inside dense rows.
struct MuxaButtonStyle: ButtonStyle {
    enum Kind { case primary, secondary, ghost }
    var kind: Kind = .secondary

    func makeBody(configuration: Configuration) -> some View {
        MuxaButtonBody(configuration: configuration, kind: kind)
    }
}

private struct MuxaButtonBody: View {
    let configuration: ButtonStyle.Configuration
    let kind: MuxaButtonStyle.Kind
    @Environment(\.isEnabled) private var isEnabled
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.controlSize) private var controlSize
    @State private var hovering = false

    private var compact: Bool { controlSize == .small || controlSize == .mini }

    var body: some View {
        configuration.label
            .labelStyle(MuxaButtonLabelStyle())
            .font(.system(size: compact ? 11 : 12, weight: kind == .primary ? .semibold : .medium))
            .lineLimit(1)
            .padding(.horizontal, compact ? 8 : 10)
            .frame(minHeight: compact ? 22 : 26)
            .background(
                RoundedRectangle(cornerRadius: 5, style: .continuous)
                    .fill(fill)
            )
            .overlay {
                RoundedRectangle(cornerRadius: 5, style: .continuous)
                    .strokeBorder(stroke, lineWidth: 1)
            }
            .foregroundStyle(foreground)
            .opacity(isEnabled ? 1 : 0.45)
            .contentShape(Rectangle())
            .onHover { hovering = $0 && isEnabled }
            .animation(.easeOut(duration: 0.12), value: hovering)
    }

    private var tint: Color {
        configuration.role == .destructive ? .red : .accentColor
    }

    private var fill: Color {
        switch kind {
        case .primary:
            if configuration.isPressed { return tint.opacity(0.78) }
            return hovering ? tint.opacity(0.9) : tint
        case .secondary, .ghost:
            if configuration.isPressed { return MuxaTheme.pressed(colorScheme) }
            return hovering ? MuxaTheme.hover(colorScheme) : .clear
        }
    }

    private var stroke: Color {
        switch kind {
        case .primary: Color.black.opacity(colorScheme == .dark ? 0.25 : 0.08)
        case .secondary: MuxaTheme.inputBorder(colorScheme)
        case .ghost: .clear
        }
    }

    private var foreground: Color {
        switch kind {
        case .primary: .white
        case .secondary, .ghost:
            configuration.role == .destructive ? .red : Color.primary.opacity(0.85)
        }
    }
}

/// Icon and title a little closer than `Label`'s default, the icon a step
/// smaller than the text so it doesn't outweigh it.
struct MuxaButtonLabelStyle: LabelStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: 5) {
            configuration.icon
                .font(.system(size: 11, weight: .medium))
                .imageScale(.small)
            configuration.title
        }
    }
}

extension ButtonStyle where Self == MuxaButtonStyle {
    static var muxaPrimary: MuxaButtonStyle { MuxaButtonStyle(kind: .primary) }
    static var muxaSecondary: MuxaButtonStyle { MuxaButtonStyle(kind: .secondary) }
    static var muxaGhost: MuxaButtonStyle { MuxaButtonStyle(kind: .ghost) }
}

// MARK: - Segmented control

/// A flat segmented control: the selected segment is a raised chip inside a
/// sunken track, in place of AppKit's bezeled `.segmented` picker.
struct MuxaSegmented<Value: Hashable>: View {
    @Binding var selection: Value
    let options: [Value]
    let label: (Value) -> Text
    @Environment(\.colorScheme) private var colorScheme
    @Namespace private var namespace

    var body: some View {
        HStack(spacing: 2) {
            ForEach(options, id: \.self) { option in
                let active = option == selection
                Button {
                    withAnimation(.easeOut(duration: 0.15)) { selection = option }
                } label: {
                    label(option)
                        .font(.system(size: 11, weight: active ? .semibold : .medium))
                        .lineLimit(1)
                        .foregroundStyle(active ? Color.primary : Color.secondary)
                        .padding(.horizontal, 10)
                        .frame(minHeight: 20)
                        .background {
                            if active {
                                RoundedRectangle(cornerRadius: 4, style: .continuous)
                                    .fill(MuxaTheme.segmentChip(colorScheme))
                                    .shadow(color: .black.opacity(colorScheme == .dark ? 0.35 : 0.1), radius: 1, y: 0.5)
                                    .matchedGeometryEffect(id: "chip", in: namespace)
                            }
                        }
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
        .padding(2)
        .background(
            RoundedRectangle(cornerRadius: 6, style: .continuous)
                .fill(MuxaTheme.segmentTrack(colorScheme))
        )
        .fixedSize()
    }
}

// MARK: - Text field

extension View {
    /// Flat field chrome for a `.plain` TextField: the input background, a
    /// hairline border, and 5pt corners, replacing `.roundedBorder`.
    func muxaFieldChrome(focused: Bool = false) -> some View {
        modifier(MuxaFieldChrome(focused: focused))
    }
}

private struct MuxaFieldChrome: ViewModifier {
    let focused: Bool
    @Environment(\.colorScheme) private var colorScheme

    func body(content: Content) -> some View {
        content
            .textFieldStyle(.plain)
            .font(.system(size: 12))
            .padding(.horizontal, 8)
            .padding(.vertical, 5)
            .background(
                RoundedRectangle(cornerRadius: 5, style: .continuous)
                    .fill(MuxaTheme.inputBackground(colorScheme))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 5, style: .continuous)
                    .strokeBorder(focused ? Color.accentColor : MuxaTheme.inputBorder(colorScheme), lineWidth: 1)
            )
    }
}

// MARK: - Section header

/// VS Code's side bar title: small, uppercase, tracked, with the view's
/// actions on the trailing edge.
struct MuxaSectionTitle<Actions: View>: View {
    let title: String
    @ViewBuilder var actions: Actions

    var body: some View {
        HStack(spacing: 2) {
            Text(verbatim: title.uppercased())
                .font(.system(size: 11, weight: .semibold))
                .tracking(0.4)
                .foregroundStyle(.secondary)
                .lineLimit(1)
            Spacer(minLength: 6)
            actions
        }
        .padding(.leading, 14)
        .padding(.trailing, 8)
        .frame(height: MuxaTheme.sectionHeaderHeight)
    }
}

// MARK: - Text tabs

/// Underlined text tabs, the way VS Code switches panel views (Problems,
/// Output, Terminal): used instead of a segmented control inside editors.
struct MuxaTextTabs<Value: Hashable & Identifiable>: View {
    let items: [Value]
    @Binding var selection: Value
    let title: (Value) -> LocalizedStringKey

    var body: some View {
        HStack(spacing: 14) {
            ForEach(items) { item in
                let active = item == selection
                Button {
                    selection = item
                } label: {
                    Text(title(item))
                        .font(.system(size: 11, weight: active ? .semibold : .regular))
                        .textCase(.uppercase)
                        .tracking(0.3)
                        .foregroundStyle(active ? Color.primary : Color.secondary)
                        .frame(maxHeight: .infinity)
                        .overlay(alignment: .bottom) {
                            Rectangle()
                                .fill(active ? Color.accentColor : Color.clear)
                                .frame(height: 1.5)
                        }
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
    }
}

// MARK: - Search field

/// The side bar's filter box: a flat, bordered 26pt field whose border takes
/// the accent while it has focus.
struct MuxaFilterField<Trailing: View>: View {
    let prompt: String
    @Binding var text: String
    var focused: FocusState<Bool>.Binding
    @ViewBuilder var trailing: Trailing
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        HStack(spacing: 5) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
            TextField(prompt, text: $text)
                .textFieldStyle(.plain)
                .font(.system(size: 12))
                .focused(focused)
            if !text.isEmpty {
                Button {
                    text = ""
                } label: {
                    Image(systemName: "xmark.circle.fill")
                }
                .buttonStyle(.muxaIcon(size: 18))
                .foregroundStyle(.secondary)
                .help("Clear")
            }
            trailing
        }
        .padding(.leading, 7)
        .padding(.trailing, 3)
        .frame(height: 26)
        .background(
            RoundedRectangle(cornerRadius: MuxaTheme.controlRadius, style: .continuous)
                .fill(MuxaTheme.inputBackground(colorScheme))
        )
        .overlay(
            RoundedRectangle(cornerRadius: MuxaTheme.controlRadius, style: .continuous)
                .strokeBorder(
                    focused.wrappedValue ? Color.accentColor : MuxaTheme.inputBorder(colorScheme),
                    lineWidth: 1
                )
        )
    }
}
