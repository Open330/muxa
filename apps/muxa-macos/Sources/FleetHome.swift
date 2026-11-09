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
    var lane: FleetLane
    let stages: [FleetWorkStage]
    /// One line: the question for the person, or what is happening now.
    var headline: String
    /// When the unit entered its current state (the oldest wait first).
    var since: Date?
    let agents: [MuxaHostedAgent]
    /// The agent to open for this unit: the one that needs the person, or
    /// the one doing the current stage.
    var focus: MuxaHostedAgent?
    /// A limit the configured policy will handle on its own.
    var handledLimit: Bool
    /// Changes whenever any agent of the unit or its pipeline moves on, so
    /// a mark made on one version of the unit never covers a later one.
    var revision: String = ""
    /// The latest moment any of the unit's agents changed state: when a
    /// finished unit finished, whichever agent the card names.
    var lastChange: Date?
    /// Sessions other agents started to help with this unit (peer
    /// requests), folded under it rather than shown as units of their own.
    var helpers: [MuxaHostedAgent] = []
    /// An agent working on someone's peer request whose requester is gone.
    var isOrphanHelper = false
    /// An agent whose prompt looks like a typed hand-over of a muxa
    /// request: probably a helper, but unconfirmed, so it keeps its own
    /// card and only folds behind a lane's "possible helpers" row.
    var isProbableHelper = false

    var id: String {
        switch source {
        case .work(let identity): "work:\(identity.id)"
        case .agent(let id): "agent:\(id)"
        }
    }

    /// Every host the unit runs on, its helpers' included, so a host filter
    /// never hides a helper's question.
    var hostAliases: [String] {
        Array(Set((agents + helpers).map(\.host.alias))).sorted()
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
        let live = agents.filter { $0.agent.state != "stopped" }
        // Helper sessions fold under the root of their request chain.
        let index = HelperIndex(agents: live)
        var helpersByRoot: [String: [MuxaHostedAgent]] = [:]
        var foldedIDs = Set<String>()
        for hosted in live where !managedIDs.contains(hosted.id) {
            guard let root = index.root(of: hosted.id) else { continue }
            helpersByRoot[root, default: []].append(hosted)
            foldedIDs.insert(hosted.id)
        }
        let managed = groups.map { group in
            attach(helpers: group.participants.flatMap { helpersByRoot[$0.id] ?? [] }, to: work(for: group, now: now), now: now)
        }
        let solo = live
            .filter { !managedIDs.contains($0.id) && !foldedIDs.contains($0.id) }
            .map { hosted in
                var unit = attach(helpers: helpersByRoot[hosted.id] ?? [], to: work(for: hosted, now: now), now: now)
                unit.isOrphanHelper = index.isHelper(hosted.id)
                unit.isProbableHelper = helperRequest(hosted.agent)?.confirmed == false
                return unit
            }
        return (managed + solo).sorted(by: order)
    }

    // MARK: - Helper sessions

    /// Who sent the peer request an agent is working on, read from the
    /// envelope muxa delivers it in.
    struct HelperRequest: Equatable, Sendable {
        /// The requester's agent session, when the envelope names it.
        var sessionID: String?
        /// The requester's `name@%pane` label.
        var label: String?
        /// Delivered by muxa (an envelope it writes), so who asked is known.
        /// A hand-over an agent typed is only a hint: nothing says who wrote
        /// it, so it never makes the agent part of another unit.
        var confirmed = true
    }

    /// The peer request in an agent's latest prompt, in any of the forms
    /// it arrives in:
    /// - `[muxa] Claimed; …` with its JSON record (the requester's session);
    /// - `[muxa:req_…] Review from codex@%39. …` (the requester's pane);
    /// - `[muxa:req_…] New Review request via … representing claude@%22`;
    /// - a prompt an agent typed that names a request (`req_…`) and its own
    ///   pane, e.g. `Main Claude %48 요청 req_… 를 claim하고 …`.
    /// A reply notice (`… Completed reply from … is ready`) goes to the
    /// requester and a console dispatch comes from a person: neither makes
    /// the agent a helper.
    static func helperRequest(_ agent: MuxaAgent) -> HelperRequest? {
        guard let raw = agent.lastPrompt else { return nil }
        let prompt = String(raw.drop(while: \.isWhitespace))
        if prompt.hasPrefix("[muxa] Claimed;") {
            guard let start = prompt.range(of: "--- peer request ---\n"),
                  let end = prompt.range(of: "\n--- end peer request ---", range: start.upperBound..<prompt.endIndex),
                  let data = String(prompt[start.upperBound..<end.lowerBound]).data(using: .utf8),
                  let record = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let from = record["from"] as? [String: Any] else { return nil }
            let request = HelperRequest(sessionID: from["agent_session_id"] as? String, label: from["label"] as? String)
            return request.sessionID == nil && request.label == nil ? nil : request
        }
        let firstLine = prompt.split(separator: "\n", maxSplits: 1).first.map(String.init) ?? ""
        if firstLine.hasPrefix("[muxa:req_") {
            if firstLine.contains("Completed reply") || firstLine.contains("replies are ready") { return nil }
            for marker in ["representing ", " from "] {
                if let range = firstLine.range(of: marker),
                   let label = firstLine[range.upperBound...]
                       .split(whereSeparator: \.isWhitespace).first
                       .map({ String($0).trimmingCharacters(in: CharacterSet(charactersIn: ",.;()")) }),
                   label.contains("@%") {
                    return HelperRequest(sessionID: nil, label: label)
                }
            }
            return nil
        }
        if firstLine.hasPrefix("[") { return nil }
        // Typed by an agent handing over a muxa request: only a prompt that
        // tells the agent to claim it (`muxa msg accept req_…` or
        // `muxa_inbox(request_id=…)`) counts, not one that merely mentions
        // a request or a pane.
        guard firstLine.range(of: #"(muxa msg accept|muxa_inbox\(request_id=\"?)\s*req_[0-9a-f]{8,}_[0-9a-f]+"#, options: .regularExpression) != nil,
              let pane = firstLine.range(of: #"(?<![A-Za-z0-9])%[0-9]+(?![0-9A-Fa-f])"#, options: .regularExpression) else { return nil }
        return HelperRequest(sessionID: nil, label: "agent@\(firstLine[pane])", confirmed: false)
    }

    /// Who asked whom for help, worked out once per board: each prompt is
    /// parsed once, requesters are looked up through indexes, and chain
    /// roots are memoized.
    struct HelperIndex {
        private var parent: [String: String] = [:]
        private var helpers: Set<String> = []
        private var roots: [String: String] = [:]

        init(agents: [MuxaHostedAgent]) {
            var bySession: [String: [MuxaHostedAgent]] = [:]
            var byPane: [String: [MuxaHostedAgent]] = [:]
            for hosted in agents {
                bySession[hosted.agent.agentSessionID, default: []].append(hosted)
                if let pane = hosted.pane {
                    byPane[Self.paneKey(hosted.host.alias, pane.endpointSocket, pane.paneID), default: []].append(hosted)
                }
            }
            for hosted in agents {
                guard let request = FleetBoard.helperRequest(hosted.agent), request.confirmed else { continue }
                helpers.insert(hosted.id)
                let candidates: [MuxaHostedAgent]
                if let session = request.sessionID {
                    // The envelope names the requester: that session or none,
                    // never whoever took over its pane.
                    candidates = bySession[session] ?? []
                } else if let label = request.label, let at = label.lastIndex(of: "@"), let pane = hosted.pane {
                    // A pane label is local to the helper's own tmux server.
                    let paneID = String(label[label.index(after: at)...])
                    candidates = byPane[Self.paneKey(hosted.host.alias, pane.endpointSocket, paneID)] ?? []
                } else {
                    candidates = []
                }
                let others = candidates.filter { $0.id != hosted.id }
                if others.count == 1, let requester = others.first { parent[hosted.id] = requester.id }
            }
            // Each chain is walked once: a helper folds to the first agent
            // up its chain that asked nobody; a loop folds nowhere.
            for id in helpers where roots[id] == nil {
                guard parent[id] != nil else { continue }
                var chain = [id]
                var seen: Set<String> = [id]
                var current = id
                var result: String?
                while let next = parent[current] {
                    guard seen.insert(next).inserted else { result = nil; break }
                    if let known = roots[next] { result = known; break }
                    if parent[next] == nil { result = next; break }
                    chain.append(next)
                    current = next
                }
                if let result {
                    for member in chain { roots[member] = result }
                }
            }
        }

        private static func paneKey(_ host: String, _ socket: String?, _ pane: String) -> String {
            "\(host)|\(socket ?? "")|\(pane)"
        }

        /// Whether the agent is working on someone's peer request.
        func isHelper(_ id: String) -> Bool { helpers.contains(id) }

        /// Where a helper folds: the first agent up its chain that asked
        /// nobody. Nil for an agent that is not helping, one whose
        /// requester cannot be found, or a chain that loops.
        func root(of id: String) -> String? { roots[id] }
    }

    /// A unit with its helpers folded in: a helper that needs the person
    /// brings the unit to Your turn with its question; a working helper
    /// keeps a finished unit in progress (it waits on the helper).
    static func attach(helpers: [MuxaHostedAgent], to work: FleetWork, now: Date) -> FleetWork {
        guard !helpers.isEmpty else { return work }
        var unit = work
        unit.helpers = helpers
        unit.revision += ";" + revision(agents: helpers, run: nil)
        unit.lastChange = [work.lastChange, latestChange(helpers)].compactMap { $0 }.max()
        unit.handledLimit = work.handledLimit || helpers.contains { isHandledLimit($0.agent, now: now) }
        if unit.lane != .needsYou,
           let asking = helpers
               .filter({ $0.agent.status(at: now).needsAttention })
               .sorted(by: { $0.agent.status(at: now).priority < $1.agent.status(at: now).priority })
               .first {
            unit.lane = .needsYou
            unit.focus = asking
            unit.headline = "\(owner(asking)): \(question(for: asking.agent, now: now))"
            unit.since = MuxaUsageTimestamp.parse(asking.agent.stateEnteredAt ?? asking.agent.lastActivityAt)
        } else if unit.lane == .finished,
                  let working = helpers.first(where: {
                      stageState(aliasStatus: nil, agent: $0.agent, now: now) == .running
                  }) {
            unit.lane = .running
            unit.headline = String(localized: "Waiting on \(owner(working)): \(statusLine(for: working.agent, now: now))")
            unit.since = MuxaUsageTimestamp.parse(working.agent.stateEnteredAt ?? working.agent.lastActivityAt)
        }
        return unit
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
            revision: revision(agents: group.participants, run: group.pipelineRun),
            lastChange: latestChange(group.participants)
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
            ?? agent.lastPrompt.flatMap(promptTitle)
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
            revision: revision(agents: [hosted], run: nil),
            lastChange: latestChange([hosted])
        )
    }

    static func latestChange(_ agents: [MuxaHostedAgent]) -> Date? {
        agents.compactMap { MuxaUsageTimestamp.parse($0.agent.stateEnteredAt ?? $0.agent.lastActivityAt) }.max()
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

    /// A prompt as a card title: muxa's delivery notices and Claude's
    /// background-task blocks read as what they are, not as their markup.
    static func promptTitle(_ prompt: String) -> String? {
        let text = String(prompt.drop(while: \.isWhitespace))
        if text.hasPrefix("<task-notification>") {
            if let start = text.range(of: "<summary>"), let end = text.range(of: "</summary>", range: start.upperBound..<text.endIndex) {
                return firstLine(String(text[start.upperBound..<end.lowerBound])).map { "⟳ \($0)" }
            }
            return String(localized: "⟳ Background task")
        }
        guard text.hasPrefix("[muxa:"), let close = text.firstIndex(of: "]") else { return firstLine(text) }
        let rest = text[text.index(after: close)...].trimmingCharacters(in: .whitespaces)
        let line = rest.split(separator: "\n", maxSplits: 1).first.map(String.init) ?? rest
        // "Completed reply from codex@%13 (…) is ready. Read it …"
        if let range = line.range(of: #"(?:Completed )?reply from \S+"#, options: [.regularExpression, .caseInsensitive]) {
            let who = line[range].split(separator: " ").last.map(String.init) ?? ""
            return String(localized: "Reply from \(who) is ready")
        }
        // "Review from codex@%39. Read one: …" / "New task request via … representing console …"
        let sentence = line.split(separator: ".", maxSplits: 1).first.map(String.init) ?? line
        return firstLine(sentence)
    }

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

// MARK: - Arranging the board

/// The units of one project within a lane.
struct FleetProjectGroup: Identifiable, Sendable {
    /// The repository name; nil for units with no known folder.
    let project: String?
    let works: [FleetWork]

    var id: String { project ?? "" }
}

extension FleetBoard {
    /// How long a finished unit stays in the Finished lane's main list.
    static let recentFinish: TimeInterval = 24 * 3600

    /// Units grouped by project, a project placed where its first unit
    /// is, so the lane's order (the longest wait, the latest change) still
    /// decides which project comes first.
    static func byProject(_ works: [FleetWork]) -> [FleetProjectGroup] {
        var order: [String?] = []
        var groups: [String: [FleetWork]] = [:]
        for work in works {
            let key = work.repo ?? ""
            if groups[key] == nil { order.append(work.repo) }
            groups[key, default: []].append(work)
        }
        return order.map { FleetProjectGroup(project: $0, works: groups[$0 ?? ""] ?? []) }
    }

    /// A finished unit that changed within `recentFinish`, or one with no
    /// known time.
    static func isRecent(_ work: FleetWork, now: Date = .now) -> Bool {
        guard let changed = work.lastChange ?? work.since else { return true }
        return now.timeIntervalSince(changed) <= recentFinish
    }

    /// The units on the chosen host and in the chosen project.
    static func filter(_ works: [FleetWork], host: String?, project: String?) -> [FleetWork] {
        works.filter { work in
            (host == nil || work.hostAliases.contains(host!))
                && (project == nil || work.repo == project)
        }
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

/// Text for one unit's agent, the question it answers, and the send in
/// flight for it. Owned by the board, not by an inspector, so closing and
/// reopening the inspector never forgets a pending send or allows a
/// second one.
struct FleetReplyDraft: Equatable, Sendable {
    struct Submission: Equatable, Sendable {
        let id: UUID
        let text: String
        let target: FleetReplyTarget
        let version: Int
    }

    private(set) var text = ""
    private(set) var target: FleetReplyTarget?
    /// Bumped by every edit, so a finished send clears only what it sent.
    private(set) var version = 0
    private(set) var pending: Submission?
    private(set) var failure: String?
    private(set) var sent = false

    var isSending: Bool { pending != nil }
    var hasText: Bool { !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
    /// Nothing to keep: no text, no send, no result to show.
    var isIdle: Bool { !hasText && pending == nil && failure == nil && !sent }

    /// The draft belongs to the question on screen when typing starts.
    mutating func edit(_ newText: String, current: FleetReplyTarget?) {
        guard pending == nil else { return }
        text = newText
        if !hasText { target = nil } else if target == nil { target = current }
        version += 1
        sent = false
    }

    mutating func retarget(_ newTarget: FleetReplyTarget) {
        guard pending == nil else { return }
        target = newTarget
        failure = nil
        version += 1
    }

    mutating func discard() {
        guard pending == nil else { return }
        self = FleetReplyDraft(version: version + 1)
    }

    private init(version: Int) { self.version = version }
    init() {}

    /// Starts a send unless one is already in flight.
    mutating func begin() -> Submission? {
        guard pending == nil, hasText, let target else { return nil }
        let submission = Submission(
            id: UUID(),
            text: text.trimmingCharacters(in: .whitespacesAndNewlines),
            target: target,
            version: version
        )
        pending = submission
        failure = nil
        sent = false
        return submission
    }

    /// Ends the send `submission` started; a stale completion is ignored.
    mutating func finish(_ submission: Submission, error: String?) {
        guard pending?.id == submission.id else { return }
        pending = nil
        if let error {
            failure = error
            return
        }
        sent = true
        if version == submission.version && target == submission.target {
            text = ""
            target = nil
            version += 1
        }
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
