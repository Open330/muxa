// The daemon's decision queue (`decision_queue_v1`): everything pending on
// the person, in the order tendd ranks it, each with the asker's decision
// context: what to decide, the options, their recommendation and evidence.
//
// Every string here is declared by the asker and recorded by tendd; the app
// renders it and never derives or verifies it. Evidence is a pointer the
// person can open; opening it is their own click.
import Foundation

/// Daemons that serve `decision_queue` and `decision_history`.
let tendDecisionQueueCapability = "decision_queue_v1"

/// What the asker wants decided and why (`review_decision_v1`,
/// `collaboration_human_decision_v1`). Every field is optional on the wire.
struct TendDecision: Decodable, Hashable, Sendable {
    var needed: String?
    var options: [String] = []
    var recommendation: String?
    var evidence: [TendEvidence] = []

    enum CodingKeys: String, CodingKey {
        case needed, options, recommendation, evidence
    }

    init(needed: String? = nil, options: [String] = [], recommendation: String? = nil, evidence: [TendEvidence] = []) {
        self.needed = needed
        self.options = options
        self.recommendation = recommendation
        self.evidence = evidence
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        needed = try values.decodeIfPresent(String.self, forKey: .needed)
        options = (try? values.decodeIfPresent([String].self, forKey: .options)) ?? []
        recommendation = try values.decodeIfPresent(String.self, forKey: .recommendation)
        // One entry of a kind this app does not know never hides the rest.
        let raw = (try? values.decodeIfPresent([TendLenient<Provenanced>].self, forKey: .evidence)) ?? []
        var evidence: [TendEvidence] = []
        var ranByTend = Set<Int>()
        for entry in raw.compactMap(\.value) {
            if entry.verified { ranByTend.insert(evidence.count) }
            evidence.append(entry.evidence)
        }
        self.evidence = evidence
        self.ranByTend = ranByTend
    }

    /// Indexes into `evidence` of the entries tendd marked `verified`: its
    /// own copies of the checks it ran, never an agent's claim (tendd clears
    /// that flag on everything a client sends).
    var ranByTend: Set<Int> = []

    /// One evidence entry and whether tendd ran it.
    private struct Provenanced: Decodable, Sendable {
        let evidence: TendEvidence
        let verified: Bool

        private enum CodingKeys: String, CodingKey { case verified }

        init(from decoder: Decoder) throws {
            evidence = try TendEvidence(from: decoder)
            verified = (try? decoder.container(keyedBy: CodingKeys.self).decodeIfPresent(Bool.self, forKey: .verified)) ?? false
        }
    }

    var isEmpty: Bool {
        needed == nil && options.isEmpty && recommendation == nil && evidence.isEmpty
    }
}

/// A typed pointer to something the person can open.
enum TendEvidence: Decodable, Hashable, Sendable {
    case commit(sha: String, repo: String?)
    case pullRequest(url: String)
    case diff(range: String, repo: String?)
    case test(command: String, passed: Bool?, tail: String?)
    case path(String)

    private enum CodingKeys: String, CodingKey {
        case kind, sha, repo, url, range, command, passed, tail, path
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try values.decode(String.self, forKey: .kind)
        switch kind {
        case "commit":
            self = .commit(sha: try values.decode(String.self, forKey: .sha), repo: try values.decodeIfPresent(String.self, forKey: .repo))
        case "pull_request":
            self = .pullRequest(url: try values.decode(String.self, forKey: .url))
        case "diff":
            self = .diff(range: try values.decode(String.self, forKey: .range), repo: try values.decodeIfPresent(String.self, forKey: .repo))
        case "test":
            self = .test(
                command: try values.decode(String.self, forKey: .command),
                passed: try values.decodeIfPresent(Bool.self, forKey: .passed),
                tail: try values.decodeIfPresent(String.self, forKey: .tail)
            )
        case "path":
            self = .path(try values.decode(String.self, forKey: .path))
        default:
            throw DecodingError.dataCorruptedError(forKey: .kind, in: values, debugDescription: "unknown evidence kind \(kind)")
        }
    }

    /// A review anchor (`commit:SHA`, `diff:RANGE`, a URL) read as evidence,
    /// the way `tend review show` reads it. Anything else stays free-form.
    static func fromAnchor(_ anchor: String) -> TendEvidence? {
        let anchor = anchor.trimmingCharacters(in: .whitespacesAndNewlines)
        if anchor.hasPrefix("https://") || anchor.hasPrefix("http://") { return .pullRequest(url: anchor) }
        guard let colon = anchor.firstIndex(of: ":") else { return nil }
        let kind = anchor[..<colon].trimmingCharacters(in: .whitespaces)
        let value = anchor[anchor.index(after: colon)...].trimmingCharacters(in: .whitespaces)
        switch kind {
        case "commit" where TendRevisionSpec.isCommitSHA(value): return .commit(sha: value, repo: nil)
        case "diff" where TendRevisionSpec.isRange(value): return .diff(range: value, repo: nil)
        case "pr" where value.hasPrefix("https://") || value.hasPrefix("http://"): return .pullRequest(url: value)
        default: return nil
        }
    }

    /// The short label on the evidence chip.
    var label: String {
        switch self {
        case .commit(let sha, _): String(sha.prefix(8))
        case .pullRequest(let url): TendEvidence.pullRequestLabel(url)
        case .diff(let range, _): range
        case .test(let command, _, _): command
        case .path(let path): (path as NSString).lastPathComponent
        }
    }

    var systemImage: String {
        switch self {
        case .commit: "smallcircle.filled.circle"
        case .pullRequest: "arrow.triangle.pull"
        case .diff: "plusminus"
        case .test(_, let passed, _):
            switch passed {
            case true?: "checkmark.circle"
            case false?: "xmark.octagon"
            case nil: "testtube.2"
            }
        case .path: "doc"
        }
    }

    /// `owner/repo#12` for a GitHub pull request, the URL otherwise.
    static func pullRequestLabel(_ url: String) -> String {
        guard let components = URLComponents(string: url), components.host == "github.com" else { return url }
        let parts = components.path.split(separator: "/")
        if parts.count >= 4, parts[2] == "pull" {
            return "\(parts[0])/\(parts[1])#\(parts[3])"
        }
        return url
    }
}

/// Revision syntax the app accepts from evidence before handing it to git:
/// the same rule tendd validates with, and never anything that reads as an
/// option.
enum TendRevisionSpec {
    static func isCommitSHA(_ value: String) -> Bool {
        (4...64).contains(value.count) && value.allSatisfy(\.isHexDigit)
    }

    static func isRange(_ value: String) -> Bool {
        guard !value.isEmpty, !value.hasPrefix("-"), value.utf8.count <= 2048 else { return false }
        let allowed = Set("._/~^@{}-")
        return value.allSatisfy { ($0.isASCII && ($0.isLetter || $0.isNumber)) || allowed.contains($0) }
    }
}

// MARK: - Queue entries

/// A human request (`tend_request_attention`) as the queue lists it.
struct TendHumanRequestEntry: Decodable, Hashable, Sendable {
    let requestID: String
    let action: String?
    let body: String
    let decision: TendDecision?
    let fromSessionID: String
    let fromPane: String
    let fromAlias: String?
    let workspaceID: String?
    let workID: String?
    let createdAt: String

    enum CodingKeys: String, CodingKey {
        case action, body, decision
        case requestID = "request_id"
        case fromSessionID = "from_session_id"
        case fromPane = "from_pane"
        case fromAlias = "from_alias"
        case workspaceID = "workspace_id"
        case workID = "work_id"
        case createdAt = "created_at"
    }
}

/// An agent blocked in its pane. Home already shows these as Work cards in
/// Your turn, so the queue's copy is decoded but not drawn twice.
struct TendAgentWaitingEntry: Decodable, Hashable, Sendable {
    let sessionID: String
    let agentKind: String
    let pane: String?
    let state: String
    let message: String?
    let since: String

    enum CodingKeys: String, CodingKey {
        case pane, state, message, since
        case sessionID = "session_id"
        case agentKind = "agent_kind"
    }
}

enum TendPendingItem: Decodable, Hashable, Sendable {
    case humanRequest(TendHumanRequestEntry)
    case agentWaiting(TendAgentWaitingEntry)
    case review(TendReviewItem)

    private enum CodingKeys: String, CodingKey { case kind }

    init(from decoder: Decoder) throws {
        let kind = try decoder.container(keyedBy: CodingKeys.self).decode(String.self, forKey: .kind)
        switch kind {
        case "human_request": self = .humanRequest(try TendHumanRequestEntry(from: decoder))
        case "agent_waiting": self = .agentWaiting(try TendAgentWaitingEntry(from: decoder))
        case "review": self = .review(try TendReviewItem(from: decoder))
        default:
            throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "unknown queue item \(kind)"))
        }
    }
}

/// `decision_queue`'s answer. Entries this app cannot read are dropped, not
/// fatal; `unavailable` names sources tendd could not read.
struct TendPendingQueue: Decodable, Hashable, Sendable {
    var items: [TendPendingItem] = []
    var truncated = false
    var unavailable: [String] = []
    /// Entries tendd sent that this app could not read.
    var undecoded = 0

    /// Every source was read in full and every entry decoded: what the
    /// queue leaves out is not pending.
    var isComplete: Bool { !truncated && unavailable.isEmpty && undecoded == 0 }

    enum CodingKeys: String, CodingKey { case items, truncated, unavailable }

    init(items: [TendPendingItem] = [], truncated: Bool = false, unavailable: [String] = []) {
        self.items = items
        self.truncated = truncated
        self.unavailable = unavailable
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        let raw = (try? values.decodeIfPresent([TendLenient<TendPendingItem>].self, forKey: .items)) ?? []
        items = raw.compactMap(\.value)
        undecoded = raw.count - items.count
        truncated = (try? values.decodeIfPresent(Bool.self, forKey: .truncated)) ?? false
        unavailable = (try? values.decodeIfPresent([String].self, forKey: .unavailable)) ?? []
    }
}

/// What tendd established itself before a result reached the person: the
/// Work's declared checks, run by tendd (`work_verify_v1`). Unlike the
/// decision, nothing here is the agent's claim. Lenient: a field this app
/// does not read never hides the item.
struct TendVerification: Decodable, Hashable, Sendable {
    struct Check: Decodable, Hashable, Sendable {
        var command: String
        var name: String?
        var passed: Bool
        var exitCode: Int?
        var timedOut: Bool
        var tail: String

        enum CodingKeys: String, CodingKey {
            case command, name, passed, tail
            case exitCode = "exit_code"
            case timedOut = "timed_out"
        }

        init(command: String, name: String? = nil, passed: Bool, exitCode: Int? = nil, timedOut: Bool = false, tail: String = "") {
            self.command = command
            self.name = name
            self.passed = passed
            self.exitCode = exitCode
            self.timedOut = timedOut
            self.tail = tail
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            command = try values.decode(String.self, forKey: .command)
            name = try? values.decodeIfPresent(String.self, forKey: .name)
            passed = (try? values.decodeIfPresent(Bool.self, forKey: .passed)) ?? false
            exitCode = try? values.decodeIfPresent(Int.self, forKey: .exitCode)
            timedOut = (try? values.decodeIfPresent(Bool.self, forKey: .timedOut)) ?? false
            tail = (try? values.decodeIfPresent(String.self, forKey: .tail)) ?? ""
        }

        /// `unit`, or the command when the check has no name.
        var label: String { name?.nilIfEmpty ?? command }

        /// How a failed check ended: `exit 2`, `timed out`, `killed`.
        var ending: String? {
            guard !passed else { return nil }
            if timedOut { return String(localized: "timed out") }
            if let exitCode { return String(localized: "exit \(exitCode)") }
            return String(localized: "killed")
        }
    }

    var passed: Bool
    var ranAt: String?
    var checks: [Check]

    enum CodingKeys: String, CodingKey {
        case passed, checks
        case ranAt = "ran_at"
    }

    init(passed: Bool, ranAt: String? = nil, checks: [Check]) {
        self.passed = passed
        self.ranAt = ranAt
        self.checks = checks
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        checks = ((try? values.decodeIfPresent([TendLenient<Check>].self, forKey: .checks)) ?? []).compactMap(\.value)
        passed = ((try? values.decodeIfPresent(Bool.self, forKey: .passed)) ?? false) && checks.allSatisfy(\.passed)
        ranAt = try? values.decodeIfPresent(String.self, forKey: .ranAt)
    }

    /// `tend ran 3/3 checks, all passed`.
    var headline: String {
        let ok = checks.filter(\.passed).count
        return checks.count == 1
            ? String(localized: "tend ran 1 check: \(ok == 1 ? String(localized: "passed") : String(localized: "failed"))")
            : String(localized: "tend ran \(checks.count) checks: \(ok) passed")
    }
}

extension TendReviewItem {
    /// The typed evidence, then the free-form anchor when it reads as
    /// evidence and is not already listed (as `tend review show` does).
    /// tendd also copies its checks into the evidence (marked `verified`)
    /// for clients that read only the decision; with the checks shown on
    /// their own, those copies are left out. An agent's own entry stays,
    /// even for the same command.
    var allEvidence: [TendEvidence] {
        let all = decision?.evidence ?? []
        let skip = verification?.checks.isEmpty == false ? (decision?.ranByTend ?? []) : []
        var evidence = all.enumerated().filter { !skip.contains($0.offset) }.map(\.element)
        if let anchor = anchor.flatMap(TendEvidence.fromAnchor), !evidence.contains(anchor) {
            evidence.append(anchor)
        }
        return evidence
    }
}

// MARK: - What Home shows

/// One decision waiting on the person, from the daemon's queue or, for
/// another host or a daemon without the queue, from the console mailbox
/// and the review list.
struct FleetDecision: Identifiable, Hashable, Sendable {
    enum Source: Hashable, Sendable {
        /// Answered with a human-request reply on `host`.
        case humanRequest(requestID: String)
        /// Decided with `review_dispose` on this Mac's daemon.
        case review(TendReviewItem)
    }

    let source: Source
    let host: TendFleetHostIdentity
    /// The question or the result to look at, as the asker wrote it.
    let summary: String
    let decision: TendDecision?
    /// Who asked, as they declared it.
    let asker: String?
    /// The asking agent's session, to find its checkout and its pane.
    let askerSessionID: String?
    let workID: String?
    let since: Date?
    let evidence: [TendEvidence]
    /// The asking agent as Home sees it, when it is on the board.
    var askerAgentID: String?
    var askerCwd: String?

    /// The asking agent's project, for Home's project filter.
    var repo: String? {
        askerCwd.map { ($0 as NSString).lastPathComponent }
    }

    var id: String {
        switch source {
        case .humanRequest(let requestID): "human:\(host.alias):\(requestID)"
        case .review(let item): "review:\(item.id)"
        }
    }

    var isHumanRequest: Bool {
        if case .humanRequest = source { return true }
        return false
    }

    /// What to decide: the asker's question when they gave one, the summary
    /// otherwise.
    var title: String {
        if let needed = decision?.needed?.trimmingCharacters(in: .whitespacesAndNewlines), !needed.isEmpty {
            return needed
        }
        return summary
    }

    /// The summary under the title, when the title is the question.
    var detail: String? {
        guard title != summary else { return nil }
        let trimmed = summary.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }

    var options: [String] {
        (decision?.options ?? []).filter { !$0.trimmingCharacters(in: .whitespaces).isEmpty }
    }

    var recommendation: String? {
        decision?.recommendation?.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty
    }

    /// The checks tendd ran itself, shown ahead of the asker's account.
    var verification: TendVerification? {
        guard case .review(let item) = source, let verification = item.verification, !verification.checks.isEmpty else { return nil }
        return verification
    }

    init(humanRequest entry: TendHumanRequestEntry, host: TendFleetHostIdentity) {
        source = .humanRequest(requestID: entry.requestID)
        self.host = host
        summary = entry.body
        decision = entry.decision
        asker = entry.fromAlias.map { "@\($0)" } ?? (entry.fromPane.isEmpty ? nil : entry.fromPane)
        askerSessionID = entry.fromSessionID.nilIfEmpty
        workID = entry.workID
        since = TendUsageTimestamp.parse(entry.createdAt)
        evidence = entry.decision?.evidence ?? []
    }

    init(humanRequest message: TendOperatorMessage) {
        let request = message.request
        source = .humanRequest(requestID: request.id)
        host = message.host
        summary = request.body
        decision = request.decision
        asker = request.from.label
        askerSessionID = request.from.agentSessionID.nilIfEmpty
        workID = request.workID
        since = TendUsageTimestamp.parse(request.createdAt)
        evidence = request.decision?.evidence ?? []
    }

    init(review item: TendReviewItem, host: TendFleetHostIdentity) {
        source = .review(item)
        self.host = host
        summary = item.summary
        decision = item.decision
        asker = item.requestedBy.label ?? (item.requestedBy.kind == "agent" ? String(localized: "an agent") : String(localized: "a person"))
        askerSessionID = item.target.sessionID
        workID = item.target.work
        since = item.requestedAt.flatMap(TendUsageTimestamp.parse)
        evidence = item.allEvidence
    }
}

enum FleetDecisions {
    /// The queue Home shows. With the daemon's queue: its human requests and
    /// review items in its order (waiting agents are already Work cards),
    /// then the console mailbox's human requests it does not hold: other
    /// hosts', and this Mac's only when the queue read was partial (a source
    /// unavailable, truncated, an entry this app could not decode). A
    /// complete queue is authoritative for this Mac: an older mailbox copy
    /// of a request answered since never comes back. Without
    /// it: this Mac's open review items, then every host's human requests.
    /// One request is listed once.
    static func merge(
        queue: TendPendingQueue?,
        reviews: [TendReviewItem],
        messages: [TendOperatorMessage],
        localHost: TendFleetHostIdentity
    ) -> [FleetDecision] {
        var result: [FleetDecision] = []
        var seen = Set<String>()
        func add(_ decision: FleetDecision) {
            if seen.insert(decision.id).inserted { result.append(decision) }
        }
        if let queue {
            for item in queue.items {
                switch item {
                case .humanRequest(let entry): add(FleetDecision(humanRequest: entry, host: localHost))
                case .review(let review): add(FleetDecision(review: review, host: localHost))
                case .agentWaiting: break
                }
            }
        } else {
            reviews.forEach { add(FleetDecision(review: $0, host: localHost)) }
        }
        let pending = messages
            .filter { $0.isHumanRequest && $0.request.needsHumanResponse }
            .filter { queue?.isComplete != true || $0.host.alias != localHost.alias }
            .sorted { $0.request.createdAt < $1.request.createdAt }
        pending.forEach { add(FleetDecision(humanRequest: $0)) }
        return result
    }
}

// MARK: - History

/// A human request and the answer it got.
struct TendHumanAnswerEntry: Decodable, Hashable, Sendable {
    let request: TendHumanRequestEntry
    /// `completed` for an answer; `declined`, `failed` or `blocked` when the
    /// person said so.
    let status: String
    let answer: String
    let answeredAt: String

    enum CodingKeys: String, CodingKey {
        case status, answer
        case answeredAt = "answered_at"
    }

    init(request: TendHumanRequestEntry, status: String, answer: String, answeredAt: String) {
        self.request = request
        self.status = status
        self.answer = answer
        self.answeredAt = answeredAt
    }

    init(from decoder: Decoder) throws {
        // The request's fields sit beside the answer's (serde flatten).
        request = try TendHumanRequestEntry(from: decoder)
        let values = try decoder.container(keyedBy: CodingKeys.self)
        status = try values.decode(String.self, forKey: .status)
        answer = try values.decode(String.self, forKey: .answer)
        answeredAt = try values.decode(String.self, forKey: .answeredAt)
    }
}

enum TendDecidedItem: Decodable, Hashable, Sendable {
    case review(TendReviewItem)
    case humanAnswer(TendHumanAnswerEntry)

    private enum CodingKeys: String, CodingKey { case kind }

    init(from decoder: Decoder) throws {
        let kind = try decoder.container(keyedBy: CodingKeys.self).decode(String.self, forKey: .kind)
        switch kind {
        case "review": self = .review(try TendReviewItem(from: decoder))
        case "human_answer": self = .humanAnswer(try TendHumanAnswerEntry(from: decoder))
        default:
            throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "unknown decided item \(kind)"))
        }
    }
}

/// `decision_history`'s answer, newest first.
struct TendDecisionHistory: Decodable, Hashable, Sendable {
    var items: [TendDecidedItem] = []
    var truncated = false
    var unavailable: [String] = []
    /// Entries tendd sent that this app could not read.
    var undecoded = 0

    enum CodingKeys: String, CodingKey { case items, truncated, unavailable }

    init(items: [TendDecidedItem] = [], truncated: Bool = false, unavailable: [String] = []) {
        self.items = items
        self.truncated = truncated
        self.unavailable = unavailable
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        let raw = (try? values.decodeIfPresent([TendLenient<TendDecidedItem>].self, forKey: .items)) ?? []
        items = raw.compactMap(\.value)
        undecoded = raw.count - items.count
        truncated = (try? values.decodeIfPresent(Bool.self, forKey: .truncated)) ?? false
        unavailable = (try? values.decodeIfPresent([String].self, forKey: .unavailable)) ?? []
    }
}

/// Text the review store accepts: no control characters but newline and
/// tab, at most `maxBytes` of UTF-8, cut on a character boundary.
enum TendRecordText {
    static func bounded(_ text: String, maxBytes: Int) -> String {
        var scalars = String.UnicodeScalarView()
        for scalar in text.unicodeScalars
        where scalar == "\n" || scalar == "\t" || scalar.properties.generalCategory != .control {
            scalars.append(scalar)
        }
        let cleaned = String(scalars).trimmingCharacters(in: .whitespacesAndNewlines)
        guard cleaned.utf8.count > maxBytes else { return cleaned }
        var result = ""
        var used = 0
        let ellipsis = "…"
        for character in cleaned {
            let size = character.utf8.count
            if used + size > maxBytes - ellipsis.utf8.count { break }
            result.append(character)
            used += size
        }
        return result + ellipsis
    }
}

private extension String {
    var nilIfEmpty: String? { isEmpty ? nil : self }
}
