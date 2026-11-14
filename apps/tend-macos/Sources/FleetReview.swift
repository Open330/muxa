// Home's review: going through the results of work the person asked for.
//
// Finished units pile up; most need a glance, a few need another
// instruction. Review gathers the ones the person asked for and has not
// reviewed, within a time range, and offers three ways through them:
// one card at a time, an inbox, or a digest. Each ends in the same three
// actions: Looks Good, Later, Send Back.
import CryptoKit
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
    /// or a unit whose latest prompt a person typed, not a tend delivery, a
    /// helper hand-over or a background-task notice.
    static func isRequestedByPerson(_ work: FleetWork) -> Bool {
        if work.isProbableHelper || work.isOrphanHelper || work.isEmptySession { return false }
        // The daemon's record of who caused the latest prompt decides when it
        // has one: a person through tend, or a peer, automation or tend
        // itself. This applies to managed Work too: a Work whose latest turn
        // a peer started is not a result the person asked for.
        switch work.focus?.agent.lastPromptOrigin {
        case "person_via_tend": return true
        case "peer", "automation", "tend": return false
        default: break
        }
        if case .work = work.source { return true }
        guard let prompt = work.focus?.agent.lastPrompt?.trimmingCharacters(in: .whitespacesAndNewlines),
              !prompt.isEmpty else { return false }
        // Unknown origin: no tend delivery matched, so a person typed it or
        // the provider generated it. Provider envelopes (background-task
        // notices, cross-session messages, interruptions) are recognized by
        // text. This is a heuristic, not provenance; "[tend" covers prompts
        // from daemons older than prompt_origin_v1.
        for machine in ["[tend", "<task-notification>", "[Cross-session", "[Request interrupted"] where prompt.hasPrefix(machine) {
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

/// Prompts for tend's configured Ask provider, and reading its answers:
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


// MARK: - Review items (explicit requests)

enum TendReviewDisposition: String, Sendable {
    case accepted, returned, withdrawn
}

/// An explicit request that one result be looked at (`tend review`).
struct TendReviewItem: Decodable, Hashable, Identifiable, Sendable {
    struct Target: Decodable, Hashable, Sendable {
        let kind: String
        let provider: String?
        let sessionID: String?
        let workspace: String?
        let work: String?
        let alias: String?
        let requestID: String?

        enum CodingKeys: String, CodingKey {
            case kind, provider, workspace, work, alias
            case sessionID = "session_id"
            case requestID = "request_id"
        }

        var text: String {
            switch kind {
            case "work":
                let base = [workspace, work].compactMap { $0 }.joined(separator: "/")
                return alias.map { "\(base) · \($0)" } ?? base
            case "session":
                return [provider, sessionID.map { String($0.prefix(8)) }].compactMap { $0 }.joined(separator: " ")
            default:
                return requestID ?? kind
            }
        }
    }

    struct Actor: Decodable, Hashable, Sendable {
        let kind: String
        let label: String?
    }

    let id: String
    let target: Target
    let generation: UInt64
    let summary: String
    let anchor: String?
    let requestedAt: String?
    let requestedBy: Actor
    /// What the asker wants decided (`review_decision_v1`); nil from an
    /// older daemon or when they gave none.
    var decision: TendDecision? = nil
    /// Set once someone decided the item (`decision_history`).
    var disposition: Disposition? = nil
    var idempotencyKey: String? = nil
    /// What tendd ran itself (the Work's declared checks) before the item
    /// reached the person; nil when it ran nothing.
    var verification: TendVerification? = nil

    struct Disposition: Decodable, Hashable, Sendable {
        let kind: String
        let by: Actor
        let at: String
        let note: String?
    }

    enum CodingKeys: String, CodingKey {
        case id, target, generation, summary, anchor, decision, disposition, verification
        case requestedAt = "requested_at"
        case requestedBy = "requested_by"
        case idempotencyKey = "idempotency_key"
    }

    /// Who asked, as they declared it (not verified).
    var askedBy: String {
        let who = requestedBy.label ?? (requestedBy.kind == "agent" ? String(localized: "an agent") : String(localized: "a person"))
        return String(localized: "Asked by \(who)")
    }
}

/// A review store target, as Home names a result.
enum TendReviewTargetRef: Hashable, Sendable {
    case work(workspace: String, work: String)
    case session(provider: String, sessionID: String)

    var json: [String: Any] {
        switch self {
        case .work(let workspace, let work):
            return ["kind": "work", "workspace": workspace, "work": work]
        case .session(let provider, let sessionID):
            return ["kind": "session", "provider": provider, "session_id": sessionID]
        }
    }
}

extension FleetBoard {
    /// The review store's target for a result Home shows: its Work, or the
    /// session of the agent it names.
    static func reviewTarget(_ work: FleetWork) -> TendReviewTargetRef? {
        if case .work(let identity) = work.source {
            return .work(workspace: identity.workspaceID, work: identity.workID)
        }
        guard let agent = work.focus?.agent else { return nil }
        return .session(provider: agent.kind, sessionID: agent.agentSessionID)
    }

    /// One key per result: the unit and the exact state it was reviewed
    /// in, hashed to fit the store's key limit. Reviewing it again after it
    /// changed is a new result.
    static func reviewKey(_ work: FleetWork) -> String {
        let digest = SHA256.hash(data: Data("\(work.id)\n\(work.revision)".utf8))
        return "home:" + digest.map { String(format: "%02x", $0) }.joined()
    }
}
