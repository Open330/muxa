import AppKit
import SwiftUI

/// What's New in Muxa X.Y.Z: opened once after an upgrade instead of the
/// full Welcome guide (see `OnboardingPreferences.launchPresentation`), and
/// from Help › What's New in Muxa.
struct WhatsNewView: View {
    @Environment(\.openWindow) private var openWindow
    @Environment(\.colorScheme) private var colorScheme
    @State private var hostWindow: NSWindow?

    private let version = OnboardingPreferences.currentVersion

    private var releases: [MuxaWhatsNew.Release] {
        MuxaWhatsNew.releasesToShow(for: version)
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(alignment: .center, spacing: 14) {
                Image(nsImage: NSApp.applicationIconImage)
                    .resizable()
                    .frame(width: 48, height: 48)
                VStack(alignment: .leading, spacing: 3) {
                    Text("What's New in Muxa \(releases.first?.version ?? version)")
                        .font(.title3.weight(.semibold))
                    Text("Highlights of this release.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                Spacer()
            }
            .padding(20)

            MuxaTheme.border(colorScheme).frame(height: 1)

            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    ForEach(releases.flatMap(\.highlights)) { highlight in
                        HStack(alignment: .top, spacing: 12) {
                            Image(systemName: highlight.systemImage)
                                .font(.title3)
                                .foregroundStyle(Color.accentColor)
                                .frame(width: 26)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(verbatim: highlight.title)
                                    .font(.system(size: 13, weight: .semibold))
                                Text(verbatim: highlight.detail)
                                    .font(.system(size: 12))
                                    .foregroundStyle(.secondary)
                                    .fixedSize(horizontal: false, vertical: true)
                            }
                        }
                    }
                    if releases.isEmpty {
                        Text("No highlights were recorded for this version.")
                            .foregroundStyle(.secondary)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(20)
            }

            MuxaTheme.border(colorScheme).frame(height: 1)

            HStack(spacing: 8) {
                Button("Open Welcome Guide") {
                    openWindow(id: OnboardingPreferences.windowID)
                    close()
                }
                .buttonStyle(.muxaSecondary)
                Spacer()
                Button("Continue") { close() }
                    .buttonStyle(.muxaPrimary)
                    .keyboardShortcut(.defaultAction)
            }
            .padding(14)
        }
        .frame(width: 520, height: 540)
        .background(MuxaTheme.editor(colorScheme))
        .background(OnboardingWindowTracker(identifier: OnboardingPreferences.whatsNewWindowIdentifier) { window in
            if let window { hostWindow = window }
        })
        .onExitCommand(perform: close)
    }

    private func close() {
        (hostWindow ?? OnboardingPreferences.existingWindow(identifier: OnboardingPreferences.whatsNewWindowIdentifier))?
            .close()
    }
}
