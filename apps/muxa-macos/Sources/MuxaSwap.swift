import Foundation

// MARK: - Wire model (`muxa swap --json`)

/// One agent's swap result, as `muxa swap --json` prints it (a plan row from
/// `--dry-run`, or an executor report inside a job). Every field decodes
/// leniently: the CLI on a Fleet host may be newer than this app.
struct MuxaSwapOutcome: Decodable, Hashable, Sendable {
    enum Status: Hashable, Sendable {
        case swapped
        case planned
        case skipped
        case failed
        case other(String)

        init(rawValue: String) {
            switch rawValue {
            case "swapped": self = .swapped
            case "planned": self = .planned
            case "skipped": self = .skipped
            case "failed": self = .failed
            default: self = .other(rawValue)
            }
        }
    }

    let pane: String?
    let agentSessionID: String
    let kind: String?
    /// The aas account the agent ran on, when its launch line names one.
    let from: String?
    /// The account it moves (or moved) to.
    let to: String?
    let status: Status
    /// The line typed (or that would be typed) into the pane.
    let command: String?
    let note: String
    /// Why this agent is planned a follow-up: `rate_limited`, `cut_off` or
    /// `always`. Eligibility, not delivery.
    let continueReason: String?

    enum CodingKeys: String, CodingKey {
        case pane, kind, from, to, status, command, note
        case agentSessionID = "agent_session_id"
        case continueReason = "continue_reason"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        pane = try values.decodeIfPresent(String.self, forKey: .pane)
        agentSessionID = try values.decodeIfPresent(String.self, forKey: .agentSessionID) ?? ""
        kind = try values.decodeIfPresent(String.self, forKey: .kind)
        from = try values.decodeIfPresent(String.self, forKey: .from)
        to = try values.decodeIfPresent(String.self, forKey: .to)
        status = Status(rawValue: try values.decodeIfPresent(String.self, forKey: .status) ?? "")
        command = try values.decodeIfPresent(String.self, forKey: .command)
        note = try values.decodeIfPresent(String.self, forKey: .note) ?? ""
        continueReason = try values.decodeIfPresent(String.self, forKey: .continueReason)
    }

    var sendsFollowUp: Bool { continueReason != nil }
}

/// The follow-up the job's journal recorded for one target.
struct MuxaSwapFollowUp: Decodable, Hashable, Sendable {
    /// `delivered`, `refused` or `unconfirmed`: the transport's word only.
    let submission: String
    /// `prompt_hook`, `screen_activity` or `none_seen`, once the wait ended.
    let evidence: String?
    /// Sent to a session kept on its original account.
    let kept: Bool

    enum CodingKeys: String, CodingKey { case submission, evidence, kept }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        submission = try values.decodeIfPresent(String.self, forKey: .submission) ?? ""
        evidence = try values.decodeIfPresent(String.self, forKey: .evidence)
        kept = try values.decodeIfPresent(Bool.self, forKey: .kept) ?? false
    }
}

/// One report row of a daemon swap job.
struct MuxaSwapJobEntry: Decodable, Hashable, Sendable, Identifiable {
    let reportIndex: Int
    let pane: String?
    /// nil while the target is still to run, or was never reached.
    let disposition: String?
    let reason: String?
    /// Effects whose result was never recorded: they may or may not have
    /// happened.
    let uncertainEffects: Int
    let outcome: MuxaSwapOutcome?
    let continuation: String?
    let followUp: MuxaSwapFollowUp?

    var id: Int { reportIndex }

    enum CodingKeys: String, CodingKey {
        case pane, disposition, reason, outcome, continuation
        case reportIndex = "report_index"
        case uncertainEffects = "uncertain_effects"
        case followUp = "follow_up"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        reportIndex = try values.decodeIfPresent(Int.self, forKey: .reportIndex) ?? 0
        pane = try values.decodeIfPresent(String.self, forKey: .pane)
        disposition = try values.decodeIfPresent(String.self, forKey: .disposition)
        reason = try values.decodeIfPresent(String.self, forKey: .reason)
        uncertainEffects = try values.decodeIfPresent(Int.self, forKey: .uncertainEffects) ?? 0
        outcome = try values.decodeIfPresent(MuxaSwapOutcome.self, forKey: .outcome)
        continuation = try values.decodeIfPresent(String.self, forKey: .continuation)
        followUp = try values.decodeIfPresent(MuxaSwapFollowUp.self, forKey: .followUp)
    }

    /// What happened to this target, in one word the UI can colour.
    var phase: MuxaSwapEntryPhase {
        switch outcome?.status {
        case .swapped?: return .swapped
        case .failed?: return .failed
        case .skipped?: return .skipped
        default: break
        }
        switch disposition {
        case nil: return .pending
        case "failed"?: return .failed
        case "cancelled"?: return .cancelled
        case "skipped"?: return .skipped
        default: return .done
        }
    }
}

enum MuxaSwapEntryPhase: Hashable, Sendable {
    case pending, swapped, done, skipped, failed, cancelled
}

/// A daemon swap job, as `muxa swap --status <key> --json` prints it.
struct MuxaSwapJobView: Decodable, Hashable, Sendable {
    let jobID: String
    let nodeID: String
    let state: String
    let revision: UInt64
    let reason: String?
    let entries: [MuxaSwapJobEntry]
    let fault: String?
    let serviceFault: String?
    let cancelRequested: Bool

    enum CodingKeys: String, CodingKey {
        case state, revision, reason, entries, fault
        case jobID = "job_id"
        case nodeID = "node_id"
        case serviceFault = "service_fault"
        case cancelRequested = "cancel_requested"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        jobID = try values.decodeIfPresent(String.self, forKey: .jobID) ?? ""
        nodeID = try values.decodeIfPresent(String.self, forKey: .nodeID) ?? ""
        state = try values.decodeIfPresent(String.self, forKey: .state) ?? ""
        revision = try values.decodeIfPresent(UInt64.self, forKey: .revision) ?? 0
        reason = try values.decodeIfPresent(String.self, forKey: .reason)
        entries = try values.decodeIfPresent([MuxaSwapJobEntry].self, forKey: .entries) ?? []
        fault = try values.decodeIfPresent(String.self, forKey: .fault)
        serviceFault = try values.decodeIfPresent(String.self, forKey: .serviceFault)
        cancelRequested = try values.decodeIfPresent(Bool.self, forKey: .cancelRequested) ?? false
    }

    /// `accepted` and `running` are the two states a job can still move from.
    var isRunning: Bool { state == "accepted" || state == "running" }
    var failedCount: Int { entries.filter { $0.phase == .failed }.count }
    var swappedCount: Int { entries.filter { $0.phase == .swapped }.count }
}

/// What `muxa swap --detach --json` prints once the daemon accepted a job.
struct MuxaSwapAck: Decodable, Hashable, Sendable {
    let requestKey: String
    let jobID: String
    let state: String

    enum CodingKeys: String, CodingKey {
        case state
        case requestKey = "request_key"
        case jobID = "job_id"
    }
}

// MARK: - Requests

/// Which agents a swap covers. A swap on one host only ever touches that
/// host's agents.
enum MuxaSwapScope: Hashable, Sendable {
    /// These agents, by provider session id (unambiguous across backends).
    case agents([String])
    /// Every agent stopped by a usage cap.
    case limited
}

/// One swap as the sheet composes it.
struct MuxaSwapRequest: Hashable, Sendable {
    var scope: MuxaSwapScope
    /// nil or empty: `auto` (the best-ranked account with quota left).
    var account: String?
    /// Round-robin over every account with quota left (ignores `account`).
    var spread = false
    /// Also stop agents that are mid-turn.
    var force = false
    /// nil keeps `[swap] continue_text`.
    var followUp: String?
    /// Send no follow-up at all.
    var noFollowUp = false

    /// The `muxa swap` argv shared by plan and start. Every value travels as
    /// one `--flag=value` word: the Fleet allowlist takes no other shape.
    var arguments: [String] {
        var words = ["swap"]
        switch scope {
        case .agents(let ids): words.append(contentsOf: ids)
        case .limited: words.append("--limited")
        }
        if spread {
            words.append("--spread")
        } else if let account = account?.trimmingCharacters(in: .whitespacesAndNewlines),
                  !account.isEmpty {
            words.append("--to=\(account)")
        }
        if force { words.append("--force") }
        if noFollowUp {
            words.append("--no-continue")
        } else if let text = followUp?.trimmingCharacters(in: .whitespacesAndNewlines),
                  !text.isEmpty {
            words.append("--continue-text=\(text)")
        }
        words.append("--json")
        return words
    }

    var planArguments: [String] { arguments + ["--dry-run"] }

    func startArguments(requestKey: String) -> [String] {
        arguments + ["--detach", "--request-key=\(requestKey)"]
    }

    /// A request key the CLI accepts: ASCII letters, digits and `-_.:@`,
    /// at most 128 characters.
    static func mintRequestKey() -> String {
        "app-" + UUID().uuidString.lowercased()
    }
}

enum MuxaSwapCommand {
    static func status(_ key: String) -> [String] {
        ["swap", "--status=\(key)", "--wait-secs=0", "--json"]
    }

    static func cancel(_ key: String) -> [String] {
        ["swap", "--cancel=\(key)", "--wait-secs=0", "--json"]
    }

    static func retry(_ key: String, requestKey: String) -> [String] {
        ["swap", "--retry=\(key)", "--detach", "--request-key=\(requestKey)", "--json"]
    }

    /// The JSON the command printed, whatever its exit code: `muxa swap`
    /// exits non-zero when any agent failed, and still reports every one.
    static func decode<T: Decodable>(
        _ type: T.Type,
        from output: MuxaWorkCommandOutput
    ) throws -> T {
        let text = output.stdout.trimmingCharacters(in: .whitespacesAndNewlines)
        if !text.isEmpty, let value = try? JSONDecoder().decode(T.self, from: Data(text.utf8)) {
            return value
        }
        throw MuxaIPCError.server(failureMessage(output))
    }

    /// The last line of stderr names what went wrong (`muxa swap: …`); the
    /// rest is advice for a terminal user.
    static func failureMessage(_ output: MuxaWorkCommandOutput) -> String {
        let stderr = output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        if stderr.isEmpty {
            return String(localized: "muxa swap exited with \(output.exitCode)")
        }
        let cleaned = stderr
            .split(separator: "\n", omittingEmptySubsequences: true)
            .map { $0.replacingOccurrences(of: "Error: ", with: "") }
            .prefix(3)
            .joined(separator: "\n")
        return cleaned
    }
}

// MARK: - Swap center

/// A swap the app started or is following, on one host.
struct MuxaSwapActivity: Identifiable, Hashable, Sendable {
    let requestKey: String
    /// Fleet host alias, or nil for this Mac.
    let host: String?
    let startedAt: Date
    /// A short description of what was asked ("@api → work@claude").
    let title: String
    var job: MuxaSwapJobView?
    var error: String?
    var acknowledged = false

    var id: String { "\(host ?? "local")|\(requestKey)" }
    var isRunning: Bool { job?.isRunning ?? (error == nil) }
    var needsLook: Bool {
        guard !isRunning else { return false }
        return error != nil || (job?.failedCount ?? 0) > 0
    }
}

/// Every swap the app is following: starts them, polls them while they run,
/// and offers retry and cancel. One per app, like `MuxaAgentAttentionCenter`.
@MainActor
final class MuxaSwapCenter: ObservableObject {
    @Published private(set) var activities: [MuxaSwapActivity] = []
    /// The sheet's target, when one is open.
    @Published var sheet: MuxaSwapSheetTarget?

    private weak var model: AppModel?
    private var pollTask: Task<Void, Never>?
    static let pollInterval: Duration = .seconds(2)
    /// How many ended swaps the activity list keeps.
    static let keptEnded = 8

    func attach(_ model: AppModel) { self.model = model }

    var running: [MuxaSwapActivity] { activities.filter(\.isRunning) }
    var hasActivity: Bool { !activities.isEmpty }
    var needsLook: Bool { activities.contains { $0.needsLook && !$0.acknowledged } }

    func present(_ target: MuxaSwapSheetTarget) { sheet = target }

    /// What a request would do on `host`, doing none of it.
    func plan(_ request: MuxaSwapRequest, host: String?) async throws -> [MuxaSwapOutcome] {
        guard let model else { throw MuxaIPCError.server(String(localized: "Muxa is not connected")) }
        let output = try await run(model, host: host, arguments: request.planArguments)
        return try MuxaSwapCommand.decode([MuxaSwapOutcome].self, from: output)
    }

    /// Start a request as a daemon job on `host` and follow it.
    @discardableResult
    func start(_ request: MuxaSwapRequest, host: String?, title: String) async throws -> MuxaSwapActivity {
        guard let model else { throw MuxaIPCError.server(String(localized: "Muxa is not connected")) }
        let key = MuxaSwapRequest.mintRequestKey()
        var activity = MuxaSwapActivity(requestKey: key, host: host, startedAt: Date(), title: title)
        insert(activity)
        do {
            let output = try await run(model, host: host, arguments: request.startArguments(requestKey: key))
            _ = try MuxaSwapCommand.decode(MuxaSwapAck.self, from: output)
        } catch {
            activity.error = error.localizedDescription
            update(activity)
            throw error
        }
        await refresh(id: activity.id)
        schedulePolling()
        return activities.first { $0.id == activity.id } ?? activity
    }

    /// Follow a job something else started (an automation firing).
    func follow(requestKey: String, host: String?, title: String) {
        let activity = MuxaSwapActivity(requestKey: requestKey, host: host, startedAt: Date(), title: title)
        if activities.contains(where: { $0.id == activity.id }) { return }
        insert(activity)
        Task { await refresh(id: activity.id); schedulePolling() }
    }

    func cancel(_ activity: MuxaSwapActivity) async {
        guard let model else { return }
        do {
            let output = try await run(model, host: activity.host, arguments: MuxaSwapCommand.cancel(activity.requestKey))
            if let view = try? MuxaSwapCommand.decode(MuxaSwapJobView.self, from: output) {
                var updated = activity
                updated.job = view
                update(updated)
            }
        } catch {
            var updated = activity
            updated.error = error.localizedDescription
            update(updated)
        }
    }

    /// Retry an ended job's safely retryable targets as a new job.
    func retry(_ activity: MuxaSwapActivity) async {
        guard let model else { return }
        let key = MuxaSwapRequest.mintRequestKey()
        var retry = MuxaSwapActivity(
            requestKey: key,
            host: activity.host,
            startedAt: Date(),
            title: String(localized: "Retry: \(activity.title)")
        )
        insert(retry)
        do {
            let output = try await run(model, host: activity.host, arguments: MuxaSwapCommand.retry(activity.requestKey, requestKey: key))
            _ = try MuxaSwapCommand.decode(MuxaSwapAck.self, from: output)
            acknowledge(activity)
            await refresh(id: retry.id)
            schedulePolling()
        } catch {
            retry.error = error.localizedDescription
            update(retry)
        }
    }

    func acknowledge(_ activity: MuxaSwapActivity) {
        guard let index = activities.firstIndex(where: { $0.id == activity.id }) else { return }
        activities[index].acknowledged = true
    }

    func dismissEnded() {
        activities.removeAll { !$0.isRunning }
    }

    /// The running swap that covers `agentSessionID`, if any: the agent's row
    /// shows it instead of offering a second swap.
    func runningSwap(for agentSessionID: String) -> MuxaSwapActivity? {
        running.first { activity in
            activity.job?.entries.contains { $0.outcome?.agentSessionID == agentSessionID } ?? false
        }
    }

    // MARK: Private

    private func run(_ model: AppModel, host: String?, arguments: [String]) async throws -> MuxaWorkCommandOutput {
        let local = model.isLocalHost(host)
        return try await model.client.workCommand(host: local ? nil : host, arguments: arguments)
    }

    private func insert(_ activity: MuxaSwapActivity) {
        activities.insert(activity, at: 0)
        let ended = activities.enumerated().filter { !$0.element.isRunning }.map(\.offset)
        if ended.count > Self.keptEnded {
            let drop = Set(ended.dropFirst(Self.keptEnded))
            activities = activities.enumerated().filter { !drop.contains($0.offset) }.map(\.element)
        }
    }

    private func update(_ activity: MuxaSwapActivity) {
        guard let index = activities.firstIndex(where: { $0.id == activity.id }) else { return }
        activities[index] = activity
    }

    private func refresh(id: String) async {
        guard let model, let activity = activities.first(where: { $0.id == id }) else { return }
        do {
            let output = try await run(model, host: activity.host, arguments: MuxaSwapCommand.status(activity.requestKey))
            let view = try MuxaSwapCommand.decode(MuxaSwapJobView.self, from: output)
            guard var current = activities.first(where: { $0.id == id }) else { return }
            let finished = (current.job?.isRunning ?? true) && !view.isRunning
            current.job = view
            current.error = nil
            update(current)
            if finished { await model.refresh() }
        } catch {
            guard var current = activities.first(where: { $0.id == id }) else { return }
            // A status read that fails once is not the job failing; keep
            // following it and only show the error while no view exists.
            if current.job == nil { current.error = error.localizedDescription }
            update(current)
        }
    }

    private func schedulePolling() {
        guard pollTask == nil else { return }
        pollTask = Task { [weak self] in
            while let self, !Task.isCancelled {
                let ids = self.running.filter { $0.job != nil || $0.error == nil }.map(\.id)
                if ids.isEmpty { break }
                for id in ids { await self.refresh(id: id) }
                try? await Task.sleep(for: Self.pollInterval)
            }
            self?.pollTask = nil
        }
    }
}

/// What the swap sheet opens on.
struct MuxaSwapSheetTarget: Identifiable, Hashable, Sendable {
    /// Fleet host alias, or nil for this Mac.
    let host: String?
    let scope: MuxaSwapScope
    /// Shown in the sheet's title: "@api", "3 agents", "every limited agent".
    let label: String
    /// The provider the agents run, when they share one (`claude_code`,
    /// `codex`), so the account picker can offer only that provider's
    /// accounts.
    let agentKind: String?
    /// Opened on an agent that is mid-turn: stopping it needs `--force`.
    let interruptsWork: Bool

    var id: String { "\(host ?? "local")|\(label)" }
}
