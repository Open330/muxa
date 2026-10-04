import AppKit
import SwiftUI

struct MuxaPaneTarget: Hashable, Sendable {
    let host: MuxaFleetHostIdentity
    let pane: MuxaPaneInfo
}

@MainActor
private final class PaneCaptureModel: ObservableObject {
    /// The screen rendered with the pane's SGR colors and attributes.
    @Published private(set) var screenContent = AttributedString(PaneCaptureModel.openingPlaceholder)
    /// Plain text of `screenContent`, for the copy action.
    private(set) var screenText = PaneCaptureModel.openingPlaceholder
    /// Terminal cells in the widest line of `screenContent`, for fitting the
    /// preview font to the panel width.
    @Published private(set) var columns = 0
    @Published private(set) var errorMessage: String?
    @Published private(set) var isRefreshing = false

    private static let openingPlaceholder = String(localized: "Opening live screen…")
    private static let unavailablePlaceholder = String(localized: "This backend cannot capture the selected pane.")

    /// What the last capture returned. Change detection compares this, not
    /// the rendered `AttributedString`.
    private enum CaptureSource: Equatable {
        case placeholder
        case raw(Data)
        case plain(String)
        case unavailable
    }

    private let client: MuxaIPCClient
    private let target: MuxaPaneTarget
    private var isVisible = true
    private var isApplicationActive = NSApp.isActive
    private var hasLoaded = false
    private var source = CaptureSource.placeholder
    private var colorScheme = ColorScheme.dark

    init(client: MuxaIPCClient, target: MuxaPaneTarget) {
        self.client = client
        self.target = target
    }

    func run() async {
        var unchangedReads = 0
        while !Task.isCancelled {
            guard isVisible, isApplicationActive else {
                do {
                    try await Task.sleep(for: .seconds(5))
                } catch {
                    return
                }
                continue
            }
            let changed = await refresh()
            unchangedReads = changed ? 0 : min(unchangedReads + 1, 8)
            let interval: Duration = switch unchangedReads {
            case 0: .milliseconds(750)
            case 1...2: .milliseconds(1500)
            case 3...5: .seconds(3)
            default: .seconds(5)
            }
            do {
                try await Task.sleep(for: interval)
            } catch {
                return
            }
        }
    }

    func setVisible(_ visible: Bool) {
        isVisible = visible
    }

    func setApplicationActive(_ active: Bool) {
        isApplicationActive = active
    }

    /// Re-renders the last capture with the palette for `scheme` when it
    /// differs from the one used so far.
    func setColorScheme(_ scheme: ColorScheme) {
        guard scheme != colorScheme else { return }
        colorScheme = scheme
        render()
    }

    private func render() {
        let formatter = TerminalCaptureFormatter(palette: .palette(for: colorScheme))
        let content: AttributedString = switch source {
        case .placeholder: AttributedString(PaneCaptureModel.openingPlaceholder)
        case .raw(let bytes): formatter.render(bytes: bytes)
        case .plain(let text): formatter.render(text: text)
        case .unavailable: AttributedString(Self.unavailablePlaceholder)
        }
        let trimmed = TerminalCaptureFormatter.trimmingTrailingBlankLines(content)
        screenText = String(trimmed.characters)
        columns = TerminalPreviewFont.columns(in: screenText)
        screenContent = trimmed
    }

    private func refresh() async -> Bool {
        guard !isRefreshing else { return false }
        if !hasLoaded { isRefreshing = true }
        defer {
            hasLoaded = true
            if isRefreshing { isRefreshing = false }
        }
        do {
            let capture = try await client.captureFleetPane(host: target.host, pane: target.pane)
            let nextSource: CaptureSource = if let bytes = capture.rawBytes {
                .raw(bytes)
            } else if let text = capture.screenText {
                .plain(text)
            } else {
                .unavailable
            }
            let changed = nextSource != source
            if changed {
                source = nextSource
                render()
            }
            if errorMessage != nil { errorMessage = nil }
            return changed
        } catch is CancellationError {
            return false
        } catch {
            let message = error.localizedDescription
            if errorMessage != message { errorMessage = message }
            return false
        }
    }
}

struct PaneCaptureView: View {
    @StateObject private var model: PaneCaptureModel
    private let target: MuxaPaneTarget
    private let showsHeader: Bool
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.scenePhase) private var scenePhase
    @State private var followsBottom = true
    /// The bottom anchor and the viewport height as last laid out.
    @State private var lastAnchor = PaneCaptureAnchor(viewport: .zero, contentY: 0)
    @State private var viewportHeight: CGFloat = 0

    private static let bottomAnchor = "pane-capture-bottom"
    private static let viewport = "pane-capture-viewport"
    private static let content = "pane-capture-content"

    init(client: MuxaIPCClient, target: MuxaPaneTarget, showsHeader: Bool = true) {
        self.target = target
        self.showsHeader = showsHeader
        _model = StateObject(wrappedValue: PaneCaptureModel(client: client, target: target))
    }

    var body: some View {
        VStack(spacing: 0) {
            if showsHeader {
                HStack(spacing: 10) {
                    Label("Live Pane", systemImage: "terminal")
                        .font(MuxaType.detail.weight(.semibold))
                        .fixedSize()
                    Text(verbatim: "\(target.host.alias) · \(target.pane.session) › \(target.pane.windowName) › \(target.pane.paneID)")
                        .font(MuxaType.meta.monospaced())
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    Text("Monitor")
                        .font(MuxaType.meta.weight(.medium))
                        .foregroundStyle(.secondary)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(Color.primary.opacity(0.07), in: Capsule())
                    Spacer(minLength: 4)
                    if model.isRefreshing {
                        ProgressView().controlSize(.mini)
                    }
                    copyButton
                }
                .padding(.horizontal, 10)
                .frame(height: 36)
                .background(MuxaSurfacePalette.sidebar(for: colorScheme))

                Divider()
            }

            GeometryReader { proxy in
                let padding = (
                    x: MuxaTerminalAppearance.paddingX,
                    y: MuxaTerminalAppearance.paddingY
                )
                let font = TerminalPreviewFont.fitted(
                    columns: model.columns,
                    width: proxy.size.width - padding.x * 2
                )
                ScrollViewReader { scroller in
                    ScrollView([.horizontal, .vertical]) {
                        VStack(alignment: .leading, spacing: 0) {
                            Text(model.screenContent)
                                .font(Font(font))
                                .foregroundStyle(TerminalCapturePalette.palette(for: colorScheme).foreground.color)
                                .textSelection(.disabled)
                                .fixedSize(horizontal: true, vertical: true)
                            Color.clear
                                .frame(height: 1)
                                .id(Self.bottomAnchor)
                                .background(
                                    GeometryReader { anchor in
                                        Color.clear.preference(
                                            key: PaneCaptureBottomKey.self,
                                            value: PaneCaptureAnchor(
                                                viewport: anchor.frame(in: .named(Self.viewport)),
                                                contentY: anchor.frame(in: .named(Self.content)).minY
                                            )
                                        )
                                    }
                                )
                        }
                        .coordinateSpace(name: Self.content)
                        .frame(
                            minWidth: max(0, proxy.size.width - padding.x * 2),
                            minHeight: max(0, proxy.size.height - padding.y * 2),
                            // Short content starts at the top, where Ghostty
                            // draws it after Click to Type.
                            alignment: .topLeading
                        )
                        .padding(.horizontal, padding.x)
                        .padding(.vertical, padding.y)
                    }
                    .coordinateSpace(name: Self.viewport)
                    .frame(width: proxy.size.width, height: proxy.size.height)
                    .onPreferenceChange(PaneCaptureBottomKey.self) { anchor in
                        // New output, a reflow, or a resized panel moves the
                        // newest line without the reader doing anything, and
                        // the follow scroll that comes with it may land
                        // before or after this report. Only a move with the
                        // same content and viewport is the reader scrolling.
                        let contentMoved = anchor.contentY != lastAnchor.contentY
                            || proxy.size.height != viewportHeight
                        lastAnchor = anchor
                        viewportHeight = proxy.size.height
                        guard !contentMoved else { return }
                        updateFollowing()
                    }
                    .onAppear { scroller.scrollTo(Self.bottomAnchor, anchor: .bottomLeading) }
                    .onChange(of: model.screenContent) { _ in
                        followToBottom(scroller)
                    }
                    .onChange(of: proxy.size) { _ in
                        followToBottom(scroller)
                    }
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(MuxaSurfacePalette.terminal(for: colorScheme))

            if let errorMessage = model.errorMessage {
                Label(errorMessage, systemImage: "exclamationmark.triangle.fill")
                    .font(MuxaType.detail)
                    .foregroundStyle(.orange)
                    .padding(.horizontal, 10)
                    .frame(maxWidth: .infinity, minHeight: 28, alignment: .leading)
                    .background(MuxaSurfacePalette.sidebar(for: colorScheme))
            }
        }
        .frame(maxWidth: .infinity, minHeight: showsHeader ? 180 : 120, maxHeight: .infinity)
        .clipped()
        .overlay {
            if showsHeader {
                Rectangle()
                    .stroke(Color(nsColor: .separatorColor).opacity(0.7), lineWidth: 0.5)
            }
        }
        .task { await model.run() }
        .onAppear {
            model.setVisible(true)
            model.setApplicationActive(scenePhase == .active)
            model.setColorScheme(colorScheme)
        }
        .onDisappear { model.setVisible(false) }
        .onChange(of: scenePhase) { phase in
            model.setApplicationActive(phase == .active)
        }
        .onChange(of: colorScheme) { scheme in
            model.setColorScheme(scheme)
        }
    }

    /// Follow the newest line until the reader scrolls it out of view, or
    /// scrolls sideways to read a wide pane (a follow scroll would also snap
    /// back to the left edge), and again once they return.
    private func updateFollowing() {
        let atBottom = lastAnchor.viewport.minY <= viewportHeight + 4
        let atLeadingEdge = lastAnchor.viewport.minX >= MuxaTerminalAppearance.paddingX - 4
        followsBottom = atBottom && atLeadingEdge
    }

    private func followToBottom(_ scroller: ScrollViewProxy) {
        guard followsBottom else { return }
        scroller.scrollTo(Self.bottomAnchor, anchor: .bottomLeading)
    }

    private var copyButton: some View {
        Button {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(model.screenText, forType: .string)
        } label: {
            Image(systemName: "doc.on.doc")
        }
        .buttonStyle(.plain)
        .help("Copy live screen")
    }
}

/// Where the bottom of a pane capture sits. In the viewport, its `minY`
/// says whether the newest line is in view and its `minX` whether the reader
/// scrolled sideways. In the content, its `contentY` changes only when the
/// capture itself grows or reflows.
struct PaneCaptureAnchor: Equatable {
    let viewport: CGRect
    let contentY: CGFloat
}

private struct PaneCaptureBottomKey: PreferenceKey {
    static let defaultValue = PaneCaptureAnchor(viewport: .zero, contentY: 0)

    static func reduce(value: inout PaneCaptureAnchor, nextValue: () -> PaneCaptureAnchor) {
        value = nextValue()
    }
}

/// Font for the read-only screen preview.
///
/// Agent prompts such as powerlevel10k and starship draw with Nerd Font
/// private-use glyphs. The system monospaced font has no glyphs for them and
/// shows a "?" box that is also wider than a cell, which shifts the rest of
/// the line. When the user has a Nerd Font installed (the interactive Ghostty
/// surface already draws these prompts with one), use its best monospace
/// variant as the preview font so both views show the same characters at
/// the same positions. A cascade-list fallback behind the system font is
/// not honored for system fonts, so the Nerd Font has to be the primary.
@MainActor
enum TerminalPreviewFont {
    /// The interactive surface's size, so a Live Pane keeps its layout
    /// when Click to Type swaps the preview for Ghostty.
    static let pointSize: CGFloat = MuxaTerminalAppearance.fontSize
    /// The smallest size `fitted` shrinks to before the preview scrolls
    /// sideways instead.
    static let minimumFittedPointSize: CGFloat = 10

    static let font: Font = Font(nsFont)

    /// One cell's width per point of font size.
    private static let cellWidthPerPoint: CGFloat = {
        let width = ("M" as NSString).size(withAttributes: [.font: nsFont]).width
        return width / pointSize
    }()

    private static var fittedFonts: [CGFloat: NSFont] = [:]

    /// The preview font at the largest size, up to `pointSize`, that shows
    /// `columns` cells in `width` points. A pane wider than the panel shrinks
    /// to `minimumFittedPointSize` before it needs a horizontal scroll.
    static func fitted(columns: Int, width: CGFloat) -> NSFont {
        guard columns > 0, width > 0 else { return nsFont }
        let fitting = width / (CGFloat(columns) * cellWidthPerPoint)
        // Half-point steps keep a resizing panel from minting a font per pixel.
        let size = (min(pointSize, max(minimumFittedPointSize, fitting)) * 2).rounded(.down) / 2
        guard size < pointSize else { return nsFont }
        if let cached = fittedFonts[size] { return cached }
        let font = NSFont(descriptor: nsFont.fontDescriptor, size: size) ?? nsFont
        fittedFonts[size] = font
        return font
    }

    /// Terminal cells in the widest line of `text`: East Asian wide and
    /// fullwidth characters (Hangul, CJK, most emoji) take two.
    nonisolated static func columns(in text: String) -> Int {
        var widest = 0
        for line in text.split(separator: "\n", omittingEmptySubsequences: false) {
            var cells = 0
            for scalar in line.unicodeScalars {
                cells += scalar == "\t" ? 8 - cells % 8 : cellWidth(of: scalar)
            }
            widest = max(widest, cells)
        }
        return widest
    }

    nonisolated static func cellWidth(of scalar: Unicode.Scalar) -> Int {
        switch scalar.value {
        case 0x0300...0x036F, 0x200B...0x200F, 0xFE00...0xFE0F:
            0
        case 0x1100...0x115F, 0x2E80...0x303E, 0x3041...0x33FF, 0x3400...0x4DBF,
             0x4E00...0x9FFF, 0xA000...0xA4CF, 0xAC00...0xD7A3, 0xF900...0xFAFF,
             0xFE30...0xFE4F, 0xFF00...0xFF60, 0xFFE0...0xFFE6, 0x1F300...0x1F64F,
             0x1F900...0x1F9FF, 0x20000...0x3FFFD:
            2
        default:
            1
        }
    }

    static let nsFont: NSFont = {
        let system = NSFont.monospacedSystemFont(ofSize: pointSize, weight: .regular)
        let manager = NSFontManager.shared
        guard let family = nerdFontFamily(available: manager.availableFontFamilies),
              let nerd = manager.font(withFamily: family, traits: [], weight: 5, size: pointSize)
        else { return system }
        return nerd
    }()

    /// Prefer the `Mono` Nerd Font variants, whose icons are exactly one cell
    /// wide, then any other installed Nerd Font family.
    nonisolated static func nerdFontFamily(available: [String]) -> String? {
        let families = available.filter { family in
            family.localizedCaseInsensitiveContains("Nerd Font")
                || family.hasSuffix(" NF")
                || family.hasSuffix(" NFM")
        }
        guard !families.isEmpty else { return nil }
        let ranked = families.sorted { left, right in
            let leftRank = monoVariantRank(left)
            let rightRank = monoVariantRank(right)
            if leftRank != rightRank { return leftRank < rightRank }
            return left.localizedStandardCompare(right) == .orderedAscending
        }
        return ranked.first
    }

    /// Nerd Fonts ship each family as `X Nerd Font`, `X Nerd Font Mono`
    /// (icons one cell wide), and `X Nerd Font Propo`. Only the variant
    /// suffix matters: a base name such as "JetBrainsMono" is not a signal.
    private nonisolated static func monoVariantRank(_ family: String) -> Int {
        let lowered = family.lowercased()
        if lowered.hasSuffix("nerd font mono") || lowered.hasSuffix(" nfm") { return 0 }
        if lowered.hasSuffix("nerd font propo") || lowered.hasSuffix(" nfp") { return 2 }
        return 1
    }
}
