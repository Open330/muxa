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
        let direction: String?
        let after: [String]

        var id: String { alias }

        private enum CodingKeys: String, CodingKey {
            case alias, program, role, task, direction, after
        }

        init(
            alias: String,
            program: String,
            role: String? = nil,
            task: String? = nil,
            direction: String? = nil,
            after: [String] = []
        ) {
            self.alias = alias
            self.program = program
            self.role = role
            self.task = task
            self.direction = direction
            self.after = after
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            alias = try values.decode(String.self, forKey: .alias)
            program = try values.decodeIfPresent(String.self, forKey: .program) ?? ""
            role = try values.decodeIfPresent(String.self, forKey: .role)
            task = try values.decodeIfPresent(String.self, forKey: .task)
            direction = try values.decodeIfPresent(String.self, forKey: .direction)
            after = try values.decodeIfPresent([String].self, forKey: .after) ?? []
        }
    }

    struct Pipeline: Decodable, Equatable, Sendable, Identifiable {
        let name: String
        let description: String?
        let layout: String?
        let agents: [Agent]

        var id: String { name }

        private enum CodingKeys: String, CodingKey {
            case name, description, layout, agents
        }

        init(name: String, description: String? = nil, layout: String? = nil, agents: [Agent]) {
            self.name = name
            self.description = description
            self.layout = layout
            self.agents = agents
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            name = try values.decode(String.self, forKey: .name)
            description = try values.decodeIfPresent(String.self, forKey: .description)
            layout = try values.decodeIfPresent(String.self, forKey: .layout)
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
