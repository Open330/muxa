// A quiet heads-up before a rate limit runs out: when an account's 5-hour
// or 7-day window crosses a threshold, Muxa says so once for that window.
import Foundation

enum MuxaUsageAlerts {
    static let enabledKey = "muxa.notifications.usage"
    static let notifiedKey = "muxa.notifications.usageNotified"
    /// The usage levels' own steps: yellow from 80%, urgent from 95%.
    static let thresholds: [Double] = [80, 95]
    static let urgent = 95.0
    /// A reading older than this is not news: a reconnect or restart that
    /// shows an agent's last figure from an hour ago does not alert.
    static let freshness: TimeInterval = 30 * 60
    /// With no reset time to tell windows apart, a fresh reading this low
    /// means the window has rolled over and may alert again.
    static let rearmBelow = 50.0

    struct Alert: Equatable, Sendable {
        /// The keys this alert answers for, one per host: see `keys`.
        let key: String
        let provider: String
        let hostAliases: [String]
        let kind: MuxaUsageWindowKind
        let percent: Double
        let threshold: Double
        let resetsAt: Date?
    }

    static func isEnabled(_ defaults: UserDefaults = .standard) -> Bool {
        defaults.object(forKey: enabledKey) as? Bool ?? true
    }

    /// One window as seen from each of its hosts: provider, host, kind and
    /// the exact reset (or "none"). Accounts are inferred, not known (see
    /// `MuxaUsageGroup`), so a key never speaks for another host: two
    /// accounts that merely share a reset never silence each other, while a
    /// host joining a group that already announced finds its partner's key
    /// and stays quiet, and one leaving keeps the rest.
    static func cycles(_ group: MuxaUsageGroup, _ window: MuxaUsageWindow) -> [String] {
        let reset = window.resetsAt.map { String(Int($0.timeIntervalSince1970)) } ?? "none"
        return group.hostAliases.map { [group.provider, $0, window.kind.rawValue, reset].joined(separator: "\u{1F}") }
    }

    static func key(cycle: String, threshold: Double) -> String {
        cycle + "\u{1F}" + String(Int(threshold))
    }

    /// The alerts due now: per window with a fresh reading, the highest
    /// threshold it has crossed that none of its hosts announced in this
    /// window yet. An account already capped is left to the limit
    /// notification. `crossed` lists every key the alert covers, every host
    /// and lower thresholds included, so a jump from 79% to 96% announces
    /// 95% only and a figure that falls back does not announce again.
    /// Within one call a key is announced once. `rearmed` lists reset-less
    /// cycles whose fresh reading fell low enough to count as a new window.
    static func due(
        groups: [MuxaUsageGroup],
        notified: Set<String>,
        now: Date
    ) -> (alerts: [(alert: Alert, crossed: [String])], rearmed: [String]) {
        var alerts: [(Alert, [String])] = []
        var rearmed: [String] = []
        var seen = notified
        for group in groups where group.capped.isEmpty {
            for window in group.windows {
                guard let sampled = window.sampledAt, now.timeIntervalSince(sampled) <= freshness else { continue }
                let cycles = cycles(group, window)
                if window.resetsAt == nil, window.percent < rearmBelow {
                    rearmed.append(contentsOf: cycles)
                    continue
                }
                let passed = thresholds.filter { window.percent >= $0 }
                guard let top = passed.max() else { continue }
                guard !cycles.contains(where: { seen.contains(key(cycle: $0, threshold: top)) }) else { continue }
                let crossed = cycles.flatMap { cycle in passed.map { key(cycle: cycle, threshold: $0) } }
                seen.formUnion(crossed)
                alerts.append((
                    Alert(
                        key: key(cycle: cycles.joined(separator: "+"), threshold: top),
                        provider: group.provider,
                        hostAliases: group.hostAliases,
                        kind: window.kind,
                        percent: window.percent,
                        threshold: top,
                        resetsAt: window.resetsAt
                    ),
                    crossed
                ))
            }
        }
        return (alerts, rearmed)
    }

    static func notification(for alert: Alert, sound: Bool) -> MuxaAgentNotification {
        let provider = MuxaUsageFormat.providerName(alert.provider)
        let window = alert.kind == .fiveHour ? String(localized: "5-hour limit") : String(localized: "weekly limit")
        let percent = Int(alert.percent.rounded())
        let reset = alert.resetsAt.map {
            String(localized: "Resets \($0.formatted(date: .omitted, time: .shortened)).")
        }
        return MuxaAgentNotification(
            identifier: "usage.\(alert.key)",
            title: String(localized: "\(provider) has used \(percent)% of its \(window)"),
            subtitle: alert.hostAliases.joined(separator: ", "),
            body: reset,
            pane: nil,
            sound: sound && alert.threshold >= urgent,
            category: nil
        )
    }

    /// Announced keys with when they were announced, pruned after eight
    /// days: no window outlives a week.
    static func loadNotified(_ defaults: UserDefaults) -> [String: Date] {
        guard let data = defaults.data(forKey: notifiedKey),
              let stored = try? JSONDecoder().decode([String: Date].self, from: data) else { return [:] }
        return stored
    }

    static func saveNotified(_ notified: [String: Date], to defaults: UserDefaults, now: Date) {
        let kept = notified.filter { now.timeIntervalSince($0.value) < 8 * 24 * 3600 }
        if let data = try? JSONEncoder().encode(kept) { defaults.set(data, forKey: notifiedKey) }
    }
}
