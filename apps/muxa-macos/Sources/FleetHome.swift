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

    var id: String {
        switch source {
        case .work(let identity): "work:\(identity.id)"
        case .agent(let id): "agent:\(id)"
        }
    }

    var hostAliases: [String] {
        Array(Set(agents.map(\.host.alias))).sorted()
    }

    /// The stage to name on a card: the first not yet done, else the last.
    var currentStageIndex: Int? {
        guard !stages.isEmpty else { return nil }
        return stages.firstIndex { $0.state != .done } ?? stages.count - 1
    }
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
        let focus = attention ?? active ?? group.participants.first
        let blockedAlias = group.pipelineRun?.aliases.values
            .filter { $0.status == "blocked" || $0.status == "failed" }
            .sorted { $0.alias < $1.alias }
            .first
        let headline: String
        if let attention {
            headline = question(for: attention.agent, now: now)
        } else if let blockedAlias {
            headline = blockedAlias.error.flatMap(firstLine)
                ?? String(localized: "Stage \(blockedAlias.alias) is \(agentStateLabel(blockedAlias.status))")
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
            handledLimit: group.participants.contains { isHandledLimit($0.agent, now: now) }
        )
    }

    static func stages(for group: MuxaWorkGroup, now: Date) -> [FleetWorkStage] {
        if let run = group.pipelineRun, !run.desired.isEmpty {
            return run.desired.map { desired in
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
        }
        return group.participants.map { participant in
            FleetWorkStage(
                name: participant.pane?.agentRole.flatMap(nonEmpty)
                    ?? participant.pane?.agentAlias.flatMap(nonEmpty)
                    ?? kindName(participant.agent.kind),
                owner: owner(participant),
                state: stageState(aliasStatus: nil, agent: participant.agent, now: now)
            )
        }
    }

    static func stageState(aliasStatus: String?, agent: MuxaAgent?, now: Date) -> FleetWorkStage.State {
        if let agent {
            let status = agent.status(at: now)
            if status.needsAttention { return .needsYou }
            switch status {
            case .working, .limited: return .running
            case .done: return .done
            default: break
            }
        }
        switch aliasStatus {
        case "done": return .done
        case "blocked", "failed": return .needsYou
        case "running", "working", "starting": return .running
        default: break
        }
        // An idle agent finished its turn; a stage with no agent yet waits.
        if let agent, agent.state == "idle" { return .done }
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
            handledLimit: isHandledLimit(agent, now: now)
        )
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

private extension MuxaAgent {
    func status(at now: Date) -> MuxaAgentStatus { MuxaAgentStatus(agent: self, now: now) }
}
