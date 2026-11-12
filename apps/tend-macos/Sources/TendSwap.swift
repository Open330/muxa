import Foundation

// MARK: - Wire model (`tend swap --json`)

/// One agent's swap result, as `tend swap --json` prints it (a plan row from
/// `--dry-run`, or an executor report inside a job). Every field decodes
/// leniently: the CLI on a Fleet host may be newer than this app.
struct TendSwapOutcome: Decodable, Hashable, Sendable {
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
struct TendSwapFollowUp: Decodable, Hashable, Sendable {
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
struct TendSwapJobEntry: Decodable, Hashable, Sendable, Identifiable {
    let reportIndex: Int
    let pane: String?
    /// nil while the target is still to run, or was never reached.
    let disposition: String?
    let reason: String?
    /// Effects whose result was never recorded: they may or may not have
    /// happened.
    let uncertainEffects: Int
    let outcome: TendSwapOutcome?
    let continuation: String?
    let followUp: TendSwapFollowUp?

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
        outcome = try values.decodeIfPresent(TendSwapOutcome.self, forKey: .outcome)
        continuation = try values.decodeIfPresent(String.self, forKey: .continuation)
        followUp = try values.decodeIfPresent(TendSwapFollowUp.self, forKey: .followUp)
    }

    /// What happened to this target, in one word the UI can colour.
    var phase: TendSwapEntryPhase {
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

enum TendSwapEntryPhase: Hashable, Sendable {
    case pending, swapped, done, skipped, failed, cancelled
}

/// A daemon swap job, as `tend swap --status <key> --json` prints it.
struct TendSwapJobView: Decodable, Hashable, Sendable {
    let jobID: String
    let nodeID: String
    let state: String
    let revision: UInt64
    let reason: String?
    let entries: [TendSwapJobEntry]
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
        entries = try values.decodeIfPresent([TendSwapJobEntry].self, forKey: .entries) ?? []
        fault = try values.decodeIfPresent(String.self, forKey: .fault)
        serviceFault = try values.decodeIfPresent(String.self, forKey: .serviceFault)
        cancelRequested = try values.decodeIfPresent(Bool.self, forKey: .cancelRequested) ?? false
    }

    /// `accepted` and `running` are the two states a job can still move from.
    var isRunning: Bool { state == "accepted" || state == "running" }
    var failedCount: Int { entries.filter { $0.phase == .failed }.count }
    var swappedCount: Int { entries.filter { $0.phase == .swapped }.count }
}

/// What `tend swap --detach --json` prints once the daemon accepted a job.
struct TendSwapAck: Decodable, Hashable, Sendable {
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
enum TendSwapScope: Hashable, Sendable {
    /// These agents, by provider session id (unambiguous across backends).
    case agents([String])
    /// Every agent stopped by a usage cap.
    case limited
}

/// One swap as the sheet composes it.
struct TendSwapRequest: Hashable, Sendable {
    var scope: TendSwapScope
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

    /// The `tend swap` argv shared by plan and start. Every value travels as
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

enum TendSwapCommand {
    static func status(_ key: String) -> [String] {
        ["swap", "--status=\(key)", "--wait-secs=0", "--json"]
    }

    static func cancel(_ key: String) -> [String] {
        ["swap", "--cancel=\(key)", "--wait-secs=0", "--json"]
    }

    static func retry(_ key: String, requestKey: String) -> [String] {
        ["swap", "--retry=\(key)", "--detach", "--request-key=\(requestKey)", "--json"]
    }

    /// The JSON the command printed, whatever its exit code: `tend swap`
    /// exits non-zero when any agent failed, and still reports every one.
    static func decode<T: Decodable>(
        _ type: T.Type,
        from output: TendWorkCommandOutput
    ) throws -> T {
        let text = output.stdout.trimmingCharacters(in: .whitespacesAndNewlines)
        if !text.isEmpty, let value = try? JSONDecoder().decode(T.self, from: Data(text.utf8)) {
            return value
        }
        throw TendIPCError.server(failureMessage(output))
    }

    /// The last line of stderr names what went wrong (`tend swap: …`); the
    /// rest is advice for a terminal user.
    static func failureMessage(_ output: TendWorkCommandOutput) -> String {
        let stderr = output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        if stderr.isEmpty {
            return String(localized: "tend swap exited with \(output.exitCode)")
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
struct TendSwapActivity: Identifiable, Hashable, Sendable {
    let requestKey: String
    /// Fleet host alias, or nil for this Mac.
    let host: String?
    let startedAt: Date
    /// A short description of what was asked ("@api → work@claude").
    let title: String
    /// Agents the request named, so a swap shows as in progress for them
    /// before the job reports per-agent outcomes; empty for `--limited`.
    var targets: Set<String> = []
    var job: TendSwapJobView?
    /// The swap could not be started: nothing was handed to the daemon.
    var error: String?
    /// The last status read failed; the job may still be running.
    var statusError: String?
    /// Status reads failed in a row.
    var statusFailures = 0
    var acknowledged = false

    var id: String { "\(host ?? "local")|\(requestKey)" }
    /// Status reads stopped answering long enough to stop asking.
    var lostTrack: Bool { statusFailures >= TendSwapCenter.maxStatusFailures }
    var isRunning: Bool {
        if error != nil || lostTrack { return false }
        return job?.isRunning ?? true
    }
    var needsLook: Bool {
        guard !isRunning else { return false }
        return error != nil || lostTrack || (job?.failedCount ?? 0) > 0
    }
}

/// Every swap the app is following: starts them, polls them while they run,
/// and offers retry and cancel. One per app, like `TendAgentAttentionCenter`.
@MainActor
final class TendSwapCenter: ObservableObject {
    @Published private(set) var activities: [TendSwapActivity] = []
    /// The sheet's target, when one is open.
    @Published var sheet: TendSwapSheetTarget?
    /// The local daemon's swap service, from the last check; nil until
    /// checked or on a daemon that cannot say.
    @Published private(set) var health: TendSwapHealth? {
        // A switch policy covers nothing while swaps cannot be taken.
        didSet { TendLimitCoverage.setSwapAvailable(health?.isReady ?? true) }
    }
    /// Why the last check could not answer.
    @Published private(set) var healthError: String?

    /// This Mac's tendd says its account module is off: account settings
    /// and the usage chip have nothing to act on. False until a daemon
    /// that can say (`swap_health_module_v1`) says so.
    var accountModuleOff: Bool { health?.moduleOff == true }

    private var healthCheckedAt: Date?

    /// `refreshHealth` at most once a minute: for the app's reconcile loop.
    func refreshHealthIfStale(now: Date = Date()) async {
        if let checked = healthCheckedAt, now.timeIntervalSince(checked) < 60 { return }
        await refreshHealth()
    }

    /// Ask the local daemon how its swap service is. Remote hosts are not
    /// asked: their swaps report their own refusals.
    func refreshHealth() async {
        guard let model else { return }
        healthCheckedAt = Date()
        do {
            health = try await model.client.swapHealth()
            healthError = nil
        } catch {
            health = nil
            healthError = error.localizedDescription
        }
    }

    private weak var model: AppModel?
    private var pollTask: Task<Void, Never>?
    static let pollInterval: Duration = .seconds(2)
    /// Failed status reads in a row (about a minute) before the app stops
    /// following a swap and says it lost track of it.
    nonisolated static let maxStatusFailures = 30
    /// How many ended swaps the activity list keeps.
    static let keptEnded = 8

    func attach(_ model: AppModel) { self.model = model }

    var running: [TendSwapActivity] { activities.filter(\.isRunning) }
    var hasActivity: Bool { !activities.isEmpty }
    var needsLook: Bool { activities.contains { $0.needsLook && !$0.acknowledged } }

    func present(_ target: TendSwapSheetTarget) { sheet = target }

    /// What a request would do on `host`, doing none of it.
    func plan(_ request: TendSwapRequest, host: String?) async throws -> [TendSwapOutcome] {
        guard let model else { throw TendIPCError.server(String(localized: "Tend is not connected")) }
        let output = try await run(model, host: host, arguments: request.planArguments)
        return try TendSwapCommand.decode([TendSwapOutcome].self, from: output)
    }

    /// Start a request as a daemon job on `host` and follow it.
    @discardableResult
    func start(_ request: TendSwapRequest, host: String?, title: String) async throws -> TendSwapActivity {
        guard let model else { throw TendIPCError.server(String(localized: "Tend is not connected")) }
        let key = TendSwapRequest.mintRequestKey()
        var activity = TendSwapActivity(requestKey: key, host: host, startedAt: Date(), title: title)
        if case .agents(let ids) = request.scope { activity.targets = Set(ids) }
        insert(activity)
        do {
            let output = try await run(model, host: host, arguments: request.startArguments(requestKey: key))
            _ = try TendSwapCommand.decode(TendSwapAck.self, from: output)
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
        let activity = TendSwapActivity(requestKey: requestKey, host: host, startedAt: Date(), title: title)
        if activities.contains(where: { $0.id == activity.id }) { return }
        insert(activity)
        Task { await refresh(id: activity.id); schedulePolling() }
    }

    func cancel(_ activity: TendSwapActivity) async {
        guard let model else { return }
        do {
            let output = try await run(model, host: activity.host, arguments: TendSwapCommand.cancel(activity.requestKey))
            if let view = try? TendSwapCommand.decode(TendSwapJobView.self, from: output) {
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
    func retry(_ activity: TendSwapActivity) async {
        guard let model else { return }
        let key = TendSwapRequest.mintRequestKey()
        var retry = TendSwapActivity(
            requestKey: key,
            host: activity.host,
            startedAt: Date(),
            title: String(localized: "Retry: \(activity.title)")
        )
        insert(retry)
        do {
            let output = try await run(model, host: activity.host, arguments: TendSwapCommand.retry(activity.requestKey, requestKey: key))
            _ = try TendSwapCommand.decode(TendSwapAck.self, from: output)
            acknowledge(activity)
            await refresh(id: retry.id)
            schedulePolling()
        } catch {
            retry.error = error.localizedDescription
            update(retry)
        }
    }

    func acknowledge(_ activity: TendSwapActivity) {
        guard let index = activities.firstIndex(where: { $0.id == activity.id }) else { return }
        activities[index].acknowledged = true
    }

    func dismissEnded() {
        activities.removeAll { !$0.isRunning }
    }

    /// The running swap that covers `agentSessionID`, if any: the agent's row
    /// shows it instead of offering a second swap.
    func runningSwap(for agentSessionID: String) -> TendSwapActivity? {
        running.first { activity in
            activity.targets.contains(agentSessionID)
                || activity.job?.entries.contains { $0.outcome?.agentSessionID == agentSessionID } == true
        }
    }

    // MARK: Private

    private func run(_ model: AppModel, host: String?, arguments: [String]) async throws -> TendWorkCommandOutput {
        let local = model.isLocalHost(host)
        return try await model.client.workCommand(host: local ? nil : host, arguments: arguments)
    }

    private func insert(_ activity: TendSwapActivity) {
        activities.insert(activity, at: 0)
        let ended = activities.enumerated().filter { !$0.element.isRunning }.map(\.offset)
        if ended.count > Self.keptEnded {
            let drop = Set(ended.dropFirst(Self.keptEnded))
            activities = activities.enumerated().filter { !drop.contains($0.offset) }.map(\.element)
        }
    }

    private func update(_ activity: TendSwapActivity) {
        guard let index = activities.firstIndex(where: { $0.id == activity.id }) else { return }
        activities[index] = activity
    }

    private func refresh(id: String) async {
        guard let model, let activity = activities.first(where: { $0.id == id }) else { return }
        do {
            let output = try await run(model, host: activity.host, arguments: TendSwapCommand.status(activity.requestKey))
            let view = try TendSwapCommand.decode(TendSwapJobView.self, from: output)
            guard var current = activities.first(where: { $0.id == id }) else { return }
            let finished = (current.job?.isRunning ?? true) && !view.isRunning
            current.job = view
            current.statusError = nil
            current.statusFailures = 0
            update(current)
            if finished { await model.refresh() }
        } catch {
            guard var current = activities.first(where: { $0.id == id }) else { return }
            // The daemon took the job, so a failed read is not the swap
            // failing: keep following it, and stop only after a minute of
            // silence (a Fleet host gone away), saying so.
            current.statusError = error.localizedDescription
            current.statusFailures += 1
            update(current)
        }
    }

    private func schedulePolling() {
        guard pollTask == nil else { return }
        pollTask = Task { [weak self] in
            while let self, !Task.isCancelled {
                let ids = self.running.map(\.id)
                if ids.isEmpty { break }
                for id in ids { await self.refresh(id: id) }
                try? await Task.sleep(for: Self.pollInterval)
            }
            self?.pollTask = nil
        }
    }
}

/// What the swap sheet opens on.
struct TendSwapSheetTarget: Identifiable, Hashable, Sendable {
    /// Fleet host alias, or nil for this Mac.
    let host: String?
    let scope: TendSwapScope
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

// MARK: - Health

/// `swap_health`: whether the daemon's swap service takes jobs, and if not
/// why. Read-only.
struct TendSwapHealth: Decodable, Equatable, Sendable {
    enum Readiness: Equatable, Sendable {
        case ready, draining, halted, unavailable, unknown(String)

        init(raw: String) {
            switch raw {
            case "ready": self = .ready
            case "draining": self = .draining
            case "halted": self = .halted
            case "unavailable": self = .unavailable
            default: self = .unknown(raw)
            }
        }
    }

    let readiness: Readiness
    let reason: String?
    let fault: String?
    let jobsRunning: Int
    /// Upkeep that failed; jobs are still taken.
    let maintenance: String?
    /// The daemon's account module is off (`swap_health_module_v1`):
    /// accounts are not configured for it, or not in its build. False from
    /// a daemon that cannot say, which keeps the account settings shown.
    let moduleOff: Bool

    enum CodingKeys: String, CodingKey {
        case readiness, reason, fault, maintenance
        case jobsRunning = "jobs_running"
        case moduleOff = "module_off"
    }

    init(readiness: Readiness, reason: String? = nil, fault: String? = nil, jobsRunning: Int = 0, maintenance: String? = nil, moduleOff: Bool = false) {
        self.readiness = readiness
        self.reason = reason
        self.fault = fault
        self.jobsRunning = jobsRunning
        self.maintenance = maintenance
        self.moduleOff = moduleOff
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        readiness = Readiness(raw: try values.decode(String.self, forKey: .readiness))
        reason = try values.decodeIfPresent(String.self, forKey: .reason)
        fault = try values.decodeIfPresent(String.self, forKey: .fault)
        jobsRunning = try values.decodeIfPresent(Int.self, forKey: .jobsRunning) ?? 0
        maintenance = try values.decodeIfPresent(String.self, forKey: .maintenance)
        moduleOff = (try? values.decodeIfPresent(Bool.self, forKey: .moduleOff)) ?? false
    }

    var isReady: Bool { readiness == .ready }

    var title: String {
        switch readiness {
        case .ready: String(localized: "Account switching is ready")
        case .draining: String(localized: "Account switching is shutting down")
        case .halted: String(localized: "Account switching stopped after a fault")
        case .unavailable: String(localized: "Account switching is unavailable")
        case .unknown(let raw): String(localized: "Account switching: \(raw)")
        }
    }

    /// Why it is not ready, and what to do about it.
    var detail: String? {
        switch readiness {
        case .ready:
            return maintenance.map { String(localized: "Cleanup of old swap records failed: \($0)") }
        case .halted:
            let cause = fault ?? reason ?? ""
            return String(localized: "\(cause) Restart tendd to take swaps again.")
                .trimmingCharacters(in: .whitespaces)
        case .draining, .unavailable, .unknown:
            return reason
        }
    }
}
