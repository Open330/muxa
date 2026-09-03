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
    func rawTerminalFormatterMakesControlsVisibleWithoutExecutingThem() {
        let bytes = Data("A\u{001B}[31mB\r\n\t한글".utf8)
        let dump = terminalRawDescription(bytes)
        #expect(dump.contains("00000000"))
        #expect(dump.contains("41 1B 5B 33 31 6D 42 0D"))
        #expect(dump.contains("A.[31mB."))
        #expect(!dump.contains("\u{001B}"))
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
        #expect(MuxaInboxHostFailureText.summary(["jiun-mbp": "ssh timed out"]) == "1 host unreachable: jiun-mbp")
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
