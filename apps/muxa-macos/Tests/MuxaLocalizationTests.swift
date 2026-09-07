import Foundation
import Testing
@testable import Muxa

/// `Resources/Localizable.xcstrings`, located from this file so the test
/// reads the checked-in catalog rather than a compiled copy.
private let catalogURL = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()
    .deletingLastPathComponent()
    .appendingPathComponent("Resources", isDirectory: true)
    .appendingPathComponent("Localizable.xcstrings")

private func localizedBundle(_ language: String) throws -> Bundle {
    let path = try #require(
        Bundle.main.path(forResource: language, ofType: "lproj"),
        "\(language).lproj is missing from \(Bundle.main.bundlePath)"
    )
    return try #require(Bundle(path: path))
}

/// Whether a catalog localization carries text: a plain string unit, a
/// complete set of plural variations, or a format string with substitutions.
private func hasValue(_ localization: Any?) -> Bool {
    guard let localization = localization as? [String: Any] else { return false }
    let unit = localization["stringUnit"] as? [String: Any]
    if let value = unit?["value"] as? String, !value.isEmpty {
        return true
    }
    if let variations = localization["variations"] as? [String: [String: Any]] {
        for forms in variations.values where !forms.isEmpty {
            if forms.values.allSatisfy(hasValue) { return true }
        }
    }
    return false
}

@Test func stringCatalogHasKoreanForEveryKey() throws {
    let data = try Data(contentsOf: catalogURL)
    let root = try #require(try JSONSerialization.jsonObject(with: data) as? [String: Any])
    #expect(root["sourceLanguage"] as? String == "en")
    let strings = try #require(root["strings"] as? [String: Any])
    #expect(strings.count > 100)
    var missing: [String] = []
    for (key, entry) in strings {
        let localizations = (entry as? [String: Any])?["localizations"] as? [String: Any]
        if !hasValue(localizations?["ko"]) { missing.append(key) }
    }
    #expect(missing.isEmpty, "keys without Korean: \(missing.sorted())")
}

@Test func appBundleShipsKoreanLocalization() throws {
    #expect(Bundle.main.localizations.contains("ko"))
    #expect(Bundle.main.localizations.contains("en"))
    let korean = try localizedBundle("ko")
    #expect(NSLocalizedString("Start Work", bundle: korean, comment: "") == "Work 시작")
    #expect(NSLocalizedString("Open Live Watch", bundle: korean, comment: "") == "Live Watch 열기")
    #expect(NSLocalizedString("Refresh", bundle: korean, comment: "") == "새로 고침")
}

@Test func pluralFormsResolvePerLanguage() throws {
    let english = try localizedBundle("en")
    let korean = try localizedBundle("ko")
    let en = Locale(identifier: "en")
    let ko = Locale(identifier: "ko")
    #expect(String(localized: "\(1) agents", bundle: english, locale: en) == "1 agent")
    #expect(String(localized: "\(3) agents", bundle: english, locale: en) == "3 agents")
    #expect(String(localized: "\(3) agents", bundle: korean, locale: ko) == "에이전트 3개")
    #expect(
        String(localized: "\(1) hosts unreachable: \("rtzr")", bundle: english, locale: en)
            == "1 host unreachable: rtzr"
    )
    #expect(
        String(localized: "\(2) hosts unreachable: \("a, b")", bundle: english, locale: en)
            == "2 hosts unreachable: a, b"
    )
    #expect(
        String(localized: "\(1) agents · \(2) work items", bundle: english, locale: en)
            == "1 agent · 2 work items"
    )
}

@Test func languageOverrideRoundTripsThroughDefaults() throws {
    let suite = "dev.muxa.tests.language.\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }

    #expect(MuxaLanguage.current(in: defaults, bundleIdentifier: suite) == .system)
    MuxaLanguage.korean.apply(to: defaults)
    #expect(defaults.persistentDomain(forName: suite)?["AppleLanguages"] as? [String] == ["ko"])
    #expect(MuxaLanguage.current(in: defaults, bundleIdentifier: suite) == .korean)
    MuxaLanguage.english.apply(to: defaults)
    #expect(MuxaLanguage.current(in: defaults, bundleIdentifier: suite) == .english)
    MuxaLanguage.system.apply(to: defaults)
    #expect(defaults.persistentDomain(forName: suite)?["AppleLanguages"] == nil)
    #expect(MuxaLanguage.current(in: defaults, bundleIdentifier: suite) == .system)
}
