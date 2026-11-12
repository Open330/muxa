import AppKit
import SwiftUI

// MARK: - Status bar

/// The status bar's usage items: one per rate-limit account that reports
/// anything, worst first. Rendered under a periodic timeline so a lifted cap
/// or a rolled-over window drops out without waiting for the next refresh.
struct TendUsageStatusItems: View {
    let agents: [TendHostedAgent]
    let openPane: (TendWatchPaneIdentity) -> Void

    /// More than this and the rest fold into a `+N` item; the status bar is
    /// shared with the connection and fleet counts.
    private static let inlineLimit = 2

    @State private var showsPopover = false

    var body: some View {
        TimelineView(.periodic(from: .now, by: 30)) { context in
            let groups = TendUsageGroup.groups(from: agents, now: context.date)
            if !groups.isEmpty {
                Button {
                    showsPopover.toggle()
                } label: {
                    HStack(spacing: 10) {
                        ForEach(groups.prefix(Self.inlineLimit)) { group in
                            Label(
                                TendUsageFormat.statusText(group, now: context.date),
                                systemImage: "gauge.with.dots.needle.67percent"
                            )
                            .foregroundStyle(group.level.tint(normal: .secondary))
                        }
                        if groups.count > Self.inlineLimit {
                            Text(verbatim: "+\(groups.count - Self.inlineLimit)")
                                .foregroundStyle(groups.dropFirst(Self.inlineLimit).map(\.level).max()?.tint(normal: .secondary) ?? .secondary)
                        }
                    }
                    .labelStyle(TendUsageStatusLabelStyle())
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Usage and rate limits")
                .popover(isPresented: $showsPopover, arrowEdge: .top) {
                    TendUsagePopover(
                        groups: groups,
                        hostCosts: TendUsageHostCost.summaries(from: agents),
                        now: context.date
                    ) { id in
                        showsPopover = false
                        openPane(id)
                    }
                }
            }
        }
    }
}

private struct TendUsageStatusLabelStyle: LabelStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: 4) {
            configuration.icon.font(.system(size: 11))
            configuration.title.monospacedDigit()
        }
    }
}

// MARK: - Popover

/// Every account's windows with their resets, the live sessions' cost, and
/// which agents are capped until when. With agents on several hosts, an
/// account shared by them shows once, and the live cost is split by host.
struct TendUsagePopover: View {
    let groups: [TendUsageGroup]
    var hostCosts: [TendUsageHostCost] = []
    let now: Date
    let openPane: (TendWatchPaneIdentity) -> Void

    @ObservedObject private var automations = AutomationStore.shared
    @EnvironmentObject private var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Usage")
                .font(.headline)
            ForEach(groups) { group in
                groupCard(group)
            }
            if hostCosts.count > 1 {
                hostCostCard
            }
            if groups.contains(where: { !$0.capped.isEmpty }) {
                autoResumeFooter
            }
        }
        .padding(14)
        .frame(width: 360, alignment: .leading)
    }

    private func groupCard(_ group: TendUsageGroup) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text(verbatim: "\(group.providerName) · \(group.hostAliases.joined(separator: ", "))")
                    .font(.system(size: 12, weight: .semibold))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 8)
                if let cost = group.liveCostUSD {
                    Text("\(cost.formatted(.currency(code: "USD"))) live sessions")
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
            if group.isShared {
                Text("Same account on \(group.hostAliases.count) hosts · freshest reading shown")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                    .help("These hosts report the same reset times, so they share one account's limits.")
            }
            ForEach(group.windows, id: \.kind) { window in
                windowRow(window)
            }
            if !group.capped.isEmpty {
                HStack {
                    Text("Rate limited")
                        .font(TendType.detail.weight(.semibold))
                        .foregroundStyle(.purple)
                    Spacer(minLength: 8)
                    TendSwitchLimitedButton(model: model, limited: group.capped.map(\.agent))
                        .controlSize(.small)
                }
                .padding(.top, 2)
                ForEach(group.capped) { capped in
                    cappedRow(capped, showsHost: group.isShared)
                }
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous))
    }

    private func windowRow(_ window: TendUsageWindow) -> some View {
        HStack(spacing: 8) {
            Text(verbatim: window.kind.shortLabel)
                .font(TendType.detail.monospaced())
                .foregroundStyle(.secondary)
                .frame(width: 20, alignment: .leading)
            TendUsageBar(percent: window.percent, width: 110, height: 6)
            Text(verbatim: TendUsageFormat.percent(window.percent))
                .font(TendType.detail.monospacedDigit())
                .foregroundStyle(window.level.tint(normal: .primary))
                .frame(width: 36, alignment: .trailing)
            if let reset = window.resetsAt {
                Text("resets \(TendUsageFormat.clock(reset, now: now))")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .help(TendUsageFormat.relative(until: reset, now: now))
            }
            Spacer(minLength: 0)
        }
    }

    private func cappedRow(_ capped: TendUsageGroup.CappedAgent, showsHost: Bool) -> some View {
        let paneID = capped.agent.pane.map {
            TendWatchPaneIdentity(hostAlias: capped.agent.host.alias, socket: $0.endpointSocket, paneID: $0.paneID)
        }
        return Button {
            if let paneID { openPane(paneID) }
        } label: {
            HStack(spacing: 6) {
                Text(verbatim: showsHost
                    ? "\(TendUsageFormat.agentTitle(capped.agent)) · \(capped.agent.host.alias)"
                    : TendUsageFormat.agentTitle(capped.agent))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 8)
                Text(verbatim: capText(capped.cap))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                if paneID != nil {
                    Image(systemName: "arrow.right")
                        .font(.system(size: 9, weight: .semibold))
                        .foregroundStyle(.tertiary)
                }
            }
            .font(TendType.detail)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(paneID == nil)
    }

    /// Live-session cost per host and in total. Each session runs on one
    /// host, so the total counts none twice even when hosts share an account.
    private var hostCostCard: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Live sessions by host")
                .font(.system(size: 12, weight: .semibold))
            ForEach(hostCosts) { host in
                costRow(Text(verbatim: host.hostAlias), cost: host.liveCostUSD, sessions: host.sessionCount)
            }
            Divider()
            costRow(
                Text("Total").fontWeight(.semibold),
                cost: hostCosts.reduce(0) { $0 + $1.liveCostUSD },
                sessions: hostCosts.reduce(0) { $0 + $1.sessionCount }
            )
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous))
    }

    private func costRow(_ label: Text, cost: Double, sessions: Int) -> some View {
        HStack(spacing: 8) {
            label
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer(minLength: 8)
            Text("\(sessions) sessions")
                .foregroundStyle(.secondary)
            Text(verbatim: cost.formatted(.currency(code: "USD")))
                .monospacedDigit()
                .frame(minWidth: 56, alignment: .trailing)
        }
        .font(TendType.detail)
    }

    private func capText(_ cap: TendRateLimitCap) -> String {
        guard let until = cap.until else { return TendUsageFormat.capText(cap, now: now) }
        return "\(TendUsageFormat.capText(cap, now: now)) (\(TendUsageFormat.relative(until: until, now: now)))"
    }

    private var autoResumeFooter: some View {
        HStack {
            TendOpenSettingsButton(tab: .accounts) {
                Text("Limit Policy…")
            }
            .buttonStyle(.tendGhost)
            Spacer(minLength: 8)
            // Only reflect what is already loaded: the status bar should not
            // start an automation IPC round-trip just to label a button.
            if let policy = automations.limitPolicy {
                Label(policy.title, systemImage: policy.systemImage)
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
    }
}

// MARK: - Meters and badges

/// A thin capsule filled to `percent`, coloured by how close it is to full.
struct TendUsageBar: View {
    let percent: Double
    var width: CGFloat = 28
    var height: CGFloat = 4

    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        let fraction = min(max(percent / 100, 0), 1)
        ZStack(alignment: .leading) {
            Capsule().fill(TendTheme.segmentTrack(colorScheme))
            Capsule()
                .fill(TendUsageLevel(percent: percent).tint(normal: .accentColor))
                .frame(width: width * fraction)
        }
        .frame(width: width, height: height)
    }
}

/// An agent's context-window fill: a small bar and `ctx 62%`.
struct TendContextMeter: View {
    let percent: Double

    var body: some View {
        HStack(spacing: 5) {
            TendUsageBar(percent: percent)
            Text("ctx \(TendUsageFormat.percent(percent))")
                .monospacedDigit()
                .foregroundStyle(TendUsageLevel(percent: percent).tint(normal: .secondary))
        }
        .font(TendType.meta)
        .lineLimit(1)
        .fixedSize()
        .help("Context window \(TendUsageFormat.percent(percent)) used")
    }
}

/// `Limited until 14:05` on a capped agent. Clicking it offers what can be
/// done now (switch the agent to another account) and what Tend should do
/// next time (the limit policy).
struct TendRateLimitBadge: View {
    @EnvironmentObject private var model: AppModel
    let agent: TendAgent
    /// Where the agent runs, so the popover can act on it; nil shows only
    /// the explanation and the policy.
    var target: TendAgentActionTarget? = nil

    @State private var showsDetails = false

    var body: some View {
        TimelineView(.periodic(from: .now, by: 30)) { context in
            if let cap = TendRateLimitCap.current(for: agent, now: context.date) {
                Button {
                    showsDetails.toggle()
                } label: {
                    Label(TendUsageFormat.capText(cap, now: context.date), systemImage: "hourglass")
                        .font(TendType.meta.weight(.medium))
                        .foregroundStyle(.purple)
                        .lineLimit(1)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(Color.purple.opacity(0.14), in: Capsule())
                        .contentShape(Capsule())
                }
                .buttonStyle(.plain)
                .fixedSize()
                .help(cap.until.map { TendUsageFormat.relative(until: $0, now: context.date) } ?? "")
                .popover(isPresented: $showsDetails, arrowEdge: .bottom) {
                    details(cap, now: context.date)
                }
            }
        }
    }

    private func details(_ cap: TendRateLimitCap, now: Date) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Rate limited")
                .font(.headline)
            if let until = cap.until {
                Text("The limit resets at \(TendUsageFormat.clock(until, now: now)) (\(TendUsageFormat.relative(until: until, now: now))).")
            } else if let scope = cap.scope {
                Text("The \(scope.shortLabel) window is used up; no reset time was reported.")
            } else {
                Text("No reset time was reported.")
            }
            if let target, target.canSwitchAccount {
                TendAgentPrimaryActionButton(model: model, target: target)
                    .simultaneousGesture(TapGesture().onEnded { showsDetails = false })
            }
            Divider()
            TendLimitPolicyPicker(model: model, compact: true)
            TendOpenSettingsButton(tab: .automations) {
                Text("More in Automations…")
            }
            .buttonStyle(.tendGhost)
        }
        .font(.callout)
        .padding(14)
        .frame(width: 320, alignment: .leading)
    }
}

/// The pane breadcrumb bar's usage accessory: the context meter and the cap
/// badge, both absent when the agent reports neither.
struct TendPaneUsageAccessory: View {
    let agent: TendAgent?
    var target: TendAgentActionTarget? = nil

    var body: some View {
        if let agent {
            HStack(spacing: 8) {
                TendRateLimitBadge(agent: agent, target: target)
                if let context = agent.contextUsedPercent {
                    TendContextMeter(percent: context)
                }
            }
        }
    }
}

// MARK: - Settings

/// Opens Settings on a chosen tab: the `openSettings` action on macOS 14,
/// the AppKit selector on 13. A bare `SettingsLink` reopens whichever tab
/// was shown last.
struct TendOpenSettingsButton<Label: View>: View {
    let tab: TendSettingsTab
    @ViewBuilder let label: Label

    var body: some View {
        if #available(macOS 14.0, *) {
            TendOpenSettingsModernButton(tab: tab) { label }
        } else {
            Button {
                TendSettingsOpener.select(tab)
                TendSettingsOpener.openLegacySettingsWindow()
            } label: {
                label
            }
        }
    }
}

@available(macOS 14.0, *)
private struct TendOpenSettingsModernButton<Label: View>: View {
    @Environment(\.openSettings) private var openSettings
    let tab: TendSettingsTab
    @ViewBuilder let label: Label

    var body: some View {
        Button {
            TendSettingsOpener.select(tab)
            openSettings()
            NSApp.activate(ignoringOtherApps: true)
        } label: {
            label
        }
    }
}

extension TendUsageFormat {
    /// How the usage popover names an agent: its pane alias, Claude's
    /// session title, or the agent kind.
    static func agentTitle(_ agent: TendHostedAgent) -> String {
        agent.pane?.agentAlias.map { "@\($0)" }
            ?? agent.agent.aiTitle
            ?? providerName(agent.agent.kind)
    }
}
