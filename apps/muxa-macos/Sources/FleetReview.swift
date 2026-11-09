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
    /// The default: every result at a glance, by project.
    case digest
    case cards
    case inbox

    static let defaultsKey = "home.review.mode.v2"

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

// MARK: - Help from the Ask provider

/// Prompts for muxa's configured Ask provider, and reading its answers:
/// organizing a batch of results, and turning one reply from the person
/// into a message that fits each agent.
enum FleetReviewAssist {
    /// How much of each result goes into a prompt.
    static let resultLimit = 1_800

    private static func context(_ work: FleetWork, index: Int) -> String {
        var lines = ["### [\(index)] \(work.title)"]
        if let repo = work.repo { lines.append("Project: \(repo)") }
        if let focus = work.focus { lines.append("Agent: \(FleetBoard.owner(focus))") }
        if let asked = FleetBoard.reviewAsked(work) { lines.append("Asked: \(asked)") }
        let result = FleetBoard.reviewResult(work).map { String($0.prefix(resultLimit)) } ?? work.headline
        lines.append("Result:\n\(result)")
        return lines.joined(separator: "\n")
    }

    /// Asks for a short, organized digest of the results the person has to
    /// review: what got done, what needs them, what to send back.
    static func summaryPrompt(_ works: [FleetWork]) -> String {
        let items = works.enumerated().map { context($0.element, index: $0.offset + 1) }.joined(separator: "\n\n")
        return """
        You help a person review the results of work they asked coding agents to do. \
        Below are \(works.count) finished results. Write a short digest in Markdown:
        - Group related results under a heading per project or theme.
        - One line per result: [number] what was done, in plain words.
        - Then a section "Needs you" listing results that ask a question, report a failure, \
        are unfinished, or wait for a review or decision, with what the person should do.
        Be brief and concrete. Write in the language the person used in their requests. \
        Do not invent details that are not in the results.

        \(items)
        """
    }

    /// Asks for one message per agent, written from the person's single
    /// reply, as JSON keyed by the ids given.
    static func tailorPrompt(reply: String, targets: [(id: String, work: FleetWork)]) -> String {
        let items = targets.enumerated().map { offset, target in
            "id: \(target.id)\n" + context(target.work, index: offset + 1)
        }.joined(separator: "\n\n")
        return """
        A person reviewed several coding agents' results and wrote one reply meant for all of them. \
        Turn it into a separate message for each agent below. Each message:
        - is addressed to that agent's own work and makes sense on its own;
        - keeps only the parts of the reply that apply to that agent, made concrete for its result;
        - is an instruction the agent can act on; keep the person's intent, do not add new requests;
        - is in the language of the person's reply.
        If nothing in the reply applies to an agent, give it an empty message.
        Answer with JSON only, no prose: {"messages":[{"id":"<id>","message":"<text>"}]}

        The person's reply:
        \(reply)

        The agents:
        \(items)
        """
    }

    /// The messages from a tailoring answer, by id: the first JSON object in
    /// the text, ids it was not asked about dropped, empty messages kept as
    /// "nothing to send".
    static func parseTailored(_ answer: String, ids: Set<String>) -> [String: String]? {
        guard let start = answer.firstIndex(of: "{"), let end = answer.lastIndex(of: "}"), start < end,
              let data = String(answer[start...end]).data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let messages = object["messages"] as? [[String: Any]] else { return nil }
        var result: [String: String] = [:]
        for entry in messages {
            guard let id = entry["id"] as? String, ids.contains(id) else { continue }
            result[id] = (entry["message"] as? String ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        }
        return result
    }
}
