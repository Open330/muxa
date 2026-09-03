import Foundation

/// What `muxa work options --json` reports: the operator's routes,
/// pipelines, message skills, and muxa's built-in presets. The native Start
/// Work form offers these as real choices instead of free text, and the Work
/// Command Center can show a pipeline before anything is launched.
struct MuxaWorkOptions: Decodable, Equatable, Sendable {
    struct Route: Decodable, Equatable, Sendable, Identifiable {
        let match: String
        let workspace: String?
        let pipeline: String?
        let cwd: String?
        let worktree: Bool
        let prepare: Bool

        var id: String { match }

        private enum CodingKeys: String, CodingKey {
            case match, workspace, pipeline, cwd, worktree, prepare
        }

        init(
            match: String,
            workspace: String? = nil,
            pipeline: String? = nil,
            cwd: String? = nil,
            worktree: Bool = false,
            prepare: Bool = false
        ) {
            self.match = match
            self.workspace = workspace
            self.pipeline = pipeline
            self.cwd = cwd
            self.worktree = worktree
            self.prepare = prepare
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            match = try values.decodeIfPresent(String.self, forKey: .match) ?? ""
            workspace = try values.decodeIfPresent(String.self, forKey: .workspace)
            pipeline = try values.decodeIfPresent(String.self, forKey: .pipeline)
            cwd = try values.decodeIfPresent(String.self, forKey: .cwd)
            worktree = try values.decodeIfPresent(Bool.self, forKey: .worktree) ?? false
            prepare = try values.decodeIfPresent(Bool.self, forKey: .prepare) ?? false
        }
    }

    struct Agent: Decodable, Equatable, Sendable, Identifiable {
        let alias: String
        let program: String
        let role: String?
        let task: String?
        let prompt: String?
        let direction: String?
        let after: [String]

        var id: String { alias }

        private enum CodingKeys: String, CodingKey {
            case alias, program, role, task, prompt, direction, after
        }

        init(
            alias: String,
            program: String,
            role: String? = nil,
            task: String? = nil,
            prompt: String? = nil,
            direction: String? = nil,
            after: [String] = []
        ) {
            self.alias = alias
            self.program = program
            self.role = role
            self.task = task
            self.prompt = prompt
            self.direction = direction
            self.after = after
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            alias = try values.decode(String.self, forKey: .alias)
            program = try values.decodeIfPresent(String.self, forKey: .program) ?? ""
            role = try values.decodeIfPresent(String.self, forKey: .role)
            task = try values.decodeIfPresent(String.self, forKey: .task)
            prompt = try values.decodeIfPresent(String.self, forKey: .prompt)
            direction = try values.decodeIfPresent(String.self, forKey: .direction)
            after = try values.decodeIfPresent([String].self, forKey: .after) ?? []
        }
    }

    struct Pipeline: Decodable, Equatable, Sendable, Identifiable {
        let name: String
        let description: String?
        let layout: String?
        let prompt: String?
        let agents: [Agent]

        var id: String { name }

        private enum CodingKeys: String, CodingKey {
            case name, description, layout, prompt, agents
        }

        init(
            name: String,
            description: String? = nil,
            layout: String? = nil,
            prompt: String? = nil,
            agents: [Agent]
        ) {
            self.name = name
            self.description = description
            self.layout = layout
            self.prompt = prompt
            self.agents = agents
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            name = try values.decode(String.self, forKey: .name)
            description = try values.decodeIfPresent(String.self, forKey: .description)
            layout = try values.decodeIfPresent(String.self, forKey: .layout)
            prompt = try values.decodeIfPresent(String.self, forKey: .prompt)
            agents = try values.decodeIfPresent([Agent].self, forKey: .agents) ?? []
        }

        /// Agents grouped into launch stages by their `after` edges.
        var stages: [[Agent]] { MuxaPipelineStages.stages(for: agents) }
    }

    struct Skill: Decodable, Equatable, Sendable, Identifiable {
        let name: String
        let summary: String?

        var id: String { name }
    }

    let configPath: String?
    let configured: Bool
    let routes: [Route]
    let pipelines: [Pipeline]
    let skills: [Skill]
    let presets: [Pipeline]
    let ticketAgent: String?

    private enum CodingKeys: String, CodingKey {
        case configured, routes, pipelines, skills, presets
        case configPath = "config_path"
        case ticketAgent = "ticket_agent"
    }

    init(
        configPath: String? = nil,
        configured: Bool = false,
        routes: [Route] = [],
        pipelines: [Pipeline] = [],
        skills: [Skill] = [],
        presets: [Pipeline] = [],
        ticketAgent: String? = nil
    ) {
        self.configPath = configPath
        self.configured = configured
        self.routes = routes
        self.pipelines = pipelines
        self.skills = skills
        self.presets = presets
        self.ticketAgent = ticketAgent
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        configPath = try values.decodeIfPresent(String.self, forKey: .configPath)
        let pipelines = try values.decodeIfPresent([Pipeline].self, forKey: .pipelines) ?? []
        self.pipelines = pipelines
        configured = try values.decodeIfPresent(Bool.self, forKey: .configured) ?? !pipelines.isEmpty
        routes = try values.decodeIfPresent([Route].self, forKey: .routes) ?? []
        skills = try values.decodeIfPresent([Skill].self, forKey: .skills) ?? []
        presets = try values.decodeIfPresent([Pipeline].self, forKey: .presets) ?? []
        ticketAgent = try values.decodeIfPresent(String.self, forKey: .ticketAgent)
    }

    static func decode(_ data: Data) throws -> MuxaWorkOptions {
        try JSONDecoder().decode(MuxaWorkOptions.self, from: data)
    }

    /// The route `muxa work up` would pick for `workID`: first match wins,
    /// matched case-insensitively as a search (the CLI's regex semantics).
    /// A route with an empty pattern never matches, and a pattern that does
    /// not compile is skipped here (the CLI refuses the whole config).
    func route(matching workID: String) -> Route? {
        let candidate = workID.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !candidate.isEmpty else { return nil }
        return routes.first { route in
            if route.match.isEmpty { return false }
            guard let regex = try? NSRegularExpression(
                pattern: route.match,
                options: [.caseInsensitive]
            ) else { return false }
            let range = NSRange(candidate.startIndex..., in: candidate)
            return regex.firstMatch(in: candidate, range: range) != nil
        }
    }

    func pipeline(named name: String) -> Pipeline? {
        pipelines.first { $0.name == name }
    }

    /// The pipeline that would staff `workID` when the operator leaves the
    /// pipeline choice on "route default".
    func defaultPipeline(for workID: String) -> Pipeline? {
        route(matching: workID)?.pipeline.flatMap(pipeline(named:))
    }
}

/// Groups pipeline agents into the order muxa launches them: an agent joins
/// the first stage once every alias in its `after` list belongs to an
/// earlier stage. Unknown aliases do not block (the CLI refuses those
/// pipelines anyway); a dependency cycle leaves the remaining agents in one
/// final stage so the picture never loses an agent.
enum MuxaPipelineStages {
    static func stages(for agents: [MuxaWorkOptions.Agent]) -> [[MuxaWorkOptions.Agent]] {
        let known = Set(agents.map(\.alias))
        var remaining = agents
        var placed = Set<String>()
        var result: [[MuxaWorkOptions.Agent]] = []
        while !remaining.isEmpty {
            let ready = remaining.filter { agent in
                agent.after.allSatisfy { !known.contains($0) || placed.contains($0) }
            }
            guard !ready.isEmpty else {
                result.append(remaining)
                break
            }
            result.append(ready)
            placed.formUnion(ready.map(\.alias))
            remaining.removeAll { placed.contains($0.alias) }
        }
        return result
    }
}

/// The editable form of a pipeline, encoded exactly as
/// `muxa work pipeline set <name> --from-json -` expects. Validation mirrors
/// the CLI so the editor can refuse a line-up before spending a round trip.
struct MuxaPipelineDefinition: Codable, Equatable, Sendable {
    struct Agent: Codable, Equatable, Sendable, Identifiable {
        var id = UUID()
        var alias: String
        var program: String
        var role: String
        var task: String
        var prompt: String
        var direction: String
        var after: [String]

        private enum CodingKeys: String, CodingKey {
            case alias, program, role, task, prompt, direction, after
        }

        init(
            alias: String = "",
            program: String = "claude",
            role: String = "",
            task: String = "",
            prompt: String = "",
            direction: String = "",
            after: [String] = []
        ) {
            self.alias = alias
            self.program = program
            self.role = role
            self.task = task
            self.prompt = prompt
            self.direction = direction
            self.after = after
        }

        init(_ agent: MuxaWorkOptions.Agent) {
            self.init(
                alias: agent.alias,
                program: agent.program,
                role: agent.role ?? "",
                task: agent.task ?? "",
                prompt: agent.prompt ?? "",
                direction: agent.direction ?? "",
                after: agent.after
            )
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            alias = try values.decodeIfPresent(String.self, forKey: .alias) ?? ""
            program = try values.decodeIfPresent(String.self, forKey: .program) ?? ""
            role = try values.decodeIfPresent(String.self, forKey: .role) ?? ""
            task = try values.decodeIfPresent(String.self, forKey: .task) ?? ""
            prompt = try values.decodeIfPresent(String.self, forKey: .prompt) ?? ""
            direction = try values.decodeIfPresent(String.self, forKey: .direction) ?? ""
            after = try values.decodeIfPresent([String].self, forKey: .after) ?? []
        }

        func encode(to encoder: Encoder) throws {
            var container = encoder.container(keyedBy: CodingKeys.self)
            try container.encode(alias.trimmingCharacters(in: .whitespaces).lowercased(), forKey: .alias)
            try container.encode(program.trimmingCharacters(in: .whitespaces).lowercased(), forKey: .program)
            try container.encode(Self.optional(role), forKey: .role)
            try container.encode(Self.optional(task), forKey: .task)
            try container.encode(Self.optional(prompt), forKey: .prompt)
            try container.encode(Self.optional(direction), forKey: .direction)
            try container.encode(after, forKey: .after)
        }

        private static func optional(_ value: String) -> String? {
            let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
            return trimmed.isEmpty ? nil : trimmed
        }

        var toOptionsAgent: MuxaWorkOptions.Agent {
            MuxaWorkOptions.Agent(
                alias: alias.trimmingCharacters(in: .whitespaces).lowercased(),
                program: program,
                role: role.isEmpty ? nil : role,
                task: task.isEmpty ? nil : task,
                prompt: prompt.isEmpty ? nil : prompt,
                direction: direction.isEmpty ? nil : direction,
                after: after
            )
        }
    }

    static let allowedPrograms = ["claude", "codex", "gemini", "agy", "opencode"]
    static let layouts = ["main-vertical", "main-horizontal", "even-horizontal", "even-vertical", "tiled"]

    var description: String
    var layout: String
    var prompt: String
    var agents: [Agent]

    private enum CodingKeys: String, CodingKey {
        case description, layout, prompt, agents
    }

    init(description: String = "", layout: String = "", prompt: String = "", agents: [Agent] = []) {
        self.description = description
        self.layout = layout
        self.prompt = prompt
        self.agents = agents
    }

    init(_ pipeline: MuxaWorkOptions.Pipeline) {
        self.init(
            description: pipeline.description ?? "",
            layout: pipeline.layout ?? "",
            prompt: pipeline.prompt ?? "",
            agents: pipeline.agents.map(Agent.init)
        )
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        description = try values.decodeIfPresent(String.self, forKey: .description) ?? ""
        layout = try values.decodeIfPresent(String.self, forKey: .layout) ?? ""
        prompt = try values.decodeIfPresent(String.self, forKey: .prompt) ?? ""
        agents = try values.decodeIfPresent([Agent].self, forKey: .agents) ?? []
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(description.isEmpty ? nil : description, forKey: .description)
        try container.encode(layout.isEmpty ? nil : layout, forKey: .layout)
        try container.encode(prompt.isEmpty ? nil : prompt, forKey: .prompt)
        try container.encode(agents, forKey: .agents)
    }

    /// The JSON the CLI reads on stdin.
    func jsonString() throws -> String {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        return String(decoding: try encoder.encode(self), as: UTF8.self)
    }

    var optionsAgents: [MuxaWorkOptions.Agent] { agents.map(\.toOptionsAgent) }

    /// TOML bare-key characters, the same rule the CLI applies to `<name>`.
    static func isValidName(_ name: String) -> Bool {
        !name.isEmpty && name.unicodeScalars.allSatisfy { scalar in
            CharacterSet.alphanumerics.contains(scalar) || scalar == "-" || scalar == "_"
        }
    }

    /// Problems the CLI would refuse, in the order an operator can fix them.
    func problems() -> [String] {
        var problems: [String] = []
        if agents.isEmpty { problems.append("Add at least one agent.") }
        var seen = Set<String>()
        let aliases = agents.map { $0.alias.trimmingCharacters(in: .whitespaces).lowercased() }
        for (index, alias) in aliases.enumerated() {
            if alias.isEmpty {
                problems.append("Agent \(index + 1) needs an alias.")
            } else if !Self.isValidName(alias) {
                problems.append("Alias \"\(alias)\" may only use letters, digits, - and _.")
            } else if !seen.insert(alias).inserted {
                problems.append("Alias \"\(alias)\" is used twice.")
            }
        }
        for agent in agents {
            let program = agent.program.trimmingCharacters(in: .whitespaces).lowercased()
            if !Self.allowedPrograms.contains(program) {
                problems.append("@\(agent.alias): program must be one of \(Self.allowedPrograms.joined(separator: ", ")).")
            }
            if !agent.direction.isEmpty, !["right", "down"].contains(agent.direction) {
                problems.append("@\(agent.alias): direction must be right or down.")
            }
            for dependency in agent.after where !aliases.contains(dependency) {
                problems.append("@\(agent.alias) waits for unknown alias \"\(dependency)\".")
            }
        }
        if problems.isEmpty, hasCycle() {
            problems.append("The after edges form a cycle, so some agents would never start.")
        }
        return problems
    }

    private func hasCycle() -> Bool {
        let staged = MuxaPipelineStages.stages(for: optionsAgents).flatMap { $0 }.count
        let known = Set(optionsAgents.map(\.alias))
        let placeable = optionsAgents.filter { agent in agent.after.allSatisfy { known.contains($0) } }
        // A cycle leaves agents that never become ready; stages() parks them
        // in a final stage, so compare against a proper topological order.
        var remaining = placeable
        var placed = Set<String>()
        while !remaining.isEmpty {
            let ready = remaining.filter { $0.after.allSatisfy { placed.contains($0) || !known.contains($0) } }
            if ready.isEmpty { return true }
            placed.formUnion(ready.map(\.alias))
            remaining.removeAll { placed.contains($0.alias) }
        }
        return staged != optionsAgents.count
    }
}

/// What the pipeline editor sheet is editing.
struct MuxaPipelineEditorTarget: Identifiable, Equatable, Sendable {
    /// nil edits the local host's config.
    let host: String?
    /// nil creates a new pipeline.
    let pipeline: MuxaWorkOptions.Pipeline?

    var id: String { "\(host ?? "local"):\(pipeline?.name ?? "+new")" }
}

/// One route row as edited in the Command Center.
struct MuxaWorkRouteEdit: Equatable, Sendable {
    var match: String
    var pipeline: String
    var workspace: String
    var cwd: String
    var position: Int?
    /// Whether this replaces an existing `[[route]]` (so cleared fields must
    /// be cleared explicitly) or appends a new one.
    var existing: Bool

    init(
        match: String = "",
        pipeline: String = "",
        workspace: String = "",
        cwd: String = "",
        position: Int? = nil,
        existing: Bool = false
    ) {
        self.match = match
        self.pipeline = pipeline
        self.workspace = workspace
        self.cwd = cwd
        self.position = position
        self.existing = existing
    }

    init(_ route: MuxaWorkOptions.Route) {
        self.init(
            match: route.match,
            pipeline: route.pipeline ?? "",
            workspace: route.workspace ?? "",
            cwd: route.cwd ?? "",
            existing: true
        )
    }
}
