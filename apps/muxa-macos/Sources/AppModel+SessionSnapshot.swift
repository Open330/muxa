import Foundation

/// Which snapshot sheet is up. Held on `AppModel` so the Explore menu, the
/// command palette, and the sheet itself agree on one presentation.
enum MuxaSessionSnapshotSheet: String, Identifiable {
    case save
    case restore

    var id: String { rawValue }
}

extension AppModel {
    func presentSessionSnapshots(_ sheet: MuxaSessionSnapshotSheet) {
        sessionSnapshotSheet = sheet
    }
}

/// State behind the Save and Restore snapshot sheets: the list, the selected
/// snapshot's plan, and a restore polled to completion.
///
/// Owned by the sheet (a `@StateObject`), not by `AppModel`: nothing outside
/// the sheet reads it, and a fresh one per presentation means a reopened
/// sheet never shows the previous restore's results as current.
@MainActor
final class SessionSnapshotViewModel: ObservableObject {
    enum Phase: Equatable {
        case idle
        case loading
        case saving
        case planning
        case restoring
        case finished
    }

    @Published private(set) var snapshots: [MuxSnapshotEntry] = []
    @Published private(set) var selection: String?
    /// The dry run for the selection, then — after a restore — the report.
    @Published private(set) var plan: MuxSnapshotPlan?
    @Published private(set) var phase: Phase = .idle
    @Published private(set) var statusMessage: String?
    @Published private(set) var error: String?
    @Published private(set) var saved: MuxSnapshotSaved?
    @Published private(set) var hasLoaded = false
    /// False against a muxad that predates `mux_snapshot_v1`.
    @Published private(set) var supported = true
    @Published private(set) var restoreStartedAt: Date?
    /// Rebuild sessions, windows and directories but start nothing in them.
    @Published var layoutOnly = false

    private let client: MuxaSessionSnapshotClient
    private let pollInterval: Duration

    init(client: MuxaSessionSnapshotClient, pollInterval: Duration = .seconds(1)) {
        self.client = client
        self.pollInterval = pollInterval
    }

    var isBusy: Bool {
        switch phase {
        case .loading, .saving, .planning, .restoring: true
        case .idle, .finished: false
        }
    }

    var selectedEntry: MuxSnapshotEntry? {
        snapshots.first { $0.id == selection }
    }

    /// Restore is offered only once the dry run says it would create
    /// something: a snapshot whose sessions all exist has nothing to do.
    var canRestore: Bool {
        supported && !isBusy && phase != .finished
            && plan?.run == false && (plan?.sessionsToCreate ?? 0) > 0
    }

    static let unsupportedMessage = String(
        localized: "This muxad doesn't support snapshots. Update it with `muxa upgrade`, then restart muxad."
    )

    func markUnsupported() {
        supported = false
        error = Self.unsupportedMessage
        hasLoaded = true
    }

    func load() async {
        guard supported else { return }
        phase = .loading
        defer {
            if phase == .loading { phase = .idle }
            hasLoaded = true
        }
        do {
            snapshots = try await client.list()
            error = nil
            let keep = selection.flatMap { id in snapshots.first { $0.id == id } }
            if let pick = keep ?? snapshots.first(where: \.readable) {
                await select(pick.id, force: keep == nil)
            } else {
                selection = nil
                plan = nil
            }
        } catch {
            self.error = error.localizedDescription
        }
    }

    func select(_ id: String, force: Bool = false) async {
        guard force || selection != id || plan == nil else { return }
        selection = id
        plan = nil
        statusMessage = nil
        if phase == .finished { phase = .idle }
        guard snapshots.first(where: { $0.id == id })?.readable == true else { return }
        let previous = phase
        phase = .planning
        defer { if phase == .planning { phase = previous == .loading ? .loading : .idle } }
        do {
            let plan = try await client.plan(id: id)
            // A later click may have moved on while this one was planning.
            if selection == id {
                self.plan = plan
                error = nil
            }
        } catch {
            if selection == id { self.error = error.localizedDescription }
        }
    }

    /// Take a manual snapshot. Returns whether one was written.
    @discardableResult
    func save() async -> Bool {
        guard supported, !isBusy else { return false }
        phase = .saving
        defer { if phase == .saving { phase = .idle } }
        do {
            let saved = try await client.save()
            self.saved = saved
            error = nil
            if let summary = saved.summary {
                statusMessage = String(localized: "Saved \(summary.countsLabel) on \(summary.serverLabel)")
            }
            return !saved.skipped
        } catch {
            self.error = error.localizedDescription
            return false
        }
    }

    func restore() async {
        guard canRestore, let id = selection else { return }
        phase = .restoring
        error = nil
        restoreStartedAt = Date()
        statusMessage = String(localized: "Restoring…")
        do {
            var operation = try await client.restore(id: id, layoutOnly: layoutOnly)
            while operation.state == .running {
                try await Task.sleep(for: pollInterval)
                operation = try await client.status(operationID: operation.operationID)
            }
            apply(operation)
            phase = .finished
        } catch {
            self.error = error.localizedDescription
            phase = .idle
        }
        restoreStartedAt = nil
    }

    func delete(_ id: String) async {
        guard !isBusy else { return }
        do {
            try await client.delete(id: id)
            if selection == id {
                selection = nil
                plan = nil
            }
            await load()
        } catch {
            self.error = error.localizedDescription
        }
    }

    func apply(_ operation: MuxSnapshotOperation) {
        if let result = operation.result { plan = result }
        switch operation.state {
        case .failed:
            statusMessage = nil
            error = operation.message
        default:
            error = nil
            statusMessage = operation.result?.totals.map(SessionSnapshotTree.totalsLine) ?? operation.message
        }
    }
}

/// One line of the snapshot tree.
struct SessionSnapshotTreeRow: Identifiable, Hashable {
    enum Badge: Hashable {
        // Sessions, in a plan.
        case willCreate
        case exists
        case willFillMissing
        // Sessions, after a restore.
        case created
        case filled
        case skipped
        case failed
        // Panes, in a plan.
        case resume(String)
        case replay(String)
        case shell
        case notReplayable
        case resumeByHand
        // Panes, after a restore.
        case relaunched
        case startByHand
    }

    let id: String
    let depth: Int
    let symbol: String
    let title: String
    let detail: String?
    let badge: Badge?
    /// The full command behind a truncated badge, or an error.
    let help: String?
    let message: String?
}

/// Flattens a plan — or, after a restore, its report — into the indented
/// rows the sheet draws. Pure, so what a person reads is tested without a
/// view.
enum SessionSnapshotTree {
    /// Replay badges longer than this are cut; the full command is the help.
    static let commandBadgeLimit = 32

    static func rows(plan: MuxSnapshotPlan?) -> [SessionSnapshotTreeRow] {
        guard let plan else { return [] }
        var rows: [SessionSnapshotTreeRow] = []
        for session in plan.sessions {
            let sessionID = "s:\(session.name)"
            rows.append(SessionSnapshotTreeRow(
                id: sessionID, depth: 0, symbol: "rectangle.stack", title: session.name,
                detail: nil, badge: sessionBadge(session), help: nil,
                message: session.result == .failed ? session.error : nil
            ))
            let skipped = session.action == .skip
            for window in session.windows {
                let windowID = "\(sessionID)/\(window.index)"
                rows.append(SessionSnapshotTreeRow(
                    id: windowID, depth: 1, symbol: "macwindow", title: "\(window.index): \(window.name)",
                    detail: window.panes.count == 1
                        ? String(localized: "1 pane")
                        : String(localized: "\(window.panes.count) panes"),
                    badge: nil, help: nil, message: nil
                ))
                for pane in window.panes {
                    // A skipped session keeps its panes on screen for
                    // reference, but nothing happens to them.
                    let badge = skipped ? nil : paneBadge(pane)
                    rows.append(SessionSnapshotTreeRow(
                        id: "\(windowID)/\(pane.index)", depth: 2,
                        symbol: pane.action == .shell ? "terminal" : "play.rectangle",
                        title: abbreviate(pane.path), detail: nil, badge: badge,
                        help: pane.note ?? pane.command,
                        message: pane.result == .failed ? pane.error : nil
                    ))
                }
            }
        }
        return rows
    }

    static func sessionBadge(_ session: MuxSnapshotPlan.Session) -> SessionSnapshotTreeRow.Badge? {
        if let result = session.result {
            switch result {
            case .created: return .created
            case .skipped: return .skipped
            case .filled: return .filled
            case .failed: return .failed
            case .unknown: return nil
            }
        }
        switch session.action {
        case .create: return .willCreate
        case .skip: return .exists
        case .fillMissing: return .willFillMissing
        case .unknown: return nil
        }
    }

    static func paneBadge(_ pane: MuxSnapshotPlan.Pane) -> SessionSnapshotTreeRow.Badge? {
        if let result = pane.result {
            switch result {
            case .relaunched: return .relaunched
            case .shell: return .shell
            case .manual: return .startByHand
            case .failed: return .failed
            case .unknown: return nil
            }
        }
        switch pane.action {
        case .resume: return .resume(agentName(pane.agentKind))
        case .replay: return .replay(truncated(pane.command ?? ""))
        case .shell: return .shell
        case .manual: return .notReplayable
        case .resumeByHand: return .resumeByHand
        case .unknown: return nil
        }
    }

    /// What the dry run will do, for the footer.
    static func planLine(_ plan: MuxSnapshotPlan) -> String {
        let create = plan.sessionsToCreate
        let skip = plan.sessionsToSkip
        if create == 0 {
            return String(localized: "Every session already exists — nothing to restore.")
        }
        let createText = create == 1
            ? String(localized: "Creates 1 session")
            : String(localized: "Creates \(create) sessions")
        guard skip > 0 else { return createText }
        return String(localized: "\(createText), skips \(skip)")
    }

    static func totalsLine(_ totals: MuxSnapshotPlan.Totals) -> String {
        var line = String(localized: "Created \(totals.sessionsCreated), skipped \(totals.sessionsSkipped); relaunched \(totals.relaunched) of \(totals.panes) panes")
        if totals.manual > 0 {
            line += " · " + String(localized: "\(totals.manual) to start by hand")
        }
        if totals.failed + totals.sessionsFailed > 0 {
            line += " · " + String(localized: "\(totals.failed + totals.sessionsFailed) failed")
        }
        return line
    }

    static func agentName(_ kind: String?) -> String {
        switch kind {
        case "claude_code": "claude"
        case "codex": "codex"
        case let kind?: kind
        case nil: String(localized: "agent")
        }
    }

    static func truncated(_ command: String) -> String {
        guard command.count > commandBadgeLimit else { return command }
        return String(command.prefix(commandBadgeLimit - 1)) + "…"
    }

    static func abbreviate(_ path: String, home: String = NSHomeDirectory()) -> String {
        guard !home.isEmpty, path == home || path.hasPrefix(home + "/") else { return path }
        return "~" + path.dropFirst(home.count)
    }
}
