// Home's review: going through the results of work the person asked for.
//
// Finished units pile up; most need a glance, a few need another
// instruction. Review gathers the ones the person asked for and has not
// reviewed, within a time range, and offers three ways through them:
// one card at a time, an inbox, or a digest. Each ends in the same three
// actions: Looks Good, Later, Send Back.
import Foundation

enum FleetReviewScope: String, CaseIterable, Identifiable, Sendable {
    case today
    case day
    case week
    case all

    var id: String { rawValue }

    var title: String {
        switch self {
        case .today: String(localized: "Today")
        case .day: String(localized: "24 hours")
        case .week: String(localized: "This week")
        case .all: String(localized: "All")
        }
    }

    /// The earliest finish the scope includes; nil for all.
    func start(now: Date, calendar: Calendar = .current) -> Date? {
        switch self {
        case .today: calendar.startOfDay(for: now)
        case .day: now.addingTimeInterval(-24 * 3600)
        case .week: now.addingTimeInterval(-7 * 24 * 3600)
        case .all: nil
        }
    }
}

enum FleetReviewMode: String, CaseIterable, Identifiable, Sendable {
    case cards
    case inbox
    case digest

    var id: String { rawValue }

    var title: String {
        switch self {
        case .cards: String(localized: "Cards")
        case .inbox: String(localized: "Inbox")
        case .digest: String(localized: "Digest")
        }
    }

    var systemImage: String {
        switch self {
        case .cards: "rectangle.stack"
        case .inbox: "tray"
        case .digest: "list.bullet.rectangle"
        }
    }
}

extension FleetBoard {
    /// A result the person asked for: a managed Work (a person started it)
    /// or a unit whose latest prompt a person typed, not a muxa delivery, a
    /// helper hand-over or a background-task notice.
    static func isRequestedByPerson(_ work: FleetWork) -> Bool {
        if work.isProbableHelper || work.isOrphanHelper || work.isEmptySession { return false }
        if case .work = work.source { return true }
        guard let prompt = work.focus?.agent.lastPrompt?.trimmingCharacters(in: .whitespacesAndNewlines),
              !prompt.isEmpty else { return false }
        for machine in ["[muxa", "<task-notification>", "[Cross-session", "[Request interrupted"] where prompt.hasPrefix(machine) {
            return false
        }
        return true
    }

    /// Finished units the person asked for and has not reviewed, finished
    /// within the scope, newest first.
    static func reviewQueue(
        _ works: [FleetWork],
        marks: FleetReviewMarks,
        scope: FleetReviewScope,
        now: Date = .now
    ) -> [FleetWork] {
        let start = scope.start(now: now)
        return works
            .filter { work in
                guard work.lane == .finished, isRequestedByPerson(work), !marks.hides(work) else { return false }
                guard let start else { return true }
                return (work.lastChange ?? work.since ?? .distantPast) >= start
            }
            .sorted { ($0.lastChange ?? .distantPast) > ($1.lastChange ?? .distantPast) }
    }

    /// What the person asked, in a few lines.
    static func reviewAsked(_ work: FleetWork) -> String? {
        work.focus?.agent.lastPrompt.flatMap { FleetQuickLookText.digest($0, limit: 600) }
    }

    /// What came back: the agent's last response, else its recap.
    static func reviewResult(_ work: FleetWork) -> String? {
        guard let agent = work.focus?.agent else { return nil }
        let response = agent.lastResponse?.trimmingCharacters(in: .whitespacesAndNewlines)
        if let response, !response.isEmpty { return response }
        return agent.recap
    }
}
