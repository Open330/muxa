import Foundation
import Testing
@testable import Muxa

private let now = Date(timeIntervalSince1970: 1_790_000_000)

private func stamp(_ offset: TimeInterval) -> String {
    let formatter = ISO8601DateFormatter()
    return formatter.string(from: now.addingTimeInterval(offset))
}

private func usageAgent(_ fields: [String: Any], id: String = "s1", kind: String = "claude") throws -> MuxaAgent {
    var object: [String: Any] = ["kind": kind, "agent_session_id": id, "state": "working"]
    object.merge(fields) { _, new in new }
    return try JSONDecoder().decode(MuxaAgent.self, from: JSONSerialization.data(withJSONObject: object))
}

private func hosted(
    _ fields: [String: Any], id: String = "s1", kind: String = "claude", host: String = "local", local: Bool = true
) throws -> MuxaHostedAgent {
    MuxaHostedAgent(
        host: MuxaFleetHostIdentity(alias: host, local: local, state: "online", mode: "control"),
        agent: try usageAgent(fields, id: id, kind: kind),
        pane: nil
    )
}

@Test func usageFieldsDecodeAndStayOptional() throws {
    let agent = try usageAgent([
        "context_used_pct": 62.4, "cost_usd": 1.5,
        "rate_limit_5h_pct": 84.0, "rate_limit_5h_resets_at": "2026-09-23T07:40:00Z",
        "rate_limit_7d_pct": 31.0, "rate_limit_7d_resets_at": "2026-09-25T00:00:00Z",
        "rate_limited_until": "2026-09-23T05:05:00Z", "rate_limit_scope": "five_hour",
        "rate_limit_source": "statusline",
    ])
    #expect(agent.rateLimit5hPercent == 84)
    #expect(agent.rateLimit5hResetsAt == "2026-09-23T07:40:00Z")
    #expect(agent.rateLimit7dPercent == 31)
    #expect(agent.rateLimit7dResetsAt == "2026-09-25T00:00:00Z")
    #expect(agent.rateLimitedUntil == "2026-09-23T05:05:00Z")
    #expect(agent.rateLimitScope == "five_hour")
    #expect(agent.rateLimitSource == "statusline")

    let old = try usageAgent([:])
    #expect(old.rateLimit5hPercent == nil && old.rateLimitedUntil == nil && old.rateLimitScope == nil)
}

@Test func usageTimestampsAcceptDaemonPrecision() {
    let base = Date(timeIntervalSince1970: 1_790_000_000)
    #expect(MuxaUsageTimestamp.parse("2026-09-21T14:13:20Z") == base)
    #expect(MuxaUsageTimestamp.parse("2026-09-21T23:13:20+09:00") == base)
    #expect(MuxaUsageTimestamp.parse("2026-09-21T14:13:20.250Z") == base.addingTimeInterval(0.25))
    #expect(MuxaUsageTimestamp.parse("2026-09-21T14:13:20.250123456Z") == base.addingTimeInterval(0.25))
    #expect(MuxaUsageTimestamp.parse("garbage") == nil)
    #expect(MuxaUsageTimestamp.parse("") == nil)
    #expect(MuxaUsageTimestamp.parse(nil) == nil)
}

@Test func usageLevelThresholds() {
    #expect(MuxaUsageLevel(percent: 79.9) == .normal)
    #expect(MuxaUsageLevel(percent: 80) == .warning)
    #expect(MuxaUsageLevel(percent: 94.9) == .warning)
    #expect(MuxaUsageLevel(percent: 95) == .critical)
    #expect(MuxaUsageLevel(percent: 120) == .critical)
}

@Test func rateLimitCapFollowsTheCLIRule() throws {
    #expect(MuxaRateLimitCap.current(for: try usageAgent(["rate_limited_until": stamp(600)]), now: now) == nil)
    #expect(
        MuxaRateLimitCap.current(for: try usageAgent(["rate_limit_scope": "seven_day"]), now: now)
            == MuxaRateLimitCap(scope: .sevenDay, until: nil)
    )
    #expect(
        MuxaRateLimitCap.current(
            for: try usageAgent(["rate_limit_scope": "five_hour", "rate_limited_until": stamp(600)]), now: now
        ) == MuxaRateLimitCap(scope: .fiveHour, until: now.addingTimeInterval(600))
    )
    #expect(
        MuxaRateLimitCap.current(
            for: try usageAgent(["rate_limit_scope": "five_hour", "rate_limited_until": stamp(-1)]), now: now
        ) == nil
    )
    #expect(
        MuxaRateLimitCap.current(for: try usageAgent(["rate_limit_scope": "unknown"]), now: now)
            == MuxaRateLimitCap(scope: nil, until: nil)
    )
}

@Test func usageGroupsUseTheFreshestSamplePerAccount() throws {
    let groups = MuxaUsageGroup.groups(from: [
        try hosted([
            "last_activity_at": stamp(-600), "rate_limit_5h_pct": 90.0, "rate_limit_5h_resets_at": stamp(3600),
            "cost_usd": 1.25,
        ], id: "old"),
        try hosted([
            "last_activity_at": stamp(-10), "rate_limit_5h_pct": 40.0, "rate_limit_5h_resets_at": stamp(3600),
            "rate_limit_7d_pct": 20.0, "rate_limit_7d_resets_at": stamp(86_400), "cost_usd": 0.75,
        ], id: "new"),
        try hosted(["last_activity_at": stamp(-5), "context_used_pct": 10.0], id: "quiet"),
    ], now: now)
    #expect(groups.count == 1)
    let group = try #require(groups.first)
    #expect(group.provider == "claude")
    #expect(group.windows.map(\.kind) == [.fiveHour, .sevenDay])
    #expect(group.windows.map(\.percent) == [40, 20])
    #expect(group.windows.first?.resetsAt == now.addingTimeInterval(3600))
    #expect(group.liveCostUSD == 2)
    #expect(group.level == .normal)
    #expect(MuxaUsageFormat.statusText(group, now: now) == "Claude 5h 40% · 7d 20%")
}

@Test func usageGroupsSplitProvidersAndHostsAndOrderWorstFirst() throws {
    let groups = MuxaUsageGroup.groups(from: [
        try hosted(["rate_limit_5h_pct": 12.0], id: "c1"),
        try hosted(["rate_limit_7d_pct": 85.0], id: "x1", kind: "codex"),
        try hosted(["rate_limit_5h_pct": 50.0], id: "r1", host: "mini", local: false),
        try hosted(["rate_limit_5h_pct": 30.0], id: "r2", kind: "codex", host: "mini", local: false),
    ], now: now)
    #expect(groups.map(\.id) == [
        "local\u{1F}codex", "local\u{1F}claude", "mini\u{1F}claude", "mini\u{1F}codex",
    ])
    #expect(groups[0].level == .warning)
    #expect(groups[0].liveCostUSD == nil)
    #expect(MuxaUsageFormat.statusText(groups[0], now: now) == "Codex 7d 85%")
    #expect(MuxaUsageFormat.statusText(groups[2], now: now) == "mini · Claude 5h 50%")
}

@Test func usageGroupsDropRolledOverWindowsAndEmptyAccounts() throws {
    #expect(MuxaUsageGroup.groups(from: [], now: now).isEmpty)
    #expect(MuxaUsageGroup.groups(from: [try hosted(["context_used_pct": 40.0, "cost_usd": 3.0])], now: now).isEmpty)

    let rolled = MuxaUsageGroup.groups(from: [
        try hosted(["rate_limit_5h_pct": 99.0, "rate_limit_5h_resets_at": stamp(-60)]),
    ], now: now)
    #expect(rolled.isEmpty)

    let partial = MuxaUsageGroup.groups(from: [
        try hosted([
            "rate_limit_5h_pct": 99.0, "rate_limit_5h_resets_at": stamp(-60),
            "rate_limit_7d_pct": 10.0, "rate_limit_7d_resets_at": stamp(60),
        ]),
    ], now: now)
    #expect(partial.first?.windows.map(\.kind) == [.sevenDay])
}

@Test func cappedAgentsMakeTheGroupCritical() throws {
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = TimeZone(identifier: "UTC")!
    let locale = Locale(identifier: "en_GB")
    let groups = MuxaUsageGroup.groups(from: [
        try hosted(["rate_limit_5h_pct": 10.0], id: "calm", host: "mini", local: false),
        try hosted(
            ["rate_limit_scope": "five_hour", "rate_limited_until": stamp(1800), "rate_limit_5h_pct": 100.0],
            id: "capped", host: "mini", local: false
        ),
        try hosted(["rate_limit_5h_pct": 90.0], id: "local"),
    ], now: now)
    #expect(groups.map(\.hostAlias) == ["mini", "local"])
    let capped = groups[0]
    #expect(capped.level == .critical)
    #expect(capped.capped.map(\.id) == ["mini:capped"])
    #expect(capped.cappedUntil == now.addingTimeInterval(1800))
    // 1_790_000_000 is 14:13:20 UTC.
    #expect(MuxaUsageFormat.statusText(capped, now: now, calendar: calendar, locale: locale) == "mini · Claude limited · 14:43")

    let noTime = try #require(MuxaUsageGroup.groups(from: [try hosted(["rate_limit_scope": "unknown"])], now: now).first)
    #expect(noTime.windows.isEmpty)
    #expect(MuxaUsageFormat.statusText(noTime, now: now) == "Claude limited")
}

@Test func usageTimesReadLikeTheCLI() {
    #expect(MuxaUsageFormat.relative(until: now.addingTimeInterval(2 * 3600 + 4 * 60 + 5), now: now) == "in 2h 04m")
    #expect(MuxaUsageFormat.relative(until: now.addingTimeInterval(47 * 60), now: now) == "in 47m")
    #expect(MuxaUsageFormat.relative(until: now.addingTimeInterval(30), now: now) == "in 30s")
    #expect(MuxaUsageFormat.relative(until: now.addingTimeInterval(-5), now: now) == "now")

    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = TimeZone(identifier: "UTC")!
    let locale = Locale(identifier: "en_GB")
    #expect(MuxaUsageFormat.clock(now.addingTimeInterval(3600), now: now, calendar: calendar, locale: locale) == "15:13")
    // Two days on is a Wednesday, so the weekday is spelled out.
    let later = MuxaUsageFormat.clock(now.addingTimeInterval(2 * 86_400), now: now, calendar: calendar, locale: locale)
    #expect(later.contains("Wed") && later.contains("14:13"))

    let cap = MuxaRateLimitCap(scope: .fiveHour, until: now.addingTimeInterval(3600))
    #expect(MuxaUsageFormat.capText(cap, now: now, calendar: calendar, locale: locale) == "Limited until 15:13")
    #expect(MuxaUsageFormat.capText(MuxaRateLimitCap(scope: .sevenDay, until: nil), now: now) == "7d capped")
    #expect(MuxaUsageFormat.capText(MuxaRateLimitCap(scope: nil, until: nil), now: now) == "Rate limited")
}

@Test func paletteAgentSubtitleCarriesContextAndCap() throws {
    let until = ISO8601DateFormatter().string(from: Date.now.addingTimeInterval(3600))
    let pane: (String) -> [String: String] = { id in
        ["pane_id": id, "title": "", "session_id": "$1", "session": "demo", "window_id": "@1",
         "window_name": "impl", "window_index": "0", "pane_index": id, "current_command": "claude",
         "current_path": "/tmp", "socket": "default"]
    }
    let host = try JSONDecoder().decode(MuxaFleetHost.self, from: JSONSerialization.data(withJSONObject: [
        "alias": "local", "local": true, "mode": "control", "state": "online",
        "remote": [
            "agents": [
                ["kind": "claude", "agent_session_id": "a1", "state": "working", "pane": "%1",
                 "tmux_socket": "default", "context_used_pct": 62.0,
                 "rate_limit_scope": "five_hour", "rate_limited_until": until],
                ["kind": "claude", "agent_session_id": "a2", "state": "idle", "pane": "%2",
                 "tmux_socket": "default"],
            ],
            "panes": [pane("%1"), pane("%2")],
        ],
    ]))
    let items = MuxaPaletteItems.agents(watchHosts: MuxaExecutionSnapshot(hosts: [host]).watchHosts)
    #expect(items.count == 2)
    let capped = try #require(items.first { $0.subtitle.contains("%1") })
    #expect(capped.subtitle.contains("ctx 62%"))
    #expect(capped.subtitle.contains("limited until"))
    let quiet = try #require(items.first { $0.subtitle.contains("%2") })
    #expect(!quiet.subtitle.contains("ctx"))
    #expect(!quiet.subtitle.contains("limited"))
}
