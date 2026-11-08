// Fleet Home: the fleet as units of work rather than as agents.
//
// A managed Work is one unit; an agent started on its own is wrapped in a
// one-stage unit, so nothing running is missing from the board. Each unit
// sits in the lane of whoever holds the ball: the person (a decision or a
// review), the agents (running), or nobody (finished, waiting for the next
// instruction).
import Foundation

enum FleetLane: Int, CaseIterable, Identifiable, Sendable {
    case needsYou
    case running
    case finished

    var id: Int { rawValue }
}

struct FleetWorkStage: Hashable, Sendable {
    enum State: Hashable, Sendable {
        case done
        case running
        case needsYou
        case waiting
    }

    let name: String
    /// Who does the stage, e.g. "claude · jiun-mini"; the program alone
    /// before an agent was spawned for it.
    let owner: String
    let state: State
}

struct FleetWork: Identifiable, Sendable {
    enum Source: Hashable, Sendable {
        case work(MuxaWorkIdentity)
        case agent(String)
    }

    let source: Source
    let title: String
    /// The last path component of the working directory.
    let repo: String?
    let lane: FleetLane
    let stages: [FleetWorkStage]
    /// One line: the question for the person, or what is happening now.
    let headline: String
    /// When the unit entered its current state (the oldest wait first).
    let since: Date?
    let agents: [MuxaHostedAgent]
    /// The agent to open for this unit: the one that needs the person, or
    /// the one doing the current stage.
    let focus: MuxaHostedAgent?
    /// A limit the configured policy will handle on its own.
    let handledLimit: Bool
    /// Changes whenever any agent of the unit or its pipeline moves on, so
    /// a mark made on one version of the unit never covers a later one.
    var revision: String = ""

    var id: String {
        switch source {
        case .work(let identity): "work:\(identity.id)"
        case .agent(let id): "agent:\(id)"
        }
    }

    var hostAliases: [String] {
        Array(Set(agents.map(\.host.alias))).sorted()
    }

    /// The stage to name on a card. Stages form a dependency graph, not a
    /// line, so the one that needs the person wins, then one that is running,
    /// then the first still waiting, else the last.
    var currentStageIndex: Int? {
        guard !stages.isEmpty else { return nil }
        return stages.firstIndex { $0.state == .needsYou }
            ?? stages.firstIndex { $0.state == .running }
            ?? stages.firstIndex { $0.state != .done }
            ?? stages.count - 1
    }

    var doneStageCount: Int { stages.filter { $0.state == .done }.count }
}

enum FleetBoard {
    static func works(groups: [MuxaWorkGroup], agents: [MuxaHostedAgent], now: Date = .now) -> [FleetWork] {
        let managedIDs = Set(groups.flatMap { $0.participants.map(\.id) })
        let managed = groups.map { work(for: $0, now: now) }
        let solo = agents
            .filter { !managedIDs.contains($0.id) && $0.agent.state != "stopped" }
            .map { work(for: $0, now: now) }
        return (managed + solo).sorted(by: order)
    }

    /// Needs-you units wait longest first; the others show the most recent
    /// change first.
    static func order(_ lhs: FleetWork, _ rhs: FleetWork) -> Bool {
        if lhs.lane != rhs.lane { return lhs.lane.rawValue < rhs.lane.rawValue }
        let left = lhs.since ?? .distantPast
        let right = rhs.since ?? .distantPast
        if left != right { return lhs.lane == .needsYou ? left < right : left > right }
        return lhs.id < rhs.id
    }

    // MARK: - Managed Work

    static func work(for group: MuxaWorkGroup, now: Date) -> FleetWork {
        let stages = stages(for: group, now: now)
        let lane = lane(for: stages)
        let attention = group.participants
            .filter { $0.agent.status(at: now).needsAttention }
            .sorted { $0.agent.status(at: now).priority < $1.agent.status(at: now).priority }
            .first
        let active = group.participants.first { $0.agent.status(at: now) == .working }
        // Keyed by the alias the desired agents use. A failure the limit
        // policy is handling is not a gate (see `stageState`).
        let handled = Set(group.participants
            .filter { isHandledLimit($0.agent, now: now) }
            .compactMap { group.desiredAgent(for: $0)?.alias })
        let blocked = group.pipelineRun?.aliases
            .filter { ($0.value.status == "blocked" || $0.value.status == "failed") && !handled.contains($0.key) }
            .sorted { $0.key < $1.key }
            .first
        let blockedAlias = blocked?.value
        // A blocked stage opens its own agent; with no agent for it, the
        // card offers Work Details rather than an unrelated branch's pane.
        let focus: MuxaHostedAgent?
        if let attention {
            focus = attention
        } else if let blockedAlias {
            focus = group.participants.first { group.desiredAgent(for: $0)?.alias == blocked?.key }
        } else {
            focus = active ?? group.participants.first
        }
        let headline: String
        if let attention {
            headline = question(for: attention.agent, now: now)
        } else if let blockedAlias {
            headline = blockedAlias.error.flatMap(firstLine)
                ?? String(localized: "Stage \(blocked?.key ?? blockedAlias.alias) is \(agentStateLabel(blockedAlias.status))")
        } else if let focus {
            headline = statusLine(for: focus.agent, now: now)
        } else {
            headline = String(localized: "Waiting for its agents to start")
        }
        return FleetWork(
            source: .work(group.identity),
            title: group.title,
            repo: group.cwd.flatMap(repoName),
            lane: lane,
            stages: stages,
            headline: headline,
            since: focus.flatMap { MuxaUsageTimestamp.parse($0.agent.stateEnteredAt ?? $0.agent.lastActivityAt) },
            agents: group.participants,
            focus: focus,
            handledLimit: group.participants.contains { isHandledLimit($0.agent, now: now) },
            revision: revision(agents: group.participants, run: group.pipelineRun)
        )
    }

    static func stages(for group: MuxaWorkGroup, now: Date) -> [FleetWorkStage] {
        if let run = group.pipelineRun, !run.desired.isEmpty {
            let planned = run.desired.map { desired in
                let participant = group.participants.first { group.desiredAgent(for: $0)?.alias == desired.alias }
                let state = stageState(
                    aliasStatus: run.aliases[desired.alias]?.status,
                    agent: participant?.agent,
                    now: now
                )
                return FleetWorkStage(
                    name: desired.role.flatMap(nonEmpty) ?? desired.alias,
                    owner: participant.map(owner) ?? desired.program,
                    state: state
                )
            }
            // An agent stamped with the Work but outside the pipeline (an ad hoc
            // peer or reviewer) is a stage of its own, so its question or its
            // activity still decides the lane.
            let extra = group.participants
                .filter { group.desiredAgent(for: $0) == nil }
                .map { adHocStage(for: $0, now: now) }
            return planned + extra
        }
        return group.participants.map { adHocStage(for: $0, now: now) }
    }

    private static func adHocStage(for participant: MuxaHostedAgent, now: Date) -> FleetWorkStage {
        FleetWorkStage(
            name: participant.pane?.agentRole.flatMap(nonEmpty)
                ?? participant.pane?.agentAlias.flatMap(nonEmpty)
                ?? kindName(participant.agent.kind),
            owner: owner(participant),
            state: stageState(aliasStatus: nil, agent: participant.agent, now: now)
        )
    }

    static func stageState(aliasStatus: String?, agent: MuxaAgent?, now: Date) -> FleetWorkStage.State {
        stageState(aliasStatus: aliasStatus, status: agent?.status(at: now), agentState: agent?.state)
    }

    /// The pipeline's durable gates outrank live activity: a blocked or
    /// failed stage stays in front of the person even while its agent keeps
    /// working, and a done stage is never demoted (the daemon does not demote
    /// it either). The one exception is a rate limit the policy covers: the
    /// daemon reports the 429 as a failed stage, but the policy resumes it,
    /// so it is still running. Live state decides everything else.
    static func stageState(aliasStatus: String?, status: MuxaAgentStatus?, agentState: String?) -> FleetWorkStage.State {
        if case .limited(let cap) = status, cap.covered { return aliasStatus == "done" ? .done : .running }
        if aliasStatus == "blocked" || aliasStatus == "failed" { return .needsYou }
        if status?.needsAttention == true { return .needsYou }
        if aliasStatus == "done" { return .done }
        switch status {
        case .working, .limited: return .running
        case .done: return .done
        default: break
        }
        if aliasStatus == "running" || aliasStatus == "working" || aliasStatus == "starting" { return .running }
        // An idle agent finished its turn; a stage with no agent yet waits.
        if agentState == "idle" { return .done }
        return .waiting
    }

    static func lane(for stages: [FleetWorkStage]) -> FleetLane {
        if stages.contains(where: { $0.state == .needsYou }) { return .needsYou }
        if stages.isEmpty || stages.allSatisfy({ $0.state == .done }) { return .finished }
        return .running
    }

    // MARK: - An agent on its own

    static func work(for hosted: MuxaHostedAgent, now: Date) -> FleetWork {
        let agent = hosted.agent
        let status = agent.status(at: now)
        let stage = FleetWorkStage(
            name: kindName(agent.kind),
            owner: owner(hosted),
            state: stageState(aliasStatus: nil, agent: agent, now: now)
        )
        let lane = lane(for: [stage])
        let title = agent.aiTitle.flatMap(nonEmpty)
            ?? agent.lastPrompt.flatMap(firstLine)
            ?? String(localized: "\(kindName(agent.kind)) on \(hosted.host.alias)")
        return FleetWork(
            source: .agent(hosted.id),
            title: title,
            repo: (hosted.pane?.currentPath).flatMap(nonEmpty).flatMap(repoName) ?? agent.cwd.flatMap(repoName),
            lane: lane,
            stages: [stage],
            headline: status.needsAttention ? question(for: agent, now: now) : statusLine(for: agent, now: now),
            since: MuxaUsageTimestamp.parse(agent.stateEnteredAt ?? agent.lastActivityAt),
            agents: [hosted],
            focus: hosted,
            handledLimit: isHandledLimit(agent, now: now),
            revision: revision(agents: [hosted], run: nil)
        )
    }

    /// Every agent's session, state and state entry, plus the pipeline's
    /// generation and stage statuses.
    static func revision(agents: [MuxaHostedAgent], run: MuxaPipelineRun?) -> String {
        var parts = agents
            .map { "\($0.id)|\($0.agent.state)|\($0.agent.stateEnteredAt ?? $0.agent.lastActivityAt ?? "")" }
            .sorted()
        if let run {
            parts.append("gen:\(run.generation)")
            parts += run.aliases.map { "\($0.key)=\($0.value.status)" }.sorted()
        }
        return parts.joined(separator: ";")
    }

    // MARK: - Wording

    /// What the agent asks of the person, in one line.
    static func question(for agent: MuxaAgent, now: Date) -> String {
        if let note = agent.lastNotification.flatMap(firstLine) { return note }
        return agent.status(at: now).label(now: now)
    }

    /// What the agent is doing or did last, in one line.
    static func statusLine(for agent: MuxaAgent, now: Date) -> String {
        if let recap = agent.recap.flatMap(firstLine) { return recap }
        if let response = agent.lastResponse.flatMap(firstLine) { return response }
        return agent.status(at: now).label(now: now)
    }

    static func isHandledLimit(_ agent: MuxaAgent, now: Date) -> Bool {
        if case .limited(let cap) = agent.status(at: now) { return cap.covered }
        return false
    }

    static func owner(_ hosted: MuxaHostedAgent) -> String {
        "\(kindName(hosted.agent.kind)) · \(hosted.host.alias)"
    }

    static func kindName(_ kind: String) -> String {
        switch kind {
        case "claude_code", "claude": "claude"
        case "codex": "codex"
        default: kind.replacingOccurrences(of: "_", with: " ")
        }
    }

    static func repoName(_ path: String) -> String? {
        let trimmed = path.hasSuffix("/") ? String(path.dropLast()) : path
        return nonEmpty((trimmed as NSString).lastPathComponent)
    }

    static func firstLine(_ text: String) -> String? {
        let line = text.split(whereSeparator: \.isNewline)
            .lazy
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .first { !$0.isEmpty }
        return line.map { $0.count > 160 ? String($0.prefix(159)) + "…" : $0 }
    }

    private static func nonEmpty(_ text: String) -> String? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}

// MARK: - Acting on a unit

/// What the person can type to a unit's agent from the inspector.
enum FleetReply: Equatable, Sendable {
    /// The agent asked a question and waits for the answer.
    case answer
    /// The unit finished; a new instruction sends it back to work.
    case sendBack
}

/// The agent and the exact question a reply is written for. A draft keeps
/// its target, so text written for one question never goes to another
/// agent, a replacement session, or the same agent's next question.
struct FleetReplyTarget: Equatable, Sendable {
    let kind: FleetReply
    let agentID: String
    let sessionID: String
    let state: String
    let stateEnteredAt: String?
    let owner: String

    init(kind: FleetReply, agent hosted: MuxaHostedAgent) {
        self.kind = kind
        agentID = hosted.id
        sessionID = hosted.agent.agentSessionID
        state = hosted.agent.state
        stateEnteredAt = hosted.agent.stateEnteredAt
        owner = FleetBoard.owner(hosted)
    }

    /// The agent at the pane is still the one, at the same question.
    func matches(_ agent: MuxaAgent?) -> Bool {
        guard let agent else { return false }
        return agent.agentSessionID == sessionID && agent.state == state && agent.stateEnteredAt == stateEnteredAt
    }
}

extension FleetBoard {
    static func replyTarget(for work: FleetWork) -> FleetReplyTarget? {
        guard let kind = reply(for: work), let focus = work.focus else { return nil }
        return FleetReplyTarget(kind: kind, agent: focus)
    }

    /// Text goes only to an agent at its prompt: one asking a question, or
    /// one done with its turn. A choice dialog, an error, or a working agent
    /// is handled in the pane, where the person sees what they answer.
    static func reply(for work: FleetWork) -> FleetReply? {
        guard let focus = work.focus, focus.pane != nil else { return nil }
        switch (work.lane, focus.agent.state) {
        case (.needsYou, "waiting_input"): return .answer
        case (.finished, "idle"), (.finished, "done"): return .sendBack
        default: return nil
        }
    }

    /// The empty state only when nothing is hidden either, so a board of
    /// reviewed units keeps the lanes and their Show reviewed control.
    static func showsEmptyBoard(visible: Int, reviewed: Int) -> Bool {
        visible == 0 && reviewed == 0
    }

    /// The agent waits on a choice only its pane can show.
    static func choosesInPane(_ work: FleetWork) -> Bool {
        work.lane == .needsYou && work.focus?.agent.state == "waiting_choice"
    }
}

/// Finished units the person reviewed and set aside. A mark holds the
/// unit's revision, so the unit returns as soon as any of its agents or its
/// pipeline moves on.
struct FleetReviewMarks: Equatable, Sendable {
    static let defaultsKey = "fleetHome.reviewed"

    private(set) var marks: [String: String] = [:]

    init(marks: [String: String] = [:]) { self.marks = marks }

    init(defaults: UserDefaults) {
        marks = defaults.dictionary(forKey: Self.defaultsKey) as? [String: String] ?? [:]
    }

    func save(to defaults: UserDefaults) {
        defaults.set(marks, forKey: Self.defaultsKey)
    }

    func hides(_ work: FleetWork) -> Bool {
        work.lane == .finished && marks[work.id] == work.revision
    }

    mutating func markReviewed(_ work: FleetWork) {
        marks[work.id] = work.revision
    }

    /// Drops marks for units no longer on the board.
    mutating func prune(keeping ids: Set<String>) {
        marks = marks.filter { ids.contains($0.key) }
    }
}

private extension MuxaAgent {
    func status(at now: Date) -> MuxaAgentStatus { MuxaAgentStatus(agent: self, now: now) }
}
