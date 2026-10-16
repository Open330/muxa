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
    /// Leave running sessions alone (`--only-missing`). Only a muxad with
    /// `mux_snapshot_plan_v1` can be asked for anything else.
    @Published private(set) var onlyMissing = true
    /// Whether this muxad plans and restores without `--only-missing`.
    @Published private(set) var canFillRunningSessions = false

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

    /// Restore is offered only once the dry run — made with the current
    /// only-missing choice — says it would do something: a snapshot whose
    /// sessions all exist has nothing to do when running ones are left alone.
    var canRestore: Bool {
        guard supported, !isBusy, phase != .finished, let plan, !plan.run,
              plan.onlyMissing == onlyMissing else { return false }
        return SessionSnapshotTree.sessionsToRestore(plan) > 0
    }

    /// What the Restore button says it will do.
    var restoreButtonTitle: String {
        guard let plan, !plan.run else { return String(localized: "Restore") }
        return SessionSnapshotTree.restoreTitle(plan)
    }

    static let unsupportedMessage = String(
        localized: "This muxad doesn't support snapshots. Update it with `muxa upgrade`, then restart muxad."
    )

    func markUnsupported() {
        supported = false
        error = Self.unsupportedMessage
        hasLoaded = true
    }

    /// The daemon advertises `mux_snapshot_plan_v1`.
    func markCanFillRunningSessions() {
        canFillRunningSessions = true
    }

    /// Switch between leaving running sessions alone and filling in what
    /// they lack, and plan the selection again for the new choice.
    func setOnlyMissing(_ value: Bool) async {
        guard canFillRunningSessions || value, value != onlyMissing, !isBusy else { return }
        onlyMissing = value
        guard let selection, phase != .finished else { return }
        await select(selection, force: true)
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
                // Always plan again: sessions may have changed outside the
                // app, or a restore just finished, and `canRestore` must not
                // stand on a stale dry run.
                await select(pick.id, force: true)
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
        let onlyMissing = onlyMissing
        do {
            let plan = try await client.plan(id: id, onlyMissing: onlyMissing)
            // A later click may have moved on while this one was planning.
            if selection == id && self.onlyMissing == onlyMissing {
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
            var operation = try await client.restore(id: id, layoutOnly: layoutOnly, onlyMissing: onlyMissing)
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
    /// How the line compares with what runs now.
    enum LiveMark: Hashable {
        /// Recorded, not running now: a restore recreates it.
        case missing
        /// Recorded and running now.
        case running
        /// Running now, not in the snapshot; a restore never closes it.
        case new
    }

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
        case unconfirmed
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
    /// Set where it adds something: on every session, and on a window or
    /// pane only when it differs from the line above it.
    var mark: LiveMark? = nil
}

/// The dry run compared with what runs now, counted for the summary line.
struct SessionSnapshotComparison: Equatable {
    /// Recorded sessions that are not running: a restore recreates them.
    var toRecreate = 0
    /// Recorded sessions that are running now.
    var running = 0
    /// Sessions running now that the snapshot does not have.
    var newSinceSnapshot = 0
    /// Windows running sessions gained since the snapshot.
    var newWindows = 0
    /// Recorded windows missing from running sessions.
    var missingWindows = 0
    /// Recorded panes missing from running sessions, their missing
    /// windows' included: what a full restore splits off.
    var missingPanes = 0
    /// Whether the CLI compared at all; without it only session presence
    /// is known.
    var compared = false

    init() {}

    init(plan: MuxSnapshotPlan) {
        compared = plan.isCompared
        newSinceSnapshot = plan.newSessions?.count ?? 0
        for session in plan.sessions {
            guard session.isRunning else {
                toRecreate += 1
                continue
            }
            running += 1
            newWindows += session.newWindows?.count ?? 0
            for window in session.windows {
                if window.state == .missing { missingWindows += 1 }
                missingPanes += window.panes.filter { $0.state == .missing }.count
            }
        }
    }
}

/// Flattens a plan — or, after a restore, its report — into the indented
/// rows the sheet draws. Pure, so what a person reads is tested without a
/// view.
enum SessionSnapshotTree {
    /// Replay badges longer than this are cut; the full command is the help.
    static let commandBadgeLimit = 32

    static func rows(plan: MuxSnapshotPlan?) -> [SessionSnapshotTreeRow] {
        guard let plan else { return [] }
        // A report's comparison was taken before the restore ran; after it,
        // the results say what changed.
        let compare = !plan.run
        var rows: [SessionSnapshotTreeRow] = []
        for session in plan.sessions {
            let sessionID = "s:\(session.name)"
            let sessionMark: SessionSnapshotTreeRow.LiveMark = session.isRunning ? .running : .missing
            rows.append(SessionSnapshotTreeRow(
                id: sessionID, depth: 0, symbol: "rectangle.stack", title: session.name,
                detail: nil, badge: sessionBadge(session), help: nil,
                message: session.result == .failed ? session.error : nil,
                mark: compare ? sessionMark : nil
            ))
            let skipped = session.action == .skip
            for window in session.windows {
                let windowID = "\(sessionID)/\(window.index)"
                let windowMark = mark(window.state)
                rows.append(SessionSnapshotTreeRow(
                    id: windowID, depth: 1, symbol: "macwindow", title: "\(window.index): \(window.name)",
                    detail: panesLabel(window.panes.count),
                    badge: nil, help: nil, message: nil,
                    mark: compare && windowMark != sessionMark ? windowMark : nil
                ))
                for pane in window.panes {
                    // A skipped session keeps its panes on screen for
                    // reference, but nothing happens to them.
                    let badge = skipped ? nil : paneBadge(pane)
                    let paneMark = mark(pane.state)
                    rows.append(SessionSnapshotTreeRow(
                        id: "\(windowID)/\(pane.index)", depth: 2,
                        symbol: pane.action == .shell ? "terminal" : "play.rectangle",
                        title: abbreviate(pane.path), detail: nil, badge: badge,
                        help: pane.note ?? pane.command,
                        message: pane.result == .failed || pane.result == .unconfirmed ? pane.error : nil,
                        mark: compare && paneMark != (windowMark ?? sessionMark) ? paneMark : nil
                    ))
                }
            }
            if compare {
                for window in session.newWindows ?? [] {
                    rows += liveRows(window, id: "\(sessionID)/+\(window.index)", depth: 1, mark: .new)
                }
            }
        }
        guard compare else { return rows }
        for session in plan.newSessions ?? [] {
            let sessionID = "n:\(session.name)"
            rows.append(SessionSnapshotTreeRow(
                id: sessionID, depth: 0, symbol: "rectangle.stack", title: session.name,
                detail: nil, badge: nil, help: nil, message: nil, mark: .new
            ))
            for window in session.windows {
                rows += liveRows(window, id: "\(sessionID)/\(window.index)", depth: 1, mark: nil)
            }
        }
        return rows
    }

    /// A window that runs now and is not in the snapshot, with its panes'
    /// directories and programs.
    private static func liveRows(
        _ window: MuxSnapshotPlan.LiveWindow, id: String, depth: Int,
        mark: SessionSnapshotTreeRow.LiveMark?
    ) -> [SessionSnapshotTreeRow] {
        var rows = [SessionSnapshotTreeRow(
            id: id, depth: depth, symbol: "macwindow", title: "\(window.index): \(window.name)",
            detail: panesLabel(window.panes.count), badge: nil, help: nil, message: nil, mark: mark
        )]
        for pane in window.panes {
            rows.append(SessionSnapshotTreeRow(
                id: "\(id)/\(pane.index)", depth: depth + 1,
                symbol: pane.command == nil ? "terminal" : "play.rectangle",
                title: abbreviate(pane.path), detail: pane.command, badge: nil,
                help: pane.command, message: nil
            ))
        }
        return rows
    }

    private static func mark(_ state: MuxSnapshotPlan.LiveState?) -> SessionSnapshotTreeRow.LiveMark? {
        switch state {
        case .missing?: .missing
        case .present?: .running
        case .unknown?, nil: nil
        }
    }

    private static func panesLabel(_ count: Int) -> String {
        count == 1 ? String(localized: "1 pane") : String(localized: "\(count) panes")
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
            case .unconfirmed: return .unconfirmed
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

    /// The comparison in one line, for the footer: "3 sessions to
    /// recreate, 5 already running, 2 new since snapshot".
    static func summaryLine(_ plan: MuxSnapshotPlan) -> String {
        let counts = SessionSnapshotComparison(plan: plan)
        var parts: [String] = []
        switch counts.toRecreate {
        case 0: parts.append(String(localized: "No sessions to recreate"))
        case 1: parts.append(String(localized: "1 session to recreate"))
        default: parts.append(String(localized: "\(counts.toRecreate) sessions to recreate"))
        }
        if counts.running > 0 {
            parts.append(String(localized: "\(counts.running) already running"))
        }
        if counts.newSinceSnapshot > 0 {
            parts.append(String(localized: "\(counts.newSinceSnapshot) new since snapshot"))
        }
        if !plan.onlyMissing && counts.missingPanes > 0 {
            parts.append(counts.missingPanes == 1
                ? String(localized: "1 pane to add to running sessions")
                : String(localized: "\(counts.missingPanes) panes to add to running sessions"))
        }
        return parts.joined(separator: ", ")
    }

    /// Sessions a restore of this plan acts on: the missing ones, plus the
    /// running ones it fills in when it does not leave them alone.
    static func sessionsToRestore(_ plan: MuxSnapshotPlan) -> Int {
        plan.sessionsToCreate + (plan.onlyMissing ? 0 : plan.sessionsToFill)
    }

    /// The Restore button, saying what pressing it does.
    static func restoreTitle(_ plan: MuxSnapshotPlan) -> String {
        let create = plan.sessionsToCreate
        let fill = plan.onlyMissing ? 0 : plan.sessionsToFill
        switch (create, fill) {
        case (0, 0): return String(localized: "Nothing to Restore")
        case (1, 0): return String(localized: "Recreate 1 Session")
        case (_, 0): return String(localized: "Recreate \(create) Sessions")
        case (0, 1): return String(localized: "Fill In 1 Session")
        case (0, _): return String(localized: "Fill In \(fill) Sessions")
        default: return String(localized: "Recreate \(create), Fill In \(fill)")
        }
    }

    static func totalsLine(_ totals: MuxSnapshotPlan.Totals) -> String {
        var line = String(localized: "Created \(totals.sessionsCreated), skipped \(totals.sessionsSkipped); relaunched \(totals.relaunched) of \(totals.panes) panes")
        if totals.unconfirmed > 0 {
            line += " · " + String(localized: "\(totals.unconfirmed) not confirmed")
        }
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
