import AppKit
import SwiftUI

// MARK: - Status bar

/// The status bar's usage items: one per rate-limit account that reports
/// anything, worst first. Rendered under a periodic timeline so a lifted cap
/// or a rolled-over window drops out without waiting for the next refresh.
struct MuxaUsageStatusItems: View {
    let agents: [MuxaHostedAgent]
    let openPane: (MuxaWatchPaneIdentity) -> Void

    /// More than this and the rest fold into a `+N` item; the status bar is
    /// shared with the connection and fleet counts.
    private static let inlineLimit = 2

    @State private var showsPopover = false

    var body: some View {
        TimelineView(.periodic(from: .now, by: 30)) { context in
            let groups = MuxaUsageGroup.groups(from: agents, now: context.date)
            if !groups.isEmpty {
                Button {
                    showsPopover.toggle()
                } label: {
                    HStack(spacing: 10) {
                        ForEach(groups.prefix(Self.inlineLimit)) { group in
                            Label(
                                MuxaUsageFormat.statusText(group, now: context.date),
                                systemImage: "gauge.with.dots.needle.67percent"
                            )
                            .foregroundStyle(group.level.tint(normal: .secondary))
                        }
                        if groups.count > Self.inlineLimit {
                            Text(verbatim: "+\(groups.count - Self.inlineLimit)")
                                .foregroundStyle(groups.dropFirst(Self.inlineLimit).map(\.level).max()?.tint(normal: .secondary) ?? .secondary)
                        }
                    }
                    .labelStyle(MuxaUsageStatusLabelStyle())
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Usage and rate limits")
                .popover(isPresented: $showsPopover, arrowEdge: .top) {
                    MuxaUsagePopover(groups: groups, now: context.date) { id in
                        showsPopover = false
                        openPane(id)
                    }
                }
            }
        }
    }
}

private struct MuxaUsageStatusLabelStyle: LabelStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: 4) {
            configuration.icon.font(.system(size: 10))
            configuration.title.monospacedDigit()
        }
    }
}

// MARK: - Popover

/// Every account's windows with their resets, the live sessions' cost, and
/// which agents are capped until when.
struct MuxaUsagePopover: View {
    let groups: [MuxaUsageGroup]
    let now: Date
    let openPane: (MuxaWatchPaneIdentity) -> Void

    @ObservedObject private var automations = AutomationStore.shared

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Usage")
                .font(.headline)
            ForEach(groups) { group in
                groupCard(group)
            }
            if groups.contains(where: { !$0.capped.isEmpty }) {
                autoResumeFooter
            }
        }
        .padding(14)
        .frame(width: 360, alignment: .leading)
    }

    private func groupCard(_ group: MuxaUsageGroup) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text(verbatim: "\(group.providerName) · \(group.hostAlias)")
                    .font(.system(size: 12, weight: .semibold))
                Spacer(minLength: 8)
                if let cost = group.liveCostUSD {
                    Text("\(cost.formatted(.currency(code: "USD"))) live sessions")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
            ForEach(group.windows, id: \.kind) { window in
                windowRow(window)
            }
            if !group.capped.isEmpty {
                Text("Rate limited")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.red)
                    .padding(.top, 2)
                ForEach(group.capped) { capped in
                    cappedRow(capped)
                }
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
    }

    private func windowRow(_ window: MuxaUsageWindow) -> some View {
        HStack(spacing: 8) {
            Text(verbatim: window.kind.shortLabel)
                .font(.caption.monospaced())
                .foregroundStyle(.secondary)
                .frame(width: 20, alignment: .leading)
            MuxaUsageBar(percent: window.percent, width: 110, height: 6)
            Text(verbatim: MuxaUsageFormat.percent(window.percent))
                .font(.caption.monospacedDigit())
                .foregroundStyle(window.level.tint(normal: .primary))
                .frame(width: 36, alignment: .trailing)
            if let reset = window.resetsAt {
                Text("resets \(MuxaUsageFormat.clock(reset, now: now))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .help(MuxaUsageFormat.relative(until: reset, now: now))
            }
            Spacer(minLength: 0)
        }
    }

    private func cappedRow(_ capped: MuxaUsageGroup.CappedAgent) -> some View {
        let paneID = capped.agent.pane.map {
            MuxaWatchPaneIdentity(hostAlias: capped.agent.host.alias, socket: $0.endpointSocket, paneID: $0.paneID)
        }
        return Button {
            if let paneID { openPane(paneID) }
        } label: {
            HStack(spacing: 6) {
                Text(verbatim: MuxaUsageFormat.agentTitle(capped.agent))
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
            .font(.caption)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(paneID == nil)
    }

    private func capText(_ cap: MuxaRateLimitCap) -> String {
        guard let until = cap.until else { return MuxaUsageFormat.capText(cap, now: now) }
        return "\(MuxaUsageFormat.capText(cap, now: now)) (\(MuxaUsageFormat.relative(until: until, now: now)))"
    }

    private var autoResumeFooter: some View {
        HStack {
            MuxaOpenSettingsButton(tab: .automations) {
                Text("Auto-Resume…")
            }
            .buttonStyle(.muxaGhost)
            Spacer(minLength: 8)
            // Only reflect what is already loaded: the status bar should not
            // start an automation IPC round-trip just to label a button.
            if automations.hasLoaded, automations.isSupported {
                Text(autoResumeConfigured ? "On" : "Not set up")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var autoResumeConfigured: Bool {
        automations.masterEnabled
            && automations.rules.contains { $0.enabled && $0.on == .rateLimited }
    }
}

// MARK: - Meters and badges

/// A thin capsule filled to `percent`, coloured by how close it is to full.
struct MuxaUsageBar: View {
    let percent: Double
    var width: CGFloat = 28
    var height: CGFloat = 4

    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        let fraction = min(max(percent / 100, 0), 1)
        ZStack(alignment: .leading) {
            Capsule().fill(MuxaTheme.segmentTrack(colorScheme))
            Capsule()
                .fill(MuxaUsageLevel(percent: percent).tint(normal: .accentColor))
                .frame(width: width * fraction)
        }
        .frame(width: width, height: height)
    }
}

/// An agent's context-window fill: a small bar and `ctx 62%`.
struct MuxaContextMeter: View {
    let percent: Double

    var body: some View {
        HStack(spacing: 5) {
            MuxaUsageBar(percent: percent)
            Text("ctx \(MuxaUsageFormat.percent(percent))")
                .monospacedDigit()
                .foregroundStyle(MuxaUsageLevel(percent: percent).tint(normal: .secondary))
        }
        .font(.caption2)
        .lineLimit(1)
        .fixedSize()
        .help("Context window \(MuxaUsageFormat.percent(percent)) used")
    }
}

/// `Limited until 14:05` on a capped agent. Clicking it explains that an
/// automation can resume the agent and opens Settings › Automations.
struct MuxaRateLimitBadge: View {
    let agent: MuxaAgent

    @State private var showsDetails = false

    var body: some View {
        TimelineView(.periodic(from: .now, by: 30)) { context in
            if let cap = MuxaRateLimitCap.current(for: agent, now: context.date) {
                Button {
                    showsDetails.toggle()
                } label: {
                    Label(MuxaUsageFormat.capText(cap, now: context.date), systemImage: "hourglass")
                        .font(.caption2.weight(.medium))
                        .foregroundStyle(.red)
                        .lineLimit(1)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(Color.red.opacity(0.14), in: Capsule())
                        .contentShape(Capsule())
                }
                .buttonStyle(.plain)
                .fixedSize()
                .help(cap.until.map { MuxaUsageFormat.relative(until: $0, now: context.date) } ?? "")
                .popover(isPresented: $showsDetails, arrowEdge: .bottom) {
                    details(cap, now: context.date)
                }
            }
        }
    }

    private func details(_ cap: MuxaRateLimitCap, now: Date) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Rate limited")
                .font(.headline)
            if let until = cap.until {
                Text("The limit resets at \(MuxaUsageFormat.clock(until, now: now)) (\(MuxaUsageFormat.relative(until: until, now: now))).")
            } else if let scope = cap.scope {
                Text("The \(scope.shortLabel) window is used up; no reset time was reported.")
            } else {
                Text("No reset time was reported.")
            }
            Text("An automation can resume this agent when the limit resets.")
                .foregroundStyle(.secondary)
            MuxaOpenSettingsButton(tab: .automations) {
                Text("Set Up Auto-Resume…")
            }
            .buttonStyle(.muxaSecondary)
        }
        .font(.callout)
        .padding(14)
        .frame(width: 300, alignment: .leading)
    }
}

/// The pane breadcrumb bar's usage accessory: the context meter and the cap
/// badge, both absent when the agent reports neither.
struct MuxaPaneUsageAccessory: View {
    let agent: MuxaAgent?

    var body: some View {
        if let agent {
            HStack(spacing: 8) {
                MuxaRateLimitBadge(agent: agent)
                if let context = agent.contextUsedPercent {
                    MuxaContextMeter(percent: context)
                }
            }
        }
    }
}

// MARK: - Settings

/// Opens Settings on a chosen tab: the `openSettings` action on macOS 14,
/// the AppKit selector on 13. A bare `SettingsLink` reopens whichever tab
/// was shown last.
struct MuxaOpenSettingsButton<Label: View>: View {
    let tab: MuxaSettingsTab
    @ViewBuilder let label: Label

    var body: some View {
        if #available(macOS 14.0, *) {
            MuxaOpenSettingsModernButton(tab: tab) { label }
        } else {
            Button {
                MuxaSettingsOpener.select(tab)
                MuxaSettingsOpener.openLegacySettingsWindow()
            } label: {
                label
            }
        }
    }
}

@available(macOS 14.0, *)
private struct MuxaOpenSettingsModernButton<Label: View>: View {
    @Environment(\.openSettings) private var openSettings
    let tab: MuxaSettingsTab
    @ViewBuilder let label: Label

    var body: some View {
        Button {
            MuxaSettingsOpener.select(tab)
            openSettings()
            NSApp.activate(ignoringOtherApps: true)
        } label: {
            label
        }
    }
}

extension MuxaUsageFormat {
    /// How the usage popover names an agent: its pane alias, Claude's
    /// session title, or the agent kind.
    static func agentTitle(_ agent: MuxaHostedAgent) -> String {
        agent.pane?.agentAlias.map { "@\($0)" }
            ?? agent.agent.aiTitle
            ?? providerName(agent.agent.kind)
    }
}
