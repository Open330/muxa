import AppKit
import SwiftUI

struct MuxaPaneTarget: Hashable, Sendable {
    let host: MuxaFleetHostIdentity
    let pane: MuxaPaneInfo
}

@MainActor
private final class PaneCaptureModel: ObservableObject {
    @Published private(set) var screenText = "Opening live screen…"
    @Published private(set) var errorMessage: String?
    @Published private(set) var isRefreshing = false

    private let client: MuxaIPCClient
    private let target: MuxaPaneTarget

    init(client: MuxaIPCClient, target: MuxaPaneTarget) {
        self.client = client
        self.target = target
    }

    func run() async {
        while !Task.isCancelled {
            await refresh()
            do {
                try await Task.sleep(for: .seconds(1))
            } catch {
                return
            }
        }
    }

    private func refresh() async {
        guard !isRefreshing else { return }
        isRefreshing = true
        defer { isRefreshing = false }
        do {
            let capture = try await client.captureFleetPane(host: target.host, pane: target.pane)
            screenText = capture.screenText.map(sanitizeTerminalCapture)
                ?? "This backend cannot capture the selected pane."
            errorMessage = nil
        } catch is CancellationError {
            return
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

struct PaneCaptureView: View {
    @StateObject private var model: PaneCaptureModel
    private let target: MuxaPaneTarget
    private let showsHeader: Bool
    @Environment(\.colorScheme) private var colorScheme

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

            ScrollView([.horizontal, .vertical]) {
                Text(verbatim: model.screenText)
                    .font(.system(size: 12, weight: .regular, design: .monospaced))
                    .foregroundStyle(colorScheme == .dark ? Color(white: 0.92) : Color(white: 0.12))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .topLeading)
                    .padding(12)
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
        .frame(minHeight: 180)
        .overlay {
            if showsHeader {
                Rectangle()
                    .stroke(Color(nsColor: .separatorColor).opacity(0.7), lineWidth: 0.5)
            }
        }
        .task { await model.run() }
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

private func sanitizeTerminalCapture(_ value: String) -> String {
    enum EscapeState {
        case text
        case escape
        case csi
        case osc
        case oscEscape
    }

    var state = EscapeState.text
    var output = String.UnicodeScalarView()
    for scalar in value.unicodeScalars {
        switch state {
        case .text where scalar.value == 0x1B:
            state = .escape
        case .text:
            if scalar.value >= 0x20 || scalar == "\n" || scalar == "\t" {
                output.append(scalar)
            }
        case .escape where scalar == "[":
            state = .csi
        case .escape where scalar == "]":
            state = .osc
        case .escape:
            state = .text
        case .csi where (0x40...0x7E).contains(scalar.value):
            state = .text
        case .csi:
            break
        case .osc where scalar.value == 0x07:
            state = .text
        case .osc where scalar.value == 0x1B:
            state = .oscEscape
        case .osc:
            break
        case .oscEscape where scalar == "\\":
            state = .text
        case .oscEscape:
            state = .osc
        }
    }
    return String(output)
}
