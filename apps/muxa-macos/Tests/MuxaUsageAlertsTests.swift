import Foundation
import Testing
@testable import Muxa

private let now = Date(timeIntervalSince1970: 1_800_000_000)

private func group(_ percent: Double, reset: Date? = now.addingTimeInterval(3600), sampled: Date? = now, hosts: [String] = ["mini"]) -> MuxaUsageGroup {
    MuxaUsageGroup(
        hostAliases: hosts,
        hostIsLocal: false,
        provider: "claude_code",
        windows: [MuxaUsageWindow(kind: .fiveHour, percent: percent, resetsAt: reset, sampledAt: sampled)],
        liveCostUSD: nil,
        capped: []
    )
}

@Test func aWindowAlertsOncePerThreshold() {
    let first = MuxaUsageAlerts.due(groups: [group(82)], notified: [], now: now)
    #expect(first.alerts.map(\.alert.threshold) == [80])
    let notified = Set(first.alerts.flatMap(\.crossed))
    #expect(MuxaUsageAlerts.due(groups: [group(88)], notified: notified, now: now).alerts.isEmpty)
    let urgent = MuxaUsageAlerts.due(groups: [group(96)], notified: notified, now: now)
    #expect(urgent.alerts.map(\.alert.threshold) == [95])
}

@Test func aJumpPastBothThresholdsAnnouncesOnlyTheHigher() {
    let due = MuxaUsageAlerts.due(groups: [group(97)], notified: [], now: now)
    #expect(due.alerts.count == 1)
    #expect(due.alerts[0].alert.threshold == 95)
    #expect(due.alerts[0].crossed.count == 2)
}

@Test func aStaleReadingDoesNotAlert() {
    let due = MuxaUsageAlerts.due(groups: [group(97, sampled: now.addingTimeInterval(-3 * 3600))], notified: [], now: now)
    #expect(due.alerts.isEmpty)
}

@Test func theSameWindowOnAnotherHostDoesNotAlertAgain() {
    let first = MuxaUsageAlerts.due(groups: [group(85, hosts: ["mini"])], notified: [], now: now)
    let notified = Set(first.alerts.flatMap(\.crossed))
    #expect(MuxaUsageAlerts.due(groups: [group(85, hosts: ["mini", "mbp"])], notified: notified, now: now).alerts.isEmpty)
}

@Test func accountsThatOnlyShareAResetDoNotSilenceEachOther() {
    let reset = now.addingTimeInterval(3600)
    let due = MuxaUsageAlerts.due(
        groups: [group(96, reset: reset, hosts: ["mini"]), group(96, reset: reset, hosts: ["rtzr"])],
        notified: [], now: now
    )
    #expect(due.alerts.count == 2)
    let later = MuxaUsageAlerts.due(groups: [group(96, reset: reset, hosts: ["mbp"])], notified: Set(due.alerts.flatMap(\.crossed)), now: now)
    #expect(later.alerts.count == 1)
}

@Test func resetsInTheSameMinuteAreDifferentWindows() {
    let first = MuxaUsageAlerts.due(groups: [group(85, reset: now.addingTimeInterval(3600))], notified: [], now: now)
    let other = group(85, reset: now.addingTimeInterval(3610))
    #expect(MuxaUsageAlerts.due(groups: [other], notified: Set(first.alerts.flatMap(\.crossed)), now: now).alerts.count == 1)
}

@Test func aHostLeavingKeepsTheHistory() {
    let first = MuxaUsageAlerts.due(groups: [group(85, hosts: ["mini", "mbp"])], notified: [], now: now)
    #expect(first.alerts.count == 1)
    #expect(MuxaUsageAlerts.due(groups: [group(85, hosts: ["mbp"])], notified: Set(first.alerts.flatMap(\.crossed)), now: now).alerts.isEmpty)
}

@Test func aHostThatJoinedKeepsTheHistoryAfterTheFirstLeaves() {
    let first = MuxaUsageAlerts.due(groups: [group(96, hosts: ["mini"])], notified: [], now: now)
    var notified = Set(first.alerts.flatMap(\.crossed))
    let joined = MuxaUsageAlerts.due(groups: [group(96, hosts: ["mini", "mbp"])], notified: notified, now: now)
    #expect(joined.alerts.isEmpty)
    #expect(!joined.silent.isEmpty)
    notified.formUnion(joined.silent)
    #expect(MuxaUsageAlerts.due(groups: [group(96, hosts: ["mbp"])], notified: notified, now: now).alerts.isEmpty)
}

@Test(arguments: [85.0, 70.0])
func anAnnouncementPassesToAJoiningHostEvenAfterTheFigureFalls(joinedAt: Double) {
    let first = MuxaUsageAlerts.due(groups: [group(96, hosts: ["mini"])], notified: [], now: now)
    var notified = Set(first.alerts.flatMap(\.crossed))
    let joined = MuxaUsageAlerts.due(groups: [group(joinedAt, hosts: ["mini", "mbp"])], notified: notified, now: now)
    #expect(joined.alerts.isEmpty)
    notified.formUnion(joined.silent)
    #expect(MuxaUsageAlerts.due(groups: [group(96, hosts: ["mbp"])], notified: notified, now: now).alerts.isEmpty)
}

@Test func aNewWindowStartsOver() {
    let first = MuxaUsageAlerts.due(groups: [group(85)], notified: [], now: now)
    let notified = Set(first.alerts.flatMap(\.crossed))
    let next = group(85, reset: now.addingTimeInterval(5 * 3600))
    #expect(MuxaUsageAlerts.due(groups: [next], notified: notified, now: now).alerts.count == 1)
}

@Test func aWindowWithoutAResetRearmsOnlyAfterALowReading() {
    let first = MuxaUsageAlerts.due(groups: [group(85, reset: nil)], notified: [], now: now)
    let notified = Set(first.alerts.flatMap(\.crossed))
    #expect(MuxaUsageAlerts.due(groups: [group(70, reset: nil)], notified: notified, now: now).rearmed.isEmpty)
    #expect(MuxaUsageAlerts.due(groups: [group(20, reset: nil)], notified: notified, now: now).rearmed.count == 1)
}

@Test func theHotKeyDefaultsToOptionSpaceAndCanBeTurnedOff() {
    let defaults = UserDefaults(suiteName: "hotkey-\(UUID().uuidString)")!
    #expect(MuxaHotKeyPreset.current(defaults) == .optionSpace)
    defaults.set(MuxaHotKeyPreset.off.rawValue, forKey: MuxaHotKeyPreset.defaultsKey)
    #expect(MuxaHotKeyPreset.current(defaults).carbonModifiers == nil)
}
