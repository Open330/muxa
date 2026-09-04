import Foundation
import Darwin
import Testing
@testable import Muxa

@Test func readableMarkdownKeepsConversationStructure() {
    let document = ReadableMarkdownDocument(source: """
    # Result

    A readable paragraph with **emphasis**.

    - first
    - second

    > important note

    ```swift
    let value = 42
    ```
    """)

    #expect(document.blocks == [
        .heading(level: 1, text: "Result"),
        .paragraph("A readable paragraph with **emphasis**."),
        .list(ordered: false, items: [
            .init(marker: "•", text: "first", depth: 0),
            .init(marker: "•", text: "second", depth: 0),
        ]),
        .quote("important note"),
        .code(language: "swift", source: "let value = 42"),
    ])
}

@Test func readableMarkdownParsesPipeTablesAsBlocks() {
    let document = ReadableMarkdownDocument(source: """
    | Round | Findings | Result |
    |---|:---:|---:|
    | 1 | 9 must-fix | **8 accepted** |
    | 2 | 4 nice-to-have | converged |

    19 of 20 findings accepted.
    """)

    #expect(document.blocks == [
        .table(
            header: ["Round", "Findings", "Result"],
            rows: [
                ["1", "9 must-fix", "**8 accepted**"],
                ["2", "4 nice-to-have", "converged"],
            ]
        ),
        .paragraph("19 of 20 findings accepted."),
    ])
}

@Test func singleTextMarkdownKeepsBlockBoundaries() {
    let source = """
    Intro line **bold**
    soft break line

    ## Heading two

    | a | b |
    |---|---|
    | 1 | 2 |

    - item one
    - item two
      - nested

    1. first
    2. second

    ```swift
    let x = 1
    ```

    > quoted

    Last para
    """

    let text = MuxaMarkdownText.plainText(markdown: source)

    #expect(text == """
    Intro line bold soft break line
    Heading two
    a  |  b
    1  |  2
    • item one
    • item two
        • nested
    1. first
    2. second
    let x = 1
    quoted
    Last para
    """)
}

@Test func inboxPreviewDropsMarkdownMarkersAndLineBreaks() {
    let preview = MuxaMarkdownText.previewText(markdown: "# 교차 리뷰 루프 완료\n\n**수렴** 리뷰어: Codex\n- one\n- two")
    #expect(preview == "교차 리뷰 루프 완료 수렴 리뷰어: Codex • one • two")
}

@Test func previewFontPrefersMonospaceNerdFontVariants() {
    #expect(TerminalPreviewFont.nerdFontFamily(available: ["Helvetica", "SF Mono"]) == nil)
    #expect(
        TerminalPreviewFont.nerdFontFamily(available: [
            "Helvetica",
            "JetBrainsMono Nerd Font",
            "MesloLGS NF",
            "JetBrainsMono Nerd Font Mono",
        ]) == "JetBrainsMono Nerd Font Mono"
    )
    #expect(TerminalPreviewFont.nerdFontFamily(available: ["MesloLGS NF"]) == "MesloLGS NF")
    #expect(
        TerminalPreviewFont.nerdFontFamily(available: ["Hack Nerd Font Propo", "Hack Nerd Font"])
            == "Hack Nerd Font"
    )
}

@Test func workOptionsDecodeAndSelectRoutesLikeTheCLI() throws {
    let json = """
    {
      "config_path": "/tmp/config.toml",
      "configured": true,
      "routes": [
        {"match": "^cal-", "workspace": "callabo", "pipeline": "triad", "worktree": true, "prepare": false},
        {"match": "", "pipeline": "never"},
        {"match": "[", "pipeline": "broken"},
        {"match": ".*", "cwd": "{{cwd}}", "pipeline": "solo"}
      ],
      "pipelines": [
        {"name": "solo", "agents": [{"alias": "claude", "program": "claude"}]},
        {"name": "triad", "description": "planner → implementer → reviewer", "layout": "main-vertical",
         "agents": [
           {"alias": "plan", "program": "codex", "role": "planner", "after": []},
           {"alias": "impl", "program": "codex", "role": "implementer", "after": ["plan"], "direction": "down"},
           {"alias": "review", "program": "claude", "role": "reviewer", "after": ["impl"]}
         ]}
      ],
      "skills": [{"name": "review", "summary": "Review the current diff"}],
      "presets": [{"name": "pair", "description": "implementer → reviewer", "agents": [
        {"alias": "impl", "program": "claude"}, {"alias": "review", "program": "codex", "after": ["impl"]}]}],
      "ticket_agent": "claude"
    }
    """

    let options = try MuxaWorkOptions.decode(Data(json.utf8))

    #expect(options.configured)
    #expect(options.pipelines.map(\.name) == ["solo", "triad"])
    #expect(options.route(matching: "cal-123")?.pipeline == "triad")
    #expect(options.route(matching: "CAL-9")?.workspace == "callabo")
    #expect(options.route(matching: "auth-cleanup")?.match == ".*")
    #expect(options.route(matching: "   ") == nil)
    #expect(options.defaultPipeline(for: "cal-1")?.name == "triad")
    #expect(options.defaultPipeline(for: "misc")?.name == "solo")
    #expect(options.skills.first?.summary == "Review the current diff")
    #expect(options.presets.first?.stages.map { $0.map(\.alias) } == [["impl"], ["review"]])
    #expect(options.pipeline(named: "triad")?.agents[1].direction == "down")
}

@Test func workOptionsDecodeToleratesAnEmptyConfig() throws {
    let options = try MuxaWorkOptions.decode(Data(#"{"config_path": "/tmp/c.toml", "routes": [], "pipelines": [], "presets": []}"#.utf8))
    #expect(!options.configured)
    #expect(options.route(matching: "x") == nil)
    #expect(options.skills.isEmpty)
}

@Test func pipelineStagesFollowAfterEdges() {
    typealias Agent = MuxaWorkOptions.Agent
    let agents = [
        Agent(alias: "plan", program: "codex"),
        Agent(alias: "impl", program: "codex", after: ["plan"]),
        Agent(alias: "review", program: "claude", after: ["impl"]),
        Agent(alias: "docs", program: "claude", after: ["plan"]),
    ]
    #expect(MuxaPipelineStages.stages(for: agents).map { $0.map(\.alias) } == [["plan"], ["impl", "docs"], ["review"]])

    let cyclic = [
        Agent(alias: "a", program: "claude", after: ["b"]),
        Agent(alias: "b", program: "codex", after: ["a"]),
    ]
    #expect(MuxaPipelineStages.stages(for: cyclic).map { $0.map(\.alias) } == [["a", "b"]])

    let unknownDependency = [Agent(alias: "a", program: "claude", after: ["ghost"])]
    #expect(MuxaPipelineStages.stages(for: unknownDependency).map { $0.map(\.alias) } == [["a"]])
    #expect(MuxaPipelineStages.stages(for: []).isEmpty)
}

@Test func workStartResultDecodesTheDryRunPlan() throws {
    let json = """
    {"work": "MUXA-APP-DRYRUN", "workspace": "muxa", "pipeline": "pair", "cwd": "/tmp/muxa", "dry_run": true,
     "layout": null, "graph": [{"alias": "impl", "after": [], "depth": 0}],
     "plan": {"steps": [
       {"action": "launch", "alias": "impl", "program": "claude", "role": "implementer", "task": "Implement the request", "prompt": "You own it."},
       {"action": "waiting", "alias": "review", "waiting_on": ["impl"]},
       {"action": "keep", "alias": "docs", "pane": "%4", "state": "idle"}
     ], "unclaimed": []}}
    """
    let result = try JSONDecoder().decode(MuxaWorkStartResult.self, from: Data(json.utf8))
    #expect(result.dryRun == true)
    #expect(result.plan?.steps.map(\.action) == ["launch", "waiting", "keep"])
    #expect(result.plan?.steps[0].prompt == "You own it.")
    #expect(result.plan?.steps[1].waitingOn == ["impl"])
    #expect(result.plan?.steps[2].pane == "%4")
}

@Test func pipelineDefinitionValidatesLikeTheCLI() throws {
    var definition = MuxaPipelineDefinition(
        description: "planner → implementer",
        layout: "main-vertical",
        agents: [
            MuxaPipelineDefinition.Agent(alias: "Plan", program: "codex", role: "planner"),
            MuxaPipelineDefinition.Agent(alias: "impl", program: "claude", task: "Implement", after: ["plan"]),
        ]
    )
    #expect(definition.problems().isEmpty)
    #expect(MuxaPipelineDefinition.isValidName("implement-review"))
    #expect(!MuxaPipelineDefinition.isValidName("bad name"))
    #expect(!MuxaPipelineDefinition.isValidName("qa-pair바나나"))

    let json = try definition.jsonString()
    #expect(json.contains(#""alias":"plan""#))
    #expect(json.contains(#""after":["plan"]"#))
    #expect(json.contains(#""layout":"main-vertical""#))
    #expect(json.contains(#""prompt":null"#))
    let decoded = try JSONDecoder().decode(MuxaPipelineDefinition.self, from: Data(json.utf8))
    #expect(decoded.agents.map(\.alias) == ["plan", "impl"])

    // The wording is localized (the test host may run in Korean), so compare
    // against the same catalog keys the editor uses.
    definition.agents.append(MuxaPipelineDefinition.Agent(alias: "impl", program: "gemini"))
    #expect(definition.problems().contains(String(localized: "Alias \"\("impl")\" is used twice.")))
    definition.agents.removeLast()

    definition.agents[1].program = "bash"
    let allowed = MuxaPipelineDefinition.allowedPrograms.joined(separator: ", ")
    #expect(definition.problems().contains(String(localized: "@\("impl"): program must be one of \(allowed).")))
    definition.agents[1].program = "claude"

    definition.agents[1].after = ["ghost"]
    #expect(definition.problems().contains(String(localized: "@\("impl") waits for unknown alias \"\("ghost")\".")))

    definition.agents[0].after = ["impl"]
    definition.agents[1].after = ["plan"]
    #expect(definition.problems().contains(String(localized: "The after edges form a cycle, so some agents would never start.")))

    #expect(!MuxaPipelineDefinition(agents: []).problems().isEmpty)
}

@Test func pipelineDefinitionRoundTripsWorkOptionsAgents() {
    let pipeline = MuxaWorkOptions.Pipeline(
        name: "pair",
        description: "implementer → reviewer",
        prompt: "Shared {{request}}",
        agents: [
            MuxaWorkOptions.Agent(alias: "impl", program: "claude", role: "implementer", prompt: "Own it."),
            MuxaWorkOptions.Agent(alias: "review", program: "codex", direction: "down", after: ["impl"]),
        ]
    )
    let definition = MuxaPipelineDefinition(pipeline)
    #expect(definition.prompt == "Shared {{request}}")
    #expect(definition.agents[0].prompt == "Own it.")
    #expect(definition.agents[1].direction == "down")
    #expect(definition.optionsAgents.map(\.alias) == ["impl", "review"])
    #expect(MuxaPipelineStages.stages(for: definition.optionsAgents).count == 2)
}

@Test func routeEditStartsFromTheExistingRoute() {
    let route = MuxaWorkOptions.Route(match: "^cal-", workspace: "callabo", pipeline: "triad", worktree: true)
    let edit = MuxaWorkRouteEdit(route)
    #expect(edit.existing)
    #expect(edit.pipeline == "triad")
    #expect(edit.workspace == "callabo")
    #expect(edit.cwd.isEmpty)
    #expect(!MuxaWorkRouteEdit().existing)
}

@Test func pipelineSyncStateComparesLibraryAgainstEachHost() {
    let library = MuxaWorkOptions.Pipeline(
        name: "pair",
        description: "implementer → reviewer",
        agents: [
            MuxaWorkOptions.Agent(alias: "impl", program: "claude", role: "implementer"),
            MuxaWorkOptions.Agent(alias: "review", program: "codex", after: ["impl"]),
        ]
    )
    let same = MuxaWorkOptions(pipelines: [library])
    let renamedRole = MuxaWorkOptions(pipelines: [MuxaWorkOptions.Pipeline(
        name: "pair",
        description: "implementer → reviewer",
        agents: [
            MuxaWorkOptions.Agent(alias: "impl", program: "claude", role: "builder"),
            MuxaWorkOptions.Agent(alias: "review", program: "codex", after: ["impl"]),
        ]
    )])
    let other = MuxaWorkOptions(pipelines: [MuxaWorkOptions.Pipeline(name: "solo", agents: [
        MuxaWorkOptions.Agent(alias: "claude", program: "claude"),
    ])])

    #expect(MuxaPipelineSyncState.compare(library: library, hostOptions: same) == .inSync)
    #expect(MuxaPipelineSyncState.compare(library: library, hostOptions: renamedRole) == .differs)
    #expect(MuxaPipelineSyncState.compare(library: library, hostOptions: other) == .missing)
    #expect(MuxaPipelineSyncState.compare(library: library, hostOptions: nil) == .unavailable)
    #expect(MuxaPipelineHostState(host: "dev", state: .missing).needsSync)
    #expect(!MuxaPipelineHostState(host: "dev", state: .inSync).needsSync)
}

/// A TOML multi-line prompt ends in a newline on the host it was written on;
/// the copy the app writes elsewhere must still count as the same pipeline.
@Test func pipelineSyncIgnoresRoundTripWhitespace() throws {
    let library = MuxaWorkOptions.Pipeline(
        name: "pair",
        prompt: "Shared.\n",
        agents: [
            MuxaWorkOptions.Agent(alias: "impl", program: "claude", role: "implementer", prompt: "Own it.\n"),
            MuxaWorkOptions.Agent(alias: "review", program: "codex", after: ["impl"]),
        ]
    )
    let trimmedCopy = MuxaWorkOptions(pipelines: [MuxaWorkOptions.Pipeline(
        name: "pair",
        prompt: "Shared.",
        agents: [
            MuxaWorkOptions.Agent(alias: "Impl", program: "claude", role: "implementer ", prompt: "Own it."),
            MuxaWorkOptions.Agent(alias: "review", program: "codex", after: ["impl"]),
        ]
    )])
    #expect(MuxaPipelineSyncState.compare(library: library, hostOptions: trimmedCopy) == .inSync)

    // The JSON handed to `pipeline set` keeps the prompt verbatim, so a host
    // that already has the trailing newline reads back identical.
    let json = try MuxaPipelineDefinition(library).jsonString()
    let decoded = try JSONDecoder().decode(MuxaPipelineDefinition.self, from: Data(json.utf8))
    #expect(decoded.agents[0].prompt == "Own it.\n")
    #expect(decoded.prompt == "Shared.\n")
}

/// An absent `direction` means `auto`: muxa splits along the pane's longer
/// side. `right` and `down` are their own values, and tmux's spellings map
/// onto them, so the editor's picker and sync agree with the daemon.
@Test func pipelineDefinitionCanonicalizesSplitDirections() {
    #expect(MuxaPipelineDefinition.Agent.canonicalDirection("") == "")
    #expect(MuxaPipelineDefinition.Agent.canonicalDirection("auto") == "")
    #expect(MuxaPipelineDefinition.Agent.canonicalDirection("right") == "right")
    #expect(MuxaPipelineDefinition.Agent.canonicalDirection(" Horizontal ") == "right")
    #expect(MuxaPipelineDefinition.Agent.canonicalDirection("vertical") == "down")
    #expect(MuxaPipelineDefinition.Agent.canonicalDirection("down") == "down")
    #expect(MuxaPipelineDefinition.Agent.canonicalDirection("sideways") == "sideways")

    let spelled = MuxaWorkOptions.Pipeline(name: "pair", agents: [
        MuxaWorkOptions.Agent(alias: "impl", program: "claude", direction: "auto"),
        MuxaWorkOptions.Agent(alias: "review", program: "codex", direction: "vertical", after: ["impl"]),
    ])
    let implied = MuxaWorkOptions(pipelines: [MuxaWorkOptions.Pipeline(name: "pair", agents: [
        MuxaWorkOptions.Agent(alias: "impl", program: "claude"),
        MuxaWorkOptions.Agent(alias: "review", program: "codex", direction: "down", after: ["impl"]),
    ])])
    #expect(MuxaPipelineDefinition(spelled).agents.map(\.direction) == ["", "down"])
    #expect(MuxaPipelineSyncState.compare(library: spelled, hostOptions: implied) == .inSync)

    // `right` is a choice, not the absence of one, so a host that pins it
    // differs from one that left the split to muxa.
    let pinned = MuxaWorkOptions.Pipeline(name: "pair", agents: [
        MuxaWorkOptions.Agent(alias: "impl", program: "claude", direction: "right"),
        MuxaWorkOptions.Agent(alias: "review", program: "codex", direction: "down", after: ["impl"]),
    ])
    #expect(MuxaPipelineSyncState.compare(library: pinned, hostOptions: implied) == .differs)

    var sideways = MuxaPipelineDefinition(spelled)
    sideways.agents[1].direction = "sideways"
    #expect(sideways.problems().contains { $0.contains("direction") })
}

@Test func readableMarkdownGroupsOrderedListsAndNormalizesNewlines() {
    let document = ReadableMarkdownDocument(source: "1. one\r\n2. two\r\n\r\nnext line")

    #expect(document.blocks == [
        .list(ordered: true, items: [
            .init(marker: "1.", text: "one", depth: 0),
            .init(marker: "2.", text: "two", depth: 0),
        ]),
        .paragraph("next line"),
    ])
}

private final class IPCProbe: @unchecked Sendable {
    private let lock = NSLock()
    private var activeRequests = 0
    private var maximumActiveRequests = 0
    private var attachmentBalance = 0
    private var maximumAttachmentBalance = 0
    private var offsets: [UInt64] = []
    private var resizes: [(columns: Int, rows: Int)] = []
    private var firstAttachDelayMicroseconds: useconds_t = 0
    private var delayedFirstAttach = false
    private var truncateFirstRead = false
    private var spawnedEnvironment: [String: String] = [:]
    private var spawnedColumns = 0
    private var spawnedRows = 0
    private var fleetCaptureAddress: (host: String, backend: String, socket: String, pane: String)?
    private var fleetPrompt: (host: String, pane: String, text: String, submit: Bool)?
    private var workStart: (work: String, workspace: String, cwd: String, noTicket: Bool)?
    private var requestKinds: [String] = []

    init(
        firstAttachDelayMicroseconds: useconds_t = 0,
        truncateFirstRead: Bool = false
    ) {
        self.firstAttachDelayMicroseconds = firstAttachDelayMicroseconds
        self.truncateFirstRead = truncateFirstRead
    }

    func request(path _: String, payload: Data) throws -> Data {
        lock.lock()
        activeRequests += 1
        maximumActiveRequests = max(maximumActiveRequests, activeRequests)
        lock.unlock()
        defer {
            lock.lock()
            activeRequests -= 1
            lock.unlock()
        }

        let object = try JSONSerialization.jsonObject(with: payload) as? [String: Any]
        let kind = object?["kind"] as? String
        lock.lock()
        if let kind { requestKinds.append(kind) }
        lock.unlock()
        switch kind {
        case "hello":
            usleep(20_000)
            return Self.json([
                "ok": true,
                "min_protocol": 1,
                "max_protocol": 6,
                "capabilities": [
                    "session_bytes_v1",
                    "session_attachment_identity_v1",
                    "session_wait_v1",
                    "pipeline_runs_v1",
                    "pipeline_subscribe",
                    "fleet_v1",
                    "fleet_raw_capture_v1",
                    "fleet_subscribe",
                    "work_control_v1",
                    "ask_subscribe",
                ],
            ])
        case "spawn_session":
            let pairs = object?["env"] as? [[String]] ?? []
            lock.lock()
            spawnedEnvironment = Dictionary(uniqueKeysWithValues: pairs.compactMap { pair in
                guard pair.count == 2 else { return nil }
                return (pair[0], pair[1])
            })
            spawnedColumns = object?["cols"] as? Int ?? 0
            spawnedRows = object?["rows"] as? Int ?? 0
            lock.unlock()
            return Self.json([
                "ok": true,
                "session": [
                    "id": "pty:spawned",
                    "backend": "pty",
                    "display_name": "Muxa Shell",
                    "cwd": "/tmp",
                    "attached_clients": 0,
                    "has_been_attached": false,
                    "exited": false,
                    "exit_status": NSNull(),
                    "pid": 1234,
                ],
            ])
        case "pipeline_runs":
            return Self.json([
                "ok": true,
                "pipeline_runs": [[
                    "identity": ["workspace_id": "muxa", "work_id": "native-app"],
                    "pipeline": "implement-review",
                    "desired": [[
                        "alias": "impl",
                        "program": "codex",
                        "role": "implementer",
                    ]],
                    "cwd": "/tmp/muxa",
                    "generation": 3,
                    "window_id": "@4",
                    "aliases": [
                        "impl": [
                            "alias": "impl",
                            "status": "running",
                            "generation": 3,
                            "pane": "%17",
                        ],
                    ],
                ]],
            ])
        case "work_up":
            let request = object?["request"] as? [String: Any]
            lock.lock()
            workStart = (
                request?["work"] as? String ?? "",
                request?["workspace"] as? String ?? "",
                request?["cwd"] as? String ?? "",
                request?["no_ticket"] as? Bool ?? false
            )
            lock.unlock()
            return Self.json([
                "ok": true,
                "work_operation": [
                    "operation_id": "native-work-1",
                    "state": "running",
                    "work": request?["work"] as? String ?? "",
                    "workspace": request?["workspace"] as? String ?? "",
                    "message": "Starting configured Work pipeline…",
                ],
            ])
        case "work_up_status":
            return Self.json([
                "ok": true,
                "work_operation": [
                    "operation_id": object?["operation_id"] as? String ?? "",
                    "state": "succeeded",
                    "work": "native-app",
                    "workspace": "muxa",
                    "message": "Work pipeline started",
                    "result": [
                        "work": "native-app",
                        "workspace": "muxa",
                        "pipeline": "implement-review",
                        "cwd": "/tmp/muxa",
                        "dry_run": false,
                    ],
                ],
            ])
        case "fleet_snapshot":
            return Self.json([
                "ok": true,
                "fleet": [
                    "hosts": [[
                        "alias": "local",
                        "local": true,
                        "mode": "control",
                        "state": "online",
                        "remote": [
                            "agents": [
                                [
                                    "kind": "codex",
                                    "agent_session_id": "agent-17",
                                    "pane": "%17",
                                    "tmux_socket": "default",
                                    "tmux_session": "muxa",
                                    "state": "working",
                                    "last_prompt": "Implement native window reporting",
                                    "last_prompt_at": "2026-08-31T09:59:00Z",
                                    "last_response": "Implemented the native window report.",
                                    "model": "gpt-5-codex",
                                    "context_used_pct": 42.5,
                                    "cost_usd": 0.12,
                                    "started_at": "2026-08-31T09:00:00Z",
                                    "last_activity_at": "2026-08-31T10:00:00Z",
                                    "state_entered_at": "2026-08-31T09:59:00Z",
                                    "workload": [
                                        "process_count": 3,
                                        "shell_count": 1,
                                        "subagent_count": 1,
                                        "helper_count": 1,
                                        "preview": [],
                                    ],
                                    "subagents": [[
                                        "kind": "reviewer",
                                        "description": "Review the window report",
                                        "started_at": "2026-08-31T09:58:00Z",
                                    ]],
                                ],
                                [
                                    "kind": "claude_code",
                                    "agent_session_id": "agent-local-deploy",
                                    "pane": "%18",
                                    "tmux_socket": "default",
                                    "tmux_session": "platform",
                                    "state": "idle",
                                    "last_prompt": "## Deploy\nShip the release",
                                ],
                            ],
                            "panes": [
                                [
                                    "pane_id": "%17",
                                    "session_id": "$4",
                                    "session": "muxa",
                                    "window_id": "@4",
                                    "window_name": "native-app",
                                    "window_index": "1",
                                    "pane_index": "0",
                                    "current_command": "codex",
                                    "title": "muxa",
                                    "current_path": "/tmp/muxa",
                                    "socket": "default",
                                    "workspace_id": "muxa",
                                    "work_id": "native-app",
                                ],
                                [
                                    "pane_id": "%18",
                                    "session_id": "$8",
                                    "session": "platform",
                                    "window_id": "@8",
                                    "window_name": "deploy",
                                    "window_index": "2",
                                    "pane_index": "0",
                                    "current_command": "claude",
                                    "title": "deploy",
                                    "current_path": "/tmp/platform",
                                    "socket": "default",
                                ],
                            ],
                        ],
                    ], [
                        "alias": "dev",
                        "local": false,
                        "mode": "observe",
                        "state": "online",
                        "latency_ms": 14,
                        "remote": [
                            "agents": [[
                                "kind": "codex",
                                "agent_session_id": "agent-remote-deploy",
                                "pane": "%2",
                                "tmux_socket": "default",
                                "tmux_session": "platform",
                                "state": "working",
                            ]],
                            "panes": [[
                                "pane_id": "%2",
                                "session_id": "$2",
                                "session": "platform",
                                "window_id": "@2",
                                "window_name": "deploy",
                                "window_index": "0",
                                "pane_index": "0",
                                "current_command": "codex",
                                "title": "deploy",
                                "current_path": "/srv/platform",
                                "socket": "default",
                                "workspace_id": "platform",
                                "work_id": "DEPLOY-1",
                            ]],
                        ],
                    ]],
                ],
            ])
        case "fleet_command":
            let operation = object?["operation"] as? [String: Any]
            let pane = operation?["pane"] as? [String: Any]
            let window = pane?["window"] as? [String: Any]
            let session = window?["session"] as? [String: Any]
            lock.lock()
            fleetCaptureAddress = (
                object?["host"] as? String ?? "",
                session?["host"] as? String ?? "",
                session?["socket"] as? String ?? "",
                pane?["pane_id"] as? String ?? ""
            )
            if operation?["kind"] as? String == "send_prompt" {
                fleetPrompt = (
                    object?["host"] as? String ?? "",
                    pane?["pane_id"] as? String ?? "",
                    operation?["text"] as? String ?? "",
                    operation?["submit"] as? Bool ?? false
                )
            }
            lock.unlock()
            if operation?["kind"] as? String == "send_prompt" {
                return Self.json([
                    "ok": true,
                    "fleet_result": [
                        "accepted": true,
                        "message": "sent",
                    ],
                ])
            }
            return Self.json([
                "ok": true,
                "fleet_result": [
                    "accepted": true,
                    "capture": "\u{001B}[31m$ echo 안녕\u{001B}[0m\n안녕",
                    "capture_raw_base64": Data("\u{001B}[31m$ echo 안녕\u{001B}[0m\r\n안녕".utf8).base64EncodedString(),
                ],
            ])
        case "set_session_attached":
            let attached = object?["attached"] as? Bool ?? false
            var delay: useconds_t = 0
            lock.lock()
            if attached, !delayedFirstAttach {
                delayedFirstAttach = true
                delay = firstAttachDelayMicroseconds
            }
            attachmentBalance += attached ? 1 : -1
            maximumAttachmentBalance = max(maximumAttachmentBalance, attachmentBalance)
            lock.unlock()
            if delay > 0 { usleep(delay) }
            return Self.json(["ok": true])
        case "read_session", "read_session_wait":
            let offset = (object?["offset"] as? NSNumber)?.uint64Value ?? 0
            lock.lock()
            offsets.append(offset)
            let readIndex = offsets.count
            lock.unlock()
            if truncateFirstRead, readIndex == 1 {
                return Self.output(offset: 50, nextOffset: 50, exited: false, truncated: true)
            }
            return Self.output(offset: offset, nextOffset: offset, exited: true, truncated: false)
        case "resize_session":
            lock.lock()
            resizes.append((object?["cols"] as? Int ?? 0, object?["rows"] as? Int ?? 0))
            lock.unlock()
            return Self.json(["ok": true])
        default:
            return Self.json(["ok": true])
        }
    }

    func snapshot() -> (maximumActive: Int, attachmentBalance: Int, maximumAttachment: Int, offsets: [UInt64]) {
        lock.lock()
        defer { lock.unlock() }
        return (
            maximumActiveRequests,
            attachmentBalance,
            maximumAttachmentBalance,
            offsets
        )
    }

    func lastSpawn() -> (environment: [String: String], columns: Int, rows: Int) {
        lock.lock()
        defer { lock.unlock() }
        return (spawnedEnvironment, spawnedColumns, spawnedRows)
    }

    func lastFleetCaptureAddress() -> (host: String, backend: String, socket: String, pane: String)? {
        lock.lock()
        defer { lock.unlock() }
        return fleetCaptureAddress
    }

    func lastFleetPrompt() -> (host: String, pane: String, text: String, submit: Bool)? {
        lock.lock()
        defer { lock.unlock() }
        return fleetPrompt
    }

    func lastWorkStart() -> (work: String, workspace: String, cwd: String, noTicket: Bool)? {
        lock.lock()
        defer { lock.unlock() }
        return workStart
    }

    func lastResize() -> (columns: Int, rows: Int)? {
        lock.lock()
        defer { lock.unlock() }
        return resizes.last
    }

    func didRequest(_ kind: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return requestKinds.contains(kind)
    }

    private static func output(
        offset: UInt64,
        nextOffset: UInt64,
        exited: Bool,
        truncated: Bool
    ) -> Data {
        json([
            "ok": true,
            "output": [
                "session_id": "pty:test",
                "offset": offset,
                "next_offset": nextOffset,
                "data": "",
                "data_base64": "",
                "truncated": truncated,
                "exited": exited,
                "exit_status": exited ? 0 : NSNull(),
            ],
        ])
    }

    private static func json(_ object: [String: Any]) -> Data {
        (try? JSONSerialization.data(withJSONObject: object))
            ?? Data(#"{"ok":false,"error":"test encoding failed"}"#.utf8)
    }
}

struct MuxaIPCTests {
    @Test
    func detectsXCTestHostEnvironment() {
        #expect(
            AppModel.isRunningTests(
                environment: ["XCTestConfigurationFilePath": "/tmp/Muxa.xctestconfiguration"]
            )
        )
        #expect(!AppModel.isRunningTests(environment: [:]))
    }

    @Test
    func environmentSocketOverridesDefault() {
        #expect(MuxaIPCClient.defaultSocketPath(environment: ["MUXA_SOCKET": "/tmp/custom.sock"]) == "/tmp/custom.sock")
    }

    @Test
    func emptyEnvironmentSocketUsesUIDFallback() {
        #expect(MuxaIPCClient.defaultSocketPath(environment: [:]).hasPrefix("/tmp/muxa-"))
    }

    @Test
    func outputDecodesByteSafePayload() throws {
        let source = Data([0x1B, 0x5B, 0x31, 0x6D, 0xFF, 0x00])
        let json = """
        {
          "session_id": "pty:test",
          "offset": 0,
          "next_offset": 6,
          "data": "lossy",
          "data_base64": "\(source.base64EncodedString())",
          "truncated": false,
          "exited": false,
          "exit_status": null
        }
        """
        let output = try JSONDecoder().decode(MuxaSessionOutput.self, from: Data(json.utf8))
        #expect(output.bytes == source)
    }

    @Test
    func terminalReadUsesEventDrivenWaitWhenDaemonAdvertisesIt() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()

        _ = try await client.readSession(id: "pty:test", offset: 0, waitForChanges: true)

        #expect(probe.didRequest("read_session_wait"))
        #expect(!probe.didRequest("read_session"))
    }

    @Test
    func shellSpawnCarriesTerminalContractAndConservativeInitialGrid() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        _ = try await client.spawnShell(
            command: "/bin/zsh",
            cwd: "/tmp",
            name: "Muxa Shell",
            environment: [
                "TERM": "xterm-256color",
                "COLORTERM": "truecolor",
                "LC_CTYPE": "en_US.UTF-8",
            ]
        )

        let spawn = probe.lastSpawn()
        #expect(spawn.environment["TERM"] == "xterm-256color")
        #expect(spawn.environment["COLORTERM"] == "truecolor")
        #expect(spawn.environment["LC_CTYPE"] == "en_US.UTF-8")
        #expect(spawn.columns == 80)
        #expect(spawn.rows == 24)
    }

    @Test
    func keepsManagedWorkSeparateFromIndependentFleetAgents() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()

        let runs = try await client.listPipelineRuns()
        let execution = try await client.executionSnapshot()

        #expect(runs.count == 1)
        #expect(runs[0].identity.id == "muxa/native-app")
        #expect(runs[0].aliases["impl"]?.pane == "%17")
        #expect(execution.agents.count == 3)
        #expect(execution.agents[0].lastActivityAt == "2026-08-31T10:00:00Z")
        #expect(execution.agents[0].lastPromptAt == "2026-08-31T09:59:00Z")
        #expect(execution.agents[0].workload?.processCount == 3)
        #expect(execution.agents[0].subagents?.first?.kind == "reviewer")
        #expect(execution.pane(for: execution.agents[0])?.windowID == "@4")

        let workGroups = execution.workGroups(pipelineRuns: runs)
        #expect(workGroups.count == 2)
        #expect(workGroups[0].participants.count == 1)
        #expect(workGroups[0].pipelineRun != nil)
        #expect(workGroups[1].identity.id == "platform/DEPLOY-1")
        #expect(workGroups[1].pipelineRun == nil)
        #expect(workGroups[1].hostAliases == ["dev"])
        #expect(execution.hostedAgents.count == 3)
        #expect(Set(execution.hostedAgents.map(\.id)).count == 3)
        #expect(execution.hostedAgents.filter { $0.pane?.windowName == "deploy" }.count == 2)

        let watchHosts = execution.watchHosts
        #expect(watchHosts.map(\.host.alias) == ["local", "dev"])
        #expect(watchHosts[0].sessions.count == 2)
        #expect(watchHosts[0].paneCount == 2)
        #expect(watchHosts[1].sessions.first?.windows.first?.panes.first?.host.alias == "dev")
        let remoteWindow = try #require(watchHosts[1].sessions.first?.windows.first)
        let remoteWatchPane = try #require(remoteWindow.panes.first)
        #expect(execution.watchWindow(containing: remoteWatchPane.id)?.id == remoteWindow.id)

        let remotePane = remoteWatchPane.pane
        let exactAddress = try AppModel.exactPaneAddressJSON(remotePane)
        let address = try #require(
            JSONSerialization.jsonObject(with: Data(exactAddress.utf8)) as? [String: Any]
        )
        let window = try #require(address["window"] as? [String: Any])
        let session = try #require(window["session"] as? [String: Any])
        #expect(session["host"] as? String == "tmux")
        #expect(session["socket"] as? String == "default")
        #expect(address["pane_id"] as? String == "%2")
    }

    @Test
    func nativeWorkControlStartsAndPollsCanonicalOperation() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let started = try await client.startWork(
            MuxaWorkStartRequest(
                work: "native-app",
                workspace: "muxa",
                pipeline: "implement-review",
                cwd: "/tmp/muxa",
                external: nil,
                skill: nil,
                body: "Build the native console",
                context: nil,
                dryRun: false
            )
        )
        #expect(started.state == .running)
        let completed = try await client.workOperation(id: started.operationID)
        #expect(completed.state == .succeeded)
        #expect(completed.result?.workspace == "muxa")
        #expect(completed.result?.work == "native-app")
        let request = try #require(probe.lastWorkStart())
        #expect(request.work == "native-app")
        #expect(request.workspace == "muxa")
        #expect(request.cwd == "/tmp/muxa")
        #expect(request.noTicket)
    }

    @Test
    func askIPCExposesDurableHistoryAndAgentSelection() async throws {
        let handler: MuxaIPCRequestHandler = { _, payload in
            let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
            switch object["kind"] as? String {
            case "hello":
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "min_protocol": 1,
                    "max_protocol": 6,
                    "capabilities": [
                        "session_bytes_v1",
                        "session_attachment_identity_v1",
                        "work_control_v1",
                        "ask_one_turn_credential_v1",
                        "ask_conversations_v1",
                    ],
                ])
            case "ask_agent":
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "ask_agent": object["agent"] as? String ?? "claude",
                ])
            case "ask_status":
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "ask_enabled": true,
                ])
            case "ask_send":
                let credential = try #require(object["credential"] as? [String: Any])
                #expect(credential["agent"] as? String == "codex")
                #expect(credential["api_key"] as? String == "test-only-secret")
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "ask_entry": [
                        "id": "ask-1",
                        "prompt": object["prompt"] as? String ?? "",
                        "answer": "",
                        "status": "running",
                        "agent": "codex",
                        "cwd": "/tmp/muxa",
                        "asked_at": "2026-08-31T10:00:00Z",
                    ],
                ])
            case "ask_list":
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "ask_entries": [[
                        "id": "ask-1",
                        "prompt": "Review the plan",
                        "answer": "Looks good.",
                        "status": "answered",
                        "agent": "codex",
                        "cwd": "/tmp/muxa",
                        "asked_at": "2026-08-31T10:00:00Z",
                        "answered_at": "2026-08-31T10:00:03Z",
                    ]],
                ])
            case "ask_conversation_list":
                let conversation: [String: Any] = [
                    "id": "conversation-1",
                    "title": "Review the plan",
                    "agent": "codex",
                    "agent_session_id": "codex-session-1",
                    "created_at": "2026-08-31T10:00:00Z",
                    "updated_at": "2026-08-31T10:00:03Z",
                ]
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "ask_conversations": [conversation],
                    "ask_conversation": conversation,
                ])
            case "ask_conversation_select":
                #expect(object["conversation_id"] as? String == "conversation-1")
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "ask_conversation": [
                        "id": "conversation-1",
                        "title": "Review the plan",
                        "agent": "codex",
                        "agent_session_id": "codex-session-1",
                        "created_at": "2026-08-31T10:00:00Z",
                        "updated_at": "2026-08-31T10:00:03Z",
                    ],
                ])
            default:
                return try JSONSerialization.data(withJSONObject: ["ok": true])
            }
        }
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-ask-test.sock", request: handler)
        try await client.hello()

        #expect(try await client.askStatus())
        #expect(try await client.selectAskAgent("codex") == "codex")
        let pending = try await client.sendAsk(
            "Review the plan",
            agent: "codex",
            apiKey: "test-only-secret"
        )
        #expect(pending.prompt == "Review the plan")
        #expect(pending.status == "running")
        let history = try await client.listAskEntries()
        #expect(history.first?.answer == "Looks good.")
        let conversations = try await client.listAskConversations()
        #expect(conversations.active?.id == "conversation-1")
        #expect(conversations.conversations.first?.agentSessionID == "codex-session-1")
        let resumed = try await client.selectAskConversation("conversation-1")
        #expect(resumed.title == "Review the plan")
    }

    @Test
    func collaborationIPCUsesSelectedFleetPaneAndDecodesMailbox() async throws {
        let handler: MuxaIPCRequestHandler = { _, payload in
            let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
            if object["kind"] as? String == "hello" {
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "min_protocol": 1,
                    "max_protocol": 6,
                    "capabilities": [
                        "session_bytes_v1",
                        "session_attachment_identity_v1",
                        "work_control_v1",
                        "fleet_v1",
                    ],
                ])
            }
            let operation = try #require(object["operation"] as? [String: Any])
            let pane = try #require(operation["pane"] as? [String: Any])
            let paneID = pane["pane_id"] as? String ?? ""
            let participant: [String: Any] = [
                "agent_kind": "codex",
                "agent_session_id": "agent-17",
                "pane": paneID,
                "room": ["host": "tmux", "window_id": "@4"],
                "state": "working",
                "alias": "impl",
            ]
            let console: [String: Any] = [
                "agent_kind": "unknown",
                "agent_session_id": "__muxa_console__",
                "pane": "console",
                "room": ["host": "tmux", "window_id": "@4"],
                "state": "idle",
                "console": true,
            ]
            let requestBody = operation["request"] as? [String: Any]
            let request: [String: Any] = [
                "id": "req-1",
                "from": console,
                "to": participant,
                "kind": requestBody?["kind"] as? String ?? "question",
                "body": requestBody?["body"] as? String ?? "Review this",
                "expects_reply": true,
                "work_mode": requestBody?["work_mode"] as? String ?? "read_only",
                "status": "queued",
                "created_at": "2026-08-31T10:00:00Z",
            ]
            let result: [String: Any]
            if operation["kind"] as? String == "collaboration_mailbox" {
                result = ["accepted": true, "collaboration_incoming": [request], "collaboration_sent": [request]]
            } else {
                result = ["accepted": true, "collaboration_request": request]
            }
            return try JSONSerialization.data(withJSONObject: ["ok": true, "fleet_result": result])
        }
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-collab-test.sock", request: handler)
        try await client.hello()
        let host = MuxaFleetHostIdentity(alias: "local", local: true, state: "online", mode: "control")
        let pane = MuxaPaneInfo(
            paneID: "%17",
            sessionID: "$4",
            session: "muxa",
            windowID: "@4",
            windowName: "native-app",
            windowIndex: "1",
            paneIndex: "0",
            currentCommand: "codex",
            title: "muxa",
            currentPath: "/tmp/muxa",
            socket: "default",
            workspaceID: nil,
            workID: nil,
            agentRole: "implementer",
            agentAlias: "impl"
        )

        let sent = try await client.sendCollaboration(
            host: host,
            pane: pane,
            kind: "review",
            body: "Review this",
            workMode: "read_only"
        )
        #expect(sent.to.pane == "%17")
        #expect(sent.kind == "review")
        let mailbox = try await client.collaborationMailbox(host: host, pane: pane)
        #expect(mailbox.incoming.first?.to.label == "@impl")
        #expect(mailbox.sent.first?.from.label == "operator")
    }

    @Test @MainActor
    func operatorInboxOpensStableAgentWhenItsRecordedPaneIsStale() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let execution = try await client.executionSnapshot()
        let route = try #require(
            execution.watchHosts.first(where: { $0.host.alias == "local" })?
                .sessions.first?.windows.first?.panes.first
        )
        let request = try JSONDecoder().decode(
            MuxaCollaborationRequest.self,
            from: Data(#"""
            {
              "id":"request-stale-pane",
              "from":{"agent_kind":"unknown","agent_session_id":"__muxa_console__","pane":"console","room":{"host":"tmux","window_id":"@4"},"console":true},
              "to":{"agent_kind":"codex","agent_session_id":"agent-17","pane":"%999","socket":"old-socket","room":{"host":"tmux","window_id":"@999"},"alias":"impl"},
              "kind":"question","body":"Choose a release target","expects_reply":true,"work_mode":"read_only","status":"queued","created_at":"2026-08-31T10:00:00Z"
            }
            """#.utf8)
        )
        let message = MuxaOperatorMessage(
            host: route.host,
            routePane: route.pane,
            request: request
        )

        let selection = AppModel.operatorSelection(for: message, in: execution)

        #expect(selection == .agent("local:agent-17"))
    }

    @Test @MainActor
    func operatorInboxOpensRecordedSessionWhenAgentAndWindowEnded() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let execution = try await client.executionSnapshot()
        let route = try #require(
            execution.watchHosts.first(where: { $0.host.alias == "local" })?
                .sessions.first?.windows.first?.panes.first
        )
        let request = try JSONDecoder().decode(
            MuxaCollaborationRequest.self,
            from: Data(#"""
            {
              "id":"request-ended-agent",
              "from":{"agent_kind":"unknown","agent_session_id":"__muxa_console__","pane":"console","room":{"host":"tmux","window_id":"@4"},"console":true},
              "to":{"agent_kind":"claude_code","agent_session_id":"ended-agent","pane":"%999","socket":"default","room":{"host":"tmux","socket":"default","window_id":"@999"},"alias":"claude","tmux_session_id":"$4","tmux_session_name":"muxa","window_name":"ended"},
              "kind":"task","body":"Historical request","expects_reply":true,"work_mode":"read_only","status":"completed","created_at":"2026-08-31T10:00:00Z"
            }
            """#.utf8)
        )
        let message = MuxaOperatorMessage(
            host: route.host,
            routePane: route.pane,
            request: request
        )

        let selection = AppModel.operatorSelection(for: message, in: execution)

        #expect(selection == .fleetSession(MuxaWatchSessionIdentity(
            hostAlias: "local",
            socket: "default",
            sessionID: "$4"
        )))
    }

    @Test
    func operatorInboxSeparatesHumanDecisionsFromOrdinaryReplies() throws {
        func message(replyStatus: String) throws -> MuxaOperatorMessage {
            let request = try JSONDecoder().decode(
                MuxaCollaborationRequest.self,
                from: Data("""
                {
                  "id":"request-\(replyStatus)",
                  "from":{"agent_kind":"unknown","agent_session_id":"__muxa_console__","pane":"console","room":{"host":"tmux","window_id":"@4"},"console":true},
                  "to":{"agent_kind":"codex","agent_session_id":"agent-17","pane":"%17","room":{"host":"tmux","window_id":"@4"},"alias":"impl"},
                  "kind":"review","body":"Review this","expects_reply":true,"work_mode":"read_only","status":"\(replyStatus)","created_at":"2026-08-31T10:00:00Z",
                  "reply":{"status":"\(replyStatus)","body":"Result","at":"2026-08-31T10:01:00Z"}
                }
                """.utf8)
            )
            return MuxaOperatorMessage(
                host: MuxaFleetHostIdentity(alias: "local", local: true, state: "online", mode: "control"),
                routePane: MuxaPaneInfo(
                    paneID: "%17",
                    sessionID: "$4",
                    session: "muxa",
                    windowID: "@4",
                    windowName: "native-app",
                    windowIndex: "1",
                    paneIndex: "0",
                    currentCommand: "codex",
                    title: "muxa",
                    currentPath: "/tmp/muxa",
                    socket: "default",
                    workspaceID: nil,
                    workID: nil,
                    agentRole: "implementer",
                    agentAlias: "impl"
                ),
                request: request
            )
        }

        #expect(try message(replyStatus: "blocked").needsHumanDecision)
        #expect(try message(replyStatus: "declined").needsHumanDecision)
        #expect(!(try message(replyStatus: "completed").needsHumanDecision))
    }

    @Test
    func fleetShellModuleSendsToExactHostPane() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let execution = try await client.executionSnapshot()
        let target = try #require(
            execution.watchHosts
                .first(where: { $0.host.alias == "dev" })?
                .sessions.first?.windows.first?.panes.first
        )
        try await client.sendFleetPrompt(
            host: target.host,
            pane: target.pane,
            text: "continue with the review"
        )
        let sent = try #require(probe.lastFleetPrompt())
        #expect(sent.host == "dev")
        #expect(sent.pane == "%2")
        #expect(sent.text == "continue with the review")
        #expect(sent.submit)
    }

    @Test
    func fleetCaptureUsesTheExactGlobalPaneAddress() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let execution = try await client.executionSnapshot()
        let target = try #require(
            execution.hostedAgents.first(where: { $0.host.alias == "dev" })
        )
        let pane = try #require(target.pane)

        let capture = try await client.captureFleetPane(host: target.host, pane: pane)

        #expect(capture.screenText?.contains("안녕") == true)
        #expect(capture.rawBytes == Data("\u{001B}[31m$ echo 안녕\u{001B}[0m\r\n안녕".utf8))
        let address = try #require(probe.lastFleetCaptureAddress())
        #expect(address.host == "dev")
        #expect(address.backend == "tmux")
        #expect(address.socket == "default")
        #expect(address.pane == "%2")
    }

    @Test
    func parsesOwnerOnlyMuxadFromLsofFieldOutput() {
        let records = DaemonSocketOwner.parseLsof(
            "p4798\ncmuxad\nu501\nf10\np9999\nczsh\nu501\nf4\n"
        )
        #expect(records.count == 2)
        #expect(records[0].pid == 4798)
        #expect(records[0].command == "muxad")
        #expect(records[0].uid == 501)
        #expect(records[1].command == "zsh")
    }

    @Test
    func recognizesHomebrewDaemonLocations() {
        #expect(
            DaemonSocketOwner.homebrewExecutablePath(
                for: "/opt/homebrew/Cellar/muxa/0.8.35/bin/muxad"
            ) == "/opt/homebrew/bin/brew"
        )
        #expect(
            DaemonSocketOwner.homebrewExecutablePath(
                for: "/Users/test/.cargo/bin/muxad"
            ) == nil
        )
        #expect(DaemonSocketOwner.legacyLaunchAgentLabels.contains("dev.open330.muxad"))
    }

    @Test
    func serializesConcurrentIPCRequests() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)

        try await withThrowingTaskGroup(of: Void.self) { group in
            for _ in 0..<24 {
                group.addTask {
                    try await client.hello()
                }
            }
            try await group.waitForAll()
        }

        #expect(probe.snapshot().maximumActive == 1)
    }

    @Test @MainActor
    func rapidPaneRestartBalancesAttachmentLifecycle() async throws {
        let probe = IPCProbe(firstAttachDelayMicroseconds: 80_000)
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let pane = TerminalPaneModel(
            client: client,
            sessionID: "pty:test",
            replayInitialHistory: false
        )

        pane.start()
        try await Task.sleep(for: .milliseconds(10))
        pane.stop()
        pane.start()
        try await Task.sleep(for: .milliseconds(250))
        pane.stop()
        try await Task.sleep(for: .milliseconds(150))

        let snapshot = probe.snapshot()
        #expect(snapshot.attachmentBalance == 0)
        #expect(snapshot.maximumAttachment == 1)
    }

    @Test @MainActor
    func emptyTruncatedReadAdvancesTerminalCursor() async throws {
        let probe = IPCProbe(truncateFirstRead: true)
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let pane = TerminalPaneModel(
            client: client,
            sessionID: "pty:test",
            replayInitialHistory: true
        )

        pane.start()
        try await Task.sleep(for: .milliseconds(180))
        pane.stop()
        try await Task.sleep(for: .milliseconds(100))

        #expect(Array(probe.snapshot().offsets.prefix(2)) == [0, 50])
    }

    @Test @MainActor
    func normalShellExitIsAHostStateNotATerminalError() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let pane = TerminalPaneModel(
            client: client,
            sessionID: "pty:test",
            replayInitialHistory: false
        )

        pane.start()
        try await Task.sleep(for: .milliseconds(120))

        #expect(pane.exited)
        #expect(pane.exitStatus == 0)
        #expect(pane.errorMessage == nil)
        pane.stop()
    }

    @Test
    func initialTerminalResizeSurvivesPumpActivation() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let pump = TerminalSessionIOPump(client: client, sessionID: "pty:test")

        await pump.enqueueResize(columns: 132, rows: 41)
        await pump.setActive(true)
        try await Task.sleep(for: .milliseconds(80))
        let resize = try #require(probe.lastResize())

        #expect(resize.columns == 132)
        #expect(resize.rows == 41)
        await pump.setActive(false)
    }

    @Test @MainActor
    func workbenchPreviewReplacesOnlyThePreviousPreview() throws {
        let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
        let groupID = tabs.focusedGroupID

        tabs.openPreview(.host("host-a"))
        tabs.openPreview(.host("host-b"))
        var group = try #require(tabs.group(id: groupID))
        #expect(group.tabs == [.workBoard, .host("host-b")])
        #expect(group.preview == .host("host-b"))

        tabs.pin(.host("host-b"), groupID: groupID)
        tabs.openPreview(.shell("pty:1"))
        group = try #require(tabs.group(id: groupID))
        #expect(group.tabs == [.workBoard, .host("host-b"), .shell("pty:1")])
        #expect(group.preview == .shell("pty:1"))
    }

    @Test @MainActor
    func closingEditorReturnsToMostRecentlyUsedOpenTab() {
        let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
        let groupID = tabs.focusedGroupID
        tabs.openPinned(.host("host-a"))
        tabs.openPinned(.shell("pty:1"))
        tabs.activate(.host("host-a"), groupID: groupID)

        let next = tabs.close(.host("host-a"), groupID: groupID)

        #expect(next == .shell("pty:1"))
        #expect(tabs.focusedSelection == .shell("pty:1"))
    }

    @Test @MainActor
    func splittingEditorCreatesIndependentPinnedGroup() throws {
        let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
        let sourceID = tabs.focusedGroupID
        tabs.openPreview(.shell("pty:1"))

        let targetID = tabs.splitRight(selection: .shell("pty:1"), from: sourceID)
        let target = try #require(tabs.group(id: targetID))

        #expect(tabs.groups.count == 2)
        #expect(target.tabs == [.shell("pty:1")])
        #expect(target.preview == nil)
        #expect(tabs.focusedSelection == .shell("pty:1"))
    }

    @Test @MainActor
    func workbenchRestoresPinnedTabsAndEditorGroups() throws {
        let suite = "dev.muxa.tests.workbench.\(UUID().uuidString)"
        let defaults = try #require(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let key = "tabs"
        let tabs = MuxaWorkbenchTabs(persistenceKey: key, defaults: defaults)
        tabs.openPinned(.host("host-a"))
        _ = tabs.splitRight(selection: .shell("pty:1"), from: tabs.focusedGroupID)

        let restored = MuxaWorkbenchTabs(persistenceKey: key, defaults: defaults)

        #expect(restored.groups.count == 2)
        #expect(restored.groups.flatMap(\.tabs).contains(.host("host-a")))
        #expect(restored.focusedSelection == .shell("pty:1"))
    }

    @Test @MainActor
    func activityBarSeparatesOutcomesTopologyInboxAndShells() {
        let model = AppModel()
        #expect(MuxaSidebarMode.allCases == [.work, .watch, .inbox, .shells])

        model.select(.host("rtzr"))

        #expect(model.sidebarMode == .watch)
        #expect(model.sidebarSelection == .host("rtzr"))
    }

    @Test @MainActor
    func watchTreeSelectionOpensAPaneEditor() {
        let model = AppModel()
        let pane = MuxaWatchPaneIdentity(
            hostAlias: "local",
            socket: "/tmp/tmux.sock",
            paneID: "%7"
        )

        model.selectWatchPane(pane)

        #expect(model.watchSelection == pane)
        #expect(model.sidebarSelection == .pane(pane))
        #expect(model.sidebarMode == .watch)
    }

    @Test @MainActor
    func globalAskIsAnIndependentEditorRoute() {
        let model = AppModel()

        model.select(.ask)

        #expect(model.sidebarSelection == .ask)
        #expect(model.sidebarMode == .inbox)
    }

    @Test @MainActor
    func operatorInboxIsAnIndependentEditorRoute() {
        let model = AppModel()

        model.select(.inbox)

        #expect(model.sidebarSelection == .inbox)
        #expect(model.sidebarMode == .inbox)
    }

    @Test @MainActor
    func activatingAnOpenPaneTabRestoresItsExplorerSelection() {
        let model = AppModel()
        let first = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%1")
        let second = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%2")
        model.selectWatchPane(second)

        model.activateEditor(.pane(first))

        #expect(model.sidebarSelection == .pane(first))
        #expect(model.watchSelection == first)
        #expect(model.sidebarMode == .watch)
    }

    @Test @MainActor
    func agentSelectionUsesTheAttentionInbox() {
        let model = AppModel()
        model.select(.agent("local:agent-17"))

        #expect(model.sidebarMode == .inbox)
        #expect(model.sidebarSelection == .agent("local:agent-17"))
        #expect(!MuxaSidebarMode.allCases.map(\.rawValue).contains("agents"))
    }

    @Test @MainActor
    func fleetSessionSelectionOpensAResourceSummaryWithoutChangingPane() {
        let model = AppModel()
        let pane = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%1")
        let session = MuxaWatchSessionIdentity(
            hostAlias: "rtzr",
            socket: "default",
            sessionID: "$107"
        )
        model.selectWatchPane(pane)

        model.selectWatchSession(session)

        #expect(model.sidebarMode == .watch)
        #expect(model.sidebarSelection == .fleetSession(session))
        #expect(model.watchSelection == pane)
    }

    @Test @MainActor
    func fleetWindowSelectionOpensAResourceSummaryWithoutChangingPane() {
        let model = AppModel()
        let pane = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%1")
        let window = MuxaWatchWindowIdentity(
            hostAlias: "rtzr",
            socket: "default",
            sessionID: "$107",
            windowID: "@213"
        )
        model.selectWatchPane(pane)

        model.selectWatchWindow(window)

        #expect(model.sidebarMode == .watch)
        #expect(model.sidebarSelection == .fleetWindow(window))
        #expect(model.watchSelection == pane)
    }
}

// MARK: - Operator Inbox per-host refresh contract

/// Wraps `IPCProbe` so the console mailbox of each fleet host can be scripted
/// and made to fail independently, which is what the Inbox refresh contract is
/// about. Every other request is answered by the base probe, including the
/// two-host `fleet_snapshot` ("local" and "dev") that gives the refresh its
/// per-host route panes.
private final class InboxMailboxProbe: @unchecked Sendable {
    private let lock = NSLock()
    private let base = IPCProbe()
    private var sentByHost: [String: [[String: Any]]] = [:]
    private var failureByHost: [String: String] = [:]
    private var mailboxReads: [String] = []
    private var getReads: [String] = []

    func setMailbox(host: String, sent: [[String: Any]]) {
        lock.lock()
        defer { lock.unlock() }
        sentByHost[host] = sent
    }

    /// `nil` restores a healthy host.
    func setFailure(host: String, message: String?) {
        lock.lock()
        defer { lock.unlock() }
        failureByHost[host] = message
    }

    /// Hosts whose mailbox was read since the last reset, in request order.
    func mailboxReadHosts() -> [String] {
        lock.lock()
        defer { lock.unlock() }
        return mailboxReads
    }

    /// Hosts whose durable collaboration get was called since the last reset.
    func getReadHosts() -> [String] {
        lock.lock()
        defer { lock.unlock() }
        return getReads
    }

    func resetReadLog() {
        lock.lock()
        defer { lock.unlock() }
        mailboxReads.removeAll()
        getReads.removeAll()
    }

    func request(path: String, payload: Data) throws -> Data {
        let object = try JSONSerialization.jsonObject(with: payload) as? [String: Any]
        guard object?["kind"] as? String == "fleet_command",
              let host = object?["host"] as? String,
              let operation = object?["operation"] as? [String: Any],
              let kind = operation["kind"] as? String,
              kind == "collaboration_mailbox" || kind == "collaboration_get"
        else {
            return try base.request(path: path, payload: payload)
        }

        lock.lock()
        defer { lock.unlock() }
        if kind == "collaboration_mailbox" {
            mailboxReads.append(host)
            if let failure = failureByHost[host] {
                return try JSONSerialization.data(withJSONObject: ["ok": false, "error": failure])
            }
            return try JSONSerialization.data(withJSONObject: [
                "ok": true,
                "fleet_result": [
                    "accepted": true,
                    "collaboration_incoming": [],
                    "collaboration_sent": sentByHost[host] ?? [],
                ],
            ])
        }

        // collaboration_get: like muxad's `get_for`, the sender reading a
        // replied request stamps `reply_read_at` and returns the request.
        getReads.append(host)
        let requestID = operation["request_id"] as? String ?? ""
        var sent = sentByHost[host] ?? []
        guard let index = sent.firstIndex(where: { $0["id"] as? String == requestID }) else {
            return try JSONSerialization.data(withJSONObject: [
                "ok": false,
                "error": "collaboration request \(requestID) not found",
            ])
        }
        if sent[index]["reply"] != nil, sent[index]["reply_read_at"] == nil {
            sent[index]["reply_read_at"] = "2026-09-02T12:00:00Z"
        }
        sentByHost[host] = sent
        return try JSONSerialization.data(withJSONObject: [
            "ok": true,
            "fleet_result": ["accepted": true, "collaboration_request": sent[index]],
        ])
    }
}

/// A console-sent request in the wire shape `MuxaCollaborationRequest` decodes.
private func inboxSentRequest(
    id: String,
    createdAt: String,
    body: String = "Review the release notes",
    status: String = "queued",
    reply: (status: String, body: String, at: String)? = nil,
    replyReadAt: String? = nil
) -> [String: Any] {
    var object: [String: Any] = [
        "id": id,
        "from": [
            "agent_kind": "unknown",
            "agent_session_id": "__muxa_console__",
            "pane": "console",
            "room": ["host": "tmux", "window_id": "@4"],
            "console": true,
        ],
        "to": [
            "agent_kind": "codex",
            "agent_session_id": "agent-17",
            "pane": "%17",
            "room": ["host": "tmux", "window_id": "@4"],
            "alias": "impl",
        ],
        "kind": "task",
        "body": body,
        "expects_reply": true,
        "work_mode": "read_only",
        "status": status,
        "created_at": createdAt,
    ]
    if let reply {
        object["reply"] = ["status": reply.status, "body": reply.body, "at": reply.at]
    }
    if let replyReadAt {
        object["reply_read_at"] = replyReadAt
    }
    return object
}

@MainActor
private func makeInboxModel(probe: InboxMailboxProbe) async throws -> AppModel {
    let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
    try await client.hello()
    let model = AppModel(client: client)
    let execution = try await client.executionSnapshot()
    model.ingestExecutionSnapshotForTesting(execution)
    return model
}

private func decodeInboxRequest(_ object: [String: Any]) throws -> MuxaCollaborationRequest {
    try JSONDecoder().decode(
        MuxaCollaborationRequest.self,
        from: JSONSerialization.data(withJSONObject: object)
    )
}

struct MuxaOperatorInboxRefreshTests {
    private static let sshHiccup = "remote request_failed: reading collaboration mailbox"

    @Test @MainActor
    func hostFailureKeepsThatHostsMessagesAndClearsWhenItRecovers() async throws {
        let probe = InboxMailboxProbe()
        probe.setMailbox(host: "local", sent: [
            inboxSentRequest(id: "local-old", createdAt: "2026-09-02T10:00:00Z"),
        ])
        probe.setMailbox(host: "dev", sent: [
            inboxSentRequest(id: "dev-old", createdAt: "2026-09-02T09:00:00Z"),
        ])
        let model = try await makeInboxModel(probe: probe)

        await model.refreshOperatorInbox(force: true)

        #expect(model.operatorMessages.map(\.id) == ["local:local-old", "dev:dev-old"])
        #expect(model.inboxHostFailures.isEmpty)
        #expect(model.inboxHostFailureSummary == nil)

        // Host A changes its mailbox while host B's SSH read fails.
        probe.setMailbox(host: "local", sent: [
            inboxSentRequest(id: "local-new", createdAt: "2026-09-02T11:00:00Z"),
        ])
        probe.setFailure(host: "dev", message: Self.sshHiccup)

        await model.refreshOperatorInbox(force: true)

        #expect(model.operatorMessages.map(\.id) == ["local:local-new", "dev:dev-old"])
        #expect(model.inboxHostFailures == ["dev": Self.sshHiccup])
        #expect(model.inboxHostFailureSummary == "1 host unreachable: dev")
        #expect(model.inboxError == nil)

        // Host B recovers with new content: its failure clears, nothing else
        // is disturbed.
        probe.setFailure(host: "dev", message: nil)
        probe.setMailbox(host: "dev", sent: [
            inboxSentRequest(id: "dev-new", createdAt: "2026-09-02T12:00:00Z"),
        ])

        await model.refreshOperatorInbox(force: true)

        #expect(model.operatorMessages.map(\.id) == ["dev:dev-new", "local:local-new"])
        #expect(model.inboxHostFailures.isEmpty)
    }

    @Test @MainActor
    func repeatedHostFailuresNeverEmptyThatHost() async throws {
        let probe = InboxMailboxProbe()
        probe.setMailbox(host: "local", sent: [
            inboxSentRequest(id: "local-1", createdAt: "2026-09-02T10:00:00Z"),
        ])
        probe.setMailbox(host: "dev", sent: [
            inboxSentRequest(id: "dev-1", createdAt: "2026-09-02T09:00:00Z"),
        ])
        let model = try await makeInboxModel(probe: probe)
        await model.refreshOperatorInbox(force: true)
        probe.setFailure(host: "dev", message: Self.sshHiccup)

        for _ in 0..<3 {
            await model.refreshOperatorInbox(force: true)
        }
        // A single-host refresh of the failing host alone behaves the same.
        await model.refreshOperatorInbox(force: true, hostAliases: ["dev"])

        #expect(model.operatorMessages.map(\.id) == ["local:local-1", "dev:dev-1"])
        #expect(model.inboxHostFailures == ["dev": Self.sshHiccup])
    }

    @Test @MainActor
    func singleHostRefreshLeavesOtherHostsFailuresAndMessagesAlone() async throws {
        let probe = InboxMailboxProbe()
        probe.setMailbox(host: "local", sent: [
            inboxSentRequest(id: "local-1", createdAt: "2026-09-02T10:00:00Z"),
        ])
        probe.setMailbox(host: "dev", sent: [
            inboxSentRequest(id: "dev-1", createdAt: "2026-09-02T09:00:00Z"),
        ])
        let model = try await makeInboxModel(probe: probe)
        await model.refreshOperatorInbox(force: true)
        probe.setFailure(host: "dev", message: Self.sshHiccup)
        await model.refreshOperatorInbox(force: true)
        probe.resetReadLog()

        await model.refreshOperatorInbox(force: true, hostAliases: ["local"])

        #expect(probe.mailboxReadHosts() == ["local"])
        #expect(model.inboxHostFailures == ["dev": Self.sshHiccup])
        #expect(model.operatorMessages.map(\.id) == ["local:local-1", "dev:dev-1"])
    }

    @Test @MainActor
    func markingAReplyReadUpdatesInPlaceAndRefreshesOnlyItsHost() async throws {
        let probe = InboxMailboxProbe()
        probe.setMailbox(host: "local", sent: [
            inboxSentRequest(
                id: "local-replied",
                createdAt: "2026-09-02T10:00:00Z",
                reply: ("completed", "Done", "2026-09-02T10:05:00Z")
            ),
        ])
        probe.setMailbox(host: "dev", sent: [
            inboxSentRequest(
                id: "dev-replied",
                createdAt: "2026-09-02T09:00:00Z",
                reply: ("completed", "Shipped", "2026-09-02T09:05:00Z")
            ),
        ])
        let model = try await makeInboxModel(probe: probe)
        await model.refreshOperatorInbox(force: true)
        let devMessage = try #require(model.operatorMessages.first { $0.host.alias == "dev" })
        #expect(devMessage.hasUnreadReply)
        probe.resetReadLog()

        await model.markOperatorMessageRead(devMessage)

        #expect(probe.getReadHosts() == ["dev"])
        #expect(probe.mailboxReadHosts() == ["dev"])
        let updated = try #require(model.operatorMessages.first { $0.id == devMessage.id })
        #expect(!updated.hasUnreadReply)
        #expect(model.operatorMessages.first { $0.host.alias == "local" }?.hasUnreadReply == true)
        #expect(model.inboxError == nil)
    }

    @Test
    func hostFailureSummaryIsOneCompactLine() {
        #expect(MuxaInboxHostFailureText.summary([:]) == nil)
        #expect(
            MuxaInboxHostFailureText.summary(["jiun-mbp": "ssh timed out"])
                == String(localized: "\(1) hosts unreachable: \("jiun-mbp")")
        )
        #expect(
            MuxaInboxHostFailureText.summary(["rtzr": "ssh timed out", "jiun-mbp": "remote request_failed"])
                == "2 hosts unreachable: jiun-mbp, rtzr"
        )
        #expect(
            MuxaInboxHostFailureText.details(["rtzr": "ssh timed out", "jiun-mbp": "remote request_failed"])
                == ["jiun-mbp: remote request_failed", "rtzr: ssh timed out"]
        )
    }

    @Test @MainActor
    func blockedRequestWithoutReplyIsADecisionNotAWait() async throws {
        let probe = InboxMailboxProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let execution = try await client.executionSnapshot()
        let route = try #require(
            execution.watchHosts.first(where: { $0.host.alias == "local" })?
                .sessions.first?.windows.first?.panes.first
        )
        func message(_ object: [String: Any]) throws -> MuxaOperatorMessage {
            let request = try decodeInboxRequest(object)
            return MuxaOperatorMessage(host: route.host, routePane: route.pane, request: request)
        }

        let waiting = try message(inboxSentRequest(id: "waiting", createdAt: "2026-09-02T10:00:00Z"))
        let blocked = try message(inboxSentRequest(id: "blocked", createdAt: "2026-09-02T09:00:00Z", status: "blocked"))
        let oldUnreadBlockedReply = try message(inboxSentRequest(
            id: "old-blocked-reply",
            createdAt: "2026-09-02T08:00:00Z",
            reply: ("blocked", "Need a decision", "2026-09-02T11:00:00Z")
        ))
        let readDeclinedReply = try message(inboxSentRequest(
            id: "read-declined-reply",
            createdAt: "2026-09-02T09:30:00Z",
            reply: ("declined", "Not doing that", "2026-09-02T12:00:00Z"),
            replyReadAt: "2026-09-02T12:01:00Z"
        ))

        #expect(waiting.isAwaitingAgentReply)
        #expect(!waiting.needsHumanDecision)
        #expect(blocked.needsReply, "the activity badge still counts it")
        #expect(!blocked.isAwaitingAgentReply)
        #expect(blocked.needsHumanDecision)

        // Needs Action: unread decisions first, then newest activity, so the
        // reply that arrived at 11:00 on the oldest request outranks the
        // read reply from 12:00 and the reply-less blocked request.
        let ordered = [blocked, readDeclinedReply, oldUnreadBlockedReply]
            .sorted(by: MuxaOperatorMessage.needsActionOrder)
        #expect(ordered.map(\.request.id) == ["old-blocked-reply", "read-declined-reply", "blocked"])
    }
}

// MARK: - Terminal capture formatter

import AppKit
import SwiftUI

private struct CaptureRun {
    let text: String
    let foreground: Color?
    let background: Color?
    let intent: InlinePresentationIntent?
    let underline: Text.LineStyle?
    let strikethrough: Text.LineStyle?
}

private func captureRuns(_ rendered: AttributedString) -> [CaptureRun] {
    rendered.runs.map { run in
        CaptureRun(
            text: String(rendered[run.range].characters),
            foreground: run[AttributeScopes.SwiftUIAttributes.ForegroundColorAttribute.self],
            background: run[AttributeScopes.SwiftUIAttributes.BackgroundColorAttribute.self],
            intent: run[AttributeScopes.FoundationAttributes.InlinePresentationIntentAttribute.self],
            underline: run[AttributeScopes.SwiftUIAttributes.UnderlineStyleAttribute.self],
            strikethrough: run[AttributeScopes.SwiftUIAttributes.StrikethroughStyleAttribute.self]
        )
    }
}

/// sRGB components scaled to 0–255, so palette colors can be compared by value.
private func srgb(_ color: Color?) -> [Int]? {
    guard let color, let resolved = NSColor(color).usingColorSpace(.sRGB) else { return nil }
    return [resolved.redComponent, resolved.greenComponent, resolved.blueComponent]
        .map { Int(($0 * 255).rounded()) }
}

private func alpha(_ color: Color?) -> Double? {
    guard let color, let resolved = NSColor(color).usingColorSpace(.sRGB) else { return nil }
    return Double(resolved.alphaComponent)
}

@Test func terminalCaptureFormatterPassesPlainTextThrough() {
    let formatter = TerminalCaptureFormatter(palette: .dark)
    let rendered = formatter.render(text: "hello\n\tworld $ ▸ 한글")
    #expect(String(rendered.characters) == "hello\n\tworld $ ▸ 한글")
    let runs = captureRuns(rendered)
    #expect(runs.count == 1)
    #expect(runs.first?.foreground == nil)
    #expect(runs.first?.background == nil)
    #expect(runs.first?.intent == nil)
    #expect(runs.first?.underline == nil)

    let bytes = formatter.render(bytes: Data("plain bytes".utf8))
    #expect(String(bytes.characters) == "plain bytes")
    #expect(captureRuns(bytes).count == 1)
}

@Test func terminalCaptureFormatterAppliesColorsAndReset() {
    let formatter = TerminalCaptureFormatter(palette: .dark)
    let rendered = formatter.render(
        text: "\u{1B}[31mred\u{1B}[0m plain\u{1B}[1;42mbold\u{1B}[22m\u{1B}[96mcyan\u{1B}[39;49mdefault"
    )
    #expect(String(rendered.characters) == "red plainboldcyandefault")
    let runs = captureRuns(rendered)
    #expect(runs.map(\.text) == ["red", " plain", "bold", "cyan", "default"])

    #expect(srgb(runs[0].foreground) == [0xAC, 0x41, 0x42])
    #expect(runs[0].background == nil)
    #expect(runs[0].intent == nil)

    #expect(runs[1].foreground == nil)
    #expect(runs[1].background == nil)

    #expect(runs[2].intent?.contains(.stronglyEmphasized) == true)
    #expect(srgb(runs[2].background) == [0x7E, 0x8E, 0x50])
    #expect(runs[2].foreground == nil)

    #expect(runs[3].intent == nil)
    #expect(srgb(runs[3].foreground) == [0x7D, 0xD5, 0xCF])
    #expect(srgb(runs[3].background) == [0x7E, 0x8E, 0x50])

    #expect(runs[4].foreground == nil)
    #expect(runs[4].background == nil)

    let light = captureRuns(TerminalCaptureFormatter(palette: .light).render(text: "\u{1B}[91mbright\u{1B}[m"))
    #expect(srgb(light[0].foreground) == [0xF0, 0x3E, 0x31])
}

@Test func terminalCaptureFormatterSupports256AndTruecolor() {
    let formatter = TerminalCaptureFormatter(palette: .dark)
    let rendered = formatter.render(
        text: "\u{1B}[38;5;196mA\u{1B}[48;5;244mB\u{1B}[38;2;10;20;30mC\u{1B}[38:2::40:50:60mD"
            + "\u{1B}[0m\u{1B}[38;5;4mE\u{1B}[0m\u{1B}[38;5mF\u{1B}[38;2;1;2mG\u{1B}[48:5:16mH"
    )
    #expect(String(rendered.characters) == "ABCDEFGH")
    let runs = captureRuns(rendered)
    #expect(runs.map(\.text) == ["A", "B", "C", "D", "E", "FG", "H"])

    #expect(srgb(runs[0].foreground) == [255, 0, 0])
    #expect(runs[0].background == nil)
    #expect(srgb(runs[1].foreground) == [255, 0, 0])
    #expect(srgb(runs[1].background) == [128, 128, 128])
    #expect(srgb(runs[2].foreground) == [10, 20, 30])
    #expect(srgb(runs[3].foreground) == [40, 50, 60])
    #expect(srgb(runs[4].foreground) == [0x6C, 0x99, 0xBB])
    // Malformed extended colors leave the style untouched.
    #expect(runs[5].foreground == nil)
    #expect(runs[5].background == nil)
    #expect(srgb(runs[6].background) == [0, 0, 0])
}

@Test func terminalCaptureFormatterDropsUnknownCSI() {
    let formatter = TerminalCaptureFormatter(palette: .dark)
    let rendered = formatter.render(
        text: "a\u{1B}[2Jb\u{1B}[?25lc\u{1B}[1;1Hd\u{1B}[0Ke\u{1B}[ qf\u{1B}[>4;2mg\u{1B}[31\u{1B}[32mh"
    )
    #expect(String(rendered.characters) == "abcdefgh")
    let runs = captureRuns(rendered)
    #expect(runs.map(\.text) == ["abcdefg", "h"])
    #expect(runs[0].foreground == nil)
    // An ESC inside a CSI aborts it and starts the next sequence.
    #expect(srgb(runs[1].foreground) == [0x7E, 0x8E, 0x50])
}

@Test func terminalCaptureFormatterDropsOSCAndOtherStrings() {
    let formatter = TerminalCaptureFormatter(palette: .dark)
    let rendered = formatter.render(
        text: "x\u{1B}]0;title\u{07}y\u{1B}]8;;https://example.com\u{1B}\\z\u{1B}Pq\u{1B}[31m\u{1B}\\w\u{1B}(B\u{1B}7v"
    )
    #expect(String(rendered.characters) == "xyzwv")
    #expect(captureRuns(rendered).count == 1)
}

@Test func terminalCaptureFormatterNormalizesCRLF() {
    let formatter = TerminalCaptureFormatter(palette: .dark)
    #expect(String(formatter.render(text: "one\r\ntwo\r\nthree\r").characters) == "one\ntwo\nthree")
    #expect(String(formatter.render(bytes: Data("a\r\n\u{1B}[1mb\r\n".utf8)).characters) == "a\nb\n")
    #expect(sanitizeTerminalCapture("one\r\ntwo") == "one\ntwo")
}

@Test func terminalCaptureFormatterHandlesReverseDimUnderlineAndItalic() {
    let formatter = TerminalCaptureFormatter(palette: .dark)
    let rendered = formatter.render(
        text: "\u{1B}[7mrev\u{1B}[27m\u{1B}[2mdim\u{1B}[22m\u{1B}[4mline\u{1B}[24m\u{1B}[3mit\u{1B}[9mstr"
            + "\u{1B}[0m\u{1B}[4:3mcurly\u{1B}[4:0moff\u{1B}[31;7mswap"
    )
    let runs = captureRuns(rendered)
    #expect(runs.map(\.text) == ["rev", "dim", "line", "it", "str", "curly", "off", "swap"])

    #expect(srgb(runs[0].foreground) == [0x21, 0x21, 0x21])
    #expect(srgb(runs[0].background) == [0xD0, 0xD0, 0xD0])

    #expect(srgb(runs[1].foreground) == [0xD0, 0xD0, 0xD0])
    #expect(alpha(runs[1].foreground).map { abs($0 - 0.6) < 0.01 } == true)
    #expect(runs[1].background == nil)

    #expect(runs[2].underline == .single)
    #expect(runs[2].foreground == nil)

    #expect(runs[3].intent == .emphasized)
    #expect(runs[3].strikethrough == nil)

    #expect(runs[4].intent == .emphasized)
    #expect(runs[4].strikethrough == .single)

    #expect(runs[5].underline == .single)
    #expect(runs[6].underline == nil)
    #expect(runs[6].foreground == nil)

    #expect(srgb(runs[7].foreground) == [0x21, 0x21, 0x21])
    #expect(srgb(runs[7].background) == [0xAC, 0x41, 0x42])
}

@Test func terminalCaptureFormatterSkipsLeadingPartialCharacter() {
    let formatter = TerminalCaptureFormatter(palette: .dark)
    var bytes = Data([0x80, 0xBF])
    bytes.append(contentsOf: "ok".utf8)
    #expect(String(formatter.render(bytes: bytes).characters) == "ok")
    #expect(TerminalCaptureFormatter.decode(Data([0x80])) == "")
}

@Test func sanitizeTerminalCaptureMatchesFormatterPlainText() {
    let formatter = TerminalCaptureFormatter(palette: .light)
    let samples = [
        "\u{1B}[31mred\u{1B}[0m\r\n\u{1B}]0;t\u{07}tab\there\u{07}\u{1B}(B\u{85}\u{7F}!",
        "no controls at all",
        "\u{1B}[38;2;1;2;3mtrue\u{1B}[m color\u{1B}[K\u{1B}[?1049h",
        "trailing escape\u{1B}",
        "unterminated \u{1B}]2;title",
    ]
    for sample in samples {
        #expect(sanitizeTerminalCapture(sample) == String(formatter.render(text: sample).characters))
    }
    #expect(sanitizeTerminalCapture(samples[0]) == "red\ntab\there!")
    #expect(sanitizeTerminalCapture(samples[2]) == "true color")
    #expect(sanitizeTerminalCapture(samples[3]) == "trailing escape")
    #expect(sanitizeTerminalCapture(samples[4]) == "unterminated ")
}

@Test func terminalCapturePaletteResolvesIndexedColors() {
    #expect(TerminalCapturePalette.dark.ansi.count == 16)
    #expect(TerminalCapturePalette.light.ansi.count == 16)
    #expect(srgb(TerminalCapturePalette.dark.color(for: .indexed(15))) == [0xF5, 0xF5, 0xF5])
    #expect(srgb(TerminalCapturePalette.dark.color(for: .indexed(16))) == [0, 0, 0])
    #expect(srgb(TerminalCapturePalette.dark.color(for: .indexed(231))) == [255, 255, 255])
    #expect(srgb(TerminalCapturePalette.dark.color(for: .indexed(232))) == [8, 8, 8])
    #expect(srgb(TerminalCapturePalette.dark.color(for: .indexed(255))) == [238, 238, 238])
    #expect(srgb(TerminalCapturePalette.dark.color(for: .indexed(21))) == [0, 0, 255])
    #expect(srgb(TerminalCapturePalette.light.color(for: .rgb(9, 8, 7))) == [9, 8, 7])
}

// MARK: - Explore tree highlight (workbench peer)

@Test
func exploreTreeHighlightsOnlyTheActiveEditorRow() {
    let paneA = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%1")
    let sessionA = MuxaWatchSessionIdentity(hostAlias: "local", socket: "default", sessionID: "$1")
    let sessionB = MuxaWatchSessionIdentity(hostAlias: "local", socket: "default", sessionID: "$2")
    // Session B is the active editor while pane A, in session A, is still the
    // followed pane. Only session B may look selected.
    let selection = WatchTreeSelection(editor: .fleetSession(sessionB), followedPane: paneA)

    #expect(selection.highlight(for: .fleetSession(sessionB), containsFollowedPane: false) == .selected)
    #expect(selection.highlight(for: .fleetSession(sessionA), containsFollowedPane: true) == .idle)
    #expect(selection.highlight(for: .pane(paneA), containsFollowedPane: true) == .idle)
    #expect(selection.highlight(for: .host("local"), containsFollowedPane: true) == .idle)
    #expect(!selection.showsFollowedPath)
}

@Test
func exploreTreeMarksFollowedPaneOnlyForPaneFollowingEditors() {
    let paneA = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%1")
    let paneB = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%2")
    let sessionA = MuxaWatchSessionIdentity(hostAlias: "local", socket: "default", sessionID: "$1")

    let liveWatch = WatchTreeSelection(editor: .watch, followedPane: paneA)
    #expect(liveWatch.showsFollowedPath)
    #expect(liveWatch.highlight(for: .pane(paneA), containsFollowedPane: true) == .followed)
    #expect(liveWatch.highlight(for: .fleetSession(sessionA), containsFollowedPane: true) == .followed)
    #expect(liveWatch.highlight(for: .host("remote"), containsFollowedPane: false) == .idle)

    let paneEditor = WatchTreeSelection(editor: .pane(paneA), followedPane: paneA)
    #expect(paneEditor.highlight(for: .pane(paneA), containsFollowedPane: true) == .selected)
    #expect(paneEditor.highlight(for: .fleetSession(sessionA), containsFollowedPane: true) == .followed)
    #expect(paneEditor.highlight(for: .pane(paneB), containsFollowedPane: false) == .idle)

    let otherPaneEditor = WatchTreeSelection(editor: .pane(paneB), followedPane: paneA)
    #expect(!otherPaneEditor.showsFollowedPath)
    #expect(otherPaneEditor.highlight(for: .pane(paneA), containsFollowedPane: true) == .idle)

    let workBoard = WatchTreeSelection(editor: .workBoard, followedPane: paneA)
    #expect(workBoard.highlight(for: .pane(paneA), containsFollowedPane: true) == .idle)
    #expect(workBoard.highlight(for: .fleetSession(sessionA), containsFollowedPane: true) == .idle)
}

// MARK: - Workbench tab restore (workbench peer)

@Test @MainActor
func workbenchRestoresPreviewTabAsPreview() throws {
    let suite = "dev.muxa.tests.workbench.\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    let key = "tabs"
    let pane = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%3")
    let tabs = MuxaWorkbenchTabs(persistenceKey: key, defaults: defaults)
    tabs.openPinned(.host("host-a"))
    tabs.openPreview(.pane(pane))

    let restored = MuxaWorkbenchTabs(persistenceKey: key, defaults: defaults)
    let group = try #require(restored.group(id: restored.focusedGroupID))

    #expect(group.tabs == [.workBoard, .host("host-a"), .pane(pane)])
    #expect(group.active == .pane(pane))
    #expect(group.preview == .pane(pane))
    #expect(restored.focusedSelection == .pane(pane))
}

@Test @MainActor
func workbenchLaunchReactivationKeepsRestoredPreview() throws {
    // Mirrors ContentView at launch: `.task` re-activates the focused editor,
    // then a refresh may reconcile the selection back to the Work board, which
    // `onChange(of: sidebarSelection)` opens as a preview.
    let suite = "dev.muxa.tests.workbench.\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    let key = "tabs"
    let pane = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%3")
    MuxaWorkbenchTabs(persistenceKey: key, defaults: defaults).openPreview(.pane(pane))

    let tabs = MuxaWorkbenchTabs(persistenceKey: key, defaults: defaults)
    let model = AppModel()
    model.activateEditor(tabs.focusedSelection)
    #expect(model.sidebarSelection == tabs.focusedSelection)

    model.select(.workBoard)
    if let selection = model.sidebarSelection, tabs.focusedSelection != selection {
        tabs.openPreview(selection)
    }
    let group = try #require(tabs.group(id: tabs.focusedGroupID))

    #expect(group.active == .workBoard)
    #expect(group.tabs == [.workBoard, .pane(pane)])
    #expect(group.preview == .pane(pane))
}

// MARK: - Onboarding (peer: onboarding)

@Test func onboardingShowsForFirstLaunchAndOlderRecordedVersions() {
    #expect(OnboardingPreferences.shouldPresent(currentVersion: "0.2.0", completedVersion: nil))
    #expect(OnboardingPreferences.shouldPresent(currentVersion: "0.2.0", completedVersion: ""))
    #expect(OnboardingPreferences.shouldPresent(currentVersion: "0.2.0", completedVersion: "0.1.0"))
    #expect(OnboardingPreferences.shouldPresent(currentVersion: "0.1.10", completedVersion: "0.1.9"))
    #expect(OnboardingPreferences.shouldPresent(currentVersion: "1.0", completedVersion: "0.9.9"))
}

@Test func onboardingStaysQuietForEqualOrNewerRecordedVersions() {
    #expect(!OnboardingPreferences.shouldPresent(currentVersion: "0.2.0", completedVersion: "0.2.0"))
    #expect(!OnboardingPreferences.shouldPresent(currentVersion: "0.2.0", completedVersion: "0.2"))
    #expect(!OnboardingPreferences.shouldPresent(currentVersion: "0.2", completedVersion: "0.2.0"))
    #expect(!OnboardingPreferences.shouldPresent(currentVersion: "0.1.9", completedVersion: "0.1.10"))
    #expect(!OnboardingPreferences.shouldPresent(currentVersion: "0.2.0", completedVersion: "0.3.0-beta"))
    #expect(OnboardingPreferences.compareVersions("0.1.0-beta", "0.1.0") == .orderedSame)
}

@Test func onboardingNeverPresentsInsideTestHosts() {
    #expect(OnboardingPreferences.isRunningTests(
        environment: ["XCTestConfigurationFilePath": "/tmp/Muxa.xctestconfiguration"]
    ))
    #expect(OnboardingPreferences.isRunningTests(environment: ["SWIFT_TESTING_ENABLED": "1"]))
    #expect(!OnboardingPreferences.isRunningTests(environment: ["PATH": "/usr/bin"]))
    #expect(OnboardingPreferences.isRunningTests())
    #expect(!OnboardingPreferences.shouldPresentOnLaunch(currentVersion: "9.9.9"))
}

@Test func onboardingRecordsTheCompletedVersionInDefaults() throws {
    let suite = "dev.muxa.tests.onboarding.\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    let environment = ["PATH": "/usr/bin"]

    #expect(OnboardingPreferences.shouldPresentOnLaunch(
        defaults: defaults, currentVersion: "0.2.0", environment: environment
    ))
    OnboardingPreferences.markCompleted(version: "0.2.0", defaults: defaults)
    #expect(defaults.string(forKey: OnboardingPreferences.completedVersionKey) == "0.2.0")
    #expect(!OnboardingPreferences.shouldPresentOnLaunch(
        defaults: defaults, currentVersion: "0.2.0", environment: environment
    ))
    #expect(OnboardingPreferences.shouldPresentOnLaunch(
        defaults: defaults, currentVersion: "0.3.0", environment: environment
    ))
}

@Test func onboardingChecklistMapsDetectedTools() {
    let detected = [
        InstalledTool(name: "tmux", path: "/opt/homebrew/bin/tmux", version: "tmux 3.5a"),
        InstalledTool(name: "claude", path: "/Users/me/.local/bin/claude", version: "2.1.3 (Claude Code)"),
        InstalledTool(name: "codex", path: "/opt/homebrew/bin/codex", version: nil),
    ]

    #expect(OnboardingChecklist.toolStatus(named: "tmux", in: nil) == .unknown)
    #expect(OnboardingChecklist.agentsStatus(in: nil) == .unknown)
    #expect(OnboardingChecklist.toolStatus(named: "tmux", in: detected) == .ready)
    #expect(OnboardingChecklist.toolStatus(named: "tmux", in: []) == .attention)
    #expect(OnboardingChecklist.agentTools(in: detected).map(\.name) == ["claude", "codex"])
    #expect(OnboardingChecklist.agentsStatus(in: detected) == .ready)
    #expect(OnboardingChecklist.agentsStatus(in: [detected[0]]) == .attention)
    #expect(OnboardingChecklist.probedPrograms == ["tmux", "claude", "codex", "gemini", "agy", "opencode"])
}

@Test @MainActor
func onboardingChecklistMapsModelState() {
    #expect(OnboardingChecklist.connectionStatus(.connected) == .ready)
    #expect(OnboardingChecklist.connectionStatus(.connecting) == .unknown)
    #expect(OnboardingChecklist.connectionStatus(.failed("socket missing")) == .attention)
    #expect(OnboardingChecklist.connectionStatus(.upgradeRequired("0.2 needed")) == .attention)

    #expect(OnboardingChecklist.askStatus(true) == .ready)
    #expect(OnboardingChecklist.askStatus(false) == .attention)
    #expect(OnboardingChecklist.askStatus(nil) == .unknown)

    #expect(OnboardingChecklist.workFolderStatus(path: "") { _ in true } == .attention)
    #expect(OnboardingChecklist.workFolderStatus(path: "  ") { _ in true } == .attention)
    #expect(OnboardingChecklist.workFolderStatus(path: "/tmp/project") { $0 == "/tmp/project" } == .ready)
    #expect(OnboardingChecklist.workFolderStatus(path: "/gone") { _ in false } == .attention)

    #expect(OnboardingChecklist.fleetHostsStatus(remoteHostCount: 0) == .attention)
    #expect(OnboardingChecklist.fleetHostsStatus(remoteHostCount: 2) == .ready)
}

@Test func installedToolsVersionLineSkipsBlankAndIndentedLines() {
    #expect(InstalledTools.versionLine(from: "\r\n\t codex-cli 0.42.0\r\nnode 22.1.0\n") == "codex-cli 0.42.0")
    #expect(InstalledTools.versionLine(from: "gemini 0.9.1") == "gemini 0.9.1")
    #expect(InstalledTools.versionLine(from: "") == nil)
}

@Test func installedToolsMergedDirectoriesKeepsFallbackOrderWhenPathIsEmpty() {
    let merged = InstalledTools.mergedDirectories(
        pathStrings: ["", "  "],
        fallback: ["/opt/homebrew/bin", "/usr/local/bin", "/opt/homebrew/bin"]
    )
    #expect(merged == ["/opt/homebrew/bin", "/usr/local/bin"])
    #expect(InstalledTools.mergedDirectories(pathStrings: [], fallback: []).isEmpty)
}

// MARK: - Ask providers (ask-settings peer)

/// The `ask_providers` wire shape from the ask-rust contract, in daemon order.
private func askProviderFixture(anthropicModel: String = "claude-sonnet-5") -> [[String: Any]] {
    [
        [
            "id": "claude", "title": "Claude Code", "kind": "cli", "executable": "claude",
            "credential_env": "ANTHROPIC_API_KEY", "credential_required": false,
            "model": NSNull(), "selected": true,
        ],
        [
            "id": "codex", "title": "Codex", "kind": "cli", "executable": "codex",
            "credential_env": "CODEX_API_KEY", "credential_required": false,
            "model": NSNull(), "selected": false,
        ],
        [
            "id": "anthropic", "title": "Anthropic API", "kind": "api", "executable": NSNull(),
            "credential_env": "ANTHROPIC_API_KEY", "credential_required": true,
            "model": anthropicModel, "selected": false,
        ],
        [
            "id": "openai", "title": "OpenAI API", "kind": "api", "executable": NSNull(),
            "credential_env": "OPENAI_API_KEY", "credential_required": true,
            "model": "gpt-5", "selected": false,
        ],
    ]
}

/// `hello` with the baseline the client requires plus the given extras.
private func askProviderHello(capabilities: [String]) throws -> Data {
    try JSONSerialization.data(withJSONObject: [
        "ok": true,
        "min_protocol": 1,
        "max_protocol": 6,
        "capabilities": ["session_bytes_v1", "session_attachment_identity_v1", "work_control_v1"] + capabilities,
    ])
}

@Test
func askProvidersDecodeTheDaemonListAndConfigureRoundTrips() async throws {
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        switch object["kind"] as? String {
        case "hello":
            return try askProviderHello(capabilities: ["ask_providers_v1", "ask_conversations_v1"])
        case "ask_providers":
            return try JSONSerialization.data(withJSONObject: [
                "ok": true,
                "ask_providers": askProviderFixture(),
            ])
        case "ask_provider_configure":
            #expect(object["provider"] as? String == "anthropic")
            #expect(object["model"] as? String == "claude-opus-5")
            // `.keep` leaves the key out so the daemon does not touch it.
            #expect(object["api_key_env"] == nil)
            return try JSONSerialization.data(withJSONObject: [
                "ok": true,
                "ask_providers": askProviderFixture(anthropicModel: "claude-opus-5"),
            ])
        default:
            return try JSONSerialization.data(withJSONObject: ["ok": true])
        }
    }
    let client = MuxaIPCClient(socketPath: "/tmp/muxa-ask-providers-test.sock", request: handler)
    try await client.hello()
    #expect(await client.supports(MuxaIPCClient.askProvidersCapability))

    let providers = try await client.listAskProviders()
    #expect(providers.map(\.id) == ["claude", "codex", "anthropic", "openai"])

    let claude = try #require(providers.first { $0.id == "claude" })
    #expect(claude.kind == .cli)
    #expect(claude.cliExecutable == "claude")
    #expect(claude.executable == "claude")
    #expect(!claude.credentialRequired)
    #expect(claude.model == nil)
    #expect(claude.selected)
    // Equality is by id, so the daemon's row matches the static provider
    // AppModel compares against (`provider == .codex`) whatever its state.
    #expect(claude == .claude)
    #expect(claude != .codex)

    let anthropic = try #require(providers.first { $0.id == "anthropic" })
    #expect(anthropic.kind == .api)
    #expect(!anthropic.isCLI)
    #expect(anthropic.cliExecutable == nil)
    #expect(anthropic.credentialEnv == "ANTHROPIC_API_KEY")
    #expect(anthropic.environmentKey == "ANTHROPIC_API_KEY")
    #expect(anthropic.credentialRequired)
    #expect(anthropic.model == "claude-sonnet-5")
    #expect(!anthropic.selected)

    let configured = try await client.configureAskProvider("anthropic", model: .set("claude-opus-5"))
    #expect(configured.first { $0.id == "anthropic" }?.model == "claude-opus-5")
}

@Test
func askProviderConfigureSendsNullToClearAndRequiresTheCapability() async throws {
    let clearing: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        switch object["kind"] as? String {
        case "hello":
            return try askProviderHello(capabilities: ["ask_providers_v1"])
        case "ask_provider_configure":
            #expect(object["provider"] as? String == "openai")
            #expect(object["model"] is NSNull)
            #expect(object["api_key_env"] as? String == "OPENAI_KEY_ALT")
            return try JSONSerialization.data(withJSONObject: [
                "ok": true,
                "ask_providers": askProviderFixture(),
            ])
        default:
            return try JSONSerialization.data(withJSONObject: ["ok": true])
        }
    }
    let client = MuxaIPCClient(socketPath: "/tmp/muxa-ask-providers-clear.sock", request: clearing)
    try await client.hello()
    let updated = try await client.configureAskProvider(
        "openai",
        model: .clear,
        apiKeyEnv: .set("OPENAI_KEY_ALT")
    )
    #expect(updated.count == 4)

    // Blank input clears; surrounding whitespace is trimmed.
    #expect(MuxaAskProviderFieldUpdate(nil) == .clear)
    #expect(MuxaAskProviderFieldUpdate("   ") == .clear)
    #expect(MuxaAskProviderFieldUpdate(" gpt-5 ") == .set("gpt-5"))

    let legacy: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        if object["kind"] as? String == "hello" {
            return try askProviderHello(capabilities: ["ask_conversations_v1"])
        }
        Issue.record("an old daemon must not receive provider requests")
        return try JSONSerialization.data(withJSONObject: ["ok": false, "error": "unknown kind"])
    }
    let old = MuxaIPCClient(socketPath: "/tmp/muxa-ask-providers-legacy.sock", request: legacy)
    try await old.hello()
    await #expect(throws: (any Error).self) {
        _ = try await old.listAskProviders()
    }
    await #expect(throws: (any Error).self) {
        _ = try await old.configureAskProvider("anthropic", model: .set("x"))
    }
}

@Test
func askProviderUsabilityRuleTable() {
    let tool = InstalledTool(name: "claude", path: "/opt/homebrew/bin/claude", version: "2.1.0")
    // CLI providers: the executable decides; a key alone is not enough.
    #expect(AskProviderStore.usability(kind: .cli, detection: .installed(tool), hasKey: false) == .usable)
    #expect(AskProviderStore.usability(kind: .cli, detection: .installed(tool), hasKey: true) == .usable)
    #expect(AskProviderStore.usability(kind: .cli, detection: .notInstalled, hasKey: true) == .notInstalled)
    #expect(AskProviderStore.usability(kind: .cli, detection: .notInstalled, hasKey: false) == .notInstalled)
    #expect(AskProviderStore.usability(kind: .cli, detection: .probing, hasKey: false) == .probing)
    // API providers: the key decides; detection is irrelevant.
    #expect(AskProviderStore.usability(kind: .api, detection: .notInstalled, hasKey: true) == .usable)
    #expect(AskProviderStore.usability(kind: .api, detection: .probing, hasKey: true) == .usable)
    #expect(AskProviderStore.usability(kind: .api, detection: .installed(tool), hasKey: false) == .missingKey)
    // Probing counts as usable so the picker never blocks on a slow probe.
    #expect(AskProviderUsability.usable.isUsable)
    #expect(AskProviderUsability.probing.isUsable)
    #expect(!AskProviderUsability.notInstalled.isUsable)
    #expect(!AskProviderUsability.missingKey.isUsable)
    #expect(AskProviderUsability.usable.reason == nil)
    #expect(AskProviderUsability.probing.reason == nil)
    #expect(AskProviderUsability.notInstalled.reason?.isEmpty == false)
    #expect(AskProviderUsability.missingKey.reason?.isEmpty == false)
    #expect(AskProviderDetection.installed(tool).tool == tool)
    #expect(AskProviderDetection.notInstalled.tool == nil)
}

@Test
func askProviderKeychainAccountsMatchEarlierBuilds() throws {
    // The enum this struct replaced used "<rawValue>-api-key" under this
    // service; keys saved by earlier builds must keep resolving.
    #expect(MuxaProviderCredentialStore.service == "dev.muxa.mac.ask-provider")
    #expect(MuxaAskProvider.claude.keychainAccount == "claude-api-key")
    #expect(MuxaAskProvider.codex.keychainAccount == "codex-api-key")
    #expect(MuxaAskProvider.claude.environmentKey == "ANTHROPIC_API_KEY")
    #expect(MuxaAskProvider.codex.environmentKey == "CODEX_API_KEY")
    #expect(MuxaAskProvider.claude.rawValue == "claude")
    #expect(MuxaAskProvider(rawValue: "claude")?.keychainAccount == "claude-api-key")
    #expect(MuxaAskProvider(rawValue: "codex")?.keychainAccount == "codex-api-key")
    #expect(MuxaAskProvider(rawValue: "codex") == .codex)
    #expect(MuxaAskProvider(rawValue: "codex")?.title == "Codex")
    #expect(MuxaAskProvider(rawValue: "") == nil)
    #expect(MuxaAskProvider(rawValue: "anthropic")?.kind == .api)
    #expect(MuxaAskProvider(rawValue: "openai")?.keychainAccount == "openai-api-key")

    // An id this build never saw still maps to a stable account and env var.
    let gemini = try #require(MuxaAskProvider(rawValue: "gemini"))
    #expect(gemini.keychainAccount == "gemini-api-key")
    #expect(gemini.environmentKey == "GEMINI_API_KEY")
    #expect(gemini.title == "Gemini")
    #expect(gemini.kind == .cli)
    #expect(gemini.executable == "gemini")
    #expect(MuxaAskProvider.defaultEnvironmentKey(for: "my-provider") == "MY_PROVIDER_API_KEY")

    // Only `id` is required on the wire; the rest has safe defaults.
    let minimal = try JSONDecoder().decode(
        MuxaAskProvider.self,
        from: Data(#"{"id":"gemini","kind":"cli","executable":"gemini"}"#.utf8)
    )
    #expect(minimal.title == "Gemini")
    #expect(minimal.keychainAccount == "gemini-api-key")
    #expect(!minimal.credentialRequired)
    #expect(!minimal.selected)
    let apiOnly = try JSONDecoder().decode(MuxaAskProvider.self, from: Data(#"{"id":"mistral"}"#.utf8))
    #expect(apiOnly.kind == .api)
    #expect(apiOnly.credentialRequired)
    #expect(apiOnly.environmentKey == "MISTRAL_API_KEY")

    // Environment injection still keys off the provider's env var and never
    // overrides a value the daemon already has.
    let env = MuxaProviderCredentialStore.environment(["ANTHROPIC_API_KEY": "from-env"], for: .claude)
    #expect(env["ANTHROPIC_API_KEY"] == "from-env")
    #expect(env["PATH"]?.isEmpty == false)
}

@Test
func askProviderFallbackListMirrorsTheSelectedAgent() {
    let fallback = AskProviderStore.fallbackProviders(selected: "codex")
    #expect(fallback.map(\.id) == ["claude", "codex"])
    #expect(fallback.map(\.selected) == [false, true])
    let allCLI = fallback.allSatisfy { $0.isCLI }
    #expect(allCLI)
    #expect(fallback == MuxaAskProvider.builtIn)
    let unknownSelectsNothing = AskProviderStore.fallbackProviders(selected: "gemini").allSatisfy { !$0.selected }
    #expect(unknownSelectsNothing)
}

@Test
func askSettingsTabKeySelectsTheProvidersTab() throws {
    let suite = "dev.muxa.tests.ask-settings.\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    #expect(MuxaPreferences.settingsTabKey == "muxa.settings.selectedTab")
    #expect(MuxaSettingsTab.providers.rawValue == "providers")

    MuxaSettingsOpener.select(.providers, in: defaults)
    #expect(defaults.string(forKey: MuxaPreferences.settingsTabKey) == "providers")
    #expect(MuxaSettingsTab(rawValue: defaults.string(forKey: MuxaPreferences.settingsTabKey) ?? "") == .providers)

    MuxaSettingsOpener.select(.general, in: defaults)
    #expect(defaults.string(forKey: MuxaPreferences.settingsTabKey) == "general")
}

// MARK: - Pipeline composer (ask-pipeline peer)

private func composeFixturePipelineJSON() -> String {
    """
    {"name":"implement-review","description":"Implementer then reviewer","layout":"main-vertical","prompt":null,"agents":[{"alias":"impl","program":"claude","role":"implementer","task":null,"prompt":"Implement the request.","direction":null,"after":[]},{"alias":"review","program":"codex","role":"reviewer","task":"review","prompt":"Read only; report findings.","direction":"down","after":["impl"]}]}
    """
}

/// What `muxa work compose --json` prints: the bare result object.
private func composeFixtureBare() -> Data {
    Data("""
    {"pipeline":\(composeFixturePipelineJSON()),"notes":"Reviewer is read-only.","raw":"```json\\n{}\\n```\\nReviewer is read-only."}
    """.utf8)
}

/// What muxad replies to `work_compose`: the same object under `work_compose`.
private func composeFixtureReply() -> Data {
    Data("""
    {"ok":true,"work_compose":\(String(decoding: composeFixtureBare(), as: UTF8.self))}
    """.utf8)
}

private final class ComposeRequestLog: @unchecked Sendable {
    private let lock = NSLock()
    private var requests: [PipelineComposerSession.Request] = []
    private var payloads: [[String: Any]] = []

    func append(_ request: PipelineComposerSession.Request) {
        lock.lock()
        requests.append(request)
        lock.unlock()
    }

    func append(payload: [String: Any]) {
        lock.lock()
        payloads.append(payload)
        lock.unlock()
    }

    var all: [PipelineComposerSession.Request] {
        lock.lock()
        defer { lock.unlock() }
        return requests
    }

    var lastPayload: [String: Any]? {
        lock.lock()
        defer { lock.unlock() }
        return payloads.last
    }
}

private final class ComposeMode: @unchecked Sendable {
    private let lock = NSLock()
    private var failure: String?
    private var hangs = false

    func fail(with message: String?) {
        lock.lock()
        failure = message
        lock.unlock()
    }

    func hang(_ value: Bool) {
        lock.lock()
        hangs = value
        lock.unlock()
    }

    var currentFailure: String? {
        lock.lock()
        defer { lock.unlock() }
        return failure
    }

    var shouldHang: Bool {
        lock.lock()
        defer { lock.unlock() }
        return hangs
    }
}

@Test
func workComposeDecodesDaemonReplyIntoPipelineDefinition() async throws {
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        #expect(object["kind"] as? String == "work_compose")
        return composeFixtureReply()
    }
    let client = MuxaWorkComposeClient(socketPath: "/tmp/muxa-compose-test.sock", request: handler)

    let result = try await client.composeWork(
        description: "implementer in claude, reviewer in codex after it",
        agent: nil,
        current: nil,
        name: nil,
        credential: nil
    )

    #expect(result.pipeline.name == "implement-review")
    #expect(result.notes == "Reviewer is read-only.")
    #expect(result.raw.contains("```json"))
    let definition = result.definition
    #expect(definition.description == "Implementer then reviewer")
    #expect(definition.layout == "main-vertical")
    #expect(definition.agents.map(\.alias) == ["impl", "review"])
    #expect(definition.agents.map(\.program) == ["claude", "codex"])
    #expect(definition.agents[1].after == ["impl"])
    #expect(definition.agents[1].direction == "down")
    #expect(definition.agents[0].prompt == "Implement the request.")
    #expect(definition.problems().isEmpty)
    #expect(MuxaPipelineStages.stages(for: definition.optionsAgents).map { $0.map(\.alias) } == [["impl"], ["review"]])
}

@Test
func workComposeDecodesBareCLIOutputAndDaemonEnvelopeAlike() throws {
    let fromCLI = try MuxaWorkComposeResult.decode(composeFixtureBare())
    let fromDaemon = try MuxaWorkComposeResult.decode(composeFixtureReply())
    #expect(fromCLI == fromDaemon)
    #expect(fromCLI.pipeline.agents.count == 2)

    // notes/raw are optional on the wire; the pipeline is not.
    let minimal = try MuxaWorkComposeResult.decode(Data("{\"pipeline\":\(composeFixturePipelineJSON())}".utf8))
    #expect(minimal.notes.isEmpty)
    #expect(minimal.raw.isEmpty)
    #expect(throws: (any Error).self) {
        try MuxaWorkComposeResult.decode(Data("{\"notes\":\"no pipeline here\"}".utf8))
    }
    #expect(throws: (any Error).self) {
        try MuxaWorkComposeResult.decode(Data("{\"ok\":false,\"error\":\"ask is disabled\"}".utf8))
    }
}

@Test
func workComposeRefineRequestCarriesCurrentDraftNameAndCredential() async throws {
    let log = ComposeRequestLog()
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        log.append(payload: object)
        return composeFixtureReply()
    }
    let client = MuxaWorkComposeClient(socketPath: "/tmp/muxa-compose-test.sock", request: handler)
    let current = MuxaPipelineDefinition(
        description: "Implementer then reviewer",
        agents: [
            MuxaPipelineDefinition.Agent(alias: "impl", program: "claude", role: "implementer"),
            MuxaPipelineDefinition.Agent(alias: "review", program: "codex", role: "reviewer", after: ["impl"]),
        ]
    )

    _ = try await client.composeWork(
        description: "make the reviewer use gemini",
        agent: "codex",
        current: current,
        name: "review-loop",
        credential: MuxaWorkComposeCredential(agent: "codex", apiKey: "test-only-secret")
    )

    let object = try #require(log.lastPayload)
    #expect(object["protocol"] as? UInt32 == MuxaIPCClient.protocolVersion)
    #expect(object["kind"] as? String == "work_compose")
    #expect(object["description"] as? String == "make the reviewer use gemini")
    #expect(object["agent"] as? String == "codex")
    let sent = try #require(object["current"] as? [String: Any])
    #expect(sent["name"] as? String == "review-loop")
    #expect(sent["description"] as? String == "Implementer then reviewer")
    let agents = try #require(sent["agents"] as? [[String: Any]])
    #expect(agents.map { $0["alias"] as? String } == ["impl", "review"])
    #expect(agents.map { $0["program"] as? String } == ["claude", "codex"])
    #expect(agents[1]["after"] as? [String] == ["impl"])
    #expect(agents[0]["after"] as? [String] == [])
    let credential = try #require(object["credential"] as? [String: String])
    #expect(credential == ["agent": "codex", "api_key": "test-only-secret"])
}

@Test
func workComposeFirstDraftSendsNullForAbsentFields() throws {
    let object = try MuxaWorkComposeClient.requestObject(
        description: "  solo claude that runs the tests  ",
        agent: nil,
        current: nil,
        name: nil,
        credential: MuxaWorkComposeCredential(agent: "claude", apiKey: "")
    )
    #expect(object["description"] as? String == "solo claude that runs the tests")
    #expect(object["agent"] is NSNull)
    #expect(object["current"] is NSNull)
    // An empty Keychain entry is not a credential.
    #expect(object["credential"] is NSNull)

    let payload = try MuxaWorkComposeClient.currentPayload(
        MuxaPipelineDefinition(agents: [MuxaPipelineDefinition.Agent(alias: "impl")]),
        name: "   "
    )
    #expect(payload["name"] as? String == "draft")
    #expect((payload["agents"] as? [[String: Any]])?.count == 1)
}

@Test
func workComposeThroughTheClientRequiresTheDaemonCapability() async throws {
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        switch object["kind"] as? String {
        case "hello":
            return try JSONSerialization.data(withJSONObject: [
                "ok": true,
                "min_protocol": 1,
                "max_protocol": 6,
                "capabilities": [
                    "session_bytes_v1",
                    "session_attachment_identity_v1",
                    "work_control_v1",
                ],
            ])
        default:
            return composeFixtureReply()
        }
    }
    let client = MuxaIPCClient(socketPath: "/tmp/muxa-compose-test.sock", request: handler)
    try await client.hello()
    #expect(await !client.supports(MuxaIPCClient.workComposeCapability))
    #expect(MuxaIPCClient.workComposeCapability == "work_compose_v1")

    await #expect(throws: MuxaIPCError.self) {
        _ = try await client.composeWork(
            description: "anything",
            agent: nil,
            current: nil,
            name: nil,
            credential: nil
        )
    }
    // No provider list on an older daemon: callers keep the built-in pair.
    let providers = try await client.workComposeProviders()
    #expect(providers == nil)
}

@Test
func workComposeProviderListDecodesJustWhatThePickerNeeds() async throws {
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        #expect(object["kind"] as? String == "ask_providers")
        return Data("""
        {"ok":true,"ask_providers":[
          {"id":"claude","title":"Claude Code","kind":"cli","executable":"claude","credential_env":"ANTHROPIC_API_KEY","credential_required":false,"model":null,"selected":true},
          {"id":"anthropic","title":"Anthropic API","kind":"api","executable":null,"credential_env":"ANTHROPIC_API_KEY","credential_required":true,"model":"claude-sonnet-5","selected":false},
          {"id":"future"}
        ]}
        """.utf8)
    }
    let client = MuxaWorkComposeClient(socketPath: "/tmp/muxa-compose-test.sock", request: handler)
    let providers = try await client.providers()
    #expect(providers.map(\.id) == ["claude", "anthropic", "future"])
    #expect(providers.map(\.title) == ["Claude Code", "Anthropic API", "future"])
    #expect(providers.map(\.selected) == [true, false, false])
}

@Test @MainActor
func pipelineComposerSessionMovesThroughPhases() async throws {
    let log = ComposeRequestLog()
    let fixture = try MuxaWorkComposeResult.decode(composeFixtureReply())
    let session = PipelineComposerSession(
        host: nil,
        defaultProvider: "codex",
        backend: .daemon,
        composer: { request in
            log.append(request)
            return fixture
        }
    )
    #expect(session.phase == .idle)
    #expect(!session.hasDraft)
    #expect(!session.canDraft)

    session.description = "implementer in claude, reviewer in codex after it"
    #expect(session.canDraft)
    session.draftPipeline()
    #expect(session.phase == .drafting)
    #expect(session.isDrafting)
    #expect(!session.canDraft)
    await session.awaitPendingRequest()

    #expect(session.phase == .ready)
    #expect(session.name == "implement-review")
    #expect(session.draft?.agents.map(\.alias) == ["impl", "review"])
    #expect(session.notes == "Reviewer is read-only.")
    #expect(session.history.isEmpty)
    #expect(session.canSave)
    #expect(session.draftAsPipeline?.name == "implement-review")
    #expect(session.draftAsPipeline?.agents.count == 2)
    let first = try #require(log.all.first)
    #expect(first.agent == "codex")
    #expect(first.current == nil)
    #expect(first.name == nil)

    // A refinement sends the draft along under the operator's chosen name.
    session.name = "review-loop"
    session.refinement = "make the reviewer use gemini"
    #expect(session.canRefine)
    session.refine()
    #expect(session.phase == .drafting)
    await session.awaitPendingRequest()

    #expect(session.phase == .ready)
    let refine = try #require(log.all.last)
    #expect(log.all.count == 2)
    #expect(refine.description == "make the reviewer use gemini")
    #expect(refine.current?.agents.map(\.alias) == ["impl", "review"])
    #expect(refine.name == "review-loop")
    #expect(session.name == "review-loop")
    #expect(session.history.map(\.request) == ["make the reviewer use gemini"])
    #expect(session.history.first?.notes == "Reviewer is read-only.")
    #expect(session.refinement.isEmpty)
    #expect(!session.canRefine)
}

@Test @MainActor
func pipelineComposerSessionKeepsTheDraftOnErrorAndCancel() async throws {
    let mode = ComposeMode()
    let fixture = try MuxaWorkComposeResult.decode(composeFixtureReply())
    let session = PipelineComposerSession(
        host: nil,
        defaultProvider: "claude",
        backend: .daemon,
        composer: { _ in
            if mode.shouldHang { try await Task.sleep(for: .seconds(30)) }
            if let failure = mode.currentFailure { throw MuxaIPCError.server(failure) }
            return fixture
        }
    )
    session.description = "solo claude"
    session.draftPipeline()
    await session.awaitPendingRequest()
    #expect(session.phase == .ready)

    mode.fail(with: "ask is disabled")
    session.refinement = "add a tester"
    session.refine()
    await session.awaitPendingRequest()
    #expect(session.phase == .error("ask is disabled"))
    #expect(session.errorMessage == "ask is disabled")
    #expect(session.draft?.agents.count == 2)
    #expect(session.history.isEmpty)
    #expect(session.refinement == "add a tester")
    #expect(session.canRefine)

    mode.fail(with: nil)
    mode.hang(true)
    session.refine()
    #expect(session.phase == .drafting)
    session.cancel()
    #expect(session.phase == .ready)
    #expect(session.draft?.agents.count == 2)
    #expect(session.errorMessage == nil)

    // Cancelling before any draft exists goes back to idle.
    let fresh = PipelineComposerSession(
        host: nil,
        defaultProvider: "claude",
        backend: .daemon,
        composer: { _ in
            try await Task.sleep(for: .seconds(30))
            return fixture
        }
    )
    fresh.description = "anything"
    fresh.draftPipeline()
    fresh.cancel()
    #expect(fresh.phase == .idle)
}

@Test @MainActor
func pipelineComposerSessionValidatesNameAndOffersTheDefaultProvider() async throws {
    let fixture = try MuxaWorkComposeResult.decode(composeFixtureReply())
    let session = PipelineComposerSession(
        host: "build-box",
        defaultProvider: "gemini",
        backend: .daemon,
        composer: { _ in fixture }
    )
    #expect(session.host == "build-box")
    // The configured Ask agent is offered even when the built-in list lacks it.
    #expect(session.providers.map(\.id) == ["claude", "codex", "gemini"])
    #expect(session.providerID == "gemini")
    #expect(session.providerTitle == "gemini")
    session.setProviders([
        PipelineComposerSession.Provider(id: "claude", title: "Claude Code"),
        PipelineComposerSession.Provider(id: "anthropic", title: "Anthropic API"),
    ])
    #expect(session.providers.map(\.id) == ["claude", "anthropic", "gemini"])

    session.description = "implementer and reviewer"
    session.draftPipeline()
    await session.awaitPendingRequest()
    #expect(session.problems.isEmpty)

    session.name = "bad name!"
    #expect(session.problems.first == "Name may only use letters, digits, - and _.")
    #expect(!session.canSave)
    #expect(!MuxaPipelineDefinition.isValidName("bad name!"))
    #expect(MuxaPipelineDefinition.isValidName("implement-review_2"))
    session.name = "ok"
    #expect(session.canSave)
    #expect(session.draftAsPipeline?.name == "ok")

    // A draft that the CLI would refuse is reported, not hidden.
    session.draft?.agents[1].after = ["ghost"]
    #expect(session.problems == ["@review waits for unknown alias \"ghost\"."])
    #expect(!session.canSave)

    // Without any backend the description alone cannot start a draft.
    let blocked = PipelineComposerSession(
        host: nil,
        defaultProvider: "claude",
        backend: .unavailable,
        composer: { _ in fixture }
    )
    blocked.description = "anything"
    #expect(!blocked.canDraft)
}

@Test @MainActor
func pipelineComposerSessionPrepareProbesTheBackendAndLoadsProviders() async throws {
    let fixture = try MuxaWorkComposeResult.decode(composeFixtureReply())
    let session = PipelineComposerSession(
        host: nil,
        defaultProvider: "claude",
        composer: { _ in fixture },
        backendProbe: { .bundledCLI },
        providerLoader: {
            [
                PipelineComposerSession.Provider(id: "claude", title: "Claude Code"),
                PipelineComposerSession.Provider(id: "openai", title: "OpenAI API"),
            ]
        }
    )
    #expect(session.backend == .checking)
    session.description = "anything"
    #expect(!session.canDraft)

    await session.prepare()

    #expect(session.backend == .bundledCLI)
    #expect(session.providers.map(\.id) == ["claude", "openai"])
    #expect(session.canDraft)

    // A daemon without a provider list leaves the built-in pair in place.
    let plain = PipelineComposerSession(
        host: nil,
        defaultProvider: "codex",
        composer: { _ in fixture },
        backendProbe: { .daemon },
        providerLoader: { nil }
    )
    await plain.prepare()
    #expect(plain.backend == .daemon)
    #expect(plain.providers.map(\.id) == ["claude", "codex"])
}

@Test
func composeBundledCLIArgumentsMirrorTheDaemonRequest() throws {
    let current = MuxaPipelineDefinition(agents: [MuxaPipelineDefinition.Agent(alias: "impl")])
    let refine = PipelineComposerSession.Request(
        description: "make the reviewer use gemini",
        agent: "codex",
        current: current,
        name: "solo",
        credential: nil
    )
    #expect(AppModel.composeCLIArguments(for: refine) == [
        "work", "compose", "make the reviewer use gemini", "--json", "--agent", "codex", "--current", "-",
    ])
    let first = PipelineComposerSession.Request(
        description: "solo claude",
        agent: nil,
        current: nil,
        name: nil,
        credential: nil
    )
    #expect(AppModel.composeCLIArguments(for: first) == ["work", "compose", "solo claude", "--json"])

    #expect(AppModel.composeCredentialEnvironmentKey(for: "claude") == "ANTHROPIC_API_KEY")
    #expect(AppModel.composeCredentialEnvironmentKey(for: "anthropic") == "ANTHROPIC_API_KEY")
    #expect(AppModel.composeCredentialEnvironmentKey(for: "codex") == "CODEX_API_KEY")
    #expect(AppModel.composeCredentialEnvironmentKey(for: "openai") == "OPENAI_API_KEY")
    #expect(AppModel.composeCredentialEnvironmentKey(for: "gemini") == "GEMINI_API_KEY")
    #expect(AppModel.composeCredentialEnvironmentKey(for: "unknown") == nil)
}

@Test
func pipelineEditorTargetTellsDraftsFromSavedPipelines() {
    let pipeline = MuxaWorkOptions.Pipeline(name: "implement-review", agents: [])
    let saved = MuxaPipelineEditorTarget(host: nil, pipeline: pipeline)
    let draft = MuxaPipelineEditorTarget(host: nil, pipeline: pipeline, isDraft: true)
    #expect(!saved.isDraft)
    #expect(draft.isDraft)
    #expect(saved.id != draft.id)
    #expect(MuxaPipelineEditorTarget(host: "build-box", pipeline: nil).id == "build-box:+new")
}

// MARK: - Inbox selection and Shells tab (inbox-shells peer)

private func inboxShellsTestHost(
    alias: String,
    local: Bool = false,
    state: String = "online",
    sshTarget: String? = nil
) -> MuxaFleetHost {
    MuxaFleetHost(
        alias: alias,
        local: local,
        sshTarget: sshTarget,
        mode: "control",
        state: state,
        latencyMS: nil,
        error: nil,
        muxaVersion: nil,
        daemonGeneration: nil,
        labels: nil,
        annotations: nil,
        remote: nil
    )
}

private func inboxShellsTestAgent(
    id: String,
    state: String = "waiting_input"
) throws -> MuxaAgent {
    try JSONDecoder().decode(
        MuxaAgent.self,
        from: Data(#"{"kind":"claude_code","agent_session_id":"\#(id)","state":"\#(state)"}"#.utf8)
    )
}

private func inboxShellsTestPane(id: String, socket: String? = nil) throws -> MuxaPaneInfo {
    let socketField = socket.map { #","socket":"\#($0)""# } ?? ""
    return try JSONDecoder().decode(
        MuxaPaneInfo.self,
        from: Data(#"""
        {"pane_id":"\#(id)","session_id":"$1","session":"muxa","window_id":"@2","window_name":"impl",
         "window_index":"1","pane_index":"0","current_command":"claude","title":"","current_path":"/tmp"\#(socketField)}
        """#.utf8)
    )
}

@Test func remoteShellLaunchDialsTheHostsSSHTarget() {
    let host = inboxShellsTestHost(alias: "devbox", sshTarget: "june@devbox.local")
    let launch = MuxaNativeShellLaunch.remoteShell(
        host: host,
        base: ["PATH": "/usr/bin", "TMUX": "/tmp/tmux-1/default,1,0", "TMUX_PANE": "%3", "LANG": "ko_KR.UTF-8"],
        shell: "/bin/zsh",
        appVersion: "1.2.3",
        home: "/Users/june",
        sshExecutable: "/usr/bin/ssh"
    )

    #expect(launch.command == "/usr/bin/ssh")
    #expect(launch.arguments == ["--", "june@devbox.local"])
    #expect(launch.cwd == "/Users/june")
    #expect(launch.name == "devbox shell")
    #expect(launch.environment["TERM"] == "xterm-256color")
    #expect(launch.environment["TERM_PROGRAM"] == "Muxa")
    #expect(launch.environment["TERM_PROGRAM_VERSION"] == "1.2.3")
    #expect(launch.environment["SHELL"] == "/bin/zsh")
    #expect(launch.environment["PATH"] == "/usr/bin")
    #expect(launch.environment["LANG"] == "ko_KR.UTF-8")
    #expect(launch.environment["TMUX"] == nil)
    #expect(launch.environment["TMUX_PANE"] == nil)
}

@Test func remoteShellLaunchFallsBackToAliasAndPathLookup() {
    let bareAlias = inboxShellsTestHost(alias: "rack-7", sshTarget: nil)
    let launch = MuxaNativeShellLaunch.remoteShell(
        host: bareAlias,
        base: [:],
        shell: "/bin/zsh",
        appVersion: "development",
        home: "/Users/june",
        sshExecutable: nil
    )

    #expect(launch.command == "/usr/bin/env")
    #expect(launch.arguments == ["ssh", "--", "rack-7"])
    #expect(launch.name == "rack-7 shell")
    // A missing locale is filled in the same way `createShell()` does it.
    #expect(launch.environment["LANG"] == "en_US.UTF-8")
    #expect(launch.environment["LC_CTYPE"] == "en_US.UTF-8")

    let localTarget = inboxShellsTestHost(alias: "local", local: true, sshTarget: "local://")
    #expect(MuxaNativeShellLaunch.sshDestination(for: localTarget) == "local")
    #expect(MuxaNativeShellLaunch.sshDestination(for: inboxShellsTestHost(alias: "x", sshTarget: "  ")) == "x")
}

@Test func terminalEnvironmentKeepsAnExplicitLocale() {
    let environment = MuxaNativeShellLaunch.terminalEnvironment(
        base: ["LC_ALL": "C.UTF-8"],
        shell: "/bin/bash",
        appVersion: "1.0"
    )

    #expect(environment["LC_ALL"] == "C.UTF-8")
    #expect(environment["LC_CTYPE"] == nil)
    #expect(environment["LANG"] == "en_US.UTF-8")
    #expect(environment["COLORTERM"] == "truecolor")
}

@Test @MainActor
func remoteShellHostsListOnlyReachableFleetHosts() {
    let model = AppModel()
    model.ingestExecutionSnapshotForTesting(MuxaExecutionSnapshot(hosts: [
        inboxShellsTestHost(alias: "local", local: true, state: "online"),
        inboxShellsTestHost(alias: "zeta", state: "online"),
        inboxShellsTestHost(alias: "alpha", state: "version_skew"),
        inboxShellsTestHost(alias: "down", state: "offline"),
        inboxShellsTestHost(alias: "flaky", state: "auth_failed"),
        inboxShellsTestHost(alias: "dialing", state: "connecting"),
    ]))

    #expect(model.remoteShellHosts.map(\.alias) == ["alpha", "zeta"])
}

@Test func shellRowStateTextDescribesExitOutcome() {
    func session(exited: Bool, status: Int32?, attached: Int = 0) -> MuxaSession {
        MuxaSession(
            id: "s",
            backend: .pty,
            displayName: "Muxa Shell 1",
            cwd: nil,
            attachedClients: attached,
            hasBeenAttached: nil,
            exited: exited,
            exitStatus: status,
            pid: nil
        )
    }

    #expect(session(exited: true, status: 0).shellStateText == "Exited")
    #expect(session(exited: true, status: nil).shellStateText == "Exited")
    #expect(session(exited: true, status: 130).shellStateText == "Exited with status 130")
    #expect(session(exited: false, status: nil).shellStateText == "Running")
    #expect(session(exited: false, status: nil, attached: 2).shellStateText == "2 attached")
}

@Test @MainActor
func inboxAttentionRowSelectsWithoutLeavingInbox() throws {
    let model = AppModel()
    let host = MuxaFleetHostIdentity(alias: "devbox", local: false, state: "online", mode: "control")
    let withPane = MuxaHostedAgent(
        host: host,
        agent: try inboxShellsTestAgent(id: "agent-1"),
        pane: try inboxShellsTestPane(id: "%7", socket: "work")
    )
    let withoutPane = MuxaHostedAgent(
        host: host,
        agent: try inboxShellsTestAgent(id: "agent-2"),
        pane: nil
    )

    model.show(.inbox)
    model.select(.agent(withPane.id))
    #expect(model.sidebarMode == .inbox)
    #expect(model.sidebarSelection == .agent("devbox:agent-1"))

    let paneIdentity = try #require(withPane.watchPaneIdentity)
    #expect(paneIdentity == MuxaWatchPaneIdentity(hostAlias: "devbox", socket: "work", paneID: "%7"))
    #expect(withoutPane.watchPaneIdentity == nil)

    // "Open in Live Watch" is the old row behaviour: follow the pane…
    model.openInLiveWatch(withPane)
    #expect(model.sidebarMode == .watch)
    #expect(model.sidebarSelection == .pane(paneIdentity))
    #expect(model.watchSelection == paneIdentity)

    // …or, with no pane, select the agent and stay in the Inbox.
    model.openInLiveWatch(withoutPane)
    #expect(model.sidebarMode == .inbox)
    #expect(model.sidebarSelection == .agent("devbox:agent-2"))
}

@Test func inboxAgentOpenRequestsMatchHostAndSession() throws {
    func request(id: String, to: String, status: String, reply: String?) throws -> MuxaCollaborationRequest {
        let replyField = reply.map {
            #","reply":{"status":"completed","body":"\#($0)","at":"2026-09-03T10:05:00Z"}"#
        } ?? ""
        return try JSONDecoder().decode(
            MuxaCollaborationRequest.self,
            from: Data(#"""
            {
              "id":"\#(id)",
              "from":{"agent_kind":"unknown","agent_session_id":"__muxa_console__","pane":"console","room":{"host":"tmux","window_id":"@4"},"console":true},
              "to":{"agent_kind":"claude_code","agent_session_id":"\#(to)","pane":"%7","room":{"host":"tmux","window_id":"@2"},"alias":"impl"},
              "kind":"task","body":"Body of \#(id)","expects_reply":true,"work_mode":"read_only","status":"\#(status)","created_at":"2026-09-03T10:00:00Z"\#(replyField)
            }
            """#.utf8)
        )
    }
    let devbox = MuxaFleetHostIdentity(alias: "devbox", local: false, state: "online", mode: "control")
    let other = MuxaFleetHostIdentity(alias: "other", local: false, state: "online", mode: "control")
    let route = try inboxShellsTestPane(id: "%1")
    let messages = [
        MuxaOperatorMessage(host: devbox, routePane: route, request: try request(id: "waiting", to: "agent-1", status: "claimed", reply: nil)),
        MuxaOperatorMessage(host: devbox, routePane: route, request: try request(id: "unread", to: "agent-1", status: "completed", reply: "done")),
        MuxaOperatorMessage(host: devbox, routePane: route, request: try request(id: "elsewhere", to: "agent-9", status: "queued", reply: nil)),
        MuxaOperatorMessage(host: other, routePane: route, request: try request(id: "other-host", to: "agent-1", status: "queued", reply: nil)),
    ]
    let participant = MuxaHostedAgent(
        host: devbox,
        agent: try inboxShellsTestAgent(id: "agent-1"),
        pane: nil
    )

    let open = participant.openRequests(in: messages)

    // Unread replies lead, then the request the agent still owes; other
    // agents and other hosts never appear.
    #expect(open.map(\.request.id) == ["unread", "waiting"])
}

// MARK: - Ask provider instances (providers-ui)

/// `ask_providers` from a daemon that understands instances: two Anthropic
/// accounts and a pinned second Claude Code binary in config, then the
/// built-in engines no config entry covers.
private func askProviderInstanceFixture() -> [[String: Any]] {
    [
        [
            "id": "anthropic-work", "title": "Anthropic (work)", "engine": "anthropic",
            "kind": "api", "executable": NSNull(), "credential_env": "ANTHROPIC_API_KEY",
            "credential_required": true, "credential_present": true,
            "model": "claude-opus-5", "selected": true, "builtin": false,
        ],
        [
            "id": "anthropic-personal", "title": "Anthropic (personal)", "engine": "anthropic",
            "kind": "api", "executable": NSNull(), "credential_env": "ANTHROPIC_API_KEY",
            "credential_required": true, "credential_present": false,
            "model": "claude-sonnet-5", "selected": false, "builtin": false,
        ],
        [
            "id": "claude", "title": "Claude Code", "engine": "claude", "kind": "cli",
            "executable": "/opt/homebrew/bin/claude", "credential_env": "ANTHROPIC_API_KEY",
            "credential_required": false, "credential_present": false,
            "model": NSNull(), "selected": false, "builtin": true,
        ],
        [
            "id": "codex", "title": "Codex CLI", "engine": "codex", "kind": "cli",
            "executable": "codex", "credential_env": "CODEX_API_KEY",
            "credential_required": false, "credential_present": false,
            "model": NSNull(), "selected": false, "builtin": true,
        ],
        [
            "id": "anthropic", "title": "Anthropic API", "engine": "anthropic", "kind": "api",
            "executable": NSNull(), "credential_env": "ANTHROPIC_API_KEY",
            "credential_required": true, "credential_present": false,
            "model": "claude-sonnet-5", "selected": false, "builtin": true,
        ],
    ]
}

@Test
func askProvidersCarryEngineAndSeparateTwoInstancesOfIt() async throws {
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        switch object["kind"] as? String {
        case "hello":
            return try askProviderHello(capabilities: ["ask_providers_v1"])
        case "ask_providers":
            return try JSONSerialization.data(withJSONObject: [
                "ok": true,
                "ask_providers": askProviderInstanceFixture(),
            ])
        default:
            return try JSONSerialization.data(withJSONObject: ["ok": true])
        }
    }
    let client = MuxaIPCClient(socketPath: "/tmp/muxa-ask-instances.sock", request: handler)
    try await client.hello()
    let providers = try await client.listAskProviders()
    #expect(providers.map(\.id) == ["anthropic-work", "anthropic-personal", "claude", "codex", "anthropic"])

    // Two instances of one engine are separate providers everywhere it
    // matters: id, title, model and — the point of the exercise — the
    // Keychain account their API key is stored under.
    let work = try #require(providers.first { $0.id == "anthropic-work" })
    let personal = try #require(providers.first { $0.id == "anthropic-personal" })
    #expect(work.engine == "anthropic")
    #expect(personal.engine == "anthropic")
    #expect(work.engineDescriptor == .anthropic)
    #expect(work != personal)
    #expect(work.keychainAccount == "anthropic-work-api-key")
    #expect(personal.keychainAccount == "anthropic-personal-api-key")
    #expect(work.title == "Anthropic (work)")
    #expect(work.model == "claude-opus-5")
    #expect(personal.model == "claude-sonnet-5")
    // They share the engine's environment variable, which is exactly why
    // each one needs its own Keychain entry.
    #expect(work.environmentKey == personal.environmentKey)
    #expect(work.symbolName == personal.symbolName)
    #expect(work.selected)
    #expect(!personal.selected)

    // Configured rows lead; the built-in engines no entry covers follow.
    #expect(providers.filter(\.isConfigured).map(\.id) == ["anthropic-work", "anthropic-personal", "claude"])
    #expect(providers.filter { !$0.isConfigured }.map(\.id) == ["codex", "anthropic"])
    // `claude` is a shipped id, so muxad keeps it flagged built-in even
    // though the operator pinned a second binary for it.
    let claude = try #require(providers.first { $0.id == "claude" })
    #expect(claude.builtin)
    #expect(claude.isConfigured)
    #expect(claude.cliExecutable == "/opt/homebrew/bin/claude")
    #expect(providers.allSatisfy { $0.declaresEngine })

    // `credential_present` says muxad can already resolve a key.
    #expect(work.credentialPresent)
    #expect(!personal.credentialPresent)
}

@Test
func askProviderAddAndRemoveSendTheContractedBodies() async throws {
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        switch object["kind"] as? String {
        case "hello":
            return try askProviderHello(capabilities: ["ask_providers_v1"])
        case "ask_provider_add":
            #expect(object["id"] as? String == "anthropic-work")
            #expect(object["engine"] as? String == "anthropic")
            #expect(object["title"] as? String == "Anthropic (work)")
            #expect(object["model"] as? String == "claude-opus-5")
            // Blank optional fields are left out, never sent as "".
            #expect(object["executable"] == nil)
            #expect(object["api_key_env"] == nil)
            return try JSONSerialization.data(withJSONObject: [
                "ok": true,
                "ask_providers": askProviderInstanceFixture(),
            ])
        case "ask_provider_remove":
            #expect(object["id"] as? String == "anthropic-work")
            return try JSONSerialization.data(withJSONObject: [
                "ok": true,
                "ask_providers": askProviderFixture(),
            ])
        default:
            return try JSONSerialization.data(withJSONObject: ["ok": true])
        }
    }
    let client = MuxaIPCClient(socketPath: "/tmp/muxa-ask-add-remove.sock", request: handler)
    try await client.hello()

    let added = try await client.addAskProvider(
        id: "anthropic-work",
        engine: "anthropic",
        title: "Anthropic (work)",
        model: "claude-opus-5",
        apiKeyEnv: "   ",
        executable: nil
    )
    #expect(added.map(\.id).contains("anthropic-work"))

    let removed = try await client.removeAskProvider("anthropic-work")
    #expect(!removed.map(\.id).contains("anthropic-work"))

    // A CLI instance carries its pinned binary through.
    let pinning: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        switch object["kind"] as? String {
        case "hello":
            return try askProviderHello(capabilities: ["ask_providers_v1"])
        case "ask_provider_add":
            #expect(object["engine"] as? String == "claude")
            #expect(object["executable"] as? String == "/opt/homebrew/bin/claude")
            #expect(object["title"] == nil)
            return try JSONSerialization.data(withJSONObject: [
                "ok": true,
                "ask_providers": askProviderInstanceFixture(),
            ])
        default:
            return try JSONSerialization.data(withJSONObject: ["ok": true])
        }
    }
    let cli = MuxaIPCClient(socketPath: "/tmp/muxa-ask-add-cli.sock", request: pinning)
    try await cli.hello()
    _ = try await cli.addAskProvider(
        id: "claude-brew",
        engine: "claude",
        title: "  ",
        executable: " /opt/homebrew/bin/claude "
    )
}

@Test
func askProviderAddAndRemoveSurfaceTheDaemonsRefusals() async throws {
    let refusing: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        switch object["kind"] as? String {
        case "hello":
            return try askProviderHello(capabilities: ["ask_providers_v1"])
        case "ask_provider_add":
            return try JSONSerialization.data(withJSONObject: [
                "ok": false,
                "error": "provider \"anthropic\" already exists",
            ])
        case "ask_provider_remove":
            return try JSONSerialization.data(withJSONObject: [
                "ok": false,
                "error": "\"gemini\" is a built-in provider with no configuration to remove",
            ])
        default:
            return try JSONSerialization.data(withJSONObject: ["ok": true])
        }
    }
    let client = MuxaIPCClient(socketPath: "/tmp/muxa-ask-refusals.sock", request: refusing)
    try await client.hello()
    await #expect(throws: (any Error).self) {
        _ = try await client.addAskProvider(id: "anthropic", engine: "anthropic")
    }
    await #expect(throws: (any Error).self) {
        _ = try await client.removeAskProvider("gemini")
    }

    // A reply that forgets the refreshed list is a protocol error, not an
    // empty provider list the pane would render.
    let silent: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        if object["kind"] as? String == "hello" {
            return try askProviderHello(capabilities: ["ask_providers_v1"])
        }
        return try JSONSerialization.data(withJSONObject: ["ok": true])
    }
    let quiet = MuxaIPCClient(socketPath: "/tmp/muxa-ask-silent.sock", request: silent)
    try await quiet.hello()
    await #expect(throws: (any Error).self) {
        _ = try await quiet.addAskProvider(id: "anthropic-work", engine: "anthropic")
    }

    // An older daemon never receives the new requests at all.
    let legacy: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        if object["kind"] as? String == "hello" {
            return try askProviderHello(capabilities: ["ask_conversations_v1"])
        }
        Issue.record("a daemon without ask_providers_v1 must not receive add or remove")
        return try JSONSerialization.data(withJSONObject: ["ok": false, "error": "unknown kind"])
    }
    let old = MuxaIPCClient(socketPath: "/tmp/muxa-ask-add-legacy.sock", request: legacy)
    try await old.hello()
    await #expect(throws: (any Error).self) {
        _ = try await old.addAskProvider(id: "anthropic-work", engine: "anthropic")
    }
    await #expect(throws: (any Error).self) {
        _ = try await old.removeAskProvider("anthropic-work")
    }
}

@Test
func askProviderRowsFromADaemonWithoutInstancesStayUsable() throws {
    // The shape muxad sent before instances existed: no `engine`, no
    // `builtin`, no `configured`.
    let rows = try JSONDecoder().decode(
        [MuxaAskProvider].self,
        from: JSONSerialization.data(withJSONObject: askProviderFixture())
    )
    #expect(rows.map(\.id) == ["claude", "codex", "anthropic", "openai"])
    #expect(rows.allSatisfy { !$0.declaresEngine })
    // Engine defaults from the id, so symbols and install hints still work…
    #expect(rows.map(\.engine) == ["claude", "codex", "anthropic", "openai"])
    #expect(rows[0].engineDescriptor == .claude)
    #expect(rows[0].installCommand == "npm install -g @anthropic-ai/claude-code")
    #expect(rows[2].symbolName == "brain")
    // …and every row reads as a built-in with nothing to remove, which is
    // what hides Add and Remove behind `supportsInstances`.
    #expect(rows.allSatisfy { $0.builtin })
    #expect(rows.allSatisfy { $0.configured == nil })
    #expect(rows.allSatisfy { !$0.isConfigured })

    // A daemon that does send `configured` overrides the inference in both
    // directions, so this app never has to guess again.
    let explicit = try JSONDecoder().decode(
        MuxaAskProvider.self,
        from: Data(#"{"id":"anthropic","engine":"anthropic","builtin":true,"configured":true}"#.utf8)
    )
    #expect(explicit.builtin)
    #expect(explicit.isConfigured)
    let bare = try JSONDecoder().decode(
        MuxaAskProvider.self,
        from: Data(#"{"id":"custom","engine":"openai","builtin":false,"configured":false}"#.utf8)
    )
    #expect(!bare.isConfigured)
    // Without the flag, an id muxa does not ship can only come from config.
    let composed = try JSONDecoder().decode(
        MuxaAskProvider.self,
        from: Data(#"{"id":"openai-work","engine":"openai","builtin":false}"#.utf8)
    )
    #expect(composed.isConfigured)
    #expect(composed.engineDescriptor == .openai)
}

@Test
func askProviderUsabilityAcceptsTheDaemonsCredential() {
    let tool = InstalledTool(name: "claude", path: "/opt/homebrew/bin/claude", version: "2.1.0")
    // An API instance whose key muxad already reads from `api_key_env` is
    // usable with nothing in this Mac's Keychain.
    #expect(
        AskProviderStore.usability(kind: .api, detection: .notInstalled, hasKey: false, credentialPresent: true)
            == .usable
    )
    #expect(
        AskProviderStore.usability(kind: .api, detection: .notInstalled, hasKey: false, credentialPresent: false)
            == .missingKey
    )
    #expect(
        AskProviderStore.usability(kind: .api, detection: .notInstalled, hasKey: true, credentialPresent: false)
            == .usable
    )
    // A CLI still needs its binary; a key on either side does not conjure one.
    #expect(
        AskProviderStore.usability(kind: .cli, detection: .notInstalled, hasKey: true, credentialPresent: true)
            == .notInstalled
    )
    #expect(
        AskProviderStore.usability(kind: .cli, detection: .installed(tool), hasKey: false, credentialPresent: false)
            == .usable
    )
}

@Test
func askProviderExecutablePathDetectionSkipsThePathProbe() throws {
    // An instance that pins an absolute binary is checked on disk; PATH
    // never enters into it.
    let present = AskProviderStore.pathDetection(for: "/opt/homebrew/bin/claude") { path in
        path == "/opt/homebrew/bin/claude"
    }
    let tool = try #require(present?.tool)
    #expect(tool.name == "claude")
    #expect(tool.path == "/opt/homebrew/bin/claude")
    #expect(AskProviderStore.pathDetection(for: "/nope/claude") { _ in false } == .notInstalled)
    // A bare command name is left to the PATH probe.
    #expect(AskProviderStore.pathDetection(for: "claude") { _ in true } == nil)
}

@Test
func askProviderDraftValidatesIdsTheWayTheDaemonDoes() {
    #expect(AskProviderDraft.isValidIdentifier("anthropic-work"))
    #expect(AskProviderDraft.isValidIdentifier("openai_2"))
    #expect(AskProviderDraft.isValidIdentifier("A1"))
    #expect(!AskProviderDraft.isValidIdentifier(""))
    #expect(!AskProviderDraft.isValidIdentifier("anthropic work"))
    #expect(!AskProviderDraft.isValidIdentifier("anthropic.work"))
    #expect(!AskProviderDraft.isValidIdentifier("anthropic/work"))
    #expect(!AskProviderDraft.isValidIdentifier("업무"))

    // The prefill is the engine id until it is taken, then -2, -3, …
    let none: Set<String> = []
    #expect(AskProviderDraft.suggestedIdentifier(for: .anthropic, taken: none) == "anthropic")
    #expect(AskProviderDraft.suggestedIdentifier(for: .anthropic, taken: ["anthropic"]) == "anthropic-2")
    #expect(
        AskProviderDraft.suggestedIdentifier(for: .anthropic, taken: ["anthropic", "anthropic-2"])
            == "anthropic-3"
    )
    #expect(AskProviderDraft.uniqueIdentifier(base: "claude", taken: ["codex"]) == "claude")

    var draft = AskProviderDraft()
    #expect(draft.engine.kind == .api)
    #expect(!draft.isReady(taken: none))
    draft.id = "  anthropic-work  "
    #expect(draft.trimmedID == "anthropic-work")
    #expect(draft.isReady(taken: none))
    #expect(draft.validationMessage(taken: none) == nil)
    #expect(draft.validationMessage(taken: ["anthropic-work"])?.isEmpty == false)
    #expect(!draft.isReady(taken: ["anthropic-work"]))
    draft.id = "anthropic work"
    #expect(draft.validationMessage(taken: none)?.isEmpty == false)

    // The engine supplies every default the sheet only hints at.
    #expect(AskProviderEngine.allCases.map(\.rawValue) == ["claude", "codex", "gemini", "anthropic", "openai"])
    #expect(AskProviderEngine.gemini.defaultExecutable == "gemini")
    #expect(AskProviderEngine.gemini.defaultCredentialEnv == "GEMINI_API_KEY")
    #expect(AskProviderEngine.anthropic.defaultExecutable == nil)
    #expect(AskProviderEngine.anthropic.defaultModel == "claude-sonnet-5")
    #expect(AskProviderEngine.openai.defaultModel == "gpt-5")
    #expect(AskProviderEngine.claude.defaultModel == nil)
    #expect(AskProviderEngine.claude.credentialRequired == false)
    #expect(AskProviderEngine.openai.credentialRequired)
}

@Test
func askProviderKeychainAccountsAreOnePerInstance() throws {
    // A new instance gets its own account under the shared service, and the
    // two ids earlier builds wrote are byte-identical.
    #expect(MuxaProviderCredentialStore.service == "dev.muxa.mac.ask-provider")
    #expect(MuxaAskProvider.claude.keychainAccount == "claude-api-key")
    #expect(MuxaAskProvider.codex.keychainAccount == "codex-api-key")
    let work = try #require(MuxaAskProvider(rawValue: "anthropic-work"))
    #expect(work.keychainAccount == "anthropic-work-api-key")
    #expect(work.engine == "anthropic-work")
    // `sendAsk` looks a provider up by the conversation's agent id, so an
    // instance this build never listed still resolves to its own key.
    let personal = try #require(MuxaAskProvider(rawValue: "anthropic-personal"))
    #expect(personal.keychainAccount != work.keychainAccount)
}

@Test
func askProviderExecutableResolutionAcceptsAPinnedBinary() {
    // An instance may pin a second install that is on nobody's PATH, so
    // "Log In…" has to take an absolute path as given.
    #expect(MuxaExecutableResolver.executablePath("/bin/sh") == "/bin/sh")
    #expect(MuxaExecutableResolver.executablePath("/bin/there-is-no-such-binary") == nil)
    // A bare name still goes through the augmented PATH.
    #expect(MuxaExecutableResolver.executablePath("sh")?.hasSuffix("/sh") == true)
    #expect(MuxaExecutableResolver.augmentedPath(nil).contains("/usr/bin"))
}

// MARK: - Settings › Automations, Behaviour and Advanced

/// Collects the raw request payloads a fake handler saw. The handler is
/// `@Sendable`, so the collection needs its own lock.
private final class SettingsPaneRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var storage: [String] = []

    func record(_ payload: Data) {
        lock.lock()
        defer { lock.unlock() }
        storage.append(String(decoding: payload, as: UTF8.self))
    }

    var payloads: [String] {
        lock.lock()
        defer { lock.unlock() }
        return storage
    }

    /// Every recorded request's `kind`, in order.
    var kinds: [String] {
        payloads.map { payload in
            let decoded = try? JSONSerialization.jsonObject(with: Data(payload.utf8))
            return ((decoded as? [String: Any])?["kind"] as? String) ?? "?"
        }
    }

    func object(at index: Int) throws -> [String: Any] {
        try #require(
            JSONSerialization.jsonObject(with: Data(payloads[index].utf8)) as? [String: Any]
        )
    }
}

/// `hello` with the baseline the client requires plus the given extras.
private func settingsPaneHello(capabilities: [String]) throws -> Data {
    try JSONSerialization.data(withJSONObject: [
        "ok": true,
        "min_protocol": 1,
        "max_protocol": 6,
        "capabilities": [
            "session_bytes_v1", "session_attachment_identity_v1", "work_control_v1",
        ] + capabilities,
    ])
}

/// The `automation_rules` payload muxad documents: engine state plus rows
/// that are complete descriptions — filters and action payload verbatim,
/// timing and guards with defaults already resolved.
private func automationRulesFixture(pausedUntil: Any = NSNull()) -> [String: Any] {
    [
        "enabled": true,
        "paused_until": pausedUntil,
        "rules": [
            [
                "name": "resume-after-limit",
                "on": "rate_limited",
                "enabled": true,
                "action": "send_prompt",

                "agent": ["claude_code", "codex"],
                "workspace": "callabo",
                "work": "^CAL-",
                "pane": "%42",
                "host": "local",
                "scope": ["five_hour"],

                "text": "continue",
                "submit": true,

                "wait": "reset+2m",
                "fallback": "20m",
                "jitter": "30s",
                "cooldown": "5m",
                "max_per_hour": 2,
                "only_if_still": "rate_limited",

                "filters": "agent=claude_code,codex work=^CAL- scope=five_hour",
                "fired_last_hour": 1,
                "last_fired_at": "2026-09-03T13:42:11Z",
            ],
            [
                "name": "tell-me",
                "on": "waiting_input",
                "enabled": true,
                "action": "notify",
                "message": "an agent needs you",
                "submit": true,
                "wait": "0s",
                "fallback": "15m",
                "jitter": "15s",
                "cooldown": "2m",
                "max_per_hour": 3,
                "only_if_still": "waiting_input",
                "filters": "any",
                "fired_last_hour": 0,
            ],
            [
                "name": "from-the-future",
                "on": "context_exhausted",
                "enabled": false,
                "action": "compact",
                "wait": "0s",
                "fallback": "15m",
                "jitter": "15s",
                "cooldown": "2m",
                "max_per_hour": 3,
                "only_if_still": "whenever",
                "filters": "any",
                "fired_last_hour": 0,
            ],
        ],
    ]
}

@Test
func automationListDecodesRulesSwitchesAndUnknownVariants() async throws {
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        #expect(object["kind"] as? String == "automation_list")
        return try JSONSerialization.data(withJSONObject: [
            "ok": true,
            "automation_rules": automationRulesFixture(pausedUntil: "2026-09-03T18:00:00Z"),
        ])
    }
    let client = MuxaAutomationClient(socketPath: "/tmp/muxa-automation-test.sock", request: handler)
    let snapshot = try await client.list()

    #expect(snapshot.enabled)
    #expect(snapshot.rules.map(\.name) == ["resume-after-limit", "tell-me", "from-the-future"])

    let full = try #require(snapshot.rules.first)
    #expect(full.on == .rateLimited)
    #expect(full.action == .sendPrompt)
    #expect(full.agent == ["claude_code", "codex"])
    #expect(full.scope == ["five_hour"])
    #expect(full.workspace == "callabo")
    #expect(full.work == "^CAL-")
    #expect(full.pane == "%42")
    #expect(full.host == "local")
    #expect(full.wait == "reset+2m")
    #expect(full.fallback == "20m")
    #expect(full.jitter == "30s")
    #expect(full.text == "continue")
    #expect(full.message == nil)
    #expect(full.submit)
    #expect(full.maxPerHour == 2)
    #expect(full.cooldown == "5m")
    #expect(full.onlyIfStill == .rateLimited)
    // The derived columns the row carries for the table.
    #expect(full.filters == "agent=claude_code,codex work=^CAL- scope=five_hour")
    #expect(full.firedLastHour == 1)
    #expect(MuxaAutomationTime.parse(full.lastFiredAt) != nil)

    // A `notify` rule carries `message` and no `text`.
    let notify = snapshot.rules[1]
    #expect(notify.action == .notify)
    #expect(notify.message == "an agent needs you")
    #expect(notify.text == nil)
    #expect(notify.firedLastHour == 0)
    #expect(notify.lastFiredAt == nil)

    // A newer daemon's event, action and condition survive the round trip
    // instead of failing the whole list.
    let future = snapshot.rules[2]
    #expect(future.on == .other("context_exhausted"))
    #expect(future.action == .other("compact"))
    #expect(future.onlyIfStill == .other("whenever"))
    #expect(future.on.rawValue == "context_exhausted")
    #expect(!future.enabled)

    // The pause is honoured until it expires, then the engine is live again.
    #expect(snapshot.isPaused(now: Date(timeIntervalSince1970: 1_772_000_000)))
    #expect(!snapshot.isPaused(now: Date(timeIntervalSince1970: 2_000_000_000)))
    #expect(!MuxaAutomationSnapshot.empty.isPaused())
}

@Test
func automationLogDecodesFiringsSkipsAndFailures() async throws {
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        #expect(object["kind"] as? String == "automation_log")
        #expect(object["limit"] as? Int == 50)
        return try JSONSerialization.data(withJSONObject: [
            "ok": true,
            "automation_log": [
                [
                    "rule": "resume-after-limit",
                    "pane": "%42",
                    "agent": "claude_code",
                    "fired_at": "2026-09-03T17:02:11Z",
                    "action": "send_prompt",
                    "outcome": "fired",
                    "detail": "continue",
                    "episode": "error@2026-09-03T12:40:02Z",
                ],
                [
                    "rule": "resume-after-limit",
                    "pane": "%42",
                    "agent": "claude_code",
                    "fired_at": "2026-09-03T16:02:11.250Z",
                    "action": "send_prompt",
                    "outcome": "skipped",
                    "detail": "condition_cleared",
                ],
                [
                    "rule": "resume-after-limit",
                    "pane": "%9",
                    "agent": "codex",
                    "fired_at": "2026-09-03T15:02:11Z",
                    "action": "send_prompt",
                    "outcome": "failed",
                    "detail": "pane refused input",
                ],
            ],
        ])
    }
    let client = MuxaAutomationClient(socketPath: "/tmp/muxa-automation-log.sock", request: handler)
    let entries = try await client.log(limit: AutomationStore.logLimit)

    #expect(entries.map(\.outcome) == [.fired, .skipped, .failed])
    #expect(entries[0].action == .sendPrompt)
    #expect(entries[0].episode == "error@2026-09-03T12:40:02Z")
    // A firing's detail is the operator's own text, so it is not a reason…
    #expect(entries[0].skipReason == nil)
    #expect(entries[0].firedDate != nil)
    // …a skip's is a token the pane can say in words. Fractional seconds parse.
    #expect(entries[1].skipReason == "condition_cleared")
    #expect(entries[1].firedDate != nil)
    #expect(
        automationSkipReasonTitle("condition_cleared") == "The agent recovered before it fired"
    )
    // An unknown token is shown as it arrived rather than hidden.
    #expect(automationSkipReasonTitle("brand_new_reason") == "brand_new_reason")
    // A failure is not a skip.
    #expect(entries[2].skipReason == nil)
    #expect(automationOutcomeTitle(.other("weird")) == "weird")
}

@Test
func automationRequestBodiesMatchTheDocumentedWireShapes() throws {
    #expect(MuxaAutomationClient.listRequest()["kind"] as? String == "automation_list")
    #expect(MuxaAutomationClient.logRequest(limit: 25)["limit"] as? Int == 25)

    let toggle = MuxaAutomationClient.setEnabledRequest(name: "resume-after-limit", enabled: false)
    #expect(toggle["kind"] as? String == "automation_set_enabled")
    #expect(toggle["name"] as? String == "resume-after-limit")
    #expect(toggle["enabled"] as? Bool == false)

    // Resuming sends an explicit null, not an absent key.
    #expect(MuxaAutomationClient.pauseRequest(until: nil)["until"] is NSNull)
    #expect(
        MuxaAutomationClient.pauseRequest(until: "2026-09-04T09:00:00Z")["until"] as? String
            == "2026-09-04T09:00:00Z"
    )

    let remove = MuxaAutomationClient.removeRuleRequest(name: "tell-me")
    #expect(remove["kind"] as? String == "automation_remove_rule")
    #expect(remove["name"] as? String == "tell-me")

    let test = MuxaAutomationClient.testRequest(name: "resume-after-limit")
    #expect(test["kind"] as? String == "automation_test")
    #expect(test["name"] as? String == "resume-after-limit")

    let request = MuxaAutomationClient.setRuleRequest(MuxaAutomationRule.sessionLimitRecommendation)
    #expect(request["kind"] as? String == "automation_set_rule")
    let rule = try #require(request["rule"] as? [String: Any])
    #expect(rule["name"] as? String == "resume-after-limit")
    #expect(rule["on"] as? String == "rate_limited")
    #expect(rule["action"] as? String == "send_prompt")
    #expect(rule["text"] as? String == "continue")
    #expect(rule["submit"] as? Bool == true)
    #expect(rule["wait"] as? String == "{{reset}}+2m")
    #expect(rule["fallback"] as? String == "20m")
    #expect(rule["max_per_hour"] as? Int == 2)
    #expect(rule["cooldown"] as? String == "5m")
    // No filters were set, so none are sent; nor is a condition the daemon
    // would rather default for itself.
    #expect(rule["agent"] == nil)
    #expect(rule["scope"] == nil)
    #expect(rule["workspace"] == nil)
    #expect(rule["only_if_still"] == nil)
    // The body has to survive JSONSerialization; a stray Swift type here
    // would only fail at runtime against the real daemon.
    #expect(JSONSerialization.isValidJSONObject(request))
}

@Test
func automationWireObjectCarriesExactlyOneActionPayload() throws {
    // The daemon refuses `message` on a send_prompt rule and `text`/`submit`
    // on anything else, so the payload keys have to be exclusive.
    var rule = MuxaAutomationRule(
        name: "poke-me",
        on: .waitingInput,
        scope: ["five_hour"],
        idleFor: "10m",
        fallback: "20m",
        action: .notify,
        text: "continue",
        message: "an agent needs you",
        submit: false
    )
    var object = rule.wireObject
    #expect(object["message"] as? String == "an agent needs you")
    #expect(object["text"] == nil)
    #expect(object["submit"] == nil)
    // `scope` and `fallback` belong to the rate-limit event; `for` to
    // `idle_for`.
    #expect(object["scope"] == nil)
    #expect(object["fallback"] == nil)
    #expect(object["for"] == nil)

    rule.action = .interrupt
    object = rule.wireObject
    #expect(object["text"] == nil)
    #expect(object["message"] == nil)
    #expect(object["submit"] == nil)

    rule.on = .idleFor
    object = rule.wireObject
    #expect(object["for"] as? String == "10m")

    rule.on = .rateLimited
    rule.action = .sendPrompt
    object = rule.wireObject
    #expect(object["scope"] as? [String] == ["five_hour"])
    #expect(object["fallback"] as? String == "20m")
    #expect(object["text"] as? String == "continue")
    #expect(object["submit"] as? Bool == false)
    #expect(object["message"] == nil)
    #expect(object["for"] == nil)
}

@Test
func automationMutationsAndTestGoThroughTheDaemon() async throws {
    let seen = SettingsPaneRecorder()
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        seen.record(payload)
        if object["kind"] as? String == "automation_test" {
            return try JSONSerialization.data(withJSONObject: [
                "ok": true,
                "automation_test": [
                    "rule": "resume-after-limit",
                    "enabled": true,
                    "engine_enabled": true,
                    "paused_until": NSNull(),
                    "candidates": [
                        [
                            "pane": "%42",
                            "agent_session_id": "sess-1",
                            "agent": "claude_code",
                            "state": "error",
                            "decision": "fire",
                            "fire_at": "2026-09-03T14:42:00Z",
                            "detail": "continue",
                        ],
                        [
                            "agent_session_id": "sess-2",
                            "agent": "codex",
                            "state": "working",
                            "decision": "event_mismatch",
                        ],
                    ],
                ],
            ])
        }
        return try JSONSerialization.data(withJSONObject: [
            "ok": true,
            "automation_rules": automationRulesFixture(),
        ])
    }
    let client = MuxaAutomationClient(socketPath: "/tmp/muxa-automation-mutate.sock", request: handler)

    #expect(try await client.setEnabled(name: "tell-me", enabled: false).rules.count == 3)
    #expect(try await client.pause(until: nil).pausedUntilText == nil)
    #expect(try await client.setRule(.sessionLimitRecommendation).enabled)
    #expect(try await client.removeRule(name: "tell-me").rules.count == 3)

    let report = try await client.test(name: "resume-after-limit")
    #expect(report.rule == "resume-after-limit")
    #expect(report.engineEnabled)
    #expect(report.candidates.count == 2)
    #expect(report.firing.map(\.pane) == ["%42"])
    #expect(report.candidates[0].wouldFire)
    #expect(report.candidates[0].fireDate != nil)
    #expect(!report.candidates[1].wouldFire)
    #expect(report.candidates[1].pane == nil)
    #expect(automationDecisionTitle("fire") == "Would fire")
    #expect(automationDecisionTitle("event_mismatch") == "The agent is not in this rule's state")

    #expect(seen.kinds == [
        "automation_set_enabled", "automation_pause", "automation_set_rule",
        "automation_remove_rule", "automation_test",
    ])

    // A reply without the payload is a protocol error, not an empty table.
    let empty = MuxaAutomationClient(socketPath: "/tmp/muxa-automation-empty.sock") { _, _ in
        try JSONSerialization.data(withJSONObject: ["ok": true])
    }
    await #expect(throws: (any Error).self) { _ = try await empty.list() }
    await #expect(throws: (any Error).self) { _ = try await empty.test(name: "x") }

    let refusing = MuxaAutomationClient(socketPath: "/tmp/muxa-automation-refuse.sock") { _, _ in
        try JSONSerialization.data(withJSONObject: [
            "ok": false,
            "error": #"automation.rule "x": `action = "notify"` requires `message`"#,
        ])
    }
    do {
        _ = try await refusing.setRule(.sessionLimitRecommendation)
        Issue.record("a rejected rule must throw")
    } catch {
        #expect(
            error.localizedDescription
                == #"automation.rule "x": `action = "notify"` requires `message`"#
        )
    }
}

@Test
func automationRequestsNeedTheCapability() async throws {
    let legacy: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        if object["kind"] as? String == "hello" {
            return try settingsPaneHello(capabilities: [])
        }
        Issue.record("an old daemon must not receive automation requests")
        return try JSONSerialization.data(withJSONObject: ["ok": false, "error": "unknown kind"])
    }
    let old = MuxaIPCClient(socketPath: "/tmp/muxa-automation-legacy.sock", request: legacy)
    try await old.hello()
    #expect(await !old.supports(MuxaIPCClient.automationCapability))
    await #expect(throws: (any Error).self) { _ = try await old.automationList() }
    await #expect(throws: (any Error).self) { _ = try await old.automationLog(limit: 10) }
    await #expect(throws: (any Error).self) { _ = try await old.automationPause(until: nil) }
    await #expect(throws: (any Error).self) { _ = try await old.automationTest(name: "x") }
    await #expect(throws: (any Error).self) {
        _ = try await old.automationSetEnabled(name: "x", enabled: true)
    }
    await #expect(throws: (any Error).self) {
        _ = try await old.automationSetRule(.sessionLimitRecommendation)
    }
    await #expect(throws: (any Error).self) { _ = try await old.automationRemoveRule(name: "x") }
    await #expect(throws: (any Error).self) { _ = try await old.readDaemonConfig() }
    await #expect(throws: (any Error).self) {
        _ = try await old.writeDaemonConfig(text: "", expectedText: nil)
    }

    let current = MuxaIPCClient(socketPath: "/tmp/muxa-automation-current.sock") { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        #expect(object["kind"] as? String == "hello")
        return try settingsPaneHello(capabilities: ["automation_v1", "config_edit_v1"])
    }
    try await current.hello()
    #expect(await current.supports(MuxaIPCClient.automationCapability))
    #expect(await current.supports(MuxaIPCClient.configEditCapability))
}

@Test
func automationDurationGrammarMatchesTheDaemons() {
    #expect(MuxaAutomationDuration.parse("45s") == 45)
    #expect(MuxaAutomationDuration.parse("5m") == 300)
    #expect(MuxaAutomationDuration.parse("2h") == 7200)
    #expect(MuxaAutomationDuration.parse("1d") == 86_400)
    #expect(MuxaAutomationDuration.parse(" 90s ") == 90)
    // A bare `0` is the one number that needs no unit.
    #expect(MuxaAutomationDuration.parse("0") == 0)
    // Any other bare number is refused on purpose: `20` reads as seconds to
    // one operator and minutes to the next.
    #expect(MuxaAutomationDuration.parse("20") == nil)
    #expect(MuxaAutomationDuration.parse("2w") == nil)
    #expect(MuxaAutomationDuration.parse("") == nil)
    #expect(MuxaAutomationDuration.parse("m") == nil)
    #expect(MuxaAutomationDuration.parse("2.5m") == nil)
    #expect(MuxaAutomationDuration.parse("-5m") == nil)

    // `wait` adds the reset anchor, in both directions.
    #expect(MuxaAutomationDuration.parseWait("reset") == .afterReset(0))
    #expect(MuxaAutomationDuration.parseWait("reset+2m") == .afterReset(120))
    #expect(MuxaAutomationDuration.parseWait("{{reset}}+2m") == .afterReset(120))
    #expect(MuxaAutomationDuration.parseWait("{{reset}}") == .afterReset(0))
    #expect(MuxaAutomationDuration.parseWait("{{reset}}-30s") == .afterReset(-30))
    #expect(MuxaAutomationDuration.parseWait("reset-30s") == .afterReset(-30))
    #expect(MuxaAutomationDuration.parseWait("reset + 2m") == .afterReset(120))
    #expect(MuxaAutomationDuration.parseWait("10m") == .delay(600))
    #expect(MuxaAutomationDuration.parseWait("0") == .delay(0))
    #expect(MuxaAutomationDuration.parseWait("") == nil)
    #expect(MuxaAutomationDuration.parseWait("reset+") == nil)
    #expect(MuxaAutomationDuration.parseWait("reset2m") == nil)
    #expect(MuxaAutomationDuration.parseWait("resets") == nil)
    #expect(MuxaAutomationDuration.parseWait("reset+2m").map(\.needsResetTime) == true)
    #expect(MuxaAutomationDuration.parseWait("10m").map(\.needsResetTime) == false)

    // The compact spelling the daemon renders back, so a value round-trips
    // through config.toml unchanged.
    #expect(MuxaAutomationDuration.render(0) == "0s")
    #expect(MuxaAutomationDuration.render(45) == "45s")
    #expect(MuxaAutomationDuration.render(300) == "5m")
    #expect(MuxaAutomationDuration.render(7200) == "2h")
    #expect(MuxaAutomationDuration.render(86_400) == "1d")
    #expect(MuxaAutomationDuration.render(90) == "90s")
}

@Test
func automationRuleDraftValidationTable() {
    let taken: Set<String> = ["resume-after-limit"]

    // The shortcut fills in a rule that is ready to save as-is.
    let recommended = MuxaAutomationRuleDraft.sessionLimitDraft
    #expect(recommended.originalName == nil)
    #expect(recommended.isReady(existingNames: []))
    #expect(recommended.rule.name == "resume-after-limit")
    #expect(recommended.rule.wait == "{{reset}}+2m")
    #expect(recommended.rule.maxPerHour == 2)
    #expect(recommended.timing == .afterReset(120))
    #expect(recommended.fallbackSeconds == 1200)
    #expect(!recommended.fallbackIsDefault)

    // …but not over a rule that already has that name.
    #expect(recommended.issues(existingNames: taken) == [.duplicateName])
    // Editing that same rule keeps its own name.
    var editing = recommended
    editing.originalName = "resume-after-limit"
    #expect(editing.isReady(existingNames: taken))

    var draft = MuxaAutomationRuleDraft()
    #expect(draft.issues(existingNames: []) == [.missingName, .missingText])

    draft.name = "no spaces please"
    #expect(draft.issues(existingNames: []).contains(.invalidName))
    // Dots are part of the daemon's key grammar; 64 characters is its cap.
    draft.name = "resume.after-limit_2"
    draft.text = "continue"
    #expect(draft.isReady(existingNames: []))
    draft.name = String(repeating: "a", count: 65)
    #expect(draft.issues(existingNames: []) == [.nameTooLong])
    draft.name = String(repeating: "a", count: 64)
    #expect(draft.isReady(existingNames: []))

    // Each action carries its own payload, and only its own.
    draft.text = "   "
    #expect(draft.issues(existingNames: []) == [.missingText])
    draft.text = "continue\u{1B}[31m"
    #expect(draft.issues(existingNames: []) == [.textNotTerminalSafe])
    draft.text = "line one\nline two\tindented"
    #expect(draft.isReady(existingNames: []))
    draft.text = String(repeating: "x", count: 4097)
    #expect(draft.issues(existingNames: []) == [.textTooLong])
    draft.text = "continue"

    draft.action = .notify
    #expect(draft.issues(existingNames: []) == [.missingMessage])
    draft.message = "an agent needs you"
    #expect(draft.isReady(existingNames: []))
    draft.action = .interrupt
    #expect(draft.isReady(existingNames: []))
    draft.action = .sendPrompt

    // Every duration field is optional, but must parse and stay under 24h.
    draft.cooldown = "soon"
    #expect(draft.issues(existingNames: []) == [.invalidDuration(.cooldown)])
    draft.cooldown = "2d"
    #expect(draft.issues(existingNames: []) == [.durationTooLong(.cooldown)])
    draft.cooldown = ""
    #expect(draft.isReady(existingNames: []))
    draft.jitter = "30"
    #expect(draft.issues(existingNames: []) == [.invalidDuration(.jitter)])
    draft.jitter = ""
    draft.wait = "reset+soon"
    #expect(draft.issues(existingNames: []) == [.invalidWait])
    draft.wait = "reset-30s"
    #expect(draft.isReady(existingNames: []))
    #expect(draft.timing == .afterReset(-30))
    draft.fallback = "later"
    #expect(draft.issues(existingNames: []) == [.invalidDuration(.fallback)])
    draft.fallback = ""

    // Only a rate limit carries a reset time, so a reset anchor needs it —
    // and a fallback means nothing without one, so it is neither validated
    // nor sent.
    draft.event = .waitingInput
    #expect(draft.issues(existingNames: []) == [.resetWaitNeedsRateLimit])
    draft.fallback = "later"
    #expect(draft.issues(existingNames: []) == [.resetWaitNeedsRateLimit])
    draft.fallback = ""
    draft.wait = "5m"
    #expect(draft.isReady(existingNames: []))
    #expect(draft.rule.fallback == nil)

    // An empty `wait` is the event's own default, not "immediately".
    draft.wait = ""
    #expect(draft.timing == .delay(0))
    draft.event = .rateLimited
    #expect(draft.timing == .afterReset(0))
    // …and an empty fallback or jitter previews muxad's default.
    #expect(draft.fallbackIsDefault)
    #expect(draft.fallbackSeconds == 900)
    #expect(draft.jitterIsDefault)
    #expect(draft.jitterSeconds == 15)

    // `idle_for` is the one event that requires its own duration.
    draft.event = .idleFor
    #expect(draft.issues(existingNames: []) == [.missingDuration(.idleFor)])
    draft.idleFor = "ages"
    #expect(draft.issues(existingNames: []) == [.invalidDuration(.idleFor)])
    draft.idleFor = "0"
    #expect(draft.issues(existingNames: []) == [.zeroIdleDuration])
    draft.idleFor = "10m"
    #expect(draft.isReady(existingNames: []))

    // Filters the daemon parses are checked here too, so the sheet says it
    // rather than the round trip.
    draft.work = "^CAL-("
    #expect(draft.issues(existingNames: []) == [.invalidWorkRegex])
    draft.work = "^CAL-"
    draft.host = "elsewhere"
    #expect(draft.issues(existingNames: []) == [.invalidHost])
    draft.host = "tmux"
    #expect(draft.isReady(existingNames: []))

    draft.maxPerHour = 0
    #expect(draft.issues(existingNames: []) == [.invalidMaxPerHour])
    draft.maxPerHour = 61
    #expect(draft.issues(existingNames: []) == [.invalidMaxPerHour])
    draft.maxPerHour = 3
    #expect(draft.isReady(existingNames: []))

    // An empty cooldown falls back to the daemon's default rather than
    // writing an empty string into the file.
    #expect(draft.rule.cooldown == MuxaAutomationRule.defaultCooldown)
}

@Test
func automationRuleDraftRoundTripsThroughEditing() {
    let original = MuxaAutomationRule(
        name: "tell-me",
        on: .waitingInput,
        enabled: false,
        agent: ["codex", "claude_code"],
        workspace: "callabo",
        host: "tmux",
        action: .notify,
        message: "an agent needs you",
        submit: false,
        maxPerHour: 2,
        cooldown: "5m",
        onlyIfStill: .any
    )
    let draft = MuxaAutomationRuleDraft.draft(editing: original)
    #expect(draft.originalName == "tell-me")
    // The set is sorted on the way back out, so the wire order is stable.
    #expect(draft.rule.agent == ["claude_code", "codex"])
    #expect(draft.rule.enabled == false)
    #expect(draft.rule.onlyIfStill == .any)
    #expect(draft.rule.message == "an agent needs you")
    #expect(draft.rule.text == nil)
    #expect(draft.rule.host == "tmux")
    #expect(draft.rule.wireObject["only_if_still"] as? String == "any")
    #expect(draft.rule.wireObject["submit"] == nil)
}

@Test
func automationRuleRendersAsAPastableTOMLBlock() {
    #expect(MuxaAutomationRule.sessionLimitRecommendation.tomlSnippet == """
    [[automation.rule]]
    name = "resume-after-limit"
    on = "rate_limited"
    enabled = true
    wait = "{{reset}}+2m"
    fallback = "20m"
    action = "send_prompt"
    text = "continue"
    submit = true
    max_per_hour = 2
    cooldown = "5m"

    """)

    // A notify rule writes `message`, never `text` or `submit`.
    let notify = MuxaAutomationRule(
        name: "tell-me",
        on: .waitingInput,
        action: .notify,
        message: "an agent needs you",
        onlyIfStill: .any
    )
    #expect(notify.tomlSnippet == """
    [[automation.rule]]
    name = "tell-me"
    on = "waiting_input"
    enabled = true
    action = "notify"
    message = "an agent needs you"
    max_per_hour = 3
    cooldown = "2m"
    only_if_still = "any"

    """)

    // Quotes and newlines in prompt text stay valid TOML.
    let awkward = MuxaAutomationRule(
        name: "quote",
        on: .error,
        action: .sendPrompt,
        text: "say \"hi\"\nthen stop"
    )
    #expect(awkward.tomlSnippet.contains(#"text = "say \"hi\"\nthen stop""#))
}

@Test
func automationTextSafetyMatchesTheDaemonsRule() {
    // Only printable text, tabs and newlines reach a live TUI.
    #expect(MuxaAutomationRule.isTerminalSafe("continue"))
    #expect(MuxaAutomationRule.isTerminalSafe("line one\nline two\ttabbed"))
    #expect(MuxaAutomationRule.isTerminalSafe("계속 진행해 주세요"))
    #expect(!MuxaAutomationRule.isTerminalSafe("continue\u{1B}[31m"))
    #expect(!MuxaAutomationRule.isTerminalSafe("continue\u{07}"))
    #expect(!MuxaAutomationRule.isTerminalSafe("continue\r"))
}

@Test
func automationPauseUntilTomorrowIsTheNextMorning() throws {
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = try #require(TimeZone(identifier: "Asia/Seoul"))
    let now = try #require(calendar.date(from: DateComponents(
        year: 2026, month: 9, day: 3, hour: 22, minute: 40
    )))
    let until = MuxaAutomationTime.tomorrowMorning(from: now, calendar: calendar)
    let parts = calendar.dateComponents([.year, .month, .day, .hour, .minute], from: until)
    #expect(parts.year == 2026)
    #expect(parts.month == 9)
    #expect(parts.day == 4)
    #expect(parts.hour == 9)
    #expect(parts.minute == 0)

    // The text the pause request carries round-trips back to the same moment.
    let text = MuxaAutomationTime.text(until)
    #expect(MuxaAutomationTime.parse(text) == until)
    #expect(MuxaAutomationTime.parse(nil) == nil)
    #expect(MuxaAutomationTime.parse("not a date") == nil)
}

@Test
func configReadAndWriteCarryTheExpectedText() async throws {
    let document = """
    [notifier]
    enabled = true

    """
    let seen = SettingsPaneRecorder()
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        seen.record(payload)
        let text = object["kind"] as? String == "config_write"
            ? (object["text"] as? String ?? "")
            : document
        return try JSONSerialization.data(withJSONObject: [
            "ok": true,
            "config": ["path": "/Users/june/.config/muxa/config.toml", "text": text, "exists": true],
        ])
    }
    let client = MuxaConfigClient(socketPath: "/tmp/muxa-config-test.sock", request: handler)

    let read = try await client.read()
    #expect(read.path == "/Users/june/.config/muxa/config.toml")
    #expect(read.text == document)
    #expect(read.exists)
    #expect(read.url?.lastPathComponent == "config.toml")

    let updated = document + "\n[collaboration]\nenabled = true\n"
    let saved = try await client.write(text: updated, expectedText: read.text)
    #expect(saved.text == updated)

    #expect(seen.kinds == ["config_read", "config_write"])
    #expect(try seen.object(at: 1)["expected_text"] as? String == document)

    // A first write to a file that does not exist yet sends an explicit
    // null rather than pretending to know what is on disk.
    #expect(MuxaConfigClient.writeRequest(text: "x", expectedText: nil)["expected_text"] is NSNull)

    // The daemon's validation message reaches the pane verbatim.
    let refusing = MuxaConfigClient(socketPath: "/tmp/muxa-config-refuse.sock") { _, _ in
        try JSONSerialization.data(withJSONObject: [
            "ok": false,
            "error": "config.toml: unknown field `enabld` at line 2",
        ])
    }
    do {
        _ = try await refusing.write(text: "", expectedText: nil)
        Issue.record("a rejected write must throw")
    } catch {
        #expect(
            error.localizedDescription == "config.toml: unknown field `enabld` at line 2"
        )
    }

    // A missing `config` object is a protocol error, not an empty document.
    let silent = MuxaConfigClient(socketPath: "/tmp/muxa-config-silent.sock") { _, _ in
        try JSONSerialization.data(withJSONObject: ["ok": true])
    }
    await #expect(throws: (any Error).self) { _ = try await silent.read() }
}

@Test
func configReadHandlesAFileThatDoesNotExistYet() async throws {
    let client = MuxaConfigClient(socketPath: "/tmp/muxa-config-absent.sock") { _, _ in
        try JSONSerialization.data(withJSONObject: [
            "ok": true,
            "config": ["path": "/Users/june/.config/muxa/config.toml", "text": "", "exists": false],
        ])
    }
    let document = try await client.read()
    #expect(!document.exists)
    #expect(document.text.isEmpty)
    #expect(document.path.hasSuffix("config.toml"))
    // The first write to a file that is not there has no text to match on.
    #expect(MuxaConfigClient.writeRequest(text: "x", expectedText: nil)["expected_text"] is NSNull)
}

@Test
func configWriteConflictCarriesTheFileAsItNowStands() async throws {
    let onDisk = """
    [notifier]
    enabled = true
    backend = "libnotify"

    """
    let conflicting = MuxaConfigClient(socketPath: "/tmp/muxa-config-conflict.sock") { _, _ in
        try JSONSerialization.data(withJSONObject: [
            "ok": false,
            "error": "config.toml changed on disk since it was read; reload it and apply the edit again",
            "config": [
                "path": "/Users/june/.config/muxa/config.toml", "text": onDisk, "exists": true,
            ],
        ])
    }
    do {
        _ = try await conflicting.write(text: "[notifier]\nenabled = false\n", expectedText: "")
        Issue.record("a stale expected_text must throw")
    } catch let conflict as MuxaConfigConflict {
        // The message reaches the pane verbatim, and the document it came
        // with is what a retry has to be built on.
        #expect(
            conflict.message
                == "config.toml changed on disk since it was read; reload it and apply the edit again"
        )
        #expect(conflict.localizedDescription == conflict.message)
        #expect(conflict.current.text == onDisk)
        #expect(conflict.current.exists)
        // Re-applying the same edit lands on top of what is on disk rather
        // than reverting someone else's change.
        let merged = MuxaTOMLPatcher.apply(
            [MuxaTOMLEdit("notifier", "enabled", .bool(false))],
            to: conflict.current.text
        )
        #expect(MuxaTOMLPatcher.bool(section: "notifier", key: "enabled", in: merged, default: true) == false)
        #expect(MuxaTOMLPatcher.string(section: "notifier", key: "backend", in: merged) == "libnotify")
    }

    // A refusal that carries no document is a bad document, not a conflict:
    // nothing changed, so there is nothing to rebase onto.
    let invalid = MuxaConfigClient(socketPath: "/tmp/muxa-config-invalid.sock") { _, _ in
        try JSONSerialization.data(withJSONObject: [
            "ok": false,
            "error": "config.toml: unknown field `enabld` at line 2",
        ])
    }
    await #expect(throws: MuxaIPCError.self) {
        _ = try await invalid.write(text: "", expectedText: "")
    }
}

/// A config file with the shapes the patcher has to survive: comments,
/// an array-of-tables, and a multi-line string that looks like a section.
private let behaviourConfigFixture = """
# muxa configuration
[notifier]
enabled = false   # quiet by default
backend = "none"

[ui]
banner = \"\"\"
[notifier]
enabled = true
\"\"\"

[collaboration]
enabled = true
wake = "idle_only"

[[automation.rule]]
name = "resume-after-limit"
enabled = true

"""

@Test
func tomlPatcherRewritesOneKeyAndLeavesTheRestAlone() {
    let text = behaviourConfigFixture

    #expect(MuxaTOMLPatcher.bool(section: "notifier", key: "enabled", in: text, default: true) == false)
    #expect(MuxaTOMLPatcher.string(section: "notifier", key: "backend", in: text) == "none")
    #expect(MuxaTOMLPatcher.bool(section: "collaboration", key: "enabled", in: text, default: false))
    #expect(MuxaTOMLPatcher.string(section: "collaboration", key: "wake", in: text) == "idle_only")
    // Absent keys and absent sections read as absent, not as a wrong value.
    #expect(MuxaTOMLPatcher.value(section: "collaboration", key: "scope", in: text) == nil)
    #expect(MuxaTOMLPatcher.value(section: "automation", key: "enabled", in: text) == nil)

    // Rewriting a value keeps the trailing comment and the whole document.
    let flipped = MuxaTOMLPatcher.apply(
        [MuxaTOMLEdit("notifier", "enabled", .bool(true))],
        to: text
    )
    #expect(flipped.contains("enabled = true   # quiet by default"))
    #expect(flipped.contains("backend = \"none\""))
    // The `enabled` inside the multi-line string and the one in the
    // array-of-tables are different keys and stay untouched: only the
    // notifier line, which carries the comment, was rewritten.
    #expect(flipped.contains("banner = \"\"\"\n[notifier]\nenabled = true\n\"\"\""))
    #expect(flipped.contains("name = \"resume-after-limit\"\nenabled = true\n"))
    #expect(flipped.components(separatedBy: "enabled = true").count == 5)
    #expect(MuxaTOMLPatcher.bool(section: "notifier", key: "enabled", in: flipped, default: false))

    // A missing key joins its table instead of splitting it.
    let scoped = MuxaTOMLPatcher.apply(
        [MuxaTOMLEdit("collaboration", "scope", .string("host"))],
        to: text
    )
    #expect(scoped.contains("wake = \"idle_only\"\nscope = \"host\"\n\n[[automation.rule]]"))
    #expect(MuxaTOMLPatcher.string(section: "collaboration", key: "scope", in: scoped) == "host")

    // A missing table is appended whole, after the existing document.
    let created = MuxaTOMLPatcher.apply(
        [MuxaTOMLEdit("automation", "enabled", .bool(false))],
        to: text
    )
    #expect(created.hasSuffix("[automation]\nenabled = false\n"))
    #expect(MuxaTOMLPatcher.bool(section: "automation", key: "enabled", in: created, default: true) == false)
    // …and writing into an empty document creates the table too.
    #expect(
        MuxaTOMLPatcher.apply([MuxaTOMLEdit("automation", "enabled", .bool(true))], to: "")
            == "[automation]\nenabled = true\n"
    )

    // Several edits compose.
    let both = MuxaTOMLPatcher.apply(
        [
            MuxaTOMLEdit("notifier", "backend", .string("libnotify")),
            MuxaTOMLEdit("notifier", "enabled", .bool(true)),
        ],
        to: text
    )
    #expect(MuxaTOMLPatcher.string(section: "notifier", key: "backend", in: both) == "libnotify")
    #expect(MuxaTOMLPatcher.bool(section: "notifier", key: "enabled", in: both, default: false))
}

@Test
func tomlPatcherReadsTheScalarsTheFormsWrite() {
    #expect(MuxaTOMLPatcher.scalar(from: " true ") == .bool(true))
    #expect(MuxaTOMLPatcher.scalar(from: "false") == .bool(false))
    #expect(MuxaTOMLPatcher.scalar(from: #""idle_only""#) == .string("idle_only"))
    #expect(MuxaTOMLPatcher.scalar(from: "'idle_only'") == .string("idle_only"))
    #expect(MuxaTOMLPatcher.scalar(from: #""a \"b\"""#) == .string("a \"b\""))
    #expect(MuxaTOMLPatcher.scalar(from: "16_384") == .integer(16384))
    #expect(MuxaTOMLPatcher.scalar(from: "[1, 2]") == nil)

    #expect(MuxaTOMLScalar.bool(true).literal == "true")
    #expect(MuxaTOMLScalar.integer(2).literal == "2")
    #expect(MuxaTOMLScalar.string("a\"b").literal == #""a\"b""#)

    // A quoted key spelling still matches, and an unrelated key does not.
    #expect(MuxaTOMLPatcher.assignment(in: #""enabled" = true"#, key: "enabled")?.value == "true")
    #expect(MuxaTOMLPatcher.assignment(in: "enabled_extra = true", key: "enabled") == nil)
    // A `#` inside a string is not the start of a comment.
    let assignment = MuxaTOMLPatcher.assignment(in: ##"text = "a # b"  # note"##, key: "text")
    #expect(assignment?.value == #""a # b""#)
    #expect(assignment?.comment == "  # note")
}

@Test
func behaviourSettingsReadDefaultsAndWriteOnlyWhatChanged() {
    // A document naming none of these keys reads back as the daemon runs.
    let defaults = MuxaBehaviourSettings.read(from: "[ask]\nenabled = true\n")
    #expect(defaults == MuxaBehaviourSettings.daemonDefaults)
    #expect(!defaults.notifierEnabled)
    #expect(defaults.notifierBackend == .none)
    #expect(!defaults.collaborationEnabled)
    #expect(defaults.collaborationWake == .idleOnly)
    #expect(defaults.collaborationWakePayload == .operatorFull)
    #expect(defaults.collaborationScope == .window)

    let current = MuxaBehaviourSettings.read(from: behaviourConfigFixture)
    #expect(!current.notifierEnabled)
    #expect(current.collaborationEnabled)
    #expect(current.collaborationWake == .idleOnly)

    // Nothing changed means nothing written.
    #expect(current.edits(against: current).isEmpty)

    var wanted = current
    wanted.notifierEnabled = true
    wanted.notifierBackend = .libnotify
    wanted.collaborationScope = .host
    let edits = wanted.edits(against: current)
    #expect(edits.count == 3)
    #expect(edits.contains(MuxaTOMLEdit("notifier", "enabled", .bool(true))))
    #expect(edits.contains(MuxaTOMLEdit("notifier", "backend", .string("libnotify"))))
    // The daemon's key is `scope`, not `pane_scope`.
    #expect(edits.contains(MuxaTOMLEdit("collaboration", "scope", .string("host"))))

    let patched = MuxaTOMLPatcher.apply(edits, to: behaviourConfigFixture)
    #expect(MuxaBehaviourSettings.read(from: patched) == wanted)
    // Everything the form does not own is byte-identical.
    #expect(patched.contains("name = \"resume-after-limit\""))
    #expect(patched.contains("banner = \"\"\""))

    // A value muxa does not know is left alone rather than silently reset.
    let odd = MuxaBehaviourSettings.read(from: "[collaboration]\nwake = \"whenever\"\n")
    #expect(odd.collaborationWake == .idleOnly)
}

// MARK: - Automations: the anchor, the marks, and the empty filters

@Test
func theResetAnchorIsReadInBothSpellingsAndSaidRatherThanPrinted() {
    // What muxad writes now, and what rules written before the braces carry.
    #expect(MuxaAutomationWaitText.parse("{{reset}}") == .afterReset(0))
    #expect(MuxaAutomationWaitText.parse("{{reset}}+10m") == .afterReset(600))
    #expect(MuxaAutomationWaitText.parse("{{reset}}-30s") == .afterReset(-30))
    #expect(MuxaAutomationWaitText.parse("reset+2m") == .afterReset(120))
    #expect(MuxaAutomationWaitText.parse("reset") == .afterReset(0))
    #expect(MuxaAutomationWaitText.parse("5m") == .delay(300))
    // A half-written or malformed anchor is not one.
    #expect(MuxaAutomationWaitText.parse("{{reset}}*2m") == nil)
    #expect(MuxaAutomationWaitText.parse("{{reset}") == nil)
    #expect(MuxaAutomationWaitText.parse("20") == nil)

    #expect(MuxaAutomationWaitText.isAnchored("{{reset}}+2m"))
    #expect(MuxaAutomationWaitText.isAnchored("reset"))
    #expect(!MuxaAutomationWaitText.isAnchored("5m"))

    // The chip reads the offset off the anchor rather than the raw token.
    #expect(MuxaAutomationWaitText.resetOffset("{{reset}}+2m") == 120)
    #expect(MuxaAutomationWaitText.resetOffset("{{reset}}-30s") == -30)
    #expect(MuxaAutomationWaitText.resetOffset("{{reset}}") == 0)
    #expect(MuxaAutomationWaitText.resetOffset("5m") == nil)
    #expect(MuxaAutomationWaitText.resetOffset("nonsense") == nil)

    // Beside the chip goes the offset, never the token — and a malformed
    // offset is still shown in full rather than dropped.
    #expect(MuxaAutomationWaitText.anchorOffsetText("{{reset}}+2m") == "+2m")
    #expect(MuxaAutomationWaitText.anchorOffsetText("reset+2m") == "+2m")
    #expect(MuxaAutomationWaitText.anchorOffsetText("{{reset}}-30s") == "-30s")
    #expect(MuxaAutomationWaitText.anchorOffsetText("{{reset}}").isEmpty)
    #expect(MuxaAutomationWaitText.anchorOffsetText("{{reset}}*2m") == "*2m")
}

@Test
func waitControlsComposeTheAnchorSoNobodyTypesIt() {
    // Every spelling the controls can express round-trips unchanged.
    for text in ["{{reset}}", "{{reset}}+2m", "{{reset}}-30s", "5m", "0s"] {
        let draft = MuxaAutomationWaitDraft.read(text, event: .rateLimited)
        #expect(draft.anchor != .freeform, "\(text) should be expressible")
        #expect(draft.text == text)
    }

    let after = MuxaAutomationWaitDraft.read("{{reset}}+2m", event: .rateLimited)
    #expect(after.anchor == .reset)
    #expect(!after.isBefore)
    #expect(after.offset == "2m")

    let before = MuxaAutomationWaitDraft.read("{{reset}}-30s", event: .rateLimited)
    #expect(before.anchor == .reset)
    #expect(before.isBefore)
    #expect(before.offset == "30s")

    // A rule written before the braces reads, and is written back the way
    // muxad spells it now.
    #expect(MuxaAutomationWaitDraft.read("reset+2m", event: .rateLimited).text == "{{reset}}+2m")
    #expect(MuxaAutomationWaitDraft.read("reset", event: .rateLimited).text == "{{reset}}")

    // A plain delay stays a delay.
    let delay = MuxaAutomationWaitDraft.read("5m", event: .idleFor)
    #expect(delay.anchor == .event)
    #expect(delay.offset == "5m")

    // An empty wait starts the controls where the daemon's own default is,
    // and an event with no reset time never starts on the anchor.
    #expect(MuxaAutomationWaitDraft.read("", event: .rateLimited).anchor == .reset)
    #expect(MuxaAutomationWaitDraft.read("", event: .idleFor).anchor == .event)
    #expect(MuxaAutomationWaitDraft.read("", event: .idleFor).text.isEmpty)

    // A spelling the controls cannot express is handed back untouched.
    for odd in ["reset*2m", "20", "whenever", "{{reset}}~1h"] {
        let draft = MuxaAutomationWaitDraft.read(odd, event: .rateLimited)
        #expect(draft.anchor == .freeform, "\(odd) should stay as it was written")
        #expect(draft.text == odd)
        #expect(!draft.freeformIsReadable)
    }
    // …unless it is edited into something readable, which is the way back
    // to the controls.
    var edited = MuxaAutomationWaitDraft.read("reset*2m", event: .rateLimited)
    edited.freeform = "10m"
    #expect(edited.freeformIsReadable)

    // The direction control, not a typed sign, decides which way it runs.
    var composed = MuxaAutomationWaitDraft(anchor: .reset, isBefore: true, offset: "45s")
    #expect(composed.text == "{{reset}}-45s")
    composed.isBefore = false
    #expect(composed.text == "{{reset}}+45s")
    composed.offset = "  "
    #expect(composed.text == "{{reset}}")

    // A composed value the daemon would refuse is refused here first.
    var rule = MuxaAutomationRuleDraft()
    rule.name = "resume"
    rule.text = "continue"
    rule.wait = MuxaAutomationWaitDraft(anchor: .reset, offset: "2x").text
    #expect(rule.wait == "{{reset}}+2x")
    #expect(rule.issues(existingNames: []) == [.invalidWait])
    rule.wait = "whenever"
    #expect(rule.issues(existingNames: []) == [.invalidWait])
    // The braces validate exactly as the bare spelling did.
    rule.wait = "{{reset}}+2m"
    #expect(rule.issues(existingNames: []).isEmpty)
    #expect(rule.timing == .afterReset(120))
}

@Test
func agentMarksAndLimitWindowsSayWhatTheWireValueMeans() {
    #expect(MuxaAgentMark.title(for: "claude_code") == "Claude Code")
    #expect(MuxaAgentMark.title(for: "codex") == "Codex")
    #expect(MuxaAgentMark.title(for: "gemini_cli") == "Gemini")
    #expect(MuxaAgentMark.title(for: "antigravity") == "Antigravity")
    #expect(MuxaAgentMark.title(for: "opencode") == "opencode")

    // Every kind the editor offers has a mark.
    for kind in MuxaAutomationRuleDraft.agentKinds {
        #expect(MuxaAgentMark.known(for: kind) != nil, "\(kind) has no mark")
    }

    // The symbols and tints come from the tables the app already had, not a
    // third one.
    #expect(MuxaAgentMark.known(for: "claude_code")?.symbol == AskProviderEngine.claude.symbolName)
    #expect(MuxaAgentMark.known(for: "codex")?.symbol == AskProviderEngine.codex.symbolName)
    #expect(MuxaAgentMark.known(for: "gemini_cli")?.symbol == AskProviderEngine.gemini.symbolName)
    #expect(MuxaAgentMark.known(for: "antigravity")?.symbol == "arrow.up.circle")
    #expect(MuxaAgentMark.known(for: "opencode")?.symbol == "terminal")
    #expect(MuxaAgentMark.known(for: "claude_code")?.tint == agentProgramTint("claude"))
    #expect(MuxaAgentMark.known(for: "codex")?.tint == agentProgramTint("codex"))
    #expect(MuxaAgentMark.known(for: "gemini_cli")?.tint == agentProgramTint("gemini"))
    #expect(MuxaAgentMark.known(for: "antigravity")?.tint == agentProgramTint("agy"))
    #expect(MuxaAgentMark.known(for: "opencode")?.tint == agentProgramTint("opencode"))

    // An agent the daemon knows and this build does not keeps working under
    // its own wire spelling, and gets no invented symbol.
    #expect(MuxaAgentMark.known(for: "aider") == nil)
    #expect(MuxaAgentMark.title(for: "aider") == "aider")
    #expect(AutomationTokenStyle.agent.title(for: "aider") == "aider")

    #expect(automationLimitScopeTitle("five_hour") == "5-hour limit")
    #expect(automationLimitScopeTitle("seven_day") == "Weekly limit")
    #expect(automationLimitScopeTitle("unknown") == "Unspecified")
    // No window the editor offers is left showing its wire spelling…
    for scope in MuxaAutomationRuleDraft.rateLimitScopes {
        #expect(automationLimitScopeTitle(scope) != scope, "\(scope) reads as a wire value")
    }
    // …and one this build does not know shows verbatim.
    #expect(automationLimitScopeTitle("thirty_day") == "thirty_day")
    #expect(AutomationTokenStyle.limitScope.title(for: "thirty_day") == "thirty_day")
}

/// A filter takes a *value*, so none of its fields may carry a placeholder:
/// an empty filter matches everything, and a greyed example reads like a
/// value that is already set. A field that takes a *format* keeps its
/// example. The rule is about the source — a SwiftUI `TextField` has nothing
/// to ask at runtime — so the test reads the pane.
@Test
func filterFieldsCarryNoPlaceholderAndFormatFieldsKeepTheirs() throws {
    let pane = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()
        .deletingLastPathComponent()
        .appendingPathComponent("Sources/AutomationSettingsView.swift")
    let source = try String(contentsOf: pane, encoding: .utf8)
    let lines = source.split(separator: "\n", omittingEmptySubsequences: false)

    for field in ["Workspace", "Work id matches", "Pane"] {
        let declaration = lines.first { $0.contains("TextField(\"\(field)\"") }
        #expect(declaration != nil, "the \(field) filter field is gone")
        #expect(declaration?.contains("prompt:") == false, "\(field) shows an example value")
    }
    // The operator's own team prefix must not ship as a hint.
    #expect(!source.contains("^CAL-"))

    // Durations and the text typed into a pane take a format, so they keep
    // their examples.
    #expect(source.contains("TextField(\"Cooldown\", text: $draft.cooldown, prompt:"))
    #expect(source.contains("TextField(\"Jitter\", text: $draft.jitter, prompt:"))
    #expect(source.contains("TextField(\"Text\", text: $draft.text, prompt:"))
}
