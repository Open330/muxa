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
    @Published private(set) var errorMessage: String?
    @Published private(set) var isRefreshing = false

    private static let openingPlaceholder = "Opening live screen…"
    private static let unavailablePlaceholder = "This backend cannot capture the selected pane."

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
        screenText = String(content.characters)
        screenContent = content
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
                        .font(.caption.weight(.semibold))
                        .fixedSize()
                    Text("\(target.host.alias) · \(target.pane.session) › \(target.pane.windowName) › \(target.pane.paneID)")
                        .font(.caption2.monospaced())
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    Text("Monitor")
                        .font(.caption2.weight(.medium))
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
                ScrollView([.horizontal, .vertical]) {
                    Text(model.screenContent)
                        .font(TerminalPreviewFont.font)
                        .foregroundStyle(TerminalCapturePalette.palette(for: colorScheme).foreground)
                        .textSelection(.disabled)
                        .fixedSize(horizontal: true, vertical: true)
                        .frame(
                            minWidth: max(0, proxy.size.width - 24),
                            minHeight: max(0, proxy.size.height - 24),
                            alignment: .topLeading
                        )
                        .padding(12)
                }
                .frame(width: proxy.size.width, height: proxy.size.height)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(MuxaSurfacePalette.terminal(for: colorScheme))

            if let errorMessage = model.errorMessage {
                Label(errorMessage, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
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
    static let pointSize: CGFloat = 12

    static let font: Font = Font(nsFont)

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
