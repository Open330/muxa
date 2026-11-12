import AppKit
import SwiftUI

/// The per-app language override. It is stored as `AppleLanguages` in the
/// app's own defaults domain, which macOS consults before the system-wide
/// language list; `system` removes the key so that list applies again.
enum TendLanguage: String, CaseIterable, Identifiable, Sendable {
    case system
    case english = "en"
    case korean = "ko"

    static let defaultsKey = "AppleLanguages"

    var id: Self { self }

    /// The `AppleLanguages` value for this choice; nil removes the override.
    var appleLanguages: [String]? {
        switch self {
        case .system: nil
        case .english: ["en"]
        case .korean: ["ko"]
        }
    }

    /// The override currently stored for `bundleIdentifier` in `defaults`.
    /// Only the app's persistent domain counts: reading the key through the
    /// composite `UserDefaults` API would also return the system-wide list.
    static func current(
        in defaults: UserDefaults = .standard,
        bundleIdentifier: String? = Bundle.main.bundleIdentifier
    ) -> TendLanguage {
        guard let bundleIdentifier,
              let languages = defaults.persistentDomain(forName: bundleIdentifier)?[defaultsKey] as? [String],
              let first = languages.first else {
            return .system
        }
        return allCases.first { $0 != .system && first.hasPrefix($0.rawValue) } ?? .system
    }

    func apply(to defaults: UserDefaults = .standard) {
        if let appleLanguages {
            defaults.set(appleLanguages, forKey: Self.defaultsKey)
        } else {
            defaults.removeObject(forKey: Self.defaultsKey)
        }
    }
}

@MainActor
enum TendLanguagePreference {
    /// The override in effect when this process started. Captured once (the
    /// app delegate touches it at launch) so the Settings pane can tell
    /// whether a relaunch is still pending, even after the window is closed
    /// and reopened.
    static let atLaunch = TendLanguage.current()

    /// Starts a second instance of this bundle and quits the current one.
    static func relaunch() {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/open")
        process.arguments = ["-n", Bundle.main.bundlePath]
        do {
            try process.run()
        } catch {
            TendLog.app.error("relaunch failed: \(error.localizedDescription, privacy: .public)")
            return
        }
        NSApp.terminate(nil)
    }
}

/// Settings › General › Language: System / English / 한국어. The choice is
/// written immediately; the UI language changes on the next launch.
struct TendLanguageSettingsSection: View {
    @State private var selection = TendLanguage.current()

    private var needsRelaunch: Bool {
        selection != TendLanguagePreference.atLaunch
    }

    var body: some View {
        Section("Language") {
            Picker("Language", selection: $selection) {
                Text("System").tag(TendLanguage.system)
                Text(verbatim: "English").tag(TendLanguage.english)
                Text(verbatim: "한국어").tag(TendLanguage.korean)
            }
            .onChange(of: selection) { language in
                language.apply()
            }
            if needsRelaunch {
                HStack {
                    Label("Relaunch Tend to apply", systemImage: "arrow.clockwise.circle")
                        .font(TendType.detail)
                        .foregroundStyle(.orange)
                    Spacer()
                    Button("Relaunch") { TendLanguagePreference.relaunch() }
                        .controlSize(.small)
                }
            } else {
                TendSettingsCaption("System follows the macOS language list. Tend is available in English and Korean.")
            }
        }
    }
}
