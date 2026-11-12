import AppKit
import SwiftUI

private struct TendAttentionKey: EnvironmentKey {
    static let defaultValue: TendAgentAttentionCenter? = nil
}

extension EnvironmentValues {
    /// Optional so a row rendered outside the workbench (a detached module
    /// window, a test host) simply shows no unread dot instead of crashing
    /// on a missing environment object.
    var tendAttention: TendAgentAttentionCenter? {
        get { self[TendAttentionKey.self] }
        set { self[TendAttentionKey.self] = newValue }
    }
}

/// The accent dot an agent pane carries while it finished or started needing
/// the operator since they last looked — the editor's "unsaved" dot, reused
/// for "unseen".
struct TendUnreadDot: View {
    let pane: TendWatchPaneIdentity
    var size: CGFloat = 6
    @Environment(\.tendAttention) private var attention

    var body: some View {
        if let attention {
            TendUnreadDotContent(center: attention, pane: pane, size: size)
        }
    }
}

private struct TendUnreadDotContent: View {
    @ObservedObject var center: TendAgentAttentionCenter
    let pane: TendWatchPaneIdentity
    let size: CGFloat

    var body: some View {
        if center.isUnread(pane) {
            Circle()
                .fill(Color.accentColor)
                .frame(width: size, height: size)
                .help("Finished or waiting since you last looked")
                .accessibilityLabel("Unread")
        }
    }
}

/// Feeds the attention center what the workbench shows, and opens the pane a
/// notification click asked for.
struct TendAttentionTracking: ViewModifier {
    @ObservedObject var model: AppModel
    @ObservedObject var tabs: TendWorkbenchTabs
    @ObservedObject var attention: TendAgentAttentionCenter
    let open: (TendWatchPaneIdentity) -> Void
    @Environment(\.openWindow) private var openWindow

    func body(content: Content) -> some View {
        content
            .environment(\.tendAttention, attention)
            .onAppear {
                TendWorkbenchPresenter.remember(openWindow)
                updateVisibility()
                openPending()
            }
            .onReceive(tabs.$groups) { _ in DispatchQueue.main.async(execute: updateVisibility) }
            .onChange(of: model.workspaceRevision) { _ in
                updateVisibility()
                openPending()
                openPendingSwitch()
            }
            .onChange(of: attention.pendingHumanMessageID) { _ in openHumanPending() }
            .onChange(of: attention.pendingOpen) { _ in openPending() }
            .onChange(of: attention.pendingSwitch) { _ in openPendingSwitch() }
            .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
                updateVisibility()
            }
            .onReceive(NotificationCenter.default.publisher(for: NSApplication.didResignActiveNotification)) { _ in
                updateVisibility()
            }
            .onReceive(NotificationCenter.default.publisher(for: NSWindow.didChangeOcclusionStateNotification)) { _ in
                updateVisibility()
            }
    }

    /// A pane counts as looked at when it is the active tab of an editor
    /// group while Tend is frontmost and the workbench is on screen.
    private func updateVisibility() {
        let panes = Set(tabs.groups.compactMap(\.active).compactMap { selection -> TendWatchPaneIdentity? in
            switch selection {
            case .pane(let id): id
            case .agent(let id):
                model.hostedAgents.first { $0.id == id }.flatMap(TendAgentAttentionCenter.paneIdentity)
            default: nil
            }
        })
        let workbench = NSApp.windows.first { $0.identifier?.rawValue == "tend.main-workbench" }
        let onScreen = workbench.map { $0.occlusionState.contains(.visible) && !$0.isMiniaturized } ?? false
        attention.setVisible(panes: panes, appActive: NSApp.isActive && onScreen)
    }

    /// A click can arrive before the pane's host has reported in (the click
    /// that launched Tend); the request waits for the next snapshot.
    private func openHumanPending() {
        guard let id = attention.pendingHumanMessageID else { return }
        attention.pendingHumanMessageID = nil
        model.pendingOperatorMessageID = id
        // Open the Inbox editor, not just its side bar: the message is
        // shown there.
        model.select(.inbox)
    }

    private func openPendingSwitch() {
        guard let pane = attention.pendingSwitch else { return }
        guard let watch = model.executionSnapshot.watchPane(id: pane),
              let target = TendAgentActionTarget(watch) else {
            // A click can launch the app: give the pane the same grace a
            // plain open gets, then say what happened instead of nothing.
            if let at = attention.pendingSwitchAt,
               Date().timeIntervalSince(at) > TendAgentAttentionCenter.pendingOpenLifetime {
                attention.pendingSwitch = nil
                model.reportAgentActionError(String(localized: "That agent is no longer running, so there is nothing to switch."))
            }
            return
        }
        attention.pendingSwitch = nil
        open(pane)
        model.swaps.present(target.swapSheetTarget)
    }

    private func openPending() {
        openHumanPending()
        guard let pane = attention.pendingOpen else { return }
        if let at = attention.pendingOpenAt,
           Date().timeIntervalSince(at) > TendAgentAttentionCenter.pendingOpenLifetime {
            attention.pendingOpen = nil
            return
        }
        guard model.executionSnapshot.watchPane(id: pane) != nil else { return }
        attention.pendingOpen = nil
        open(pane)
    }
}

/// Settings › Behaviour: the Mac app's own notifications, above tendd's.
struct TendAppNotificationsSection: View {
    let attention: TendAgentAttentionCenter
    /// Whether tendd also posts desktop notifications (enabled with a
    /// delivery method), which would notify twice.
    let daemonNotifierEnabled: Bool
    @AppStorage(TendNotificationPreferences.attentionKey) private var notifyAttention = true
    @AppStorage(TendNotificationPreferences.finishedKey) private var notifyFinished = true
    @AppStorage(TendNotificationPreferences.soundKey) private var sound = true
    @AppStorage(TendNotificationPreferences.dockBadgeKey) private var dockBadge = true
    @AppStorage(TendUsageAlerts.enabledKey) private var notifyUsage = true
    @State private var authorization: TendUserNotifications.Authorization?

    private var isDenied: Bool { authorization == .denied }

    var body: some View {
        Section("Tend notifications") {
            // Permission first: while macOS blocks Tend, the switches below
            // cannot do anything, so they are dimmed rather than looking live.
            authorizationRow
            Group {
                Toggle("Notify when an agent needs you", isOn: $notifyAttention)
                Toggle("Notify when an agent finishes a turn", isOn: $notifyFinished)
                Toggle("Play a sound", isOn: $sound)
                    .disabled(!notifyAttention && !notifyFinished)
                Toggle("Show the count on the Dock icon", isOn: $dockBadge)
                    .onChange(of: dockBadge) { _ in attention.refreshDockBadge() }
                Toggle("Notify before a usage limit runs out", isOn: $notifyUsage)
                Text("Once at 80% and once at 95% of a 5-hour or weekly window, with a sound at 95%.")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                Text("Tend skips the pane you are looking at and repeats for the same agent within 30 seconds. An agent that finished or started waiting since you last opened it keeps a dot until you do.")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
            }
            .disabled(isDenied)
            if daemonNotifierEnabled, !isDenied, notifyAttention || notifyFinished {
                Label("tendd's desktop notifications are also on, so you may be notified twice.", systemImage: "exclamationmark.circle")
                    .font(TendType.detail)
                    .foregroundStyle(.orange)
            }
        }
        .task { authorization = await TendUserNotifications.shared.authorization() }
    }

    @ViewBuilder
    private var authorizationRow: some View {
        switch authorization {
        case nil:
            EmptyView()
        case .allowed:
            Label("macOS allows Tend to notify you.", systemImage: "checkmark.circle")
                .font(TendType.detail)
                .foregroundStyle(.secondary)
        case .notDetermined:
            HStack {
                Text("macOS will ask the first time Tend has something to tell you.")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Allow Notifications…") {
                    Task {
                        await TendUserNotifications.shared.requestAuthorization()
                        authorization = await TendUserNotifications.shared.authorization()
                    }
                }
                .buttonStyle(.tendSecondary)
            }
        case .denied:
            HStack {
                Label("Notifications for Tend are off in System Settings.", systemImage: "bell.slash")
                    .font(TendType.detail)
                    .foregroundStyle(.orange)
                Spacer()
                Button("Open System Settings") {
                    if let url = URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension") {
                        NSWorkspace.shared.open(url)
                    }
                }
                .buttonStyle(.tendSecondary)
            }
        }
    }
}
