import AppKit
import SwiftUI
import UserNotifications

/// The Mac app's own notification preferences. They live in UserDefaults
/// rather than muxa's config because they describe this app, not muxad: the
/// daemon's notifier (Settings › Behaviour › Notifications) is separate.
enum MuxaNotificationPreferences {
    static let attentionKey = "muxa.notifications.attention"
    static let finishedKey = "muxa.notifications.finished"
    static let soundKey = "muxa.notifications.sound"
    static let dockBadgeKey = "muxa.notifications.dockBadge"

    static func current(_ defaults: UserDefaults = .standard) -> MuxaNotificationSettings {
        func flag(_ key: String) -> Bool { defaults.object(forKey: key) as? Bool ?? true }
        return MuxaNotificationSettings(
            notifyAttention: flag(attentionKey),
            notifyFinished: flag(finishedKey),
            sound: flag(soundKey),
            dockBadge: flag(dockBadgeKey)
        )
    }
}

/// Brings the workbench forward for a notification click. The workbench is a
/// WindowGroup, so once its window is closed there is nothing to order front;
/// then it is reopened through an `openWindow` action remembered from a live
/// scene (the workbench itself or the menu-bar extra).
@MainActor
enum MuxaWorkbenchPresenter {
    private static var openWindow: OpenWindowAction?

    static func remember(_ action: OpenWindowAction) {
        openWindow = action
    }

    static func present() {
        if let window = NSApp.windows.first(where: { $0.identifier?.rawValue == "muxa.main-workbench" }) {
            if window.isMiniaturized { window.deminiaturize(nil) }
            window.makeKeyAndOrderFront(nil)
        } else {
            openWindow?(id: "main")
        }
        NSApp.activate(ignoringOtherApps: true)
    }
}

/// `UNUserNotificationCenter` behind `MuxaNotificationPosting`, plus the
/// delegate that routes a click back to its pane.
///
/// Permission is asked for lazily — the first time there is something to
/// post, or from Settings — never at launch, where the prompt would come
/// with no reason attached.
@MainActor
final class MuxaUserNotifications: NSObject, MuxaNotificationPosting {
    static let shared = MuxaUserNotifications()

    enum Authorization: Equatable {
        case notDetermined, allowed, denied
    }

    weak var attention: MuxaAgentAttentionCenter? {
        didSet {
            guard let attention, let pending = pendingPane else { return }
            pendingPane = nil
            attention.pendingOpen = pending
        }
    }
    /// A click that arrived before the app model existed, e.g. the one that
    /// launched the app.
    private var pendingPane: MuxaWatchPaneIdentity?
    private var center: UNUserNotificationCenter { .current() }

    /// Must run before launch finishes so a click that launched the app is
    /// delivered to the delegate.
    func installDelegate() {
        center.delegate = self
    }

    func authorization() async -> Authorization {
        // The callback form: before the macOS 26 SDK, UNNotificationSettings
        // is not Sendable, so the async form cannot hand it to the main actor.
        let status = await withCheckedContinuation { continuation in
            center.getNotificationSettings { continuation.resume(returning: $0.authorizationStatus) }
        }
        switch status {
        case .notDetermined: return .notDetermined
        case .denied: return .denied
        default: return .allowed
        }
    }

    @discardableResult
    func requestAuthorization() async -> Bool {
        (try? await center.requestAuthorization(options: [.alert, .sound, .badge])) ?? false
    }

    func post(_ notification: MuxaAgentNotification) {
        let content = UNMutableNotificationContent()
        content.title = notification.title
        content.subtitle = notification.subtitle
        if let body = notification.body { content.body = body }
        content.threadIdentifier = notification.identifier
        if notification.sound { content.sound = .default }
        if let pane = notification.pane {
            content.userInfo = ["host": pane.hostAlias, "socket": pane.socket, "pane": pane.paneID]
        }
        let request = UNNotificationRequest(identifier: notification.identifier, content: content, trigger: nil)
        Task {
            switch await authorization() {
            case .denied: return
            case .notDetermined: guard await requestAuthorization() else { return }
            case .allowed: break
            }
            do {
                try await center.add(request)
            } catch {
                MuxaLog.app.warning("notification post failed: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    func removeDelivered(identifiers: [String]) {
        center.removeDeliveredNotifications(withIdentifiers: identifiers)
    }

    func setDockBadge(_ count: Int) {
        let label = count > 0 ? "\(count)" : nil
        if NSApp.dockTile.badgeLabel != label { NSApp.dockTile.badgeLabel = label }
    }

    fileprivate func open(_ pane: MuxaWatchPaneIdentity?) {
        MuxaWorkbenchPresenter.present()
        guard let pane else { return }
        if let attention {
            attention.pendingOpen = pane
        } else {
            pendingPane = pane
        }
    }
}

extension MuxaUserNotifications: UNUserNotificationCenterDelegate {
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        let info = response.notification.request.content.userInfo
        let pane: MuxaWatchPaneIdentity? = if let host = info["host"] as? String,
            let socket = info["socket"] as? String,
            let paneID = info["pane"] as? String {
            MuxaWatchPaneIdentity(hostAlias: host, socket: socket, paneID: paneID)
        } else {
            nil
        }
        await MuxaUserNotifications.shared.open(pane)
    }

    /// The center already skips the pane being looked at, so anything that
    /// reaches here while Muxa is frontmost is about another pane and still
    /// deserves a banner.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        notification.request.content.sound == nil ? [.banner, .list] : [.banner, .list, .sound]
    }
}
