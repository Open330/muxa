import Foundation
import SwiftUI

// Usage, quota and context figures the daemon already reports per agent:
// Claude's statusline heartbeat and Codex's rollout carry 5-hour and 7-day
// rate-limit utilisation with reset times, a `RateLimited` event pins a cap,
// and Claude reports how full its context window is. Everything here is pure
// so the views stay thin and the rules can be tested without a daemon.

/// RFC 3339 timestamps as muxad writes them. The daemon's `time` crate emits
/// up to nine fractional digits, which `ISO8601DateFormatter` refuses, so the
/// fraction is cut to milliseconds before parsing.
enum MuxaUsageTimestamp {
    static func parse(_ text: String?) -> Date? {
        guard var text = text?.trimmingCharacters(in: .whitespacesAndNewlines), !text.isEmpty else {
            return nil
        }
        if let dot = text.firstIndex(of: "."), text[..<dot].contains("T") {
            let digits = text[text.index(after: dot)...].prefix(while: \.isNumber)
            if digits.count > 3 {
                text.replaceSubrange(digits.startIndex..<digits.endIndex, with: digits.prefix(3))
            }
        }
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = formatter.date(from: text) { return date }
        formatter.formatOptions = [.withInternetDateTime]
        return formatter.date(from: text)
    }
}

/// How close a figure is to its ceiling. The thresholds match the CLI's
/// LIMITS column (yellow from 80%), with a second step so a nearly spent
/// window reads as urgently as an actual cap.
enum MuxaUsageLevel: Int, Comparable, Sendable {
    case normal
    case warning
    case critical

    init(percent: Double) {
        switch percent {
        case 95...: self = .critical
        case 80...: self = .warning
        default: self = .normal
        }
    }

    static func < (lhs: Self, rhs: Self) -> Bool { lhs.rawValue < rhs.rawValue }

    /// Text colour; `normal` is whatever the surrounding chrome uses.
    func tint(normal: Color) -> Color {
        switch self {
        case .normal: normal
        case .warning: .orange
        case .critical: .red
        }
    }
}

enum MuxaUsageWindowKind: String, Sendable {
    case fiveHour = "five_hour"
    case sevenDay = "seven_day"

    var shortLabel: String {
        switch self {
        case .fiveHour: "5h"
        case .sevenDay: "7d"
        }
    }
}

struct MuxaUsageWindow: Hashable, Sendable {
    let kind: MuxaUsageWindowKind
    let percent: Double
    let resetsAt: Date?
    /// When the reporting agent was last active, which is when the figure
    /// was sampled; picks the freshest reading of an account across hosts.
    var sampledAt: Date? = nil

    var level: MuxaUsageLevel { MuxaUsageLevel(percent: percent) }
}

/// A rate-limit cap the agent is under right now.
struct MuxaRateLimitCap: Hashable, Sendable {
    /// Which window hit its ceiling; nil when the source didn't say.
    let scope: MuxaUsageWindowKind?
    /// When the cap lifts; nil when the source (a 429 `StopFailure`) carried
    /// no reset time.
    let until: Date?

    /// Mirrors the CLI's `is_currently_capped`: every `RateLimited` event
    /// sets the scope, so the scope is what marks a cap, and a known reset
    /// time in the past means the cap has lifted even though the daemon
    /// only clears the fields on the agent's next start.
    static func current(for agent: MuxaAgent, now: Date = .now) -> MuxaRateLimitCap? {
        guard let scopeText = agent.rateLimitScope else { return nil }
        let until = MuxaUsageTimestamp.parse(agent.rateLimitedUntil)
        if let until, until <= now { return nil }
        return MuxaRateLimitCap(scope: MuxaUsageWindowKind(rawValue: scopeText), until: until)
    }
}

/// One rate-limit account: every live agent of one provider on one host,
/// merged across hosts when their readings show it is the same account.
///
/// The daemon reports no account id, so a host is assumed to hold one
/// account per provider, and two Claude accounts on one host (different
/// `CLAUDE_CONFIG_DIR`s) merge into one group showing the freshest sample.
/// Across hosts the reset instants are the account's fingerprint (see
/// `MuxaUsageAccountMatch`). Limits are never summed: a merged account
/// shows the freshest reading any of its hosts has.
struct MuxaUsageGroup: Identifiable, Sendable {
    struct CappedAgent: Identifiable, Sendable {
        let agent: MuxaHostedAgent
        let cap: MuxaRateLimitCap

        var id: String { agent.id }
    }

    /// Every host whose sessions report this account: the local host first,
    /// then by name. More than one only when their readings matched.
    let hostAliases: [String]
    let hostIsLocal: Bool
    let provider: String
    let windows: [MuxaUsageWindow]
    /// Summed `cost_usd` of the group's live sessions on all its hosts;
    /// stopped agents are gone from the snapshot, so this is not a daily
    /// total. Each session lives on one host, so none is counted twice.
    let liveCostUSD: Double?
    let capped: [CappedAgent]

    var id: String { "\(hostAliases.joined(separator: "+"))\u{1F}\(provider)" }

    var providerName: String { MuxaUsageFormat.providerName(provider) }

    /// The host that names the group where one name must do.
    var hostAlias: String { hostAliases.first ?? "" }

    /// The same account seen from more than one host.
    var isShared: Bool { hostAliases.count > 1 }

    var level: MuxaUsageLevel {
        if !capped.isEmpty { return .critical }
        return windows.map(\.level).max() ?? .normal
    }

    /// The latest known end of the group's caps: the account is free only
    /// once every capped session is.
    var cappedUntil: Date? {
        capped.compactMap(\.cap.until).max()
    }

    /// Builds the groups worth showing, worst first, then the local host,
    /// then by name. A group needs a live window or a cap; agents that
    /// report nothing produce no group at all, so the UI can hide instead of
    /// showing zeros.
    static func groups(from agents: [MuxaHostedAgent], now: Date = .now) -> [MuxaUsageGroup] {
        let byHost = Dictionary(grouping: agents) { AccountKey(host: $0.host.alias, provider: $0.agent.kind) }
        let readings = byHost.compactMap { key, members -> MuxaUsageHostReading? in
            let windows = [
                window(.fiveHour, in: members, now: now, percent: \.rateLimit5hPercent, resetsAt: \.rateLimit5hResetsAt),
                window(.sevenDay, in: members, now: now, percent: \.rateLimit7dPercent, resetsAt: \.rateLimit7dResetsAt),
            ].compactMap { $0 }
            let capped = members.compactMap { member in
                MuxaRateLimitCap.current(for: member.agent, now: now).map { CappedAgent(agent: member, cap: $0) }
            }
            guard !windows.isEmpty || !capped.isEmpty else { return nil }
            let costs = members.compactMap(\.agent.costUSD)
            return MuxaUsageHostReading(
                hostAlias: key.host,
                hostIsLocal: members.contains { $0.host.local },
                provider: key.provider,
                windows: windows,
                liveCostUSD: costs.isEmpty ? nil : costs.reduce(0, +),
                capped: capped
            )
        }
        let groups = Dictionary(grouping: readings, by: \.provider).values.flatMap { providerReadings in
            MuxaUsageAccountMatch.accounts(providerReadings).map(merged)
        }
        return groups.sorted { lhs, rhs in
            if lhs.level != rhs.level { return lhs.level > rhs.level }
            if lhs.hostIsLocal != rhs.hostIsLocal { return lhs.hostIsLocal }
            if lhs.hostAlias != rhs.hostAlias { return lhs.hostAlias < rhs.hostAlias }
            return lhs.providerName < rhs.providerName
        }
    }

    private struct AccountKey: Hashable {
        let host: String
        let provider: String
    }

    /// One account's readings from its hosts as one group: per window the
    /// freshest sample, every cap, and the cost of every live session.
    private static func merged(_ readings: [MuxaUsageHostReading]) -> MuxaUsageGroup {
        let ordered = readings.sorted { lhs, rhs in
            if lhs.hostIsLocal != rhs.hostIsLocal { return lhs.hostIsLocal }
            return lhs.hostAlias < rhs.hostAlias
        }
        let windows = [MuxaUsageWindowKind.fiveHour, .sevenDay].compactMap { kind in
            ordered.compactMap { reading in reading.windows.first { $0.kind == kind } }
                .max { ($0.sampledAt ?? .distantPast) < ($1.sampledAt ?? .distantPast) }
        }
        let costs = ordered.compactMap(\.liveCostUSD)
        return MuxaUsageGroup(
            hostAliases: ordered.map(\.hostAlias),
            hostIsLocal: ordered.contains(where: \.hostIsLocal),
            provider: ordered.first?.provider ?? "",
            windows: windows,
            liveCostUSD: costs.isEmpty ? nil : costs.reduce(0, +),
            capped: ordered.flatMap(\.capped)
        )
    }

    /// The window as the freshest reporting agent saw it. Taking the maximum
    /// instead would let an old session that stopped heartbeating pin a
    /// stale high figure. A window whose reset has passed has rolled over,
    /// so its figure no longer says anything and it is left out.
    private static func window(
        _ kind: MuxaUsageWindowKind,
        in members: [MuxaHostedAgent],
        now: Date,
        percent: KeyPath<MuxaAgent, Double?>,
        resetsAt: KeyPath<MuxaAgent, String?>
    ) -> MuxaUsageWindow? {
        let freshest = members
            .filter { $0.agent[keyPath: percent] != nil }
            .max { activity($0) < activity($1) }
        guard let agent = freshest?.agent, let value = agent[keyPath: percent] else { return nil }
        let reset = MuxaUsageTimestamp.parse(agent[keyPath: resetsAt])
        if let reset, reset <= now { return nil }
        return MuxaUsageWindow(
            kind: kind,
            percent: value,
            resetsAt: reset,
            sampledAt: MuxaUsageTimestamp.parse(agent.lastActivityAt)
        )
    }

    private static func activity(_ member: MuxaHostedAgent) -> Date {
        MuxaUsageTimestamp.parse(member.agent.lastActivityAt) ?? .distantPast
    }
}

/// One provider's figures as one host reports them, before accounts are
/// matched across hosts.
struct MuxaUsageHostReading: Sendable {
    let hostAlias: String
    let hostIsLocal: Bool
    let provider: String
    let windows: [MuxaUsageWindow]
    let liveCostUSD: Double?
    let capped: [MuxaUsageGroup.CappedAgent]
}

/// Which hosts share one provider account, so the same Claude account
/// signed in on several Macs shows once instead of once per Mac.
///
/// Claude's statusline and Codex's rollout both carry each window's reset
/// instant as the provider computed it for the account, so hosts on one
/// account report the same instants and two accounts almost never do.
/// "Almost" matters for Claude, whose 5-hour resets can fall on the hour: a
/// shared on-the-minute 5-hour reset alone is not proof, so a match needs
/// the 7-day reset or a reset precise to the second. Every window both
/// hosts report must agree; one disagreement means different accounts.
enum MuxaUsageAccountMatch {
    /// Slack for a provider that derives the instant per request.
    static let tolerance: TimeInterval = 60

    static func sameAccount(_ lhs: MuxaUsageHostReading, _ rhs: MuxaUsageHostReading) -> Bool {
        guard lhs.provider == rhs.provider, lhs.hostAlias != rhs.hostAlias else { return false }
        var conclusive = false
        for left in lhs.windows {
            guard let leftReset = left.resetsAt,
                  let rightReset = rhs.windows.first(where: { $0.kind == left.kind })?.resetsAt else { continue }
            guard abs(leftReset.timeIntervalSince(rightReset)) <= tolerance else { return false }
            if left.kind == .sevenDay || !isWholeMinute(leftReset) { conclusive = true }
        }
        return conclusive
    }

    /// One provider's readings split into accounts, in input order. Matching
    /// is transitive: hosts A–B and B–C matching make one account of three.
    static func accounts(_ readings: [MuxaUsageHostReading]) -> [[MuxaUsageHostReading]] {
        var parent = Array(readings.indices)
        func root(_ index: Int) -> Int {
            var index = index
            while parent[index] != index { index = parent[index] }
            return index
        }
        for left in readings.indices {
            for right in readings.indices where right > left && sameAccount(readings[left], readings[right]) {
                let (leftRoot, rightRoot) = (root(left), root(right))
                if leftRoot != rightRoot { parent[max(leftRoot, rightRoot)] = min(leftRoot, rightRoot) }
            }
        }
        return Dictionary(grouping: readings.indices, by: root)
            .sorted { $0.key < $1.key }
            .map { $0.value.map { readings[$0] } }
    }

    private static func isWholeMinute(_ date: Date) -> Bool {
        date.timeIntervalSince1970.truncatingRemainder(dividingBy: 60) == 0
    }
}

/// What the live sessions on one host have cost so far, across providers.
struct MuxaUsageHostCost: Identifiable, Hashable, Sendable {
    let hostAlias: String
    let hostIsLocal: Bool
    let liveCostUSD: Double
    /// Live sessions that reported a cost.
    let sessionCount: Int

    var id: String { hostAlias }

    /// Hosts whose live sessions report a cost, the local host first, then
    /// by name. Each session counts once, on the host running it.
    static func summaries(from agents: [MuxaHostedAgent]) -> [MuxaUsageHostCost] {
        Dictionary(grouping: agents.filter { $0.agent.costUSD != nil }, by: \.host.alias)
            .map { alias, members in
                MuxaUsageHostCost(
                    hostAlias: alias,
                    hostIsLocal: members.contains { $0.host.local },
                    liveCostUSD: members.compactMap(\.agent.costUSD).reduce(0, +),
                    sessionCount: members.count
                )
            }
            .sorted { lhs, rhs in
                if lhs.hostIsLocal != rhs.hostIsLocal { return lhs.hostIsLocal }
                return lhs.hostAlias < rhs.hostAlias
            }
    }
}

enum MuxaUsageFormat {
    static func providerName(_ kind: String) -> String {
        switch kind {
        case "claude": "Claude"
        case "codex": "Codex"
        default: kind.replacingOccurrences(of: "_", with: " ").capitalized
        }
    }

    static func percent(_ value: Double) -> String {
        "\(Int(value.rounded()))%"
    }

    /// The status-bar text: `Claude 5h 62% · 7d 31%`, or `Claude limited ·
    /// 14:05` while capped. Remote hosts are named so two accounts with the
    /// same provider can be told apart; an account shared by several remote
    /// hosts is named by its first, `mini +1 · Claude …`.
    static func statusText(
        _ group: MuxaUsageGroup, now: Date = .now, calendar: Calendar = .current, locale: Locale = .current
    ) -> String {
        let host = group.isShared ? "\(group.hostAlias) +\(group.hostAliases.count - 1)" : group.hostAlias
        let prefix = group.hostIsLocal ? group.providerName : "\(host) · \(group.providerName)"
        if !group.capped.isEmpty {
            let limited = String(localized: "limited")
            guard let until = group.cappedUntil else { return "\(prefix) \(limited)" }
            return "\(prefix) \(limited) · \(clock(until, now: now, calendar: calendar, locale: locale))"
        }
        let windows = group.windows.map { "\($0.kind.shortLabel) \(percent($0.percent))" }
        return "\(prefix) \(windows.joined(separator: " · "))"
    }

    /// A wall-clock time, with the weekday when it isn't today: a 7-day
    /// reset at "09:00" would otherwise read as this morning.
    static func clock(_ date: Date, now: Date = .now, calendar: Calendar = .current, locale: Locale = .current) -> String {
        let formatter = DateFormatter()
        formatter.calendar = calendar
        formatter.timeZone = calendar.timeZone
        formatter.locale = locale
        formatter.setLocalizedDateFormatFromTemplate(calendar.isDate(date, inSameDayAs: now) ? "jmm" : "EEEjmm")
        return formatter.string(from: date)
    }

    /// The gap to `until`, the way the CLI renders it: `in 2h 04m`,
    /// `in 47m`, `in 30s`, or `now` once it has passed.
    static func relative(until: Date, now: Date = .now) -> String {
        let total = Int(until.timeIntervalSince(now))
        guard total > 0 else { return String(localized: "now") }
        let hours = total / 3_600
        let minutes = (total % 3_600) / 60
        if hours > 0 {
            let padded = String(format: "%02d", minutes)
            return String(localized: "in \(hours)h \(padded)m")
        }
        if minutes > 0 { return String(localized: "in \(minutes)m") }
        return String(localized: "in \(total)s")
    }

    /// The badge text for a cap: `Limited until 14:05`, `7d capped` when
    /// only the window is known, `Rate limited` when neither is.
    static func capText(
        _ cap: MuxaRateLimitCap, now: Date = .now, calendar: Calendar = .current, locale: Locale = .current
    ) -> String {
        if let until = cap.until {
            return String(localized: "Limited until \(clock(until, now: now, calendar: calendar, locale: locale))")
        }
        if let scope = cap.scope {
            return String(localized: "\(scope.shortLabel) capped")
        }
        return String(localized: "Rate limited")
    }

    /// What the ⌘J palette adds to an agent's subtitle: its context fill and
    /// any cap, so the agent list doubles as a quick usage scan.
    static func paletteFragments(for agent: MuxaAgent?, now: Date = .now) -> [String] {
        guard let agent else { return [] }
        var fragments: [String] = []
        if let context = agent.contextUsedPercent {
            fragments.append(String(localized: "ctx \(percent(context))"))
        }
        if let cap = MuxaRateLimitCap.current(for: agent, now: now) {
            fragments.append(
                cap.until.map { String(localized: "limited until \(clock($0, now: now))") }
                    ?? capText(cap, now: now)
            )
        }
        return fragments
    }
}
