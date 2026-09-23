import AppKit
import SwiftUI

private struct MuxaAttentionKey: EnvironmentKey {
    static let defaultValue: MuxaAgentAttentionCenter? = nil
}

extension EnvironmentValues {
    /// Optional so a row rendered outside the workbench (a detached module
    /// window, a test host) simply shows no unread dot instead of crashing
    /// on a missing environment object.
    var muxaAttention: MuxaAgentAttentionCenter? {
        get { self[MuxaAttentionKey.self] }
        set { self[MuxaAttentionKey.self] = newValue }
    }
}

/// The accent dot an agent pane carries while it finished or started needing
/// the operator since they last looked — the editor's "unsaved" dot, reused
/// for "unseen".
struct MuxaUnreadDot: View {
    let pane: MuxaWatchPaneIdentity
    var size: CGFloat = 6
    @Environment(\.muxaAttention) private var attention

    var body: some View {
        if let attention {
            MuxaUnreadDotContent(center: attention, pane: pane, size: size)
        }
    }
}

private struct MuxaUnreadDotContent: View {
    @ObservedObject var center: MuxaAgentAttentionCenter
    let pane: MuxaWatchPaneIdentity
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
struct MuxaAttentionTracking: ViewModifier {
    @ObservedObject var model: AppModel
    @ObservedObject var tabs: MuxaWorkbenchTabs
    @ObservedObject var attention: MuxaAgentAttentionCenter
    let open: (MuxaWatchPaneIdentity) -> Void
    @Environment(\.openWindow) private var openWindow

    func body(content: Content) -> some View {
        content
            .environment(\.muxaAttention, attention)
            .onAppear {
                MuxaWorkbenchPresenter.remember(openWindow)
                updateVisibility()
                openPending()
            }
            .onReceive(tabs.$groups) { _ in DispatchQueue.main.async(execute: updateVisibility) }
            .onChange(of: model.workspaceRevision) { _ in
                updateVisibility()
                openPending()
            }
            .onChange(of: attention.pendingOpen) { _ in openPending() }
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
    /// group while Muxa is frontmost and the workbench is on screen.
    private func updateVisibility() {
        let panes = Set(tabs.groups.compactMap(\.active).compactMap { selection -> MuxaWatchPaneIdentity? in
            switch selection {
            case .pane(let id): id
            case .agent(let id):
                model.hostedAgents.first { $0.id == id }.flatMap(MuxaAgentAttentionCenter.paneIdentity)
            default: nil
            }
        })
        let workbench = NSApp.windows.first { $0.identifier?.rawValue == "muxa.main-workbench" }
        let onScreen = workbench.map { $0.occlusionState.contains(.visible) && !$0.isMiniaturized } ?? false
        attention.setVisible(panes: panes, appActive: NSApp.isActive && onScreen)
    }

    /// A click can arrive before the pane's host has reported in (the click
    /// that launched Muxa); the request waits for the next snapshot.
    private func openPending() {
        guard let pane = attention.pendingOpen else { return }
        if let at = attention.pendingOpenAt,
           Date().timeIntervalSince(at) > MuxaAgentAttentionCenter.pendingOpenLifetime {
            attention.pendingOpen = nil
            return
        }
        guard model.executionSnapshot.watchPane(id: pane) != nil else { return }
        attention.pendingOpen = nil
        open(pane)
    }
}

/// Settings › Behaviour: the Mac app's own notifications, above muxad's.
struct MuxaAppNotificationsSection: View {
    let attention: MuxaAgentAttentionCenter
    /// Whether muxad's desktop notifier is also on, which would notify twice.
    let daemonNotifierEnabled: Bool
    @AppStorage(MuxaNotificationPreferences.attentionKey) private var notifyAttention = true
    @AppStorage(MuxaNotificationPreferences.finishedKey) private var notifyFinished = true
    @AppStorage(MuxaNotificationPreferences.soundKey) private var sound = true
    @AppStorage(MuxaNotificationPreferences.dockBadgeKey) private var dockBadge = true
    @State private var authorization: MuxaUserNotifications.Authorization?

    var body: some View {
        Section("Muxa notifications") {
            Toggle("Notify when an agent needs you", isOn: $notifyAttention)
            Toggle("Notify when an agent finishes a turn", isOn: $notifyFinished)
            Toggle("Play a sound", isOn: $sound)
                .disabled(!notifyAttention && !notifyFinished)
            Toggle("Show the count on the Dock icon", isOn: $dockBadge)
                .onChange(of: dockBadge) { _ in attention.refreshDockBadge() }
            Text("Muxa skips the pane you are looking at and repeats for the same agent within 30 seconds. An agent that finished or started waiting since you last opened it keeps a dot until you do.")
                .font(.caption)
                .foregroundStyle(.secondary)
            authorizationRow
            if daemonNotifierEnabled, notifyAttention || notifyFinished {
                Label("muxad's desktop notifications are also on, so you may be notified twice.", systemImage: "exclamationmark.circle")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
        }
        .task { authorization = await MuxaUserNotifications.shared.authorization() }
    }

    @ViewBuilder
    private var authorizationRow: some View {
        switch authorization {
        case nil:
            EmptyView()
        case .allowed:
            Label("macOS allows Muxa to notify you.", systemImage: "checkmark.circle")
                .font(.caption)
                .foregroundStyle(.secondary)
        case .notDetermined:
            HStack {
                Text("macOS will ask the first time Muxa has something to tell you.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Allow Notifications…") {
                    Task {
                        await MuxaUserNotifications.shared.requestAuthorization()
                        authorization = await MuxaUserNotifications.shared.authorization()
                    }
                }
                .buttonStyle(.muxaSecondary)
            }
        case .denied:
            HStack {
                Label("Notifications for Muxa are off in System Settings.", systemImage: "bell.slash")
                    .font(.caption)
                    .foregroundStyle(.orange)
                Spacer()
                Button("Open System Settings") {
                    if let url = URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension") {
                        NSWorkspace.shared.open(url)
                    }
                }
                .buttonStyle(.muxaSecondary)
            }
        }
    }
}
