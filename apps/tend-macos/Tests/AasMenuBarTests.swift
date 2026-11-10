import Foundation
import SwiftUI
import Testing
@testable import Tend

@Test func aasMenuBarSummarizesActiveAccountsWithoutAddingTheirLimits() {
    let now = Date(timeIntervalSince1970: 1_800_000_000)
    let ms = Int64(now.timeIntervalSince1970 * 1000)
    let accounts = [
        AasAccount(provider: "claude", name: "active-c", active: true, fetchedAtMs: ms,
            meters: [AasMeter(label: "5h", usedPct: 25, resetMs: ms + 60_000), AasMeter(label: "7d", usedPct: 63, resetMs: nil)]),
        AasAccount(provider: "codex", name: "active-x", active: true, fetchedAtMs: ms,
            meters: [AasMeter(label: "5h", usedPct: 50, resetMs: nil)]),
        AasAccount(provider: "codex", name: "other", active: false, fetchedAtMs: ms,
            meters: [AasMeter(label: "7d", usedPct: 99, resetMs: nil)]),
    ]
    #expect(AasMenuBarUsage.peak(accounts, now: now) == 63)
    #expect(!AasMenuBarUsage.hasWarning(accounts, now: now))
    #expect(AasMenuBarUsage.title(accounts, failed: false, now: now).contains("63"))
    #expect(AasMenuBarUsage.title(accounts, failed: true, now: now).hasSuffix("!"))
}

@Test func aasMenuBarRejectsExpiredAndUnknownUsageAndMarksStaleReadings() {
    let now = Date(timeIntervalSince1970: 1_800_000_000)
    let ms = Int64(now.timeIntervalSince1970 * 1000)
    let expired = AasAccount(provider: "codex", name: "expired", active: true, fetchedAtMs: ms,
        meters: [AasMeter(label: "5h", usedPct: 100, resetMs: ms), AasMeter(label: "7d", usedPct: .nan, resetMs: nil)])
    #expect(AasMenuBarUsage.meters(expired, now: now).isEmpty)
    #expect(AasMenuBarUsage.peak([expired], now: now) == nil)
    #expect(AasMenuBarUsage.title([expired], failed: false, now: now) == "AAS — !")
    let stale = AasAccount(provider: "claude", name: "old", active: true, fetchedAtMs: ms - 601_000,
        meters: [AasMeter(label: "7d", usedPct: 80, resetMs: nil)])
    #expect(AasMenuBarUsage.isStale(stale, now: now))
    #expect(AasMenuBarUsage.title([stale], failed: false, now: now).hasSuffix("!"))
    #expect(AasMenuBarUsage.title([], failed: false, now: now) == "AAS —")
}

@Test func aasMenuBarKeepsAllAccountsDespitePartialProviderMetadata() {
    let accounts = [AasAccount(provider: "claude", name: "one"), AasAccount(provider: "codex", name: "two")]
    let groups = AasMenuBarUsage.groups(accounts, headings: [AasProviderGroup(provider: "claude", title: "Claude", accounts: [])])
    #expect(groups.map(\.title) == ["Claude", "codex"])
    #expect(groups.flatMap(\.accounts).map(\.id) == accounts.map(\.id))
}

@MainActor private final class AasMenuBarLifecycleModule: TendModule {
    nonisolated static let identity = TendModuleIdentity(id: "test-usage", title: "Usage", blurb: "", symbolName: "gauge.medium")
    var availability: TendModuleAvailability = .available(version: nil, detail: nil)
    var stopped = 0
    func probe() async {}
    func shutdown() { stopped += 1 }
    func settingsPane(model: AppModel) -> AnyView { AnyView(EmptyView()) }
}

@Test @MainActor func disablingModuleStopsItsMenuBarResources() throws {
    let suite = "tend-test-module-\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    let module = AasMenuBarLifecycleModule()
    let registry = TendModuleRegistry(modules: [module], defaults: defaults)
    registry.setEnabled(module.id, true)
    registry.setEnabled(module.id, false)
    #expect(module.stopped == 1)
    #expect(registry.enabledModules.isEmpty)
}
