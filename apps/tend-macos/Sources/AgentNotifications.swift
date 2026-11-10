import AppKit
import SwiftUI
import UserNotifications

/// The Mac app's own notification preferences. They live in UserDefaults
/// rather than tend's config because they describe this app, not tendd: the
/// daemon's notifier (Settings › Behaviour › Notifications) is separate.
enum TendNotificationPreferences {
    static let attentionKey = "tend.notifications.attention"
    static let finishedKey = "tend.notifications.finished"
    static let soundKey = "tend.notifications.sound"
    static let dockBadgeKey = "tend.notifications.dockBadge"

    static func current(_ defaults: UserDefaults = .standard) -> TendNotificationSettings {
        func flag(_ key: String) -> Bool { defaults.object(forKey: key) as? Bool ?? true }
        return TendNotificationSettings(
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
enum TendWorkbenchPresenter {
    private static var openWindow: OpenWindowAction?

    static func remember(_ action: OpenWindowAction) {
        openWindow = action
    }

    static func present() {
        if let window = NSApp.windows.first(where: { $0.identifier?.rawValue == "tend.main-workbench" }) {
            if window.isMiniaturized { window.deminiaturize(nil) }
            window.makeKeyAndOrderFront(nil)
        } else {
            openWindow?(id: "main")
        }
        NSApp.activate(ignoringOtherApps: true)
    }
}

/// `UNUserNotificationCenter` behind `TendNotificationPosting`, plus the
/// delegate that routes a click or an action button back to its pane.
///
/// Permission is asked for lazily — the first time there is something to
/// post, or from Settings — never at launch, where the prompt would come
/// with no reason attached.
@MainActor
final class TendUserNotifications: NSObject, TendNotificationPosting {
    static let shared = TendUserNotifications()

    enum Authorization: Equatable {
        case notDetermined, allowed, denied
    }

    weak var attention: TendAgentAttentionCenter? {
        didSet {
            guard let attention else { return }
            let responses = pendingResponses
            pendingResponses = []
            for (route, pane) in responses {
                Task { await self.perform(route, for: pane, in: attention) }
            }
            if let human = pendingHumanMessageID {
                pendingHumanMessageID = nil
                attention.pendingHumanMessageID = human
            }
            guard let pending = pendingPane else { return }
            pendingPane = nil
            attention.pendingOpen = pending
        }
    }
    /// A click that arrived before the app model existed, e.g. the one that
    /// launched the app.
    private var pendingPane: TendWatchPaneIdentity?
    private var pendingHumanMessageID: String?
    private var postGenerations: [String: UUID] = [:]
    /// Mark as Read and Reply… that arrived before the app model existed; a
    /// background action can launch the app just like a click.
    private var pendingResponses: [(TendNotificationActions.Route, TendWatchPaneIdentity)] = []
    private var center: UNUserNotificationCenter { .current() }

    /// Must run before launch finishes so a click that launched the app is
    /// delivered to the delegate.
    func installDelegate() {
        center.delegate = self
        center.setNotificationCategories(Self.categories())
    }

    /// Open brings Tend forward like a click; Mark as Read and Reply… run in
    /// the background so answering an agent does not pull the operator away.
    private static func categories() -> Set<UNNotificationCategory> {
        let open = UNNotificationAction(
            identifier: TendNotificationActions.open,
            title: String(localized: "Open"),
            options: [.foreground]
        )
        let markRead = UNNotificationAction(
            identifier: TendNotificationActions.markRead,
            title: String(localized: "Mark as Read"),
            options: []
        )
        let reply = UNTextInputNotificationAction(
            identifier: TendNotificationActions.reply,
            title: String(localized: "Reply…"),
            options: [],
            textInputButtonTitle: String(localized: "Send"),
            textInputPlaceholder: String(localized: "Message for the agent")
        )
        let switchAccount = UNNotificationAction(
            identifier: TendNotificationActions.switchAccount,
            title: String(localized: "Switch Account…"),
            options: [.foreground]
        )
        return [
            UNNotificationCategory(
                identifier: TendNotificationActions.limitedCategory,
                actions: [switchAccount, open, markRead],
                intentIdentifiers: [],
                options: []
            ),
            UNNotificationCategory(
                identifier: TendNotificationActions.agentCategory,
                actions: [open, markRead],
                intentIdentifiers: [],
                options: []
            ),
            UNNotificationCategory(
                identifier: TendNotificationActions.inputCategory,
                actions: [open, reply, markRead],
                intentIdentifiers: [],
                options: []
            ),
        ]
    }

    func authorization() async -> Authorization {
        // Callback forms throughout: before the macOS 26 SDK neither
        // UNUserNotificationCenter nor UNNotificationSettings is Sendable, so
        // the async forms cannot cross from the main actor and back.
        let status = await withCheckedContinuation { continuation in
            center.getNotificationSettings { @Sendable settings in continuation.resume(returning: settings.authorizationStatus) }
        }
        switch status {
        case .notDetermined: return .notDetermined
        case .denied: return .denied
        default: return .allowed
        }
    }

    @discardableResult
    func requestAuthorization() async -> Bool {
        await withCheckedContinuation { continuation in
            center.requestAuthorization(options: [.alert, .sound, .badge]) { @Sendable granted, _ in
                continuation.resume(returning: granted)
            }
        }
    }

    func post(_ notification: TendAgentNotification) {
        let generation = UUID()
        postGenerations[notification.identifier] = generation
        let content = UNMutableNotificationContent()
        content.title = notification.title
        content.subtitle = notification.subtitle
        if let body = notification.body { content.body = body }
        content.threadIdentifier = notification.identifier
        if notification.sound { content.sound = .default }
        if let category = notification.category { content.categoryIdentifier = category }
        if let pane = notification.pane {
            content.userInfo = ["host": pane.hostAlias, "socket": pane.socket, "pane": pane.paneID]
        }
        if let human = notification.humanMessageID { content.userInfo["human_message"] = human }
        let request = UNNotificationRequest(identifier: notification.identifier, content: content, trigger: nil)
        Task {
            switch await authorization() {
            case .denied: return
            case .notDetermined: guard await requestAuthorization() else { return }
            case .allowed: break
            }
            // A human response may arrive on another device while the first
            // permission prompt is open. Do not post an already-resolved alert.
            guard postGenerations[notification.identifier] == generation else { return }
            center.add(request) { @Sendable error in
                guard let error else { return }
                TendLog.app.warning("notification post failed: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    func removeDelivered(identifiers: [String]) {
        for id in identifiers { postGenerations.removeValue(forKey: id) }
        center.removePendingNotificationRequests(withIdentifiers: identifiers)
        center.removeDeliveredNotifications(withIdentifiers: identifiers)
    }

    func setDockBadge(_ count: Int) {
        let label = count > 0 ? "\(count)" : nil
        if NSApp.dockTile.badgeLabel != label { NSApp.dockTile.badgeLabel = label }
    }

    fileprivate func respond(_ route: TendNotificationActions.Route, pane: TendWatchPaneIdentity?, humanMessageID: String? = nil) async {
        if let humanMessageID {
            guard route == .open else { return }
            if let attention { attention.pendingHumanMessageID = humanMessageID }
            else { pendingHumanMessageID = humanMessageID }
            TendWorkbenchPresenter.present()
            return
        }
        if route == .open {
            open(pane)
            return
        }
        if route == .switchAccount {
            TendWorkbenchPresenter.present()
            if let pane, let attention { attention.pendingSwitch = pane }
            return
        }
        guard let pane, route != .ignore else { return }
        if let attention {
            await perform(route, for: pane, in: attention)
        } else {
            pendingResponses.append((route, pane))
        }
    }

    private func perform(
        _ route: TendNotificationActions.Route,
        for pane: TendWatchPaneIdentity,
        in attention: TendAgentAttentionCenter
    ) async {
        switch route {
        case .markRead: await attention.markRead(pane)
        case .reply(let text): await attention.reply(text, to: pane)
        case .open, .switchAccount, .ignore: break
        }
    }

    private func open(_ pane: TendWatchPaneIdentity?) {
        TendWorkbenchPresenter.present()
        guard let pane else { return }
        if let attention {
            attention.pendingOpen = pane
        } else {
            pendingPane = pane
        }
    }
}

extension TendUserNotifications: UNUserNotificationCenterDelegate {
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        let info = response.notification.request.content.userInfo
        let route = TendNotificationActions.route(
            action: response.actionIdentifier,
            userText: (response as? UNTextInputNotificationResponse)?.userText
        )
        let pane: TendWatchPaneIdentity? = if let host = info["host"] as? String,
            let socket = info["socket"] as? String,
            let paneID = info["pane"] as? String {
            TendWatchPaneIdentity(hostAlias: host, socket: socket, paneID: paneID)
        } else {
            nil
        }
        await TendUserNotifications.shared.respond(route, pane: pane, humanMessageID: info["human_message"] as? String)
    }

    /// The center already skips the pane being looked at, so anything that
    /// reaches here while Tend is frontmost is about another pane and still
    /// deserves a banner.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        notification.request.content.sound == nil ? [.banner, .list] : [.banner, .list, .sound]
    }
}
