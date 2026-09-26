import Foundation

/// What the operator last saw of the agent in one pane: the agent session and
/// the state it was in, stamped with the moment the daemon says it entered
/// that state. `lastObservedAt` only drives pruning.
struct MuxaSeenRecord: Codable, Equatable, Sendable {
    var agentSessionID: String
    var stamp: String
    var lastObservedAt: Date
}

/// Pure rules for the "finished but unseen" state, kept apart from the
/// center so tests can state them directly.
enum MuxaUnreadRules {
    /// Fleet agents report `state_entered_at` from their own host's clock, so
    /// it is never compared with the Mac's clock — only with the stamp the
    /// agent carried when the operator last looked. Equality needs no clock.
    static func stamp(_ agent: MuxaAgent) -> String {
        "\(agent.state)|\(agent.stateEnteredAt ?? "")"
    }

    static func record(_ agent: MuxaAgent, at date: Date) -> MuxaSeenRecord {
        MuxaSeenRecord(agentSessionID: agent.agentSessionID, stamp: stamp(agent), lastObservedAt: date)
    }

    /// States worth coming back to: the agent is waiting on the operator, or
    /// it is idle after running a turn. A freshly started agent that has
    /// never been prompted is idle too, but has not finished anything.
    static func isNotable(_ agent: MuxaAgent) -> Bool {
        if MuxaAttention.states.contains(agent.state) { return true }
        return agent.state == "idle" && agent.lastPromptAt != nil
    }

    static func isUnread(_ agent: MuxaAgent, seen: MuxaSeenRecord?) -> Bool {
        guard let seen, isNotable(agent) else { return false }
        return seen.agentSessionID != agent.agentSessionID || seen.stamp != stamp(agent)
    }
}

/// Which state changes post a notification, and how often.
enum MuxaNotificationRules {
    enum Event: Equatable, Sendable {
        case attention(String)
        case finished
    }

    /// muxad's notifier uses the same window (`crates/muxa/src/notify.rs`).
    static let debounce: TimeInterval = 30

    static func event(from previous: String, to current: String) -> Event? {
        if MuxaAttention.states.contains(current), previous != current {
            return .attention(current)
        }
        // Only a turn that was actually working finishes: every new agent
        // passes starting → idle on its way up, and muxad's notifier skips
        // that step too (`notify.rs`).
        if previous == "working", current == "idle" || current == "stopped" {
            return .finished
        }
        return nil
    }

    /// A flapping agent must not spam: a repeat of the same state within the
    /// window is dropped, while a genuinely different state always fires.
    static func shouldFire(
        last: (state: String, at: Date)?,
        state: String,
        now: Date,
        window: TimeInterval = debounce
    ) -> Bool {
        guard let last, last.state == state else { return true }
        return now.timeIntervalSince(last.at) >= window
    }

    /// Notification bodies are one glance long; agent text can be a page.
    static func body(_ text: String?, limit: Int = 160) -> String? {
        guard let text else { return nil }
        let collapsed = text.split(whereSeparator: \.isWhitespace).joined(separator: " ")
        guard !collapsed.isEmpty else { return nil }
        guard collapsed.count > limit else { return collapsed }
        return String(collapsed.prefix(limit - 1)) + "…"
    }
}

/// The buttons a notification offers and what a response to one means,
/// kept free of `UNUserNotificationCenter` so tests can state them.
enum MuxaNotificationActions {
    static let open = "muxa.action.open"
    static let markRead = "muxa.action.mark-read"
    static let reply = "muxa.action.reply"
    /// Open and Mark as Read.
    static let agentCategory = "muxa.agent"
    /// Open, Reply…, and Mark as Read: only an agent waiting for free-form
    /// input can take typed text; a choice needs its menu, and an error or a
    /// finished turn is better answered from the pane.
    static let inputCategory = "muxa.agent.input"
    /// `UNNotificationDefaultActionIdentifier`: the notification body itself.
    static let defaultAction = "com.apple.UNNotificationDefaultActionIdentifier"
    /// Fleet host states that can take a prompt from the Mac.
    static let replyHostStates: Set<String> = ["online", "version_skew"]

    enum Route: Equatable, Sendable {
        case open
        case markRead
        case reply(String)
        case ignore
    }

    static func category(for event: MuxaNotificationRules.Event) -> String {
        event == .attention("waiting_input") ? inputCategory : agentCategory
    }

    static func route(action: String, userText: String?) -> Route {
        switch action {
        case defaultAction, open: return .open
        case markRead: return .markRead
        case reply:
            // An empty reply would only press Return in the agent's prompt.
            let text = userText?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            return text.isEmpty ? .ignore : .reply(text)
        default: return .ignore
        }
    }

    /// Why a reply cannot reach `pane`, or nil when it can be sent.
    static func replyBlocker(_ pane: MuxaWatchPane?) -> String? {
        guard let pane else { return String(localized: "This pane is no longer available") }
        let host = pane.host
        if host.local || (host.mode == "control" && replyHostStates.contains(host.state)) { return nil }
        return String(localized: "Not connected: \(host.alias)")
    }
}

struct MuxaNotificationSettings: Equatable, Sendable {
    var notifyAttention = true
    var notifyFinished = true
    var sound = true
    var dockBadge = true
}

struct MuxaAgentNotification: Equatable, Sendable {
    let identifier: String
    let title: String
    let subtitle: String
    let body: String?
    let pane: MuxaWatchPaneIdentity?
    let sound: Bool
    /// A `MuxaNotificationActions` category, or nil for no buttons.
    let category: String?
}

@MainActor
protocol MuxaNotificationPosting: AnyObject {
    func post(_ notification: MuxaAgentNotification)
    func removeDelivered(identifiers: [String])
    func setDockBadge(_ count: Int)
}

/// Tracks which agents finished or started needing the operator since they
/// last looked, and posts the macOS notifications for those transitions.
///
/// Unread state is derived on every execution snapshot from persisted "seen"
/// stamps, so it survives a relaunch; notifications come only from
/// transitions observed while the app runs, so a relaunch shows what was
/// missed as unread rather than as a burst of banners.
@MainActor
final class MuxaAgentAttentionCenter: ObservableObject {
    @Published private(set) var unreadPanes: Set<MuxaWatchPaneIdentity> = []
    /// Agents needing attention plus unread panes, for the Dock badge.
    @Published private(set) var dockCount = 0
    /// A pane a notification click asked to open; the workbench consumes it.
    @Published var pendingOpen: MuxaWatchPaneIdentity? {
        didSet { pendingOpenAt = pendingOpen == nil ? nil : Date() }
    }
    /// When the click came in. A request for a pane that never shows up
    /// expires, so a later pane reusing its `%N` id is not opened by it.
    private(set) var pendingOpenAt: Date?
    static let pendingOpenLifetime: TimeInterval = 30

    static let persistenceKey = "muxa.attention.seen.v1"
    static let pruneAfter: TimeInterval = 7 * 24 * 60 * 60
    /// `lastObservedAt` only needs day precision for pruning; refreshing it
    /// hourly keeps every fleet event from rewriting UserDefaults.
    private static let observeRefresh: TimeInterval = 60 * 60

    private var seen: [String: MuxaSeenRecord]
    private var previousStates: [String: String] = [:]
    private var lastFired: [String: (state: String, at: Date)] = [:]
    private var visiblePanes: Set<MuxaWatchPaneIdentity> = []
    private var appActive = false
    private var snapshot = MuxaExecutionSnapshot.empty
    private let defaults: UserDefaults?
    private let poster: MuxaNotificationPosting?
    private let now: () -> Date
    var settings: () -> MuxaNotificationSettings
    /// Delivers a notification reply; the app model wires it to muxad's
    /// `send_prompt`, the path the pane's prompt composer uses.
    var sendPrompt: (@MainActor (MuxaFleetHostIdentity, MuxaPaneInfo, String) async throws -> Void)?
    /// How long a notification action waits for its pane to show up.
    var actionPaneWait: TimeInterval = 10

    init(
        defaults: UserDefaults? = .standard,
        poster: MuxaNotificationPosting? = nil,
        settings: @escaping () -> MuxaNotificationSettings = { MuxaNotificationSettings() },
        now: @escaping () -> Date = Date.init
    ) {
        self.defaults = defaults
        self.poster = poster
        self.settings = settings
        self.now = now
        if let data = defaults?.data(forKey: Self.persistenceKey),
           let stored = try? JSONDecoder().decode([String: MuxaSeenRecord].self, from: data) {
            seen = stored
        } else {
            seen = [:]
        }
    }

    static func key(_ pane: MuxaWatchPaneIdentity) -> String {
        "\(pane.hostAlias)\u{0}\(pane.socket)\u{0}\(pane.paneID)"
    }

    func isUnread(_ pane: MuxaWatchPaneIdentity) -> Bool {
        unreadPanes.contains(pane)
    }

    var hasUnread: Bool { !unreadPanes.isEmpty }

    func ingest(_ snapshot: MuxaExecutionSnapshot) {
        self.snapshot = snapshot
        let date = now()
        var changed = false
        for pane in Self.agentPanes(in: snapshot) {
            guard let agent = pane.agent else { continue }
            let key = Self.key(pane.id)
            if seen[key] == nil || isLookedAt(pane.id) {
                let record = MuxaUnreadRules.record(agent, at: date)
                if seen[key]?.agentSessionID != record.agentSessionID || seen[key]?.stamp != record.stamp {
                    seen[key] = record
                    changed = true
                }
            }
            if let observed = seen[key]?.lastObservedAt,
               date.timeIntervalSince(observed) >= Self.observeRefresh {
                seen[key]?.lastObservedAt = date
                changed = true
            }
        }
        let stale = seen.filter { date.timeIntervalSince($0.value.lastObservedAt) > Self.pruneAfter }.map(\.key)
        for key in stale { seen.removeValue(forKey: key) }
        if changed || !stale.isEmpty { persist() }

        postTransitions(in: snapshot, at: date)
        recompute()
    }

    /// The panes shown as the active tab of an editor group, and whether the
    /// workbench is what the operator is looking at. A visible pane is seen
    /// continuously, so an agent that finishes in front of the operator never
    /// turns unread.
    func setVisible(panes: Set<MuxaWatchPaneIdentity>, appActive: Bool) {
        guard panes != visiblePanes || appActive != self.appActive else { return }
        visiblePanes = panes
        self.appActive = appActive
        guard appActive else { return }
        markSeen(panes)
    }

    func markAllRead() {
        markSeen(Set(Self.agentPanes(in: snapshot).map(\.id)))
    }

    /// Mark as Read on a notification: the same as looking at the pane, and
    /// it withdraws the pane's delivered notification.
    func markRead(_ pane: MuxaWatchPaneIdentity) async {
        _ = await resolve(pane)
        markSeen([pane])
    }

    /// Reply… on a notification types `text` into the agent's pane as a
    /// prompt. A reply that cannot be delivered comes back as a notification
    /// carrying the text, so nothing the operator typed is dropped silently.
    func reply(_ text: String, to pane: MuxaWatchPaneIdentity) async {
        let target = await resolve(pane)
        if let blocker = MuxaNotificationActions.replyBlocker(target) {
            postReplyFailure(text, to: pane, reason: blocker)
            return
        }
        guard let target, let sendPrompt else {
            postReplyFailure(text, to: pane, reason: String(localized: "Muxa is not connected to its daemon."))
            return
        }
        do {
            try await sendPrompt(target.host, target.pane, text)
        } catch {
            MuxaLog.app.warning("notification reply failed: \(error.localizedDescription, privacy: .public)")
            postReplyFailure(text, to: pane, reason: error.localizedDescription)
            return
        }
        markSeen([pane])
    }

    /// Re-applies settings that change the Dock badge without a snapshot.
    func refreshDockBadge() {
        poster?.setDockBadge(settings().dockBadge ? dockCount : 0)
    }

    /// The pane an action names. An action can launch the app, so the pane
    /// gets a short grace period to arrive with the first snapshot.
    private func resolve(_ pane: MuxaWatchPaneIdentity) async -> MuxaWatchPane? {
        var remaining = actionPaneWait
        while true {
            if let found = snapshot.watchPane(id: pane) { return found }
            guard remaining > 0 else { return nil }
            try? await Task.sleep(nanoseconds: 250_000_000)
            remaining -= 0.25
        }
    }

    private func postReplyFailure(_ text: String, to pane: MuxaWatchPaneIdentity, reason: String) {
        poster?.post(MuxaAgentNotification(
            identifier: "reply:\(Self.key(pane))",
            title: String(localized: "Your reply was not sent"),
            subtitle: reason,
            body: text,
            pane: pane,
            sound: settings().sound,
            category: nil
        ))
    }

    private func isLookedAt(_ pane: MuxaWatchPaneIdentity) -> Bool {
        appActive && visiblePanes.contains(pane)
    }

    private func markSeen(_ panes: Set<MuxaWatchPaneIdentity>) {
        let date = now()
        var changed = false
        for pane in panes {
            guard let agent = snapshot.watchPane(id: pane)?.agent else { continue }
            let record = MuxaUnreadRules.record(agent, at: date)
            let key = Self.key(pane)
            if seen[key]?.agentSessionID != record.agentSessionID || seen[key]?.stamp != record.stamp {
                seen[key] = record
                changed = true
            }
        }
        if changed { persist() }
        if !panes.isEmpty {
            poster?.removeDelivered(identifiers: panes.map(Self.key))
        }
        recompute()
    }

    private func recompute() {
        var unread = Set<MuxaWatchPaneIdentity>()
        for pane in Self.agentPanes(in: snapshot) {
            guard let agent = pane.agent else { continue }
            if MuxaUnreadRules.isUnread(agent, seen: seen[Self.key(pane.id)]) {
                unread.insert(pane.id)
            }
        }
        var needing = Set(unread.map(Self.key))
        for hosted in snapshot.hostedAgents where MuxaAttention.states.contains(hosted.agent.state) {
            needing.insert(Self.paneIdentity(for: hosted).map(Self.key) ?? "agent:\(hosted.id)")
        }
        if unreadPanes != unread { unreadPanes = unread }
        if dockCount != needing.count { dockCount = needing.count }
        refreshDockBadge()
    }

    private func postTransitions(in snapshot: MuxaExecutionSnapshot, at date: Date) {
        var current: [String: String] = [:]
        for hosted in snapshot.hostedAgents {
            let state = hosted.agent.state
            current[hosted.id] = state
            guard let previous = previousStates[hosted.id],
                  let event = MuxaNotificationRules.event(from: previous, to: state) else { continue }
            let settings = settings()
            switch event {
            case .attention: guard settings.notifyAttention else { continue }
            case .finished: guard settings.notifyFinished else { continue }
            }
            let pane = Self.paneIdentity(for: hosted)
            if let pane, isLookedAt(pane) { continue }
            guard MuxaNotificationRules.shouldFire(last: lastFired[hosted.id], state: state, now: date) else {
                continue
            }
            lastFired[hosted.id] = (state, date)
            poster?.post(Self.notification(for: hosted, event: event, pane: pane, sound: settings.sound))
        }
        previousStates = current
        lastFired = lastFired.filter { current[$0.key] != nil }
    }

    static func notification(
        for hosted: MuxaHostedAgent,
        event: MuxaNotificationRules.Event,
        pane: MuxaWatchPaneIdentity?,
        sound: Bool
    ) -> MuxaAgentNotification {
        let agent = hosted.agent
        let name = hosted.pane?.agentAlias.map { "@\($0)" }
            ?? agent.aiTitle
            ?? agent.kind.replacingOccurrences(of: "_", with: " ")
        let title: String
        let body: String?
        switch event {
        case .attention(let state):
            switch state {
            case "waiting_choice": title = String(localized: "\(name) is waiting for a choice")
            case "error", "failed": title = String(localized: "\(name) hit an error")
            case "blocked": title = String(localized: "\(name) is blocked")
            default: title = String(localized: "\(name) needs your input")
            }
            body = MuxaNotificationRules.body(agent.lastNotification)
                ?? MuxaNotificationRules.body(agent.lastResponse)
        case .finished:
            title = String(localized: "\(name) finished")
            body = MuxaNotificationRules.body(agent.lastResponse)
                ?? MuxaNotificationRules.body(agent.recap)
        }
        let location = hosted.pane.map { pane in
            let window = pane.windowName.isEmpty ? pane.stableWindowID : pane.windowName
            return "\(pane.session) › \(window)"
        }
        return MuxaAgentNotification(
            identifier: pane.map(key) ?? "agent:\(hosted.id)",
            title: title,
            subtitle: [hosted.host.alias, location].compactMap { $0 }.joined(separator: " · "),
            body: body,
            pane: pane,
            sound: sound,
            category: MuxaNotificationActions.category(for: event)
        )
    }

    static func paneIdentity(for hosted: MuxaHostedAgent) -> MuxaWatchPaneIdentity? {
        hosted.pane.map {
            MuxaWatchPaneIdentity(hostAlias: hosted.host.alias, socket: $0.endpointSocket, paneID: $0.paneID)
        }
    }

    private static func agentPanes(in snapshot: MuxaExecutionSnapshot) -> [MuxaWatchPane] {
        snapshot.watchHosts
            .flatMap(\.sessions).flatMap(\.windows).flatMap(\.panes)
            .filter { $0.agent != nil }
    }

    private func persist() {
        guard let defaults, let data = try? JSONEncoder().encode(seen) else { return }
        defaults.set(data, forKey: Self.persistenceKey)
    }
}
